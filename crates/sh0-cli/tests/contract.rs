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
