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
    let cli = Cli::parse();
    // 只有后台 daemon 才落文件日志；其余命令保持"输出到 stdout"的命令行习惯
    let file_log = match &cli.command {
        Cmd::Daemon { cmd: DaemonCmd::Run { background: true } } => Some(()),
        _ => None,
    };
    let store = Arc::new(Store::new()?);
    let cfg = store.load_config()?;
    init_logging(file_log.map(|_| (store.paths().daemon_log(), cfg.log.clone())));

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

            // 兜底日志（启动期 stdout/stderr），正常日志由 daemon 自己写 daemon.log
            let log = store.paths().daemon_boot_log();
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
            HostCmd::Import {
                from,
                path,
                dry_run,
                json,
            } => {
                let source = sh0_store::ImportSource::parse(&from, path.as_deref())?;
                let report = source.collect()?;
                let merged = store.import_hosts(report, dry_run)?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "dry_run": dry_run,
                            "imported": merged.imported.iter().map(|h| serde_json::json!({
                                "name": h.name, "endpoint": h.endpoint(), "group": h.group_path(),
                            })).collect::<Vec<_>>(),
                            "duplicated": merged.duplicated,
                            "skipped": merged.skipped,
                        }))?
                    );
                } else {
                    println!(
                        "{} {} 台，跳过 {} 条，重名未覆盖 {} 台",
                        if dry_run { "将要导入" } else { "已导入" },
                        merged.imported.len(),
                        merged.skipped.len(),
                        merged.duplicated.len()
                    );
                    for h in &merged.imported {
                        println!("  + {:<20} {}", h.name, h.endpoint());
                    }
                    for (name, why) in &merged.skipped {
                        println!("  - {name}: {why}");
                    }
                    for name in &merged.duplicated {
                        println!("  = {name}: 已存在，保留现有配置");
                    }
                    if dry_run && !merged.imported.is_empty() {
                        println!("（--dry-run：未写入；去掉该参数才落盘）");
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
            ConfigCmd::Show { json } => {
                let cfg = store.load_config()?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&cfg)?);
                } else {
                    println!("{}", toml::to_string_pretty(&cfg)?);
                }
            }
            ConfigCmd::Get { key } => match store.config_get(&key)? {
                Some(v) => println!("{}", sh0::config_cmd::value_to_string(&v)),
                None => {
                    eprintln!("未知配置项：{key}（用 `sh0 config show` 看有哪些）");
                    std::process::exit(2);
                }
            },
            ConfigCmd::Set { key, value } => {
                store.config_set(&key, &value)?;
                println!("已写入 {key} = {value}");
                // 有 daemon 在跑就顺手热重载，省得用户改完以为没生效
                if let Some(note) = sh0::config_cmd::notify_daemon(&store).await {
                    println!("{note}");
                } else {
                    println!("（daemon 未运行，下次启动生效）");
                }
            }
            ConfigCmd::Reload => match sh0::config_cmd::notify_daemon(&store).await {
                Some(note) => println!("{note}"),
                None => println!("daemon 未运行，无需重载（下次启动自动读取）"),
            },
            ConfigCmd::Check => {
                // 解析失败会直接以错误退出
                let cfg = store.load_config()?;
                let hosts = store.load_hosts()?;
                let rules = store.load_risk_rules(&cfg.risk.rules_file).unwrap_or_default();
                println!("config.toml  OK（version {}）", cfg.version);
                println!("hosts.toml   OK（{} 台）", hosts.hosts.len());
                println!("risk.toml    OK（{} 条自定义规则）", rules.len());
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
