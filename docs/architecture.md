# sh0 —— SSH/SFTP 管理工具 架构设计

| 项目 | 内容 |
| --- | --- |
| 文档名称 | 架构设计（Architecture Design）+ AI 接口契约 |
| 版本 | v1.3（M5 打磨层已落地） |
| 日期 | 2026-10-26 |
| 状态 | 已实现至 M5（命令面板 / 主题 / 热重载 / 导入 / ProxyJump / 日志轮转，见 §5.7）；W5.7 端口转发与 W5.8 MCP adapter 两项 P2 未做 |
| 上游文档 | `docs/requirements.md` |
| 配套文档 | `docs/usage.md`（使用说明：怎么用）、`docs/development-plan.md`（里程碑进度） |

> 本文描述**设计与契约**（为什么这么设计、字段与错误码是什么）；
> 具体怎么用（命令参数、键位、配置字段、排查）见 `docs/usage.md`。

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

- 主机树面板默认宽度 28 列，`Ctrl+B` 折叠。
- 终端视图为自绘 widget：直接把 `Screen` 网格按行渲染为 ratatui `Span`，保留颜色属性。
- 确认对话框：`confirm` 档命令触发时阻塞当前会话输入并置顶显示（显示完整命令、风险理由、来源 actor）。

#### 5.3.1 双栏文件视图（M4 / FR-4.2）

与终端视图共用主区，`F9` 切换（三套键位预设里都绑了这个动作）：

```
┌─── 本地 D:\work ─────────────┬─── web-01:/var/log ──────────┐
│ nginx/           <DIR>  …    │ app/            <DIR>  …     │
│ a.txt            2.0K  …     │ access.log       12M   …     │
│ b.log            240K  …     │ error.log       1.2M   …     │
│ 3 项 · 排序 名称             │ 2 项 · 排序 名称             │
└──────────────────────────────┴──────────────────────────────┘
```

- 状态放在 `sh0-tui/src/files.rs`（`FilePane`），绘制在 `app.rs`：导航与排序因此能脱离终端做单测。
- 两侧共用 `sh0_core::sftp::FileEntry`：左栏由 `std::fs` 就地构造，右栏经 IPC `file.list` 拿到，
  于是渲染、排序（`SortKey`）、体积/时间格式化只写一遍。
- 右栏绑定当前标签页的会话，**换标签页自动重新列目录**；列目录走会话已有的 SSH 连接，不新开 TCP。
- `file.list` 在 daemon 侧先 `canonicalize` 再列：UI 第一次给的 `.` 会被换成可信绝对路径，
  否则 `./foo` 会一层层叠下去，审计里的路径也跟着糊掉。
- 键位（焦点在文件面板时）：`↑↓`/`jk` 选择，`Enter`/`→` 进入，`Backspace`/`←` 上级，
  `Tab` 切栏，`g` 路径跳转，`s` 切排序，`r` 刷新，`Esc` 回终端视图。

#### 5.3.2 文件操作（M4 / FR-4.3）

| 键 | 动作 | 走的路径 |
| --- | --- | --- |
| `F5` | 把选中文件传到对面栏（上传/下载） | 上传：`std::fs::read` → `file.write`；下载：`file.read` → `std::fs::write` |
| `F6` | 重命名 | 本地 `std::fs::rename` / 远端 `file.rename` |
| `F7` | 新建目录 | 本地 `create_dir` / 远端 `file.mkdir` |
| `F8` / `Delete` | 删除（先确认） | 本地 `remove_file`/`remove_dir` / 远端 `file.remove` |
| `p` | 改权限（输入八进制） | 本地 `set_permissions` / 远端 `file.chmod` |

- 传输是**整体读写**，单次上限 8 MB：UI 同步等 IPC 返回，更大的文件要等 W4.4 的传输队列
  （带进度、失败重试）。目录传输同样留给 W4.4——它必须递归，而递归就该有进度。
