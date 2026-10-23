//! daemon 的发现与自动拉起。
//!
//! 前端（TUI / `sh0 ai`）不自己持有会话，一律连到 daemon。若 daemon 未运行则
//! 后台拉起一个，等它写好 daemon.json 并完成握手后再返回。

use std::process::Stdio;
use std::time::Duration;

use sh0_ipc::{IpcClient, IpcConnection};
use sh0_ipc::protocol::DaemonInfo;
use sh0_store::Paths;

/// 连接已有 daemon；不可用则拉起一个新的
pub async fn connect_or_start(paths: &Paths) -> anyhow::Result<(IpcConnection, DaemonInfo)> {
    if let Some(info) = read_info(paths) {
        if let Ok(conn) = IpcClient::connect(&info.addr, &info.token).await {
            return Ok((conn, info));
        }
        // 信息陈旧：清掉后重新拉起
        tracing::debug!("daemon.json 已失效，清理后重启 daemon");
        let _ = std::fs::remove_file(paths.daemon_info());
        let _ = std::fs::remove_file(paths.pid_file());
    }

    spawn_daemon()?;
    wait_for_daemon(paths).await
}

pub fn read_info(paths: &Paths) -> Option<DaemonInfo> {
    let raw = std::fs::read_to_string(paths.daemon_info()).ok()?;
    serde_json::from_str::<DaemonInfo>(&raw).ok()
}

fn spawn_daemon() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon").arg("run").arg("--background");

    // 关键：必须断开 stdio 继承。否则后台 daemon 会一直持有父进程的 stdout 管道，
    // 使 `$(sh0 ai ...)` 这类脚本化调用永远等不到 EOF 而挂死。
    // M1 会把 daemon 日志改为写入 runtime_dir/daemon.log。
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：后台运行时不弹控制台窗口
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd.spawn()?;
    Ok(())
}

/// 轮询等待 daemon 就绪，最多 15 秒
async fn wait_for_daemon(paths: &Paths) -> anyhow::Result<(IpcConnection, DaemonInfo)> {
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(info) = read_info(paths)
            && let Ok(conn) = IpcClient::connect(&info.addr, &info.token).await {
                return Ok((conn, info));
            }
    }
    anyhow::bail!("等待 daemon 启动超时，可手动执行 `sh0 daemon run` 查看日志")
}

/// 关闭正在运行的 daemon；未运行时返回 false
pub async fn stop_daemon(paths: &Paths) -> anyhow::Result<bool> {
    let Some(info) = read_info(paths) else {
        return Ok(false);
    };
    let conn = match IpcClient::connect(&info.addr, &info.token).await {
        Ok(c) => c,
        Err(_) => {
            // 陈旧记录：清掉，避免下次误判
            let _ = std::fs::remove_file(paths.daemon_info());
            let _ = std::fs::remove_file(paths.pid_file());
            return Ok(false);
        }
    };
    conn.client
        .call(sh0_ipc::protocol::methods::SHUTDOWN, serde_json::json!({}))
        .await?;
    Ok(true)
}
