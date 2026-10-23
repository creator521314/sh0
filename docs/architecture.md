# sh0 —— SSH/SFTP 管理工具 架构设计

| 项目 | 内容 |
| --- | --- |
| 文档名称 | 架构设计（Architecture Design）+ AI 接口契约 |
| 版本 | v1.0（初稿） |
| 日期 | 2026-10-24 |
| 状态 | 待评审 |
| 上游文档 | `docs/requirements.md` |

---

## 1. 架构总览

### 1.1 核心决策：常驻 daemon + 多前端

TUI 与 AI CLI 必须看到**同一批会话**。若 AI 子命令每次都新建独立进程、独立连接，会导致：

- AI 与人类看到的会话互相隔离，AI 无法接管人正在操作的交互式会话；
- 每次调用都重新走完整 SSH 握手，慢且无法利用长连接；
- 审计日志分散，风险引擎难以统一限流。

因此采用**单 daemon 持有全部会话**的模型：

```
┌─────────────────────────────────────────────────────────────┐
│                     前端（Frontends）                        │
│  ┌──────────────┐   ┌──────────────┐   ┌────────────────┐  │
│  │ TUI (ratatui)│   │ AI CLI       │   │ (未来) MCP /   │  │
│  │ 常驻交互     │   │ 一次性调用   │   │ HTTP adapter   │  │
│  └──────┬───────┘   └──────┬───────┘   └───────┬────────┘  │
└─────────┼──────────────────┼───────────────────┼───────────┘
          │    IPC（本机）    │                   │
          └──────────────────┴───────────────────┘
                             │
┌────────────────────────────▼────────────────────────────────┐
│                     sh0 daemon（常驻）                       │
│  ┌───────────────────────────────────────────────────────┐  │
│  │ SessionManager（会话生命周期 / 状态机 / 重连）          │  │
│  ├───────────────────────────────────────────────────────┤  │
│  │ TermEmulator（xterm 网格 + 滚动缓冲，每个会话一份）    │  │
│  ├───────────────────────────────────────────────────────┤  │
│  │ SshPool（russh 连接 / channel / PTY / SFTP）           │  │
│  ├───────────────────────────────────────────────────────┤  │
│  │ RiskEngine（三档分级） · AuditSink · CredentialStore   │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
                             │
                    SSH over TCP (22)
```

**进程形态：**

| 命令 | 行为 |
| --- | --- |
| `sh0` | 若 daemon 未运行则自动拉起（fork/spawn 后台），随后以 TUI 前端接入 |
| `sh0 daemon run` | 显式前台运行 daemon（调试/系统服务场景） |
| `sh0 daemon stop` | 优雅关闭（先断开全部会话再退出） |
| `sh0 ai ...` | 连 daemon 执行一次性动作，返回后退出 |
| `sh0 host/config/log ...` | 管理类子命令，可直接读配置库，无需 daemon |

### 1.2 数据流

**远端 → 本地（渲染/读屏）**

```
SSH channel data ──▶ vte::Parser ──▶ Screen(Grid + Scrollback) ──▶ TUI 渲染
                                          │
                                          └──▶ screen read (AI) ──▶ JSON
```

**本地 → 远端（输入）**

```
键盘事件 / ai input send ──▶ 键位映射 ──▶ 编码为字节序列 ──▶ SSH channel stdin
```

**命令执行（AI）**

```
ai exec run ──▶ RiskEngine 判定 ──▶ [deny] 拒绝并审计
                                 ──▶ [confirm] 无令牌 → 返回 needs_confirmation
                                 ──▶ [auto/已确认] 在会话上写入 cmd + 哨兵回显标记
                                         │
                                    等待哨兵或超时
                                         │
                                    截取输出 → 脱敏 → 审计 → 返回 JSON
```

---

## 2. Crate 划分

采用 Cargo workspace，按"core 不依赖任何具体前端/传输"的原则分层：

