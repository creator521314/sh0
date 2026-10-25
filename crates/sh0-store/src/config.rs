//! TOML 配置：全局偏好 + 主机清单（需求 FR-9.1 / FR-1.1）

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use sh0_core::{Host, HostSelector, RiskLevel};
use crate::import::ImportReport;
use crate::log::LogConfig;

use crate::error::{Result, StoreError};
use crate::paths::Paths;

/// 配置文件格式版本，跨版本迁移用（需求 FR-9.5）
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    /// 请求远端 PTY 时上报的终端类型
    #[serde(default = "default_term")]
    pub term: String,
    /// 滚动缓冲行数（需求 FR-2.3）
    #[serde(default = "default_scrollback")]
    pub scrollback_lines: usize,
    /// keepalive 心跳间隔（秒，0 表示关闭）
    #[serde(default = "default_keepalive")]
    pub keepalive_secs: u64,
    /// 新建会话的默认尺寸
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// 全部会话共享的 scrollback 内存预算（MB，0 表示不限制）。
    /// 这是 AC-2「20 会话 ≤ 120 MB」的真正约束——行数上限只是软的。
    #[serde(default = "default_budget_mb")]
    pub memory_budget_mb: usize,
    /// 键位预设：default / vim / xshell（需求 FR-3.6）
    #[serde(default = "default_keymap")]
    pub keymap: String,
    /// 鼠标支持（需求 FR-3.9）：点击主机树选中 / 双击连接 / 滚轮滚动。
    /// 关闭后鼠标事件全部交回父终端——想用系统自带的"拖选复制"时关掉它。
    #[serde(default = "default_mouse")]
    pub mouse: bool,
    /// 配色主题：dark / light / high-contrast（需求 FR-3.7）
    #[serde(default = "default_theme")]
    pub theme: String,
    /// 键位覆盖表，形如 `quit = "ctrl+x"`，优先级高于预设
    #[serde(default)]
    pub keys: HashMap<String, String>,
    /// 断线重连行为（需求 FR-2.5 / FR-2.6）
    #[serde(default)]
    pub reconnect: ReconnectConfig,
    /// 审计与日志（需求 FR-8）
    #[serde(default)]
    pub audit: AuditConfig,
    /// AI 接口的保护参数（需求 FR-6.11 / FR-7.7）
    #[serde(default)]
    pub limits: LimitsConfig,
    /// 风险引擎（需求 FR-7.5）
    #[serde(default)]
    pub risk: RiskConfig,
    /// daemon 日志轮转与保留（需求 FR-8.5）
    #[serde(default)]
    pub log: LogConfig,
}

/// 重连策略的可持久化形式
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconnectConfig {
    /// 最大尝试次数，null 表示无限重试
    #[serde(default = "default_max_attempts")]
    pub max_attempts: Option<u32>,
    #[serde(default = "default_base_ms")]
    pub base_ms: u64,
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
    #[serde(default = "default_jitter")]
    pub jitter: bool,
    /// 重连成功后是否在历史输出里插入分隔线（AC-3）
    #[serde(default = "default_mark")]
    pub mark: bool,
}

fn default_version() -> u32 {
    CONFIG_VERSION
}
fn default_term() -> String {
    "xterm-256color".to_string()
}
fn default_scrollback() -> usize {
    10_000
}
fn default_keepalive() -> u64 {
    30
}
fn default_cols() -> u16 {
    120
}
fn default_rows() -> u16 {
    32
}
fn default_budget_mb() -> usize {
    64
}
fn default_keymap() -> String {
    "default".to_string()
}
fn default_mouse() -> bool {
    true
}
fn default_theme() -> String {
    "dark".to_string()
}
fn default_max_attempts() -> Option<u32> {
    Some(10)
}
fn default_base_ms() -> u64 {
    500
}
fn default_max_delay_ms() -> u64 {
    30_000
}
fn default_jitter() -> bool {
    true
}
fn default_mark() -> bool {
    true
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            base_ms: default_base_ms(),
            max_delay_ms: default_max_delay_ms(),
            jitter: default_jitter(),
            mark: default_mark(),
        }
    }
}

