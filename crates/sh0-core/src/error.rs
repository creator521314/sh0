use thiserror::Error;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("host not found: {0}")]
    HostNotFound(String),

    #[error("ambiguous host selector '{selector}', matched {count} hosts")]
    AmbiguousHost { selector: String, count: usize },

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("credential resolution failed: {0}")]
    Credential(String),

    #[error("feature not implemented yet in this milestone: {0}")]
    NotImplemented(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
