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
//!        └─▶ creds.vault：Argon2id(主密码) 派生密钥 +
//!             ChaCha20-Poly1305 加密的 JSON（无 dbus 的 Linux 等场景）
//! ```
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

/// 主密码环境变量（无人值守场景）
pub const MASTER_PASSWORD_ENV: &str = "SH0_MASTER_PASSWORD";

/// 设为任意非空值可禁用钥匙串后端（排障 / 测试用）
pub const DISABLE_KEYRING_ENV: &str = "SH0_DISABLE_KEYRING";

/// Argon2id 参数：OWASP 推荐基线（19 MiB / t=2 / p=1）
const ARGON2_M_COST: u32 = 19 * 1024;
const ARGON2_T_COST: u32 = 2;
const ARGON2_P_COST: u32 = 1;
const KEY_LEN: usize = 32;

/// vault 文件的 JSON 结构
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaultFile {
    version: u32,
    /// hex 编码的 salt / nonce / 密文
    salt: String,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    nonce: String,
    data: String,
}

/// 解密后的条目表：key = "service/account"
type Entries = BTreeMap<String, String>;

enum VaultState {
    /// 已解密（或首次创建的空表）
    Loaded {
        key: [u8; KEY_LEN],
        salt: Vec<u8>,
        entries: Entries,
    },
    /// vault 文件存在但拿不到主密码
    Locked,
    /// 没有 vault 文件也没有主密码：只读空表，写入报错
    Empty,
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
    /// 主密码来源：`SH0_MASTER_PASSWORD` 环境变量优先，否则尝试 stdin 提示
    /// （daemon 后台场景 stdin 为 null，此时落到 `Empty`/`Locked`）。
    pub fn open(paths: &Paths) -> Result<Self> {
        let path = paths.vault_file();
        // 目录可能还没建（首次运行、或测试里直接给 Paths）：不建的话首次 set 会
        // 撞上 NotFound——那是个"打开成功、写入才炸"的坏失败模式。
        if let Some(dir) = path.parent()
            && !dir.exists()
        {
            std::fs::create_dir_all(dir)
                .map_err(|e| StoreError::Credential(format!("创建凭据目录失败: {e}")))?;
        }
        let master = master_password_from_env_or_prompt();

        let state = if path.exists() {
            match master {
                Some(pw) => match load_vault(&path, &pw) {
                    Ok((key, salt, entries)) => VaultState::Loaded { key, salt, entries },
                    Err(e) => {
                        tracing::warn!("vault 解密失败（主密码错误？）: {e}");
                        VaultState::Locked
                    }
                },
                None => VaultState::Locked,
            }
        } else {
            match master {
                Some(pw) => {
                    let mut salt = [0u8; 16];
                    fill_random(&mut salt);
                    let key = derive_key(&pw, &salt)?;
                    VaultState::Loaded {
                        key,
                        salt: salt.to_vec(),
                        entries: Entries::new(),
                    }
                }
                None => VaultState::Empty,
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
        let (key, salt, entries) = match &mut *state {
            VaultState::Loaded { key, salt, entries } => (key, salt, entries),
            VaultState::Locked | VaultState::Empty => {
                return Err(StoreError::Credential(format!(
                    "无法写入 {key_name}：需要主密码（设置环境变量 {MASTER_PASSWORD_ENV}）"
                )));
            }
        };
        entries.insert(key_name.clone(), secret.to_string());
        save_vault(&self.path, key, salt, entries)?;
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
                "无法读取 {key_name}：vault 已锁定（设置环境变量 {MASTER_PASSWORD_ENV}）"
            ))),
            VaultState::Empty => Ok(None),
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
        if let VaultState::Loaded { key, salt, entries } = &mut *state
            && entries.remove(&key_name).is_some()
        {
            save_vault(&self.path, key, salt, entries)?;
            removed = true;
        }
        Ok(removed)
    }

    /// 列出 vault 中的条目名（不含值）。钥匙串无法枚举，只列 vault。
    pub fn list(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        match &*state {
            VaultState::Loaded { entries, .. } => entries.keys().cloned().collect(),
            _ => Vec::new(),
        }
    }

    /// 当前后端："keyring" / "vault" / "none"
    pub fn backend(&self) -> &'static str {
        *self.backend.lock().unwrap()
    }

    /// vault 是否处于锁定/不可写状态（排障用）
    pub fn is_locked(&self) -> bool {
        matches!(&*self.state.lock().unwrap(), VaultState::Locked)
    }
}

// ---------------------------------------------------------------------------
// 主密码与加解密
// ---------------------------------------------------------------------------

fn master_password_from_env_or_prompt() -> Option<String> {
    if let Ok(v) = std::env::var(MASTER_PASSWORD_ENV)
        && !v.is_empty()
    {
        return Some(v);
    }
    // stdin 提示：交互式场景（`sh0 secret set`）用；daemon 后台 stdin 为 null，
    // read_line 直接失败，安静落到 None。
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return None;
    }
    eprint!("vault 主密码（设 {MASTER_PASSWORD_ENV} 可免交互）: ");
    use std::io::Write;
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(_) => {
            let pw = line.trim_end().to_string();
            if pw.is_empty() { None } else { Some(pw) }
        }
        Err(_) => None,
    }
}

fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST, Some(KEY_LEN))
        .map_err(|e| StoreError::Credential(format!("Argon2 参数错误: {e}")))?;
    let a = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    a.hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| StoreError::Credential(format!("密钥派生失败: {e}")))?;
    Ok(key)
}

