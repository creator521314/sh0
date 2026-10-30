# sh0 使用说明

> 对应版本：**v0.4（当前工作区，M0~M6 已完成）**
> 本文覆盖：安装构建、TUI 主机管理与连接、`ai` 接口、配置与凭据、审计复盘、故障排查。
> 顶层命令：核心入口 `tui / ai / help`，外加运维命令 `daemon`（run / stop）。
> 契约细节（JSON 信封字段、错误码、表结构）以 `docs/architecture.md` 为准，本文件只讲"怎么用"。

---

## 1. 安装与构建

sh0 是单个二进制，无运行时依赖（SQLite 与 crypto 已静态编译进产物）。

```bash
cargo build                      # 调试构建
cargo build --release            # 发布构建（已用 lto="thin"，见 §11）
cargo test --workspace           # 171 条测试
cargo clippy --workspace --all-targets   # 应零警告
```

前置：Rust 工具链 1.7x+；Windows 需要可用的 MSVC 工具链；Linux/macOS 需要 C 工具链（用于
`rusqlite bundled` 与 `ring`）。

构建产物：`target/debug/sh0`（或 `target/release/sh0`）。把它放进 `PATH` 即可全局使用。

---

## 2. 五分钟上手

```bash
sh0 help               # 用法 + 配置/数据目录
sh0 tui                # 进入界面；主机清单为空也能直接按 n 新建
#   → 填地址/用户/密码（默认 password，掩码；agent/key 用 ←/→ 切换）
#   → Enter 保存并连接；Ctrl+Q 退出
sh0 tui web-01         # 也可以直接指定主机进入
```

在 TUI 里：`Ctrl+Q` 退出、`Ctrl+T` 选主机开新会话、`Alt+←/→` 切标签、`PageUp/PageDown` 回看历史。
主机树里：`n` 新建并连接、`e` 编辑、`d` 删除、`/` 搜索、`Enter` 连接。

不进界面，直接用命令行（AI 接口同时也是很好的脚本接口）：

```bash
sh0 ai host list --json
sh0 ai exec run --host web-01 --cmd "df -h /" --json
```

---

## 3. 配置目录与文件

`sh0 help` 会打印三个目录：

| 平台 | 配置目录 | 数据目录 |
| --- | --- | --- |
| Linux | `~/.config/sh0/` | `~/.local/share/sh0/` |
| macOS | `~/Library/Application Support/sh0/` | 同左 |
| Windows | `%APPDATA%\sh0\` | 同左 |

文件布局：

```
<config_dir>/sh0/
├── config.toml      # 全局偏好（键位、缓冲、并发、超时、审计、限速、风险）
├── hosts.toml       # 主机与分组（不含任何明文口令）
└── risk.toml        # 自定义风险规则（可选，缺省时用内置规则集）
<data_dir>/sh0/
├── known_hosts      # OpenSSH 兼容格式，TOFU 自动记录
├── creds.key        # 本地 data-key（0600，首次使用自动生成）
├── creds.vault      # 密码/私钥的密文库（ChaCha20-Poly1305）
├── audit.db         # SQLite：审计与输出归档
└── run/
    ├── daemon.json  # daemon 存活时的 IPC 端点 + token
    ├── daemon.log   # 后台 daemon 的日志（后台进程无控制台，出错靠它）
    └── sh0.pid
```

> 注意 `known_hosts` 在**数据目录**（不是配置目录）下。想让某台机器重新走 TOFU，
> 删的是 `<data_dir>/sh0/known_hosts` 里的对应行。

### 3.1 `config.toml`

全部字段都可省略，省略即用默认值。首次加载时若文件不存在，会自动写一份**全字段默认值**的
`config.toml`（此时你会看到一个填满默认值的文件，直接改你想改的那几项即可）。

```toml
version = 1
term = "xterm-256color"      # 请求 PTY 时上报的终端类型
scrollback_lines = 10000     # 每个会话的滚动缓冲行数（软上限）
keepalive_secs = 30          # SSH 心跳间隔，0 = 关闭
cols = 120                   # 新建会话默认尺寸
rows = 32
memory_budget_mb = 64        # 全部会话共享的 scrollback 内存预算，0 = 不限制
keymap = "default"           # default | vim | xshell
mouse = true                 # 鼠标：主机树单击选中 / 双击连接 / 滚轮滚动（false = 完全不用鼠标）
theme = "dark"               # dark | light | high-contrast（high-contrast 给强光/弱视环境）

[keys]                       # 单键覆盖，优先级高于预设
quit = "ctrl+x"

[reconnect]
max_attempts = 10            # 整数；不写这一行 = 无限重试
base_ms = 500                # 指数退避起点
max_delay_ms = 30000         # 退避上限
jitter = true                # 抖动，避免同时重连打爆服务端
mark = true                  # 重连成功后在历史输出里插一条分隔线

[audit]
enabled = true
archive_output = true        # 归档命令输出
retention_days = 30
max_db_mb = 512              # 超限从最旧记录开始淘汰

[log]                        # daemon 运行日志（与上面 [audit] 的审计库是两套策略）
max_mb = 8                   # 单个 daemon.log 的大小上限，超出即切新文件
keep = 3                     # 保留的历史文件个数（.1 ~ .keep）
retention_days = 30          # 超期的历史文件直接删

