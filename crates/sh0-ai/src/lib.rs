//! AI -facing 子命令层：`sh0 ai <对象> <动作> [选项]`（需求 FR-6.1）
//!
//! 输出约定（需求 FR-6.2）：默认人类可读纯文本；加 `--json` 统一切换为结构化 JSON 信封。
//! M0 只落地骨架与 `host list` / `session list` / `ping`，命令执行与读屏在 M2 展开。

use std::sync::Arc;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use sh0_ipc::IpcClient;
use sh0_ipc::protocol::methods;

#[derive(Debug, Parser)]
#[command(name = "sh0-ai", about = "面向 AI 的结构化操作接口")]
pub struct AiArgs {
    #[command(subcommand)]
    pub object: AiObject,

    /// 输出结构化 JSON（AI 调用时建议始终带上）
    #[arg(long, global = true)]
    pub json: bool,

    /// 调用链追踪 ID，会写入审计日志
    #[arg(long, global = true)]
    pub trace_id: Option<String>,

    /// 超时（秒）
    #[arg(long, global = true, default_value_t = 30)]
    pub timeout: u64,
}

#[derive(Debug, Subcommand)]
pub enum AiObject {
    /// 主机清单查询
    Host {
        #[command(subcommand)]
        cmd: HostCmd,
    },
    /// 会话管理
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// 读取会话屏幕（AI 读屏，架构文档 §6.4）
    Screen {
        #[command(subcommand)]
        cmd: ScreenCmd,
    },
    /// 向会话注入按键
    Input {
        #[command(subcommand)]
        cmd: InputCmd,
    },
    /// 连通性探针
    Ping,
}