- 二进制走 `file.write` 的 `encoding=base64`：JSON 里塞不下裸字节，文本则原样传，省一层编解码。
- 删除一律先弹确认；远端删目录要求目录为空（非空递归删除也在 W4.4）。

#### 5.3.3 标记多选与传输队列（M4 / FR-4.4）

- `Space` 标记/取消当前项（渲染为 `*` 前缀），`F5` 把**全部标记项**排进队列；
  无标记时退化为传当前项。传完即清空标记，避免按两次 F5 传两遍。
- 队列是 `Vec<Transfer>`，**由事件循环每帧推进一块**（`tick_transfers`）：
  每帧只做 256 KB，所以再大的文件界面也不卡，进度条是真的会动。
- 分块靠 `file.read_range` / `file.write_at` 两个 IPC 方法（底层是 SFTP 的
  `open + seek + read/write`）；审计只在**首块**落一条，否则一个 100 MB 的文件
  能往审计表里灌四百条"读了同一个文件"。
- 单块失败自动重试，最多 3 次后标 `Failed`；`R` 重新排队，`c` 清理已完成项。
- 面板显示：`✓/↑/…/✗ 文件名 → ████░░░░░░ 42% 4.0M 1.2M/s` + 失败原因。

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

### 5.6 批量与广播（M3 / FR-5.x）

从"单台精细操作"扩展到"一组机器统一动作"，两组 IPC 方法：`broadcast.*`、`batch.*`。

**广播（FR-5.1）**

- 会话名单存在 daemon 的 `Broadcast { enabled, targets }`，`targets` **空 = 全部已打开会话**
  ——最常见的用法就是连上几台、打开广播、敲的命令同时生效，不必维护名单。
- `broadcast.send` 在**广播未开启且未显式给名单时直接报错**：一次误触把命令打进所有机器
  的代价太大，宁可让调用方多写一次 `broadcast.set`。
- TUI：`Alt+B` 开关、`Alt+M` 逐台勾选/取消；状态栏常驻 `广播×N`，参与的标签带 `*` 前缀
  ——开着广播时不该让人忘了自己开着。勾选同步到 daemon（**空名单也要发**，否则最后一个
  勾选取消不掉）。
- 审计只落**一条汇总**（`host_name = broadcast×N`）：TUI 广播时每次按键都会走到这里，
  逐台记录会把审计表刷爆。

**批量（FR-5.2 / FR-5.4）**

- 选择器 `HostSelector { hosts, group, tag, all }`：三种来源取并集去重，
  `all` 必须显式写出——"对全部机器执行"不能由"条件都没给"隐式触发。
  显式名单里有找不到的名字 → **整批失败**：少跑一台比跑不了更难发现。
- 并发：`tokio::Semaphore` 限并发（默认 16，`limits.batch_concurrency` 可配）+ `JoinSet`
  收结果，**逐台独立超时**；结果按输入顺序归位（`JoinSet` 返回无序，靠 idx 回填）。
- `fail_fast`：先失败的那台置位 `cancelled`，还在排队的任务直接标 `skipped`。
- 模式：默认 **exec channel**（各建一次性连接，30 台并行才有意义）；
  `--mode shell` 时改为一次性会话 + 哨兵截取，且**不进会话表**——
  否则批量一次几十条临时会话会把 `session.list` 淹没。

**风险与配额按整批算一次（FR-5.5）**

- 多台主机的 `risk_cap` 取**最严**的那台（cap 越小越严，未配置视为不限制），
  用一次 `assess().capped(min_cap)` 得出整批档位。
- 速率配额整批扣 **1 次**：30 台各扣一次等于把批量判死。
- confirm 档签发**批量令牌**（`PendingConfirm.batch`），带令牌重放时校验命令一致，
  令牌同样一次性；错误信封的 `data` 里带回 `hosts` 清单，AI 知道这次会影响哪几台。

**结果与历史（FR-5.3 / OQ-5）**

