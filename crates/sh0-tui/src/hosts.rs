//! 主机树：分组展示、模糊搜索、增删改（需求 FR-1.1 / FR-1.3 / W1.6）
//!
//! 表单在 M6 起承担「新建连接」：除了地址/用户，还可以直接输入密码或
//! 导入私钥。密码/私钥不写进 hosts.toml，由 `HostDraft` 交给 app 层经
//! `credential.set` 写入 daemon 的加密库。

use std::collections::HashMap;

use sh0_core::model::{CredentialRef, Host, SecretRef};

/// 凭据在加密库里的 service 名（与旧 `sh0 secret set sh0 <account>` 一致）
pub const CRED_SERVICE: &str = "sh0";

/// 树里的一行：要么是一个分组，要么是一台主机
#[derive(Debug, Clone)]
pub enum Row {
    Group { path: String, count: usize, expanded: bool },
    // 装箱：Host 有 6 个 String/Vec 字段，直接内联会让每行占 256 字节，
    // 而行列表每次渲染都要重建一次。
    Host(Box<Host>),
}

#[derive(Debug, Default, Clone)]
pub struct HostTree {
    pub hosts: Vec<Host>,
    /// 搜索词。模糊匹配主机名的**子序列**，不区分大小写
    pub query: Option<String>,
    pub selected: usize,
    expanded: HashMap<String, bool>,
}

impl HostTree {
    pub fn new(hosts: Vec<Host>) -> Self {
        let mut expanded = HashMap::new();
        for h in &hosts {
            let path = h.group_path();
            if !path.is_empty() {
                expanded.entry(path).or_insert(true);
            }
        }
        Self { hosts, query: None, selected: 0, expanded }
    }

    /// 全部主机（不过滤、不折叠），命令面板与批量选择用
    pub fn hosts_snapshot(&self) -> Vec<Host> {
        self.hosts.clone()
    }

    /// 当前可见行（已应用搜索过滤与折叠状态）
    pub fn rows(&self) -> Vec<Row> {
        let hosts: Vec<&Host> = match &self.query {
            Some(q) if !q.is_empty() => self
                .hosts
                .iter()
                .filter(|h| fuzzy_match(q, &h.name) || fuzzy_match(q, &h.addr))
                .collect(),
            _ => self.hosts.iter().collect(),
        };

        let mut groups: Vec<(String, Vec<&Host>)> = Vec::new();
        let mut order: Vec<String> = Vec::new();
        for h in hosts {
            let path = h.group_path();
            if let Some(entry) = groups.iter_mut().find(|(p, _)| *p == path) {
                entry.1.push(h);
            } else {
                order.push(path.clone());
                groups.push((path, vec![h]));
            }
        }

        let mut out = Vec::new();
        for (path, items) in groups {
            if !path.is_empty() {
                let expanded = *self.expanded.get(&path).unwrap_or(&true);
                out.push(Row::Group { path: path.clone(), count: items.len(), expanded });
                if expanded {
                    for h in items {
                        out.push(Row::Host(Box::new(h.clone())));
                    }
                }
            } else {
                for h in items {
                    out.push(Row::Host(Box::new(h.clone())));
                }
            }
        }
        out
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.rows().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected as isize + delta;
        self.selected = cur.clamp(0, len as isize - 1) as usize;
    }

    /// 光标所在行
    pub fn current(&self) -> Option<Row> {
        self.rows().get(self.selected).cloned()
    }

    /// 光标所在主机（若停在分组上则取该组第一台）
    pub fn current_host(&self) -> Option<Host> {
        match self.current()? {
            Row::Host(h) => Some(*h),
            Row::Group { path, .. } => self
                .hosts
                .iter()
                .find(|h| h.group_path() == path)
                .cloned(),
        }
    }

    /// 展开/折叠当前分组，或者连接当前主机。返回是否应发起连接。
    pub fn activate(&mut self) -> Option<Host> {
        match self.current()? {
            Row::Group { path, .. } => {
                let cur = *self.expanded.get(&path).unwrap_or(&true);
                self.expanded.insert(path, !cur);
                None
            }
            Row::Host(h) => Some(*h),
        }
    }

