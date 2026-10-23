//! 终端仿真的 golden 测试。
//!
//! 这些用例是 M1 的"地基"：终端仿真的 bug 几乎都表现为"看起来差一格"，
//! 靠人眼在真实会话里很难定位，必须靠固定输入 → 固定输出的对比守住。
//!
//! 命名沿用计划里的三组真实序列（vim / htop / tmux），后续继续往里加。

use sh0_term::{Screen, ScreenOptions};

fn screen(cols: usize, rows: usize) -> Screen {
    Screen::new(ScreenOptions::new(cols, rows))
}

#[test]
fn plain_text_and_cursor() {
    let mut s = screen(20, 3);
    s.feed(b"hello");
    assert_eq!(s.screen_text(), "hello");
    assert_eq!(s.view_cursor(), Some((0, 5)));

    s.feed(b"\r\nworld");
    assert_eq!(s.screen_text(), "hello\nworld");
    assert_eq!(s.view_cursor(), Some((1, 5)));
}

#[test]
fn cjk_double_width_keeps_cursor_aligned() {
    let mut s = screen(20, 2);
    // 三个汉字占 6 列
    s.feed("你好世界".as_bytes());
    assert_eq!(s.view_cursor(), Some((0, 8)));

    // 宽字符后接 ASCII 不应错位
    s.feed(b"ab");
    let line = s.grid.line(0).unwrap();
    assert_eq!(line.cell_text(8), "a");
    // 第 1、3、5、7 列是宽字符的续位格
    assert!(line.cells[1].is_wide_tail());
    assert!(line.cells[3].is_wide_tail());
    assert!(!line.cells[8].is_wide_tail());
}

#[test]
fn wide_char_does_not_break_at_line_end() {
    let mut s = screen(4, 2);
    s.feed("你".as_bytes()); // 占 0-1
    s.feed("你".as_bytes()); // 占 2-3，刚好填满
    // 光标逻辑位置是第 4 列（即"下一格要换行"），显示时钳到最后一列
    assert_eq!(s.grid.cursor().x, 4);
    assert_eq!(s.view_cursor(), Some((0, 3)));
    s.feed(b"x"); // 触发换行
    assert_eq!(s.view_cursor(), Some((1, 1)));
    assert_eq!(s.grid.line(1).unwrap().cell_text(0), "x");
}

#[test]
fn combining_char_attaches_to_previous_cell() {
    let mut s = screen(10, 2);
    // e + 组合重音符 U+0301
    s.feed("e\u{0301}x".as_bytes());
    let line = s.grid.line(0).unwrap();
    assert_eq!(line.cell_text(0), "e\u{0301}");
    // 组合字符不推进光标：x 停在第 1 列
    assert_eq!(line.cell_text(1), "x");
    assert_eq!(s.view_cursor(), Some((0, 2)));
}

#[test]
fn sgr_colors_and_reset() {
    let mut s = screen(10, 2);
    s.feed(b"\x1b[31;1mred\x1b[0mplain");
    let line = s.grid.line(0).unwrap();
    assert_eq!(line.cells[0].fg, sh0_term::Color::Indexed(1));
    assert_ne!(line.cells[0].flags & sh0_term::cell::BOLD, 0);
    // reset 之后恢复默认
    assert_eq!(line.cells[4].fg, sh0_term::Color::Default);
    assert_eq!(line.cells[4].flags & sh0_term::cell::BOLD, 0);
}

#[test]
fn truecolor_and_256color() {
    let mut s = screen(10, 2);
    s.feed(b"\x1b[38;2;10;20;30mA\x1b[48;5;200mB");
    let line = s.grid.line(0).unwrap();
    assert_eq!(line.cells[0].fg, sh0_term::Color::Rgb(10, 20, 30));
    assert_eq!(line.cells[1].bg, sh0_term::Color::Indexed(200));
}

#[test]
fn erase_commands() {
    let mut s = screen(10, 3);
    s.feed(b"abcde\r\nfghij\r\nklmno");
    // EL 0：清光标之后
    s.feed(b"\x1b[2;3H\x1b[K");
    assert_eq!(s.grid.line(1).unwrap().text(), "fg");
    // ED 0：清光标之后的所有内容
    s.feed(b"\x1b[1;2H\x1b[J");
    // 尾部空行被去掉（无信息量），中间的保留
    assert_eq!(s.screen_text(), "a");
}

