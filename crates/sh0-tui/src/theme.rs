//! 主题配色（需求 FR-3.4）。
//!
//! M1 只内置一套默认主题；配色集中在这里，之后加主题文件只需要再加一个 `Theme`。

use ratatui::style::Color as RColor;

/// 主题配色
#[derive(Debug, Clone)]
pub struct Theme {
    /// 终端默认前景/背景（对应 `Color::Default`）
    pub fg: RColor,
    pub bg: RColor,
    /// 面板边框
    pub border: RColor,
    /// 未选中面板的边框
    pub border_dim: RColor,
    /// 强调色：选中项、活动标签
    pub accent: RColor,
    /// 状态栏文字
    pub status_fg: RColor,
    pub status_bg: RColor,
    /// 状态语义色
    pub ok: RColor,
    pub warn: RColor,
    pub err: RColor,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            fg: RColor::Rgb(0xdc, 0xdc, 0xdc),
            bg: RColor::Rgb(0x18, 0x18, 0x1c),
            border: RColor::Rgb(0x50, 0x60, 0x78),
            border_dim: RColor::Rgb(0x33, 0x3a, 0x44),
            accent: RColor::Rgb(0x5f, 0xaf, 0xd7),
            status_fg: RColor::Rgb(0x18, 0x18, 0x1c),
            status_bg: RColor::Rgb(0x5f, 0xaf, 0xd7),
            ok: RColor::Rgb(0x5f, 0xd7, 0x5f),
            warn: RColor::Rgb(0xd7, 0xaf, 0x5f),
            err: RColor::Rgb(0xd7, 0x5f, 0x5f),
        }
    }
}

impl Theme {
    /// xterm `Color::Default` 落到主题默认前景
    pub fn default_fg(&self) -> RColor {
        self.fg
    }

    pub fn default_bg(&self) -> RColor {
        self.bg
    }

    /// 会话状态对应的颜色
    pub fn state_color(&self, state: sh0_core::SessionState) -> RColor {
        use sh0_core::SessionState as S;
        match state {
            S::Ready => self.ok,
            S::Connecting | S::Authenticating => self.warn,
            S::Degraded | S::Reconnecting => self.warn,
            S::Closed => self.err,
        }
    }
}
