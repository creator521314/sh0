//! 字符网格：可视区 + 光标 + 滚动区域 + 各种编辑原语。
//!
//! 这一层只做"网格该长什么样"，不碰字节流解析（见 `parser.rs`），
//! 也不管 scrollback 的存放（见 `scrollback.rs`）——被滚出屏幕的行会落到
//! [`Grid::take_scrolled_out`]，由 `Screen` 收进环形缓冲。

use crate::cell::{ATTR_MASK, Cell, Color, Line, WIDE_TAIL, char_width};

/// 光标位置与"接下来写出的字符长什么样"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub x: usize,
    pub y: usize,
    pub fg: Color,
    pub bg: Color,
    pub flags: u8,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            fg: Color::Default,
            bg: Color::Default,
            flags: 0,
        }
    }
}

impl Cursor {
    /// 写字符用的属性（去掉宽字符续位标记）
    pub fn attrs(&self) -> (Color, Color, u8) {
        (self.fg, self.bg, self.flags & ATTR_MASK)
    }
}

/// 鼠标上报模式。M1 只记录，不上报（没有鼠标支持）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    Off,
    /// ?1000 普通点击
    Click,
    /// ?1002 按下+拖动
    Drag,
    /// ?1003 任意移动
    Motion,
}

/// 可视区网格
#[derive(Debug, Clone)]
pub struct Grid {
    cols: usize,
    rows: usize,
    lines: Vec<Line>,
    cursor: Cursor,
    saved: Option<Cursor>,
    /// DECSTBM 滚动区域上界（inclusive）
    scroll_top: usize,
    /// DECSTBM 滚动区域下界（inclusive）
    scroll_bottom: usize,
    tabs: Vec<bool>,

    // ---- 模式 ----
    /// DECAWM 自动换行
    pub auto_wrap: bool,
    /// DECOM 原点模式（光标坐标相对滚动区域）
    pub origin_mode: bool,
    /// IRM 插入模式
    pub insert_mode: bool,
    pub cursor_visible: bool,
    /// DECCKM：应用光标键序列（方向键发 `ESC O A` 而非 `ESC [ A`）
    pub app_cursor_keys: bool,
    /// DECKPAM：小键盘应用模式
    pub app_keypad: bool,
    /// ?2004 括号粘贴
    pub bracketed_paste: bool,
    pub mouse_mode: MouseMode,
    /// SGR 像素坐标鼠标上报（?1006）
    pub sgr_mouse: bool,

