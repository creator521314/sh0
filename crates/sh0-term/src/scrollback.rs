//! 滚动缓冲（scrollback）与全局内存预算（需求 FR-2.3 / NFR-3，验收 AC-2）。
//!
//! 需求写的是"默认 10000 行"，但 AC-2 又要求 20 个会话常驻内存 ≤ 120 MB。
//! 单会话满配（10000 × 120 × 16 B）约 19 MB，20 个就是 380 MB——两者直接冲突。
//!
//! 解决办法：**行数上限是软的，内存预算是硬的**。所有会话共享一个
//! [`MemoryBudget`]，超出时从最旧的缓冲区开始淘汰。于是单会话可以真的用到
//! 10000 行，而会话一多就自动收缩，总量恒定。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::cell::Line;

/// 每行估算的固定开销（Vec 分配头 + Line 结构）
const LINE_OVERHEAD: usize = 64;
const CELL_SIZE: usize = std::mem::size_of::<crate::cell::Cell>();

/// 全局内存预算：所有会话共享的 scrollback 字节账本
#[derive(Debug, Clone)]
pub struct MemoryBudget {
    used: Arc<AtomicUsize>,
    limit: usize,
}

impl MemoryBudget {
    pub fn new(limit_bytes: usize) -> Self {
        Self {
            used: Arc::new(AtomicUsize::new(0)),
            limit: limit_bytes,
        }
    }

    /// 默认 64 MB。留出余量给二进制本体、tokio 运行时与 20 条 SSH 连接。
    pub fn default_limit() -> usize {
        64 * 1024 * 1024
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    fn add(&self, n: usize) {
        self.used.fetch_add(n, Ordering::Relaxed);
    }

    fn sub(&self, n: usize) {
        self.used.fetch_sub(n.min(self.used.load(Ordering::Relaxed)), Ordering::Relaxed);
    }
}

impl Default for MemoryBudget {
    fn default() -> Self {
        Self::new(Self::default_limit())
    }
}

/// 环形滚动缓冲
#[derive(Debug)]
pub struct Scrollback {
    lines: VecDeque<Line>,
    /// 行数软上限
    max_lines: usize,
    bytes: usize,
    budget: Option<MemoryBudget>,
    /// 因容量被丢弃的行数，用于状态栏提示
    dropped: u64,
}

impl Scrollback {
    pub fn new(max_lines: usize, budget: Option<MemoryBudget>) -> Self {
        Self {
            lines: VecDeque::new(),
            max_lines: max_lines.max(1),
            bytes: 0,
            budget,
            dropped: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn clear(&mut self) {
        if let Some(b) = &self.budget {
            b.sub(self.bytes);
        }
        self.lines.clear();
        self.bytes = 0;
    }

    pub fn push(&mut self, line: Line) {
        let n = line_bytes(&line);
        self.lines.push_back(line);
        self.bytes += n;
        if let Some(b) = &self.budget {
            b.add(n);
        }
        self.trim();
    }

    pub fn extend(&mut self, lines: Vec<Line>) {
        if lines.is_empty() {
            return;
        }
        let n: usize = lines.iter().map(line_bytes).sum();
        self.lines.extend(lines);
        self.bytes += n;
        if let Some(b) = &self.budget {
            b.add(n);
        }
        self.trim();
    }

    /// 先按行数上限淘汰，再按全局内存预算淘汰
    fn trim(&mut self) {
        while self.lines.len() > self.max_lines {
            self.pop_front();
        }
        if let Some(b) = self.budget.clone() {
            while b.used() > b.limit() && !self.lines.is_empty() {
                self.pop_front();
            }
        }
    }

    fn pop_front(&mut self) {
        if let Some(l) = self.lines.pop_front() {
            let n = line_bytes(&l);
            self.bytes = self.bytes.saturating_sub(n);
            self.dropped += 1;
            if let Some(b) = &self.budget {
                b.sub(n);
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Line> {
        self.lines.iter()
    }

    /// 取出全部行（克隆）。供 `screen read` 与 plain 文本导出使用。
    pub fn to_vec(&self) -> Vec<Line> {
        self.lines.iter().cloned().collect()
    }
}

impl Drop for Scrollback {
    fn drop(&mut self) {
        if let Some(b) = &self.budget {
            b.sub(self.bytes);
        }
    }
}

fn line_bytes(line: &Line) -> usize {
    let comb: usize = line.comb.iter().map(|(_, s)| s.len()).sum();
    LINE_OVERHEAD + line.cells.len() * CELL_SIZE + comb
}
