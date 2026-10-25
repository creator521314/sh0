# sh0

轻量级、跨平台、**TUI** 的 SSH / SFTP 连接管理工具，对标 Xshell 的核心体验，
但具备两个差异化特征：

- **单二进制、跨平台**：纯 Rust，Linux / macOS / Windows 行为一致。
- **原生为 AI 设计**：所有能力以结构化子命令暴露，AI Agent 通过命令行即可操作远程主机，
  并受内置风险分级引擎约束。

## 当前进度：M3「批量与广播」+ M4「SFTP 落地」已完成 ★

M2 让人类能用 TUI 运维、AI 能用 CLI 操作；**M4 补上了文件这一半**（双栏管理、
传输队列、`ai file *`），**M3 则从"单台"走到"一组机器"**——广播输入与批量执行。

| 能力 | 说明 |
| --- | --- |
| 终端仿真 | `vte` 解析器 + 字符网格（256 色 / truecolor / 粗斜体下划线反显），CJK 双宽与 emoji、组合字符 |
| 滚动缓冲 | 默认 10000 行环形缓冲 + 全局内存预算淘汰，PageUp / PageDown 回看 |
| TUI 布局 | 左侧主机树 + 顶部标签栏 + 主区终端 + 底部状态栏，主题配色 |
| 主机管理 | 增删改查表单、分组折叠、模糊搜索，写回 `hosts.toml` |
| 会话状态机 | connecting / ready / reconnecting / closed，断线检测 + 指数退避重连（带抖动），重连保留历史输出并插入分隔线 |
| 保活 | SSH keepalive，间隔可在配置里改 |
| resize | 窗口变化同步到远端（`window_change`），本地网格同步重建 |
| 按键编码 | 方向键 / F1-F12 / Ctrl-Alt 组合编码，键位映射三套预设（default / vim / xshell） |
| 文本选择与剪贴板 | 选择模式（Alt+V）方向键扩选，Enter/y 复制，OSC 52 写父终端剪贴板 |
| AI 接口 | `ai host list/show`、`ai session open/close/list/status/reconnect`、`ai exec run`、`ai screen read`、`ai input send`、`ai log query`、`ai batch exec/list/show`、`ai broadcast enable/disable/status/send`、`ai ping`，默认纯文本 + `--json`，12 类语义化退出码 |
| 命令执行 | 双模式：`shell`（默认，复用交互 shell + 哨兵截取）/ `exec`（独立通道，stdout/stderr 分离）；一键式 `--host X --cmd Y` 自动建连→执行→关闭 |
| 风险引擎 | 三档 `auto / confirm / deny`：shell 词法切分 + 分段判定 + 内置规则集 + 用户 `risk.toml` + 主机 `risk_cap`；默认档是 `confirm`（宁拦勿放） |
| 确认机制 | 有人值守：TUI 弹确认框（y / n）；无人值守：返回 `needs_confirmation` + 一次性令牌，带令牌二次调用闭环 |
| 审计 | SQLite 单文件，脱敏后落库（口令 / token / 私钥 / AKIA），输出可归档，默认保留 30 天 / 512 MB |
| 凭据 | OS 钥匙串优先（Keychain / 凭据管理器 / Secret Service），回落主密码 vault（Argon2id + ChaCha20-Poly1305）；配置与数据目录**无明文** |
| 双栏文件管理 | `F9` 切到文件视图：左本机 / 右远端，各自独立 cwd、排序与选中行；`Space` 标记多项、`F5` 传走、`F6` 改名、`F7` 建目录、`F8` 删除、`p` 改权限、`g` 跳路径。远端列目录复用会话已有的 SSH 连接，不新开 TCP |
| 传输队列 | 每帧推进 256 KB 的块，底部面板显示进度条 / 速度 / 失败原因；单块失败自动重试 3 次（`R` 重排、`c` 清理）。二进制自动 base64，逐字节一致 |
| AI 文件接口 | `ai file list/stat/read/write/mkdir/rm/rename/chmod/upload/download`，`--session`（复用连接）与 `--host`（一键式）两种用法；`upload/download` 同样分块搬运 |
| 鼠标操作（主机树） | 单击选中主机并把焦点给树、双击连接、单击分组行展开折叠、滚轮移动选择；`Shift + 鼠标` 把鼠标交回父终端，保住系统自带的拖选复制。配置 `mouse = false` 可完全关闭 |
| 广播输入 | `Alt+B` 开广播、`Alt+M` 逐台勾选；开着时终端输入同时发到所有参与会话，状态栏常驻 `广播×N`、参与的标签带 `*`。AI 侧 `ai broadcast enable/disable/status/send` |
| 批量执行 | `ai batch exec (--hosts a,b \| --group G \| --tag T \| --all) --cmd C`：并发池（默认 16）+ 逐台超时，`--fail-fast` 提前收尾；结果汇总支持 `--out table/json/csv` 与 `--output` 导出。`ai batch list/show` 回看历史 |
| 批量风险与配额 | 整批只判一次风险（多台 `risk_cap` 取最严）、只扣一次速率配额；confirm 档签发批量令牌，带令牌重放校验命令一致 |

