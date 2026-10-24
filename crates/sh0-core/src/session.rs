//! 会话运行时：状态机、断线检测、指数退避重连（需求 FR-2.5 / FR-2.6，验收 AC-3）。
//!
//! 一个会话 = 一个 tokio task（actor）+ 一份 `Screen` + 一条输入队列。
//! 所有状态变更都通过 `SessionEvent` 广播出去，daemon 再转成 IPC 事件推给前端。
//!
//! 之所以放在 core 而不是 daemon：重连策略与状态机是纯逻辑，必须能在没有
//! SSH、没有 IPC 的情况下单测——这类 bug（退避算错、状态卡死）一旦进了线上
//! 就只能靠用户碰运气复现。

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use sh0_term::{MemoryBudget, Screen, ScreenOptions};

use crate::ids::{SessionId, new_session_id};
use crate::model::{Host, SessionSnapshot, SessionState};
use crate::transport::{SessionTransport, TransportEvent, TransportFactory, TransportResult};

/// 重连退避策略
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    /// 首次重试前的等待
    pub base: Duration,
    /// 单次等待上限
    pub max_delay: Duration,
    /// 最大尝试次数；None 表示无限重试
    pub max_attempts: Option<u32>,
    /// 是否加抖动。多会话同时断线时，抖动能避免重连风暴
    pub jitter: bool,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            max_attempts: Some(10),
            jitter: true,
        }
    }
}

impl ReconnectPolicy {
    /// 第 attempt 次重试（0-based）的等待时长。
    ///
    /// 抖动是**确定性**的：由 seed（会话 id 派生）与 attempt 算出，
    /// 这样单测可以断言具体数值，而多会话之间仍然互相错开。
    pub fn backoff_delay(&self, attempt: u32, seed: u64) -> Duration {
        let exp = self.base.as_millis() as u64 * 2u64.saturating_pow(attempt.min(10));
        let capped = exp.min(self.max_delay.as_millis() as u64);
        let jitter = if self.jitter {
            fnv1a(seed ^ (attempt as u64).wrapping_mul(0x9E37_79B9)) % (capped / 4 + 1)
        } else {
            0
        };
        Duration::from_millis(capped + jitter)
    }
}

fn fnv1a(v: u64) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in v.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// 会话配置
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub cols: u16,
    pub rows: u16,
    pub scrollback_lines: usize,
    pub reconnect: ReconnectPolicy,
    pub budget: Option<MemoryBudget>,
    /// 重连时是否在历史输出里插入分隔线（AC-3）
    pub mark_reconnect: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            cols: 120,
            rows: 32,
            scrollback_lines: 10_000,
            reconnect: ReconnectPolicy::default(),
            budget: None,
            mark_reconnect: true,
        }
    }
}

/// 会话广播给 daemon 的事件
#[derive(Debug, Clone)]
pub enum SessionEvent {
    StateChanged {
        session: SessionId,
        state: SessionState,
    },
    /// 远端输出。已按 UTF-8 边界切好，前端可直接喂给自己的 Screen
    Output {
        session: SessionId,
        data: String,
    },
    Reconnected {
        session: SessionId,
        attempt: u32,
    },
    Closed {
        session: SessionId,
        reason: Option<String>,
    },
}

/// 发给 actor 的控制命令
#[derive(Debug)]
enum SessionCmd {
    Input(Vec<u8>),
    Resize(u16, u16),
    Reconnect,
    Close,
    Clear,
}

/// 命令处理结果。
///
/// 之所以不直接在 `handle_one` 里重连：`handle_one` 会被重连等待路径调用，
/// 而重连路径本身就在重连，形成 async 递归（Rust 要求装箱）。把"需要重连"
/// 作为返回值交给调用方，递归就断了。
enum CmdOutcome {
    Continue,
    /// 会话应当结束
    Closed,
    /// 调用方需要发起重连
    NeedsReconnect,
}