    pub fn set_query(&mut self, q: String) {
        self.query = if q.is_empty() { None } else { Some(q) };
        self.selected = 0;
    }

    pub fn clear_query(&mut self) {
        self.query = None;
    }

    /// 新增或替换一台主机（编辑后按 id 覆盖）
    pub fn upsert(&mut self, host: Host) {
        if let Some(existing) = self.hosts.iter_mut().find(|h| h.id == host.id) {
            *existing = host;
        } else {
            self.hosts.push(host);
        }
    }

    /// 删除当前选中的主机
    pub fn remove_current(&mut self) -> Option<Host> {
        let host = self.current_host()?;
        self.hosts.retain(|h| h.id != host.id);
        let len = self.rows().len();
        if self.selected >= len && len > 0 {
            self.selected = len - 1;
        }
        Some(host)
    }
}

/// 子序列模糊匹配：输入 "wj" 能命中 "web-01-jp"
pub fn fuzzy_match(query: &str, target: &str) -> bool {
    let q = query.to_lowercase();
    let t = target.to_lowercase();
    if q.is_empty() {
        return true;
    }
    let mut it = t.chars();
    q.chars().all(|c| it.any(|tc| tc == c))
}

// ---------------------------------------------------------------------------
// 主机表单
// ---------------------------------------------------------------------------

/// 认证方式：表单里用 ←/→ 循环切换
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Agent,
    Password,
    Key,
    None,
}

impl AuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthMode::Agent => "agent",
            AuthMode::Password => "password",
            AuthMode::Key => "key",
            AuthMode::None => "none",
        }
    }

    fn parse(s: &str) -> Self {
        match s.trim() {
            "password" => AuthMode::Password,
            "key" => AuthMode::Key,
            "none" => AuthMode::None,
            _ => AuthMode::Agent,
        }
    }

    fn from_auth(auth: &CredentialRef) -> Self {
        match auth {
            CredentialRef::Agent => AuthMode::Agent,
            CredentialRef::Password { .. } => AuthMode::Password,
            // 已导入的私钥与外部私钥文件都归到 key 这一档；
            // TUI 保存时统一导入加密库（外部路径会被读入）。
            CredentialRef::Key { .. } | CredentialRef::KeyFile { .. } => AuthMode::Key,
            CredentialRef::Interactive => AuthMode::Agent,
            CredentialRef::None => AuthMode::None,
        }
    }

    fn cycle(self, delta: isize) -> Self {
        const ALL: [AuthMode; 4] = [
            AuthMode::Agent,
            AuthMode::Password,
            AuthMode::Key,
            AuthMode::None,
        ];
        let i = ALL.iter().position(|m| *m == self).unwrap_or(0) as isize;
        ALL[(i + delta).rem_euclid(ALL.len() as isize) as usize]
    }
}

#[derive(Debug, Clone)]
pub struct FormField {
    pub label: &'static str,
    pub value: String,
    /// 渲染时用 `*` 代替，且不进入任何日志
    pub secret: bool,
}

const F_NAME: usize = 0;
const F_GROUP: usize = 1;
const F_ADDR: usize = 2;
const F_PORT: usize = 3;
const F_USER: usize = 4;
const F_AUTH: usize = 5;
const F_PASSWORD: usize = 6;
const F_KEY_PATH: usize = 7;
const F_KEY_PASS: usize = 8;
const F_JUMP: usize = 9;

/// 表单校验通过后的产物：主机 + 待写入加密库的明文凭据。
///
/// 明文只在内存里短暂存在；app 层提交后立刻丢弃，不落日志、不写 hosts.toml。
/// 刻意不 derive Debug：避免哪天 `{:?}` 把密码打进日志。
pub struct HostDraft {
    pub host: Host,
    pub password: Option<String>,
    pub key_path: Option<String>,
    pub key_passphrase: Option<String>,
    /// 认证方式变化后需要清理的旧 account
    pub deletes: Vec<String>,
}