#[derive(Debug, Subcommand)]
pub enum HostCmd {
    /// 列出主机（不含任何凭据信息）
    List {
        /// 按分组过滤
        #[arg(long)]
        group: Option<String>,
        /// 按标签过滤
        #[arg(long)]
        tag: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SessionCmd {
    /// 列出活跃会话
    List,
    /// 打开会话
    Open {
        /// 主机名或主机 ID
        #[arg(long)]
        host: String,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
    },
    /// 关闭会话
    Close {
        /// 会话 ID，或 --all
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// 手动触发重连
    Reconnect {
        #[arg(long)]
        session: String,
    },
    /// 清空屏幕与滚动缓冲
    Clear {
        #[arg(long)]
        session: String,
    },
    /// 调整终端尺寸（会同步通知远端）
    Resize {
        #[arg(long)]
        session: String,
        #[arg(long)]
        cols: u16,
        #[arg(long)]
        rows: u16,
    },
    /// 查询单个会话状态。
    ///
    /// 会话已结束并从列表里摘掉时，仍可查到最后一份快照
    /// （含 `last_error`），用来回答"它为什么没了"。
    Status {
        #[arg(long)]
        session: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ScreenCmd {
    /// 读取屏幕内容。
    ///
    /// 默认 `plain` 模式（纯文本 + 滚动缓冲）。`--no-scrollback` 只取当前可见区。
    Read {
        #[arg(long)]
        session: String,
        /// 只返回当前可见区，不含历史
        #[arg(long)]
        no_scrollback: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum InputCmd {
    /// 发送原始字节（不做任何转义，直接投递给远端）
    Send {
        #[arg(long)]
        session: String,
        /// 要发送的内容；用 `--data` 传原始文本
        #[arg(long)]
        data: String,
        /// 末尾补一个换行（等价于敲回车）
        #[arg(long)]
        enter: bool,
    },
}

/// 退出码（架构文档 §6.3 的语义化退出码表）
pub mod exit_code {
    pub const OK: i32 = 0;
    pub const REMOTE_NONZERO: i32 = 1;
    pub const RISK_DENIED: i32 = 2;
    pub const NEEDS_CONFIRMATION: i32 = 3;
    pub const TIMEOUT: i32 = 4;
    pub const SESSION_UNAVAILABLE: i32 = 5;
    pub const AUTH_FAILED: i32 = 6;
    pub const AUTH_REQUIRED: i32 = 7;
    pub const HOST_NOT_FOUND: i32 = 8;
    pub const RATE_LIMITED: i32 = 9;
    pub const IPC_UNREACHABLE: i32 = 10;
    pub const INVALID_ARGS: i32 = 11;
    pub const INTERNAL: i32 = 12;
}

/// 架构文档 §6.3：把 daemon 返回的业务错误码映射成语义化退出码。
///
/// 映射错了比不映射更糟——AI 是靠退出码决定"重试/换方案/转人工"的。
/// 例如把 `session_unavailable` 报成 `ipc_unreachable`，AI 会去重启 daemon，
/// 而真正该做的是重新打开会话。
pub fn code_to_exit(code: &str) -> (String, i32) {
    match code {
        "remote_nonzero" => (code.to_string(), exit_code::REMOTE_NONZERO),
        "risk_denied" => (code.to_string(), exit_code::RISK_DENIED),
        "needs_confirmation" => (code.to_string(), exit_code::NEEDS_CONFIRMATION),
        "timeout" => (code.to_string(), exit_code::TIMEOUT),
        "session_unavailable" => (code.to_string(), exit_code::SESSION_UNAVAILABLE),
        "auth_failed" => (code.to_string(), exit_code::AUTH_FAILED),
        "auth_required" => (code.to_string(), exit_code::AUTH_REQUIRED),
        "host_not_found" => (code.to_string(), exit_code::HOST_NOT_FOUND),
        "rate_limited" => (code.to_string(), exit_code::RATE_LIMITED),
        "invalid_args" => (code.to_string(), exit_code::INVALID_ARGS),
        // 未知的业务码按内部错误处理，但保留原码字样便于排查
        other => (other.to_string(), exit_code::INTERNAL),
    }
}

/// 统一输出：纯文本或 JSON 信封
fn emit(json_mode: bool, text: String, data: Value) {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(&json!({
            "ok": true,
            "data": data,
        })).unwrap_or_else(|_| "{}".to_string()));
    } else {
        print!("{text}");
    }
}

fn emit_err(json_mode: bool, code: &str, message: String) {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(&json!({
            "ok": false,
            "error": { "code": code, "message": message },
        })).unwrap_or_else(|_| "{}".to_string()));
    } else {
        eprintln!("错误[{code}]: {message}");
    }
}

/// 执行一条 ai 子命令，返回进程退出码
pub async fn run(args: AiArgs, client: Option<Arc<IpcClient>>) -> i32 {
    let json_mode = args.json;
    if let Some(tid) = &args.trace_id {
        tracing::info!("ai 调用 trace_id={tid}");
    }

    let result = dispatch(&args, client).await;

    match result {
        Ok((text, data)) => {
            emit(json_mode, text, data);
            exit_code::OK
        }
        Err(e) => {
            let (code, num) = match &e {
                AiError::Ipc(_) => (
                    "ipc_unreachable".to_string(),
                    exit_code::IPC_UNREACHABLE,
                ),
                AiError::Remote { code, .. } => code_to_exit(code),
                AiError::Args(_) => ("invalid_args".to_string(), exit_code::INVALID_ARGS),
                AiError::Other(_) => ("internal".to_string(), exit_code::INTERNAL),
            };
            emit_err(json_mode, &code, e.to_string());
            num
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// 传输层问题：daemon 没起来、令牌不对、连接断了
    #[error("{0}")]
    Ipc(String),
    /// daemon 正常应答但业务上失败（带它给出的错误码）
    #[error("{message}")]
    Remote { code: String, message: String },
    #[error("{0}")]
    Args(String),
    #[error("{0}")]
    Other(String),
}

impl From<sh0_ipc::IpcError> for AiError {
    fn from(e: sh0_ipc::IpcError) -> Self {
        match e {
            // daemon 明确回了一个业务错误码——它是"正常应答但失败了"，
            // 不是"连不上 daemon"，必须分开，退出码语义完全不同
            sh0_ipc::IpcError::Remote { code, message } => AiError::Remote { code, message },
            other => AiError::Ipc(other.to_string()),
        }
    }
}

async fn dispatch(args: &AiArgs, client: Option<Arc<IpcClient>>) -> Result<(String, Value), AiError> {
    match &args.object {
        AiObject::Ping => {
            let c = require_client(client)?;
            let data = c.call(methods::PING, json!({})).await?;
            Ok((format!("daemon 可达: {data}\n"), data))
        }
        AiObject::Host { cmd } => match cmd {
            HostCmd::List { group, tag } => {
                let store = sh0_store::Store::new()
                    .map_err(|e| AiError::Other(format!("无法读取配置: {e}")))?;
                let hosts_file = store
                    .load_hosts()
                    .map_err(|e| AiError::Other(format!("无法读取主机清单: {e}")))?;

                let mut hosts: Vec<Value> = hosts_file
                    .hosts
                    .iter()
                    .filter(|h| group.as_ref().is_none_or(|g| h.group.contains(g)))
                    .filter(|h| tag.as_ref().is_none_or(|t| h.tags.contains(t)))
                    .map(|h| {
                        json!({
                            "id": h.id,
                            "name": h.name,
                            "group": h.group,
                            "endpoint": h.endpoint(),
                            "tags": h.tags,
                        })
                    })
                    .collect();
                let _ = &mut hosts;

                let mut text = String::new();
                for h in &hosts_file.hosts {
                    if group.as_ref().is_some_and(|g| !h.group.contains(g)) {
                        continue;
                    }
                    if tag.as_ref().is_some_and(|t| !h.tags.contains(t)) {
                        continue;
                    }
                    text.push_str(&format!("{:<16} {:<28} {}\n", h.name, h.endpoint(), h.group_path()));
                }
                if text.is_empty() {
                    text = "（无匹配主机，可用编辑器编辑 hosts.toml 添加）\n".to_string();
                }

                Ok((text, json!({ "hosts": hosts })))
            }
        },
        AiObject::Session { cmd } => match cmd {
            SessionCmd::List => {
                let c = require_client(client)?;
                let data = c.call(methods::SESSION_LIST, json!({})).await?;
                let sessions = data.get("sessions").cloned().unwrap_or(json!([]));
                let mut text = String::new();
                if let Some(list) = sessions.as_array() {
                    if list.is_empty() {
                        text = "（暂无活跃会话）\n".to_string();
                    }
                    for s in list {
                        text.push_str(&format!(
                            "{:<28} {:<16} {:<12} {}x{}\n",
                            s.get("id").and_then(|v| v.as_str()).unwrap_or("-"),
                            s.get("host_name").and_then(|v| v.as_str()).unwrap_or("-"),
                            s.get("state").and_then(|v| v.as_str()).unwrap_or("-"),
                            s.get("cols").and_then(|v| v.as_u64()).unwrap_or(0),
                            s.get("rows").and_then(|v| v.as_u64()).unwrap_or(0),
                        ));
                    }
                }
                Ok((text, json!({ "sessions": sessions })))
            }
            SessionCmd::Close { session, all } => {
                let c = require_client(client)?;
                let sid = match (session, all) {
                    (Some(s), _) => s.clone(),
                    (None, true) => "*".to_string(),
                    (None, false) => {
                        return Err(AiError::Args("需要 --session <id> 或 --all".into()));
                    }
                };
                let data = c
                    .call(methods::SESSION_CLOSE, json!({ "session": sid }))
                    .await?;
                Ok((format!("已关闭 {sid}\n"), data))
            }
            SessionCmd::Open { host, cols, rows } => {
                let c = require_client(client)?;
                let data = c
                    .call(
                        methods::SESSION_OPEN,
                        json!({
                            "host": host,
                            "cols": cols.unwrap_or(120),
                            "rows": rows.unwrap_or(32),
                        }),
                    )
                    .await?;
                let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("-");
                Ok((format!("已打开会话 {id}\n"), data))
            }
            SessionCmd::Reconnect { session } => {
                let c = require_client(client)?;
                let data = c
                    .call(methods::SESSION_RECONNECT, json!({ "session": session }))
                    .await?;
                Ok((format!("正在重连 {session}\n"), data))
            }
            SessionCmd::Clear { session } => {
                let c = require_client(client)?;
                let data = c
                    .call(methods::SESSION_CLEAR, json!({ "session": session }))
                    .await?;
                Ok((format!("已清空 {session}\n"), data))
            }
            SessionCmd::Resize { session, cols, rows } => {
                let c = require_client(client)?;
                let data = c
                    .call(methods::RESIZE, json!({ "session": session, "cols": cols, "rows": rows }))
                    .await?;
                Ok((format!("{session} 尺寸改为 {cols}x{rows}\n"), data))
            }
            SessionCmd::Status { session } => {
                let c = require_client(client)?;
                let data = c
                    .call(methods::SESSION_STATUS, json!({ "session": session }))
                    .await?;
                let state = data.get("state").and_then(|v| v.as_str()).unwrap_or("-");
                let host = data.get("host_name").and_then(|v| v.as_str()).unwrap_or("-");
                let mut text = format!("{host}  [{state}]\n");
                if let Some(e) = data.get("last_error").and_then(|v| v.as_str()) {
                    text.push_str(&format!("最近错误: {e}\n"));
                }
                Ok((text, data))
            }
        },
        AiObject::Input { cmd } => match cmd {
            InputCmd::Send { session, data, enter } => {
                let c = require_client(client)?;
                let payload = if *enter {
                    format!("{data}\n")
                } else {
                    data.clone()
                };
                let out = c
                    .call(methods::INPUT_SEND, json!({ "session": session, "data": payload }))
                    .await?;
                Ok((String::new(), out))
            }
        },
        AiObject::Screen { cmd } => match cmd {
            ScreenCmd::Read { session, no_scrollback } => {
                let c = require_client(client)?;
                let with_scrollback = !no_scrollback;
                let data = c
                    .call(
                        methods::SCREEN_READ,
                        json!({ "session": session, "scrollback": with_scrollback }),
                    )
                    .await?;
                let text = data.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                Ok((format!("{text}\n"), data))
            }
        },
    }
}

fn require_client(client: Option<Arc<IpcClient>>) -> Result<Arc<IpcClient>, AiError> {
    client.ok_or_else(|| AiError::Ipc("daemon 未运行，请先执行 `sh0 daemon run`".into()))
}
