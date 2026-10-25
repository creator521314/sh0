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
    /// 主机清单管理
    Host {
        #[command(subcommand)]
        cmd: HostCmd,
    },
    /// 凭据管理（OS 钥匙串 / 加密 vault，FR-1.4~FR-1.7）
    Secret {
        #[command(subcommand)]
        cmd: SecretCmd,
    },
    /// 配置目录与校验
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Debug, Subcommand)]
pub enum SecretCmd {
    /// 存入一条秘密（--stdin 从标准输入读，避免进入 shell 历史）
    Set {
        service: String,
        account: String,
        #[arg(long)]
        stdin: bool,
    },
    /// 取出一条秘密（明文打印到 stdout，小心管道）
    Get { service: String, account: String },
    /// 列出 vault 中的条目名（钥匙串无法枚举）
    List,
    /// 删除一条秘密
    Rm { service: String, account: String },
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
    /// 导入既有配置（只读源文件，FR-1.10）
    Import {
        /// 来源：ssh（~/.ssh/config）或 xsh（Xshell 会话文件/目录）
        #[arg(long, value_name = "ssh|xsh")]
        from: String,
        /// 源文件路径；ssh 缺省用 ~/.ssh/config
        #[arg(long)]
        path: Option<String>,
        /// 只打印将要导入什么，不写入 hosts.toml
        #[arg(long)]
        dry_run: bool,
        /// 输出结构化 JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// 打印配置、数据、运行时目录
    Path,
    /// 打印当前生效的配置（TOML）
    Show {
        /// 输出结构化 JSON
        #[arg(long)]
        json: bool,
    },
    /// 取一项配置，如 `sh0 config get limits.exec_timeout_sec`
    Get { key: String },
    /// 写一项配置，如 `sh0 config set limits.exec_timeout_sec 60`
    Set { key: String, value: String },
    /// 让运行中的 daemon 重新加载配置与主机清单
    Reload,
    /// 校验配置文件能否解析（不给改动就报错退出）
    Check,
}