/// 审计配置（需求 FR-8.1~FR-8.4）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditConfig {
    /// 总开关。关闭后不写库、不归档，但 daemon 仍会输出 tracing 日志
    #[serde(default = "default_audit_enabled")]
    pub enabled: bool,
    /// 命令输出归档（FR-8.2，默认开）
    #[serde(default = "default_archive_output")]
    pub archive_output: bool,
    /// 保留天数（FR-8.5 的策略化下限，默认 30 天）
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// 审计库体积上限（MB）。超出后从最旧记录开始淘汰
    #[serde(default = "default_max_db_mb")]
    pub max_db_mb: u32,
}

/// AI 接口保护参数（需求 FR-6.11 / FR-7.7）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    /// 单次 exec 的 stdout+stderr 输出上限（字节，默认 64KB）。
    /// 超出即截断并置 `truncated` 标记——AI 读不到被截断的尾部，
    /// 因此这个值要能覆盖常见巡检命令的输出。
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    /// exec 默认超时（秒），可被 --timeout 覆盖（上限 3600）
    #[serde(default = "default_exec_timeout")]
    pub exec_timeout_secs: u64,
    /// 单 actor 每分钟 exec 次数上限，0 = 关闭（FR-7.7）
    #[serde(default)]
    pub rate_limit_per_min: u32,
    /// 批量执行的默认并发数（FR-5.2）；0 或未配置 = 内置默认 16。
    /// 30 台各跑一个 `df -h`，并发 16 时总耗时应控制在 10s 内。
    #[serde(default = "default_batch_concurrency")]
    pub batch_concurrency: usize,
}

/// 风险引擎配置（需求 FR-7.5）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    /// 用户规则文件，相对 config_dir
    #[serde(default = "default_rules_file")]
    pub rules_file: String,
    /// 未命中任何规则时的默认档位。
    /// 默认 `confirm`：宁可多问一次，不可放过一条（FR-7 的安全取向）
    #[serde(default = "default_unmatched")]
    pub unmatched: RiskLevel,
}

fn default_audit_enabled() -> bool {
    true
}
fn default_archive_output() -> bool {
    true
}
fn default_retention_days() -> u32 {
    30
}
fn default_max_db_mb() -> u32 {
    512
}
fn default_max_output_bytes() -> usize {
    64 * 1024
}
fn default_exec_timeout() -> u64 {
    30
}
fn default_batch_concurrency() -> usize {
    16
}
fn default_rules_file() -> String {
    "risk.toml".to_string()
}
fn default_unmatched() -> RiskLevel {
    RiskLevel::Confirm
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: default_audit_enabled(),
            archive_output: default_archive_output(),
            retention_days: default_retention_days(),
            max_db_mb: default_max_db_mb(),
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_output_bytes: default_max_output_bytes(),
            exec_timeout_secs: default_exec_timeout(),
            rate_limit_per_min: 0,
            batch_concurrency: default_batch_concurrency(),
        }
    }
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            rules_file: default_rules_file(),
            unmatched: default_unmatched(),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            term: default_term(),
            scrollback_lines: default_scrollback(),
            keepalive_secs: default_keepalive(),
            cols: default_cols(),
            rows: default_rows(),
            memory_budget_mb: default_budget_mb(),
            keymap: default_keymap(),
            mouse: default_mouse(),
            theme: default_theme(),
            keys: HashMap::new(),
            reconnect: ReconnectConfig::default(),
            audit: AuditConfig::default(),
            limits: LimitsConfig::default(),
            risk: RiskConfig::default(),
            log: LogConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HostsFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub hosts: Vec<sh0_core::Host>,
}

/// 配置与主机清单的读写入口
pub struct Store {
    paths: Paths,
}

impl Store {
    pub fn new() -> Result<Self> {
        let paths = Paths::discover()?;
        paths.ensure_dirs()?;
        Ok(Self { paths })
    }

