//! Database — SQLite 统一存储。
//!
//! 单文件 `~/.local/share/ductile/ductile.db`，4 张表：
//! pipelines / procs / runs / compositions。
//! 替代旧版 .gcf 文件系统。

use crate::core::ast::*;
use crate::harvest::civil_from_days;
use rusqlite::{params, Connection};
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

// ── Path ──

pub fn db_path() -> PathBuf {
    db_path_with(std::env::var_os("DUCTILE_DATA"))
}

/// db_path 的参数化内核（测试注入用，不碰全局 env——set_var 在并行测试下
/// 会把其他线程的 db::open() 重定向到临时目录，曾致 2 例 flaky）。
pub fn db_path_with(data: Option<std::ffi::OsString>) -> PathBuf {
    // v0.15 fix: 尊重 DUCTILE_DATA——selftest 探针/隔离 E2E 设置它就是期望库落在
    // 隔离目录，但本函数从未读过它，导致所有"隔离"写入全部落进真库
    // （l4 探针断言 log_only 被真库污染成 enforcing 才暴露）。
    if let Some(data) = data {
        let dir = PathBuf::from(&data);
        let _ = fs::create_dir_all(&dir);
        return dir.join("ductile.db");
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                std::env::temp_dir().to_string_lossy().into_owned()
            } else {
                "/tmp".into()
            }
        });
    let dir = PathBuf::from(&home)
        .join(".local")
        .join("share")
        .join("ductile");
    let _ = fs::create_dir_all(&dir);
    dir.join("ductile.db")
}

pub fn open() -> Connection {
    let path = db_path();
    let conn = Connection::open(&path).unwrap_or_else(|e| panic!("Cannot open ductile.db: {}", e));
    conn.execute_batch("PRAGMA journal_mode=WAL;").ok();
    // Ensure schema exists (fresh clones / first Windows run never called init_db).
    conn.execute_batch(SCHEMA_DDL).ok();
    // v0.18.5 制度修：迁移必须与建库同路径——此前迁移只挂在 init_db()，
    // open() 只跑 SCHEMA_DDL（IF NOT EXISTS 不给老表补列）。本机老库经
    // open() 打开时 origin 列缺失，SELECT 直接炸。两条路径一条迁移，单一事实源。
    migrate(&conn);
    conn
}

/// 老库补列迁移（幂等；init_db 与 open 共用——不要在别处另写 ALTER）。
pub fn migrate(conn: &Connection) {
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
    // v0.18.5 节点平等迁移：patches 补 origin（human / llm:<model> / machine）。
    // 没有出处就无法按"物种"统计 patch 存活率——问责基建（同款 pragma 幂等）。
    let has_origin = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('patches') WHERE name='origin'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0);
    if has_origin == 0 {
        conn.execute_batch("ALTER TABLE patches ADD COLUMN origin TEXT DEFAULT 'human';")
            .ok();
    }
}

/// 非致命版 open：打不开/没权限返回 Err（incident 记录等旁路写入用，
/// 失败静默降级——事故记录不能反过来搞死主管线）。
pub fn open_try() -> Result<Connection, String> {
    let path = db_path();
    let conn = Connection::open(&path).map_err(|e| format!("open ductile.db: {e}"))?;
    conn.execute_batch("PRAGMA journal_mode=WAL;").ok();
    conn.execute_batch(SCHEMA_DDL).ok();
    Ok(conn)
}

