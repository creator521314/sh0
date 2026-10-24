//! 常驻 daemon：全部会话的唯一持有者（架构文档 §1.1）
//!
//! M1 的变化：会话编排从"这里手写循环"换成 `sh0_core::session::spawn_session`。
//! 这一层现在只剩三件事——把配置翻译成 `SessionConfig`、把会话事件转成 IPC 事件、
//! 以及方法分发。状态机与重连逻辑都在 core 里，可独立单测。
//!
//! M2 的变化：本文件长出 `exec` / `log` 两组方法（AI 接口的核心），
//! 并持有风险引擎、审计库、确认令牌与速率限制器。exec 的纯逻辑
//! （哨兵截取、令牌、限速）在 `exec.rs`，可单测。

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};

use sh0_core::session::{SessionConfig, SessionEvent, SessionHandle, spawn_session};
use sh0_core::sftp::{SftpClient, SftpError, SftpResult};
use sh0_core::{RiskEngine, RiskLevel, SessionSnapshot, SessionState, TransportFactory, new_session_id};
use sh0_ipc::protocol::{DaemonInfo, ErrorBody, events, methods};
use sh0_ipc::{EventHub, Handler, ServerMsg, serve};
use sh0_ssh::session::RusshFactory;
use sh0_store::{AppConfig, AuditDb, AuditFilter, AuditRecord, CredentialStore, Store};
use sh0_term::{GridView, MemoryBudget};

use crate::exec::{
    ConfirmStore, ExecData, ExecMode, ExecRequest, PendingConfirm, RateLimiter, SentinelScan,
    extract_output, new_sentinel, truncate_output,
};

/// 保留多少个已结束会话的快照，供事后查询
const RECENT_LIMIT: usize = 32;

/// 一次性会话等待就绪的宽限（秒）：连接失败不该把 exec 整体超时吃光
const CONNECT_GRACE_SECS: u64 = 20;

/// 确认令牌有效期（FR-7.4：无人值守时 AI 拿着 token 二次调用）
const CONFIRM_TTL_SECS: u64 = 300;

pub struct DaemonState {
    sessions: Mutex<HashMap<String, SessionHandle>>,
    /// 已结束会话的最后一份快照，只查不写
    recent: Mutex<VecDeque<SessionSnapshot>>,
    hub: EventHub,
    store: Arc<Store>,
    cfg: AppConfig,
    factory: Arc<dyn TransportFactory>,
    budget: Option<MemoryBudget>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    /// 风险引擎（内置 + 用户规则，FR-7）
    engine: Arc<RiskEngine>,
    /// 审计库；None = 关闭或打开失败
    audit: Option<Arc<AuditDb>>,
    /// 待确认令牌（FR-7.4）
    confirms: ConfirmStore,
    /// 单 actor 每分钟 exec 上限（FR-7.7）
    rate: RateLimiter,
}

