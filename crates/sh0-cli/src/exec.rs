//! exec.run 的执行引擎（任务 W2.4）。
//!
//! 双模式（架构文档 §9 的备选路径在这里兑现）：
//!
//! - **shell 哨兵模式**（默认）：复用会话的交互 shell，发送
//!   `cmd\necho SH0_<ulid>:$?`，等哨兵行出现在屏幕里再截取输出。
//!   保留 cwd / 环境变量等交互上下文，但依赖终端回显，遇 `clear`
//!   或超长命令回行时按启发式处理。
//! - **exec channel 模式**（`--mode exec`）：独立 SSH channel 直接执行，
//!   stdout/stderr 分离、退出码精确，但拿不到交互上下文。
//!
//! 哨兵截取算法是本文件的核心，也是纯函数——"命令结束了吗、输出从哪到哪"
//! 这种判断错一次，AI 读到的就是残缺或错位的输出，因此必须可单测。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use sh0_core::RiskLevel;

/// 执行模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecMode {
    /// 交互 shell + 哨兵回显（默认）
    #[default]
    Shell,
    /// 独立 exec channel
    Exec,
}

impl ExecMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecMode::Shell => "shell",
            ExecMode::Exec => "exec",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "shell" => Some(ExecMode::Shell),
            "exec" => Some(ExecMode::Exec),
            _ => None,
        }
    }
}

/// 一次 exec 请求（daemon 内部表示，确认令牌里存的是它）
#[derive(Debug, Clone)]
pub struct ExecRequest {
    /// 会话 ID；None 表示用 host 选择器临时建连（FR-6.8 一键式调用）
    pub session: Option<String>,
    pub host: Option<String>,
    pub cmd: String,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub mode: ExecMode,
    /// 调用方身份：ai / tui / human（FR-6.9）
    pub actor: String,
    pub trace_id: Option<String>,
}

/// 执行结果的数据体（JSON 信封里的 `data`）
#[derive(Debug, Clone, Serialize)]
pub struct ExecData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub host: String,
    /// 远端退出码；exec channel 超时未拿到时为 null
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: i64,
    pub truncated: bool,
    pub mode: ExecMode,
    pub risk: RiskLevel,
    /// 执行超时：输出可能不完整，`sh0-ai` 据此映射退出码 4。
    ///
    /// 恒出现在 JSON 里（不 skip false）：AI 判读"输出是否完整"只看这一个字段，
    /// 让它有时缺席、有时是 false，等于逼调用方写 `timeout == true` 之外的分支。
    pub timeout: bool,
}

/// 待确认条目（FR-7.4 的无人值守令牌）
#[derive(Debug, Clone)]
pub struct PendingConfirm {
    pub req: ExecRequest,
    pub host_name: String,
    pub risk: RiskLevel,
    pub reason: Option<String>,
    pub expires_at: Instant,
}

/// 一次性确认令牌存储。令牌取出即消费——防止重放。
#[derive(Debug, Default)]
pub struct ConfirmStore {
    tokens: Mutex<HashMap<String, PendingConfirm>>,
}

impl ConfirmStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, token: String, p: PendingConfirm) {
        self.tokens.lock().unwrap().insert(token, p);
    }

    /// 取出（消费）一个未过期的令牌
    pub fn take(&self, token: &str) -> Option<PendingConfirm> {
        let mut tokens = self.tokens.lock().unwrap();
        let p = tokens.get(token)?;
        if Instant::now() > p.expires_at {
            tokens.remove(token);
            return None;
        }
        tokens.remove(token)
    }

    pub fn len(&self) -> usize {
        self.tokens.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.lock().unwrap().is_empty()
    }
}