#[test]
fn scroll_pushes_into_scrollback() {
    let mut s = screen(10, 3);
    s.feed(b"one\r\ntwo\r\nthree\r\nfour");
    // 3 行屏幕：'one' 已被挤进 scrollback
    assert_eq!(s.scrollback_len(), 1);
    assert_eq!(s.screen_text(), "two\nthree\nfour");
    let full = s.plain_text(true);
    assert_eq!(full, "one\ntwo\nthree\nfour");
}

#[test]
fn scrollback_respects_line_cap() {
    let mut opts = ScreenOptions::new(10, 2);
    opts.scrollback_lines = 5;
    let mut s = Screen::new(opts);
    for i in 0..20 {
        s.feed(format!("line{i}\r\n").as_bytes());
    }
    assert_eq!(s.scrollback_len(), 5);
    assert!(!s.plain_text(true).contains("line10"));
    assert!(s.plain_text(true).contains("line19"));
}

#[test]
fn memory_budget_trims_oldest() {
    use sh0_term::MemoryBudget;
    // 极小预算：每条新行都会把最旧的挤掉
    let budget = MemoryBudget::new(1024);
    let mut s = Screen::new(ScreenOptions::new(40, 2).with_budget(budget.clone()));
    for i in 0..200 {
        s.feed(format!("line {i}\r\n").as_bytes());
    }
    assert!(budget.used() <= budget.limit() + 4096);
    assert!(s.scrollback_len() < 200);
}

#[test]
fn scroll_region_and_reverse_index() {
    let mut s = screen(6, 4);
    // 设定 2-4 行为滚动区域
    s.feed(b"\x1b[2;4r");
    s.feed(b"\x1b[4;1H");
    s.feed(b"a\r\nb"); // 在区域内下滚
    assert_eq!(s.grid.line(3).unwrap().text(), "b");
    // RI 到区域顶部时区域内下滚
    s.feed(b"\x1b[2;1H\x1bM");
    assert_eq!(s.grid.line(2).unwrap().text(), "");
    assert_eq!(s.grid.line(3).unwrap().text(), "a");
}

#[test]
fn insert_and_delete_lines() {
    let mut s = screen(6, 4);
    s.feed(b"1\r\n2\r\n3\r\n4");
    s.feed(b"\x1b[2;1H"); // 到第 2 行
    s.feed(b"\x1b[L"); // 插入一行
    assert_eq!(s.screen_text(), "1\n\n2\n3");
    s.feed(b"\x1b[M"); // 删除一行
    assert_eq!(s.screen_text(), "1\n2\n3");
}

#[test]
fn tabs_align_text() {
    let mut s = screen(20, 2);
    s.feed(b"a\tb\tc");
    // 默认 tab stop 每 8 列：a 在 0，b 在 8，c 在下一个 stop 即 16
    assert_eq!(s.grid.line(0).unwrap().cell_text(8), "b");
    assert_eq!(s.grid.line(0).unwrap().cell_text(16), "c");
}

#[test]
fn resize_keeps_content_and_clamps_cursor() {
    let mut s = screen(20, 4);
    s.feed(b"hello\r\nworld");
    s.resize(10, 3);
    assert_eq!(s.cols(), 10);
    assert_eq!(s.rows(), 3);
    // 高度变小时顶部内容不丢，而是进 scrollback
    assert!(s.screen_text().contains("world"));
    assert!(s.plain_text(true).contains("hello"));
    assert_eq!(s.view_cursor().unwrap().1, 5);

    // 行数增加时顶部补空行，底部内容保留
    s.resize(10, 5);
    let text = s.screen_text();
    assert!(text.ends_with("world"), "实际: {text:?}");
}

#[test]
fn application_cursor_keys_switch() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use sh0_term::encode_key;

    let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(encode_key(up, false), Some(b"\x1b[A".to_vec()));
    assert_eq!(encode_key(up, true), Some(b"\x1bOA".to_vec()));
}

