//! 凭据存储（需求 FR-1.4~FR-1.7，任务 W2.12）。
//!
//! 两级后端，读取时**钥匙串优先、加密文件兜底**：
//!
//! ```text
//! SecretRef::Keyring { service, account }
//!        │
//!        ├─▶ OS 钥匙串（macOS Keychain / Windows Credential Manager /
//!        │    Linux Secret Service）——能用就用，最安全
//!        │
//!        └─▶ creds.vault：本地 data-key（creds.key, 0600）+
//!             ChaCha20-Poly1305 加密的 JSON（无 dbus 的 Linux 等场景）
//! ```
//!
//! 为什么不再用「主密码 + Argon2id」：后台 daemon 没有 TTY，主密码只能常驻环境变量，
//! 与「在 TUI 里输一次密码、之后无感重连」的目标冲突。data-key 方案把信任边界
//! 明确成「与 ~/.ssh 私钥同档」：防其他用户 / 备份泄漏，不防本机 OS 账户本身。
//!
//! 铁律（FR-1.7 / AC-7）：配置文件里永远只有 `SecretRef` **引用**，
//! 明文值只可能出现在：环境变量、用户 stdin、钥匙串、加密 vault。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};
use crate::paths::Paths;

/// 旧版主密码 vault 的迁移入口（不再是运行期必需）
pub const MASTER_PASSWORD_ENV: &str = "SH0_MASTER_PASSWORD";

/// 设为任意非空值可禁用钥匙串后端（排障 / 测试用）
pub const DISABLE_KEYRING_ENV: &str = "SH0_DISABLE_KEYRING";

/// Argon2id 参数（仅用于解密旧版 v1 vault）
const ARGON2_M_COST: u32 = 19 * 1024;
const ARGON2_T_COST: u32 = 2;
const ARGON2_P_COST: u32 = 1;
const KEY_LEN: usize = 32;

/// v2 vault（当前格式）：data-key + ChaCha20-Poly1305
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaultFileV2 {
    version: u32,
    /// hex 编码的 nonce / 密文
    nonce: String,
    data: String,
}

/// v1 vault（旧格式）：主密码 + Argon2id。只用于迁移读取。
#[derive(Debug, Clone, Deserialize)]
struct VaultFileV1 {
    salt: String,
    #[serde(default = "default_m_cost")]
    m_cost: u32,
    #[serde(default = "default_t_cost")]
    t_cost: u32,
    #[serde(default = "default_p_cost")]
    p_cost: u32,
    nonce: String,
    data: String,
}

fn default_m_cost() -> u32 {
    ARGON2_M_COST
}
fn default_t_cost() -> u32 {
    ARGON2_T_COST
}
fn default_p_cost() -> u32 {
    ARGON2_P_COST
}

/// 解密后的条目表：key = "service/account"
type Entries = BTreeMap<String, String>;

enum VaultState {
    /// 已解密（或首次创建的空表）
    Loaded {
        key: [u8; KEY_LEN],
        entries: Entries,
    },
    /// vault 文件存在但解不开（data-key 丢失 / 文件损坏）
    Locked,
}

/// 凭据后端状态（`credential.status` 用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialStatus {
    /// 钥匙串当前是否可用（缓存值：首次失败后会置 false）
    pub keyring_available: bool,
    /// vault 是否可读写（未被锁定）
    pub vault_ready: bool,
    /// 最近一次实际使用的后端："keyring" / "vault" / "none"
    pub backend: &'static str,
}

pub struct CredentialStore {
    path: PathBuf,
    state: Mutex<VaultState>,
    /// 钥匙串是否可用（首次失败后置 false，后续直接走 vault）
    keyring_ok: AtomicBool,
    /// 当前实际后端名，用于 host show / 排障
    backend: Mutex<&'static str>,
}

impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialStore")
            .field("path", &self.path)
            .field("backend", &self.backend.lock().unwrap())
            .finish_non_exhaustive()
    }
}