/// 单 actor 每分钟调用上限（FR-7.7）。0 = 不限制。
#[derive(Debug, Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次调用；返回 false 表示已超限（本次不计入）。
    pub fn check(&self, actor: &str, limit_per_min: u32) -> bool {
        if limit_per_min == 0 {
            return true;
        }
        let now = Instant::now();
        let mut windows = self.windows.lock().unwrap();
        let q = windows.entry(actor.to_string()).or_default();
        while let Some(front) = q.front() {
            if now.duration_since(*front) > Duration::from_secs(60) {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() as u32 >= limit_per_min {
            return false;
        }
        q.push_back(now);
        true
    }
}

/// 哨兵扫描结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentinelScan {
    /// 哨兵已出现，拿到完整输出与退出码
    Complete { output: String, exit_code: i32 },
    /// 超时：只有部分输出，退出码未知
    Partial { output: String },
}

/// 从"发送前"与"发现哨兵后"的屏幕纯文本里截出命令输出。
///
/// 屏幕形态（`df -h /` 为例）：
///
/// ```text
/// user@host:~$ df -h /            ← 命令回显（含提示符）
/// Filesystem      Size  Used Avail Use% Mounted on
/// /dev/sda1        40G   18G   22G  46% /
/// user@host:~$ echo SH0_01Jxxx:$?  ← echo 命令的回显
/// SH0_01Jxxx:0                     ← 哨兵输出行（退出码在这里）
/// user@host:~$                     ← 下一个提示符（不计入）
/// ```
///
/// 启发式规则：
/// 0. 先按终端列宽把**物理折行**还原成逻辑行（见 [`merge_wrapped`]）；
/// 1. 哨兵行 = 去空白后恰为 `SENTINEL:<整数>` 的行（从后往前找第一行）；
/// 2. 输出起点 = `after` 中从 `start` 起第一行**包含命令文本**的下一行
///    （容忍回显前夹杂着上一条命令的残余输出）；
/// 3. 哨兵行前一行若含哨兵 id（echo 命令的回显），剥掉；
/// 4. `after` 不以 `before` 为前缀时（`clear` 刷屏 / 滚动丢失），从 0 开始找。
///
/// `cols` 为终端列宽；传 0 表示未知（跳过折行还原，行为同旧版）。
pub fn extract_output(
    before: &str,
    after: &str,
    cmd: &str,
    sentinel: &str,
    cols: usize,
) -> SentinelScan {
    let merged_before = merge_wrapped(before.lines().collect(), cols);
    let merged_after = merge_wrapped(after.lines().collect(), cols);
    let before_lines: Vec<&str> = merged_before.iter().map(|s| s.as_str()).collect();
    let after_lines: Vec<&str> = merged_after.iter().map(|s| s.as_str()).collect();
    let sentinel_pat = format!("{sentinel}:");

    // 1) 找哨兵输出行（从后往前）
    let mut sentinel_idx = None;
    let mut exit_code = None;
    for (i, line) in after_lines.iter().enumerate().rev() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix(&sentinel_pat)
            && let Ok(code) = rest.trim().parse::<i32>()
        {
            sentinel_idx = Some(i);
            exit_code = Some(code);
            break;
        }
    }

    // 命令回显探针：长命令会按终端宽度折行，取前 40 字符足够定位
    let probe: String = cmd.trim().chars().take(40).collect();
    let prefix_intact = after_lines.len() >= before_lines.len()
        && after_lines[..before_lines.len()] == before_lines[..];
    let start = if prefix_intact { before_lines.len() } else { 0 };

    // echo 命令的回显行含哨兵 id——它是我们自己塞进去的噪声，必须剥掉。
    // ⚠️ 不能只剥最后一行：真机上 PTY 常把两行输入**一起**回显，形态是
    //   `prompt$ cmd` / `echo SH0_x:$?` / <输出> / <哨兵行>
    // 此时回显行紧跟命令回显行、不在末尾（一键式会话尤其常见，shell 还没空闲）。
    // 哨兵 id 是随机 ULID，真实输出里不可能出现，所以整段过滤是安全的。
    // （直接调函数，不用闭包——闭包会把两个 &str 的生命周期绑在一起）

    let Some(sentinel_idx) = sentinel_idx else {
        // 2) 超时：尽力给出已有输出
        let first = find_echo_line(&after_lines, start, &probe);
        let out_start = skip_echo_block(
            &after_lines,
            first.min(after_lines.len()),
            after_lines.len(),
            &probe,
            &sentinel_pat,
        );
        let lines: Vec<&str> = after_lines[out_start.min(after_lines.len())..].to_vec();
        let out = join_trimmed(&drop_echo_lines(lines, &sentinel_pat));
        return SentinelScan::Partial { output: out };
    };

    // 3) 正常路径：输出区间 = (命令回显行, 哨兵行)
    let first = find_echo_line(&after_lines, start, &probe);
    let out_start = skip_echo_block(&after_lines, first, sentinel_idx, &probe, &sentinel_pat);
    let lines: Vec<&str> = after_lines[out_start..sentinel_idx].to_vec();
    let out = join_trimmed(&drop_echo_lines(lines, &sentinel_pat));
    SentinelScan::Complete {
        output: out,
        exit_code: exit_code.unwrap_or(-1),
    }
}