impl HostDraft {
    /// 读取私钥文件后，把「要写哪些 account、值是什么」算出来。
    pub fn cred_writes(&self, pem: Option<String>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(pw) = &self.password {
            out.push((self.host.id.clone(), pw.clone()));
        }
        if let Some(pem) = pem {
            out.push((format!("{}.key", self.host.id), pem));
        }
        if let Some(p) = &self.key_passphrase {
            out.push((format!("{}.key.pass", self.host.id), p.clone()));
        }
        out
    }
}

pub struct HostForm {
    /// 正在编辑的主机 id；None 表示新建
    pub editing: Option<String>,
    pub fields: Vec<FormField>,
    pub focus: usize,
    /// 编辑前的主机：用于「留空保持不变」与切换认证方式后的清理
    pub original: Option<Host>,
}

impl HostForm {
    pub fn new(host: Option<&Host>) -> Self {
        // 新建默认 password：大多数人的心智是「用户名 + 密码」，
        // 默认落在 agent 会让人填完地址却连不上。agent/key 用 ←/→ 切换。
        let mode = host
            .map(|h| AuthMode::from_auth(&h.auth))
            .unwrap_or(AuthMode::Password);
        let key_path = host
            .and_then(|h| match &h.auth {
                CredentialRef::KeyFile { path, .. } => Some(path.display().to_string()),
                _ => None,
            })
            .unwrap_or_default();

        Self {
            editing: host.map(|h| h.id.clone()),
            fields: vec![
                FormField { label: "名称", value: host.map(|h| h.name.clone()).unwrap_or_default(), secret: false },
                FormField {
                    label: "分组(用/分隔)",
                    value: host.map(|h| h.group_path()).unwrap_or_default(),
                    secret: false,
                },
                FormField { label: "地址", value: host.map(|h| h.addr.clone()).unwrap_or_default(), secret: false },
                FormField {
                    label: "端口",
                    value: host.map(|h| h.port.to_string()).unwrap_or_else(|| "22".into()),
                    secret: false,
                },
                FormField { label: "用户", value: host.map(|h| h.user.clone()).unwrap_or_default(), secret: false },
                FormField { label: "认证方式(左右)", value: mode.as_str().into(), secret: false },
                FormField { label: "密码(留空=不改)", value: String::new(), secret: true },
                FormField {
                    label: "私钥文件(留空=不改)",
                    value: key_path,
                    secret: false,
                },
                FormField { label: "私钥口令(可空)", value: String::new(), secret: true },
                FormField {
                    label: "跳板机(可空)",
                    value: host.and_then(|h| h.proxy_jump.clone()).unwrap_or_default(),
                    secret: false,
                },
            ],
            focus: 0,
            original: host.cloned(),
        }
    }

    pub fn auth_mode(&self) -> AuthMode {
        AuthMode::parse(&self.fields[F_AUTH].value)
    }

    /// 焦点是否在「认证方式」这一行（只有这时 ←/→ 才切换）
    pub fn auth_focused(&self) -> bool {
        self.focus == F_AUTH
    }

    /// 当前认证方式下这一行是否显示
    pub fn field_visible(&self, i: usize) -> bool {
        match i {
            F_PASSWORD => self.auth_mode() == AuthMode::Password,
            F_KEY_PATH | F_KEY_PASS => self.auth_mode() == AuthMode::Key,
            _ => i < self.fields.len(),
        }
    }

    pub fn next(&mut self) {
        self.step(1);
    }

    pub fn prev(&mut self) {
        self.step(-1);
    }

    fn step(&mut self, delta: isize) {
        let n = self.fields.len() as isize;
        let mut i = self.focus as isize;
        for _ in 0..n {
            i = (i + delta).rem_euclid(n);
            if self.field_visible(i as usize) {
                self.focus = i as usize;
                return;
            }
        }
    }

    pub fn input(&mut self, c: char) {
        if self.focus == F_AUTH || !self.field_visible(self.focus) {
            return;
        }
        self.fields[self.focus].value.push(c);
    }

