//! TOML 配置与持久化层。
//!
//! M0 落地配置层（TOML）；M2 引入 SQLite 审计库（`db`）与凭据存储
//! （`creds`：OS 钥匙串 + 主密码加密 vault 双后端）。

pub mod config;
pub mod creds;
pub mod db;
pub mod error;
pub mod log;
pub mod paths;

pub use config::{AppConfig, AuditConfig, HostsFile, LimitsConfig, RiskConfig, Store, CONFIG_VERSION};
pub use creds::{CredentialStore, MASTER_PASSWORD_ENV};
pub use db::{
    AuditDb, AuditFilter, AuditRecord, BatchResultRow, BatchRunDetail, BatchRunRecord,
    BatchRunSummary, redact,
};
pub use error::{Result, StoreError};
pub use log::{LogConfig, RotatingFileWriter, prune_old_logs, rotate_file};
pub use paths::{Paths, APP_NAME};
