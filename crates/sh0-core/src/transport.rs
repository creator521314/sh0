//! 会话传输层抽象（需求 NFR-9 / NFR-10）。
//!
//! `sh0-ssh` 的 russh 实现只是这个 trait 的一种实现；测试与契约验证可以注入
//! `MockTransport`，不必真的连一台服务器。
//!
//! 注意 `wait` 与 `send` 都取 `&mut self`：russh 的 `Channel::wait()` 需要可变借用，
//! 与其在 select! 里和 send 抢借用（M0 已经踩过一次），不如一开始就统一成
//! `&mut self`，由调用方串行调度。

use async_trait::async_trait;
use std::time::Duration;
use thiserror::Error;

use crate::Host;
use crate::sftp::SftpClient;

/// 传输层事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    /// 远端输出
    Data(Vec<u8>),
    /// 通道 EOF
    Eof,
    /// 命令/会话退出，携带退出码
    Exit(i32),
    /// 连接中断（网络故障、服务端断开等），携带原因
    Broken(String),
}

/// exec channel 一次执行的结果（`ai exec run --mode exec` 的备选路径）。
///
/// 与 shell 哨兵模式的区别：stdout / stderr 天然分离、退出码精确，
/// 但拿不到交互式上下文（cwd、环境变量、前一条命令的状态）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    /// 远端没给退出码（超时被掐断等）时为 None
    pub exit_code: Option<i32>,
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("连接失败: {0}")]
    Connect(String),
    #[error("认证失败: {0}")]
    Auth(String),
    #[error("已断开: {0}")]
    Disconnected(String),
    #[error("未实现: {0}")]
    NotImplemented(String),
}

pub type TransportResult<T> = std::result::Result<T, TransportError>;

#[async_trait]
pub trait SessionTransport: Send {
    /// 等待下一个事件；返回 None 表示连接已彻底关闭
    async fn wait(&mut self) -> Option<TransportEvent>;
    async fn send(&mut self, data: &[u8]) -> TransportResult<()>;
    /// 通知远端终端尺寸变化（需求 FR-2.4）
    async fn resize(&mut self, cols: u16, rows: u16) -> TransportResult<()>;
    async fn close(&mut self) -> TransportResult<()>;

    /// 在**本条连接**上打开 SFTP 子系统（FR-4.1：复用现有 SSH 连接，不新开 TCP）。
    ///
    /// 默认未实现：终端传输不需要它。只有真的能复用连接的实现才 override。
    async fn sftp(&mut self) -> TransportResult<Box<dyn SftpClient>> {
        Err(TransportError::NotImplemented(
            "该传输不支持 SFTP".into(),
        ))
    }
}

/// 传输层工厂：一次重连就是一次 `connect`
#[async_trait]
pub trait TransportFactory: Send + Sync {
    async fn connect(
        &self,
        host: &Host,
        cols: u16,
        rows: u16,
    ) -> TransportResult<Box<dyn SessionTransport + Send>>;

    /// 独立 exec channel 执行命令（W2.4 的备选路径，不复用交互 shell）。
    ///
    /// 默认未实现：shell 传输不需要它，mock 与真实 SSH 工厂按需支持。
    /// daemon 在 `--mode exec` 时调用；不支持时回退到 shell 哨兵模式。
    async fn exec(&self, host: &Host, cmd: &str, timeout: Duration) -> TransportResult<ExecOutput> {
        let _ = (host, cmd, timeout);
        Err(TransportError::NotImplemented("exec channel 不可用".into()))
    }

    /// 一键式 SFTP：没有现成会话时新建一条连接，开 SFTP channel。
    ///
    /// 调用方用完即 `close()`（连同一条 TCP 连接一起释放），
    /// 因此只适合 `sh0 ai file --host X` 这种一次性操作；
    /// 有会话时优先走 [`SessionTransport::sftp`] 复用连接。
    async fn open_sftp(&self, host: &Host) -> TransportResult<Box<dyn SftpClient>> {
        let _ = host;
        Err(TransportError::NotImplemented("SFTP 不可用".into()))
    }
}
