//! Incident — 事故一等实体（cognition spec §7 缺口 #3）。
//!
//! 一次运行 × 一个节点的所有误差信号**先束成 incident 候选**再进归因，
//! 不逐条处理（spec §1）。runs 表只有节点结果；incidents 表是信号束的载体：
//!
//! - signals  : 信号栈层号列表（如 "L0.5,L2"——contract violation = L1/L2，
//!              finish_reason=length = L0.5，超时 = L0）
//! - err_code : errflow 分类（contract/truncation/timeout/…）
//! - evidence : 证据快照（错误消息 + 结果字段摘要）
//! - status   : open → closed（归因闸处置完毕后关闭）
//!
//! 落库点：executor 的 Left 落库处（与 record_run 同点位）。去重：同
//! (pipeline, proc_name, err_code) 的 open incident 只留一条（信号束聚合，
//! 不逐条开新事故）。

use crate::db::IncidentRow;
use rusqlite::Connection;

/// 从错误消息 + 结果文本提取信号栈层号（L0/L0.5/L1/L2…）。
/// 纯函数，测试钉住。
pub fn signals_from_error(err_code: &str, err_msg: &str, result_text: &str) -> Vec<String> {
    let mut sig = Vec::new();
    match err_code {
        "contract" => {
            // contract violation 细分：缺字段 = L1；谓词违例 = L2
            if err_msg.contains("missing required output field") {
                sig.push("L1".to_string());
            } else {
                sig.push("L2".to_string());
            }
        }
        "timeout" | "network" | "memory" | "resource" => sig.push("L0".to_string()),
        "truncation" => sig.push("L0.5".to_string()),
        "auth" | "ratelimit" => sig.push("L0.5".to_string()),
        _ => sig.push("L0".to_string()),
    }
    // L0.5 溯源：结果里带 finish_reason=length 的 META（截断现场证据）
    if result_text.contains("meta_finish_reason=length") && !sig.contains(&"L0.5".to_string()) {
        sig.push("L0.5".to_string());
    }
    sig
}

/// 证据快照：err_msg + 结果字段前几个（截 300 字，入库体积控制）。
pub fn evidence_snapshot(err_msg: &str, result_text: &str) -> String {
    let mut ev = format!("err: {}", trunc(err_msg, 200));
    if let Some(rest) = result_text.strip_prefix("§§FIELDS§§") {
        let fields = rest.split("§§RAW§§").next().unwrap_or(rest);
        ev.push_str(" | fields: ");
        ev.push_str(trunc(fields, 100));
    }
    ev
}

fn trunc(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// 落库（聚合去重）：同 (pipeline, proc, err_code) 已有 open → 追加计数语义
/// （这里实现为更新 evidence + created_at 保持最新现场，不开新行）。
pub fn record_incident_conn(
    conn: &Connection,
    pipeline: &str,
    proc_name: &str,
    err_code: &str,
    err_msg: &str,
    result_text: &str,
) -> i64 {
    let signals = signals_from_error(err_code, err_msg, result_text).join(",");
    let evidence = evidence_snapshot(err_msg, result_text);
    let now = now_str();
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM incidents WHERE pipeline=?1 AND proc_name=?2 AND err_code=?3 AND status='open'",
            rusqlite::params![pipeline, proc_name, err_code],
            |r| r.get(0),
        )
        .ok();
    match existing {
        Some(id) => {
            let _ = conn.execute(
                "UPDATE incidents SET signals=?1, evidence=?2, created_at=?3 WHERE id=?4",
                rusqlite::params![signals, evidence, now, id],
            );
            id
        }
        None => {
            let _ = conn.execute(
                "INSERT INTO incidents (pipeline, proc_name, signals, err_code, evidence, status, created_at) VALUES (?1,?2,?3,?4,?5,'open',?6)",
                rusqlite::params![pipeline, proc_name, signals, err_code, evidence, now],
            );
            conn.last_insert_rowid()
        }
    }
}

