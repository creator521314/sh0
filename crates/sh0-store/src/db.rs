//! SQLite 审计库与输出归档（需求 FR-8.1~FR-8.4 / FR-9.2）。
//!
//! 落库的两条铁律：
//! 1. **先脱敏再落库**（FR-8.3）——密码、token、私钥内容进了库就等于明文落盘；
//! 2. **daemon 崩溃不能丢已确认的记录**——每条记录一个事务，提交即持久。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};

/// 一条审计记录（FR-8.1 的字段全集）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    /// RFC3339 UTC 时间戳
    pub ts: String,
    /// 调用方传入的 trace id（FR-6.9）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,
    /// actor：ai / tui / human
    pub actor: String,
    /// 动作类型：exec.run / session.open / session.close …
    pub action: String,
    /// 原始命令（脱敏后）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// 风险档位：auto / confirm / deny
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
    /// 决策结果：executed / denied / needs_confirmation / confirmed / timeout
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    // ---- 以下为归档字段，仅 archive_output 开启时落库 ----
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

/// 一次批量执行的落库形态（OQ-5：批量历史回看）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRunRecord {
    pub run_id: String,
    pub ts: String,
    pub trace_id: Option<String>,
    pub actor: String,
    pub command: String,
    pub mode: String,
    pub risk: String,
    pub decision: String,
    pub concurrency: usize,
    pub total: usize,
    pub ok: usize,
    pub failed: usize,
    pub skipped: usize,
    pub duration_ms: i64,
    pub results: Vec<BatchResultRow>,
}

/// 批量执行里单台主机的结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResultRow {
    pub host: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub duration_ms: i64,
    pub truncated: bool,
    pub timeout: bool,
    pub stdout: String,
    pub error: Option<String>,
}

/// 列表项：不含逐台明细（30 台的明细不该在列表里反复搬运）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRunSummary {
    pub run_id: String,
    pub ts: String,
    /// 调用链追踪 ID：批量往往是 AI 编排里的一步，回看要能对上号
    pub trace_id: Option<String>,
    pub actor: String,
    pub command: String,
    pub mode: String,
    pub risk: String,
    pub decision: String,
    pub concurrency: i64,
    pub total: i64,
    pub ok: i64,
    pub failed: i64,
    pub skipped: i64,
    pub duration_ms: i64,
}

/// 一次批量执行的完整视图：汇总 + 逐台明细
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRunDetail {
    pub run: BatchRunSummary,
    pub results: Vec<BatchResultRow>,
}

/// 查询过滤条件（`ai log query`）
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub host: Option<String>,
    pub session: Option<String>,
    pub actor: Option<String>,
    pub risk: Option<String>,
    /// RFC3339 或日期（"2026-10-20"），ts >= since
    pub since: Option<String>,
    pub limit: Option<usize>,
}

/// 打开（或创建）审计库。`archive_output` 与保留策略来自 `cfg.audit`。
pub struct AuditDb {
    conn: Mutex<Connection>,
    archive_output: bool,
    retention_days: u32,
    max_db_mb: u32,
    /// 距上次清理写入多少条后再次清理（清理本身有 I/O 成本）
    writes_since_cleanup: std::sync::atomic::AtomicU32,
}