[limits]
max_output_bytes = 65536     # 单次 exec 的 stdout+stderr 上限，超出截断并置 truncated
exec_timeout_secs = 30       # exec 默认超时，--timeout 可覆盖（会被夹到 1~3600 秒）
rate_limit_per_min = 0       # 单 actor 每分钟 exec 次数，0 = 不限
batch_concurrency = 16       # 批量执行默认并发数

[risk]
rules_file = "risk.toml"     # 相对 config_dir
unmatched = "confirm"        # 未命中任何规则时的默认档：auto | confirm | deny
```

> `unmatched` 默认 `confirm` 是刻意的：宁可多问一次，不可放过一条。想让 AI 更自主，
> 把它改成 `auto`，但请同时确认 `risk.toml` 里的 deny 规则覆盖了你的红线。

### 3.2 `hosts.toml`

> 正常使用不需要手写：在 `sh0 tui` 里按 `n` 新建，TUI 会生成 id、写好认证引用，
> 并把密码/私钥存进加密库。下面是文件形态，便于排障与手工维护。

```toml
version = 1

[[hosts]]
id = "01M5N06QYP6GW5W2JK73XVCH38"   # 可省略，加载时自动生成
name = "web-01"
group = ["prod", "web"]             # 分组面包屑，支持多级
addr = "10.0.0.11"
port = 22                           # 默认 22
user = "root"
auth = "agent"                      # agent | key_file | key | password
tags = ["nginx"]
note = "主站"
risk_cap = "confirm"                # 该主机允许的最高档位，省略 = 不限制

[[hosts]]
name = "db-01"
group = ["prod", "db"]
addr = "10.0.0.21"
user = "dba"

[hosts.auth]
# 外部私钥文件（只引用路径，不代管）
key_file = { path = "/home/u/.ssh/id_ed25519" }
# 已导入加密库的私钥（TUI「认证文件」导入后写的就是这种）
# key = { secret = { keyring = { service = "sh0", account = "<host.id>.key" } },
#         passphrase = { keyring = { service = "sh0", account = "<host.id>.key.pass" } } }
# password = { secret = { env = "SH0_PW" } }
# password = { secret = { keyring = { service = "sh0", account = "<host.id>" } } }
```

三条易踩的写法规则：

1. `auth` 只有两种写法：单元变体写字符串（`auth = "agent"`），结构化变体写成嵌套表
   （`[hosts.auth]` 下填 `key_file = {...}`、`key = {...}` 或 `password = {...}`）。
2. `password` / `key` 的字段名是 `secret`，中间这一层别漏。
3. 外部私钥路径（`key_file`）不支持 `~` 展开，写绝对路径；TUI 导入模式不受此限。

`risk_cap` 的语义：命令判定档位高于 `risk_cap` 时按 `risk_cap` 处理（引擎仍可能给更高档，
daemon 取较小值）。给生产组设 `confirm`，可以让 auto 档的危险命令也被拦下来问一次。

### 3.3 `risk.toml`

可选。写成内置规则的补充/覆盖：

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
max_level = "confirm"
```

---

## 4. 凭据管理

配置文件**永远不存明文口令**，只存"去哪里取"：`agent` / `key_file` / `key` / `env` /
`prompt` / `keyring`。正常使用**不需要任何命令行**：在 TUI 新建/编辑主机时直接输密码或
导入私钥，daemon 负责写入加密库。

- 后端：OS 钥匙串优先（Keychain / 凭据管理器 / Secret Service）；不可用时回落本地
  `creds.vault`，用 `creds.key`（0600）做 data-key 加密，**不再需要主密码**。
- 主机引用：密码存 `<host.id>`，私钥 PEM 存 `<host.id>.key`，私钥口令存
  `<host.id>.key.pass`（`service = "sh0"`）。
- 删除主机会先关闭该主机关联的连接，再级联清理旧 account；切换认证方式只清理旧 account。
- 旧版主密码 vault：设置 `SH0_MASTER_PASSWORD` 后任意 `sh0` 启动会自动迁移；
  未设置则备份为 `creds.vault.legacy` 并新建空库。

> 信任边界：`creds.key` 与 `creds.vault` 同机同用户，强度等于文件权限（0600），
> 与 `~/.ssh` 私钥同档——防其他用户/备份泄漏，不防本机 OS 账户本身。

---

## 5. TUI 使用说明

### 5.1 界面布局

```
┌──────────────┬────────────────────────────────────────────┐
│ 主机树        │ [web-01] [db-01*] [web-02]      ← 标签栏   │
│ ▾ prod        │                                            │
│   ▾ web       │ 主区：终端（或 F9 切换的双栏文件视图）        │
│     web-01    │                                            │
│   ▾ db        ├────────────────────────────────────────────┤
│     db-01     │ 状态栏：会话状态 · 广播×2 · 提示信息          │
└──────────────┴────────────────────────────────────────────┘
```

标签上的 `*` 表示这台参与了广播；状态栏常驻 `广播×N` 表示广播已开启。

### 5.2 键位（default 预设）