```
sh0/
├── Cargo.toml                # workspace
├── crates/
│   ├── sh0-core/             # 领域模型 + 业务编排（无 UI、无网络细节）
│   │   ├── model.rs          # Host / Credential / SessionId / RiskLevel
│   │   ├── session.rs        # SessionManager、状态机、重连策略
│   │   ├── risk.rs           # 风险分级引擎与规则
│   │   ├── audit.rs          # 审计事件定义与 sink 抽象
│   │   └── batch.rs          # 广播与批量执行编排
│   ├── sh0-ssh/              # russh 封装（唯一接触 SSH 协议的地方）
│   │   ├── client.rs         # 连接、认证（密码/私钥/agent/KBI/2FA）
│   │   ├── channel.rs        # PTY / shell / exec channel
│   │   ├── sftp.rs           # SFTP 子系统
│   │   └── host_key.rs       # known_hosts 校验与 TOFU
│   ├── sh0-term/             # 终端仿真
│   │   ├── parser.rs         # vte 解析器适配
│   │   ├── grid.rs           # 字符网格 + 属性（颜色/粗体/下划线）
│   │   ├── scrollback.rs     # 环形缓冲 + 配额
│   │   └── keys.rs           # 按键 → 字节序列编码
│   ├── sh0-store/            # 持久化
│   │   ├── config.rs         # TOML 加载/保存/迁移/热重载
│   │   ├── creds.rs          # 钥匙串 + 加密文件双后端
│   │   └── db.rs             # SQLite（审计、历史、批量结果）
│   ├── sh0-ipc/              # 进程间通信
│   │   ├── transport.rs      # UDS（Unix）/ Named Pipe（Windows）统一抽象
│   │   └── protocol.rs       # 请求/响应/事件帧定义
│   ├── sh0-ai/               # AI 子命令层
│   │   ├── cli.rs            # clap 定义：sh0 ai <object> <verb>
│   │   ├── render.rs         # 纯文本 / JSON 双渲染
│   │   └── error.rs          # 退出码映射
│   ├── sh0-tui/              # ratatui 前端
│   │   ├── app.rs            # 主事件循环与状态分发
│   │   ├── widgets/          # host_tree / tabs / terminal_view / status_bar
│   │   ├── views/            # sftp_pane / batch_panel / confirm_dialog
│   │   └── theme.rs
│   └── sh0-cli/              # 顶层二进制入口（命令路由）
└── docs/
```

**依赖方向（严格单向）：**

```
sh0-cli ──▶ sh0-tui ──▶ sh0-core
   │           │
   └──▶ sh0-ai ┘──▶ sh0-core ──▶ sh0-store
                                 sh0-ssh
                                 sh0-term
                                 sh0-ipc
```

`sh0-core` 通过 trait 依赖 `sh0-ssh` / `sh0-term`（`SshTransport`、`Screen`），便于测试时注入 mock。

---

## 3. 运行时模型

### 3.1 并发模型

- 异步运行时：**tokio**（multi-thread），SSH I/O 全异步。
- **一会话一 actor**：每个 `Session` 是一个独立 tokio task，拥有自己的 channel 与 Screen，通过 `mpsc` 收发命令。单会话 panic 由 supervisor 捕获并标记为 `Closed`，不影响其他会话。
- UI 线程与 I/O 分离：TUI 渲染跑在主线程固定 tick（默认 60fps，空闲降频），通过 `try_recv` 拉取状态快照，避免 I/O 阻塞界面。
- 背压：远端输出洪峰时，Screen 更新合并为批量帧（每 16ms flush 一次），防止 TUI 被刷爆。

### 3.2 会话状态机

```
                 ┌──────────────────┐
                 │      Idle        │
                 └────────┬─────────┘
                          │ open()
                          ▼
        ┌─────────▶ Connecting ────失败───▶ Closed(error)
        │                │
        │                ▼
        │        Authenticating ────失败───▶ Closed(auth_error)
        │                │
        │                ▼
        │            Ready ◀──────────────┐
        │                │                │
   重连 │          连接中断                │ 重连成功
        │                ▼                │
        └────────  Reconnecting ──────────┘
                          │ 次数耗尽
                          ▼
                      Closed
```

状态变更以事件形式广播给所有前端（TUI 刷新状态栏、CLI 可轮询）。

### 3.3 重连策略

- 触发：TCP 断开、`channel eof` 且非用户主动关闭、keepalive 超时。
- 退避：`min(30s, 1s * 2^n)`，n 上限 5，可配；最多重试次数默认 5。
- **重连成功保留滚动缓冲**，在缓冲末尾插入一条系统行：`── 连接中断，已于 HH:MM:SS 自动重连 ──`。
- 重连期间收到的 AI `exec` 请求：返回 `session_unavailable` 错误码，不排队（避免命令在恢复后延迟执行造成语义错乱）。