    pub fn backspace(&mut self) {
        if self.focus == F_AUTH || !self.field_visible(self.focus) {
            return;
        }
        self.fields[self.focus].value.pop();
    }

    /// 切换认证方式；切换后旧方式的明文输入清空，避免误写
    pub fn cycle_auth(&mut self, delta: isize) {
        let mode = self.auth_mode().cycle(delta);
        self.fields[F_AUTH].value = mode.as_str().into();
        if mode != AuthMode::Password {
            self.fields[F_PASSWORD].value.clear();
        }
        if mode != AuthMode::Key {
            self.fields[F_KEY_PATH].value.clear();
            self.fields[F_KEY_PASS].value.clear();
        }
    }

    /// 表单 → 草稿。校验失败时返回人类可读的原因。
    pub fn to_host(&self) -> Result<HostDraft, String> {
        let name = self.fields[F_NAME].value.trim();
        if name.is_empty() {
            return Err("名称不能为空".into());
        }
        let addr = self.fields[F_ADDR].value.trim();
        if addr.is_empty() {
            return Err("地址不能为空".into());
        }
        let port: u16 = self.fields[F_PORT]
            .value
            .trim()
            .parse()
            .map_err(|_| "端口必须是数字")?;
        let user = self.fields[F_USER].value.trim().to_string();
        if user.is_empty() {
            return Err("用户不能为空".into());
        }

        let id = self.editing.clone().unwrap_or_else(sh0_core::new_host_id);
        let mode = self.auth_mode();

        let mut password = None;
        let mut key_path = None;
        let mut key_passphrase = None;
        let auth = match mode {
            AuthMode::Agent => CredentialRef::Agent,
            AuthMode::None => CredentialRef::None,
            AuthMode::Password => {
                let pw = self.fields[F_PASSWORD].value.clone();
                if pw.is_empty() {
                    // 编辑时留空：原样保留引用（兼容旧 account = 主机名），
                    // 绝不能趁机改写成 id，否则旧密码会变成孤儿并被 deletes 清掉
                    if let Some(orig) = &self.original
                        && let CredentialRef::Password { secret } = &orig.auth
                    {
                        CredentialRef::Password {
                            secret: secret.clone(),
                        }
                    } else {
                        return Err("密码不能为空（或改用 agent/key 认证）".into());
                    }
                } else {
                    password = Some(pw);
                    CredentialRef::Password {
                        secret: keyring_ref(&id),
                    }
                }
            }
            AuthMode::Key => {
                let path = self.fields[F_KEY_PATH].value.trim().to_string();
                let pass = self.fields[F_KEY_PASS].value.clone();
                if path.is_empty() {
                    // 只有「已导入加密库」的私钥允许留空；外部 key_file 留空会
                    // 变成一条没有 PEM 的 Key，连接必然失败。
                    if let Some(orig) = &self.original
                        && let CredentialRef::Key { secret, passphrase } = &orig.auth
                    {
                        CredentialRef::Key {
                            secret: secret.clone(),
                            passphrase: passphrase.clone(),
                        }
                    } else {
                        return Err("请填私钥文件路径（或改用 agent 认证）".into());
                    }
                } else {
                    key_path = Some(path);
                    if !pass.is_empty() {
                        key_passphrase = Some(pass);
                    }
                    CredentialRef::Key {
                        secret: keyring_ref(&format!("{id}.key")),
                        passphrase: key_passphrase
                            .as_ref()
                            .map(|_| keyring_ref(&format!("{id}.key.pass"))),
                    }
                }
            }
        };

        let group: Vec<String> = self.fields[F_GROUP]
            .value
            .split('/')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // 表单不编辑 tags / note / risk_cap，编辑时必须原样保留，不能清空
        let (tags, note, risk_cap) = match &self.original {
            Some(h) => (h.tags.clone(), h.note.clone(), h.risk_cap),
            None => (vec![], None, None),
        };
        let host = Host {
            id,
            name: name.to_string(),
            group,
            addr: addr.to_string(),
            port,
            user,
            auth,
            tags,
            note,
            risk_cap,
            proxy_jump: {
                let j = self.fields[F_JUMP].value.trim();
                if j.is_empty() { None } else { Some(j.to_string()) }
            },
            jump: None,
        };

        // 认证方式变了：旧 account 不再被新主机引用，提交成功后清掉
        let old = self
            .original
            .as_ref()
            .map(accounts_of_host)
            .unwrap_or_default();
        let new = accounts_of_host(&host);
        let deletes = old.into_iter().filter(|a| !new.contains(a)).collect();

        Ok(HostDraft {
            host,
            password,
            key_path,
            key_passphrase,
            deletes,
        })
    }
}