| 按键 | 作用 |
| --- | --- |
| Ctrl+Q | 退出 |
| Ctrl+T / Ctrl+W | 选择主机开新会话 / 关闭会话 |
| Alt+← / Alt+→ | 切换会话 |
| Ctrl+B | 焦点在主机树与终端间切换 |
| Ctrl+F | 搜索主机 |
| Ctrl+N | 新建主机并连接 |
| PageUp / PageDown | 回看历史输出 |
| Ctrl+PageUp / Ctrl+PageDown | 回到顶部 / 回到底部 |
| Alt+V | 进入/退出文本选择模式 |
| Alt+C | 复制选区 |
| Alt+R | 手动重连 |
| Ctrl+L | 清屏 |
| Alt+B | 广播开关 |
| Alt+M | 勾选/取消当前会话参与广播 |
| F9 | 终端视图 / 双栏文件视图切换 |
| **Ctrl+Shift+P** | **命令面板**（模糊搜索：内置动作 / 连接主机 / 切换主题） |

> 焦点标识：当前焦点区域的边框使用主题强调色；终端区标题在聚焦时显示
> `终端 [焦点]`，否则只显示 `终端`。`Ctrl+B` 在主机树与终端（或文件区）之间切换。

另两套预设：`keymap = "vim"`（hjkl / `/` 搜索，`Alt+G` 到顶、`Shift+G` 到底）、
`keymap = "xshell"`（Alt 组合为主，`Shift+PageUp` 回看）。任意单键可在 `config.toml` 的
`[keys]` 里覆盖，例如 `quit = "ctrl+x"`、`command.palette = "alt+p"`。写错的键名只会在日志里
告警，不会让程序起不来。

带 Shift 的字母键按**小写**匹配：终端上报的是 `Shift+G` 对应的大写 `G`，配置里写 `shift+g`
即可，两边都会归一成小写再比。

**主机树内**（焦点在左侧树时）：

| 按键 | 作用 |
| --- | --- |
| `n` | 新建主机并连接 |
| `e` | 编辑当前主机 |
| `d` | 删除当前主机（再按 `y` 确认；该主机的连接会一起关闭） |
| `/` | 搜索主机 |
| `Enter` | 连接当前主机 / 展开分组 |

**新建/编辑表单**：

| 按键 | 作用 |
| --- | --- |
| `Tab` / `Shift+Tab` / `↑` / `↓` | 切换字段（跳过当前认证方式下不显示的字段） |
| `←` / `→` | 切换认证方式：agent → password → key → none |
| `Enter` | 保存并连接 |
| `Ctrl+S` | 仅保存，不连接 |
| `Esc` | 取消 |

新建默认 `password`，此时出现「密码」字段（掩码 `*`，编辑时留空 = 保持原密码）；
选 `key` 时出现「私钥文件」与「私钥口令」：填路径后保存会把 PEM 导入加密库，
之后即使原文件被删除也能连接。选 `agent` / `none` 无凭据字段。

### 5.2.1 命令面板（Ctrl+Shift+P）

面板里能搜到三类东西：

- **动作**：全部内置命令（搜索主机、新建会话、广播开关、清屏……）。动作名也能搜，
  输 `reconnect` 能命中"重新连接"。
- **主机**：`连接 web-01` 这类条目，回车直连，右侧显示 `user@addr:port`。
- **主题**：`主题：dark / light / high-contrast`，回车切换并**写回 config.toml**。

`↑↓` 选择、`Enter` 执行、`Esc` 取消、直接打字过滤（模糊子序列匹配，不用连着打）。
执行完面板关闭，焦点回到原来的地方。

### 5.2.2 主题

`config.toml` 里 `theme = "dark" | "light" | "high-contrast"`（默认 `dark`），
或在命令面板里随时切换。`high-contrast` 是纯黑白 + 高饱和语义色，给强光/弱视环境用。

### 5.3 鼠标操作

默认开启（`config.toml` 的 `mouse = true`）：

**主机树**

| 操作 | 效果 |
| --- | --- |
| 单击主机行 | 选中它，并把焦点交给主机树（省掉先按 `Ctrl+B`） |
| 双击主机行 | 连接（与回车完全同一条路径） |
| 单击分组行 | 展开 / 折叠（`▸` ⇄ `▾`） |
| 滚轮 | 上下移动选择 |

**标签栏**

| 操作 | 效果 |
| --- | --- |
| 单击标签 | 切到那个会话，并把焦点给它 |
| 单击标签上的 `×` | 关闭那个会话（不用先切过去） |
| 中键单击标签 | 同上，关闭（浏览器习惯） |

**分隔条**（主机树与主区之间）

| 操作 | 效果 |
| --- | --- |
| 按住左键左右拖 | 调整主机树宽度，松手后终端尺寸同步到远端 |
| 拖到最左 | 树宽最小 12 列；再窄 `[名字]` 就显示不下了 |
| 拖到最右 | 终端至少留 20 列，否则远端 shell 的折行会乱 |

**终端区**

| 操作 | 效果 |
| --- | --- |
| 按住左键拖 | 框选文本，松手即复制（走 OSC 52 写剪贴板，与 `Alt+V` 同一条路） |
| 单击（没拖动） | 只是取消选区，不会复制出东西 |
| 滚轮 | 回看历史输出，一次 3 行；往下滚到底会贴回最新输出 |