---

## 4. 关键数据结构

```rust
// sh0-core
pub struct HostId(pub Ulid);

pub struct Host {
    pub id: HostId,
    pub name: String,
    pub group: Vec<String>,          // 面包屑路径，如 ["prod", "web"]
    pub addr: String,
    pub port: u16,                   // 默认 22
    pub user: String,
    pub auth: CredentialRef,
    pub tags: Vec<String>,
    pub jump: Option<HostId>,        // 跳板机（P1）
    pub env: HashMap<String, String>,
    pub auto_connect: bool,
    pub risk_cap: RiskLevel,         // 该主机允许的最高风险档
}

pub enum CredentialRef {
    Agent,
    KeyFile { path: PathBuf, passphrase_ref: Option<SecretRef> },
    Password { secret: SecretRef },
    KeyboardInteractive { responder: ResponderKind },   // 含 2FA/OTP
    None,
}

pub enum RiskLevel { Auto, Confirm, Deny }

pub struct SessionId(pub Ulid);

pub struct SessionSnapshot {
    pub id: SessionId,
    pub host_id: HostId,
    pub state: SessionState,
    pub cols: u16,
    pub rows: u16,
    pub scrollback_lines: usize,
    pub last_rtt_ms: Option<u32>,
    pub reconnect_count: u32,
}

pub struct AuditEvent {
    pub ts: DateTime<Utc>,
    pub session_id: Option<SessionId>,
    pub host_id: Option<HostId>,
    pub actor: Actor,                // User | Ai { trace_id }
    pub action: ActionKind,          // Exec | Input | FileOp | SessionOp | ...
    pub raw: String,                 // 脱敏后的命令/路径
    pub risk: RiskLevel,
    pub decision: Decision,          // Allowed | Confirmed | Denied | RateLimited
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
}
```

---

## 5. 模块设计要点

### 5.1 `sh0-ssh`（russh 封装）

- 连接：`russh::client::connect` + `handle.authenticate_*`，按 `CredentialRef` 分派认证方式；KBI/2FA 通过回调向上抛 `AuthPrompt` 事件，由 TUI 弹输入框或 CLI 返回 `auth_required`。
- 算法放宽开关：`config.preferred.algorithms` 允许显式启用旧算法，连接失败时错误信息中给出"是否启用兼容模式"提示。
- PTY：`channel.request_pty(term, cols, rows)` 后 `request_shell()`；resize 走 `channel.window_change()`。
- SFTP：`channel.request_subsystem("sftp")` 复用同一 TCP 连接，用 `russh_sftp` 或自实现的最小 SFTP 客户端（首版接口先行，实现可延后）。
- Trait 抽象：

```rust
#[async_trait]
pub trait SshTransport: Send + Sync {
    async fn connect(&self, host: &Host, cred: &ResolvedCredential) -> Result<Box<dyn SshSession>>;
}

#[async_trait]
pub trait SshSession: Send + Sync {
    async fn open_shell(&self, cols: u16, rows: u16) -> Result<ShellHandle>;
    async fn exec(&self, cmd: &str) -> Result<ExecHandle>;
    async fn sftp(&self) -> Result<Box<dyn SftpClient>>;
    async fn close(&self) -> Result<()>;
}
```

### 5.2 `sh0-term`（终端仿真）

- 解析器：优先 `vte`（Alacritty 同款，纯 Rust，无 unsafe 依赖）；备选 `wezterm-term`（功能更全但依赖较重）。
- Grid：`Vec<Cell>` 行式存储，每 Cell 含 `char / fg / bg / attrs`；配合宽字符标记（CJK 双宽占位）。
- Scrollback：环形缓冲，默认 10000 行，超出按配额淘汰；全局总内存预算可配（防止 100 会话吃爆内存）。
- 屏幕查询接口（供 AI 读屏）：

```rust
pub trait Screen: Send {
    fn size(&self) -> (u16, u16);
    fn rows(&self, range: Range<i64>) -> Vec<String>;   // 负数索引取滚动缓冲
    fn cursor(&self) -> (u16, u16);
    fn text_plain(&self, with_scrollback: bool) -> String;
}
```