/// 把终端按列宽硬折的物理行还原成逻辑行。
///
/// PTY 折行不发换行符，网格里就是连续两行，且**续行既不含提示符也不含命令文本**——
/// 于是回显噪声剥不干净（续行被当成输出）、真实输出被切成碎片。
/// 唯一可用的信号是"上一行是否填满列宽"：折行必然填满，程序自己输出 `\n` 通常不满。
///
/// 留 1 列余量：readline 重绘命令行时会预留一列避免换行歧义。
/// `cols < 2` 时不做还原（宽度未知，硬猜比不猜更糟）。
fn merge_wrapped(lines: Vec<&str>, cols: usize) -> Vec<String> {
    if cols < 2 {
        return lines.into_iter().map(|s| s.to_string()).collect();
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut prev_filled = false;
    for line in lines {
        if prev_filled {
            // 上一条是被折断的，续行拼回去
            out.last_mut().expect("prev_filled 为真时必有上一条").push_str(line);
        } else {
            out.push(line.to_string());
        }
        // 只有"恰好填满"或"差一列"才算被折断。网格每行只有 cols 个 cell，
        // 物理行长度不可能超过 cols；超出说明这份文本不是真实网格（如单测构造），
        // 此时硬拼会吞掉本该独立的下一行。
        let len = line.chars().count();
        prev_filled = len >= cols - 1 && len <= cols;
    }
    out
}

/// 从 `from` 起找第一行包含探针文本的行，返回其下一行的下标；
/// 找不到返回 `from`（可能没有回显，如 exec 场景）
fn find_echo_line(lines: &[&str], from: usize, probe: &str) -> usize {
    if probe.is_empty() {
        return from;
    }
    for (i, line) in lines.iter().enumerate().skip(from) {
        if line.contains(probe) {
            return i + 1;
        }
    }
    from
}

/// 跳过紧跟在命令回显后面的"回显噪声块"。
///
/// 真机形态（bash + readline，一键式会话尤其明显）比预想的脏：
///
/// ```text
/// date                    ← PTY 对第一行输入的裸回显（提示符还没打印）
/// echo SH0_x:$?           ← 第二行输入的裸回显
/// hell@hell:~ $ date      ← readline 换行后重绘的提示符 + 命令
/// Sat 24 Oct 21:30:15 HKT 2026   ← 真正的输出
/// hell@hell:~ $ echo SH0_x:$?
/// SH0_x:0
/// ```
///
/// 只剥"第一行回显"会留下重绘的命令行，只剥"最后一行"会留下两块噪声。
/// 这里从首个回显行起连续跳过：含命令文本的行，或含哨兵 id 的行。
/// 上限 4 行——回显块不会更长，也免得命令输出里恰好出现命令文本时被整段吞掉。
fn skip_echo_block(
    lines: &[&str],
    start: usize,
    end: usize,
    probe: &str,
    sentinel_pat: &str,
) -> usize {
    let mut i = start;
    let mut prev_was_echo = false;
    let mut skipped = 0usize;
    while i < end && skipped < 4 {
        let l = lines[i];
        if l.contains(sentinel_pat) {
            // echo SH0_x:$? 的回显：哨兵 id 是随机 ULID，真实输出里不可能出现
            prev_was_echo = true;
            i += 1;
            skipped += 1;
            continue;
        }
        // 紧跟在 echo 回显行后面的、同样含命令文本的行 = readline 的重绘。
        // 只认这一种，避免把"输出里恰好含命令文本"的第一行也吞掉。
        if prev_was_echo && l.contains(probe) {
            prev_was_echo = false;
            i += 1;
            skipped += 1;
            continue;
        }
        break;
    }
    i
}

/// 剥掉所有含哨兵 id 的行：`echo SH0_x:$?` 这条命令是我们自己塞进去的，
/// 它的回显是噪声。真机上两行输入常被 PTY 一起回显，所以不能只剥最后一行。
fn drop_echo_lines<'a>(lines: Vec<&'a str>, sentinel_pat: &str) -> Vec<&'a str> {
    lines
        .into_iter()
        .filter(|l| !l.contains(sentinel_pat))
        .collect()
}

