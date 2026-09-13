//! L4 端到端复核 — 冷启动 log-only（cognition spec §7 缺口 #4）。
//!
//! L4 = 任务意图 vs deliver 的独立复核（唯一合法 LLM 检测层，独立会话最小上下文）。
//! 冷启动矛盾：复核者自己也需要验证。解法：**log-only 攒标签**——
//! 初期 L4 复核结果只记录不拦截；攒够 N 个操作者标签且 L4 与标签一致率
//! 达标，才升格 enforcing（失败可拦截）。
//!
//! 全部纯函数 + *_conn 注入（与 db.rs 惯例一致，测试用内存库）。

use crate::db::L4ReviewRow;
use rusqlite::Connection;

/// 升格阈值（v1 固定，后续可迁 config）：需要 ≥8 个操作者标签
pub const L4_ENFORCE_MIN_LABELED: i64 = 8;
/// L4 verdict 与操作者标签一致率阈值（0.0-1.0）
pub const L4_ENFORCE_MIN_AGREEMENT: f64 = 0.7;

/// 升格状态机：log_only（冷启动，只记不拦）→ enforcing（可拦截）。
#[derive(Debug, Clone, PartialEq)]
pub enum L4Phase {
    LogOnly,
    Enforcing,
}

impl L4Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            L4Phase::LogOnly => "log_only",
            L4Phase::Enforcing => "enforcing",
        }
    }
}

/// 计算当前库的 L4 升格状态。
/// labeled 条数 ≥ 阈值 且 一致率 ≥ 阈值 → Enforcing，否则 LogOnly。
pub fn phase_for_conn(conn: &Connection) -> L4Phase {
    let labeled: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM l4_reviews WHERE label IN ('ok','bad')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if labeled < L4_ENFORCE_MIN_LABELED {
        return L4Phase::LogOnly;
    }
    match agreement_rate(conn) {
        Some(a) if a >= L4_ENFORCE_MIN_AGREEMENT => L4Phase::Enforcing,
        _ => L4Phase::LogOnly,
    }
}

/// 一致率：在 labeled 样本中，L4 verdict 与操作者标签同向的比例。
/// verdict=pass + label=ok，或 verdict=fail + label=bad 记为一致。
/// labeled=0 → None（冷启动无标签可算）。
pub fn agreement_rate(conn: &Connection) -> Option<f64> {
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM l4_reviews WHERE label IN ('ok','bad')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if total == 0 {
        return None;
    }
    let agree: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM l4_reviews WHERE label IN ('ok','bad') AND ((verdict='pass' AND label='ok') OR (verdict='fail' AND label='bad'))",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Some(agree as f64 / total as f64)
}