- 按键编码：`keys.rs` 维护键名 → 字节序列表（`Enter`、`Tab`、`Esc`、`F1..F12`、`Ctrl-C`、`Up/Down`…），AI 的 `input send --keys` 复用同一张表，保证与人按键完全一致。

### 5.3 `sh0-tui`（ratatui）

布局（`Layout` 约束）：

```
┌────────────────────────────────────────────────────────┐
│ [web-01] web-02 │ web-03 +          │  ← 标签栏（3行?不，1行）
├────────────────┼───────────────────────────────────────┤
│ ▾ prod         │                                       │
│   web-01 ●     │   $ systemctl status nginx            │
│   web-02 ○     │   ● nginx.service - A high perf...    │
│ ▾ staging      │   ...                                 │
│   stg-01 ○     │                                       │
│ 搜索: __       │                                       │
├────────────────┴───────────────────────────────────────┤
│ web-01 · Ready · 32ms · auto档 · AI:2       Ctrl+? 帮助 │
└────────────────────────────────────────────────────────┘
```

- 主机树面板默认宽度 24 列，`Ctrl+B` 折叠。
- 终端视图为自绘 widget：直接把 `Screen` 网格按行渲染为 ratatui `Span`，保留颜色属性。
- SFTP 视图（P1）与终端视图共用主区，用 `F9` 切换。
- 确认对话框：`confirm` 档命令触发时阻塞当前会话输入并置顶显示（显示完整命令、风险理由、来源 actor）。

### 5.4 `sh0-ai`（CLI 契约层）

用 `clap` derive 定义，全局选项：

```
--json                结构化输出（AI 必带）
--timeout <secs>      超时，默认 30
--trace-id <id>       调用链追踪，写入审计
--yes / -y            跳过确认（仅对 risk_cap <= Confirm 且策略允许时生效）
--quiet               静默，仅输出退出码
--dry-run             只做风险判定，不执行
```

### 5.5 `sh0-ipc`

```rust
pub enum Request {
    SessionOpen { host: HostSelector, cols: u16, rows: u16 },
    SessionClose { session: SessionId },
    SessionList,
    Exec { session: Option<SessionId>, host: Option<HostSelector>, cmd: String, timeout: Duration },
    ScreenRead { session: SessionId, lines: Option<usize>, mode: ScreenMode },
    InputSend { session: SessionId, keys: Option<String>, text: Option<String>, raw_hex: Option<String> },
    FileOp { op: FileOpKind, .. },
    Batch { hosts: Vec<HostSelector>, cmd: String, concurrency: usize },
    Broadcast { sessions: Vec<SessionId>, text: String },
    LogQuery { filter: LogFilter },
    Confirm { token: String },
}

pub enum Response {
    Ok(serde_json::Value),
    Err { code: ErrorCode, message: String, detail: Option<serde_json::Value> },
}

pub enum Event {   // daemon → 前端的推送
    ScreenUpdated { session: SessionId, seq: u64 },
    StateChanged { session: SessionId, state: SessionState },
    ConfirmRequest { token: String, session: SessionId, cmd: String, reason: String },
    TransferProgress { id: u64, done: u64, total: u64 },
}
```

**M0 落地的传输层**：loopback TCP（绑 `127.0.0.1`，端口由系统分配）+ 首帧一次性令牌握手。
线路格式为 JSON Lines（每帧一行 JSON，以 `\n` 结尾）。

选型理由与后续演进：

| 方案 | 状态 | 说明 |
| --- | --- | --- |
| loopback TCP + 令牌 | **M0 采用** | 三平台行为一致、tokio 原生支持、实现成本最低。只绑 127.0.0.1，不对外可达；令牌存 `runtime_dir/daemon.json` |
| Unix UDS | M2 评估 | 可用 0600 文件权限做系统级保护，Unix 上更安全；用 `tokio::net::UnixListener` 替换即可 |
| Windows Named Pipe | M2 评估 | 需配 DACL 限当前用户；用 `interprocess` 或 `tokio::net::windows::named_pipe` |

`IpcClient::connect` / `serve` 是唯一接触网络细节的函数，换后端时协议层（`protocol.rs`）
与上层代码完全不动。

---

## 6. AI 子命令契约

### 6.1 命令总表

