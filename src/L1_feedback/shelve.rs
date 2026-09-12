//! Shelve — 判别实验模糊搁置队列（cognition spec §7 缺口 #5 test-the-test）。
//!
//! 五分类归因闸靠 canary 判别 class2（上游投毒）vs class3/4（本地问题）：
//! - canary 稳定通过 + 真实输入失败 → class2，不落本地 patch
//! - canary 也失败 → class3/4 本地问题
//! - **通过率落灰区 → 判别实验本身模糊**——强Test不可信，写下的任何认知都可能是错的
//!
//! test-the-test 原则：结果模糊时唯一安全动作是搁置进 designer 审队列，
//! 等人工/designer 裁决，绝不自动写认知（patch/权重）。
//!
//! 全部纯函数 + *_conn 注入（与 canary/l4 惯例一致，测试用内存库）。

use crate::db::ShelvedRow;
use rusqlite::Connection;

/// 灰区边界：canary 通过率 ≥ 此值 → 明确绿（class2）
pub const SHELVE_GREEN_THRESHOLD: f64 = 0.75;
/// 灰区边界：canary 通过率 < 此值 → 明确红（class3/4）
pub const SHELVE_RED_THRESHOLD: f64 = 0.25;

/// 判别实验裁决。
#[derive(Debug, Clone, PartialEq)]
pub enum Discriminant {
    /// canary 稳定通过 → 上游投毒（class2），本节点清白
    Green,
    /// canary 也失败 → 本地问题（class3 描述错 / class4 能力不足）
    Red,
    /// 通过率在灰区 → 判别实验模糊，强制搁置
    Ambiguous,
    /// 无 canary 可跑 → 硬门禁：禁止本地 patch（class2 无法排除）
    NoCanary,
}

/// 判别实验分类器（纯函数）。
/// pass_rate: Some(通过率 0.0-1.0)；None = 无 canary 记录。
pub fn classify_discriminant(pass_rate: Option<f64>) -> Discriminant {
    match pass_rate {
        None => Discriminant::NoCanary,
        Some(r) if r >= SHELVE_GREEN_THRESHOLD => Discriminant::Green,
        Some(r) if r < SHELVE_RED_THRESHOLD => Discriminant::Red,
        Some(_) => Discriminant::Ambiguous,
    }
}

