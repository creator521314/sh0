//! TUI 主应用：布局、键盘、事件循环（需求 FR-3.1~FR-3.4，W1.4 / W1.5 / W1.7）

use std::cell::Cell;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use serde_json::json;
use sh0_ipc::ServerMsg;
use sh0_ipc::protocol::methods;
use tokio::sync::mpsc::UnboundedReceiver;

use sh0_core::SessionState;
use sh0_store::Store;
use sh0_term::{Keymap, Screen, ScreenOptions};

use crate::files::{FilePane, entry_line};
use crate::hosts::{HostForm, HostTree, Row};
use crate::render::{render_line, render_line_with_cursor};
use crate::palette;
use crate::palette::Palette;
use crate::theme::Theme;
use sh0_core::sftp::FileEntry;
use sh0_term::keymap::Action;

/// 主机树默认宽度。可以被鼠标拖分隔条改掉，所以是"默认"而不是固定值
const DEFAULT_TREE_WIDTH: u16 = 28;
/// 主机树最窄拖到多少：再窄就显示不下 `[名字]` 了
const MIN_TREE_WIDTH: u16 = 12;
/// 终端区至少留这么多列，否则远端 shell 的折行会乱成一团
const MIN_TERM_COLS: u16 = 20;
/// 滚轮一次滚几行。比整页慢、比单行快，手感接近一般终端
const WHEEL_LINES: isize = 3;
const FRAME_MS: u64 = 16;
/// 双击判定窗口（FR-3.9）：同一个主机行两次点击落在这个间隔内才算双击
const DOUBLE_CLICK_MS: u128 = 400;
/// 传输分块大小：每帧一块，256 KB 在"进度够细"和"往返次数"之间取平衡
const CHUNK_SIZE: usize = 256 * 1024;
/// 单块失败后的最大重试次数（FR-4.4）
const MAX_RETRIES: u32 = 3;

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
    /// 双栏文件面板（FR-4.2）。具体是哪一栏看 [`App::file_focus_left`]
    Files,
}

/// 主区渲染什么：终端 或 双栏文件管理
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Terminal,
    Files,
}

/// 文件视图里等待输入的动作（统一走一个输入框，省得每种操作各写一套状态机）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePrompt {
    /// 跳到某个路径
    Goto,
    /// 在当前目录下新建子目录
    Mkdir,
    /// 重命名，携带被改的**完整路径**
    Rename(String),
    /// 改权限，携带目标完整路径
    Chmod(String),
}

/// 一个传输任务的状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferState {
    /// 排队中
    Pending,
    /// 正在传
    Active,
    Done,
    /// 重试次数用尽
    Failed,
}

/// 传输队列里的一项（FR-4.4）
#[derive(Debug, Clone)]
pub struct Transfer {
    pub name: String,
    /// 源路径
    pub src: String,
    /// 目标路径
    pub dst: String,
    /// true = 上传（本地 → 远端）
    pub upload: bool,
    pub size: u64,
    pub done: u64,
    pub state: TransferState,
    pub retries: u32,
    /// 平均速度（字节/秒）
    pub speed: u64,
    pub error: Option<String>,
    started: Option<std::time::Instant>,
}

impl Transfer {
    /// 进度 0.0~1.0；未知大小（0）时按未完成处理
    pub fn progress(&self) -> f64 {
        if self.size == 0 {
            return if self.state == TransferState::Done { 1.0 } else { 0.0 };
        }
        (self.done as f64 / self.size as f64).clamp(0.0, 1.0)
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state, TransferState::Done | TransferState::Failed)
    }
}

impl FilePrompt {
    /// 输入框前缀（提示用户现在在填什么）
    pub fn label(&self) -> &'static str {
        match self {
            FilePrompt::Goto => "跳转路径",
            FilePrompt::Mkdir => "新建目录名",
            FilePrompt::Rename(_) => "重命名为",
            FilePrompt::Chmod(_) => "权限(八进制)",
        }
    }
}

/// 文本选区，坐标是**视图坐标**（随滚动一起移动）
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

/// 标签栏上一个标签的命中区（FR-3.9 的"点击切换标签"）
///
/// 标签栏只有一行高，所以只需要横向区间。分两段：
/// 主体点一下切过去，`×` 那一段点一下关掉。
#[derive(Debug, Clone, Copy)]
pub struct TabHit {
    pub idx: usize,
    /// 标签主体 `[起, 止)`，点这里 = 切换会话
    pub body: (u16, u16),
    /// 关闭按钮 `[起, 止)`，点这里 = 关闭会话。没开鼠标时是空区间
    pub close: (u16, u16),
}

/// 渲染一段标签要的东西：文本、是否当前标签，外加它的命中区
#[derive(Debug, Clone)]
pub struct TabSegment {
    pub hit: TabHit,
    pub label: String,
    pub active: bool,
}

/// 点在标签栏的哪儿：主体（切换）还是 `×`（关闭）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabTarget {
    Switch(usize),
    Close(usize),
}

/// 鼠标拖拽中：要么在拖分隔条，要么在终端里框选文本
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drag {
    /// 拖主机树与主区之间的分隔条
    Divider,
    /// 终端内框选（`selection.anchor` 已经在按下时定好）
    Select,
}

/// 待人工确认的 exec 请求（daemon 广播 confirm.requested，FR-7.4）
///
/// 会话型调用带 session  id，一键式 `--host` 调用不带——所以确认事件
/// 不能走"按 session 找标签页"的那条路，必须在事件入口优先分流。
#[derive(Debug, Clone)]
pub struct PendingConfirm {
    pub token: String,
    pub host: String,
    pub command: String,
    pub reason: Option<String>,
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
    /// 待确认的 exec 弹窗（confirm.requested）；Some 时拦截全部按键
    confirm: Option<PendingConfirm>,
    /// 主区视图：终端还是文件管理
    view: View,
    /// 左侧：本机目录
    files_left: FilePane,
    /// 右侧：远端目录（走当前会话的 SFTP 通道）
    files_right: FilePane,
    /// 文件视图里焦点在哪一栏
    file_focus_left: bool,
    /// 输入提示：`(要做什么, 已输入内容)`
    file_prompt: Option<(FilePrompt, String)>,
    /// 待确认的删除：`(是否左栏, 路径, 是否目录)`
    file_delete: Option<(bool, String, bool)>,
    /// 右栏当前绑定到的会话（换标签页要重新列目录）
    file_sid: Option<String>,
    /// 左栏被标记的文件路径
    marks_left: std::collections::HashSet<String>,
    /// 右栏被标记的文件路径
    marks_right: std::collections::HashSet<String>,
    /// 传输队列（FR-4.4）
    transfers: Vec<Transfer>,
    /// 广播开关（FR-5.1）：开着时终端输入同时发给所有参与会话
    broadcast: bool,
    /// 参与广播的会话 id；空 = 全部已打开会话
    broadcast_targets: std::collections::HashSet<String>,
    /// 鼠标支持总开关（配置 `mouse`，FR-3.9）
    mouse: bool,
    /// 当前是否处于鼠标捕获态。Shift 拖选时会临时挂起，之后由按键事件恢复
    mouse_captured: bool,
    /// 最近一次绘制的主机树内区（鼠标命中测试要用；`draw` 是 `&self`，故用 `Cell`）
    tree_area: Cell<Rect>,
    /// 主机树当前宽度（鼠标拖分隔条可调，FR-3.9）
    tree_width: u16,
    /// 最近一次绘制的标签栏矩形（命中区由它现场算，见 `tab_hits`）
    tab_bar_area: Cell<Rect>,
    /// 最近一次绘制的主区（终端或文件视图）——滚轮与拖选的命中区
    main_area: Cell<Rect>,
    /// 最近一次绘制的文件双栏内区（鼠标点条目要用）
    pane_left_area: Cell<Rect>,
    pane_right_area: Cell<Rect>,
    /// 鼠标正在拖什么：`None` 表示没在拖
    drag: Option<Drag>,
    /// 上一次点击 `(时刻, 行号)`，用于双击判定
    last_click: Option<(Instant, usize)>,
    /// 命令面板（FR-3.6 / W5.1）；Some 时拦截全部按键
    palette: Option<Palette>,
    /// 当前主题名，供面板显示"当前"标记与切换后回写配置
    theme_name: String,
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
        let theme = Theme::preset(&cfg.theme);
        let budget = if cfg.memory_budget_mb == 0 {
            None
        } else {
            Some(sh0_term::MemoryBudget::new(
                cfg.memory_budget_mb * 1024 * 1024,
            ))
        };
        let _ = budget; // TUI 侧每个标签各自持有 Screen，预算在 daemon 侧统一约束

