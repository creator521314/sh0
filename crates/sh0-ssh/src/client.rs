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

use sh0_core::transport::ExecOutput;
use sh0_core::{CredentialRef, Host, SecretRef, SecretResolver};

use crate::error::{Result, SshError};
use crate::host_key::{HostKeyVerdict, verify};

/// 连接参数
#[derive(Clone)]
pub struct ConnectOptions {
    pub known_hosts: PathBuf,
    /// 0 表示不启用 keepalive
    pub keepalive_secs: u64,
    /// 秘密解析后端；None 时用 `sh0_core::DefaultResolver`
    pub resolver: Option<Arc<dyn SecretResolver>>,
}

impl std::fmt::Debug for ConnectOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectOptions")
            .field("known_hosts", &self.known_hosts)
            .field("keepalive_secs", &self.keepalive_secs)
            .field("resolver", &self.resolver.as_ref().map(|r| r.backend_name()))
            .finish()
    }
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            known_hosts: PathBuf::from("known_hosts"),
            keepalive_secs: 30,
            resolver: None,
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

        authenticate(&mut handle, host, opts.resolver.as_deref()).await?;

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

    /// 断开连接。`&self` 即可：russh 的 `Handle::disconnect` 内部走 channel 发送，
    /// 不需要可变借用。这样 SFTP 这类"自持连接"的对象能在 `&self` 方法里释放它。
    pub async fn close(&self) -> Result<()> {
        self.handle
            .disconnect(Disconnect::ByApplication, "", "English")
            .await
            .map_err(SshError::Connect)
    }

    /// 在本条连接上打开 SFTP 子系统（FR-4.1：复用 TCP，不新建连接）。
    ///
    /// 每个 channel 独立：SFTP 有自己的window/流控，与 shell channel 互不干扰，
    /// 所以可以放心地开完就关，不必长期持有。
    pub async fn open_sftp(&self) -> Result<russh_sftp::client::SftpSession> {
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(SshError::Connect)?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(SshError::Connect)?;
        russh_sftp::client::SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| SshError::Sftp(e.to_string()))
    }

    /// 独立 exec channel 执行一条命令（W2.4 备选路径，不申请 PTY）。
    ///
    /// 与 shell 哨兵模式的分工：
    /// - exec channel：stdout/stderr 天然分离、退出码精确，适合一次性非交互命令；
    /// - shell 哨兵：保留交互上下文（cwd、环境变量），适合连续操作。
    ///
    /// 超时到达时直接收线，已收到的输出照常返回（`exit_code` 为 None）。
    pub async fn exec_command(&self, cmd: &str, timeout: Duration) -> Result<ExecOutput> {
        let mut channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(SshError::Connect)?;
        channel
            .exec(true, cmd)
            .await
            .map_err(SshError::Connect)?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;
        let deadline = tokio::time::Instant::now() + timeout;
        // EOF 之后服务端通常**紧接着**补一个 exit-status。见 EOF 就退出会把退出码
        // 丢掉（真机实测：树莓派 OpenSSH 上 exit_code 恒为 null），而"命令成功还是
        // 失败"正是 AI 最需要的信息——所以 EOF 后再多等一小会儿收退出码。
        let mut eof_grace: Option<tokio::time::Instant> = None;

        loop {
            let now = tokio::time::Instant::now();
            let mut left = deadline.saturating_duration_since(now);
            if let Some(g) = eof_grace {
                left = left.min(g.saturating_duration_since(now));
            }
            if left.is_zero() {
                if eof_grace.is_none() {
                    tracing::warn!("exec channel 超时（{timeout:?}），命令可能仍在远端运行");
                }
                break;
            }
            match tokio::time::timeout(left, channel.wait()).await {
                Ok(Some(ChannelMsg::Data { data })) => stdout.extend_from_slice(&data),
                Ok(Some(ChannelMsg::ExtendedData { data, ext: 1 })) => {
                    stderr.extend_from_slice(&data)
                }
                Ok(Some(ChannelMsg::ExitStatus { exit_status })) => {
                    exit_code = Some(exit_status as i32);
                }
                Ok(Some(ChannelMsg::Eof)) => {
                    if eof_grace.is_none() {
                        eof_grace = Some(now + Duration::from_millis(500));
                    }
                }
                Ok(Some(ChannelMsg::Close)) | Ok(None) => break,
                Ok(Some(_)) => {}
                Err(_) => break, // 超时
            }
        }

        let _ = channel.eof().await;
        let _ = channel.close().await;

        Ok(ExecOutput {
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
            exit_code,
        })
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

async fn authenticate(
    session: &mut Handle<ClientHandler>,
    host: &Host,
    resolver: Option<&dyn SecretResolver>,
) -> Result<()> {
    match &host.auth {
        CredentialRef::Password { secret } => {
            let pw = resolve_secret(resolver, secret, &format!("{} 的密码", host.endpoint()))?;
            let res = session
                .authenticate_password(host.user.clone(), pw)
                .await
                .map_err(|e| SshError::Auth(e.to_string()))?;
            ensure_success(res)?;
        }
        CredentialRef::KeyFile { path, passphrase } => {
            let pp = match passphrase {
                Some(s) => Some(resolve_secret(
                    resolver,
                    s,
                    &format!("私钥 {} 的 passphrase", path.display()),
                )?),
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
/// 有注入 resolver（钥匙串 / vault）时优先走它；否则用 `DefaultResolver`
/// （环境变量 / stdin 提示）。注意 `Prompt` 直接从 stdin 读取，
/// 因此**不适用于 TUI raw mode 场景**——TUI 下请用钥匙串或 `Env`。
pub fn resolve_secret(
    resolver: Option<&dyn SecretResolver>,
    r: &SecretRef,
    prompt: &str,
) -> Result<String> {
    match resolver {
        Some(res) => res
            .resolve(r, prompt)
            .map_err(|e| SshError::Credential(e.to_string())),
        None => sh0_core::DefaultResolver
            .resolve(r, prompt)
            .map_err(|e| SshError::Credential(e.to_string())),
    }
}

/// 便于测试：确认私钥文件可被解析
pub fn probe_key(path: &Path, passphrase: Option<&str>) -> Result<()> {
    load_secret_key(path, passphrase)
        .map(|_| ())
        .map_err(|e| SshError::Credential(format!("{path:?}: {e}")))
}