尚未实现（后续里程碑）：主题与键位自定义、导入导出、`keyboard-interactive` / 2FA（M5）；
文件侧还差目录递归传输与断点续传（P2）；批量暂未提供 TUI 结果面板（当前走 AI 侧汇总）；
鼠标目前只覆盖左侧主机树，点击标签栏与拖拽分隔条仍在 M5。

## 发布

| 标签 | 内容 |
| --- | --- |
| v0.1 | M0~M2：终端、会话、AI 接口、风险引擎、审计。已在真实 Debian / aarch64 主机上端到端验证 |
| **v0.2（当前工作区）** | M3 + M4：批量与广播（`ai batch exec` + `ai broadcast *`）、SFTP 文件管理（双栏 UI + 传输队列 + `ai file *`）。112 条测试 + clippy 零警告；M4 已在真机（Debian 13 / aarch64）跑通 600 KB 分块上传下载逐字节一致 |

构建：`cargo build --release`。跨平台产物与 CI 矩阵属 M5（W5.4），v0.1 阶段只保证
Windows / macOS / Linux 能从源码构建。

## 快速开始

```bash
cargo build                       # 构建（Windows 需可用的 MSVC 工具链）

sh0 config path                   # 查看配置目录位置
sh0 tui                           # 首次运行会生成示例主机条目
# 编辑 hosts.toml 填入真实主机，然后：
sh0 tui <主机名>                  # 进入终端视图，Ctrl+Q 退出

sh0 ai host list --json           # AI 接口：列出主机
sh0 ai session open --host web-01 --json
sh0 ai session list --json        # 列出活跃会话（含 state 与 last_error）
sh0 ai screen read --session <id> --json
sh0 ai input send --session <id> --data "uptime" --enter --json
sh0 ai ping --json                # 检查 daemon 连通性

# 命令执行：一键式（自动建连→执行→关闭），返回 stdout/stderr/exit_code/duration_ms
sh0 ai exec run --host web-01 --cmd "uname -a" --timeout 10 --json
sh0 ai exec run --host web-01 --cmd "df -h /" --mode exec --json   # 独立通道，流分离

# 风险三档：deny 直接拦；confirm 无人值守返回 needs_confirmation + 令牌
sh0 ai exec run --host web-01 --cmd "rm -rf /" --json              # 退出码 2，一律拒绝
TOKEN=$(sh0 ai exec run --host web-01 --cmd "systemctl restart nginx" --json \
        | jq -r .data.confirm_token)
sh0 ai exec run --host web-01 --cmd "systemctl restart nginx" --confirm-token "$TOKEN" --json

sh0 ai log query --actor ai --risk confirm --since "2026-10-20" --json  # 审计回溯

# 批量：一组机器同一条命令，汇总可导出
sh0 ai batch exec --group prod --cmd "df -h / | tail -1" --out csv --output ./df.csv
sh0 ai batch list                 # 回看历史；sh0 ai batch show --run-id <id>

# 广播：开会话 → 开广播 → 一份输入发给所有参与会话
sh0 ai broadcast enable --session <id> --session <id2> --json
sh0 ai broadcast send --text "uptime" --enter --json
sh0 ai broadcast disable --json

# 文件：--session 复用连接，--host 一键式；upload/download 分块、二进制安全
sh0 ai file list --session <id> --path /var/log --json
sh0 ai file upload --host web-01 --local ./app.tar --remote /tmp/app.tar --json

printf '%s' "$PW" | sh0 secret set sh0 web-01 --stdin                   # 存凭据（无明文）

sh0 daemon stop                   # 优雅停止后台 daemon
```

