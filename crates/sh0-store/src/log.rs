//! daemon 日志的轮转与保留（需求 FR-8.5）。
//!
//! 后台 daemon 没有控制台，日志是唯一的现场。但一直追加下去总有一天会
//! 把磁盘写满，所以既要轮转（按大小切文件）也要保留策略（按时间删旧的）。
//!
//! 这里自己实现而不是引入 `tracing-appender`：那边只能按时间切，
//! 而"单个文件不超过 N MB"才是我们要的约束；实现也就一百行。

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// 日志配置
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LogConfig {
    /// 单个日志文件的大小上限（MB）。超出即切新文件
    #[serde(default = "default_max_mb")]
    pub max_mb: u32,
    /// 保留的历史文件个数（`.1` ~ `.keep`）
    #[serde(default = "default_keep")]
    pub keep: u32,
    /// 历史文件保留天数；超期的直接删
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
}

fn default_max_mb() -> u32 {
    8
}
fn default_keep() -> u32 {
    3
}
fn default_retention_days() -> u32 {
    30
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            max_mb: default_max_mb(),
            keep: default_keep(),
            retention_days: default_retention_days(),
        }
    }
}

/// 内部状态：当前写的文件 + 已写字节数
struct State {
    file: std::fs::File,
    path: PathBuf,
    written: u64,
    max_bytes: u64,
    keep: usize,
}

impl State {
    fn rotate(&mut self) -> io::Result<()> {
        let _ = self.file.flush();
        rotate_file(&self.path, self.keep)?;
        // 新文件从头写，计数归零
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

/// 一个 `io::Write`，写满 `max_bytes` 就自动切文件。
///
/// 同时实现 `MakeWriter`，可以直接交给 `tracing_subscriber::fmt()`。
/// 内部用 `Mutex` 是因为 tracing 可能从多个任务并发写。
#[derive(Clone)]
pub struct RotatingFileWriter {
    inner: Arc<Mutex<State>>,
}

impl RotatingFileWriter {
    pub fn new(path: PathBuf, max_mb: u32, keep: u32) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            inner: Arc::new(Mutex::new(State {
                file,
                path,
                written,
                max_bytes: max_mb.max(1) as u64 * 1024 * 1024,
                keep: keep.max(1) as usize,
            })),
        })
    }
}

impl Write for RotatingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut st = self.inner.lock().unwrap();
        let n = st.file.write(buf)?;
        st.written += n as u64;
        if st.written >= st.max_bytes {
            st.rotate()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.lock().unwrap().file.flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RotatingFileWriter {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// 把 `path` 切成 `path.1`，原有的 `.1` 顺推为 `.2`，超出 `keep` 的丢弃。
///
/// 返回是否真的切了（文件不存在时什么也不做）。
pub fn rotate_file(path: &Path, keep: usize) -> io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    // 从最老的一个开始删，再依次往后推
    let oldest = format!("{}.{}", path.display(), keep);
    if Path::new(&oldest).exists() {
        std::fs::remove_file(&oldest)?;
    }
    for i in (1..keep).rev() {
        let from = format!("{}.{}", path.display(), i);
        let to = format!("{}.{}", path.display(), i + 1);
        if Path::new(&from).exists() {
            std::fs::rename(&from, &to)?;
        }
    }
    std::fs::rename(path, format!("{}.1", path.display()))?;
    Ok(true)
}

/// 删除超过 `retention_days` 的轮转文件（`.1`、`.2`……），返回删了几个。
///
/// 只对带序号的历史文件动手——正在写的那个永远不动。
pub fn prune_old_logs(path: &Path, retention_days: u32) -> usize {
    if retention_days == 0 {
        return 0;
    }
    let cutoff = SystemTime::now() - Duration::from_secs(retention_days as u64 * 86400);
    let base = path.display().to_string();
    let mut removed = 0;
    for i in 1..=64 {
        let p = PathBuf::from(format!("{base}.{i}"));
        if !p.exists() {
            continue;
        }
        let expired = std::fs::metadata(&p)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| t < cutoff)
            .unwrap_or(false);
        if expired {
            // 删不掉就算了，下次再试——为日志清理去中断 daemon 不值得
            if std::fs::remove_file(&p).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sh0-log-test-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("daemon.log")
    }

    #[test]
    fn rotates_when_exceeding_max() {
        let path = tmp("max");
        // 1 MB 上限、保留 2 个：写 3 MB 应该切出 .1 / .2
        let mut w = RotatingFileWriter::new(path.clone(), 1, 2).unwrap();
        let chunk = vec![b'x'; 256 * 1024];
        for _ in 0..12 {
            w.write_all(&chunk).unwrap();
        }
        w.flush().unwrap();
        assert!(path.exists(), "当前文件应存在");
        assert!(Path::new(&format!("{}.1", path.display())).exists());
        assert!(Path::new(&format!("{}.2", path.display())).exists());
        assert!(
            !Path::new(&format!("{}.3", path.display())).exists(),
            "keep=2 时不该出现 .3"
        );
    }

    #[test]
    fn prune_removes_expired_only() {
        let path = tmp("prune");
        std::fs::write(&path, b"x").unwrap();
        let old = PathBuf::from(format!("{}.1", path.display()));
        std::fs::write(&old, b"old").unwrap();
        // 把历史文件的 mtime 推到 60 天前
        let past = SystemTime::now() - Duration::from_secs(60 * 86400);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(past)
            .unwrap();

        let removed = prune_old_logs(&path, 30);
        assert_eq!(removed, 1, "过期文件应被删掉");
        assert!(!old.exists());
        assert!(path.exists(), "当前日志不能删");
    }

    #[test]
    fn retention_zero_keeps_everything() {
        let path = tmp("zero");
        let old = PathBuf::from(format!("{}.1", path.display()));
        std::fs::write(&old, b"old").unwrap();
        assert_eq!(prune_old_logs(&path, 0), 0);
        assert!(old.exists());
    }
}
