use thiserror::Error;

pub type Result<T> = std::result::Result<T, SshError>;

#[derive(Debug, Error)]
pub enum SshError {
    #[error("连接失败: {0}")]
    Connect(#[source] russh::Error),

    #[error("认证失败: {0}")]
    Auth(String),

    #[error("主机密钥校验失败: {0}")]
    HostKey(String),

    #[error("凭据解析失败: {0}")]
    Credential(String),

    #[error("SFTP 失败: {0}")]
    Sftp(String),

    #[error("尚未实现（后续里程碑）: {0}")]
    NotImplemented(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