- 返回 `{run_id, total, ok, failed, skipped, duration_ms, results[]}`，
  每台带 `status: ok|failed|timeout|error|skipped` 与 `exit_code / duration_ms / error`。
- AI 侧部分失败 → 退出码 1（`batch_partial`），**整份结果仍在 `data` 里**——
  批量场景下"哪几台失败"比"失败了"重要得多。
- `--out csv` 按 RFC4180 转义（Excel / `jq` 可直接读），`--output F` 落文件。
- 落库：`batch_run` 存汇总、`batch_result` 存逐台，**同一事务**；`ai batch list/show` 回看，
  明细里的 stdout 跟随 `archive_output` 策略（见 §8.2）。

### 5.7 打磨层（M5 / FR-1.9、FR-1.10、FR-3.6~3.8、FR-8.5、FR-9.3~9.5）

这一节记的是 M5 新增的五块能力，以及各自**为什么这么做**的取舍。

**命令面板（FR-3.8）**

- 新增 `crates/sh0-tui/src/palette.rs`：动作清单是**手工维护的静态表**（`items()`，
  带标题 / 分组 / 说明），过滤用简单的子串匹配（`filter()`），不引入模糊匹配库。
  代价是覆盖不全——目前只有连接、断开、搜索、切换主题、切视图、批量等常用动作；
  好处是每一项都能写清楚人话说明，且不与键位表产生第二份真相。
- 键位归一有个坑：xterm 把 `Shift+G` 上报成大写 `Char('G')`，而键位表写的是 `shift+g`。
  解决办法是 `sh0-term/src/keymap.rs` 末尾的自由函数 `normalize()`，在 `bind` / `get` /
  `from_pairs` 三处统一折成小写再比对，否则三套预设里 `Ctrl+Shift+P` 在部分终端上会失灵。
- 面板打开时鼠标事件一律忽略：面板占满主区，此时把点击透传给主机树会造成"点了别处
  却连上了某台主机"的误操作。

**多主题（FR-3.7）**

- `config.theme` 一个字符串决定整套配色（`dark` 默认 / `light` / `high-contrast`，
  见 `sh0-tui/src/theme.rs` 的 `THEME_NAMES`），主机树、标签栏、状态栏、边框共用一张配色表，
  不提供逐项覆盖——逐项配色是配置膨胀的经典起点，收益却只在极少数人身上。
- 命令面板里也列了"切换主题"这一类动作，改完 `config.toml` 由热重载生效，不用重启。

**配置热重载（FR-9.3）**

- daemon 侧把 `AppConfig` 放进 `RwLock`，每个请求进来的第一行取**快照**克隆
  （`let cfg = self.cfg.read().unwrap().clone();`），而不是全程持锁。
  这样配置变更不会和请求处理抢锁，也避免了一次请求里读到"半新半旧"的配置。
- 检测方式是 **mtime 轮询（2 秒）**，没上 `notify` 之类的文件监视库：
  Windows 上网络盘与部分编辑器的事件不可靠，轮询的实现成本更低，2 秒延迟对配置项可接受。
- **不是所有键都能热生效**。`term.*`、`keepalive_secs`、`cols/rows`、`memory_budget_mb`、
  `audit.enabled` 属于"必须重启"——它们影响终端网格的初始尺寸、已建连接的保活定时器等
  已经固化的状态。这些键变更时日志明确提示需要重启，而不是假装生效。
- IPC 新增 `CONFIG_RELOAD` / `CONFIG_GET`，`sh0 config reload` 走的就是前者。

**主机导入（FR-1.10，仅导入侧）**

- `crates/sh0-store/src/import.rs`：`ImportSource::SshConfig` 读 `~/.ssh/config`（含 `Include`
  展开），`ImportSource::Xsh` 读 Xshell 会话文件并在 UTF-8 失败时回退 UTF-16。
