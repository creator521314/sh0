//! daemon 的发现与自动拉起。
//!
//! 前端（TUI / `sh0 ai`）不自己持有会话，一律连到 daemon。若 daemon 未运行则
//! 后台拉起一个，等它写好 daemon.json 并完成握手后再返回。

use std::process::Stdio;
use std::time::Duration;

use sh0_ipc::{IpcClient, IpcConnection};
use sh0_ipc::protocol::DaemonInfo;
use sh0_store::Paths;

/// 连接已有 daemon；不可用则拉起一个新的。
///
/// `log_path` 为 Some 时，后台 daemon 的 stdout/stderr 会写到该文件——
/// 后台进程没有控制台，不落盘的话启动失败将完全无从排查。
pub async fn connect_or_start(
    paths: &Paths,
    log_path: Option<&std::path::Path>,
) -> anyhow::Result<(IpcConnection, DaemonInfo)> {
    connect_or_start_inner(paths, log_path).await
}

async fn connect_or_start_inner(
    paths: &Paths,
    log_path: Option<&std::path::Path>,
) -> anyhow::Result<(IpcConnection, DaemonInfo)> {
    if let Some(info) = read_info(paths) {
        if let Ok(conn) = IpcClient::connect(&info.addr, &info.token).await {
            return Ok((conn, info));
        }
        // 信息陈旧：清掉后重新拉起
        tracing::debug!("daemon.json 已失效，清理后重启 daemon");
        let _ = std::fs::remove_file(paths.daemon_info());
        let _ = std::fs::remove_file(paths.pid_file());
    }

    spawn_daemon(log_path)?;
    wait_for_daemon(paths).await
}

pub fn read_info(paths: &Paths) -> Option<DaemonInfo> {
    let raw = std::fs::read_to_string(paths.daemon_info()).ok()?;
    serde_json::from_str::<DaemonInfo>(&raw).ok()
}

fn spawn_daemon(log_path: Option<&std::path::Path>) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon").arg("run").arg("--background");

    // 关键：必须断开 stdio 继承。否则后台 daemon 会一直持有父进程的 stdout 管道，
    // 使 `$(sh0 ai ...)` 这类脚本化调用永远等不到 EOF 而挂死（M0 踩过）。
    //
    // M1 改为落到 runtime_dir/daemon.log：后台进程没有控制台，
    // 一旦启动失败或运行期报错，没有日志就完全无从排查。
    cmd.stdin(Stdio::null());
    let log_file = match log_path {
        Some(p) => match std::fs::File::create(p) {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::warn!("无法创建 daemon 日志 {}: {}", p.display(), e);
                None
            }
        },
        None => None,
    };
    if let Some(f) = &log_file {
        let dup = f.try_clone()?;
        cmd.stdout(dup);
        let dup2 = f.try_clone()?;
        cmd.stderr(dup2);
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }

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
