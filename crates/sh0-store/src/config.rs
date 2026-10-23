//! TOML 配置：全局偏好 + 主机清单（需求 FR-9.1 / FR-1.1）

use serde::{Deserialize, Serialize};

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

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            term: default_term(),
            scrollback_lines: default_scrollback(),
            keepalive_secs: default_keepalive(),
            cols: default_cols(),
            rows: default_rows(),
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
}
