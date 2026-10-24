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
            );",
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
}
