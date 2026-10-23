//! 终端仿真层。
//!
//! M0 故意留空：这一阶段 TUI 直接渲染剥离 ANSI 后的裸字节流（见 sh0-tui::strip_ansi），
//! 目的是先打通键盘 → 远端 → 回显的闭环。
//!
//! M1 会在这里落地：
//!   - `parser.rs`  —— vte 解析器适配（W1.1）
//!   - `grid.rs`    —— 字符网格与颜色属性（W1.1）
//!   - `scrollback.rs` —— 环形滚动缓冲与内存预算（W1.3）
//!   - `keys.rs`    —— 按键 → 字节序列表，TUI 与 `ai input send` 共用（W1.4）
//!
//! 到时 `sh0-tui::strip_ansi` 与按键编码表都会下沉到本 crate。

/// 里程碑占位：M0 尚未实现终端仿真。
pub const MILESTONE: &str = "M0: terminal emulation not implemented yet";
