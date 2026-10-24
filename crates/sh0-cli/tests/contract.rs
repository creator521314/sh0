//! M2 契约测试：AI CLI 的 P0 子命令 × 成功 / 失败 / 需确认三条路径（NFR-10）。
//!
//! daemon 以完整生产装配起在进程内（`daemon::run_for_test` 注入假传输层），
//! 断言的是"AI 通过 IPC 实际看到的东西"——与 `sh0 ai` 真实用法同一条链路。
//! 不连真机：哨兵截取这类易错逻辑由 exec.rs 单测 + 本文件的假 shell 双路覆盖。
//!
//! 覆盖的验收：
//!   AC-4 exec 返回 JSON 含 stdout/stderr/exit_code/duration_ms
//!   AC-5 deny 档一律拦截（连 --yes 也拦）且落审计
//!   AC-6 confirm 档无人值守返回 needs_confirmation + 令牌，带令牌可闭环
//!   AC-7 配置/数据目录无明文密码
//!   AC-8 AI 与 TUI 动作进同一审计日志

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sh0::daemon;
use sh0_core::transport::{
    ExecOutput, SessionTransport, TransportError, TransportEvent, TransportFactory, TransportResult,
};
use sh0_core::{CredentialRef, Host};
use sh0_ipc::protocol::methods;
use sh0_ipc::{IpcClient, IpcError};
use sh0_store::{AppConfig, CredentialStore, HostsFile, Paths, Store};
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// 假远端：一个会应答的 shell
// ---------------------------------------------------------------------------

/// 解析 daemon 发来的 `cmd\necho SH0_x:$?\n`，回编造的输出。
///
/// 回复刻意模仿 PTY 的真实形态（回显命令 → 输出 → 哨兵行，`\r\n` 结尾），
/// 这样哨兵截取逻辑面对的就是真机同款字节流。
fn shell_reply(payload: &str) -> Option<Vec<u8>> {
    let lines: Vec<&str> = payload.lines().collect();
    let echo_idx = lines.iter().rposition(|l| {
        let t = l.trim();
        t.starts_with("echo SH0_") && t.ends_with(":$?")
    })?;
    let sentinel = lines[echo_idx]
        .trim()
        .trim_start_matches("echo ")
        .trim_end_matches(":$?")
        .to_string();
    let cmd = lines[..echo_idx].join("\n");
    let cmd = cmd.trim();
    let exit = if cmd.contains("false") || cmd.contains("exit 1") { 1 } else { 0 };
    let out = if let Some(rest) = cmd.strip_prefix("echo ") {
        rest.to_string()
    } else if cmd.is_empty() {
        String::new()
    } else {
        format!("[mock-output] {cmd}")
    };
    Some(
        format!("{cmd}\r\n{out}\r\necho {sentinel}:$?\r\n{sentinel}:{exit}\r\n").into_bytes(),
    )
}

/// 收到带哨兵的发送就回复；其余发送（纯输入）静默丢弃
struct EchoTransport {
    pending: VecDeque<TransportEvent>,
}

#[async_trait]
impl SessionTransport for EchoTransport {
    async fn wait(&mut self) -> Option<TransportEvent> {
        if let Some(ev) = self.pending.pop_front() {
            return Some(ev);
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
        Some(TransportEvent::Data(Vec::new()))
    }

    async fn send(&mut self, data: &[u8]) -> TransportResult<()> {
        if let Some(reply) = shell_reply(&String::from_utf8_lossy(data)) {
            self.pending.push_back(TransportEvent::Data(reply));
        }
        Ok(())
    }

    async fn resize(&mut self, _cols: u16, _rows: u16) -> TransportResult<()> {
        Ok(())
    }

    async fn close(&mut self) -> TransportResult<()> {
        Ok(())
    }
}

/// 双模式假工厂：shell 走 EchoTransport，exec channel 走脚本化输出
/// （与 MockFactory::with_exec 同语义：按序取，空了重复最后一个）
#[derive(Clone, Default)]
struct EchoShellFactory {
    exec_outputs: Arc<std::sync::Mutex<VecDeque<ExecOutput>>>,
}

impl EchoShellFactory {
    fn with_exec(outputs: Vec<ExecOutput>) -> Self {
        Self {
            exec_outputs: Arc::new(std::sync::Mutex::new(VecDeque::from(outputs))),
        }
    }
}

#[async_trait]
impl TransportFactory for EchoShellFactory {
    async fn connect(
        &self,
        _host: &Host,
        _cols: u16,
        _rows: u16,
    ) -> TransportResult<Box<dyn SessionTransport + Send>> {
        Ok(Box::new(EchoTransport {
            pending: VecDeque::new(),
        }))
    }

