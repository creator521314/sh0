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
    /// 关闭会话
    Close {
        /// 会话 ID，或 --all
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        all: bool,
    },
}

/// 退出码（架构文档 §6.3 的语义化退出码表）
pub mod exit_code {
    pub const OK: i32 = 0;
    pub const INVALID_ARGS: i32 = 11;
    pub const IPC_UNREACHABLE: i32 = 10;
    pub const INTERNAL: i32 = 12;
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
                AiError::Ipc(_) => ("ipc_unreachable", exit_code::IPC_UNREACHABLE),
                AiError::Args(_) => ("invalid_args", exit_code::INVALID_ARGS),
                AiError::Other(_) => ("internal", exit_code::INTERNAL),
            };
            emit_err(json_mode, code, e.to_string());
            num
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("{0}")]
    Ipc(String),
    #[error("{0}")]
    Args(String),
    #[error("{0}")]
    Other(String),
}

impl From<sh0_ipc::IpcError> for AiError {
    fn from(e: sh0_ipc::IpcError) -> Self {
        AiError::Ipc(e.to_string())
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
        },
    }
}

fn require_client(client: Option<Arc<IpcClient>>) -> Result<Arc<IpcClient>, AiError> {
    client.ok_or_else(|| AiError::Ipc("daemon 未运行，请先执行 `sh0 daemon run`".into()))
}
