use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid utf-16 in path or filename")]
    InvalidUtf16,

    #[error("path is not absolute: {0}")]
    NotAbsolute(PathBuf),

    #[error("plugin error: {0}")]
    Plugin(String),

    #[error("prism error: {0}")]
    Prism(String),

    #[error("rclone error: {0}")]
    Rclone(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }
}