- 两条硬约束：**只读源文件**（绝不回写），**重名不覆盖**（按主机名去重，已存在则跳过）。
  导入是"我已有几十台机器，别让我重录一遍"的场景，静默覆盖已有条目的风险远大于收益。
- `--dry-run` 先报"会新增几台、跳过几台"，确认后再真跑。

**ProxyJump（FR-1.9）**

- 模型层：`Host` 加 `proxy_jump`（配置里写的名字，如 `bastion` 或内联 `user@host:port`），
  运行时再加一个 `#[serde(skip)]` 的 `jump: Option<Box<Host>>`——**展开后的链只在内存里**，
  不写回配置文件，避免下次加载时把已展开的链又当配置读一遍。
- 传输层：`sh0-ssh` 的 `open_stream()` 返回 `ConnStream` 枚举，`Tcp` 是直连，
  `Jump` 持有 `channel_open_direct_tcpip` 出来的 `Pin<Box<ChannelStream>>`。
  上层拿到的还是同一个 `ConnStream`，跳板与否对调用方透明。
- 这里踩过一个栈溢出：最初 `find_host` → `with_jump_chain` → `find_host` 相互递归，
  深度计数每轮归零，配置里若写成环就无限展开。修法是把"只查库不展开"的 `lookup_host`
  拆出来，链内递归一律调 `lookup_host`，展开只在最外层做一次，并加 `MAX_JUMP_DEPTH = 5`。

**日志轮转（FR-8.5）**

- 运行日志与**审计库保留是两套策略**，别混：审计库是 SQLite（30 天 / 512 MB，超限时删最旧
  并 VACUUM，见 §8.2）；这里的轮转针对 `data_dir/daemon.log`（按 `log.max_mb` 切分、保留
  `log.keep` 个，再按 `log.retention_days` 清理过期文件）。
- 启动期日志单独走 `daemon.boot.log`，**覆盖写**不参与轮转：启动阶段的问题要看的是"这一次"
  的完整过程，跟历史混在一起反而添乱；而 `daemon.log` 由 tracing 追加写并轮转。

**体积与启动（NFR-1 / NFR-2）**

- release profile：`opt-level="z"` + `lto="thin"` + `codegen-units=1` + `panic="abort"` + `strip`。
- 实测（Windows x86_64 / MSVC，2026-10-26）：二进制 **5.77 MB**（上限 15 MB），
  `sh0 --version` 冷启动 **82~94 ms**（首次冷页 182 ms，上限 200 ms）。
- 选 thin 而非 fat LTO 是实测后的决定：本项目的体积大头是 russh / ring / rusqlite(bundled)
  的固有代码，fat 只能再省几百 KB，却让 Windows 链接耗时涨到十几分钟、内存峰值翻倍
  （曾直接把构建拖挂）。

---

## 6. AI 子命令契约

### 6.1 命令总表

