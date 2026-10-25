//! TUI 自身的键位映射（需求 FR-3.6）：三套预设 + TOML 覆盖。
//!
//! 与 `keys.rs` 的分工：那边是"按键发什么字节给远端"，这边是
//! "按键触发本地什么动作"。命中本表的按键**不会**发给远端，
//! 否则 Ctrl+Q 会被当成 XON 之类的终端控制码吃掉。

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

use crate::keys::parse_key;

/// TUI 可绑定的动作
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Action {
    Quit,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    /// 焦点在主机树与终端之间切换
    ToggleFocus,
    HostSearch,
    HostNew,
    HostEdit,
    HostDelete,
    ScrollUp,
    ScrollDown,
    ScrollTop,
    ScrollBottom,
    Reconnect,
    ClearScreen,
    /// 进入/退出文本选择模式
    SelectMode,
    /// 复制当前选区
    Copy,
    /// 终端视图与双栏文件视图之间切换（FR-4.2）
    ToggleFiles,
    /// 刷新当前文件面板
    FileRefresh,
}

impl Action {
    /// 配置里用的名字
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Quit => "quit",
            Action::NewTab => "tab.new",
            Action::CloseTab => "tab.close",
            Action::NextTab => "tab.next",
            Action::PrevTab => "tab.prev",
            Action::ToggleFocus => "focus.toggle",
            Action::HostSearch => "host.search",
            Action::HostNew => "host.new",
            Action::HostEdit => "host.edit",
            Action::HostDelete => "host.delete",
            Action::ScrollUp => "scroll.up",
            Action::ScrollDown => "scroll.down",
            Action::ScrollTop => "scroll.top",
            Action::ScrollBottom => "scroll.bottom",
            Action::Reconnect => "session.reconnect",
            Action::ClearScreen => "screen.clear",
            Action::SelectMode => "screen.select",
            Action::Copy => "screen.copy",
            Action::ToggleFiles => "file.toggle",
            Action::FileRefresh => "file.refresh",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        // 线性查找：动作只有十几个，比维护一张静态表更不容易写错
        [
            Action::Quit,
            Action::NewTab,
            Action::CloseTab,
            Action::NextTab,
            Action::PrevTab,
            Action::ToggleFocus,
            Action::HostSearch,
            Action::HostNew,
            Action::HostEdit,
            Action::HostDelete,
            Action::ScrollUp,
            Action::ScrollDown,
            Action::ScrollTop,
            Action::ScrollBottom,
            Action::Reconnect,
            Action::ClearScreen,
            Action::SelectMode,
            Action::Copy,
            Action::ToggleFiles,
            Action::FileRefresh,
        ]
        .into_iter()
        .find(|a| a.as_str() == s)
    }

    /// 状态栏提示用
    pub fn label(self) -> &'static str {
        match self {
            Action::Quit => "退出",
            Action::NewTab => "新建会话",
            Action::CloseTab => "关闭会话",
            Action::NextTab => "下一个会话",
            Action::PrevTab => "上一个会话",
            Action::ToggleFocus => "切换焦点",
            Action::HostSearch => "搜索主机",
            Action::HostNew => "新建主机",
            Action::HostEdit => "编辑主机",
            Action::HostDelete => "删除主机",
            Action::ScrollUp => "向上翻页",
            Action::ScrollDown => "向下翻页",
            Action::ScrollTop => "回到顶部",
            Action::ScrollBottom => "回到最新",
            Action::Reconnect => "重新连接",
            Action::ClearScreen => "清屏",
            Action::SelectMode => "选择文本",
            Action::Copy => "复制选区",
            Action::ToggleFiles => "文件管理",
            Action::FileRefresh => "刷新文件列表",
        }
    }
}

/// 键位表。用 Vec 线性查找而非 HashMap —— KeyEvent 的哈希不稳定，
/// 而绑定条目只有几十条，线性扫描完全够快。
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    bindings: Vec<(KeyEvent, Action)>,
}

impl Keymap {
    pub fn preset(name: &str) -> Self {
        match name.to_lowercase().as_str() {
            "vim" => Self::vim(),
            "xshell" => Self::xshell(),
            _ => Self::default_preset(),
        }
    }

    /// default：贴近现代终端的习惯
    fn default_preset() -> Self {
        Self::from_pairs(&[
            ("ctrl+q", Action::Quit),
            ("ctrl+t", Action::NewTab),
            ("ctrl+w", Action::CloseTab),
            ("alt+right", Action::NextTab),
            ("alt+left", Action::PrevTab),
            ("ctrl+b", Action::ToggleFocus),
            ("ctrl+f", Action::HostSearch),
            ("ctrl+n", Action::HostNew),
            ("pageup", Action::ScrollUp),
            ("pagedown", Action::ScrollDown),
            ("ctrl+pageup", Action::ScrollTop),
            ("ctrl+pagedown", Action::ScrollBottom),
            ("alt+r", Action::Reconnect),
            ("ctrl+l", Action::ClearScreen),
            ("alt+v", Action::SelectMode),
            ("alt+c", Action::Copy),
            ("f9", Action::ToggleFiles),
            ("f5", Action::FileRefresh),
        ])
    }

