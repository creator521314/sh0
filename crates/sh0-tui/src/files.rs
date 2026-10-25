//! 双栏文件面板的模型层（M4 / FR-4.2）。
//!
//! 左本地（`std::fs`）、右远程（经 IPC `file.list`，复用会话的 SSH 连接）。
//! 两侧共用 [`FileEntry`]，于是渲染、排序、导航只写一遍——远程侧多出来的
//! 只是"列目录要等一次 IPC"。
//!
//! 这一层**不碰 ratatui**：状态放在这里，绘制在 `app.rs`。这样导航与排序
//! 可以脱离终端做单测。

use std::path::Path;
use std::time::UNIX_EPOCH;

use sh0_core::sftp::{FileEntry, human_size, mode_string};

/// 排序键：目录恒排在最前，再按所选键排（与 `ls` 的直觉一致）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Time,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "名称",
            SortKey::Size => "大小",
            SortKey::Time => "时间",
        }
    }

    pub fn next(self) -> Self {
        match self {
            SortKey::Name => SortKey::Size,
            SortKey::Size => SortKey::Time,
            SortKey::Time => SortKey::Name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

/// 一侧面板的全部状态
#[derive(Debug)]
pub struct FilePane {
    pub side: Side,
    /// 当前目录。远程侧首次是 `.`，列目录成功后被服务端规范化路径覆盖
    pub cwd: String,
    pub entries: Vec<FileEntry>,
    pub selected: usize,
    /// 视口顶部对应的条目下标
    pub scroll: usize,
    pub sort: SortKey,
    /// 最近一次操作的结果提示（错误也走这里，红字显示）
    pub status: Option<String>,
    /// 是否处于"正在列目录"的状态（远程侧有往返延迟，得让人知道在忙）
    pub loading: bool,
}

impl FilePane {
    pub fn local() -> Self {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());
        let mut p = Self::new(Side::Local, cwd);
        p.reload_local();
        p
    }

    pub fn remote() -> Self {
        Self::new(Side::Remote, ".".to_string())
    }

    fn new(side: Side, cwd: String) -> Self {
        Self {
            side,
            cwd,
            entries: Vec::new(),
            selected: 0,
            scroll: 0,
            sort: SortKey::Name,
            status: None,
            loading: false,
        }
    }

    /// 本地列目录：同步做就好，`read_dir` 的耗时在 UI 帧预算内
    pub fn reload_local(&mut self) {
        self.loading = false;
        match std::fs::read_dir(&self.cwd) {
            Ok(rd) => {
                let mut entries: Vec<FileEntry> = rd
                    .filter_map(|e| e.ok())
                    .filter_map(|e| entry_from_path(&e.path()))
                    .collect();
                sort_entries(&mut entries, self.sort);
                self.entries = entries;
                self.clamp_sel();
                self.status = None;
            }
            Err(e) => {
                self.entries.clear();
                self.status = Some(format!("读目录失败：{e}"));
            }
        }
    }

    /// 远程列目录的结果回填。`resolved` 是服务端规范化后的路径（审计也是它），
    /// 拿到就覆盖 cwd——否则 `./foo` 会一层层叠下去，UI 与审计对不上。
    pub fn set_entries(&mut self, mut entries: Vec<FileEntry>, resolved: Option<String>) {
        self.loading = false;
        if let Some(p) = resolved.filter(|p| !p.is_empty()) {
            self.cwd = p;
        }
        sort_entries(&mut entries, self.sort);
        self.entries = entries;
        self.clamp_sel();
        self.status = None;
    }

    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.loading = false;
        self.status = Some(msg.into());
    }

    pub fn current(&self) -> Option<&FileEntry> {
        self.entries.get(self.selected)
    }

    /// 把 cwd 直接改掉（真正的列目录由调用方发起）
    pub fn goto(&mut self, path: impl Into<String>) {
        let path = path.into();
        self.cwd = if path.is_empty() {
            self.cwd.clone()
        } else {
            path
        };
        self.selected = 0;
        self.scroll = 0;
    }

    /// 选中条目的完整路径。远程侧 `entry.path` 由服务端拼好，直接用
    pub fn current_path(&self) -> Option<String> {
        self.current().map(|e| e.path.clone())
    }

    /// 上级目录；已在根或无法确定时返回 None
    pub fn parent(&self) -> Option<String> {
        if self.side == Side::Remote {
            let p = self.cwd.trim_end_matches('/');
            if p.is_empty() {
                return None;
            }
            return match p.rfind('/') {
                Some(0) => Some("/".to_string()),
                Some(i) => Some(p[..i].to_string()),
                None => None, // 相对路径（如 "."）没有可信的上级
            };
        }
        let path = Path::new(&self.cwd);
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_string_lossy().to_string())
    }

    /// 把 cwd 换成上级目录。返回是否真的动了
    pub fn up(&mut self) -> bool {
        match self.parent() {
            Some(p) => {
                self.goto(p);
                true
            }
            None => false,
        }
    }

    /// 进入目录：只改 cwd，列目录由调用方发起（本地/远程路径不同）
    pub fn enter(&mut self, path: &str) {
        self.goto(path);
    }

    pub fn move_sel(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected = 0;
            return;
        }
        let n = self.entries.len() as isize;
        let cur = self.selected as isize;
        self.selected = ((cur + delta).rem_euclid(n)) as usize;
        self.clamp_sel();
    }

    pub fn clamp_sel(&mut self) {
        if self.entries.is_empty() {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        if self.selected >= self.entries.len() {
            self.selected = self.entries.len() - 1;
        }
    }

    /// 让选中行落在视口内（视口高度由调用方给出）
    pub fn ensure_visible(&mut self, rows: usize) {
        let rows = rows.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
        let max_scroll = self.entries.len().saturating_sub(rows);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        let mut entries = std::mem::take(&mut self.entries);
        sort_entries(&mut entries, self.sort);
        self.entries = entries;
    }

    /// 面板标题：本地显示目录，远程显示 `主机:目录`
    pub fn title(&self, host: Option<&str>) -> String {
        match self.side {
            Side::Local => format!(" 本地 {} ", shorten(&self.cwd)),
            Side::Remote => match host {
                Some(h) => format!(" {h}:{} ", shorten(&self.cwd)),
                None => format!(" 远程（未连接） {} ", shorten(&self.cwd)),
            },
        }
    }
}