| 命令 | 说明 | 优先级 |
| --- | --- | --- |
| `sh0 ai host list [--group G] [--tag T]` | 列出主机（不含凭据） | P0 |
| `sh0 ai host show <selector>` | 单台主机详情 | P0 |
| `sh0 ai host import --from ssh\|xsh [--path P] [--dry-run]` | 导入既有主机（只读源文件、重名不覆盖）✅ | P2 |
| `sh0 ai session list` | 列出活跃会话 | P0 |
| `sh0 ai session open --host <sel> [--cols N] [--rows N]` | 打开会话，返回 session_id | P0 |
| `sh0 ai session close --session <id>\|--all` | 关闭会话 | P0 |
| `sh0 ai session status --session <id>` | 会话状态快照 | P0 |
| `sh0 ai session reconnect --session <id>` | 手动重连 | P0 |
| `sh0 ai exec run (--session <id> \| --host <sel>) --cmd <cmd> [--timeout N]` | 执行命令并取回输出 | P0 |
| `sh0 ai screen read --session <id> [--lines N] [--mode plain\|grid]` | 读取屏幕/滚动缓冲 | P0 |
| `sh0 ai input send --session <id> (--text T \| --keys K \| --hex H)` | 注入按键 | P0 |
| `sh0 ai log query [--host X] [--session S] [--since TS] [--actor ai] [--risk deny] [--limit N]` | 审计查询 | P0 |
| `sh0 ai file list (--session <id> \| --host X) --path P` | 列远程目录 ✅ | P1 |
| `sh0 ai file upload (--session <id> \| --host X) --local L --remote R` | 上传（分块，二进制安全） ✅ | P1 |
| `sh0 ai file download (--session <id> \| --host X) --remote R --local L` | 下载（分块） ✅ | P1 |
| `sh0 ai file read (--session <id> \| --host X) --path P [--max-bytes N]` | 读远程文件 ✅ | P1 |
| `sh0 ai file write (--session <id> \| --host X) --path P [--data S]` | 写远程文件（支持 `--encoding base64`） ✅ | P1 |
| `sh0 ai file stat / mkdir / rm / rename / chmod` | 其余文件操作 ✅ | P1 |
| `sh0 ai batch exec (--hosts a,b \| --group G \| --tag T \| --all) --cmd C [--mode exec\|shell] [--concurrency N] [--fail-fast] [--out table\|json\|csv] [--output F]` | 批量执行并汇总 ✅ | P1 |
| `sh0 ai batch list [--limit N]` | 最近 N 次批量执行 ✅ | P1 |
| `sh0 ai batch show --run-id <id> [--out table\|json]` | 回看一次批量的逐台明细 ✅ | P1 |
| `sh0 ai broadcast enable [--session S...]` / `disable` / `status` | 广播开关与参与名单 ✅ | P1 |
| `sh0 ai broadcast send (--text T \| --keys K \| --hex H) [--session S...]` | 把同一份输入发给所有参与会话 ✅ | P1 |

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

失败（`data` 与成功信封同级——AI 的解析路径只有一个，不必按 `ok` 分叉）：

```json
{
  "ok": false,
  "trace_id": "ai-7f3c",
  "error": {
    "code": "risk_denied",
    "message": "命令命中禁止规则: recursive delete on /"
  },
  "data": { "risk": "deny", "reason": "recursive delete on /" }
}
```

需确认（令牌必须随错误返回：`sh0 ai` 是一次性进程，收不到广播事件）：

```json
{
  "ok": false,
  "trace_id": "ai-7f3c",
  "error": {
    "code": "needs_confirmation",
    "message": "命令需确认后执行: 服务重启类操作"
  },
  "data": {
    "confirm_token": "cf_01J...",
    "expires_in_secs": 300,
    "host": "web-01",
    "command": "systemctl restart nginx",
    "risk": "confirm",
    "retry": "sh0 ai exec run --host web-01 --cmd 'systemctl restart nginx' --confirm-token cf_01J..."
  }
}
```

> `remote_nonzero`（退出码 1）与 `timeout`（退出码 4）是**带数据的失败**：
> 命令确实跑了、输出也在 `data` 里，AI 必须能读到半截输出再决定下一步。

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

### 6.4 exec 双模式

`exec run` 有两种跑法，AI 按场景选（`--mode shell | exec`，默认 `shell`）：

| 模式 | 机制 | 优点 | 代价 |
| --- | --- | --- | --- |
| `shell`（默认） | 复用交互 shell：发 `cmd\necho SH0_<ulid>:$?`，轮询屏幕直到哨兵输出行出现 | 拿得到交互上下文（`cd`、环境变量、`sudo` 会话）；不额外建连 | 依赖回显形态，`clear` / 改写 PS1 时可能退化为部分输出 |
| `exec` | 新开一条 SSH exec channel：`channel.exec(true, cmd)` | stdout / stderr 天然分离、退出码精确 | 独立通道，拿不到交互上下文；每条命令一次建连 |

**哨兵截取的启发式**（`crates/sh0-cli/src/exec.rs::extract_output`，纯函数、有单测）：

