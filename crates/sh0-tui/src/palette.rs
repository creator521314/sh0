//! 命令面板（需求 FR-3.6 / W5.1）：`Ctrl+Shift+P` 唤出，模糊搜索并执行命令。
//!
//! 与主机搜索（`/`）的分工：那边只找主机，这里找**一切可执行的东西**——
//! 内置动作、连接某台主机、切换主题。做成独立模块是为了能脱离终端测过滤逻辑。

use sh0_term::keymap::Action;

/// 面板里一条可执行项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteItem {
    /// 主标题，如 "重新连接"
    pub title: String,
    /// 右侧灰字提示，如键位或主机地址
    pub hint: String,
    /// 分类前缀，用于展示与排序：`动作` / `主机` / `主题`
    pub kind: &'static str,
    pub cmd: PaletteCmd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteCmd {
    /// 触发一个 TUI 内置动作
    Action(Action),
    /// 连接指定主机（存的是主机名，执行时再查树，避免持有 Host 副本）
    Connect(String),
    /// 切换配色主题
    Theme(&'static str),
}

/// 面板状态：输入框 + 当前选中行
#[derive(Debug, Clone, Default)]
pub struct Palette {
    pub input: String,
    pub sel: usize,
}

impl Palette {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, c: char) {
        self.input.push(c);
        self.sel = 0;
    }

    pub fn backspace(&mut self) {
        self.input.pop();
        self.sel = 0;
    }

    /// 在 `total` 项里循环移动选中行
    pub fn move_sel(&mut self, delta: isize, total: usize) {
        if total == 0 {
            self.sel = 0;
            return;
        }
        let cur = self.sel.min(total - 1) as isize;
        let next = (cur + delta).rem_euclid(total as isize);
        self.sel = next as usize;
    }
}

/// 候选集：内置动作 + 全部主机 + 主题。动作顺序按使用频率手排过。
pub fn items(hosts: &[(String, String)], current_theme: &str) -> Vec<PaletteItem> {
    const ACTIONS: [Action; 21] = [
        Action::HostSearch,
        Action::HostNew,
        Action::HostEdit,
        Action::HostDelete,
        Action::NewTab,
        Action::CloseTab,
        Action::NextTab,
        Action::PrevTab,
        Action::ToggleFocus,
        Action::ToggleFiles,
        Action::FileRefresh,
        Action::FileCopy,
        Action::BroadcastToggle,
        Action::BroadcastMark,
        Action::Reconnect,
        Action::ClearScreen,
        Action::ScrollUp,
        Action::ScrollDown,
        Action::ScrollTop,
        Action::ScrollBottom,
        Action::Quit,
    ];

    let mut out = Vec::with_capacity(ACTIONS.len() + hosts.len() + 3);
    for a in ACTIONS {
        out.push(PaletteItem {
            title: a.label().to_string(),
            hint: format!("{} ({})", "动作", a.as_str()),
            kind: "动作",
            cmd: PaletteCmd::Action(a),
        });
    }
    for (name, endpoint) in hosts {
        out.push(PaletteItem {
            title: format!("连接 {name}"),
            hint: endpoint.clone(),
            kind: "主机",
            cmd: PaletteCmd::Connect(name.clone()),
        });
    }
    for t in crate::theme::THEME_NAMES {
        let cur = t == current_theme;
        out.push(PaletteItem {
            title: format!("主题：{t}{}", if cur { "（当前）" } else { "" }),
            hint: "主题".to_string(),
            kind: "主题",
            cmd: PaletteCmd::Theme(t),
        });
    }
    out
}

/// 模糊过滤：查询串作为**子序列**命中标题即可（"recon" 命中 "重新连接" 靠的是 hint 里的
/// `session.reconnect`，所以标题与 hint 一起参与匹配）。
///
/// 打分规则：连续命中 > 分散命中，越靠前得分越高，据此排序。
pub fn filter<'a>(all: &'a [PaletteItem], query: &str) -> Vec<&'a PaletteItem> {
    if query.trim().is_empty() {
        return all.iter().collect();
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let mut scored: Vec<(usize, &'a PaletteItem)> = Vec::new();
    for it in all {
        if let Some(s) = score(it, &q) {
            scored.push((s, it));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, it)| it).collect()
}

/// 返回 None 表示未命中。分数越大越靠前。
fn score(it: &PaletteItem, q: &[char]) -> Option<usize> {
    let haystack: Vec<char> = format!("{} {}", it.title, it.hint)
        .to_lowercase()
        .chars()
        .collect();
    let mut qi = 0usize;
    let mut score = 0usize;
    let mut last_hit: Option<usize> = None;
    for (i, c) in haystack.iter().enumerate() {
        if qi < q.len() && *c == q[qi] {
            // 连着命中额外加分，越早命中分越高
            score += 10 + match last_hit {
                Some(p) if i == p + 1 => 8,
                _ => 0,
            } + (haystack.len().saturating_sub(i) / 8);
            last_hit = Some(i);
            qi += 1;
            if qi == q.len() {
                return Some(score);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Vec<(String, String)> {
        vec![
            ("web-01".into(), "root@10.0.0.1:22".into()),
            ("db-01".into(), "root@10.0.0.2:22".into()),
        ]
    }

    #[test]
    fn empty_query_returns_everything_in_order() {
        let all = items(&hosts(), "dark");
        let got = filter(&all, "");
        assert_eq!(got.len(), all.len());
        assert_eq!(got[0].title, all[0].title);
    }

    #[test]
    fn host_name_matches() {
        let all = items(&hosts(), "dark");
        let got = filter(&all, "web");
        assert!(!got.is_empty());
        assert!(got.iter().any(|i| i.title == "连接 web-01"));
        assert!(!got.iter().any(|i| i.title == "连接 db-01"));
    }

    #[test]
    fn action_id_matches_through_hint() {
        let all = items(&hosts(), "dark");
        let got = filter(&all, "reconnect");
        assert_eq!(got[0].title, "重新连接");
    }

    #[test]
    fn theme_entry_marks_the_current_one() {
        let all = items(&[], "light");
        let light = all.iter().find(|i| i.title.contains("light")).unwrap();
        assert!(light.title.contains("当前"));
        let dark = all.iter().find(|i| i.title.contains("dark")).unwrap();
        assert!(!dark.title.contains("当前"));
    }

    #[test]
    fn no_match_returns_empty() {
        let all = items(&hosts(), "dark");
        assert!(filter(&all, "zzzqqq").is_empty());
    }

    #[test]
    fn selection_wraps_around() {
        let mut p = Palette::new();
        p.move_sel(-1, 3);
        assert_eq!(p.sel, 2, "从第 0 项往上应绕到最后一项");
        p.move_sel(1, 3);
        assert_eq!(p.sel, 0);
        p.move_sel(1, 0);
        assert_eq!(p.sel, 0, "空列表不应越界");
    }

    #[test]
    fn typing_resets_selection() {
        let mut p = Palette::new();
        p.move_sel(1, 5);
        p.push('a');
        assert_eq!(p.sel, 0);
        p.backspace();
        assert!(p.input.is_empty());
    }
}
