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
use sh0_core::mock::{MemFs, MockSftp};
use sh0_core::sftp::SftpClient;
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
    let exit = if cmd.contains("false") || cmd.contains("exit 1") {
        1
    } else {
        0
    };
    let out = if let Some(rest) = cmd.strip_prefix("echo ") {
        rest.to_string()
    } else if cmd.is_empty() {
        String::new()
    } else {
        format!("[mock-output] {cmd}")
    };
    Some(format!("{cmd}\r\n{out}\r\necho {sentinel}:$?\r\n{sentinel}:{exit}\r\n").into_bytes())
}

/// 收到带哨兵的发送就回复；其余发送（纯输入）静默丢弃。
///
/// 挂了 `fs` 就能在这条连接上开 SFTP——契约测试用它证明"会话内 SFTP 不新开 TCP"。
struct EchoTransport {
    pending: VecDeque<TransportEvent>,
    fs: Option<MemFs>,
    /// 记下收到的输入：广播/输入类断言靠它
    sends: Arc<std::sync::Mutex<Vec<String>>>,
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
        let text = String::from_utf8_lossy(data).to_string();
        self.sends.lock().unwrap().push(text.clone());
        if let Some(reply) = shell_reply(&text) {
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

    async fn sftp(&mut self) -> TransportResult<Box<dyn SftpClient>> {
        match &self.fs {
            Some(fs) => Ok(Box::new(MockSftp::new(fs.clone()))),
            None => Err(TransportError::NotImplemented("mock 未挂文件系统".into())),
        }
    }
}

/// 双模式假工厂：shell 走 EchoTransport，exec channel 走脚本化输出
/// （与 MockFactory::with_exec 同语义：按序取，空了重复最后一个）
#[derive(Clone, Default)]
struct EchoShellFactory {
    exec_outputs: Arc<std::sync::Mutex<VecDeque<ExecOutput>>>,
    /// 挂在连接上的内存文件系统；None 时 SFTP 不可用（模拟不支持的服务端）
    fs: Arc<std::sync::Mutex<Option<MemFs>>>,
    pub connect_calls: Arc<std::sync::Mutex<u32>>,
    pub sftp_calls: Arc<std::sync::Mutex<u32>>,
    /// 每条 `send` 的内容（广播测试要断言"同一份输入到了几个会话"）
    pub sends: Arc<std::sync::Mutex<Vec<String>>>,
    /// 指定"连不上/执行失败"的主机（批量测试要造部分失败）
    pub fail_hosts: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl EchoShellFactory {
    fn with_exec(outputs: Vec<ExecOutput>) -> Self {
        Self {
            exec_outputs: Arc::new(std::sync::Mutex::new(VecDeque::from(outputs))),
            fs: Arc::new(std::sync::Mutex::new(None)),
            connect_calls: Arc::new(std::sync::Mutex::new(0)),
            sftp_calls: Arc::new(std::sync::Mutex::new(0)),
            sends: Arc::new(std::sync::Mutex::new(Vec::new())),
            fail_hosts: Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
        }
    }

    /// 让这台"主机"支持 SFTP
    fn with_sftp(fs: MemFs) -> Self {
        let f = Self::default();
        *f.fs.lock().unwrap() = Some(fs);
        f
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
        *self.connect_calls.lock().unwrap() += 1;
        Ok(Box::new(EchoTransport {
            pending: VecDeque::new(),
            fs: self.fs.lock().unwrap().clone(),
            sends: self.sends.clone(),
        }))
    }

    async fn exec(
        &self,
        host: &Host,
        _cmd: &str,
        _timeout: Duration,
    ) -> TransportResult<ExecOutput> {
        // 注入的失败主机：批量要能造出"部分失败"的现场
        if self.fail_hosts.lock().unwrap().contains(&host.name) {
            return Err(TransportError::NotImplemented(format!(
                "mock: {} 执行失败",
                host.name
            )));
        }
        let mut q = self.exec_outputs.lock().unwrap();
        let out = if q.len() > 1 {
            q.pop_front()
        } else {
            q.front().cloned()
        };
        out.ok_or_else(|| TransportError::NotImplemented("exec 脚本为空".into()))
    }

    async fn open_sftp(&self, _host: &Host) -> TransportResult<Box<dyn SftpClient>> {
        *self.sftp_calls.lock().unwrap() += 1;
        match self.fs.lock().unwrap().clone() {
            // 没挂 fs = 服务端没开 sftp subsystem：真机上这是常见配置
            None => Err(TransportError::NotImplemented("SFTP 子系统不可用".into())),
            Some(fs) => Ok(Box::new(MockSftp::new(fs))),
        }
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
    /// 留一份工厂：SFTP 的复用断言要数 connect 次数
    factory: EchoShellFactory,
    /// TUI 侧集成测试要用同一份 Store
    store: Arc<Store>,
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
        proxy_jump: None,
        jump: None,
    }
}

async fn fixture() -> Fixture {
    fixture_with(EchoShellFactory::default()).await
}

async fn fixture_with(factory: EchoShellFactory) -> Fixture {
    fixture_with_cfg(factory, AppConfig::default()).await
}

/// 多台主机的装置（批量测试用：选择器、并发、部分失败都要 >1 台）
async fn fixture_with_hosts(factory: EchoShellFactory, hosts: Vec<Host>) -> Fixture {
    fixture_with_hosts_cfg(factory, AppConfig::default(), hosts).await
}

async fn fixture_with_hosts_cfg(
    factory: EchoShellFactory,
    cfg: AppConfig,
    hosts: Vec<Host>,
) -> Fixture {
    let fx = fixture_with_cfg(factory, cfg).await;
    fx.store
        .save_hosts(&HostsFile {
            version: 1,
            hosts,
        })
        .expect("写入主机清单失败");
    fx
}

fn host_named(name: &str, group: &[&str], tags: &[&str]) -> Host {
    Host {
        id: sh0_core::new_host_id(),
        name: name.into(),
        group: group.iter().map(|s| s.to_string()).collect(),
        addr: "127.0.0.1".into(),
        port: 22,
        user: "u".into(),
        auth: CredentialRef::None,
        tags: tags.iter().map(|s| s.to_string()).collect(),
        note: None,
        risk_cap: None,
        proxy_jump: None,
        jump: None,
    }
}

/// 自定义配置的装置（配额类断言要改 AppConfig）
async fn fixture_with_cfg(factory: EchoShellFactory, cfg: AppConfig) -> Fixture {
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
    store
        .save_hosts(&HostsFile {
            version: 1,
            hosts: vec![test_host()],
        })
        .expect("写入主机清单失败");

    // 留一份给测试断言（SFTP 复用要数 connect 次数），另一份交给 daemon
    let (addr, token, stop) = daemon::run_for_test(store.clone(), cfg, Arc::new(factory.clone()))
        .await
        .expect("测试 daemon 启动失败");
    let conn = sh0_ipc::IpcClient::connect(&addr.to_string(), &token)
        .await
        .expect("IPC 连接失败");
    Fixture {
        dir,
        client: conn.client,
        stop: Some(stop),
        factory,
        store,
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
            err_code(&err),
            "risk_denied",
            "rm -rf / 必须一律 deny（yes={yes} 也不例外）"
        );
    }

