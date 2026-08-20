//! Harvest — wake-phase sensor for library learning (v0.8.1).
//!
//! 解决"跨时间主体不一致"的机制层：
//! - `wrap`:  命令现在就执行 + 顺手记录为可复用 proc（铸渠成本≈0）
//! - `harvest`: 从 Hermes state.db 挖重复命令序列 → 提议为新 proc
//! - `doctor`: 环境自检自修（lunar-env 等持久环境钉在 ~/.local/share/ductile/envs）
//! - degraded flag: run 失败落 flag，下次会话必须处理（降级从习惯变成 issue）

use crate::db;
use rusqlite::{params, Connection};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

// ── Paths ──

pub fn share_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let d = PathBuf::from(&home).join(".local/share/ductile");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn envs_dir() -> PathBuf {
    let d = share_dir().join("envs");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn degraded_dir() -> PathBuf {
    let d = share_dir().join("degraded");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn state_db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(&home).join(".hermes/state.db")
}

// ── Degraded flags ──

/// run 失败时落 flag；修复路径清 flag。会话例行检查项。
pub fn set_degraded(name: &str, reason: &str) {
    let f = degraded_dir().join(format!("{}.flag", name.replace('/', "_")));
    let _ = std::fs::write(&f, reason);
}

pub fn clear_degraded(name: &str) -> bool {
    let f = degraded_dir().join(format!("{}.flag", name.replace('/', "_")));
    std::fs::remove_file(&f).is_ok()
}

pub fn list_degraded() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(degraded_dir()) {
        for e in rd.flatten() {
            if let Some(n) = e.file_name().to_str() {
                if n.ends_with(".flag") {
                    out.push(n.trim_end_matches(".flag").to_string());
                }
            }
        }
    }
    out.sort();
    out
}

// ── Doctor ──

pub struct DoctorReport {
    pub checks: Vec<(String, bool, String)>, // (name, ok, detail)
}

impl DoctorReport {
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|c| c.1)
    }
}

pub fn doctor() -> DoctorReport {
    let mut checks = Vec::new();

    // 1. lunar-env persists outside /tmp
    let lunar = envs_dir().join("lunar-env/bin/python");
    let lunar_ok = lunar.exists();
    checks.push((
        "lunar-env persistent".into(),
        lunar_ok,
        if lunar_ok {
            format!("{}", lunar.display())
        } else {
            "MISSING: cp -r /tmp/lunar-env ~/.local/share/ductile/envs/ or rebuild via uv".into()
        },
    ));

    // 2. /tmp lunar-env straggler (warn only if persistent copy missing)
    let tmp_lunar = PathBuf::from("/tmp/lunar-env");
    if tmp_lunar.exists() && !lunar_ok {
        checks.push((
            "migrate /tmp/lunar-env".into(),
            false,
            "run: cp -r /tmp/lunar-env ~/.local/share/ductile/envs/".into(),
        ));
    }

    // 3. main db writable
    let db_ok = {
        let conn = Connection::open(db::db_path()).ok();
        match conn {
            Some(c) => c
                .execute_batch("CREATE TABLE IF NOT EXISTS _doctor_ping(x)")
                .is_ok(),
            None => false,
        }
    };
    checks.push((
        "ductile.db writable".into(),
        db_ok,
        db::db_path().display().to_string(),
    ));

    // 4. degraded flags outstanding
    let flags = list_degraded();
    checks.push((
        "degraded flags".into(),
        flags.is_empty(),
        if flags.is_empty() {
            "none".into()
        } else {
            format!(
                "OUTSTANDING: {} — fix and clear: {}",
                flags.len(),
                flags.join(", ")
            )
        },
    ));

    DoctorReport { checks }
}

// ── Wrap: execute now + record proc ──

/// Execute the command now (streaming to parent), then record it as a reusable
/// proc in the library under a synthetic "wrapped" pipeline.
pub fn wrap_and_run(cmd: &str, tag: &str) -> Result<(i32, String), String> {
    // 1. run it for real — user wants the output NOW
    let status = Command::new("bash").arg("-c").arg(cmd).status();
    let code = match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => return Err(format!("spawn failed: {}", e)),
    };

    // 2. record into library: pipeline "_wrapped", proc = tag, impl = run(cmd)
    let conn = db_open()?;
    let ts = now_ts();
    conn.execute(
        "INSERT INTO pipelines (name, description, source_file, imported_at)
         VALUES ('_wrapped', 'Commands captured via ductile wrap', 'wrap', ?1)
         ON CONFLICT(name) DO UPDATE SET imported_at = ?1",
        params![ts],
    )
    .map_err(|e| e.to_string())?;
    let pid: i64 = conn
        .query_row(
            "SELECT id FROM pipelines WHERE name = '_wrapped'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    // upsert proc keyed by tag name
    conn.execute(
        "INSERT INTO procs (name, pipeline_id, description, tags, impl_count, is_deliver)
         VALUES (?1, ?2, 'captured by wrap', ?3, 1, 0)
         ON CONFLICT DO NOTHING",
        params![tag, pid, format!("{},wrap", tag)],
    )
    .ok();
    // store the command itself in a side table (procs has no body column)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS wrapped_cmds (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tag TEXT NOT NULL, cmd TEXT NOT NULL, exit_code INTEGER, recorded_at TEXT,
            UNIQUE(tag, cmd)
        )",
        [],
    )
    .map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT OR IGNORE INTO wrapped_cmds (tag, cmd, exit_code, recorded_at) VALUES (?1, ?2, ?3, ?4)",
        params![tag, cmd, code, ts],
    )
    .map_err(|e| e.to_string())?;

    Ok((code, format!("recorded #{} → {}", tag, trunc(cmd, 60))))
}

