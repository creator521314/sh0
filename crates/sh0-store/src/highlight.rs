//! 终端输出正则高亮（`highlight.toml`）。
//!
//! 设计取舍：
//! - **方案（scheme）** 是一组规则的命名集合；内置 `default / minimal / logs / none`，
//!   用户可在 `highlight.toml` 里新增或覆盖同名方案；
//! - 规则文件独立于 `config.toml`：数组表（`[[scheme]]`）在全局配置里会把文件撑得很长；
//! - 正则用 `regex` crate（线性时间，用户写错也不会把 TUI 拖死）；
//! - 颜色支持常用 ANSI 名称与 `#RGB/#RRGGBB`，实际映射到终端色由 TUI 层完成。

use std::collections::HashMap;
use std::path::Path;

use regex::{Regex, RegexBuilder};
use serde::de::Deserializer;
use serde::{Deserialize, de};

use crate::error::{Result, StoreError};

/// 内置方案名（`none` 永远为空，用于关闭高亮）
pub const BUILTIN_SCHEMES: [&str; 4] = ["default", "minimal", "logs", "none"];

/// 首次运行时写入的默认规则。注意 TOML 用单引号字符串，`\b` 等不会被转义。
///
/// 内置 `minimal / logs / none` 不需要在这里重复；这里放 `default` 方案，
/// 方便用户直接照着改。
pub const DEFAULT_HIGHLIGHT_TOML: &str = r#"# sh0 终端输出高亮规则（正则）
#
# 内置方案：default / minimal / logs / none（none = 不启用高亮）。
# 新增方案：复制一段 [[scheme]] 改名即可；同名会覆盖内置方案。
# 修改后 TUI 会在约 2 秒内自动重载。
#
# 规则按 priority 从高到低匹配，高优先级先占位；
# 颜色：black/red/green/yellow/blue/magenta/cyan/white、bright*/light*、
#       gray/grey/darkgray，或 #RGB / #RRGGBB。
version = 1

[[scheme]]
name = "default"

[[scheme.rule]]
name = "error"
pattern = '(?i)\b(?:errors?|fatal|panic|exception|traceback|fail(?:ed|ure|ing)?)\b'
fg = "red"
bold = true
priority = 100

[[scheme.rule]]
name = "warn"
pattern = '(?i)\b(?:warnings?|deprecated|timeout|timed out|retrying?)\b'
fg = "yellow"
bold = true
priority = 90

[[scheme.rule]]
name = "success"
pattern = '(?i)\b(?:ok|success(?:ful|fully)?|succeeded|done|passed|healthy|active \(running\))\b'
fg = "green"
priority = 80

[[scheme.rule]]
name = "info"
pattern = '(?i)\b(?:infos?|notice|debug|trace)\b'
fg = "cyan"
priority = 70

[[scheme.rule]]
name = "url"
pattern = 'https?://[^\s<>"]+'
fg = "blue"
underline = true
priority = 60

[[scheme.rule]]
name = "ipv4"
pattern = '\b(?:\d{1,3}\.){3}\d{1,3}\b'
fg = "magenta"
priority = 50

[[scheme.rule]]
name = "timestamp"
pattern = '\b\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?\b'
fg = "gray"
dim = true
priority = 40
"#;

/// `minimal`：只保留错误/警告，适合日常盯着看的生产终端。
const MINIMAL_HIGHLIGHT_TOML: &str = r#"
[[scheme]]
name = "minimal"

[[scheme.rule]]
name = "error"
pattern = '(?i)\b(?:errors?|fatal|panic|exception|traceback|fail(?:ed|ure|ing)?)\b'
fg = "red"
bold = true
priority = 100

[[scheme.rule]]
name = "warn"
pattern = '(?i)\b(?:warnings?|deprecated|timeout|timed out|retrying?)\b'
fg = "yellow"
bold = true
priority = 90
"#;

/// `logs`：面向 `tail -f` / 日志排查，弱化普通文本、突出级别与网络信息。
const LOGS_HIGHLIGHT_TOML: &str = r#"
[[scheme]]
name = "logs"

[[scheme.rule]]
name = "error"
pattern = '(?i)\b(?:errors?|fatal|panic|exception|traceback|fail(?:ed|ure|ing)?)\b'
fg = "red"
bold = true
priority = 100

[[scheme.rule]]
name = "warn"
pattern = '(?i)\b(?:warnings?|deprecated|timeout|timed out|retrying?)\b'
fg = "yellow"
bold = true
priority = 90

[[scheme.rule]]
name = "info"
pattern = '(?i)\b(?:infos?|notice|debug|trace)\b'
fg = "cyan"
priority = 70

[[scheme.rule]]
name = "url"
pattern = 'https?://[^\s<>"]+'
fg = "blue"
underline = true
priority = 60

[[scheme.rule]]
name = "ipv4"
pattern = '\b(?:\d{1,3}\.){3}\d{1,3}\b'
fg = "magenta"
priority = 50

[[scheme.rule]]
name = "timestamp"
pattern = '\b\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?\b'
fg = "gray"
dim = true
priority = 40
"#;

