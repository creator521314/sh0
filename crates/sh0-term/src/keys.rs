//! 按键编码：本地按键 → 发往远端 PTY 的字节序列（需求 FR-3.5 / FR-6.5）。
//!
//! 这张表被两处共用：TUI 的键盘输入，以及 M2 的 `sh0 ai input send --keys`。
//! 共用同一实现才能避免"人按的和 AI 发的不一样"这种最难排查的问题。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// 把按键编码成字节序列。
///
/// `app_cursor_keys` 为 DECCKM 状态：开启时方向键发 `ESC O A`，关闭时发 `ESC [ A`。
/// 该状态由 `vim` 等程序在进入时自行设置，因此必须跟随远端而不是写死。
pub fn encode_key(key: KeyEvent, app_cursor_keys: bool) -> Option<Vec<u8>> {
    let mods = key.modifiers;
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);

    // Alt+可打印字符：ESC 前缀（xterm 的 meta 行为）
    if alt && !ctrl {
        let inner = KeyEvent::new(key.code, mods & !KeyModifiers::ALT);
        if let Some(mut b) = encode_plain(inner, app_cursor_keys) {
            let mut out = vec![0x1b];
            out.append(&mut b);
            return Some(out);
        }
    }

    // Ctrl+字母 / Ctrl+特殊键
    if ctrl {
        let b = match key.code {
            KeyCode::Char(' ') => Some(0x00),
            KeyCode::Char('[') => Some(0x1b),
            KeyCode::Char('\\') => Some(0x1c),
            KeyCode::Char(']') => Some(0x1d),
            KeyCode::Char('^') => Some(0x1e),
            KeyCode::Char('_') => Some(0x1f),
            KeyCode::Char('?') => Some(0x7f),
            KeyCode::Char(c @ 'a'..='z') => Some(c as u8 - b'a' + 1),
            KeyCode::Char(c @ 'A'..='Z') => Some(c as u8 - b'A' + 1),
            KeyCode::Backspace => Some(0x08),
            KeyCode::Delete => Some(0x7f),
            _ => None,
        };
        if let Some(b) = b {
            return Some(vec![b]);
        }
    }

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                return None;
            }
            Some(c.to_string().into_bytes())
        }
        _ => encode_plain(key, app_cursor_keys),
    }
}

/// 不带修饰键（或修饰键已处理）时的编码
fn encode_plain(key: KeyEvent, app_cursor_keys: bool) -> Option<Vec<u8>> {
    let mods = key.modifiers;
    let m = modifier_param(mods);

    let (base_bracket, final_char): (&[u8], u8) = match key.code {
        KeyCode::Up => (b"\x1b[", b'A'),
        KeyCode::Down => (b"\x1b[", b'B'),
        KeyCode::Right => (b"\x1b[", b'C'),
        KeyCode::Left => (b"\x1b[", b'D'),
        KeyCode::Home => (b"\x1b[", b'H'),
        KeyCode::End => (b"\x1b[", b'F'),
        KeyCode::Insert => return Some(b"\x1b[2~".to_vec()),
        KeyCode::Delete => return Some(b"\x1b[3~".to_vec()),
        KeyCode::PageUp => return Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => return Some(b"\x1b[6~".to_vec()),
        KeyCode::BackTab => return Some(b"\x1b[Z".to_vec()),
        KeyCode::Tab => return Some(b"\t".to_vec()),
        KeyCode::Enter => return Some(b"\r".to_vec()),
        KeyCode::Backspace => return Some(vec![0x7f]),
        KeyCode::Esc => return Some(vec![0x1b]),
        KeyCode::Char(c) => return Some(c.to_string().into_bytes()),
        KeyCode::F(n @ 1..=12) => {
            // F1-F4 基础序列是 ESC O P/Q/R/S，其余是 CSI n~
            let seq: Vec<u8> = match n {
                1 => b"\x1bOP".to_vec(),
                2 => b"\x1bOQ".to_vec(),
                3 => b"\x1bOR".to_vec(),
                4 => b"\x1bOS".to_vec(),
                5 => b"\x1b[15~".to_vec(),
                6 => b"\x1b[17~".to_vec(),
                7 => b"\x1b[18~".to_vec(),
                8 => b"\x1b[19~".to_vec(),
                9 => b"\x1b[20~".to_vec(),
                10 => b"\x1b[21~".to_vec(),
                11 => b"\x1b[23~".to_vec(),
                12 => b"\x1b[24~".to_vec(),
                _ => unreachable!(),
            };
            return if let Some(mm) = m {
                // 带修饰键时统一走 CSI 1;<mod> <final>
                let f = match n {
                    1 => b'P',
                    2 => b'Q',
                    3 => b'R',
                    4 => b'S',
                    _ => return Some(seq),
                };
                Some(format!("\x1b[1;{mm}{}", f as char).into_bytes())
            } else {
                Some(seq)
            };
        }
        _ => return None,
    };

    // 应用光标键模式：方向键/Home/End 走 ESC O 前缀
    if app_cursor_keys && m.is_none() {
        let ss3: Option<u8> = match key.code {
            KeyCode::Up => Some(b'A'),
            KeyCode::Down => Some(b'B'),
            KeyCode::Right => Some(b'C'),
            KeyCode::Left => Some(b'D'),
            KeyCode::Home => Some(b'H'),
            KeyCode::End => Some(b'F'),
            _ => None,
        };
        if let Some(c) = ss3 {
            return Some(vec![0x1b, b'O', c]);
        }
    }

    match m {
        Some(mm) => Some(format!(
            "\x1b[1;{mm}{}",
            final_char as char
        ).into_bytes()),
        None => {
            let mut v = base_bracket.to_vec();
            v.push(final_char);
            Some(v)
        }
    }
}

/// xterm 修饰键参数：1 + shift(1) + alt(2) + ctrl(4) + meta(8)
fn modifier_param(mods: KeyModifiers) -> Option<u8> {
    let mut m = 1;
    if mods.contains(KeyModifiers::SHIFT) {
        m += 1;
    }
    if mods.contains(KeyModifiers::ALT) {
        m += 2;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        m += 4;
    }
    if mods.contains(KeyModifiers::SUPER) {
        m += 8;
    }
    if m == 1 {
        None
    } else {
        Some(m)
    }
}

/// 解析 "ctrl+q" / "alt+enter" / "f5" / "a" 这样的键名。
///
/// 供键位映射表（TOML）使用——写配置的人不应该去查 KeyCode 枚举。
pub fn parse_key(s: &str) -> Option<KeyEvent> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split('+').map(|p| p.trim()).collect();
    let last = parts.last()?;

    let mut mods = KeyModifiers::NONE;
    for p in &parts[..parts.len() - 1] {
        match *p {
            "ctrl" | "c" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "a" | "meta" => mods |= KeyModifiers::ALT,
            "shift" | "s" => mods |= KeyModifiers::SHIFT,
            "super" | "win" | "cmd" => mods |= KeyModifiers::SUPER,
            _ => return None,
        }
    }

    let code = match *last {
        "enter" | "return" | "cr" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" | "shift-tab" => KeyCode::BackTab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        other if other.starts_with('f') && other.len() <= 3 => {
            let n: u8 = other[1..].parse().ok()?;
            if (1..=12).contains(&n) {
                KeyCode::F(n)
            } else {
                return None;
            }
        }
        other if other.chars().count() == 1 => KeyCode::Char(other.chars().next()?),
        _ => return None,
    };

    Some(KeyEvent::new(code, mods))
}
