//! 用于测试与契约验证的假传输层（需求 NFR-10）。
//!
//! 放在 core 而不是测试目录里，是因为 M2 的契约测试、`ai` 子命令集成测试
//! 都要用它——没有它，"AI 接口是否真的能驱动一个会话"就只能靠真机验证。
//!
//! M4 起这里还住着 [`MemFs`] / [`MockSftp`]：SFTP 的契约同样要能在没有真机
//! 的情况下跑（真机验证只能证明"这次能通"，证明不了"改完还能通"）。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::Host;
use crate::sftp::{FileEntry, SftpClient, SftpError, SftpResult};
use crate::transport::{
    ExecOutput, SessionTransport, TransportError, TransportEvent, TransportFactory, TransportResult,
};

/// 脚本耗尽后的行为
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnEmpty {
    /// 返回 None：连接正常关闭，不重连
    Close,
    /// 报告断开：触发重连流程
    Broken,
    /// 保持连接但不再产生数据。测试里很关键——脚本一播完会话就关闭的话，
    /// 后面的断言根本没机会执行。
    Idle,
}

/// 脚本化的假连接
pub struct MockTransport {
    script: VecDeque<TransportEvent>,
    on_empty: OnEmpty,
    pub sent: Vec<Vec<u8>>,
    pub resizes: Vec<(u16, u16)>,
    pub closed: bool,
    /// 有值时支持在这条连接上开 SFTP（模拟"复用同一条 SSH 连接"）
    fs: Option<MemFs>,
}

impl MockTransport {
    /// 按脚本构造。脚本可以是 `TransportEvent`，也可以直接用字节切片字面量。
    pub fn new(events: Vec<TransportEvent>, on_empty: OnEmpty) -> Self {
        Self {
            script: events.into_iter().collect(),
            on_empty,
            sent: Vec::new(),
            resizes: Vec::new(),
            closed: false,
            fs: None,
        }
    }

    pub fn from_bytes(chunks: &[&[u8]], on_empty: OnEmpty) -> Self {
        Self::new(
            chunks
                .iter()
                .map(|b| TransportEvent::Data(b.to_vec()))
                .collect(),
            on_empty,
        )
    }

    /// 挂一个内存文件系统：之后 `sftp()` 就返回它
    pub fn with_fs(mut self, fs: MemFs) -> Self {
        self.fs = Some(fs);
        self
    }

    /// 播放完后转为 Box<dyn SessionTransport>
    pub fn boxed(self) -> Box<dyn SessionTransport + Send> {
        Box::new(self)
    }
}

#[async_trait]
impl SessionTransport for MockTransport {
    async fn wait(&mut self) -> Option<TransportEvent> {
        match self.script.pop_front() {
            Some(ev) => Some(ev),
            None => match self.on_empty {
                OnEmpty::Close => None,
                OnEmpty::Broken => {
                    // 只报一次，避免测试里无限重连
                    self.on_empty = OnEmpty::Close;
                    Some(TransportEvent::Broken("mock 断开".into()))
                }
                OnEmpty::Idle => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    Some(TransportEvent::Data(Vec::new()))
                }
            },
        }
    }

    async fn send(&mut self, data: &[u8]) -> TransportResult<()> {
        self.sent.push(data.to_vec());
        Ok(())
    }

    async fn resize(&mut self, cols: u16, rows: u16) -> TransportResult<()> {
        self.resizes.push((cols, rows));
        Ok(())
    }

    async fn close(&mut self) -> TransportResult<()> {
        self.closed = true;
        Ok(())
    }

    async fn sftp(&mut self) -> TransportResult<Box<dyn SftpClient>> {
        match &self.fs {
            Some(fs) => Ok(Box::new(MockSftp { fs: fs.clone() })),
            None => Err(TransportError::NotImplemented("mock 未挂文件系统".into())),
        }
    }
}

