//! 屏幕：解析器 + 网格 + 滚动缓冲的组合体。
//!
//! 对外只暴露"喂字节进去、把内容取出来"这两件事，内部状态全部私有，
//! 便于 M2 的 `screen read` 与 TUI 渲染共用同一份实现。

use vte::Parser;

use crate::cell::{Cell, Color, Line};
use crate::grid::Grid;
use crate::scrollback::{MemoryBudget, Scrollback};

/// 默认 scrollback 行数上限（需求 FR-2.3）。
/// 注意这是**软上限**：真正的约束是 [`MemoryBudget`]（NFR-3 / AC-2）。
pub const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// 屏幕的二维网格视图（`screen read --mode grid` 的返回体）
#[derive(Debug, Clone)]
pub struct GridView {
    pub cols: usize,
    pub rows: usize,
    /// 光标 (行, 列)；回看历史时不可见则为 None
    pub cursor: Option<(usize, usize)>,
    /// 当前视图每行的文本
    pub lines: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ScreenOptions {
    pub cols: usize,
    pub rows: usize,
    pub scrollback_lines: usize,
    pub budget: Option<MemoryBudget>,
}
impl ScreenOptions {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
            budget: None,
        }
    }

    pub fn with_budget(mut self, budget: MemoryBudget) -> Self {
        self.budget = Some(budget);
        self
    }
}

pub struct Screen {
    parser: Option<Parser>,
    pub grid: Grid,
    scrollback: Scrollback,
    /// OSC 0/2 设置的窗口标题
    pub title: String,
    /// OSC 52 收到的剪贴板内容（base64）
    pub clipboard: Option<String>,
    /// 视图向上滚动的行数，0 = 贴着最新输出
    scroll_offset: usize,
    /// 已经喂进来的字节数，用于审计与调试
    bytes_fed: u64,
}

impl Screen {
    pub fn new(opts: ScreenOptions) -> Self {
        Self {
            parser: Some(Parser::new()),
            grid: Grid::new(opts.cols, opts.rows),
            scrollback: Scrollback::new(opts.scrollback_lines, opts.budget),
            title: String::new(),
            clipboard: None,
            scroll_offset: 0,
            bytes_fed: 0,
        }
    }

    // ---- 输入 ----

    /// 喂入远端字节流
    pub fn feed(&mut self, bytes: &[u8]) {
        self.bytes_fed += bytes.len() as u64;

        // Parser 需要 &mut，而 Perform 实现在 self 上，故临时取出避免双重借用
        let mut parser = self.parser.take().unwrap_or_default();
        parser.advance(self, bytes);
        self.parser = Some(parser);

        self.drain_grid();
    }

    /// 把解析过程中滚出屏幕的行收进 scrollback
    fn drain_grid(&mut self) {
        let out = self.grid.take_scrolled_out();
        if !out.is_empty() {
            self.scrollback.extend(out);
        }
        if self.grid.take_clear_scrollback() {
            self.scrollback.clear();
            self.scroll_offset = 0;
        }
    }

