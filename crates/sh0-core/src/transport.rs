//! 会话传输层抽象（需求 NFR-9 / NFR-10）。
//!
//! `sh0-ssh` 的 russh 实现只是这个 trait 的一种实现；测试与契约验证可以注入
//! `MockTransport`，不必真的连一台服务器。
//!
//! 注意 `wait` 与 `send` 都取 `&mut self`：russh 的 `Channel::wait()` 需要可变借用，
//! 与其在 select! 里和 send 抢借用（M0 已经踩过一次），不如一开始就统一成
//! `&mut self`，由调用方串行调度。

use async_trait::async_trait;
use thiserror::Error;

use crate::Host;

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
}