/// 可控失败的工厂。
///
/// 三个维度可以分别注入：
///   - `fail_times`：前 N 次 `connect` 直接失败（模拟服务端不可达）
///   - `disconnect_times`：前 N 次连接播完脚本后**报断线**（模拟网络闪断，触发重连）
///   - 其余连接播完脚本后正常关闭
///
/// 必须能分别控制，否则"播完就断"会让每次重连都再断一次，
/// 测试里表现为无限重连，掩盖真正要验证的行为。
#[derive(Clone)]
pub struct MockFactory {
    scripts: Arc<Mutex<VecDeque<Vec<TransportEvent>>>>,
    fail_times: Arc<Mutex<u32>>,
    disconnect_times: Arc<Mutex<u32>>,
    /// exec channel 的脚本化返回（按调用顺序取，空了重复最后一个）
    exec_outputs: Arc<Mutex<VecDeque<ExecOutput>>>,
    /// 挂在连接上的内存文件系统；`with_sftp` 后会话内 SFTP 才能用
    fs: Arc<Mutex<Option<MemFs>>>,
    pub connect_calls: Arc<Mutex<u32>>,
    pub exec_calls: Arc<Mutex<u32>>,
    pub sftp_calls: Arc<Mutex<u32>>,
}

impl MockFactory {
    fn new(scripts: Vec<Vec<TransportEvent>>, fail_times: u32, disconnect_times: u32) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(VecDeque::from(scripts))),
            fail_times: Arc::new(Mutex::new(fail_times)),
            disconnect_times: Arc::new(Mutex::new(disconnect_times)),
            exec_outputs: Arc::new(Mutex::new(VecDeque::new())),
            fs: Arc::new(Mutex::new(None)),
            connect_calls: Arc::new(Mutex::new(0)),
            exec_calls: Arc::new(Mutex::new(0)),
            sftp_calls: Arc::new(Mutex::new(0)),
        }
    }

    /// 每次连接播同一份脚本，播完正常关闭（不触发重连）
    pub fn repeating(script: Vec<TransportEvent>, fail_times: u32) -> Self {
        Self::new(vec![script], fail_times, 0)
    }

    /// 按序取脚本，播完正常关闭
    pub fn sequenced(scripts: Vec<Vec<TransportEvent>>, fail_times: u32) -> Self {
        Self::new(scripts, fail_times, 0)
    }

    /// 前 `disconnect_times` 次连接播完脚本后报断线，用于验证重连链路（AC-3）
    pub fn disconnecting(
        scripts: Vec<Vec<TransportEvent>>,
        fail_times: u32,
        disconnect_times: u32,
    ) -> Self {
        Self::new(scripts, fail_times, disconnect_times)
    }

    /// 配置 exec channel 的返回脚本（W2.4 备选路径）
    pub fn with_exec(self, outputs: Vec<ExecOutput>) -> Self {
        *self.exec_outputs.lock().unwrap() = VecDeque::from(outputs);
        self
    }

    pub fn calls(&self) -> u32 {
        *self.connect_calls.lock().unwrap()
    }

    pub fn exec_calls(&self) -> u32 {
        *self.exec_calls.lock().unwrap()
    }

    /// 挂上内存文件系统：会话内 SFTP 与一键式 SFTP 都会用它
    pub fn with_sftp(self, fs: MemFs) -> Self {
        *self.fs.lock().unwrap() = Some(fs);
        self
    }

    pub fn sftp_calls(&self) -> u32 {
        *self.sftp_calls.lock().unwrap()
    }
}

#[async_trait]
impl TransportFactory for MockFactory {
    async fn connect(
        &self,
        _host: &Host,
        _cols: u16,
        _rows: u16,
    ) -> TransportResult<Box<dyn SessionTransport + Send>> {
        *self.connect_calls.lock().unwrap() += 1;

        let mut fails = self.fail_times.lock().unwrap();
        if *fails > 0 {
            *fails -= 1;
            return Err(TransportError::Connect("mock 连接失败".into()));
        }
        drop(fails);

        let mut scripts = self.scripts.lock().unwrap();
        let script = match scripts.pop_front() {
            Some(s) => {
                scripts.push_back(s.clone());
                s
            }
            None => Vec::new(),
        };
        drop(scripts);

        let mut disc = self.disconnect_times.lock().unwrap();
        let on_empty = if *disc > 0 {
            *disc -= 1;
            OnEmpty::Broken
        } else {
            OnEmpty::Idle
        };
        drop(disc);

        let mut t = MockTransport::new(script, on_empty);
        if let Some(fs) = self.fs.lock().unwrap().clone() {
            t = t.with_fs(fs);
        }
        Ok(t.boxed())
    }