pub fn list_incidents_conn(conn: &Connection, status: Option<&str>) -> Vec<IncidentRow> {
    let mut sql = "SELECT id, pipeline, proc_name, signals, err_code, evidence, status, created_at, closed_at FROM incidents".to_string();
    if status.is_some() {
        sql.push_str(" WHERE status = ?1");
    }
    sql.push_str(" ORDER BY id DESC");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let map = |row: &rusqlite::Row| -> rusqlite::Result<IncidentRow> {
        Ok(IncidentRow {
            id: row.get(0)?,
            pipeline: row.get(1)?,
            proc_name: row.get(2)?,
            signals: row.get(3)?,
            err_code: row.get(4)?,
            evidence: row.get(5)?,
            status: row.get(6)?,
            created_at: row.get(7)?,
            closed_at: row.get(8)?,
        })
    };
    match status {
        Some(s) => stmt
            .query_map(rusqlite::params![s], map)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
        None => stmt
            .query_map([], map)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
    }
}

pub fn close_incident_conn(conn: &Connection, id: i64, resolution: &str) -> Result<(), String> {
    let n = conn
        .execute(
            "UPDATE incidents SET status='closed', closed_at=?2, evidence = evidence || ?3 WHERE id=?1 AND status='open'",
            rusqlite::params![id, now_str(), format!(" | resolved: {}", trunc(resolution, 200))],
        )
        .map_err(|e| format!("close failed: {e}"))?;
    if n == 0 {
        return Err(format!("incident {id} not found or already closed"));
    }
    Ok(())
}

fn now_str() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}", d.as_secs()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS incidents (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pipeline TEXT NOT NULL,
                proc_name TEXT NOT NULL,
                signals TEXT NOT NULL DEFAULT '',
                err_code TEXT DEFAULT '',
                evidence TEXT DEFAULT '',
                status TEXT NOT NULL DEFAULT 'open',
                created_at TEXT DEFAULT '',
                closed_at TEXT DEFAULT ''
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn signals_layer_mapping() {
        // L1 缺字段 / L2 谓词违例 / L0 传输 / L0.5 截断
        assert_eq!(
            signals_from_error("contract", "contract violation: proc 'g' missing required output field 'path'", ""),
            vec!["L1"]
        );
        assert_eq!(
            signals_from_error("contract", "contract violation: proc 'j' invariant failed: @self.score >= 80", ""),
            vec!["L2"]
        );
        assert_eq!(signals_from_error("timeout", "run timed out", ""), vec!["L0"]);
        assert_eq!(signals_from_error("truncation", "finish_reason length", ""), vec!["L0.5"]);
        // 结果带 META 截断证据 → 追加 L0.5
        assert_eq!(
            signals_from_error("data", "value error", "§§FIELDS§§meta_finish_reason=length§§RAW§§x"),
            vec!["L0", "L0.5"]
        );
    }

    #[test]
    fn incident_dedupe_and_lifecycle() {
        let conn = mem_conn();
        let id1 = record_incident_conn(&conn, "p", "judge", "contract", "invariant failed: x", "raw");
        let id2 = record_incident_conn(&conn, "p", "judge", "contract", "invariant failed: y", "raw");
        assert_eq!(id1, id2, "同 (pipeline,proc,code) open 事故聚合为一条");
        let rows = list_incidents_conn(&conn, Some("open"));
        assert_eq!(rows.len(), 1);
        assert!(rows[0].signals.contains("L2"));
        // 不同 err_code → 新事故
        record_incident_conn(&conn, "p", "judge", "timeout", "t", "raw");
        assert_eq!(list_incidents_conn(&conn, Some("open")).len(), 2);
        // close
        close_incident_conn(&conn, id1, "class3: 修了 desc，canary 转绿").unwrap();
        assert_eq!(list_incidents_conn(&conn, Some("open")).len(), 1);
        assert!(close_incident_conn(&conn, id1, "again").is_err(), "重复关闭报错");
        let closed = list_incidents_conn(&conn, Some("closed"));
        assert_eq!(closed.len(), 1);
        assert!(closed[0].evidence.contains("resolved: class3"));
    }
}