/// 全部表结构 DDL（幂等 CREATE IF NOT EXISTS）。init_db 与测试内存库共用。
pub const SCHEMA_DDL: &str = "CREATE TABLE IF NOT EXISTS pipelines (
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
            origin      TEXT DEFAULT 'human',
            UNIQUE(pipeline, proc_name, impl_name, field)
        );
        CREATE TABLE IF NOT EXISTS cost_cache (
            proc_name   TEXT NOT NULL,
            impl_name   TEXT NOT NULL,
            field       TEXT NOT NULL,
            value       REAL NOT NULL,
            measured_at INTEGER NOT NULL,
            PRIMARY KEY (proc_name, impl_name, field)
        );
        CREATE TABLE IF NOT EXISTS scripts (
            name        TEXT PRIMARY KEY,
            path        TEXT NOT NULL,
            lang        TEXT NOT NULL,
            desc        TEXT NOT NULL,
            params      TEXT NOT NULL DEFAULT '',
            output      TEXT NOT NULL DEFAULT '',
            pure        INTEGER NOT NULL DEFAULT 0,
            idempotent  INTEGER NOT NULL DEFAULT 0,
            concurrency TEXT NOT NULL DEFAULT 'serial',
            effects     TEXT NOT NULL DEFAULT '',
            timeout_secs INTEGER NOT NULL DEFAULT 300,
            retries     INTEGER NOT NULL DEFAULT 0,
            attached_at TEXT DEFAULT (datetime('now'))
        );
        CREATE TABLE IF NOT EXISTS canaries (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            pipeline  TEXT NOT NULL,
            proc_name TEXT NOT NULL,
            input     TEXT NOT NULL,
            expect    TEXT NOT NULL DEFAULT '@self.ok == 1',
            note      TEXT DEFAULT '',
            saved_at  TEXT DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_canaries_proc ON canaries(proc_name);
        CREATE TABLE IF NOT EXISTS canary_runs (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            pipeline  TEXT NOT NULL,
            proc_name TEXT NOT NULL,
            pass      INTEGER NOT NULL,
            detail    TEXT DEFAULT '',
            ran_at    TEXT DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_canary_runs_proc ON canary_runs(pipeline, proc_name);
        CREATE TABLE IF NOT EXISTS incidents (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            pipeline    TEXT NOT NULL,
            proc_name   TEXT NOT NULL,
            signals     TEXT NOT NULL DEFAULT '',
            err_code    TEXT DEFAULT '',
            evidence    TEXT DEFAULT '',
            status      TEXT NOT NULL DEFAULT 'open',
            created_at  TEXT DEFAULT '',
            closed_at   TEXT DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_incidents_status ON incidents(status);
        CREATE TABLE IF NOT EXISTS l4_reviews (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            pipeline    TEXT NOT NULL,
            verdict     TEXT NOT NULL,
            evidence    TEXT DEFAULT '',
            label       TEXT NOT NULL DEFAULT '',
            run_id      INTEGER,
            reviewed_at TEXT DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_l4_reviews_pipeline ON l4_reviews(pipeline);
        CREATE TABLE IF NOT EXISTS shelved (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            pipeline    TEXT NOT NULL,
            proc_name   TEXT NOT NULL,
            reason      TEXT DEFAULT '',
            evidence    TEXT DEFAULT '',
            status      TEXT NOT NULL DEFAULT 'open',
            resolution  TEXT DEFAULT '',
            created_at  TEXT DEFAULT '',
            resolved_at TEXT DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_shelved_status ON shelved(status);";

pub fn init_db() {
    let conn = open();
    conn.execute_batch(SCHEMA_DDL).expect("init_db failed");
    // v0.18.5：迁移收敛到 migrate()（open() 也走同一条——单一事实源，别在
    // 这儿另写 ALTER）
    migrate(&conn);
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

/// v0.15 incident 行（incident.rs 的查询载体）
#[derive(Debug, Clone)]
pub struct IncidentRow {
    pub id: i64,
    pub pipeline: String,
    pub proc_name: String,
    pub signals: String,
    pub err_code: String,
    pub evidence: String,
    pub status: String,
    pub created_at: String,
    pub closed_at: String,
}

/// v0.15 canary 行（canary.rs 的查询载体）
#[derive(Debug, Clone)]
pub struct ShelvedRow {
    pub id: i64,
    pub pipeline: String,
    pub proc_name: String,
    pub reason: String,
    pub evidence: String,
    pub status: String,
    pub resolution: String,
    pub created_at: String,
    pub resolved_at: String,
}

pub struct L4ReviewRow {
    pub id: i64,
    pub pipeline: String,
    pub verdict: String,
    pub evidence: String,
    pub label: String,
    pub run_id: Option<i64>,
    pub reviewed_at: String,
}

pub struct CanaryRow {
    pub id: i64,
    pub pipeline: String,
    pub proc_name: String,
    pub input: String,
    pub expect: String,
    pub note: String,
    pub saved_at: String,
}

// ── Import pipeline ──

pub fn import_pipeline_conn(conn: &Connection, pl: &Pipeline, source_file: &str) {
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

pub fn import_pipeline(pl: &Pipeline, source_file: &str) {
    let conn = open();
    import_pipeline_conn(&conn, pl, source_file)
}

pub fn import_pipeline_file(path: &str) -> Result<String, String> {
    let pl = crate::parser::parse_pipeline_file(path).map_err(|e| format!("{}", e))?;
    import_pipeline(&pl, path);
    Ok(pl.name)
}

// ── Search procs ──

pub fn search_procs_conn(conn: &Connection, query: &str) -> Vec<ProcRow> {
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

pub fn search_procs(query: &str) -> Vec<ProcRow> {
    let conn = open();
    search_procs_conn(&conn, query)
}

pub fn all_procs_conn(conn: &Connection) -> Vec<ProcRow> {
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

pub fn all_procs() -> Vec<ProcRow> {
    let conn = open();
    all_procs_conn(&conn)
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
pub fn record_run_rd_conn(
    conn: &Connection,
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
    let conn = open();
    record_run_rd_conn(
        &conn,
        proc_name,
        impl_name,
        pipeline,
        status,
        latency_ms,
        err_hash,
        err_at,
        rate_tokens,
        est_loss,
    )
}

/// v0.11 cost 实测缓存：measure 测试脚本的最近一次取值（TTL 内直接复用）。
pub fn cost_cache_get_fresh_conn(
    conn: &Connection,
    proc_name: &str,
    impl_name: &str,
    field: &str,
    ttl_secs: i64,
) -> Option<f64> {
    let now = unix_now();
    conn.query_row(
        "SELECT value FROM cost_cache
         WHERE proc_name = ?1 AND impl_name = ?2 AND field = ?3 AND measured_at > ?4",
        params![proc_name, impl_name, field, now - ttl_secs],
        |r| r.get(0),
    )
    .ok()
}

pub fn cost_cache_get_fresh(
    proc_name: &str,
    impl_name: &str,
    field: &str,
    ttl_secs: i64,
) -> Option<f64> {
    let conn = open();
    cost_cache_get_fresh_conn(&conn, proc_name, impl_name, field, ttl_secs)
}

pub fn cost_cache_put_conn(
    conn: &Connection,
    proc_name: &str,
    impl_name: &str,
    field: &str,
    value: f64,
) {
    conn.execute(
        "INSERT INTO cost_cache (proc_name, impl_name, field, value, measured_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(proc_name, impl_name, field)
         DO UPDATE SET value = excluded.value, measured_at = excluded.measured_at",
        params![proc_name, impl_name, field, value, unix_now()],
    )
    .ok();
}

pub fn cost_cache_put(proc_name: &str, impl_name: &str, field: &str, value: f64) {
    let conn = open();
    cost_cache_put_conn(&conn, proc_name, impl_name, field, value)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn recent_runs_conn(conn: &Connection, proc_name: &str) -> Vec<RunRow> {
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

pub fn recent_runs(proc_name: &str) -> Vec<RunRow> {
    let conn = open();
    recent_runs_conn(&conn, proc_name)
}

/// Recent runs with a caller-chosen limit (API/frontend layer; default 20 elsewhere).
pub fn recent_runs_limit(proc_name: &str, limit: usize) -> Vec<RunRow> {
    let conn = open();
    recent_runs_limit_conn(&conn, proc_name, limit)
}

pub fn recent_runs_limit_conn(conn: &Connection, proc_name: &str, limit: usize) -> Vec<RunRow> {
    let mut stmt = conn
        .prepare(
            "SELECT proc_name, impl_name, status, latency_ms, recorded_at, rate_tokens, est_loss
             FROM runs WHERE proc_name = ?1
             ORDER BY id DESC LIMIT ?2",
        )
        .unwrap();
    stmt.query_map(params![proc_name, limit as i64], |row| {
        Ok(RunRow {
            proc_name: row.get(0)?,
            impl_name: row.get(1)?,
            status: row.get(2)?,
            latency_ms: row.get(3).unwrap_or(0),
            recorded_at: row.get(4)?,
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
pub fn load_impl_prefs_conn(conn: &Connection) -> Vec<(String, String, f64)> {
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

pub fn load_impl_prefs() -> Vec<(String, String, f64)> {
    let conn = open();
    load_impl_prefs_conn(&conn)
}

/// Upsert one learned impl weight.
pub fn upsert_impl_pref_conn(conn: &Connection, proc_name: &str, impl_name: &str, weight: f64) {
    conn.execute(
        "INSERT INTO impl_prefs (proc_name, impl_name, weight, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(proc_name, impl_name)
         DO UPDATE SET weight = ?3, updated_at = ?4",
        params![proc_name, impl_name, weight, now_ts()],
    )
    .ok();
}

pub fn upsert_impl_pref(proc_name: &str, impl_name: &str, weight: f64) {
    let conn = open();
    upsert_impl_pref_conn(&conn, proc_name, impl_name, weight)
}

/// v0.8 preference bump — 单语句原子乘性更新（免读改写）。
/// success: ×1.1；fail: ÷1.5；clamp [0.05, 20]；初值 1.0。
pub fn record_pref_conn(conn: &Connection, proc_name: &str, impl_name: &str, success: bool) {
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

pub fn record_pref(proc_name: &str, impl_name: &str, success: bool) {
    let conn = open();
    record_pref_conn(&conn, proc_name, impl_name, success)
}

// ── Composition ──

pub fn save_composition_conn(conn: &Connection, name: &str, desc: &str, proc_names: &[String]) {
    let ts = now_ts();
    let names = proc_names.join(",");
    conn.execute(
        "INSERT INTO compositions (name, description, proc_names, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![name, desc, names, ts],
    )
    .ok();
}

pub fn save_composition(name: &str, desc: &str, proc_names: &[String]) {
    let conn = open();
    save_composition_conn(&conn, name, desc, proc_names)
}

// ── Stats ──

pub fn db_stats_conn(conn: &Connection) -> (i64, i64, i64, i64) {
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

pub fn db_stats() -> (i64, i64, i64, i64) {
    let conn = open();
    db_stats_conn(&conn)
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
    /// v0.18.5 出处：human / llm:<model_id> / machine。默认 human（老数据）。
    pub origin: String,
}

/// Upsert a patch: if (pipeline, proc, impl, field) already exists, update value.
pub fn set_patch_conn(
    conn: &Connection,
    pipeline: &str,
    proc_name: &str,
    impl_name: &str,
    field: &str,
    value: &str,
    origin: &str,
) {
    let ts = now_ts();
    conn.execute(
        "INSERT INTO patches (pipeline, proc_name, impl_name, field, value, created_at, origin)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(pipeline, proc_name, impl_name, field)
         DO UPDATE SET value=excluded.value, created_at=excluded.created_at, origin=excluded.origin",
        params![pipeline, proc_name, impl_name, field, value, ts, origin],
    )
    .ok();
}

pub fn set_patch(
    pipeline: &str,
    proc_name: &str,
    impl_name: &str,
    field: &str,
    value: &str,
    origin: &str,
) {
    let conn = open();
    set_patch_conn(&conn, pipeline, proc_name, impl_name, field, value, origin)
}

/// Remove a specific patch.
pub fn remove_patch_conn(
    conn: &Connection,
    pipeline: &str,
    proc_name: &str,
    impl_name: &str,
    field: &str,
) {
    conn.execute(
        "DELETE FROM patches WHERE pipeline=?1 AND proc_name=?2 AND impl_name=?3 AND field=?4",
        params![pipeline, proc_name, impl_name, field],
    )
    .ok();
}

pub fn remove_patch(pipeline: &str, proc_name: &str, impl_name: &str, field: &str) {
    let conn = open();
    remove_patch_conn(&conn, pipeline, proc_name, impl_name, field)
}

/// Load all patches for a given pipeline.
pub fn load_patches_conn(conn: &Connection, pipeline: &str) -> Vec<PatchRow> {
    let mut stmt = conn
        .prepare(
            "SELECT pipeline, proc_name, impl_name, field, value, origin
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
            origin: row.get(5)?,
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

pub fn load_patches(pipeline: &str) -> Vec<PatchRow> {
    let conn = open();
    load_patches_conn(&conn, pipeline)
}

/// List all patches across all pipelines.
pub fn all_patches_conn(conn: &Connection) -> Vec<PatchRow> {
    let mut stmt = conn
        .prepare(
            "SELECT pipeline, proc_name, impl_name, field, value, origin
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
            origin: row.get(5)?,
        })
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

pub fn all_patches() -> Vec<PatchRow> {
    let conn = open();
    all_patches_conn(&conn)
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
pub fn rebuild_fts_conn(conn: &Connection) {
    conn.execute_batch("INSERT INTO procs_fts(procs_fts) VALUES('rebuild');")
        .expect("FTS rebuild failed — does your SQLite support FTS5?");
}

pub fn rebuild_fts() {
    let conn = open();
    rebuild_fts_conn(&conn)
}

/// BM25 full-text search over procs.
///
/// Returns results ranked by BM25 relevance (lower score = better match,
/// consistent with SQLite's negative BM25 convention).
pub fn search_fts_conn(conn: &Connection, query: &str, limit: usize) -> Vec<FtsRow> {
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

pub fn search_fts(query: &str, limit: usize) -> Vec<FtsRow> {
    let conn = open();
    search_fts_conn(&conn, query, limit)
}

// ── v0.12 脚本契约库（脚本即 API） ──

use crate::core::script_card::{Concurrency, ScriptCard};

/// 注册（upsert）脚本契约。契约解析已在 script::parse_contract 完成。
pub fn script_attach_conn(conn: &Connection, card: &ScriptCard) -> Result<(), String> {
    conn.execute(
        "INSERT INTO scripts (name, path, lang, desc, params, output, pure, idempotent,
                              concurrency, effects, timeout_secs, retries)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
         ON CONFLICT(name) DO UPDATE SET
            path=excluded.path, lang=excluded.lang, desc=excluded.desc,
            params=excluded.params, output=excluded.output, pure=excluded.pure,
            idempotent=excluded.idempotent, concurrency=excluded.concurrency,
            effects=excluded.effects, timeout_secs=excluded.timeout_secs,
            retries=excluded.retries, attached_at=datetime('now')",
        rusqlite::params![
            card.name,
            card.path,
            card.lang,
            card.desc,
            card.params,
            card.output,
            card.pure as i64,
            card.idempotent as i64,
            card.concurrency.as_str(),
            card.effects,
            card.timeout_secs as i64,
            card.retries as i64,
        ],
    )
    .map_err(|e| format!("script_attach failed: {}", e))?;
    Ok(())
}

pub fn script_attach(card: &ScriptCard) -> Result<(), String> {
    let conn = open();
    script_attach_conn(&conn, card)
}

pub fn script_detach_conn(conn: &Connection, name: &str) -> bool {
    conn.execute("DELETE FROM scripts WHERE name = ?1", [name])
        .map(|n| n > 0)
        .unwrap_or(false)
}

pub fn script_detach(name: &str) -> bool {
    let conn = open();
    script_detach_conn(&conn, name)
}

pub fn script_get_conn(conn: &Connection, name: &str) -> Option<ScriptCard> {
    conn.query_row(
        "SELECT name, path, lang, desc, params, output, pure, idempotent,
                concurrency, effects, timeout_secs, retries
         FROM scripts WHERE name = ?1",
        [name],
        |r| {
            Ok(ScriptCard {
                name: r.get(0)?,
                path: r.get(1)?,
                lang: r.get(2)?,
                desc: r.get(3)?,
                params: r.get(4)?,
                output: r.get(5)?,
                pure: r.get::<_, i64>(6)? != 0,
                idempotent: r.get::<_, i64>(7)? != 0,
                concurrency: Concurrency::parse(&r.get::<_, String>(8)?)
                    .unwrap_or(Concurrency::Serial),
                effects: r.get(9)?,
                timeout_secs: r.get::<_, i64>(10)? as u64,
                retries: r.get::<_, i64>(11)? as usize,
            })
        },
    )
    .ok()
}

pub fn script_get(name: &str) -> Option<ScriptCard> {
    let conn = open();
    script_get_conn(&conn, name)
}

pub fn script_list_conn(conn: &Connection) -> Vec<ScriptCard> {
    let mut stmt = conn
        .prepare(
            "SELECT name, path, lang, desc, params, output, pure, idempotent,
                    concurrency, effects, timeout_secs, retries
             FROM scripts ORDER BY name",
        )
        .expect("script_list failed");
    let rows = stmt
        .query_map([], |r| {
            Ok(ScriptCard {
                name: r.get(0)?,
                path: r.get(1)?,
                lang: r.get(2)?,
                desc: r.get(3)?,
                params: r.get(4)?,
                output: r.get(5)?,
                pure: r.get::<_, i64>(6)? != 0,
                idempotent: r.get::<_, i64>(7)? != 0,
                concurrency: Concurrency::parse(&r.get::<_, String>(8)?)
                    .unwrap_or(Concurrency::Serial),
                effects: r.get(9)?,
                timeout_secs: r.get::<_, i64>(10)? as u64,
                retries: r.get::<_, i64>(11)? as usize,
            })
        })
        .expect("script_list query failed");
    rows.filter_map(|r| r.ok()).collect()
}

pub fn script_list() -> Vec<ScriptCard> {
    let conn = open();
    script_list_conn(&conn)
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

    /// v0.18.5 节点平等：patches.origin 问责基建。
    /// 用内存库走完整 DDL + 迁移路径，验证 (1) 老库形态可补列 (2) origin
    /// 全链路往返（写→读→upsert 覆盖）(3) 迁移幂等（跑两遍不炸）。
    /// 不用 set_var（规范 #9）：全程参数注入，不碰全局 env。
    #[test]
    fn patch_origin_roundtrip_and_migration() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_DDL).unwrap();

        // 老库形态模拟：删掉 origin 列不可行，改验"新库默认值"——
        // 直接验迁移分支：手工建一张无 origin 的老表，跑同款 ALTER。
        let old = Connection::open_in_memory().unwrap();
        old.execute_batch(
            "CREATE TABLE patches (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pipeline TEXT NOT NULL, proc_name TEXT NOT NULL,
                impl_name TEXT NOT NULL, field TEXT NOT NULL, value TEXT NOT NULL,
                created_at TEXT DEFAULT '',
                UNIQUE(pipeline, proc_name, impl_name, field));
             INSERT INTO patches (pipeline, proc_name, impl_name, field, value, created_at)
             VALUES ('p1','proc','impl','guide','legacy text','2026-01-01');",
        )
        .unwrap();
        let has: i64 = old
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('patches') WHERE name='origin'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has, 0, "老库起点：无 origin 列");
        old.execute_batch("ALTER TABLE patches ADD COLUMN origin TEXT DEFAULT 'human';")
            .unwrap();
        // 幂等第二遍：pragma 检测应拦住重复 ALTER（模拟 init_db 的守卫逻辑）
        let has2: i64 = old
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('patches') WHERE name='origin'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has2, 1);
        // 老数据自动落 human 默认值
        let legacy: String = old
            .query_row("SELECT origin FROM patches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(legacy, "human");

        // 新库全链路：llm 处方写入 → 读出 origin → human upsert 覆盖
        set_patch_conn(
            &conn,
            "p1",
            "arch",
            "llm",
            "guide",
            "R1 白名单版",
            "llm:qwen3.8:27b",
        );
        set_patch_conn(&conn, "p1", "arch", "llm", "guide", "human 改", "human");
        let rows = load_patches_conn(&conn, "p1");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, "human 改");
        assert_eq!(
            rows[0].origin, "human",
            "upsert 必须覆盖 origin——出处跟着值走"
        );
        // 再写一条 llm 出处，验 all_patches 路径
        set_patch_conn(
            &conn,
            "p2",
            "brk",
            "llm",
            "guide",
            "R2 版",
            "llm:qwen3.8:27b",
        );
        let all = all_patches_conn(&conn);
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|r| r.origin == "llm:qwen3.8:27b"));
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

// ── v0.12.1 内存库单元测试（*_conn 内核 + Connection 注入，零真实库污染）──

#[cfg(test)]
mod conn_tests {
    use super::*;

    fn memdb() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_DDL).unwrap();
        conn
    }

    // ── scripts 契约卡 CRUD ──

    #[test]
    fn script_card_roundtrip() {
        let conn = memdb();
        let card = ScriptCard {
            name: "word_stats".into(),
            path: "/tmp/word_stats.py".into(),
            lang: "python".into(),
            desc: "词数统计".into(),
            params: "text(str, required), top(int, default=5)".into(),
            output: "words=int".into(),
            pure: true,
            idempotent: true,
            concurrency: crate::core::script_card::Concurrency::Safe,
            effects: "none".into(),
            timeout_secs: 60,
            retries: 1,
        };
        assert!(script_attach_conn(&conn, &card).is_ok());
        let got = script_get_conn(&conn, "word_stats").unwrap();
        assert_eq!(got.params, card.params);
        assert!(got.pure);
        assert_eq!(got.timeout_secs, 60);
        // upsert：同名重挂覆盖
        let mut v2 = card.clone();
        v2.desc = "v2 描述".into();
        v2.timeout_secs = 120;
        assert!(script_attach_conn(&conn, &v2).is_ok());
        let got2 = script_get_conn(&conn, "word_stats").unwrap();
        assert_eq!(got2.desc, "v2 描述");
        assert_eq!(got2.timeout_secs, 120);
        // list 含它
        assert!(script_list_conn(&conn)
            .iter()
            .any(|c| c.name == "word_stats"));
        // detach
        assert!(script_detach_conn(&conn, "word_stats"));
        assert!(script_get_conn(&conn, "word_stats").is_none());
        assert!(!script_detach_conn(&conn, "word_stats")); // 二次 detach false
    }

    // ── cost_cache TTL 语义 ──

    #[test]
    fn cost_cache_ttl_freshness() {
        let conn = memdb();
        cost_cache_put_conn(&conn, "p", "i", "latency", 201.0);
        // TTL 内命中
        assert_eq!(
            cost_cache_get_fresh_conn(&conn, "p", "i", "latency", 86400),
            Some(201.0)
        );
        // TTL 0 → 永远过期
        assert_eq!(
            cost_cache_get_fresh_conn(&conn, "p", "i", "latency", 0),
            None
        );
        // 不存在的 key
        assert_eq!(
            cost_cache_get_fresh_conn(&conn, "p", "i", "risk", 86400),
            None
        );
        // 覆盖更新
        cost_cache_put_conn(&conn, "p", "i", "latency", 300.0);
        assert_eq!(
            cost_cache_get_fresh_conn(&conn, "p", "i", "latency", 86400),
            Some(300.0)
        );
    }

    #[test]
    fn cost_cache_backdated_entry_expired() {
        let conn = memdb();
        // 手工插入一条 25h 前的测量 → TTL 24h 应视为过期
        conn.execute(
            "INSERT INTO cost_cache (proc_name, impl_name, field, value, measured_at)
             VALUES ('p','i','latency',999.0, ?1)",
            [unix_now() - 90000],
        )
        .unwrap();
        assert_eq!(
            cost_cache_get_fresh_conn(&conn, "p", "i", "latency", 86400),
            None
        );
    }

    // ── impl_prefs 乘性学习回路 ──

    #[test]
    fn pref_multiplicative_roundtrip() {
        let conn = memdb();
        record_pref_conn(&conn, "p", "i", true); // 1.0 × 1.1
        let w1 = load_impl_prefs_conn(&conn)
            .into_iter()
            .find(|(p, i, _)| p == "p" && i == "i")
            .unwrap()
            .2;
        assert!((w1 - 1.1).abs() < 1e-9);
        record_pref_conn(&conn, "p", "i", false); // 1.1 ÷ 1.5
        let w2 = load_impl_prefs_conn(&conn)
            .into_iter()
            .find(|(p, i, _)| p == "p" && i == "i")
            .unwrap()
            .2;
        assert!((w2 - 1.1 / 1.5).abs() < 1e-9);
        // clamp：连败 100 次 → 0.05
        for _ in 0..100 {
            record_pref_conn(&conn, "p", "i", false);
        }
        let w3 = load_impl_prefs_conn(&conn)
            .into_iter()
            .find(|(p, i, _)| p == "p" && i == "i")
            .unwrap()
            .2;
        assert!((w3 - 0.05).abs() < 1e-9);
    }

    // ── patches 回路 ──

    #[test]
    fn patch_set_load_remove() {
        let conn = memdb();
        set_patch_conn(&conn, "pl", "proc", "impl", "enabled", "false", "human");
        set_patch_conn(&conn, "pl", "proc", "impl", "cost.latency", "42", "human");
        let patches = load_patches_conn(&conn, "pl");
        assert_eq!(patches.len(), 2);
        // 同字段重设 = 覆盖非追加
        set_patch_conn(&conn, "pl", "proc", "impl", "cost.latency", "99", "human");
        let patches = load_patches_conn(&conn, "pl");
        assert_eq!(patches.len(), 2);
        assert!(patches
            .iter()
            .any(|p| p.field == "cost.latency" && p.value == "99"));
        // 移除
        remove_patch_conn(&conn, "pl", "proc", "impl", "cost.latency");
        let patches = load_patches_conn(&conn, "pl");
        assert_eq!(patches.len(), 1);
        // 别的 pipeline 不串
        assert!(load_patches_conn(&conn, "other").is_empty());
    }

    // ── runs 记录 + recent_runs 窗口 ──

    #[test]
    fn runs_record_and_recent_window() {
        let conn = memdb();
        for k in 0..25 {
            record_run_rd_conn(
                &conn,
                "p",
                &format!("i{k}"),
                "",
                "Ok",
                k,
                None,
                None,
                10,
                0.0,
            );
        }
        let rows = recent_runs_conn(&conn, "p");
        assert_eq!(rows.len(), 20, "recent window = 20");
        assert_eq!(rows[0].impl_name, "i24", "ORDER BY id DESC");
        assert_eq!(rows[0].latency_ms, 24);
    }

    #[test]
    fn runs_status_mixing() {
        let conn = memdb();
        record_run_rd_conn(&conn, "p", "ok", "", "Ok", 5, None, None, 8, 0.0);
        record_run_rd_conn(
            &conn,
            "p",
            "bad",
            "",
            "Fail",
            7,
            Some("abc12345"),
            Some("p.bad.step"),
            0,
            0.0,
        );
        let rows = recent_runs_conn(&conn, "p");
        assert_eq!(rows.len(), 2);
        let bad = rows.iter().find(|r| r.impl_name == "bad").unwrap();
        assert_eq!(bad.status, "Fail");
    }

    // ── db_stats ──

    #[test]
    fn stats_counts_after_writes() {
        let conn = memdb();
        record_run_rd_conn(&conn, "p", "i", "", "Ok", 1, None, None, 0, 0.0);
        save_composition_conn(&conn, "comp", "desc", &["p".to_string()]);
        let (pipelines, procs, runs, compositions) = db_stats_conn(&conn);
        assert_eq!(runs, 1);
        assert_eq!(compositions, 1);
        assert_eq!(pipelines, 0);
        assert_eq!(procs, 0);
    }

    #[test]
    fn db_path_respects_ductile_data() {
        // v0.15 fix 回归钉：DUCTILE_DATA 必须决定库路径，否则隔离探针污染真库。
        // 参数注入而非 set_var——全局 env 在并行测试下会把别的线程的 db::open()
        // 重定向到临时目录（flaky 竞态）。
        let tmp = std::env::temp_dir().join(format!("dt_data_test_{}", std::process::id()));
        let p = db_path_with(Some(tmp.clone().into_os_string()));
        assert_eq!(p, tmp.join("ductile.db"), "db must live under DUCTILE_DATA");
        let fallback = db_path_with(None);
        assert!(fallback.ends_with("ductile.db"));
    }
}
