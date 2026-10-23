//! TUI 主应用：布局、键盘、事件循环（需求 FR-3.1~FR-3.4，W1.4 / W1.5 / W1.7）

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use serde_json::json;
use sh0_ipc::ServerMsg;
use sh0_ipc::protocol::methods;
use tokio::sync::mpsc::UnboundedReceiver;

use sh0_core::SessionState;
use sh0_store::Store;
use sh0_term::{Keymap, Screen, ScreenOptions};

use crate::hosts::{HostForm, HostTree, Row};
use sh0_term::keymap::Action;
use crate::render::{render_line, render_line_with_cursor};
use crate::theme::Theme;

const TREE_WIDTH: u16 = 28;
const FRAME_MS: u64 = 16;

/// 一个标签页 = 一个远端会话 + 本地一份 Screen
pub struct Tab {
    pub id: String,
    pub name: String,
    pub host: String,
    pub screen: Screen,
    pub state: SessionState,
    pub reconnect_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Terminal,
}

/// 文本选区，坐标是**视图坐标**（随滚动一起移动）
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl Selection {
    /// 该行被选中的列范围（闭区间）
    pub fn cols_for(&self, row: usize) -> Option<(usize, usize)> {
        let (r0, r1) = if self.anchor.0 <= self.head.0 {
            (self.anchor.0, self.head.0)
        } else {
            (self.head.0, self.anchor.0)
        };
        if row < r0 || row > r1 {
            return None;
        }
        let (c0, c1) = if self.anchor.1 <= self.head.1 {
            (self.anchor.1, self.head.1)
        } else {
            (self.head.1, self.anchor.1)
        };
        // 只有起止行才按列裁剪，中间行整行选中
        if row == r0 && row == r1 {
            Some((c0, c1))
        } else if row == r0 {
            Some((c0, usize::MAX))
        } else if row == r1 {
            Some((0, c1))
        } else {
            Some((0, usize::MAX))
        }
    }
}

pub struct App {
    client: Arc<sh0_ipc::IpcClient>,
    events: UnboundedReceiver<ServerMsg>,
    tabs: Vec<Tab>,
    active: usize,
    focus: Focus,
    keymap: Keymap,
    theme: Theme,
    tree: HostTree,
    store: Arc<Store>,
    /// 搜索输入缓冲；Some 表示正在输入
    query: Option<String>,
    form: Option<HostForm>,
    /// 待确认删除的主机
    pending_delete: Option<sh0_core::Host>,
    notice: String,
    show_tree: bool,
    should_quit: bool,
    /// 最近一次计算出的终端区尺寸（cols, rows）
    term_size: (u16, u16),
    /// 文本选区；Some 表示正处于选择模式
    selection: Option<Selection>,
    /// 最近一次复制的内容（剪贴板不可用时至少能粘回会话）
    last_copy: String,
}

impl App {
    pub fn new(
        client: Arc<sh0_ipc::IpcClient>,
        events: UnboundedReceiver<ServerMsg>,
        store: Arc<Store>,
        cfg: sh0_store::AppConfig,
    ) -> Self {
        let hosts = store.load_hosts().map(|h| h.hosts).unwrap_or_default();
        let mut keymap = Keymap::preset(&cfg.keymap);
        keymap.apply_overrides(&cfg.keys);
        let budget = if cfg.memory_budget_mb == 0 {
            None
        } else {
            Some(sh0_term::MemoryBudget::new(cfg.memory_budget_mb * 1024 * 1024))
        };
        let _ = budget; // TUI 侧每个标签各自持有 Screen，预算在 daemon 侧统一约束

        Self {
            client,
            events,
            tabs: Vec::new(),
            active: 0,
            focus: Focus::Tree,
            keymap,
            theme: Theme::default(),
            tree: HostTree::new(hosts),
            store,
            query: None,
            form: None,
            pending_delete: None,
            notice: String::new(),
            show_tree: true,
            should_quit: false,
            term_size: (cfg.cols, cfg.rows),
            selection: None,
            last_copy: String::new(),
        }
    }

