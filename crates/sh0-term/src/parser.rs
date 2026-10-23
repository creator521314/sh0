//! vte 解析器适配：把字节流转成网格操作。
//!
//! `Perform` 的每个回调对应一类终端动作，这里只做"语义分派"，
//! 真正的网格编辑都在 [`crate::grid::Grid`] 上。
//!
//! 覆盖原则是**够用即止**：先把 `vim` / `htop` / `tmux` 真实会发的东西做对，
//! 罕见序列（DCS、Sixel、REP）先记日志忽略，不为了覆盖率写没验证过的分支。

use vte::{Params, Perform};

use crate::cell::{BOLD, Color, DIM, HIDDEN, ITALIC, REVERSE, STRIKE, UNDERLINE};
use crate::grid::MouseMode;
use crate::screen::Screen;

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.grid.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            // LF / VT / FF
            b'\n' | 0x0b | 0x0c => self.grid.linefeed(),
            b'\r' => self.grid.carriage_return(),
            0x08 => self.grid.backspace(),
            b'\t' => self.grid.tab(1),
            // BEL、SO/SI（字符集切换）、NUL 与其余 C0 一律忽略
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        // ESC ( B 这类字符集指定：整条吞掉，我们只处理 UTF-8
        if matches!(intermediates.first(), Some(b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/')) {
            return;
        }

        match (intermediates.first(), byte) {
            (None, b'7') => self.grid.save_cursor(),
            (None, b'8') => self.grid.restore_cursor(),
            (None, b'D') => self.grid.linefeed(),      // IND
            (None, b'E') => self.grid.cursor_next_line(1), // NEL
            (None, b'M') => self.grid.reverse_index(), // RI
            (None, b'H') => self.grid.set_tab_stop(),  // HTS
            (None, b'c') => self.grid.reset(),         // RIS
            (None, b'=') => self.grid.app_keypad = true,
            (None, b'>') => self.grid.app_keypad = false,
            (Some(b'#'), b'8') => self.grid.alignment_test(), // DECALN
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let private = matches!(intermediates.first(), Some(b'?'));
        let gt = matches!(intermediates.first(), Some(b'>'));

        // 先取参数，避免后续借用冲突
        let p0 = param(params, 0);
        let p1 = param(params, 1);
        let p2 = param(params, 2);

        match action {
            '@' => self.grid.insert_blanks(p0.max(1)),
            'A' => self.grid.cursor_up(p0.max(1)),
            'B' | 'e' => self.grid.cursor_down(p0.max(1)),
            'C' | 'a' => self.grid.cursor_forward(p0.max(1)),
            'D' => self.grid.cursor_back(p0.max(1)),
            'E' => self.grid.cursor_next_line(p0.max(1)),
            'F' => self.grid.cursor_prev_line(p0.max(1)),
            'G' | '`' => self.grid.cursor_col(p0.max(1)),
            'H' | 'f' => self.grid.cursor_goto(p0.max(1), p1.max(1)),
            'J' => self.grid.erase_in_display(p0),
            'K' => self.grid.erase_in_line(p0),
            'L' => self.grid.insert_lines(p0.max(1)),
            'M' => self.grid.delete_lines(p0.max(1)),
            'P' => self.grid.delete_chars(p0.max(1)),
            'S' => self.grid.scroll_up(p0.max(1)),
            'T' => self.grid.scroll_down(p0.max(1)),
            'X' => self.grid.erase_chars(p0.max(1)),
            'Z' => self.grid.back_tab(p0.max(1)),
            'd' => self.grid.cursor_row(p0.max(1)),
            'g' => self.grid.clear_tab_stop(p0),
            'm' => self.sgr(params),
            'r' if !private => self.grid.set_scroll_region(p0.max(1), p1.max(1)),
            's' if !private => self.grid.save_cursor(),
            'u' if !private => self.grid.restore_cursor(),
            'h' => self.set_mode(private, params, true),
            'l' => self.set_mode(private, params, false),
            'n' => self.device_status_report(p0),
            'c' if gt => self
                .grid
                .push_response(b"\x1b[>0;95;0c"),
            'c' if !private => self.grid.push_response(b"\x1b[?6c"),
            't' => {} // 窗口操作：本地不需要
            _ => {
                if !action.is_ascii_alphanumeric() || action == 'q' || action == 'p' {
                    // 极低频序列，忽略即可
                }
            }
        }
        let _ = p2;
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let first = params.first().map(|b| String::from_utf8_lossy(b).to_string());
        match first.as_deref() {
            // OSC 0 / 2：窗口标题
            Some("0") | Some("2") => {
                if let Some(t) = params.get(1) {
                    self.title = String::from_utf8_lossy(t).to_string();
                }
            }
            // OSC 52：剪贴板（需求 FR-2.10）
            Some("52") => {
                let sel = params.get(1).map(|b| String::from_utf8_lossy(b).to_string());
                if let Some(s) = sel
                    && let Some(payload) = s.strip_prefix("c;")
                {
                    self.clipboard = Some(payload.to_string());
                }
            }
            // OSC 4（调色板）/ 8（超链接）/ 10-19（前景背景查询）忽略
            _ => {}
        }
    }
}