    /// 用指定 paths 构造，便于测试注入临时目录
    pub fn with_paths(paths: Paths) -> Result<Self> {
        paths.ensure_dirs()?;
        Ok(Self { paths })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn load_config(&self) -> Result<AppConfig> {
        let path = self.paths.config_file();
        if !path.exists() {
            let cfg = AppConfig::default();
            self.save_config(&cfg)?;
            return Ok(cfg);
        }
        let raw = std::fs::read_to_string(&path)?;
        toml::from_str::<AppConfig>(&raw).map_err(|e| StoreError::Parse {
            path: path.clone(),
            source: e,
        })
    }

    pub fn save_config(&self, cfg: &AppConfig) -> Result<()> {
        let raw = toml::to_string_pretty(cfg).map_err(StoreError::Serialize)?;
        std::fs::write(self.paths.config_file(), raw)?;
        Ok(())
    }

    /// 取一项配置，key 用点路径（`limits.exec_timeout_sec`）。
    /// 返回 JSON 值便于 CLI 直接打印；找不到返回 None（而不是报错，
    /// 让调用方决定"拼错键名"算不算错误）。
    pub fn config_get(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let cfg = self.load_config()?;
        let mut v = serde_json::to_value(&cfg)?;
        for part in key.split('.') {
            match v.get(part) {
                Some(next) => v = next.clone(),
                None => return Ok(None),
            }
        }
        Ok(Some(v))
    }

    /// 写一项配置并落盘。未知键名直接报错——宁可拒绝也不静默忽略，
    /// 否则 `sh0 config set limits.exec_timeut 5` 这种拼写错误会假装成功。
    pub fn config_set(&self, key: &str, value: &str) -> Result<()> {
        let mut cfg = self.load_config()?;
        let mut v = serde_json::to_value(&cfg)?;

        // 先确认路径存在，再写入
        {
            let mut cur = &v;
            for part in key.split('.') {
                cur = cur
                    .get(part)
                    .ok_or_else(|| StoreError::UnknownConfigKey(key.to_string()))?;
            }
        }
        let parsed = parse_scalar(value);
        let mut cur = &mut v;
        let parts: Vec<&str> = key.split('.').collect();
        for part in &parts[..parts.len() - 1] {
            cur = cur.get_mut(*part).expect("路径已校验过");
        }
        cur[parts[parts.len() - 1]] = parsed;

        cfg = serde_json::from_value(v)?;
        self.save_config(&cfg)
    }

    /// 把导入结果并入 hosts.toml（FR-1.10）。
    ///
    /// `dry_run` 为真时只算不写——导入会动用户的主机清单，先看一眼再落盘
    /// 是应该的。重名条目一律保留现有配置，绝不覆盖用户手工改过的东西。
    pub fn import_hosts(&self, report: ImportReport, dry_run: bool) -> Result<ImportReport> {
        let mut file = self.load_hosts()?;
        let merged = crate::import::merge_into(&mut file.hosts, report);
        if !dry_run && !merged.imported.is_empty() {
            self.save_hosts(&file)?;
        }
        Ok(merged)
    }

    /// 配置文件与主机清单的修改时间，热重载据此判断要不要重新读
    pub fn config_mtime(&self) -> Option<std::time::SystemTime> {
        mtime_of(&self.paths.config_file()).max(mtime_of(&self.paths.hosts_file()))
    }

    pub fn load_hosts(&self) -> Result<HostsFile> {
        let path = self.paths.hosts_file();
        if !path.exists() {
            let hosts = HostsFile {
                version: CONFIG_VERSION,
                hosts: Vec::new(),
            };
            self.save_hosts(&hosts)?;
            return Ok(hosts);
        }
        let raw = std::fs::read_to_string(&path)?;
        toml::from_str::<HostsFile>(&raw).map_err(|e| StoreError::Parse {
            path: path.clone(),
            source: e,
        })
    }

    pub fn save_hosts(&self, hosts: &HostsFile) -> Result<()> {
        let raw = toml::to_string_pretty(hosts).map_err(StoreError::Serialize)?;
        std::fs::write(self.paths.hosts_file(), raw)?;
        Ok(())
    }

    /// 按名称或 ID 精确匹配一台主机；返回 CoreError 便于上层复用语义。
    /// 返回的主机**已展开跳板链**（`Host::jump`）。
    pub fn find_host(&self, selector: &str) -> std::result::Result<sh0_core::Host, sh0_core::CoreError> {
        self.lookup_host(selector)
            .map(|h| self.with_jump_chain(&h, 0))
    }

    /// 只查清单、不展开跳板。展开过程内部用它，否则 `find_host → with_jump_chain
    /// → find_host` 会把 depth 每次归零，配置成环时直接爆栈。
    fn lookup_host(&self, selector: &str) -> std::result::Result<sh0_core::Host, sh0_core::CoreError> {
        let hosts = self.load_hosts().map_err(|e| {
            sh0_core::CoreError::Credential(format!("failed to read hosts.toml: {e}"))
        })?;
        let mut matches = hosts
            .hosts
            .into_iter()
            .filter(|h| h.name == selector || h.id == selector);

        match matches.next() {
            Some(first) => match matches.next() {
                Some(_) => Err(sh0_core::CoreError::AmbiguousHost {
                    selector: selector.to_string(),
                    count: 2,
                }),
                None => Ok(first),
            },
            None => Err(sh0_core::CoreError::HostNotFound(selector.to_string())),
        }
    }

    /// 把 `host.proxy_jump` 展开成完整的跳板 `Host` 挂在 `host.jump` 上。
    ///
    /// 跳板值有两种写法：hosts.toml 里另一台主机的名字，或内联的
    /// `user@host:port`（user / port 可省）。跳板自己也可以有跳板，
    /// 递归展开即可串成链；`depth` 用来兜住写错造成的循环引用。
    pub fn with_jump_chain(&self, host: &sh0_core::Host, depth: usize) -> sh0_core::Host {
        let Some(spec) = host.proxy_jump.as_deref() else {
            let mut out = host.clone();
            out.jump = None;
            return out;
        };
        if depth >= MAX_JUMP_DEPTH {
            tracing::warn!(
                "{} 的跳板链超过 {} 层（可能配置成了环），就此打住",
                host.name,
                MAX_JUMP_DEPTH
            );
            let mut out = host.clone();
            out.jump = None;
            return out;
        }
        let mut out = host.clone();
        match self.lookup_host(spec) {
            Ok(jump) => {
                // 跳板自己再展开一层
                let jump = self.with_jump_chain(&jump, depth + 1);
                out.jump = Some(Box::new(jump));
            }
            Err(_) => match parse_jump_spec(spec) {
                Some(jump) => {
                    let jump = self.with_jump_chain(&jump, depth + 1);
                    out.jump = Some(Box::new(jump));
                }
                None => {
                    // 既不是已知主机、也不是 user@host:port —— 交给连接层直连失败报错，
                    // 这里不擅自改用户意图，只记一条警告
                    tracing::warn!("{} 的跳板 `{spec}` 无法解析，本次直连", host.name);
                    out.jump = None;
                }
            },
        }
        out
    }

    /// 批量选择主机（FR-5.2）：显式列表 / 分组前缀 / 标签，取并集去重。
    ///
    /// 显式列表里的名字找不到就**整个失败**——批量场景里"少跑了一台"
    /// 比"跑不了"更难发现，宁可提前报错。
    pub fn select_hosts(
        &self,
        sel: &HostSelector,
    ) -> std::result::Result<Vec<Host>, sh0_core::CoreError> {
        let all = self.load_hosts().map_err(|e| {
            sh0_core::CoreError::Credential(format!("failed to read hosts.toml: {e}"))
        })?;
        if sel.is_empty() {
            return Err(sh0_core::CoreError::HostNotFound("选择器为空".into()));
        }
        if sel.all {
            return Ok(all.hosts);
        }

        let mut out: Vec<Host> = Vec::new();
        let push = |h: &Host, out: &mut Vec<Host>| {
            if !out.iter().any(|x| x.id == h.id) {
                out.push(h.clone());
            }
        };

        for name in &sel.hosts {
            let Some(h) = all
                .hosts
                .iter()
                .find(|h| h.name == *name || h.id == *name)
            else {
                return Err(sh0_core::CoreError::HostNotFound(name.clone()));
            };
            push(h, &mut out);
        }
        if let Some(g) = &sel.group {
            let prefix = format!("{g}/");
            for h in all
                .hosts
                .iter()
                .filter(|h| h.group_path() == g.as_str() || h.group_path().starts_with(&prefix))
            {
                push(h, &mut out);
            }
        }
        if let Some(t) = &sel.tag {
            for h in all.hosts.iter().filter(|h| h.tags.iter().any(|x| x == t)) {
                push(h, &mut out);
            }
        }
        if out.is_empty() {
            return Err(sh0_core::CoreError::HostNotFound(format!(
                "没有任何主机匹配：{:?}",
                sel
            )));
        }
        // 批量同样要展开跳板：每台机器都有自己的 proxy_jump
        Ok(out
            .into_iter()
            .map(|h| self.with_jump_chain(&h, 0))
            .collect())
    }

    /// 用户风险规则文件路径（FR-7.5）。`rules_file` 来自 `cfg.risk.rules_file`
    pub fn risk_rules_path(&self, rules_file: &str) -> std::path::PathBuf {
        self.paths.config_dir.join(rules_file)
    }

    /// 读取用户风险规则。文件不存在 = 没有自定义规则（不是错误）；
    /// 文件存在但解析失败 = 错误（静默忽略规则等于悄悄放宽风险）。
    pub fn load_risk_rules(&self, rules_file: &str) -> Result<Vec<sh0_core::Rule>> {
        let path = self.risk_rules_path(rules_file);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&path)?;
        #[derive(Deserialize)]
        struct RulesFile {
            #[serde(default)]
            rule: Vec<sh0_core::Rule>,
        }
        toml::from_str::<RulesFile>(&raw)
            .map(|f| f.rule)
            .map_err(|e| StoreError::Parse {
                path,
                source: e,
            })
    }
}

