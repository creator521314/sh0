//! russh 封装：连接、认证、PTY 会话（需求 FR-2.1）
//!
//! 这是整个项目里**唯一**接触 SSH 协议的地方。上层（core / tui / ai）只看到
//! `SshConnection` 与 `ShellHandle`，不直接依赖 russh，便于后续替换实现或注入
//! mock（NFR-9 / NFR-10）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, AuthResult, Handle, Handler};
use russh::keys::agent::client::AgentClient;
use russh::keys::agent::AgentIdentity;
use russh::keys::ssh_key::PublicKey;
use russh::keys::{load_secret_key, HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};

use sh0_core::{CredentialRef, Host, SecretRef};

use crate::error::{Result, SshError};
use crate::host_key::{HostKeyVerdict, verify};

/// 连接参数
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub known_hosts: PathBuf,
    /// 0 表示不启用 keepalive
    pub keepalive_secs: u64,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            known_hosts: PathBuf::from("known_hosts"),
            keepalive_secs: 30,
        }
    }
}

/// russh 的事件回调。M0 只关心主机密钥校验。
struct ClientHandler {
    known_hosts: PathBuf,
    host: String,
    port: u16,
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let key: &PublicKey = match server_public_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            PublicKeyOrCertificate::Certificate(_) => {
                tracing::warn!("M0 不接受 OpenSSH 证书，拒绝连接");
                return Ok(false);
            }
        };

        match verify(&self.known_hosts, &self.host, self.port, key) {
            HostKeyVerdict::Known => Ok(true),
            HostKeyVerdict::Learned => {
                tracing::info!("首次连接 {}，已记录主机密钥（TOFU）", self.host);
                Ok(true)
            }
            HostKeyVerdict::Mismatch { line } => {
                tracing::error!(
                    "{} 的主机密钥与 known_hosts 第 {} 行记录不一致，拒绝连接",
                    self.host,
                    line
                );
                Ok(false)
            }
        }
    }
}

/// 一条已认证的 SSH 连接
pub struct SshConnection {
    handle: Handle<ClientHandler>,
}

impl SshConnection {
    /// 建立连接并完成认证
    pub async fn connect(host: &Host, opts: ConnectOptions) -> Result<Self> {
        let config = Arc::new(client::Config {
            keepalive_interval: if opts.keepalive_secs > 0 {
                Some(Duration::from_secs(opts.keepalive_secs))
            } else {
                None
            },
            keepalive_max: 3,
            ..<_>::default()
        });

        let handler = ClientHandler {
            known_hosts: opts.known_hosts,
            host: host.addr.clone(),
            port: host.port,
        };

        let mut handle = client::connect(config, (host.addr.as_str(), host.port), handler)
            .await
            .map_err(SshError::Connect)?;

        authenticate(&mut handle, host).await?;

        Ok(Self { handle })
    }

    /// 打开一个带 PTY 的交互 shell
    pub async fn open_shell(&self, term: &str, cols: u16, rows: u16) -> Result<ShellHandle> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(SshError::Connect)?;

        channel
            .request_pty(true, term, cols as u32, rows as u32, 0, 0, &[])
            .await
            .map_err(SshError::Connect)?;
        channel
            .request_shell(true)
            .await
            .map_err(SshError::Connect)?;

        Ok(ShellHandle { channel })
    }

    pub async fn close(&mut self) -> Result<()> {
        self.handle
            .disconnect(Disconnect::ByApplication, "", "English")
            .await
            .map_err(SshError::Connect)
    }
}

/// 一个交互式 shell 通道
pub struct ShellHandle {
    channel: russh::Channel<russh::client::Msg>,
}

impl ShellHandle {
    /// 把本地按键发往远端
    pub async fn send(&self, bytes: &[u8]) -> Result<()> {
        self.channel
            .data_bytes(bytes.to_vec())
            .await
            .map_err(SshError::Connect)
    }

    /// 等待下一条通道消息
    pub async fn wait(&mut self) -> Option<ChannelMsg> {
        self.channel.wait().await
    }

    /// 终端尺寸变化同步（需求 FR-2.4）
    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.channel
            .window_change(cols as u32, rows as u32, 0, 0)
            .await
            .map_err(SshError::Connect)
    }

    pub async fn eof(&self) -> Result<()> {
        self.channel.eof().await.map_err(SshError::Connect)
    }
}

