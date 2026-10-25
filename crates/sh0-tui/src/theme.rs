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
        Self::preset(DEFAULT_THEME)
    }
}

/// 主题名：dark / light / high-contrast（需求 FR-3.7）
pub const DEFAULT_THEME: &str = "dark";

/// 全部内置主题名，命令面板与 `--list` 用
pub const THEME_NAMES: [&str; 3] = ["dark", "light", "high-contrast"];

impl Theme {
    /// 按名字取主题；未知名字回落到 dark
    pub fn preset(name: &str) -> Self {
        match name.to_lowercase().as_str() {
            "light" => Self::light(),
            "high-contrast" | "high_contrast" | "hc" => Self::high_contrast(),
            _ => Self::dark(),
        }
    }

    /// dark：默认的暗色主题
    fn dark() -> Self {
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

    /// light：亮底深字，白天或投屏时用
    fn light() -> Self {
        Self {
            fg: RColor::Rgb(0x1c, 0x1c, 0x20),
            bg: RColor::Rgb(0xf2, 0xf2, 0xf0),
            border: RColor::Rgb(0x8a, 0x93, 0xa3),
            border_dim: RColor::Rgb(0xc4, 0xc8, 0xd0),
            accent: RColor::Rgb(0x0f, 0x6f, 0x9c),
            status_fg: RColor::Rgb(0xf2, 0xf2, 0xf0),
            status_bg: RColor::Rgb(0x0f, 0x6f, 0x9c),
            ok: RColor::Rgb(0x1a, 0x7f, 0x3c),
            warn: RColor::Rgb(0x9c, 0x6a, 0x00),
            err: RColor::Rgb(0xa3, 0x1f, 0x1f),
        }
    }

    /// high-contrast：纯黑白 + 高饱和语义色，弱视/强光环境
    fn high_contrast() -> Self {
        Self {
            fg: RColor::Rgb(0xff, 0xff, 0xff),
            bg: RColor::Rgb(0x00, 0x00, 0x00),
            border: RColor::Rgb(0xff, 0xff, 0xff),
            border_dim: RColor::Rgb(0x80, 0x80, 0x80),
            accent: RColor::Rgb(0x00, 0xff, 0xff),
            status_fg: RColor::Rgb(0x00, 0x00, 0x00),
            status_bg: RColor::Rgb(0xff, 0xff, 0x00),
            ok: RColor::Rgb(0x00, 0xff, 0x00),
            warn: RColor::Rgb(0xff, 0xff, 0x00),
            err: RColor::Rgb(0xff, 0x00, 0x00),
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
