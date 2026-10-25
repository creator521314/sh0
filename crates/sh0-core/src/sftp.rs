//! SFTP 子系统抽象（M4 / FR-4.x）。
//!
//! 与 [`crate::transport::SessionTransport`] 的分工：那边是**字符流**（终端），
//! 这边是**结构化文件操作**。两者共用同一条 TCP 连接——SFTP 走
//! `channel.request_subsystem("sftp")` 另开一个 channel，不新建连接（FR-4.1）。
//!
//! 做成 trait 的理由同传输层：契约测试可以注入内存实现，不必真的连服务器。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 远端目录项（`ai file list` 与双栏 UI 的数据源）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    /// 文件名（不含路径）
    pub name: String,
    /// 完整路径
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    /// mtime（unix 秒）。服务端没给时是 None
    pub modified: Option<i64>,
    /// 权限位（含文件类型位，语义同 POSIX `st_mode`）
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

impl FileEntry {
    /// 权限位的低 9 位（rwxrwxrwx），用于显示 `-rw-r--r--`
    pub fn perm_bits(&self) -> u32 {
        self.mode.unwrap_or(0) & 0o777
    }
}

#[derive(Debug, Error)]
pub enum SftpError {
    #[error("连接已断开: {0}")]
    Disconnected(String),
    #[error("远端拒绝: {0}")]
    Remote(String),
    #[error("路径不存在: {0}")]
    NotFound(String),
    #[error("不是目录: {0}")]
    NotDir(String),
    #[error("已存在: {0}")]
    AlreadyExists(String),
    #[error("权限不足: {0}")]
    PermissionDenied(String),
    #[error("超出限制: {0}")]
    TooLarge(String),
    #[error("未实现: {0}")]
    NotImplemented(String),
}

pub type SftpResult<T> = std::result::Result<T, SftpError>;

/// 单个 SFTP channel 上的文件操作。
///
/// 方法都取 `&self`：实现内部是 `Arc`，且 russh-sftp 的高层 API 本身就是 `&self`，
/// 这样 trait 对象能安全地放进 daemon 的共享状态里。
#[async_trait]
pub trait SftpClient: Send + Sync {
    /// 列目录；按文件名排序，目录在前
    async fn list(&self, path: &str) -> SftpResult<Vec<FileEntry>>;
    async fn stat(&self, path: &str) -> SftpResult<FileEntry>;
    /// 读文件；超过 `max_bytes` 报 [`SftpError::TooLarge`]（不让 AI 一口气拉爆内存）
    async fn read(&self, path: &str, max_bytes: usize) -> SftpResult<Vec<u8>>;
    async fn write(&self, path: &str, data: &[u8]) -> SftpResult<()>;
    async fn mkdir(&self, path: &str) -> SftpResult<()>;
    async fn remove_file(&self, path: &str) -> SftpResult<()>;
    async fn remove_dir(&self, path: &str) -> SftpResult<()>;
    async fn rename(&self, from: &str, to: &str) -> SftpResult<()>;
    async fn chmod(&self, path: &str, mode: u32) -> SftpResult<()>;
    /// 路径规范化（`~`、相对路径、符号链接都要先展开，否则审计里的路径不可信）
    async fn canonicalize(&self, path: &str) -> SftpResult<String>;
    /// 关闭 channel。不调用也行（drop 会关），但显式关能立刻释放服务端资源
    async fn close(&self) -> SftpResult<()>;
}

/// 把 `rwxrwxrwx` 渲染成 `ls` 风格的字符串，供纯文本输出用
pub fn mode_string(is_dir: bool, mode: Option<u32>) -> String {
    let bits = mode.unwrap_or(0);
    let type_ch = if is_dir { 'd' } else { '-' };
    let mut s = String::with_capacity(10);
    s.push(type_ch);
    for shift in [6u32, 3, 0] {
        let tri = (bits >> shift) & 0o7;
        s.push(if tri & 0o4 != 0 { 'r' } else { '-' });
        s.push(if tri & 0o2 != 0 { 'w' } else { '-' });
        s.push(if tri & 0o1 != 0 { 'x' } else { '-' });
    }
    s
}

/// 人类可读的体积：`1.5K`、`238M`。给 UI 与 AI 共用，避免两处各写一套。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut n = bytes as f64;
    let mut i = 0usize;
    while n >= 1024.0 && i + 1 < UNITS.len() {
        n /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes}B")
    } else if n < 10.0 {
        format!("{n:.1}{}", UNITS[i])
    } else {
        format!("{:.0}{}", n, UNITS[i])
    }
}

/// mtime（unix 秒）→ `YYYY-MM-DD HH:MM:SS`；无值时给等宽占位，方便对齐。
///
/// 手写 civil_from_days 而不是引 chrono：整个项目只有这一处需要日历换算。
pub fn format_mtime(secs: Option<i64>) -> String {
    let Some(s) = secs.filter(|v| *v > 0) else {
        return "-".to_string();
    };
    let days = s.div_euclid(86_400);
    let rem = s.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant 的 civil_from_days：从 1970 起算的天数 → (年, 月, 日)
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perm_bits_masks_file_type() {
        // 0o100644：常规文件 + rw-r--r--
        let e = FileEntry {
            name: "a.txt".into(),
            path: "/tmp/a.txt".into(),
            is_dir: false,
            is_symlink: false,
            size: 10,
            modified: None,
            mode: Some(0o100644),
            uid: None,
            gid: None,
        };
        assert_eq!(e.perm_bits(), 0o644);
    }

    #[test]
    fn mode_string_renders_ls_style() {
        assert_eq!(mode_string(false, Some(0o100644)), "-rw-r--r--");
        assert_eq!(mode_string(true, Some(0o40755)), "drwxr-xr-x");
        assert_eq!(mode_string(false, None), "----------");
    }

    #[test]
    fn human_size_steps_units() {
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(999), "999B");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(15 * 1024), "15K");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0M");
    }

    #[test]
    fn format_mtime_uses_calendar_days() {
        // 2024-03-01 12:34:56 UTC
        let secs = 1_709_296_496;
        assert_eq!(format_mtime(Some(secs)), "2024-03-01 12:34:56");
        assert_eq!(format_mtime(None), "-");
        assert_eq!(format_mtime(Some(0)), "-");
    }
}
