//! 顶层命令面的冒烟测试（M6）：核心入口 tui / ai / help，daemon 作为运维命令暴露。
//!
//! 不启动 daemon、不连网：只验证参数解析与帮助文本。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sh0-cli-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sh0"))
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env_remove("SH0_MASTER_PASSWORD")
        .env_remove("SH0_DISABLE_KEYRING")
        .output()
        .expect("无法执行 sh0")
}

#[test]
fn top_level_help_lists_core_commands_and_daemon() {
    let home = temp_home("help");
    let out = run(&home, &["--help"]);
    assert!(out.status.success(), "sh0 --help 应成功");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("tui"), "缺少 tui: {text}");
    assert!(text.contains("ai"), "缺少 ai: {text}");
    assert!(text.contains("help"), "缺少 help: {text}");
    // daemon 现在是可见的运维命令，和 tui/ai/help 一起列出
    let commands = text.split("Options:").next().unwrap_or("");
    assert!(
        commands.contains("daemon"),
        "daemon 应作为可见命令列出: {text}"
    );
    assert!(
        text.contains("后台 daemon"),
        "帮助里应说明后台 daemon 的作用: {text}"
    );

    let out = run(&home, &["daemon", "--help"]);
    assert!(out.status.success(), "sh0 daemon --help 应成功");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("run"), "daemon 应列出 run: {text}");
    assert!(text.contains("stop"), "daemon 应列出 stop: {text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn bare_sh0_and_help_print_paths() {
    let home = temp_home("bare");
    let out = run(&home, &[]);
    assert!(out.status.success(), "裸 sh0 应成功");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("配置目录"), "应打印配置目录: {text}");
    assert!(text.contains("数据目录"), "应打印数据目录: {text}");
    assert!(text.contains("sh0 tui"), "应给出 TUI 用法: {text}");

    let out = run(&home, &["help"]);
    assert!(out.status.success(), "sh0 help 应成功");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("sh0 ai"), "应给出 ai 用法: {text}");
    assert!(text.contains("后台 daemon"), "应说明 daemon 的作用: {text}");
    assert!(text.contains("daemon.log"), "应指出 daemon 日志: {text}");
    assert!(text.contains("sh0 daemon run"), "应给出 daemon 用法: {text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn removed_subcommands_are_rejected() {
    let home = temp_home("removed");
    for cmd in ["secret", "config", "host"] {
        let out = run(&home, &[cmd]);
        assert!(
            !out.status.success(),
            "`sh0 {cmd}` 应被拒绝，实际成功: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("unrecognized subcommand"),
            "`sh0 {cmd}` 的错误应说明未知子命令: {err}"
        );
    }
    let _ = std::fs::remove_dir_all(&home);
}