**文件视图**

| 操作 | 效果 |
| --- | --- |
| 单击条目 | 选中它，并把焦点给这一栏（左栏 ⇄ 右栏） |
| 双击条目 | 进入目录 / 打开文件操作（与回车同一条路） |
| 滚轮 | 在当前焦点栏里上下移动选择 |

**通用**

| 操作 | 效果 |
| --- | --- |
| Shift + 鼠标 | **交回父终端**，此时可以用系统自带的"按住左键拖选复制" |

> Shift 这个约定是必须的：开了鼠标捕获，父终端就收不到鼠标事件，拖选复制会失效。
> 按一次 Shift+鼠标即挂起捕获，之后按任意键恢复。
> 完全不需要鼠标就在配置里写 `mouse = false`，捕获从头到尾不开。

表单 / 确认弹窗 / 输入提示 / 命令面板期间不接受点击——那些界面是键盘模态的，
点半下可能把正在填的东西弄丢。

> 命中区与渲染共用同一份布局计算（`App::tab_segments` 之类），不是两处各算一遍——
> 否则改一次间距就会出现"看着点在标签上却没反应"这种毛病。

### 5.4 文本选择与复制

`Alt+V` 进入选择模式，方向键或 hjkl 扩选，`Enter` / `y` 复制，`p` 把复制内容粘回会话，
`Esc` 取消。复制走 OSC 52 写父终端剪贴板——需要父终端支持（Windows Terminal / iTerm2 /
kitty / Alacritty 均支持）；不支持时内容仍留在 sh0 内部，可用 `p` 粘回会话。

### 5.5 双栏文件视图（F9）

左本机 / 右远端，各自独立 cwd、排序与选中行。远端列目录复用当前会话已有的 SSH 连接，
**不新开 TCP 连接**（所以文件视图要先用终端视图连上那台机器）。

| 按键 | 作用 |
| --- | --- |
| Tab | 左右栏切换 |
| ↑↓ 或 k/j | 上下选择 |
| Enter / → | 进入目录 |
| Backspace / ← | 回到上级目录 |
| g | 输入路径跳转 |
| s | 切换排序键 |
| r | 刷新 |
| Space | 标记/取消标记当前项（可多选） |
| F5 | 把标记项（无标记时是当选项）传向对面栏 |
| F6 | 重命名 |
| F7 | 新建目录 |
| F8 / Delete | 删除（需确认） |
| p | 改权限（输入八进制，如 644） |
| R | 重新排队失败的传输 |
| c | 清理已完成的传输 |
| Esc | 回终端视图 |

传输走队列：每帧推进 256 KB 的块，底部面板显示进度条 / 速度 / 失败原因，
单块失败自动重试 3 次。二进制自动 base64，逐字节一致。

### 5.6 广播

1. 用终端视图打开若干会话（每台机器一个标签）。
2. `Alt+M` 在当前标签上勾选/取消参与广播；逐台切过去勾完。
3. `Alt+B` 打开广播。此后你的键盘输入会**同时发给所有参与会话**——状态栏显示 `广播×N`，
   参与的标签带 `*`。
4. 再按 `Alt+B` 关闭广播，名单保留，下次开启沿用。

广播是"同一份输入发给多台"，不是"结果汇总"——看结果请逐台切标签，或用 `ai batch exec`。

> 危险：广播开着时，你敲的 `rm -rf xxx` 会在所有参与机器上执行。批量 destructive 操作
> 建议走 `ai batch exec`，因为它会过风险引擎并留审计。

---

## 6. 命令行

顶层命令：

```bash
sh0 tui [主机名或ID]     # 启动 TUI；省略时进入主机树（即使一台主机都没有）
sh0 ai <对象> <命令> ...  # 面向 AI/脚本的结构化接口（见 §7）
sh0 help                 # 用法 + 配置/数据/日志目录
sh0 daemon run           # 前台运行 daemon（调试用）
sh0 daemon stop          # 优雅停止后台 daemon
```

daemon 平时由 `sh0` 自动拉起，一般不需要手动执行；`daemon` 命令保留给调试/排障。
它在**没有活跃会话、且 5 分钟没有请求**时自动退出（TUI 每 30 秒 ping 一次保活），
所以正常使用不需要停它。`sh0 help` 的「后台 daemon」一段会说明它的职责与日志位置。

> 旧版的 `sh0 host` / `sh0 config` / `sh0 secret` 已移除：
> 主机管理在 TUI 里；`ai host list/import` 覆盖了只读查询与导入；
> 配置直接改文件（自动热重载）；密码/私钥在 TUI 新建主机时录入。

### 6.1 配置热重载

daemon 每 2 秒检查一次 `config.toml` / `hosts.toml` 的 mtime，改完**不用重启**：

| 配置 | 是否热生效 |
| --- | --- |
| `limits.*`（超时、输出上限、限速、批量并发） | 立即 |
| `risk.*`（默认档位、规则文件） | 立即 |
| `hosts.toml` 的主机增删 | 立即（每次按名字现读文件） |
| `term` / `keepalive_secs` / `cols` / `rows` / `memory_budget_mb` / `audit.enabled` | 需重启 |

