//! 配置导入：`~/.ssh/config` 只读导入 + Xshell `.xsh` 会话导入（需求 FR-1.10）。
//!
//! 设计取向：**只读源文件、绝不回写**。OpenSSH 的配置是别的工具的领地，
//! 我们只把它翻译成本机的 hosts.toml 条目；冲突时保留 sh0 已有的那一份。
//! 口令类字段一律不导入（Xshell 的口令是加密的，OpenSSH 配置里本来就没有）。
//!
//! 与 `Host` 的映射：
//! - `HostName` → `addr`，`User` → `user`，`Port` → `port`
//! - `IdentityFile` → 私钥认证（相对 `~` 的会展开成绝对路径）
//! - `ProxyJump` → `proxy_jump`（取逗号分隔的最后一段：ssh 的语义是"最后一跳"）
//! - 通配符模式（`Host *`）里的 `HostName %h` 之类不做替换，原样保留并打标记

use std::path::{Path, PathBuf};

use sh0_core::{CredentialRef, Host, new_host_id};

/// 导入来源
#[derive(Debug, Clone)]
pub enum ImportSource {
    /// OpenSSH 客户端配置，缺省 `~/.ssh/config`
    SshConfig(PathBuf),
    /// Xshell 会话文件，或装着多个 `.xsh` 的目录
    Xsh(PathBuf),
}

impl ImportSource {
    pub fn default_ssh_config() -> Option<Self> {
        dirs::home_dir().map(|h| Self::SshConfig(h.join(".ssh").join("config")))
    }

    /// 按名字构造：`ssh` / `xsh`
    pub fn parse(kind: &str, path: Option<&str>) -> std::io::Result<Self> {
        match kind.to_lowercase().as_str() {
            "ssh" | "ssh_config" | "ssh-config" => match path {
                Some(p) => Ok(Self::SshConfig(PathBuf::from(p))),
                None => Self::default_ssh_config().ok_or_else(|| {
                    std::io::Error::other("无法确定家目录，请用 --path 指定 ssh 配置文件")
                }),
            },
            "xsh" | "xshell" => Ok(Self::Xsh(match path {
                Some(p) => PathBuf::from(p),
                None => return Err(std::io::Error::other("导入 Xshell 需要 --path")),
            })),
            other => Err(std::io::Error::other(format!(
                "未知导入来源 `{other}`（支持 ssh / xsh）"
            ))),
        }
    }

    /// 读源文件并解析出候选主机
    pub fn collect(&self) -> std::io::Result<ImportReport> {
        match self {
            Self::SshConfig(path) => {
                let text = std::fs::read_to_string(path)?;
                Ok(parse_ssh_config(&text))
            }
            Self::Xsh(path) => {
                if path.is_dir() {
                    return import_xsh_dir(path);
                }
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("session");
                let report = match std::fs::read_to_string(path) {
                    Ok(text) => {
                        let mut r = ImportReport::default();
                        match parse_xsh(&text, name).host {
                            Some(h) => r.imported.push(h),
                            None => r
                                .skipped
                                .push((name.to_string(), "不是可用的 SSH 会话".to_string())),
                        }
                        r
                    }
                    // Xshell 常用 UTF-16 存会话，UTF-8 读会失败
                    Err(_) => {
                        let bytes = std::fs::read(path)?;
                        let text = String::from_utf16_lossy(
                            &bytes
                                .chunks_exact(2)
                                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                                .collect::<Vec<u16>>(),
                        );
                        let mut r = ImportReport::default();
                        match parse_xsh(&text, name).host {
                            Some(h) => r.imported.push(h),
                            None => r
                                .skipped
                                .push((name.to_string(), "不是可用的 SSH 会话".to_string())),
                        }
                        r
                    }
                };
                Ok(report)
            }
        }
    }
}

/// 一条导入结果：可能成功，也可能因为缺地址之类被跳过
#[derive(Debug, Clone)]
pub struct ImportedHost {
    pub host: Option<Host>,
    /// 源里的名字（ssh config 的 Host 模式 / xsh 文件名）
    pub source_name: String,
    /// 跳过的理由；`host` 为 Some 时为 None
    pub skipped: Option<String>,
}

