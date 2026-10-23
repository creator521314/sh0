//! 常驻 daemon：全部会话的唯一持有者（架构文档 §1.1）
//!
//! M0 把会话编排放在这里，而不是 sh0-core —— 因为 core 不应依赖 sh0-ssh。
//! M1 会把 `SessionManager` 下沉到 core，并通过 trait 抽象掉 SSH 细节（NFR-9）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use russh::ChannelMsg;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use sh0_core::{Host, SessionSnapshot, SessionState, new_session_id, new_host_id};
use sh0_ipc::protocol::{DaemonInfo, ErrorBody, events, methods};
use sh0_ipc::{EventHub, Handler, ServerMsg, serve};
use sh0_ssh::{ConnectOptions, SshConnection};
use sh0_store::{AppConfig, Store};

struct SessionEntry {
    id: String,
    host: Host,
    state: SessionState,
    cols: u16,
    rows: u16,
    reconnect_count: u32,
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    task: JoinHandle<()>,
}

pub struct DaemonState {
    sessions: Mutex<HashMap<String, SessionEntry>>,
    hub: EventHub,
    store: Arc<Store>,
    cfg: AppConfig,
    known_hosts: PathBuf,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
}

impl DaemonState {
    pub fn new(
        store: Arc<Store>,
        cfg: AppConfig,
        known_hosts: PathBuf,
        hub: EventHub,
    ) -> (Arc<Self>, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let state = Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            hub,
            store,
            cfg,
            known_hosts,
            shutdown: Mutex::new(Some(tx)),
        });
        (state, rx)
    }

    async fn open(
        &self,
        selector: &str,
        cols: u16,
        rows: u16,
    ) -> std::result::Result<Value, ErrorBody> {
        let host = self
            .store
            .find_host(selector)
            .map_err(|e| ErrorBody { code: "host_not_found".into(), message: e.to_string() })?;

        let opts = ConnectOptions {
            known_hosts: self.known_hosts.clone(),
            keepalive_secs: self.cfg.keepalive_secs,
        };

        let conn = SshConnection::connect(&host, opts)
            .await
            .map_err(|e| ErrorBody { code: "auth_failed".into(), message: e.to_string() })?;

        let shell = conn
            .open_shell(&self.cfg.term, cols, rows)
            .await
            .map_err(|e| ErrorBody { code: "internal".into(), message: e.to_string() })?;

        let id = new_session_id();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let hub = self.hub.clone();
        let sid = id.clone();

        let task = tokio::spawn(async move {
            let mut conn = conn;
            let mut shell = shell;
            let sid = sid;

            'outer: loop {
                // 先把积压的输入全部发走
                while let Ok(bytes) = input_rx.try_recv() {
                    if shell.send(&bytes).await.is_err() {
                        break 'outer;
                    }
                }

                // 短超时轮询输出，避免 select! 同时可变借用 shell 的两个分支
                match tokio::time::timeout(Duration::from_millis(5), shell.wait()).await {
                    Ok(Some(ChannelMsg::Data { data })) => {
                        let text = String::from_utf8_lossy(&data).to_string();
                        hub.emit(ServerMsg::event(events::OUTPUT, Some(sid.clone()), json!({ "data": text })));
                    }
                    Ok(Some(ChannelMsg::ExitStatus { exit_status })) => {
                        hub.emit(ServerMsg::event(
                            events::STATE,
                            Some(sid.clone()),
                            json!({ "state": "closed", "exit_code": exit_status }),
                        ));
                        break;
                    }
                    Ok(Some(ChannelMsg::Close)) | Ok(Some(ChannelMsg::Eof)) | Ok(None) => {
                        hub.emit(ServerMsg::event(
                            events::STATE,
                            Some(sid.clone()),
                            json!({ "state": "closed" }),
                        ));
                        break;
                    }
                    Ok(Some(_)) => {}
                    Err(_) => {}
                }
            }

            let _ = conn.close().await;
        });

        self.sessions.lock().await.insert(
            id.clone(),
            SessionEntry {
                id: id.clone(),
                host,
                state: SessionState::Ready,
                cols,
                rows,
                reconnect_count: 0,
                input_tx,
                task,
            },
        );

        self.hub.emit(ServerMsg::event(
            events::STATE,
            Some(id.clone()),
            json!({ "state": "ready" }),
        ));

        Ok(json!({ "id": id, "state": "ready" }))
    }

    async fn close(&self, selector: &str) -> std::result::Result<Value, ErrorBody> {
        let mut sessions = self.sessions.lock().await;

        if selector == "*" {
            let n = sessions.len();
            for (id, entry) in sessions.drain() {
                entry.task.abort();
                self.hub.emit(ServerMsg::event(
                    events::STATE,
                    Some(id),
                    json!({ "state": "closed" }),
                ));
            }
            return Ok(json!({ "closed": n }));
        }

        let entry = sessions.remove(selector).ok_or_else(|| ErrorBody {
            code: "session_unavailable".into(),
            message: format!("会话 {selector} 不存在"),
        })?;
        entry.task.abort();
        self.hub.emit(ServerMsg::event(
            events::STATE,
            Some(entry.id),
            json!({ "state": "closed" }),
        ));
        Ok(json!({ "closed": 1 }))
    }

    async fn snapshot_list(&self) -> Vec<SessionSnapshot> {
        self.sessions
            .lock()
            .await
            .values()
            .map(|e| SessionSnapshot {
                id: e.id.clone(),
                host_id: e.host.id.clone(),
                host_name: e.host.name.clone(),
                state: e.state,
                cols: e.cols,
                rows: e.rows,
                reconnect_count: e.reconnect_count,
            })
            .collect()
    }
}