/// 本地路径 → FileEntry。符号链接按它指向的目标取元信息（和 `ls` 一致）
fn entry_from_path(path: &Path) -> Option<FileEntry> {
    let name = path.file_name()?.to_string_lossy().to_string();
    let md = std::fs::metadata(path).ok()?;
    let is_symlink = std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    Some(FileEntry {
        name,
        path: path.to_string_lossy().to_string(),
        is_dir: md.is_dir(),
        is_symlink,
        size: md.len(),
        modified: mtime_secs(&md),
        mode: Some(perm_bits(&md)),
        uid: None,
        gid: None,
    })
}

fn mtime_secs(md: &std::fs::Metadata) -> Option<i64> {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// 权限位：Unix 直接取 mode；Windows 只有"只读"这一位，退化成 444/666
fn perm_bits(md: &std::fs::Metadata) -> u32 {
    let type_bits: u32 = if md.is_dir() { 0o40_000 } else { 0o100_000 };
    #[cfg(unix)]
    let perm = {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o777
    };
    #[cfg(not(unix))]
    let perm = if md.permissions().readonly() {
        0o444
    } else {
        0o666
    };
    type_bits | perm
}

/// 目录优先，再按所选键排；同值时用名称兜底，避免顺序随文件系统抖动
pub fn sort_entries(entries: &mut [FileEntry], key: SortKey) {
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| match key {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Size => a.size.cmp(&b.size).then_with(|| a.name.cmp(&b.name)),
            SortKey::Time => b
                .modified
                .unwrap_or(0)
                .cmp(&a.modified.unwrap_or(0))
                .then_with(|| a.name.cmp(&b.name)),
        })
    });
}

