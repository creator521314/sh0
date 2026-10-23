//! daemon 与前端之间的本机 IPC。

pub mod error;
pub mod protocol;
pub mod transport;

pub use error::{IpcError, Result};
pub use protocol::{ClientMsg, DaemonInfo, ErrorBody, ServerMsg};
pub use transport::{EventHub, Handler, IpcClient, IpcConnection, serve};