/// 搁置进 designer 审队列（模糊判别的唯一合法去向）。
pub fn shelve_conn(
    conn: &Connection,
    pipeline: &str,
    proc_name: &str,
    reason: &str,
    evidence: &str,
) -> Result<i64, String> {
    let now = now_str();
    conn.execute(
        "INSERT INTO shelved (pipeline, proc_name, reason, evidence, status, created_at) VALUES (?1,?2,?3,?4,'open',?5)",
        rusqlite::params![pipeline, proc_name, reason, evidence, now],
    )
    .map_err(|e| format!("shelve failed: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// designer 裁决闭环：resolution 写明最终归因（class1-5），状态翻 resolved。
/// 重复 resolve 报错（裁决不可撤销，防覆盖认知历史）。
pub fn resolve_shelved_conn(conn: &Connection, id: i64, resolution: &str) -> Result<(), String> {
    let status: String = conn
        .query_row(
            "SELECT status FROM shelved WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .map_err(|_| format!("shelved #{id} not found"))?;
    if status != "open" {
        return Err(format!(
            "shelved #{id} already resolved — verdicts are immutable"
        ));
    }
    let now = now_str();
    let n = conn
        .execute(
            "UPDATE shelved SET status='resolved', resolution=?1, resolved_at=?2 WHERE id=?3",
            rusqlite::params![resolution, now, id],
        )
        .map_err(|e| format!("shelve resolve failed: {e}"))?;
    if n == 0 {
        return Err(format!("shelved #{id} not found"));
    }
    Ok(())
}

pub fn list_shelved_conn(conn: &Connection, status: Option<&str>) -> Vec<ShelvedRow> {
    let mut sql = "SELECT id, pipeline, proc_name, reason, evidence, status, resolution, created_at, resolved_at FROM shelved".to_string();
    if status.is_some() {
        sql.push_str(" WHERE status = ?1");
    }
    sql.push_str(" ORDER BY id DESC");
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let map_row = |row: &rusqlite::Row| -> rusqlite::Result<ShelvedRow> {
        Ok(ShelvedRow {
            id: row.get(0)?,
            pipeline: row.get(1)?,
            proc_name: row.get(2)?,
            reason: row.get(3)?,
            evidence: row.get(4)?,
            status: row.get(5)?,
            resolution: row.get(6).unwrap_or_default(),
            created_at: row.get(7)?,
            resolved_at: row.get(8).unwrap_or_default(),
        })
    };
    // status=None 时 SQL 无占位符——params![] 空参才不报参数计数错（曾致静默空表）
    let rows = if let Some(st) = status {
        stmt.query_map(rusqlite::params![st], map_row)
    } else {
        stmt.query_map(rusqlite::params![], map_row)
    };
    match rows {
        Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// CLI 渲染。
pub fn render_shelved_conn(conn: &Connection, status: Option<&str>) -> String {
    let rows = list_shelved_conn(conn, status);
    if rows.is_empty() {
        return format!(
            "(no shelved items{})",
            status.map(|s| format!(" [{s}]")).unwrap_or_default()
        );
    }
    let mut out = String::new();
    for r in rows {
        out.push_str(&format!(
            "#{} [{}] {}::{} reason={}\n    evidence: {}\n",
            r.id, r.status, r.pipeline, r.proc_name, r.reason, r.evidence
        ));
        if r.status == "resolved" {
            out.push_str(&format!("    resolved: {}\n", r.resolution));
        }
    }
    out
}

fn now_str() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("unix:{}", d.as_secs()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("shelve_schema.sql"))
            .unwrap();
        conn
    }

    #[test]
    fn thresholds_classify() {
        assert_eq!(classify_discriminant(None), Discriminant::NoCanary);
        assert_eq!(classify_discriminant(Some(1.0)), Discriminant::Green);
        assert_eq!(classify_discriminant(Some(0.75)), Discriminant::Green);
        assert_eq!(classify_discriminant(Some(0.74)), Discriminant::Ambiguous);
        assert_eq!(classify_discriminant(Some(0.5)), Discriminant::Ambiguous);
        assert_eq!(classify_discriminant(Some(0.25)), Discriminant::Ambiguous);
        assert_eq!(classify_discriminant(Some(0.24)), Discriminant::Red);
        assert_eq!(classify_discriminant(Some(0.0)), Discriminant::Red);
    }

    #[test]
    fn ambiguous_shelves_and_resolves() {
        let conn = mem();
        let id = shelve_conn(
            &conn,
            "p",
            "node",
            "ambiguous canary rate 0.5",
            "canary 1/2 pass",
        )
        .unwrap();
        let rows = list_shelved_conn(&conn, Some("open"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reason, "ambiguous canary rate 0.5");
        resolve_shelved_conn(&conn, id, "class4: model capability gap").unwrap();
        let open = list_shelved_conn(&conn, Some("open"));
        let resolved = list_shelved_conn(&conn, Some("resolved"));
        assert!(open.is_empty());
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].resolution, "class4: model capability gap");
    }

    #[test]
    fn resolution_is_immutable() {
        let conn = mem();
        let id = shelve_conn(&conn, "p", "n", "r", "e").unwrap();
        resolve_shelved_conn(&conn, id, "class2").unwrap();
        assert!(resolve_shelved_conn(&conn, id, "class3").is_err());
    }

    #[test]
    fn missing_id_errors() {
        let conn = mem();
        assert!(resolve_shelved_conn(&conn, 99, "x").is_err());
    }

    #[test]
    fn render_lists_items() {
        let conn = mem();
        let out = render_shelved_conn(&conn, None);
        assert!(out.contains("(no shelved items)"));
        shelve_conn(&conn, "p", "n", "why", "ev").unwrap();
        let out = render_shelved_conn(&conn, None);
        assert!(out.contains("#1 [open] p::n"));
    }
}