impl CredentialStore {
    /// 打开（或按需创建）凭据存储。
    ///
    /// 不再要求主密码：v2 vault 用本地 `creds.key` 解密。
    /// 若发现旧版 v1 vault，按 §3.4 迁移（有 `SH0_MASTER_PASSWORD` 则自动迁移，
    /// 否则备份为 `creds.vault.legacy` 并新建空库）。
    pub fn open(paths: &Paths) -> Result<Self> {
        let path = paths.vault_file();
        let key_path = paths.cred_key();
        // 目录可能还没建（首次运行、或测试里直接给 Paths）：不建的话首次 set 会
        // 撞上 NotFound——那是个"打开成功、写入才炸"的坏失败模式。
        if let Some(dir) = path.parent()
            && !dir.exists()
        {
            std::fs::create_dir_all(dir)
                .map_err(|e| StoreError::Credential(format!("创建凭据目录失败: {e}")))?;
        }

        let state = if path.exists() {
            match vault_version(&path)? {
                1 => {
                    // 旧格式必须先有新的 data-key（迁移后会立刻重写 vault）
                    let key = ensure_data_key(&key_path)?;
                    migrate_v1(&path, &key)?
                }
                _ => match read_data_key(&key_path)? {
                    Some(key) => match load_vault_v2(&path, &key) {
                        Ok(entries) => VaultState::Loaded { key, entries },
                        Err(e) => {
                            tracing::warn!("vault 解密失败（creds.key 不匹配或文件损坏）: {e}");
                            VaultState::Locked
                        }
                    },
                    None => {
                        tracing::warn!(
                            "发现 {} 但缺少 {}，无法解密；请从备份恢复 data-key",
                            path.display(),
                            key_path.display()
                        );
                        VaultState::Locked
                    }
                },
            }
        } else if let Some((key, entries)) = try_migrate_legacy(&path, &key_path)? {
            VaultState::Loaded { key, entries }
        } else {
            VaultState::Loaded {
                key: ensure_data_key(&key_path)?,
                entries: Entries::new(),
            }
        };

        Ok(Self {
            path,
            state: Mutex::new(state),
            keyring_ok: AtomicBool::new(std::env::var(DISABLE_KEYRING_ENV).is_err()),
            backend: Mutex::new("none"),
        })
    }

    /// 存入一条秘密。先写钥匙串，不可用则写 vault（首次 vault 写入会创建文件）。
    pub fn set(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        let key_name = format!("{service}/{account}");

        // 1) 钥匙串优先
        if self.keyring_ok.load(Ordering::Relaxed) {
            match keyring::Entry::new(service, account) {
                Ok(entry) => match entry.set_password(secret) {
                    Ok(()) => {
                        *self.backend.lock().unwrap() = "keyring";
                        tracing::debug!("秘密已写入钥匙串 {key_name}");
                        return Ok(());
                    }
                    Err(keyring::Error::NoEntry) => {} // 不该发生，按不可用处理
                    Err(e) => {
                        tracing::info!("钥匙串不可用（{e}），回落到加密 vault");
                        self.keyring_ok.store(false, Ordering::Relaxed);
                    }
                },
                Err(e) => {
                    tracing::info!("钥匙串不可用（{e}），回落到加密 vault");
                    self.keyring_ok.store(false, Ordering::Relaxed);
                }
            }
        }

        // 2) vault 兜底
        let mut state = self.state.lock().unwrap();
        let VaultState::Loaded { key, entries } = &mut *state else {
            return Err(StoreError::Credential(format!(
                "无法写入 {key_name}：vault 已锁定（{} 不可用或与 creds.key 不匹配）",
                self.path.display()
            )));
        };
        // 先写副本再落盘：save 失败时内存与磁盘不能分叉
        let mut next = entries.clone();
        next.insert(key_name.clone(), secret.to_string());
        save_vault(&self.path, key, &next)?;
        *entries = next;
        *self.backend.lock().unwrap() = "vault";
        tracing::debug!("秘密已写入加密 vault {key_name}");
        Ok(())
    }