fn db_open() -> Result<Connection, String> {
    db::init_db();
    Connection::open(db::db_path()).map_err(|e| e.to_string())
}

fn now_ts() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| {
            let secs = d.as_secs();
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                1970 + secs / 31_536_000,
                (secs % 31_536_000) / 2_592_000 + 1,
                (secs % 2_592_000) / 86400 + 1,
                (secs % 86400) / 3600,
                (secs % 3600) / 60,
                secs % 60
            )
        })
        .unwrap_or_default()
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push_str("...");
        out
    }
}

// ── Harvest: mine repeated commands from Hermes state.db ──

pub struct HarvestHit {
    pub count: usize,
    pub cmd: String,
    pub last_seen: String,
}

/// Read terminal commands from the agent's own session store and rank by
/// repetition. Returns commands seen >=2 times (candidates for procs).
pub fn harvest(days: u32) -> Result<Vec<HarvestHit>, String> {
    let path = state_db_path();
    if !path.exists() {
        return Err(format!("state.db not found at {}", path.display()));
    }
    let conn = Connection::open(&path).map_err(|e| e.to_string())?;

    // tool_calls holds doubly-escaped JSON: {\"command\":\"...\"}
    // match either form via plain LIKE on the word terminal/command pair
    let cutoff_secs: i64 = (days as i64) * 86400;
    let since_ts: f64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() - cutoff_secs as f64)
        .unwrap_or(0.0);

    let sql = if has_column(&conn, "timestamp") {
        "SELECT tool_calls, timestamp FROM messages
         WHERE tool_calls LIKE '%command%' AND tool_calls LIKE '%terminal%'
           AND timestamp >= ?1
         ORDER BY timestamp DESC LIMIT 4000"
    } else {
        "SELECT tool_calls, 0 FROM messages
         WHERE tool_calls LIKE '%command%' AND tool_calls LIKE '%terminal%'
         ORDER BY id DESC LIMIT 4000"
    };

    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let rows: Vec<(String, f64)> = stmt
        .query_map(params![since_ts], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1).unwrap_or(0.0)))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();

    // extract "command":"..." values
    let mut counts: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for (tc, ts) in &rows {
        for cmd in extract_commands(tc) {
            let e = counts.entry(normalize(&cmd)).or_insert((0, 0.0));
            e.0 += 1;
            e.1 = e.1.max(*ts);
        }
    }

    let mut hits: Vec<HarvestHit> = counts
        .into_iter()
        .filter(|(_, (c, _))| *c >= 2)
        .map(|(cmd, (c, ts))| HarvestHit {
            count: c,
            cmd,
            last_seen: fmt_ts(ts),
        })
        .collect();
    hits.sort_by(|a, b| b.count.cmp(&a.count));
    Ok(hits)
}

fn has_column(conn: &Connection, col: &str) -> bool {
    conn.prepare(&format!("PRAGMA table_info(messages)"))
        .and_then(|mut s| {
            let cols: Vec<String> = s
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .flatten()
                .collect();
            Ok(cols.iter().any(|c| c == col))
        })
        .unwrap_or(false)
}

/// crude but effective: pull "command":"..." JSON string values
fn extract_commands(tool_calls: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = tool_calls;
    while let Some(i) = rest.find("command") {
        let after = &rest[i + 7..];
        let Some(colon) = after.find(':') else { break };
        let val = after[colon + 1..].trim_start();
        // handle both raw JSON ("cmd") and double-escaped (\"cmd\")
        if let Some(stripped) = val.strip_prefix("\\\"") {
            if let Some(endrel) = stripped.find("\\\"") {
                let cmd = &stripped[..endrel];
                // unescape JSON escapes
                let cmd = cmd.replace("\\\\", "\\").replace("\\\"", "\"");
                out.push(cmd);
                rest = &stripped[endrel + 2..];
                continue;
            }
        }
        if val.starts_with('"') {
            if let Some(endrel) = find_json_string_end(&val[1..]) {
                out.push(val[1..1 + endrel].to_string());
                rest = &val[1 + endrel..];
                continue;
            }
        }
        rest = after;
    }
    out
}

fn find_json_string_end(s: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, ch) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(i),
            _ => {}
        }
    }
    None
}

/// Normalize: first meaningful line; mark truncated invocations with …;
/// canonicalize known temp paths so near-identical invocations merge.
fn normalize(cmd: &str) -> String {
    let c = cmd.trim();
    let mut line1 = c.lines().next().unwrap_or("").trim().to_string();
    if c.lines().count() > 1 || c.ends_with('\\') {
        if !line1.ends_with('…') {
            line1.push('…');
        }
    }
    line1 = line1.split(" # ").next().unwrap_or(&line1).to_string();
    line1.replace("/tmp/lunar-env", "~/.local/share/ductile/envs/lunar-env")
}

fn fmt_ts(ts: f64) -> String {
    if ts <= 0.0 {
        return "?".into();
    }
    let secs = ts as i64 + 8 * 3600; // CST
    let days = secs.div_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let rem = secs.rem_euclid(86400);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Howard Hinnant's civil_from_days (days since 1970-01-01 → y/m/d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