/// 把命令行里的字符串转成 JSON 标量：`true`/`false`、整数、或字符串。
///
/// 不支持数组与对象——`sh0 config set` 的定位是"改一两项开关"，
/// 复杂结构请直接编辑 config.toml。
fn parse_scalar(value: &str) -> serde_json::Value {
    if let Ok(b) = value.parse::<bool>() {
        return serde_json::Value::Bool(b);
    }
    if let Ok(n) = value.parse::<i64>() {
        return serde_json::Value::from(n);
    }
    if let Ok(n) = value.parse::<f64>() {
        return serde_json::Value::from(n);
    }
    serde_json::Value::String(value.to_string())
}

fn mtime_of(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// 跳板链最大层数：兜住 A→B→A 这种配置错误
pub const MAX_JUMP_DEPTH: usize = 5;

/// 解析内联跳板写法 `user@host:port`，user 与 port 可省。
///
/// 跳板一般走 agent / 默认私钥，所以这里固定 `CredentialRef::Agent`——
/// 需要指定私钥时请把它写成 hosts.toml 里的一台正式主机。
fn parse_jump_spec(spec: &str) -> Option<sh0_core::Host> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let (user, rest) = match spec.rsplit_once('@') {
        Some((u, r)) if !u.is_empty() && !r.is_empty() => (u.to_string(), r.to_string()),
        _ => (
            std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "root".to_string()),
            spec.to_string(),
        ),
    };
    // 端口只在最后一段是数字时才切，避免把 IPv6 地址的冒号切坏
    let (addr, port) = match rest.rsplit_once(':') {
        Some((a, p)) if !a.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            match p.parse::<u16>() {
                Ok(port) => (a.to_string(), port),
                Err(_) => return None,
            }
        }
        _ => (rest, 22),
    };
    if addr.is_empty() {
        return None;
    }
    Some(sh0_core::Host {
        id: sh0_core::new_host_id(),
        name: format!("jump:{addr}"),
        group: vec![],
        addr,
        port,
        user,
        auth: sh0_core::CredentialRef::Agent,
        tags: vec![],
        note: Some("由 proxy_jump 内联生成的跳板".to_string()),
        risk_cap: None,
        proxy_jump: None,
        jump: None,
    })
}
