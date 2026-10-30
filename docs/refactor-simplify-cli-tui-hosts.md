# sh0 简化重构方案：CLI 收敛 + TUI 主机管理与凭据本地加密

> 版本：v0.4 设计稿（重构目标，非现状描述）
> 面向：先由 deepseek-pro 评审/细化，再由 deepseek-flash 落地开发。
> 本文描述「目标状态」与「改动路径」；现状的代码位置均已标注，改动以当前工作区
> （`crates/` 下 sh0-core / sh0-store / sh0-ipc / sh0-ssh / sh0-tui / sh0-ai / sh0-cli）为准。

---

## 实施状态（v0.4，2026-10-30）

本文的 P0–P5 已全部落地，`cargo test --workspace` 171 条通过、clippy 零警告。
落地时与设计稿的少量偏差，均已按「体验不变、实现更简」处理：

- **表单形态**：TUI 用**单张条件表单**（新建默认 `password`，认证方式用 `←/→` 切换，password/key 字段按需显示），
  而不是设计稿里的多步模态；没有做「密码二次确认」。`Enter` 保存并连接、`Ctrl+S` 仅保存。
- **daemon 命令**：评审后取消了「只留三个命令」的约定，`daemon` 作为可见运维命令（run/stop）暴露；`sh0 help` 同时说明它的作用与日志；正常路径仍靠
  TUI 每 30s ping + daemon 5 分钟空闲自退出，不再需要手动停。
- **配置入口**：`sh0 config` 已删除；`sh0 help` 打印真实目录，配置改文件 + 2s 热重载。
- **凭据 account**：新写入用 `<host.id>` / `<host.id>.key` / `<host.id>.key.pass`；
  旧 `keyring` 引用（含 `account = <name>`）保持可读。
- **私钥导入**：TUI 的 key 模式会把 PEM 读入加密库（`CredentialRef::Key`）；
  手写 `hosts.toml` 仍可用 `key_file` 引用外部路径。

---


## 0. 目标与非目标

### 0.1 目标（用户诉求的转译）

1. **CLI 收敛**：`sh0` 只保留三个用户可见入口 —— `tui`、`ai`、`help`（外加 clap 自带的
   `--help` / `--version`）。现有顶层 `daemon` / `host` / `secret` / `config` 四个子命令
   退出用户视野。
2. **TUI 承担主机管理**：在 TUI 里完成主机的创建 / 更新 / 删除 / 连接，并支持「新建即连接」。
3. **凭据就地录入 + 本地加密**：新建/编辑主机时，直接在 TUI 里输入密码、私钥（认证文件）及
   私钥口令；密码与私钥**加密后落盘**，后续连接无感取用，不再要求用户手改 `hosts.toml` 或
   先跑 `sh0 secret set`。
4. **`ai` 保持不变**：`sh0 ai …` 的全部子命令、JSON 信封、退出码语义、风险/审计行为一律不动。
   它只读、不写凭据（维持「改配置属于人类运维动作」的边界）。

### 0.2 非目标（本次不做）

