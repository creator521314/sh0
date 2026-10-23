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

    /// 目录不存在时创建（幂等）
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.runtime_dir)?;
        Ok(())
    }
}
