//! `sh0 config` 的共用逻辑（FR-9.3 / FR-9.4）。
//!
//! 单独拎出来是因为 main.rs 只该做命令行粘合，而"通知 daemon 热重载"
//! 需要 IPC 连接，放这里也方便契约测试直接调。

use std::sync::Arc;

use serde_json::Value;
use sh0_ipc::protocol::methods;

use sh0_store::Store;

/// 把配置值打印成人类可读的一行：字符串不加引号，其余用 JSON 原文。
///
/// `sh0 config get term` 应该输出 `xterm-256color` 而不是 `"xterm-256color"`，
/// 前者才能直接被 `$(...)` 或脚本用。
pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 通知运行中的 daemon 重新加载配置。daemon 没跑返回 None——
/// 这不是错误：配置改在文件里了，下次启动自然会读到。
pub async fn notify_daemon(store: &Arc<Store>) -> Option<String> {
    let paths = store.paths().clone();
    let info = crate::ensure::read_info(&paths)?;
    let conn = match sh0_ipc::IpcClient::connect(&info.addr, &info.token).await {
        Ok(c) => c,
        Err(_) => return None,
    };
    let client = conn.client;
    match client.call(methods::CONFIG_RELOAD, serde_json::json!({})).await {
        Ok(res) => {
            let changed = res.get("changed").and_then(|v| v.as_bool()).unwrap_or(false);
            let note = res
                .get("note")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Some(if changed {
                format!("daemon 已重载：{note}")
            } else {
                format!("daemon 已检查：{note}")
            })
        }
        Err(e) => Some(format!("通知 daemon 失败：{e}")),
    }
}
