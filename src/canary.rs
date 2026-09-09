//! Canary — 节点已知好输入库（cognition spec §7 缺口 #2）。
//!
//! 五分类归因闸的硬门禁：判定 class2（上游投毒）vs class3/4（本地描述/能力）
//! 的唯一手段是用设计期归档的 canary 输入重跑本节点。
//!
//! - canary 绿（canary 通过 + 真实输入失败）→ 上游投毒，本节点清白，不落 patch
//! - canary 红（canary 也失败）→ 本地问题（class3 描述错 / class4 能力不足）
//! - 无 canary → 禁止本地 patch（class2 无法排除，硬门禁）
//!
//! canary 记录 = 固定输入快照（llm 节点：prompt 文本；run 节点：命令体）
//! + expect 谓词（when.rs 语法，@self.field 引用结果字段）。
//!
//! 全部纯函数 + *_conn 注入（与 db.rs 惯例一致，测试用内存库）。

use crate::db::CanaryRow;
use rusqlite::Connection;

/// 新建 canary 记录（add 子命令的纯逻辑）。
/// expect 为空时默认 `@self.ok == 1`（llm 桥标准字段）。
pub fn normalize_expect(expect: &str) -> String {
    let e = expect.trim();
    if e.is_empty() {
        "@self.ok == 1".to_string()
    } else {
        e.to_string()
    }
}

/// canary 校验：把 run 出的结果文本按 expect 谓词求值。
/// 复用 when.rs 求值器；结果构造为 BTreeMap 单元素（与 check_contract 同法）。
pub fn eval_expect(expect: &str, result_text: &str) -> bool {
    use crate::ast::Value;
    let mut results = std::collections::BTreeMap::new();
    results.insert("self".to_string(), Value::Text(result_text.to_string()));
    crate::when::eval_cond_str(expect, &std::collections::BTreeMap::new(), &results)
}

/// db CRUD 薄封装（conn 注入）。
pub fn add_canary_conn(
    conn: &Connection,
    pipeline: &str,
    proc_name: &str,
    input: &str,
    expect: &str,
    note: &str,
) -> Result<i64, String> {
    let expect = normalize_expect(expect);
    let now = now_str();
    conn.execute(
        "INSERT INTO canaries (pipeline, proc_name, input, expect, note, saved_at) VALUES (?1,?2,?3,?4,?5,?6)",
        rusqlite::params![pipeline, proc_name, input, expect, note, now],
    )
    .map_err(|e| format!("canary add failed: {e}"))?;
    Ok(conn.last_insert_rowid())
}

pub fn list_canaries_conn(conn: &Connection, proc_name: Option<&str>) -> Vec<CanaryRow> {
    let mut sql = "SELECT id, pipeline, proc_name, input, expect, note, saved_at FROM canaries".to_string();
    if proc_name.is_some() {
        sql.push_str(" WHERE proc_name = ?1");
    }
    sql.push_str(" ORDER BY id");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let map_row = |row: &rusqlite::Row| -> rusqlite::Result<CanaryRow> {
        Ok(CanaryRow {
            id: row.get(0)?,
            pipeline: row.get(1)?,
            proc_name: row.get(2)?,
            input: row.get(3)?,
            expect: row.get(4)?,
            note: row.get(5)?,
            saved_at: row.get(6)?,
        })
    };
    match proc_name {
        Some(p) => stmt
            .query_map(rusqlite::params![p], map_row)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
        None => stmt
            .query_map([], map_row)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default(),
    }
}

pub fn rm_canary_conn(conn: &Connection, id: i64) -> Result<usize, String> {
    let n = conn
        .execute("DELETE FROM canaries WHERE id = ?1", rusqlite::params![id])
        .map_err(|e| format!("canary rm failed: {e}"))?;
    if n == 0 {
        return Err(format!("canary id {id} not found"));
    }
    Ok(n)
}

/// canary 通过历史（硬门禁查询：has_pass_record）。
/// canary_runs 表记录每次 canary run 的结果，pass=1 才计入。
pub fn has_canary_pass_conn(conn: &Connection, pipeline: &str, proc_name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM canary_runs WHERE pipeline = ?1 AND proc_name = ?2 AND pass = 1",
        rusqlite::params![pipeline, proc_name],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

pub fn record_canary_run_conn(
    conn: &Connection,
    pipeline: &str,
    proc_name: &str,
    pass: bool,
    detail: &str,
) -> Result<i64, String> {
    let now = now_str();
    conn.execute(
        "INSERT INTO canary_runs (pipeline, proc_name, pass, detail, ran_at) VALUES (?1,?2,?3,?4,?5)",
        rusqlite::params![pipeline, proc_name, pass as i64, detail, now],
    )
    .map_err(|e| format!("canary run record failed: {e}"))?;
    Ok(conn.last_insert_rowid())
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
        conn.execute_batch(include_str!("canary_schema.sql")).unwrap();
        conn
    }

    #[test]
    fn normalize_expect_default() {
        assert_eq!(normalize_expect(""), "@self.ok == 1");
        assert_eq!(normalize_expect("  @self.score >= 80  "), "@self.score >= 80");
    }

    #[test]
    fn eval_expect_structured_field() {
        let ok = "§§FIELDS§§ok=1§§score=85§§RAW§§raw";
        assert!(eval_expect("@self.score >= 80", ok));
        assert!(!eval_expect("@self.score >= 90", ok));
        assert!(eval_expect("@self.ok == 1", ok));
    }

    #[test]
    fn canary_crud_and_pass_record() {
        let conn = mem_conn();
        let id = add_canary_conn(&conn, "p", "judge", "已知好输入", "@self.score >= 80", "note").unwrap();
        assert!(id > 0);
        let rows = list_canaries_conn(&conn, Some("judge"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].expect, "@self.score >= 80");
        // 硬门禁：无通过记录
        assert!(!has_canary_pass_conn(&conn, "p", "judge"));
        record_canary_run_conn(&conn, "p", "judge", true, "score=85").unwrap();
        assert!(has_canary_pass_conn(&conn, "p", "judge"));
        // rm
        rm_canary_conn(&conn, id).unwrap();
        assert!(list_canaries_conn(&conn, Some("judge")).is_empty());
        assert!(rm_canary_conn(&conn, id).is_err());
    }
}
