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

use sh0_core::session::SessionHandle;
use sh0_core::{HostSelector, RiskLevel};

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

/// 一次批量请求（FR-5.2）。
///
/// 与 `ExecRequest` 的关键差别是目标从"一台"变成"一组"，
/// 因此风险判定与配额都按**整批**算一次（FR-5.5），而不是每台各算一次。
#[derive(Debug, Clone)]
pub struct BatchRequest {
    pub selector: HostSelector,
    pub cmd: String,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub mode: ExecMode,
    /// 并发上限（FR-5.4）
    pub concurrency: usize,
    /// 有失败就尽快停下，剩余主机标记为 skipped
    pub fail_fast: bool,
    pub actor: String,
    pub trace_id: Option<String>,
}

impl BatchRequest {
    /// 退化成单台请求，供确认令牌复用 exec 的存储结构（FR-5.5）
    pub fn as_exec_request(&self) -> ExecRequest {
        ExecRequest {
            session: None,
            host: self.selector.hosts.first().cloned(),
            cmd: self.cmd.clone(),
            timeout: self.timeout,
            max_output_bytes: self.max_output_bytes,
            mode: self.mode,
            actor: self.actor.clone(),
            trace_id: self.trace_id.clone(),
        }
    }
}

/// 从 IPC params 解析批量请求；参数不合法一律 `invalid_args`
pub fn parse_batch_request(
    params: &serde_json::Value,
    default_timeout_secs: u64,
    default_max_bytes: usize,
    default_concurrency: usize,
) -> std::result::Result<BatchRequest, ErrorBodyLite> {
    let mut hosts: Vec<String> = params
        .get("hosts")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // 逗号分隔的便捷写法：`--host a,b,c`
    if let Some(s) = params.get("host").and_then(|v| v.as_str())
        && !s.is_empty()
    {
        hosts.extend(s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()));
    }
    let group = params
        .get("group")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let tag = params
        .get("tag")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let all = params.get("all").and_then(|v| v.as_bool()).unwrap_or(false);

    let selector = HostSelector {
        hosts,
        group,
        tag,
        all,
    };
    if selector.is_empty() {
        return Err(ErrorBodyLite {
            code: "invalid_args".into(),
            message: "需要 --host / --group / --tag / --all 之一指定目标主机".into(),
        });
    }
    let cmd = params
        .get("cmd")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if cmd.trim().is_empty() {
        return Err(ErrorBodyLite {
            code: "invalid_args".into(),
            message: "命令不能为空".into(),
        });
    }
    let mode = match params.get("mode").and_then(|v| v.as_str()) {
        Some(m) => ExecMode::parse(m).ok_or_else(|| ErrorBodyLite {
            code: "invalid_args".into(),
            message: format!("未知 mode: {m}（可选 shell / exec）"),
        })?,
        // 批量默认走 exec channel：一批机器各建一条一次性连接，
        // 不为每台维护会话，30 台并行时才有意义
        None => ExecMode::Exec,
    };
    let timeout = Duration::from_secs(
        params
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .map(|ms| ms / 1000)
            .unwrap_or(default_timeout_secs)
            .clamp(1, 3600),
    );
    let max_output_bytes = params
        .get("max_output_bytes")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(default_max_bytes)
        .clamp(1024, 16 * 1024 * 1024);
    let concurrency = params
        .get("concurrency")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(default_concurrency)
        .clamp(1, 256);

    Ok(BatchRequest {
        selector,
        cmd,
        timeout,
        max_output_bytes,
        mode,
        concurrency,
        fail_fast: params.get("fail_fast").and_then(|v| v.as_bool()).unwrap_or(false),
        actor: params
            .get("actor")
            .and_then(|v| v.as_str())
            .unwrap_or("ai")
            .to_string(),
        trace_id: params
            .get("trace_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

/// exec.rs 不该依赖 daemon 的错误类型，这里放一个最小错误体
#[derive(Debug, Clone)]
pub struct ErrorBodyLite {
    pub code: String,
    pub message: String,
}

/// 一次性会话等待就绪的宽限（秒）：连接失败不该把 exec 的整体超时吃光
const CONNECT_GRACE_SECS: u64 = 20;

/// 屏幕文本里扫描哨兵**输出行**（整行恰为 `SENTINEL:<整数>`），返回其退出码。
///
/// 与 `extract_output` 内部分开：那边输入是"已判定出现"的文本；
/// 这边在轮询里被反复调用，提前命中就能少等一轮。
/// 只管"跑完了没有"——输出内容由 `extract_output` 结合屏底样截取。
pub fn scan_sentinel(text: &str, sentinel: &str) -> Option<i32> {
    let pat = format!("{sentinel}:");
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix(&pat)
            && let Ok(code) = rest.trim().parse::<i32>()
        {
            return Some(code);
        }
    }
    None
}

/// 在一个会话上跑一条命令（shell 哨兵法）。
///
/// 抽成自由函数而非 `DaemonState` 的方法，是因为批量执行要在并发任务里
/// 跑它——tokio 任务要求 'static，拿不到 `&DaemonState`（FR-5.4）。
pub async fn exec_sentinel_on(
    handle: &SessionHandle,
    cmd: &str,
    timeout: Duration,
    max_output_bytes: usize,
    host_name: &str,
) -> std::result::Result<ExecData, ErrorBodyLite> {
    use sh0_core::SessionState;

    let started = Instant::now();

    // 一次性会话：等它就绪（或失败）。别把整体超时吃光。
    if handle.state().await != SessionState::Ready {
        let grace = Duration::from_secs(CONNECT_GRACE_SECS).min(timeout);
        let deadline = Instant::now() + grace;
        loop {
            let st = handle.state().await;
            if st == SessionState::Ready {
                break;
            }
            if st == SessionState::Closed {
                let snap = handle.snapshot().await;
                return Err(ErrorBodyLite {
                    code: "session_unavailable".into(),
                    message: format!(
                        "连接失败: {}",
                        snap.last_error.unwrap_or_else(|| "会话已关闭".into())
                    ),
                });
            }
            if Instant::now() >= deadline {
                return Err(ErrorBodyLite {
                    code: "session_unavailable".into(),
                    message: format!("等待连接就绪超时（{grace:?} 后仍未 ready）"),
                });
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    let sentinel = new_sentinel();
    let screen = handle.screen();
    let cols = {
        let s = screen.lock().await;
        s.cols()
    };
    let before = {
        let s = screen.lock().await;
        s.plain_text(true)
    };
    let payload = format!("{cmd}\necho {sentinel}:$?\n");
    handle
        .send(payload.as_bytes())
        .await
        .map_err(|e| ErrorBodyLite {
            code: "session_unavailable".into(),
            message: format!("发送命令失败: {e}"),
        })?;

    let deadline = started + timeout;
    let mut last_after;
    let (scan, timed_out) = loop {
        let after = {
            let s = screen.lock().await;
            s.plain_text(true)
        };
        last_after = after.clone();
        if scan_sentinel(&after, &sentinel).is_some() {
            break (
                extract_output(&before, &after, cmd, &sentinel, cols),
                false,
            );
        }
        if Instant::now() >= deadline {
            break (extract_output(&before, &after, cmd, &sentinel, cols), true);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    tracing::debug!(
        "exec 抓取 timed_out={timed_out} sentinel={sentinel} before={before:?} after={last_after:?}"
    );
    let (raw_stdout, exit_code) = match scan {
        SentinelScan::Complete { output, exit_code } => (output, Some(exit_code)),
        SentinelScan::Partial { output } => (output, None),
    };
    let (stdout, stderr, truncated) = truncate_output(raw_stdout, String::new(), max_output_bytes);

    Ok(ExecData {
        session_id: Some(handle.id.clone()),
        host: host_name.to_string(),
        exit_code,
        stdout,
        stderr,
        duration_ms: started.elapsed().as_millis() as i64,
        truncated,
        mode: ExecMode::Shell,
        risk: RiskLevel::Auto,
        timeout: timed_out,
    })
}

/// 待确认条目（FR-7.4 的无人值守令牌）
#[derive(Debug, Clone)]
pub struct PendingConfirm {
    pub req: ExecRequest,
    pub host_name: String,
    pub risk: RiskLevel,
    pub reason: Option<String>,
    pub expires_at: Instant,
    /// 批量请求（FR-5.5）：Some 表示这次确认放行的是一整批
    pub batch: Option<BatchRequest>,
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
                batch: None,
            },
        );
        assert!(store.take("cf_1").is_some());
        assert!(store.take("cf_1").is_none()); // 第二次取不到
    }
}