    async fn exec(
        &self,
        _host: &Host,
        _cmd: &str,
        _timeout: Duration,
    ) -> TransportResult<ExecOutput> {
        let mut q = self.exec_outputs.lock().unwrap();
        let out = if q.len() > 1 {
            q.pop_front()
        } else {
            q.front().cloned()
        };
        out.ok_or_else(|| TransportError::NotImplemented("exec 脚本为空".into()))
    }
}

// ---------------------------------------------------------------------------
// 测试装置
// ---------------------------------------------------------------------------

/// 单条 IPC 调用的等待上限（daemon 最坏路径是 exec 超时，测试里都设得远小于此）
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// 取出 daemon 的业务错误码（IPC 层错误直接 panic——测试里不允许出现）
fn err_code(e: &IpcError) -> &str {
    match e {
        IpcError::Remote { code, .. } => code,
        other => panic!("期望 daemon 业务错误码，实际: {other:?}"),
    }
}

struct Fixture {
    dir: PathBuf,
    client: Arc<IpcClient>,
    stop: Option<oneshot::Sender<()>>,
}

impl Fixture {
    /// 所有调用都带超时：契约测试挂住等于 CI 挂住，宁可失败也要给出方法名
    async fn call(&self, method: &str, params: Value) -> Result<Value, IpcError> {
        tokio::time::timeout(CALL_TIMEOUT, self.client.call(method, params))
            .await
            .unwrap_or_else(|_| panic!("调用超时（>{CALL_TIMEOUT:?}）: {method}"))
    }

