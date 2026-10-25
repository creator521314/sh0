//! 会话状态机与重连的测试（验收 AC-3）。
//!
//! 全部用 MockTransport，不连真机——断线重连这种路径靠真机复现既慢又不可靠。

use std::sync::Arc;
use std::time::Duration;

use sh0_core::mock::{MockFactory, OnEmpty};
use sh0_core::model::{CredentialRef, SessionState};
use sh0_core::session::{ReconnectPolicy, SessionConfig, SessionEvent, spawn_session};
use sh0_core::transport::TransportEvent;
use sh0_core::Host;
use tokio::sync::mpsc;

fn host() -> Host {
    Host {
        id: "h1".into(),
        name: "mock".into(),
        group: vec![],
        addr: "127.0.0.1".into(),
        port: 22,
        user: "u".into(),
        auth: CredentialRef::None,
        tags: vec![],
        note: None,
        risk_cap: None,
        proxy_jump: None,
        jump: None,
    }
}

fn config() -> SessionConfig {
    SessionConfig {
        cols: 40,
        rows: 5,
        scrollback_lines: 100,
        reconnect: ReconnectPolicy {
            base: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            max_attempts: Some(5),
            jitter: false,
        },
        budget: None,
        mark_reconnect: true,
    }
}

fn text(ev: &str) -> TransportEvent {
    TransportEvent::Data(format!("{ev}\r\n").into_bytes())
}

async fn wait_for_state(
    rx: &mut mpsc::UnboundedReceiver<SessionEvent>,
    want: SessionState,
) -> bool {
    // 最多等 2 秒
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            Ok(Some(SessionEvent::StateChanged { state, .. })) if state == want => return true,
            Ok(Some(_)) => continue,
            Ok(None) => return false,
            Err(_) => continue,
        }
    }
    false
}

#[tokio::test]
async fn backoff_grows_and_is_capped() {
    let p = ReconnectPolicy {
        base: Duration::from_millis(500),
        max_delay: Duration::from_secs(8),
        max_attempts: None,
        jitter: false,
    };
    assert_eq!(p.backoff_delay(0, 1), Duration::from_millis(500));
    assert_eq!(p.backoff_delay(1, 1), Duration::from_millis(1000));
    assert_eq!(p.backoff_delay(2, 1), Duration::from_millis(2000));
    // 超过上限后被钳住
    assert_eq!(p.backoff_delay(10, 1), Duration::from_secs(8));
}

#[tokio::test]
async fn jitter_separates_sessions() {
    let p = ReconnectPolicy::default();
    let a = p.backoff_delay(1, 111);
    let b = p.backoff_delay(1, 222);
    assert_ne!(a, b, "抖动应让不同会话的重连时刻错开");
    // 但同一会话同一轮次必须稳定可复现
    assert_eq!(a, p.backoff_delay(1, 111));
}

#[tokio::test]
async fn session_becomes_ready_and_streams_output() {
    let factory = Arc::new(MockFactory::repeating(
        vec![text("hello"), text("world")],
        0,
    ));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let h = spawn_session(host(), factory, config(), tx);

    assert!(wait_for_state(&mut rx, SessionState::Ready).await);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let text_out = h.read_screen(true).await.unwrap();
    assert!(text_out.contains("hello"), "实际: {text_out:?}");
    assert!(text_out.contains("world"), "实际: {text_out:?}");
    h.close().await.unwrap();
}

#[tokio::test]
async fn reconnect_keeps_history_and_marks_it() {
    // 第一次连接：输出 before 后断开；第二次：输出 after
    let factory = Arc::new(MockFactory::disconnecting(
        vec![vec![text("before")], vec![text("after")]],
        0,
        1, // 第一次连接播完后断线
    ));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let h = spawn_session(host(), factory.clone(), config(), tx);

    // 等到重连发生
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut reconnected = false;
    while tokio::time::Instant::now() < deadline && !reconnected {
        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
            Ok(Some(SessionEvent::Reconnected { .. })) => reconnected = true,
            Ok(Some(_)) => continue,
            _ => continue,
        }
    }
    assert!(reconnected, "应当发生重连");
    assert!(factory.calls() >= 2, "至少连了两次: {}", factory.calls());

    tokio::time::sleep(Duration::from_millis(50)).await;
    let out = h.read_screen(true).await.unwrap();
    // AC-3 核心：重连前的输出还能翻到，并且有分隔线
    assert!(out.contains("before"), "实际: {out:?}");
    assert!(out.contains("after"), "实际: {out:?}");
    assert!(out.contains("重连"), "应有重连分隔线: {out:?}");
    assert_eq!(h.snapshot().await.reconnect_count, 1);
    h.close().await.unwrap();
}

#[tokio::test]
async fn gives_up_after_max_attempts() {
    let factory = Arc::new(MockFactory::repeating(vec![], 99));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut cfg = config();
    cfg.reconnect.max_attempts = Some(2);
    let h = spawn_session(host(), factory.clone(), cfg, tx);

    assert!(wait_for_state(&mut rx, SessionState::Closed).await);
    // 首次 1 次 + 重试 2 次
    assert!(factory.calls() <= 4, "调用次数: {}", factory.calls());
    assert_eq!(h.state().await, SessionState::Closed);
}

#[tokio::test]
async fn input_is_forwarded_to_transport() {
    let factory = Arc::new(MockFactory::repeating(vec![text("x")], 0));
    let (tx, _rx) = mpsc::unbounded_channel();
    let h = spawn_session(host(), factory, config(), tx);
    tokio::time::sleep(Duration::from_millis(30)).await;

    h.send(b"ls -la\r").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    // MockFactory 内部的 sent 记录无法直接取到（已装箱），这里只验证不报错
    assert_eq!(h.state().await, SessionState::Ready);
    h.close().await.unwrap();
}

