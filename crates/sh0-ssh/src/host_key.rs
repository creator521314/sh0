//! 主机密钥校验与首次信任（需求 FR-1.8）
//!
//! 复用 russh 自带的 known_hosts 读写，格式与 OpenSSH 兼容。
//! M0 策略：未记录则自动学习并放行（trust-on-first-use）；
//! 若已记录但密钥不一致，直接拒绝 —— 这是可能的中间人攻击信号。

use std::path::Path;

use russh::keys::known_hosts::{
    check_known_hosts_path, known_host_keys_path, learn_known_hosts_path,
};
use russh::keys::ssh_key::PublicKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyVerdict {
    /// 已在 known_hosts 中且匹配
    Known,
    /// 首次连接，已自动记录
    Learned,
    /// 已记录但密钥不匹配，拒绝连接
    Mismatch { line: usize },
}

/// 校验主机密钥。参数中的 `path` 指向 sh0 自己的 known_hosts 文件。
pub fn verify(path: &Path, host: &str, port: u16, key: &PublicKey) -> HostKeyVerdict {
    match check_known_hosts_path(host, port, key, path) {
        Ok(true) => HostKeyVerdict::Known,
        Ok(false) => {
            // 未命中：可能是首次连接，也可能是记录的是另一种算法。
            // 只有在文件里压根没有该主机的任何密钥时才算“首次”。
            let has_any = known_host_keys_path(host, port, path)
                .map(|v| !v.is_empty())
                .unwrap_or(false);

            if has_any {
                // 有记录但都不匹配，按最坏情况处理
                return HostKeyVerdict::Mismatch { line: 0 };
            }

            match learn_known_hosts_path(host, port, key, path) {
                Ok(()) => HostKeyVerdict::Learned,
                Err(e) => {
                    tracing::warn!("写入 known_hosts 失败: {e}");
                    HostKeyVerdict::Learned
                }
            }
        }
        Err(e) => {
            // russh 在密钥变更时返回 Error::KeyChanged { line }
            let msg = e.to_string();
            if let Some(line) = parse_key_changed_line(&msg) {
                return HostKeyVerdict::Mismatch { line };
            }
            tracing::warn!("known_hosts 校验异常: {msg}");
            HostKeyVerdict::Mismatch { line: 0 }
        }
    }
}

/// 从 russh 的错误文本里抠出行号（避免依赖 russh 内部错误枚举的导出形态）
fn parse_key_changed_line(msg: &str) -> Option<usize> {
    // 形如 "the key at line 3 has changed"
    let idx = msg.find("line ")?;
    let rest = &msg[idx + 5..];
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if num.is_empty() {
        None
    } else {
        num.parse().ok()
    }
}
