use std::path::PathBuf;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("cannot determine platform config directory")]
    NoConfigDir,

    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("failed to serialize config: {0}")]
    Serialize(#[source] toml::ser::Error),

    #[error("credential error: {0}")]
    Credential(String),

    #[error("database error: {0}")]
    Db(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