    /// 取错误信封（调用失败时）；panics 若调用成功
    async fn call_err(&self, method: &str, params: Value) -> IpcError {
        self.call(method, params)
            .await
            .expect_err("调用应当失败，但却成功了")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // 先收线再删目录：daemon 任务要时间释放 SQLite 句柄（Windows 上删不掉占用的文件）
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        for _ in 0..10 {
            if std::fs::remove_dir_all(&self.dir).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}

fn test_host() -> Host {
    Host {
        id: sh0_core::new_host_id(),
        name: "mockbox".into(),
        group: vec![],
        addr: "127.0.0.1".into(),
        port: 22,
        user: "u".into(),
        auth: CredentialRef::None,
        tags: vec![],
        note: None,
        risk_cap: None,
    }
}

async fn fixture() -> Fixture {
    fixture_with(EchoShellFactory::default()).await
}

async fn fixture_with(factory: EchoShellFactory) -> Fixture {
    // 诊断用：RUST_LOG=debug 时能看到 daemon 内部走到哪一步
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let dir = std::env::temp_dir().join(format!("sh0-contract-{}", sh0_core::new_session_id()));
    let paths = Paths {
        config_dir: dir.join("config"),
        data_dir: dir.join("data"),
        runtime_dir: dir.join("data").join("run"),
    };
    let store = Arc::new(Store::with_paths(paths).expect("临时 Store 创建失败"));
    let cfg = AppConfig::default();
    store
        .save_hosts(&HostsFile {
            version: 1,
            hosts: vec![test_host()],
        })
        .expect("写入主机清单失败");

    let (addr, token, stop) = daemon::run_for_test(store, cfg, Arc::new(factory))
        .await
        .expect("测试 daemon 启动失败");
    let conn = sh0_ipc::IpcClient::connect(&addr.to_string(), &token)
        .await
        .expect("IPC 连接失败");
    Fixture {
        dir,
        client: conn.client,
        stop: Some(stop),
    }
}

// ---------------------------------------------------------------------------
// AC-4：exec 的 JSON 信封
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac4_exec_shell_mode_full_envelope() {
    let fx = fixture().await;

    // 一键式调用（--host）：自动建连 → 执行 → 关闭
    let data = fx
        .call(
            methods::EXEC_RUN,
            json!({
                "host": "mockbox",
                "cmd": "echo hello-contract",
                "mode": "shell",
                "timeout_ms": 8000,
                "actor": "ai",
            }),
        )
        .await
        .expect("auto 档命令应当直接执行");

    // AC-4 四要素齐备
    assert_eq!(data["stdout"], "hello-contract");
    assert_eq!(data["stderr"], "");
    assert_eq!(data["exit_code"], 0);
    assert!(
        data["duration_ms"].is_i64(),
        "duration_ms 必须是整数毫秒，实际: {data}"
    );
    assert_eq!(data["mode"], "shell");
    assert_eq!(data["host"], "mockbox");
    assert_eq!(data["timeout"], false);
}

#[tokio::test]
async fn ac4_exec_channel_mode_separates_streams() {
    let fx = fixture_with(EchoShellFactory::with_exec(vec![ExecOutput {
        stdout: "channel-out".into(),
        stderr: "channel-err".into(),
        exit_code: Some(2),
    }]))
    .await;

    // ls 在 auto 白名单，不会撞确认流
    let data = fx
        .call(
            methods::EXEC_RUN,
            json!({
                "host": "mockbox",
                "cmd": "ls /",
                "mode": "exec",
                "timeout_ms": 8000,
                "actor": "ai",
            }),
        )
        .await
        .expect("exec channel 模式应当直接返回");

    // channel 模式的卖点：stdout/stderr 天然分离、退出码精确
    assert_eq!(data["stdout"], "channel-out");
    assert_eq!(data["stderr"], "channel-err");
    assert_eq!(data["exit_code"], 2);
    assert_eq!(data["mode"], "exec");
}

#[tokio::test]
async fn ac4_nonzero_exit_is_data_not_transport_error() {
    let fx = fixture().await;

    // 命令跑成了但退出码非零：daemon 返回 Ok(data)，由 ai 侧映射退出码 1（§6.3）
    let data = fx
        .call(
            methods::EXEC_RUN,
            json!({
                "host": "mockbox",
                "cmd": "false",
                "timeout_ms": 8000,
                "actor": "ai",
            }),
        )
        .await
        .expect("remote_nonzero 必须是 Ok(data)");
    assert_eq!(data["exit_code"], 1);
}

// ---------------------------------------------------------------------------
// AC-5：deny 一律拦截
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac5_deny_is_absolute_even_with_yes() {
    let fx = fixture().await;

    for yes in [false, true] {
        let err = fx
            .call_err(
                methods::EXEC_RUN,
                json!({
                    "host": "mockbox",
                    "cmd": "rm -rf /",
                    "yes": yes,
                    "timeout_ms": 8000,
                    "actor": "ai",
                }),
            )
            .await;
        assert_eq!(
            err_code(&err), "risk_denied",
            "rm -rf / 必须一律 deny（yes={yes} 也不例外）"
        );
    }

    // 拦截动作本身要落审计（AC-8）
    let log = fx
        .call(
            methods::LOG_QUERY,
            json!({ "actor": "ai", "risk": "deny" }),
        )
        .await
        .expect("审计查询应当成功");
    let recs = log["records"].as_array().expect("records 必须是数组");
    assert!(
        recs.iter().any(|r| r["decision"] == "denied"
            && r["command"].as_str().is_some_and(|c| c.contains("rm -rf /"))),
        "deny 决策必须出现在审计里: {log}"
    );
}

#[tokio::test]
async fn ac5_dry_run_reports_risk_without_executing() {
    let fx = fixture().await;

    let data = fx
        .call(
            methods::EXEC_RUN,
            json!({
                "host": "mockbox",
                "cmd": "rm -rf /",
                "dry_run": true,
                "actor": "ai",
            }),
        )
        .await
        .expect("dry-run 只判定不执行，不应报错");
    assert_eq!(data["risk"], "deny");
    assert_eq!(data["exit_code"], Value::Null);

    let safe = fx
        .call(
            methods::EXEC_RUN,
            json!({ "host": "mockbox", "cmd": "ls /", "dry_run": true, "actor": "ai" }),
        )
        .await
        .unwrap();
    assert_eq!(safe["risk"], "auto");
}

// ---------------------------------------------------------------------------
// AC-6：confirm 档的确认闭环
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac6_confirm_flow_closes_with_token() {
    let fx = fixture().await;
    let cmd = "systemctl restart nginx"; // 不在 auto 白名单 → 落到默认档 confirm

    // 1) 无人值守：needs_confirmation + 令牌（AI 是一次性进程，只能从错误信封拿令牌）
    let err = fx
        .call_err(
            methods::EXEC_RUN,
            json!({ "host": "mockbox", "cmd": cmd, "timeout_ms": 8000, "actor": "ai" }),
        )
        .await;
    let IpcError::Remote { code, data, .. } = &err else {
        panic!("needs_confirmation 必须是 Remote 错误，实际: {err:?}");
    };
    assert_eq!(code, "needs_confirmation");
    let token = data.as_ref().expect("错误信封必须带 data")["confirm_token"]
        .as_str()
        .expect("data 里必须有 confirm_token")
        .to_string();

    // 2) 带令牌重试：一次性消费，执行
    let data = fx
        .call(
            methods::EXEC_RUN,
            json!({
                "host": "mockbox",
                "cmd": cmd,
                "confirm_token": token,
                "timeout_ms": 8000,
                "actor": "ai",
            }),
        )
        .await
        .expect("带有效令牌应当放行");
    assert_eq!(data["exit_code"], 0);
    assert!(
        data["stdout"]
            .as_str()
            .is_some_and(|s| s.contains("systemctl restart nginx"))
    );

    // 3) 同一个令牌不能重放
    let err = fx
        .call_err(
            methods::EXEC_RUN,
            json!({ "host": "mockbox", "cmd": cmd, "confirm_token": token, "timeout_ms": 8000, "actor": "ai" }),
        )
        .await;
    assert_eq!(err_code(&err), "invalid_args", "令牌必须单次消费");

    // 4) 审计里能看到完整的确认流水：needs_confirmation → confirmed
    let log = fx
        .call(methods::LOG_QUERY, json!({ "actor": "ai" }))
        .await
        .unwrap();
    let decisions: Vec<&str> = log["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["command"].as_str() == Some(cmd))
        .filter_map(|r| r["decision"].as_str())
        .collect();
    assert!(
        decisions.contains(&"needs_confirmation"),
        "缺少 needs_confirmation 记录: {decisions:?}"
    );
    assert!(
        decisions.contains(&"confirmed"),
        "缺少 confirmed 记录: {decisions:?}"
    );
}

#[tokio::test]
async fn ac6_yes_flag_acts_as_manual_approval() {
    let fx = fixture().await;

    let data = fx
        .call(
            methods::EXEC_RUN,
            json!({
                "host": "mockbox",
                "cmd": "systemctl restart nginx",
                "yes": true,
                "timeout_ms": 8000,
                "actor": "ai",
            }),
        )
        .await
        .expect("--yes 等价人工批准，应直接执行");
    assert_eq!(data["exit_code"], 0);
}

// ---------------------------------------------------------------------------
// AC-7：落盘无明文
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac7_no_plaintext_secrets_on_disk() {
    // 强制走 vault：钥匙串在测试机上行为不一（Windows 有凭据管理器、
    // Linux 上可能根本没有 Secret Service），禁用后才能确定性地验证密文落盘
    let master = "test-master-password";
    unsafe {
        std::env::set_var(sh0_store::MASTER_PASSWORD_ENV, master);
        std::env::set_var("SH0_DISABLE_KEYRING", "1");
    }

    let dir = std::env::temp_dir().join(format!("sh0-creds-{}", sh0_core::new_session_id()));
    let paths = Paths {
        config_dir: dir.join("config"),
        data_dir: dir.join("data"),
        runtime_dir: dir.join("data").join("run"),
    };
    let secret = "SuperSecret-Pw-9527";
    {
        let store = CredentialStore::open(&paths).expect("vault 创建失败");
        store.set("mockbox", "deploy", secret).expect("写入失败");
        assert_eq!(
            store.get("mockbox", "deploy").unwrap().as_deref(),
            Some(secret),
            "取回来必须原样"
        );
    }

    // 走遍配置与数据目录：任何文件都不许出现明文（含主密码本身）
    let mut checked = 0usize;
    for root in [&paths.config_dir, &paths.data_dir] {
        let mut stack = vec![root.clone()];
        while let Some(p) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&p) else {
                continue;
            };
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let raw = std::fs::read(&path).unwrap_or_default();
                let text = String::from_utf8_lossy(&raw);
                assert!(
                    !text.contains(secret),
                    "{} 含有明文密码",
                    path.display()
                );
                assert!(
                    !text.contains(master),
                    "{} 含有主密码",
                    path.display()
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 0, "至少要检查到 vault 文件");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// P0 子命令的成功/失败路径
// ---------------------------------------------------------------------------

#[tokio::test]
async fn p0_session_lifecycle_and_screen() {
    let fx = fixture().await;

    let opened = fx
        .call(
            methods::SESSION_OPEN,
            json!({ "host": "mockbox", "cols": 80, "rows": 24, "actor": "tui" }),
        )
        .await
        .expect("打开会话应当成功");
    let sid = opened["id"].as_str().expect("必须返回 session id").to_string();

    let list = fx.call(methods::SESSION_LIST, json!({})).await.unwrap();
    assert!(
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == sid),
        "session.list 必须能看到新会话"
    );

    let status = fx
        .call(methods::SESSION_STATUS, json!({ "session": sid }))
        .await
        .unwrap();
    assert_eq!(status["state"], "ready");

    // screen read：plain 与 grid 两种模式
    let plain = fx
        .call(
            methods::SCREEN_READ,
            json!({ "session": sid, "mode": "plain" }),
        )
        .await
        .unwrap();
    assert!(plain["text"].is_string(), "plain 模式必须返回 text: {plain}");

    let grid = fx
        .call(
            methods::SCREEN_READ,
            json!({ "session": sid, "mode": "grid" }),
        )
        .await
        .unwrap();
    assert_eq!(grid["cols"], 80);
    assert!(grid["lines"].is_array(), "grid 模式必须返回 lines: {grid}");

    // input send：按字节计数
    let sent = fx
        .call(
            methods::INPUT_SEND,
            json!({ "session": sid, "data": "echo hi\n", "actor": "tui" }),
        )
        .await
        .unwrap();
    assert_eq!(sent["sent"], 8);

    // 在既有会话上跑 exec（复用交互 shell 的哨兵模式）
    let data = fx
        .call(
            methods::EXEC_RUN,
            json!({ "session": sid, "cmd": "echo session-exec", "timeout_ms": 8000, "actor": "ai" }),
        )
        .await
        .expect("既有 ready 会话上的 exec 应当成功");
    assert_eq!(data["stdout"], "session-exec");
    assert_eq!(data["session_id"], sid);

    // 关闭后 exec 报 session_unavailable（不是 internal）
    fx.call(methods::SESSION_CLOSE, json!({ "session": sid }))
        .await
        .unwrap();
    let err = fx
        .call_err(
            methods::EXEC_RUN,
            json!({ "session": sid, "cmd": "ls", "timeout_ms": 2000, "actor": "ai" }),
        )
        .await;
    assert_eq!(err_code(&err), "session_unavailable");
}

#[tokio::test]
async fn p0_failure_paths_are_machine_readable() {
    let fx = fixture().await;

    // 主机不存在
    let err = fx
        .call_err(
            methods::EXEC_RUN,
            json!({ "host": "no-such-host", "cmd": "ls", "timeout_ms": 2000, "actor": "ai" }),
        )
        .await;
    assert_eq!(err_code(&err), "host_not_found");

    // 既没给 session 也没给 host
    let err = fx
        .call_err(
            methods::EXEC_RUN,
            json!({ "cmd": "ls", "timeout_ms": 2000, "actor": "ai" }),
        )
        .await;
    assert_eq!(err_code(&err), "invalid_args");

    // 空命令
    let err = fx
        .call_err(
            methods::EXEC_RUN,
            json!({ "host": "mockbox", "cmd": "  ", "timeout_ms": 2000, "actor": "ai" }),
        )
        .await;
    assert_eq!(err_code(&err), "invalid_args");

    // 未知 mode
    let err = fx
        .call_err(
            methods::EXEC_RUN,
            json!({ "host": "mockbox", "cmd": "ls", "mode": "carrier-pigeon", "timeout_ms": 2000, "actor": "ai" }),
        )
        .await;
    assert_eq!(err_code(&err), "invalid_args");

    // 未知方法
    let err = fx.call_err("no.such.method", json!({})).await;
    assert_eq!(err_code(&err), "invalid_args");
}

// ---------------------------------------------------------------------------
// AC-8：审计同一本账
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac8_ai_and_tui_actions_share_one_audit_log() {
    let fx = fixture().await;

    // AI 动作
    fx.call(
        methods::EXEC_RUN,
        json!({ "host": "mockbox", "cmd": "echo audited", "timeout_ms": 8000, "actor": "ai" }),
    )
    .await
    .unwrap();

    // TUI 动作
    let opened = fx
        .call(
            methods::SESSION_OPEN,
            json!({ "host": "mockbox", "cols": 80, "rows": 24, "actor": "tui" }),
        )
        .await
        .unwrap();
    fx.call(
        methods::INPUT_SEND,
        json!({ "session": opened["id"], "data": "x", "actor": "tui" }),
    )
    .await
    .unwrap();

    let log = fx.call(methods::LOG_QUERY, json!({})).await.unwrap();
    let recs = log["records"].as_array().unwrap();
    let actions: Vec<&str> = recs.iter().filter_map(|r| r["action"].as_str()).collect();
    assert!(actions.contains(&"exec.run"), "缺 exec.run: {actions:?}");
    assert!(actions.contains(&"session.open"), "缺 session.open: {actions:?}");
    assert!(actions.contains(&"input.send"), "缺 input.send: {actions:?}");

    // actor 区分得开（同一本账，不是两个库）
    assert!(recs.iter().any(|r| r["actor"] == "ai"));
    assert!(recs.iter().any(|r| r["actor"] == "tui"));

    // 按 actor 过滤（FR-8.4）
    let ai_only = fx
        .call(methods::LOG_QUERY, json!({ "actor": "ai" }))
        .await
        .unwrap();
    assert!(
        ai_only["records"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["actor"] == "ai")
    );
}