fn join_trimmed(lines: &[&str]) -> String {
    let mut out = lines.join("\n");
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// 生成哨兵标识：shell 安全（字母数字下划线）、不可预测。
/// 复用 core 的 ULID 生成器（sh0-cli 不直接依赖 ulid）。
pub fn new_sentinel() -> String {
    format!("SH0_{}", sh0_core::new_session_id())
}

/// 按上限截断输出。stdout / stderr 各自独立计数，
/// 任一被截断即置 `truncated`（AI 需要知道"还有我没看到的内容"）。
pub fn truncate_output(stdout: String, stderr: String, max_bytes: usize) -> (String, String, bool) {
    fn cut(s: &str, max_bytes: usize) -> (String, bool) {
        if s.len() <= max_bytes {
            return (s.to_string(), false);
        }
        // 按字符边界截断，避免把多字节字符切成两半
        let mut out = String::new();
        let mut used = 0;
        for c in s.chars() {
            let n = c.len_utf8();
            if used + n > max_bytes {
                break;
            }
            out.push(c);
            used += n;
        }
        (out, true)
    }

    let (so, t1) = cut(&stdout, max_bytes);
    let (se, t2) = cut(&stderr, max_bytes);
    (so, se, t1 || t2)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "SH0_01JTEST";

    fn screen(prompt: &str, cmd: &str, out: &str) -> String {
        format!(
            "{prompt} {cmd}\n{out}\n{prompt} echo {S}:$?\n{S}:0\n{prompt} "
        )
    }

    #[test]
    fn extracts_output_and_exit_code() {
        let before = "user@host:~$ ";
        let after = screen("user@host:~$", "df -h /", "Filesystem x\n/dev/sda1 40G");
        let scan = extract_output(before, &after, "df -h /", S, 80);
        assert_eq!(
            scan,
            SentinelScan::Complete {
                output: "Filesystem x\n/dev/sda1 40G".into(),
                exit_code: 0
            }
        );
    }

    #[test]
    fn exit_code_nonzero_is_captured() {
        let mut after = screen("user@host:~$", "false", "");
        after = after.replace(&format!("{S}:0"), &format!("{S}:1"));
        let scan = extract_output("p$ ", &after, "false", S, 80);
        assert_eq!(
            scan,
            SentinelScan::Complete {
                output: String::new(),
                exit_code: 1
            }
        );
    }

    #[test]
    fn empty_output_yields_empty_string() {
        let after = screen("user@host:~$", "true", "");
        let scan = extract_output("user@host:~$ ", &after, "true", S, 80);
        assert_eq!(
            scan,
            SentinelScan::Complete {
                output: String::new(),
                exit_code: 0
            }
        );
    }

    #[test]
    fn readline_repaint_is_not_part_of_output() {
        // 真机一键式形态：裸回显两行 → readline 重绘提示符+命令 → 输出 → 哨兵
        let before = "Last login: Sat Oct 24 21:29:42 2026 from 192.168.31.100";
        let after = format!(
            "{before}\ndate\necho {S}:$?\nhell@hell:~ $ date\nSat 24 Oct 21:30:15 HKT 2026\nhell@hell:~ $ echo {S}:$?\n{S}:0\nhell@hell:~ $ "
        );
        let scan = extract_output(before, &after, "date", S, 80);
        assert_eq!(
            scan,
            SentinelScan::Complete {
                output: "Sat 24 Oct 21:30:15 HKT 2026".into(),
                exit_code: 0
            }
        );
    }

    #[test]
    fn echo_block_skip_is_capped() {
        // 命令输出里恰好出现命令文本时，不能被"跳过回显块"整段吞掉（上限 4 行）
        let before = "p$ ";
        let after = format!(
            "p$ echo hi\necho {S}:$?\nhi\nhi\nhi\nhi\nhi\nhi\n{S}:0\np$ "
        );
        let scan = extract_output(before, &after, "echo hi", S, 80);
        let out = match scan {
            SentinelScan::Complete { output, .. } => output,
            other => panic!("应当是 Complete: {other:?}"),
        };
        assert!(out.contains("hi"), "输出不该被跳过逻辑吞掉: {out:?}");
    }

    #[test]
    fn two_line_echo_is_stripped_wherever_it_sits() {
        // 真机形态（一键式会话常见）：PTY 把两行输入一起回显，echo 回显行紧跟
        // 命令回显行、不在末尾。哨兵 id 只可能来自我们自己的 echo，整段过滤。
        let before = "hell@hell:~ $ ";
        let after = format!(
            "hell@hell:~ $ date\necho {S}:$?\nSat 24 Oct 21:27:05 HKT 2026\n{S}:0\nhell@hell:~ $ "
        );
        let scan = extract_output(before, &after, "date", S, 80);
        assert_eq!(
            scan,
            SentinelScan::Complete {
                output: "Sat 24 Oct 21:27:05 HKT 2026".into(),
                exit_code: 0
            }
        );
    }

    #[test]
    fn timeout_partial_also_drops_echo_line() {
        // 超时分支同样要把半截的 echo 回显剥掉
        let before = "hell@hell:~ $ ";
        let after = format!("hell@hell:~ $ sleep 30\necho {S}:$?\nsome partial\n");
        let scan = extract_output(before, &after, "sleep 30", S, 80);
        assert_eq!(
            scan,
            SentinelScan::Partial {
                output: "some partial".into()
            }
        );
    }

    #[test]
    fn cleared_screen_falls_back_to_full_scan() {
        // clear 之后 before 不再是前缀：起点退化为 0，仍靠回显行定位
        let after = screen("user@host:~$", "ls", "a\nb");
        let scan = extract_output("some old prompt$ ", &after, "ls", S, 80);
        assert_eq!(
            scan,
            SentinelScan::Complete {
                output: "a\nb".into(),
                exit_code: 0
            }
        );
    }

    #[test]
    fn pending_output_before_echo_is_skipped() {
        // 回显行之前还夹杂着上一条命令的残余输出
        let before = "user@host:~$ ";
        let after = format!(
            "user@host:~$ stale line from prev cmd\n{after}",
            after = screen("user@host:~$", "date", "Sat Oct 24")
        );
        // before 不是前缀（stale 行在中间），start=0；靠"包含 cmd 的行"定位
        let scan = extract_output(before, &after, "date", S, 80);
        assert_eq!(
            scan,
            SentinelScan::Complete {
                output: "Sat Oct 24".into(),
                exit_code: 0
            }
        );
    }

    #[test]
    fn multiline_output_is_kept_whole() {
        let after = screen("user@host:~$", "ps aux", "a\nb\nc\nd");
        let scan = extract_output("user@host:~$ ", &after, "ps aux", S, 80);
        assert_eq!(
            scan,
            SentinelScan::Complete {
                output: "a\nb\nc\nd".into(),
                exit_code: 0
            }
        );
    }

    #[test]
    fn no_sentinel_yields_partial() {
        // 命令卡在交互程序里，哨兵始终不出现
        let before = "user@host:~$ ";
        let after = "user@host:~$ vim nginx.conf\nsome vim screen";
        let scan = extract_output(before, after, "vim nginx.conf", S, 80);
        assert_eq!(
            scan,
            SentinelScan::Partial {
                output: "some vim screen".into()
            }
        );
    }

    #[test]
    fn wrapped_echo_block_is_rewrapped_before_stripping() {
        // 真机形态：80 列终端，155 字符的命令被 PTY 硬折成多行，
        // 续行既不含提示符也不含命令文本——必须先按列宽还原成逻辑行再剥噪声
        const COLS: usize = 80;
        let cmd = format!("echo {}", "a".repeat(150));
        let echo_cmd = format!("echo {}:$?", S);
        let real_out = "a".repeat(150);

        let fold = |s: &str| -> Vec<String> {
            let cs: Vec<char> = s.chars().collect();
            if cs.len() <= COLS {
                return vec![s.to_string()];
            }
            cs.chunks(COLS).map(|c| c.iter().collect::<String>()).collect()
        };

        let mut rows = vec!["p$ ".to_string()];
        rows.extend(fold(&cmd));                    // 裸回显命令（折行）
        rows.extend(fold(&echo_cmd));               // 裸回显 echo（含哨兵 id）
        rows.extend(fold(&format!("p$ {cmd}")));    // readline 重绘（折行）
        rows.extend(fold(&real_out));               // 真实输出（折行）
        rows.push(format!("p$ {echo_cmd}"));
        rows.push(format!("{S}:0"));
        let after = rows.join("\n");

        let scan = extract_output("p$ ", &after, &cmd, S, COLS);
        assert_eq!(
            scan,
            SentinelScan::Complete {
                output: real_out,
                exit_code: 0
            }
        );
    }

    #[test]
    fn unknown_width_skips_rewrapping() {
        // 列宽未知时不做还原，行为与旧版一致（短行不受影响）
        let before = "p$ ";
        let after = format!("p$ date\nSat 24 Oct\np$ echo {S}:$?\n{S}:0\np$ ");
        let scan = extract_output(before, &after, "date", S, 0);
        assert_eq!(
            scan,
            SentinelScan::Complete {
                output: "Sat 24 Oct".into(),
                exit_code: 0
            }
        );
    }

    #[test]
    fn long_command_probe_uses_first_40_chars() {
        let long = "echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let before = "p$ ";
        let after = format!("p$ {long}\nhello\np$ echo {S}:$?\n{S}:0\np$ ");
        let scan = extract_output(before, &after, long, S, 80);
        assert_eq!(
            scan,
            SentinelScan::Complete {
                output: "hello".into(),
                exit_code: 0
            }
        );
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let (so, se, t) = truncate_output("你好世界".into(), String::new(), 6);
        assert_eq!(so, "你好"); // 6 字节 = 2 个汉字
        assert_eq!(se, "");
        assert!(t);

        let (so, _, t) = truncate_output("short".into(), String::new(), 100);
        assert_eq!(so, "short");
        assert!(!t);
    }

    #[test]
    fn rate_limiter_sliding_window() {
        let rl = RateLimiter::new();
        assert!(rl.check("ai", 2));
        assert!(rl.check("ai", 2));
        assert!(!rl.check("ai", 2)); // 第 3 次超限
        assert!(rl.check("tui", 2)); // 不同 actor 独立计数
    }

    #[test]
    fn rate_limiter_zero_means_off() {
        let rl = RateLimiter::new();
        for _ in 0..100 {
            assert!(rl.check("ai", 0));
        }
    }

    #[test]
    fn confirm_token_is_single_use() {
        let store = ConfirmStore::new();
        store.insert(
            "cf_1".into(),
            PendingConfirm {
                req: ExecRequest {
                    session: None,
                    host: Some("web".into()),
                    cmd: "ls".into(),
                    timeout: Duration::from_secs(1),
                    max_output_bytes: 1024,
                    mode: ExecMode::Shell,
                    actor: "ai".into(),
                    trace_id: None,
                },
                host_name: "web".into(),
                risk: RiskLevel::Confirm,
                reason: None,
                expires_at: Instant::now() + Duration::from_secs(60),
            },
        );
        assert!(store.take("cf_1").is_some());
        assert!(store.take("cf_1").is_none()); // 第二次取不到
    }
}