    /// 取秘密。钥匙串未命中时查 vault；都没有返回 Ok(None)。
    pub fn get(&self, service: &str, account: &str) -> Result<Option<String>> {
        let key_name = format!("{service}/{account}");

        if self.keyring_ok.load(Ordering::Relaxed) {
            match keyring::Entry::new(service, account) {
                Ok(entry) => match entry.get_password() {
                    Ok(v) => {
                        *self.backend.lock().unwrap() = "keyring";
                        return Ok(Some(v));
                    }
                    // 未录入：可能躺在 vault 里，继续找
                    Err(keyring::Error::NoEntry) => {}
                    Err(e) => {
                        tracing::info!("钥匙串不可用（{e}），回落到加密 vault");
                        self.keyring_ok.store(false, Ordering::Relaxed);
                    }
                },
                Err(e) => {
                    tracing::info!("钥匙串不可用（{e}），回落到加密 vault");
                    self.keyring_ok.store(false, Ordering::Relaxed);
                }
            }
        }

        let state = self.state.lock().unwrap();
        match &*state {
            VaultState::Loaded { entries, .. } => {
                if entries.contains_key(&key_name) {
                    *self.backend.lock().unwrap() = "vault";
                }
                Ok(entries.get(&key_name).cloned())
            }
            VaultState::Locked => Err(StoreError::Credential(format!(
                "无法读取 {key_name}：vault 已锁定（{} 或 creds.key 不可用）",
                self.path.display()
            ))),
        }
    }

    /// 删除秘密（两边都删，幂等）
    pub fn delete(&self, service: &str, account: &str) -> Result<bool> {
        let key_name = format!("{service}/{account}");
        let mut removed = false;

        if self.keyring_ok.load(Ordering::Relaxed)
            && let Ok(entry) = keyring::Entry::new(service, account)
        {
            match entry.delete_credential() {
                Ok(()) => removed = true,
                Err(keyring::Error::NoEntry) => {}
                Err(e) => {
                    tracing::info!("钥匙串删除失败（{e}）");
                    self.keyring_ok.store(false, Ordering::Relaxed);
                }
            }
        }

        let mut state = self.state.lock().unwrap();
        if let VaultState::Loaded { key, entries } = &mut *state
            && entries.contains_key(&key_name)
        {
            let mut next = entries.clone();
            next.remove(&key_name);
            save_vault(&self.path, key, &next)?;
            *entries = next;
            removed = true;
        }
        Ok(removed)
    }

    /// 列出 vault 中的条目名（不含值）。钥匙串无法枚举，只列 vault。
    pub fn list(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        match &*state {
            VaultState::Loaded { entries, .. } => entries.keys().cloned().collect(),
            VaultState::Locked => Vec::new(),
        }
    }

    /// 当前后端："keyring" / "vault" / "none"
    pub fn backend(&self) -> &'static str {
        *self.backend.lock().unwrap()
    }

    /// 后端可用性快照（TUI 用来提示「密码将存在哪里」）
    pub fn status(&self) -> CredentialStatus {
        CredentialStatus {
            keyring_available: self.keyring_ok.load(Ordering::Relaxed),
            vault_ready: !self.is_locked(),
            backend: self.backend(),
        }
    }

    /// vault 是否处于锁定/不可写状态（排障用）
    pub fn is_locked(&self) -> bool {
        matches!(&*self.state.lock().unwrap(), VaultState::Locked)
    }
}

// ---------------------------------------------------------------------------
// data-key 与 v2 vault
// ---------------------------------------------------------------------------

/// 读取 data-key；文件不存在返回 None（不新建）。
fn read_data_key(path: &Path) -> Result<Option<[u8; KEY_LEN]>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if bytes.len() != KEY_LEN {
                return Err(StoreError::Credential(format!(
                    "{} 长度错误（期望 {KEY_LEN} 字节，实际 {}）",
                    path.display(),
                    bytes.len()
                )));
            }
            let mut key = [0u8; KEY_LEN];
            key.copy_from_slice(&bytes);
            Ok(Some(key))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::Credential(format!(
            "读取 {} 失败: {e}",
            path.display()
        ))),
    }
}