1. 哨兵行 = 去空白后**整行恰为** `SENTINEL:<整数>`（从后往前找第一行）。不能用 `contains`——
   `echo SH0_x:$?` 的**回显行**也含哨兵 id，会在命令还没跑完时误判结束；
2. 输出起点 = 从屏底样之后起，第一行包含命令前 40 字符（容忍折行）的**下一行**；
3. 剥掉回显噪声：**所有**含哨兵 id 的行（echo 回显），以及紧跟其后、同样含命令文本的
   readline 重绘行；
4. 超时（`timeout_ms` 到点）返回 `Partial`：退出码为 `null`、`timeout: true`，
   AI 必须能看到"命令跑了一半"这件事。

第 3 条是**真机实测**逼出来的。真机屏幕比预想的脏（bash + readline，一键式会话尤其明显）：

```text
date                    ← PTY 对第一行输入的裸回显（提示符还没打印）
echo SH0_x:$?           ← 第二行输入的裸回显
hell@hell:~ $ date      ← readline 换行后重绘的提示符 + 命令行
Sat 24 Oct 21:30:15 HKT 2026   ← 真正的输出
hell@hell:~ $ echo SH0_x:$?
SH0_x:0
```

只剥"第一行回显"会留下重绘的命令行，只剥"最后一行"会留下两块噪声；而把所有"含命令
文本的行"都剥掉，又会吞掉那些输出里恰好含命令文本的命令（如 `echo foo`）。
最终规则：剥含哨兵 id 的行（哨兵是随机 ULID，真实输出里不可能出现）+ 紧跟其后的一行
readline 重绘，跳过上限 4 行。

`timeout` 字段恒出现在 JSON 里（即便为 `false`）——AI 判读输出完整性只看这一个字段，
让它时有时无等于逼调用方多写一个分支。

### 6.5 确认流程（有人值守 / 无人值守）

```
AI: exec run（confirm 档，无令牌、无 --yes）
      │
      ├─ 签发一次性令牌 cf_<ulid>（TTL 5 分钟）存入待确认表
      ├─ 广播 confirm.requested 事件 ──► TUI 弹模态框（y 批准 / 其它键拒绝）
      │                                   └─ 批准：TUI 调 exec.confirm(token)
      └─ 返回 needs_confirmation + 令牌（无人值守时 AI 走这条路）
             │
             └─ AI 二次调用 exec run --confirm-token <token>
                   ├─ 令牌取出即消费（防重放），校验 session/命令匹配
                   ├─ 执行 + 广播 confirm.resolved
                   └─ 审计记 decision=confirmed（区别于直接放行的 executed）
```

- `--yes` 等价人工批准，但**对 `deny` 档无效**（AC-5：一律拦截）。
- `--dry-run` 只回风险判定，不执行、不建连。
- 审计流水成对出现：`needs_confirmation` → `confirmed`（或 `failed`）。

### 6.6 AI 使用的推荐范式

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

- 默认**关闭**：`limits.rate_limit_per_min = 0` 表示不限；配成正整数后生效
  （需求文档给的参考值是单 actor 每分钟 60 条命令）。
- 超限返回 `rate_limited`，`detail.retry_after_sec` 给出建议等待时间。
- 批量执行按整体计为 1 次配额（避免 30 台机器瞬间打满）。

---

## 8. 持久化设计

### 8.1 文件布局

```
<config_dir>/sh0/
├── config.toml          # 全局偏好：主题、键位、缓冲、并发、超时、审计与限速
├── hosts.toml           # 主机与分组（无明文密钥）
├── risk.toml            # 风险规则（用户自定义，覆盖内置默认值）
<data_dir>/sh0/
├── known_hosts          # OpenSSH 兼容格式（注意在数据目录，不在配置目录）
├── creds.vault          # 主密码加密的凭据（钥匙串不可用时的回落）
├── audit.db             # SQLite：审计与输出归档
└── run/
    ├── daemon.json      # 运行时 IPC 端点 + token（daemon 存活时才存在）
    ├── daemon.log       # 后台 daemon 日志
    └── sh0.pid
```

