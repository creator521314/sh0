//! 文本复制：OSC 52 + 本机剪贴板。
//!
//! 为什么两条路都走：
//! - OSC 52 能穿过 SSH / tmux 到达**用户本地终端**，但不少终端默认禁用它
//!   （GNOME Terminal/VTE、部分 Windows Terminal 版本）；
//! - `arboard` 在本机桌面环境（X11/Wayland/Windows/macOS）更可靠，
//!   但在 SSH 远程运行 sh0 时只会写远端机器，所以不能只靠它。
//!
//! `arboard::Clipboard` 在 X11 下需要保持存活才能持有选区，因此由 `App`
//! 长期持有，而不是每次复制临时 new 一个。

use std::io::Write;
use std::process::{Command, Stdio};

pub struct Clipboard {
    native: Option<arboard::Clipboard>,
    tried_native: bool,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard {
    pub fn new() -> Self {
        Self {
            native: None,
            tried_native: false,
        }
    }

    /// 提前初始化本机剪贴板。
    ///
    /// `arboard` 在 Wayland 缺少 data-control 时会打一条 WARN 再回退 X11；
    /// 如果等第一次复制才初始化，这条日志会直接画进 TUI。TUI 在进入备用
    /// 屏幕前调用它，让日志落在正常终端/日志文件里。
    pub fn warm(&mut self) {
        if self.tried_native {
            return;
        }
        self.tried_native = true;
        self.native = arboard::Clipboard::new().ok();
    }

    /// 写入剪贴板。返回 `true` 表示本机剪贴板也成功了。
    pub fn copy(&mut self, text: &str) -> bool {
        write_osc52(text);
        self.warm();
        if let Some(native) = &mut self.native
            && native.set_text(text).is_ok()
        {
            return true;
        }

        // arboard 不可用（未装桌面库 / 无显示）时，再试常见的命令行工具
        command_fallback(text)
    }
}

fn write_osc52(text: &str) {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{b64}\x1b\\");
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

#[cfg(target_os = "macos")]
fn command_fallback(text: &str) -> bool {
    pipe("pbcopy", &[], text)
}

#[cfg(target_os = "windows")]
fn command_fallback(text: &str) -> bool {
    pipe("clip", &[], text)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn command_fallback(text: &str) -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && pipe("wl-copy", &[], text) {
        return true;
    }
    if std::env::var_os("DISPLAY").is_some() {
        if pipe("xclip", &["-selection", "clipboard"], text) {
            return true;
        }
        if pipe("xsel", &["--clipboard", "--input"], text) {
            return true;
        }
    }
    false
}

#[cfg(not(any(unix, windows)))]
fn command_fallback(_text: &str) -> bool {
    false
}

fn pipe(cmd: &str, args: &[&str], text: &str) -> bool {
    let mut child = match Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(text.as_bytes()).is_err()
    {
        let _ = child.kill();
        return false;
    }

    // xclip / wl-copy 会驻留以持有选区，不能同步 wait；后台回收。
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    true
}
