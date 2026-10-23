//! 常驻 daemon：全部会话的唯一持有者（架构文档 §1.1）
//!
//! M1 的变化：会话编排从"这里手写循环"换成 `sh0_core::session::spawn_session`。
//! 这一层现在只剩三件事——把配置翻译成 `SessionConfig`、把会话事件转成 IPC 事件、
//! 以及方法分发。状态机与重连逻辑都在 core 里，可独立单测。

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};

use sh0_core::session::{SessionConfig, SessionEvent, SessionHandle, spawn_session};
use sh0_core::{SessionSnapshot, SessionState, new_session_id};
use sh0_ipc::protocol::{DaemonInfo, ErrorBody, events, methods};
use sh0_ipc::{EventHub, Handler, ServerMsg, serve};
use sh0_ssh::session::RusshFactory;
use sh0_store::{AppConfig, Store};
use sh0_term::MemoryBudget;

/// 保留多少个已结束会话的快照，供事后查询
const RECENT_LIMIT: usize = 32;

pub struct DaemonState {
    sessions: Mutex<HashMap<String, SessionHandle>>,
    /// 已结束会话的最后一份快照，只查不写
    recent: Mutex<VecDeque<SessionSnapshot>>,
    hub: EventHub,
    store: Arc<Store>,
    cfg: AppConfig,
    factory: Arc<RusshFactory>,
    budget: Option<MemoryBudget>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
}

