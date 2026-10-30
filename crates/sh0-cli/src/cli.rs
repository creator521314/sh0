//! 顶层命令路由。
//!
//! 用户入口：`tui` / `ai` / `help`，外加运维命令 `daemon`（run / stop）。
//! daemon 平时由 `ensure` 自动拉起；把它暴露出来是为了调试与排障时能手动
//! 前台运行或优雅停止，而不是只能 kill 进程。

use clap::{Parser, Subcommand};
use sh0_ai::AiObject;

#[derive(Debug, Parser)]
#[command(
    name = "sh0",
    version,
    about = "轻量级跨平台 TUI SSH/SFTP 管理工具",
    disable_help_subcommand = true,
    after_help = "后台 daemon：首次运行 tui/ai 时自动拉起，持有 SSH 会话、风险判定与审计；\
                 无需手动管理，空闲 5 分钟自动退出。详见 `sh0 help`。"
)]
pub struct Cli {
    /// 省略子命令时等价于 `sh0 help`
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

// 只在进程启动时解析一次，枚举大小无所谓；Box 反而让 clap derive 更绕
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// 启动 TUI 界面
    Tui {
        /// 主机名称或 ID；省略时进入主机树选择
        host: Option<String>,
    },
    /// 面向 AI 的结构化接口
    Ai {
        #[command(subcommand)]
        object: AiObject,

        /// 输出结构化 JSON
        #[arg(long, global = true)]
        json: bool,

        /// 调用链追踪 ID
        #[arg(long, global = true)]
        trace_id: Option<String>,

        /// 超时（秒）
        #[arg(long, global = true, default_value_t = 30)]
        timeout: u64,

        /// 无人值守场景下显式批准 confirm 档命令（等价于用户点了"确认"）
        #[arg(long, global = true)]
        yes: bool,

        /// 只做风险判定不执行（AI 自检：这条命令会被放行吗）
        #[arg(long, global = true)]
        dry_run: bool,

        /// 安静模式：不输出人类可读文本（JSON 仍会打印）
        #[arg(long, global = true)]
        quiet: bool,
    },
    /// 显示帮助（含配置/数据目录与快速开始）
    Help,
    /// 常驻后端进程（通常自动拉起；调试/排障时可手动 run / stop）
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCmd {
    /// 前台运行（调试用）。由 sh0 自动拉起时会带 --background
    Run {
        /// 后台模式：无控制台，不注册 Ctrl+C 处理（否则信号会立即触发导致退出）
        #[arg(long)]
        background: bool,
    },
    /// 优雅停止
    Stop,
}