#[async_trait]
impl Handler for DaemonState {
    async fn call(
        &self,
        method: &str,
        params: Value,
        _hub: &EventHub,
    ) -> std::result::Result<Value, ErrorBody> {
        match method {
            methods::PING => Ok(json!({ "pong": true, "pid": std::process::id() })),

            methods::SESSION_LIST => {
                let sessions = self.snapshot_list().await;
                Ok(json!({ "sessions": sessions }))
            }

            methods::SESSION_OPEN => {
                let host = params.get("host").and_then(|v| v.as_str()).ok_or_else(|| ErrorBody {
                    code: "invalid_args".into(),
                    message: "缺少 host 参数".into(),
                })?;
                let cols = params.get("cols").and_then(|v| v.as_u64()).unwrap_or(120) as u16;
                let rows = params.get("rows").and_then(|v| v.as_u64()).unwrap_or(32) as u16;
                self.open(host, cols, rows).await
            }

            methods::SESSION_CLOSE => {
                let sid = params.get("session").and_then(|v| v.as_str()).ok_or_else(|| ErrorBody {
                    code: "invalid_args".into(),
                    message: "缺少 session 参数".into(),
                })?;
                self.close(sid).await
            }

            methods::SESSION_STATUS => {
                let sid = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let sessions = self.snapshot_list().await;
                let found = sessions.into_iter().find(|s| s.id == sid);
                match found {
                    Some(s) => Ok(json!(s)),
                    None => Err(ErrorBody {
                        code: "session_unavailable".into(),
                        message: format!("会话 {sid} 不存在"),
                    }),
                }
            }

            methods::INPUT_SEND => {
                let sid = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let data = params.get("data").and_then(|v| v.as_str()).unwrap_or("");
                let sessions = self.sessions.lock().await;
                let entry = sessions.get(sid).ok_or_else(|| ErrorBody {
                    code: "session_unavailable".into(),
                    message: format!("会话 {sid} 不存在"),
                })?;
                entry
                    .input_tx
                    .send(data.as_bytes().to_vec())
                    .map_err(|_| ErrorBody {
                        code: "internal".into(),
                        message: "输入通道已关闭".into(),
                    })?;
                Ok(json!({ "sent": data.len() }))
            }

            methods::RESIZE => {
                // M0 只记录尺寸，真正的 window_change 需要把 ShellHandle 从
                // 读写 task 里暴露出来，计划随 M1 的终端仿真一起做（FR-2.4）。
                let sid = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let cols = params.get("cols").and_then(|v| v.as_u64()).unwrap_or(120) as u16;
                let rows = params.get("rows").and_then(|v| v.as_u64()).unwrap_or(32) as u16;
                let mut sessions = self.sessions.lock().await;
                match sessions.get_mut(sid) {
                    Some(e) => {
                        e.cols = cols;
                        e.rows = rows;
                        Ok(json!({ "cols": cols, "rows": rows }))
                    }
                    None => Err(ErrorBody {
                        code: "session_unavailable".into(),
                        message: format!("会话 {sid} 不存在"),
                    }),
                }
            }

            methods::SHUTDOWN => {
                if let Some(tx) = self.shutdown.lock().await.take() {
                    let _ = tx.send(());
                }
                Ok(json!({ "ok": true }))
            }

            other => Err(ErrorBody {
                code: "invalid_args".into(),
                message: format!("未知方法 {other}"),
            }),
        }
    }
}