    /// vim：hjkl 与 / 风格
    fn vim() -> Self {
        Self::from_pairs(&[
            ("ctrl+q", Action::Quit),
            ("alt+n", Action::NewTab),
            ("alt+x", Action::CloseTab),
            ("alt+l", Action::NextTab),
            ("alt+h", Action::PrevTab),
            ("ctrl+b", Action::ToggleFocus),
            ("/", Action::HostSearch),
            ("alt+pageup", Action::ScrollUp),
            ("alt+pagedown", Action::ScrollDown),
            ("alt+g", Action::ScrollTop),
            ("shift+g", Action::ScrollBottom),
            ("alt+r", Action::Reconnect),
            ("ctrl+l", Action::ClearScreen),
            ("alt+v", Action::SelectMode),
            ("alt+c", Action::Copy),
            ("f9", Action::ToggleFiles),
            ("f5", Action::FileRefresh),
        ])
    }

    /// xshell：Alt 组合为主
    fn xshell() -> Self {
        Self::from_pairs(&[
            ("ctrl+q", Action::Quit),
            ("alt+n", Action::NewTab),
            ("alt+d", Action::CloseTab),
            ("ctrl+tab", Action::NextTab),
            ("ctrl+shift+tab", Action::PrevTab),
            ("alt+1", Action::ToggleFocus),
            ("alt+f", Action::HostSearch),
            ("shift+pageup", Action::ScrollUp),
            ("shift+pagedown", Action::ScrollDown),
            ("ctrl+home", Action::ScrollTop),
            ("ctrl+end", Action::ScrollBottom),
            ("alt+r", Action::Reconnect),
            ("ctrl+l", Action::ClearScreen),
            ("alt+v", Action::SelectMode),
            ("alt+c", Action::Copy),
            ("f9", Action::ToggleFiles),
            ("f5", Action::FileRefresh),
        ])
    }

    fn from_pairs(pairs: &[(&str, Action)]) -> Self {
        let mut bindings = Vec::new();
        for (k, a) in pairs {
            match parse_key(k) {
                Some(ev) => bindings.push((ev, *a)),
                None => tracing::warn!("键位预设里的 `{}` 无法解析，已跳过", k),
            }
        }
        Self { bindings }
    }

    pub fn get(&self, key: KeyEvent) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, a)| *a)
    }

    pub fn bind(&mut self, key: KeyEvent, action: Action) {
        self.bindings.retain(|(k, _)| *k != key);
        self.bindings.push((key, action));
    }

    /// 用 TOML 里的 `action = "键名"` 覆盖预设。
    /// 无法解析的键名只告警不失败——配置写错不该让程序起不来。
    pub fn apply_overrides(&mut self, overrides: &HashMap<String, String>) {
        for (action, key) in overrides {
            let Some(a) = Action::parse(action) else {
                tracing::warn!("未知动作 `{action}`，已忽略");
                continue;
            };
            match parse_key(key) {
                Some(ev) => self.bind(ev, a),
                None => tracing::warn!("键位 `{key}` 无法解析，已忽略"),
            }
        }
    }

    /// 导出全部绑定，供 `sh0 config keymap` 展示
    pub fn entries(&self) -> &[(KeyEvent, Action)] {
        &self.bindings
    }
}

/// 键名 → 可读描述，用于帮助面板
pub fn key_label(key: KeyEvent) -> String {
    let mut s = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        s += "Ctrl+";
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        s += "Alt+";
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        s += "Shift+";
    }
    s.push_str(match key.code {
        KeyCode::Char(' ') => "Space",
        KeyCode::Char(c) => return format!("{s}{}", c.to_uppercase()),
        KeyCode::Enter => "Enter",
        KeyCode::Tab => "Tab",
        KeyCode::BackTab => "Shift+Tab",
        KeyCode::Backspace => "Backspace",
        KeyCode::Delete => "Delete",
        KeyCode::Insert => "Insert",
        KeyCode::Esc => "Esc",
        KeyCode::Up => "↑",
        KeyCode::Down => "↓",
        KeyCode::Left => "←",
        KeyCode::Right => "→",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "PageUp",
        KeyCode::PageDown => "PageDown",
        KeyCode::F(n) => return format!("F{n}"),
        _ => return format!("{s}{:?}", key.code),
    });
    s
}