    /// 接入 daemon 里已经存在的会话（TUI 退出并不关闭会话）
    pub async fn attach_existing(&mut self) -> anyhow::Result<()> {
        let res = self.client.call(methods::SESSION_LIST, json!({})).await?;
        let empty = vec![];
        let list = res.get("sessions").and_then(|v| v.as_array()).unwrap_or(&empty);
        for s in list {
            let id = s.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            if id.is_empty() || self.tabs.iter().any(|t| t.id == id) {
                continue;
            }
            let name = s.get("host_name").and_then(|v| v.as_str()).unwrap_or("?").to_string();
            let state = s.get("state").and_then(|v| v.as_str()).unwrap_or("closed");
            let mut tab = self.new_tab(id.to_string(), name.clone(), name.clone());
            tab.state = parse_state(state);
            tab.screen.push_notice("已接入 daemon 中的既有会话（历史输出请用 `sh0 ai screen read` 查看）");
            self.tabs.push(tab);
        }
        Ok(())
    }

    fn new_tab(&self, id: String, name: String, host: String) -> Tab {
        let (cols, rows) = self.term_size;
        let mut opts = ScreenOptions::new(cols.max(20) as usize, rows.max(5) as usize);
        opts.scrollback_lines = 5000;
        Tab {
            id,
            name,
            host,
            screen: Screen::new(opts),
            state: SessionState::Connecting,
            reconnect_count: 0,
        }
    }