- keyboard-interactive / 2FA 交互应答（仍缺，见 [client.rs:389](../crates/sh0-ssh/src/client.rs#L389)）。
- 端口转发、MCP adapter、目录递归传输、断点续传（既有 P2 项，与本次无关）。
- TUI 内「设置面板」可视化编辑 `config.toml` 全部字段（`config.toml` 仍走文件 + 热重载）。
- 把凭据写入能力暴露给 `ai`（明确禁止）。

---

## 1. 现状速览（只列与本文相关的部分）

- 顶层命令定义在 [cli.rs](../crates/sh0-cli/src/cli.rs)，当前是 6 个：`daemon / tui / ai /
  host / secret / config`。
- 主机模型在 [model.rs](../crates/sh0-core/src/model.rs)：
  - `Host { id, name, group, addr, port, user, auth, tags, note, risk_cap, proxy_jump, jump }`，
    必填是 `id / name / addr / user`，`port` 默认 22，`auth` 默认 `agent`。
  - `CredentialRef` 现有：`Agent / KeyFile{path,passphrase} / Password{secret} / Interactive / None`。
  - `SecretRef` 现有：`Prompt / Env / Keyring{service,account}`。
- 凭据存储 [creds.rs](../crates/sh0-store/src/creds.rs)：钥匙串优先，回落「主密码 Argon2id +
  ChaCha20-Poly1305 vault」。主密码来自 `SH0_MASTER_PASSWORD` 环境变量或 stdin 提示。
- TUI 已具备主机树的增/删/改/连（[hosts.rs](../crates/sh0-tui/src/hosts.rs) 的 `HostForm`、
  [app.rs](../crates/sh0-tui/src/app.rs) 里 `n / e / d / Enter` 及命令面板动作），**但**：
  - `HostForm::to_host` 里 `password` 写死成 `SecretRef::Prompt`（[hosts.rs:252](../crates/sh0-tui/src/hosts.rs#L252)），
    而连接发生在后台 daemon（stdin 为 null，[ensure.rs:43](../crates/sh0-cli/src/ensure.rs#L43)），
    所以「密码登录」在默认路径下不可用 —— 这是当前最大的 UX 断点。
  - 私钥只支持「外部路径引用」，不加密、不导入。
- IPC 方法（[protocol.rs](../crates/sh0-ipc/src/protocol.rs)）已有 `session.* / exec.* / file.* /
  batch.* / broadcast.* / config.reload / config.get`，**没有凭据读写方法**。
- `ai host list/show/import` 在 CLI 进程内直接读/写 `hosts.toml`（不经 daemon），
  而 `session.open` 走 daemon 且每次 `find_host` 现读文件 —— 因此「TUI 直接改 hosts.toml +
  daemon 现读」的写路径天然一致，无需额外同步。

---

## 2. 目标 CLI 表面

### 2.1 命令树

```
sh0                  # 无子命令 → 打印 help（等价 sh0 help），退出码 0
sh0 tui [host]       # host 可选：名称或 id；省略进入主机树选择
sh0 ai <object> <cmd> [flags]   # 与现状完全一致
sh0 help             # 打印人类可读的简洁用法 + 配置/数据目录 + 常见故障入口
sh0 daemon run [--background]   # 可见运维命令：前台运行 / 由 ensure 后台拉起
sh0 daemon stop                 # 可见运维命令：优雅停止
sh0 --help / -h      # clap 自动生成的完整帮助
sh0 --version / -V
```

> 注：初版方案要求只保留 `tui / ai / help`，评审后取消了该约定，
> `daemon` 作为可见运维命令暴露（run/stop），不再隐藏。

### 2.2 各子命令的去向

| 现状命令 | 去向 |
| --- | --- |
| `sh0 daemon run --background` | 保留为子命令，仅供 `ensure` 内部 re-exec；`daemon` 整体作为可见运维命令（run/stop）暴露 |
| `sh0 daemon stop` | 保留为可见运维命令。daemon 同时保留**空闲自动退出**（见 §6.5），日常无需手动停 |
| `sh0 host list` | 由 `sh0 ai host list` 覆盖（已存在，保持） |
| `sh0 host import` | 由 `sh0 ai host import` 覆盖（已存在，保持） |
| `sh0 secret set/get/list/rm` | 删除；能力迁入 TUI（§6） |
| `sh0 config path` | 并入 `sh0 help`（运行时打印真实路径） |
| `sh0 config show/get/set` | 删除 CLI 入口；`config.toml` 仍文件编辑 + 2s 热重载 |
| `sh0 config reload` | 删除 CLI 入口；改文件自动热重载，无需手动 |
| `sh0 config check` | 删除 CLI 入口；任意 `sh0` 调用启动时都会解析配置，出错即报（等效校验） |

> 说明：`ai host import` 与 `sh0 host import` 是同一实现（`sh0_store::import`），删除 CLI 侧
> `Host` 枚举只砍掉了一层壳，`ai` 侧能力无损。

### 2.3 `help` 输出内容（建议）

```text
sh0 —— 轻量级跨平台 TUI SSH/SFTP 连接管理工具

用法：
  sh0 tui [主机名|id]        进入图形界面（不填则从主机树选择）
  sh0 ai <对象> <命令> ...   面向 AI / 脚本的结构化接口
  sh0 help                   本帮助
  sh0 ai --help              列出 ai 全部子命令

配置目录：  <config_dir>     (hosts.toml / config.toml / risk.toml)
数据目录：  <data_dir>       (known_hosts / creds.* / audit.db)
日志目录：  <data_dir>/run/  (daemon.log)

快速开始：
  1) sh0 tui                在界面里按 n 新建主机，输入地址/用户/密码即可连接
  2) sh0 ai host import --from ssh   从 ~/.ssh/config 导入既有主机
  3) sh0 ai exec run --host web-01 --cmd "uname -a" --json

故障排查：查看 <data_dir>/run/daemon.log；凭据位于 <data_dir>/creds.vault（本地加密）。
```

路径由 `Paths::discover()` 运行时计算（[paths.rs](../crates/sh0-store/src/paths.rs)），
`help` **不拉起 daemon**。

---

## 3. 核心决策：凭据「本地加密」怎么做

### 3.1 问题

当前 vault 回落路径需要**主密码**（`SH0_MASTER_PASSWORD` 或 stdin），而后台 daemon 没有 stdin。
这与「在 TUI 里输一次密码、之后无感重连」的目标冲突：要么主密码永远驻留环境变量（不现实），
要么每次重连都要人来输（破坏体验）。

### 3.2 方案：数据密钥（data-key）vault

- 首次使用生成 **32 字节随机 data-key**，存 `<data_dir>/creds.key`，权限 `0600`
  （仅本机当前用户可读写；Windows 依赖用户 profile ACL，后续可选 DPAPI 加固）。
- `<data_dir>/creds.vault` 用 `ChaCha20-Poly1305` 以 data-key 加密，每次保存随机 nonce。
- **去掉默认的 Argon2id 主密码流程**（Argon2id 相关依赖可保留为后续「可选口令加固」的钩子，
  不在本次实现）。
- 信任模型 = 与 `~/.ssh/id_*` 私钥、`known_hosts` 同档：**防其他用户 / 备份泄漏**，不防
  本机 OS 账户本身。这是对「密钥本地加密保存」的诚实实现。

结构示意（新 vault 格式 v2）：

```json
{ "version": 2, "nonce": "<hex>", "data": "<hex-密文>" }
```

解密流程：读 `creds.key` → 作为 ChaCha20 密钥 → 解密 `data` → 得 `service/account -> value`
映射（与现状 `Entries` 一致）。

### 3.3 写入通道：以 daemon 为唯一写者

`CredentialStore` 的 vault 条目是**内存态**（`Loaded`）。若 TUI 进程自己开一个 store 写盘，
daemon 进程里的内存副本不会自动更新。因此：

- 凭据的**读写统一走 daemon**：新增 IPC 方法 `credential.set / credential.delete /
  credential.status`（§5）。
- daemon 持有唯一 `CredentialStore` 实例（`Arc<CredentialStore>`），同时继续把它注入
  `VaultResolver` 供 SSH 认证取秘密。
- TUI 只发 IPC 请求，不直接碰 `creds.vault` / `creds.key`。

### 3.4 旧 vault 迁移

现网 `<data_dir>/creds.vault` 可能是「v1 主密码格式」（带 `salt/m_cost/t_cost/p_cost`）。
迁移策略：

1. `CredentialStore::open` 检测版本：
   - 无 `creds.key` 且存在 v1 vault：
     - 若设置了 `SH0_MASTER_PASSWORD` → 用其解密旧 vault，生成 data-key，改写为 v2，删除旧主密码依赖。
     - 否则 → 将旧文件重命名为 `creds.vault.legacy` 备份，新建空 v2 vault，并 `tracing::warn!`
       提示「旧凭据未迁移，需原主密码后手动迁移」。
   - 有 `creds.key` 且 vault 是 v2 → 直接加载。
2. 提供一次性内部工具（可放在 `help` 提示里说明）：设置 `SH0_MASTER_PASSWORD` 后执行任意
   `sh0` 命令即自动完成迁移；或未来在 TUI 里提供一个「导入旧凭据」入口（P2，非必须）。

> 说明：旧条目里若已用 OS 钥匙串（`keyring` 后端），不受影响——钥匙串路径本就不走 vault。

---

## 4. 数据模型变更

### 4.1 `Host`：补默认 id（顺手修复文档不一致）

现状文档写「id 省略会自动生成」，但 `model.rs` 的 `Host.id` 没有 `serde(default)`，实测手写
省略会报 `missing field id`。本次顺带修复，使「手改 hosts.toml 也能省略 id」：

```rust
#[serde(default = "sh0_core::new_host_id")]
pub id: HostId,
```

（TUI 表单仍显式生成 ULID，行为不变；此改动只惠及手写场景，与 §0 不冲突。）

### 4.2 `CredentialRef`：新增「存储型私钥」

私钥的「本地加密保存」需要支持「PEM 字节在加密库内」，而非仅引用外部路径：

```rust
pub enum CredentialRef {
    Agent,
    /// 外部私钥文件（保持兼容，不导入、不代管）
    KeyFile { path: PathBuf, #[serde(default)] passphrase: Option<SecretRef> },
    /// 新增：PEM 私钥已导入加密库，path 无关（本次推荐默认）
    Key { secret: SecretRef, #[serde(default)] passphrase: Option<SecretRef> },
    Password { secret: SecretRef },
    Interactive,
    None,
}
```

- `Key.secret` 指向存储 PEM 的 `SecretRef::Keyring { service, account }`；
  `passphrase` 可选，指向存储私钥口令的另一个 `SecretRef`。
- `sh0-ssh` 认证逻辑对 `Key` 变体：先经 `SecretResolver` 取 PEM 字节与口令，再用 russh 的
  「从字节解析私钥」入口（对应现在 `load_secret_key(path, pass)` 的字节版，实现时核对签名）
  构造 `PrivateKeyWithHashAlg`。现有 `KeyFile` 分支保持不变。

TOML 形态（存储型私钥）：

```toml
[hosts.auth]
key = {
  secret = { keyring = { service = "sh0", account = "<host.id>.key" } },
  passphrase = { keyring = { service = "sh0", account = "<host.id>.key.pass" } },
}
```

### 4.3 account 命名约定

| 凭据 | `service` | `account` |
| --- | --- | --- |
| 密码 | `sh0` | `<host.id>` |
| 私钥 PEM | `sh0` | `<host.id>.key` |
| 私钥口令 | `sh0` | `<host.id>.key.pass` |

- 用 `host.id`（ULID）而非 `name`：主机改名不破坏引用，也避免重名歧义。
- **兼容**：现存 `hosts.toml` 里 `keyring { service="sh0", account="<name>" }` 的旧引用继续可读，
  不强制迁移（§3.4 只处理 vault 格式，不重写引用）。

### 4.4 `ai host show` 的 `auth` 种类映射

新增一个映射值以区分两类私钥（[sh0-ai/src/lib.rs](../crates/sh0-ai/src/lib.rs) 的 `auth_kind`）：

```
Agent -> "agent"
KeyFile{..} -> "keyfile"     # 外部路径
Key{..}     -> "key"         # 存储型（新增）
Password{..} -> "password"
Interactive -> "interactive"
None -> "none"
```

`ai host list` 的空清单提示文案从「可用编辑器编辑 hosts.toml 添加」改为「在 `sh0 tui` 里新建主机」。

---

## 5. IPC 变更

在 [protocol.rs](../crates/sh0-ipc/src/protocol.rs) 的 `methods` 增加（凭据能力仅 TUI 调用，
`ai` 侧不暴露）：

```
credential.set    参数 { service, account, value }         -> { backend: "keyring"|"vault" }
credential.delete 参数 { service, account }                -> { removed: bool }
credential.status 参数 {}                                  -> { keyring_available: bool, vault_ready: bool, backend: str }
```

daemon 侧 handler（[daemon.rs](../crates/sh0-cli/src/daemon.rs) 的 `Handler::call`）：

- `credential.set` → `self.creds.set(...)`；**绝不回显 value，绝不写审计明文**（必要时只记
  `actor=tui, action=credential.set, account=<account>`，不含值）。
- `credential.delete` → `self.creds.delete(...)`。
- `credential.status` → 新增 `CredentialStore::status()` 返回 `{ keyring_available, vault_ready }`。
  - `vault_ready = !is_locked()`；`keyring_available` 由内部 `keyring_ok` 探测导出。

为此 `DaemonState` 增加字段 `creds: Option<Arc<CredentialStore>>`，`DaemonState::new` 增加
参数；`run_foreground` 把已打开的 `creds` 同时传给 state 与 factory（[daemon.rs:2452](../crates/sh0-cli/src/daemon.rs#L2452) 一带）。
`run_for_test` 传 `None`（契约测试不碰凭据，或按需注入临时 store）。

---

## 6. TUI 变更

### 6.1 主机表单重构

将 [hosts.rs](../crates/sh0-tui/src/hosts.rs) 的 `HostForm` 从「认证=自由文本」改为「分步模态」：

字段顺序（新建）：

1. 名称（必填，校验全清单唯一）
2. 分组（`/` 分隔，可空）
3. 地址（必填）
4. 端口（默认 22）
5. 用户（必填）
6. 认证方式：单选 `agent / password / key / none`（默认 agent）
7. 认证文件路径：仅 `key` 时显示（可空 = 外部路径模式；填了 = 导入模式）
8. 跳板机（可空）

凭据录入子步骤（认证方式选 `password` 或 `key` 后进入）：

- `password`：掩码输入密码 + 二次确认；`Esc` 取消、`Enter` 确认。
- `key`：填路径后「导入并加密」；若私钥有口令，追加一次掩码输入。导入动作 = 读文件字节 →
  IPC `credential.set("<id>.key", pem)` +（口令）`credential.set("<id>.key.pass", pass)`。
- `agent` / `none`：无凭据子步骤。

提交动作：

- `Enter` = **保存并连接**（写 hosts.toml + 写凭据 → `open(name)`）。
- `Ctrl+S`（或 `F2`）= 仅保存。

`HostForm::to_host` 的输出改为：返回 `(Host, Vec<CredWrite>)`，其中 `CredWrite =
{ account, value }` 由 app 层在保存后经 IPC 下发；认证引用按 §4.3 生成。

### 6.2 编辑 / 删除 / 连接

- 编辑（`e`）：预填现有字段。凭据子步骤：
  - 密码/口令留空 = **保留旧凭据**（不下发 `credential.set`）。
  - 输入新值 = 覆盖（`credential.set` 同 account）。
  - 切换认证方式（如 password→key）= 按新方式采集，并 `credential.delete` 旧 account。
- 删除（`d`）：确认后删除主机，并级联 `credential.delete` 该主机相关 account（密码 / `.key` /
  `.key.pass`），避免孤儿秘密。
- 连接：现状 `Enter`（树内）/ 命令面板「连接 X」不变，直接 `open(name)`。

### 6.3 掩码输入

新增一个 `SecretInput` 交互态（与现有 `query` / `FilePrompt` 同级的模态）：字符回显 `*`，
支持 `Backspace`，`Enter` 提交，`Esc` 取消；不回写 shell 历史、不落日志、不进入 screen 缓冲。
渲染层在状态栏/输入区绘制占位符即可（ratatui 无内建 password 组件）。

### 6.4 命令面板动作

[palette.rs](../crates/sh0-tui/src/palette.rs) 的 `ACTIONS` 追加：

- `HostNew` 文案改为「新建主机并连接」（体现「新建连接」）。
- 新增 `HostImport`（可选，P2）：面板内触发 `ai host import --from ssh` 的等价能力，让
  「从 ~/.ssh/config 导入」也能在 TUI 里一键完成。

### 6.5 daemon 生命周期简化

即使 `daemon stop` 已作为可见命令，日常也不需要手动停：

- `run_foreground` 增加**空闲自退出**：连续 N 秒（建议 300s，可配置，P2）无活跃会话且无
  未完成请求时，优雅 `shutdown`；有活跃会话时永不退出。
- TUI 的 `Quit` 动作：`Ctrl+Q` 退出 TUI 即可，daemon 留驻并按空闲策略回收（避免每次进 TUI
  都冷启动）。若要「退出即停 daemon」，作为可选动作放在命令面板（P2）。

---

## 7. 分层改动清单（按 crate）

### sh0-store

- [paths.rs](../crates/sh0-store/src/paths.rs)：新增 `cred_key()` → `<data_dir>/creds.key`。
- [creds.rs](../crates/sh0-store/src/creds.rs)：
  - v2 vault 格式（data-key + ChaCha20-Poly1305），去掉默认主密码路径。
  - `open()` 改为「加载/生成 data-key → 解密 v2 vault」；`create_dir_all` + `0600` 写 key 文件
    （Unix 用 `PermissionsExt`；Windows 记 `#[cfg]` 分支）。
  - `status()` 暴露 `keyring_available / vault_ready / backend`。
  - v1→v2 迁移（§3.4）。
  - `set/get/delete/list` 语义不变。
  - 删除或弃用 `MASTER_PASSWORD_ENV` 的默认必需性（保留环境变量作为「旧 vault 迁移」入口，
    不再作为运行期解锁必需）。
- [config.rs](../crates/sh0-store/src/config.rs)：`Host.id` 补 `serde(default = "new_host_id")`
  （实际改动在 `sh0-core`，此处确认序列化兼容）。

### sh0-core

- [model.rs](../crates/sh0-core/src/model.rs)：
  - `Host.id` 加默认。
  - `CredentialRef` 增 `Key { secret, passphrase }` 变体。

### sh0-ssh

- [client.rs](../crates/sh0-ssh/src/client.rs)：`authenticate` 增 `CredentialRef::Key` 分支：
  resolve PEM + 口令 → 字节解码私钥 → `authenticate_publickey`。现有 `KeyFile` 不动。

### sh0-ipc

- [protocol.rs](../crates/sh0-ipc/src/protocol.rs)：`methods` 增 `credential.set/delete/status`。

### sh0-cli

- [cli.rs](../crates/sh0-cli/src/cli.rs)：
  - 顶层枚举删 `Host / Secret / Config`；`Daemon` 保留为可见命令（评审后取消 `hide = true`）。
  - 新增 `Help`；`command` 改为 `Option<Cmd>`，`None` → 打印 help（或 `arg_required_else_help`）。
  - `Tui` 保留可选 `host` 位置参数。
- [main.rs](../crates/sh0-cli/src/main.rs)：
  - 路由删 `Host / Secret / Config` 分支；`Help` 打印 §2.3 文本（含运行时路径）。
  - `secret set` 相关分支删除（能力迁 TUI）。
- [daemon.rs](../crates/sh0-cli/src/daemon.rs)：
  - `DaemonState` 增 `creds`；`new()` 增参；`run_for_test` 传 `None`。
  - `Handler::call` 增三个 `credential.*` handler。
  - `run_foreground` 增空闲自退出。
- [ensure.rs](../crates/sh0-cli/src/ensure.rs)：re-exec 参数改用隐藏 `daemon`（路径不变，
  只是不再在帮助里展示）。

### sh0-tui

- [hosts.rs](../crates/sh0-tui/src/hosts.rs)：`HostForm` 重构（分步 + 认证单选 + 凭据采集 +
  `to_host -> (Host, Vec<CredWrite>)`）。
- [app.rs](../crates/sh0-tui/src/app.rs)：
  - 表单提交后经 IPC 下发 `credential.set`（失败则回滚 hosts.toml 写入并提示）。
  - 删除主机时级联 `credential.delete`。
  - 新增 `SecretInput` 模态处理。
  - `Action::HostNew` 文案 / 行为对齐「新建并连接」。
- [palette.rs](../crates/sh0-tui/src/palette.rs)：动作文案与可选 `HostImport`。

### sh0-ai

- [lib.rs](../crates/sh0-ai/src/lib.rs)：`auth_kind` 增 `"key"`；空清单提示文案改为指向 TUI。
  **其余零改动。**

---

## 8. 迁移与兼容

- **hosts.toml**：向后兼容。新增 `key = {…}` 是叠加变体；旧 `agent / key_file / password{env,
  keyring} / interactive / none` 全部继续可用。
- **creds.vault**：v1 → v2 一次性迁移（§3.4）。OS 钥匙串条目不受影响。
- **CLI 脚本**：
  - `sh0 secret set …` → 改用 `sh0 tui`（或保留隐藏命令过渡，P2）。
  - `sh0 host list` → `sh0 ai host list`。
  - `sh0 host import` → `sh0 ai host import`。
  - `sh0 config path/check` → `sh0 help` / 任意 `sh0` 启动即校验。
- **文档**：README 快速开始、`docs/usage.md` 中所有 `sh0 secret / sh0 config / sh0 host` 示例
  同步改写；补一节「在 TUI 里新建主机（含密码/私钥）」。

---

## 9. 测试策略

契约测试 [contract.rs](../crates/sh0-cli/tests/contract.rs) 是进程内 IPC + mock 传输，
**不依赖 CLI 表面**，因此删顶层子命令不会破坏其主体；需做增量与回归：

1. **新增 store 单测**：
   - data-key 生成、`0600` 权限、v2 vault 加解密往返、损坏文件报错。
   - v1→v2 迁移（有 `SH0_MASTER_PASSWORD` 时成功；无则备份 + 新建空 vault）。
2. **新增 IPC 契约**（复用 `run_for_test`，注入临时 `CredentialStore`）：
   - `credential.set` 后 `ai session.open`（mock 认证）能通过 `VaultResolver` 取到值。
   - `credential.delete` 后取不到、`removed=true`。
   - `credential.status` 返回正确后端。
3. **TUI 单元测试**（`hosts.rs` 已有 `to_host` 测试的扩展）：
   - `to_host` 对四种认证方式生成的 `CredentialRef` 与 `CredWrite` 列表正确（含 §4.3 account 命名）。
   - 编辑时「留空密码 = 不生成覆盖写入」。
4. **ssh 层单测**：`CredentialRef::Key` 的字节解码路径（用测试 PEM，不开真机）。
5. **CLI 冒烟**：
   - `sh0` / `sh0 help` 退出码 0 且打印路径。
   - `sh0 tui --help`、`sh0 ai --help` 正常；`sh0 secret …` / `sh0 config …` 报「未知命令」。
6. 全量 `cargo test --workspace`、`cargo clippy --workspace --all-targets`（零警告）、
   `cargo build --release`（体积/启动回归，NFR-1/NFR-2）。

---

## 10. 分阶段实施顺序（给 flash）

按依赖序，每阶段可独立提交并保持可编译：

- **P0 · CLI 收敛**：删顶层子命令、隐藏 daemon、加 `help`、`command: Option<Cmd>`。
  同步更新 main.rs 路由与文档占位。此阶段不碰凭据。
- **P1 · 凭据存储 v2**：`paths::cred_key`、v2 vault、data-key、迁移、`status()`。
  保持 `secret set` 仍能临时验证（若尚未删除 CLI，可先共存在隐藏态）。
- **P2 · IPC + daemon 接线**：`credential.*` 方法、`DaemonState.creds`、handler。
- **P3 · 模型 + ssh**：`CredentialRef::Key`、ssh 字节解码、`ai auth_kind`。
- **P4 · TUI 主机管理**：表单重构、掩码输入、保存并连接、级联删除、命令面板文案。
- **P5 · 收尾**：删除 `secret` 旧实现、daemon 空闲自退出、README/usage 改写、
  全量测试 + clippy + release 验证。

> P0 与 P1 可并行（分别由两人或先后做）；P2/P3 依赖 P1，P4 依赖 P2/P3。

---

## 11. 验收标准

- `sh0 --help` 显示 `tui / ai / help / daemon`（daemon 为运维命令）。
- `sh0 help` 打印真实配置/数据/日志目录。
- 全新用户流程：`sh0 tui` → `n` → 填地址/用户 → 选 password → 掩码输密码 → `Enter`
  → 自动保存并进入终端；`Ctrl+Q` 退出后再次 `sh0 tui web-01` 无需重输密码。
- 密码与私钥在 `<data_dir>/creds.*` 内为密文（`grep` 不到明文）；`hosts.toml` 只有 `SecretRef` 引用。
- 私钥走「导入」后即使原文件被删除，连接仍可用；走「外部路径」时仍按路径读取。
- `sh0 ai …` 全量子命令回归通过（信封 / 退出码 / 风险 / 审计不变），且 `ai host show`
  能区分 `keyfile` 与 `key`。
- `cargo test --workspace`、clippy 零警告、release 构建通过且体积/冷启动不劣化。

---

## 12. 风险与待定

1. **headless Linux 无 Secret Service**：`keyring` 后端可能不可用 → 必须靠 v2 data-key vault 兜底；
   `credential.status` 的 `keyring_available` 探测要快（避免 dbus 超时拖慢首次 `set`）。
2. **`creds.key` 与 vault 同机同用户**：加密强度 = 文件权限（0600）。明确记录信任边界；
   后续如需更强可加 OS DPAPI/Keychain 专用后端或可选主密码加固（不在本次）。
3. **daemon 空闲自退出的阈值**：默认 300s 需实测（避免「退出 TUI 立刻重进」的冷启动抖动）。
4. **凭据写入失败的回滚**：`credential.set` 成功后 hosts.toml 写失败（或反之）需定义顺序与回滚，
   避免「有主机无凭据 / 有凭据无主机」的不一致。建议顺序：先写 hosts.toml，再写凭据；
   凭据失败则回滚 hosts.toml（内存里保留，落盘前校验）。
5. **russh 字节解码 API 签名**：实现 P3 时核对（`decode_secret_key` 或等价入口），
   与现有 `load_secret_key` 的参数/返回一致即可。
6. **主机改名与 account**：用 `host.id` 作 account 已规避；但「外部路径私钥」与旧 `account=name`
   条目仍需在编辑表单里正确回显、不得误删。
7. **审计脱敏**：`credential.set` 只记 account、不记 value；禁止复用可能把 value 拼进消息的路径。

---

## 附：决策记录（ADR 摘要）

| 决策 | 结论 | 理由 | 备选 |
| --- | --- | --- | --- |
| vault 解锁方式 | data-key 文件（0600） | 无主密码也能无感重连，符合「本地加密」诉求 | 主密码/DPAPI（体验或复杂度代价高） |
| 凭据写通道 | daemon 唯一写者 + IPC | 内存 vault 一致，免跨进程同步 | TUI 直写文件（需 read-through，更易出 bug） |
| 私钥存储 | 导入加密库（`CredentialRef::Key`） | 「密钥本地加密保存」的字面要求 | 仅引用路径（保留兼容，不满足导入） |
| account 命名 | `<host.id>[...]` | 改名/重名安全 | `<name>`（旧引用继续兼容） |
| 顶层命令 | `tui / ai / help` + 可见 daemon(run/stop) | 用户先要求三命令，评审后取消该约定 | 继续隐藏 daemon |