| 命令 | 说明 | 优先级 |
| --- | --- | --- |
| `sh0 ai host list [--group G] [--tag T]` | 列出主机（不含凭据） | P0 |
| `sh0 ai host show <selector>` | 单台主机详情 | P0 |
| `sh0 ai session list` | 列出活跃会话 | P0 |
| `sh0 ai session open --host <sel> [--cols N] [--rows N]` | 打开会话，返回 session_id | P0 |
| `sh0 ai session close --session <id>\|--all` | 关闭会话 | P0 |
| `sh0 ai session status --session <id>` | 会话状态快照 | P0 |
| `sh0 ai session reconnect --session <id>` | 手动重连 | P0 |
| `sh0 ai exec run (--session <id> \| --host <sel>) --cmd <cmd> [--timeout N]` | 执行命令并取回输出 | P0 |
| `sh0 ai screen read --session <id> [--lines N] [--mode plain\|grid]` | 读取屏幕/滚动缓冲 | P0 |
| `sh0 ai input send --session <id> (--text T \| --keys K \| --hex H)` | 注入按键 | P0 |
| `sh0 ai log query [--host X] [--session S] [--since TS] [--actor ai] [--risk deny] [--limit N]` | 审计查询 | P0 |
| `sh0 ai file list --session <id> --path P` | 列远程目录 | P1 |
| `sh0 ai file upload --session <id> --local L --remote R` | 上传 | P1 |
| `sh0 ai file download --session <id> --remote R --local L` | 下载 | P1 |
| `sh0 ai file read --session <id> --path P [--max-bytes N]` | 读远程文件 | P1 |
| `sh0 ai file write --session <id> --path P --content-stdin` | 写远程文件 | P1 |
| `sh0 ai batch exec --hosts a,b,c --cmd <cmd> [--concurrency N]` | 批量执行并汇总 | P1 |
| `sh0 ai broadcast send --sessions a,b --text T` | 广播输入 | P1 |

### 6.2 输出规范

**纯文本（默认）** —— 给人看，简洁直观：

```
$ sh0 ai exec run --host web-01 --cmd "df -h /"
web-01 (172.20.0.11) · exit 0 · 0.42s
Filesystem      Size  Used Avail Use% Mounted on
/dev/sda1        40G   18G   22G  46% /
```

**JSON（`--json`）** —— 给 AI 看，统一信封：

成功：

```json
{
  "ok": true,
  "trace_id": "ai-7f3c",
  "data": {
    "session_id": "01J...",
    "host": "web-01",
    "exit_code": 0,
    "stdout": "Filesystem ...\n",
    "stderr": "",
    "duration_ms": 421,
    "truncated": false
  }
}
```

失败：

```json
{
  "ok": false,
  "trace_id": "ai-7f3c",
  "error": {
    "code": "risk_denied",
    "message": "命令命中禁止规则: recursive delete on /",
    "detail": { "rule": "deny-rm-root", "risk": "deny" }
  }
}
```

需确认：

```json
{
  "ok": false,
  "trace_id": "ai-7f3c",
  "error": {
    "code": "needs_confirmation",
    "message": "命令需确认后执行",
    "detail": {
      "token": "cf_01J...",
      "expires_in_sec": 300,
      "risk": "confirm",
      "command": "systemctl restart nginx",
      "retry": "sh0 ai exec run --session S --cmd '...' --confirm-token cf_01J..."
    }
  }
}
```

`screen read --mode grid` 返回二维数组：

```json
{ "ok": true, "data": { "cols": 120, "rows": 40, "cursor": [0, 12],
  "lines": ["$ vim nginx.conf", "  ...", ""] } }
```

### 6.3 退出码

| 退出码 | code 字符串 | 含义 | AI 应对 |
| --- | --- | --- | --- |
| 0 | `ok` | 成功 | — |
| 1 | `remote_nonzero` | 远端命令退出码非零（stdout/stderr 仍在 data 中） | 读输出自行判断 |
| 2 | `risk_denied` | 命中禁止规则 | 换方案，勿重试 |
| 3 | `needs_confirmation` | 需确认，带 token | 向用户确认后用 token 重试 |
| 4 | `timeout` | 执行超时 | 减小范围或加超时重试 |
| 5 | `session_unavailable` | 会话不存在/重连中/已关闭 | 重新 open 或等待 |
| 6 | `auth_failed` | 认证失败 | 检查凭据，勿盲目重试 |
| 7 | `auth_required` | 需要交互式应答（KBI/2FA） | 无法自动完成，转人工 |
| 8 | `host_not_found` | 主机选择器无匹配 | 先 `host list` |
| 9 | `rate_limited` | 触发速率限制 | 退避后重试 |
| 10 | `ipc_unreachable` | daemon 未运行或连不上 | 启动 daemon |
| 11 | `invalid_args` | 参数错误 | 修正参数 |
| 12 | `internal` | 内部错误（附 trace_id） | 上报 |

