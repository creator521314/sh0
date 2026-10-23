//! 单元格：一个字符 + 前景/背景色 + 属性位。
//!
//! 内存是这个结构体的第一约束：AC-2 要求 20 个会话常驻内存 ≤ 120 MB，
//! 而单会话满配 scrollback（10000 行 × 120 列）若每格 16 字节就是 19 MB。
//! 因此这里用位域存属性、颜色用三变体枚举（4 字节），配合
//! [`crate::scrollback::MemoryBudget`] 做全局淘汰。

use unicode_width::UnicodeWidthChar;

/// 终端颜色
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// 终端默认色，具体值由上层主题决定（需求 FR-3.4）
    Default,
    /// xterm 256 色索引
    Indexed(u8),
    /// 24 位真彩
    Rgb(u8, u8, u8),
}

impl Color {
    /// 转成 RGB 三元组。`Default` 返回 `None`，由上层主题填充。
    pub fn to_rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Color::Default => None,
            Color::Indexed(i) => Some(indexed_to_rgb(i)),
            Color::Rgb(r, g, b) => Some((r, g, b)),
        }
    }
}

/// xterm 256 色调色板 → RGB。
///
/// 0-15 为系统色，16-231 为 6×6×6 色立方，232-255 为灰阶。
pub fn indexed_to_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0..=15 => SYSTEM_COLORS[idx as usize],
        16..=231 => {
            let i = idx - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            // 0→0, 1→95, 2→135, ... 5→255
            let conv = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (conv(r), conv(g), conv(b))
        }
        232..=255 => {
            let v = 8 + (idx - 232) * 10;
            (v, v, v)
        }
    }
}

/// xterm 系统色（0-15）。数值取自 xterm 默认调色板。
const SYSTEM_COLORS: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

// ---- 属性位 ----

pub const BOLD: u8 = 1 << 0;
pub const DIM: u8 = 1 << 1;
pub const ITALIC: u8 = 1 << 2;
pub const UNDERLINE: u8 = 1 << 3;
pub const REVERSE: u8 = 1 << 4;
pub const HIDDEN: u8 = 1 << 5;
pub const STRIKE: u8 = 1 << 6;
/// 宽字符（CJK / emoji）的第二格，渲染时跳过
pub const WIDE_TAIL: u8 = 1 << 7;

/// 除宽字符续位外的所有属性位
pub const ATTR_MASK: u8 = BOLD | DIM | ITALIC | UNDERLINE | REVERSE | HIDDEN | STRIKE;

/// 屏幕上的一个字符格
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    /// 见 [`BOLD`] 等常量
    pub flags: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank(Color::Default, Color::Default, 0)
    }
}

impl Cell {
    pub fn new(ch: char, fg: Color, bg: Color, flags: u8) -> Self {
        Self { ch, fg, bg, flags }
    }

    /// 空格，保留属性（清屏时背景色要跟着当前属性走）
    pub fn blank(fg: Color, bg: Color, flags: u8) -> Self {
        Self { ch: ' ', fg, bg, flags }
    }

    pub fn wide_tail(fg: Color, bg: Color, flags: u8) -> Self {
        Self { ch: ' ', fg, bg, flags: flags | WIDE_TAIL }
    }

    pub fn is_wide_tail(&self) -> bool {
        self.flags & WIDE_TAIL != 0
    }

    /// 该格在屏幕上占用的列数
    pub fn width(&self) -> usize {
        if self.is_wide_tail() {
            0
        } else {
            char_width(self.ch)
        }
    }
}

/// 字符显示宽度：ASCII/CJK/emoji 为 1 或 2，组合字符为 0。
///
/// 宽字符与零宽组合字符的正确处理是 AC-9 的核心——宽度算错，
/// 光标位置就会整体错位，中文界面会越写越乱。
pub fn char_width(c: char) -> usize {
    if c == '\0' || c == '\u{7}' {
        return 0;
    }
    c.width().unwrap_or(0)
}

/// 一整行。
///
/// 组合字符（零宽/组合记号）按列稀疏存储：绝大多数行没有组合字符，
/// 因此用 [(列, 串)] 而不是给每个格都留位置。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    pub cells: Vec<Cell>,
    pub comb: Vec<(usize, String)>,
}

impl Line {
    pub fn blank(cols: usize, fg: Color, bg: Color, flags: u8) -> Self {
        Self {
            cells: vec![Cell::blank(fg, bg, flags); cols],
            comb: Vec::new(),
        }
    }

    /// 该格上附加的组合字符
    pub fn combining(&self, col: usize) -> Option<&str> {
        self.comb
            .iter()
            .find(|(c, _)| *c == col)
            .map(|(_, s)| s.as_str())
    }

    /// 把组合字符追加到指定列
    pub fn push_combining(&mut self, col: usize, c: char) {
        match self.comb.iter_mut().find(|(c2, _)| *c2 == col) {
            Some((_, s)) => s.push(c),
            None => self.comb.push((col, c.to_string())),
        }
    }

    /// 该格渲染出的完整文本（字符 + 组合记号）
    pub fn cell_text(&self, col: usize) -> String {
        match self.cells.get(col) {
            None => String::new(),
            Some(cell) => match self.combining(col) {
                Some(extra) => format!("{}{}", cell.ch, extra),
                None => cell.ch.to_string(),
            },
        }
    }

    /// 整行文本，去掉尾部空白
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (i, cell) in self.cells.iter().enumerate() {
            if cell.is_wide_tail() {
                continue;
            }
            out.push(cell.ch);
            if let Some(extra) = self.combining(i) {
                out.push_str(extra);
            }
        }
        out.trim_end().to_string()
    }

    /// 取 `[from, to)` 列范围的文本（宽字符尾格与组合字符的处理同 [`Line::text`]）
    pub fn text_range(&self, from: usize, to: usize) -> String {
        let mut out = String::new();
        for (i, cell) in self.cells.iter().enumerate() {
            if i < from || i >= to {
                continue;
            }
            if cell.is_wide_tail() {
                continue;
            }
            out.push(cell.ch);
            if let Some(extra) = self.combining(i) {
                out.push_str(extra);
            }
        }
        out
    }

    /// 该行是否全空白（用于判断"有效内容"）
    pub fn is_blank(&self) -> bool {
        self.cells.iter().all(|c| c.is_wide_tail() || c.ch == ' ')
    }
}
