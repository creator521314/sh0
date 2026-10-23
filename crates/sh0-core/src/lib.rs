//! sh0 领域模型与编排核心。
//!
//! 依赖方向约束：本 crate **不得**依赖 TUI、CLI 或任何具体传输实现。
//! 它对下层的 SSH / 终端仿真只通过 trait 依赖，便于测试注入 mock（NFR-9）。

pub mod error;
pub mod ids;
pub mod model;

pub use error::{CoreError, Result};
pub use ids::{HostId, SessionId, new_host_id, new_session_id};
pub use model::{CredentialRef, Host, RiskLevel, SecretRef, SessionSnapshot, SessionState};
