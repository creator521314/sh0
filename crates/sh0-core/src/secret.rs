//! 秘密解析 trait（W2.12）。
//!
//! `SecretRef` 只描述"去哪里取秘密"，真正的取值逻辑在这里：
//! 环境变量 / 交互式提示 / OS 钥匙串 / 加密 vault。
//!
//! 为什么是 trait 而不是直接调 `sh0-store`：`sh0-ssh` 不能依赖 `sh0-store`
//! （依赖方向约束），而 daemon 两边都依赖——由它把实现注入进来。

use crate::error::Result;
use crate::model::SecretRef;

/// 把 `SecretRef` 解析成实际值。
///
/// 实现方负责：环境变量读取、stdin 提示、钥匙串/加密库查询。
/// **绝不**把解析出的值写进日志或返回值以外的任何地方。
pub trait SecretResolver: Send + Sync {
    fn resolve(&self, r: &SecretRef, prompt: &str) -> Result<String>;
    /// 后端名称，用于 `host show` 与审计（"keyring" / "vault" / "env" / "prompt"）
    fn backend_name(&self) -> &'static str;
}

/// 只支持环境变量与 stdin 提示的兜底实现（无 vault / 无钥匙串时）。
pub struct DefaultResolver;

impl SecretResolver for DefaultResolver {
    fn resolve(&self, r: &SecretRef, prompt: &str) -> Result<String> {
        match r {
            SecretRef::Env(name) => std::env::var(name).map_err(|_| {
                crate::error::CoreError::Credential(format!("环境变量 {name} 未设置"))
            }),
            SecretRef::Prompt => {
                use std::io::Write;
                eprint!("{prompt}: ");
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                std::io::stdin()
                    .read_line(&mut line)
                    .map_err(crate::error::CoreError::Io)?;
                Ok(line.trim_end().to_string())
            }
            SecretRef::Keyring { .. } => Err(crate::error::CoreError::NotImplemented(
                "未配置凭据存储后端（钥匙串/vault 均不可用）".into(),
            )),
        }
    }

    fn backend_name(&self) -> &'static str {
        "default"
    }
}