    async fn exec(
        &self,
        _host: &Host,
        _cmd: &str,
        _timeout: std::time::Duration,
    ) -> TransportResult<ExecOutput> {
        *self.exec_calls.lock().unwrap() += 1;
        let mut outs = self.exec_outputs.lock().unwrap();
        let out = match outs.pop_front() {
            Some(o) => {
                outs.push_back(o.clone());
                o
            }
            None => ExecOutput::default(),
        };
        Ok(out)
    }

    async fn open_sftp(&self, _host: &Host) -> TransportResult<Box<dyn SftpClient>> {
        *self.sftp_calls.lock().unwrap() += 1;
        let fs = self.fs.lock().unwrap().clone().unwrap_or_default();
        Ok(Box::new(MockSftp { fs }))
    }
}

// ---- 内存文件系统（FR-4.x 的 SFTP 替身）----

#[derive(Debug, Clone)]
struct MemNode {
    is_dir: bool,
    data: Vec<u8>,
    mtime: i64,
    mode: u32,
}

/// 契约测试用的内存文件系统。路径一律 POSIX 风格，不处理 `..`（够用即可）。
#[derive(Clone, Default)]
pub struct MemFs {
    nodes: Arc<Mutex<HashMap<String, MemNode>>>,
    cwd: Arc<Mutex<String>>,
}

impl MemFs {
    /// 预置一棵小树：`/tmp/hello.txt`、`/tmp/logs/app.log`
    pub fn new() -> Self {
        let fs = Self {
            nodes: Arc::new(Mutex::new(HashMap::new())),
            cwd: Arc::new(Mutex::new("/tmp".to_string())),
        };
        fs.insert_dir("/");
        fs.insert_dir("/tmp");
        fs.insert_dir("/tmp/logs");
        fs.insert_file("/tmp/hello.txt", b"hello");
        fs.insert_file("/tmp/logs/app.log", b"line1\nline2\n");
        fs
    }

    pub fn insert_file(&self, path: &str, data: &[u8]) {
        self.nodes.lock().unwrap().insert(
            self.norm(path),
            MemNode {
                is_dir: false,
                data: data.to_vec(),
                mtime: 1_700_000_000,
                mode: 0o100_644,
            },
        );
    }

    pub fn insert_dir(&self, path: &str) {
        self.nodes.lock().unwrap().insert(
            self.norm(path),
            MemNode {
                is_dir: true,
                data: Vec::new(),
                mtime: 1_700_000_000,
                mode: 0o40_755,
            },
        );
    }

    /// 规范化：相对路径按 cwd 展开，去掉末尾斜杠
    fn norm(&self, path: &str) -> String {
        let cwd = self.cwd.lock().unwrap().clone();
        let p = match path {
            "" | "." => cwd,
            p if p.starts_with('/') => p.to_string(),
            p => format!("{cwd}/{p}"),
        };
        let s = p.trim_end_matches('/').to_string();
        if s.is_empty() { "/".to_string() } else { s }
    }

    fn get(&self, path: &str) -> Option<MemNode> {
        self.nodes.lock().unwrap().get(&self.norm(path)).cloned()
    }

    pub fn exists(&self, path: &str) -> bool {
        self.nodes.lock().unwrap().contains_key(&self.norm(path))
    }

    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        self.get(path).map(|n| n.data.clone())
    }

    fn list(&self, path: &str) -> SftpResult<Vec<FileEntry>> {
        let dir = self.norm(path);
        let nodes = self.nodes.lock().unwrap();
        if !nodes.contains_key(&dir) {
            return Err(SftpError::NotFound(dir.clone()));
        }
        if !nodes[&dir].is_dir {
            return Err(SftpError::NotDir(dir.clone()));
        }
        let mut out: Vec<FileEntry> = nodes
            .iter()
            .filter(|(k, _)| parent_of(k).as_deref() == Some(dir.as_str()))
            .map(|(k, n)| FileEntry {
                name: k.rsplit('/').next().unwrap_or(k).to_string(),
                path: k.clone(),
                is_dir: n.is_dir,
                is_symlink: false,
                size: n.data.len() as u64,
                modified: Some(n.mtime),
                mode: Some(n.mode),
                uid: Some(1000),
                gid: Some(1000),
            })
            .collect();
        out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }
}

