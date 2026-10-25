//! daemon 与前端之间的协议帧定义。
//!
//! 采用 JSON-RPC 风格的方法分发（`method` + `params`），而不是为每种操作定义
//! 一个枚举变体 —— 这样未来加 MCP / HTTP adapter 时无需改动协议层（需求 FR-6.12）。
//!
//! 线路格式：JSON Lines（每条消息一行，以 '\n' 结尾）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 前端 → daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// 连接后的第一帧，携带本机令牌
    Auth { token: String },
    /// 方法调用
    Request {
        id: u64,
        method: String,
        params: Value,
    },
}

/// daemon → 前端
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// 方法调用的应答
    Response {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorBody>,
    },
    /// daemon 主动推送的事件（输出流、状态变更等）
    Event {
        event: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    /// 机器可读的错误码，与 `sh0 ai` 退出码表一一对应（见架构文档 §6.3）
    pub code: String,
    pub message: String,
    /// "失败了但仍有东西要交付"时挂在这里（架构文档 §6.3）：
    /// needs_confirmation 带确认令牌，remote_nonzero / timeout 带部分输出。
    /// AI 侧必须能拿到——否则确认流永远闭环不了。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ServerMsg {
    pub fn ok(id: u64, data: Value) -> Self {
        ServerMsg::Response {
            id,
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn err(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        ServerMsg::Response {
            id,
            ok: false,
            data: None,
            error: Some(ErrorBody {
                code: code.into(),
                message: message.into(),
                data: None,
            }),
        }
    }

    /// 带数据的错误应答：调用方失败了，但 data 里有关键交付物
    pub fn err_with_data(
        id: u64,
        code: impl Into<String>,
        message: impl Into<String>,
        data: Value,
    ) -> Self {
        ServerMsg::Response {
            id,
            ok: false,
            data: None,
            error: Some(ErrorBody {
                code: code.into(),
                message: message.into(),
                data: Some(data),
            }),
        }
    }

    pub fn event(event: impl Into<String>, session: Option<String>, data: Value) -> Self {
        ServerMsg::Event {
            event: event.into(),
            session,
            data: Some(data),
        }
    }
}

/// daemon 运行时信息：监听地址 + 令牌。写入 runtime 目录供前端发现。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub addr: String,
    pub token: String,
    pub started_at: String,
}

/// 方法名常量，避免前后端字符串漂移
pub mod methods {
    pub const PING: &str = "ping";
    pub const SHUTDOWN: &str = "shutdown";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_OPEN: &str = "session.open";
    pub const SESSION_CLOSE: &str = "session.close";
    pub const SESSION_STATUS: &str = "session.status";
    pub const SESSION_RECONNECT: &str = "session.reconnect";
    pub const SESSION_CLEAR: &str = "session.clear";
    /// M2 的 `screen read` 命令，M1 先落地实现（AI 读屏依赖它）
    pub const SCREEN_READ: &str = "screen.read";
    pub const INPUT_SEND: &str = "input.send";
    pub const RESIZE: &str = "session.resize";
    /// M2：命令执行（shell 哨兵 / exec channel 双模式）
    pub const EXEC_RUN: &str = "exec.run";
    /// M2：有人值守的确认应答（TUI 弹窗批准/拒绝）
    pub const EXEC_CONFIRM: &str = "exec.confirm";
    /// M2：审计查询
    pub const LOG_QUERY: &str = "log.query";

    // ---- M4 · SFTP（FR-4.x / FR-6.6）----
    pub const FILE_LIST: &str = "file.list";
    pub const FILE_STAT: &str = "file.stat";
    pub const FILE_READ: &str = "file.read";
    pub const FILE_WRITE: &str = "file.write";
    pub const FILE_MKDIR: &str = "file.mkdir";
    pub const FILE_REMOVE: &str = "file.remove";
    pub const FILE_RENAME: &str = "file.rename";
    pub const FILE_CHMOD: &str = "file.chmod";
    /// 分块读写：给传输队列的进度与断点续传用
    pub const FILE_READ_RANGE: &str = "file.read_range";
    pub const FILE_WRITE_AT: &str = "file.write_at";

    // ---- M3 · 批量与广播（FR-5.x）----
    /// 广播开关与参与会话的设定（FR-5.1）
    pub const BROADCAST_SET: &str = "broadcast.set";
    /// 广播态查询
    pub const BROADCAST_STATUS: &str = "broadcast.status";
    /// 向参与会话同时发送同一份输入（FR-5.1）
    pub const BROADCAST_SEND: &str = "broadcast.send";
    /// 批量执行：一组主机跑同一条命令（FR-5.2 / FR-5.4）
    pub const BATCH_RUN: &str = "batch.run";
    /// 批量历史回看（FR-5.3 / OQ-5）
    pub const BATCH_LIST: &str = "batch.list";
    pub const BATCH_SHOW: &str = "batch.show";
    /// 重新加载 config.toml / hosts.toml / risk.toml（FR-9.3 配置热重载）
    pub const CONFIG_RELOAD: &str = "config.reload";
    /// 回看 daemon 当前生效的配置（AI 用它确认"改的是不是生效的那份"）
    pub const CONFIG_GET: &str = "config.get";
}

/// 事件名常量
pub mod events {
    pub const OUTPUT: &str = "output";
    pub const STATE: &str = "state";
    /// 重连成功，携带 attempt
    pub const RECONNECTED: &str = "reconnected";
    /// 会话结束，携带 reason
    pub const CLOSED: &str = "closed";
    /// M2：有命令进入 confirm 档等待确认（TUI 弹窗用）。
    /// data: { token, session, host, command, risk, reason }
    pub const CONFIRM_REQUESTED: &str = "confirm.requested";
    /// M2：确认已处理。data: { token, approved, summary, audit_id }
    pub const CONFIRM_RESOLVED: &str = "confirm.resolved";
}