impl DaemonState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<Store>,
        cfg: AppConfig,
        hub: EventHub,
        factory: Arc<dyn TransportFactory>,
        engine: Arc<RiskEngine>,
        audit: Option<Arc<AuditDb>>,
    ) -> (Arc<Self>, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let budget = if cfg.memory_budget_mb == 0 {
            None
        } else {
            Some(MemoryBudget::new(cfg.memory_budget_mb * 1024 * 1024))
        };
        let state = Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            recent: Mutex::new(VecDeque::new()),
            hub,
            store,
            cfg,
            factory,
            budget,
            shutdown: Mutex::new(Some(tx)),
            engine,
            audit,
            confirms: ConfirmStore::new(),
            rate: RateLimiter::new(),
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
    ) -> std::result::Result<SessionHandle, ErrorBody> {
        let host = self
            .store
            .find_host(selector)
            .map_err(|e| ErrorBody { code: "host_not_found".into(), message: e.to_string(), data: None })?;

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

        self.sessions.lock().await.insert(id.clone(), handle.clone());
        Ok(handle)
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
            data: None,
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

    // -----------------------------------------------------------------------
    // 审计（FR-8）
    // -----------------------------------------------------------------------

    /// 写一条审计记录。库没开时静默跳过（tracing 仍在）。
    fn audit(&self, rec: AuditRecord) {
        let Some(db) = &self.audit else {
            return;
        };
        if let Err(e) = db.record(&rec) {
            tracing::warn!("审计写入失败: {e}");
        }
    }

    // -----------------------------------------------------------------------
    // SFTP（FR-4.x / FR-6.6，M4）
    // -----------------------------------------------------------------------

    /// 取一个 SFTP 客户端：给了会话就**复用它的连接**，否则按主机一键式建连。
    ///
    /// 复用连接是 FR-4.1 的硬要求——同一条 TCP 上另开一个 channel，
    /// 而不是为一次 `ls` 再握手一遍。
    async fn sftp_for(
        &self,
        params: &Value,
    ) -> std::result::Result<(Box<dyn SftpClient>, String, Option<String>), ErrorBody> {
        let session = params
            .get("session")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let host = params
            .get("host")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        match (session, host) {
            (Some(sid), _) => {
                let h = {
                    let sessions = self.sessions.lock().await;
                    sessions.get(sid).cloned().ok_or_else(|| ErrorBody {
                        code: "session_unavailable".into(),
                        message: format!("会话 {sid} 不存在"),
                        data: None,
                    })?
                };
                let c = h.sftp().await.map_err(|e| ErrorBody {
                    code: "session_unavailable".into(),
                    message: e.to_string(),
                    data: None,
                })?;
                Ok((c, h.host.name.clone(), Some(sid.to_string())))
            }
            (None, Some(sel)) => {
                let h = self.store.find_host(sel).map_err(|e| ErrorBody {
                    code: "host_not_found".into(),
                    message: e.to_string(),
                    data: None,
                })?;
                let c = self.factory.open_sftp(&h).await.map_err(|e| ErrorBody {
                    code: "session_unavailable".into(),
                    message: e.to_string(),
                    data: None,
                })?;
                Ok((c, h.name.clone(), None))
            }
            _ => Err(ErrorBody {
                code: "invalid_args".into(),
                message: "需要 session 或 host 之一".into(),
                data: None,
            }),
        }
    }

    /// 每个 file.* 分支的收口：审计 + 错误映射。
    ///
    /// 集中在一个地方是为了不漏审计——文件操作同样要能回溯"谁改了哪个文件"。
    fn file_done<T>(
        &self,
        params: &Value,
        action: &str,
        detail: &str,
        host: &str,
        sid: Option<&str>,
        r: SftpResult<T>,
    ) -> std::result::Result<T, ErrorBody> {
        match r {
            Ok(v) => {
                self.audit_file(params, action, detail, host, sid, None);
                Ok(v)
            }
            Err(e) => {
                let msg = e.to_string();
                self.audit_file(params, action, detail, host, sid, Some(&msg));
                Err(sftp_err(e))
            }
        }
    }

    fn audit_file(
        &self,
        params: &Value,
        action: &str,
        detail: &str,
        host: &str,
        sid: Option<&str>,
        error: Option<&str>,
    ) {
        self.audit(AuditRecord {
            ts: now_rfc3339(),
            trace_id: params
                .get("trace_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            session_id: sid.map(str::to_string),
            host_name: Some(host.to_string()),
            actor: params
                .get("actor")
                .and_then(|v| v.as_str())
                .unwrap_or("tui")
                .to_string(),
            action: action.to_string(),
            // 路径记在 command 列：审计只有一个"目标"列，单开一列反而难检索
            command: Some(detail.to_string()),
            risk: None,
            decision: Some(if error.is_some() { "failed" } else { "executed" }.into()),
            exit_code: None,
            duration_ms: None,
            truncated: false,
            error: error.map(str::to_string),
            stdout: None,
            stderr: None,
        });
    }

    // -----------------------------------------------------------------------
    // exec（FR-6.4 / FR-6.8 / FR-7）
    // -----------------------------------------------------------------------

    /// exec.run 的参数解析与校验（不执行）
    fn parse_exec_request(&self, params: &Value) -> std::result::Result<ExecRequest, ErrorBody> {
        let session = params
            .get("session")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let host = params
            .get("host")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if session.is_none() && host.is_none() {
            return Err(ErrorBody {
                code: "invalid_args".into(),
                message: "需要 --session <id> 或 --host <选择器>".into(),
                data: None,
            });
        }
        let cmd = params
            .get("cmd")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if cmd.trim().is_empty() {
            return Err(ErrorBody {
                code: "invalid_args".into(),
                message: "命令不能为空".into(),
                data: None,
            });
        }
        let mode = match params.get("mode").and_then(|v| v.as_str()) {
            Some(m) => ExecMode::parse(m).ok_or_else(|| ErrorBody {
                code: "invalid_args".into(),
                message: format!("未知 mode: {m}（可选 shell / exec）"),
                data: None,
            })?,
            None => ExecMode::Shell,
        };
        let timeout_secs = params
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .map(|ms| ms / 1000)
            .unwrap_or(self.cfg.limits.exec_timeout_secs)
            .clamp(1, 3600);
        let max_output_bytes = params
            .get("max_output_bytes")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(self.cfg.limits.max_output_bytes)
            .clamp(1024, 16 * 1024 * 1024);

        Ok(ExecRequest {
            session,
            host,
            cmd,
            timeout: Duration::from_secs(timeout_secs),
            max_output_bytes,
            mode,
            actor: params
                .get("actor")
                .and_then(|v| v.as_str())
                .unwrap_or("ai")
                .to_string(),
            trace_id: params
                .get("trace_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        })
    }

    /// exec.run 主入口。返回 Ok(data) 或带错误码的失败。
    ///
    /// 注意 `timeout` 走 Ok(data) 返回——AI 需要看到部分输出，
    /// 退出码由 `sh0-ai` 依据 data 内容映射（架构文档 §6.3）。
    async fn exec_run(
        &self,
        params: &Value,
    ) -> std::result::Result<ExecData, ErrorBody> {
        let dry_run = params.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
        let yes = params.get("yes").and_then(|v| v.as_bool()).unwrap_or(false);
        let token = params
            .get("confirm_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let req = self.parse_exec_request(params)?;

        // 1) 风险判定（在一切之前：deny 的命令连会话都不该建）
        let host = match (&req.session, &req.host) {
            (Some(sid), _) => self
                .sessions
                .lock()
                .await
                .get(sid)
                .map(|h| h.host.clone())
                .ok_or_else(|| ErrorBody {
                    code: "session_unavailable".into(),
                    message: format!("会话 {sid} 不存在"),
                    data: None,
                })?,
            (None, Some(sel)) => self.store.find_host(sel).map_err(|e| ErrorBody {
                code: "host_not_found".into(),
                message: e.to_string(),
                data: None,
            })?,
            _ => unreachable!("parse_exec_request 已保证二者其一存在"),
        };
        let assessment = self.engine.assess(&req.cmd).capped(host.risk_cap);

        // 2) dry-run：只判定不执行（AI 自检用）
        if dry_run {
            return Ok(ExecData {
                session_id: req.session.clone(),
                host: host.name.clone(),
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: 0,
                truncated: false,
                mode: req.mode,
                risk: assessment.level,
                timeout: false,
            });
        }

        // 3) 三档分流
        match assessment.level {
            RiskLevel::Deny => {
                self.audit(AuditRecord {
                    ts: now_rfc3339(),
                    trace_id: req.trace_id.clone(),
                    session_id: req.session.clone(),
                    host_name: Some(host.name.clone()),
                    actor: req.actor.clone(),
                    action: "exec.run".into(),
                    command: Some(req.cmd.clone()),
                    risk: Some("deny".into()),
                    decision: Some("denied".into()),
                    exit_code: None,
                    duration_ms: None,
                    truncated: false,
                    error: assessment.reason.clone(),
                    stdout: None,
                    stderr: None,
                });
                Err(ErrorBody {
                    code: "risk_denied".into(),
                    message: format!(
                        "命令命中禁止规则: {}",
                        assessment.reason.as_deref().unwrap_or("未说明")
                    ),
                    data: None,
                })
            }
            RiskLevel::Confirm => {
                if let Some(t) = token {
                    // 带令牌：一次性消费；令牌与调用不匹配时拒绝（防挪用）
                    match self.confirms.take(&t) {
                        Some(p) if p.req.session == req.session && p.req.cmd == req.cmd => {
                            self.exec_after_risk(&req, &host.name, assessment.level, true)
                                .await
                        }
                        Some(_) => Err(ErrorBody {
                            code: "invalid_args".into(),
                            message: "确认令牌与本次调用不匹配（会话或命令不同）".into(),
                            data: None,
                        }),
                        None => Err(ErrorBody {
                            code: "invalid_args".into(),
                            message: "确认令牌无效或已过期，请重新发起调用".into(),
                            data: None,
                        }),
                    }
                } else if yes {
                    self.exec_after_risk(&req, &host.name, assessment.level, true).await
                } else {
                    // 无人值守：签发令牌并广播给可能在场的 TUI（FR-7.4）
                    let t = format!("cf_{}", new_session_id());
                    self.confirms.insert(
                        t.clone(),
                        PendingConfirm {
                            req: req.clone(),
                            host_name: host.name.clone(),
                            risk: RiskLevel::Confirm,
                            reason: assessment.reason.clone(),
                            expires_at: Instant::now() + Duration::from_secs(CONFIRM_TTL_SECS),
                        },
                    );
                    self.audit(AuditRecord {
                        ts: now_rfc3339(),
                        trace_id: req.trace_id.clone(),
                        session_id: req.session.clone(),
                        host_name: Some(host.name.clone()),
                        actor: req.actor.clone(),
                        action: "exec.run".into(),
                        command: Some(req.cmd.clone()),
                        risk: Some("confirm".into()),
                        decision: Some("needs_confirmation".into()),
                        exit_code: None,
                        duration_ms: None,
                        truncated: false,
                        error: assessment.reason.clone(),
                        stdout: None,
                        stderr: None,
                    });
                    self.hub.emit(ServerMsg::event(
                        events::CONFIRM_REQUESTED,
                        req.session.clone(),
                        json!({
                            "token": t,
                            "host": host.name,
                            "command": req.cmd,
                            "risk": "confirm",
                            "reason": assessment.reason,
                        }),
                    ));
                    // 无人值守：错误信封里必须带回令牌（AC-6）——
                    // `sh0 ai` 是一次性进程，收不到广播事件，拿不到令牌就永远无法二次确认
                    Err(ErrorBody {
                        code: "needs_confirmation".into(),
                        message: format!(
                            "命令需确认后执行: {}",
                            assessment.reason.as_deref().unwrap_or("命中确认规则")
                        ),
                        data: Some(json!({
                            "confirm_token": t,
                            "host": host.name,
                            "command": req.cmd,
                            "risk": "confirm",
                            "reason": assessment.reason,
                            "expires_in_secs": CONFIRM_TTL_SECS,
                        })),
                    })
                }
            }
            RiskLevel::Auto => {
                self.exec_after_risk(&req, &host.name, assessment.level, false)
                    .await
            }
        }
    }

    /// 风险已放行后的公共路径：限速 → 执行 → 审计
    async fn exec_after_risk(
        &self,
        req: &ExecRequest,
        host_name: &str,
        risk: RiskLevel,
        // 是否经人工放行（令牌确认或 `--yes`）：审计要能区分"判定就放行"与"人拍板过"
        confirmed: bool,
    ) -> std::result::Result<ExecData, ErrorBody> {
        // 速率限制（FR-7.7）：超限的调用不执行、不审计（避免刷屏）
        let limit = self.cfg.limits.rate_limit_per_min;
        if !self.rate.check(&req.actor, limit) {
            return Err(ErrorBody {
                code: "rate_limited".into(),
                message: format!(
                    "actor {actor} 超出每分钟 {limit} 次的上限",
                    actor = req.actor
                ),
                data: None,
            });
        }

        let started = Instant::now();
        let result = self.exec_execute(req, host_name).await;
        let duration_ms = started.elapsed().as_millis() as i64;

        match result {
            Ok(mut data) => {
                data.risk = risk;
                self.audit(AuditRecord {
                    ts: now_rfc3339(),
                    trace_id: req.trace_id.clone(),
                    session_id: data.session_id.clone(),
                    host_name: Some(data.host.clone()),
                    actor: req.actor.clone(),
                    action: "exec.run".into(),
                    command: Some(req.cmd.clone()),
                    risk: Some(risk_str(risk)),
                    decision: Some(if data.timeout {
                        "timeout".into()
                    } else if confirmed {
                        "confirmed".into()
                    } else {
                        "executed".into()
                    }),
                    exit_code: data.exit_code,
                    duration_ms: Some(duration_ms),
                    truncated: data.truncated,
                    error: None,
                    stdout: Some(data.stdout.clone()),
                    stderr: Some(data.stderr.clone()),
                });
                Ok(data)
            }
            Err(e) => {
                self.audit(AuditRecord {
                    ts: now_rfc3339(),
                    trace_id: req.trace_id.clone(),
                    session_id: req.session.clone(),
                    host_name: Some(host_name.to_string()),
                    actor: req.actor.clone(),
                    action: "exec.run".into(),
                    command: Some(req.cmd.clone()),
                    risk: Some(risk_str(risk)),
                    decision: Some("failed".into()),
                    exit_code: None,
                    duration_ms: Some(duration_ms),
                    truncated: false,
                    error: Some(e.message.clone()),
                    stdout: None,
                    stderr: None,
                });
                Err(e)
            }
        }
    }

    /// 真正把命令跑起来。`Err` 只用于"根本没跑成"（会话不可用、建连失败）；
    /// 跑成了但结果不理想（非零退出码、超时截断）走 Ok(data)。
    async fn exec_execute(
        &self,
        req: &ExecRequest,
        host_name: &str,
    ) -> std::result::Result<ExecData, ErrorBody> {
        match req.mode {
            ExecMode::Exec => self.exec_via_channel(req).await,
            ExecMode::Shell => self.exec_via_shell(req, host_name).await,
        }
    }

    /// exec channel 模式：一次连接一条命令，不碰会话表
    async fn exec_via_channel(
        &self,
        req: &ExecRequest,
    ) -> std::result::Result<ExecData, ErrorBody> {
        let host = match &req.host {
            Some(sel) => self.store.find_host(sel).map_err(|e| ErrorBody {
                code: "host_not_found".into(),
                message: e.to_string(),
                data: None,
            })?,
            None => {
                // 只给了 session：从会话里反查主机
                let sid = req.session.clone().unwrap_or_default();
                let sessions = self.sessions.lock().await;
                sessions
                    .get(&sid)
                    .map(|h| h.host.clone())
                    .ok_or_else(|| ErrorBody {
                        code: "session_unavailable".into(),
                        message: format!("会话 {sid} 不存在"),
                        data: None,
                    })?
            }
        };
        let started = Instant::now();
        let out = self
            .factory
            .exec(&host, &req.cmd, req.timeout)
            .await
            .map_err(|e| ErrorBody {
                code: "internal".into(),
                message: format!("exec channel 失败: {e}"),
                data: None,
            })?;
        let (stdout, stderr, truncated) =
            truncate_output(out.stdout, out.stderr, req.max_output_bytes);
        let timed_out = started.elapsed() >= req.timeout && out.exit_code.is_none();
        Ok(ExecData {
            session_id: req.session.clone(),
            host: host.name,
            exit_code: out.exit_code,
            stdout,
            stderr,
            duration_ms: started.elapsed().as_millis() as i64,
            truncated,
            mode: ExecMode::Exec,
            risk: RiskLevel::Auto,
            timeout: timed_out,
        })
    }

    /// shell 哨兵模式：在会话的交互 shell 上跑命令，靠哨兵行判断结束
    async fn exec_via_shell(
        &self,
        req: &ExecRequest,
        host_name: &str,
    ) -> std::result::Result<ExecData, ErrorBody> {
        // 1) 找到（或临时建立）会话
        let (handle, oneshot) = match &req.session {
            Some(sid) => {
                let sessions = self.sessions.lock().await;
                let h = sessions.get(sid).cloned().ok_or_else(|| ErrorBody {
                    code: "session_unavailable".into(),
                    message: format!("会话 {sid} 不存在"),
                    data: None,
                })?;
                drop(sessions);
                // 重连期间不排队执行——命令会在恢复后延迟跑，语义全错
                // （架构文档 §9：返回 session_unavailable，让调用方决定重试）
                if h.state().await != SessionState::Ready {
                    return Err(ErrorBody {
                        code: "session_unavailable".into(),
                        message: format!(
                            "会话 {sid} 当前状态为 {}，不能执行命令",
                            h.state().await.as_str()
                        ),
                        data: None,
                    });
                }
                (h, false)
            }
            None => {
                // 一键式调用（FR-6.8）：自动建连 → 执行 → 关闭
                let sel = req.host.clone().unwrap_or_default();
                let h = self.open(&sel, self.cfg.cols, self.cfg.rows).await?;
                (h, true)
            }
        };

        let result = self.exec_sentinel(&handle, req, host_name).await;
        if oneshot {
            // 一次性会话执行完就关，别在表里堆积
            let _ = self.close(&handle.id).await;
        }
        result
    }

    /// 哨兵回显法本体
    async fn exec_sentinel(
        &self,
        handle: &SessionHandle,
        req: &ExecRequest,
        host_name: &str,
    ) -> std::result::Result<ExecData, ErrorBody> {
        let started = Instant::now();

        // 一次性会话：等它就绪（或失败）。别把整体超时吃光。
        if handle.state().await != SessionState::Ready {
            let grace = Duration::from_secs(CONNECT_GRACE_SECS).min(req.timeout);
            let deadline = Instant::now() + grace;
            loop {
                let st = handle.state().await;
                if st == SessionState::Ready {
                    break;
                }
                if st == SessionState::Closed {
                    let snap = handle.snapshot().await;
                    return Err(ErrorBody {
                        code: "session_unavailable".into(),
                        message: format!(
                            "连接失败: {}",
                            snap.last_error.unwrap_or_else(|| "会话已关闭".into())
                        ),
                        data: None,
                    });
                }
                if Instant::now() >= deadline {
                    return Err(ErrorBody {
                        code: "session_unavailable".into(),
                        message: format!("等待连接就绪超时（{grace:?} 后仍未 ready）"),
                        data: None,
                    });
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        let sentinel = new_sentinel();
        // 发送前先留一份屏幕底样：回显定位与"新输出"边界都靠它
        let screen = handle.screen();
        // 列宽用于把 PTY 硬折的行还原成逻辑行（长命令回显会折行）
        let cols = {
            let s = screen.lock().await;
            s.cols()
        };
        let before = {
            let s = screen.lock().await;
            s.plain_text(true)
        };
        let payload = format!("{}\necho {}:$?\n", req.cmd, sentinel);
        handle
            .send(payload.as_bytes())
            .await
            .map_err(|e| ErrorBody {
                code: "session_unavailable".into(),
                message: format!("发送命令失败: {e}"),
                data: None,
            })?;

        // 轮询哨兵**输出行**（不是 contains——echo 命令的回显行也含哨兵 id，
        // 会在命令还没跑完时就误判结束）
        let deadline = started + req.timeout;
        let mut last_after;
        let (scan, timed_out) = loop {
            let after = {
                let s = screen.lock().await;
                s.plain_text(true)
            };
            last_after = after.clone();
            if scan_sentinel(&after, &sentinel).is_some() {
                // 哨兵已出现：交给 extract_output 结合屏底样截输出与退出码
                break (
                    extract_output(&before, &after, &req.cmd, &sentinel, cols),
                    false,
                );
            }
            if Instant::now() >= deadline {
                break (
                    extract_output(&before, &after, &req.cmd, &sentinel, cols),
                    true,
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };

        tracing::debug!(
            "exec 抓取 timed_out={timed_out} sentinel={sentinel} before={before:?} after={last_after:?}"
        );
        let (raw_stdout, exit_code) = match scan {
            SentinelScan::Complete { output, exit_code } => (output, Some(exit_code)),
            SentinelScan::Partial { output } => (output, None),
        };
        let (stdout, stderr, truncated) =
            truncate_output(raw_stdout, String::new(), req.max_output_bytes);

        Ok(ExecData {
            session_id: Some(handle.id.clone()),
            host: host_name.to_string(),
            exit_code,
            stdout,
            stderr,
            duration_ms: started.elapsed().as_millis() as i64,
            truncated,
            mode: ExecMode::Shell,
            risk: RiskLevel::Auto,
            timeout: timed_out,
        })
    }

    /// exec.confirm：有人值守时 TUI 弹窗的应答入口（FR-7.4）
    async fn exec_confirm(&self, params: &Value) -> std::result::Result<Value, ErrorBody> {
        let token = params
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let approve = params.get("approve").and_then(|v| v.as_bool()).unwrap_or(true);

        let Some(pending) = self.confirms.take(&token) else {
            return Err(ErrorBody {
                code: "invalid_args".into(),
                message: "确认令牌无效或已过期".into(),
                data: None,
            });
        };

        if !approve {
            self.hub.emit(ServerMsg::event(
                events::CONFIRM_RESOLVED,
                pending.req.session.clone(),
                json!({ "token": token, "approved": false, "summary": "用户拒绝了该命令" }),
            ));
            return Ok(json!({ "approved": false }));
        }

        // 批准 → 用令牌里存的原始请求直接执行
        let req = pending.req.clone();
        let started = Instant::now();
        let result = self.exec_execute(&req, &pending.host_name).await;
        let duration_ms = started.elapsed().as_millis() as i64;

        let (summary, ok) = match &result {
            Ok(data) => (
                format!(
                    "exit {:?} · {}ms{}",
                    data.exit_code,
                    duration_ms,
                    if data.truncated { " · 已截断" } else { "" }
                ),
                true,
            ),
            Err(e) => (e.message.clone(), false),
        };
        // 审计：确认决策与执行结果落在同一条流水上
        {
            let (exit_code, truncated, error, decision) = match &result {
                Ok(d) => (
                    d.exit_code,
                    d.truncated,
                    None,
                    if d.timeout { "timeout" } else { "confirmed" },
                ),
                Err(e) => (None, false, Some(e.message.clone()), "failed"),
            };
            self.audit(AuditRecord {
                ts: now_rfc3339(),
                trace_id: req.trace_id.clone(),
                session_id: req.session.clone(),
                host_name: Some(pending.host_name.clone()),
                actor: req.actor.clone(),
                action: "exec.confirm".into(),
                command: Some(req.cmd.clone()),
                risk: Some(risk_str(pending.risk)),
                decision: Some(decision.into()),
                exit_code,
                duration_ms: Some(duration_ms),
                truncated,
                error,
                stdout: result.as_ref().ok().map(|d| d.stdout.clone()),
                stderr: result.as_ref().ok().map(|d| d.stderr.clone()),
            });
        }
        self.hub.emit(ServerMsg::event(
            events::CONFIRM_RESOLVED,
            req.session.clone(),
            json!({ "token": token, "approved": true, "ok": ok, "summary": summary }),
        ));

        match result {
            Ok(data) => Ok(json!(data)),
            Err(e) => Err(e),
        }
    }

    /// log.query（FR-8.4）
    async fn log_query(&self, params: &Value) -> std::result::Result<Value, ErrorBody> {
        let Some(db) = &self.audit else {
            return Err(ErrorBody {
                code: "internal".into(),
                message: "审计未启用（config.toml 的 audit.enabled = false）".into(),
                data: None,
            });
        };
        let filter = AuditFilter {
            host: params.get("host").and_then(|v| v.as_str()).map(str::to_string),
            session: params.get("session").and_then(|v| v.as_str()).map(str::to_string),
            actor: params.get("actor").and_then(|v| v.as_str()).map(str::to_string),
            risk: params.get("risk").and_then(|v| v.as_str()).map(str::to_string),
            since: params.get("since").and_then(|v| v.as_str()).map(str::to_string),
            limit: params.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize),
        };
        let records = db
            .query(&filter)
            .map_err(|e| ErrorBody { code: "internal".into(), message: e.to_string(), data: None })?;
        Ok(json!({ "records": records, "count": records.len() }))
    }
}

/// 屏幕文本里扫描哨兵输出行（整行恰为 `SENTINEL:<整数>`）。
///
/// 与 `exec.rs::extract_output` 内部分开：那边输入是"已判定出现"的文本；
/// 这边在轮询里被反复调用，提前命中就能少等一轮。
/// 探测哨兵输出行是否已出现，返回其中携带的退出码。
///
/// 只管"跑完了没有"——输出内容由 `extract_output` 专门截取。
/// 两者必须分开：本函数逐行扫的是**整行恰为** `SENTINEL:<整数>` 的那一行的
/// 退出码，而输出需要结合发送前的屏底样才能算准。
fn scan_sentinel(text: &str, sentinel: &str) -> Option<i32> {
    let pat = format!("{sentinel}:");
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix(&pat)
            && let Ok(code) = rest.trim().parse::<i32>()
        {
            return Some(code);
        }
    }
    None
}

fn risk_str(level: RiskLevel) -> String {
    match level {
        RiskLevel::Auto => "auto",
        RiskLevel::Confirm => "confirm",
        RiskLevel::Deny => "deny",
    }
    .to_string()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// 取 file.* 的目标路径（必填）
fn file_path(params: &Value) -> std::result::Result<String, ErrorBody> {
    params
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ErrorBody {
            code: "invalid_args".into(),
            message: "缺少 path 参数".into(),
            data: None,
        })
}

/// SFTP 错误 → IPC 错误码。能分的都分开：
/// "路径不存在"与"权限不足"对 AI 的下一步决策完全不同。
fn sftp_err(e: SftpError) -> ErrorBody {
    let code = match &e {
        SftpError::NotFound(_) => "not_found",
        SftpError::PermissionDenied(_) => "permission_denied",
        SftpError::TooLarge(_) => "too_large",
        SftpError::Disconnected(_) => "session_unavailable",
        SftpError::NotImplemented(_) => "invalid_args",
        _ => "internal",
    };
    ErrorBody {
        code: code.into(),
        message: e.to_string(),
        data: None,
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// 把 `GridView` 序列化成 JSON 值（`screen read --mode grid`）
fn grid_view_json(v: &GridView) -> Value {
    json!({
        "cols": v.cols,
        "rows": v.rows,
        "cursor": v.cursor,
        "lines": v.lines,
    })
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
                    data: None,
                })?;
                let cols = params.get("cols").and_then(|v| v.as_u64()).unwrap_or(120) as u16;
                let rows = params.get("rows").and_then(|v| v.as_u64()).unwrap_or(32) as u16;
                let handle = self.open(host, cols, rows).await?;
                let actor = params
                    .get("actor")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tui")
                    .to_string();
                self.audit(AuditRecord {
                    ts: now_rfc3339(),
                    trace_id: params.get("trace_id").and_then(|v| v.as_str()).map(str::to_string),
                    session_id: Some(handle.id.clone()),
                    host_name: Some(handle.host.name.clone()),
                    actor,
                    action: "session.open".into(),
                    command: None,
                    risk: None,
                    decision: Some("opened".into()),
                    exit_code: None,
                    duration_ms: None,
                    truncated: false,
                    error: None,
                    stdout: None,
                    stderr: None,
                });
                Ok(json!({ "id": handle.id, "state": SessionState::Connecting.as_str(), "host": handle.host.name }))
            }

            methods::SESSION_CLOSE => {
                let sid = params.get("session").and_then(|v| v.as_str()).ok_or_else(|| ErrorBody {
                    code: "invalid_args".into(),
                    message: "缺少 session 参数".into(),
                    data: None,
                })?;
                let data = self.close(sid).await?;
                let actor = params
                    .get("actor")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tui")
                    .to_string();
                self.audit(AuditRecord {
                    ts: now_rfc3339(),
                    trace_id: params.get("trace_id").and_then(|v| v.as_str()).map(str::to_string),
                    session_id: Some(sid.to_string()),
                    host_name: None,
                    actor,
                    action: "session.close".into(),
                    command: None,
                    risk: None,
                    decision: Some("closed".into()),
                    exit_code: None,
                    duration_ms: None,
                    truncated: false,
                    error: None,
                    stdout: None,
                    stderr: None,
                });
                Ok(data)
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
                        data: None,
                    }),
                }
            }

            methods::SESSION_RECONNECT => {
                let sid = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let sessions = self.sessions.lock().await;
                let h = sessions.get(sid).ok_or_else(|| ErrorBody {
                    code: "session_unavailable".into(),
                    message: format!("会话 {sid} 不存在"),
                    data: None,
                })?;
                h.reconnect()
                    .await
                    .map_err(|e| ErrorBody { code: "internal".into(), message: e.to_string(), data: None })?;
                Ok(json!({ "reconnecting": true }))
            }

            methods::SESSION_CLEAR => {
                let sid = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let sessions = self.sessions.lock().await;
                let h = sessions.get(sid).ok_or_else(|| ErrorBody {
                    code: "session_unavailable".into(),
                    message: format!("会话 {sid} 不存在"),
                    data: None,
                })?;
                h.clear()
                    .await
                    .map_err(|e| ErrorBody { code: "internal".into(), message: e.to_string(), data: None })?;
                Ok(json!({ "cleared": true }))
            }

            methods::SCREEN_READ => {
                let sid = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let with_scrollback =
                    params.get("scrollback").and_then(|v| v.as_bool()).unwrap_or(true);
                let mode = params.get("mode").and_then(|v| v.as_str()).unwrap_or("plain");
                let lines_limit = params.get("lines").and_then(|v| v.as_u64()).map(|n| n as usize);

                let sessions = self.sessions.lock().await;
                let h = sessions.get(sid).ok_or_else(|| ErrorBody {
                    code: "session_unavailable".into(),
                    message: format!("会话 {sid} 不存在"),
                    data: None,
                })?;
                let screen = h.screen();
                let s = screen.lock().await;
                let data = match mode {
                    "grid" => {
                        let mut v = s.grid_view();
                        if let Some(n) = lines_limit
                            && v.lines.len() > n
                        {
                            v.lines = v.lines.split_off(v.lines.len() - n);
                        }
                        grid_view_json(&v)
                    }
                    _ => {
                        let text = s.plain_text(with_scrollback);
                        let text = match lines_limit {
                            Some(n) => {
                                let all: Vec<&str> = text.lines().collect();
                                let start = all.len().saturating_sub(n);
                                all[start..].join("\n")
                            }
                            None => text,
                        };
                        json!({ "text": text, "scrollback": with_scrollback })
                    }
                };
                Ok(data)
            }

            methods::INPUT_SEND => {
                let sid = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
                let data = params.get("data").and_then(|v| v.as_str()).unwrap_or("");
                let (h, host_name) = {
                    let sessions = self.sessions.lock().await;
                    let h = sessions.get(sid).cloned().ok_or_else(|| ErrorBody {
                        code: "session_unavailable".into(),
                        message: format!("会话 {sid} 不存在"),
                        data: None,
                    })?;
                    (h.clone(), h.host.name.clone())
                };
                h.send(data.as_bytes())
                    .await
                    .map_err(|e| ErrorBody { code: "internal".into(), message: e.to_string(), data: None })?;
                let actor = params
                    .get("actor")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tui")
                    .to_string();
                self.audit(AuditRecord {
                    ts: now_rfc3339(),
                    trace_id: params.get("trace_id").and_then(|v| v.as_str()).map(str::to_string),
                    session_id: Some(sid.to_string()),
                    host_name: Some(host_name),
                    actor,
                    action: "input.send".into(),
                    command: None,
                    risk: None,
                    decision: Some("sent".into()),
                    exit_code: None,
                    duration_ms: None,
                    truncated: false,
                    error: None,
                    stdout: None,
                    stderr: None,
                });
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
                    data: None,
                })?;
                h.resize(cols, rows)
                    .await
                    .map_err(|e| ErrorBody { code: "internal".into(), message: e.to_string(), data: None })?;
                Ok(json!({ "cols": cols, "rows": rows }))
            }

            methods::EXEC_RUN => self.exec_run(&params).await.map(|d| json!(d)),

            methods::EXEC_CONFIRM => self.exec_confirm(&params).await,

            methods::LOG_QUERY => self.log_query(&params).await,

            // ---- M4 · SFTP（FR-4.x / FR-6.6）----
            methods::FILE_LIST => {
                let path = file_path(&params)?;
                let (sftp, host_name, sid) = self.sftp_for(&params).await?;
                let r = sftp.list(&path).await;
                let _ = sftp.close().await;
                let entries = self.file_done(
                    &params,
                    "file.list",
                    &path,
                    &host_name,
                    sid.as_deref(),
                    r,
                )?;
                Ok(json!({ "path": path, "host": host_name, "entries": entries }))
            }

            methods::FILE_STAT => {
                let path = file_path(&params)?;
                let (sftp, host_name, sid) = self.sftp_for(&params).await?;
                let r = sftp.stat(&path).await;
                let _ = sftp.close().await;
                let entry =
                    self.file_done(&params, "file.stat", &path, &host_name, sid.as_deref(), r)?;
                Ok(json!({ "path": path, "host": host_name, "entry": entry }))
            }

            methods::FILE_READ => {
                let path = file_path(&params)?;
                let max_bytes = params
                    .get("max_bytes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1024 * 1024) as usize;
                let (sftp, host_name, sid) = self.sftp_for(&params).await?;
                let r = sftp.read(&path, max_bytes).await;
                let _ = sftp.close().await;
                let bytes =
                    self.file_done(&params, "file.read", &path, &host_name, sid.as_deref(), r)?;
                let size = bytes.len();
                // 文本直接给内容，二进制给 base64：AI 拿乱码当文本会静默出错
                Ok(match String::from_utf8(bytes) {
                    Ok(s) => json!({
                        "path": path, "host": host_name, "size": size,
                        "encoding": "utf8", "content": s,
                    }),
                    Err(e) => json!({
                        "path": path, "host": host_name, "size": size,
                        "encoding": "base64",
                        "content": base64_encode(&e.into_bytes()),
                    }),
                })
            }

            methods::FILE_WRITE => {
                let path = file_path(&params)?;
                let body = params.get("data").and_then(|v| v.as_str()).unwrap_or("");
                let (sftp, host_name, sid) = self.sftp_for(&params).await?;
                let n = body.len();
                let r = sftp.write(&path, body.as_bytes()).await;
                let _ = sftp.close().await;
                self.file_done(&params, "file.write", &path, &host_name, sid.as_deref(), r)?;
                Ok(json!({ "path": path, "host": host_name, "bytes": n }))
            }

            methods::FILE_MKDIR => {
                let path = file_path(&params)?;
                let (sftp, host_name, sid) = self.sftp_for(&params).await?;
                let r = sftp.mkdir(&path).await;
                let _ = sftp.close().await;
                self.file_done(&params, "file.mkdir", &path, &host_name, sid.as_deref(), r)?;
                Ok(json!({ "path": path, "host": host_name }))
            }

            methods::FILE_REMOVE => {
                let path = file_path(&params)?;
                let is_dir = params.get("dir").and_then(|v| v.as_bool()).unwrap_or(false);
                let (sftp, host_name, sid) = self.sftp_for(&params).await?;
                let r = if is_dir {
                    sftp.remove_dir(&path).await
                } else {
                    sftp.remove_file(&path).await
                };
                let _ = sftp.close().await;
                self.file_done(&params, "file.remove", &path, &host_name, sid.as_deref(), r)?;
                Ok(json!({ "path": path, "host": host_name, "dir": is_dir }))
            }

            methods::FILE_RENAME => {
                let from = params
                    .get("from")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ErrorBody {
                        code: "invalid_args".into(),
                        message: "缺少 from 参数".into(),
                        data: None,
                    })?
                    .to_string();
                let to = params
                    .get("to")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ErrorBody {
                        code: "invalid_args".into(),
                        message: "缺少 to 参数".into(),
                        data: None,
                    })?
                    .to_string();
                let (sftp, host_name, sid) = self.sftp_for(&params).await?;
                let r = sftp.rename(&from, &to).await;
                let _ = sftp.close().await;
                self.file_done(
                    &params,
                    "file.rename",
                    &format!("{from} -> {to}"),
                    &host_name,
                    sid.as_deref(),
                    r,
                )?;
                Ok(json!({ "from": from, "to": to, "host": host_name }))
            }

            methods::FILE_CHMOD => {
                let path = file_path(&params)?;
                let mode = params.get("mode").and_then(|v| v.as_u64()).ok_or_else(|| ErrorBody {
                    code: "invalid_args".into(),
                    message: "缺少 mode 参数".into(),
                    data: None,
                })? as u32;
                let (sftp, host_name, sid) = self.sftp_for(&params).await?;
                let r = sftp.chmod(&path, mode).await;
                let _ = sftp.close().await;
                self.file_done(&params, "file.chmod", &path, &host_name, sid.as_deref(), r)?;
                Ok(json!({ "path": path, "host": host_name, "mode": mode }))
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
                data: None,
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
        risk_cap: None,
    });
    store.save_hosts(&hosts)?;
    tracing::info!("已生成示例主机条目 example");
    Ok(())
}