    // 拦截动作本身要落审计（AC-8）
    let log = fx
        .call(methods::LOG_QUERY, json!({ "actor": "ai", "risk": "deny" }))
        .await
        .expect("审计查询应当成功");
    let recs = log["records"].as_array().expect("records 必须是数组");
    assert!(
        recs.iter().any(|r| r["decision"] == "denied"
            && r["command"]
                .as_str()
                .is_some_and(|c| c.contains("rm -rf /"))),
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
                assert!(!text.contains(secret), "{} 含有明文密码", path.display());
                assert!(!text.contains(master), "{} 含有主密码", path.display());
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
    let sid = opened["id"]
        .as_str()
        .expect("必须返回 session id")
        .to_string();

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
    assert!(
        plain["text"].is_string(),
        "plain 模式必须返回 text: {plain}"
    );

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
    assert!(
        actions.contains(&"session.open"),
        "缺 session.open: {actions:?}"
    );
    assert!(
        actions.contains(&"input.send"),
        "缺 input.send: {actions:?}"
    );

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

// ---------------------------------------------------------------------------
// M4 · SFTP（FR-4.1 复用连接 / FR-4.2 UI 依赖的规范化路径 / FR-6.6）
// ---------------------------------------------------------------------------

async fn fixture_sftp() -> (Fixture, MemFs) {
    let fs = MemFs::new();
    let fx = fixture_with(EchoShellFactory::with_sftp(fs.clone())).await;
    (fx, fs)
}

/// 把传输队列跑到空：UI 里由事件循环每帧调一次 `tick_transfers`
async fn drain_transfers(app: &mut sh0_tui::App) {
    for _ in 0..500 {
        if !app.has_pending_transfer() {
            break;
        }
        app.tick_transfers().await;
    }
    assert!(!app.has_pending_transfer(), "传输队列没跑完");
}

/// 会话内列目录：复用已建连接（FR-4.1），且返回规范化绝对路径（FR-4.2 的 UI 依赖）
#[tokio::test]
async fn sftp_list_reuses_session_and_returns_canonical_path() {
    let (fx, _fs) = fixture_sftp().await;

    let opened = fx
        .call(
            methods::SESSION_OPEN,
            json!({ "host": "mockbox", "cols": 80, "rows": 24, "actor": "tui" }),
        )
        .await
        .unwrap();
    let sid = opened["id"].as_str().unwrap().to_string();
    let connects_after_open = *fx.factory.connect_calls.lock().unwrap();

    // UI 第一次列目录给的是 "."：服务端必须把它变成可信绝对路径，
    // 否则 `./foo` 会一层层叠下去，审计里的路径也跟着糊掉
    let data = fx
        .call(
            methods::FILE_LIST,
            json!({ "session": sid, "path": ".", "actor": "tui" }),
        )
        .await
        .unwrap();

    assert_eq!(data["path"], "/tmp", "应返回规范化后的绝对路径: {data}");
    assert_eq!(data["host"], "mockbox");
    let entries = data["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "预置 logs/ 与 hello.txt: {data}");
    assert_eq!(entries[0]["name"], "logs", "目录排前面");
    assert_eq!(entries[0]["is_dir"], true);
    assert_eq!(entries[1]["name"], "hello.txt");

    // 关键：没有为新开 SFTP 而新建 TCP 连接
    assert_eq!(
        *fx.factory.connect_calls.lock().unwrap(),
        connects_after_open,
        "SFTP 应当复用会话连接，不新开 TCP"
    );
}

/// 读写往返 + 审计落账（FR-4.3 / FR-8.x）
#[tokio::test]
async fn sftp_write_read_roundtrip_and_audited() {
    let (fx, fs) = fixture_sftp().await;

    fx.call(
        methods::FILE_WRITE,
        json!({ "host": "mockbox", "path": "/tmp/ui.txt", "data": "双栏 UI", "actor": "ai" }),
    )
    .await
    .unwrap();
    assert_eq!(fs.read("/tmp/ui.txt").unwrap(), "双栏 UI".as_bytes());

    let data = fx
        .call(
            methods::FILE_READ,
            json!({ "host": "mockbox", "path": "/tmp/ui.txt", "actor": "ai" }),
        )
        .await
        .unwrap();
    assert_eq!(data["encoding"], "utf8");
    assert_eq!(data["content"], "双栏 UI");

    // 不存在的路径 → not_found（AI 靠这个码决定下一步，不能是笼统的 internal）
    let err = fx
        .call_err(
            methods::FILE_READ,
            json!({ "host": "mockbox", "path": "/tmp/nope.txt", "actor": "ai" }),
        )
        .await;
    assert_eq!(err_code(&err), "not_found");

    let log = fx.call(methods::LOG_QUERY, json!({})).await.unwrap();
    let actions: Vec<&str> = log["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["action"].as_str())
        .collect();
    assert!(
        actions.contains(&"file.write"),
        "缺 file.write: {actions:?}"
    );
    assert!(actions.contains(&"file.read"), "缺 file.read: {actions:?}");
}

/// 目录/重命名/权限/删除：UI 的 `g` 跳转与后续操作都建立在这些之上
#[tokio::test]
async fn sftp_lifecycle_mkdir_rename_chmod_remove() {
    let (fx, fs) = fixture_sftp().await;

    fx.call(
        methods::FILE_MKDIR,
        json!({ "host": "mockbox", "path": "/tmp/sh0-ui", "actor": "ai" }),
    )
    .await
    .unwrap();
    assert!(fs.exists("/tmp/sh0-ui"));

    fx.call(
        methods::FILE_RENAME,
        json!({ "host": "mockbox", "from": "/tmp/sh0-ui", "to": "/tmp/sh0-ui2", "actor": "ai" }),
    )
    .await
    .unwrap();
    assert!(!fs.exists("/tmp/sh0-ui"));
    assert!(fs.exists("/tmp/sh0-ui2"));

    fx.call(
        methods::FILE_CHMOD,
        json!({ "host": "mockbox", "path": "/tmp/hello.txt", "mode": 0o600, "actor": "ai" }),
    )
    .await
    .unwrap();
    let st = fx
        .call(
            methods::FILE_STAT,
            json!({ "host": "mockbox", "path": "/tmp/hello.txt", "actor": "ai" }),
        )
        .await
        .unwrap();
    assert_eq!(st["entry"]["mode"], 0o600);

    fx.call(
        methods::FILE_REMOVE,
        json!({ "host": "mockbox", "path": "/tmp/sh0-ui2", "dir": true, "actor": "ai" }),
    )
    .await
    .unwrap();
    assert!(!fs.exists("/tmp/sh0-ui2"));

    // 删掉之后再列：空目录而不是报错
    let listed = fx
        .call(
            methods::FILE_LIST,
            json!({ "host": "mockbox", "path": "/tmp", "actor": "ai" }),
        )
        .await
        .unwrap();
    assert!(
        !listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "sh0-ui2")
    );
}

/// 一键式（`--host`，没有会话）走工厂的 open_sftp，用完即关
#[tokio::test]
async fn sftp_one_shot_uses_host_selector() {
    let (fx, _fs) = fixture_sftp().await;

    let data = fx
        .call(
            methods::FILE_LIST,
            json!({ "host": "mockbox", "path": "/tmp", "actor": "ai" }),
        )
        .await
        .unwrap();
    assert_eq!(data["path"], "/tmp");
    assert_eq!(*fx.factory.sftp_calls.lock().unwrap(), 1);

    // 既没 session 也没 host → invalid_args，而不是去猜
    let err = fx
        .call_err(methods::FILE_LIST, json!({ "path": "/tmp", "actor": "ai" }))
        .await;
    assert_eq!(err_code(&err), "invalid_args");
}

/// 服务端不支持 SFTP 时给出可读错误（真机上旧 sshd / 被禁 subsystem 会走到这）
#[tokio::test]
async fn sftp_unavailable_is_machine_readable() {
    // 工厂默认不挂文件系统，等价于"服务端不支持 SFTP"
    let fx = fixture().await;
    let err = fx
        .call_err(
            methods::FILE_LIST,
            json!({ "host": "mockbox", "path": "/tmp", "actor": "ai" }),
        )
        .await;
    assert_eq!(
        err_code(&err),
        "not_implemented",
        "能力缺失要能被 AI 识别成'重试无用'"
    );
}

/// TUI 的双栏文件视图真的能经 IPC 列出远端目录（FR-4.2）
///
/// 不跑事件循环，只驱动 App 的行为方法——验证的是"F9 之后右栏有没有东西"，
/// 渲染本身由 `files.rs` 的单测覆盖。这样不必真的开一个终端。
#[tokio::test]
async fn tui_file_view_lists_remote_over_ipc() {
    let (fx, _fs) = fixture_sftp().await;

    let events = tokio::sync::mpsc::unbounded_channel().1;
    let mut app = sh0_tui::App::new(
        fx.client.clone(),
        events,
        fx.store.clone(),
        AppConfig::default(),
    );
    app.open("mockbox").await.unwrap();

    // 左栏开局就是本机目录，且排过序
    assert!(!app.local_files().entries.is_empty(), "左栏应列出本机目录");

    app.enter_files().await;
    assert!(app.in_file_view());

    // 右栏：daemon 把 "." 规范化成了绝对路径
    assert_eq!(app.remote_files().cwd, "/tmp");
    let names: Vec<&str> = app
        .remote_files()
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["logs", "hello.txt"],
        "目录在前、文件在后: {names:?}"
    );

    // 焦点默认在左栏（要往外传文件）：先 Tab 到右栏
    app.toggle_file_pane();
    // 进入子目录：选中的第一行是 logs
    app.file_enter().await;
    assert_eq!(app.remote_files().cwd, "/tmp/logs");
    let inner: Vec<&str> = app
        .remote_files()
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(inner, vec!["app.log"]);

    // 上级目录
    app.file_up().await;
    assert_eq!(app.remote_files().cwd, "/tmp");

    // 路径跳转
    app.file_goto("/tmp/logs").await;
    assert_eq!(app.remote_files().cwd, "/tmp/logs");

    // 排序切换不影响条目数
    let before = app.remote_files().entries.len();
    app.file_sort();
    assert_eq!(app.remote_files().entries.len(), before);
}

/// 文件视图的基本操作：上传/下载（含二进制）、建目录、改名、改权限、删除（FR-4.3）
#[tokio::test]
async fn tui_file_ops_transfer_and_edit() {
    let (fx, fs) = fixture_sftp().await;

    // 本地临时目录：一个文本文件 + 一个二进制文件（后者逼着走 base64）
    let tmp = std::env::temp_dir().join(format!("sh0-w43-{}", sh0_core::new_session_id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("up.txt"), "hello-sh0").unwrap();
    let bin: Vec<u8> = (0..=255u8).cycle().take(1024).collect();
    std::fs::write(tmp.join("bin.dat"), &bin).unwrap();

    let events = tokio::sync::mpsc::unbounded_channel().1;
    let mut app =
        sh0_tui::App::new(fx.client.clone(), events, fx.store.clone(), AppConfig::default());
    app.open("mockbox").await.unwrap();
    app.enter_files().await;
    app.local_files_mut().goto(tmp.to_string_lossy().to_string());
    app.local_files_mut().reload_local();

    // 上传 up.txt（焦点默认在左栏）
    let i = app.local_files().entries.iter().position(|e| e.name == "up.txt").unwrap();
    app.local_files_mut().selected = i;
    app.file_transfer().await;
    drain_transfers(&mut app).await;
    assert_eq!(fs.read("/tmp/up.txt").unwrap(), b"hello-sh0", "文本上传应原样到达");

    // 上传 bin.dat：非 UTF-8，必须走 base64 才能不失真
    let i = app.local_files().entries.iter().position(|e| e.name == "bin.dat").unwrap();
    app.local_files_mut().selected = i;
    app.file_transfer().await;
    drain_transfers(&mut app).await;
    assert_eq!(fs.read("/tmp/bin.dat").unwrap(), bin, "二进制上传必须逐字节一致");

    // 下载 hello.txt 到本地目录
    app.toggle_file_pane();
    let i = app.remote_files().entries.iter().position(|e| e.name == "hello.txt").unwrap();
    app.remote_files_mut().selected = i;
    app.file_transfer().await;
    drain_transfers(&mut app).await;
    assert_eq!(std::fs::read(tmp.join("hello.txt")).unwrap(), b"hello");

    // 远端建目录 → 改名 → 删目录
    app.file_mkdir("w43").await;
    assert!(fs.exists("/tmp/w43"));
    app.file_rename("/tmp/w43", "w43b").await;
    assert!(fs.exists("/tmp/w43b"));
    assert!(!fs.exists("/tmp/w43"));
    app.file_delete(false, "/tmp/w43b", true).await;
    assert!(!fs.exists("/tmp/w43b"));

    // 改权限（八进制字符串）
    app.file_chmod("/tmp/hello.txt", "600").await;
    let st = fx
        .call(
            methods::FILE_STAT,
            json!({ "host": "mockbox", "path": "/tmp/hello.txt", "actor": "ai" }),
        )
        .await
        .unwrap();
    assert_eq!(st["entry"]["mode"], 0o600);

    // 非法权限输入不该落到 IPC 上
    app.file_chmod("/tmp/hello.txt", "abc").await;
    assert!(app.notice().contains("不是八进制"));

    // 删文件（非目录）
    app.file_delete(false, "/tmp/up.txt", false).await;
    assert!(!fs.exists("/tmp/up.txt"));

    let _ = std::fs::remove_dir_all(&tmp);
}

/// 标记多选 + 传输队列：多块传输、进度、失败重试（FR-4.4）
#[tokio::test]
async fn tui_transfer_queue_marks_chunks_and_retries() {
    let (fx, fs) = fixture_sftp().await;

    let tmp = std::env::temp_dir().join(format!("sh0-w44-{}", sh0_core::new_session_id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("a.txt"), "aaa").unwrap();
    std::fs::write(tmp.join("b.txt"), "bbb").unwrap();
    // 600 KB：超过 256 KB 的块大小，必须分多块才能传完
    let big = vec![7u8; 600 * 1024];
    std::fs::write(tmp.join("big.dat"), &big).unwrap();

    let events = tokio::sync::mpsc::unbounded_channel().1;
    let mut app =
        sh0_tui::App::new(fx.client.clone(), events, fx.store.clone(), AppConfig::default());
    app.open("mockbox").await.unwrap();
    app.enter_files().await;
    app.local_files_mut().goto(tmp.to_string_lossy().to_string());
    app.local_files_mut().reload_local();

    // Space 标记两个文件 → F5 应该把两个都排进队列
    for name in ["a.txt", "b.txt"] {
        let i = app.local_files().entries.iter().position(|e| e.name == name).unwrap();
        app.local_files_mut().selected = i;
        app.toggle_mark();
    }
    assert_eq!(app.marks(true).len(), 2, "标记应落在左栏");

    app.file_transfer().await;
    assert_eq!(app.transfers().len(), 2, "两个标记项都应入队");
    assert_eq!(app.marks(true).len(), 0, "传完该清空标记，避免重复传");
    drain_transfers(&mut app).await;

    assert_eq!(fs.read("/tmp/a.txt").unwrap(), b"aaa");
    assert_eq!(fs.read("/tmp/b.txt").unwrap(), b"bbb");
    assert!(
        app.transfers()
            .iter()
            .all(|t| t.state == sh0_tui::app::TransferState::Done),
        "全部应完成：{:?}",
        app.transfers().iter().map(|t| t.state).collect::<Vec<_>>()
    );

    // 大文件分块：字节数必须严丝合缝，多一块少一块都会体现在长度上
    let i = app.local_files().entries.iter().position(|e| e.name == "big.dat").unwrap();
    app.local_files_mut().selected = i;
    app.file_transfer().await;
    drain_transfers(&mut app).await;
    assert_eq!(fs.read("/tmp/big.dat").unwrap().len(), big.len(), "分块传输不能丢字节");
    let last = app.transfers().last().unwrap();
    assert_eq!(last.done, big.len() as u64, "进度应累计到完整大小");
    assert!(last.speed > 0, "完成时应有速度统计");

    // 传到不存在的目录：重试到上限后失败，而不是把队列卡死
    app.remote_files_mut().goto("/nope");
    let i = app.local_files().entries.iter().position(|e| e.name == "a.txt").unwrap();
    app.local_files_mut().selected = i;
    app.file_transfer().await;
    drain_transfers(&mut app).await;
    let failed = app.transfers().last().unwrap();
    assert_eq!(failed.state, sh0_tui::app::TransferState::Failed);
    assert_eq!(failed.retries, 3, "应重试到上限再放弃");
    assert!(failed.error.is_some());

    // R 重新排队、c 清理已完成项
    app.retry_failed();
    assert!(app.has_pending_transfer(), "R 应把失败项重新排队");
    app.clear_finished();
    assert!(app.transfers().iter().all(|t| !t.is_finished()));

    let _ = std::fs::remove_dir_all(&tmp);
}

/// `ai file upload/download` 端到端（FR-6.6 / W4.5）：分块、二进制安全
#[tokio::test]
async fn ai_file_upload_download_roundtrip() {
    let (fx, fs) = fixture_sftp().await;

    let tmp = std::env::temp_dir().join(format!("sh0-w45-{}", sh0_core::new_session_id()));
    std::fs::create_dir_all(&tmp).unwrap();
    // 400 KB 二进制：必然跨块，也必然走 base64
    let bin: Vec<u8> = (0..=255u8).cycle().take(400 * 1024).collect();
    let src = tmp.join("payload.bin");
    std::fs::write(&src, &bin).unwrap();

    let upload = sh0_ai::AiArgs {
        object: sh0_ai::AiObject::File {
            cmd: sh0_ai::FileCmd::Upload {
                session: None,
                host: Some("mockbox".into()),
                local: src.to_string_lossy().to_string(),
                remote: "/tmp/payload.bin".into(),
            },
        },
        json: false,
        quiet: true,
        trace_id: Some("w45-upload".into()),
        timeout: 30,
        yes: false,
        dry_run: false,
    };
    assert_eq!(sh0_ai::run(upload, Some(fx.client.clone())).await, 0, "upload 应成功");
    assert_eq!(fs.read("/tmp/payload.bin").unwrap(), bin, "上传的字节必须逐字节一致");

    // 下载回来
    let dst = tmp.join("back.bin");
    let download = sh0_ai::AiArgs {
        object: sh0_ai::AiObject::File {
            cmd: sh0_ai::FileCmd::Download {
                session: None,
                host: Some("mockbox".into()),
                remote: "/tmp/payload.bin".into(),
                local: dst.to_string_lossy().to_string(),
            },
        },
        json: false,
        quiet: true,
        trace_id: None,
        timeout: 30,
        yes: false,
        dry_run: false,
    };
    assert_eq!(sh0_ai::run(download, Some(fx.client.clone())).await, 0, "download 应成功");
    assert_eq!(std::fs::read(&dst).unwrap(), bin, "下载回来也该逐字节一致");

    // 本地文件不存在：应当是参数错误，而不是崩在 IPC 上
    let bad = sh0_ai::AiArgs {
        object: sh0_ai::AiObject::File {
            cmd: sh0_ai::FileCmd::Upload {
                session: None,
                host: Some("mockbox".into()),
                local: tmp.join("nope.bin").to_string_lossy().to_string(),
                remote: "/tmp/nope.bin".into(),
            },
        },
        json: false,
        quiet: true,
        trace_id: None,
        timeout: 30,
        yes: false,
        dry_run: false,
    };
    assert_eq!(
        sh0_ai::run(bad, Some(fx.client.clone())).await,
        sh0_ai::exit_code::INVALID_ARGS,
        "本地文件不存在应报参数错误"
    );

    // trace_id 落进了审计（FR-6.9）
    let log = fx.call(methods::LOG_QUERY, json!({})).await.unwrap();
    assert!(
        log["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["trace_id"] == "w45-upload"),
        "上传的 trace_id 应出现在审计里"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

// ---------------------------------------------------------------------------
// M3 W3.1：广播（FR-5.1）
// ---------------------------------------------------------------------------

/// 广播把同一份输入投给所有参与会话；未开启时必须显式指定名单
#[tokio::test]
async fn broadcast_fans_out_to_all_sessions() {
    let fx = fixture().await;

    let a = fx
        .call(methods::SESSION_OPEN, json!({ "host": "mockbox" }))
        .await
        .unwrap();
    fx.call(methods::SESSION_OPEN, json!({ "host": "mockbox" }))
        .await
        .unwrap();
    let sa = a["id"].as_str().unwrap().to_string();

    // 广播没开、也没给名单：直接拒绝（免得一次误触打进所有机器）
    let e = fx
        .call_err(methods::BROADCAST_SEND, json!({ "data": "ls\n" }))
        .await;
    assert_eq!(err_code(&e), "invalid_args");

    // 开启广播：不给名单 = 全部已打开会话
    let st = fx
        .call(methods::BROADCAST_SET, json!({ "enabled": true }))
        .await
        .unwrap();
    assert!(st["enabled"].as_bool().unwrap());
    assert!(st["all"].as_bool().unwrap(), "不给名单应等于全部会话");

    let before = fx.factory.sends.lock().unwrap().len();
    let r = fx
        .call(methods::BROADCAST_SEND, json!({ "data": "ls -la\n" }))
        .await
        .unwrap();
    assert_eq!(r["sent"].as_u64().unwrap(), 2, "两台都该收到: {r}");
    // `sent` 只代表已投递到会话通道；mock 传输层的 send 在会话任务里异步执行，
    // 这里让出一次调度再断言实际收到的字节。
    tokio::time::sleep(Duration::from_millis(20)).await;
    let hits = fx.factory.sends.lock().unwrap()[before..]
        .iter()
        .filter(|s| *s == "ls -la\n")
        .count();
    assert_eq!(hits, 2, "同一份输入应各发一份: {hits}");

    // 显式指定一台：只到那台
    let before2 = fx.factory.sends.lock().unwrap().len();
    let r2 = fx
        .call(
            methods::BROADCAST_SEND,
            json!({ "data": "whoami\n", "sessions": [&sa] }),
        )
        .await
        .unwrap();
    assert_eq!(r2["sent"].as_u64().unwrap(), 1);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let hits2 = fx.factory.sends.lock().unwrap()[before2..]
        .iter()
        .filter(|s| *s == "whoami\n")
        .count();
    assert_eq!(hits2, 1, "指定名单时只发一份: {hits2}");

    // 关掉后仍不指定 → 依旧拒绝
    fx.call(methods::BROADCAST_SET, json!({ "enabled": false }))
        .await
        .unwrap();
    let e2 = fx
        .call_err(methods::BROADCAST_SEND, json!({ "data": "x" }))
        .await;
    assert_eq!(err_code(&e2), "invalid_args");

    // 广播落审计（一条汇总，不是逐台）
    let log = fx.call(methods::LOG_QUERY, json!({ "limit": 50 })).await.unwrap();
    let rows = log["records"].as_array().unwrap();
    let bc = rows.iter().find(|r| r["action"] == "broadcast.send");
    assert!(bc.is_some(), "广播应落审计: {rows:?}");
    assert_eq!(bc.unwrap()["decision"], "sent");
}

/// TUI 的广播开关与逐台勾选要落到 daemon（名单在 daemon 侧生效）
#[tokio::test]
async fn tui_broadcast_toggle_and_mark() {
    let fx = fixture().await;
    let events = tokio::sync::mpsc::unbounded_channel().1;
    let mut app = sh0_tui::App::new(
        fx.client.clone(),
        events,
        fx.store.clone(),
        AppConfig::default(),
    );
    app.open("mockbox").await.unwrap();
    app.open("mockbox").await.unwrap();

    assert!(!app.broadcast(), "默认不开广播");
    app.toggle_broadcast().await;
    assert!(app.broadcast());

    let st = fx.call(methods::BROADCAST_STATUS, json!({})).await.unwrap();
    assert!(st["enabled"].as_bool().unwrap(), "TUI 开关要同步到 daemon");
    assert!(st["all"].as_bool().unwrap(), "默认目标为全部会话");

    // 勾选当前会话 → 名单变成显式一份
    app.mark_broadcast().await;
    let st2 = fx.call(methods::BROADCAST_STATUS, json!({})).await.unwrap();
    assert!(!st2["all"].as_bool().unwrap(), "勾选后目标应为指定名单");
    assert_eq!(st2["sessions"].as_array().unwrap().len(), 1);

    // 再点一次 = 取消勾选，回到"全部"
    app.mark_broadcast().await;
    let st3 = fx.call(methods::BROADCAST_STATUS, json!({})).await.unwrap();
    assert!(st3["all"].as_bool().unwrap(), "取消勾选后应回到全部会话");
}

// ---------------------------------------------------------------------------
// M5 W5.1（先行）：鼠标操作主机树（FR-3.9）
// ---------------------------------------------------------------------------

/// 左键按下
fn mouse_down(col: u16, row: u16) -> crossterm::event::MouseEvent {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn mouse_event(
    kind: crossterm::event::MouseEventKind,
    col: u16,
    row: u16,
    modifiers: crossterm::event::KeyModifiers,
) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind,
        column: col,
        row,
        modifiers,
    }
}

/// 鼠标点击主机树：单击选中、双击连接、滚轮滚动；点在树外不响应（FR-3.9）
///
/// 不真的渲染一帧——直接把布局矩形喂给 App（真实渲染时由 `draw_tree` 写入同一个字段）。
#[tokio::test]
async fn tui_mouse_click_selects_and_double_click_connects() {
    use crossterm::event::{KeyModifiers, MouseEventKind};

    let fx = fixture_with_hosts(
        EchoShellFactory::default(),
        vec![
            host_named("h1", &["prod"], &[]),
            host_named("h2", &["prod"], &[]),
        ],
    )
    .await;
    let events = tokio::sync::mpsc::unbounded_channel().1;
    let mut app = sh0_tui::App::new(
        fx.client.clone(),
        events,
        fx.store.clone(),
        AppConfig::default(),
    );

    // 主机树内区画在 (1,2) 起、26×8：第 0 行是分组 prod，第 1/2 行是 h1 / h2
    app.set_tree_area(ratatui::layout::Rect::new(1, 2, 26, 8));

    // 1) 单击 h1 那一行：选中它，并把焦点交给树（省掉先按 Ctrl+B）
    app.on_mouse(mouse_down(3, 3)).await.unwrap();
    assert_eq!(app.tree_selection(), 1, "点击第 1 行应选中 h1");
    assert!(app.focus_is_tree(), "点在树上应把焦点给树");
    assert_eq!(app.tab_count(), 0, "单击不发起连接");

    // 2) 同一行再点一次（双击）→ 发起连接
    app.on_mouse(mouse_down(3, 3)).await.unwrap();
    assert_eq!(app.tab_count(), 1, "双击应打开一个会话");

    // 3) 滚轮在树区滚动选择
    app.on_mouse(mouse_event(MouseEventKind::ScrollDown, 3, 3, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(app.tree_selection(), 2, "滚轮下滚应选中下一行");
    app.on_mouse(mouse_event(MouseEventKind::ScrollUp, 3, 3, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(app.tree_selection(), 1, "滚轮上滚应回到上一行");

    // 4) 点在树区之外（终端区）不该动选择
    app.on_mouse(mouse_down(60, 5)).await.unwrap();
    assert_eq!(app.tree_selection(), 1, "树区外的点击不该改变选中行");
    assert_eq!(app.tab_count(), 1, "树区外的点击不该开会话");

    // 5) 点在可见行之外（第 20 行 > 8 行高度）也不该越界
    app.on_mouse(mouse_down(3, 20)).await.unwrap();
    assert_eq!(app.tree_selection(), 1, "超出可见区的点击应忽略");
}

/// 命中测试用的矩形必须来自**真实渲染**，不是拍脑袋写死的常量：
/// 渲染一帧后按渲染结果点击，验证 draw → 记录矩形 → 命中 这条链路是通的。
#[tokio::test]
async fn tui_mouse_hit_test_uses_the_rendered_tree_area() {
    let fx = fixture_with_hosts(
        EchoShellFactory::default(),
        vec![
            host_named("h1", &["prod"], &[]),
            host_named("h2", &["prod"], &[]),
        ],
    )
    .await;
    let events = tokio::sync::mpsc::unbounded_channel().1;
    let mut app = sh0_tui::App::new(
        fx.client.clone(),
        events,
        fx.store.clone(),
        AppConfig::default(),
    );

    // 未渲染过时矩形是全零，点击不该命中
    assert_eq!(app.tree_area().height, 0, "还没画过就不该有命中区");

    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut term = ratatui::Terminal::new(backend).unwrap();
    term.draw(|f| app.draw(f)).unwrap();

    // 左栏 28 列 + 边框 → 内区 x=1；上面让给标签栏一行 + 边框 → 内区 y=2
    let area = app.tree_area();
    assert_eq!((area.x, area.y), (1, 2), "渲染后应记下主机树内区: {area:?}");
    assert!(area.height >= 3, "要装得下分组行 + 两台主机: {area:?}");

    // 按渲染出来的坐标点第二行（h1）
    app.on_mouse(mouse_down(area.x + 2, area.y + 1)).await.unwrap();
    assert_eq!(app.tree_selection(), 1, "按渲染坐标点击应命中 h1");
}

/// Shift + 鼠标 = 把鼠标交回父终端（拖选复制要用），覆盖期间不接受点击
#[tokio::test]
async fn tui_mouse_shift_hands_back_to_terminal() {
    use crossterm::event::{KeyModifiers, MouseEventKind};

    let fx = fixture_with_hosts(
        EchoShellFactory::default(),
        vec![host_named("h1", &["prod"], &[])],
    )
    .await;
    let events = tokio::sync::mpsc::unbounded_channel().1;
    let mut app = sh0_tui::App::new(
        fx.client.clone(),
        events,
        fx.store.clone(),
        AppConfig::default(),
    );
    app.set_tree_area(ratatui::layout::Rect::new(1, 2, 26, 8));

    // 先把捕获打开（测试环境 stdout 未必是终端，拿不到就算了，不强断言）
    app.restore_mouse();
    if app.mouse_captured() {
        app.on_mouse(mouse_event(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            3,
            3,
            KeyModifiers::SHIFT,
        ))
        .await
        .unwrap();
        assert!(!app.mouse_captured(), "Shift + 鼠标后应挂起捕获");
        assert_eq!(app.tree_selection(), 0, "挂起期间不该处理点击");
    }

    // 配置关掉鼠标时，一切鼠标事件都不处理
    let cfg = AppConfig {
        mouse: false,
        ..AppConfig::default()
    };
    let events2 = tokio::sync::mpsc::unbounded_channel().1;
    let mut off = sh0_tui::App::new(fx.client.clone(), events2, fx.store.clone(), cfg);
    off.set_tree_area(ratatui::layout::Rect::new(1, 2, 26, 8));
    off.on_mouse(mouse_down(3, 3)).await.unwrap();
    assert_eq!(off.tree_selection(), 0, "mouse=false 时点击应完全忽略");
    assert!(!off.mouse_captured(), "mouse=false 时不该开捕获");
}

// ---------------------------------------------------------------------------
// M5 补完：标签栏点击、分隔条拖拽、终端滚轮与拖选（FR-3.9 剩下的三项）
// ---------------------------------------------------------------------------

/// 直接构造 App（几个鼠标用例都要，省得每个都抄一遍）
fn new_app(fx: &Fixture) -> sh0_tui::App {
    let events = tokio::sync::mpsc::unbounded_channel().1;
    sh0_tui::App::new(fx.client.clone(), events, fx.store.clone(), AppConfig::default())
}

/// 标签栏：点主体切会话、点 × 关会话、中键也关（FR-3.9）
#[tokio::test]
async fn tui_mouse_click_tab_switches_and_closes() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::layout::Rect;

    let fx = fixture_with_hosts(
        EchoShellFactory::default(),
        vec![host_named("web-01", &[], &[]), host_named("db-01", &[], &[])],
    )
    .await;
    let mut app = new_app(&fx);
    app.open("web-01").await.unwrap();
    app.open("db-01").await.unwrap();
    assert_eq!(app.tab_count(), 2);

    let bar = Rect::new(0, 0, 80, 1);
    app.set_tab_bar_area(bar);
    let hits = app.tab_hits(bar);
    assert_eq!(hits.len(), 2, "两个标签都该有命中区");
    assert_eq!(app.active_tab(), 1, "刚打开的会话是当前的");

    // 1) 点第一个标签的主体 → 切回去
    app.on_mouse(mouse_down(hits[0].body.0 + 1, 0)).await.unwrap();
    assert_eq!(app.active_tab(), 0, "点标签主体应切到该会话");
    // 再点第二个
    let hits = app.tab_hits(bar);
    app.on_mouse(mouse_down(hits[1].body.0 + 1, 0)).await.unwrap();
    assert_eq!(app.active_tab(), 1);

    // 2) 点第一个标签的 × → 关掉它（当前会话仍是 db-01，active 要往回退一格）
    let hits = app.tab_hits(bar);
    assert!(hits[0].close.1 > hits[0].close.0, "开了鼠标就该画出 ×");
    app.on_mouse(mouse_down(hits[0].close.0, 0)).await.unwrap();
    assert_eq!(app.tab_count(), 1, "点 × 应关掉那个会话");
    assert_eq!(app.active_tab(), 0);

    // 3) 中键关标签：浏览器习惯
    app.open("web-01").await.unwrap();
    let hits = app.tab_hits(bar);
    app.on_mouse(mouse_event(
        MouseEventKind::Down(MouseButton::Middle),
        hits[0].body.0 + 1,
        0,
        KeyModifiers::NONE,
    ))
    .await
    .unwrap();
    assert_eq!(app.tab_count(), 1, "中键应关掉被点的那个标签");
}

/// 拖拽主机树与主区之间的分隔条（FR-3.9）
#[tokio::test]
async fn tui_mouse_drag_divider_resizes_tree() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::layout::Rect;

    let fx = fixture_with_hosts(EchoShellFactory::default(), vec![host_named("h1", &[], &[])])
        .await;
    let mut app = new_app(&fx);
    // 主机树宽 28，主区从 x=28 开始
    app.set_main_area(Rect::new(28, 1, 52, 20));
    let w0 = app.tree_width();

    // 按在缝上（树的最右一列）
    app.on_mouse(mouse_down(w0 - 1, 5)).await.unwrap();
    // 拖到 40 列
    app.on_mouse(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        40,
        5,
        KeyModifiers::NONE,
    ))
    .await
    .unwrap();
    assert!(app.tree_width() > w0, "往右拖应该把树拖宽：{w0} → {}", app.tree_width());

    // 松手：把新尺寸同步下去（没有会话也不会崩）
    app.on_mouse(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        40,
        5,
        KeyModifiers::NONE,
    ))
    .await
    .unwrap();

    // 往回拖：不能把终端挤到没地方显示
    let w1 = app.tree_width();
    app.on_mouse(mouse_down(w1 - 1, 5)).await.unwrap();
    app.on_mouse(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        0,
        5,
        KeyModifiers::NONE,
    ))
    .await
    .unwrap();
    assert!(
        app.tree_width() >= 12,
        "树再窄也得留 12 列，现在是 {}",
        app.tree_width()
    );
}

/// 滚轮在终端区回看历史（FR-3.9 的滚动一侧）
#[tokio::test]
async fn tui_mouse_wheel_scrolls_terminal() {
    use crossterm::event::{KeyModifiers, MouseEventKind};
    use ratatui::layout::Rect;

    let fx = fixture_with_hosts(EchoShellFactory::default(), vec![host_named("h1", &[], &[])])
        .await;
    let mut app = new_app(&fx);
    app.open("h1").await.unwrap();
    // 喂够多的行，让 scrollback 里真有东西可回看
    let lines: Vec<&str> = (0..80).map(|_| "some output line").collect();
    app.feed_lines(0, &lines);
    assert_eq!(app.scroll_offset(), 0, "刚连上应该贴着最新输出");

    app.set_main_area(Rect::new(28, 1, 52, 20));
    app.on_mouse(mouse_event(MouseEventKind::ScrollUp, 40, 5, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(app.scroll_offset(), 3, "滚轮向上 = 往回看 3 行");

    app.on_mouse(mouse_event(MouseEventKind::ScrollDown, 40, 5, KeyModifiers::NONE))
        .await
        .unwrap();
    assert_eq!(app.scroll_offset(), 0, "滚轮向下应回到最新");
}

/// 终端里拖选文本，松手即复制（FR-3.9）
#[tokio::test]
async fn tui_mouse_drag_selects_text_and_copies() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::layout::Rect;

    let fx = fixture_with_hosts(EchoShellFactory::default(), vec![host_named("h1", &[], &[])])
        .await;
    let mut app = new_app(&fx);
    app.open("h1").await.unwrap();
    app.feed_lines(0, &["hello world", "second line"]);
    // 主区从 (0,1) 开始：屏幕 y=1 就是视图第 0 行
    app.set_main_area(Rect::new(0, 1, 80, 20));

    // 从 "hello world" 的 h 拖到 o（列 0 → 4）
    app.on_mouse(mouse_down(0, 1)).await.unwrap();
    app.on_mouse(mouse_event(
        MouseEventKind::Drag(MouseButton::Left),
        4,
        1,
        KeyModifiers::NONE,
    ))
    .await
    .unwrap();
    assert!(app.selection().is_some(), "拖动时应有选区");

    app.on_mouse(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        4,
        1,
        KeyModifiers::NONE,
    ))
    .await
    .unwrap();
    assert!(app.selection().is_none(), "松手复制完该清掉选区");
    assert_eq!(app.last_copy(), "hello", "拖到哪就该复制到哪");
}

/// 文件视图：点条目选中并把焦点给那一栏，双击进入目录（FR-3.9 的鼠标一侧）
#[tokio::test]
async fn tui_mouse_click_file_row_selects_and_double_click_enters() {
    use ratatui::layout::Rect;

    let (fx, _fs) = fixture_sftp().await;
    let events = tokio::sync::mpsc::unbounded_channel().1;
    let mut app = sh0_tui::App::new(fx.client.clone(), events, fx.store.clone(), AppConfig::default());
    app.open("mockbox").await.unwrap();
    app.enter_files().await;
    assert!(app.in_file_view());

    // 左栏 (1,2) 起、右栏 (41,2) 起，各自 38×18 的内区
    let left = Rect::new(1, 2, 38, 18);
    let right = Rect::new(41, 2, 38, 18);
    app.set_pane_areas(left, right);

    // 点右栏第 0 行（logs）：选中它，并把焦点从默认的左栏挪到右栏
    assert!(app.file_focus_is_left(), "默认焦点在左栏");
    app.remote_files(); // 右栏开局已列过目录
    app.on_mouse(mouse_down(right.x + 2, right.y)).await.unwrap();
    assert!(!app.file_focus_is_left(), "点右栏应把焦点给右栏");
    assert_eq!(app.remote_files().selected, 0);

    // 双击同一个条目 → 进入目录（与回车同一条路）
    app.on_mouse(mouse_down(right.x + 2, right.y)).await.unwrap();
    assert_eq!(app.remote_files().cwd, "/tmp/logs", "双击目录应进入");
}

/// 只是点一下（没拖动）不该往剪贴板里塞东西
#[tokio::test]
async fn tui_mouse_click_without_drag_clears_selection() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::layout::Rect;

    let fx = fixture_with_hosts(EchoShellFactory::default(), vec![host_named("h1", &[], &[])])
        .await;
    let mut app = new_app(&fx);
    app.open("h1").await.unwrap();
    app.feed_lines(0, &["hello world"]);
    app.set_main_area(Rect::new(0, 1, 80, 20));

    app.on_mouse(mouse_down(0, 1)).await.unwrap();
    app.on_mouse(mouse_event(
        MouseEventKind::Up(MouseButton::Left),
        0,
        1,
        KeyModifiers::NONE,
    ))
    .await
    .unwrap();
    assert!(app.selection().is_none(), "没拖动就不该留下选区");
    assert!(app.last_copy().is_empty(), "没拖动就不该复制出东西");
}

// ---------------------------------------------------------------------------
// M3 W3.2 / W3.3：批量执行编排与结果汇总（FR-5.2 / FR-5.3）
// ---------------------------------------------------------------------------

/// 主机选择器：显式列表 / 分组前缀 / 标签 / 全部（FR-5.2）
#[tokio::test]
async fn batch_selector_picks_hosts_by_list_group_tag_all() {
    let fx = fixture_with_hosts(
        EchoShellFactory::default(),
        vec![
            host_named("web1", &["prod", "web"], &["web"]),
            host_named("web2", &["prod", "web"], &["web"]),
            host_named("db1", &["prod", "db"], &["db"]),
            host_named("dev1", &["dev"], &[]),
        ],
    )
    .await;

    // dry-run 不建连：只回答"选到了谁、风险几档"
    let g = fx
        .call(
            methods::BATCH_RUN,
            json!({ "cmd": "df -h", "group": "prod", "dry_run": true }),
        )
        .await
        .unwrap();
    assert_eq!(g["total"], 3, "prod 分组应命中 3 台: {g}");

    let t = fx
        .call(
            methods::BATCH_RUN,
            json!({ "cmd": "df -h", "tag": "web", "dry_run": true }),
        )
        .await
        .unwrap();
    assert_eq!(t["total"], 2, "web 标签应命中 2 台: {t}");

    let l = fx
        .call(
            methods::BATCH_RUN,
            json!({ "cmd": "df -h", "hosts": ["web1", "db1"], "dry_run": true }),
        )
        .await
        .unwrap();
    assert_eq!(l["total"], 2);

    let a = fx
        .call(
            methods::BATCH_RUN,
            json!({ "cmd": "df -h", "all": true, "dry_run": true }),
        )
        .await
        .unwrap();
    assert_eq!(a["total"], 4);

    // 空选择器：不隐式等于"全部"
    let e = fx
        .call_err(methods::BATCH_RUN, json!({ "cmd": "df -h" }))
        .await;
    assert_eq!(err_code(&e), "invalid_args");

    // 名单里的名字不存在 → 整批失败（少跑一台比跑不了更难发现）
    let e2 = fx
        .call_err(methods::BATCH_RUN, json!({ "cmd": "df -h", "hosts": ["nope"] }))
        .await;
    assert_eq!(err_code(&e2), "host_not_found");
}

/// 30 台并发 16 跑 `df -h /`：验收要求总耗时 ≤ 10s（FR-5.2 / FR-5.4）
#[tokio::test]
async fn batch_runs_in_parallel_within_budget() {
    let fx = fixture_with_hosts(
        EchoShellFactory::with_exec(vec![ExecOutput {
            stdout: "/dev/root  20G  8G  12G  40% /".into(),
            stderr: String::new(),
            exit_code: Some(0),
        }]),
        (0..30).map(|i| host_named(&format!("h{i}"), &[], &[])).collect(),
    )
    .await;

    let started = std::time::Instant::now();
    let d = fx
        .call(
            methods::BATCH_RUN,
            json!({
                "cmd": "df -h /",
                "all": true,
                "mode": "exec",
                "concurrency": 16,
                "timeout_ms": 5000,
            }),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(d["total"], 30);
    assert_eq!(d["ok"], 30, "全部成功: {d}");
    assert_eq!(d["concurrency"], 16);
    assert_eq!(d["results"].as_array().unwrap().len(), 30);
    assert!(
        elapsed < Duration::from_secs(10),
        "30 台并发 16 应 ≤10s，实际 {elapsed:?}"
    );
    // 每台都有退出码与耗时：汇总视图靠这些字段着色
    assert_eq!(d["results"][0]["exit_code"], 0);
    assert!(d["results"][0]["duration_ms"].is_i64());
}

/// 部分失败要如实汇总；fail_fast 时剩下的不再跑（FR-5.3 / FR-5.4）
#[tokio::test]
async fn batch_reports_partial_failure_and_fail_fast() {
    let f = EchoShellFactory::with_exec(vec![ExecOutput {
        stdout: "ok".into(),
        stderr: String::new(),
        exit_code: Some(0),
    }]);
    f.fail_hosts.lock().unwrap().insert("h1".into());
    let fx = fixture_with_hosts(
        f,
        vec![
            host_named("h0", &[], &[]),
            host_named("h1", &[], &[]),
            host_named("h2", &[], &[]),
        ],
    )
    .await;

    // 默认：三台都跑，失败的那台如实标记
    let d = fx
        .call(
            methods::BATCH_RUN,
            json!({ "cmd": "ls", "all": true, "mode": "exec" }),
        )
        .await
        .unwrap();
    assert_eq!(d["total"], 3);
    assert_eq!(d["ok"], 2, "应有两台成功: {d}");
    assert_eq!(d["failed"], 1, "应有一台失败: {d}");
    let bad = d["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["host"] == "h1")
        .unwrap();
    assert_eq!(bad["status"], "error");
    assert!(bad["error"].as_str().unwrap().contains("h1"));

    // fail_fast + 并发 1：h1 失败后 h2 不再执行
    let d2 = fx
        .call(
            methods::BATCH_RUN,
            json!({
                "cmd": "ls",
                "all": true,
                "mode": "exec",
                "fail_fast": true,
                "concurrency": 1,
            }),
        )
        .await
        .unwrap();
    assert_eq!(d2["failed"], 1);
    assert!(
        d2["skipped"].as_u64().unwrap() >= 1,
        "fail_fast 应让剩余主机被跳过: {d2}"
    );
}

/// 批量同样被风险引擎拦截：deny 档整批拒绝，confirm 档要令牌（FR-5.5）
#[tokio::test]
async fn batch_is_gated_by_risk_engine() {
    let fx = fixture_with_hosts(
        EchoShellFactory::with_exec(vec![ExecOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        }]),
        vec![host_named("h0", &[], &[]), host_named("h1", &[], &[])],
    )
    .await;

    // deny：--yes 也拦不住
    let e = fx
        .call_err(
            methods::BATCH_RUN,
            json!({ "cmd": "rm -rf /", "all": true, "mode": "exec", "yes": true }),
        )
        .await;
    assert_eq!(err_code(&e), "risk_denied");

    // confirm：先要令牌
    let e2 = fx
        .call_err(
            methods::BATCH_RUN,
            json!({ "cmd": "systemctl restart nginx", "all": true, "mode": "exec" }),
        )
        .await;
    assert_eq!(err_code(&e2), "needs_confirmation");
    let data = match &e2 {
        IpcError::Remote { data, .. } => data.clone().unwrap(),
        other => panic!("期望带 data 的错误信封: {other:?}"),
    };
    let token = data["confirm_token"].as_str().unwrap().to_string();
    assert_eq!(data["hosts"].as_array().unwrap().len(), 2, "令牌里要带目标主机清单");

    // 带令牌重放 → 放行
    let d = fx
        .call(
            methods::BATCH_RUN,
            json!({
                "cmd": "systemctl restart nginx",
                "all": true,
                "mode": "exec",
                "confirm_token": token,
            }),
        )
        .await
        .unwrap();
    assert_eq!(d["ok"], 2, "令牌放行后整批执行: {d}");

    // 令牌一次性：再用来一次应当失效
    let e3 = fx
        .call_err(
            methods::BATCH_RUN,
            json!({
                "cmd": "systemctl restart nginx",
                "all": true,
                "mode": "exec",
                "confirm_token": token,
            }),
        )
        .await;
    assert_eq!(err_code(&e3), "invalid_args");
}

/// 批量只扣一次速率配额（FR-5.5）：30 台不等于 30 次
#[tokio::test]
async fn batch_consumes_one_quota_not_per_host() {
    let mut cfg = AppConfig::default();
    cfg.limits.rate_limit_per_min = 2; // 配额给 2：1 次批量 + 1 次单台，之后应被限流
    let fx = fixture_with_hosts_cfg(
        EchoShellFactory::with_exec(vec![ExecOutput {
            stdout: "ok".into(),
            stderr: String::new(),
            exit_code: Some(0),
        }]),
        cfg,
        (0..30).map(|i| host_named(&format!("h{i}"), &[], &[])).collect(),
    )
    .await;

    // 30 台整批：只算 1 次
    let d = fx
        .call(
            methods::BATCH_RUN,
            json!({ "cmd": "df -h", "all": true, "mode": "exec", "concurrency": 16 }),
        )
        .await
        .unwrap();
    assert_eq!(d["ok"], 30, "30 台只扣一次配额，不该被限流: {d}");

    // 第 2 次：单台 exec 还能过
    fx.call(
        methods::EXEC_RUN,
        json!({ "host": "h0", "cmd": "df -h", "mode": "exec" }),
    )
    .await
    .expect("配额还剩一次，单台 exec 应放行");

    // 第 3 次：配额用尽
    let e = fx
        .call_err(
            methods::EXEC_RUN,
            json!({ "host": "h0", "cmd": "df -h", "mode": "exec" }),
        )
        .await;
    assert_eq!(err_code(&e), "rate_limited");
}

// ---------------------------------------------------------------------------
// M3 W3.5：批量历史持久化与回看（OQ-5）
// ---------------------------------------------------------------------------

/// 批量执行要落库并可按 run_id 回看（汇总 + 逐台明细）
#[tokio::test]
async fn batch_history_is_persisted_and_replayable() {
    let fx = fixture_with_hosts(
        EchoShellFactory::with_exec(vec![ExecOutput {
            stdout: "/dev/root 40%".into(),
            stderr: String::new(),
            exit_code: Some(0),
        }]),
        vec![
            host_named("h0", &[], &[]),
            host_named("h1", &[], &[]),
            host_named("h2", &[], &[]),
        ],
    )
    .await;

    let d = fx
        .call(
            methods::BATCH_RUN,
            json!({
                "cmd": "df -h /",
                "all": true,
                "mode": "exec",
                "concurrency": 2,
                "trace_id": "trace-batch-1",
                "actor": "ai",
            }),
        )
        .await
        .unwrap();
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert!(run_id.starts_with("br_"), "run_id 应可检索: {run_id}");

    // 列表里有它
    let list = fx
        .call(methods::BATCH_LIST, json!({ "limit": 10 }))
        .await
        .unwrap();
    let runs = list["runs"].as_array().unwrap();
    assert!(runs.iter().any(|r| r["run_id"] == run_id), "列表应有本次: {list}");
    assert_eq!(runs[0]["total"], 3);
    assert_eq!(runs[0]["ok"], 3);
    assert_eq!(runs[0]["command"], "df -h /");
    assert_eq!(runs[0]["trace_id"], "trace-batch-1");

    // 明细：3 台都在，且带退出码
    let show = fx
        .call(methods::BATCH_SHOW, json!({ "run_id": run_id }))
        .await
        .unwrap();
    let results = show["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "明细应含 3 台: {show}");
    assert!(results.iter().all(|r| r["status"] == "ok"));
    assert_eq!(show["run"]["run_id"], run_id);

    // 不存在的 run_id → not_found（AI 靠这个区分"没跑过"和"跑过但全失败"）
    let e = fx
        .call_err(methods::BATCH_SHOW, json!({ "run_id": "br_nope" }))
        .await;
    assert_eq!(err_code(&e), "not_found");
}

// ---------------------------------------------------------------------------
// M5 W5.1：命令面板（Ctrl+Shift+P）与主题切换（FR-3.6 / FR-3.7）
// ---------------------------------------------------------------------------

fn key(code: crossterm::event::KeyCode, mods: crossterm::event::KeyModifiers) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, mods)
}

/// Ctrl+Shift+P 唤出面板，输入主机名过滤，回车直连
#[tokio::test]
async fn tui_command_palette_filters_and_connects() {
    use crossterm::event::{KeyCode, KeyModifiers};

    let fx = fixture_with_hosts(
        EchoShellFactory::default(),
        vec![
            host_named("web-01", &["prod"], &[]),
            host_named("db-01", &["prod"], &[]),
        ],
    )
    .await;
    let events = tokio::sync::mpsc::unbounded_channel().1;
    let mut app = sh0_tui::App::new(
        fx.client.clone(),
        events,
        fx.store.clone(),
        AppConfig::default(),
    );

    // 唤出：三套预设都绑了 ctrl+shift+p
    app.on_key(key(KeyCode::Char('P'), KeyModifiers::CONTROL | KeyModifiers::SHIFT))
        .await
        .unwrap();
    assert!(app.palette_open(), "Ctrl+Shift+P 应打开命令面板");
    assert_eq!(app.tab_count(), 0);

    // 输入主机名过滤
    for c in "web".chars() {
        app.on_key(key(KeyCode::Char(c), KeyModifiers::NONE)).await.unwrap();
    }
    assert_eq!(app.palette_input(), "web");
    let hits = app.palette_matches();
    assert!(
        hits.iter().any(|i| i.title == "连接 web-01"),
        "过滤应命中 web-01: {hits:?}"
    );
    assert!(
        !hits.iter().any(|i| i.title == "连接 db-01"),
        "不该再出现 db-01: {hits:?}"
    );

    // 回车执行第一项 = 连接 web-01
    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)).await.unwrap();
    assert!(!app.palette_open(), "执行后面板应关闭");
    assert_eq!(app.tab_count(), 1, "回车应打开会话");
}

/// 面板里能直接切主题，并且要写回 config.toml（下次启动保持一致）
#[tokio::test]
async fn tui_command_palette_switches_theme_and_persists() {
    use crossterm::event::{KeyCode, KeyModifiers};

    let cfg = AppConfig {
        theme: "dark".to_string(),
        ..AppConfig::default()
    };
    let fx = fixture_with_hosts_cfg(EchoShellFactory::default(), cfg, vec![]).await;
    let events = tokio::sync::mpsc::unbounded_channel().1;
    let mut app = sh0_tui::App::new(
        fx.client.clone(),
        events,
        fx.store.clone(),
        fx.store.load_config().unwrap(),
    );

    app.open_palette();
    for c in "light".chars() {
        app.on_key(key(KeyCode::Char(c), KeyModifiers::NONE)).await.unwrap();
    }
    let hits = app.palette_matches();
    let light = hits.iter().find(|i| i.title.contains("light")).expect("应能搜到 light 主题");
    assert_eq!(light.cmd, sh0_tui::palette::PaletteCmd::Theme("light"));

    app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)).await.unwrap();
    assert_eq!(app.theme_name(), "light", "主题应切到 light");
    assert_eq!(
        fx.store.load_config().unwrap().theme,
        "light",
        "主题要写回 config.toml"
    );
}

/// Esc 关闭面板，且关闭后按键恢复正常路由（不会被面板吃掉）
#[tokio::test]
async fn tui_command_palette_esc_cancels() {
    use crossterm::event::{KeyCode, KeyModifiers};

    let fx = fixture_with_hosts(EchoShellFactory::default(), vec![host_named("h1", &[], &[])]).await;
    let events = tokio::sync::mpsc::unbounded_channel().1;
    let mut app = sh0_tui::App::new(
        fx.client.clone(),
        events,
        fx.store.clone(),
        AppConfig::default(),
    );

    app.open_palette();
    app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE)).await.unwrap();
    assert_eq!(app.palette_input(), "x");
    app.on_key(key(KeyCode::Esc, KeyModifiers::NONE)).await.unwrap();
    assert!(!app.palette_open(), "Esc 应关闭面板");

    // 退格不该再被面板吃掉：面板关闭后走的是普通路由
    app.on_key(key(KeyCode::Backspace, KeyModifiers::NONE)).await.unwrap();
    assert!(!app.palette_open(), "关闭后按键不该重新唤出面板");
}

