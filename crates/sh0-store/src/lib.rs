//! TOML 配置与持久化层。
//!
//! M0 只落地配置层（TOML）；SQLite 审计库计划在 M2 的 W2.7 引入。

pub mod config;
pub mod error;
pub mod paths;

pub use config::{AppConfig, HostsFile, Store, CONFIG_VERSION};
pub use error::{Result, StoreError};
pub use paths::{Paths, APP_NAME};