/// 前台运行 daemon，直到收到 shutdown 或 Ctrl+C
pub async fn run_foreground(
    store: Arc<Store>,
    cfg: AppConfig,
    background: bool,
) -> anyhow::Result<()> {
    let paths = store.paths().clone();
    let known_hosts = paths.known_hosts();

    // 先探一个空闲端口，写 daemon.json 后再正式监听
    let probe = TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = probe.local_addr()?;
    drop(probe);

    let token = new_session_id();
    let info = DaemonInfo {
        pid: std::process::id(),
        addr: addr.to_string(),
        token: token.clone(),
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    std::fs::write(paths.daemon_info(), serde_json::to_string_pretty(&info)?)?;
    std::fs::write(paths.pid_file(), info.pid.to_string())?;

    tracing::info!("daemon 已启动 pid={} addr={}", info.pid, info.addr);

    let hub = EventHub::new();
    let (state, shutdown_rx) = DaemonState::new(store, cfg, known_hosts, hub.clone());

    // 后台模式下进程没有控制台，注册 ctrl_c 会立即就绪、导致 daemon 启动即退出。
    // 因此只在手动前台运行时才监听 Ctrl+C；后台一律通过 shutdown 方法优雅停止。
    if background {
        tokio::select! {
            res = serve(addr, token, state, hub) => {
                res?;
            }
            _ = shutdown_rx => {
                tracing::info!("收到 shutdown 指令，正在退出");
            }
        }
    } else {
        tokio::select! {
            res = serve(addr, token, state, hub) => {
                res?;
            }
            _ = shutdown_rx => {
                tracing::info!("收到 shutdown 指令，正在退出");
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("收到 Ctrl+C，正在退出");
            }
        }
    }

    let _ = std::fs::remove_file(paths.daemon_info());
    let _ = std::fs::remove_file(paths.pid_file());
    Ok(())
}

/// 首次使用时生成一个示例主机条目，避免用户面对空配置无从下手
pub fn seed_example_host(store: &Store) -> anyhow::Result<()> {
    let mut hosts = store.load_hosts()?;
    if !hosts.hosts.is_empty() {
        return Ok(());
    }
    hosts.hosts.push(Host {
        id: new_host_id(),
        name: "example".into(),
        group: vec!["demo".into()],
        addr: "127.0.0.1".into(),
        port: 22,
        user: std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "root".into()),
        auth: sh0_core::CredentialRef::Agent,
        tags: vec![],
        note: Some("示例条目：编辑 ~/.config/sh0/hosts.toml 改成你自己的主机".into()),
    });
    store.save_hosts(&hosts)?;
    tracing::info!("已生成示例主机条目 example");
    Ok(())
}
