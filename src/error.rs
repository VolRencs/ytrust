use std::fmt;
use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, YtrustError>;

/// Higher values are more useful to callers when several client attempts fail.
pub const fn error_priority(code: ErrorCode) -> u8 {
    match code {
        ErrorCode::BotCheck => 10,
        ErrorCode::AgeRestricted => 9,
        ErrorCode::Private => 8,
        ErrorCode::Cookies => 7,
        ErrorCode::ChallengeFailed => 6,
        ErrorCode::Unavailable => 5,
        ErrorCode::Timeout => 4,
        ErrorCode::Network => 3,
        ErrorCode::InvalidResponse => 2,
        ErrorCode::Http => 1,
        _ => 0,
    }
}

/// Pick the most actionable error from a set of failed attempts instead of
/// blindly returning the last one.
pub fn best_error(errors: Vec<YtrustError>) -> Option<YtrustError> {
    errors
        .into_iter()
        .max_by_key(|error| error_priority(error.code()))
}

/// Stable, machine-readable error codes exposed by the Node module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    AgeRestricted,
    BotCheck,
    Private,
    Unavailable,
    Cookies,
    UnsupportedUrl,
    InvalidInput,
    Timeout,
    Network,
    Http,
    NoAudio,
    InvalidResponse,
    NativeLoad,
    FormatNotFound,
    Ffmpeg,
    Io,
    ChallengeFailed,
    Unknown,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgeRestricted => "AGE_RESTRICTED",
            Self::BotCheck => "BOTCHECK",
            Self::Private => "PRIVATE",
            Self::Unavailable => "UNAVAILABLE",
            Self::Cookies => "COOKIES",
            Self::UnsupportedUrl => "UNSUPPORTED_URL",
            Self::InvalidInput => "INVALID_INPUT",
            Self::Timeout => "TIMEOUT",
            Self::Network => "NETWORK",
            Self::Http => "HTTP",
            Self::NoAudio => "NO_AUDIO",
            Self::InvalidResponse => "INVALID_RESPONSE",
            Self::NativeLoad => "NATIVE_LOAD",
            Self::FormatNotFound => "FORMAT_NOT_FOUND",
            Self::Ffmpeg => "FFMPEG",
            Self::Io => "IO",
            Self::ChallengeFailed => "CHALLENGE_FAILED",
            Self::Unknown => "UNKNOWN",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Error)]
pub enum YtrustError {
    #[error("HTTP: {0}")]
    Http(reqwest::Error),

    #[error("{message}")]
    HttpStatus { status: u16, message: String },

    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("invalid URL: {0}")]
    Url(#[from] url::ParseError),

    #[error("{0}")]
    Message(String),

    #[error("{message}")]
    Coded { code: ErrorCode, message: String },

    #[error("{context}: {source}")]
    Context {
        context: String,
        #[source]
        source: Box<YtrustError>,
    },

    #[error("format not available: {0}")]
    FormatNotFound(String),

    #[error("ffmpeg was not found. Install it or pass --ffmpeg-location")]
    FfmpegMissing,

    #[error("ffmpeg failed with status {status}: {stderr}")]
    Ffmpeg { status: String, stderr: String },

    #[error("output path is invalid: {0}")]
    InvalidOutput(PathBuf),
}

impl From<reqwest::Error> for YtrustError {
    fn from(error: reqwest::Error) -> Self {
        // Signed googlevideo URLs must never leak into errors exposed to
        // callers, and reqwest includes the full URL by default.
        Self::Http(error.without_url())
    }
}

impl YtrustError {
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    pub fn coded(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Coded {
            code,
            message: message.into(),
        }
    }

    /// Wrap an error with context while preserving its stable code and source.
    pub fn context(self, context: impl fmt::Display) -> Self {
        Self::Context {
            context: context.to_string(),
            source: Box::new(self),
        }
    }

