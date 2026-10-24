//! 风险分级引擎（需求 FR-7.1~FR-7.3，架构文档 §7）。
//!
//! 判定流程（架构文档 §7.1）：
//!
//! ```text
//! 原始命令字符串
//!    │
//!    ├─(0) 全局危险模式（整条命令上的正则）：管道喂 shell、fork 炸弹、写块设备…
//!    │
//!    ├─(1) 按 shell 词法切分成段（; && || | & 换行），逐段判定
//!    │        ├─ 内置 deny 规则（含 rm -rf / 这类需要代码特判的）
//!    │        ├─ 用户自定义规则（risk.toml，优先级高于内置 confirm/auto）
//!    │        ├─ 内置 confirm 规则
//!    │        └─ 内置 auto 白名单；都没命中 → default_level（默认 confirm）
//!    │
//!    ├─(2) 取所有段中的最高风险作为整条命令的风险
//!    │
//!    └─(3) 调用方再与主机 risk_cap 取较小值（FR-7.6）
//! ```
//!
//! 设计约束：本模块**不碰 I/O、不碰网络**，只吃字符串吐结论——
//! 这样"AI 会不会被放行执行危险命令"可以在单测里穷举验证，
//! 而不是等用户在真机上 `rm -rf` 之后才发现规则写错了。

use std::collections::BTreeSet;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::model::RiskLevel;

/// 一条规则的匹配条件。所有非空条件是 AND 关系；全空则不命中任何命令。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuleMatch {
    /// 程序名（basename）。已剥掉 `sudo` / `env` 等包装命令与前导 `VAR=val`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prog: Option<String>,
    /// 参数中出现任意一个即命中（支持 `-r*` 这样的尾部通配）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args_any: Vec<String>,
    /// 参数中必须全部出现
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args_all: Vec<String>,
    /// 对**整段**命令文本的正则（管道、重定向等词法切分后丢失的结构）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

impl RuleMatch {
    fn is_empty(&self) -> bool {
        self.prog.is_none() && self.args_any.is_empty() && self.args_all.is_empty() && self.pattern.is_none()
    }
}

/// risk.toml 里的一条用户规则（FR-7.5）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub level: RiskLevel,
    #[serde(rename = "match")]
    pub matcher: RuleMatch,
    pub reason: String,
}

/// 单段判定明细
#[derive(Debug, Clone)]
pub struct SegmentAssessment {
    pub segment: String,
    pub prog: Option<String>,
    pub level: RiskLevel,
    pub rule: Option<String>,
}

/// 整条命令的判定结果
#[derive(Debug, Clone)]
pub struct Assessment {
    pub level: RiskLevel,
    /// 命中规则的 reason；未命中任何规则时为 None
    pub reason: Option<String>,
    /// 命中规则的稳定 id（内置规则如 `deny-rm-root`，用户规则为 `user:N`）
    pub rule: Option<String>,
    /// 逐段明细，便于 UI 展示与事后排查"到底是哪段被判危"
    pub segments: Vec<SegmentAssessment>,
}

impl Assessment {
    /// 与主机风险上限取较小值（FR-7.6：命令风险 > risk_cap 时按 risk_cap 处理）
    pub fn capped(mut self, cap: Option<RiskLevel>) -> Self {
        if let Some(cap) = cap
            && self.level > cap
        {
            self.level = cap;
            self.rule = Some(format!("risk_cap:{cap:?}").to_lowercase());
            self.reason = Some(format!("主机风险上限为 {cap:?}"));
        }
        self
    }
}

/// 编译后的规则（正则已编译，非法正则的规则在建引擎时被跳过）
struct CompiledRule {
    level: RiskLevel,
    id: String,
    reason: String,
    prog: Option<String>,
    args_any: Vec<String>,
    args_all: Vec<String>,
    pattern: Option<Regex>,
}

