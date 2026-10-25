//! 三平台配置/数据/运行时目录解析（需求 FR-9.1）

use std::path::PathBuf;

use crate::error::{Result, StoreError};

/// 应用名，同时决定配置目录名
pub const APP_NAME: &str = "sh0";

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .ok_or(StoreError::NoConfigDir)?
            .join(APP_NAME);
        let data_dir = dirs::data_dir()
            .ok_or(StoreError::NoConfigDir)?
            .join(APP_NAME);
        let runtime_dir = data_dir.join("run");
        Ok(Self {
            config_dir,
            data_dir,
            runtime_dir,
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn hosts_file(&self) -> PathBuf {
        self.config_dir.join("hosts.toml")
    }

    /// known_hosts 走独立文件，格式与 OpenSSH 兼容（需求 FR-1.8）
    pub fn known_hosts(&self) -> PathBuf {
        self.data_dir.join("known_hosts")
    }

    /// daemon 监听地址与鉴权令牌，仅本机可读
    pub fn daemon_info(&self) -> PathBuf {
        self.runtime_dir.join("daemon.json")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.runtime_dir.join("sh0.pid")
    }

    /// 后台 daemon 的日志。后台进程没有控制台，不落盘的话出错时完全无迹可寻。
    ///
    /// 由 tracing 写入，按大小轮转（FR-8.5）：`daemon.log` / `.1` / `.2`……
    pub fn daemon_log(&self) -> PathBuf {
        self.runtime_dir.join("daemon.log")
    }

    /// 兜底日志：进程启动早期（tracing 还没接管）的 stdout/stderr 与 panic。
    ///
    /// 与 `daemon_log` 分开是因为这边是**覆盖写**、每次启动归零，
    /// 而那边是追加 + 轮转。混在一个文件里会互相截断。
    pub fn daemon_boot_log(&self) -> PathBuf {
        self.runtime_dir.join("daemon.boot.log")
    }

    /// 审计库（SQLite 单文件，需求 FR-9.2：与配置同目录 → 放 data_dir）
    pub fn audit_db(&self) -> PathBuf {
        self.data_dir.join("audit.db")
    }

    /// 凭据加密 vault（钥匙串不可用时的回落，需求 FR-1.6 / FR-1.7）
    pub fn vault_file(&self) -> PathBuf {
        self.data_dir.join("creds.vault")
    }

    /// 目录不存在时创建（幂等）
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.runtime_dir)?;
        Ok(())
    }
}
