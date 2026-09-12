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

    // 4. Hermes state.db health — harvest's perception layer (2026-08-20 lesson:
    // corrupted state.db silently blinded harvest to 0 candidates). Probe what
    // harvest actually reads (messages recency, same query shape), NOT full
    // integrity_check — that validates FTS5 inverted indexes and requires write
    // access, so a read-only probe always false-alarms on healthy DBs.
    let sdb = state_db_path();
    let (sdb_ok, sdb_msg) = if !sdb.exists() {
        (false, format!("MISSING at {}", sdb.display()))
    } else {
        match Connection::open_with_flags(&sdb, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => {
                let recent = c
                    .query_row(
                        "SELECT COUNT(*) FROM messages WHERE timestamp >= strftime('%s','now') - 604800",
                        [],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap_or(-1);
                match recent {
                    n if n > 0 => (true, format!("readable, {} msgs in last 7d", n)),
                    0 => (
                        false,
                        "opens but 0 recent messages (last 7d) — perception layer blind? recovered/reset?"
                            .into(),
                    ),
                    _ => (
                        false,
                        "messages table unreadable — corrupted? check *.corrupt.bak / recover".into(),
                    ),
                }
            }
            Err(e) => (false, format!("cannot open: {}", e)),
        }
    };
    checks.push(("hermes state.db health".into(), sdb_ok, sdb_msg));

    // 5. degraded flags outstanding
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

    Ok((code, format!("recorded #{} → {}", tag, trunc_str(cmd, 60))))
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

pub fn trunc_str(s: &str, n: usize) -> String {
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

/// 命令计数 (calls + sessions) — promote/grow 的取数层.
/// V28 物理: 99% 调用是会话内迭代, 跨会话计数才是复用证据.
pub struct CmdCount {
    pub calls: u32,
    pub sessions: u32,
}

/// 全文命令计数 (无首行归一化) — grow.rs 的取数层.
/// 返回 (command_full_text -> CmdCount); 多行 heredoc 保持完整.
pub fn harvest_full_counts(
    days: u32,
) -> Result<std::collections::HashMap<String, CmdCount>, String> {
    let path = state_db_path();
    if !path.exists() {
        return Err(format!("state.db not found at {}", path.display()));
    }
    let conn = Connection::open(&path).map_err(|e| e.to_string())?;
    let cutoff_secs: i64 = (days as i64) * 86400;
    let since_ts: f64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() - cutoff_secs as f64)
        .unwrap_or(0.0);
    let sql = if has_column(&conn, "timestamp") {
        "SELECT tool_calls, timestamp, session_id FROM messages
         WHERE tool_calls LIKE '%command%' AND tool_calls LIKE '%terminal%'
           AND timestamp >= ?1
         ORDER BY timestamp DESC LIMIT 20000"
    } else {
        "SELECT tool_calls, 0, session_id FROM messages
         WHERE tool_calls LIKE '%command%' AND tool_calls LIKE '%terminal%'
         ORDER BY id DESC LIMIT 20000"
    };
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let rows: Vec<(String, f64, String)> = stmt
        .query_map(params![since_ts], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1).unwrap_or(0.0),
                r.get::<_, String>(2).unwrap_or_default(),
            ))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    let mut counts: std::collections::HashMap<String, CmdCount> = std::collections::HashMap::new();
    let mut seen_sess: std::collections::HashMap<(String, String), ()> =
        std::collections::HashMap::new();
    for (tc, _, sid) in &rows {
        for cmd in extract_commands_exact(tc) {
            let c = cmd.trim();
            if c.len() >= 6 {
                let e = counts.entry(c.to_string()).or_insert(CmdCount {
                    calls: 0,
                    sessions: 0,
                });
                e.calls += 1;
                seen_sess.entry((c.to_string(), sid.clone())).or_insert(());
            }
        }
    }
    for (c, sid) in seen_sess.keys() {
        if let Some(e) = counts.get_mut(c) {
            e.sessions += 1;
        }
    }
    Ok(counts)
}

// ── Minimal exact JSON extraction (v0.9.2): tool_calls 是合法 JSON ──

#[derive(Debug, PartialEq)]
enum Jv {
    S(String),
    A(Vec<Jv>),
    O(Vec<(String, Jv)>),
    Other,
}

