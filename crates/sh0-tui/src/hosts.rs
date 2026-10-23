//! 主机树：分组展示、模糊搜索、增删改（需求 FR-1.1 / FR-1.3 / W1.6）

use std::collections::HashMap;

use sh0_core::model::{CredentialRef, Host, SecretRef};

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

/// 主机编辑表单
#[derive(Debug, Clone)]
pub struct HostForm {
    /// 正在编辑的主机 id；None 表示新建
    pub editing: Option<String>,
    pub fields: Vec<FormField>,
    pub focus: usize,
}

#[derive(Debug, Clone)]
pub struct FormField {
    pub label: &'static str,
    pub value: String,
}

impl HostForm {
    pub fn new(host: Option<&Host>) -> Self {
        let auth = match host.map(|h| &h.auth) {
            Some(CredentialRef::Agent) | None => "agent",
            Some(CredentialRef::KeyFile { .. }) => "key",
            Some(CredentialRef::Password { .. }) => "password",
            Some(CredentialRef::Interactive) => "interactive",
            Some(CredentialRef::None) => "none",
        };
        let key_path = host.and_then(|h| match &h.auth {
            CredentialRef::KeyFile { path, .. } => Some(path.display().to_string()),
            _ => None,
        }).unwrap_or_default();

        Self {
            editing: host.map(|h| h.id.clone()),
            fields: vec![
                FormField { label: "名称", value: host.map(|h| h.name.clone()).unwrap_or_default() },
                FormField {
                    label: "分组(用/分隔)",
                    value: host.map(|h| h.group_path()).unwrap_or_default(),
                },
                FormField { label: "地址", value: host.map(|h| h.addr.clone()).unwrap_or_default() },
                FormField {
                    label: "端口",
                    value: host.map(|h| h.port.to_string()).unwrap_or_else(|| "22".into()),
                },
                FormField { label: "用户", value: host.map(|h| h.user.clone()).unwrap_or_default() },
                FormField { label: "认证(agent/key/password/none)", value: auth.into() },
                FormField { label: "私钥路径(key 时填)", value: key_path },
            ],
            focus: 0,
        }
    }

    pub fn next(&mut self) {
        self.focus = (self.focus + 1) % self.fields.len();
    }

    pub fn prev(&mut self) {
        self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
    }

    pub fn input(&mut self, c: char) {
        self.fields[self.focus].value.push(c);
    }

    pub fn backspace(&mut self) {
        self.fields[self.focus].value.pop();
    }

    /// 表单 → Host。校验失败时返回错误说明。
    pub fn to_host(&self) -> Result<Host, String> {
        let name = self.fields[0].value.trim();
        if name.is_empty() {
            return Err("名称不能为空".into());
        }
        let addr = self.fields[2].value.trim();
        if addr.is_empty() {
            return Err("地址不能为空".into());
        }
        let port: u16 = self.fields[3].value.trim().parse().map_err(|_| "端口必须是数字")?;
        let user = self.fields[4].value.trim().to_string();
        if user.is_empty() {
            return Err("用户不能为空".into());
        }

        let auth = match self.fields[5].value.trim() {
            "key" => {
                let path = std::path::PathBuf::from(self.fields[6].value.trim());
                if self.fields[6].value.trim().is_empty() {
                    return Err("选择 key 认证时必须填私钥路径".into());
                }
                CredentialRef::KeyFile { path, passphrase: None }
            }
            "password" => CredentialRef::Password { secret: SecretRef::Prompt },
            "interactive" => CredentialRef::Interactive,
            "none" => CredentialRef::None,
            _ => CredentialRef::Agent,
        };

        let group: Vec<String> = self.fields[1]
            .value
            .split('/')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(Host {
            id: self.editing.clone().unwrap_or_else(sh0_core::new_host_id),
            name: name.to_string(),
            group,
            addr: addr.to_string(),
            port,
            user,
            auth,
            tags: vec![],
            note: None,
        })
    }
}