fn keyring_ref(account: &str) -> SecretRef {
    SecretRef::Keyring {
        service: CRED_SERVICE.to_string(),
        account: account.to_string(),
    }
}

/// 一台主机引用了哪些加密库 account（删除/切换认证方式时用来清理）
pub fn accounts_of_host(host: &Host) -> Vec<String> {
    let mut out = Vec::new();
    match &host.auth {
        CredentialRef::Password { secret } => push_account(&mut out, secret),
        CredentialRef::Key { secret, passphrase } => {
            push_account(&mut out, secret);
            if let Some(p) = passphrase {
                push_account(&mut out, p);
            }
        }
        CredentialRef::KeyFile {
            passphrase: Some(p),
            ..
        } => push_account(&mut out, p),
        _ => {}
    }
    out
}

fn push_account(out: &mut Vec<String>, r: &SecretRef) {
    if let SecretRef::Keyring { account, .. } = r
        && !out.contains(account)
    {
        out.push(account.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_form() -> HostForm {
        let mut f = HostForm::new(None);
        f.fields[F_NAME].value = "web-01".into();
        f.fields[F_ADDR].value = "10.0.0.11".into();
        f.fields[F_USER].value = "root".into();
        f.fields[F_PORT].value = "22".into();
        f
    }

    fn host_with(auth: CredentialRef) -> Host {
        Host {
            id: "01TESTHOSTID0000000000000".into(),
            name: "web-01".into(),
            group: vec![],
            addr: "10.0.0.11".into(),
            port: 22,
            user: "root".into(),
            auth,
            tags: vec![],
            note: None,
            risk_cap: None,
            proxy_jump: None,
            jump: None,
        }
    }

    #[test]
    fn password_form_builds_keyring_ref_by_host_id() {
        let mut f = base_form();
        f.fields[F_AUTH].value = "password".into();
        f.fields[F_PASSWORD].value = "pw".into();
        let d = f.to_host().unwrap();
        assert!(d.password.as_deref() == Some("pw"));
        assert!(d.key_path.is_none());
        match &d.host.auth {
            CredentialRef::Password { secret } => match secret {
                SecretRef::Keyring { service, account } => {
                    assert_eq!(service, CRED_SERVICE);
                    assert_eq!(account, &d.host.id);
                }
                other => panic!("应为 keyring 引用: {other:?}"),
            },
            other => panic!("应为密码认证: {other:?}"),
        }
        assert!(d.deletes.is_empty());
    }

    #[test]
    fn blank_password_on_edit_keeps_existing_secret() {
        let orig = host_with(CredentialRef::Password {
            secret: SecretRef::Keyring {
                service: CRED_SERVICE.into(),
                account: "01TESTHOSTID0000000000000".into(),
            },
        });
        let f = HostForm::new(Some(&orig));
        // 编辑时密码留空：不产生覆盖写入
        let d = f.to_host().unwrap();
        assert!(d.password.is_none());
        assert_eq!(d.host.id, orig.id);
    }

    #[test]
    fn blank_password_edit_preserves_legacy_account() {
        // 旧版可能用主机名作 account；编辑时密码留空必须原样保留，
        // 否则会把旧密码改成 id 引用并当作孤儿删掉。
        let orig = host_with(CredentialRef::Password {
            secret: SecretRef::Keyring {
                service: CRED_SERVICE.into(),
                account: "web-01".into(),
            },
        });
        let f = HostForm::new(Some(&orig));
        let d = f.to_host().unwrap();
        match &d.host.auth {
            CredentialRef::Password { secret } => match secret {
                SecretRef::Keyring { account, .. } => assert_eq!(account, "web-01"),
                other => panic!("应为 keyring 引用: {other:?}"),
            },
            other => panic!("应为密码认证: {other:?}"),
        }
        assert!(d.password.is_none());
        assert!(d.deletes.is_empty(), "不该把仍在用的旧 account 删掉");
    }

    #[test]
    fn key_form_imports_pem_under_id_account() {
        let mut f = base_form();
        f.fields[F_AUTH].value = "key".into();
        f.fields[F_KEY_PATH].value = "/home/u/.ssh/id_ed25519".into();
        let d = f.to_host().unwrap();
        assert_eq!(d.key_path.as_deref(), Some("/home/u/.ssh/id_ed25519"));
        match &d.host.auth {
            CredentialRef::Key { secret, passphrase } => {
                assert!(passphrase.is_none());
                match secret {
                    SecretRef::Keyring { account, .. } => {
                        assert_eq!(account, &format!("{}.key", d.host.id));
                    }
                    other => panic!("应为 keyring 引用: {other:?}"),
                }
            }
            other => panic!("应为存储型私钥: {other:?}"),
        }
    }

    #[test]
    fn switching_auth_mode_marks_old_account_for_delete() {
        let orig = host_with(CredentialRef::Password {
            secret: SecretRef::Keyring {
                service: CRED_SERVICE.into(),
                account: "old-account".into(),
            },
        });
        let mut f = HostForm::new(Some(&orig));
        f.fields[F_AUTH].value = "key".into();
        f.fields[F_KEY_PATH].value = "/home/u/.ssh/id_ed25519".into();
        let d = f.to_host().unwrap();
        assert_eq!(d.deletes, vec!["old-account".to_string()]);
    }

    #[test]
    fn edit_preserves_fields_not_in_form() {
        let mut orig = host_with(CredentialRef::Password {
            secret: SecretRef::Keyring {
                service: CRED_SERVICE.into(),
                account: "acc".into(),
            },
        });
        orig.tags = vec!["prod".into()];
        orig.note = Some("别删我".into());
        orig.risk_cap = Some(sh0_core::RiskLevel::Confirm);
        let f = HostForm::new(Some(&orig));
        let d = f.to_host().unwrap();
        assert_eq!(d.host.tags, vec!["prod".to_string()]);
        assert_eq!(d.host.note.as_deref(), Some("别删我"));
        assert_eq!(d.host.risk_cap, Some(sh0_core::RiskLevel::Confirm));
    }

    #[test]
    fn auth_fields_follow_selected_mode() {
        let mut f = base_form();
        // 新建默认 password，符合「用户名 + 密码」的直觉
        assert_eq!(f.auth_mode(), AuthMode::Password);
        assert!(f.field_visible(F_PASSWORD));
        assert!(!f.field_visible(F_KEY_PATH));
        f.cycle_auth(1); // password -> key
        assert_eq!(f.auth_mode(), AuthMode::Key);
        assert!(f.field_visible(F_KEY_PATH));
        assert!(!f.field_visible(F_PASSWORD));
        f.cycle_auth(1); // key -> none
        f.cycle_auth(1); // none -> agent
        assert_eq!(f.auth_mode(), AuthMode::Agent);
        assert!(!f.field_visible(F_PASSWORD));
        assert!(!f.field_visible(F_KEY_PATH));
    }

    #[test]
    fn accounts_of_host_collects_password_and_key_refs() {
        let h = host_with(CredentialRef::Key {
            secret: SecretRef::Keyring {
                service: CRED_SERVICE.into(),
                account: "a.key".into(),
            },
            passphrase: Some(SecretRef::Keyring {
                service: CRED_SERVICE.into(),
                account: "a.key.pass".into(),
            }),
        });
        assert_eq!(
            accounts_of_host(&h),
            vec!["a.key".to_string(), "a.key.pass".to_string()]
        );
    }
}