/// 读取 data-key；不存在则生成并以 0600 写入。
fn ensure_data_key(path: &Path) -> Result<[u8; KEY_LEN]> {
    for _ in 0..4 {
        match std::fs::read(path) {
            Ok(bytes) => {
                if bytes.len() != KEY_LEN {
                    return Err(StoreError::Credential(format!(
                        "{} 长度错误（期望 {KEY_LEN} 字节，实际 {}）",
                        path.display(),
                        bytes.len()
                    )));
                }
                let mut key = [0u8; KEY_LEN];
                key.copy_from_slice(&bytes);
                return Ok(key);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut key = [0u8; KEY_LEN];
                fill_random(&mut key);
                match write_private_key(path, &key) {
                    Ok(()) => return Ok(key),
                    // 并发下别人先创建了：回到循环重读，而不是覆盖
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => {
                        return Err(StoreError::Credential(format!(
                            "创建 {} 失败: {e}",
                            path.display()
                        )));
                    }
                }
            }
            Err(e) => {
                return Err(StoreError::Credential(format!(
                    "读取 {} 失败: {e}",
                    path.display()
                )));
            }
        }
    }
    Err(StoreError::Credential(format!(
        "无法创建 {}（并发冲突）",
        path.display()
    )))
}

/// 以「仅当前用户可读写」创建 data-key。
#[cfg(unix)]
fn write_private_key(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(not(unix))]
fn write_private_key(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn vault_version(path: &Path) -> Result<u32> {
    let raw = std::fs::read_to_string(path)?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| StoreError::Credential(format!("vault 损坏: {e}")))?;
    Ok(v.get("version")
        .and_then(|x| x.as_u64())
        .unwrap_or(1)
        .try_into()
        .unwrap_or(1))
}

fn load_vault_v2(path: &Path, key: &[u8; KEY_LEN]) -> Result<Entries> {
    let raw = std::fs::read_to_string(path)?;
    let vf: VaultFileV2 =
        serde_json::from_str(&raw).map_err(|e| StoreError::Credential(format!("vault 损坏: {e}")))?;
    let nonce = unhex(&vf.nonce)?;
    let ct = unhex(&vf.data)?;
    let plain = decrypt(key, &nonce, &ct)?;
    serde_json::from_slice(&plain)
        .map_err(|e| StoreError::Credential(format!("vault 内容损坏: {e}")))
}

