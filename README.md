# sh0

轻量级、跨平台、**TUI** 的 SSH / SFTP 连接管理工具，对标 Xshell 的核心体验，
但具备两个差异化特征：

- **单二进制、跨平台**：纯 Rust，Linux / macOS / Windows 行为一致。
- **原生为 AI 设计**：所有能力以结构化子命令暴露，AI Agent 通过命令行即可操作远程主机，
  并受内置风险分级引擎约束。

## 当前进度：M0「骨架与连通」已完成

已落地的能力：

| 能力 | 说明 |
| --- | --- |
| workspace 骨架 | 8 个 crate，依赖方向受控（core 不依赖 TUI / SSH 细节） |
| 配置层 | TOML 配置 + 主机清单，自动落盘到平台配置目录 |
| SSH 连接 | russh 0.64，支持密码 / 私钥（含 passphrase）/ ssh-agent（Unix）与 Pageant（Windows） |
| 主机密钥 | known_hosts 校验 + 首次信任（TOFU），密钥变更直接拒绝 |
| daemon | 常驻进程持有全部会话，前端一律通过本机 IPC 接入 |
| IPC | loopback TCP + 一次性令牌，JSON Lines 帧格式 |
| AI 接口 | `sh0 ai host list`、`sh0 ai session list`、`sh0 ai ping`，默认纯文本 + `--json` |
| TUI | 最小终端视图：键盘 → 远端 → 回显闭环（尚未做终端仿真） |

尚未实现（后续里程碑）：终端仿真与滚动缓冲（M1）、命令执行与读屏注入（M2）、
批量与广播（M3）、SFTP（M4）、主题键位与导入导出（M5）。

## 快速开始

```bash
cargo build                       # 构建（Windows 需可用的 MSVC 工具链）

sh0 config path                   # 查看配置目录位置
sh0 tui                           # 首次运行会生成示例主机条目
# 编辑 hosts.toml 填入真实主机，然后：
sh0 tui <主机名>                  # 进入终端视图，Ctrl+Q 退出

sh0 ai host list --json           # AI 接口：列出主机
sh0 ai session list --json        # AI 接口：列出活跃会话
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
├── sh0-term/    终端仿真（M0 留空，M1 落地）
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

## 已知限制（M0）

- TUI 不做终端仿真：ANSI 转义被剥离后显示，不支持 `vim` / `htop` 这类全屏程序
- 不支持窗口 resize 同步（M1）
- `keyboard-interactive` / 2FA 与系统钥匙串未实现（M1 / M2）
- TUI raw mode 下无法交互式输入口令，请使用 `agent` 或 `env`
- 私钥路径不支持 `~` 展开，请写绝对路径（M1 补）
- 后台 daemon 的日志尚未写入文件（M1 改为 `runtime_dir/daemon.log`）
