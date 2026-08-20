//! Database — SQLite 统一存储。
//!
//! 单文件 `~/.local/share/ductile/ductile.db`，4 张表：
//! pipelines / procs / runs / compositions。
//! 替代旧版 .gcf 文件系统。

use crate::ast::*;
use crate::harvest::civil_from_days;
use rusqlite::{params, Connection};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

// ── Path ──

pub fn db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dir = PathBuf::from(&home).join(".local/share/ductile");
    let _ = fs::create_dir_all(&dir);
    dir.join("ductile.db")
}

pub fn open() -> Connection {
    let path = db_path();
    let conn = Connection::open(&path).unwrap_or_else(|e| panic!("Cannot open ductile.db: {}", e));
    conn.execute_batch("PRAGMA journal_mode=WAL;").ok();
    conn
}

pub fn init_db() {
    let conn = open();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pipelines (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT UNIQUE NOT NULL,
            description TEXT DEFAULT '',
            source_file TEXT DEFAULT '',
            imported_at TEXT DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS procs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT NOT NULL,
            pipeline_id INTEGER NOT NULL,
            description TEXT DEFAULT '',
            tags        TEXT DEFAULT '',
            impl_count  INTEGER DEFAULT 0,
            is_deliver  INTEGER DEFAULT 0,
            FOREIGN KEY (pipeline_id) REFERENCES pipelines(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_procs_tags ON procs(tags);
        CREATE INDEX IF NOT EXISTS idx_procs_name ON procs(name);
        CREATE TABLE IF NOT EXISTS impl_prefs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            proc_name   TEXT NOT NULL,
            impl_name   TEXT NOT NULL,
            weight      REAL NOT NULL DEFAULT 1.0,
            updated_at  TEXT DEFAULT '',
            UNIQUE(proc_name, impl_name)
        );
        CREATE TABLE IF NOT EXISTS runs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            proc_name   TEXT NOT NULL,
            impl_name   TEXT NOT NULL,
            pipeline    TEXT DEFAULT '',
            status      TEXT NOT NULL,
            latency_ms  INTEGER DEFAULT 0,
            err_hash    TEXT DEFAULT '',
            err_at      TEXT DEFAULT '',
            recorded_at TEXT DEFAULT '',
            rate_tokens INTEGER DEFAULT 0,
            est_loss    REAL DEFAULT 0.0
        );
        CREATE INDEX IF NOT EXISTS idx_runs_proc ON runs(proc_name);
        CREATE TABLE IF NOT EXISTS compositions (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT NOT NULL,
            description TEXT DEFAULT '',
            proc_names  TEXT DEFAULT '',
            created_at  TEXT DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS patches (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            pipeline    TEXT NOT NULL,
            proc_name   TEXT NOT NULL,
            impl_name   TEXT NOT NULL,
            field       TEXT NOT NULL,
            value       TEXT NOT NULL,
            created_at  TEXT DEFAULT '',
            UNIQUE(pipeline, proc_name, impl_name, field)
        );",
    )
    .expect("init_db failed");
    // RD 扩展迁移：老库补列（新库 CREATE 已含；ALTER 幂等检测防重）
    let has_rate = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('runs') WHERE name='rate_tokens'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0);
    if has_rate == 0 {
        conn.execute_batch(
            "ALTER TABLE runs ADD COLUMN rate_tokens INTEGER DEFAULT 0;
             ALTER TABLE runs ADD COLUMN est_loss REAL DEFAULT 0.0;",
        )
        .ok();
    }
}

// ── Timestamp ──

fn now_ts() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + 8 * 3600; // CST (UTC+8), matches harvest::fmt_ts convention
    let days = secs.div_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let rem = secs.rem_euclid(86400);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// ── Row types ──

#[derive(Debug, Clone)]
pub struct ProcRow {
    pub id: i64,
    pub name: String,
    pub pipeline: String,
    pub description: String,
    pub tags: BTreeSet<String>,
    pub impl_count: i64,
    pub is_deliver: bool,
}

#[derive(Debug, Clone)]
pub struct RunRow {
    pub proc_name: String,
    pub impl_name: String,
    pub status: String,
    pub latency_ms: i64,
    pub recorded_at: String,
    pub rate_tokens: i64,
    pub est_loss: f64,
}

// ── Import pipeline ──