impl DaemonState {
    pub fn new(
        store: Arc<Store>,
        cfg: AppConfig,
        known_hosts: std::path::PathBuf,
        hub: EventHub,
    ) -> (Arc<Self>, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let budget = if cfg.memory_budget_mb == 0 {
            None
        } else {
            Some(MemoryBudget::new(cfg.memory_budget_mb * 1024 * 1024))
        };
        let factory = Arc::new(RusshFactory::new(
            known_hosts,
            cfg.keepalive_secs,
            cfg.term.clone(),
        ));
        let state = Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            recent: Mutex::new(VecDeque::new()),
            hub,
            store,
            cfg,
            factory,
            budget,
            shutdown: Mutex::new(Some(tx)),
        });
        (state, rx)
    }

    fn session_config(&self, cols: u16, rows: u16) -> SessionConfig {
        let r = &self.cfg.reconnect;
        SessionConfig {
            cols,
            rows,
            scrollback_lines: self.cfg.scrollback_lines,
            reconnect: sh0_core::session::ReconnectPolicy {
                base: Duration::from_millis(r.base_ms),
                max_delay: Duration::from_millis(r.max_delay_ms),
                max_attempts: r.max_attempts,
                jitter: r.jitter,
            },
            budget: self.budget.clone(),
            mark_reconnect: r.mark,
        }
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

        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<SessionEvent>();
        let cfg = self.session_config(cols, rows);
        let handle = spawn_session(host.clone(), self.factory.clone(), cfg, ev_tx);
        let id = handle.id.clone();

        // 会话事件 → IPC 广播
        let hub = self.hub.clone();
        tokio::spawn(async move {
            while let Some(ev) = ev_rx.recv().await {
                let msg = match ev {
                    SessionEvent::Output { session, data } => {
                        ServerMsg::event(events::OUTPUT, Some(session), json!({ "data": data }))
                    }
                    SessionEvent::StateChanged { session, state } => ServerMsg::event(
                        events::STATE,
                        Some(session),
                        json!({ "state": state.as_str() }),
                    ),
                    SessionEvent::Reconnected { session, attempt } => ServerMsg::event(
                        events::RECONNECTED,
                        Some(session),
                        json!({ "attempt": attempt }),
                    ),
                    SessionEvent::Closed { session, reason } => {
                        ServerMsg::event(events::CLOSED, Some(session), json!({ "reason": reason }))
                    }
                };
                hub.emit(msg);
            }
        });

        self.sessions.lock().await.insert(id.clone(), handle);

        Ok(json!({ "id": id, "state": SessionState::Connecting.as_str(), "host": host.name }))
    }

    async fn close(&self, selector: &str) -> std::result::Result<Value, ErrorBody> {
        let mut sessions = self.sessions.lock().await;

        if selector == "*" {
            let n = sessions.len();
            for (id, h) in sessions.drain() {
                let _ = h.close().await;
                self.hub.emit(ServerMsg::event(
                    events::CLOSED,
                    Some(id),
                    json!({ "reason": "closed by request" }),
                ));
            }
            return Ok(json!({ "closed": n }));
        }

        let h = sessions.remove(selector).ok_or_else(|| ErrorBody {
            code: "session_unavailable".into(),
            message: format!("会话 {selector} 不存在"),
        })?;
        let _ = h.close().await;
        // 与 reap_closed 一样留一份快照，否则关闭之后就再也查不到它了
        let mut recent = self.recent.lock().await;
        if recent.len() >= RECENT_LIMIT {
            recent.pop_front();
        }
        recent.push_back(h.snapshot().await);
        self.hub.emit(ServerMsg::event(
            events::CLOSED,
            Some(h.id.clone()),
            json!({ "reason": "closed by request" }),
        ));
        Ok(json!({ "closed": 1 }))
    }

    /// 会话结束（Closed）后从表里摘掉，避免列表里堆积僵尸条目。
    ///
    /// 摘掉前把快照转存到 `recent`：会话一旦消失，`session status` 就无从
    /// 回答"它为什么没了"，而这是 AI 调用时最常见的疑问。
    async fn reap_closed(&self) {
        let mut sessions = self.sessions.lock().await;
        let mut gone = Vec::new();
        for (id, h) in sessions.iter() {
            if h.state().await == SessionState::Closed {
                gone.push(id.clone());
            }
        }
        for id in gone {
            let Some(h) = sessions.remove(&id) else {
                continue;
            };
            let mut recent = self.recent.lock().await;
            if recent.len() >= RECENT_LIMIT {
                recent.pop_front();
            }
            recent.push_back(h.snapshot().await);
        }
    }

    async fn snapshot_list(&self) -> Vec<SessionSnapshot> {
        // 已结束的会话不再出现在列表里，否则 TUI 的标签栏会越积越多
        self.reap_closed().await;
        let sessions = self.sessions.lock().await;
        let mut out = Vec::with_capacity(sessions.len());
        for h in sessions.values() {
            out.push(h.snapshot().await);
        }
        out
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
                if let Some(s) = sessions.into_iter().find(|s| s.id == sid) {
                    return Ok(json!(s));
                }
                // 活跃列表里没有：可能是已结束的会话（结束原因在 last_error 里）
                let recent = self.recent.lock().await;
                match recent.iter().rev().find(|s| s.id == sid) {
                    Some(s) => Ok(json!(s)),
                    None => Err(ErrorBody {
                        code: "session_unavailable".into(),
                        message: format!("会话 {sid} 不存在"),
                    }),
                }
            }

            methods::SESSION_RECONNECT => {
                let sid = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let sessions = self.sessions.lock().await;
                let h = sessions.get(sid).ok_or_else(|| ErrorBody {
                    code: "session_unavailable".into(),
                    message: format!("会话 {sid} 不存在"),
                })?;
                h.reconnect()
                    .await
                    .map_err(|e| ErrorBody { code: "internal".into(), message: e.to_string() })?;
                Ok(json!({ "reconnecting": true }))
            }

            methods::SESSION_CLEAR => {
                let sid = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let sessions = self.sessions.lock().await;
                let h = sessions.get(sid).ok_or_else(|| ErrorBody {
                    code: "session_unavailable".into(),
                    message: format!("会话 {sid} 不存在"),
                })?;
                h.clear()
                    .await
                    .map_err(|e| ErrorBody { code: "internal".into(), message: e.to_string() })?;
                Ok(json!({ "cleared": true }))
            }

            methods::SCREEN_READ => {
                let sid = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let with_scrollback =
                    params.get("scrollback").and_then(|v| v.as_bool()).unwrap_or(true);
                let sessions = self.sessions.lock().await;
                let h = sessions.get(sid).ok_or_else(|| ErrorBody {
                    code: "session_unavailable".into(),
                    message: format!("会话 {sid} 不存在"),
                })?;
                let text = h
                    .read_screen(with_scrollback)
                    .await
                    .map_err(|e| ErrorBody { code: "internal".into(), message: e.to_string() })?;
                Ok(json!({ "text": text, "scrollback": with_scrollback }))
            }

            methods::INPUT_SEND => {
                let sid = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let data = params.get("data").and_then(|v| v.as_str()).unwrap_or("");
                let sessions = self.sessions.lock().await;
                let h = sessions.get(sid).ok_or_else(|| ErrorBody {
                    code: "session_unavailable".into(),
                    message: format!("会话 {sid} 不存在"),
                })?;
                h.send(data.as_bytes())
                    .await
                    .map_err(|e| ErrorBody { code: "internal".into(), message: e.to_string() })?;
                Ok(json!({ "sent": data.len() }))
            }

            methods::RESIZE => {
                let sid = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let cols = params.get("cols").and_then(|v| v.as_u64()).unwrap_or(120) as u16;
                let rows = params.get("rows").and_then(|v| v.as_u64()).unwrap_or(32) as u16;
                let sessions = self.sessions.lock().await;
                let h = sessions.get(sid).ok_or_else(|| ErrorBody {
                    code: "session_unavailable".into(),
                    message: format!("会话 {sid} 不存在"),
                })?;
                h.resize(cols, rows)
                    .await
                    .map_err(|e| ErrorBody { code: "internal".into(), message: e.to_string() })?;
                Ok(json!({ "cols": cols, "rows": rows }))
            }

            methods::SHUTDOWN => {
                // 先优雅关闭所有会话，再退出
                self.close("*").await?;
                self.reap_closed().await;
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

/// 首次使用时生成一个示例主机条目，避免用户面对空配置无从下手
pub fn seed_example_host(store: &Store) -> anyhow::Result<()> {
    let mut hosts = store.load_hosts()?;
    if !hosts.hosts.is_empty() {
        return Ok(());
    }
    hosts.hosts.push(sh0_core::Host {
        id: sh0_core::new_host_id(),
        name: "example".into(),
        group: vec!["demo".into()],
        addr: "127.0.0.1".into(),
        port: 22,
        user: std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "root".into()),
        auth: sh0_core::CredentialRef::Agent,
        tags: vec![],
        note: Some("示例条目：编辑 ~/.config/sh0/hosts.toml 改成你自己的主机".into()),
    });
    store.save_hosts(&hosts)?;
    tracing::info!("已生成示例主机条目 example");
    Ok(())
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