/// 回归：Windows 控制台（与开了 kitty 键盘协议的终端）为**每次**按键都给 Press + Release
/// 两个事件。不过滤 Release，敲 `ls` 就会往远端发两遍变成 `llss`。
/// 这里走的是事件循环用的同一个入口 `dispatch_key`，不是在测试里复制一遍 if。
#[tokio::test]
async fn tui_key_release_event_is_not_treated_as_input() {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    let fx = fixture_with_hosts(
        EchoShellFactory::default(),
        vec![host_named("web-01", &["prod"], &[])],
    )
    .await;
    let events = tokio::sync::mpsc::unbounded_channel().1;
    let mut app = sh0_tui::App::new(
        fx.client.clone(),
        events,
        fx.store.clone(),
        AppConfig::default(),
    );

    // 先单独钉住过滤规则：Release 掉，Press / Repeat 留（Repeat 是长按，属于真实重复）
    let mut k = key(KeyCode::Char('l'), KeyModifiers::NONE);
    assert!(sh0_tui::should_handle_key(k), "Press 应当处理");
    k.kind = KeyEventKind::Repeat;
    assert!(sh0_tui::should_handle_key(k), "Repeat（长按）应当处理");
    k.kind = KeyEventKind::Release;
    assert!(!sh0_tui::should_handle_key(k), "Release 必须丢弃");

    // 再走真实分发路径：命令面板当输入框，按下 + 松开成对投递
    sh0_tui::dispatch_key(
        &mut app,
        key(KeyCode::Char('P'), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
    )
    .await
    .unwrap();
    assert!(app.palette_open(), "面板应先打开");

    for c in "web".chars() {
        let mut down = key(KeyCode::Char(c), KeyModifiers::NONE);
        down.kind = KeyEventKind::Press;
        let mut up = key(KeyCode::Char(c), KeyModifiers::NONE);
        up.kind = KeyEventKind::Release;
        sh0_tui::dispatch_key(&mut app, down).await.unwrap();
        sh0_tui::dispatch_key(&mut app, up).await.unwrap();
    }
    assert_eq!(
        app.palette_input(),
        "web",
        "松开事件不该被算作一次输入（否则会变成 wweebb）"
    );
}

/// 键位归一：终端对 Shift+G 上报大写 G，配置里写的却是 `shift+g`
#[test]
fn keymap_matches_uppercase_shift_letters() {
    use crossterm::event::{KeyCode, KeyModifiers};
    use sh0_term::keymap::{Action, Keymap};

    let km = Keymap::preset("vim");
    let upper = key(KeyCode::Char('G'), KeyModifiers::SHIFT);
    let lower = key(KeyCode::Char('g'), KeyModifiers::SHIFT);
    assert_eq!(
        km.get(upper),
        Some(Action::ScrollBottom),
        "Shift+G（上报为大写）应命中 shift+g 绑定"
    );
    assert_eq!(km.get(lower), Some(Action::ScrollBottom));

    // 三套预设都要能唤出命令面板
    for preset in ["default", "vim", "xshell"] {
        let km = Keymap::preset(preset);
        let ctrl_shift_p = key(KeyCode::Char('P'), KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(
            km.get(ctrl_shift_p),
            Some(Action::CommandPalette),
            "{preset} 预设应绑定 ctrl+shift+p"
        );
    }
}

// ---------------------------------------------------------------------------
// M5 W5.2：配置读写与热重载（FR-9.3 / FR-9.4）
// ---------------------------------------------------------------------------

/// `config.get` / `config.set` 走点路径，拼错键名要报错而不是静默成功
#[tokio::test]
async fn config_get_and_set_roundtrip() {
    let fx = fixture().await;

    // 默认值
    let v = fx.store.config_get("limits.exec_timeout_secs").unwrap().unwrap();
    assert_eq!(v, 30, "默认 exec 超时应是 30s");

    // 写入并回读：数字不能变字符串
    fx.store.config_set("limits.exec_timeout_secs", "60").unwrap();
    let v2 = fx.store.config_get("limits.exec_timeout_secs").unwrap().unwrap();
    assert_eq!(v2, 60);
    let cfg = fx.store.load_config().unwrap();
    assert_eq!(cfg.limits.exec_timeout_secs, 60);

    // 布尔
    fx.store.config_set("mouse", "false").unwrap();
    assert_eq!(fx.store.config_get("mouse").unwrap().unwrap(), false);

    // 字符串项：get 出来不该带引号
    fx.store.config_set("theme", "light").unwrap();
    let t = fx.store.config_get("theme").unwrap().unwrap();
    assert_eq!(t.as_str().unwrap(), "light");
    assert_eq!(sh0::config_cmd::value_to_string(&t), "light");

    // 未知键名 → 报错（拼写错误不能假装成功）
    let e = fx.store.config_set("limits.exec_timeut", "5");
    assert!(e.is_err(), "拼错的键名必须报错");
    assert!(fx.store.config_get("limits.exec_timeut").unwrap().is_none());
}

/// daemon 内存里的配置要能被热重载替换：改文件 → config.reload → 立即生效
#[tokio::test]
async fn config_reload_applies_without_restart() {
    let fx = fixture().await;

    // daemon 当前生效的超时
    let cur = fx.call(methods::CONFIG_GET, json!({})).await.unwrap();
    assert_eq!(cur["config"]["limits"]["exec_timeout_secs"], 30);

    // 改文件
    fx.store.config_set("limits.exec_timeout_secs", "90").unwrap();

    // 没重载前，daemon 里还是旧值（证明"改文件不会自动生效"）
    let before = fx.call(methods::CONFIG_GET, json!({})).await.unwrap();
    assert_eq!(before["config"]["limits"]["exec_timeout_secs"], 30);

    let r = fx.call(methods::CONFIG_RELOAD, json!({})).await.unwrap();
    assert!(r["changed"].as_bool().unwrap(), "有改动应报告 changed");
    let after = fx.call(methods::CONFIG_GET, json!({})).await.unwrap();
    assert_eq!(
        after["config"]["limits"]["exec_timeout_secs"], 90,
        "重载后 daemon 应立刻用上新值"
    );

    // 再重载一次：无变化
    let again = fx.call(methods::CONFIG_RELOAD, json!({})).await.unwrap();
    assert!(!again["changed"].as_bool().unwrap(), "无改动不该重复应用");
}

/// 改了只有重启才生效的项时，重载提示要点出来（免得用户以为改了没用）
#[tokio::test]
async fn config_reload_notes_restart_only_keys() {
    let fx = fixture().await;
    fx.store.config_set("keepalive_secs", "15").unwrap();
    let r = fx.call(methods::CONFIG_RELOAD, json!({})).await.unwrap();
    let note = r["note"].as_str().unwrap();
    assert!(note.contains("keepalive_secs"), "提示应点名 keepalive_secs: {note}");
    assert!(note.contains("重启"), "提示应说明要重启: {note}");
}

// ---------------------------------------------------------------------------
// M5 W5.6：ProxyJump 跳板机（FR-1.9）
// ---------------------------------------------------------------------------

/// 跳板可以是 hosts.toml 里的另一台主机，也可以内联 `user@host:port`
#[tokio::test]
async fn proxy_jump_resolves_from_hosts_and_inline() {
    let mut target = host_named("target", &["prod"], &[]);
    target.proxy_jump = Some("jump1".to_string());
    let mut inline = host_named("inline", &["prod"], &[]);
    inline.proxy_jump = Some("me@10.0.0.9:2222".to_string());

    let fx = fixture_with_hosts(
        EchoShellFactory::default(),
        vec![host_named("jump1", &["net"], &[]), target, inline],
    )
    .await;

    // 1) 引用另一台主机
    let h = fx.store.find_host("target").unwrap();
    let jump = h.jump.as_ref().expect("应展开出跳板");
    assert_eq!(jump.name, "jump1");
    assert!(jump.jump.is_none(), "jump1 自己没有跳板");

    // 2) 内联写法
    let h2 = fx.store.find_host("inline").unwrap();
    let j2 = h2.jump.as_ref().expect("内联跳板应能解析");
    assert_eq!(j2.addr, "10.0.0.9");
    assert_eq!(j2.port, 2222);
    assert_eq!(j2.user, "me");

    // 3) 没配跳板的主机不该凭空多出 jump
    let plain = fx.store.find_host("jump1").unwrap();
    assert!(plain.jump.is_none());

    // 4) 跳板信息不落盘：hosts.toml 里只留 proxy_jump 这个名字
    let raw = std::fs::read_to_string(fx.store.paths().hosts_file()).unwrap();
    assert!(raw.contains("proxy_jump"), "配置里应保留 proxy_jump");
    assert!(!raw.contains("jump:"), "解析出的跳板不该写回文件");
}

/// 链式跳板（A → B → C）要逐级展开
#[tokio::test]
async fn proxy_jump_chain_expands_recursively() {
    let mut a = host_named("a", &[], &[]);
    a.proxy_jump = Some("b".to_string());
    let mut b = host_named("b", &[], &[]);
    b.proxy_jump = Some("c".to_string());

    let fx = fixture_with_hosts(EchoShellFactory::default(), vec![a, b, host_named("c", &[], &[])]).await;

    let ha = fx.store.find_host("a").unwrap();
    let jb = ha.jump.as_ref().expect("a → b");
    assert_eq!(jb.name, "b");
    let jc = jb.jump.as_ref().expect("b → c");
    assert_eq!(jc.name, "c");
    assert!(jc.jump.is_none(), "c 是链尾");
}

/// 配置成环（A → B → A）不能把进程栈打爆
#[tokio::test]
async fn proxy_jump_cycle_does_not_recurse_forever() {
    let mut a = host_named("a", &[], &[]);
    a.proxy_jump = Some("b".to_string());
    let mut b = host_named("b", &[], &[]);
    b.proxy_jump = Some("a".to_string());

    let fx = fixture_with_hosts(EchoShellFactory::default(), vec![a, b]).await;

    let ha = fx.store.find_host("a").unwrap();
    // 展开到上限就停：不 panic、不死循环即可
    let mut depth = 0;
    let mut cur = ha.jump.as_deref();
    while let Some(j) = cur {
        depth += 1;
        cur = j.jump.as_deref();
        assert!(depth <= 8, "跳板链不该无限展开");
    }
    assert!(depth >= 1, "至少要展开出第一层");
}

// ---------------------------------------------------------------------------
// M5 W5.3：配置导入（FR-1.10）
// ---------------------------------------------------------------------------

/// 导入 ssh config：dry-run 不落盘，正式导入才写 hosts.toml
#[tokio::test]
async fn host_import_from_ssh_config() {
    let fx = fixture().await;

    let dir = std::env::temp_dir().join(format!("sh0-import-{}", sh0_core::new_host_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = dir.join("ssh_config");
    std::fs::write(
        &cfg_path,
        "Host *\n    ServerAliveInterval 30\n\n\
         Host web-01\n    HostName 10.0.0.1\n    User root\n    Port 2222\n\n\
         Host db-01\n    HostName db.internal\n    User admin\n",
    )
    .unwrap();

    // dry-run：报出来但不写（fixture 自带一台 mockbox，故比较导入前后的数量）
    let before = fx.store.load_hosts().unwrap().hosts.len();
    let src = sh0_store::ImportSource::parse("ssh", Some(cfg_path.to_str().unwrap())).unwrap();
    let dry = fx.store.import_hosts(src.collect().unwrap(), true).unwrap();
    assert_eq!(dry.imported.len(), 2, "应识别出两台: {dry:?}");
    assert_eq!(
        fx.store.load_hosts().unwrap().hosts.len(),
        before,
        "dry-run 不该落盘"
    );

    // 正式导入
    let real = fx.store.import_hosts(src.collect().unwrap(), false).unwrap();
    assert_eq!(real.imported.len(), 2);
    let hosts = fx.store.load_hosts().unwrap().hosts;
    assert_eq!(hosts.len(), before + 2);
    let web = hosts.iter().find(|h| h.name == "web-01").unwrap();
    assert_eq!(web.addr, "10.0.0.1");
    assert_eq!(web.port, 2222);
    assert_eq!(web.group_path(), "imported");

    // 再导一次：重名不覆盖
    let again = fx.store.import_hosts(src.collect().unwrap(), false).unwrap();
    assert_eq!(again.imported.len(), 0);
    assert_eq!(again.duplicated.len(), 2, "重名应全部列为已存在");
    assert_eq!(
        fx.store.load_hosts().unwrap().hosts.len(),
        before + 2,
        "不该重复写入"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Xshell 会话文件（含非 SSH 协议的要跳过）
#[tokio::test]
async fn host_import_from_xsh() {
    let fx = fixture().await;
    let dir = std::env::temp_dir().join(format!("sh0-xsh-{}", sh0_core::new_host_id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("prod-web.xsh"),
        "[CONNECTION]\nPROTOCOL=SSH\nHOSTNAME=192.168.1.50\nPORT=2201\nUSERNAME=deploy\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("old-switch.xsh"),
        "[CONNECTION]\nPROTOCOL=TELNET\nHOSTNAME=10.9.9.9\n",
    )
    .unwrap();

    let src = sh0_store::ImportSource::parse("xsh", Some(dir.to_str().unwrap())).unwrap();
    let merged = fx.store.import_hosts(src.collect().unwrap(), false).unwrap();

    assert_eq!(merged.imported.len(), 1, "只有 SSH 会话该导入: {merged:?}");
    let h = &merged.imported[0];
    assert_eq!(h.name, "prod-web");
    assert_eq!(h.addr, "192.168.1.50");
    assert_eq!(h.port, 2201);
    assert_eq!(h.user, "deploy");
    assert!(merged.skipped.iter().any(|(n, _)| n == "old-switch"));

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// M6：TUI 凭据写入（credential.* IPC）
// ---------------------------------------------------------------------------

/// 密码经 IPC 写入本地 data-key vault：落盘为密文、应答不回显、可删可查状态
#[tokio::test]
async fn credential_set_status_delete_over_ipc() {
    // 强制走 vault，避免测试机钥匙串可用性影响断言
    unsafe { std::env::set_var("SH0_DISABLE_KEYRING", "1") };

    let dir = std::env::temp_dir().join(format!("sh0-cred-ipc-{}", sh0_core::new_host_id()));
    let paths = Paths {
        config_dir: dir.join("config"),
        data_dir: dir.join("data"),
        runtime_dir: dir.join("data").join("run"),
    };
    let store = Arc::new(Store::with_paths(paths.clone()).unwrap());
    store
        .save_hosts(&HostsFile {
            version: 1,
            hosts: vec![],
        })
        .unwrap();
    let creds = Arc::new(CredentialStore::open(&paths).unwrap());
    let cfg = AppConfig::default();

    let (addr, token, _stop) = daemon::run_for_test_with_creds(
        store,
        cfg,
        Arc::new(sh0_core::mock::MockFactory::repeating(vec![], 0)),
        Some(creds.clone()),
    )
    .await
    .expect("测试 daemon 启动失败");
    let conn = sh0_ipc::IpcClient::connect(&addr.to_string(), &token)
        .await
        .expect("IPC 连接失败");
    let client = conn.client;

    let r = client
        .call(
            methods::CREDENTIAL_SET,
            json!({ "service": "sh0", "account": "acc-1", "value": "s3cr3t" }),
        )
        .await
        .expect("credential.set 失败");
    assert_eq!(r["backend"], "vault");
    assert!(
        !r.to_string().contains("s3cr3t"),
        "IPC 应答不能回显明文: {r}"
    );
    assert_eq!(
        creds.get("sh0", "acc-1").unwrap().as_deref(),
        Some("s3cr3t")
    );

    // SSH 认证走的就是这条解析路径：SecretRef::Keyring -> VaultResolver -> 加密库
    let resolver = sh0::creds_bridge::VaultResolver {
        store: creds.clone(),
    };
    let resolved = sh0_core::SecretResolver::resolve(
        &resolver,
        &sh0_core::SecretRef::Keyring {
            service: "sh0".into(),
            account: "acc-1".into(),
        },
        "密码",
    )
    .unwrap();
    assert_eq!(resolved, "s3cr3t");

    let st = client
        .call(methods::CREDENTIAL_STATUS, json!({}))
        .await
        .expect("credential.status 失败");
    assert_eq!(st["available"], true);
    assert_eq!(st["vault_ready"], true);

    let vault = std::fs::read_to_string(paths.vault_file()).unwrap();
    assert!(!vault.contains("s3cr3t"), "vault 落盘必须是密文");

    let d = client
        .call(
            methods::CREDENTIAL_DELETE,
            json!({ "service": "sh0", "account": "acc-1" }),
        )
        .await
        .expect("credential.delete 失败");
    assert_eq!(d["removed"], true);
    assert_eq!(creds.get("sh0", "acc-1").unwrap(), None);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// M6：终端区边框与焦点标识
// ---------------------------------------------------------------------------

fn buffer_to_text(buf: &ratatui::buffer::Buffer) -> String {
    let area = *buf.area();
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

#[tokio::test]
async fn tui_terminal_area_has_border_and_focus_marker() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let fx = fixture_with_hosts(
        EchoShellFactory::default(),
        vec![host_named("h1", &[], &[])],
    )
    .await;
    let mut app = new_app(&fx);
    app.open("h1").await.unwrap();

    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    let text = buffer_to_text(term.backend().buffer());
    // 宽字符在 buffer 里占两格，第二格是空 symbol；比对前去掉空白
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        compact.contains("终端[焦点]"),
        "焦点在终端时标题应带焦点标识:\n{text}"
    );
    assert!(
        text.contains('┌') && text.contains('┐') && text.contains('└'),
        "终端区应有边框:\n{text}"
    );

    // Ctrl+B 把焦点交给主机树：终端标题不再带 [焦点]
    app.on_key(key(
        crossterm::event::KeyCode::Char('b'),
        crossterm::event::KeyModifiers::CONTROL,
    ))
    .await
    .unwrap();
    term.draw(|f| app.draw(f)).unwrap();
    let text = buffer_to_text(term.backend().buffer());
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(compact.contains("主机"), "树标题应在:\n{text}");
    assert!(
        !compact.contains("终端[焦点]"),
        "焦点移走后不应再显示 [焦点]:\n{text}"
    );
}

/// 删除主机时必须同时关掉它的连接，不能留下“主机没了、会话还在”的右栏标签
#[tokio::test]
async fn deleting_host_closes_its_tabs() {
    use crossterm::event::{KeyCode, KeyModifiers};

    let fx = fixture_with_hosts(
        EchoShellFactory::default(),
        vec![host_named("h1", &[], &[]), host_named("h2", &[], &[])],
    )
    .await;
    let mut app = new_app(&fx);
    app.open("h1").await.unwrap();
    app.open("h2").await.unwrap();
    assert_eq!(app.tab_count(), 2);

    // 焦点交给主机树（默认选中第一台 h1），d → y 删除
    app.on_key(key(KeyCode::Char('b'), KeyModifiers::CONTROL))
        .await
        .unwrap();
    app.on_key(key(KeyCode::Char('d'), KeyModifiers::NONE))
        .await
        .unwrap();
    app.on_key(key(KeyCode::Char('y'), KeyModifiers::NONE))
        .await
        .unwrap();

    assert_eq!(app.tab_count(), 1, "只该关掉 h1 的连接，h2 要保留");
    assert_eq!(app.tab_host_names(), vec!["h2"], "留下的必须是 h2");
    assert!(
        app.notice().contains("关闭 1 个连接"),
        "notice 应说明关了几个连接: {}",
        app.notice()
    );
    assert!(
        fx.store
            .load_hosts()
            .unwrap()
            .hosts
            .iter()
            .all(|h| h.name != "h1"),
        "h1 应从 hosts.toml 删除"
    );
}