pub fn import_pipeline(pl: &Pipeline, source_file: &str) {
    init_db();
    let conn = open();
    let ts = now_ts();

    // Upsert pipeline
    conn.execute(
        "INSERT INTO pipelines (name, description, source_file, imported_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET
           description=excluded.description,
           source_file=excluded.source_file,
           imported_at=excluded.imported_at",
        params![pl.name, pl.description, source_file, ts],
    )
    .ok();

    let pid: i64 = conn
        .query_row(
            "SELECT id FROM pipelines WHERE name = ?1",
            params![pl.name],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Replace procs
    conn.execute("DELETE FROM procs WHERE pipeline_id = ?1", params![pid])
        .ok();

    // Ensure FTS5 table exists before inserting
    ensure_fts(&conn);

    for proc in &pl.procs {
        let tags: Vec<String> = proc_tags(proc).iter().cloned().collect();
        let tags_str = tags.join(",");
        conn.execute(
            "INSERT INTO procs (name, pipeline_id, description, tags, impl_count, is_deliver)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                proc.name,
                pid,
                proc.description,
                tags_str,
                proc.plan.len(),
                if proc.deliver { 1 } else { 0 },
            ],
        )
        .ok();
    }
}

pub fn import_pipeline_file(path: &str) -> Result<String, String> {
    let pl = crate::parser::parse_pipeline_file(path).map_err(|e| format!("{}", e))?;
    import_pipeline(&pl, path);
    Ok(pl.name)
}

// ── Search procs ──

