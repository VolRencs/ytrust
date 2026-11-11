use std::fmt;
use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, YtrustError>;

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
    Http(#[from] reqwest::Error),

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

    #[error("format not available: {0}")]
    FormatNotFound(String),

    #[error("ffmpeg was not found. Install it or pass --ffmpeg-location")]
    FfmpegMissing,

    #[error("ffmpeg failed with status {status}: {stderr}")]
    Ffmpeg { status: String, stderr: String },

    #[error("output path is invalid: {0}")]
    InvalidOutput(PathBuf),
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

    /// Wrap an error with context while preserving its stable code.
    pub fn context(self, context: impl fmt::Display) -> Self {
        let code = self.code();
        Self::Coded {
            code,
            message: format!("{context}: {self}"),
        }
    }

    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Http(error) if error.is_timeout() => ErrorCode::Timeout,
            Self::Http(error) if error.is_connect() => ErrorCode::Network,
            Self::Http(error) if error.is_status() => ErrorCode::Http,
            Self::Http(_) => ErrorCode::Network,
            Self::Io(_) => ErrorCode::Io,
            Self::Json(_) => ErrorCode::InvalidResponse,
            Self::Sqlite(_) => ErrorCode::Cookies,
            Self::Url(_) => ErrorCode::InvalidInput,
            Self::Coded { code, .. } => *code,
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
    fn context_preserves_code() {
        let error = YtrustError::coded(ErrorCode::Cookies, "bad").context("load");
        assert_eq!(error.code(), ErrorCode::Cookies);
        assert!(error.to_string().contains("load: bad"));
    }
}