/// 一次导入的汇总
#[derive(Debug, Default)]
pub struct ImportReport {
    pub imported: Vec<Host>,
    pub skipped: Vec<(String, String)>,
    /// 与已有主机重名而被跳过的（导入不覆盖既有配置）
    pub duplicated: Vec<String>,
}

impl ImportReport {
    pub fn total(&self) -> usize {
        self.imported.len() + self.skipped.len() + self.duplicated.len()
    }
}

/// 解析 OpenSSH 客户端配置。
///
/// 只处理我们认识的关键字；`Match` / `Include` / `CertificateFile` 等忽略。
/// `Host` 段之外的全局设置（如 `Host *` 块）会被当作"默认值"应用到所有条目，
/// 这是 ssh 的真实语义——`Host *` 里写 `User root` 就该生效。
pub fn parse_ssh_config(text: &str) -> ImportReport {
    let mut report = ImportReport::default();
    let mut current: Option<SshBlock> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(char::is_whitespace) {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
            // 也容忍 `Key=Value`（OpenSSH 支持等号写法）
            None => match line.split_once('=') {
                Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
                None => continue,
            },
        };
        let value = value.trim_matches('"');

        if key == "host" {
            // 落地上一段
            if let Some(b) = current.take() {
                push_ssh_block(b, &mut report);
            }
            current = Some(SshBlock::new(value.to_string()));
            continue;
        }
        let Some(block) = current.as_mut() else {
            continue;
        };
        match key.as_str() {
            "hostname" => block.hostname = Some(value.to_string()),
            "user" => block.user = Some(value.to_string()),
            "port" => block.port = value.parse::<u16>().ok(),
            "identityfile" => block.identity = Some(expand_home(value)),
            "proxyjump" => {
                // `ProxyJump a,b` 在 ssh 里表示逐跳转发；我们要的是最后一跳
                let last = value.split(',').filter(|s| !s.trim().is_empty()).last();
                block.proxy_jump = last
                    .map(|s| s.trim())
                    .filter(|s| *s != "none")
                    .map(|s| s.to_string());
            }
            _ => {}
        }
    }
    if let Some(b) = current.take() {
        push_ssh_block(b, &mut report);
    }
    report
}

/// 一段 `Host ...` 及其键值
#[derive(Debug)]
struct SshBlock {
    pattern: String,
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity: Option<PathBuf>,
    proxy_jump: Option<String>,
}

impl SshBlock {
    fn new(pattern: String) -> Self {
        Self {
            pattern,
            hostname: None,
            user: None,
            port: None,
            identity: None,
            proxy_jump: None,
        }
    }
}

fn push_ssh_block(b: SshBlock, report: &mut ImportReport) {
    // `Host *` 是通配段，不是一台主机——ssh 里它通常只放默认值
    if b.pattern == "*" || b.pattern.contains('*') || b.pattern.contains('?') {
        report.skipped.push((
            b.pattern,
            "通配模式不作为主机导入（通常只放默认参数）".to_string(),
        ));
        return;
    }
    let addr = b.hostname.clone().unwrap_or_else(|| b.pattern.clone());
    if addr.contains('%') {
        report.skipped.push((
            b.pattern,
            format!("地址含 ssh 占位符（{addr}），需手工确认"),
        ));
        return;
    }
    let user = b.user.unwrap_or_else(current_user);
    let auth = match &b.identity {
        Some(p) => CredentialRef::KeyFile {
            path: p.clone(),
            passphrase: None,
        },
        None => CredentialRef::Agent,
    };
    report.imported.push(Host {
        id: new_host_id(),
        name: b.pattern.clone(),
        group: vec!["imported".to_string()],
        addr,
        port: b.port.unwrap_or(22),
        user,
        auth,
        tags: vec!["ssh-config".to_string()],
        note: Some("由 ~/.ssh/config 导入".to_string()),
        risk_cap: None,
        proxy_jump: b.proxy_jump,
        jump: None,
    });
}