/// 颜色值：ANSI 名称或真彩
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HighlightColor {
    Named(String),
    Rgb(u8, u8, u8),
}

impl<'de> Deserialize<'de> for HighlightColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse_color(&raw).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HighlightStyle {
    #[serde(default)]
    pub fg: Option<HighlightColor>,
    #[serde(default)]
    pub bg: Option<HighlightColor>,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub dim: bool,
}

/// 编译后的规则
#[derive(Debug, Clone)]
pub struct HighlightRule {
    pub name: String,
    pub regex: Regex,
    pub style: HighlightStyle,
    pub priority: i32,
}

#[derive(Debug, Deserialize)]
struct HighlightFile {
    #[serde(default)]
    scheme: Vec<SchemeConfig>,
    /// 兼容旧版扁平 `[[rule]]`：视为 default 方案
    #[serde(default)]
    rule: Vec<RuleConfig>,
}

#[derive(Debug, Deserialize)]
struct SchemeConfig {
    name: String,
    #[serde(default)]
    rule: Vec<RuleConfig>,
}

#[derive(Debug, Deserialize)]
struct RuleConfig {
    #[serde(default)]
    name: String,
    pattern: String,
    #[serde(flatten)]
    style: HighlightStyle,
    #[serde(default)]
    priority: i32,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

/// 内置方案（始终可用；用户文件可覆盖同名方案）。
pub fn builtin_highlight_schemes() -> HashMap<String, Vec<HighlightRule>> {
    let mut out = HashMap::new();
    for raw in [DEFAULT_HIGHLIGHT_TOML, MINIMAL_HIGHLIGHT_TOML, LOGS_HIGHLIGHT_TOML] {
        let parsed = parse_scheme_file(raw, Path::new("<builtin highlight.toml>"))
            .expect("内置高亮方案必须可解析");
        out.extend(parsed);
    }
    out.insert("none".into(), Vec::new());
    out
}

/// 读取方案；文件不存在时先写一份默认配置（用户可以直接改）。
///
/// 内置方案始终存在；用户文件里同名方案会覆盖内置方案，新名字则新增。
pub fn load_or_init_schemes(path: &Path) -> Result<HashMap<String, Vec<HighlightRule>>> {
    let mut schemes = builtin_highlight_schemes();
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, DEFAULT_HIGHLIGHT_TOML)?;
    }
    let raw = std::fs::read_to_string(path)?;
    let user = parse_scheme_file(&raw, path)?;
    for (name, rules) in user {
        if name == "none" {
            // 保留 none 作为「关闭高亮」，用户的同名规则不生效
            continue;
        }
        schemes.insert(name, rules);
    }
    schemes.insert("none".into(), Vec::new());
    Ok(schemes)
}

/// 内置 `default` 方案（测试 / 兜底用）
pub fn default_highlight_rules() -> Vec<HighlightRule> {
    builtin_highlight_schemes()
        .remove("default")
        .unwrap_or_default()
}

fn parse_scheme_file(raw: &str, path: &Path) -> Result<HashMap<String, Vec<HighlightRule>>> {
    let file: HighlightFile = toml::from_str(raw).map_err(|source| StoreError::Parse {
        path: path.to_path_buf(),
        source,
    })?;

    let mut out = HashMap::new();
    for scheme in file.scheme {
        let name = scheme.name.trim().to_string();
        if name.is_empty() {
            return Err(StoreError::BadConfigValue {
                key: "highlight.scheme.name".into(),
                value: "方案名不能为空".into(),
            });
        }
        out.insert(name.clone(), compile_rules(scheme.rule, &name)?);
    }

    // 旧版扁平规则：作为 default 方案；若文件里已有 default，则追加在其后
    if !file.rule.is_empty() {
        let rules = compile_rules(file.rule, "default")?;
        out.entry("default".to_string())
            .or_insert_with(Vec::new)
            .extend(rules);
    }

    Ok(out)
}

fn compile_rules(configs: Vec<RuleConfig>, scheme: &str) -> Result<Vec<HighlightRule>> {
    let mut rules = Vec::new();
    for (idx, cfg) in configs.into_iter().enumerate() {
        if !cfg.enabled {
            continue;
        }
        let name = if cfg.name.trim().is_empty() {
            format!("#{}", idx + 1)
        } else {
            cfg.name
        };
        let regex = RegexBuilder::new(&cfg.pattern)
            .build()
            .map_err(|e| StoreError::BadConfigValue {
                key: format!("highlight.{scheme}.{name}.pattern"),
                value: e.to_string(),
            })?;
        rules.push(HighlightRule {
            name,
            regex,
            style: cfg.style,
            priority: cfg.priority,
        });
    }
    // 稳定的降序：同优先级保持文件中的先后顺序
    rules.sort_by_key(|r| std::cmp::Reverse(r.priority));
    Ok(rules)
}