改到最后一类时，`daemon.log` 里会提示哪些项需要重启。配置写错时，任意 `sh0` 调用
启动时都会解析并报错（等效于旧版的 `config check`）。

### 6.2 导入既有配置（只读）

```bash
sh0 ai host import --from ssh --dry-run        # 先看会导入什么
sh0 ai host import --from ssh                  # 真的写进 hosts.toml
sh0 ai host import --from xsh --path "D:/Xshell/Sessions"
```

- 只**读**源文件，绝不回写 `~/.ssh/config`。
- 映射：`HostName→addr`、`User→user`、`Port→port`、`IdentityFile→私钥认证`
  （`~/` 会自动展开成绝对路径）、`ProxyJump→proxy_jump`（取逗号分隔的最后一跳）。
- `Host *` 这类通配段、含 `%h` 占位符的条目会被**跳过**并在回显里说明原因。
- Xshell 只导入 `PROTOCOL=SSH` 的会话；**口令字段一律不导入**，需要连接时在 TUI 里补。
- 与现有主机**重名时保留现有配置**，回显里标为 `=`。

---

## 7. AI 接口使用说明

所有能力都在 `sh0 ai` 下，一次调用一个进程，输出结构化。

### 7.1 通用参数

| 参数 | 说明 |
| --- | --- |
| `--json` | 输出结构化 JSON 信封（默认是人类可读文本） |
| `--trace-id <id>` | 调用链追踪 ID，会原样回显并写进审计 |
| `--timeout <秒>` | 本次调用超时，默认 30，会被夹到 1~3600 |
| `--yes` | 无人值守场景下显式批准 confirm 档命令（等价于用户点了"确认"） |
| `--dry-run` | 只做风险判定不执行，用来自检"这条命令会被放行吗" |
| `--quiet` | 安静模式：不输出人类可读文本（`--json` 仍会打印） |

### 7.2 输出与退出码

JSON 信封统一为 `{ ok, trace_id, data }` 或 `{ ok, trace_id, error, data }`——
失败时 `data` 与成功同级，AI 的解析路径只有一个。

退出码是给 AI 判断"重试 / 换方案 / 转人工"用的：

| 码 | code | 含义 | AI 应对 |
| --- | --- | --- | --- |
| 0 | `ok` | 成功 | — |
| 1 | `remote_nonzero` / `batch_partial` | 远端退出码非零 / 批量部分失败 | 读 `data` 明细 |
| 2 | `risk_denied` | 命中禁止规则 | 换方案，不要重试 |
| 3 | `needs_confirmation` | 需确认 | 带 `confirm_token` 二次调用 |
| 4 | `timeout` | 超时（可能已有半截输出） | 读 `data` 后决定 |
| 5 | `session_unavailable` | 会话不可用 | 重新开会话，别去重启 daemon |
| 6 | `auth_failed` | 认证失败 | 检查凭据 |
| 7 | `auth_required` | 需要认证信息 | 补凭据 |
| 8 | `host_not_found` | 主机不存在 | 检查名字/ID |
| 9 | `rate_limited` | 触发限速 | 按 `retry_after_sec` 等待 |
| 10 | `ipc_unreachable` | daemon 不可达 | 检查 daemon |
| 11 | `invalid_args` | 参数错误 | 检查参数 |
| 12 | `internal` | 内部错误 | 上报 |

`remote_nonzero` 与 `timeout` 是**带数据的失败**：命令确实跑了，输出在 `data` 里。

### 7.3 主机与会话

```bash
sh0 ai host list [--group prod] [--tag nginx] --json
sh0 ai host show web-01 --json                       # 输出不含任何凭据

sh0 ai session open --host web-01 [--cols 120] [--rows 32] --json   # 返回 session_id
sh0 ai session list --json                           # 含 state 与 last_error
sh0 ai session status --session <id> --json          # 会话结束后仍可查到最后一份快照
sh0 ai session reconnect --session <id> --json
sh0 ai session resize --session <id> --cols 160 --rows 40 --json
sh0 ai session clear --session <id> --json
sh0 ai session close --session <id> --json
sh0 ai session close --all --json
```

### 7.4 命令执行

```bash
# 一键式：自动建连 → 执行 → 关闭，返回 stdout/stderr/exit_code/duration_ms
sh0 ai exec run --host web-01 --cmd "uname -a" --timeout 10 --json

# 复用既有会话（保留 cd / 环境变量等交互上下文）
sh0 ai exec run --session <id> --cmd "pwd" --json

# 双模式
sh0 ai exec run --host web-01 --cmd "df -h /" --mode shell --json   # 默认：哨兵截取
sh0 ai exec run --host web-01 --cmd "df -h /" --mode exec  --json   # 独立通道，流分离
```

- `shell`（默认）：在交互 shell 里跑，用哨兵截取输出，**保留交互上下文**；
  在远端改写 `PS1`、`clear` 刷屏、回显折行等极端形态下可能退化成部分输出
  （此时 `timeout=true`、退出码为 `null`），切 `--mode exec` 可规避。
- `exec`：独立 channel，stdout/stderr 分离、退出码精确，但每次都是干净上下文。

### 7.5 读屏与按键注入

