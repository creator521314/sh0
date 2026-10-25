//! sh0 的库目标。
//!
//! 与 `main.rs`（二进制）同包：daemon 的装配逻辑放这里，
//! 集成测试（`tests/contract.rs`）才能不启真实进程就驱动整条
//! IPC → daemon → mock 传输链路——这是 M2 契约测试（NFR-10）的前提。

pub mod cli;
pub mod config_cmd;
pub mod creds_bridge;
pub mod daemon;
pub mod ensure;
pub mod exec;
