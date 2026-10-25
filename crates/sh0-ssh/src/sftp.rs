//! [`sh0_core::sftp::SftpClient`] 的 russh-sftp 实现。
//!
//! 这一层只做两件事：把 russh-sftp 的类型转成 core 的 [`FileEntry`]，
//! 以及把它的错误码翻译成 core 的 [`SftpError`]（错误码要能被 AI 语义化消费，
//! "路径不存在"和"权限不足"对 AI 的下一步决策完全不同）。

use std::io::SeekFrom;

use async_trait::async_trait;
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, StatusCode};
use sh0_core::sftp::{FileEntry, SftpClient, SftpError, SftpResult};

use crate::client::SshConnection;

/// 一个 SFTP channel 上的文件操作
pub struct RusshSftp {
    session: SftpSession,
    /// 一键式场景必须自己持有连接：russh 的 `Handle` 被 drop 会直接断链，
    /// 而 `SftpSession` 只持有 channel，撑不住底层 TCP。复用会话连接时为 None。
    conn: Option<SshConnection>,
}

impl RusshSftp {
    /// 复用已有会话的连接（channel 独立，连接归会话所有）
    pub fn new(session: SftpSession) -> Self {
        Self {
            session,
            conn: None,
        }
    }

    /// 一键式：自己建连、自己释放
    pub fn owned(session: SftpSession, conn: SshConnection) -> Self {
        Self {
            session,
            conn: Some(conn),
        }
    }
}

#[async_trait]
impl SftpClient for RusshSftp {
    async fn list(&self, path: &str) -> SftpResult<Vec<FileEntry>> {
        let rd = self.session.read_dir(path).await.map_err(map_err)?;
        let mut entries: Vec<FileEntry> = rd
            .map(|e| entry_of(e.path(), &e.metadata()))
            .collect::<Vec<_>>();
        // 目录在前、再按文件名排：UI 与 AI 都不该自己再排一遍
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        Ok(entries)
    }

    async fn stat(&self, path: &str) -> SftpResult<FileEntry> {
        let m = self.session.metadata(path).await.map_err(map_err)?;
        Ok(entry_of(path.to_string(), &m))
    }

    async fn read(&self, path: &str, max_bytes: usize) -> SftpResult<Vec<u8>> {
        // 先问尺寸再读：不让 AI 一个 `read` 把 2GB 日志拉进内存
        let m = self.session.metadata(path).await.map_err(map_err)?;
        let size = m.size.unwrap_or(0) as usize;
        if size > max_bytes {
            return Err(SftpError::TooLarge(format!(
                "{path} 有 {size} 字节，超过上限 {max_bytes} 字节"
            )));
        }
        self.session.read(path).await.map_err(map_err)
    }

    async fn write(&self, path: &str, data: &[u8]) -> SftpResult<()> {
        // 不用 `SftpSession::write`：它只带 `OpenFlags::WRITE`（不创建），
        // 写新文件会直接 NoSuchFile。`create()` = CREATE|TRUNCATE|WRITE，
        // 正好是我们要的"整体覆盖"语义。
        let mut file = self.session.create(path).await.map_err(map_err)?;
        file.write_all(data).await.map_err(io_err)?;
        file.close().await.map_err(io_err)?;
        Ok(())
    }

    async fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> SftpResult<()> {
        // CREATE|WRITE 不带 TRUNCATE：续传时前面的块还在，不能覆盖掉
        let mut file = self
            .session
            .open_with_flags(path, OpenFlags::CREATE | OpenFlags::WRITE)
            .await
            .map_err(map_err)?;
        file.seek(SeekFrom::Start(offset)).await.map_err(io_err)?;
        file.write_all(data).await.map_err(io_err)?;
        file.flush().await.map_err(io_err)?;
        file.close().await.map_err(io_err)?;
        Ok(())
    }

    async fn read_range(&self, path: &str, offset: u64, len: usize) -> SftpResult<Vec<u8>> {
        let mut file = self.session.open(path).await.map_err(map_err)?;
        file.seek(SeekFrom::Start(offset)).await.map_err(io_err)?;
        let mut buf = vec![0u8; len];
        let mut read = 0usize;
        // read_exact 到文件尾会报 UnexpectedEof，这里自己读满为止
        while read < len {
            let n = file.read(&mut buf[read..]).await.map_err(io_err)?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);
        file.close().await.map_err(io_err)?;
        Ok(buf)
    }

    async fn mkdir(&self, path: &str) -> SftpResult<()> {
        self.session.create_dir(path).await.map_err(map_err)
    }

    async fn remove_file(&self, path: &str) -> SftpResult<()> {
        self.session.remove_file(path).await.map_err(map_err)
    }

    async fn remove_dir(&self, path: &str) -> SftpResult<()> {
        self.session.remove_dir(path).await.map_err(map_err)
    }

    async fn rename(&self, from: &str, to: &str) -> SftpResult<()> {
        self.session.rename(from, to).await.map_err(map_err)
    }

    async fn chmod(&self, path: &str, mode: u32) -> SftpResult<()> {
        let attrs = FileAttributes {
            permissions: Some(mode),
            ..Default::default()
        };
        self.session.set_metadata(path, attrs).await.map_err(map_err)
    }

    async fn canonicalize(&self, path: &str) -> SftpResult<String> {
        self.session.canonicalize(path).await.map_err(map_err)
    }

    async fn close(&self) -> SftpResult<()> {
        let r = self.session.close().await.map_err(map_err);
        // 自持连接的一并断开；复用会话的不能碰（会话还要继续用）
        if let Some(conn) = self.conn.as_ref() {
            let _ = conn.close().await;
        }
        r
    }
}

fn entry_of(path: String, m: &FileAttributes) -> FileEntry {
    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
    let ft = m.file_type();
    FileEntry {
        name,
        path,
        is_dir: ft.is_dir(),
        is_symlink: matches!(ft, FileType::Symlink),
        size: m.size.unwrap_or(0),
        modified: m.mtime.map(|t| t as i64),
        mode: m.permissions,
        uid: m.uid,
        gid: m.gid,
    }
}

/// `File` 上的 io 错误没有状态码可查，一律按"链路出问题"处理
fn io_err(e: std::io::Error) -> SftpError {
    SftpError::Disconnected(e.to_string())
}

/// SFTP 状态码 → core 错误。能区分的都区分开，
/// 落不进具体类别的一律 `Remote`（宁可笼统，也不要把"未知"说成"不存在"）。
fn map_err(e: russh_sftp::client::error::Error) -> SftpError {
    use russh_sftp::client::error::Error as E;
    match e {
        E::Status(s) => match s.status_code {
            StatusCode::NoSuchFile => SftpError::NotFound(s.error_message),
            StatusCode::PermissionDenied => SftpError::PermissionDenied(s.error_message),
            other => SftpError::Remote(format!("{other}: {}", s.error_message)),
        },
        E::IO(msg) => SftpError::Disconnected(msg),
        E::Timeout => SftpError::Remote("SFTP 请求超时".into()),
        E::Limited(msg) => SftpError::TooLarge(msg),
        E::UnexpectedPacket => SftpError::Remote("收到意外的数据包".into()),
        E::UnexpectedBehavior(msg) => SftpError::Remote(msg),
    }
}
