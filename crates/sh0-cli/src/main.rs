//! sh0 入口：命令路由与 daemon 生命周期。
//! 业务逻辑在 lib 目标（`sh0::daemon` 等），这里只做命令行粘合。

use std::sync::Arc;

use clap::Parser;
use sh0_ai::AiObject;
use sh0_store::Store;

use sh0::cli::{Cmd, Cli, DaemonCmd};
use sh0::daemon;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // 只有后台 daemon 才落文件日志；其余命令保持"输出到 stdout"的命令行习惯
    let file_log = match &cli.command {
        Some(Cmd::Daemon {
            cmd: DaemonCmd::Run { background: true },
        }) => Some(()),
        _ => None,
    };
    let store = Arc::new(Store::new()?);
    let cfg = store.load_config()?;
    init_logging(file_log.map(|_| (store.paths().daemon_log(), cfg.log.clone())));

    match cli.command {
        None | Some(Cmd::Help) => print_help(&store),

        Some(Cmd::Daemon { cmd }) => match cmd {
            DaemonCmd::Run { background } => {
                daemon::run_foreground(store, cfg, background).await?
            }
            DaemonCmd::Stop => {
                if sh0::ensure::stop_daemon(store.paths()).await? {
                    println!("daemon 已停止");
                } else {
                    println!("daemon 未在运行");
                }
            }
        },

        Some(Cmd::Tui { host }) => {
            let hosts = store.load_hosts()?;
            // 不再生成示例主机、也不再因为清单为空而退出：
            // TUI 本身就是主机管理界面，空清单直接进去按 n 新建。
            let selector = match host {
                Some(h) => Some(h),
                None if hosts.hosts.len() == 1 => Some(hosts.hosts[0].name.clone()),
                None => None,
            };

            // 兜底日志（启动期 stdout/stderr），正常日志由 daemon 自己写 daemon.log
            let log = store.paths().daemon_boot_log();
            let (conn, _info) =
                sh0::ensure::connect_or_start(store.paths(), Some(&log)).await?;
            sh0_tui::run(conn.client, conn.events, store, cfg, selector.as_deref()).await?;
        }

        Some(Cmd::Ai {
            object,
            json,
            trace_id,
            timeout,
            yes,
            dry_run,
            quiet,
        }) => {
            let args = sh0_ai::AiArgs {
                object,
                json,
                trace_id,
                timeout,
                yes,
                dry_run,
                quiet,
            };

            // host 类子命令只读配置文件，不必拉起 daemon
            let needs_daemon = !matches!(args.object, AiObject::Host { .. });
            let client = if needs_daemon {
                let log = store.paths().daemon_boot_log();
                Some(
                    sh0::ensure::connect_or_start(store.paths(), Some(&log))
                        .await?
                        .0
                        .client,
                )
            } else {
                None
            };

            let code = sh0_ai::run(args, client).await;
            std::process::exit(code);
        }
    }

    Ok(())
}

/// `sh0 help` / 无子命令时的精简说明。
///
/// 只讲人类最需要的几件事：怎么进界面、怎么给 AI 用、文件在哪，
/// 以及后台 daemon 是什么、调试/排障时怎么手动 run/stop、什么时候该看日志。
fn print_help(store: &Store) {
    let p = store.paths();
    println!(
        "\
sh0 —— 轻量级跨平台 TUI SSH/SFTP 连接管理工具

用法：
  sh0 tui [主机名|id]        进入图形界面（不填则从主机树选择）
  sh0 ai <对象> <命令> ...   面向 AI / 脚本的结构化接口
  sh0 help                   本帮助
  sh0 ai --help              列出 ai 全部子命令
  sh0 daemon run             前台运行 daemon（调试用）
  sh0 daemon stop            优雅停止后台 daemon

配置目录：  {}
数据目录：  {}
日志目录：  {}

后台 daemon：
  首次运行 tui/ai 时自动拉起，持有全部 SSH 会话，并负责执行命令、
  风险判定、确认令牌与审计；TUI 退出后会话仍由它保持。
  无需手动管理；没有会话且 5 分钟无请求时自动退出。
  调试 / 排障：sh0 daemon run 前台运行，sh0 daemon stop 优雅停止。
  日志：{}/daemon.log

快速开始：
  1) sh0 tui                按 n 新建，填地址/用户/密码（默认 password，←/→ 切 agent/key）
  2) sh0 ai host import --from ssh   从 ~/.ssh/config 导入既有主机
  3) sh0 ai exec run --host web-01 --cmd \"uname -a\" --json

凭据：密码与导入的私钥加密保存在数据目录的 creds.vault（本地 data-key）。
故障排查：先看后台 daemon 的日志（见上）。",
        p.config_dir.display(),
        p.data_dir.display(),
        p.runtime_dir.display(),
        p.runtime_dir.display(),
    );
}

/// 日志初始化。
///
/// 后台 daemon 没有控制台，日志必须落到文件，且要按大小轮转（FR-8.5）：
/// 一直追加到把磁盘写满是最难排查的一类故障。前台运行仍写 stdout，
/// 免得 `sh0 daemon run` 调试时看不到东西。
fn init_logging(log_file: Option<(std::path::PathBuf, sh0_store::LogConfig)>) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    match log_file {
        Some((path, cfg)) => {
            // 过期历史文件先清一轮
            let removed = sh0_store::prune_old_logs(&path, cfg.retention_days);
            if removed > 0 {
                tracing::debug!("清理了 {removed} 个过期日志文件");
            }
            match sh0_store::RotatingFileWriter::new(path.clone(), cfg.max_mb, cfg.keep) {
                Ok(w) => {
                    builder.with_writer(w).init();
                }
                Err(e) => {
                    eprintln!("无法打开日志 {}（{e}），退回 stdout", path.display());
                    builder.init();
                }
            }
        }
        None => builder.init(),
    }
}
