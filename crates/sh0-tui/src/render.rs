//! 把 `sh0_term` 的网格渲染成 ratatui 内容。
//!
//! 性能要点：一行 120 格若每格一个 Span，20 个标签页每帧就是几万个对象。
//! 这里把**相邻且样式相同**的格合并成一个 Span，实测通常能压到几个 Span。
//!
//! 反显（选区 / 光标）统一走 [`invert_style`]：先把一格解析成**最终**的
//! 前景/背景色，再交换两者。不要用 `Color::Default` 交换或 `Modifier::REVERSED`
//! 叠加——前者对默认色不生效，后者会和颜色交换互相抵消。

use ratatui::style::{Color as RColor, Modifier, Style};
use ratatui::text::{Line as RLine, Span};

use sh0_term::cell::{BOLD, Cell, Color, DIM, HIDDEN, ITALIC, Line, REVERSE, STRIKE, UNDERLINE};

use crate::theme::Theme;

/// `(fg, bg, flags, 是否反显)`：选区与普通格共用一套合并逻辑
type StyleKey = (Color, Color, u8, bool);

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
    let mut cur: Option<StyleKey> = None;

    for i in 0..cols.min(line.cells.len()) {
        let cell = &line.cells[i];
        if cell.is_wide_tail() {
            continue;
        }
        let selected = matches!(sel_cols, Some((a, b)) if i >= a && i <= b);
        let style_key = (cell.fg, cell.bg, cell.flags, selected);
        match cur {
            Some(k) if k == style_key => {}
            Some(k) => {
                if !buf.is_empty() {
                    spans.push(Span::styled(
                        std::mem::take(&mut buf),
                        style_for_key(k, theme),
                    ));
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
        && let Some(k) = cur
    {
        spans.push(Span::styled(std::mem::take(&mut buf), style_for_key(k, theme)));
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
            && let Some(k) = cur
        {
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
            spans.push(Span::styled(text, cursor_style(cell, theme)));
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

fn style_for_key((fg, bg, flags, inverted): StyleKey, theme: &Theme) -> Style {
    let base = to_style((fg, bg, flags), theme);
    if inverted {
        invert_style(base, theme)
    } else {
        base
    }
}

/// 解析一格的最终样式（含 REVERSE 处理）。
///
/// REVERSE 在这里就把前景/背景色换掉，**不再**额外加 `Modifier::REVERSED`，
/// 否则终端会再反一次，两次抵消。
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
    if flags & STRIKE != 0 {
        m |= Modifier::CROSSED_OUT;
    }

    // HIDDEN：ratatui 没有对应 modifier，用"前景=背景"实现视觉隐藏
    let (fg_c, bg_c) = if flags & REVERSE != 0 {
        (
            to_ratatui(bg, theme.default_bg()),
            to_ratatui(fg, theme.default_fg()),
        )
    } else {
        (
            to_ratatui(fg, theme.default_fg()),
            to_ratatui(bg, theme.default_bg()),
        )
    };
    let fg_c = if flags & HIDDEN != 0 { bg_c } else { fg_c };

    Style::default().fg(fg_c).bg(bg_c).add_modifier(m)
}

/// 把「已经解析好的最终样式」再做一次前景/背景互换。
///
/// 选区与光标共用。关键点：调换的是 `Style` 里已经落到具体 RGB 的 fg/bg，
/// 而不是 `sh0_term::Color`（`Default` 交换后两个角色仍映射回各自默认色，
/// 结果看起来完全没变——这正是之前光标/选区不显示的根因）。
fn invert_style(base: Style, theme: &Theme) -> Style {
    let fg = base.fg.unwrap_or(theme.default_fg());
    let bg = base.bg.unwrap_or(theme.default_bg());
    Style::default()
        .fg(bg)
        .bg(fg)
        .add_modifier(base.add_modifier)
}

fn cursor_style(cell: &Cell, theme: &Theme) -> Style {
    invert_style(to_style((cell.fg, cell.bg, cell.flags), theme), theme)
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
    Span::styled(text, cursor_style(cell, theme))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::preset("dark")
    }

    fn line_with(fg: Color, bg: Color) -> Line {
        let mut line = Line::blank(3, fg, bg, 0);
        for (i, ch) in ['a', 'b', 'c'].into_iter().enumerate() {
            line.cells[i] = Cell::new(ch, fg, bg, 0);
        }
        line
    }

    /// 选区必须真的交换最终颜色：默认色和带颜色的格都要可见
    #[test]
    fn selection_inverts_final_colors() {
        let t = theme();
        for (fg, bg) in [
            (Color::Default, Color::Default),
            (Color::Indexed(1), Color::Indexed(4)),
        ] {
            let line = line_with(fg, bg);
            let out = render_line(&line, 3, &t, Some((0, 1)));
            assert_eq!(out.spans.len(), 2, "ab 选中、c 未选中应合成 2 个 Span");
            let sel = &out.spans[0].style;
            let normal = &out.spans[1].style;
            assert_eq!(
                sel.fg.unwrap(),
                normal.bg.unwrap(),
                "选中格的前景应等于普通格的背景"
            );
            assert_eq!(
                sel.bg.unwrap(),
                normal.fg.unwrap(),
                "选中格的背景应等于普通格的前景"
            );
        }
    }

    /// 光标对默认色的空格也必须可见（之前 Default 交换不生效）
    #[test]
    fn cursor_inverts_default_cell() {
        let t = theme();
        let line = line_with(Color::Default, Color::Default);
        let out = render_line_with_cursor(&line, 3, 1, &t);
        let cursor = &out.spans[1].style;
        assert_eq!(cursor.fg.unwrap(), t.default_bg());
        assert_eq!(cursor.bg.unwrap(), t.default_fg());
    }

    /// REVERSE 格只反一次，不能和选区/光标叠加成“反两次”
    #[test]
    fn reverse_cell_is_inverted_once() {
        let t = theme();
        let mut line = line_with(Color::Default, Color::Default);
        line.cells[0].flags = REVERSE;
        let out = render_line(&line, 3, &t, None);
        let s = &out.spans[0].style;
        assert_eq!(s.fg.unwrap(), t.default_bg());
        assert_eq!(s.bg.unwrap(), t.default_fg());
    }
}