impl CompiledRule {
    fn matches(&self, segment: &str, prog: Option<&str>, args: &[String]) -> bool {
        if let Some(p) = &self.prog
            && prog != Some(p.as_str())
        {
            return false;
        }
        if !self.args_any.is_empty()
            && !args
                .iter()
                .any(|a| self.args_any.iter().any(|pat| arg_matches(pat, a)))
        {
            return false;
        }
        if !self.args_all.is_empty()
            && !self
                .args_all
                .iter()
                .all(|pat| args.iter().any(|a| arg_matches(pat, a)))
        {
            return false;
        }
        if let Some(re) = &self.pattern
            && !re.is_match(segment)
        {
            return false;
        }
        // 至少有一个有效条件才可能命中（构造时已保证，这里防御性返回 false）
        self.prog.is_some()
            || !self.args_any.is_empty()
            || !self.args_all.is_empty()
            || self.pattern.is_some()
    }
}

/// 参数通配：`*` 匹配任意；`-r*` 匹配前缀；其余精确匹配
fn arg_matches(pattern: &str, arg: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return arg.starts_with(prefix);
    }
    arg == pattern
}

/// 包装命令：剥掉它们才能看到真正的程序名
const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "nohup", "nice", "ionice", "stdbuf", "command", "timeout", "setsid",
    "stdbuf", "busybox",
];

pub struct RiskEngine {
    /// 全局危险模式：对整条命令生效的正则（管道喂 shell 等）
    global_deny: Vec<(Regex, &'static str, &'static str)>,
    deny: Vec<CompiledRule>,
    user: Vec<CompiledRule>,
    confirm: Vec<CompiledRule>,
    /// 内置 auto 白名单（程序名集合）
    auto: BTreeSet<&'static str>,
    /// 什么都没命中时的默认档位
    pub default_level: RiskLevel,
}

impl Default for RiskEngine {
    fn default() -> Self {
        Self::builtin()
    }
}

impl RiskEngine {
    /// 内置规则集（架构文档 §7.2 的初始集）
    pub fn builtin() -> Self {
        Self {
            global_deny: builtin_global_deny(),
            deny: builtin_deny(),
            user: Vec::new(),
            confirm: builtin_confirm(),
            auto: builtin_auto(),
            default_level: RiskLevel::Confirm,
        }
    }

    /// 叠加用户规则（FR-7.5）。非法正则的规则会被跳过并记警告。
    pub fn with_user_rules(mut self, rules: &[Rule]) -> Self {
        for (i, r) in rules.iter().enumerate() {
            if r.matcher.is_empty() {
                warn!("risk.toml 第 {i} 条规则没有任何匹配条件，已跳过");
                continue;
            }
            let pattern = match &r.matcher.pattern {
                Some(p) => match Regex::new(p) {
                    Ok(re) => Some(re),
                    Err(e) => {
                        warn!("risk.toml 第 {i} 条规则正则无效（{p}）: {e}，已跳过");
                        continue;
                    }
                },
                None => None,
            };
            self.user.push(CompiledRule {
                level: r.level,
                id: format!("user:{i}"),
                reason: r.reason.clone(),
                prog: r.matcher.prog.clone(),
                args_any: r.matcher.args_any.clone(),
                args_all: r.matcher.args_all.clone(),
                pattern,
            });
        }
        self
    }

    /// 对一条命令做完整判定
    pub fn assess(&self, cmd: &str) -> Assessment {
        let mut segments = Vec::new();
        let mut worst = RiskLevel::Auto;
        let mut hit_reason: Option<String> = None;
        let mut hit_rule: Option<String> = None;

        // (0) 全局模式优先：管道喂 shell 这类结构在分段之后就读不出来了
        for (re, id, reason) in &self.global_deny {
            if re.is_match(cmd) {
                return Assessment {
                    level: RiskLevel::Deny,
                    reason: Some((*reason).to_string()),
                    rule: Some((*id).to_string()),
                    segments: Vec::new(),
                };
            }
        }

        for seg in split_segments(cmd) {
            let seg = seg.trim();
            if seg.is_empty() {
                continue;
            }
            let tokens = tokenize(seg);
            let (prog, args) = unwrap_command(&tokens);
            let (level, rule, reason) = self.assess_segment(seg, prog.as_deref(), &args);

            // 取所有段中的最高风险；同时记住它是被哪条规则判的
            if level > worst {
                worst = level;
                hit_reason = reason;
                hit_rule = rule.clone();
            }
            segments.push(SegmentAssessment {
                segment: seg.to_string(),
                prog,
                level,
                rule,
            });
        }

        Assessment {
            level: worst,
            reason: hit_reason,
            rule: hit_rule,
            segments,
        }
    }

