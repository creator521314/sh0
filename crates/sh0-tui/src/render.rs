//! 把 `sh0_term` 的网格渲染成 ratatui 内容。
//!
//! 性能要点：一行 120 格若每格一个 Span，20 个标签页每帧就是几万个对象。
//! 这里把**相邻且样式相同**的格合并成一个 Span，实测通常能压到几个 Span。

use ratatui::style::{Color as RColor, Modifier, Style};
use ratatui::text::{Line as RLine, Span};

use sh0_term::cell::{BOLD, Cell, Color, DIM, HIDDEN, ITALIC, Line, REVERSE, STRIKE, UNDERLINE};

use crate::theme::Theme;

/// 一行网格 → 一个 ratatui 行
///
/// `sel_cols` 是该行被选中的列范围（闭区间），选中部分按反显渲染。
/// 选区判断并入样式 key，这样相邻同属性的格仍然能合并成少量 Span。
pub fn render_line(
    line: &Line,
    cols: usize,
    theme: &Theme,
    sel_cols: Option<(usize, usize)>,
) -> RLine<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<(Color, Color, u8)> = None;

    for i in 0..cols.min(line.cells.len()) {
        let cell = &line.cells[i];
        if cell.is_wide_tail() {
            continue;
        }
        let selected = matches!(sel_cols, Some((a, b)) if i >= a && i <= b);
        let style_key = if selected {
            (cell.bg, cell.fg, cell.flags ^ REVERSE)
        } else {
            (cell.fg, cell.bg, cell.flags)
        };
        match cur {
            Some(k) if k == style_key => {}
            Some(k) => {
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), to_style(k, theme)));
                }
                cur = Some(style_key);
            }
            None => cur = Some(style_key),
        }
        buf.push(cell.ch);
        if let Some(extra) = line.combining(i) {
            buf.push_str(extra);
        }
    }

    if !buf.is_empty()
        && let Some(k) = cur {
            spans.push(Span::styled(std::mem::take(&mut buf), to_style(k, theme)));
        }
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    RLine::from(spans)
}

/// 带光标高亮的一行：光标所在格单独切成反显 Span
pub fn render_line_with_cursor(
    line: &Line,
    cols: usize,
    cursor_col: usize,
    theme: &Theme,
) -> RLine<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<(Color, Color, u8)> = None;
    let flush = |buf: &mut String, cur: Option<(Color, Color, u8)>, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty()
            && let Some(k) = cur {
                spans.push(Span::styled(std::mem::take(buf), to_style(k, theme)));
            }
    };

    for i in 0..cols.min(line.cells.len()) {
        let cell = &line.cells[i];
        if i == cursor_col {
            flush(&mut buf, cur, &mut spans);
            cur = None;
            let mut text = cell.ch.to_string();
            if let Some(extra) = line.combining(i) {
                text.push_str(extra);
            }
            let key = (cell.bg, cell.fg, cell.flags); // 前景背景互换 = 反显
            spans.push(Span::styled(text, to_style(key, theme)));
            continue;
        }
        if cell.is_wide_tail() {
            continue;
        }
        let style_key = (cell.fg, cell.bg, cell.flags);
        match cur {
            Some(k) if k == style_key => {}
            Some(k) => {
                flush(&mut buf, Some(k), &mut spans);
                cur = Some(style_key);
            }
            None => cur = Some(style_key),
        }
        buf.push(cell.ch);
        if let Some(extra) = line.combining(i) {
            buf.push_str(extra);
        }
    }
    flush(&mut buf, cur, &mut spans);

    if spans.is_empty() {
        spans.push(Span::styled(" ", Style::default().bg(theme.default_fg())));
    }
    RLine::from(spans)
}

fn to_style((fg, bg, flags): (Color, Color, u8), theme: &Theme) -> Style {
    let mut m = Modifier::empty();
    if flags & BOLD != 0 {
        m |= Modifier::BOLD;
    }
    if flags & DIM != 0 {
        m |= Modifier::DIM;
    }
    if flags & ITALIC != 0 {
        m |= Modifier::ITALIC;
    }
    if flags & UNDERLINE != 0 {
        m |= Modifier::UNDERLINED;
    }
    if flags & REVERSE != 0 {
        m |= Modifier::REVERSED;
    }
    if flags & STRIKE != 0 {
        m |= Modifier::CROSSED_OUT;
    }

    // HIDDEN：ratatui 没有对应 modifier，用"前景=背景"实现视觉隐藏
    let (fg_c, bg_c) = if flags & REVERSE != 0 {
        (to_ratatui(bg, theme.default_bg()), to_ratatui(fg, theme.default_fg()))
    } else {
        (to_ratatui(fg, theme.default_fg()), to_ratatui(bg, theme.default_bg()))
    };
    let fg_c = if flags & HIDDEN != 0 { bg_c } else { fg_c };

    Style::default().fg(fg_c).bg(bg_c).add_modifier(m)
}

fn to_ratatui(c: Color, default: RColor) -> RColor {
    match c.to_rgb() {
        Some((r, g, b)) => RColor::Rgb(r, g, b),
        None => default,
    }
}

/// 渲染光标：把光标所在格反显出来
pub fn render_cursor_cell(cell: &Cell, theme: &Theme) -> Span<'static> {
    let text = cell.ch.to_string();
    let fg = to_ratatui(cell.bg, theme.default_bg());
    let bg = to_ratatui(cell.fg, theme.default_fg());
    Span::styled(text, Style::default().fg(fg).bg(bg))
}