fn load_vault(path: &Path, password: &str) -> Result<([u8; KEY_LEN], Vec<u8>, Entries)> {
    let raw = std::fs::read_to_string(path)?;
    let vf: VaultFile =
        serde_json::from_str(&raw).map_err(|e| StoreError::Credential(format!("vault 损坏: {e}")))?;
    let salt = unhex(&vf.salt)?;
    let nonce = unhex(&vf.nonce)?;
    let ct = unhex(&vf.data)?;
    let key = derive_key(password, &salt)?;

    let plain = decrypt(&key, &nonce, &ct)?;
    let entries: Entries = serde_json::from_slice(&plain)
        .map_err(|e| StoreError::Credential(format!("vault 内容损坏: {e}")))?;
    Ok((key, salt, entries))
}

fn save_vault(
    path: &Path,
    key: &[u8; KEY_LEN],
    salt: &[u8],
    entries: &Entries,
) -> Result<()> {
    let plain = serde_json::to_vec(entries)
        .map_err(|e| StoreError::Credential(format!("序列化失败: {e}")))?;
    let mut nonce = [0u8; 12];
    fill_random(&mut nonce);
    let ct = encrypt(key, &nonce, &plain)?;

    let vf = VaultFile {
        version: 1,
        salt: hex(salt),
        m_cost: ARGON2_M_COST,
        t_cost: ARGON2_T_COST,
        p_cost: ARGON2_P_COST,
        nonce: hex(&nonce),
        data: hex(&ct),
    };
    let json = serde_json::to_string_pretty(&vf)
        .map_err(|e| StoreError::Credential(format!("序列化失败: {e}")))?;
    // 先写临时文件再改名：写到一半断电不该留下一个坏 vault
    let tmp = path.with_extension("vault.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn encrypt(key: &[u8; KEY_LEN], nonce: &[u8; 12], plaintext: &[u8]) -> Result<Vec<u8>> {
    use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, aead::Aead};
    use chacha20poly1305::Nonce;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .encrypt(Nonce::from_slice(nonce), plaintext)
        .map_err(|e| StoreError::Credential(format!("加密失败: {e}")))
}

fn decrypt(key: &[u8; KEY_LEN], nonce: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
    use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, aead::Aead};
    use chacha20poly1305::Nonce;
    if nonce.len() != 12 {
        return Err(StoreError::Credential("vault nonce 长度错误".into()));
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| StoreError::Credential("主密码错误或 vault 已损坏".into()))
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

    fn temp_paths(dir: &Path) -> Paths {
        Paths {
            config_dir: dir.join("config"),
            data_dir: dir.join("data"),
            runtime_dir: dir.join("run"),
        }
    }

    #[test]
    fn vault_roundtrip_and_tamper_detection() {
        let dir = std::env::temp_dir().join(format!("sh0-creds-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = temp_paths(&dir);
        std::fs::create_dir_all(&paths.data_dir).unwrap();

        // 固定走 vault 路径：钥匙串的有无取决于运行环境，
        // 测试必须确定性地验证"加密文件"这条链路本身
        unsafe {
            std::env::set_var(DISABLE_KEYRING_ENV, "1");
            std::env::set_var(MASTER_PASSWORD_ENV, "test-master-pw");
        }
        let store = CredentialStore::open(&paths).unwrap();
        store.set("sh0", "web-01", "s3cr3t").unwrap();
        let got = store.get("sh0", "web-01").unwrap();
        assert_eq!(got.as_deref(), Some("s3cr3t"));
        assert!(store.list().contains(&"sh0/web-01".to_string()));
        assert_eq!(store.backend(), "vault");

        // 重开（模拟 daemon 重启）：主密码相同，条目应仍在
        let store_reopened = CredentialStore::open(&paths).unwrap();
        assert_eq!(
            store_reopened.get("sh0", "web-01").unwrap().as_deref(),
            Some("s3cr3t")
        );

        // 篡改密文 → 解密必须失败（而不是返回乱码）
        let raw = std::fs::read_to_string(paths.vault_file()).unwrap();
        let mut vf: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let data = vf.get("data").and_then(|v| v.as_str()).unwrap().to_string();
        let mut flipped = data.clone();
        flipped.replace_range(0..2, if &data[0..2] == "00" { "01" } else { "00" });
        vf.as_object_mut()
            .unwrap()
            .insert("data".into(), serde_json::json!(flipped));
        std::fs::write(paths.vault_file(), serde_json::to_string(&vf).unwrap()).unwrap();

        let store2 = CredentialStore::open(&paths).unwrap();
        assert!(store2.is_locked());

        // 错误主密码 → 同样锁住，不会 panic
        unsafe {
            std::env::set_var(MASTER_PASSWORD_ENV, "wrong-password");
        }
        let store3 = CredentialStore::open(&paths).unwrap();
        assert!(store3.is_locked());
        assert!(store3.get("sh0", "web-01").is_err());

        // 删除后查不到
        unsafe {
            std::env::set_var(MASTER_PASSWORD_ENV, "test-master-pw");
        }
        let store4 = CredentialStore::open(&paths).unwrap();
        // 篡改过的 vault 打不开，重新写一个
        let _ = std::fs::remove_file(paths.vault_file());
        let store5 = CredentialStore::open(&paths).unwrap();
        store5.set("sh0", "web-01", "s3cr3t").unwrap();
        assert!(store5.delete("sh0", "web-01").unwrap());
        assert_eq!(store5.get("sh0", "web-01").unwrap(), None);
        let _ = store4;

        unsafe {
            std::env::remove_var(MASTER_PASSWORD_ENV);
            std::env::remove_var(DISABLE_KEYRING_ENV);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
        assert_eq!(unhex("000fff").unwrap(), vec![0, 15, 255]);
        assert!(unhex("0f0").is_err());
    }
}