    fn assess_segment(
        &self,
        segment: &str,
        prog: Option<&str>,
        args: &[String],
    ) -> (RiskLevel, Option<String>, Option<String>) {
        // (1) 内置 deny（含需要代码特判的 rm -rf /）
        if let Some((id, reason)) = builtin_deny_special(segment, prog, args) {
            return (RiskLevel::Deny, Some(id), Some(reason));
        }
        for r in &self.deny {
            if r.matches(segment, prog, args) {
                return (r.level, Some(r.id.clone()), Some(r.reason.clone()));
            }
        }
        // (2) 用户规则：优先级高于内置 confirm/auto（FR-7.5 允许调整档位）
        for r in &self.user {
            if r.matches(segment, prog, args) {
                return (r.level, Some(r.id.clone()), Some(r.reason.clone()));
            }
        }
        // (3) 内置 confirm
        for r in &self.confirm {
            if r.matches(segment, prog, args) {
                return (r.level, Some(r.id.clone()), Some(r.reason.clone()));
            }
        }
        // (4) 内置 auto 白名单
        if let Some(p) = prog
            && self.auto.contains(p)
        {
            return (RiskLevel::Auto, None, None);
        }
        // (5) 兜底
        (
            self.default_level,
            Some("default".into()),
            Some("未命中任何规则，按默认档处理".into()),
        )
    }
}

// ---------------------------------------------------------------------------
// shell 词法切分
// ---------------------------------------------------------------------------

/// 把一条命令按 `;` `&&` `||` `|` `&` 与换行切成段（尊重引号）。
///
/// 不能简单 `split(';')`：`echo "a;b"` 里的分号不是分隔符，
/// 而 `find . -name '*.tmp' -delete` 整体就是一段。
pub fn split_segments(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = cmd.chars().peekable();
    let mut quote: Option<char> = None;

    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                cur.push(c);
            }
            ';' | '\n' | '&' | '|' => {
                // && 与 || 是两个字符的分隔符；单 & 与 | 各算一个
                if matches!(c, '&' | '|') && chars.peek() == Some(&c) {
                    chars.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// 把一个段切分成 token（尊重单/双引号与反斜杠转义）。
///
/// 与真正 shell 的差别：不做变量展开、不处理 `$'...'` 与 here-doc——
/// 风险判定只需要"程序名 + 参数形状"，不需要求值。
pub fn tokenize(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut chars = segment.chars();

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                // 单引号内全字面
                has = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    cur.push(q);
                }
            }
            '"' => {
                has = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            // 双引号内只转义 " \ $ `
                            if let Some(n) = chars.next() {
                                if matches!(n, '"' | '\\' | '$' | '`') {
                                    cur.push(n);
                                } else {
                                    cur.push('\\');
                                    cur.push(n);
                                }
                            }
                        }
                        _ => cur.push(q),
                    }
                }
            }
            '\\' => {
                has = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() => {
                if has {
                    tokens.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                has = true;
                cur.push(c);
            }
        }
    }
    if has {
        tokens.push(cur);
    }
    tokens
}