/// 会话句柄：daemon 与外部只通过这个对象操作会话。
///
/// `Clone` 是廉价的可共享引用（内部全是 `Arc` + channel）——
/// daemon 的 exec 路径需要从会话表里取出一份来跑命令，
/// 同时不把表锁整个锁住。
#[derive(Clone)]
pub struct SessionHandle {
    pub id: SessionId,
    pub host: Host,
    state: Arc<Mutex<SessionState>>,
    screen: Arc<Mutex<Screen>>,
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
    reconnect_count: Arc<AtomicU32>,
    size: Arc<Mutex<(u16, u16)>>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl SessionHandle {
    pub async fn send(&self, data: &[u8]) -> Result<(), SessionCmdError> {
        self.cmd_tx
            .send(SessionCmd::Input(data.to_vec()))
            .map_err(|_| SessionCmdError::Gone)
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<(), SessionCmdError> {
        *self.size.lock().await = (cols, rows);
        self.cmd_tx
            .send(SessionCmd::Resize(cols, rows))
            .map_err(|_| SessionCmdError::Gone)
    }

    pub async fn reconnect(&self) -> Result<(), SessionCmdError> {
        self.cmd_tx
            .send(SessionCmd::Reconnect)
            .map_err(|_| SessionCmdError::Gone)
    }

    pub async fn close(&self) -> Result<(), SessionCmdError> {
        self.cmd_tx.send(SessionCmd::Close).map_err(|_| SessionCmdError::Gone)
    }

    pub async fn clear(&self) -> Result<(), SessionCmdError> {
        self.cmd_tx.send(SessionCmd::Clear).map_err(|_| SessionCmdError::Gone)
    }

    /// 读屏（`ai screen read` 与 TUI 共用）。
    ///
    /// 直接读共享的 `Screen`，不走 actor 的命令通道——屏幕内容由 Mutex 保护，
    /// 一致性不依赖 actor；更重要的是**会话结束后屏幕仍可查询**，
    /// 这正是排查「为什么连不上」最需要的那一眼。
    pub async fn read_screen(&self, with_scrollback: bool) -> Result<String, SessionCmdError> {
        let screen = self.screen.lock().await;
        Ok(screen.plain_text(with_scrollback))
    }

    pub async fn state(&self) -> SessionState {
        *self.state.lock().await
    }

    pub async fn size(&self) -> (u16, u16) {
        *self.size.lock().await
    }

    /// daemon 侧的 Screen（AI 读屏与审计走这一份；TUI 各自维护自己的一份）
    pub fn screen(&self) -> Arc<Mutex<Screen>> {
        self.screen.clone()
    }

    pub async fn snapshot(&self) -> SessionSnapshot {
        let (cols, rows) = self.size().await;
        SessionSnapshot {
            id: self.id.clone(),
            host_id: self.host.id.clone(),
            host_name: self.host.name.clone(),
            state: self.state().await,
            cols,
            rows,
            reconnect_count: self.reconnect_count.load(Ordering::Relaxed),
            last_error: self.last_error.lock().await.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionCmdError {
    #[error("会话已结束")]
    Gone,
}

/// 启动一个会话 actor
pub fn spawn_session(
    host: Host,
    factory: Arc<dyn TransportFactory>,
    cfg: SessionConfig,
    events: mpsc::UnboundedSender<SessionEvent>,
) -> SessionHandle {
    let id = new_session_id();
    let mut opts = ScreenOptions::new(cfg.cols as usize, cfg.rows as usize);
    opts.scrollback_lines = cfg.scrollback_lines;
    if let Some(b) = cfg.budget.clone() {
        opts = opts.with_budget(b);
    }
    let screen = Arc::new(Mutex::new(Screen::new(opts)));
    let state = Arc::new(Mutex::new(SessionState::Connecting));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let reconnect_count = Arc::new(AtomicU32::new(0));
    let size = Arc::new(Mutex::new((cfg.cols, cfg.rows)));
    let last_error = Arc::new(Mutex::new(None::<String>));

    let actor = SessionActor {
        id: id.clone(),
        host: host.clone(),
        factory,
        cfg,
        screen: screen.clone(),
        state: state.clone(),
        events,
        cmd_rx,
        reconnect_count: reconnect_count.clone(),
        transport: None,
        last_error: last_error.clone(),
        pending_utf8: Vec::new(),
        seed: fnv1a(id.bytes().fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64))),
    };

    tokio::spawn(actor.run());

    SessionHandle {
        id,
        host,
        state,
        screen,
        cmd_tx,
        reconnect_count,
        size,
        last_error,
    }
}

struct SessionActor {
    id: SessionId,
    host: Host,
    factory: Arc<dyn TransportFactory>,
    cfg: SessionConfig,
    screen: Arc<Mutex<Screen>>,
    state: Arc<Mutex<SessionState>>,
    events: mpsc::UnboundedSender<SessionEvent>,
    cmd_rx: mpsc::UnboundedReceiver<SessionCmd>,
    reconnect_count: Arc<AtomicU32>,
    transport: Option<Box<dyn SessionTransport + Send>>,
    last_error: Arc<Mutex<Option<String>>>,
    /// 跨块截断的多字节字符残余，攒够再发（见 `emit_data`）
    pending_utf8: Vec<u8>,
    seed: u64,
}

impl SessionActor {
    async fn run(mut self) {
        // 首次连接
        match self.connect().await {
            Ok(()) => self.set_state(SessionState::Ready).await,
            Err(e) => {
                warn!("会话 {} 首次连接失败: {}", self.id, e);
                self.set_error(e.to_string()).await;
                self.notice(&e.to_string()).await;
                let ok = self.reconnect_loop().await;
                if !ok {
                    self.finish(Some(e.to_string())).await;
                    return;
                }
            }
        }

        // 主循环：轮询而非 select!——transport 的 wait/send 都要 &mut self，
        // 同时构造两个 future 会撞上借用检查（M0 已在 daemon 里踩过一次）。
        loop {
            if self.handle_commands().await {
                return;
            }

            let Some(transport) = self.transport.as_mut() else {
                return;
            };

            match tokio::time::timeout(Duration::from_millis(5), transport.wait()).await {
                Ok(Some(TransportEvent::Data(data))) => {
                    let mut screen = self.screen.lock().await;
                    screen.feed(&data);
                    // 远端可能要求回话（如光标位置报告）
                    let resp = screen.take_response();
                    drop(screen);
                    if !resp.is_empty() {
                        let _ = transport.send(&resp).await;
                    }
                    self.emit_data(&data).await;
                }
                Ok(Some(TransportEvent::Eof)) => {
                    info!("会话 {} 收到 EOF", self.id);
                    self.finish(None).await;
                    return;
                }
                Ok(Some(TransportEvent::Exit(code))) => {
                    info!("会话 {} 退出，code={}", self.id, code);
                    self.finish(Some(format!("远端退出码 {code}"))).await;
                    return;
                }
                Ok(Some(TransportEvent::Broken(reason))) => {
                    warn!("会话 {} 断开: {}", self.id, reason);
                    self.set_error(reason.clone()).await;
                    self.notice(&format!("连接断开：{reason}")).await;
                    if !self.reconnect_loop().await {
                        self.finish(Some(reason)).await;
                        return;
                    }
                }
                Ok(None) => {
                    // 连接彻底关闭
                    self.notice("远端关闭了连接").await;
                    if !self.reconnect_loop().await {
                        self.finish(Some("连接已关闭".into())).await;
                        return;
                    }
                }
                Err(_) => {
                    // 超时：没有数据，继续下一轮
                }
            }
        }
    }

    /// 处理积压的控制命令。返回 true 表示会话应当结束。
    async fn handle_commands(&mut self) -> bool {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match self.handle_one(cmd).await {
                CmdOutcome::Continue => {}
                CmdOutcome::Closed => return true,
                CmdOutcome::NeedsReconnect => {
                    if !self.reconnect_loop().await {
                        self.finish(Some("手动重连失败".into())).await;
                        return true;
                    }
                }
            }
        }
        false
    }

    async fn handle_one(&mut self, cmd: SessionCmd) -> CmdOutcome {
        match cmd {
            SessionCmd::Input(bytes) => {
                if let Some(t) = self.transport.as_mut()
                    && let Err(e) = t.send(&bytes).await
                {
                    warn!("会话 {} 发送输入失败: {}", self.id, e);
                }
                CmdOutcome::Continue
            }
            SessionCmd::Resize(cols, rows) => {
                let mut screen = self.screen.lock().await;
                screen.resize(cols as usize, rows as usize);
                drop(screen);
                if let Some(t) = self.transport.as_mut() {
                    let _ = t.resize(cols, rows).await;
                }
                CmdOutcome::Continue
            }
            SessionCmd::Reconnect => {
                if let Some(t) = self.transport.as_mut() {
                    let _ = t.close().await;
                }
                self.transport = None;
                CmdOutcome::NeedsReconnect
            }
            SessionCmd::Clear => {
                let mut screen = self.screen.lock().await;
                screen.clear();
                CmdOutcome::Continue
            }
            SessionCmd::Close => {
                self.finish(None).await;
                CmdOutcome::Closed
            }
        }
    }

    /// 重连前的等待：等满 `delay`，期间**持续**处理命令。
    /// 返回 true 表示收到 Close，应当放弃重连。
    ///
    /// 不能先睡完再一次性收命令：退避可能长达数秒，这期间用户拖窗口、
    /// 敲命令、翻屏都是正常的，等到下一次重试才响应会明显卡顿。
    /// 这里改成边睡边收，命令一到就处理。
    async fn wait_and_drain(&mut self, delay: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + delay;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            match tokio::time::timeout(left, self.cmd_rx.recv()).await {
                Ok(Some(cmd)) => {
                    match self.handle_one(cmd).await {
                        CmdOutcome::Closed => return true,
                        // 已经在重连了，再来一次 Reconnect 没有意义
                        CmdOutcome::NeedsReconnect | CmdOutcome::Continue => {}
                    }
                }
                // 发送端全挂了：没人会再下命令，等同于放弃
                Ok(None) => return true,
                Err(_) => return false,
            }
        }
    }

    /// 建立一条新连接，**不改动会话状态**。
    ///
    /// 状态由调用方决定：首次连接是 `Connecting`，重连期间必须保持
    /// `Reconnecting` —— 否则每次重试都会把状态打回 `Connecting`，
    /// 调用方永远看不到"正在重连"，也无法区分首次连接与重连。
    async fn try_connect(&mut self) -> TransportResult<()> {
        let (cols, rows) = (self.cfg.cols, self.cfg.rows);
        let t = self.factory.connect(&self.host, cols, rows).await?;
        self.transport = Some(t);
        Ok(())
    }

    async fn connect(&mut self) -> TransportResult<()> {
        self.set_state(SessionState::Connecting).await;
        self.try_connect().await
    }

    /// 把本地诊断信息写进屏幕缓冲。
    ///
    /// 连接失败时屏幕是一片空白，用户和 AI 都无从判断发生了什么
    /// （AI 读屏拿到空字符串会误判成"命令执行成功但无输出"）。
    async fn notice(&mut self, text: &str) {
        let mut screen = self.screen.lock().await;
        screen.push_notice(text);
    }

    /// 记录最近一次错误，供快照与 `session status` 查询
    async fn set_error(&mut self, text: String) {
        *self.last_error.lock().await = Some(text);
    }

    /// 指数退避重连。成功返回 true。
    async fn reconnect_loop(&mut self) -> bool {
        self.set_state(SessionState::Reconnecting).await;
        let mut attempt = 0u32;

        loop {
            if let Some(max) = self.cfg.reconnect.max_attempts
                && attempt >= max
            {
                warn!("会话 {} 重连 {} 次仍失败，放弃", self.id, attempt);
                return false;
            }

            let delay = self.cfg.reconnect.backoff_delay(attempt, self.seed);
            info!("会话 {} 将在 {:?} 后第 {} 次重连", self.id, delay, attempt + 1);
            // 等待期间持续处理命令（不复用 handle_commands：它内部会触发重连，
            // 与此处形成异步递归，必须装箱，纯属自找麻烦）
            if self.wait_and_drain(delay).await {
                return false;
            }

            match self.try_connect().await {
                Ok(()) => {
                    attempt += 1;
                    self.reconnect_count.fetch_add(1, Ordering::Relaxed);
                    if self.cfg.mark_reconnect {
                        let mut screen = self.screen.lock().await;
                        screen.mark_reconnect(&format!("第 {attempt} 次重连成功"));
                    }
                    self.set_state(SessionState::Ready).await;
                    let _ = self.events.send(SessionEvent::Reconnected {
                        session: self.id.clone(),
                        attempt,
                    });
                    info!("会话 {} 重连成功（第 {} 次）", self.id, attempt);
                    return true;
                }
                Err(e) => {
                    warn!("会话 {} 第 {} 次重连失败: {}", self.id, attempt + 1, e);
                    self.set_error(e.to_string()).await;
                    self.notice(&format!("第 {} 次重连未成功：{e}", attempt + 1))
                        .await;
                    attempt += 1;
                }
            }
        }
    }

    async fn set_state(&mut self, s: SessionState) {
        *self.state.lock().await = s;
        let _ = self.events.send(SessionEvent::StateChanged {
            session: self.id.clone(),
            state: s,
        });
    }

    /// 把远端字节安全地转成字符串广播出去。
    ///
    /// 一次 Data 事件可能在多字节字符中间切断（TCP 分片），直接
    /// `from_utf8_lossy` 会插入 U+FFFD 让前端渲染错位。因此这里攒住残余字节。
    async fn emit_data(&mut self, data: &[u8]) {
        self.pending_utf8.extend_from_slice(data);
        let valid = match std::str::from_utf8(&self.pending_utf8) {
            Ok(_) => self.pending_utf8.len(),
            Err(e) => e.valid_up_to(),
        };
        if valid == 0 {
            return;
        }
        let s = String::from_utf8_lossy(&self.pending_utf8[..valid]).to_string();
        self.pending_utf8.drain(..valid);
        let _ = self.events.send(SessionEvent::Output {
            session: self.id.clone(),
            data: s,
        });
    }

    async fn finish(&mut self, reason: Option<String>) {
        if let Some(t) = self.transport.as_mut() {
            let _ = t.close().await;
        }
        self.transport = None;
        // 会话结束后屏幕缓冲仍可查询，把终止原因留在里面，
        // 这样即便会话已从列表里摘掉，AI / 用户仍能查到它是怎么结束的。
        if let Some(r) = &reason {
            self.set_error(r.clone()).await;
            self.notice(&format!("会话结束：{r}")).await;
        }
        self.set_state(SessionState::Closed).await;
        let _ = self.events.send(SessionEvent::Closed {
            session: self.id.clone(),
            reason,
        });
    }
}

/// 便于测试：不需要真实 SSH 的工厂
pub struct BoxedFactory<F>(pub F);

#[async_trait]
impl<F> TransportFactory for BoxedFactory<F>
where
    F: Fn() -> TransportResult<Box<dyn SessionTransport + Send>> + Send + Sync,
{
    async fn connect(
        &self,
        _host: &Host,
        _cols: u16,
        _rows: u16,
    ) -> TransportResult<Box<dyn SessionTransport + Send>> {
        (self.0)()
    }
}
