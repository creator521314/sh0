# sh0

轻量级、跨平台、**TUI** 的 SSH / SFTP 连接管理工具，对标 Xshell 的核心体验，
但具备两个差异化特征：

- **单二进制、跨平台**：纯 Rust，Linux / macOS / Windows 行为一致。
- **原生为 AI 设计**：所有能力以结构化子命令暴露，AI Agent 通过命令行即可操作远程主机，
  并受内置风险分级引擎约束。

## 当前进度：M1「终端与会话」已完成

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
| AI 接口 | `ai host list`、`ai session open/close/list/status/reconnect/clear/resize`、`ai screen read`、`ai input send`、`ai ping`，默认纯文本 + `--json` |

尚未实现（后续里程碑）：命令执行与风险分级引擎（M2）、批量与广播（M3）、SFTP（M4）、
主题与键位自定义、导入导出（M5）。

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

sh0 daemon stop                   # 优雅停止后台 daemon
```

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
# password  = { secret = { keyring = { service = "sh0", account = "web-01" } } }  # M2 支持
```

**凭据不落明文**：配置文件只存「去哪里取」（`agent` / `key_file` / `env` / `prompt` / `keyring`），
不存口令本身。

注意 `auth` 只有两种写法：单元变体直接写字符串（`auth = "agent"`），
结构化变体写成嵌套表（`[hosts.auth]` 下填 `key_file = {...}` 或 `password = {...}`）。
`Password` 的字段名是 `secret`，别漏掉这一层。

## 目录结构

```
crates/
├── sh0-core/    领域模型：Host / Credential / SessionState / RiskLevel
├── sh0-ssh/     russh 封装（唯一接触 SSH 协议的地方）
├── sh0-term/    终端仿真：vte 解析 + 网格 + 滚动缓冲 + 按键编码
├── sh0-store/   TOML 配置与平台路径（SQLite 审计在 M2）
├── sh0-ipc/     本机 IPC 协议与传输
├── sh0-ai/      `sh0 ai` 子命令层
├── sh0-tui/     ratatui 前端
└── sh0-cli/     二进制入口与 daemon
docs/
├── requirements.md        需求说明书
├── architecture.md        架构设计与 AI 接口契约
└── development-plan.md    开发计划与里程碑
```

## 文档

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
| Alt+G / Ctrl+PageUp | 回到顶部 |
| Alt+V | 进入/退出文本选择模式 |
| Alt+C | 复制选区 |
| Alt+R | 手动重连 |
| Ctrl+L | 清屏 |

选择模式内：方向键或 hjkl 扩选，Enter / y 复制，p 粘贴回会话，Esc 取消。
键位可在 `config.toml` 里改，`keymap = "vim"` / `"xshell"` 切换预设。

## 已知限制（M1）

- 复制走 OSC 52，需要父终端支持（Windows Terminal / iTerm2 / kitty / Alacritty 均支持）；
  不支持时内容仍会留在 `sh0` 内部，可用 `p` 粘回会话
- `keyboard-interactive` / 2FA 与系统钥匙串未实现（M2）
- TUI raw mode 下无法交互式输入口令，请使用 `agent` 或 `env`
- 私钥路径不支持 `~` 展开，请写绝对路径
- 审计日志与输出归档未落盘（M2 的 SQLite）
- 尚未在真实 SSH 服务端做过端到端验证（开发机上没有可用 SSH 服务），
  终端仿真与重连是用 mock 传输层 + 23 条 golden 用例覆盖的
