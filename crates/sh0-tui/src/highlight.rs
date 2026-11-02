//! 终端输出的正则高亮：把 `highlight.toml` 的规则叠加到 `sh0_term::Line` 上。
//!
//! 性能取舍：
//! - 规则匹配结果按**行文本**缓存（`Spans` 是字符区间，不含原始终端颜色），
//!   同一行文本重复出现时不再跑正则；
//! - 只有真的命中时才 clone 一行改颜色，未命中直接借用原行；
//! - 缓存有上限，滚动很久也不会无限增长。

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use regex::Regex;
use sh0_store::{HighlightColor, HighlightRule};
use sh0_term::cell::{BOLD, Cell, Color, DIM, ITALIC, Line, UNDERLINE};

/// 叠加到一格上的样式；`None` 表示保留该格原来的颜色
#[derive(Debug, Clone, Copy)]
struct CellStyle {
    fg: Option<Color>,
    bg: Option<Color>,
    flags: u8,
}

/// 一段命中的字符区间 `[start, end)`（字符下标，不是字节）
#[derive(Debug, Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
    style: CellStyle,
}

struct CompiledRule {
    regex: Regex,
    style: CellStyle,
}

pub struct Highlighter {
    rules: Vec<CompiledRule>,
    cache: RefCell<HashMap<String, Arc<Vec<Span>>>>,
}

impl Highlighter {
    pub fn new(rules: Vec<HighlightRule>) -> Self {
        // 调用方（store）已按 priority 降序；这里再排一次，保证直接构造也正确
        let mut rules = rules;
        rules.sort_by_key(|r| std::cmp::Reverse(r.priority));

        let rules = rules
            .into_iter()
            .filter_map(|r| {
                let style = CellStyle {
                    fg: r.style.fg.as_ref().map(to_color),
                    bg: r.style.bg.as_ref().map(to_color),
                    flags: style_flags(&r.style),
                };
                // 没有颜色也没有属性的规则不会产生任何效果，直接丢掉
                if style.fg.is_none() && style.bg.is_none() && style.flags == 0 {
                    None
                } else {
                    Some(CompiledRule {
                        regex: r.regex,
                        style,
                    })
                }
            })
            .collect();

        Self {
            rules,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// 返回叠加高亮后的行；没有规则命中时借用原行，避免无谓 clone。
    pub fn highlight<'a>(&self, line: &'a Line) -> Cow<'a, Line> {
        if self.rules.is_empty() {
            return Cow::Borrowed(line);
        }
        let (text, cell_of_char) = text_and_cells(line);
        if text.is_empty() {
            return Cow::Borrowed(line);
        }
        let spans = self.spans_for(&text);
        if spans.is_empty() {
            return Cow::Borrowed(line);
        }

        let mut out = line.clone();
        for span in spans.iter() {
            let Some(&start_cell) = cell_of_char.get(span.start) else {
                continue;
            };
            let Some(&end_cell) = cell_of_char.get(span.end.saturating_sub(1)) else {
                continue;
            };
            for i in start_cell..=end_cell {
                if let Some(cell) = out.cells.get_mut(i) {
                    apply(cell, span.style);
                }
            }
        }
        Cow::Owned(out)
    }

    fn spans_for(&self, text: &str) -> Arc<Vec<Span>> {
        if let Some(hit) = self.cache.borrow().get(text) {
            return hit.clone();
        }

        let chars = text.chars().count();
        let mut claimed = vec![false; chars];
        let mut spans = Vec::new();

        for rule in &self.rules {
            for m in rule.regex.find_iter(text) {
                if m.start() == m.end() {
                    continue; // 空匹配没有可着色的字符
                }
                let start = text[..m.start()].chars().count();
                let end = text[..m.end()].chars().count();
                let mut run: Option<usize> = None;
                for (ci, claimed_cell) in claimed
                    .iter_mut()
                    .enumerate()
                    .take(end.min(chars))
                    .skip(start)
                {
                    if *claimed_cell {
                        if let Some(s) = run.take() {
                            spans.push(Span {
                                start: s,
                                end: ci,
                                style: rule.style,
                            });
                        }
                    } else {
                        *claimed_cell = true;
                        if run.is_none() {
                            run = Some(ci);
                        }
                    }
                }
                if let Some(s) = run {
                    spans.push(Span {
                        start: s,
                        end: end.min(chars),
                        style: rule.style,
                    });
                }
            }
        }

        let spans = Arc::new(spans);
        let mut cache = self.cache.borrow_mut();
        if cache.len() >= 1024 {
            cache.clear();
        }
        cache.insert(text.to_string(), spans.clone());
        spans
    }
}