/// 记录一次 L4 复核（log-only 阶段的主要写入面）。
/// verdict: pass/fail；evidence: 一句话证据；run_id: 关联 run。
pub fn record_review_conn(
    conn: &Connection,
    pipeline: &str,
    verdict: &str,
    evidence: &str,
    run_id: Option<i64>,
) -> Result<i64, String> {
    let v = normalize_verdict(verdict)?;
    let now = now_str();
    conn.execute(
        "INSERT INTO l4_reviews (pipeline, verdict, evidence, label, run_id, reviewed_at) VALUES (?1,?2,?3,'',?4,?5)",
        rusqlite::params![pipeline, v, evidence, run_id, now],
    )
    .map_err(|e| format!("l4 review record failed: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// 操作者打标签（校准面）。label: ok/bad（ok=这次 deliver 确实好，bad=确实坏）。
/// 复核通过后补标——log-only 阶段攒标签就是为升格准备。
pub fn label_review_conn(conn: &Connection, id: i64, label: &str) -> Result<(), String> {
    label_review_sourced_conn(conn, id, label, "human")
}

/// v0.18.6 P1-2：带出处打标。human（默认，兼容旧调用）/ blind:regcheck3
/// （盲评 verdict 独立源——regcheck3 的 auto vs baseline 相对判断，与 L4
/// reviewer 的 intent+deliver 判断不同源，可做校准标签）。
pub fn label_review_sourced_conn(
    conn: &Connection,
    id: i64,
    label: &str,
    source: &str,
) -> Result<(), String> {
    let l = normalize_label(label)?;
    let src = match source {
        "blind:regcheck3" => "blind:regcheck3",
        _ => "human",
    };
    let n = conn
        .execute(
            "UPDATE l4_reviews SET label = ?1, label_source = ?3 WHERE id = ?2",
            rusqlite::params![l, id, src],
        )
        .map_err(|e| format!("l4 label failed: {e}"))?;
    if n == 0 {
        return Err(format!("l4 review #{id} not found"));
    }
    Ok(())
}

/// L4 复核 prompt 生成：独立会话最小上下文（intent + deliver 摘要，不带管线内部）。
pub fn review_prompt(intent: &str, deliver_summary: &str) -> String {
    format!(
        "你是独立复核者。只依据给定的任务意图与最终交付物，判断交付是否达成了意图。\
不要参考任何管线内部执行细节。输出一行判断：pass 或 fail，加一句话证据。\n\n\
## 任务意图\n{}\n\n## 交付物摘要\n{}",
        intent, deliver_summary
    )
}

fn normalize_verdict(v: &str) -> Result<&str, String> {
    match v.trim() {
        "pass" => Ok("pass"),
        "fail" => Ok("fail"),
        other => Err(format!("verdict must be pass/fail, got {other:?}")),
    }
}

fn normalize_label(l: &str) -> Result<&str, String> {
    match l.trim() {
        "ok" => Ok("ok"),
        "bad" => Ok("bad"),
        other => Err(format!("label must be ok/bad, got {other:?}")),
    }
}

fn now_str() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("unix:{}", d.as_secs()))
        .unwrap_or_default()
}

// ── 查询/渲染 ──

pub fn list_reviews_conn(conn: &Connection, limit: usize) -> Vec<L4ReviewRow> {
    let mut stmt = match conn.prepare(
        "SELECT id, pipeline, verdict, evidence, label, run_id, reviewed_at FROM l4_reviews ORDER BY id DESC LIMIT ?1",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map(rusqlite::params![limit as i64], |row| {
        Ok(L4ReviewRow {
            id: row.get(0)?,
            pipeline: row.get(1)?,
            verdict: row.get(2)?,
            evidence: row.get(3)?,
            label: row.get(4)?,
            run_id: row.get(5).unwrap_or_default(),
            reviewed_at: row.get(6)?,
        })
    });
    match rows {
        Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// CLI 列表渲染 + 当前升格状态。
pub fn render_reviews_conn(conn: &Connection, limit: usize) -> String {
    let phase = phase_for_conn(conn);
    let rate = agreement_rate(conn);
    let mut out = format!(
        "L4 phase: {} (agreement: {})\n",
        phase.as_str(),
        match rate {
            Some(a) => format!("{:.2}", a),
            None => "n/a".to_string(),
        }
    );
    let rows = list_reviews_conn(conn, limit);
    if rows.is_empty() {
        out.push_str("(no l4 reviews)");
        return out;
    }
    for r in &rows {
        out.push_str(&format!(
            "#{} [{}] {} evidence={} label={}\n",
            r.id,
            r.verdict,
            r.pipeline,
            r.evidence,
            if r.label.is_empty() { "-" } else { &r.label }
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("l4_schema.sql")).unwrap();
        conn
    }

    #[test]
    fn cold_start_is_log_only() {
        let conn = mem();
        assert_eq!(phase_for_conn(&conn), L4Phase::LogOnly);
    }

    #[test]
    fn record_and_label_then_check_agreement() {
        let conn = mem();
        let id = record_review_conn(&conn, "p", "pass", "all gates green", None).unwrap();
        label_review_conn(&conn, id, "ok").unwrap();
        assert_eq!(agreement_rate(&conn), Some(1.0));
    }

    /// v0.18.6 P1-2：盲评出处打标 + 幂等跳过语义（有 label 不覆盖）。
    #[test]
    fn sourced_label_writes_source_and_keeps_idempotent() {
        let conn = mem();
        let id = record_review_conn(&conn, "p", "pass", "e", None).unwrap();
        label_review_sourced_conn(&conn, id, "ok", "blind:regcheck3").unwrap();
        let (label, src): (String, String) = conn
            .query_row(
                "SELECT label, label_source FROM l4_reviews WHERE id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(label, "ok");
        assert_eq!(src, "blind:regcheck3");
        // 未知 source 归一为 human（fail-safe，不炸）
        let id2 = record_review_conn(&conn, "p", "fail", "e2", None).unwrap();
        label_review_sourced_conn(&conn, id2, "bad", "weird-source").unwrap();
        let src2: String = conn
            .query_row(
                "SELECT label_source FROM l4_reviews WHERE id = ?1",
                rusqlite::params![id2],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(src2, "human");
    }

    #[test]
    fn agreement_counts_mismatches() {
        let conn = mem();
        let a = record_review_conn(&conn, "p", "pass", "e1", None).unwrap();
        label_review_conn(&conn, a, "bad").unwrap(); // 不一致
        let b = record_review_conn(&conn, "p", "fail", "e2", None).unwrap();
        label_review_conn(&conn, b, "bad").unwrap(); // 一致
        assert_eq!(agreement_rate(&conn), Some(0.5));
    }

    #[test]
    fn promotion_needs_min_labeled_and_agreement() {
        let conn = mem();
        // 7 个一致标签：不够 8 个，仍 log_only
        for _ in 0..7 {
            let id = record_review_conn(&conn, "p", "pass", "e", None).unwrap();
            label_review_conn(&conn, id, "ok").unwrap();
        }
        assert_eq!(phase_for_conn(&conn), L4Phase::LogOnly);
        // 第 8 个：升格 enforcing
        let id = record_review_conn(&conn, "p", "pass", "e", None).unwrap();
        label_review_conn(&conn, id, "ok").unwrap();
        assert_eq!(phase_for_conn(&conn), L4Phase::Enforcing);
    }

    #[test]
    fn low_agreement_blocks_promotion() {
        let conn = mem();
        // 8 个标签但一半不一致（50% < 70%）：不升格
        for i in 0..8 {
            let id = record_review_conn(
                &conn,
                "p",
                if i % 2 == 0 { "pass" } else { "fail" },
                "e",
                None,
            )
            .unwrap();
            label_review_conn(&conn, id, "ok").unwrap();
        }
        assert_eq!(phase_for_conn(&conn), L4Phase::LogOnly);
    }

    #[test]
    fn bad_verdict_and_bad_labels_agree() {
        let conn = mem();
        for _ in 0..8 {
            let id = record_review_conn(&conn, "p", "fail", "e", None).unwrap();
            label_review_conn(&conn, id, "bad").unwrap();
        }
        assert_eq!(phase_for_conn(&conn), L4Phase::Enforcing);
    }

    #[test]
    fn invalid_verdict_rejected() {
        let conn = mem();
        assert!(record_review_conn(&conn, "p", "maybe", "e", None).is_err());
    }

    #[test]
    fn label_missing_review_errors() {
        let conn = mem();
        assert!(label_review_conn(&conn, 99, "ok").is_err());
    }

    #[test]
    fn review_prompt_is_minimal_context() {
        let p = review_prompt("做X", "交付Y");
        assert!(p.contains("任务意图"));
        assert!(p.contains("做X"));
        assert!(p.contains("交付Y"));
        assert!(!p.contains("plan"));
    }
}