    /// 尺寸变化（需求 FR-2.4）
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.grid.resize(cols, rows);
        // 高度变小时被挤出屏幕的行要收进 scrollback，不能凭空丢弃
        self.drain_grid();
        self.scroll_offset = 0;
    }

    /// 重连分隔线（验收 AC-3）：在历史输出末尾插一条醒目标记，
    /// 让用户/AI 能一眼看出"这之后是重连后的新内容"。
    pub fn mark_reconnect(&mut self, text: &str) {
        let cols = self.grid.cols();
        let mut line = Line::blank(cols, Color::Indexed(3), Color::Default, 0);
        let label = format!("─── {text} ───");
        let start = cols.saturating_sub(label.chars().count()) / 2;
        for (i, c) in label.chars().enumerate() {
            if start + i < cols {
                line.cells[start + i] = Cell::new(c, Color::Indexed(3), Color::Default, 0);
            }
        }
        line.comb.clear();
        self.scrollback.push(line);
    }

    /// 直接插入一行文本（供 daemon 注入本地提示信息）
    pub fn push_notice(&mut self, text: &str) {
        let cols = self.grid.cols();
        let mut line = Line::blank(cols, Color::Indexed(6), Color::Default, 0);
        for (i, c) in text.chars().enumerate() {
            if i < cols {
                line.cells[i] = Cell::new(c, Color::Indexed(6), Color::Default, 0);
            }
        }
        self.scrollback.push(line);
    }

    // ---- 输出 ----

    pub fn cols(&self) -> usize {
        self.grid.cols()
    }

    pub fn rows(&self) -> usize {
        self.grid.rows()
    }

    /// 当前视图要渲染的 `rows` 行（含 scrollback 回看部分）
    pub fn view_lines(&self) -> Vec<&Line> {
        let rows = self.grid.rows();
        let sb_len = self.scrollback.len();
        let take = self.scroll_offset.min(sb_len);
        let mut out: Vec<&Line> = Vec::with_capacity(rows);

        if take <= rows {
            if take > 0 {
                out.extend(self.scrollback.iter().skip(sb_len - take));
            }
            out.extend(self.grid.lines().iter().take(rows - take));
        } else {
            let start = sb_len - take;
            out.extend(self.scrollback.iter().skip(start).take(rows));
        }
        out
    }

    /// 视图是否正在回看历史（用于状态栏提示）
    pub fn is_scrolled(&self) -> bool {
        self.scroll_offset > 0
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// 提取当前视图中一个矩形区域的文本（用于文本选择复制）。
    ///
    /// 坐标是**视图坐标**：`(0,0)` 是视图左上角，随 `scroll_offset` 变化。
    /// 两端点会被规范化（任意顺序都行），行尾的填充空格裁掉，
    /// 免得复制出来每行都拖着一长串空白。
    pub fn extract_view_text(&self, a: (usize, usize), b: (usize, usize)) -> String {
        let lines = self.view_lines();
        let (r0, r1) = if a.0 <= b.0 { (a.0, b.0) } else { (b.0, a.0) };
        let (c0, c1) = if a.1 <= b.1 { (a.1, b.1) } else { (b.1, a.1) };
        let mut out = String::new();
        let max_col = self.cols().saturating_sub(1);
        for (i, line) in lines.iter().enumerate().take(r1 + 1).skip(r0) {
            let (from, to) = if i == r0 && i == r1 {
                (c0, c1)
            } else if i == r0 {
                (c0, max_col)
            } else if i == r1 {
                (0, c1)
            } else {
                (0, max_col)
            };
            out.push_str(line.text_range(from, to.saturating_add(1)).trim_end());
            if i != r1 {
                out.push('\n');
            }
        }
        out
    }

    /// 光标在视图中的坐标（已扣除 scroll_offset）
    pub fn view_selection_head(&self) -> Option<(usize, usize)> {
        self.view_cursor()
    }

    pub fn scroll_view(&mut self, delta: isize) {
        let cur = self.scroll_offset as isize;
        let max = self.scrollback.len() as isize;
        self.scroll_offset = (cur + delta).clamp(0, max) as usize;
    }

    pub fn reset_view(&mut self) {
        self.scroll_offset = 0;
    }

    /// 纯文本导出：`plain` 模式读屏用它（架构文档 §6.4 主推模式）
    pub fn plain_text(&self, with_scrollback: bool) -> String {
        let mut out = String::new();
        if with_scrollback {
            for l in self.scrollback.iter() {
                out.push_str(&l.text());
                out.push('\n');
            }
        }
        for l in self.grid.window_text() {
            out.push_str(&l);
            out.push('\n');
        }
        // 尾部空行没有信息量，全部去掉；行间空行保留
        out.trim_end_matches('\n').to_string()
    }

    /// 当前屏幕的纯文本
    pub fn screen_text(&self) -> String {
        self.plain_text(false)
    }

    /// 网格视图（`ai screen read --mode grid`，架构文档 §6.2）。
    ///
    /// 与 `plain_text` 的分工：plain 给人/AI 读语义，grid 保留二维结构
    /// （行列号、光标位置），AI 需要定位屏幕上的某个元素时用它。
    pub fn grid_view(&self) -> GridView {
        let lines: Vec<String> = self.view_lines().iter().map(|l| l.text()).collect();
        GridView {
            cols: self.cols(),
            rows: self.rows(),
            cursor: self.view_cursor(),
            lines,
        }
    }

    /// 光标在视图中的行（考虑回看偏移）；不在视图内返回 None
    pub fn view_cursor(&self) -> Option<(usize, usize)> {
        if self.scroll_offset > 0 {
            return None;
        }
        Some((self.grid.cursor().y, self.grid.render_cursor_col()))
    }

    // ---- 状态 ----

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    pub fn scrollback_bytes(&self) -> usize {
        self.scrollback.bytes()
    }

    pub fn bytes_fed(&self) -> u64 {
        self.bytes_fed
    }

    /// 需要回传给远端的字节（光标位置报告等）
    pub fn take_response(&mut self) -> Vec<u8> {
        self.grid.take_response()
    }

    /// 清空屏幕与 scrollback
    pub fn clear(&mut self) {
        self.grid.erase_in_display(3);
        self.drain_grid();
        self.scrollback.clear();
        self.scroll_offset = 0;
    }
}

impl std::fmt::Debug for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Screen")
            .field("cols", &self.grid.cols())
            .field("rows", &self.grid.rows())
            .field("scrollback", &self.scrollback.len())
            .field("bytes_fed", &self.bytes_fed)
            .finish()
    }
}
