//! AI-facing 子命令层：`sh0 ai <对象> <动作> [选项]`（需求 FR-6.1）
//!
//! 输出约定（需求 FR-6.2）：默认人类可读纯文本；加 `--json` 统一切换为结构化 JSON 信封。
//! 信封三形态（架构文档 §6.2）：
//! - 成功：`{ok: true, trace_id, data}`
//! - 失败：`{ok: false, trace_id, error: {code, message}}`
//! - 失败但带数据（远端退出码非零 / 超时）：`error` 之外仍带 `data`
//!   ——AI 必须能看到输出才能判断"命令失败了但错误信息是什么"。

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

    /// 超时（秒，exec 用）
    #[arg(long, global = true, default_value_t = 30)]
    pub timeout: u64,

    /// 无人值守场景下显式批准 confirm 档命令（等价于用户点了"确认"）
    #[arg(long, global = true)]
    pub yes: bool,

    /// 只做风险判定不执行（AI 自检：这条命令会被放行吗）
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// 安静模式：不打印人类可读文本（`--json` 仍会输出信封）
    #[arg(long, global = true)]
    pub quiet: bool,
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
    /// 命令执行（FR-6.4 / FR-6.8）
    Exec {
        #[command(subcommand)]
        cmd: ExecCmd,
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
    /// 广播：把同一份输入同时发给多个会话（M3 / FR-5.1）
    Broadcast {
        #[command(subcommand)]
        cmd: BroadcastCmd,
    },
    /// 批量执行：一组主机跑同一条命令（M3 / FR-5.2）
    Batch {
        #[command(subcommand)]
        cmd: BatchCmd,
    },
    /// 审计查询（FR-8.4）
    Log {
        #[command(subcommand)]
        cmd: LogCmd,
    },
    /// 远端文件操作（M4 / FR-6.6）
    File {
        #[command(subcommand)]
        cmd: FileCmd,
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
    /// 单台主机详情（同样不含凭据）
    Show {
        /// 主机名或主机 ID
        selector: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ExecCmd {
    /// 执行命令并取回输出
    ///
    /// `--session S` 在既有会话上执行；`--host X` 一键式调用
    /// （自动建连 → 执行 → 关闭，FR-6.8）。
    Run {
        /// 会话 ID（与 --host 二选一）
        #[arg(long)]
        session: Option<String>,
        /// 主机名或 ID（与 --session 二选一）
        #[arg(long)]
        host: Option<String>,
        /// 要执行的命令
        #[arg(long)]
        cmd: String,
        /// 执行模式：shell（默认，保留交互上下文）/ exec（独立 channel，退出码更精确）
        #[arg(long, default_value = "shell")]
        mode: String,
        /// confirm 档的二次确认令牌（首次调用返回 needs_confirmation 时带上它重试）
        #[arg(long)]
        confirm_token: Option<String>,
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
    /// `--mode plain`（默认）：纯文本 + 滚动缓冲。
    /// `--mode grid`：二维数组 + 光标位置（架构文档 §6.2）。
    Read {
        #[arg(long)]
        session: String,
        /// 只取最后 N 行
        #[arg(long)]
        lines: Option<usize>,
        /// plain / grid
        #[arg(long, default_value = "plain")]
        mode: String,
        /// 只返回当前可见区，不含历史（plain 模式）
        #[arg(long)]
        no_scrollback: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum InputCmd {
    /// 注入输入。三种形态二选一/三选一：
    /// `--text` 原始文本；`--keys` 键名序列（如 `:q!<Enter>`）；
    /// `--hex` 十六进制字节（如 `1b5b41`）。
    Send {
        #[arg(long)]
        session: String,
        /// 原始文本
        #[arg(long)]
        text: Option<String>,
        /// 键名序列，支持 `<Enter>` `<Esc>` `<Tab>` `<BS>` `<Up>` `<C-c>` 等占位符
        #[arg(long)]
        keys: Option<String>,
        /// 十六进制字节串
        #[arg(long)]
        hex: Option<String>,
        /// 末尾补一个换行（等价于敲回车，仅对 --text 有意义）
        #[arg(long)]
        enter: bool,
    },
}

/// 批量执行（FR-5.2 / FR-5.3）：一组主机、一条命令。
///
/// 目标三选一并可叠加：显式 `--host`（可重复或逗号分隔）、`--group`、
/// `--tag`；`--all` 必须显式写出——"对全部机器执行"不该由"条件都没给"隐式触发。
#[derive(Debug, Subcommand)]
pub enum BatchCmd {
    /// 在一组主机上执行同一条命令
    Exec {
        /// 主机名或 id，可重复传入，也可逗号分隔
        #[arg(long = "host")]
        hosts: Vec<String>,
        /// 分组前缀，如 `prod`（命中 prod 及其子分组）
        #[arg(long)]
        group: Option<String>,
        /// 标签
        #[arg(long)]
        tag: Option<String>,
        /// 全部主机
        #[arg(long)]
        all: bool,
        /// 要执行的命令
        #[arg(long)]
        cmd: String,
        /// shell（哨兵截取，保留交互上下文）/ exec（独立 channel，默认）
        #[arg(long, default_value = "exec")]
        mode: String,
        /// 并发上限，默认取配置（16）
        #[arg(long)]
        concurrency: Option<usize>,
        /// 有失败就尽快停下，剩下的标记 skipped
        #[arg(long)]
        fail_fast: bool,
        /// 汇总格式：table（人读）/ json / csv
        #[arg(long, default_value = "table")]
        out: String,
        /// 导出到文件（配合 --out json|csv），不写则只打印
        #[arg(long)]
        output: Option<std::path::PathBuf>,
    },
    /// 最近 N 次批量执行（OQ-5）
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 查看一次批量执行的逐台明细
    Show {
        #[arg(long)]
        run_id: String,
        #[arg(long, default_value = "table")]
        out: String,
    },
}

/// 广播（FR-5.1）：一份输入、多个会话。
///
/// 会话名单存在 daemon 里（`broadcast.set`），因此开一次广播后
/// 后续 `send` 不用再带名单；`--session` 可重复传入做一次性指定。
#[derive(Debug, Subcommand)]
pub enum BroadcastCmd {
    /// 打开广播；不给 --session 表示全部已打开会话
    Enable {
        /// 参与广播的会话 id，可重复传入
        #[arg(long = "session")]
        sessions: Vec<String>,
    },
    /// 关闭广播（名单保留，下次 enable 沿用）
    Disable,
    /// 查看当前广播态
    Status,
    /// 把同一份输入发给参与会话
    Send {
        /// 原始文本
        #[arg(long)]
        text: Option<String>,
        /// 键名序列，支持 `<Enter>` `<Esc>` `<Tab>` `<BS>` `<Up>` `<C-c>` 等占位符
        #[arg(long)]
        keys: Option<String>,
        /// 十六进制字节串
        #[arg(long)]
        hex: Option<String>,
        /// 末尾补一个换行（等价于敲回车，仅对 --text 有意义）
        #[arg(long)]
        enter: bool,
        /// 一次性指定目标会话；不给则按当前广播名单
        #[arg(long = "session")]
        sessions: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum LogCmd {
    /// 查询审计记录（FR-8.4）
    Query {
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        session: Option<String>,
        /// RFC3339 或日期，如 2026-10-20
        #[arg(long)]
        since: Option<String>,
        /// ai / tui / human
        #[arg(long)]
        actor: Option<String>,
        /// auto / confirm / deny
        #[arg(long)]
        risk: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

#[derive(Debug, Subcommand)]
pub enum FileCmd {
    /// 列目录（FR-6.6）
    List {
        /// 会话 ID（与 --host 二选一；给会话时复用其连接）
        #[arg(long)]
        session: Option<String>,
        /// 主机名或 ID（一键式：自建自用连接）
        #[arg(long)]
        host: Option<String>,
        /// 远端路径
        #[arg(long)]
        path: String,
    },
    /// 单个文件/目录的元信息
    Stat {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        path: String,
    },
    /// 读文件内容（默认上限 1 MB，防止拉爆内存）
    Read {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        path: String,
        #[arg(long, default_value_t = 1024 * 1024)]
        max_bytes: usize,
    },
    /// 写文件（整体覆盖）
    Write {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        path: String,
        /// 文件内容；不给时从 stdin 读
        #[arg(long)]
        data: Option<String>,
    },
    /// 新建目录
    Mkdir {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        path: String,
    },
    /// 删除文件或目录（目录需加 --dir）
    Remove {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        path: String,
        #[arg(long)]
        dir: bool,
    },
    /// 重命名 / 移动
    Rename {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
    /// 上传本地文件到远端（分块，二进制安全；FR-6.6）
    Upload {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        host: Option<String>,
        /// 本地文件路径
        #[arg(long)]
        local: String,
        /// 远端目标路径
        #[arg(long)]
        remote: String,
    },
    /// 下载远端文件到本地（分块；FR-6.6）
    Download {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        host: Option<String>,
        /// 远端文件路径
        #[arg(long)]
        remote: String,
        /// 本地目标路径
        #[arg(long)]
        local: String,
    },
    /// 改权限（八进制，如 644）
    Chmod {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        path: String,
        /// 权限位，八进制字符串（如 "644"）或 0o644
        mode: String,
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
        // 批量里有一部分机器失败：与"远端退出码非零"同档——
        // AI 的应对都是"看 data 里的明细"，退出码不该再发明一个数字
        "batch_partial" => (code.to_string(), exit_code::REMOTE_NONZERO),
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

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// 传输层问题：daemon 没起来、令牌不对、连接断了
    #[error("{0}")]
    Ipc(String),
    /// daemon 正常应答但业务上失败（带它给出的错误码）。
    /// `data` 非空表示"失败了但仍有输出要看"（remote_nonzero / timeout）——
    /// 这正是架构文档 §6.3 里"stdout/stderr 仍在 data 中"的约定。
    #[error("{message}")]
    Remote {
        code: String,
        message: String,
        data: Option<Value>,
    },
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
            sh0_ipc::IpcError::Remote { code, message, data } => AiError::Remote {
                code,
                message,
                data,
            },
            other => AiError::Ipc(other.to_string()),
        }
    }
}

/// 统一输出：纯文本或 JSON 信封。
///
/// `data` 为 None 时（部分失败路径）不进信封。
fn emit(ctx: &RunCtx, text: String, data: Value) {
    if ctx.json {
        let mut env = json!({ "ok": true, "trace_id": ctx.trace_id, "data": data });
        if ctx.trace_id.is_none() {
            env.as_object_mut().unwrap().remove("trace_id");
        }
        println!("{}", serde_json::to_string_pretty(&env).unwrap_or_else(|_| "{}".into()));
    } else if !ctx.quiet {
        print!("{text}");
    }
}

fn emit_err(ctx: &RunCtx, code: &str, message: String, data: Option<Value>) {
    if ctx.json {
        let mut env = json!({
            "ok": false,
            "trace_id": ctx.trace_id,
            "error": { "code": code, "message": message },
        });
        if ctx.trace_id.is_none() {
            env.as_object_mut().unwrap().remove("trace_id");
        }
        if let Some(d) = data {
            env.as_object_mut().unwrap().insert("data".into(), d);
        }
        println!("{}", serde_json::to_string_pretty(&env).unwrap_or_else(|_| "{}".into()));
    } else if !ctx.quiet {
        eprintln!("错误[{code}]: {message}");
    }
}

/// 一次调用的上下文（全局选项）
struct RunCtx {
    json: bool,
    quiet: bool,
    trace_id: Option<String>,
    timeout_secs: u64,
    yes: bool,
    dry_run: bool,
}

impl RunCtx {
    fn from_args(args: &AiArgs) -> Self {
        Self {
            json: args.json,
            quiet: args.quiet,
            trace_id: args.trace_id.clone(),
            timeout_secs: args.timeout,
            yes: args.yes,
            dry_run: args.dry_run,
        }
    }

    /// 把 trace_id / actor 塞进 IPC 参数（FR-6.9：AI 动作全部带 trace_id 落审计）
    fn trace(&self, params: &mut Value) {
        if let Some(t) = &self.trace_id {
            params["trace_id"] = json!(t);
        }
        params["actor"] = json!("ai");
    }
}

/// 执行一条 ai 子命令，返回进程退出码
pub async fn run(args: AiArgs, client: Option<Arc<IpcClient>>) -> i32 {
    let ctx = RunCtx::from_args(&args);
    if let Some(tid) = &ctx.trace_id {
        tracing::info!("ai 调用 trace_id={tid}");
    }

    let result = dispatch(&ctx, &args.object, client).await;

    match result {
        Ok((text, data)) => {
            emit(&ctx, text, data);
            exit_code::OK
        }
        Err(e) => {
            let (code, num) = match &e {
                AiError::Ipc(_) => ("ipc_unreachable".to_string(), exit_code::IPC_UNREACHABLE),
                AiError::Remote { code, .. } => code_to_exit(code),
                AiError::Args(_) => ("invalid_args".to_string(), exit_code::INVALID_ARGS),
                AiError::Other(_) => ("internal".to_string(), exit_code::INTERNAL),
            };
            let data = match &e {
                AiError::Remote { data, .. } => data.clone(),
                _ => None,
            };
            emit_err(&ctx, &code, e.to_string(), data);
            num
        }
    }
}

async fn dispatch(
    ctx: &RunCtx,
    object: &AiObject,
    client: Option<Arc<IpcClient>>,
) -> Result<(String, Value), AiError> {
    match object {
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

                let matched: Vec<&sh0_core::Host> = hosts_file
                    .hosts
                    .iter()
                    .filter(|h| group.as_ref().is_none_or(|g| h.group.contains(g)))
                    .filter(|h| tag.as_ref().is_none_or(|t| h.tags.contains(t)))
                    .collect();
                let hosts: Vec<Value> = matched
                    .iter()
                    .map(|h| {
                        json!({
                            "id": h.id,
                            "name": h.name,
                            "group": h.group,
                            "endpoint": h.endpoint(),
                            "tags": h.tags,
                            "risk_cap": h.risk_cap,
                        })
                    })
                    .collect();

                let mut text = String::new();
                for h in &matched {
                    text.push_str(&format!(
                        "{:<16} {:<28} {}\n",
                        h.name,
                        h.endpoint(),
                        h.group_path()
                    ));
                }
                if text.is_empty() {
                    text = "（无匹配主机，可用编辑器编辑 hosts.toml 添加）\n".to_string();
                }

                Ok((text, json!({ "hosts": hosts })))
            }
            HostCmd::Show { selector } => {
                let store = sh0_store::Store::new()
                    .map_err(|e| AiError::Other(format!("无法读取配置: {e}")))?;
                let h = store.find_host(selector).map_err(|e| AiError::Remote {
                    code: "host_not_found".into(),
                    message: e.to_string(),
                    data: None,
                })?;
                let auth_kind = match &h.auth {
                    sh0_core::CredentialRef::Agent => "agent",
                    sh0_core::CredentialRef::KeyFile { .. } => "keyfile",
                    sh0_core::CredentialRef::Password { .. } => "password",
                    sh0_core::CredentialRef::Interactive => "interactive",
                    sh0_core::CredentialRef::None => "none",
                };
                // 只暴露认证"种类"，绝不暴露任何秘密值（FR-1.7）
                let data = json!({
                    "id": h.id,
                    "name": h.name,
                    "group": h.group,
                    "addr": h.addr,
                    "port": h.port,
                    "user": h.user,
                    "auth": auth_kind,
                    "tags": h.tags,
                    "note": h.note,
                    "risk_cap": h.risk_cap,
                });
                let text = format!(
                    "{}  {}\n  分组: {}  标签: {}  认证: {}  风险上限: {}\n",
                    h.name,
                    h.endpoint(),
                    if h.group.is_empty() { "（无）".into() } else { h.group_path() },
                    if h.tags.is_empty() { "（无）".into() } else { h.tags.join(",") },
                    auth_kind,
                    h.risk_cap
                        .map(|r| format!("{r:?}").to_lowercase())
                        .unwrap_or_else(|| "无".into()),
                );
                Ok((text, data))
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
                let mut params = json!({ "session": sid });
                ctx.trace(&mut params);
                let data = c.call(methods::SESSION_CLOSE, params).await?;
                Ok((format!("已关闭 {sid}\n"), data))
            }
            SessionCmd::Open { host, cols, rows } => {
                let c = require_client(client)?;
                let mut params = json!({
                    "host": host,
                    "cols": cols.unwrap_or(120),
                    "rows": rows.unwrap_or(32),
                });
                ctx.trace(&mut params);
                let data = c.call(methods::SESSION_OPEN, params).await?;
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
                    .call(
                        methods::RESIZE,
                        json!({ "session": session, "cols": cols, "rows": rows }),
                    )
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
        AiObject::Exec { cmd } => match cmd {
            ExecCmd::Run {
                session,
                host,
                cmd,
                mode,
                confirm_token,
            } => {
                if session.is_none() && host.is_none() {
                    return Err(AiError::Args(
                        "需要 --session <id> 或 --host <选择器>".into(),
                    ));
                }
                if !matches!(mode.as_str(), "shell" | "exec") {
                    return Err(AiError::Args(format!(
                        "未知 mode: {mode}（可选 shell / exec）"
                    )));
                }
                let c = require_client(client)?;
                let mut params = json!({
                    "session": session,
                    "host": host,
                    "cmd": cmd,
                    "mode": mode,
                    "timeout_ms": ctx.timeout_secs * 1000,
                    "dry_run": ctx.dry_run,
                    "yes": ctx.yes,
                    "confirm_token": confirm_token,
                });
                ctx.trace(&mut params);

                let data = c.call(methods::EXEC_RUN, params).await?;

                // dry-run：只回判定结果
                if ctx.dry_run {
                    let risk = data.get("risk").and_then(|v| v.as_str()).unwrap_or("?");
                    let text = format!(
                        "{}  风险评估: {risk}{}\n",
                        cmd,
                        if risk == "auto" { "（会被放行）" } else { "（会被拦截，需确认或拒绝）" }
                    );
                    return Ok((text, data));
                }

                // 超时：退出码 4，但数据照常返回（AI 要看部分输出）
                if data.get("timeout").and_then(|v| v.as_bool()).unwrap_or(false) {
                    return Err(AiError::Remote {
                        code: "timeout".into(),
                        message: "执行超时，data 中为已收到的部分输出".into(),
                        data: Some(data),
                    });
                }

                // 远端退出码非零：退出码 1，stdout/stderr 仍在 data（§6.3）
                let exit_code = data.get("exit_code").and_then(|v| v.as_i64());
                if let Some(code) = exit_code
                    && code != 0
                {
                    return Err(AiError::Remote {
                        code: "remote_nonzero".into(),
                        message: format!("远端退出码 {code}"),
                        data: Some(data),
                    });
                }

                let text = exec_text(&data, None);
                Ok((text, data))
            }
        },
        AiObject::Input { cmd } => match cmd {
            InputCmd::Send {
                session,
                text,
                keys,
                hex,
                enter,
            } => {
                let payload = build_input_payload(text.as_deref(), keys.as_deref(), hex.as_deref(), *enter)?;
                let c = require_client(client)?;
                let mut params = json!({ "session": session, "data": payload });
                ctx.trace(&mut params);
                let out = c.call(methods::INPUT_SEND, params).await?;
                Ok((String::new(), out))
            }
        },
        AiObject::Broadcast { cmd } => match cmd {
            BroadcastCmd::Enable { sessions } => {
                let c = require_client(client)?;
                let mut params = json!({ "enabled": true });
                if !sessions.is_empty() {
                    params["sessions"] = json!(sessions);
                }
                ctx.trace(&mut params);
                let data = c.call(methods::BROADCAST_SET, params).await?;
                Ok((
                    format!("广播已开启：{}\n", broadcast_status_text(&data)),
                    data,
                ))
            }
            BroadcastCmd::Disable => {
                let c = require_client(client)?;
                let mut params = json!({ "enabled": false });
                ctx.trace(&mut params);
                let data = c.call(methods::BROADCAST_SET, params).await?;
                Ok(("广播已关闭\n".into(), data))
            }
            BroadcastCmd::Status => {
                let c = require_client(client)?;
                let data = c.call(methods::BROADCAST_STATUS, json!({})).await?;
                Ok((format!("{}\n", broadcast_status_text(&data)), data))
            }
            BroadcastCmd::Send {
                text,
                keys,
                hex,
                enter,
                sessions,
            } => {
                let payload =
                    build_input_payload(text.as_deref(), keys.as_deref(), hex.as_deref(), *enter)?;
                let c = require_client(client)?;
                let mut params = json!({ "data": payload });
                if !sessions.is_empty() {
                    params["sessions"] = json!(sessions);
                }
                ctx.trace(&mut params);
                let data = c.call(methods::BROADCAST_SEND, params).await?;
                let n = data.get("sent").and_then(|v| v.as_u64()).unwrap_or(0);
                let mut out = format!("已广播到 {n} 个会话\n");
                if let Some(failed) = data.get("failed").and_then(|v| v.as_array())
                    && !failed.is_empty()
                {
                    out.push_str(&format!("失败 {} 个：\n", failed.len()));
                    for f in failed {
                        let sid = f.get("session").and_then(|v| v.as_str()).unwrap_or("-");
                        let err = f.get("error").and_then(|v| v.as_str()).unwrap_or("-");
                        out.push_str(&format!("  {sid}: {err}\n"));
                    }
                }
                Ok((out, data))
            }
        },
        AiObject::Batch { cmd } => match cmd {
            BatchCmd::Exec {
                hosts,
                group,
                tag,
                all,
                cmd,
                mode,
                concurrency,
                fail_fast,
                out,
                output,
            } => {
                if !matches!(mode.as_str(), "shell" | "exec") {
                    return Err(AiError::Args(format!(
                        "未知 mode: {mode}（可选 shell / exec）"
                    )));
                }
                if !matches!(out.as_str(), "table" | "json" | "csv") {
                    return Err(AiError::Args(format!(
                        "未知 out: {out}（可选 table / json / csv）"
                    )));
                }
                let c = require_client(client)?;
                let mut params = json!({
                    "hosts": hosts,
                    "group": group,
                    "tag": tag,
                    "all": all,
                    "cmd": cmd,
                    "mode": mode,
                    "fail_fast": fail_fast,
                    "timeout_ms": ctx.timeout_secs * 1000,
                    "dry_run": ctx.dry_run,
                    "yes": ctx.yes,
                });
                if let Some(n) = concurrency {
                    params["concurrency"] = json!(n);
                }
                ctx.trace(&mut params);
                let data = c.call(methods::BATCH_RUN, params).await?;

                if ctx.dry_run {
                    let risk = data.get("risk").and_then(|v| v.as_str()).unwrap_or("?");
                    let total = data.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
                    let hosts: Vec<&str> = data
                        .get("hosts")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                        .unwrap_or_default();
                    return Ok((
                        format!(
                            "{cmd}  ·  {total} 台  ·  风险评估: {risk}{}\n目标: {}\n",
                            if risk == "auto" {
                                "（会被放行）"
                            } else {
                                "（会被拦截，需确认或拒绝）"
                            },
                            hosts.join(", ")
                        ),
                        data,
                    ));
                }

                // 导出：--out json/csv 时把整份 data 或表格落到文件
                if let Some(path) = output {
                    let text = match out.as_str() {
                        "csv" => batch_csv(&data),
                        _ => serde_json::to_string_pretty(&data).unwrap_or_default(),
                    };
                    std::fs::write(path, text).map_err(|e| {
                        AiError::Other(format!("写导出文件失败 {}: {e}", path.display()))
                    })?;
                    if !ctx.quiet {
                        eprintln!("已导出到 {}", path.display());
                    }
                }

                let text = batch_text(&data, out);
                let failed = data.get("failed").and_then(|v| v.as_u64()).unwrap_or(0);
                if failed > 0 {
                    // 部分失败：退出码非零，但整份结果仍在 data 里（§6.3）
                    return Err(AiError::Remote {
                        code: "batch_partial".into(),
                        message: format!(
                            "{} 台未成功（共 {} 台），明细见 data.results",
                            failed,
                            data.get("total").and_then(|v| v.as_u64()).unwrap_or(0)
                        ),
                        data: Some(data),
                    });
                }
                Ok((text, data))
            }
            BatchCmd::List { limit } => {
                let c = require_client(client)?;
                let data = c
                    .call(methods::BATCH_LIST, json!({ "limit": limit }))
                    .await?;
                let runs = data.get("runs").and_then(|v| v.as_array());
                let mut text = String::new();
                let empty: Vec<Value> = Vec::new();
                for r in runs.unwrap_or(&empty) {
                    let id = r.get("run_id").and_then(|v| v.as_str()).unwrap_or("-");
                    let ts = r.get("ts").and_then(|v| v.as_str()).unwrap_or("-");
                    let cmd = r.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    let total = r.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
                    let ok = r.get("ok").and_then(|v| v.as_i64()).unwrap_or(0);
                    let failed = r.get("failed").and_then(|v| v.as_i64()).unwrap_or(0);
                    let ms = r.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0);
                    text.push_str(&format!(
                        "{id}  {ts}  {cmd}  ·  {total} 台 / 成功 {ok} / 失败 {failed}  ·  {:.2}s\n",
                        ms as f64 / 1000.0
                    ));
                }
                if text.is_empty() {
                    text = "还没有批量执行记录\n".into();
                }
                Ok((text, data))
            }
            BatchCmd::Show { run_id, out } => {
                if !matches!(out.as_str(), "table" | "json") {
                    return Err(AiError::Args(format!(
                        "未知 out: {out}（可选 table / json）"
                    )));
                }
                let c = require_client(client)?;
                let data = c
                    .call(methods::BATCH_SHOW, json!({ "run_id": run_id }))
                    .await?;
                if out == "json" {
                    return Ok((
                        serde_json::to_string_pretty(&data).unwrap_or_default() + "\n",
                        data,
                    ));
                }
                // 回看时把明细重新排成与实时执行一致的表格（失败在前）
                let mut text = String::new();
                let (cmd, total, ok, failed, ms) = (
                    data["run"]["command"].as_str().unwrap_or(""),
                    data["run"]["total"].as_i64().unwrap_or(0),
                    data["run"]["ok"].as_i64().unwrap_or(0),
                    data["run"]["failed"].as_i64().unwrap_or(0),
                    data["run"]["duration_ms"].as_i64().unwrap_or(0),
                );
                text.push_str(&format!(
                    "{run_id}  {cmd}  ·  {total} 台 / 成功 {ok} / 失败 {failed}  ·  {:.2}s\n",
                    ms as f64 / 1000.0
                ));
                let empty: Vec<Value> = Vec::new();
                let mut rows: Vec<&Value> = data
                    .get("results")
                    .and_then(|v| v.as_array())
                    .unwrap_or(&empty)
                    .iter()
                    .collect();
                rows.sort_by_key(|r| {
                    match r.get("status").and_then(|v| v.as_str()).unwrap_or("") {
                        "ok" => 2,
                        "skipped" => 1,
                        _ => 0,
                    }
                });
                for r in rows {
                    let host = r.get("host").and_then(|v| v.as_str()).unwrap_or("-");
                    let st = r.get("status").and_then(|v| v.as_str()).unwrap_or("-");
                    let code = r
                        .get("exit_code")
                        .and_then(|v| v.as_i64())
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "-".into());
                    let d = r.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0);
                    let mark = match st {
                        "ok" => "✔",
                        "skipped" => "○",
                        _ => "✘",
                    };
                    let err = r
                        .get("error")
                        .and_then(|v| v.as_str())
                        .filter(|e| !e.is_empty())
                        .map(|e| format!("  {e}"))
                        .unwrap_or_default();
                    text.push_str(&format!("{mark} {host:<20} {st:<8} exit={code:<4} {d}ms{err}\n"));
                }
                Ok((text, data))
            }
        },
        AiObject::Screen { cmd } => match cmd {
            ScreenCmd::Read {
                session,
                lines,
                mode,
                no_scrollback,
            } => {
                if !matches!(mode.as_str(), "plain" | "grid") {
                    return Err(AiError::Args(format!(
                        "未知 mode: {mode}（可选 plain / grid）"
                    )));
                }
                let c = require_client(client)?;
                let data = c
                    .call(
                        methods::SCREEN_READ,
                        json!({
                            "session": session,
                            "scrollback": !no_scrollback,
                            "mode": mode,
                            "lines": lines,
                        }),
                    )
                    .await?;
                let text = if mode == "grid" {
                    // grid 模式：人读版就是每行拼起来
                    data.get("lines")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|l| l.as_str())
                                .map(|s| s.to_string())
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default()
                        + "\n"
                } else {
                    let t = data.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    format!("{t}\n")
                };
                Ok((text, data))
            }
        },
        AiObject::Log { cmd } => match cmd {
            LogCmd::Query {
                host,
                session,
                since,
                actor,
                risk,
                limit,
            } => {
                let c = require_client(client)?;
                let data = c
                    .call(
                        methods::LOG_QUERY,
                        json!({
                            "host": host,
                            "session": session,
                            "since": since,
                            "actor": actor,
                            "risk": risk,
                            "limit": limit,
                        }),
                    )
                    .await?;
                let records = data.get("records").cloned().unwrap_or(json!([]));
                let mut text = String::new();
                if let Some(list) = records.as_array() {
                    if list.is_empty() {
                        text = "（无匹配的审计记录）\n".to_string();
                    }
                    for r in list {
                        text.push_str(&format!(
                            "{}  {:<8} {:<16} {:<8} {:<18} {}\n",
                            r.get("ts").and_then(|v| v.as_str()).unwrap_or("-"),
                            r.get("actor").and_then(|v| v.as_str()).unwrap_or("-"),
                            r.get("host_name").and_then(|v| v.as_str()).unwrap_or("-"),
                            r.get("risk").and_then(|v| v.as_str()).unwrap_or("-"),
                            r.get("decision").and_then(|v| v.as_str()).unwrap_or("-"),
                            r.get("command")
                                .and_then(|v| v.as_str())
                                .or_else(|| r.get("action").and_then(|v| v.as_str()))
                                .unwrap_or("-"),
                        ));
                    }
                }
                Ok((text, json!({ "records": records })))
            }
        },
        AiObject::File { cmd } => match cmd {
            FileCmd::List {
                session,
                host,
                path,
            } => {
                let c = require_client(client)?;
                let mut p = json!({ "session": session, "host": host, "path": path });
                ctx.trace(&mut p);
                let data = c.call(methods::FILE_LIST, p).await?;
                let entries = data.get("entries").cloned().unwrap_or(json!([]));
                let mut text = String::new();
                if let Some(list) = entries.as_array() {
                    if list.is_empty() {
                        text = "（空目录）\n".to_string();
                    }
                    for e in list {
                        let is_dir = e.get("is_dir").and_then(|v| v.as_bool()).unwrap_or(false);
                        let mode = e.get("mode").and_then(|v| v.as_u64()).map(|m| m as u32);
                        let size = e.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                        let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let ts = e
                            .get("modified")
                            .and_then(|v| v.as_i64())
                            .map(fmt_ts)
                            .unwrap_or_else(|| "                ".to_string());
                        text.push_str(&format!(
                            "{} {:>10}  {}  {}{}\n",
                            sh0_core::mode_string(is_dir, mode),
                            size,
                            ts,
                            name,
                            if is_dir { "/" } else { "" },
                        ));
                    }
                }
                Ok((text, data))
            }
            FileCmd::Stat {
                session,
                host,
                path,
            } => {
                let c = require_client(client)?;
                let mut p = json!({ "session": session, "host": host, "path": path });
                ctx.trace(&mut p);
                let data = c.call(methods::FILE_STAT, p).await?;
                let e = data.get("entry").cloned().unwrap_or(json!(null));
                let text = if e.is_null() {
                    "（无此路径）\n".to_string()
                } else {
                    format!(
                        "{}  {}  {} bytes  mode {:o}  uid {:?}  gid {:?}\n",
                        e.get("path").and_then(|v| v.as_str()).unwrap_or("?"),
                        if e.get("is_dir").and_then(|v| v.as_bool()).unwrap_or(false) {
                            "目录"
                        } else {
                            "文件"
                        },
                        e.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                        e.get("mode").and_then(|v| v.as_u64()).unwrap_or(0),
                        e.get("uid").and_then(|v| v.as_u64()),
                        e.get("gid").and_then(|v| v.as_u64()),
                    )
                };
                Ok((text, data))
            }
            FileCmd::Upload {
                session,
                host,
                local,
                remote,
            } => {
                let c = require_client(client)?;
                // 分块而不是整体 write：AI 传个几百兆的日志也不该把内存吃干。
                // 块大小与 TUI 队列一致（256 KB），两端行为对齐，排查时不用换算。
                let mut file =
                    std::fs::File::open(local).map_err(|e| AiError::Args(format!("打开 {local} 失败：{e}")))?;
                let total = file
                    .metadata()
                    .map(|m| m.len())
                    .unwrap_or(0);
                let mut offset = 0u64;
                let mut sent = 0usize;
                let mut chunks = 0u32;
                loop {
                    let mut buf = vec![0u8; AI_CHUNK];
                    let n = read_at(&mut file, offset, &mut buf)
                        .map_err(|e| AiError::Other(format!("读 {local} 失败：{e}")))?;
                    if n == 0 {
                        break;
                    }
                    buf.truncate(n);
                    let (encoding, data) = match std::str::from_utf8(&buf) {
                        Ok(s) => ("utf8", s.to_string()),
                        Err(_) => ("base64", base64_encode(&buf)),
                    };
                    let mut p = json!({
                        "session": session, "host": host,
                        "path": remote, "offset": offset,
                        "data": data, "encoding": encoding,
                    });
                    ctx.trace(&mut p);
                    c.call(methods::FILE_WRITE_AT, p).await?;
                    offset += n as u64;
                    sent += n;
                    chunks += 1;
                }
                let text = format!("已上传 {local} → {remote}（{sent} 字节 / {chunks} 块）\n");
                Ok((text, json!({
                    "local": local, "remote": remote,
                    "bytes": sent, "chunks": chunks, "size": total,
                })))
            }
            FileCmd::Download {
                session,
                host,
                remote,
                local,
            } => {
                let c = require_client(client)?;
                let mut offset = 0u64;
                let mut got = 0usize;
                let mut chunks = 0u32;
                let mut created = false;
                loop {
                    let mut p = json!({
                        "session": session, "host": host,
                        "path": remote, "offset": offset, "len": AI_CHUNK,
                    });
                    ctx.trace(&mut p);
                    let data = c.call(methods::FILE_READ_RANGE, p).await?;
                    let enc = data.get("encoding").and_then(|v| v.as_str()).unwrap_or("utf8");
                    let content = data.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    let bytes = if enc == "base64" {
                        base64_decode(content)
                            .ok_or_else(|| AiError::Other("下载内容无法解码".into()))?
                    } else {
                        content.as_bytes().to_vec()
                    };
                    if bytes.is_empty() {
                        break;
                    }
                    if !created {
                        std::fs::write(local, &bytes)
                            .map_err(|e| AiError::Args(format!("写 {local} 失败：{e}")))?;
                        created = true;
                    } else {
                        append_to(local, &bytes)
                            .map_err(|e| AiError::Args(format!("写 {local} 失败：{e}")))?;
                    }
                    offset += bytes.len() as u64;
                    got += bytes.len();
                    chunks += 1;
                }
                let text = format!("已下载 {remote} → {local}（{got} 字节 / {chunks} 块）\n");
                Ok((text, json!({
                    "remote": remote, "local": local,
                    "bytes": got, "chunks": chunks,
                })))
            }
            FileCmd::Read {
                session,
                host,
                path,
                max_bytes,
            } => {
                let c = require_client(client)?;
                let mut p =
                    json!({ "session": session, "host": host, "path": path, "max_bytes": max_bytes });
                ctx.trace(&mut p);
                let data = c.call(methods::FILE_READ, p).await?;
                let enc = data.get("encoding").and_then(|v| v.as_str()).unwrap_or("utf8");
                let text = if enc == "base64" {
                    format!(
                        "（二进制文件，{} 字节，base64 见 data.content）\n",
                        data.get("size").and_then(|v| v.as_u64()).unwrap_or(0)
                    )
                } else {
                    data.get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                Ok((text, data))
            }
            FileCmd::Write {
                session,
                host,
                path,
                data,
            } => {
                let c = require_client(client)?;
                let body = match data {
                    Some(d) => d.clone(),
                    None => {
                        let mut s = String::new();
                        use std::io::Read;
                        std::io::stdin()
                            .read_to_string(&mut s)
                            .map_err(|e| AiError::Ipc(format!("读取 stdin 失败: {e}")))?;
                        s
                    }
                };
                let mut p = json!({ "session": session, "host": host, "path": path, "data": body });
                ctx.trace(&mut p);
                let data = c.call(methods::FILE_WRITE, p).await?;
                Ok((
                    format!("已写入 {} 字节\n", data.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0)),
                    data,
                ))
            }
            FileCmd::Mkdir {
                session,
                host,
                path,
            } => {
                let c = require_client(client)?;
                let mut p = json!({ "session": session, "host": host, "path": path });
                ctx.trace(&mut p);
                let data = c.call(methods::FILE_MKDIR, p).await?;
                Ok((format!("已创建目录 {path}\n"), data))
            }
            FileCmd::Remove {
                session,
                host,
                path,
                dir,
            } => {
                let c = require_client(client)?;
                let mut p = json!({ "session": session, "host": host, "path": path, "dir": dir });
                ctx.trace(&mut p);
                let data = c.call(methods::FILE_REMOVE, p).await?;
                Ok((format!("已删除 {path}\n"), data))
            }
            FileCmd::Rename {
                session,
                host,
                from,
                to,
            } => {
                let c = require_client(client)?;
                let mut p = json!({ "session": session, "host": host, "from": from, "to": to });
                ctx.trace(&mut p);
                let data = c.call(methods::FILE_RENAME, p).await?;
                Ok((format!("已重命名 {from} → {to}\n"), data))
            }
            FileCmd::Chmod {
                session,
                host,
                path,
                mode,
            } => {
                let c = require_client(client)?;
                let m = parse_octal(mode)?;
                let mut p = json!({ "session": session, "host": host, "path": path, "mode": m });
                ctx.trace(&mut p);
                let data = c.call(methods::FILE_CHMOD, p).await?;
                Ok((format!("{path} 权限已改为 {mode}\n"), data))
            }
        },
    }
}

/// 八进制权限串 → u32。接受 `644`、`0644`、`0o644`
fn parse_octal(s: &str) -> Result<u32, AiError> {
    let t = s.trim().trim_start_matches("0o");
    u32::from_str_radix(t, 8)
        .map_err(|_| AiError::Args(format!("`{s}` 不是合法八进制权限（如 644）")))
}

/// unix 秒 → `YYYY-MM-DD HH:MM:SS`（UTC）。
///
/// 不引 chrono：这里只要一个稳定的可读串，为它多一个依赖不划算。
/// 算法是 Howard Hinnant 的 civil_from_days（公历，1970 起算）。
fn fmt_ts(secs: i64) -> String {
    fn civil_from_days(z: i64) -> (i64, i64, i64) {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (if m <= 2 { y + 1 } else { y }, m, d)
    }
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 与 TUI 传输队列一致的块大小
const AI_CHUNK: usize = 256 * 1024;

/// 从文件的 `offset` 处读一块
fn read_at(f: &mut std::fs::File, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::io::{Read, Seek};
    f.seek(std::io::SeekFrom::Start(offset))?;
    let mut read = 0usize;
    while read < buf.len() {
        let n = f.read(&mut buf[read..])?;
        if n == 0 {
            break;
        }
        read += n;
    }
    Ok(read)
}

fn append_to(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
    f.write_all(bytes)
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// exec 的人类可读输出：一行头部 + stdout + stderr
fn exec_text(data: &Value, suffix: Option<&str>) -> String {
    let host = data.get("host").and_then(|v| v.as_str()).unwrap_or("?");
    let exit = data
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".into());
    let dur = data.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0);
    let mut head = format!("{host} · exit {exit} · {}ms", dur);
    if let Some(s) = suffix {
        head.push_str(&format!(" · {s}"));
    }
    if data.get("truncated").and_then(|v| v.as_bool()).unwrap_or(false) {
        head.push_str(" · 已截断");
    }
    let mut out = format!("{head}\n");
    if let Some(s) = data.get("stdout").and_then(|v| v.as_str())
        && !s.is_empty()
    {
        out.push_str(s);
        if !s.ends_with('\n') {
            out.push('\n');
        }
    }
    if let Some(s) = data.get("stderr").and_then(|v| v.as_str())
        && !s.is_empty()
    {
        out.push_str(s);
        if !s.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// 把 --text / --keys / --hex 合成最终要发送的字节串
/// 批量结果的人读汇总：一行总览 + 逐台结果（失败排前面，FR-5.3）
fn batch_text(data: &Value, out: &str) -> String {
    if out == "json" {
        return serde_json::to_string_pretty(data).unwrap_or_default();
    }
    if out == "csv" {
        return batch_csv(data);
    }

    let total = data.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    let ok = data.get("ok").and_then(|v| v.as_u64()).unwrap_or(0);
    let failed = data.get("failed").and_then(|v| v.as_u64()).unwrap_or(0);
    let skipped = data.get("skipped").and_then(|v| v.as_u64()).unwrap_or(0);
    let ms = data.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0);
    let conc = data.get("concurrency").and_then(|v| v.as_u64()).unwrap_or(0);
    let risk = data.get("risk").and_then(|v| v.as_str()).unwrap_or("-");

    let mut s = format!(
        "{} 台 · 成功 {ok} · 失败 {failed} · 跳过 {skipped} · 并发 {conc} · 风险 {risk} · 耗时 {:.2}s\n",
        total,
        ms as f64 / 1000.0
    );

    let mut rows: Vec<&Value> = data
        .get("results")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    // 失败的排前面：批量巡检时人要第一时间看到哪几台有问题
    rows.sort_by_key(|r| {
        let st = r.get("status").and_then(|v| v.as_str()).unwrap_or("");
        match st {
            "ok" => 2,
            "skipped" => 1,
            _ => 0,
        }
    });
    for r in rows {
        let host = r.get("host").and_then(|v| v.as_str()).unwrap_or("-");
        let st = r.get("status").and_then(|v| v.as_str()).unwrap_or("-");
        let code = r
            .get("exit_code")
            .and_then(|v| v.as_i64())
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into());
        let d = r.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0);
        let mark = match st {
            "ok" => "✔",
            "skipped" => "○",
            _ => "✘",
        };
        let out_first = r
            .get("stdout")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        let err = r
            .get("error")
            .and_then(|v| v.as_str())
            .filter(|e| !e.is_empty())
            .map(|e| format!("  {e}"))
            .unwrap_or_default();
        s.push_str(&format!(
            "{mark} {host:<20} {st:<8} exit={code:<4} {d}ms  {out_first}{err}\n"
        ));
    }
    s
}

/// 批量结果导出 CSV：表头固定，字段按 RFC4180 转义（Excel / jq 可直接读）
fn batch_csv(data: &Value) -> String {
    let mut s = String::from("host,status,ok,exit_code,duration_ms,timeout,truncated,stdout,error\n");
    let empty: Vec<Value> = Vec::new();
    let rows = data.get("results").and_then(|v| v.as_array()).unwrap_or(&empty);
    for r in rows {
        let pick = |k: &str| -> String {
            match r.get(k) {
                Some(Value::String(x)) => x.clone(),
                Some(Value::Null) | None => String::new(),
                Some(other) => other.to_string(),
            }
        };
        let ok = r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut fields = vec![
            pick("host"),
            pick("status"),
            ok.to_string(),
            pick("exit_code"),
            pick("duration_ms"),
            pick("timeout"),
            pick("truncated"),
            pick("stdout"),
            pick("error"),
        ];
        fields = fields
            .into_iter()
            .map(|f| {
                if f.contains([',', '"', '\n', '\r']) {
                    format!("\"{}\"", f.replace('"', "\"\""))
                } else {
                    f
                }
            })
            .collect();
        s.push_str(&fields.join(","));
        s.push('\n');
    }
    s
}

/// 广播态的人读表述（开/关 + 目标范围）
fn broadcast_status_text(data: &Value) -> String {
    let enabled = data.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
    let all = data.get("all").and_then(|v| v.as_bool()).unwrap_or(true);
    let n = data
        .get("sessions")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let state = if enabled { "开启" } else { "关闭" };
    if all {
        format!("{state}（目标：全部已打开会话）")
    } else {
        format!("{state}（目标：{n} 个指定会话）")
    }
}

fn build_input_payload(
    text: Option<&str>,
    keys: Option<&str>,
    hex: Option<&str>,
    enter: bool,
) -> Result<String, AiError> {
    let mut out = String::new();
    if let Some(t) = text {
        out.push_str(t);
    }
    if let Some(k) = keys {
        out.push_str(&parse_keys(k)?);
    }
    if let Some(h) = hex {
        out.push_str(&parse_hex(h)?);
    }
    if enter {
        out.push('\n');
    }
    if out.is_empty() {
        return Err(AiError::Args(
            "至少给出 --text / --keys / --hex 之一".into(),
        ));
    }
    Ok(out)
}

/// 解析键名序列：字面文本 + `<Enter>` `<Esc>` `<Tab>` `<BS>` `<Del>` `<Up>` …
/// `<C-c>`（ctrl 组合）等占位符。复用 sh0-term 的按键编码表——
/// 人按的和 AI 发的必须是同一套字节。
fn parse_keys(s: &str) -> Result<String, AiError> {
    use sh0_term::keys::{encode_key, parse_key};

    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('<') {
        // '<' 之前是字面文本
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('>') else {
            // 没有闭合的 '<'：按字面量处理
            out.push('<');
            rest = after;
            continue;
        };
        let name = &after[..end];
        rest = &after[end + 1..];
        if name.is_empty() {
            out.push_str("<>");
            continue;
        }
        match parse_key(name) {
            Some(key) => match encode_key(key, false) {
                Some(bytes) => out.push_str(&String::from_utf8_lossy(&bytes)),
                None => {
                    return Err(AiError::Args(format!("按键 <{name}> 无法编码为字节")));
                }
            },
            None => {
                return Err(AiError::Args(format!(
                    "未知按键名 <{name}>（可用: Enter/Esc/Tab/BS/Del/Up/Down/Left/Right/Home/End/PageUp/PageDown/F1-F12/C-a 等）"
                )));
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// 解析十六进制字节串："1b5b41" → ESC [ A
fn parse_hex(s: &str) -> Result<String, AiError> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !cleaned.len().is_multiple_of(2) {
        return Err(AiError::Args("hex 长度必须是偶数".into()));
    }
    let mut bytes = Vec::with_capacity(cleaned.len() / 2);
    for i in (0..cleaned.len()).step_by(2) {
        match u8::from_str_radix(&cleaned[i..i + 2], 16) {
            Ok(b) => bytes.push(b),
            Err(_) => {
                return Err(AiError::Args(format!(
                    "hex 含非法字符: {}",
                    &cleaned[i..i + 2]
                )));
            }
        }
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn require_client(client: Option<Arc<IpcClient>>) -> Result<Arc<IpcClient>, AiError> {
    client.ok_or_else(|| AiError::Ipc("daemon 未运行，请先执行 `sh0 daemon run`".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_batch() -> Value {
        json!({
            "run_id": "br_1",
            "command": "df -h /",
            "mode": "exec",
            "risk": "auto",
            "concurrency": 16,
            "total": 3,
            "ok": 2,
            "failed": 1,
            "skipped": 0,
            "duration_ms": 1200,
            "results": [
                {"host":"h0","status":"ok","ok":true,"exit_code":0,"stdout":"40%","stderr":"","duration_ms":12,"truncated":false,"timeout":false,"error":null},
                {"host":"h1","status":"error","ok":false,"exit_code":null,"stdout":"","stderr":"","duration_ms":30,"truncated":false,"timeout":false,"error":"boom, \"quoted\""},
                {"host":"h2","status":"ok","ok":true,"exit_code":0,"stdout":"a,b","stderr":"","duration_ms":15,"truncated":false,"timeout":false,"error":null}
            ]
        })
    }

    /// CSV 要能被 Excel / jq 直接读：逗号与引号按 RFC4180 转义（FR-5.3）
    #[test]
    fn batch_csv_escapes_commas_and_quotes() {
        let csv = batch_csv(&sample_batch());
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(
            lines[0],
            "host,status,ok,exit_code,duration_ms,timeout,truncated,stdout,error"
        );
        assert_eq!(lines.len(), 4, "表头 + 3 行: {csv}");
        assert!(csv.contains("\"a,b\""), "含逗号的字段要加引号: {csv}");
        assert!(csv.contains("\"boom, \"\"quoted\"\"\""), "引号要双写: {csv}");
    }

    /// 汇总文本把失败的排前面：巡检时人第一时间看到有问题的机器
    #[test]
    fn batch_text_puts_failures_first() {
        let t = batch_text(&sample_batch(), "table");
        let lines: Vec<&str> = t.lines().collect();
        assert!(lines[0].contains("3 台"), "首行是总览: {lines:?}");
        assert!(lines[0].contains("成功 2"), "总览含成功数: {}", lines[0]);
        let h1 = lines.iter().position(|l| l.contains("h1")).unwrap();
        let h0 = lines.iter().position(|l| l.contains("h0")).unwrap();
        assert!(h1 < h0, "失败的要排在成功前面: {t}");
        assert!(lines.iter().any(|l| l.starts_with("✘")), "失败行用 ✘ 标记: {t}");
    }

    #[test]
    fn batch_text_json_mode_emits_json() {
        let t = batch_text(&sample_batch(), "json");
        assert!(t.trim_start().starts_with('{'), "json 模式应输出 JSON: {t}");
    }
}