### 8.2 SQLite 表（M2 实现）

审计与输出归档合在一张表里（命令输出是短文本，分表只增加 join 成本）：

```sql
CREATE TABLE audit (
  id INTEGER PRIMARY KEY, ts TEXT NOT NULL,           -- RFC3339，便于直接读
  trace_id TEXT, session_id TEXT, host_name TEXT,
  actor TEXT NOT NULL, action TEXT NOT NULL,          -- exec.run / exec.confirm / session.open ...
  command TEXT, risk TEXT, decision TEXT,             -- auto|confirm|deny / executed|confirmed|denied|needs_confirmation|timeout|failed
  exit_code INTEGER, duration_ms INTEGER, truncated INTEGER,
  error TEXT, stdout TEXT, stderr TEXT                -- 归档（可关）
);
CREATE INDEX idx_audit_ts ON audit(ts);
CREATE INDEX idx_audit_host ON audit(host_name, ts);
CREATE TABLE meta (k TEXT PRIMARY KEY, v TEXT);       -- schema 版本等

-- M3：批量执行历史（OQ-5）。一条 run 对多台 result，
-- 列表页只查汇总，明细按 run_id 单独取。
CREATE TABLE batch_run (
  id INTEGER PRIMARY KEY, run_id TEXT NOT NULL UNIQUE, ts TEXT NOT NULL,
  trace_id TEXT, actor TEXT NOT NULL, command TEXT, mode TEXT,
  risk TEXT, decision TEXT, concurrency INTEGER,
  total INTEGER, ok INTEGER, failed INTEGER, skipped INTEGER, duration_ms INTEGER
);
CREATE TABLE batch_result (
  id INTEGER PRIMARY KEY, run_id TEXT NOT NULL, host TEXT NOT NULL,
  status TEXT NOT NULL,                -- ok|failed|timeout|error|skipped
  exit_code INTEGER, duration_ms INTEGER, truncated INTEGER, timeout INTEGER,
  stdout TEXT, error TEXT              -- stdout 跟随 archive_output
);
CREATE INDEX idx_batch_run_ts ON batch_run(ts);
CREATE INDEX idx_batch_result_run ON batch_result(run_id);
```

- **脱敏先行**：写入前对 `key=value` 口令、`Authorization: Bearer/Basic`、PEM 私钥块、
  `AKIA*` 访问键、`--password x`、` -p<值>` 做掩码，落库的永远是掩码后的文本。
- **保留策略**：默认 30 天 / 512 MB，超期与超体积都删最旧（体积超限后 `VACUUM`）；
  每 100 条写入触发一次清理。可在 `config.toml` 的 `[audit]` 段关闭归档或改阈值。
- 查询接口：`sh0 ai log query [--host] [--session] [--since] [--actor] [--risk] [--limit≤1000]`。

### 8.3 凭据

- 优先 OS 钥匙串：`keyring` crate（macOS Keychain / Windows Credential Manager / Linux Secret Service）。
- 回落：主密码 → Argon2id（19 MiB / t=2 / p=1）派生密钥 → ChaCha20-Poly1305 加密 `creds.vault`
  （JSON 文件，salt / nonce / 密文 hex 编码，tmp + rename 原子写）。
- 主密码来源：`SH0_MASTER_PASSWORD` 环境变量，否则 stdin 提示（daemon 后台场景 stdin 为 null，
  此时 vault 处于 `Locked`——不会静默降级成"什么都读不到但也不报错"）。
- `SH0_DISABLE_KEYRING=1` 可强制走 vault（CI / 测试用它保证确定性）。
- 私钥：只存文件路径，不复制内容；私钥自身的 passphrase 走上述加密存储。
- `sh0 secret set/get/list/rm` 是面向命令行的管理入口。

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

