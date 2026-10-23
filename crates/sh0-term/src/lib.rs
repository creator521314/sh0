//! 终端仿真层（M1 落地的三个模块都在这里）。
//!
//! ```text
//! 字节流 ──▶ parser (vte) ──▶ Grid ──▶ 渲染 / 纯文本导出
//!                               │
//!                               └─▶ Scrollback（受全局内存预算约束）
//! ```
//!
//! - [`screen::Screen`] 是唯一对外入口：喂字节进去，取文本/行出来。
//! - [`keys::encode_key`] 与 [`keymap::Keymap`] 分别负责"发给远端的字节"和
//!   "本地快捷键"，两者共用 [`keys::parse_key`] 的键名解析。

pub mod cell;
pub mod grid;
pub mod keymap;
pub mod keys;
pub mod parser;
pub mod screen;
pub mod scrollback;

pub use cell::{Cell, Color, Line};
pub use grid::Grid;
pub use keymap::Keymap;
pub use keys::encode_key;
pub use screen::{Screen, ScreenOptions};
pub use scrollback::MemoryBudget;