完整的 TUI 键位、AI 子命令参数、配置字段与故障排查见 **`docs/usage.md`**。

配置目录：

| 平台 | 路径 |
| --- | --- |
| Linux | `~/.config/sh0/` |
| macOS | `~/Library/Application Support/sh0/` |
| Windows | `%APPDATA%\sh0\` |

## 主机配置示例（`hosts.toml`）

```toml
version = 1

[[hosts]]
id = "01M5N06QYP6GW5W2JK73XVCH38"
name = "web-01"
group = ["prod", "web"]
addr = "10.0.0.11"
port = 22
user = "root"
auth = "agent"                    # agent | { key_file = {...} } | { password = {...} }
tags = ["nginx"]
note = "主站"

[[hosts]]
name = "db-01"
group = ["prod", "db"]
addr = "10.0.0.21"
port = 22
user = "dba"

[hosts.auth]
key_file = { path = "/home/u/.ssh/id_ed25519" }    # 无 passphrase
# key_file = { path = "/home/u/.ssh/id_rsa", passphrase = { env = "SH0_KEY_PW" } }
# password  = { secret = { env = "SH0_PW" } }      # 从环境变量取
# password  = { secret = "prompt" }                # 运行时询问（不适用 TUI raw mode）
# password  = { secret = { keyring = { service = "sh0", account = "web-01" } } }  # 钥匙串（推荐）
```

**凭据不落明文**：配置文件只存「去哪里取」（`agent` / `key_file` / `env` / `prompt` / `keyring`），
不存口令本身。

注意 `auth` 只有两种写法：单元变体直接写字符串（`auth = "agent"`），
结构化变体写成嵌套表（`[hosts.auth]` 下填 `key_file = {...}` 或 `password = {...}`）。
`Password` 的字段名是 `secret`，别漏掉这一层。

## 目录结构

```
crates/
├── sh0-core/    领域模型：Host / Credential / SessionState / RiskLevel / 风险引擎 / SecretResolver
├── sh0-ssh/     russh 封装（唯一接触 SSH 协议的地方）
├── sh0-term/    终端仿真：vte 解析 + 网格 + 滚动缓冲 + 按键编码
├── sh0-store/   TOML 配置 + SQLite 审计 + 凭据（钥匙串 / vault）+ 平台路径
├── sh0-ipc/     本机 IPC 协议与传输（loopback TCP + 令牌）
├── sh0-ai/      `sh0 ai` 子命令层（双渲染器 + 退出码映射）
├── sh0-tui/     ratatui 前端（含风险确认弹窗）
└── sh0-cli/     二进制入口、daemon、exec 编排（哨兵截取 / 确认 / 审计 / 限速）
docs/
├── usage.md               使用说明（TUI / AI 接口 / 配置 / 排查）
├── requirements.md        需求说明书
├── architecture.md        架构设计与 AI 接口契约
└── development-plan.md    开发计划与里程碑
```

## 文档

- `docs/usage.md` —— **使用说明**：安装、TUI 键位与广播、AI 全套子命令、配置与凭据、
  风险与审计、故障排查
- `docs/requirements.md` —— 需求说明书（FR / NFR / 验收标准 / 待定问题）
- `docs/architecture.md` —— 架构设计、AI 子命令契约、错误码表
- `docs/development-plan.md` —— 里程碑分解、工时估算、三条排期路线

## 键盘（default 预设）

| 按键 | 作用 |
| --- | --- |
| Ctrl+Q | 退出 |
| Ctrl+T / Ctrl+W | 新建 / 关闭会话 |
| Alt+← / Alt+→ | 切换会话 |
| Ctrl+B | 焦点在主机树与终端间切换 |
| Ctrl+F | 搜索主机 |
| Ctrl+N | 新建主机 |
| PageUp / PageDown | 回看历史输出 |
| Ctrl+PageUp / Ctrl+PageDown | 回到顶部 / 回到底部（vim 预设为 Alt+G / Shift+G） |
| Alt+V | 进入/退出文本选择模式 |
| Alt+C | 复制选区 |
| Alt+R | 手动重连 |
| Ctrl+L | 清屏 |
| Alt+B | 广播开关（开着时输入同时发给所有参与会话） |
| Alt+M | 勾选/取消当前会话参与广播 |
| F9 | 终端视图 / 双栏文件视图切换 |

选择模式内：方向键或 hjkl 扩选，Enter / y 复制，p 粘贴回会话，Esc 取消。

文件视图内（`F9` 进入，左本机 / 右远端）：`Tab` 切栏，`↑↓` 或 `jk` 选择，
`Enter` / `→` 进入目录，`Backspace` / `←` 上级，`g` 输入路径跳转，`s` 切换排序键，
`r` 刷新，`Esc` 回终端视图。远端列目录复用当前会话已建立的 SSH 连接，不新开 TCP。

鼠标（默认开，配置 `mouse = false` 关闭）：主机树里**单击选中、双击连接、单击分组行展开折叠、
滚轮上下移动**；`Shift + 鼠标` 把鼠标交回父终端（挂起捕获，按任意键恢复），系统自带的拖选复制
照旧可用。

文件操作：`Space` 标记多项，`F5` 把标记项（或当前项）传向对面栏，`F6` 改名，
`F7` 建目录，`F8` / `Delete` 删除（需确认），`p` 改权限（输入八进制），`r` 刷新。
传输走队列：每帧推进 256 KB，底部面板显示进度、速度与失败原因，单块失败自动重试 3 次
（`R` 重新排队、`c` 清理已完成）。二进制自动 base64。目录递归传输暂不支持。
键位可在 `config.toml` 里改，`keymap = "vim"` / `"xshell"` 切换预设。

## 已知限制

- 复制走 OSC 52，需要父终端支持（Windows Terminal / iTerm2 / kitty / Alacritty 均支持）；
  不支持时内容仍会留在 `sh0` 内部，可用 `p` 粘回会话
- `keyboard-interactive` / 2FA 交互应答未实现（M5）
- TUI raw mode 下无法交互式输入口令，请使用 `agent` / `env`，或 `sh0 secret set` 预存
- 私钥路径不支持 `~` 展开，请写绝对路径
- 已在真实环境（Debian / aarch64 树莓派，密码认证）跑通端到端一轮：会话、双模式 exec、
  屏幕读取、按键注入、风险三档、确认闭环、审计回溯全部验证通过（2026-10-24）
- `exec` 的 shell 模式依赖哨兵回显判断命令结束：远端改写 `PS1`、`clear` 刷屏、
  回显折行等极端形态下可能退化为部分输出（此时 `timeout=true`、退出码为 `null`），
  可切 `--mode exec` 规避
- `known_hosts` 采用 TOFU：首次连接自动记录指纹，指纹变更时拒绝连接
- 文件视图不支持目录递归传输与非空目录删除（要带进度的递归，属于传输队列的下一轮）
- M4 的分块读写（`file.read_range` / `file.write_at`）已在真机（OpenSSH / Debian 13 aarch64）
  跑通：600 KB 分 3 块上传下载，与本地文件逐字节一致
- 批量执行目前只有 AI 侧入口（`ai batch exec`），TUI 里的批量结果面板尚未实现；
  批量默认走 exec channel，`--mode shell` 会为每台临时建会话（不进会话表）
