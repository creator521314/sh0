//! TUI 前端（ratatui + crossterm）。
//!
//! M1 的布局遵循需求 FR-3.1：左侧主机树 + 顶部标签栏 + 主区终端 + 底部状态栏。
//! 每个标签页在**本地**持有一份 `sh0_term::Screen`——daemon 只推字节流过来，
//! 渲染在自己进程里做，避免每帧跨进程传整个网格。

pub mod app;
pub mod files;
pub mod hosts;
pub mod palette;
pub mod render;
pub mod theme;

pub use app::{App, Focus, Tab, View, dispatch_key, run, should_handle_key};
pub use hosts::{HostForm, HostTree};
pub use palette::Palette;
pub use theme::Theme;