fn save_vault(path: &Path, key: &[u8; KEY_LEN], entries: &Entries) -> Result<()> {
    let plain =
        serde_json::to_vec(entries).map_err(|e| StoreError::Credential(format!("序列化失败: {e}")))?;
    let mut nonce = [0u8; 12];
    fill_random(&mut nonce);
    let ct = encrypt(key, &nonce, &plain)?;

    let vf = VaultFileV2 {
        version: 2,
        nonce: hex(&nonce),
        data: hex(&ct),
    };
    let json = serde_json::to_string_pretty(&vf)
        .map_err(|e| StoreError::Credential(format!("序列化失败: {e}")))?;
    // 先写临时文件再改名：写到一半断电不该留下一个坏 vault。
    // 临时文件也按 0600 创建，避免密文在短暂窗口里被其他用户读到。
    let tmp = path.with_extension("vault.tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// v1 → v2 迁移
// ---------------------------------------------------------------------------

fn migrate_v1(path: &Path, data_key: &[u8; KEY_LEN]) -> Result<VaultState> {
    let master = std::env::var(MASTER_PASSWORD_ENV)
        .ok()
        .filter(|v| !v.is_empty());

    let Some(master) = master else {
        tracing::warn!(
            "检测到旧版主密码 vault，但未设置 {MASTER_PASSWORD_ENV}；\
             已备份为 .legacy 并新建空库（旧凭据需原主密码才能恢复）"
        );
        backup_legacy(path)?;
        return Ok(VaultState::Loaded {
            key: *data_key,
            entries: Entries::new(),
        });
    };

    match load_vault_v1(path, &master) {
        Ok(entries) => {
            save_vault(path, data_key, &entries)?;
            tracing::info!("已把旧版主密码 vault 迁移为本地 data-key 格式");
            Ok(VaultState::Loaded {
                key: *data_key,
                entries,
            })
        }
        Err(e) => {
            tracing::warn!("旧 vault 解密失败（主密码错误？）: {e}；已备份并新建空库");
            backup_legacy(path)?;
            Ok(VaultState::Loaded {
                key: *data_key,
                entries: Entries::new(),
            })
        }
    }
}

fn load_vault_v1(path: &Path, master: &str) -> Result<Entries> {
    let raw = std::fs::read_to_string(path)?;
    let vf: VaultFileV1 =
        serde_json::from_str(&raw).map_err(|e| StoreError::Credential(format!("vault 损坏: {e}")))?;
    let salt = unhex(&vf.salt)?;
    let nonce = unhex(&vf.nonce)?;
    let ct = unhex(&vf.data)?;
    let key = derive_key(master, &salt, vf.m_cost, vf.t_cost, vf.p_cost)?;
    let plain = decrypt(&key, &nonce, &ct)?;
    serde_json::from_slice(&plain)
        .map_err(|e| StoreError::Credential(format!("vault 内容损坏: {e}")))
}

/// 启动时发现「已备份的旧库」且当前设置了主密码：自动恢复并转成 v2。
///
/// 覆盖这个场景：升级后第一次启动没设 `SH0_MASTER_PASSWORD`，旧库被备份成
/// `creds.vault.legacy`；之后用户设好主密码重启，应该能把它救回来。
fn try_migrate_legacy(
    path: &Path,
    key_path: &Path,
) -> Result<Option<([u8; KEY_LEN], Entries)>> {
    let legacy = path.with_extension("vault.legacy");
    if !legacy.exists() {
        return Ok(None);
    }
    let Some(master) = std::env::var(MASTER_PASSWORD_ENV)
        .ok()
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    let key = ensure_data_key(key_path)?;
    match load_vault_v1(&legacy, &master) {
        Ok(entries) => {
            save_vault(path, &key, &entries)?;
            let _ = std::fs::remove_file(&legacy);
            tracing::info!(
                "已从 {} 恢复旧凭据并重加密为 data-key 格式",
                legacy.display()
            );
            Ok(Some((key, entries)))
        }
        Err(e) => {
            tracing::warn!("旧凭据库 {} 解码失败（主密码错误？）: {e}", legacy.display());
            Ok(None)
        }
    }
}

fn backup_legacy(path: &Path) -> Result<()> {
    let mut dest = path.with_extension("vault.legacy");
    if dest.exists() {
        let ts = chrono::Utc::now().format("%Y%m%d%H%M%S");
        dest = path.with_extension(format!("vault.legacy.{ts}"));
    }
    std::fs::rename(path, &dest)?;
    tracing::warn!("旧凭据库已备份到 {}", dest.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// 加解密
// ---------------------------------------------------------------------------

fn derive_key(password: &str, salt: &[u8], m_cost: u32, t_cost: u32, p_cost: u32) -> Result<[u8; KEY_LEN]> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(m_cost, t_cost, p_cost, Some(KEY_LEN))
        .map_err(|e| StoreError::Credential(format!("Argon2 参数错误: {e}")))?;
    let a = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    a.hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| StoreError::Credential(format!("密钥派生失败: {e}")))?;
    Ok(key)
}

fn encrypt(key: &[u8; KEY_LEN], nonce: &[u8; 12], plaintext: &[u8]) -> Result<Vec<u8>> {
    use chacha20poly1305::Nonce;
    use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, aead::Aead};
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .encrypt(Nonce::from_slice(nonce), plaintext)
        .map_err(|e| StoreError::Credential(format!("加密失败: {e}")))
}

fn decrypt(key: &[u8; KEY_LEN], nonce: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
    use chacha20poly1305::Nonce;
    use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, aead::Aead};
    if nonce.len() != 12 {
        return Err(StoreError::Credential("vault nonce 长度错误".into()));
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| StoreError::Credential("解密失败：密钥/主密码错误或 vault 已损坏".into()))
}

