//! 顶层命令路由（W0.2）

use clap::{Parser, Subcommand};
use sh0_ai::AiObject;

#[derive(Debug, Parser)]
#[command(name = "sh0", version, about = "轻量级跨平台 TUI SSH/SFTP 管理工具")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// 常驻后端进程（通常由 sh0 自动拉起，无需手动执行）
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// 启动 TUI 界面
    Tui {
        /// 主机名称或 ID；省略时若只有一台主机则自动使用
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
    },
    /// 主机清单管理
    Host {
        #[command(subcommand)]
        cmd: HostCmd,
    },
    /// 配置目录与校验
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
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

#[derive(Debug, Subcommand)]
pub enum HostCmd {
    /// 列出主机
    List {
        /// 输出结构化 JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// 打印配置、数据、运行时目录
    Path,
}
