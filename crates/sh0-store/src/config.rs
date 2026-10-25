//! TOML 配置：全局偏好 + 主机清单（需求 FR-9.1 / FR-1.1）

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use sh0_core::{Host, HostSelector, RiskLevel};

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
            keys: HashMap::new(),
            reconnect: ReconnectConfig::default(),
            audit: AuditConfig::default(),
            limits: LimitsConfig::default(),
            risk: RiskConfig::default(),
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

    /// 按名称或 ID 精确匹配一台主机；返回 CoreError 便于上层复用语义
    pub fn find_host(&self, selector: &str) -> std::result::Result<sh0_core::Host, sh0_core::CoreError> {
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
        Ok(out)
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