/// 取第 i 个参数；缺省或显式 0 时都返回 0（调用方自行 `.max(1)`）。
///
/// 子参数（如 `38:2::r:g:b`）只取第一个。
fn param(params: &Params, i: usize) -> usize {
    params.iter().nth(i).and_then(|s| s.first()).map(|v| *v as usize).unwrap_or(0)
}

/// 参数扁平化，供 SGR 这类多参数序列使用
fn flatten(params: &Params) -> Vec<u16> {
    params.iter().filter_map(|s| s.first().copied()).collect()
}

impl Screen {
    fn sgr(&mut self, params: &Params) {
        let vals = flatten(params);
        let mut i = 0;
        let (mut fg, mut bg, mut flags) = self.grid.cursor().attrs();

        while i < vals.len() {
            match vals[i] {
                0 => {
                    fg = Color::Default;
                    bg = Color::Default;
                    flags = 0;
                }
                1 => flags |= BOLD,
                2 => flags |= DIM,
                3 => flags |= ITALIC,
                4 => flags |= UNDERLINE,
                5 | 6 => {} // blink：不渲染，也不占属性位
                7 => flags |= REVERSE,
                8 => flags |= HIDDEN,
                9 => flags |= STRIKE,
                21 => flags &= !BOLD,
                22 => flags &= !(BOLD | DIM),
                23 => flags &= !ITALIC,
                24 => flags &= !UNDERLINE,
                27 => flags &= !REVERSE,
                28 => flags &= !HIDDEN,
                29 => flags &= !STRIKE,
                30..=37 => fg = Color::Indexed(vals[i] as u8 - 30),
                38 => {
                    if let Some(c) = extended_color(&vals, &mut i) {
                        fg = c;
                    }
                }
                39 => fg = Color::Default,
                40..=47 => bg = Color::Indexed(vals[i] as u8 - 40),
                48 => {
                    if let Some(c) = extended_color(&vals, &mut i) {
                        bg = c;
                    }
                }
                49 => bg = Color::Default,
                90..=97 => fg = Color::Indexed(vals[i] as u8 - 90 + 8),
                100..=107 => bg = Color::Indexed(vals[i] as u8 - 100 + 8),
                _ => {}
            }
            i += 1;
        }

        let cur = self.grid.cursor_mut();
        cur.fg = fg;
        cur.bg = bg;
        cur.flags = flags;
    }

    /// SM / RM 与 DEC 私有模式
    fn set_mode(&mut self, private: bool, params: &Params, enable: bool) {
        for v in flatten(params) {
            match (private, v) {
                (false, 4) => self.grid.insert_mode = enable,
                (false, 20) => {} // LNM：本地无换行模式概念
                (true, 1) => self.grid.app_cursor_keys = enable,
                (true, 6) => {
                    self.grid.origin_mode = enable;
                    self.grid.cursor_goto(1, 1);
                }
                (true, 7) => self.grid.auto_wrap = enable,
                (true, 12) | (true, 25) => self.grid.cursor_visible = enable,
                (true, 47) | (true, 1047) | (true, 1049) => {
                    // 备用屏幕：M1 不实现，视为普通清屏
                    if enable {
                        self.grid.erase_in_display(2);
                    }
                }
                (true, 1000) => self.grid.mouse_mode = pick(enable, MouseMode::Click),
                (true, 1002) => self.grid.mouse_mode = pick(enable, MouseMode::Drag),
                (true, 1003) => self.grid.mouse_mode = pick(enable, MouseMode::Motion),
                (true, 1006) => self.grid.sgr_mouse = enable,
                (true, 2004) => self.grid.bracketed_paste = enable,
                _ => {}
            }
        }
    }

    /// DSR：只回应"光标位置报告"，它是很多 TUI 程序测量终端尺寸的手段
    fn device_status_report(&mut self, kind: usize) {
        match kind {
            5 => self.grid.push_response(b"\x1b[0n"),
            6 => {
                let (x, y) = (self.grid.cursor().x, self.grid.cursor().y);
                let (top, _) = self.grid.scroll_region();
                let row = if self.grid.origin_mode { y.saturating_sub(top) } else { y };
                let resp = format!("\x1b[{};{}R", row + 1, x.min(self.grid.cols() - 1) + 1);
                self.grid.push_response(resp.as_bytes());
            }
            _ => {}
        }
    }
}

fn pick(enable: bool, on: MouseMode) -> MouseMode {
    if enable {
        on
    } else {
        MouseMode::Off
    }
}

/// 处理 `38;5;n`（256 色）与 `38;2;r;g;b`（真彩）。
/// 会把索引推进到本段最后一个参数。
fn extended_color(vals: &[u16], i: &mut usize) -> Option<Color> {
    match vals.get(*i + 1)? {
        5 => {
            let idx = *vals.get(*i + 2)? as u8;
            *i += 2;
            Some(Color::Indexed(idx))
        }
        2 => {
            let r = *vals.get(*i + 2)? as u8;
            let g = *vals.get(*i + 3)? as u8;
            let b = *vals.get(*i + 4)? as u8;
            *i += 4;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}