### 9.2 关于 SQLite 的 bundled 编译（M2 引入的第二处 C 依赖）

审计库用 `rusqlite` 的 `bundled` feature：SQLite 源码随 crate 一起编译，产物**不依赖系统
SQLite**——这是"单二进制、跨平台行为一致"（NFR-1 / NFR-5）的必要代价，与 `ring` 同属
"只要 C 编译器、无需额外工具链"的一类（`cc` crate 自动探测 MSVC / gcc），不引入 cmake / nasm。

已评估的替代：`sqlx`（异步、但要链接系统库或同样 bundled）、纯 Rust 的 `redb`（无 SQL，
审计的多条件过滤与回溯查询要自己写索引，不划算）。结论：保留 `rusqlite bundled`。

### 9.3 IPC 帧上的一个易错点（M2 实测）

客户端读循环收到**事件帧**时，若事件通道已无接收者（`sh0 ai` 这类一次性客户端不订阅事件），
必须**丢弃事件继续读**，不能中断循环——一旦中断，后续所有应答再无人分发，调用方永久挂起。
M2 契约测试正是被这个点卡住过（症状：daemon 明明已应答，客户端仍在超时）。

同时：错误信封支持挂载 `data`（确认令牌走这条路），服务端 `ErrorBody { code, message, data }`。

### 9.4 真机（Debian / OpenSSH）踩到的两个 SSH 层陷阱

1. **不认识的通道消息不能当成 EOF**。pty-req 的 `Success`、`WindowAdjust` 等非数据消息
   会先于 shell 输出到达；早期实现把它们一律映射成 `TransportEvent::Eof`，结果会话刚连上
   就"EOF"断开了。mock 传输层不产生这类消息，所以 79 条测试全绿也没能发现——真机一轮就暴露。
   现在未知消息一律忽略并继续等下一条。
2. **EOF 后要再等一会儿收退出码**。OpenSSH 常在 `EOF` 之后才补发 `exit-status`，
   见 EOF 就退出会让 `exit_code` 恒为 `null`（AC-4 直接不成立）。现在 EOF 后多等 500ms。

### 9.5 按键事件必须过滤 `Release`（否则输入全是双份）

crossterm 0.26 起 `KeyEvent` 带 `kind`：`Press` / `Repeat` / `Release`。**Windows 控制台
（以及开启 kitty 键盘协议的终端）会为每次按键同时产生按下与松开两个事件**。事件循环若
不加区分地 `Event::Key(k) => on_key(k)`，每个字符就会被发往远端两遍——敲 `ls` 变成 `llss`
（M5 收尾时在 Windows 上实测到的现象）。

修法是在事件循环入口丢掉 `Release`、保留 `Press` 与 `Repeat`（长按属于真实的重复输入）：

```rust
pub fn should_handle_key(key: KeyEvent) -> bool {
    !matches!(key.kind, KeyEventKind::Release)
}
```

两点值得记：

- 过滤放在**事件循环**而不是 `on_key` 里，是因为 `on_key` 只是"处理一个按键"的动作，
  是否算一次输入属于事件源的语义。
- 为了让回归测试测到生产代码本身，过滤被抽成 `sh0_tui::dispatch_key(app, key)`，
  测试投递"按下 + 松开"成对事件，断言输入框里是 `web` 而不是 `wweebb`——
  若只在测试里复制一遍同样的 if，这类 bug 下次还会溜过去。

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
| 命令面板 / 主题（§5.7） | FR-3.7、FR-3.8 |
| 配置热重载与 `sh0 config`（§5.7） | FR-9.3、FR-9.4、FR-9.5 |
| 主机导入（§5.7） | FR-1.10（导入侧） |
| ProxyJump（§5.7） | FR-1.9 |
| 日志轮转（§5.7） | FR-8.5 |
| release profile（§5.7） | NFR-1、NFR-2 |