fn fill_random(buf: &mut [u8]) {
    // getrandom 失败意味着系统随机源不可用——此时宁可崩溃也不要静默用确定性值
    getrandom::getrandom(buf).expect("系统随机源不可用");
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(StoreError::Credential("hex 长度错误".into()));
    }
    (0..s.len() / 2)
        .map(|i| {
            u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|_| StoreError::Credential("hex 含非法字符".into()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// 环境变量是进程级的；所有会改 env 的用例串行，避免互相踩。
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn temp_paths(tag: &str) -> (PathBuf, Paths) {
        let dir = std::env::temp_dir().join(format!(
            "sh0-creds-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let paths = Paths {
            config_dir: dir.join("config"),
            data_dir: dir.join("data"),
            runtime_dir: dir.join("run"),
        };
        (dir, paths)
    }

    fn force_vault_backend() {
        // SAFETY: 调用方持有 env_lock()
        unsafe {
            std::env::set_var(DISABLE_KEYRING_ENV, "1");
            std::env::remove_var(MASTER_PASSWORD_ENV);
        }
    }

    #[test]
    fn vault_roundtrip_uses_data_key() {
        let _g = env_lock();
        force_vault_backend();
        let (dir, paths) = temp_paths("roundtrip");

        let store = CredentialStore::open(&paths).unwrap();
        store.set("sh0", "web-01", "s3cr3t").unwrap();
        assert_eq!(store.get("sh0", "web-01").unwrap().as_deref(), Some("s3cr3t"));
        assert!(store.list().contains(&"sh0/web-01".to_string()));
        assert_eq!(store.backend(), "vault");

        // 重开（模拟 daemon 重启）：data-key 不变，条目应仍在
        let reopened = CredentialStore::open(&paths).unwrap();
        assert_eq!(
            reopened.get("sh0", "web-01").unwrap().as_deref(),
            Some("s3cr3t")
        );

        // 落盘的是密文，不能 grep 到明文
        let vault = std::fs::read_to_string(paths.vault_file()).unwrap();
        assert!(!vault.contains("s3cr3t"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&vault)
                .unwrap()
                .get("version")
                .and_then(|v| v.as_u64()),
            Some(2)
        );

        // data-key 是 32 字节、0600
        let key = std::fs::read(paths.cred_key()).unwrap();
        assert_eq!(key.len(), KEY_LEN);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.cred_key())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "creds.key 必须是 0600");

            let vault_mode = std::fs::metadata(paths.vault_file())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(vault_mode & 0o777, 0o600, "creds.vault 必须是 0600");
        }

        // 删除后查不到
        assert!(reopened.delete("sh0", "web-01").unwrap());
        assert_eq!(reopened.get("sh0", "web-01").unwrap(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_vault_is_locked() {
        let _g = env_lock();
        force_vault_backend();
        let (dir, paths) = temp_paths("tamper");

        let store = CredentialStore::open(&paths).unwrap();
        store.set("sh0", "web-01", "s3cr3t").unwrap();

        let raw = std::fs::read_to_string(paths.vault_file()).unwrap();
        let mut vf: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let data = vf.get("data").and_then(|v| v.as_str()).unwrap().to_string();
        let mut flipped = data.clone();
        flipped.replace_range(0..2, if &data[0..2] == "00" { "01" } else { "00" });
        vf.as_object_mut()
            .unwrap()
            .insert("data".into(), serde_json::json!(flipped));
        std::fs::write(paths.vault_file(), serde_json::to_string(&vf).unwrap()).unwrap();

        let locked = CredentialStore::open(&paths).unwrap();
        assert!(locked.is_locked());
        assert!(locked.get("sh0", "web-01").is_err());
        assert!(locked.set("sh0", "web-01", "x").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_v1_migrates_with_master_password() {
        let _g = env_lock();
        force_vault_backend();
        let (dir, paths) = temp_paths("migrate");
        write_legacy_vault(&paths, "old-master", &[("sh0/web-01", "legacy-pw")]);

        // SAFETY: 持有 env_lock()
        unsafe { std::env::set_var(MASTER_PASSWORD_ENV, "old-master") };
        let store = CredentialStore::open(&paths).unwrap();
        assert_eq!(
            store.get("sh0", "web-01").unwrap().as_deref(),
            Some("legacy-pw")
        );

        // 已改写成 v2，且不再依赖主密码
        let v = vault_version(&paths.vault_file()).unwrap();
        assert_eq!(v, 2);
        unsafe { std::env::remove_var(MASTER_PASSWORD_ENV) };
        let reopened = CredentialStore::open(&paths).unwrap();
        assert_eq!(
            reopened.get("sh0", "web-01").unwrap().as_deref(),
            Some("legacy-pw")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_v1_without_master_is_backed_up() {
        let _g = env_lock();
        force_vault_backend();
        let (dir, paths) = temp_paths("legacy");
        write_legacy_vault(&paths, "old-master", &[("sh0/web-01", "legacy-pw")]);

        let store = CredentialStore::open(&paths).unwrap();
        assert_eq!(store.get("sh0", "web-01").unwrap(), None);
        assert!(!paths.vault_file().exists());
        assert!(paths.vault_file().with_extension("vault.legacy").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_backup_restores_when_master_is_set_later() {
        let _g = env_lock();
        force_vault_backend();
        let (dir, paths) = temp_paths("legacy-restore");
        write_legacy_vault(&paths, "old-master", &[("sh0/web-01", "legacy-pw")]);

        // 第一次启动没设主密码：备份为空库
        let empty = CredentialStore::open(&paths).unwrap();
        assert_eq!(empty.get("sh0", "web-01").unwrap(), None);
        let legacy = paths.vault_file().with_extension("vault.legacy");
        assert!(legacy.exists());

        // 之后设好主密码重启：自动从 .legacy 恢复
        unsafe { std::env::set_var(MASTER_PASSWORD_ENV, "old-master") };
        let restored = CredentialStore::open(&paths).unwrap();
        assert_eq!(
            restored.get("sh0", "web-01").unwrap().as_deref(),
            Some("legacy-pw")
        );
        assert!(!legacy.exists(), "恢复后应清掉 .legacy");
        assert_eq!(vault_version(&paths.vault_file()).unwrap(), 2);
        unsafe { std::env::remove_var(MASTER_PASSWORD_ENV) };

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert_eq!(unhex("000fff").unwrap(), vec![0, 15, 255]);
        assert!(unhex("0f0").is_err());
    }

    /// 造一个 v1（主密码 + Argon2id）vault，用于迁移测试
    fn write_legacy_vault(paths: &Paths, master: &str, entries: &[(&str, &str)]) {
        let map: Entries = entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut salt = [0u8; 16];
        fill_random(&mut salt);
        let key = derive_key(master, &salt, ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST).unwrap();
        let mut nonce = [0u8; 12];
        fill_random(&mut nonce);
        let plain = serde_json::to_vec(&map).unwrap();
        let ct = encrypt(&key, &nonce, &plain).unwrap();
        let vf = serde_json::json!({
            "version": 1,
            "salt": hex(&salt),
            "m_cost": ARGON2_M_COST,
            "t_cost": ARGON2_T_COST,
            "p_cost": ARGON2_P_COST,
            "nonce": hex(&nonce),
            "data": hex(&ct),
        });
        std::fs::write(
            paths.vault_file(),
            serde_json::to_string_pretty(&vf).unwrap(),
        )
        .unwrap();
    }
}
