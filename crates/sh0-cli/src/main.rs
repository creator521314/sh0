//! sh0 入口：命令路由与 daemon 生命周期。

mod cli;
mod daemon;
mod ensure;

use std::sync::Arc;

use clap::Parser;
use sh0_ai::AiObject;
use sh0_store::Store;

use cli::{Cmd, Cli, ConfigCmd, DaemonCmd, HostCmd};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();

    let cli = Cli::parse();
    let store = Arc::new(Store::new()?);
    let cfg = store.load_config()?;

    match cli.command {
        Cmd::Daemon { cmd } => match cmd {
            DaemonCmd::Run { background } => {
                daemon::run_foreground(store, cfg, background).await?
            }
            DaemonCmd::Stop => {
                if ensure::stop_daemon(store.paths()).await? {
                    println!("daemon 已停止");
                } else {
                    println!("daemon 未在运行");
                }
            }
        },

        Cmd::Tui { host } => {
            let hosts = store.load_hosts()?;
            if hosts.hosts.is_empty() {
                daemon::seed_example_host(&store)?;
                println!("已生成示例配置：{}", store.paths().hosts_file().display());
                println!("请编辑该文件填入真实主机后重试。");
                return Ok(());
            }

            let selector = match host {
                Some(h) => h,
                None if hosts.hosts.len() == 1 => hosts.hosts[0].name.clone(),
                None => {
                    anyhow::bail!(
                        "配置中有多台主机，请用 `sh0 tui <主机名>` 指定。可用：{}",
                        hosts
                            .hosts
                            .iter()
                            .map(|h| h.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            };

            let (conn, _info) = ensure::connect_or_start(store.paths()).await?;
            sh0_tui::run(
                conn.client,
                conn.events,
                &selector,
                cfg.cols,
                cfg.rows,
            )
            .await?;
        }

        Cmd::Ai {
            object,
            json,
            trace_id,
            timeout,
        } => {
            let args = sh0_ai::AiArgs {
                object,
                json,
                trace_id,
                timeout,
            };

            // host 类子命令只读配置文件，不必拉起 daemon
            let needs_daemon = !matches!(args.object, AiObject::Host { .. });
            let client = if needs_daemon {
                Some(ensure::connect_or_start(store.paths()).await?.0.client)
            } else {
                None
            };

            let code = sh0_ai::run(args, client).await;
            std::process::exit(code);
        }

        Cmd::Host { cmd } => match cmd {
            HostCmd::List { json } => {
                let hosts = store.load_hosts()?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&hosts.hosts)?);
                } else {
                    for h in &hosts.hosts {
                        println!(
                            "{:<16} {:<28} {:<12} {}",
                            h.name,
                            h.endpoint(),
                            h.group_path(),
                            h.note.clone().unwrap_or_default()
                        );
                    }
                }
            }
        },

        Cmd::Config { cmd } => match cmd {
            ConfigCmd::Path => {
                let p = store.paths();
                println!("config_dir  : {}", p.config_dir.display());
                println!("data_dir    : {}", p.data_dir.display());
                println!("runtime_dir : {}", p.runtime_dir.display());
                println!("hosts_file  : {}", p.hosts_file().display());
            }
        },
    }

    Ok(())
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
