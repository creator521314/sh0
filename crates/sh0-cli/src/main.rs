//! sh0 入口：命令路由与 daemon 生命周期。
//! 业务逻辑在 lib 目标（`sh0::daemon` 等），这里只做命令行粘合。

use std::sync::Arc;

use clap::Parser;
use sh0_ai::AiObject;
use sh0_store::Store;

use sh0::cli::{Cmd, Cli, ConfigCmd, DaemonCmd, HostCmd, SecretCmd};
use sh0::daemon;

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
                if sh0::ensure::stop_daemon(store.paths()).await? {
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

            // M1 起 TUI 自带主机树，选择器变成可选项：不带参数直接进树里挑
            let selector = match host {
                Some(h) => Some(h),
                None if hosts.hosts.len() == 1 => Some(hosts.hosts[0].name.clone()),
                None => None,
            };

            let log = store.paths().daemon_log();
            let (conn, _info) =
                sh0::ensure::connect_or_start(store.paths(), Some(&log)).await?;
            sh0_tui::run(conn.client, conn.events, store, cfg, selector.as_deref()).await?;
        }

        Cmd::Ai {
            object,
            json,
            trace_id,
            timeout,
            yes,
            dry_run,
            quiet,
        } => {
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
                let log = store.paths().daemon_log();
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
                println!("audit_db    : {}", p.audit_db().display());
            }
        },

        Cmd::Secret { cmd } => match cmd {
            SecretCmd::Set { service, account, stdin } => {
                let paths = store.paths().clone();
                let value = if stdin {
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line.trim_end_matches(['\n', '\r']).to_string()
                } else {
                    // 交互读取（无回显做不到就退而求其次——提示说清楚）
                    eprint!("{service}/{account} 的值: ");
                    use std::io::Write;
                    let _ = std::io::stderr().flush();
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line.trim_end_matches(['\n', '\r']).to_string()
                };
                if value.is_empty() {
                    anyhow::bail!("值为空，已取消");
                }
                let creds = sh0_store::CredentialStore::open(&paths)?;
                creds.set(&service, &account, &value)?;
                println!("已存入 {service}/{account}（后端: {}）", creds.backend());
            }
            SecretCmd::Get { service, account } => {
                let paths = store.paths().clone();
                let creds = sh0_store::CredentialStore::open(&paths)?;
                match creds.get(&service, &account)? {
                    Some(v) => println!("{v}"),
                    None => println!("（无此条目）"),
                }
            }
            SecretCmd::List => {
                let paths = store.paths().clone();
                let creds = sh0_store::CredentialStore::open(&paths)?;
                let entries = creds.list();
                if entries.is_empty() {
                    println!("（vault 为空；钥匙串条目不在此列举）");
                }
                for e in entries {
                    println!("{e}");
                }
            }
            SecretCmd::Rm { service, account } => {
                let paths = store.paths().clone();
                let creds = sh0_store::CredentialStore::open(&paths)?;
                if creds.delete(&service, &account)? {
                    println!("已删除 {service}/{account}");
                } else {
                    println!("（无此条目）");
                }
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