    /// 被滚出屏幕顶部的行，交给 Screen 收进 scrollback
    scrolled_out: Vec<Line>,
    /// ED 3 触发的"连 scrollback 一起清"请求
    clear_scrollback: bool,
    /// 需要回传给远端的应答字节（DSR / DA / CPR）
    response: Vec<u8>,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            lines: vec![Line::blank(cols, Color::Default, Color::Default, 0); rows],
            cursor: Cursor::default(),
            saved: None,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            tabs: default_tabs(cols),
            auto_wrap: true,
            origin_mode: false,
            insert_mode: false,
            cursor_visible: true,
            app_cursor_keys: false,
            app_keypad: false,
            bracketed_paste: false,
            mouse_mode: MouseMode::Off,
            sgr_mouse: false,
            scrolled_out: Vec::new(),
            clear_scrollback: false,
            response: Vec::new(),
        }
    }

    // ---- 基本访问 ----

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn cursor_mut(&mut self) -> &mut Cursor {
        &mut self.cursor
    }

    /// 渲染光标列：`x == cols` 表示"下一个字符会换到行首"，显示时钳到最后一列
    pub fn render_cursor_col(&self) -> usize {
        self.cursor.x.min(self.cols - 1)
    }

    pub fn line(&self, y: usize) -> Option<&Line> {
        self.lines.get(y)
    }

    pub fn scroll_region(&self) -> (usize, usize) {
        (self.scroll_top, self.scroll_bottom)
    }

    /// 取走滚出屏幕的行（scrollback 用）
    pub fn take_scrolled_out(&mut self) -> Vec<Line> {
        std::mem::take(&mut self.scrolled_out)
    }

    /// 取走并清除"连 scrollback 一起清屏"标志（ED 3）
    pub fn take_clear_scrollback(&mut self) -> bool {
        let v = self.clear_scrollback;
        self.clear_scrollback = false;
        v
    }

    /// 取走待回传远端的应答（光标位置报告等）
    pub fn take_response(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.response)
    }

    pub fn push_response(&mut self, bytes: &[u8]) {
        self.response.extend_from_slice(bytes);
    }

    // ---- 尺寸 ----

    /// 窗口尺寸变化（需求 FR-2.4）。
    ///
    /// 策略：行数变多时在**顶部**补空行（保留底部内容，与终端滚动方向一致），
    /// 行数变少时从顶部丢弃；列数变化时逐行截断或补空白。
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        let new_cols = new_cols.max(1);
        let new_rows = new_rows.max(1);
        if new_cols == self.cols && new_rows == self.rows {
            return;
        }

        let (fg, bg, flags) = self.cursor.attrs();

        // 行
        if new_rows > self.rows {
            let add = new_rows - self.rows;
            let mut new_lines = Vec::with_capacity(new_rows);
            for _ in 0..add {
                new_lines.push(Line::blank(self.cols, fg, bg, flags));
            }
            new_lines.append(&mut self.lines);
            self.lines = new_lines;
            self.cursor.y += add;
        } else if new_rows < self.rows {
            // 高度变小时顶部内容移出屏幕——推给 scrollback 而不是丢弃，
            // 否则窗口一缩小历史就没了（AC-3 要求重连前输出仍可翻到）
            let drop = self.rows - new_rows;
            for line in self.lines.drain(..drop) {
                self.scrolled_out.push(line);
            }
            self.cursor.y = self.cursor.y.saturating_sub(drop);
        }

        // 列
        if new_cols != self.cols {
            for line in &mut self.lines {
                resize_line(line, new_cols, fg, bg, flags);
            }
        }

        self.cols = new_cols;
        self.rows = new_rows;
        self.scroll_top = 0;
        self.scroll_bottom = new_rows - 1;
        self.tabs = default_tabs(new_cols);
        self.cursor.x = self.cursor.x.min(new_cols - 1);
        self.cursor.y = self.cursor.y.min(new_rows - 1);
    }

    // ---- 写入 ----

    /// 在当前光标处写一个字符。
    ///
    /// 三处容易出错：(1) 宽字符占两格，第二格要打 `WIDE_TAIL` 标记；
    /// (2) 零宽组合字符要**附加到前一格**而不是推进光标（AC-9）；
    /// (3) 自动换行是"写下一个字符时才换行"，不是写完最后一列立刻换。
    pub fn put_char(&mut self, c: char) {
        let w = char_width(c);

        if w == 0 {
            self.attach_combining(c);
            return;
        }

        // 待换行：光标已停在最后一列之外
        if self.cursor.x + w > self.cols {
            if self.auto_wrap {
                self.wrap_line();
            } else {
                self.cursor.x = self.cols.saturating_sub(w);
            }
        }

        if self.insert_mode {
            self.insert_blanks(w);
        }

        let (fg, bg, flags) = self.cursor.attrs();
        let y = self.cursor.y;
        let x = self.cursor.x;
        let line = &mut self.lines[y];

        // 覆盖写：目标格上旧的组合记号要清掉
        line.comb.retain(|(col, _)| *col != x && *col != x + 1);
        line.cells[x] = Cell::new(c, fg, bg, flags);
        if w == 2 && x + 1 < self.cols {
            line.cells[x + 1] = Cell::wide_tail(fg, bg, flags);
        }

        self.cursor.x += w;
    }

    /// 零宽组合字符：附加到光标左邻格；行首则附加到上一行末格
    fn attach_combining(&mut self, c: char) {
        let (y, x) = if self.cursor.x > 0 {
            (self.cursor.y, self.cursor.x - 1)
        } else if self.cursor.y > 0 {
            (self.cursor.y - 1, self.cols - 1)
        } else {
            return;
        };
        self.lines[y].push_combining(x, c);
    }

    fn wrap_line(&mut self) {
        self.cursor.x = 0;
        self.linefeed();
    }

    /// 全屏重置（RIS / ESC c）
    pub fn reset(&mut self) {
        let (fg, bg, flags) = self.cursor.attrs();
        self.lines = vec![Line::blank(self.cols, fg, bg, flags); self.rows];
        self.cursor = Cursor::default();
        self.saved = None;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.tabs = default_tabs(self.cols);
        self.auto_wrap = true;
        self.origin_mode = false;
        self.insert_mode = false;
        self.cursor_visible = true;
        self.app_cursor_keys = false;
        self.app_keypad = false;
        self.bracketed_paste = false;
        self.mouse_mode = MouseMode::Off;
        self.sgr_mouse = false;
    }

    // ---- 光标移动 ----

    pub fn carriage_return(&mut self) {
        self.cursor.x = 0;
    }

    /// LF / VT / FF
    pub fn linefeed(&mut self) {
        if self.cursor.y == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor.y < self.rows - 1 {
            self.cursor.y += 1;
        }
    }

    /// RI：反向下移，到顶时区域内容下滚
    pub fn reverse_index(&mut self) {
        if self.cursor.y == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor.y > 0 {
            self.cursor.y -= 1;
        }
    }

    pub fn backspace(&mut self) {
        self.cursor.x = self.cursor.x.saturating_sub(1);
    }

    /// HT：跳到下一个 tab stop
    pub fn tab(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            let next = (self.cursor.x + 1..self.cols)
                .find(|&i| self.tabs.get(i) == Some(&true))
                .unwrap_or(self.cols - 1);
            self.cursor.x = next;
        }
        self.cursor.x = self.cursor.x.min(self.cols - 1);
    }

    /// CBT：反向跳 tab stop
    pub fn back_tab(&mut self, count: usize) {
        for _ in 0..count.max(1) {
            let prev = (0..self.cursor.x)
                .rev()
                .find(|&i| self.tabs.get(i) == Some(&true))
                .unwrap_or(0);
            self.cursor.x = prev;
        }
    }

    /// HTS：在当前列设 tab stop
    pub fn set_tab_stop(&mut self) {
        let x = self.cursor.x.min(self.cols - 1);
        if let Some(t) = self.tabs.get_mut(x) {
            *t = true;
        }
    }

    pub fn clear_tab_stop(&mut self, mode: usize) {
        match mode {
            0 => {
                let x = self.cursor.x.min(self.cols - 1);
                if let Some(t) = self.tabs.get_mut(x) {
                    *t = false;
                }
            }
            3 => self.tabs.iter_mut().for_each(|t| *t = false),
            _ => {}
        }
    }

    pub fn cursor_up(&mut self, n: usize) {
        let top = if self.cursor.y >= self.scroll_top {
            self.scroll_top
        } else {
            0
        };
        self.cursor.y = self.cursor.y.saturating_sub(n).max(top);
    }

    pub fn cursor_down(&mut self, n: usize) {
        let bottom = if self.cursor.y <= self.scroll_bottom {
            self.scroll_bottom
        } else {
            self.rows - 1
        };
        self.cursor.y = (self.cursor.y + n).min(bottom);
    }

    pub fn cursor_forward(&mut self, n: usize) {
        self.cursor.x = (self.cursor.x + n).min(self.cols - 1);
    }

    pub fn cursor_back(&mut self, n: usize) {
        self.cursor.x = self.cursor.x.saturating_sub(n);
    }

    /// CNL / CPL：下移/上移 n 行并回到行首
    pub fn cursor_next_line(&mut self, n: usize) {
        self.cursor_down(n);
        self.cursor.x = 0;
    }

    pub fn cursor_prev_line(&mut self, n: usize) {
        self.cursor_up(n);
        self.cursor.x = 0;
    }

    /// CUP / HVP：参数 1-based；DECOM 下相对滚动区域原点
    pub fn cursor_goto(&mut self, row: usize, col: usize) {
        let (fg, bg, flags) = self.cursor.attrs();
        let y = if self.origin_mode {
            (self.scroll_top + row.saturating_sub(1)).min(self.scroll_bottom)
        } else {
            row.saturating_sub(1)
        };
        self.cursor = Cursor {
            x: col.saturating_sub(1).min(self.cols - 1),
            y: y.min(self.rows - 1),
            fg,
            bg,
            flags,
        };
    }

    /// CHA / HPA：光标移到本行第 col 列（1-based）
    pub fn cursor_col(&mut self, col: usize) {
        self.cursor.x = col.saturating_sub(1).min(self.cols - 1);
    }

    /// VPA：光标移到第 row 行（1-based）
    pub fn cursor_row(&mut self, row: usize) {
        let y = if self.origin_mode {
            (self.scroll_top + row.saturating_sub(1)).min(self.scroll_bottom)
        } else {
            row.saturating_sub(1)
        };
        self.cursor.y = y.min(self.rows - 1);
    }

    pub fn save_cursor(&mut self) {
        self.saved = Some(self.cursor);
    }

    pub fn restore_cursor(&mut self) {
        if let Some(c) = self.saved {
            self.cursor = c;
        }
    }

    // ---- 滚动区域 ----

    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let top = top.saturating_sub(1).min(self.rows - 1);
        let bottom = bottom.saturating_sub(1).min(self.rows - 1);
        if top >= bottom {
            // 退化区域：xterm 视为全屏
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
        } else {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        }
        // DECSTBM 会把光标移到原点
        self.cursor_goto(1, 1);
    }

    /// 区域内容上滚 n 行：顶部行移出（scroll_top == 0 时进 scrollback），底部补空行
    pub fn scroll_up(&mut self, n: usize) {
        let (fg, bg, flags) = self.cursor.attrs();
        let (top, bottom) = (self.scroll_top, self.scroll_bottom.min(self.rows - 1));
        for _ in 0..n.min(bottom - top + 1) {
            let line = self.lines.remove(top);
            if top == 0 {
                self.scrolled_out.push(line);
            }
            self.lines.insert(bottom, Line::blank(self.cols, fg, bg, flags));
        }
    }

    /// 区域内容下滚 n 行
    pub fn scroll_down(&mut self, n: usize) {
        let (fg, bg, flags) = self.cursor.attrs();
        let (top, bottom) = (self.scroll_top, self.scroll_bottom.min(self.rows - 1));
        for _ in 0..n.min(bottom - top + 1) {
            self.lines.remove(bottom);
            self.lines.insert(top, Line::blank(self.cols, fg, bg, flags));
        }
    }

    // ---- 擦除 ----

    /// ED：0=光标后，1=光标前，2=全屏，3=全屏+scrollback
    pub fn erase_in_display(&mut self, mode: usize) {
        let (fg, bg, flags) = self.cursor.attrs();
        let cols = self.cols;
        let rows = self.rows;
        let cy = self.cursor.y;
        let blank = move || Line::blank(cols, fg, bg, flags);

        match mode {
            0 => {
                self.erase_in_line(0);
                for y in cy + 1..rows {
                    self.lines[y] = blank();
                }
            }
            1 => {
                self.erase_in_line(1);
                for y in 0..cy {
                    self.lines[y] = blank();
                }
            }
            2 => {
                for y in 0..rows {
                    self.lines[y] = blank();
                }
            }
            3 => {
                for y in 0..rows {
                    self.lines[y] = blank();
                }
                self.clear_scrollback = true;
            }
            _ => {}
        }
    }

    /// EL：0=光标后，1=光标前，2=整行
    pub fn erase_in_line(&mut self, mode: usize) {
        let (fg, bg, flags) = self.cursor.attrs();
        let x = self.cursor.x.min(self.cols - 1);
        let line = &mut self.lines[self.cursor.y];
        let mut cell = Cell::blank(fg, bg, flags);

        // 宽字符被擦掉一半时，另一半也要一起清，否则会留下半个字符
        if x > 0 && line.cells[x].is_wide_tail() {
            line.cells[x - 1] = Cell::blank(fg, bg, flags);
        }
        if line.cells[x].width() == 2 && x + 1 < line.cells.len() {
            line.cells[x + 1] = Cell::blank(fg, bg, flags);
        }
        let _ = &mut cell;

        match mode {
            0 => {
                for c in &mut line.cells[x..] {
                    *c = Cell::blank(fg, bg, flags);
                }
                line.comb.retain(|(col, _)| *col < x);
            }
            1 => {
                for c in &mut line.cells[..=x] {
                    *c = Cell::blank(fg, bg, flags);
                }
                line.comb.retain(|(col, _)| *col > x);
            }
            2 => {
                for c in &mut line.cells {
                    *c = Cell::blank(fg, bg, flags);
                }
                line.comb.clear();
            }
            _ => {}
        }
    }

    /// ECH：从光标处清 n 个字符（不移动内容）
    pub fn erase_chars(&mut self, n: usize) {
        let (fg, bg, flags) = self.cursor.attrs();
        let x = self.cursor.x.min(self.cols - 1);
        let line = &mut self.lines[self.cursor.y];
        for c in line.cells.iter_mut().skip(x).take(n.max(1)) {
            *c = Cell::blank(fg, bg, flags);
        }
        line.comb.retain(|(col, _)| *col < x || *col >= x + n.max(1));
    }

    // ---- 行/字符编辑 ----

    /// IL：在光标行插入 n 个空行，区域内底部行被挤出
    pub fn insert_lines(&mut self, n: usize) {
        if self.cursor.y < self.scroll_top || self.cursor.y > self.scroll_bottom {
            return;
        }
        let (fg, bg, flags) = self.cursor.attrs();
        let y = self.cursor.y;
        let bottom = self.scroll_bottom.min(self.rows - 1);
        for _ in 0..n.min(bottom - y + 1) {
            self.lines.remove(bottom);
            self.lines.insert(y, Line::blank(self.cols, fg, bg, flags));
        }
        self.cursor.x = 0;
    }

    /// DL：删除光标行起 n 行，底部补空行
    pub fn delete_lines(&mut self, n: usize) {
        if self.cursor.y < self.scroll_top || self.cursor.y > self.scroll_bottom {
            return;
        }
        let (fg, bg, flags) = self.cursor.attrs();
        let y = self.cursor.y;
        let bottom = self.scroll_bottom.min(self.rows - 1);
        for _ in 0..n.min(bottom - y + 1) {
            self.lines.remove(y);
            self.lines.insert(bottom, Line::blank(self.cols, fg, bg, flags));
        }
        self.cursor.x = 0;
    }

    /// ICH：光标处插入 n 个空白，右侧内容右移
    pub fn insert_blanks(&mut self, n: usize) {
        let (fg, bg, flags) = self.cursor.attrs();
        let x = self.cursor.x.min(self.cols - 1);
        let line = &mut self.lines[self.cursor.y];
        for _ in 0..n.min(self.cols - x) {
            line.cells.insert(x, Cell::blank(fg, bg, flags));
            line.cells.pop();
        }
        line.comb.retain(|(col, _)| *col < x);
    }

    /// DCH：删除光标处 n 个字符，右侧左移，末尾补空白
    pub fn delete_chars(&mut self, n: usize) {
        let (fg, bg, flags) = self.cursor.attrs();
        let x = self.cursor.x.min(self.cols - 1);
        let n = n.max(1).min(self.cols - x);
        let line = &mut self.lines[self.cursor.y];
        line.cells.drain(x..x + n);
        line.cells.resize(self.cols, Cell::blank(fg, bg, flags));
        line.comb.retain(|(col, _)| *col < x);
    }

    /// DECALN（ESC # 8）：全屏填 'E'，用于对齐测试
    pub fn alignment_test(&mut self) {
        let (fg, bg, flags) = self.cursor.attrs();
        for line in &mut self.lines {
            for c in &mut line.cells {
                *c = Cell::new('E', fg, bg, flags);
            }
            line.comb.clear();
        }
        self.cursor.x = 0;
        self.cursor.y = 0;
    }

    // ---- 输出 ----

    /// 可视区逐行文本（尾部空白已去掉）
    pub fn window_text(&self) -> Vec<String> {
        self.lines.iter().map(|l| l.text()).collect()
    }

    pub fn lines(&self) -> &[Line] {
        &self.lines
    }
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|i| i % 8 == 0 && i != 0).collect()
}

fn resize_line(line: &mut Line, cols: usize, fg: Color, bg: Color, flags: u8) {
    line.cells.resize(cols, Cell::blank(fg, bg, flags));
    line.comb.retain(|(col, _)| *col < cols);
    // 宽字符被截断时，首格降级为空格，避免留下悬空的半个字符
    if let Some(last) = line.cells.last()
        && last.is_wide_tail()
        && cols > 0
    {
        let n = line.cells.len();
        if n >= 2 && char_width(line.cells[n - 2].ch) == 2 {
            line.cells[n - 2] = Cell::blank(fg, bg, flags);
        }
        line.cells[n - 1].flags &= !WIDE_TAIL;
    }
}