### 6.4 AI 使用的推荐范式

```bash
# 1. 先看有哪些机器
sh0 ai host list --json

# 2. 一次性调用（无需手动开关会话）
sh0 ai exec run --host web-01 --cmd "uname -a" --timeout 10 --json

# 3. 需要连续交互时，显式开会话复用连接
SID=$(sh0 ai session open --host web-01 --json | jq -r .data.session_id)
sh0 ai exec run  --session "$SID" --cmd "sudo tail -n 50 /var/log/nginx/error.log" --json
sh0 ai screen read --session "$SID" --lines 80 --json

# 4. 卡在交互式程序里：读屏 → 注入按键
sh0 ai screen read --session "$SID" --json
sh0 ai input send --session "$SID" --keys ":q!<Enter>" --json

# 5. 危险操作会返回 needs_confirmation，带 token 二次确认
sh0 ai exec run --session "$SID" --cmd "systemctl restart nginx" --json
sh0 ai exec run --session "$SID" --cmd "systemctl restart nginx" --confirm-token "$TOKEN" --json

# 6. 批量巡检
sh0 ai batch exec --hosts "prod/*" --cmd "df -h /" --concurrency 16 --json

# 7. 事后复盘
sh0 ai log query --actor ai --risk confirm --since "2026-10-20" --json
```

---

## 7. 风险分级引擎

### 7.1 判定流程

```
原始命令字符串
   │
   ├─(1) 规范化：去除多余空白、展开别名、按 shell 词法切分（保留管道/重定向分段）
   │
   ├─(2) 逐"段"判定：cmd | cmd | cmd  以及  ; && || 分隔的每段都独立判定
   │
   ├─(3) 匹配顺序：deny 规则 → 用户自定义规则 → confirm 规则 → 内置 auto 白名单
   │
   ├─(4) 取所有段中"最高"风险作为整条命令的风险
   │
   └─(5) 与主机 risk_cap 比较：若命令风险 > risk_cap，按 risk_cap 处理
```

### 7.2 内置规则（初始集）

| 档位 | 规则示例 |
| --- | --- |
| `deny` | `rm -rf /`、`rm -rf /*`、`mkfs*`、`dd of=/dev/*`、`shutdown`、`reboot`、`init 0`、fork 炸弹 `:(){...}`、`curl|sh`、`wget -O- |sh`、写入 `/etc/ssh/sshd_config`、`~/.ssh/authorized_keys`、`iptables -F`、`:> /dev/sda` |
| `confirm` | `rm`（非根路径）、`mv/ cp` 覆盖、`chmod/chown` 递归、`systemctl *stop/restart/*`、`docker rm/rmi`、`apt/yum/dnf install|remove`、`kill/killall`、`git reset --hard`、`git push --force`、写 `/etc/` 下任意文件、`reboot` 类服务重启 |
| `auto` | `ls`、`cat`、`head`、`tail`、`less`、`pwd`、`whoami`、`uname`、`ps`、`top -b`、`df`、`du`、`free`、`uptime`、`hostname`、`ip/ss/netstat`（只读）、`grep`、`find`（不接 `-delete/-exec`）、`git status/log/diff/show`、`journalctl`、`systemctl status`、`docker ps/logs`、`curl -sI`、`echo`、`date`、`env` |

规则文件示例（`risk.toml`）：

```toml
[[rule]]
level = "confirm"
match = { prog = "rm", args_any = ["-r", "-rf", "--recursive"] }
reason = "递归删除"

[[rule]]
level = "deny"
match = { regex = "^\\s*rm\\s+(-[a-zA-Z]*r[a-zA-Z]*\\s+)+\\s*/\\s*$" }
reason = "删除根目录"

[[host_cap]]
group = "prod"
max_level = "confirm"      # 生产环境不自动放行 confirm
```

### 7.3 速率限制

