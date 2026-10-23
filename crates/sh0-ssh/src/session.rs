//! russh 对 [`sh0_core::transport`] 的实现。
//!
//! 有了这一层，daemon 只依赖 `TransportFactory` trait，不再直接碰 russh 类型；
//! 测试时把 `RusshFactory` 换成 `MockFactory` 即可跑通整条链路。

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use russh::ChannelMsg;

use sh0_core::Host;
use sh0_core::transport::{
    SessionTransport, TransportError, TransportEvent, TransportFactory, TransportResult,
};

use crate::client::{ConnectOptions, ShellHandle, SshConnection};
use crate::error::SshError;

/// 真实 SSH 连接的工厂
#[derive(Debug, Clone)]
pub struct RusshFactory {
    pub known_hosts: PathBuf,
    /// 0 表示不启用 SSH 层保活（需求 FR-2.7）
    pub keepalive_secs: u64,
    /// 远端 TERM 环境变量值
    pub term: String,
}

impl RusshFactory {
    pub fn new(known_hosts: impl AsRef<Path>, keepalive_secs: u64, term: impl Into<String>) -> Self {
        Self {
            known_hosts: known_hosts.as_ref().to_path_buf(),
            keepalive_secs,
            term: term.into(),
        }
    }
}

#[async_trait]
impl TransportFactory for RusshFactory {
    async fn connect(
        &self,
        host: &Host,
        cols: u16,
        rows: u16,
    ) -> TransportResult<Box<dyn SessionTransport + Send>> {
        let opts = ConnectOptions {
            known_hosts: self.known_hosts.clone(),
            keepalive_secs: self.keepalive_secs,
        };
        let conn = SshConnection::connect(host, opts).await?;
        let shell = conn.open_shell(&self.term, cols, rows).await?;
        Ok(Box::new(RusshTransport { conn, shell }))
    }
}

/// 一个交互式 shell 通道
pub struct RusshTransport {
    conn: SshConnection,
    shell: ShellHandle,
}

#[async_trait]
impl SessionTransport for RusshTransport {
    async fn wait(&mut self) -> Option<TransportEvent> {
        let msg = self.shell.wait().await?;
        Some(match msg {
            ChannelMsg::Data { data } => TransportEvent::Data(data.to_vec()),
            // ext == 1 是 stderr：终端场景里与 stdout 一起渲染才不会乱序
            ChannelMsg::ExtendedData { data, ext: 1 } => TransportEvent::Data(data.to_vec()),
            ChannelMsg::ExitStatus { exit_status } => TransportEvent::Exit(exit_status as i32),
            ChannelMsg::Eof | ChannelMsg::Close => TransportEvent::Eof,
            _ => TransportEvent::Eof,
        })
    }

    async fn send(&mut self, data: &[u8]) -> TransportResult<()> {
        self.shell.send(data).await?;
        Ok(())
    }

    async fn resize(&mut self, cols: u16, rows: u16) -> TransportResult<()> {
        self.shell.resize(cols, rows).await?;
        Ok(())
    }

    async fn close(&mut self) -> TransportResult<()> {
        let _ = self.shell.eof().await;
        self.conn.close().await?;
        Ok(())
    }
}

impl From<SshError> for TransportError {
    fn from(e: SshError) -> Self {
        match e {
            SshError::Auth(m) => TransportError::Auth(m),
            SshError::Credential(m) => TransportError::Auth(m),
            SshError::Connect(inner) => TransportError::Connect(inner.to_string()),
            SshError::NotImplemented(m) => TransportError::NotImplemented(m),
            other => TransportError::Connect(other.to_string()),
        }
    }
}

/// 保活间隔的默认值（需求 FR-2.7）
pub const DEFAULT_KEEPALIVE_SECS: u64 = 30;

/// 便于配置解析：把秒转成 russh 需要的 Duration
pub fn keepalive_interval(secs: u64) -> Option<Duration> {
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}