```bash
sh0 ai screen read --session <id> [--lines 80] [--mode plain|grid] [--no-scrollback] --json
sh0 ai input send --session <id> --text "uptime" --enter --json
sh0 ai input send --session <id> --keys ":q!<Enter>" --json
sh0 ai input send --session <id> --hex "1b5b41" --json
```

`--keys` 支持 `<Enter>` `<Esc>` `<Tab>` `<BS>` `<Up>` `<C-c>` 等占位符。

### 7.6 批量执行

```bash
# 四种目标选择（可叠加，取并集去重）；--all 必须显式写出
sh0 ai batch exec --host web-01 --host web-02 --cmd "df -h /" --json
sh0 ai batch exec --host web-01,web-02 --cmd "uptime" --json
sh0 ai batch exec --group prod --cmd "systemctl is-active nginx" --json
sh0 ai batch exec --tag nginx --cmd "nginx -t" --json
sh0 ai batch exec --all --cmd "hostname" --json

# 常用开关
sh0 ai batch exec --group prod --cmd "df -h /" \
     --concurrency 8 --fail-fast --out csv --output ./df.csv
sh0 ai batch exec --group prod --cmd "df -h /" --out json --json

# 历史回看
sh0 ai batch list --limit 20
sh0 ai batch show --run-id <id> [--out table|json|csv]
```

要点：

- `--host` 可重复传入，也可逗号分隔；**显式名单里有一台名字写错，整批直接失败**
  （宁可不跑，不要跑错机器）。
- `--group` 是前缀匹配：`prod` 命中 `prod` 及其子分组（`prod/web`）。
- 并发池默认 16（`limits.batch_concurrency`），`--concurrency` 覆盖；每台独立超时。
- `--fail-fast` 有失败就尽快收尾，剩下的标记 `skipped`。
- 汇总格式 `--out table`（人读，失败排前面）/ `json` / `csv`（RFC4180，可被 Excel 直接打开）；
  `--output` 导出到文件。
- 风险与配额**整批算一次**：多台 `risk_cap` 取最严，速率配额只扣 1 次。
- 批量默认走 `--mode exec`；`--mode shell` 会为每台临时建会话（不进会话表）。
- 历史回看依赖 `audit.enabled = true`：关掉审计就不落 `batch_run`，`batch list` 会提示未启用。

### 7.7 广播

```bash
sh0 ai broadcast enable --session <id> --session <id2> --json   # 不给 --session = 全部已开会话
sh0 ai broadcast status --json
sh0 ai broadcast send --text "uptime" --enter --json
sh0 ai broadcast send --keys "<C-c>" --json
sh0 ai broadcast send --text "uptime" --enter --session <id> --json  # 一次性指定目标
sh0 ai broadcast disable --json
```

名单存在 daemon 里，`enable` 一次后后续 `send` 不必再带名单；`disable` 保留名单，
下次 `enable` 沿用。广播未开启且本次也没给 `--session` → `invalid_args`。

审计只落**一条汇总**，不会按会话炸开成几十条。

### 7.8 文件操作

两种目标：`--session <id>` 复用已有连接（推荐），或 `--host X` 一键式自建自用连接。

```bash
sh0 ai file list    (--session <id> | --host X) --path /var/log --json
sh0 ai file stat    (--session <id> | --host X) --path /etc/hosts --json
sh0 ai file read    (--session <id> | --host X) --path /etc/hosts [--max-bytes 1048576] --json
sh0 ai file write   (--session <id> | --host X) --path /tmp/a.txt --data "hello" --json
sh0 ai file write   (--session <id> | --host X) --path /tmp/a.txt < local.txt   # --data 省略时读 stdin
sh0 ai file mkdir   (--session <id> | --host X) --path /tmp/d --json
sh0 ai file remove  (--session <id> | --host X) --path /tmp/d --dir --json
sh0 ai file rename  (--session <id> | --host X) --from /tmp/a --to /tmp/b --json
sh0 ai file chmod   (--session <id> | --host X) --path /tmp/a --mode 644 --json

# 分块搬运，二进制安全
sh0 ai file upload   (--session <id> | --host X) --local ./app.tar --remote /tmp/app.tar --json
sh0 ai file download (--session <id> | --host X) --remote /tmp/app.tar --local ./app.tar --json
```

`file read` 默认上限 1 MB，防拉爆内存。`upload/download` 同样分块，真机上 600 KB
上传下载与本地文件逐字节一致（sha256 校验通过）。

### 7.9 审计与探针

```bash
sh0 ai log query --actor ai --risk confirm --since "2026-10-20" --limit 100 --json
sh0 ai log query --host web-01 --session <id> --json
sh0 ai ping --json                # 检查 daemon 连通性
```

---

## 8. 风险引擎：三条使用路径

判定流程：shell 词法切分 → 按 `|` `;` `&&` `||` 分段 → 逐段匹配
（deny 规则 → 用户规则 → confirm 规则 → auto 白名单）→ 取最高档 → 与主机 `risk_cap` 比较。

**有人值守（TUI）**：confirm 档会弹确认框，`y` 放行、`n` 拒绝。

**无人值守（AI / 脚本）**：返回退出码 3 + `confirm_token`，带令牌二次调用闭环：

