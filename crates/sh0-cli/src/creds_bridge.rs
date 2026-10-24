//! 凭据存储 → SSH 层的桥接（W2.12）。
//!
//! `sh0-ssh` 不能依赖 `sh0-store`（依赖方向约束），于是把
//! `sh0_core::SecretResolver` 的实现在这里补齐——daemon 装配时
//! 注入给 `RusshFactory`，认证时按需取秘密。

use std::sync::Arc;

use sh0_core::error::Result;
use sh0_core::model::SecretRef;
use sh0_core::SecretResolver;
use sh0_store::CredentialStore;

pub struct VaultResolver {
    pub store: Arc<CredentialStore>,
}

impl SecretResolver for VaultResolver {
    fn resolve(&self, r: &SecretRef, prompt: &str) -> Result<String> {
        match r {
            SecretRef::Env(name) => std::env::var(name).map_err(|_| {
                sh0_core::CoreError::Credential(format!("环境变量 {name} 未设置"))
            }),
            SecretRef::Prompt => {
                use std::io::Write;
                eprint!("{prompt}: ");
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                std::io::stdin()
                    .read_line(&mut line)
                    .map_err(sh0_core::CoreError::Io)?;
                Ok(line.trim_end().to_string())
            }
            SecretRef::Keyring { service, account } => self
                .store
                .get(service, account)
                .map_err(|e| sh0_core::CoreError::Credential(e.to_string()))?
                .ok_or_else(|| {
                    sh0_core::CoreError::Credential(format!(
                        "钥匙串/vault 中找不到 {service}/{account}（可用 `sh0 secret set {service} {account}` 存入）"
                    ))
                }),
        }
    }

    fn backend_name(&self) -> &'static str {
        self.store.backend()
    }
}