async fn authenticate(session: &mut Handle<ClientHandler>, host: &Host) -> Result<()> {
    match &host.auth {
        CredentialRef::Password { secret } => {
            let pw = resolve_secret(secret, &format!("{} 的密码", host.endpoint()))?;
            let res = session
                .authenticate_password(host.user.clone(), pw)
                .await
                .map_err(|e| SshError::Auth(e.to_string()))?;
            ensure_success(res)?;
        }
        CredentialRef::KeyFile { path, passphrase } => {
            let pp = match passphrase {
                Some(s) => Some(resolve_secret(s, &format!("私钥 {} 的 passphrase", path.display()))?),
                None => None,
            };
            let key = load_secret_key(path, pp.as_deref())
                .map_err(|e| SshError::Credential(format!("{path:?} 加载失败: {e}")))?;
            let hash: Option<HashAlg> = session
                .best_supported_rsa_hash()
                .await
                .map_err(|e| SshError::Auth(e.to_string()))?
                .flatten();
            let res = session
                .authenticate_publickey(
                    host.user.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await
                .map_err(|e| SshError::Auth(e.to_string()))?;
            ensure_success(res)?;
        }
        CredentialRef::Agent => {
            authenticate_with_agent(session, host).await?;
        }
        CredentialRef::Interactive => {
            return Err(SshError::NotImplemented(
                "keyboard-interactive / 2FA 需要交互式应答界面，计划于 M1 随 TUI 输入框落地".into(),
            ));
        }
        CredentialRef::None => {
            let res = session
                .authenticate_none(host.user.clone())
                .await
                .map_err(|e| SshError::Auth(e.to_string()))?;
            ensure_success(res)?;
        }
    }
    Ok(())
}

async fn authenticate_with_agent(session: &mut Handle<ClientHandler>, host: &Host) -> Result<()> {
    // Unix 走 SSH_AUTH_SOCK，Windows 走 Pageant 命名管道
    #[cfg(unix)]
    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|e| SshError::Credential(format!("连接 ssh-agent 失败: {e}")))?;
    #[cfg(windows)]
    let mut agent = AgentClient::connect_pageant()
        .await
        .map_err(|e| SshError::Credential(format!("连接 Pageant 失败: {e}")))?;

    let identities = agent
        .request_identities()
        .await
        .map_err(|e| SshError::Credential(format!("读取 agent 身份失败: {e}")))?;

    for identity in &identities {
        let (key, comment) = match identity {
            AgentIdentity::PublicKey { key, comment } => (key, comment),
            AgentIdentity::Certificate { .. } => continue,
        };

        let res = session
            .authenticate_publickey_with(host.user.clone(), key.clone(), None, &mut agent)
            .await
            .map_err(|e| SshError::Auth(format!("{e}")))?;

        if res.success() {
            tracing::info!("用 agent 密钥 `{comment}` 认证成功");
            return Ok(());
        }
    }

    Err(SshError::Auth(
        "agent 中没有任何密钥被服务端接受".to_string(),
    ))
}

fn ensure_success(res: AuthResult) -> Result<()> {
    if res.success() {
        Ok(())
    } else {
        Err(SshError::Auth("服务端拒绝了本次认证".to_string()))
    }
}

/// 把 `SecretRef` 解析成实际的秘密值。
///
/// 注意：M0 的 `Prompt` 直接从 stdin 读取，因此**不适用于 TUI raw mode 场景**。
/// TUI 下请使用 `Env` 或无 passphrase 的私钥；带隐藏输入的提示框计划于 M1 落地。
pub fn resolve_secret(r: &SecretRef, prompt: &str) -> Result<String> {
    match r {
        SecretRef::Env(name) => std::env::var(name)
            .map_err(|_| SshError::Credential(format!("环境变量 {name} 未设置"))),
        SecretRef::Prompt => {
            use std::io::Write;
            eprint!("{prompt}: ");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .map_err(SshError::Io)?;
            Ok(line.trim_end().to_string())
        }
        SecretRef::Keyring { .. } => Err(SshError::NotImplemented(
            "系统钥匙串（FR-1.6）计划于 M2 落地".into(),
        )),
    }
}

/// 便于测试：确认私钥文件可被解析
pub fn probe_key(path: &Path, passphrase: Option<&str>) -> Result<()> {
    load_secret_key(path, passphrase)
        .map(|_| ())
        .map_err(|e| SshError::Credential(format!("{path:?}: {e}")))
}
