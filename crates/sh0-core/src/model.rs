use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::{HostId, SessionId};

/// 命令风险档位。MVP 阶段仅定义类型，规则引擎在 M2 落地。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum RiskLevel {
    /// 安全命令，自动执行
    Auto,
    /// 需确认后执行
    #[default]
    Confirm,
    /// 直接拒绝
    Deny,
}


/// 密钥/口令的引用方式。
///
/// 设计约束：配置文件中**绝不出现明文口令**（需求 FR-1.7）。
/// 因此这里只存“去哪里取”，不存值本身。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretRef {
    /// 运行时向用户询问（默认，最安全）
    #[default]
    Prompt,
    /// 从环境变量读取，适合 CI / 无人值守场景
    Env(String),
    /// 系统钥匙串：macOS Keychain / Windows Credential Manager / Linux Secret Service。
    /// 计划于 M2 落地（需求 FR-1.6），M0 阶段返回 NotImplemented。
    Keyring { service: String, account: String },
}

/// 认证方式引用（需求 FR-1.4）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRef {
    /// 交给 ssh-agent（Unix）或 Pageant（Windows）
    #[default]
    Agent,
    /// 私钥文件，可选 passphrase
    KeyFile {
        path: PathBuf,
        #[serde(default)]
        passphrase: Option<SecretRef>,
    },
    /// 密码认证
    Password { secret: SecretRef },
    /// keyboard-interactive，含 2FA/OTP 交互应答
    Interactive,
    /// 不做认证（服务端允许时使用）
    None,
}

/// 一台远程主机的连接配置（需求 FR-1.1）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Host {
    pub id: HostId,
    pub name: String,
    /// 分组面包屑，如 ["prod", "web"]
    #[serde(default)]
    pub group: Vec<String>,
    pub addr: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub auth: CredentialRef,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// 该主机允许的最高风险档位（FR-7.6）。None = 不限制。
    /// 例如生产组设 `confirm`，则任何 auto 档命令照旧、但判定为 auto 的
    /// 危险命令会被压到 confirm 档——引擎仍可能给出更高档，由 daemon 取较小值。
    #[serde(default)]
    pub risk_cap: Option<RiskLevel>,
}

fn default_port() -> u16 {
    22
}

impl Host {
    /// 便于展示的 "user@addr:port" 形式
    pub fn endpoint(&self) -> String {
        format!("{}@{}:{}", self.user, self.addr, self.port)
    }

    /// 分组路径，如 "prod/web"
    pub fn group_path(&self) -> String {
        self.group.join("/")
    }
}

/// 批量操作的主机选择器（FR-5.2）。
///
/// 三种来源可叠加：显式列表 + 分组 + 标签，最终取并集去重；
/// `all` 一旦为真就直接覆盖其余条件——"对全部机器执行"是很危险的动作，
/// 必须让人显式写出来，而不是靠"条件都没给"隐式触发。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostSelector {
    /// 主机名称或 id
    #[serde(default)]
    pub hosts: Vec<String>,
    /// 分组前缀：`prod` 命中 prod 及其子分组（prod/web）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default)]
    pub all: bool,
}

impl HostSelector {
    /// 什么都没给：既不是"全部"，也没有具体目标
    pub fn is_empty(&self) -> bool {
        !self.all && self.hosts.is_empty() && self.group.is_none() && self.tag.is_none()
    }
}

/// 会话状态机（需求 FR-2.6）。M0 只用到 Connecting / Ready / Closed。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Connecting,
    Authenticating,
    Ready,
    Degraded,
    Reconnecting,
    Closed,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Connecting => "connecting",
            SessionState::Authenticating => "authenticating",
            SessionState::Ready => "ready",
            SessionState::Degraded => "degraded",
            SessionState::Reconnecting => "reconnecting",
            SessionState::Closed => "closed",
        }
    }
}

/// 会话快照，供 TUI 状态栏与 `ai session status` 使用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub host_id: HostId,
    pub host_name: String,
    pub state: SessionState,
    pub cols: u16,
    pub rows: u16,
    pub reconnect_count: u32,
    /// 最近一次连接/传输失败的原因。
    ///
    /// 断线重连期间调用方最想知道的就是"到底为什么连不上"，
    /// 只靠日志的话 AI 拿不到（它只能走 IPC）。因此把它放进快照。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}