pub fn search_procs(query: &str) -> Vec<ProcRow> {
    init_db();
    let conn = open();
    let pattern = format!("%{}%", query);
    let mut stmt = conn
        .prepare(
            "SELECT p.id, p.name, pl.name, p.description, p.tags, p.impl_count, p.is_deliver
             FROM procs p JOIN pipelines pl ON p.pipeline_id = pl.id
             WHERE p.tags LIKE ?1 OR p.name LIKE ?1 OR p.description LIKE ?1
             ORDER BY p.tags",
        )
        .unwrap();

    stmt.query_map(params![pattern], |row| {
        let tags_str: String = row.get(4).unwrap_or_default();
        let tags: BTreeSet<String> = tags_str
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        Ok(ProcRow {
            id: row.get(0)?,
            name: row.get(1)?,
            pipeline: row.get(2)?,
            description: row.get(3).unwrap_or_default(),
            tags,
            impl_count: row.get(5).unwrap_or(0),
            is_deliver: row.get::<_, i64>(6).unwrap_or(0) == 1,
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

pub fn all_procs() -> Vec<ProcRow> {
    init_db();
    let conn = open();
    let mut stmt = conn
        .prepare(
            "SELECT p.id, p.name, pl.name, p.description, p.tags, p.impl_count, p.is_deliver
             FROM procs p JOIN pipelines pl ON p.pipeline_id = pl.id
             ORDER BY p.tags, p.name",
        )
        .unwrap();

    stmt.query_map([], |row| {
        let tags_str: String = row.get(4).unwrap_or_default();
        let tags: BTreeSet<String> = tags_str
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        Ok(ProcRow {
            id: row.get(0)?,
            name: row.get(1)?,
            pipeline: row.get(2)?,
            description: row.get(3).unwrap_or_default(),
            tags,
            impl_count: row.get(5).unwrap_or(0),
            is_deliver: row.get::<_, i64>(6).unwrap_or(0) == 1,
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

// ── Run records ──

pub fn record_run(
    proc_name: &str,
    impl_name: &str,
    pipeline: &str,
    status: &str,
    latency_ms: i64,
    err_hash: Option<&str>,
    err_at: Option<&str>,
) {
    record_run_rd(
        proc_name, impl_name, pipeline, status, latency_ms, err_hash, err_at, 0, 0.0,
    );
}

/// RD-aware run record: rate_tokens = impl 输出 token 数（rate 的操作代理），
/// est_loss = 失真代理（v0: 下游 check 失败=1.0, 通过=0.0; 由 executor 填）。
pub fn record_run_rd(
    proc_name: &str,
    impl_name: &str,
    pipeline: &str,
    status: &str,
    latency_ms: i64,
    err_hash: Option<&str>,
    err_at: Option<&str>,
    rate_tokens: i64,
    est_loss: f64,
) {
    init_db();
    let conn = open();
    let ts = now_ts();
    conn.execute(
        "INSERT INTO runs (proc_name, impl_name, pipeline, status, latency_ms, err_hash, err_at, recorded_at, rate_tokens, est_loss)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            proc_name,
            impl_name,
            pipeline,
            status,
            latency_ms,
            err_hash.unwrap_or("-"),
            err_at.unwrap_or("-"),
            ts,
            rate_tokens,
            est_loss,
        ],
    )
    .ok();
}

pub fn recent_runs(proc_name: &str) -> Vec<RunRow> {
    init_db();
    let conn = open();
    let mut stmt = conn
        .prepare(
            "SELECT proc_name, impl_name, status, latency_ms, recorded_at, rate_tokens, est_loss
             FROM runs WHERE proc_name = ?1
             ORDER BY id DESC LIMIT 20",
        )
        .unwrap();

    stmt.query_map(params![proc_name], |row| {
        Ok(RunRow {
            proc_name: row.get(0)?,
            impl_name: row.get(1)?,
            status: row.get(2)?,
            latency_ms: row.get(3).unwrap_or(0),
            recorded_at: row.get(4).unwrap_or_default(),
            rate_tokens: row.get(5).unwrap_or(0),
            est_loss: row.get(6).unwrap_or(0.0),
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

// ── Impl preferences (v0.8 LGuess-style multiplicative weights) ──

/// Load all learned impl weights: (proc_name, impl_name) -> weight.
pub fn load_impl_prefs() -> Vec<(String, String, f64)> {
    init_db();
    let conn = open();
    let mut stmt = conn
        .prepare("SELECT proc_name, impl_name, weight FROM impl_prefs")
        .unwrap();
    stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, f64>(2)?,
        ))
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

/// Upsert one learned impl weight.
pub fn upsert_impl_pref(proc_name: &str, impl_name: &str, weight: f64) {
    init_db();
    let conn = open();
    conn.execute(
        "INSERT INTO impl_prefs (proc_name, impl_name, weight, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(proc_name, impl_name)
         DO UPDATE SET weight = ?3, updated_at = ?4",
        params![proc_name, impl_name, weight, now_ts()],
    )
    .ok();
}

/// v0.8 preference bump — 单语句原子乘性更新（免读改写）。
/// success: ×1.1；fail: ÷1.5；clamp [0.05, 20]；初值 1.0。
pub fn record_pref(proc_name: &str, impl_name: &str, success: bool) {
    init_db();
    let conn = open();
    let factor = if success { 1.1f64 } else { 1.0 / 1.5 };
    conn.execute(
        "INSERT INTO impl_prefs (proc_name, impl_name, weight, updated_at)
         VALUES (?1, ?2, MAX(0.05, MIN(20.0, 1.0 * ?3)), ?4)
         ON CONFLICT(proc_name, impl_name)
         DO UPDATE SET weight = MAX(0.05, MIN(20.0, weight * ?3)), updated_at = ?4",
        params![proc_name, impl_name, factor, now_ts()],
    )
    .ok();
}

// ── Composition ──

pub fn save_composition(name: &str, desc: &str, proc_names: &[String]) {
    init_db();
    let conn = open();
    let ts = now_ts();
    let names = proc_names.join(",");
    conn.execute(
        "INSERT INTO compositions (name, description, proc_names, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![name, desc, names, ts],
    )
    .ok();
}

// ── Stats ──

pub fn db_stats() -> (i64, i64, i64, i64) {
    init_db();
    let conn = open();
    let count = |table: &str| -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {}", table), [], |row| {
            row.get(0)
        })
        .unwrap_or(0)
    };
    (
        count("pipelines"),
        count("procs"),
        count("runs"),
        count("compositions"),
    )
}

// ── Isomorphic groups (tag-based, cross-pipeline) ──

#[derive(Debug, Clone)]
pub struct IsoGroup {
    pub tags: BTreeSet<String>,
    pub members: Vec<ProcRow>,
}

pub fn isomorphic_groups() -> Vec<IsoGroup> {
    let procs = all_procs();
    let with_tags: Vec<ProcRow> = procs.into_iter().filter(|p| !p.tags.is_empty()).collect();

    // Group by tags
    let mut groups: std::collections::BTreeMap<Vec<String>, Vec<ProcRow>> =
        std::collections::BTreeMap::new();
    for p in with_tags {
        let key: Vec<String> = p.tags.iter().cloned().collect();
        groups.entry(key).or_default().push(p);
    }

    groups
        .into_iter()
        .filter(|(_, members)| {
            // Need 2+ members from different pipelines
            let unique_pipelines: BTreeSet<&str> =
                members.iter().map(|m| m.pipeline.as_str()).collect();
            unique_pipelines.len() >= 2
        })
        .map(|(tags, members)| IsoGroup {
            tags: tags.into_iter().collect(),
            members,
        })
        .collect()
}

// ── Patches (hot overrides without editing source files) ──

#[derive(Debug, Clone)]
pub struct PatchRow {
    pub pipeline: String,
    pub proc_name: String,
    pub impl_name: String,
    pub field: String,
    pub value: String,
}

/// Upsert a patch: if (pipeline, proc, impl, field) already exists, update value.
pub fn set_patch(pipeline: &str, proc_name: &str, impl_name: &str, field: &str, value: &str) {
    init_db();
    let conn = open();
    let ts = now_ts();
    conn.execute(
        "INSERT INTO patches (pipeline, proc_name, impl_name, field, value, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(pipeline, proc_name, impl_name, field)
         DO UPDATE SET value=excluded.value, created_at=excluded.created_at",
        params![pipeline, proc_name, impl_name, field, value, ts],
    )
    .ok();
}

/// Remove a specific patch.
pub fn remove_patch(pipeline: &str, proc_name: &str, impl_name: &str, field: &str) {
    init_db();
    let conn = open();
    conn.execute(
        "DELETE FROM patches WHERE pipeline=?1 AND proc_name=?2 AND impl_name=?3 AND field=?4",
        params![pipeline, proc_name, impl_name, field],
    )
    .ok();
}

/// Load all patches for a given pipeline.
pub fn load_patches(pipeline: &str) -> Vec<PatchRow> {
    init_db();
    let conn = open();
    let mut stmt = conn
        .prepare(
            "SELECT pipeline, proc_name, impl_name, field, value
             FROM patches WHERE pipeline = ?1
             ORDER BY proc_name, impl_name, field",
        )
        .unwrap();
    stmt.query_map(params![pipeline], |row| {
        Ok(PatchRow {
            pipeline: row.get(0)?,
            proc_name: row.get(1)?,
            impl_name: row.get(2)?,
            field: row.get(3)?,
            value: row.get(4)?,
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

/// List all patches across all pipelines.
pub fn all_patches() -> Vec<PatchRow> {
    init_db();
    let conn = open();
    let mut stmt = conn
        .prepare(
            "SELECT pipeline, proc_name, impl_name, field, value
             FROM patches ORDER BY pipeline, proc_name, impl_name",
        )
        .unwrap();
    stmt.query_map([], |row| {
        Ok(PatchRow {
            pipeline: row.get(0)?,
            proc_name: row.get(1)?,
            impl_name: row.get(2)?,
            field: row.get(3)?,
            value: row.get(4)?,
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

// ── Full-text search (FTS5 + BM25) ──

/// Search result with BM25 relevance score.
#[derive(Debug, Clone)]
pub struct FtsRow {
    pub name: String,
    pub pipeline: String,
    pub description: String,
    pub tags: BTreeSet<String>,
    pub impl_count: i64,
    pub is_deliver: bool,
    pub bm25_score: f64,
}

/// Initialize FTS5 virtual table + triggers (idempotent).
/// Creates `procs_fts` synced to `procs` via triggers.
fn ensure_fts(conn: &Connection) {
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS procs_fts USING fts5(
            name,
            description,
            tags,
            content='procs',
            content_rowid='id',
            tokenize='porter unicode61'
        );",
    )
    .ok();

    // Triggers keep FTS in sync with procs table.
    // `tokenize='porter unicode61'` — porter stemming for English + unicode for CJK.
    conn.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS procs_ai AFTER INSERT ON procs BEGIN
            INSERT INTO procs_fts(rowid, name, description, tags)
            VALUES (new.id, new.name, new.description, new.tags);
        END;
        CREATE TRIGGER IF NOT EXISTS procs_ad AFTER DELETE ON procs BEGIN
            INSERT INTO procs_fts(procs_fts, rowid, name, description, tags)
            VALUES ('delete', old.id, old.name, old.description, old.tags);
        END;
        CREATE TRIGGER IF NOT EXISTS procs_au AFTER UPDATE ON procs BEGIN
            INSERT INTO procs_fts(procs_fts, rowid, name, description, tags)
            VALUES ('delete', old.id, old.name, old.description, old.tags);
            INSERT INTO procs_fts(rowid, name, description, tags)
            VALUES (new.id, new.name, new.description, new.tags);
        END;",
    )
    .ok();
}

/// Rebuild FTS index from scratch (use after bulk import or migration).
pub fn rebuild_fts() {
    init_db();
    let conn = open();
    conn.execute_batch("INSERT INTO procs_fts(procs_fts) VALUES('rebuild');")
        .expect("FTS rebuild failed — does your SQLite support FTS5?");
}

/// BM25 full-text search over procs.
///
/// Returns results ranked by BM25 relevance (lower score = better match,
/// consistent with SQLite's negative BM25 convention).
pub fn search_fts(query: &str, limit: usize) -> Vec<FtsRow> {
    init_db();
    let conn = open();
    ensure_fts(&conn);

    // Sanitize query for FTS5: wrap each token in double quotes to avoid
    // FTS5 query syntax injection (AND/OR/NOT/NEAR/column: etc.)
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect();
    if tokens.is_empty() {
        return Vec::new();
    }
    let fts_query = tokens.join(" "); // implicit AND

    let sql = format!(
        "SELECT p.name, pl.name, p.description, p.tags, p.impl_count, p.is_deliver,
                bm25(procs_fts) AS score
         FROM procs_fts
         JOIN procs p ON p.id = procs_fts.rowid
         JOIN pipelines pl ON p.pipeline_id = pl.id
         WHERE procs_fts MATCH ?1
         ORDER BY score ASC
         LIMIT {}",
        limit
    );

    let mut stmt = conn.prepare(&sql).unwrap();
    stmt.query_map(params![fts_query], |row| {
        let tags_str: String = row.get(3).unwrap_or_default();
        let tags: BTreeSet<String> = tags_str
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        Ok(FtsRow {
            name: row.get(0)?,
            pipeline: row.get(1)?,
            description: row.get(2).unwrap_or_default(),
            tags,
            bm25_score: row.get::<_, f64>(6).unwrap_or(0.0),
            impl_count: row.get(4).unwrap_or(0),
            is_deliver: row.get::<_, i64>(5).unwrap_or(0) == 1,
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_init_and_stats() {
        // Use in-memory for test
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE pipelines (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE procs (id INTEGER PRIMARY KEY, name TEXT);",
        )
        .unwrap();
        conn.execute("INSERT INTO pipelines (name) VALUES ('test')", [])
            .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM pipelines", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn fts5_is_available() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(content)")
            .expect("FTS5 is required — your bundled SQLite does not support it");
        conn.execute("INSERT INTO t (content) VALUES ('hello world')", [])
            .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM t WHERE t MATCH 'hello'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn runs_rd_columns_exist_after_migration() {
        // 模拟老库（无 rate_tokens/est_loss）→ init 迁移逻辑的等价路径
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                proc_name TEXT NOT NULL, impl_name TEXT NOT NULL,
                pipeline TEXT DEFAULT '', status TEXT NOT NULL,
                latency_ms INTEGER DEFAULT 0, err_hash TEXT DEFAULT '',
                err_at TEXT DEFAULT '', recorded_at TEXT DEFAULT ''
            );",
        )
        .unwrap();
        let has_rate: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('runs') WHERE name='rate_tokens'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_rate, 0, "老库不应有 rate_tokens");
        conn.execute_batch(
            "ALTER TABLE runs ADD COLUMN rate_tokens INTEGER DEFAULT 0;
             ALTER TABLE runs ADD COLUMN est_loss REAL DEFAULT 0.0;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs (proc_name, impl_name, status, rate_tokens, est_loss)
             VALUES ('p', 'i', 'Ok', 6, 0.0)",
            [],
        )
        .unwrap();
        let (rt, el): (i64, f64) = conn
            .query_row(
                "SELECT rate_tokens, est_loss FROM runs WHERE proc_name='p'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(rt, 6);
        assert!((el - 0.0).abs() < 1e-12);
    }
}
