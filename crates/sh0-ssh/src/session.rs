//! russh 对 [`sh0_core::transport`] 的实现。
//!
//! 有了这一层，daemon 只依赖 `TransportFactory` trait，不再直接碰 russh 类型；
//! 测试时把 `RusshFactory` 换成 `MockFactory` 即可跑通整条链路。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use russh::ChannelMsg;

use sh0_core::transport::{
    ExecOutput, SessionTransport, TransportError, TransportEvent, TransportFactory, TransportResult,
};
use sh0_core::{Host, SecretResolver};

use crate::client::{ConnectOptions, ShellHandle, SshConnection};
use crate::error::SshError;

/// 真实 SSH 连接的工厂
#[derive(Clone)]
pub struct RusshFactory {
    pub known_hosts: PathBuf,
    /// 0 表示不启用 SSH 层保活（需求 FR-2.7）
    pub keepalive_secs: u64,
    /// 远端 TERM 环境变量值
    pub term: String,
    /// 秘密解析后端（钥匙串 / vault）。None 时用 DefaultResolver。
    pub resolver: Option<Arc<dyn SecretResolver>>,
}

impl std::fmt::Debug for RusshFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RusshFactory")
            .field("known_hosts", &self.known_hosts)
            .field("keepalive_secs", &self.keepalive_secs)
            .field("term", &self.term)
            .field("resolver", &self.resolver.as_ref().map(|r| r.backend_name()))
            .finish()
    }
}

impl RusshFactory {
    pub fn new(known_hosts: impl AsRef<Path>, keepalive_secs: u64, term: impl Into<String>) -> Self {
        Self {
            known_hosts: known_hosts.as_ref().to_path_buf(),
            keepalive_secs,
            term: term.into(),
            resolver: None,
        }
    }

    /// 注入凭据存储后端（W2.12）
    pub fn with_resolver(mut self, resolver: Arc<dyn SecretResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    fn opts(&self) -> ConnectOptions {
        ConnectOptions {
            known_hosts: self.known_hosts.clone(),
            keepalive_secs: self.keepalive_secs,
            resolver: self.resolver.clone(),
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
        let conn = SshConnection::connect(host, self.opts()).await?;
        let shell = conn.open_shell(&self.term, cols, rows).await?;
        Ok(Box::new(RusshTransport { conn, shell }))
    }

    /// exec channel 备选路径（W2.4）：不复用交互 shell，
    /// 因此 stdout/stderr 分离、退出码精确，代价是拿不到 cwd 等上下文。
    async fn exec(&self, host: &Host, cmd: &str, timeout: Duration) -> TransportResult<ExecOutput> {
        let conn = SshConnection::connect(host, self.opts()).await?;
        conn.exec_command(cmd, timeout)
            .await
            .map_err(TransportError::from)
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
        // ⚠️ 千万别把"不认识的消息"映射成 EOF。
        // 真机上海量非数据消息（pty-req 的 Success、WindowAdjust、XonXoff…）
        // 会先于 shell 的输出到达；旧版把它们一律当 EOF，导致会话刚连上就断开
        // ——mock 传输层不产生这些消息，所以测试全绿也没能发现（真机实测才暴露）。
        loop {
            let msg = self.shell.wait().await?;
            let ev = match msg {
                ChannelMsg::Data { data } => TransportEvent::Data(data.to_vec()),
                // ext == 1 是 stderr：终端场景里与 stdout 一起渲染才不会乱序
                ChannelMsg::ExtendedData { data, ext: 1 } => TransportEvent::Data(data.to_vec()),
                ChannelMsg::ExitStatus { exit_status } => TransportEvent::Exit(exit_status as i32),
                ChannelMsg::Eof | ChannelMsg::Close => TransportEvent::Eof,
                other => {
                    tracing::trace!("忽略与终端无关的通道消息: {other:?}");
                    continue;
                }
            };
            return Some(ev);
        }
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