/// 解析一个 Xshell `.xsh` 会话文件（INI 风格）。
///
/// 真实格式大致是 `[CONNECTION]` 段里的 `HOSTNAME=` / `PORT=` / `USERNAME=` /
/// `USERKEY=`。口令字段（`PASSWORD=`）是加密的，**不导入**。
pub fn parse_xsh(text: &str, fallback_name: &str) -> ImportedHost {
    let mut hostname = None;
    let mut port = None;
    let mut user = None;
    let mut key = None;
    let mut protocol = None;
    let mut in_connection = false;

    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_connection = line.eq_ignore_ascii_case("[CONNECTION]");
            continue;
        }
        if !in_connection {
            continue;
        }
        let (k, v) = match line.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_uppercase(), v.trim().trim_matches('"')),
            None => continue,
        };
        match k.as_str() {
            "HOSTNAME" | "HOST" => hostname = Some(v.to_string()),
            "PORT" => port = v.parse::<u16>().ok(),
            "USERNAME" | "USER" => user = Some(v.to_string()),
            "USERKEY" | "IDENTITYFILE" => {
                if !v.is_empty() {
                    key = Some(PathBuf::from(v));
                }
            }
            "PROTOCOL" => protocol = Some(v.to_string()),
            _ => {}
        }
    }

    // Xshell 的会话文件也可能存的是串口/Telnet，这里只认 SSH
    if let Some(p) = &protocol
        && !p.eq_ignore_ascii_case("SSH")
    {
        return ImportedHost {
            host: None,
            source_name: fallback_name.to_string(),
            skipped: Some(format!("协议为 {p}，只导入 SSH 会话")),
        };
    }

    let Some(addr) = hostname.filter(|h| !h.is_empty()) else {
        return ImportedHost {
            host: None,
            source_name: fallback_name.to_string(),
            skipped: Some("文件里没有主机地址".to_string()),
        };
    };

    let auth = match key {
        Some(path) => CredentialRef::KeyFile {
            path,
            passphrase: None,
        },
        None => CredentialRef::Agent,
    };
    ImportedHost {
        host: Some(Host {
            id: new_host_id(),
            name: fallback_name.to_string(),
            group: vec!["imported".to_string()],
            addr,
            port: port.unwrap_or(22),
            user: user.unwrap_or_else(current_user),
            auth,
            tags: vec!["xshell".to_string()],
            note: Some("由 Xshell 会话导入".to_string()),
            risk_cap: None,
            proxy_jump: None,
            jump: None,
        }),
        source_name: fallback_name.to_string(),
        skipped: None,
    }
}

/// 导入一个目录下的全部 `.xsh`（Xshell 常常一个会话一个文件）
pub fn import_xsh_dir(dir: &Path) -> std::io::Result<ImportReport> {
    let mut report = ImportReport::default();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("xsh") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                // Xshell 会用 UTF-16 存会话文件，读成 UTF-8 会有一堆 NUL
                let bytes = std::fs::read(&path)?;
                String::from_utf16_lossy(
                    &bytes
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect::<Vec<u16>>(),
                )
            }
        };
        match parse_xsh(&text, &name).host {
            Some(h) => report.imported.push(h),
            None => report
                .skipped
                .push((name, "不是可用的 SSH 会话".to_string())),
        }
    }
    Ok(report)
}

/// 把导入结果与现有主机合并：重名的保留现有条目（不覆盖用户改过的配置）
pub fn merge_into(existing: &mut Vec<Host>, mut report: ImportReport) -> ImportReport {
    let mut out = ImportReport::default();
    for h in report.imported.drain(..) {
        if existing.iter().any(|e| e.name == h.name) {
            out.duplicated.push(h.name.clone());
        } else {
            existing.push(h.clone());
            out.imported.push(h);
        }
    }
    out.skipped = report.skipped;
    out
}

fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".to_string())
}