/// 一行条目渲染成定宽文本：`名称  大小  时间  权限`
///
/// 宽度不够时优先砍掉权限列，再砍掉时间列——名字永远完整，否则没法导航。
pub fn entry_line(e: &FileEntry, width: usize) -> String {
    let name = if e.is_dir {
        format!("{}/", e.name)
    } else if e.is_symlink {
        format!("{}@", e.name)
    } else {
        e.name.clone()
    };
    let size = if e.is_dir {
        "<DIR>".to_string()
    } else {
        human_size(e.size)
    };
    // 秒对导航没用，砍掉换给文件名
    let time: String = sh0_core::sftp::format_mtime(e.modified)
        .chars()
        .take(16)
        .collect();
    let perm = mode_string(e.is_dir, e.mode);

    let full = format!("{name}  {size}  {time}  {perm}");
    if full.chars().count() <= width {
        return full;
    }
    let mid = format!("{name}  {size}  {time}");
    if mid.chars().count() <= width {
        return mid;
    }
    let short = format!("{name}  {size}");
    if short.chars().count() <= width {
        return short;
    }
    truncate_chars(&name, width)
}

/// 长路径显示成 `.../末两级`
pub fn shorten(path: &str) -> String {
    let sep = if path.contains('\\') && !path.contains('/') {
        '\\'
    } else {
        '/'
    };
    let parts: Vec<&str> = path.split(sep).filter(|s| !s.is_empty()).collect();
    if parts.len() <= 3 {
        return path.to_string();
    }
    let head = parts[0];
    let tail = parts[parts.len() - 1];
    // 盘符（"C:"）后面本来就有分隔符，别再补一个
    if head.ends_with(':') {
        format!("{head}{sep}…{sep}{tail}")
    } else {
        format!("{sep}{head}{sep}…{sep}{tail}")
    }
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool, size: u64, mtime: i64) -> FileEntry {
        FileEntry {
            name: name.into(),
            path: format!("/t/{name}"),
            is_dir,
            is_symlink: false,
            size,
            modified: Some(mtime),
            mode: Some(if is_dir { 0o40_755 } else { 0o100_644 }),
            uid: None,
            gid: None,
        }
    }

    #[test]
    fn sort_puts_dirs_first_then_by_key() {
        let mut v = vec![
            entry("b.txt", false, 300, 30),
            entry("a.txt", false, 100, 10),
            entry("dir", true, 0, 1),
        ];
        sort_entries(&mut v, SortKey::Name);
        assert_eq!(v[0].name, "dir");
        assert_eq!(v[1].name, "a.txt");

        sort_entries(&mut v, SortKey::Size);
        assert_eq!(v[0].name, "dir");
        assert_eq!(v[1].name, "a.txt"); // 100 < 300

        sort_entries(&mut v, SortKey::Time);
        assert_eq!(v[1].name, "b.txt"); // mtime 30 最新
    }

    #[test]
    fn selection_wraps_and_clamps() {
        let mut p = FilePane::new(Side::Local, "/t".into());
        p.entries = vec![entry("a", false, 1, 1), entry("b", false, 1, 1)];
        p.move_sel(-1);
        assert_eq!(p.selected, 1, "从 0 往上应该绕到最后一行");
        p.move_sel(1);
        assert_eq!(p.selected, 0);
        p.selected = 99;
        p.clamp_sel();
        assert_eq!(p.selected, 1);
    }

    #[test]
    fn viewport_follows_selection() {
        let mut p = FilePane::new(Side::Local, "/t".into());
        p.entries = (0..20)
            .map(|i| entry(&format!("f{i}"), false, i as u64, i as i64))
            .collect();
        p.selected = 15;
        p.ensure_visible(5);
        assert_eq!(p.scroll, 11, "选中行应贴在视口底部");
        p.selected = 0;
        p.ensure_visible(5);
        assert_eq!(p.scroll, 0);
    }

    #[test]
    fn remote_parent_is_posix() {
        let mut p = FilePane::new(Side::Remote, "/home/hell".into());
        assert_eq!(p.parent(), Some("/home".into()));
        p.goto("/home");
        assert_eq!(p.parent(), Some("/".into()));
        p.goto("/");
        assert_eq!(p.parent(), None, "根目录没有上级");
        p.goto(".");
        assert_eq!(p.parent(), None, "相对路径不猜上级");
    }

    #[test]
    fn entry_line_degrades_gracefully() {
        let e = entry("hello.txt", false, 2048, 1_709_296_496);
        let full = entry_line(&e, 80);
        assert!(full.contains("hello.txt") && full.contains("2.0K"));
        let narrow = entry_line(&e, 12);
        assert!(narrow.chars().count() <= 12);
        assert!(narrow.starts_with("hello"));
    }
}