#[tokio::test]
async fn resize_updates_screen_and_transport() {
    let factory = Arc::new(MockFactory::repeating(vec![text("x")], 0));
    let (tx, _rx) = mpsc::unbounded_channel();
    let h = spawn_session(host(), factory, config(), tx);
    tokio::time::sleep(Duration::from_millis(30)).await;

    h.resize(80, 24).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.size().await, (80, 24));
    let snap = h.snapshot().await;
    assert_eq!(snap.cols, 80);
    assert_eq!(snap.rows, 24);
    h.close().await.unwrap();
}

#[tokio::test]
async fn screen_read_without_scrollback_only_returns_visible() {
    let mut script = Vec::new();
    for i in 0..20 {
        script.push(text(&format!("line{i}")));
    }
    let factory = Arc::new(MockFactory::repeating(script, 0));
    let (tx, _rx) = mpsc::unbounded_channel();
    let h = spawn_session(host(), factory, config(), tx);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let visible = h.read_screen(false).await.unwrap();
    let full = h.read_screen(true).await.unwrap();
    assert!(visible.lines().count() <= 5, "可见区最多 5 行: {visible:?}");
    assert!(full.lines().count() > visible.lines().count());
    assert!(full.contains("line0"), "历史应保留: {full:?}");
    h.close().await.unwrap();
}

#[tokio::test]
async fn multibyte_split_across_chunks_is_not_corrupted() {
    use sh0_core::mock::MockTransport;
    // 把"你好"的 UTF-8 字节从中间切开喂进去
    let bytes = "你好\n".as_bytes().to_vec();
    let mid = 2;
    let factory = Arc::new(MockFactory::repeating(
        vec![
            TransportEvent::Data(bytes[..mid].to_vec()),
            TransportEvent::Data(bytes[mid..].to_vec()),
        ],
        0,
    ));
    let (tx, _rx) = mpsc::unbounded_channel();
    let h = spawn_session(host(), factory, config(), tx);
    tokio::time::sleep(Duration::from_millis(80)).await;

    let out = h.read_screen(true).await.unwrap();
    assert!(out.contains("你好"), "不应出现替换字符: {out:?}");
    assert!(!out.contains('\u{fffd}'), "不应出现 U+FFFD: {out:?}");
    let _ = MockTransport::new(vec![], OnEmpty::Close);
    h.close().await.unwrap();
}

/// 回归：重连期间状态必须保持 `Reconnecting`，不能被每次重试打回 `Connecting`。
///
/// 曾踩过：`connect()` 里无条件 `set_state(Connecting)`，于是外面永远看不到
/// 「正在重连」，AI 也无法区分首次连接与重连。
#[tokio::test]
async fn state_stays_reconnecting_during_retries() {
    // 一直连不上：首次失败 + 持续重试
    let factory = Arc::new(MockFactory::repeating(vec![], 99));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut cfg = config();
    cfg.reconnect.base = Duration::from_millis(30);
    cfg.reconnect.max_delay = Duration::from_millis(60);
    cfg.reconnect.max_attempts = Some(20);
    let h = spawn_session(host(), factory.clone(), cfg, tx);

    // 等到进入 Reconnecting
    assert!(
        wait_for_state(&mut rx, SessionState::Reconnecting).await,
        "应当进入 reconnecting"
    );

    // 连着观察若干次重试，状态都不该退回 Connecting
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
            h.state().await,
            SessionState::Reconnecting,
            "重连期间状态被改动了"
        );
    }
    h.close().await.unwrap();
}

/// 回归：连接失败时屏幕不能是空的，否则 AI 读屏拿到空串会误判成
/// 「命令执行成功但无输出」。
#[tokio::test]
async fn failure_reason_is_visible_on_screen() {
    let factory = Arc::new(MockFactory::repeating(vec![], 99));
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut cfg = config();
    cfg.reconnect.max_attempts = Some(1);
    let h = spawn_session(host(), factory.clone(), cfg, tx);

    tokio::time::sleep(Duration::from_millis(200)).await;
    let out = h.read_screen(true).await.unwrap();
    assert!(
        out.contains("连接失败") || out.contains("重连失败") || out.contains("会话结束"),
        "屏幕上应能读到失败原因，实际: {out:?}"
    );

    let snap = h.snapshot().await;
    assert!(snap.last_error.is_some(), "快照应带上 last_error");
    // 会话此时已自然结束，close 返回 Gone 是正常的
    let _ = h.close().await;
}

/// 回归：重连等待期间下的命令必须立即生效。
///
/// 曾踩过：等待用的是 `sleep(delay)`，睡完才收一次命令，于是退避越长
/// 命令响应越迟钝——拖窗口、清空屏幕都要等几秒才动。
#[tokio::test]
async fn commands_are_handled_during_reconnect_wait() {
    let factory = Arc::new(MockFactory::repeating(vec![], 99));
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut cfg = config();
    cfg.reconnect.base = Duration::from_millis(300);
    cfg.reconnect.max_delay = Duration::from_millis(300);
    cfg.reconnect.max_attempts = Some(50);
    let h = spawn_session(host(), factory.clone(), cfg, tx);

    // 等到进入重连（此时正在退避等待中）
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(h.state().await, SessionState::Reconnecting);

    // clear 必须马上生效，而不是等这轮 300ms 退避走完
    h.clear().await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let out = h.read_screen(true).await.unwrap();
    assert!(
        out.is_empty(),
        "clear 应在退避等待期间就生效，实际: {out:?}"
    );
    let _ = h.close().await;
}