/// M2 组件装配：风险引擎 + 审计库。run_foreground 与契约测试共用，
/// 保证测试验的就是生产装配（而不是另起一套宽松配置）。
fn assemble_m2(store: &Store, cfg: &AppConfig) -> (Arc<RiskEngine>, Option<Arc<AuditDb>>) {
    let user_rules = store.load_risk_rules(&cfg.risk.rules_file).unwrap_or_else(|e| {
        tracing::warn!("风险规则文件解析失败（{e}），本次使用内置规则");
        Vec::new()
    });
    let mut engine = RiskEngine::builtin().with_user_rules(&user_rules);
    engine.default_level = cfg.risk.unmatched;
    let audit = if cfg.audit.enabled {
        match AuditDb::open(
            &store.paths().audit_db(),
            cfg.audit.archive_output,
            cfg.audit.retention_days,
            cfg.audit.max_db_mb,
        ) {
            Ok(db) => Some(Arc::new(db)),
            Err(e) => {
                tracing::warn!("审计库打开失败（{}），AI 动作将不落库: {e}", store.paths().audit_db().display());
                None
            }
        }
    } else {
        None
    };
    (Arc::new(engine), audit)
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

    // ---- M2 组件装配 ----
    // 1) 凭据存储（钥匙串 / vault）→ SSH 工厂的秘密解析后端
    let creds = match CredentialStore::open(&paths) {
        Ok(c) => Some(Arc::new(c)),
        Err(e) => {
            tracing::warn!("凭据存储打开失败，钥匙串/vault 不可用: {e}");
            None
        }
    };
    let mut factory = RusshFactory::new(&known_hosts, cfg.keepalive_secs, cfg.term.clone());
    if let Some(c) = &creds {
        factory = factory.with_resolver(Arc::new(crate::creds_bridge::VaultResolver {
            store: c.clone(),
        }));
    }
    let factory: Arc<dyn TransportFactory> = Arc::new(factory);

    // 2) 风险引擎 + 3) 审计库
    let (engine, audit) = assemble_m2(&store, &cfg);

    let (state, shutdown_rx) =
        DaemonState::new(store, cfg, hub.clone(), factory, engine, audit);

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

/// 契约测试入口：注入 MockFactory，在随机回环端口起一个**完整装配**的 daemon。
///
/// 与 run_foreground 共用 assemble_m2，区别只有两点：传输层是假的、
/// 不写 daemon.json/pid 文件（测试用临时目录，避免和真实 daemon 的发现机制互相干扰）。
/// 返回监听地址、鉴权令牌和停止开关——调用方 drop/触发 stop_tx 即收线。
pub async fn run_for_test(
    store: Arc<Store>,
    cfg: AppConfig,
    factory: Arc<dyn TransportFactory>,
) -> anyhow::Result<(SocketAddr, String, oneshot::Sender<()>)> {
    let (engine, audit) = assemble_m2(&store, &cfg);
    let hub = EventHub::new();
    let (state, shutdown_rx) = DaemonState::new(store, cfg, hub.clone(), factory, engine, audit);

    let probe = TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = probe.local_addr()?;
    drop(probe);
    let token = new_session_id();
    let client_token = token.clone();

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        tokio::select! {
            res = serve(addr, token, state, hub) => {
                if let Err(e) = res {
                    tracing::warn!("测试 daemon 退出: {e}");
                }
            }
            _ = shutdown_rx => {}
            _ = stop_rx => {}
        }
    });

    Ok((addr, client_token, stop_tx))
}