- 默认：单 actor 每分钟 60 条命令、每分钟 20 次文件上传。
- 超限返回 `rate_limited`，`detail.retry_after_sec` 给出建议等待时间。
- 批量执行按整体计为 1 次配额（避免 30 台机器瞬间打满）。

---

## 8. 持久化设计

### 8.1 文件布局

```
<config_dir>/sh0/
├── config.toml          # 全局偏好：主题、键位、缓冲、并发、超时
├── hosts.toml           # 主机与分组（无明文密钥）
├── risk.toml            # 风险规则
├── credentials.enc      # 主密码加密的凭据（钥匙串不可用时的回落）
├── known_hosts          # OpenSSH 兼容格式
├── sh0.db               # SQLite：审计、历史输出、批量结果
└── sh0.sock / pipe      # 运行时 IPC 端点 + token
```

### 8.2 SQLite 表（摘要）

```sql
CREATE TABLE audit (
  id INTEGER PRIMARY KEY, ts INTEGER NOT NULL,
  session_id TEXT, host_id TEXT, actor TEXT NOT NULL,
  action TEXT NOT NULL, raw TEXT, risk TEXT, decision TEXT,
  exit_code INTEGER, duration_ms INTEGER, trace_id TEXT
);
CREATE INDEX idx_audit_ts ON audit(ts);
CREATE INDEX idx_audit_host ON audit(host_id, ts);

CREATE TABLE output_archive (
  id INTEGER PRIMARY KEY, audit_id INTEGER REFERENCES audit(id),
  seq INTEGER, stream TEXT, chunk BLOB
);

CREATE TABLE batch_run (
  id INTEGER PRIMARY KEY, ts INTEGER, cmd TEXT,
  host_count INTEGER, ok_count INTEGER, fail_count INTEGER
);
```

保留策略：默认 30 天 / 1 GB，超出按时间淘汰（可配）。

### 8.3 凭据

- 优先 OS 钥匙串：`keyring` crate（macOS Keychain / Windows Credential Manager / Linux Secret Service）。
- 回落：主密码 → Argon2id 派生密钥 → ChaCha20-Poly1305 加密 `credentials.enc`；主密码只在本次进程内存中的 `secrecy::SecretBox` 保存，可设置会话级缓存时长。
- 私钥：只存文件路径，不复制内容；私钥自身的 passphrase 走上述加密存储。

---

## 9. 技术选型

| 领域 | 选型 | 备选 | 理由 |
| --- | --- | --- | --- |
| 异步运行时 | `tokio` | `smol` | 生态最好，russh 原生基于 tokio |
| SSH | `russh` 0.64 | `ssh2` / `thrussh` | 异步原生、API 完整。⚠️ 见下方「crypto backend」备注 |
| SFTP | `russh-sftp` | 自实现最小客户端 | 与 russh 同源 |
| TUI | `ratatui` + `crossterm` | `cursive` | 生态活跃、跨平台含 Windows、组件可控 |
| 终端解析 | `vte` | `wezterm-term` | 纯 Rust、Alacritty 同款、体积可控 |
| CLI | `clap` (derive) | `bpaf` | 自动生成帮助，AI 可读性重要 |
| 序列化 | `serde` + `serde_json` / `toml` | — | 配置与 IPC 统一 |
| 数据库 | `rusqlite` (bundled) | `sqlx` | 单文件、可 bundled 静态编译 |
| 加密 | `argon2` + `chacha20poly1305` + `secrecy` | `ring` | 纯 Rust，无 OpenSSL |
| 钥匙串 | `keyring` | — | 三平台统一 |
| 错误 | `thiserror` + `anyhow` | — | 库用 thiserror，应用用 anyhow |
| 日志 | `tracing` | `log` | 结构化，便于 NFR-8 |
| ID | `ulid` | `uuid` | 时间有序，便于日志排序 |
| IPC | loopback TCP + 令牌（M0） | UDS / Named Pipe（M2 评估） | 默认不对外监听（NFR-7），见 §5.5 |

### 9.1 关于 russh 的 crypto backend（与 NFR-11 的偏差）

NFR-11 要求「纯 Rust 依赖栈优先」。**实测 russh 0.64 无法完全满足**：它强制要求启用
`ring` 或 `aws-lc-rs` 之一作为 crypto backend，否则直接 `compile_error!`。两者都是 C 库。

实际取舍：