/// 行文本 + 每个字符对应的 cell 下标（宽字符尾格跳过）
fn text_and_cells(line: &Line) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut map = Vec::new();
    for (i, cell) in line.cells.iter().enumerate() {
        if cell.is_wide_tail() {
            continue;
        }
        text.push(cell.ch);
        map.push(i);
        if let Some(extra) = line.combining(i) {
            text.push_str(extra);
            for _ in extra.chars() {
                map.push(i);
            }
        }
    }
    // 与 `Line::text()` 保持一致：去掉尾部空白（否则整屏空格也会参与正则）
    let keep = text.trim_end().len();
    text.truncate(keep);
    map.truncate(text.chars().count());
    (text, map)
}

fn apply(cell: &mut Cell, style: CellStyle) {
    if let Some(fg) = style.fg {
        cell.fg = fg;
    }
    if let Some(bg) = style.bg {
        cell.bg = bg;
    }
    cell.flags |= style.flags;
}

fn style_flags(style: &sh0_store::HighlightStyle) -> u8 {
    let mut flags = 0;
    if style.bold {
        flags |= BOLD;
    }
    if style.dim {
        flags |= DIM;
    }
    if style.italic {
        flags |= ITALIC;
    }
    if style.underline {
        flags |= UNDERLINE;
    }
    flags
}

fn to_color(c: &HighlightColor) -> Color {
    match c {
        HighlightColor::Rgb(r, g, b) => Color::Rgb(*r, *g, *b),
        HighlightColor::Named(name) => match name.as_str() {
            "black" => Color::Indexed(0),
            "red" => Color::Indexed(1),
            "green" => Color::Indexed(2),
            "yellow" => Color::Indexed(3),
            "blue" => Color::Indexed(4),
            "magenta" => Color::Indexed(5),
            "cyan" => Color::Indexed(6),
            "white" => Color::Indexed(7),
            "brightblack" | "gray" | "grey" | "darkgray" | "darkgrey" => Color::Indexed(8),
            "brightred" | "lightred" => Color::Indexed(9),
            "brightgreen" | "lightgreen" => Color::Indexed(10),
            "brightyellow" | "lightyellow" => Color::Indexed(11),
            "brightblue" | "lightblue" => Color::Indexed(12),
            "brightmagenta" | "lightmagenta" => Color::Indexed(13),
            "brightcyan" | "lightcyan" => Color::Indexed(14),
            "brightwhite" | "lightwhite" => Color::Indexed(15),
            _ => Color::Default,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use sh0_store::{HighlightRule, HighlightStyle, default_highlight_rules};

    fn line(s: &str) -> Line {
        let mut l = Line::blank(s.chars().count(), Color::Default, Color::Default, 0);
        for (i, ch) in s.chars().enumerate() {
            l.cells[i] = Cell::new(ch, Color::Default, Color::Default, 0);
        }
        l
    }

    fn rule(pattern: &str, fg: &str, priority: i32) -> HighlightRule {
        HighlightRule {
            name: pattern.into(),
            regex: Regex::new(pattern).unwrap(),
            style: HighlightStyle {
                fg: Some(HighlightColor::Named(fg.into())),
                ..Default::default()
            },
            priority,
        }
    }

    #[test]
    fn default_error_rule_colors_the_word() {
        let h = Highlighter::new(default_highlight_rules());
        let src = line("ERROR fine");
        let out = h.highlight(&src);
        assert_eq!(out.cells[0].fg, Color::Indexed(1));
        assert_ne!(out.cells[0].flags & BOLD, 0);
        assert_eq!(out.cells[6].fg, Color::Default, "未命中的字不能改色");
    }

    #[test]
    fn no_match_returns_borrowed_line() {
        let h = Highlighter::new(default_highlight_rules());
        let src = line("nothing to see");
        assert!(matches!(h.highlight(&src), Cow::Borrowed(_)));
    }

    #[test]
    fn higher_priority_wins_on_overlap() {
        let h = Highlighter::new(vec![
            rule("foo", "red", 10),
            rule("foo", "blue", 20),
        ]);
        let src = line("foo");
        let out = h.highlight(&src);
        assert_eq!(out.cells[0].fg, Color::Indexed(4), "高优先级 blue 应胜出");
    }

    #[test]
    fn cjk_width_does_not_shift_match_columns() {
        let h = Highlighter::new(vec![rule("ERROR", "red", 10)]);
        // "错" 占两格：main + wide tail；后面才是空格与 ERROR
        let mut l = Line::blank(8, Color::Default, Color::Default, 0);
        l.cells[0] = Cell::new('错', Color::Default, Color::Default, 0);
        l.cells[1] = Cell::wide_tail(Color::Default, Color::Default, 0);
        l.cells[2] = Cell::new(' ', Color::Default, Color::Default, 0);
        for (i, ch) in "ERROR".chars().enumerate() {
            l.cells[3 + i] = Cell::new(ch, Color::Default, Color::Default, 0);
        }
        let out = h.highlight(&l);
        assert_eq!(out.cells[3].fg, Color::Indexed(1), "E 应被高亮");
        assert_eq!(out.cells[2].fg, Color::Default, "空格不能被误伤");
    }
}