/// 剥掉包装命令与前导环境变量赋值，返回 (真正程序名 basename, 参数)。
///
/// `sudo -u deploy rm -rf x` → ("rm", ["-rf", "x"])
/// `FOO=bar make` → ("make", [])
pub fn unwrap_command(tokens: &[String]) -> (Option<String>, Vec<String>) {
    let mut i = 0;
    // 前导 VAR=value 赋值
    while i < tokens.len() && is_env_assignment(&tokens[i]) {
        i += 1;
    }

    let mut prog = tokens.get(i).cloned();
    let mut args: Vec<String> = tokens.iter().skip(i + 1).cloned().collect();

    // 最多剥四层包装，再套娃按原样处理（规则仍能命中内层）
    for _ in 0..4 {
        let Some(p) = prog.as_deref().map(basename) else {
            break;
        };
        if !WRAPPERS.contains(&p.as_str()) {
            break;
        }
        let mut j = 0;
        // 包装命令自己的选项：-u user / -E / --login 之类
        while j < args.len() && args[j].starts_with('-') {
            // 取值的选项：-u/-g/-h/-p/-C 后跟一个参数
            if matches!(args[j].as_str(), "-u" | "-g" | "-h" | "-p" | "-C" | "-n" | "--user")
                && j + 1 < args.len()
            {
                j += 2;
            } else {
                j += 1;
            }
        }
        // env 的 VAR=val 与 timeout 的时长参数
        while j < args.len() && (is_env_assignment(&args[j]) || (p == "timeout" && j == 0)) {
            j += 1;
        }
        prog = args.get(j).cloned();
        args = args.iter().skip(j + 1).cloned().collect();
    }

    (prog.map(|p| basename(&p)), args)
}

fn is_env_assignment(t: &str) -> bool {
    match t.split_once('=') {
        Some((k, _)) => !k.is_empty()
            && k.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !t.starts_with('-'),
        None => false,
    }
}

/// 取路径的最后一段：`/bin/rm` → `rm`，`rm` → `rm`
pub fn basename(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}

// ---------------------------------------------------------------------------
// 内置规则集
// ---------------------------------------------------------------------------