| backend | 构建要求 | 结论 |
| --- | --- | --- |
| `ring` | 只要 C 编译器（`cc` crate 能自动探测 MSVC / gcc） | **采用** |
| `aws-lc-rs` | 额外需要 `cmake`，特定平台还要 `nasm` / Go | 弃用（本机无 cmake） |

代价与影响：

- 引入 C 依赖，**偏离** NFR-11 的「纯 Rust 优先」（该条目本身是「优先」而非强制）。
- 但**不违背** NFR-1：ring 静态链接进二进制，运行时不依赖系统 OpenSSL/libssh2，
  「单文件丢到任意机器就能跑」的目标仍然成立。
- 代价是 Windows 构建需要可用的 MSVC 工具链（已验证本机 VS 2022 可正常工作）。

若将来 russh 提供纯 Rust crypto 路径，可去掉 `ring` feature 回归纯 Rust 依赖栈。
工作区 `Cargo.toml` 中已就地注明该取舍。

---

## 10. 测试策略

| 层级 | 手段 |
| --- | --- |
| 单元 | 风险引擎规则集（表驱动，覆盖全部内置规则）、按键编码表、Grid 属性合并 |
| 终端仿真 | golden-file：喂入录制的真实序列（`vim`/`htop`/`tmux` 输出），比对渲染后的文本网格 |
| SSH 层 | trait mock（`MockTransport`）；可选：起本地 sshd 容器做集成测试 |
| IPC | 契约测试：请求/响应 JSON schema 快照 |
| AI CLI | 端到端：脚本化调用全部子命令，校验退出码与 JSON 字段；`--dry-run` 验证风险判定 |
| TUI | 快照测试（`ratatui::backend::TestBackend`）验证布局与关键交互 |
| 跨平台 | CI matrix：Linux(gnu/musl) / macOS(aarch64/x86_64) / Windows，产物体积与冒烟检查 |

---

## 11. 风险与对策

| 风险 | 影响 | 对策 |
| --- | --- | --- |
| `russh` 对老旧设备算法支持不足 | 连不上部分老交换机/嵌入式设备 | 提供"兼容模式"开关；连接失败时错误提示指明缺失算法；极端情况允许回落到调用系统 `ssh` |
| 终端仿真还原度不足导致 AI 读屏失真 | AI 误判界面状态 | 优先 `plain` 模式（带滚动缓冲的纯文本）作为 AI 主要读取方式，`grid` 模式作为补充；积累 golden 用例 |
| IPC 在 Windows 上的权限与安全 | 越权访问会话 | Named Pipe DACL 限当前用户 + 一次性 token；默认不启用网络监听 |
| 大量会话内存膨胀 | OOM | 滚动缓冲全局预算 + 按会话配额；超限淘汰最旧行并标记 |
| AI 失控执行危险命令 | 生产事故 | 三档引擎 + 速率限制 + 主机 risk_cap + 全量审计；`deny` 硬拦截 |
| 凭据泄露 | 安全事故 | 无明文落盘、日志脱敏、钥匙串优先 |
| 二进制体积超标 | 分发困难 | release profile 优化；按需 feature 裁剪（如 `sftp` feature 可选） |

---

## 12. 演进路线

1. **M2 之后**：在 `sh0-ai` 之上加 MCP（JSON-RPC over stdio）adapter——core 与契约不变，只新增传输层，成本极低。
2. **M4 之后**：SFTP 能力完整化，接入断点续传与目录同步。
3. **远期**：脚本扩展（Rhai/Lua）用于自动化巡检；插件式文件预览；会话录制与回放（asciinema 格式）。

---

## 附录 A：与需求文档的追踪关系

| 架构组件 | 覆盖需求 |
| --- | --- |
| daemon + IPC（§1、§5.5） | FR-6.1、FR-6.3、NFR-7 |
| `sh0-term` Screen（§5.2） | FR-2.2、FR-2.3、FR-6.5 |
| 重连策略（§3.3） | FR-2.5 |
| `sh0-ai` CLI（§6） | FR-6.1~FR-6.11 |
| 风险引擎（§7） | FR-7.1~FR-7.7 |
| `sh0-store`（§8） | FR-8、FR-9、FR-1.6 |
| 批量编排（`sh0-core/batch`） | FR-5.1~FR-5.5 |
| `sh0-ssh` trait 化（§5.1） | NFR-9、NFR-10、约束 4 |