    pub fn is_broken_pipe(&self) -> bool {
        match self {
            Self::Io(error) => error.kind() == std::io::ErrorKind::BrokenPipe,
            Self::Context { source, .. } => source.is_broken_pipe(),
            _ => false,
        }
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http(error) => {
                error.is_timeout()
                    || error.is_connect()
                    || error.is_body()
                    || (error.is_request() && !error.is_builder())
                    || error.status().is_some_and(|status| {
                        status == 408 || status == 429 || status == 425 || status.is_server_error()
                    })
            }
            Self::HttpStatus { status, .. } => {
                *status == 408 || *status == 425 || *status == 429 || *status >= 500
            }
            Self::Coded { code, .. } => matches!(code, ErrorCode::Timeout | ErrorCode::Network),
            Self::Context { source, .. } => source.is_retryable(),
            Self::Io(error) => matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
            ),
            _ => false,
        }
    }

    pub fn is_forbidden(&self) -> bool {
        match self {
            Self::Http(error) => error
                .status()
                .is_some_and(|status| matches!(status.as_u16(), 401 | 403 | 410)),
            Self::HttpStatus { status, .. } => matches!(status, 401 | 403 | 410),
            Self::Ffmpeg { stderr, .. } => {
                let stderr = stderr.to_ascii_lowercase();
                (stderr.contains("403") || stderr.contains("410"))
                    && (stderr.contains("forbidden")
                        || stderr.contains("access denied")
                        || stderr.contains("http"))
            }
            Self::Context { source, .. } => source.is_forbidden(),
            _ => false,
        }
    }

    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Http(error) if error.is_timeout() => ErrorCode::Timeout,
            Self::Http(error) if error.is_connect() => ErrorCode::Network,
            Self::Http(error) if error.is_status() => ErrorCode::Http,
            Self::Http(_) => ErrorCode::Network,
            Self::HttpStatus { .. } => ErrorCode::Http,
            Self::Io(_) => ErrorCode::Io,
            Self::Json(_) => ErrorCode::InvalidResponse,
            Self::Sqlite(_) => ErrorCode::Cookies,
            Self::Url(_) => ErrorCode::InvalidInput,
            Self::Coded { code, .. } => *code,
            Self::Context { source, .. } => source.code(),
            Self::FormatNotFound(_) => ErrorCode::FormatNotFound,
            Self::FfmpegMissing | Self::Ffmpeg { .. } => ErrorCode::Ffmpeg,
            Self::InvalidOutput(_) => ErrorCode::InvalidInput,
            Self::Message(_) => ErrorCode::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_stable_codes() {
        assert_eq!(
            YtrustError::coded(ErrorCode::AgeRestricted, "x").code(),
            ErrorCode::AgeRestricted
        );
        assert_eq!(
            YtrustError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "x")).code(),
            ErrorCode::Io
        );
        assert_eq!(YtrustError::msg("x").code(), ErrorCode::Unknown);
        assert_eq!(
            YtrustError::FormatNotFound("x".into()).code(),
            ErrorCode::FormatNotFound
        );
    }

    #[test]
    fn classifies_forbidden_errors() {
        assert!(
            YtrustError::HttpStatus {
                status: 403,
                message: "x".into()
            }
            .is_forbidden()
        );
        assert!(
            YtrustError::HttpStatus {
                status: 410,
                message: "x".into()
            }
            .is_forbidden()
        );
        assert!(
            !YtrustError::HttpStatus {
                status: 404,
                message: "x".into()
            }
            .is_forbidden()
        );
        assert!(
            YtrustError::Ffmpeg {
                status: "8".into(),
                stderr: "Error opening input: Server returned 403 Forbidden".into()
            }
            .is_forbidden()
        );
        let wrapped = YtrustError::HttpStatus {
            status: 403,
            message: "x".into(),
        }
        .context("download video");
        assert!(wrapped.is_forbidden());
    }

    #[test]
    fn classifies_retryable_statuses() {
        assert!(
            YtrustError::HttpStatus {
                status: 503,
                message: "x".into()
            }
            .is_retryable()
        );
        assert!(
            YtrustError::HttpStatus {
                status: 429,
                message: "x".into()
            }
            .is_retryable()
        );
        assert!(
            !YtrustError::HttpStatus {
                status: 404,
                message: "x".into()
            }
            .is_retryable()
        );
        assert!(!YtrustError::coded(ErrorCode::InvalidInput, "x").is_retryable());
    }

    #[test]
    fn context_preserves_code() {
        let error = YtrustError::coded(ErrorCode::Cookies, "bad").context("load");
        assert_eq!(error.code(), ErrorCode::Cookies);
        assert!(error.to_string().contains("load: bad"));
    }

    #[test]
    fn context_preserves_broken_pipe() {
        let error = YtrustError::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed",
        ))
        .context("playlist item 1");
        assert!(error.is_broken_pipe());
        assert_eq!(error.code(), ErrorCode::Io);
    }

    #[test]
    fn retries_request_timeouts() {
        assert!(
            YtrustError::HttpStatus {
                status: 408,
                message: "x".into()
            }
            .is_retryable()
        );
        assert!(
            !YtrustError::HttpStatus {
                status: 403,
                message: "x".into()
            }
            .is_retryable()
        );
    }
}
