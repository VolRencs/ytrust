use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, YtrustError>;

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
}
