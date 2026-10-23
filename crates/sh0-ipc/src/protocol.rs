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
}

/// 事件名常量
pub mod events {
    pub const OUTPUT: &str = "output";
    pub const STATE: &str = "state";
    /// 重连成功，携带 attempt
    pub const RECONNECTED: &str = "reconnected";
    /// 会话结束，携带 reason
    pub const CLOSED: &str = "closed";
}