/// 直接实现：与 `RusshSftp` 一对一，测试里替换掉真连接
pub struct MockSftp {
    fs: MemFs,
}

impl MockSftp {
    /// 包一个内存文件系统。契约测试里会话与一键式两条路径共用同一份，
    /// 于是"改完还能读回来"这件事能被断言。
    pub fn new(fs: MemFs) -> Self {
        Self { fs }
    }
}

#[async_trait]
impl SftpClient for MockSftp {
    async fn list(&self, path: &str) -> SftpResult<Vec<FileEntry>> {
        self.fs.list(path)
    }

    async fn stat(&self, path: &str) -> SftpResult<FileEntry> {
        let p = self.fs.norm(path);
        let n = self
            .fs
            .get(&p)
            .ok_or_else(|| SftpError::NotFound(p.clone()))?;
        Ok(FileEntry {
            name: p.rsplit('/').next().unwrap_or(&p).to_string(),
            path: p,
            is_dir: n.is_dir,
            is_symlink: false,
            size: n.data.len() as u64,
            modified: Some(n.mtime),
            mode: Some(n.mode),
            uid: Some(1000),
            gid: Some(1000),
        })
    }

    async fn read(&self, path: &str, max_bytes: usize) -> SftpResult<Vec<u8>> {
        let p = self.fs.norm(path);
        let n = self
            .fs
            .get(&p)
            .ok_or_else(|| SftpError::NotFound(p.clone()))?;
        if n.data.len() > max_bytes {
            return Err(SftpError::TooLarge(format!(
                "{p} 有 {} 字节，超过上限 {max_bytes}",
                n.data.len()
            )));
        }
        Ok(n.data.clone())
    }

    async fn write(&self, path: &str, data: &[u8]) -> SftpResult<()> {
        let p = self.fs.norm(path);
        // 父目录必须已存在：真服务端也是这个行为
        match parent_of(&p) {
            Some(parent) if !self.fs.exists(&parent) => {
                return Err(SftpError::NotFound(parent));
            }
            _ => {}
        }
        self.fs.insert_file(&p, data);
        Ok(())
    }

    async fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> SftpResult<()> {
        let p = self.fs.norm(path);
        // 与整体 write 一致：父目录不存在就该报错，不能默默造出来
        if let Some(parent) = parent_of(&p)
            && !self.fs.exists(&parent)
        {
            return Err(SftpError::NotFound(parent));
        }
        let mut nodes = self.fs.nodes.lock().unwrap();
        let node = nodes.entry(p.clone()).or_insert(MemNode {
            is_dir: false,
            data: Vec::new(),
            mtime: 1_700_000_000,
            mode: 0o100_644,
        });
        let at = offset as usize;
        if node.data.len() < at {
            // 中间空档补 0：真文件系统也是这个语义
            node.data.resize(at, 0);
        }
        if node.data.len() < at + data.len() {
            node.data.resize(at + data.len(), 0);
        }
        node.data[at..at + data.len()].copy_from_slice(data);
        Ok(())
    }

    async fn read_range(&self, path: &str, offset: u64, len: usize) -> SftpResult<Vec<u8>> {
        let p = self.fs.norm(path);
        let n = self.fs.get(&p).ok_or_else(|| SftpError::NotFound(p.clone()))?;
        let at = (offset as usize).min(n.data.len());
        let end = (at + len).min(n.data.len());
        Ok(n.data[at..end].to_vec())
    }