impl AuditDb {
    pub fn open(path: &Path, archive_output: bool, retention_days: u32, max_db_mb: u32) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).map_err(|e| StoreError::Db(e.to_string()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts TEXT NOT NULL,
                trace_id TEXT,
                session_id TEXT,
                host_name TEXT,
                actor TEXT NOT NULL,
                action TEXT NOT NULL,
                command TEXT,
                risk TEXT,
                decision TEXT,
                exit_code INTEGER,
                duration_ms INTEGER,
                truncated INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                stdout TEXT,
                stderr TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit(ts);
            CREATE INDEX IF NOT EXISTS idx_audit_host ON audit(host_name);
            CREATE INDEX IF NOT EXISTS idx_audit_actor ON audit(actor);
            CREATE INDEX IF NOT EXISTS idx_audit_session ON audit(session_id);
            CREATE TABLE IF NOT EXISTS meta (
                k TEXT PRIMARY KEY,
                v TEXT NOT NULL
            );
            -- M3：批量执行历史（OQ-5）。一条 run 对多台 result，
            -- run 表里只放汇总，明细单独一张表，列表页才不会搬一堆输出。
            CREATE TABLE IF NOT EXISTS batch_run (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL UNIQUE,
                ts TEXT NOT NULL,
                trace_id TEXT,
                actor TEXT NOT NULL,
                command TEXT,
                mode TEXT,
                risk TEXT,
                decision TEXT,
                concurrency INTEGER,
                total INTEGER,
                ok INTEGER,
                failed INTEGER,
                skipped INTEGER,
                duration_ms INTEGER
            );
            CREATE TABLE IF NOT EXISTS batch_result (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                host TEXT NOT NULL,
                status TEXT NOT NULL,
                exit_code INTEGER,
                duration_ms INTEGER,
                truncated INTEGER NOT NULL DEFAULT 0,
                timeout INTEGER NOT NULL DEFAULT 0,
                stdout TEXT,
                error TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_batch_run_ts ON batch_run(ts);
            CREATE INDEX IF NOT EXISTS idx_batch_result_run ON batch_result(run_id);",
        )
        .map_err(|e| StoreError::Db(e.to_string()))?;

        let db = Self {
            conn: Mutex::new(conn),
            archive_output,
            retention_days,
            max_db_mb,
            writes_since_cleanup: std::sync::atomic::AtomicU32::new(0),
        };
        db.cleanup()?;
        Ok(db)
    }

    /// 写入一条记录，返回行 id。归档字段受 `archive_output` 控制。
    pub fn record(&self, rec: &AuditRecord) -> Result<i64> {
        // 铁律 1：脱敏发生在一切之前
        let command = rec.command.as_deref().map(redact);
        let (stdout, stderr) = if self.archive_output {
            (rec.stdout.as_deref().map(redact), rec.stderr.as_deref().map(redact))
        } else {
            (None, None)
        };

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO audit
                (ts, trace_id, session_id, host_name, actor, action, command,
                 risk, decision, exit_code, duration_ms, truncated, error, stdout, stderr)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                rec.ts,
                rec.trace_id,
                rec.session_id,
                rec.host_name,
                rec.actor,
                rec.action,
                command,
                rec.risk,
                rec.decision,
                rec.exit_code,
                rec.duration_ms,
                rec.truncated as i32,
                rec.error,
                stdout,
                stderr,
            ],
        )
        .map_err(|e| StoreError::Db(e.to_string()))?;
        let id = conn.last_insert_rowid();

        // 每 100 条做一次保留/体积清理，避免每次写入都付 I/O 代价
        let n = self
            .writes_since_cleanup
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        drop(conn);
        if n % 100 == 99 {
            self.cleanup()?;
        }
        Ok(id)
    }

    /// 落一次批量执行（汇总 + 逐台明细，同一事务）。
    ///
    /// 明细里的 stdout 与审计归档同策略：`archive_output` 关闭时不存，
    /// 否则 30 台的输出会把库撑大，而"哪台失败了"靠 status 就够看。
    pub fn record_batch(&self, rec: &BatchRunRecord) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn
            .transaction()
            .map_err(|e| StoreError::Db(e.to_string()))?;
        tx.execute(
            "INSERT INTO batch_run
                (run_id, ts, trace_id, actor, command, mode, risk, decision,
                 concurrency, total, ok, failed, skipped, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                rec.run_id,
                rec.ts,
                rec.trace_id,
                rec.actor,
                redact(&rec.command),
                rec.mode,
                rec.risk,
                rec.decision,
                rec.concurrency as i64,
                rec.total as i64,
                rec.ok as i64,
                rec.failed as i64,
                rec.skipped as i64,
                rec.duration_ms,
            ],
        )
        .map_err(|e| StoreError::Db(e.to_string()))?;

        for r in &rec.results {
            let stdout = if self.archive_output {
                Some(redact(&r.stdout))
            } else {
                None
            };
            tx.execute(
                "INSERT INTO batch_result
                    (run_id, host, status, exit_code, duration_ms, truncated, timeout, stdout, error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    rec.run_id,
                    r.host,
                    r.status,
                    r.exit_code,
                    r.duration_ms,
                    r.truncated as i64,
                    r.timeout as i64,
                    stdout,
                    r.error.as_deref().map(redact),
                ],
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;
        }
        tx.commit().map_err(|e| StoreError::Db(e.to_string()))
    }

    /// 最近 N 次批量执行（新在前）
    pub fn list_batch(&self, limit: usize) -> Result<Vec<BatchRunSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT run_id, ts, trace_id, actor, command, mode, risk, decision,
                        concurrency, total, ok, failed, skipped, duration_ms
                 FROM batch_run ORDER BY id DESC LIMIT ?1",
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(BatchRunSummary {
                    run_id: row.get(0)?,
                    ts: row.get(1)?,
                    trace_id: row.get(2)?,
                    actor: row.get(3)?,
                    command: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    mode: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    risk: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    decision: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                    concurrency: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    total: row.get::<_, Option<i64>>(9)?.unwrap_or(0),
                    ok: row.get::<_, Option<i64>>(10)?.unwrap_or(0),
                    failed: row.get::<_, Option<i64>>(11)?.unwrap_or(0),
                    skipped: row.get::<_, Option<i64>>(12)?.unwrap_or(0),
                    duration_ms: row.get::<_, Option<i64>>(13)?.unwrap_or(0),
                })
            })
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| StoreError::Db(e.to_string()))?);
        }
        Ok(out)
    }

    /// 按 run_id 取完整明细（汇总 + 逐台）
    pub fn get_batch(&self, run_id: &str) -> Result<Option<BatchRunDetail>> {
        let conn = self.conn.lock().unwrap();
        let run = conn
            .query_row(
                "SELECT run_id, ts, trace_id, actor, command, mode, risk, decision,
                        concurrency, total, ok, failed, skipped, duration_ms
                 FROM batch_run WHERE run_id = ?1",
                params![run_id],
                |row| {
                    Ok(BatchRunSummary {
                        run_id: row.get(0)?,
                        ts: row.get(1)?,
                        trace_id: row.get(2)?,
                        actor: row.get(3)?,
                        command: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        mode: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                        risk: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                        decision: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                        concurrency: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
                        total: row.get::<_, Option<i64>>(9)?.unwrap_or(0),
                        ok: row.get::<_, Option<i64>>(10)?.unwrap_or(0),
                        failed: row.get::<_, Option<i64>>(11)?.unwrap_or(0),
                        skipped: row.get::<_, Option<i64>>(12)?.unwrap_or(0),
                        duration_ms: row.get::<_, Option<i64>>(13)?.unwrap_or(0),
                    })
                },
            )
            .optional()
            .map_err(|e| StoreError::Db(e.to_string()))?;

        let Some(run) = run else {
            return Ok(None);
        };

        let mut stmt = conn
            .prepare(
                "SELECT host, status, exit_code, duration_ms, truncated, timeout, stdout, error
                 FROM batch_result WHERE run_id = ?1 ORDER BY id",
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let rows = stmt
            .query_map(params![run_id], |row| {
                Ok(BatchResultRow {
                    host: row.get(0)?,
                    status: row.get(1)?,
                    exit_code: row.get(2)?,
                    duration_ms: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    truncated: row.get::<_, i64>(4)? != 0,
                    timeout: row.get::<_, i64>(5)? != 0,
                    stdout: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    error: row.get(7)?,
                })
            })
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r.map_err(|e| StoreError::Db(e.to_string()))?);
        }
        Ok(Some(BatchRunDetail { run, results }))
    }

    /// 查询（FR-8.4）。按时间倒序，`limit` 默认 100。
    pub fn query(&self, filter: &AuditFilter) -> Result<Vec<AuditRecord>> {
        let mut sql = String::from(
            "SELECT ts, trace_id, session_id, host_name, actor, action, command,
                    risk, decision, exit_code, duration_ms, truncated, error, stdout, stderr
             FROM audit WHERE 1=1",
        );
        let mut args: Vec<String> = Vec::new();

        if let Some(v) = &filter.host {
            sql.push_str(" AND host_name = ?");
            args.push(v.clone());
        }
        if let Some(v) = &filter.session {
            sql.push_str(" AND session_id = ?");
            args.push(v.clone());
        }
        if let Some(v) = &filter.actor {
            sql.push_str(" AND actor = ?");
            args.push(v.clone());
        }
        if let Some(v) = &filter.risk {
            sql.push_str(" AND risk = ?");
            args.push(v.clone());
        }
        if let Some(v) = &filter.since {
            sql.push_str(" AND ts >= ?");
            args.push(v.clone());
        }
        let limit = filter.limit.unwrap_or(100).min(1000);
        sql.push_str(&format!(" ORDER BY ts DESC LIMIT {limit}"));

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql).map_err(|e| StoreError::Db(e.to_string()))?;
        let params_ref: Vec<&dyn rusqlite::ToSql> =
            args.iter().map(|a| a as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(params_ref.as_slice(), |row| {
                Ok(AuditRecord {
                    ts: row.get(0)?,
                    trace_id: row.get(1)?,
                    session_id: row.get(2)?,
                    host_name: row.get(3)?,
                    actor: row.get(4)?,
                    action: row.get(5)?,
                    command: row.get(6)?,
                    risk: row.get(7)?,
                    decision: row.get(8)?,
                    exit_code: row.get(9)?,
                    duration_ms: row.get(10)?,
                    truncated: row.get::<_, i32>(11)? != 0,
                    error: row.get(12)?,
                    stdout: row.get(13)?,
                    stderr: row.get(14)?,
                })
            })
            .map_err(|e| StoreError::Db(e.to_string()))?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| StoreError::Db(e.to_string()))?);
        }
        Ok(out)
    }

    /// 记录总数（`log query` 的汇总信息用）
    pub fn count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM audit", [], |r| r.get(0))
            .map_err(|e| StoreError::Db(e.to_string()))
    }

    /// 保留策略：超期删除 + 体积上限淘汰（FR-8.5 的 MVP 版）
    pub fn cleanup(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        // 1) 超期删除
        if self.retention_days > 0 {
            let cutoff = chrono::Utc::now() - chrono::Duration::days(self.retention_days as i64);
            let _ = conn.execute(
                "DELETE FROM audit WHERE ts < ?1",
                params![cutoff.to_rfc3339()],
            );
        }

        // 2) 体积上限：超了就从最旧的开始删，直到回到上限内。
        //    删完 VACUUM 一下，否则文件不会真的缩小（只是内部留空洞）。
        let cap = self.max_db_mb as u64 * 1024 * 1024;
        if cap == 0 {
            return Ok(());
        }
        let path: Option<PathBuf> = conn.path().map(PathBuf::from);
        let Some(path) = path else {
            return Ok(());
        };
        let mut size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut vacuumed = false;
        while size > cap {
            let n = conn
                .execute("DELETE FROM audit WHERE id IN (SELECT id FROM audit ORDER BY id ASC LIMIT 500)", [])
                .unwrap_or(0);
            if n == 0 {
                break;
            }
            let _ = conn.execute_batch("VACUUM");
            vacuumed = true;
            size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        }
        let _ = vacuumed;
        Ok(())
    }

    /// 是否归档输出（供上层判断"查不到 stdout"是不是配置关的）
    pub fn archives_output(&self) -> bool {
        self.archive_output
    }

    /// 按 id 取单条（供 exec.confirm 结果回查、TUI 展示）
    pub fn get(&self, id: i64) -> Result<Option<AuditRecord>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT ts, trace_id, session_id, host_name, actor, action, command,
                    risk, decision, exit_code, duration_ms, truncated, error, stdout, stderr
             FROM audit WHERE id = ?1",
            params![id],
            |row| {
                Ok(AuditRecord {
                    ts: row.get(0)?,
                    trace_id: row.get(1)?,
                    session_id: row.get(2)?,
                    host_name: row.get(3)?,
                    actor: row.get(4)?,
                    action: row.get(5)?,
                    command: row.get(6)?,
                    risk: row.get(7)?,
                    decision: row.get(8)?,
                    exit_code: row.get(9)?,
                    duration_ms: row.get(10)?,
                    truncated: row.get::<_, i32>(11)? != 0,
                    error: row.get(12)?,
                    stdout: row.get(13)?,
                    stderr: row.get(14)?,
                })
            },
        )
        .optional()
        .map_err(|e| StoreError::Db(e.to_string()))
    }
}