/// 全局 deny 模式：对整条命令文本生效。
///
/// 这些模式**必须在分段之前**匹配——`curl x | sh` 切开之后，
/// 两段各自都是"下载"和"执行脚本"，单看都不危险，合起来才是。
fn builtin_global_deny() -> Vec<(Regex, &'static str, &'static str)> {
    let raw: &[(&str, &str, &str)] = &[
        // 管道喂 shell：curl|sh、wget -O-|bash、curl|sudo sh…
        (r"(curl|wget)[^|;&\n]*\|\s*(sudo\s+)?(ba|z|da|a)?sh\b", "deny-pipe-shell", "下载内容直接管道给 shell 执行"),
        // fork 炸弹 :(){ :|:& };:
        (r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;?\s*:", "deny-fork-bomb", "fork 炸弹"),
        // 直接写块设备：> /dev/sda、:> /dev/nvme0n1、dd…of=/dev/…
        (r"(>>?|:>|tee\s+(-a\s+)?)\s*[\x22\x27]?/dev/(sd|nvme|hd|vd|mmcblk|xvd)", "deny-raw-disk", "直接写块设备"),
        (r"\bdd\b[^|;&\n]*\bof=/dev/", "deny-dd-device", "dd 写块设备"),
        // 覆写 SSH 配置与授权密钥
        (r"(>>?|tee\s+(-a\s+)?)\s*[\x22\x27]?(/etc/ssh/sshd_config|/?root/\.ssh/authorized_keys|~?/\.ssh/authorized_keys)", "deny-ssh-config", "覆写 SSH 配置或 authorized_keys"),
        // 清空防火墙
        (r"\biptables\s+(-F|--flush)\b", "deny-iptables-flush", "清空 iptables 规则"),
    ];
    raw.iter()
        .filter_map(|(p, id, reason)| match Regex::new(p) {
            Ok(re) => Some((re, *id, *reason)),
            Err(e) => {
                warn!("内置 deny 正则无效 {id}: {e}");
                None
            }
        })
        .collect()
}

/// 需要代码特判的内置 deny（FR-7.3 的 `rm -rf /` 家族）。
///
/// 为什么不用正则：`rm -rf /`、`rm -fr /`、`rm -r /`、`rm --recursive --force /`、
/// `rm -rf /*` 的旗标组合太多，词法切分后逐个比对参数比一条巨型正则可靠。
fn builtin_deny_special(
    _segment: &str,
    prog: Option<&str>,
    args: &[String],
) -> Option<(String, String)> {
    if prog != Some("rm") {
        return None;
    }
    let recursive = args.iter().any(|a| {
        a.starts_with('-')
            && !a.starts_with("--")
            && (a.contains('r') || a.contains('R'))
            && a[1..].chars().all(|c| c.is_ascii_alphabetic())
    }) || args.iter().any(|a| a == "--recursive");
    if !recursive {
        return None;
    }
    let root = args
        .iter()
        .any(|a| a == "/" || a == "/*" || a.starts_with("/*") || a == "~" || a == "~/" || a == "$HOME");
    if root {
        return Some((
            "deny-rm-root".into(),
            "递归删除根目录（recursive delete on /）".into(),
        ));
    }
    None
}

fn builtin_deny() -> Vec<CompiledRule> {
    let raw: Vec<(&str, &str, RuleMatch)> = vec![
        (
            "deny-mkfs",
            "格式化文件系统",
            RuleMatch {
                prog: None,
                args_any: vec![],
                args_all: vec![],
                pattern: Some(r"\bmkfs(\b|\.)".into()),
            },
        ),
        (
            "deny-shutdown",
            "关机/重启类命令",
            RuleMatch {
                prog: None,
                args_any: vec![],
                args_all: vec![],
                pattern: Some(
                    r"(^|[|;&\s])(shutdown|poweroff|reboot|halt)\b|(^|\s)init\s+[06]\b".into(),
                ),
            },
        ),
    ];
    compile_level(RiskLevel::Deny, raw)
}

fn builtin_confirm() -> Vec<CompiledRule> {
    let raw: Vec<(&str, &str, RuleMatch)> = vec![
        ("confirm-rm", "删除文件", RuleMatch { prog: Some("rm".into()), ..Default::default() }),
        ("confirm-mv", "移动/覆盖文件", RuleMatch { prog: Some("mv".into()), ..Default::default() }),
        (
            "confirm-cp-force",
            "强制覆盖复制",
            RuleMatch {
                prog: Some("cp".into()),
                args_any: vec!["-f".into(), "--force".into()],
                ..Default::default()
            },
        ),
        (
            "confirm-sed-i",
            "sed 原地改写文件",
            RuleMatch {
                prog: Some("sed".into()),
                args_any: vec!["-i".into(), "--in-place".into()],
                ..Default::default()
            },
        ),
        (
            "confirm-chmod-r",
            "递归改权限",
            RuleMatch {
                prog: Some("chmod".into()),
                args_any: vec!["-R".into(), "--recursive".into()],
                ..Default::default()
            },
        ),
        (
            "confirm-chown-r",
            "递归改属主",
            RuleMatch {
                prog: Some("chown".into()),
                args_any: vec!["-R".into(), "--recursive".into()],
                ..Default::default()
            },
        ),
        (
            "confirm-systemctl-service",
            "启停系统服务",
            RuleMatch {
                prog: Some("systemctl".into()),
                args_any: vec![
                    "stop".into(),
                    "restart".into(),
                    "reload".into(),
                    "kill".into(),
                    "disable".into(),
                    "mask".into(),
                ],
                ..Default::default()
            },
        ),
        (
            "confirm-docker-destructive",
            "docker 破坏性操作",
            RuleMatch {
                prog: Some("docker".into()),
                args_any: vec!["rm".into(), "rmi".into(), "stop".into(), "kill".into(), "prune".into(), "system".into()],
                ..Default::default()
            },
        ),
        (
            "confirm-apt",
            "包管理变更",
            RuleMatch {
                prog: None,
                args_any: vec![],
                args_all: vec![],
                pattern: Some(
                    r"(^|\s)(apt|apt-get|yum|dnf|zypper|pacman)\s+(-[^\s]+\s+)*(install|remove|purge|upgrade|update|erase|autoremove)\b".into(),
                ),
            },
        ),
        (
            "confirm-kill",
            "杀进程",
            RuleMatch {
                prog: None,
                args_any: vec![],
                args_all: vec![],
                pattern: Some(r"(^|\s)(kill|killall|pkill)\b".into()),
            },
        ),
        (
            "confirm-git-force",
            "git 强推/硬重置",
            RuleMatch {
                prog: Some("git".into()),
                args_all: vec!["reset".into(), "--hard".into()],
                ..Default::default()
            },
        ),
        (
            "confirm-git-push-force",
            "git 强推",
            RuleMatch {
                prog: Some("git".into()),
                args_all: vec!["push".into()],
                args_any: vec!["-f".into(), "--force".into()],
                ..Default::default()
            },
        ),
        (
            "confirm-find-exec",
            "find 执行/删除动作",
            RuleMatch {
                prog: Some("find".into()),
                args_any: vec!["-delete".into(), "-exec".into(), "-execdir".into(), "-ok".into(), "-okdir".into()],
                ..Default::default()
            },
        ),
        (
            "confirm-write-etc",
            "写 /etc 下文件",
            RuleMatch {
                prog: None,
                args_any: vec![],
                args_all: vec![],
                pattern: Some(r"(>>?|tee\s+(-a\s+)?)\s*[\x22\x27]?/etc/".into()),
            },
        ),
        (
            "confirm-user-mgmt",
            "用户/组管理",
            RuleMatch {
                prog: None,
                args_any: vec![],
                args_all: vec![],
                pattern: Some(r"(^|\s)(useradd|userdel|usermod|groupadd|groupdel|passwd|chpasswd)\b".into()),
            },
        ),
        (
            "confirm-k8s",
            "k8s 破坏性操作",
            RuleMatch {
                prog: Some("kubectl".into()),
                args_any: vec!["delete".into(), "drain".into(), "cordon".into(), "taint".into()],
                ..Default::default()
            },
        ),
        (
            "confirm-truncate",
            "截断文件",
            RuleMatch { prog: Some("truncate".into()), ..Default::default() },
        ),
    ];
    compile_level(RiskLevel::Confirm, raw)
}

fn compile_level(level: RiskLevel, raw: Vec<(&str, &str, RuleMatch)>) -> Vec<CompiledRule> {
    raw.into_iter()
        .filter_map(|(id, reason, m)| {
            let pattern = match &m.pattern {
                Some(p) => match Regex::new(p) {
                    Ok(re) => Some(re),
                    Err(e) => {
                        warn!("内置规则 {id} 正则无效: {e}");
                        return None;
                    }
                },
                None => None,
            };
            Some(CompiledRule {
                level,
                id: id.into(),
                reason: reason.into(),
                prog: m.prog,
                args_any: m.args_any,
                args_all: m.args_all,
                pattern,
            })
        })
        .collect()
}

fn builtin_auto() -> BTreeSet<&'static str> {
    // 架构文档 §7.2 的 auto 初始集 + 常见只读工具
    [
        "ls", "cat", "head", "tail", "less", "more", "pwd", "whoami", "id", "uname", "ps", "top",
        "df", "du", "free", "uptime", "hostname", "ip", "ss", "netstat", "ifconfig", "grep",
        "egrep", "fgrep", "rg", "find", "git", "journalctl", "systemctl", "docker", "curl",
        "wget", "echo", "date", "env", "printenv", "w", "last", "lsb_release", "lscpu", "lsblk",
        "lsmod", "vmstat", "iostat", "sar", "dmesg", "mount", "which", "whereis", "type", "stat",
        "file", "wc", "sort", "uniq", "cut", "awk", "gawk", "sed", "tr", "diff", "cmp", "md5sum",
        "sha1sum", "sha256sum", "base64", "xxd", "od", "hexdump", "ping", "traceroute", "dig",
        "nslookup", "host", "cd", "pushd", "popd", "exit", "true", "false", "test", "sleep",
        "nvidia-smi", "tar", "gzip", "gunzip", "zip", "unzip", "zcat", "bc", "expr", "seq",
        "watch", "time", "strace", "lsof", "fuser", "readlink", "realpath", "basename",
        "dirname", "column", "ts", "jq", "yq",
        // 清屏 / 查帮助：无副作用。注意 clear 会清掉屏幕底样，
        // exec 的哨兵截取有专门兜底（extract_output 规则 4）
        "clear", "reset", "tput", "history", "alias", "man", "info", "help",
        // 非递归的权限/属主变更按 auto 处理；递归形式由 confirm 规则先拦截
        "chmod", "chown", "chgrp", "setfacl", "getfacl",
    ]
    .into_iter()
    .collect()
}