    async fn mkdir(&self, path: &str) -> SftpResult<()> {
        let p = self.fs.norm(path);
        if self.fs.exists(&p) {
            return Err(SftpError::AlreadyExists(p));
        }
        self.fs.insert_dir(&p);
        Ok(())
    }

    async fn remove_file(&self, path: &str) -> SftpResult<()> {
        let p = self.fs.norm(path);
        let n = self
            .fs
            .get(&p)
            .ok_or_else(|| SftpError::NotFound(p.clone()))?;
        if n.is_dir {
            return Err(SftpError::NotDir(p));
        }
        self.fs.nodes.lock().unwrap().remove(&p);
        Ok(())
    }

    async fn remove_dir(&self, path: &str) -> SftpResult<()> {
        let p = self.fs.norm(path);
        let n = self
            .fs
            .get(&p)
            .ok_or_else(|| SftpError::NotFound(p.clone()))?;
        if !n.is_dir {
            return Err(SftpError::Remote(format!("{p} 不是目录")));
        }
        if self.fs.list(&p).is_ok_and(|v| !v.is_empty()) {
            return Err(SftpError::Remote(format!("{p} 非空")));
        }
        self.fs.nodes.lock().unwrap().remove(&p);
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> SftpResult<()> {
        let a = self.fs.norm(from);
        let b = self.fs.norm(to);
        let mut nodes = self.fs.nodes.lock().unwrap();
        let n = nodes
            .remove(&a)
            .ok_or_else(|| SftpError::NotFound(a.clone()))?;
        nodes.insert(b, n);
        Ok(())
    }

    async fn chmod(&self, path: &str, mode: u32) -> SftpResult<()> {
        let p = self.fs.norm(path);
        let mut nodes = self.fs.nodes.lock().unwrap();
        let n = nodes
            .get_mut(&p)
            .ok_or_else(|| SftpError::NotFound(p.clone()))?;
        n.mode = mode;
        Ok(())
    }

    async fn canonicalize(&self, path: &str) -> SftpResult<String> {
        let p = self.fs.norm(path);
        if !self.fs.exists(&p) {
            return Err(SftpError::NotFound(p));
        }
        Ok(p)
    }

    async fn close(&self) -> SftpResult<()> {
        Ok(())
    }
}

fn parent_of(path: &str) -> Option<String> {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(i) => Some(p[..i].to_string()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mem_fs_lists_and_walks() {
        let fs = MemFs::new();
        let sftp = MockSftp::new(fs.clone());
        let root = sftp.list("/tmp").await.unwrap();
        assert_eq!(root[0].name, "logs", "目录排前面");
        assert!(root.iter().any(|e| e.name == "hello.txt"));

        // 相对路径按 cwd 展开，UI 第一次列目录用的就是 "."
        let rel = sftp.list(".").await.unwrap();
        assert_eq!(rel.len(), root.len());
        assert_eq!(sftp.canonicalize(".").await.unwrap(), "/tmp");
    }

    #[tokio::test]
    async fn mem_fs_write_read_rename_remove() {
        let fs = MemFs::new();
        let sftp = MockSftp::new(fs.clone());
        sftp.write("/tmp/a.txt", b"abc").await.unwrap();
        assert_eq!(sftp.read("/tmp/a.txt", 100).await.unwrap(), b"abc");
        assert!(matches!(
            sftp.read("/tmp/a.txt", 1).await,
            Err(SftpError::TooLarge(_))
        ));

        sftp.rename("/tmp/a.txt", "/tmp/b.txt").await.unwrap();
        assert!(!fs.exists("/tmp/a.txt"));
        assert_eq!(fs.read("/tmp/b.txt").unwrap(), b"abc");

        sftp.remove_file("/tmp/b.txt").await.unwrap();
        assert!(!fs.exists("/tmp/b.txt"));

        sftp.mkdir("/tmp/new").await.unwrap();
        assert!(fs.exists("/tmp/new"));
        sftp.remove_dir("/tmp/new").await.unwrap();
        assert!(!fs.exists("/tmp/new"));

        // 写入不存在的目录要报错，而不是默默建出来
        assert!(sftp.write("/nope/x.txt", b"x").await.is_err());
    }
}