        Self {
            client,
            events,
            tabs: Vec::new(),
            active: 0,
            focus: Focus::Tree,
            keymap,
            theme,
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
            confirm: None,
            view: View::Terminal,
            files_left: FilePane::local(),
            files_right: FilePane::remote(),
            file_focus_left: true,
            file_prompt: None,
            file_delete: None,
            file_sid: None,
            marks_left: std::collections::HashSet::new(),
            marks_right: std::collections::HashSet::new(),
            transfers: Vec::new(),
            broadcast: false,
            broadcast_targets: std::collections::HashSet::new(),
            mouse: cfg.mouse,
            mouse_captured: false,
            tree_area: Cell::new(Rect::default()),
            tree_width: DEFAULT_TREE_WIDTH,
            tab_bar_area: Cell::new(Rect::default()),
            main_area: Cell::new(Rect::default()),
            pane_left_area: Cell::new(Rect::default()),
            pane_right_area: Cell::new(Rect::default()),
            drag: None,
            last_click: None,
            palette: None,
            theme_name: cfg.theme.clone(),
        }
    }

    // --- 鼠标（FR-3.9）----------------------------------------------------
    //
    // 取舍：开启鼠标捕获后，父终端就收不到鼠标事件了，"按住左键拖选复制"会失效。
    // 因此约定 **Shift + 鼠标 = 交回父终端**：检测到 Shift 修饰就挂起捕获，
    // 之后的键盘事件再恢复。这样两种用法都保得住。

    /// 鼠标事件入口。目前只处理左侧主机树：单击选中、双击连接、滚轮滚动。
    pub async fn on_mouse(&mut self, m: MouseEvent) -> anyhow::Result<()> {
        if !self.mouse {
            return Ok(());
        }
        // Shift + 鼠标：用户想用系统自带的拖选复制，把事件交回父终端
        if m.modifiers.contains(KeyModifiers::SHIFT) {
            self.suspend_mouse();
            return Ok(());
        }
        // 表单 / 确认弹窗 / 输入提示期间不接受点击——这些界面是键盘模态的，
        // 点半下就可能把正在填的东西弄丢
        if self.form.is_some()
            || self.confirm.is_some()
            || self.pending_delete.is_some()
            || self.file_prompt.is_some()
            || self.file_delete.is_some()
            || self.palette.is_some()
        {
            return Ok(());
        }

        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // 1) 分隔条：压在主机树与主区的缝上，最容易误触，优先级给最高
                if self.on_divider(m.column, m.row) {
                    self.drag = Some(Drag::Divider);
                    return Ok(());
                }
                // 2) 标签栏：点主体切过去，点 × 关掉（FR-3.9）
                if let Some(target) = self.tab_hit_at(m.column, m.row) {
                    match target {
                        TabTarget::Switch(i) => self.switch_to_tab(i).await?,
                        TabTarget::Close(i) => self.close_tab(i).await?,
                    }
                    return Ok(());
                }
                // 3) 主区终端：按下即定选区起点，拖到哪选到哪，松手复制
                if self.view == View::Terminal
                    && let Some(cell) = self.view_cell_at(m.column, m.row)
                {
                    self.selection = Some(Selection {
                        anchor: cell,
                        head: cell,
                    });
                    self.drag = Some(Drag::Select);
                    self.focus = Focus::Terminal;
                    return Ok(());
                }
                // 4) 文件视图：点一下选中并把焦点给这一栏，双击进入（与回车同一条路）
                if self.view == View::Files
                    && let Some((is_left, idx)) = self.file_row_at(m.column, m.row)
                {
                    self.file_focus_left = is_left;
                    let pane = if is_left {
                        &mut self.files_left
                    } else {
                        &mut self.files_right
                    };
                    pane.selected = idx;
                    let now = Instant::now();
                    let is_double = matches!(self.last_click,
                        Some((t, r)) if r == idx
                            && now.duration_since(t).as_millis() < DOUBLE_CLICK_MS);
                    self.last_click = Some((now, idx));
                    if is_double {
                        self.file_enter().await;
                    }
                    return Ok(());
                }
                // 5) 主机树（M5 先行项的行为不变）
                let Some(idx) = self.tree_row_at(m.column, m.row) else {
                    return Ok(());
                };
                // 点在树上就等于把焦点给树（省掉先按 Ctrl+B 这一步）
                if self.focus != Focus::Tree {
                    self.focus = Focus::Tree;
                }
                let now = Instant::now();
                let is_double = matches!(self.last_click,
                    Some((t, r)) if r == idx
                        && now.duration_since(t).as_millis() < DOUBLE_CLICK_MS);
                self.last_click = Some((now, idx));

                self.tree.selected = idx;
                if is_double {
                    // 与回车完全同一条路径：分组行展开/折叠，主机行发起连接
                    if let Some(h) = self.tree.activate() {
                        self.open(&h.name).await?;
                    }
                }
            }
            // 中键关标签：浏览器的习惯，比精准点那个 × 省事
            MouseEventKind::Down(MouseButton::Middle) => {
                if let Some(target) = self.tab_hit_at(m.column, m.row) {
                    let i = match target {
                        TabTarget::Switch(i) | TabTarget::Close(i) => i,
                    };
                    self.close_tab(i).await?;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => match self.drag {
                Some(Drag::Divider) => self.drag_divider_to(m.column),
                Some(Drag::Select) => {
                    if let (Some(cell), Some(sel)) =
                        (self.view_cell_at(m.column, m.row), self.selection.as_mut())
                    {
                        sel.head = cell;
                    }
                }
                None => {}
            },
            MouseEventKind::Up(MouseButton::Left) => {
                match self.drag {
                    // 拖的过程中只改本地布局，松手才把新尺寸同步给所有会话，
                    // 否则每一帧都要发一轮 resize，拖一下就是几十次 IPC
                    Some(Drag::Divider) => {
                        let (c, r) = self.term_size;
                        self.on_resize(c, r).await?;
                    }
                    Some(Drag::Select) => self.finish_mouse_select().await?,
                    None => {}
                }
                self.drag = None;
            }
            MouseEventKind::ScrollUp => self.on_wheel(WHEEL_LINES, m.column, m.row).await,
            MouseEventKind::ScrollDown => self.on_wheel(-WHEEL_LINES, m.column, m.row).await,
            _ => {}
        }
        Ok(())
    }

    async fn on_wheel(&mut self, delta: isize, col: u16, row: u16) {
        // 主机树：滚选中行（M5 先行项的行为）。
        // delta 的符号沿用终端那套（正 = 往回看/往上），所以树里要反过来用
        if self.tree_row_at(col, row).is_some() {
            if delta > 0 {
                self.tree.move_selection(-1);
            } else {
                self.tree.move_selection(1);
            }
            return;
        }
        if !self.in_main(col, row) {
            return;
        }
        match self.view {
            View::Terminal => {
                // 正 delta = 往回看历史（scroll_offset 变大）
                self.scroll_active(delta);
            }
            View::Files => self.file_move(if delta > 0 { -1 } else { 1 }),
        }
    }

    /// 标签栏命中测试（只有一行高，故只需比对 x 区间）
    fn tab_hit_at(&self, col: u16, row: u16) -> Option<TabTarget> {
        let area = self.tab_bar_area.get();
        if row != area.y || area.width == 0 {
            return None;
        }
        self.tab_hits(area)
            .into_iter()
            .find_map(|h| {
                if h.body.0 <= col && col < h.body.1 {
                    Some(TabTarget::Switch(h.idx))
                } else if h.close.0 <= col && col < h.close.1 {
                    Some(TabTarget::Close(h.idx))
                } else {
                    None
                }
            })
    }

    /// 是否落在主区（终端 / 文件视图）
    fn in_main(&self, col: u16, row: u16) -> bool {
        let a = self.main_area.get();
        a.width > 0
            && a.height > 0
            && col >= a.x
            && col < a.x.saturating_add(a.width)
            && row >= a.y
            && row < a.y.saturating_add(a.height)
    }

    /// 屏幕坐标 → 终端视图坐标 `(行, 列)`。
    ///
    /// 终端内容是从主区左上角 (0,0) 开始画的，而 `extract_view_text` 用的也是视图坐标，
    /// 所以这里**不用**扣滚动偏移——滚动已经在 `view_lines()` 里体现过了。
    fn view_cell_at(&self, col: u16, row: u16) -> Option<(usize, usize)> {
        if !self.in_main(col, row) {
            return None;
        }
        let a = self.main_area.get();
        Some((
            (row.saturating_sub(a.y)) as usize,
            (col.saturating_sub(a.x)) as usize,
        ))
    }

    /// 文件视图命中测试：屏幕坐标 → `(是否左栏, 条目索引)`。
    ///
    /// 底部那一行是状态（条目数 / 排序 / 错误），点它不算点条目。
    fn file_row_at(&self, col: u16, row: u16) -> Option<(bool, usize)> {
        if self.view != View::Files {
            return None;
        }
        for (is_left, area) in [
            (true, self.pane_left_area.get()),
            (false, self.pane_right_area.get()),
        ] {
            if area.width == 0 || area.height == 0 {
                continue;
            }
            if col < area.x
                || col >= area.x.saturating_add(area.width)
                || row < area.y
                || row >= area.y.saturating_add(area.height)
            {
                continue;
            }
            let pane = if is_left {
                &self.files_left
            } else {
                &self.files_right
            };
            // 条目区比内区矮一行（最后一行留给状态）
            let rows = area.height.saturating_sub(1) as usize;
            let line = (row - area.y) as usize;
            if line >= rows {
                continue;
            }
            let idx = pane.scroll + line;
            if idx < pane.entries.len() {
                return Some((is_left, idx));
            }
        }
        None
    }

    /// 是否点在主机树与主区之间的分隔条上
    fn on_divider(&self, col: u16, row: u16) -> bool {
        if !self.show_tree {
            return false;
        }
        let a = self.main_area.get();
        if a.height == 0 || row < a.y || row >= a.y.saturating_add(a.height) {
            return false;
        }
        let seam = self.tree_width;
        col == seam.saturating_sub(1) || col == seam
    }

    /// 拖动分隔条：跟着鼠标改树宽，同时把腾出来/占掉的列数记进终端尺寸
    fn drag_divider_to(&mut self, col: u16) {
        // 总宽 = 当前终端列数 + 当前树宽（拖拽中终端还没同步，两者相加仍是窗口宽度）
        let total = self.term_size.0.saturating_add(self.tree_width);
        let max_w = total
            .saturating_sub(MIN_TERM_COLS)
            .max(MIN_TREE_WIDTH);
        let want = col.saturating_add(1).max(MIN_TREE_WIDTH);
        let w = want.clamp(MIN_TREE_WIDTH, max_w);
        let delta = w as isize - self.tree_width as isize;
        self.tree_width = w;
        self.term_size.0 =
            (self.term_size.0 as isize - delta).clamp(MIN_TERM_COLS as isize, total as isize) as u16;
    }

    /// 鼠标框选松手：真拖过就复制，只点一下就当取消
    async fn finish_mouse_select(&mut self) -> anyhow::Result<()> {
        let Some(sel) = self.selection else {
            return Ok(());
        };
        if sel.anchor == sel.head {
            // 没有拖动：清掉选区就好，别往剪贴板里塞一个字符
            self.selection = None;
            return Ok(());
        }
        self.copy_selection().await
    }

    /// 切到第 idx 个标签，并把焦点给终端——点标签的人就是要去打字
    async fn switch_to_tab(&mut self, idx: usize) -> anyhow::Result<()> {
        if idx >= self.tabs.len() {
            return Ok(());
        }
        // 点标签的人是要去操作那个会话：终端视图给终端，文件视图给文件区
        self.focus = if self.view == View::Files {
            Focus::Files
        } else {
            Focus::Terminal
        };
        if idx == self.active {
            return Ok(());
        }
        self.active = idx;
        self.tab_changed().await;
        Ok(())
    }

    /// 关掉第 idx 个标签（键盘的 Ctrl+W 只关当前，鼠标能点任意一个）
    async fn close_tab(&mut self, idx: usize) -> anyhow::Result<()> {
        if idx >= self.tabs.len() {
            return Ok(());
        }
        let tab = self.tabs.remove(idx);
        if self.tabs.is_empty() {
            self.active = 0;
        } else if idx < self.active {
            self.active -= 1;
        } else if idx == self.active && self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        let _ = self
            .client
            .call(methods::SESSION_CLOSE, json!({ "session": tab.id }))
            .await;
        self.notice = format!("已关闭 {}", tab.name);
        self.tab_changed().await;
        Ok(())
    }

    /// 屏幕坐标 → 主机树行号。不在树区、或落在可见行之外，返回 `None`。
    fn tree_row_at(&self, col: u16, row: u16) -> Option<usize> {
        if !self.show_tree {
            return None;
        }
        let area = self.tree_area.get();
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if col < area.x
            || col >= area.x.saturating_add(area.width)
            || row < area.y
            || row >= area.y.saturating_add(area.height)
        {
            return None;
        }
        let idx = (row - area.y) as usize;
        // 渲染时只画 `inner.height` 行，超出部分点不到
        let visible = self.tree.rows().len().min(area.height as usize);
        (idx < visible).then_some(idx)
    }

    /// 挂起鼠标捕获（Shift 拖选用）。失败只告警——捕获态错了最多是鼠标不灵，
    /// 不该让 TUI 崩掉。
    fn suspend_mouse(&mut self) {
        if self.mouse_captured {
            let _ = execute!(io::stdout(), DisableMouseCapture);
            self.mouse_captured = false;
        }
    }

    /// 恢复鼠标捕获。挂起之后由任意键盘事件触发（捕获关着时收不到鼠标事件，
    /// 只能借键盘回来）。
    pub fn restore_mouse(&mut self) {
        if self.mouse && !self.mouse_captured && execute!(io::stdout(), EnableMouseCapture).is_ok()
        {
            self.mouse_captured = true;
        }
    }

    /// 测试与宿主注入布局用：告诉 App 主机树画在哪（避免为命中测试真的渲染一帧）
    pub fn set_tree_area(&mut self, area: Rect) {
        self.tree_area.set(area);
    }

    /// 最近一次渲染记下的主机树内区（测试用）
    pub fn tree_area(&self) -> Rect {
        self.tree_area.get()
    }

    /// 测试与宿主注入布局用：标签栏画在哪
    pub fn set_tab_bar_area(&mut self, area: Rect) {
        self.tab_bar_area.set(area);
    }

    /// 测试与宿主注入布局用：主区（终端 / 文件视图）画在哪
    pub fn set_main_area(&mut self, area: Rect) {
        self.main_area.set(area);
    }

    /// 测试与宿主注入布局用：文件双栏的内区画在哪
    pub fn set_pane_areas(&mut self, left: Rect, right: Rect) {
        self.pane_left_area.set(left);
        self.pane_right_area.set(right);
    }

    /// 文件视图里焦点是否在左栏（测试用）
    pub fn file_focus_is_left(&self) -> bool {
        self.file_focus_left
    }

    /// 当前活动标签的序号（测试用）
    pub fn active_tab(&self) -> usize {
        self.active
    }

    /// 主机树当前宽度（测试用；拖分隔条会改它）
    pub fn tree_width(&self) -> u16 {
        self.tree_width
    }

    /// 当前选区（测试用）
    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// 测试与宿主注入用：往第 idx 个会话的屏幕塞几行（模拟 daemon 推送输出）。
    ///
    /// 契约测试跑不起真终端，滚动与选区这类"依赖屏幕上真有内容"的行为
    /// 只能这样喂进去——否则 scrollback 是空的，测什么都是 0。
    pub fn feed_lines(&mut self, idx: usize, lines: &[&str]) {
        if let Some(tab) = self.tabs.get_mut(idx) {
            for l in lines {
                tab.screen.feed(format!("{l}\r\n").as_bytes());
            }
        }
    }

    /// 最近一次复制的内容（测试用）
    pub fn last_copy(&self) -> &str {
        &self.last_copy
    }

    /// 当前标签回看了多少行（测试用；0 表示贴着最新输出）
    pub fn scroll_offset(&self) -> usize {
        self.tabs
            .get(self.active)
            .map(|t| t.screen.scroll_offset())
            .unwrap_or(0)
    }

    /// 当前选中的主机树行号（测试用）
    pub fn tree_selection(&self) -> usize {
        self.tree.selected
    }

    /// 当前主题名（测试用）
    pub fn theme_name(&self) -> &str {
        &self.theme_name
    }

    /// 已打开的会话（标签）数量（测试用）
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// 焦点是否在主机树上（测试用）
    pub fn focus_is_tree(&self) -> bool {
        self.focus == Focus::Tree
    }

    /// 当前是否处于鼠标捕获态（测试用）
    pub fn mouse_captured(&self) -> bool {
        self.mouse_captured
    }

    /// 接入 daemon 里已经存在的会话（TUI 退出并不关闭会话）
    pub async fn attach_existing(&mut self) -> anyhow::Result<()> {
        let res = self.client.call(methods::SESSION_LIST, json!({})).await?;
        let empty = vec![];
        let list = res
            .get("sessions")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);
        for s in list {
            let id = s.get("id").and_then(|v| v.as_str()).unwrap_or_default();
            if id.is_empty() || self.tabs.iter().any(|t| t.id == id) {
                continue;
            }
            let name = s
                .get("host_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            let state = s.get("state").and_then(|v| v.as_str()).unwrap_or("closed");
            let mut tab = self.new_tab(id.to_string(), name.clone(), name.clone());
            tab.state = parse_state(state);
            tab.screen.push_notice(
                "已接入 daemon 中的既有会话（历史输出请用 `sh0 ai screen read` 查看）",
            );
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
            .call(
                methods::SESSION_OPEN,
                json!({ "host": host, "cols": cols, "rows": rows }),
            )
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
        self.tabs
            .push(self.new_tab(id, name.clone(), host.to_string()));
        self.active = self.tabs.len() - 1;
        if self.view != View::Files {
            self.focus = Focus::Terminal;
        }
        self.notice = format!("已连接 {name}");
        // 文件视图下右栏要切到这台主机
        self.tab_changed().await;
        Ok(())
    }

    // ---- 事件 ----

    /// 按键入口。pub 是为了让契约测试能直接喂按键，不必真的跑一个终端。
    pub async fn on_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        // 鼠标捕获可能因为 Shift 拖选被挂起，按键回来时顺手恢复
        self.restore_mouse();
        // 0) 风险确认弹窗优先：AI 在等一个"人"拍板，此刻任何键都属于这次决定
        if let Some(pc) = self.confirm.take() {
            // 强退键放行：令牌留给 daemon 5 分钟自然过期，人永远能退出 TUI
            if is_hard_quit(key) {
                self.should_quit = true;
                return Ok(());
            }
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    let res = self
                        .client
                        .call(
                            methods::EXEC_CONFIRM,
                            json!({ "token": pc.token, "approve": true }),
                        )
                        .await;
                    self.notice = match res {
                        Ok(_) => "已批准，正在执行…".into(),
                        Err(e) => format!("批准失败：{e}"),
                    };
                }
                _ => {
                    let _ = self
                        .client
                        .call(
                            methods::EXEC_CONFIRM,
                            json!({ "token": pc.token, "approve": false }),
                        )
                        .await;
                    self.notice = "已拒绝该命令".into();
                }
            }
            return Ok(());
        }

        // 1) 命令面板：它自己就是个输入框，按键全归它
        if self.palette.is_some() {
            // 先按当前输入算好候选，再进可变分支，免得借用打架
            let total = self.palette_matches().len();
            let Some(p) = self.palette.as_mut() else {
                return Ok(());
            };
            match key.code {
                KeyCode::Esc => self.palette = None,
                KeyCode::Up => p.move_sel(-1, total),
                KeyCode::Down => p.move_sel(1, total),
                KeyCode::Backspace => p.backspace(),
                KeyCode::Enter => {
                    let sel = p.sel;
                    let input = p.input.clone();
                    self.palette = None;
                    // 用关闭前的输入重算候选：面板已关，palette_matches() 会退回全量
                    let picked = self.matches_for(&input).get(sel).map(|i| i.cmd.clone());
                    if let Some(cmd) = picked {
                        self.run_palette_cmd(cmd).await?;
                    }
                }
                KeyCode::Char(c) => p.push(c),
                _ => {}
            }
            return Ok(());
        }

        // 2) 主机编辑表单优先
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

        // 3) 删除确认
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

        // 4) 搜索输入
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

        // 5) 选择模式：方向键用来移动选区，不发给远端
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

        // 6) 键位映射（命中的按键不会发给远端）
        if let Some(action) = self.keymap.get(key) {
            return self.on_action(action).await;
        }

        // 7) 焦点相关的裸按键
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
            Focus::Files => return self.on_file_key(key).await,
            Focus::Terminal => {
                // 广播开着时，这份输入要发到所有参与会话（FR-5.1）
                let broadcast = self.broadcast;
                let targets: Vec<String> = if broadcast && !self.broadcast_targets.is_empty() {
                    self.broadcast_targets.iter().cloned().collect()
                } else {
                    Vec::new()
                };
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    let app_keys = tab.screen.grid.app_cursor_keys;
                    if let Some(bytes) = sh0_term::encode_key(key, app_keys) {
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        if broadcast {
                            let mut params = json!({ "data": text });
                            if !targets.is_empty() {
                                params["sessions"] = json!(targets);
                            }
                            let _ = self.client.call(methods::BROADCAST_SEND, params).await;
                        } else {
                            let _ = self
                                .client
                                .call(
                                    methods::INPUT_SEND,
                                    json!({ "session": tab.id, "data": text }),
                                )
                                .await;
                        }
                    }
                    // 有输入说明用户在看最新输出，回到底部
                    tab.screen.reset_view();
                }
            }
        }
        Ok(())
    }

    /// 文件视图的按键：删除确认 → 输入提示 → 导航/操作
    async fn on_file_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        // 0) 删除确认：删文件是不可逆的，一律先问一句
        if let Some((is_left, path, is_dir)) = self.file_delete.take() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.file_delete(is_left, &path, is_dir).await
                }
                _ => self.notice = "已取消删除".into(),
            }
            return Ok(());
        }

        // 1) 输入提示（跳转 / 新建目录 / 重命名 / 改权限）
        if self.file_prompt.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.file_prompt = None;
                    self.notice.clear();
                }
                KeyCode::Enter => {
                    let (kind, raw) = self.file_prompt.take().unwrap_or((FilePrompt::Goto, String::new()));
                    let text = raw.trim().to_string();
                    if !text.is_empty() {
                        match kind {
                            FilePrompt::Goto => self.file_goto(&text).await,
                            FilePrompt::Mkdir => self.file_mkdir(&text).await,
                            FilePrompt::Rename(p) => self.file_rename(&p, &text).await,
                            FilePrompt::Chmod(p) => self.file_chmod(&p, &text).await,
                        }
                    } else {
                        self.notice = "已取消".into();
                    }
                }
                KeyCode::Backspace => {
                    if let Some((_, b)) = self.file_prompt.as_mut() {
                        b.pop();
                    }
                }
                KeyCode::Char(c) => {
                    if let Some((_, b)) = self.file_prompt.as_mut() {
                        b.push(c);
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        // 2) 导航与操作
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.file_move(-1),
            KeyCode::Down | KeyCode::Char('j') => self.file_move(1),
            KeyCode::Enter | KeyCode::Right => self.file_enter().await,
            KeyCode::Backspace | KeyCode::Left => self.file_up().await,
            KeyCode::Tab => {
                self.toggle_file_pane();
                // 切到右栏时若还没列过目录，顺手列一次
                if !self.file_focus_left && self.file_sid.is_none() {
                    self.reload_remote().await;
                }
            }
            KeyCode::Char('g') => {
                self.file_prompt = Some((FilePrompt::Goto, String::new()));
                self.notice = "跳转：输入路径后回车（Esc 取消）".into();
            }
            KeyCode::Char('s') => self.file_sort(),
            KeyCode::Char('r') => self.file_refresh().await,
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Char('R') => self.retry_failed(),
            KeyCode::Char('c') => self.clear_finished(),
            KeyCode::Char('p') => {
                match self.pane().current_path() {
                    Some(p) => {
                        self.file_prompt = Some((FilePrompt::Chmod(p), String::new()));
                        self.notice = "权限：输入八进制，如 644（Esc 取消）".into();
                    }
                    None => self.notice = "没有选中项".into(),
                }
            }
            KeyCode::F(6) => {
                match self.pane().current().cloned() {
                    Some(e) => {
                        let cur = e.name.clone();
                        let path = e.path.clone();
                        self.file_prompt = Some((FilePrompt::Rename(path), cur));
                        self.notice = "重命名：改成新名字后回车（Esc 取消）".into();
                    }
                    None => self.notice = "没有选中项".into(),
                }
            }
            KeyCode::F(7) => {
                self.file_prompt = Some((FilePrompt::Mkdir, String::new()));
                self.notice = "新建目录：输入名字后回车（Esc 取消）".into();
            }
            KeyCode::F(8) | KeyCode::Delete => {
                match self.pane().current().cloned() {
                    Some(e) => {
                        let path = e.path.clone();
                        let name = e.name.clone();
                        self.file_delete = Some((self.file_focus_left, path, e.is_dir));
                        self.notice = format!("删除 `{name}`？按 y 确认，其它键取消");
                    }
                    None => self.notice = "没有选中项".into(),
                }
            }
            KeyCode::Esc => self.leave_files(),
            _ => {}
        }
        Ok(())
    }

    async fn on_action(&mut self, action: Action) -> anyhow::Result<()> {
        // 翻页/清屏这些是终端动作：文件视图下先退回去，别打在看不见的地方
        if self.view == View::Files
            && matches!(
                action,
                Action::ScrollUp
                    | Action::ScrollDown
                    | Action::ScrollTop
                    | Action::ScrollBottom
                    | Action::ClearScreen
            )
        {
            self.leave_files();
        }
        match action {
            Action::Quit => self.should_quit = true,
            Action::NewTab => {
                self.focus = Focus::Tree;
                self.query = Some(String::new());
            }
            Action::CloseTab => self.close_active().await?,
            Action::NextTab => {
                self.cycle_tab(1);
                self.tab_changed().await;
            }
            Action::PrevTab => {
                self.cycle_tab(-1);
                self.tab_changed().await;
            }
            Action::ToggleFocus => {
                let main = if self.view == View::Files {
                    Focus::Files
                } else {
                    Focus::Terminal
                };
                self.focus = match self.focus {
                    Focus::Tree => main,
                    _ => Focus::Tree,
                };
            }
            Action::ToggleFiles => {
                if self.view == View::Files {
                    self.leave_files();
                } else {
                    self.enter_files().await;
                }
            }
            Action::FileRefresh => self.file_refresh().await,
            Action::FileCopy => self.file_transfer().await,
            Action::HostSearch => self.query = Some(String::new()),
            Action::CommandPalette => self.open_palette(),
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
                    // 选择的是终端里的文字：文件视图下先退回去，否则选了个寂寞
                    let head = tab.screen.view_cursor().unwrap_or((0, 0));
                    if self.view == View::Files {
                        self.leave_files();
                    }
                    self.selection = Some(Selection { anchor: head, head });
                    self.focus = Focus::Terminal;
                    self.notice = "选择模式：方向键移动，Enter/y 复制，Esc 取消".into();
                }
            }
            Action::Copy => self.copy_selection().await?,
            Action::BroadcastToggle => self.toggle_broadcast().await,
            Action::BroadcastMark => self.mark_broadcast().await,
        }
        Ok(())
    }

    /// 广播开关（FR-5.1）。
    ///
    /// 名单为空表示"全部已打开会话"——最常见的用法就是连上几台、打开广播、
    /// 敲的命令同时生效；需要例外时用 `Alt+M` 把某台剔出去。
    pub async fn toggle_broadcast(&mut self) {
        self.broadcast = !self.broadcast;
        let sessions: Vec<String> = self.broadcast_targets.iter().cloned().collect();
        let mut params = json!({ "enabled": self.broadcast });
        if !sessions.is_empty() {
            params["sessions"] = json!(sessions);
        }
        match self.client.call(methods::BROADCAST_SET, params).await {
            Ok(_) => {
                self.notice = if !self.broadcast {
                    "广播已关闭".into()
                } else if sessions.is_empty() {
                    "广播已开启：所有会话（Alt+M 可逐台取消）".into()
                } else {
                    format!("广播已开启：{} 个会话", sessions.len())
                };
            }
            Err(e) => {
                // daemon 没认下这次切换，本地也别装作开了
                self.broadcast = !self.broadcast;
                self.notice = format!("广播切换失败：{e}");
            }
        }
    }

    /// 勾选/取消当前会话参与广播（FR-5.1）
    pub async fn mark_broadcast(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            self.notice = "没有活动会话".into();
            return;
        };
        let sid = tab.id.clone();
        let name = tab.name.clone();
        let joined = if self.broadcast_targets.contains(&sid) {
            self.broadcast_targets.remove(&sid);
            false
        } else {
            self.broadcast_targets.insert(sid);
            true
        };
        // 名单变了要同步给 daemon：broadcast.send 的落点在 daemon 侧。
        // 空数组必须照样发——它表示"回到全部会话"，不发就清不掉最后一个勾选。
        let sessions: Vec<String> = self.broadcast_targets.iter().cloned().collect();
        let params = json!({ "sessions": sessions });
        match self.client.call(methods::BROADCAST_SET, params).await {
            Ok(_) => {
                self.notice = if joined {
                    format!("{name} 已加入广播")
                } else {
                    format!("{name} 已退出广播")
                };
            }
            Err(e) => self.notice = format!("广播名单同步失败：{e}"),
        }
    }

    /// 该会话是否参与广播（名单为空 = 全部参与）
    fn in_broadcast(&self, sid: &str) -> bool {
        self.broadcast_targets.is_empty() || self.broadcast_targets.contains(sid)
    }

    /// 当前参与广播的会话数，供状态栏显示
    fn broadcast_count(&self) -> usize {
        if self.broadcast_targets.is_empty() {
            self.tabs.len()
        } else {
            self.broadcast_targets.len()
        }
    }

    pub fn broadcast(&self) -> bool {
        self.broadcast
    }

    // --- 命令面板（FR-3.6 / W5.1）-----------------------------------------

    /// 打开命令面板
    pub fn open_palette(&mut self) {
        self.palette = Some(Palette::new());
    }

    /// 面板是否打开（测试用）
    pub fn palette_open(&self) -> bool {
        self.palette.is_some()
    }

    /// 面板当前输入（测试用）
    pub fn palette_input(&self) -> String {
        self.palette.as_ref().map(|p| p.input.clone()).unwrap_or_default()
    }

    /// 展示用：按面板当前输入过滤后的候选项（复制一份，避免借用打架）
    pub fn palette_matches(&self) -> Vec<palette::PaletteItem> {
        let q = self.palette.as_ref().map(|p| p.input.clone()).unwrap_or_default();
        self.matches_for(&q)
    }

    /// 按指定查询串过滤。执行选中项时面板已关闭，必须拿关闭前的输入来算，
    /// 否则 query 变空、候选集变了，`sel` 就指向了另一条命令。
    fn matches_for(&self, query: &str) -> Vec<palette::PaletteItem> {
        let hosts: Vec<(String, String)> = self
            .tree
            .hosts_snapshot()
            .into_iter()
            .map(|h| (h.name.clone(), h.endpoint()))
            .collect();
        let all = palette::items(&hosts, &self.theme_name);
        palette::filter(&all, query).into_iter().cloned().collect()
    }

    /// 执行面板选中项。连接主机走的是和回车/双击同一条 `open` 路径
    pub async fn run_palette_cmd(&mut self, cmd: palette::PaletteCmd) -> anyhow::Result<()> {
        match cmd {
            palette::PaletteCmd::Action(a) => {
                self.on_action(a).await?;
            }
            palette::PaletteCmd::Connect(name) => {
                self.open(&name).await?;
            }
            palette::PaletteCmd::Theme(t) => {
                self.theme = Theme::preset(t);
                self.theme_name = t.to_string();
                // 主题写回 config.toml，下次启动保持一致
                match self.store.load_config() {
                    Ok(mut cfg) => {
                        cfg.theme = t.to_string();
                        if let Err(e) = self.store.save_config(&cfg) {
                            self.notice = format!("主题已切换，但写回配置失败：{e}");
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        self.notice = format!("主题已切换，但读配置失败：{e}");
                        return Ok(());
                    }
                }
                self.notice = format!("主题已切换为 {t}");
            }
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
            .call(
                methods::INPUT_SEND,
                json!({ "session": tab.id, "data": data }),
            )
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

    /// 当前标签页变了：文件视图的右栏要跟着换会话重新列目录
    async fn tab_changed(&mut self) {
        if self.view == View::Files {
            self.reload_remote().await;
        }
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
        // 文件视图的右栏可能正指着这个会话
        self.tab_changed().await;
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
        let ServerMsg::Event {
            event,
            session,
            data,
            ..
        } = msg
        else {
            return;
        };

        // 确认类事件不绑定会话（一键式 --host 调用时 session 为 null），
        // 必须先于下面"按 session 找标签页"的逻辑分流，否则永远弹不出来
        match event.as_str() {
            "confirm.requested" => {
                let d = data.unwrap_or(json!({}));
                let token = d.get("token").and_then(|v| v.as_str()).unwrap_or_default();
                if token.is_empty() {
                    return;
                }
                if self.confirm.is_some() {
                    self.notice = "有新的确认请求，上一个已被顶掉".into();
                }
                self.confirm = Some(PendingConfirm {
                    token: token.to_string(),
                    host: d
                        .get("host")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string(),
                    command: d
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    reason: d.get("reason").and_then(|v| v.as_str()).map(str::to_string),
                });
                return;
            }
            "confirm.resolved" => {
                let d = data.unwrap_or(json!({}));
                let token = d.get("token").and_then(|v| v.as_str()).unwrap_or_default();
                let approved = d.get("approved").and_then(|v| v.as_bool()).unwrap_or(false);
                let summary = d.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                if self.confirm.as_ref().is_some_and(|c| c.token == token) {
                    self.confirm = None;
                }
                self.notice = format!(
                    "{}：{summary}",
                    if approved {
                        "已批准执行"
                    } else {
                        "已拒绝"
                    }
                );
                return;
            }
            _ => {}
        }

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
                let reason = data
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("已结束");
                tab.screen.push_notice(&format!("会话结束：{reason}"));
            }
            _ => {}
        }
    }

    // ---- 文件视图（FR-4.2）----

    /// 左栏（本地）状态。测试与调试用
    pub fn local_files(&self) -> &FilePane {
        &self.files_left
    }

    /// 右栏（远程）状态。测试与调试用
    pub fn remote_files(&self) -> &FilePane {
        &self.files_right
    }

    /// 左栏可写句柄（测试与调试用）
    pub fn local_files_mut(&mut self) -> &mut FilePane {
        &mut self.files_left
    }

    /// 右栏可写句柄（测试与调试用）
    pub fn remote_files_mut(&mut self) -> &mut FilePane {
        &mut self.files_right
    }

    /// 是否正处于双栏文件视图
    pub fn in_file_view(&self) -> bool {
        self.view == View::Files
    }

    /// 最近一条提示/错误（测试与调试用）
    pub fn notice(&self) -> &str {
        &self.notice
    }

    /// 文件视图里把焦点切到另一栏（Tab 键走的就是它）
    pub fn toggle_file_pane(&mut self) {
        self.file_focus_left = !self.file_focus_left;
    }

    fn pane(&self) -> &FilePane {
        if self.file_focus_left {
            &self.files_left
        } else {
            &self.files_right
        }
    }

    fn pane_mut(&mut self) -> &mut FilePane {
        if self.file_focus_left {
            &mut self.files_left
        } else {
            &mut self.files_right
        }
    }

    pub async fn enter_files(&mut self) {
        self.view = View::Files;
        self.focus = Focus::Files;
        self.notice =
            "文件管理：Tab 切栏 · ↑↓ 选择 · Enter 进入 · Backspace 上级 · g 跳转 · s 排序".into();
        self.reload_remote().await;
    }

    pub fn leave_files(&mut self) {
        self.view = View::Terminal;
        self.focus = Focus::Terminal;
        self.notice.clear();
    }

    /// 右栏列目录：走 `file.list`，复用当前会话已建立的 SSH 连接
    pub async fn reload_remote(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            self.files_right
                .set_error("没有活动会话：先在左侧连一台主机");
            return;
        };
        let sid = tab.id.clone();
        let host = tab.name.clone();
        // 换会话了：远端侧回到该会话的起始目录，别把上一台的 cwd 带过去
        if self.file_sid.as_deref() != Some(sid.as_str()) {
            self.files_right.goto(".");
            self.file_sid = Some(sid.clone());
        }
        self.files_right.loading = true;
        let path = self.files_right.cwd.clone();
        match self
            .client
            .call(methods::FILE_LIST, json!({ "session": sid, "path": path }))
            .await
        {
            Ok(v) => {
                let entries: Vec<FileEntry> = v
                    .get("entries")
                    .cloned()
                    .and_then(|x| serde_json::from_value(x).ok())
                    .unwrap_or_default();
                let resolved = v.get("path").and_then(|x| x.as_str()).map(str::to_string);
                self.files_right.set_entries(entries, resolved);
            }
            Err(e) => self
                .files_right
                .set_error(format!("{host} 列目录失败：{e}")),
        }
    }

    /// 文件视图下的路径跳转：本地按相对 cwd 解析，远程按 SFTP 语义交给服务端
    pub async fn file_goto(&mut self, target: &str) {
        if target.trim().is_empty() {
            return;
        }
        if self.file_focus_left {
            let joined = std::path::Path::new(&self.files_left.cwd).join(target);
            self.files_left.goto(joined.to_string_lossy().to_string());
            self.files_left.reload_local();
            self.notice = format!("本地 → {}", self.files_left.cwd);
        } else {
            self.files_right.goto(target);
            self.reload_remote().await;
            self.notice = format!("远程 → {}", self.files_right.cwd);
        }
    }

    /// 进入光标下的目录；不是目录就给个提示（打开与传输在 W4.3）
    pub async fn file_enter(&mut self) {
        let Some(cur) = self.pane().current().cloned() else {
            return;
        };
        if !cur.is_dir {
            self.notice = format!("{} 不是目录（文件操作在 W4.3）", cur.name);
            return;
        }
        if self.file_focus_left {
            self.files_left.goto(cur.path);
            self.files_left.reload_local();
        } else {
            self.files_right.goto(cur.path);
            self.reload_remote().await;
        }
    }

    pub async fn file_up(&mut self) {
        if self.file_focus_left {
            if self.files_left.up() {
                self.files_left.reload_local();
            } else {
                self.notice = "本地已在根目录".into();
            }
        } else if self.files_right.up() {
            self.reload_remote().await;
        } else {
            self.notice = "远程已在根目录".into();
        }
    }

    pub fn file_move(&mut self, delta: isize) {
        let rows = self.term_size.1.saturating_sub(2) as usize;
        let p = self.pane_mut();
        p.move_sel(delta);
        p.ensure_visible(rows.max(5));
    }

    pub fn file_sort(&mut self) {
        let p = self.pane_mut();
        p.cycle_sort();
        self.notice = format!("排序：{}", p.sort.label());
    }

    /// 刷新当前栏
    pub async fn file_refresh(&mut self) {
        if self.file_focus_left {
            self.files_left.reload_local();
            self.notice = "本地已刷新".into();
        } else {
            self.reload_remote().await;
            self.notice = "远程已刷新".into();
        }
    }

    /// 在当前栏的 cwd 下建子目录
    pub async fn file_mkdir(&mut self, name: &str) {
        if self.file_focus_left {
            let path = std::path::Path::new(&self.files_left.cwd).join(name);
            match std::fs::create_dir(&path) {
                Ok(_) => {
                    self.notice = format!("已建目录 {}", path.display());
                    self.files_left.reload_local();
                }
                Err(e) => self.notice = format!("新建目录失败：{e}"),
            }
            return;
        }
        let path = join_posix(&self.files_right.cwd, name);
        let Some(sid) = self.active_session() else {
            self.notice = "没有活动会话".into();
            return;
        };
        match self
            .client
            .call(methods::FILE_MKDIR, json!({ "session": sid, "path": path }))
            .await
        {
            Ok(_) => {
                self.notice = format!("已建目录 {path}");
                self.reload_remote().await;
            }
            Err(e) => self.notice = format!("新建目录失败：{e}"),
        }
    }

    /// 重命名：只改名字，目录位置不变
    pub async fn file_rename(&mut self, from: &str, new_name: &str) {
        let to = if self.file_focus_left {
            match std::path::Path::new(from).parent() {
                Some(p) => p.join(new_name).to_string_lossy().to_string(),
                None => new_name.to_string(),
            }
        } else {
            match from.rfind('/') {
                Some(i) => format!("{}/{new_name}", &from[..i]),
                None => new_name.to_string(),
            }
        };
        if self.file_focus_left {
            match std::fs::rename(from, &to) {
                Ok(_) => {
                    self.notice = format!("已重命名为 {new_name}");
                    self.files_left.reload_local();
                }
                Err(e) => self.notice = format!("重命名失败：{e}"),
            }
            return;
        }
        let Some(sid) = self.active_session() else {
            self.notice = "没有活动会话".into();
            return;
        };
        match self
            .client
            .call(
                methods::FILE_RENAME,
                json!({ "session": sid, "from": from, "to": to }),
            )
            .await
        {
            Ok(_) => {
                self.notice = format!("已重命名为 {new_name}");
                self.reload_remote().await;
            }
            Err(e) => self.notice = format!("重命名失败：{e}"),
        }
    }

    /// 改权限：输入的是八进制（`644` / `0755`）
    pub async fn file_chmod(&mut self, path: &str, mode: &str) {
        let Some(m) = parse_octal(mode) else {
            self.notice = format!("`{mode}` 不是八进制权限（例如 644）");
            return;
        };
        if self.file_focus_left {
            let msg = match chmod_local(path, m) {
                Ok(_) => {
                    self.files_left.reload_local();
                    format!("权限已改为 {mode}")
                }
                Err(e) => e,
            };
            self.notice = msg;
            return;
        }
        let Some(sid) = self.active_session() else {
            self.notice = "没有活动会话".into();
            return;
        };
        match self
            .client
            .call(
                methods::FILE_CHMOD,
                json!({ "session": sid, "path": path, "mode": m }),
            )
            .await
        {
            Ok(_) => {
                self.notice = format!("权限已改为 {mode}");
                self.reload_remote().await;
            }
            Err(e) => self.notice = format!("改权限失败：{e}"),
        }
    }

    /// 删除（已确认过）：目录要求是空的，非空目录递归删除留到 W4.4
    pub async fn file_delete(&mut self, is_left: bool, path: &str, is_dir: bool) {
        if is_left {
            let r = if is_dir {
                std::fs::remove_dir(path)
            } else {
                std::fs::remove_file(path)
            };
            match r {
                Ok(_) => {
                    self.notice = "已删除".into();
                    self.files_left.reload_local();
                }
                Err(e) => self.notice = format!("删除失败：{e}"),
            }
            return;
        }
        let Some(sid) = self.active_session() else {
            self.notice = "没有活动会话".into();
            return;
        };
        match self
            .client
            .call(
                methods::FILE_REMOVE,
                json!({ "session": sid, "path": path, "dir": is_dir }),
            )
            .await
        {
            Ok(_) => {
                self.notice = "已删除".into();
                self.reload_remote().await;
            }
            Err(e) => self.notice = format!("删除失败：{e}"),
        }
    }

    /// 把焦点栏的**选中项或全部标记项**排进传输队列。
    ///
    /// 真正的数据搬运在 [`App::tick_transfers`] 里按块推进：UI 每帧只做一块，
    /// 于是不管多大的文件，界面都不会卡住，进度条也真的会动。
    pub async fn file_transfer(&mut self) {
        if self.view != View::Files {
            return;
        }
        let upload = self.file_focus_left;
        let sid = self.active_session().unwrap_or_default();
        if sid.is_empty() {
            self.notice = "没有活动会话".into();
            return;
        }
        let dst_dir = if upload {
            self.files_right.cwd.clone()
        } else {
            self.files_left.cwd.clone()
        };
        let marks = if upload { &self.marks_left } else { &self.marks_right };
        let picked: Vec<FileEntry> = if marks.is_empty() {
            self.pane().current().cloned().into_iter().collect()
        } else {
            self.pane()
                .entries
                .iter()
                .filter(|e| marks.contains(&e.path))
                .cloned()
                .collect()
        };
        if picked.is_empty() {
            self.notice = "没有选中项".into();
            return;
        }
        let mut queued = 0usize;
        let mut skipped_dirs = 0usize;
        for e in picked {
            if e.is_dir {
                skipped_dirs += 1;
                continue;
            }
            let dst = if upload {
                join_posix(&dst_dir, &e.name)
            } else {
                std::path::Path::new(&dst_dir)
                    .join(&e.name)
                    .to_string_lossy()
                    .to_string()
            };
            self.transfers.push(Transfer {
                name: e.name.clone(),
                src: e.path.clone(),
                dst,
                upload,
                size: e.size,
                done: 0,
                state: TransferState::Pending,
                retries: 0,
                speed: 0,
                error: None,
                started: None,
            });
            queued += 1;
        }
        // 传完就清空标记：不然下次 F5 会把这批再传一遍
        if upload {
            self.marks_left.clear();
        } else {
            self.marks_right.clear();
        }
        self.notice = match (queued, skipped_dirs) {
            (0, _) => "没有可传的文件".into(),
            (n, 0) => format!("已排队 {n} 个文件"),
            (n, d) => format!("已排队 {n} 个文件（跳过 {d} 个目录，递归传输待 W4.5）"),
        };
    }

    /// 队列里还有活儿吗（测试驱动用）
    pub fn has_pending_transfer(&self) -> bool {
        self.transfers
            .iter()
            .any(|t| matches!(t.state, TransferState::Pending | TransferState::Active))
    }

    /// 推进传输队列**一块**。事件循环每帧调一次：
    /// 每帧只做 [`CHUNK_SIZE`] 字节，UI 因此始终有响应。
    pub async fn tick_transfers(&mut self) {
        // 串行执行：同一时刻只推一个任务，避免并发刷爆连接与审计
        let Some(idx) = self
            .transfers
            .iter()
            .position(|t| matches!(t.state, TransferState::Active | TransferState::Pending))
        else {
            return;
        };
        let t = &mut self.transfers[idx];
        if t.state == TransferState::Pending {
            t.state = TransferState::Active;
            t.started = Some(std::time::Instant::now());
        }
        let offset = t.done;
        let (name, src, dst, upload) = (t.name.clone(), t.src.clone(), t.dst.clone(), t.upload);

        let result = if upload {
            self.upload_chunk(&src, &dst, offset).await
        } else {
            self.download_chunk(&src, &dst, offset).await
        };

        let t = &mut self.transfers[idx];
        match result {
            Ok(n) => {
                t.done += n as u64;
                if let Some(started) = t.started {
                    let secs = started.elapsed().as_secs_f64().max(0.001);
                    t.speed = (t.done as f64 / secs) as u64;
                }
                if n < CHUNK_SIZE || (t.size > 0 && t.done >= t.size) {
                    t.state = TransferState::Done;
                    t.error = None;
                    // 目标栏要看到新文件
                    if upload {
                        self.reload_remote().await;
                    } else {
                        self.files_left.reload_local();
                    }
                }
            }
            Err(e) => {
                t.retries += 1;
                t.error = Some(e);
                if t.retries >= MAX_RETRIES {
                    t.state = TransferState::Failed;
                } else {
                    // 留在队列里，下一帧（或下一次 tick）重试
                    t.state = TransferState::Pending;
                    t.done = offset; // 从当前偏移继续，不回退
                }
            }
        }
        let _ = name;
    }

    /// 上传一块：本地读 → `file.write_at`
    async fn upload_chunk(
        &self,
        src: &str,
        dst: &str,
        offset: u64,
    ) -> std::result::Result<usize, String> {
        use std::io::{Read, Seek};
        let mut f = std::fs::File::open(src).map_err(|e| format!("打开本地文件失败：{e}"))?;
        f.seek(std::io::SeekFrom::Start(offset))
            .map_err(|e| format!("定位失败：{e}"))?;
        let mut buf = vec![0u8; CHUNK_SIZE];
        let n = f.read(&mut buf).map_err(|e| format!("读本地文件失败：{e}"))?;
        if n == 0 {
            return Ok(0);
        }
        buf.truncate(n);
        let sid = self.active_session().unwrap_or_default();
        let (encoding, data) = match std::str::from_utf8(&buf) {
            Ok(s) => ("utf8", s.to_string()),
            Err(_) => ("base64", base64_encode(&buf)),
        };
        self.client
            .call(
                methods::FILE_WRITE_AT,
                json!({
                    "session": sid, "path": dst, "offset": offset,
                    "data": data, "encoding": encoding,
                }),
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(n)
    }

    /// 下载一块：`file.read_range` → 本地追加写
    async fn download_chunk(
        &self,
        src: &str,
        dst: &str,
        offset: u64,
    ) -> std::result::Result<usize, String> {
        let sid = self.active_session().unwrap_or_default();
        let v = self
            .client
            .call(
                methods::FILE_READ_RANGE,
                json!({ "session": sid, "path": src, "offset": offset, "len": CHUNK_SIZE }),
            )
            .await
            .map_err(|e| e.to_string())?;
        let enc = v.get("encoding").and_then(|x| x.as_str()).unwrap_or("utf8");
        let content = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
        let bytes = if enc == "base64" {
            base64_decode(content).ok_or_else(|| "下载的内容无法解码".to_string())?
        } else {
            content.as_bytes().to_vec()
        };
        if bytes.is_empty() {
            return Ok(0);
        }
        // 首块覆盖写，之后追加
        if offset == 0 {
            std::fs::write(dst, &bytes).map_err(|e| format!("写本地文件失败：{e}"))?;
        } else {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(dst)
                .map_err(|e| format!("打开本地文件失败：{e}"))?;
            f.write_all(&bytes).map_err(|e| format!("写本地文件失败：{e}"))?;
        }
        Ok(bytes.len())
    }

    /// 把失败的项重新排队（键 `R`）
    pub fn retry_failed(&mut self) {
        let mut n = 0;
        for t in &mut self.transfers {
            if t.state == TransferState::Failed {
                t.state = TransferState::Pending;
                t.retries = 0;
                t.error = None;
                n += 1;
            }
        }
        self.notice = if n == 0 { "没有失败的传输".into() } else { format!("已重新排队 {n} 项") };
    }

    /// 清掉已完成的项（键 `c`）
    pub fn clear_finished(&mut self) {
        let before = self.transfers.len();
        self.transfers.retain(|t| !t.is_finished());
        self.notice = format!("已清理 {} 项", before - self.transfers.len());
    }

    /// 标记/取消标记当前项（键 `Space`）
    pub fn toggle_mark(&mut self) {
        let Some(e) = self.pane().current().cloned() else {
            return;
        };
        let marks = if self.file_focus_left {
            &mut self.marks_left
        } else {
            &mut self.marks_right
        };
        if !marks.insert(e.path.clone()) {
            marks.remove(&e.path);
        }
        let n = marks.len();
        self.notice = format!("已标记 {n} 项（F5 传走）");
    }

    pub fn marks(&self, left: bool) -> &std::collections::HashSet<String> {
        if left {
            &self.marks_left
        } else {
            &self.marks_right
        }
    }

    pub fn transfers(&self) -> &[Transfer] {
        &self.transfers
    }

    fn active_session(&self) -> Option<String> {
        self.tabs.get(self.active).map(|t| t.id.clone())
    }

    // ---- 渲染 ----

    pub fn draw(&self, f: &mut ratatui::Frame) {
        if let Some(form) = &self.form {
            self.draw_form(f, form);
            // 确认弹窗优先级更高，可以压在表单之上
            if let Some(pc) = &self.confirm {
                self.draw_confirm(f, pc);
            }
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(f.area());

        self.draw_tabs(f, chunks[0]);

        let body = if self.show_tree {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(self.tree_width), Constraint::Min(1)])
                .split(chunks[1]);
            self.draw_tree(f, cols[0]);
            cols[1]
        } else {
            chunks[1]
        };
        // 主区矩形留给鼠标：滚轮滚终端、拖选文本都要拿它做命中测试
        self.main_area.set(body);

        if self.view == View::Files {
            self.draw_files(f, body);
        } else {
            self.draw_terminal(f, body);
        }
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
                Paragraph::new(text)
                    .block(block)
                    .alignment(Alignment::Center),
                area,
            );
        }

        if let Some(pc) = &self.confirm {
            self.draw_confirm(f, pc);
        }

        // 命令面板压在最上层
        if let Some(p) = &self.palette {
            self.draw_palette(f, p);
        }
    }

    /// 风险确认弹窗（FR-7.4）：AI 发起的 confirm 档命令在这里等一个"人"拍板
    fn draw_confirm(&self, f: &mut ratatui::Frame, pc: &PendingConfirm) {
        let area = centered_rect(70, 9, f.area());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.warn))
            .title(" 命令需确认 ");
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut lines: Vec<Line> = vec![
            Line::from(vec![
                Span::styled("主机 ", Style::default().fg(self.theme.border)),
                Span::styled(
                    pc.host.clone(),
                    Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("命令 ", Style::default().fg(self.theme.border)),
                Span::styled(
                    pc.command.clone(),
                    Style::default()
                        .fg(self.theme.status_fg)
                        .bg(self.theme.warn)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
        ];
        if let Some(reason) = &pc.reason {
            lines.push(Line::from(vec![
                Span::styled("原因 ", Style::default().fg(self.theme.border)),
                Span::raw(truncate(reason, inner.width as usize)),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "按 y 批准执行，其它键拒绝（令牌 5 分钟内有效）",
            Style::default().fg(self.theme.warn),
        )));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    /// 命令面板：顶部输入框 + 下方候选列表，居中浮层（FR-3.6 / W5.1）
    fn draw_palette(&self, f: &mut ratatui::Frame, p: &Palette) {
        let area = centered_rect(60, 16, f.area());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.accent))
            .title(" 命令面板 ");
        f.render_widget(Clear, area);
        let inner = block.inner(area);
        f.render_widget(block, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(inner);

        f.render_widget(
            Paragraph::new(format!("> {}", p.input)).style(
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            rows[0],
        );

        let matches = self.palette_matches();
        let visible = rows[1].height as usize;
        // 选中行永远可见：滚到它越过可视区下沿时把窗口往下推
        let start = if p.sel >= visible.saturating_add(0) && visible > 0 {
            p.sel + 1 - visible
        } else {
            0
        };
        let mut lines: Vec<Line> = Vec::new();
        if matches.is_empty() {
            lines.push(Line::from(Span::styled(
                "（无匹配）",
                Style::default().fg(self.theme.border_dim),
            )));
        }
        for (i, item) in matches.iter().enumerate().skip(start).take(visible) {
            let selected = i == p.sel;
            let style = if selected {
                Style::default()
                    .fg(self.theme.status_fg)
                    .bg(self.theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(self.theme.fg)
            };
            let w = rows[1].width as usize;
            let title_w = item.title.chars().count();
            let hint_w = w.saturating_sub(title_w + 3);
            let hint = if hint_w > 2 {
                let h: String = item.hint.chars().take(hint_w - 1).collect();
                format!(" {h}")
            } else {
                String::new()
            };
            let mut spans = vec![Span::styled(
                format!(" {}{}", item.title, " ".repeat(w.saturating_sub(title_w + 1 + hint.chars().count()))),
                style,
            )];
            spans.push(Span::styled(
                hint,
                if selected { style } else { Style::default().fg(self.theme.border_dim) },
            ));
            lines.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(lines), rows[1]);
    }

    /// 双栏文件视图：左本地、右远程（FR-4.2）+ 底部传输队列（FR-4.4）
    fn draw_files(&self, f: &mut ratatui::Frame, area: Rect) {
        // 队列有任务时才占地方，最多 5 行；平时把空间全留给文件列表
        let queue_rows = if self.transfers.is_empty() {
            0
        } else {
            (self.transfers.len() as u16 + 1).min(6)
        };
        let (files_area, queue_area) = if queue_rows == 0 {
            (area, None)
        } else {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(queue_rows)])
                .split(area);
            (rows[0], Some(rows[1]))
        };

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(files_area);
        let host = self.tabs.get(self.active).map(|t| t.name.as_str());
        self.draw_pane(f, cols[0], &self.files_left, self.file_focus_left, None);
        self.draw_pane(f, cols[1], &self.files_right, !self.file_focus_left, host);

        if let Some(qa) = queue_area {
            self.draw_queue(f, qa);
        }

        // 路径跳转输入框：浮在底部状态栏之上，样式与主机搜索一致
        if let Some((kind, j)) = &self.file_prompt {
            let r = Rect {
                x: area.x,
                y: area.bottom().saturating_sub(1),
                width: area.width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(format!("{}: {j}", kind.label())).style(
                    Style::default()
                        .fg(self.theme.status_bg)
                        .add_modifier(Modifier::BOLD),
                ),
                r,
            );
        }
    }

    fn draw_pane(
        &self,
        f: &mut ratatui::Frame,
        area: Rect,
        pane: &FilePane,
        focused: bool,
        host: Option<&str>,
    ) {
        let border = if focused {
            self.theme.accent
        } else {
            self.theme.border_dim
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(pane.title(host));
        let inner = block.inner(area);
        // 记下这一栏的内区，鼠标点条目要用（draw 是 &self，故用 Cell）
        match pane.side {
            crate::files::Side::Local => self.pane_left_area.set(inner),
            crate::files::Side::Remote => self.pane_right_area.set(inner),
        }
        f.render_widget(block, area);

        let width = inner.width as usize;
        // 底部留一行给状态（条目数/排序/错误），条目区矮一行
        let rows = inner.height.saturating_sub(1) as usize;
        let mut lines: Vec<Line> = Vec::with_capacity(inner.height as usize);

        if pane.loading {
            lines.push(Line::from(Span::styled(
                "列目录…",
                Style::default().fg(self.theme.warn),
            )));
        } else if pane.entries.is_empty() {
            lines.push(Line::from(Span::styled(
                "（空目录）",
                Style::default().fg(self.theme.border),
            )));
        } else {
            for (i, e) in pane.entries.iter().skip(pane.scroll).take(rows).enumerate() {
                let idx = pane.scroll + i;
                let marked = self
                    .marks(self.file_focus_left == (pane.side == crate::files::Side::Local))
                    .contains(&e.path);
                let text = if marked {
                    format!("*{}", entry_line(e, width.saturating_sub(1)))
                } else {
                    entry_line(e, width)
                };
                let style = match (idx == pane.selected, focused) {
                    (true, true) => Style::default()
                        .fg(self.theme.status_fg)
                        .bg(self.theme.accent)
                        .add_modifier(Modifier::BOLD),
                    (true, false) => Style::default()
                        .fg(self.theme.fg)
                        .bg(self.theme.border_dim)
                        .add_modifier(Modifier::BOLD),
                    (false, _) => {
                        if e.is_dir {
                            Style::default().fg(self.theme.accent)
                        } else if e.is_symlink {
                            Style::default().fg(self.theme.ok)
                        } else {
                            Style::default().fg(self.theme.fg)
                        }
                    }
                };
                lines.push(Line::from(Span::styled(format!("{text:<width$}"), style)));
            }
        }
        while lines.len() < rows {
            lines.push(Line::raw(""));
        }

        // 底部状态：错误优先，否则给条目数与排序键
        let tail = match &pane.status {
            Some(s) => Span::styled(
                crate::files::truncate_chars(s, width),
                Style::default().fg(self.theme.err),
            ),
            None => Span::styled(
                format!("{} 项 · 排序 {}", pane.entries.len(), pane.sort.label()),
                Style::default().fg(self.theme.border),
            ),
        };
        lines.push(Line::from(tail));
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// 传输队列面板（FR-4.4）：进度条 + 速度 + 失败原因
    fn draw_queue(&self, f: &mut ratatui::Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.border))
            .title(format!(" 传输队列 {} ", self.transfers.len()));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let width = inner.width as usize;
        let mut lines: Vec<Line> = Vec::new();
        for t in self.transfers.iter().take(inner.height as usize) {
            let (mark, color) = match t.state {
                TransferState::Pending => ("…", self.theme.border),
                TransferState::Active => ("↑", self.theme.accent),
                TransferState::Done => ("✓", self.theme.ok),
                TransferState::Failed => ("✗", self.theme.err),
            };
            let arrow = if t.upload { "→" } else { "←" };
            let pct = (t.progress() * 100.0) as u32;
            let bar = progress_bar(t.progress(), 12);
            let size = sh0_core::sftp::human_size(t.size);
            let speed = if t.state == TransferState::Active || t.state == TransferState::Done {
                format!("{}/s", sh0_core::sftp::human_size(t.speed))
            } else {
                String::new()
            };
            let mut spans = vec![
                Span::styled(format!("{mark} "), Style::default().fg(color)),
                Span::styled(
                    crate::files::truncate_chars(&t.name, 20),
                    Style::default().fg(self.theme.fg),
                ),
                Span::raw(format!(" {arrow} {bar} {pct:3}% {size} {speed}")),
            ];
            if let Some(e) = &t.error {
                spans.push(Span::styled(
                    format!("  {}", crate::files::truncate_chars(e, 24)),
                    Style::default().fg(self.theme.err),
                ));
            }
            if t.retries > 0 && t.state != TransferState::Failed {
                spans.push(Span::styled(
                    format!(" (重试 {})", t.retries),
                    Style::default().fg(self.theme.warn),
                ));
            }
            lines.push(Line::from(spans));
        }
        while lines.len() < inner.height as usize {
            lines.push(Line::raw(""));
        }
        let _ = width;
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// 标签栏布局：把每个标签算成"文本 + 命中区"。
    ///
    /// 抽成纯函数是为了让命中测试与渲染共用**同一份**计算——
    /// 两边各算一遍的话，稍微改一下间距就会出现"看着点在标签上却没反应"。
    pub fn tab_segments(&self, area: Rect) -> Vec<TabSegment> {
        let mut out: Vec<TabSegment> = Vec::new();
        let mut x = area.x;
        for (i, tab) in self.tabs.iter().enumerate() {
            // 广播开着时给参与的标签加个 *：一眼能看出命令会打到哪几台
            let mark = if self.broadcast && self.in_broadcast(&tab.id) {
                "*"
            } else {
                " "
            };
            // 关闭按钮只在开了鼠标时才画：没鼠标就没法点，白占两列
            let (close_txt, close_w) = if self.mouse { ("× ", 2u16) } else { ("", 0u16) };
            let label = format!(" {}{}{} {}", mark, state_dot(tab.state), tab.name, close_txt);
            let w = label.chars().count() as u16;
            if x.saturating_add(w) > area.x.saturating_add(area.width) {
                break;
            }
            out.push(TabSegment {
                hit: TabHit {
                    idx: i,
                    body: (x, x + w.saturating_sub(close_w)),
                    close: (x + w.saturating_sub(close_w), x + w),
                },
                label,
                active: i == self.active,
            });
            x += w;
        }
        out
    }

    /// 命中区（不含文本），鼠标事件用它
    pub fn tab_hits(&self, area: Rect) -> Vec<TabHit> {
        self.tab_segments(area)
            .into_iter()
            .map(|s| s.hit)
            .collect()
    }

    fn draw_tabs(&self, f: &mut ratatui::Frame, area: Rect) {
        self.tab_bar_area.set(area);
        let segs = self.tab_segments(area);
        let mut spans: Vec<Span> = Vec::with_capacity(segs.len() + 1);
        for seg in &segs {
            let style = if seg.active {
                Style::default()
                    .fg(self.theme.status_fg)
                    .bg(self.theme.status_bg)
            } else {
                Style::default().fg(self.theme.fg)
            };
            spans.push(Span::styled(seg.label.clone(), style));
        }
        // 画不下的那部分用 "+N" 提示还有几个
        if segs.len() < self.tabs.len() {
            spans.push(Span::raw(format!(" +{}", self.tabs.len() - segs.len())));
        }
        if self.tabs.is_empty() {
            spans.push(Span::styled(
                " 无会话：在左侧选择主机回车连接，Ctrl+T 新建 ",
                Style::default()
                    .fg(self.theme.border)
                    .add_modifier(Modifier::DIM),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_tree(&self, f: &mut ratatui::Frame, area: Rect) {
        let focused = self.focus == Focus::Tree;
        let border = if focused {
            self.theme.accent
        } else {
            self.theme.border_dim
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(" 主机 ");
        let inner = block.inner(area);
        // 记下主机树的内区，鼠标事件要拿它做命中测试
        self.tree_area.set(inner);
        f.render_widget(block, area);

        let rows = self.tree.rows();
        let mut lines: Vec<Line> = Vec::new();
        for (i, row) in rows.iter().enumerate().take(inner.height as usize) {
            let selected = i == self.tree.selected;
            let text = match row {
                Row::Group {
                    path,
                    count,
                    expanded,
                } => {
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
            lines.push(Line::from(Span::styled(
                truncate(&text, inner.width as usize),
                style,
            )));
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
                    Style::default()
                        .fg(self.theme.border)
                        .add_modifier(Modifier::DIM),
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
        ) && cy < rows
        {
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
                    format!(
                        " 回看 {}/{} ",
                        tab.screen.scroll_offset(),
                        tab.screen.scrollback_len()
                    ),
                    Style::default().fg(self.theme.warn),
                ));
            }
        }
        // 广播态：开着就常驻显示，避免"忘了自己开着广播"把命令打进所有机器
        if self.broadcast {
            spans.push(Span::styled(
                format!(" 广播×{} ", self.broadcast_count()),
                Style::default()
                    .fg(self.theme.status_fg)
                    .bg(self.theme.warn),
            ));
        }
        if !self.notice.is_empty() {
            spans.push(Span::styled(
                format!(" {} ", self.notice),
                Style::default().fg(self.theme.warn),
            ));
        }
        let right = if self.view == View::Files {
            "Tab 切栏 · ↑↓ 选择 · Space 标记 · F5 传走 · F6 改名 · F7 建目录 · F8 删除 · p 权限 · F9 回终端"
        } else {
            "Ctrl+Q 退出 · Ctrl+T 新建 · Ctrl+W 关闭 · Ctrl+B 焦点"
        };
        let left_len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let pad = (area.width as usize).saturating_sub(left_len + right.chars().count());
        let mut all = spans;
        if pad > 1 {
            all.push(Span::raw(" ".repeat(pad)));
        }
        all.push(Span::styled(
            right,
            Style::default()
                .fg(self.theme.border)
                .add_modifier(Modifier::DIM),
        ));
        f.render_widget(Paragraph::new(Line::from(all)), area);
    }

    fn draw_form(&self, f: &mut ratatui::Frame, form: &HostForm) {
        let area = centered_rect(60, form.fields.len() as u16 + 5, f.area());
        let title = if form.editing.is_some() {
            " 编辑主机 "
        } else {
            " 新建主机 "
        };
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
                Style::default()
                    .fg(self.theme.status_fg)
                    .bg(self.theme.accent)
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
    let term_cols = cols.saturating_sub(DEFAULT_TREE_WIDTH).max(MIN_TERM_COLS);
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
    // 鼠标捕获（FR-3.9）。关掉配置 `mouse` 就完全不碰鼠标，父终端照旧能拖选复制
    if app.mouse && execute!(stdout, EnableMouseCapture).is_ok() {
        app.mouse_captured = true;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = loop_inner(&mut terminal, &mut app).await;

    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    // 捕获态必须还原，否则退出后父终端的鼠标选择还是坏的
    let _ = execute!(stdout, DisableMouseCapture);
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
                Event::Key(key) => dispatch_key(app, key).await?,
                Event::Mouse(m) => app.on_mouse(m).await?,
                Event::Resize(_, _) => {
                    let size = terminal.size()?;
                    let cols = size.width.saturating_sub(app.tree_width).max(MIN_TERM_COLS);
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

        // 3) 推进传输队列（每帧一块，界面不卡）
        app.tick_transfers().await;

        // 4) 渲染
        terminal.draw(|f| app.draw(f))?;

        tokio::time::sleep(Duration::from_millis(FRAME_MS)).await;
    }
}

/// 这个按键事件要不要交给 `on_key` 处理。
///
/// Windows 控制台为**每一次**按键产生两个事件：按下（`Press`）与松开（`Release`）
/// （启用 kitty 键盘协议的终端同理）。不过滤的话，每个字符都会被发往远端两遍——
/// 敲 `ls` 会变成 `llss`。长按产生的 `Repeat` 是真实的重复输入，要保留。
pub fn should_handle_key(key: KeyEvent) -> bool {
    !matches!(key.kind, KeyEventKind::Release)
}

/// 事件循环里处理按键的**唯一入口**：先按 `should_handle_key` 过滤，再交给 `App::on_key`。
///
/// 单独抽出来是因为"过滤"如果只写在事件循环里，测试就只能复制一遍同样的 if，
/// 那样测不到生产代码走的是不是同一条路。
pub async fn dispatch_key(app: &mut App, key: KeyEvent) -> anyhow::Result<()> {
    if should_handle_key(key) {
        app.on_key(key).await
    } else {
        Ok(())
    }
}

/// Ctrl+Q / Ctrl+C 都退出（raw mode 下 Ctrl+C 不会自动终止进程）
pub fn is_hard_quit(key: KeyEvent) -> bool {
    matches!(
        (key.code, key.modifiers),
        (KeyCode::Char('q'), KeyModifiers::CONTROL) | (KeyCode::Char('c'), KeyModifiers::CONTROL)
    )
}

/// 文本进度条：`████░░░░░░`
fn progress_bar(p: f64, width: usize) -> String {
    let filled = ((p.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// POSIX 风格拼接（远端路径一律用 `/`）
fn join_posix(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "/" {
        format!("/{name}")
    } else {
        format!("{}/{name}", dir.trim_end_matches('/'))
    }
}

/// 权限输入解析：只认八进制，且不超过 7777
fn parse_octal(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() || !s.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return None;
    }
    u32::from_str_radix(s, 8).ok().filter(|m| *m <= 0o7777)
}

/// 本地改权限：Unix 用 mode 的低 9 位；Windows 只认"只读"这一位
#[cfg(unix)]
fn chmod_local(path: &str, mode: u32) -> std::result::Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let md = std::fs::metadata(path).map_err(|e| format!("读取失败：{e}"))?;
    let mut perm = md.permissions();
    perm.set_mode(mode & 0o777);
    std::fs::set_permissions(path, perm).map_err(|e| format!("改权限失败：{e}"))
}

#[cfg(not(unix))]
fn chmod_local(path: &str, mode: u32) -> std::result::Result<(), String> {
    let md = std::fs::metadata(path).map_err(|e| format!("读取失败：{e}"))?;
    let mut perm = md.permissions();
    perm.set_readonly(mode & 0o222 == 0);
    std::fs::set_permissions(path, perm).map_err(|e| format!("改权限失败：{e}"))
}

/// base64 解码（下载二进制文件时用）
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
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
