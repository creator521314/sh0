use thiserror::Error;

pub type Result<T> = std::result::Result<T, IpcError>;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("daemon 未运行或无法连接: {0}")]
    Unreachable(String),

    #[error("鉴权失败: {0}")]
    Unauthorized(String),

    #[error("daemon 返回错误 [{code}]: {message}")]
    Remote { code: String, message: String },

    #[error("协议错误: {0}")]
    Protocol(String),

    #[error("连接已关闭")]
    Disconnected,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