fn parse_color(raw: &str) -> std::result::Result<HighlightColor, String> {
    let s = raw.trim().to_ascii_lowercase();
    if let Some(hex) = s.strip_prefix('#') {
        let (r, g, b) = match hex.len() {
            3 => {
                let r = hex_nibble(&hex[0..1])?;
                let g = hex_nibble(&hex[1..2])?;
                let b = hex_nibble(&hex[2..3])?;
                (r * 17, g * 17, b * 17)
            }
            6 => (
                u8::from_str_radix(&hex[0..2], 16).map_err(|_| "非法十六进制颜色".to_string())?,
                u8::from_str_radix(&hex[2..4], 16).map_err(|_| "非法十六进制颜色".to_string())?,
                u8::from_str_radix(&hex[4..6], 16).map_err(|_| "非法十六进制颜色".to_string())?,
            ),
            _ => return Err(format!("颜色 `{raw}` 应写成 #RGB 或 #RRGGBB")),
        };
        return Ok(HighlightColor::Rgb(r, g, b));
    }

    const NAMES: &[&str] = &[
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        "brightblack", "brightred", "brightgreen", "brightyellow", "brightblue",
        "brightmagenta", "brightcyan", "brightwhite", "gray", "grey", "darkgray",
        "darkgrey", "lightred", "lightgreen", "lightyellow", "lightblue",
        "lightmagenta", "lightcyan", "lightwhite",
    ];
    if NAMES.contains(&s.as_str()) {
        Ok(HighlightColor::Named(s))
    } else {
        Err(format!(
            "未知颜色 `{raw}`；可用名称：black/red/green/yellow/blue/magenta/cyan/white、bright*、gray/#RRGGBB"
        ))
    }
}

fn hex_nibble(s: &str) -> std::result::Result<u8, String> {
    u8::from_str_radix(s, 16).map_err(|_| "非法十六进制颜色".to_string())
}

impl crate::config::Store {
    /// 读取 `highlight.toml` 的全部方案；不存在时生成默认文件。
    pub fn load_highlight_schemes(&self) -> Result<HashMap<String, Vec<HighlightRule>>> {
        load_or_init_schemes(&self.paths().highlight_file())
    }

    /// 高亮规则文件的 mtime（TUI 热重载用）
    pub fn highlight_mtime(&self) -> Option<std::time::SystemTime> {
        std::fs::metadata(self.paths().highlight_file())
            .ok()
            .and_then(|m| m.modified().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sh0-highlight-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("highlight.toml")
    }

    #[test]
    fn builtins_have_expected_schemes() {
        let schemes = builtin_highlight_schemes();
        for name in BUILTIN_SCHEMES {
            assert!(schemes.contains_key(name), "缺少内置方案 {name}");
        }
        assert!(schemes["none"].is_empty());
        assert!(!schemes["default"].is_empty());
        assert!(schemes["minimal"].len() < schemes["default"].len());
    }

    #[test]
    fn named_and_hex_colors() {
        assert_eq!(
            parse_color("red").unwrap(),
            HighlightColor::Named("red".into())
        );
        assert_eq!(
            parse_color("#f80").unwrap(),
            HighlightColor::Rgb(0xff, 0x88, 0x00)
        );
        assert_eq!(
            parse_color("#112233").unwrap(),
            HighlightColor::Rgb(0x11, 0x22, 0x33)
        );
        assert!(parse_color("no-such-color").is_err());
    }

    #[test]
    fn invalid_regex_is_reported() {
        let raw = r#"
[[scheme]]
name = "bad"
[[scheme.rule]]
name = "bad"
pattern = "("
fg = "red"
"#;
        let err = parse_scheme_file(raw, Path::new("highlight.toml")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("highlight.bad.bad.pattern"), "{msg}");
    }

    #[test]
    fn user_can_add_and_override_schemes() {
        let path = temp_path("schemes");
        std::fs::write(
            &path,
            r##"
[[scheme]]
name = "default"
[[scheme.rule]]
name = "only"
pattern = "xyz"
fg = "red"

[[scheme]]
name = "custom"
[[scheme.rule]]
name = "magic"
pattern = "magic"
fg = "#123456"
"##,
        )
        .unwrap();
        let schemes = load_or_init_schemes(&path).unwrap();
        assert_eq!(schemes["default"].len(), 1, "同名方案应覆盖内置 default");
        assert!(schemes.contains_key("custom"), "新方案应被加入");
        assert!(schemes.contains_key("minimal"), "未覆盖的内置方案仍在");
        assert!(schemes["none"].is_empty(), "none 必须保持为空");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn legacy_flat_rules_become_default_scheme() {
        let path = temp_path("legacy");
        std::fs::write(
            &path,
            r#"
[[rule]]
name = "old"
pattern = "old"
fg = "green"
"#,
        )
        .unwrap();
        let schemes = load_or_init_schemes(&path).unwrap();
        assert_eq!(schemes["default"].len(), 1);
        assert_eq!(schemes["default"][0].name, "old");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_file_is_created_with_defaults() {
        let path = temp_path("missing");
        let schemes = load_or_init_schemes(&path).unwrap();
        assert!(path.exists());
        assert_eq!(
            schemes["default"].len(),
            default_highlight_rules().len()
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
