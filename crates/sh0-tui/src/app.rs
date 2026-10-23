//! M0 的最小终端视图。
//!
//! 这一版**不做终端仿真**：直接把远端字节流按 UTF-8 解码、剥掉 ANSI 转义后
//! 逐行显示。目的是先打通「键盘 → 远端 → 回显」这条闭环（W0.8）。
//! 真正的 xterm 网格与滚动缓冲在 M1 的 W1.1~W1.3 落地。

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use serde_json::json;
use sh0_ipc::ServerMsg;
use sh0_ipc::protocol::methods;
use tokio::sync::mpsc::UnboundedReceiver;

use std::sync::Arc;

/// 最多保留的显示行数，避免长时间跑满后无限增长
const MAX_LINES: usize = 5000;

pub struct TuiApp {
    lines: Vec<String>,
    partial: String,
    status: String,
    host_label: String,
    session_id: String,
}

impl TuiApp {
    fn new(host_label: String, session_id: String) -> Self {
        Self {
            lines: Vec::new(),
            partial: String::new(),
            status: "connecting".to_string(),
            host_label,
            session_id,
        }
    }

    fn push_output(&mut self, raw: &str) {
        let clean = strip_ansi(raw);
        let mut parts = clean.split('\n');
        if let Some(first) = parts.next() {
            self.partial.push_str(first);
        }
        for part in parts {
            self.lines.push(std::mem::take(&mut self.partial));
            self.partial.push_str(part);
        }
        if self.lines.len() > MAX_LINES {
            let drain = self.lines.len() - MAX_LINES;
            self.lines.drain(..drain);
        }
    }

    fn visible_lines(&self, height: usize) -> Vec<Line<'static>> {
        let start = self.lines.len().saturating_sub(height.saturating_sub(1));
        let mut out: Vec<Line<'static>> = self.lines[start..]
            .iter()
            .map(|l| Line::raw(l.clone()))
            .collect();
        out.push(Line::raw(self.partial.clone()));
        out
    }
}

/// 跑起 TUI 主循环。调用方已建立好 IPC 连接。
pub async fn run(
    client: Arc<sh0_ipc::IpcClient>,
    mut events: UnboundedReceiver<ServerMsg>,
    host_selector: &str,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()> {
    // 开会话
    let res = client
        .call(
            methods::SESSION_OPEN,
            json!({ "host": host_selector, "cols": cols, "rows": rows }),
        )
        .await?;
    let session_id = res
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("daemon 未返回 session id"))?
        .to_string();

    let mut app = TuiApp::new(host_selector.to_string(), session_id.clone());

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = loop_inner(&mut terminal, &mut app, &client, &mut events).await;

    // 无论成功失败都要恢复终端
    let _ = disable_raw_mode();
    let _ = terminal.backend_mut();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen);

    // 退出时关闭会话
    let _ = client
        .call(methods::SESSION_CLOSE, json!({ "session": session_id }))
        .await;

    result
}

async fn loop_inner(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut TuiApp,
    client: &Arc<sh0_ipc::IpcClient>,
    events: &mut UnboundedReceiver<ServerMsg>,
) -> anyhow::Result<()> {
    loop {
        // 1) 键盘输入
        if event::poll(Duration::from_millis(0))?
            && let Event::Key(key) = event::read()? {
                if is_quit(key) {
                    return Ok(());
                }
                if let Some(bytes) = encode_key(key) {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    let _ = client
                        .call(
                            methods::INPUT_SEND,
                            json!({ "session": app.session_id, "data": text }),
                        )
                        .await;
                }
            }

        // 2) 远端输出与状态事件
        while let Ok(msg) = events.try_recv() {
            match msg {
                ServerMsg::Event {
                    event, data, session, ..
                } => {
                    if session.as_deref() != Some(app.session_id.as_str()) {
                        continue;
                    }
                    match event.as_str() {
                        "output" => {
                            if let Some(d) = data.and_then(|d| d.get("data").cloned())
                                && let Some(s) = d.as_str() {
                                    app.push_output(s);
                                }
                        }
                        "state" => {
                            if let Some(d) = data.and_then(|d| d.get("state").cloned())
                                && let Some(s) = d.as_str() {
                                    app.status = s.to_string();
                                }
                        }
                        _ => {}
                    }
                }
                ServerMsg::Response { .. } => {}
            }
        }

        // 3) 渲染
        terminal.draw(|f| draw(f, app))?;

        tokio::time::sleep(Duration::from_millis(16)).await;
    }
}

fn draw(f: &mut ratatui::Frame, app: &TuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());

    let height = chunks[0].height as usize;
    let body = Paragraph::new(app.visible_lines(height)).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::NONE)
            .title(format!(" {} ", app.host_label)),
    );
    f.render_widget(body, chunks[0]);

    let hint = Line::from(vec![
        Span::styled(
            format!(" {} ", app.status),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::raw(" "),
        Span::styled(
            "Ctrl+Q 退出",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]);
    f.render_widget(Paragraph::new(hint), chunks[1]);
}

fn is_quit(key: KeyEvent) -> bool {
    matches!(
        (key.code, key.modifiers),
        (KeyCode::Char('q'), KeyModifiers::CONTROL)
    )
}

/// 把按键编码成发往远端 PTY 的字节序列。
/// M1 会把这张表下沉到 sh0-term 的 keys.rs，与 `ai input send` 共用。
fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => Some(vec![0x03]),
        (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => Some(vec![0x04]),
        (KeyCode::Char('l'), m) if m.contains(KeyModifiers::CONTROL) => Some(vec![0x0c]),
        (KeyCode::Char(_), m) if m.contains(KeyModifiers::CONTROL) => None,
        (KeyCode::Char(c), _) => Some(c.to_string().into_bytes()),
        (KeyCode::Enter, _) => Some(vec![b'\r']),
        (KeyCode::Backspace, _) => Some(vec![0x7f]),
        (KeyCode::Delete, _) => Some(b"\x1b[3~".to_vec()),
        (KeyCode::Tab, _) => Some(vec![b'\t']),
        (KeyCode::BackTab, _) => Some(b"\x1b[Z".to_vec()),
        (KeyCode::Esc, _) => Some(vec![0x1b]),
        (KeyCode::Up, _) => Some(b"\x1b[A".to_vec()),
        (KeyCode::Down, _) => Some(b"\x1b[B".to_vec()),
        (KeyCode::Right, _) => Some(b"\x1b[C".to_vec()),
        (KeyCode::Left, _) => Some(b"\x1b[D".to_vec()),
        (KeyCode::Home, _) => Some(b"\x1b[H".to_vec()),
        (KeyCode::End, _) => Some(b"\x1b[F".to_vec()),
        _ => None,
    }
}

/// 极简 ANSI 剥离：只保留可显示字符。
/// 光标定位、颜色等语义留给 M1 的终端仿真器处理。
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // 吞掉转义序列，直到遇到终止字母或 BEL/ST
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() || c2 == '~' || c2 == '\u{7}' {
                        break;
                    }
                }
            }
            '\r' | '\u{7}' => {}
            _ => out.push(c),
        }
    }
    out
}