fn jskip_ws(s: &str, i: &mut usize) {
    while *i < s.len() && s.as_bytes()[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

pub(crate) fn jparse(s: &str, i: &mut usize) -> Option<Jv> {
    jskip_ws(s, i);
    let b = s.as_bytes();
    if *i >= s.len() {
        return None;
    }
    match b[*i] {
        b'"' => jparse_str(s, i).map(Jv::S),
        b'[' => {
            *i += 1;
            let mut out = Vec::new();
            loop {
                jskip_ws(s, i);
                if *i < s.len() && b[*i] == b']' {
                    *i += 1;
                    break;
                }
                out.push(jparse(s, i)?);
                jskip_ws(s, i);
                if *i < s.len() && b[*i] == b',' {
                    *i += 1;
                }
            }
            Some(Jv::A(out))
        }
        b'{' => {
            *i += 1;
            let mut out = Vec::new();
            loop {
                jskip_ws(s, i);
                if *i < s.len() && b[*i] == b'}' {
                    *i += 1;
                    break;
                }
                let key = jparse_str(s, i)?;
                jskip_ws(s, i);
                if *i >= s.len() || b[*i] != b':' {
                    return None;
                }
                *i += 1;
                let val = jparse(s, i)?;
                out.push((key, val));
                jskip_ws(s, i);
                if *i < s.len() && b[*i] == b',' {
                    *i += 1;
                }
            }
            Some(Jv::O(out))
        }
        _ => {
            // number / true / false / null — 跳过
            let start = *i;
            while *i < s.len() && !matches!(b[*i], b',' | b']' | b'}') {
                *i += 1;
            }
            if *i > start {
                Some(Jv::Other)
            } else {
                None
            }
        }
    }
}

pub(crate) fn jparse_str(s: &str, i: &mut usize) -> Option<String> {
    let b = s.as_bytes();
    if *i >= s.len() || b[*i] != b'"' {
        return None;
    }
    *i += 1;
    let mut out = String::new();
    loop {
        if *i >= s.len() {
            return None;
        }
        match b[*i] {
            b'"' => {
                *i += 1;
                return Some(out);
            }
            b'\\' => {
                *i += 1;
                if *i >= s.len() {
                    return None;
                }
                match b[*i] {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'u' => {
                        if *i + 4 >= s.len() {
                            return None;
                        }
                        let hex = &s[*i + 1..*i + 5];
                        let cp = u32::from_str_radix(hex, 16).ok()?;
                        *i += 4;
                        if (0xD800..0xDC00).contains(&cp)
                            && *i + 6 < s.len()
                            && b[*i + 1] == b'\\'
                            && b[*i + 2] == b'u'
                        {
                            let hex2 = &s[*i + 3..*i + 7];
                            if let Ok(lo) = u32::from_str_radix(hex2, 16) {
                                if (0xDC00..0xE000).contains(&lo) {
                                    let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                    if let Some(ch) = char::from_u32(c) {
                                        out.push(ch);
                                        // 第二个 \uXXXX 跨 7 字节（含起始反斜杠）；
                                        // 外层 escape 收尾还有一次 +=1，此处只推进 6。
                                        // 旧代码 +6+1 与外层重复计数 → 索引冲过闭引号：
                                        // 代理对在串尾返回 None；后随字符被静默丢弃。
                                        *i += 6;
                                    }
                                }
                            }
                        } else if let Some(ch) = char::from_u32(cp) {
                            out.push(ch);
                        }
                    }
                    other => out.push(other as char),
                }
                *i += 1;
            }
            _ => {
                // 多字节 UTF-8: 按字符推进
                let ch = s[*i..].chars().next()?;
                out.push(ch);
                *i += ch.len_utf8();
            }
        }
    }
}

/// 精确提取: tool_calls JSON → function.arguments(JSON字符串) → command.
/// 失败时回退旧启发式.
pub(crate) fn extract_commands_exact(tool_calls: &str) -> Vec<String> {
    let mut i = 0usize;
    let Some(Jv::A(calls)) = jparse(tool_calls, &mut i) else {
        return extract_commands(tool_calls);
    };
    let mut out = Vec::new();
    for call in &calls {
        let Jv::O(fields) = call else { continue };
        for (k, val) in fields {
            if k == "function" {
                let Jv::O(fns) = val else { continue };
                for (k2, v2) in fns {
                    if k2 == "arguments" {
                        let Jv::S(args_str) = v2 else { continue };
                        let mut j = 0usize;
                        if let Some(Jv::O(aps)) = jparse(args_str, &mut j) {
                            for (k3, v3) in aps {
                                if k3 == "command" {
                                    if let Jv::S(c) = v3 {
                                        out.push(c);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if out.is_empty() {
        return extract_commands(tool_calls);
    }
    out
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
pub(crate) fn extract_commands(tool_calls: &str) -> Vec<String> {
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

pub(crate) fn find_json_string_end(s: &str) -> Option<usize> {
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
pub(crate) fn normalize(cmd: &str) -> String {
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
    // L0 时间原语薄委托：负数/NaN → "?"（历史约定），CST 无秒展示格式
    if !ts.is_finite() {
        return "?".into();
    }
    crate::L0_physical::time::fmt_ts_cst(ts as i64, " ", false)
}

// ── v0.12.1 JSON 解析器与命令提取单测（纯函数，零 I/O）──

#[cfg(test)]
mod tests {
    use super::*;

    fn jv(s: &str) -> Option<Jv> {
        let mut i = 0usize;
        jparse(s, &mut i)
    }

    // ── jparse 基础形态 ──

    #[test]
    fn jparse_scalar_and_other() {
        assert_eq!(jv("\"hi\""), Some(Jv::S("hi".into())));
        assert_eq!(jv("123"), Some(Jv::Other)); // number → Other
        assert_eq!(jv("true"), Some(Jv::Other));
        assert_eq!(jv("null"), Some(Jv::Other));
        assert_eq!(jv(""), None);
        assert_eq!(jv("   "), None);
    }

    #[test]
    fn jparse_nested_containers() {
        let v = jv("[1, {\"a\": [\"x\", \"y\"]}]").unwrap();
        let Jv::A(items) = v else { panic!("array") };
        assert_eq!(items.len(), 2);
        let Jv::O(pairs) = &items[1] else {
            panic!("object")
        };
        assert_eq!(pairs[0].0, "a");
        let Jv::A(inner) = &pairs[0].1 else {
            panic!("inner array")
        };
        assert_eq!(inner.len(), 2);
    }

    #[test]
    fn jparse_trailing_garbage_tolerated() {
        // jparse 只解析首个值——尾随垃圾不报错（调用方语义：提取即可）
        assert_eq!(jv("\"cmd\" junk"), Some(Jv::S("cmd".into())));
    }

    #[test]
    fn jparse_malformed_returns_none() {
        assert_eq!(jv("[1,"), None);
        assert_eq!(jv("{\"k\""), None);
        assert_eq!(jv("{\"k\":"), None);
    }

    #[test]
    fn jparse_empty_containers() {
        assert_eq!(jv("[]"), Some(Jv::A(vec![])));
        assert_eq!(jv("{}"), Some(Jv::O(vec![])));
    }

    // ── jparse_str 转义与 UTF-16 代理对 ──

    #[test]
    fn jparse_str_escapes() {
        // JSON 文本: "a\nb\tc\"" = quote a \n b \t c \" quote
        let s: String = vec!['"', 'a', '\\', 'n', 'b', '\\', 't', 'c', '\\', '"', '"']
            .into_iter()
            .collect();
        let mut i = 0usize;
        assert_eq!(jparse_str(&s, &mut i), Some("a\nb\tc\"".into()));
        // JSON 文本: "\\/"
        let s2: String = vec!['"', '\\', '\\', '/', '"'].into_iter().collect();
        let mut i = 0usize;
        assert_eq!(jparse_str(&s2, &mut i), Some("\\/".into()));
        // unterminated
        let mut i = 0usize;
        assert_eq!(jparse_str("\"abc", &mut i), None);
    }

    #[test]
    fn jparse_str_unicode_escape() {
        // \u4e2d = 中
        let mut i = 0usize;
        assert_eq!(jparse_str("\"\\u4e2d\"", &mut i), Some("中".into()));
    }

    #[test]
    fn jparse_str_surrogate_pair() {
        // U+1F600 = \ud83d\ude00（UTF-16 代理对），代理对恰在串尾
        let s: String = vec![
            '"', '\\', 'u', 'd', '8', '3', 'd', '\\', 'u', 'd', 'e', '0', '0', '"',
        ]
        .into_iter()
        .collect();
        let mut i = 0usize;
        assert_eq!(jparse_str(&s, &mut i), Some("\u{1F600}".into()));
    }
    #[test]
    fn jparse_str_surrogate_pair_followed_by_char() {
        // 回归：代理对后跟普通字符不得被吞（旧 off-by-one 会静默丢 x）
        let s: String = vec![
            '"', '\\', 'u', 'd', '8', '3', 'd', '\\', 'u', 'd', 'e', '0', '0', 'x', '"',
        ]
        .into_iter()
        .collect();
        let mut i = 0usize;
        assert_eq!(jparse_str(&s, &mut i), Some("\u{1F600}x".into()));
    }

    #[test]
    fn jparse_str_raw_utf8_multibyte() {
        let mut i = 0usize;
        assert_eq!(jparse_str("\"你好\"", &mut i), Some("你好".into()));
    }

    // ── extract_commands_exact：tool_calls → command 链 ──

    #[test]
    fn exact_simple_tool_call() {
        let tc = r#"[{"function":{"name":"terminal","arguments":"{\"command\":\"echo hi\"}"}}]"#;
        assert_eq!(extract_commands_exact(tc), vec!["echo hi"]);
    }

    #[test]
    fn exact_multiple_calls() {
        let tc = r#"[
            {"function":{"name":"t","arguments":"{\"command\":\"ls -la\"}"}},
            {"function":{"name":"t","arguments":"{\"command\":\"pwd\"}"}}
        ]"#;
        assert_eq!(extract_commands_exact(tc), vec!["ls -la", "pwd"]);
    }

    #[test]
    fn exact_nested_escapes() {
        // arguments 的 JSON 文本: {"command":"echo \"hi\" \\"}
        // 解码后 command = echo "hi" \（含真实引号与反斜杠）
        let args_json: String = vec![
            '{', '"', 'c', 'o', 'm', 'm', 'a', 'n', 'd', '"', ':', '"', 'e', 'c', 'h', 'o', ' ',
            '\\', '"', 'h', 'i', '\\', '"', ' ', '\\', '\\', '"', '}',
        ]
        .into_iter()
        .collect();
        let expected: String = vec!['e', 'c', 'h', 'o', ' ', '"', 'h', 'i', '"', ' ', '\\']
            .into_iter()
            .collect();
        // arguments 是字符串化的 JSON → 整体再转义一层：\ → \\\\，" → \\"
        let args_lit: String = args_json
            .chars()
            .flat_map(|c| match c {
                '\\' => vec!['\\', '\\'],
                '"' => vec!['\\', '"'],
                other => vec![other],
            })
            .collect();
        let tc = format!("[{{\"function\":{{\"arguments\":\"{}\"}}}}]", args_lit);
        assert_eq!(extract_commands_exact(&tc), vec![expected]);
    }

    #[test]
    fn exact_falls_back_to_heuristic_on_garbage() {
        // 非 JSON 输入 → 回退 extract_commands 启发式
        let out = extract_commands_exact("total garbage \"command\":\"x\" tail");
        assert!(!out.is_empty(), "heuristic fallback must find command");
    }

    #[test]
    fn exact_empty_on_empty() {
        assert!(extract_commands_exact("[]").is_empty());
    }

    // ── extract_commands 启发式 ──

    #[test]
    fn heuristic_raw_json() {
        let s = r#"pre {"command": "cargo test"} post"#;
        assert_eq!(extract_commands(s), vec!["cargo test"]);
    }

    #[test]
    fn heuristic_finds_multiple() {
        let s = r#"{"command":"a"} {"command":"b"}"#;
        assert_eq!(extract_commands(s), vec!["a", "b"]);
    }

    #[test]
    fn heuristic_no_command_key() {
        assert!(extract_commands("nothing here").is_empty());
    }

    // ── find_json_string_end ──

    #[test]
    fn json_string_end_escape_aware() {
        // 内容: \"bX" —— 索引1 的转义引号不是终点，索引4 的引号才是
        let s: String = r#"\"bX""#.chars().collect();
        assert_eq!(find_json_string_end(&s), Some(4));
        // 无终点
        assert_eq!(find_json_string_end("ab"), None);
    }

    // ── normalize ──

    #[test]
    fn normalize_first_line_and_truncation_mark() {
        assert_eq!(normalize("line1\nline2"), "line1…");
        assert_eq!(normalize("  single  "), "single");
    }

    #[test]
    fn normalize_strips_hash_comment() {
        assert_eq!(normalize("cmd arg # comment"), "cmd arg");
    }
}