// ---------------------------------------------------------------------------
// 脱敏（FR-8.3）
// ---------------------------------------------------------------------------

/// 对疑似敏感内容打掩码。
///
/// 判定标准刻意偏宽：误掩码只是让日志少一点信息，
/// 漏掩码可是把密码写进了明文文件。
pub fn redact(text: &str) -> String {
    use std::sync::LazyLock;
    static RES: LazyLock<Vec<(regex::Regex, &'static str)>> = LazyLock::new(|| {
        vec![
            // key = value / key: value（password/token/secret/api_key…）
            (
                regex::Regex::new(r#"(?i)\b((?:api[_-]?key|access[_-]?key|secret[_-]?key|private[_-]?key|password|passwd|pwd|passphrase|secret|token|auth)[_-]?(?:key|token)?)\s*[=:]\s*("[^"]*"|'[^']*'|[^\s,;]+)"#).unwrap(),
                "$1=***",
            ),
            // Authorization: Bearer xxxx / Basic xxxx
            (
                regex::Regex::new(r"(?i)(authorization\s*:\s*)(bearer|basic|token)\s+\S+").unwrap(),
                "$1$2 ***",
            ),
            // PEM 私钥块
            (
                regex::Regex::new(r"-----BEGIN [^-]*PRIVATE KEY-----[\s\S]*?-----END [^-]*PRIVATE KEY-----").unwrap(),
                "-----BEGIN PRIVATE KEY----- ***已脱敏*** -----END PRIVATE KEY-----",
            ),
            // AWS Access Key ID
            (
                regex::Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap(),
                "AKIA****************",
            ),
            // 命令行里的 -p<pass> / --password=<pass>
            (
                regex::Regex::new(r#"(--?password[= ])[^\s,;]+"#).unwrap(),
                "$1***",
            ),
            // -p 后紧跟值（无等号）：mysql -pSecret123。前面必须是空白，
            // 免得把 `docker run -p 8080:80` 的端口映射也掩掉
            (
                regex::Regex::new(r"(\s-p)[^\s,;]+").unwrap(),
                "$1***",
            ),
        ]
    });

    let mut out = text.to_string();
    for (re, rep) in RES.iter() {
        out = re.replace_all(&out, *rep).to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_common_shapes() {
        assert_eq!(redact("password=hunter2"), "password=***");
        assert_eq!(redact("API_KEY: abc123"), "API_KEY=***");
        assert_eq!(
            redact("Authorization: Bearer eyJhbGci"),
            "Authorization: Bearer ***"
        );
        assert_eq!(
            redact("-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----"),
            "-----BEGIN PRIVATE KEY----- ***已脱敏*** -----END PRIVATE KEY-----"
        );
        assert_eq!(redact("aws key AKIAIOSFODNN7EXAMPLE ok"), "aws key AKIA**************** ok");
        assert_eq!(redact("mysql -pSecret123 -h x"), "mysql -p*** -h x");
        // 普通输出不受伤
        assert_eq!(redact("Filesystem Size Used"), "Filesystem Size Used");
    }

    /// M5 安全复查补的回归：脱敏既要盖住新形态，也不能误伤正常命令
    #[test]
    fn redact_edge_cases_from_security_review() {
        // URL query 里的 token
        assert!(
            redact("curl 'https://api.example.com/v1?token=abc123&x=1'").contains("***"),
            "URL 里的 token 也该脱敏"
        );
        // 带引号、值里有空格
        assert_eq!(redact("export PASSWORD='p@ss word'"), "export PASSWORD=***");
        // 命令行 --password=xxx
        assert_eq!(redact("mysql --password=hunter2"), "mysql --password=***");
        // 端口映射不该被当成密码（-p 后是空格）
        assert_eq!(redact("docker run -p 8080:80 nginx"), "docker run -p 8080:80 nginx");
        // 中文与 emoji 不能被脱敏规则切成乱码
        assert_eq!(redact("已连接 服务器 ✅"), "已连接 服务器 ✅");
        // 同一行里多处都要盖住
        let many = redact("password=a token=b");
        assert!(!many.contains('a') || many.contains("***"), "多处敏感值应都脱敏: {many}");
    }
}