    /// 打开一个会话并建立标签页
    pub async fn open(&mut self, host: &str) -> anyhow::Result<()> {
        let (cols, rows) = self.term_size;
        let res = self
            .client
            .call(methods::SESSION_OPEN, json!({ "host": host, "cols": cols, "rows": rows }))
            .await?;
        let id = res
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("daemon 未返回 session id"))?
            .to_string();
        let name = res
            .get("host")
            .and_then(|v| v.as_str())
            .unwrap_or(host)
            .to_string();
        self.tabs.push(self.new_tab(id, name.clone(), host.to_string()));
        self.active = self.tabs.len() - 1;
        self.focus = Focus::Terminal;
        self.notice = format!("已连接 {name}");
        Ok(())
    }

    // ---- 事件 ----

    async fn on_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        // 1) 主机编辑表单优先
        if let Some(form) = self.form.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    self.form = None;
                }
                KeyCode::Tab => form.next(),
                KeyCode::BackTab => form.prev(),
                KeyCode::Up => form.prev(),
                KeyCode::Down => form.next(),
                KeyCode::Backspace => form.backspace(),
                KeyCode::Enter => {
                    let host: sh0_core::Host = match form.to_host() {
                        Ok(h) => h,
                        Err(e) => {
                            self.notice = format!("表单有误：{e}");
                            return Ok(());
                        }
                    };
                    let editing = form.editing.clone();
                    self.form = None;
                    self.tree.upsert(host.clone());
                    let mut file = self.store.load_hosts()?;
                    if let Some(id) = editing {
                        if let Some(h) = file.hosts.iter_mut().find(|h| h.id == id) {
                            *h = host;
                        }
                    } else {
                        file.hosts.push(host);
                    }
                    self.store.save_hosts(&file)?;
                    self.notice = "主机已保存".into();
                }
                KeyCode::Char(c) => form.input(c),
                _ => {}
            }
            return Ok(());
        }

        // 2) 删除确认
        if let Some(host) = self.pending_delete.take() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.tree.remove_current();
                    let mut file = self.store.load_hosts()?;
                    file.hosts.retain(|h| h.id != host.id);
                    self.store.save_hosts(&file)?;
                    self.notice = format!("已删除 {}", host.name);
                }
                _ => self.notice = "已取消删除".into(),
            }
            return Ok(());
        }

        // 3) 搜索输入
        if let Some(q) = self.query.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    self.query = None;
                    self.tree.clear_query();
                }
                KeyCode::Enter => {
                    let host = self.tree.current_host();
                    self.query = None;
                    self.tree.clear_query();
                    if let Some(h) = host {
                        self.open(&h.name).await?;
                    }
                }
                KeyCode::Backspace => {
                    q.pop();
                    self.tree.set_query(q.clone());
                }
                KeyCode::Char(c) => {
                    q.push(c);
                    self.tree.set_query(q.clone());
                }
                _ => {}
            }
            return Ok(());
        }

        // 4) 选择模式：方向键用来移动选区，不发给远端
        if self.selection.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.selection = None;
                    self.notice = "已取消选择".into();
                }
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.copy_selection().await?;
                }
                KeyCode::Up | KeyCode::Char('k') => self.move_selection_head(-1, 0),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection_head(1, 0),
                KeyCode::Left | KeyCode::Char('h') => self.move_selection_head(0, -1),
                KeyCode::Right | KeyCode::Char('l') => self.move_selection_head(0, 1),
                KeyCode::Char('p') => self.paste_last_copy().await?,
                _ => {}
            }
            return Ok(());
        }

        // 5) 键位映射（命中的按键不会发给远端）
        if let Some(action) = self.keymap.get(key) {
            return self.on_action(action).await;
        }

        // 6) 焦点相关的裸按键
        match self.focus {
            Focus::Tree => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.tree.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.tree.move_selection(1),
                KeyCode::Enter => {
                    if let Some(h) = self.tree.activate() {
                        self.open(&h.name).await?;
                    }
                }
                KeyCode::Char('/') => self.query = Some(String::new()),
                KeyCode::Char('n') => self.form = Some(HostForm::new(None)),
                KeyCode::Char('e') => {
                    let host = self.tree.current_host();
                    self.form = Some(HostForm::new(host.as_ref()));
                }
                KeyCode::Char('d') => self.pending_delete = self.tree.current_host(),
                _ => {}
            },
            Focus::Terminal => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    let app_keys = tab.screen.grid.app_cursor_keys;
                    if let Some(bytes) =
                        sh0_term::encode_key(key, app_keys)
                    {
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        let _ = self
                            .client
                            .call(
                                methods::INPUT_SEND,
                                json!({ "session": tab.id, "data": text }),
                            )
                            .await;
                    }
                    // 有输入说明用户在看最新输出，回到底部
                    tab.screen.reset_view();
                }
            }
        }
        Ok(())
    }

    async fn on_action(&mut self, action: Action) -> anyhow::Result<()> {
        match action {
            Action::Quit => self.should_quit = true,
            Action::NewTab => {
                self.focus = Focus::Tree;
                self.query = Some(String::new());
            }
            Action::CloseTab => self.close_active().await?,
            Action::NextTab => self.cycle_tab(1),
            Action::PrevTab => self.cycle_tab(-1),
            Action::ToggleFocus => {
                self.focus = match self.focus {
                    Focus::Tree => Focus::Terminal,
                    Focus::Terminal => Focus::Tree,
                };
            }
            Action::HostSearch => self.query = Some(String::new()),
            Action::HostNew => self.form = Some(HostForm::new(None)),
            Action::HostEdit => {
                let host = self.tree.current_host();
                self.form = Some(HostForm::new(host.as_ref()));
            }
            Action::HostDelete => self.pending_delete = self.tree.current_host(),
            Action::ScrollUp => self.scroll_active(-10),
            Action::ScrollDown => self.scroll_active(10),
            Action::ScrollTop => self.scroll_active(isize::MIN / 4),
            Action::ScrollBottom => self.scroll_active(isize::MAX / 4),
            Action::Reconnect => {
                if let Some(tab) = self.tabs.get(self.active) {
                    let _ = self
                        .client
                        .call(methods::SESSION_RECONNECT, json!({ "session": tab.id }))
                        .await;
                    self.notice = "正在重连…".into();
                }
            }
            Action::ClearScreen => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.screen.clear();
                }
            }
            Action::SelectMode => {
                let Some(tab) = self.tabs.get(self.active) else {
                    self.notice = "没有活动会话".into();
                    return Ok(());
                };
                if self.selection.is_some() {
                    self.selection = None;
                    self.notice = "已退出选择模式".into();
                } else {
                    let head = tab.screen.view_cursor().unwrap_or((0, 0));
                    self.selection = Some(Selection { anchor: head, head });
                    self.focus = Focus::Terminal;
                    self.notice = "选择模式：方向键移动，Enter/y 复制，Esc 取消".into();
                }
            }
            Action::Copy => self.copy_selection().await?,
        }
        Ok(())
    }

    /// 把选区内容送出：先写父终端剪贴板（OSC 52），失败则发回会话。
    ///
    /// 用 OSC 52 而不是引入剪贴板库：我们跑在用户的终端里，让父终端去写
    /// 系统剪贴板才是最省依赖、也最符合终端惯例的做法（arboard 之类的库
    /// 在 Linux 上会拖进 X11/Wayland 依赖，与"轻量"冲突）。
    async fn copy_selection(&mut self) -> anyhow::Result<()> {
        let Some(sel) = self.selection else {
            self.notice = "没有选区，先用 Alt+V 进入选择模式".into();
            return Ok(());
        };
        let Some(tab) = self.tabs.get(self.active) else {
            return Ok(());
        };
        let text = tab.screen.extract_view_text(sel.anchor, sel.head);
        if text.is_empty() {
            self.notice = "选区是空的".into();
            return Ok(());
        }
        self.last_copy = text.clone();
        self.write_clipboard(&text);
        self.selection = None;
        let n = text.lines().count();
        self.notice = format!("已复制 {n} 行");
        Ok(())
    }

    /// OSC 52：把内容交给父终端写入系统剪贴板
    fn write_clipboard(&self, text: &str) {
        use std::io::Write;
        let b64 = base64_encode(text.as_bytes());
        // ]52;c;<base64>\ —— c 表示系统剪贴板
        let seq = format!("\x1b]52;c;{b64}\x1b\\");
        let mut out = std::io::stdout();
        let _ = out.write_all(seq.as_bytes());
        let _ = out.flush();
    }

    /// 把最近复制的内容粘回当前会话（剪贴板不可用时的兜底路径）
    async fn paste_last_copy(&mut self) -> anyhow::Result<()> {
        if self.last_copy.is_empty() {
            return Ok(());
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return Ok(());
        };
        let data = self.last_copy.clone();
        let _ = self
            .client
            .call(methods::INPUT_SEND, json!({ "session": tab.id, "data": data }))
            .await;
        Ok(())
    }

    /// 选择模式下的光标移动
    fn move_selection_head(&mut self, dr: isize, dc: isize) {
        let Some(sel) = self.selection.as_mut() else {
            return;
        };
        let max_r = self.term_size.1.saturating_sub(1) as usize;
        let max_c = self.term_size.0.saturating_sub(1) as usize;
        let r = (sel.head.0 as isize + dr).clamp(0, max_r as isize) as usize;
        let c = (sel.head.1 as isize + dc).clamp(0, max_c as isize) as usize;
        sel.head = (r, c);
    }

    fn cycle_tab(&mut self, delta: isize) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        self.active = ((self.active as isize + delta).rem_euclid(n)) as usize;
    }

    fn scroll_active(&mut self, delta: isize) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.screen.scroll_view(delta);
        }
    }

    async fn close_active(&mut self) -> anyhow::Result<()> {
        if self.tabs.is_empty() {
            return Ok(());
        }
        let tab = self.tabs.remove(self.active);
        if self.active >= self.tabs.len() && !self.tabs.is_empty() {
            self.active = self.tabs.len() - 1;
        }
        let _ = self
            .client
            .call(methods::SESSION_CLOSE, json!({ "session": tab.id }))
            .await;
        self.notice = format!("已关闭 {}", tab.name);
        Ok(())
    }

    /// 终端尺寸变化：同步本地 Screen 并通知远端（需求 FR-2.4）
    async fn on_resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        for tab in &mut self.tabs {
            tab.screen.resize(cols as usize, rows as usize);
            let _ = self
                .client
                .call(
                    methods::RESIZE,
                    json!({ "session": tab.id, "cols": cols, "rows": rows }),
                )
                .await;
        }
        self.term_size = (cols, rows);
        Ok(())
    }

    fn on_ipc(&mut self, msg: ServerMsg) {
        let ServerMsg::Event { event, session, data, .. } = msg else {
            return;
        };
        let Some(sid) = session else { return };
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == sid) else {
            return;
        };
        let data = data.unwrap_or(json!({}));
        match event.as_str() {
            "output" => {
                if let Some(s) = data.get("data").and_then(|v| v.as_str()) {
                    tab.screen.feed(s.as_bytes());
                }
            }
            "state" => {
                if let Some(s) = data.get("state").and_then(|v| v.as_str()) {
                    tab.state = parse_state(s);
                }
            }
            "reconnected" => {
                tab.state = SessionState::Ready;
                if let Some(a) = data.get("attempt").and_then(|v| v.as_u64()) {
                    tab.reconnect_count = a as u32;
                    tab.screen.mark_reconnect(&format!("第 {a} 次重连成功"));
                }
                self.notice = format!("{} 已重连", tab.name);
            }
            "closed" => {
                tab.state = SessionState::Closed;
                let reason = data.get("reason").and_then(|v| v.as_str()).unwrap_or("已结束");
                tab.screen.push_notice(&format!("会话结束：{reason}"));
            }
            _ => {}
        }
    }

    // ---- 渲染 ----

    fn draw(&self, f: &mut ratatui::Frame) {
        if let Some(form) = &self.form {
            self.draw_form(f, form);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
            .split(f.area());

        self.draw_tabs(f, chunks[0]);

        let body = if self.show_tree {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(TREE_WIDTH), Constraint::Min(1)])
                .split(chunks[1]);
            self.draw_tree(f, cols[0]);
            cols[1]
        } else {
            chunks[1]
        };

        self.draw_terminal(f, body);
        self.draw_status(f, chunks[2]);

        // 搜索框浮在底部状态栏之上
        if let Some(q) = &self.query {
            let area = Rect {
                x: chunks[2].x,
                y: chunks[2].y.saturating_sub(1),
                width: chunks[2].width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(format!("/{q}")).style(
                    Style::default()
                        .fg(self.theme.status_bg)
                        .add_modifier(Modifier::BOLD),
                ),
                area,
            );
        }

        if let Some(host) = &self.pending_delete {
            let area = centered_rect(50, 5, f.area());
            let block = Block::default().borders(Borders::ALL).title("确认删除");
            let text = vec![
                Line::from(format!("确定删除主机 `{}`？", host.name)),
                Line::from(""),
                Line::from("按 y 确认，其它键取消"),
            ];
            f.render_widget(
                Paragraph::new(text).block(block).alignment(Alignment::Center),
                area,
            );
        }
    }

    fn draw_tabs(&self, f: &mut ratatui::Frame, area: Rect) {
        let mut spans: Vec<Span> = Vec::new();
        let mut used = 0usize;
        for (i, tab) in self.tabs.iter().enumerate() {
            let label = format!(" {}{} ", state_dot(tab.state), tab.name);
            if used + label.chars().count() > area.width as usize {
                spans.push(Span::raw(format!(" +{}", self.tabs.len() - i)));
                break;
            }
            used += label.chars().count();
            let style = if i == self.active {
                Style::default().fg(self.theme.status_fg).bg(self.theme.status_bg)
            } else {
                Style::default().fg(self.theme.fg)
            };
            spans.push(Span::styled(label, style));
        }
        if self.tabs.is_empty() {
            spans.push(Span::styled(
                " 无会话：在左侧选择主机回车连接，Ctrl+T 新建 ",
                Style::default().fg(self.theme.border).add_modifier(Modifier::DIM),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_tree(&self, f: &mut ratatui::Frame, area: Rect) {
        let focused = self.focus == Focus::Tree;
        let border = if focused { self.theme.accent } else { self.theme.border_dim };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(" 主机 ");
        let inner = block.inner(area);
        f.render_widget(block, area);

        let rows = self.tree.rows();
        let mut lines: Vec<Line> = Vec::new();
        for (i, row) in rows.iter().enumerate().take(inner.height as usize) {
            let selected = i == self.tree.selected;
            let text = match row {
                Row::Group { path, count, expanded } => {
                    format!("{} {path} ({count})", if *expanded { "▾" } else { "▸" })
                }
                Row::Host(h) => {
                    let auth = match &h.auth {
                        sh0_core::CredentialRef::Agent => "agent",
                        sh0_core::CredentialRef::KeyFile { .. } => "key",
                        sh0_core::CredentialRef::Password { .. } => "pw",
                        sh0_core::CredentialRef::Interactive => "kbd",
                        sh0_core::CredentialRef::None => "none",
                    };
                    format!("{} [{}]", h.name, auth)
                }
            };
            let style = if selected {
                Style::default()
                    .fg(self.theme.status_fg)
                    .bg(self.theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(self.theme.fg)
            };
            lines.push(Line::from(Span::styled(truncate(&text, inner.width as usize), style)));
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "（空）按 n 新建",
                Style::default().fg(self.theme.border),
            )));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_terminal(&self, f: &mut ratatui::Frame, area: Rect) {
        let Some(tab) = self.tabs.get(self.active) else {
            let hint = "左侧选择主机后回车连接 · Ctrl+T 新建 · Ctrl+B 切换焦点 · Ctrl+Q 退出";
            f.render_widget(
                Paragraph::new(hint).style(
                    Style::default().fg(self.theme.border).add_modifier(Modifier::DIM),
                ),
                area,
            );
            return;
        };

        let rows = area.height as usize;
        let cols = area.width as usize;
        let lines = tab.screen.view_lines();
        let mut out: Vec<Line> = Vec::with_capacity(rows);

        for i in 0..rows {
            match lines.get(i) {
                Some(line) => out.push(render_line(
                    line,
                    cols,
                    &self.theme,
                    self.selection.as_ref().and_then(|s| s.cols_for(i)),
                )),
                None => out.push(Line::raw("")),
            }
        }

        // 光标：只在贴着最新输出、且焦点在终端时显示
        if let (Some((cy, cx)), true) = (
            tab.screen.view_cursor(),
            tab.state == SessionState::Ready && self.focus == Focus::Terminal,
        )
            && cy < rows {
                let src = lines.get(cy).copied();
                if let Some(line) = src {
                    out[cy] = render_line_with_cursor(line, cols, cx, &self.theme);
                }
            }
        f.render_widget(Paragraph::new(out), area);
    }

    fn draw_status(&self, f: &mut ratatui::Frame, area: Rect) {
        let mut spans: Vec<Span> = Vec::new();
        if let Some(tab) = self.tabs.get(self.active) {
            spans.push(Span::styled(
                format!(" {} ", tab.state.as_str()),
                Style::default()
                    .fg(self.theme.status_fg)
                    .bg(self.theme.state_color(tab.state)),
            ));
            spans.push(Span::raw(format!(" {} ", tab.host)));
            if tab.reconnect_count > 0 {
                spans.push(Span::raw(format!("重连 {} 次 ", tab.reconnect_count)));
            }
            if tab.screen.is_scrolled() {
                spans.push(Span::styled(
                    format!(" 回看 {}/{} ", tab.screen.scroll_offset(), tab.screen.scrollback_len()),
                    Style::default().fg(self.theme.warn),
                ));
            }
        }
        if !self.notice.is_empty() {
            spans.push(Span::styled(
                format!(" {} ", self.notice),
                Style::default().fg(self.theme.warn),
            ));
        }
        let right = "Ctrl+Q 退出 · Ctrl+T 新建 · Ctrl+W 关闭 · Ctrl+B 焦点";
        let left_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let pad = area.width as usize - left_len - right.chars().count();
        let mut all = spans;
        if pad > 1 {
            all.push(Span::raw(" ".repeat(pad)));
        }
        all.push(Span::styled(
            right,
            Style::default().fg(self.theme.border).add_modifier(Modifier::DIM),
        ));
        f.render_widget(Paragraph::new(Line::from(all)), area);
    }

    fn draw_form(&self, f: &mut ratatui::Frame, form: &HostForm) {
        let area = centered_rect(60, form.fields.len() as u16 + 5, f.area());
        let title = if form.editing.is_some() { " 编辑主机 " } else { " 新建主机 " };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent))
            .title(title);
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();
        for (i, field) in form.fields.iter().enumerate() {
            let focused = i == form.focus;
            let marker = if focused { "▸" } else { " " };
            let style = if focused {
                Style::default().fg(self.theme.status_fg).bg(self.theme.accent)
            } else {
                Style::default().fg(self.theme.fg)
            };
            lines.push(Line::from(vec![
                Span::raw(format!("{marker} {:<28}", field.label)),
                Span::styled(format!(" {} ", field.value), style),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Enter 保存 · Esc 取消 · Tab/↑↓ 切换字段",
            Style::default().fg(self.theme.border),
        )));
        f.render_widget(Paragraph::new(lines), inner);
    }
}

fn state_dot(state: SessionState) -> &'static str {
    match state {
        SessionState::Ready => "●",
        SessionState::Connecting | SessionState::Authenticating => "◐",
        SessionState::Degraded | SessionState::Reconnecting => "◑",
        SessionState::Closed => "○",
    }
}

fn parse_state(s: &str) -> SessionState {
    match s {
        "ready" => SessionState::Ready,
        "connecting" => SessionState::Connecting,
        "authenticating" => SessionState::Authenticating,
        "degraded" => SessionState::Degraded,
        "reconnecting" => SessionState::Reconnecting,
        _ => SessionState::Closed,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn centered_rect(width_pct: u16, height: u16, area: Rect) -> Rect {
    let w = area.width * width_pct / 100;
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// 跑起 TUI。调用方已建立好 IPC 连接。
pub async fn run(
    client: Arc<sh0_ipc::IpcClient>,
    events: UnboundedReceiver<ServerMsg>,
    store: Arc<Store>,
    cfg: sh0_store::AppConfig,
    host_selector: Option<&str>,
) -> anyhow::Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((cfg.cols, cfg.rows));
    // 终端区尺寸：减去标签栏与状态栏，再减去可选树宽
    let term_cols = cols.saturating_sub(TREE_WIDTH).max(20);
    let term_rows = rows.saturating_sub(2).max(5);

    let mut app = App::new(client, events, store, cfg);
    app.term_size = (term_cols, term_rows);
    app.attach_existing().await?;

    if let Some(h) = host_selector {
        app.open(h).await?;
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = loop_inner(&mut terminal, &mut app).await;

    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen);

    result
}

async fn loop_inner(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> anyhow::Result<()> {
    loop {
        if app.should_quit {
            return Ok(());
        }

        // 1) 本地输入
        while event::poll(Duration::from_millis(0))? {
            match event::read()? {
                Event::Key(key) => app.on_key(key).await?,
                Event::Resize(_, _) => {
                    let size = terminal.size()?;
                    let cols = size.width.saturating_sub(TREE_WIDTH).max(20);
                    let rows = size.height.saturating_sub(2).max(5);
                    app.on_resize(cols, rows).await?;
                }
                Event::Paste(text) => {
                    // 粘贴：带括号粘贴标记（若远端开启）
                    if let Some(tab) = app.tabs.get(app.active) {
                        let payload = if tab.screen.grid.bracketed_paste {
                            format!("\x1b[200~{text}\x1b[201~")
                        } else {
                            text
                        };
                        let _ = app
                            .client
                            .call(
                                methods::INPUT_SEND,
                                json!({ "session": tab.id, "data": payload }),
                            )
                            .await;
                    }
                }
                _ => {}
            }
        }

        // 2) daemon 推送
        while let Ok(msg) = app.events.try_recv() {
            app.on_ipc(msg);
        }

        // 3) 渲染
        terminal.draw(|f| app.draw(f))?;

        tokio::time::sleep(Duration::from_millis(FRAME_MS)).await;
    }
}

/// Ctrl+Q / Ctrl+C 都退出（raw mode 下 Ctrl+C 不会自动终止进程）
pub fn is_hard_quit(key: KeyEvent) -> bool {
    matches!(
        (key.code, key.modifiers),
        (KeyCode::Char('q'), KeyModifiers::CONTROL) | (KeyCode::Char('c'), KeyModifiers::CONTROL)
    )
}

/// 极简 base64 编码：只为 OSC 52 服务，不值得为它引入一个依赖
fn base64_encode(input: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TBL[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TBL[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}