```bash
sh0 ai exec run --host web-01 --cmd "systemctl restart nginx" --json
# → exit 3，data.confirm_token = cf_01J...

sh0 ai exec run --host web-01 --cmd "systemctl restart nginx" \
    --confirm-token cf_01J... --json
```

令牌一次有效、带过期时间，且**会校验命令一致**（改一个字符就不给过）。

**想跳过询问**：`--yes` 显式批准（等价于点了确认，仍会写审计）。
拿不准某条命令会被怎么判，先 `--dry-run` 只看判定不执行。

保守默认值：未命中任何规则 → `confirm`。想放开就把 `risk.unmatched` 改成 `auto`，
但先确认 `risk.toml` 的 deny 规则覆盖了你的红线。

---

## 9. 审计与复盘

审计默认开（`audit.enabled = true`），每次执行落一条 SQLite 记录，含 actor、主机、命令、
风险档、退出码、耗时，输出可选归档（`audit.archive_output`）。

- 落库前先脱敏：口令 / token / 私钥 / `AKIA...` 等模式会被替换。
- 保留策略：默认 30 天 / 512 MB，超出从最旧记录开始淘汰。
- 直接查库（想写复杂 SQL 时）：`<data_dir>/sh0/audit.db`。
- 批量执行额外落在 `batch_run` / `batch_result` 两张表里，`ai batch list/show` 就是读它们。

---

## 10. 典型范式

```bash
# 巡检：一组机器跑同一条命令，导出 CSV
sh0 ai batch exec --group prod --cmd "df -h / | tail -1" --out csv --output ./df.csv

# 先看机器清单，再决定对哪组动手
sh0 ai host list --json
sh0 ai host list --group prod --json

# 需要连续交互（cd、环境变量、sudo 后保持）：显式开会话复用连接
SID=$(sh0 ai session open --host web-01 --json | jq -r .data.session_id)
sh0 ai exec run --session "$SID" --cmd "cd /srv/app && ls" --json
sh0 ai exec run --session "$SID" --cmd "./deploy.sh" --json     # confirm 档会要令牌
sh0 ai session close --session "$SID" --json

# 卡在交互式程序里：读屏 → 注入按键
sh0 ai screen read --session "$SID" --lines 40 --json
sh0 ai input send --session "$SID" --keys ":q!<Enter>" --json

# 改配置：先确认风险档，再执行
sh0 ai exec run --host web-01 --cmd "systemctl restart nginx" --dry-run --json
sh0 ai exec run --host web-01 --cmd "systemctl restart nginx" --yes --json

# 事后复盘
sh0 ai log query --actor ai --risk confirm --since "2026-10-20" --json
```

---

## 11. 故障排查

| 现象 | 原因与处理 |
| --- | --- |
| 复制后剪贴板没有内容 | OSC 52 需要父终端支持。不支持时内容仍在 sh0 内，用 `p` 粘回会话 |
| TUI 新建主机时密码框没反应 | 先用 `←/→` 把认证方式切到 `password`，密码字段才会出现；输入显示为 `*` |
| 密码登录报认证失败 | 编辑主机时密码留空表示"保持原密码"，新主机则必须输入；也检查是否误选了 `agent` |
| 私钥读不到 | 外部 `key_file` 路径不支持 `~` 展开，写绝对路径；TUI 导入模式会把 PEM 存进加密库，不依赖原路径 |
| 报 `vault 已锁定` | `<data_dir>/sh0/creds.vault` 存在但 `creds.key` 丢失/不匹配。从备份恢复 `creds.key`，或删掉 vault 重新录入凭据 |
| 旧版凭据不见了 | 升级后首次启动会把旧主密码 vault 迁移为 data-key；若当时没设 `SH0_MASTER_PASSWORD`，旧库被备份为 `creds.vault.legacy`，设好主密码后重启即可迁移 |
| 没法用鼠标拖选复制 | 鼠标捕获开着时父终端收不到鼠标。按一次 **Shift + 鼠标** 挂起捕获，或配置 `mouse = false` |
| 首次连不上 / 指纹变更被拒 | TOFU：首次自动记录指纹，指纹变更时拒绝连接。确认主机身份后删掉 `<data_dir>/sh0/known_hosts` 里对应行 |
| daemon 起不来 / 行为异常 | 看 `<data_dir>/sh0/run/daemon.log`；`sh0 daemon stop` 后重试（失效的 `daemon.json` 会自动清理）。没有会话时 daemon 也会在 5 分钟空闲后自退出 |
| shell 模式拿到半截输出 | 远端改写 `PS1`、`clear` 刷屏、回显折行会让哨兵退化为部分输出。切 `--mode exec` |
| `ai` 报 `ipc_unreachable` | daemon 没起来。看 `<data_dir>/sh0/run/` 与 daemon 日志；必要时 `sh0 daemon stop` 后重试 |
| 报 `session_unavailable` | 会话没了，重新 `session open`。**不要**去重启 daemon |
| 触发 `rate_limited` | 按 `data.retry_after_sec` 等待；批量执行整批只算 1 次配额 |
| Windows 上 release 构建崩 | `0xc0000005`（rustc fat LTO 访问冲突）。改用 `lto = "thin"` 或 debug 构建；磁盘空间不足时也会报同样的错 |
| Git Bash 里 `/tmp/xxx` 报 `not_found` | MSYS 会把 `/tmp` 转成 Windows Temp 路径再传给程序。用绝对路径，或 `MSYS_NO_PATHCONV=1` |
| 改了 `config.toml` 没生效 | 只有 `limits.*` / `risk.*` / 主机清单是热生效；`term`、`keepalive_secs`、`cols/rows`、`memory_budget_mb`、`audit.enabled` 要重启 daemon（`daemon.log` 会点名） |
| `Ctrl+Shift+P` 唤不出命令面板 | 部分终端不区分 `Ctrl+Shift+P` 与 `Ctrl+P`。在 `config.toml` 里覆盖：`[keys] command.palette = "alt+p"` |
| 导入后主机连不上 | 检查导入的 `addr`/`user`；ssh config 里的 `%h` 占位符与通配段会被跳过（回显里标 `-`） |
| 输入每个字符都变双份（`ls` → `llss`） | 按键的"松开"事件也被当成了一次输入。Windows 控制台每次按键会给按下 + 松开两个事件，已在事件循环过滤（`should_handle_key` 丢弃 `Release`）。若仍出现，确认用的是最新构建 |