#[test]
fn ctrl_and_alt_encoding() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use sh0_term::encode_key;

    assert_eq!(
        encode_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
        Some(vec![0x03])
    );
    assert_eq!(
        encode_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT), false),
        Some(b"\x1bx".to_vec())
    );
    assert_eq!(
        encode_key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE), false),
        Some(b"\x1b[15~".to_vec())
    );
}

#[test]
fn vim_like_sequence_smoke() {
    // vim 启动时典型的：切备用屏、关自动换行、隐藏光标、定位、画状态栏
    let mut s = screen(40, 6);
    s.feed(b"\x1b[?1049h\x1b[?1h\x1b[?25l");
    s.feed(b"\x1b[1;1H\x1b[0m\x1b[7m\x1b[1;40H");
    s.feed(b"\x1b[2;1H#include <stdio.h>\r\n");
    s.feed(b"int main(void) {\r\n    return 0;\r\n}\r\n");
    s.feed(b"\x1b[6;1H\x1b[7m-- INSERT --\x1b[0m");
    let text = s.screen_text();
    assert!(text.contains("#include <stdio.h>"));
    assert!(text.contains("-- INSERT --"));
    // 应用光标键模式应被记住
    assert!(s.grid.app_cursor_keys);
}

#[test]
fn htop_like_sequence_smoke() {
    // htop：大量 SGR、绝对定位、行内重绘
    let mut s = screen(60, 8);
    s.feed(b"\x1b[?25l\x1b[1;1H");
    s.feed(b"\x1b[0m\x1b[7m  1  [");
    s.feed(b"\x1b[1m\x1b[36m|||||||||\x1b[0m\x1b[7m");
    s.feed(b"                                          ]\x1b[0m");
    s.feed(b"\x1b[2;1H\x1b[0m\x1b[7m  2  [\x1b[1m\x1b[36m||\x1b[0m\x1b[7m");
    let text = s.screen_text();
    assert!(text.starts_with("  1  ["));
    assert!(text.contains("  2  ["));
    // 进度条字符必须连续，不能被错误截断
    assert!(text.contains("|||||||||"));
}

#[test]
fn tmux_like_sequence_smoke() {
    // tmux：状态栏 + 分屏边框 + 局部刷新（CUU/CUD + EL）
    let mut s = screen(30, 5);
    s.feed(b"\x1b[1;1H\x1b[0m\x1b[34m\x1b[1m[0] 0:bash*\x1b[0m");
    s.feed(b"\x1b[5;1H\x1b[0m\x1b[37m\x1b[44m[sh0]");
    s.feed(b"\x1b[2;1H\x1b[Kls -la\r\n");
    assert!(s.screen_text().contains("[0] 0:bash*"));
    assert!(s.screen_text().contains("ls -la"));
}

#[test]
fn reconnect_mark_is_visible_in_plain_text() {
    let mut s = screen(20, 3);
    s.feed(b"before\r\n");
    s.mark_reconnect("重新连接成功");
    s.feed(b"after\r\n");
    let text = s.plain_text(true);
    assert!(text.contains("before"));
    assert!(text.contains("重新连接成功"));
    assert!(text.contains("after"));
}

/// 文本选择：起止行按列裁剪，中间行整行保留，行尾填充空格不进剪贴板
#[test]
fn selection_extracts_expected_text() {
    let mut s = screen(20, 5);
    s.feed(b"alpha\r\nbravo\r\ncharlie\r\ndelta\r\n");
    let text = s.extract_view_text((0, 1), (2, 3));
    assert_eq!(text, "lpha\nbravo\nchar");
}

/// 反向选择（从右下往左上拖）应与正向得到同样结果
#[test]
fn selection_is_order_independent() {
    let mut s = screen(20, 5);
    s.feed(b"alpha\r\nbravo\r\ncharlie\r\ndelta\r\n");
    let forward = s.extract_view_text((0, 1), (2, 3));
    let backward = s.extract_view_text((2, 3), (0, 1));
    assert_eq!(forward, backward);
}

/// 宽字符占两格，选择时不能把半个字切出来
#[test]
fn selection_respects_wide_chars() {
    let mut s = screen(20, 3);
    s.feed("中文abc\r\n".as_bytes());
    // 中文占 0-3 格，选 0..1 落在"中"上，应得到完整的中
    let text = s.extract_view_text((0, 0), (0, 1));
    assert_eq!(text, "中");
}
