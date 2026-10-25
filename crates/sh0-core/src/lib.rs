//! sh0 领域模型与编排核心。
//!
//! 依赖方向约束：本 crate **不得**依赖 TUI、CLI 或任何具体传输实现。
//! 它对下层的 SSH / 终端仿真只通过 trait 依赖，便于测试注入 mock（NFR-9）。

pub mod error;
pub mod ids;
pub mod model;
pub mod risk;
pub mod secret;
pub mod session;
pub mod sftp;
pub mod transport;

#[cfg(feature = "mock")]
pub mod mock;

pub use error::{CoreError, Result};
pub use ids::{HostId, SessionId, new_host_id, new_session_id};
pub use model::{
    CredentialRef, Host, HostSelector, RiskLevel, SecretRef, SessionSnapshot, SessionState,
};
pub use risk::{Assessment, RiskEngine, Rule};
pub use secret::{DefaultResolver, SecretResolver};
pub use session::{
    ReconnectPolicy, SessionConfig, SessionEvent, SessionHandle, spawn_session,
};
pub use sftp::{FileEntry, SftpClient, SftpError, SftpResult, mode_string};
pub use transport::{
    ExecOutput, SessionTransport, TransportError, TransportEvent, TransportFactory, TransportResult,
};