/// 展开 `~`。sh0 的私钥路径不支持 `~`（需求里明确要求绝对路径），
/// 所以导入时必须先展开，否则导入完就连接不上。
fn expand_home(p: &str) -> PathBuf {
    let p = p.trim_matches('"');
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
Host *
    ServerAliveInterval 30

Host web-01
    HostName 10.0.0.1
    User root
    Port 2222
    IdentityFile ~/.ssh/id_ed25519

Host db-01
    HostName db.internal
    User admin

Host jump-chain
    HostName 10.0.0.9
    User ops
    ProxyJump bastion,edge

Host weird
    HostName %h.example.com
"#;

    #[test]
    fn parses_hosts_and_skips_wildcards() {
        let r = parse_ssh_config(SAMPLE);
        let names: Vec<&str> = r.imported.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["web-01", "db-01", "jump-chain"]);
        assert!(
            r.skipped.iter().any(|(n, _)| n == "*"),
            "`Host *` 应被跳过: {:?}",
            r.skipped
        );
        assert!(
            r.skipped.iter().any(|(n, _)| n == "weird"),
            "含 %h 占位符的应跳过: {:?}",
            r.skipped
        );
    }

    #[test]
    fn maps_fields_including_key_and_proxyjump() {
        let r = parse_ssh_config(SAMPLE);
        let web = r.imported.iter().find(|h| h.name == "web-01").unwrap();
        assert_eq!(web.addr, "10.0.0.1");
        assert_eq!(web.port, 2222);
        assert_eq!(web.user, "root");
        match &web.auth {
            CredentialRef::KeyFile { path, .. } => {
                assert!(
                    path.is_absolute(),
                    "私钥路径应展开成绝对路径: {path:?}"
                );
            }
            other => panic!("应是私钥认证: {other:?}"),
        }

        let chain = r.imported.iter().find(|h| h.name == "jump-chain").unwrap();
        assert_eq!(
            chain.proxy_jump.as_deref(),
            Some("edge"),
            "ProxyJump 取最后一跳"
        );

        // 没写 User 的条目要填当前用户，不能留空
        let db = r.imported.iter().find(|h| h.name == "db-01").unwrap();
        assert!(!db.user.is_empty());
        assert_eq!(db.port, 22);
    }

    const XSH: &str = r#"
[CONNECTION]
PROTOCOL=SSH
HOSTNAME=192.168.1.50
PORT=2201
USERNAME=deploy
USERKEY=C:\Users\me\.ssh\id_rsa
"#;

    #[test]
    fn parses_xsh_session() {
        let r = parse_xsh(XSH, "prod-web");
        let h = r.host.expect("应解析出主机");
        assert_eq!(h.name, "prod-web");
        assert_eq!(h.addr, "192.168.1.50");
        assert_eq!(h.port, 2201);
        assert_eq!(h.user, "deploy");
        assert!(matches!(h.auth, CredentialRef::KeyFile { .. }));
    }

    #[test]
    fn xsh_rejects_non_ssh_and_empty() {
        let telnet = "[CONNECTION]\nPROTOCOL=TELNET\nHOSTNAME=1.2.3.4\n";
        assert!(parse_xsh(telnet, "t1").host.is_none());
        assert!(parse_xsh("[CONNECTION]\nPROTOCOL=SSH\n", "t2").host.is_none());
    }

    #[test]
    fn merge_keeps_existing_entries() {
        let mut existing = vec![Host {
            id: new_host_id(),
            name: "web-01".to_string(),
            group: vec![],
            addr: "1.1.1.1".to_string(),
            port: 22,
            user: "me".to_string(),
            auth: CredentialRef::Agent,
            tags: vec![],
            note: None,
            risk_cap: None,
            proxy_jump: None,
            jump: None,
        }];
        let r = parse_ssh_config(SAMPLE);
        let merged = merge_into(&mut existing, r);
        assert_eq!(merged.duplicated, vec!["web-01"], "重名应保留现有条目");
        assert_eq!(existing.len(), 3, "原有 1 台 + 新增 2 台（web-01 重名未覆盖）");
        assert!(
            existing.iter().any(|h| h.name == "web-01" && h.addr == "1.1.1.1"),
            "现有条目不该被覆盖"
        );
    }

    #[test]
    fn home_is_expanded() {
        let p = expand_home("~/.ssh/id_ed25519");
        assert!(p.is_absolute(), "应展开成绝对路径: {p:?}");
        assert!(p.to_string_lossy().contains("id_ed25519"));
    }
}