---

## 12. 已知限制

- 复制走 OSC 52，依赖父终端支持。
- `keyboard-interactive` / 2FA 交互应答未实现（M5）。
- TUI raw mode 下无法交互式输入口令。
- 私钥路径不支持 `~` 展开。
- `shell` 模式在极端形态下可能退化为部分输出。
- `known_hosts` 采用 TOFU，指纹变更即拒绝。
- 文件视图：不支持目录递归传输与非空目录删除；不支持断点续传（均为 P2）。
- 批量执行目前只有 AI 侧入口，TUI 里的批量结果面板尚未实现。
- 鼠标已覆盖主机树 / 标签栏 / 分隔条 / 终端区 / 文件视图（见 §5.3）；还没做的只有
  **终端内的右键菜单与点击定位光标**，以及"拖完的主机树宽度写回配置"（目前只存在内存里）。
- `Ctrl+Shift+P` 命令面板已可用，但面板里目前只有内置动作 / 连主机 / 切主题三类。
- 跳板机的密钥认证若需要指定私钥，请把跳板写成 hosts.toml 里的一台正式主机；
  内联写法 `user@host:port` 固定走 agent。
- 端口转发（本地 / 远程 / SOCKS）与 MCP adapter 未实现（P2）。

---

## 13. 安全与威胁模型

M5 做了一轮安全复查，下面是**当前的防护边界**——知道它防什么、不防什么，
比"我们做了加密"更重要。

| 资产 | 怎么保护 | 边界 |
| --- | --- | --- |
| 口令 / 私钥口令 | 优先 OS 钥匙串（macOS Keychain / Windows Credential Manager / Linux Secret Service）；不可用时落到本地 data-key 加密的 `creds.vault`（`creds.key` 0600 + ChaCha20-Poly1305） | 钥匙串强度取决于 OS 登录态；vault 强度取决于 `creds.key` 的文件权限，与 `~/.ssh` 私钥同档 |
| 审计库 | 明文落 SQLite，但写入前过 `redact()` 脱敏（口令、token、Bearer、PEM 私钥、AKIA、`-p` 后的值） | 脱敏是**正则兜底**，不是保证。自定义规则请写 `risk.toml`，敏感输出尽量别落库（`audit.archive_output = false`） |
| IPC | 只监听 127.0.0.1 的随机端口 + 一次性令牌；`daemon.json` 仅本机可读 | 同机的**任何用户进程**若能读到该文件即可调用本机 daemon。多用户机器请自行收紧目录权限 |
| 主机密钥 | TOFU + OpenSSH 兼容 `known_hosts`，指纹变更直接拒绝 | 首次连接不校验指纹来源（与 OpenSSH 默认行为一致） |
| 命令风险 | 三档引擎（auto / confirm / deny）+ 主机级 `risk_cap` 上限 + 令牌确认（5 分钟过期） | `--yes` 与 AI 侧的 `auto` 档命令不会打断人 |
| 令牌文件 | 目录 0700（Unix）/ 仅当前用户（Windows） | 若目录权限被改宽，令牌会泄露 |

**复查时确认过的几条：**

- AI 侧任何路径都拿不到明文凭据：`host list/show`、`session status` 只返回认证**方式**（agent/key/keyfile/password），不返回值。
- 广播输入只落**一条**汇总审计，不逐台复制命令内容。
- 导入配置文件只**读** `~/.ssh/config`，不回写；Xshell 的加密口令字段不导入。
- 跳板机的凭据与目标的凭据各自独立解析，不会互相串用。

**还没做的（明确不在 v1.0）**：`keyboard-interactive` / 2FA 交互应答；对 daemon 的跨用户鉴权。

---

## 14. 相关文档

- `docs/requirements.md` —— 需求说明书（FR / NFR / 验收标准）
- `docs/architecture.md` —— 架构设计、AI 子命令契约、错误码表、SQLite 表结构
- `docs/development-plan.md` —— 里程碑分解与排期
- `README.md` —— 项目概览与快速开始
