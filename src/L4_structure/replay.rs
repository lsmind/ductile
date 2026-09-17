//! v0.20 Replay-RSI P3：patch 重放门（confirm 前的重放对比 + 永不退化条款）。
//!
//! 论文 §3 Policy selection：候选集含 π0（当前策略），按全历史重放均分选优，
//! 结构保证 V* ≥ V⁰。ductile 落法：
//! - 「策略」的实例 = patches 表的 tentative patch（改 guide/when/retry 等 field）
//! - 「重放」= 对全历史 sessions 重新评分。P3 的保守语义（可判定、零执行）：
//!   patch 改变的 field 会影响未来执行，但历史 sessions 是既成事实——
//!   重放对比的对象是「策略若早就在场，历史树会怎么被评分」。
//!   v0 代理：field 不改变已记录的节点结果，只改变「会不会去跑那个节点」。
//!   因此对比 = 同一棵树上两种前缀展开：
//!     π0 展开：全树（实际发生的）
//!     πnew 展开：patch 声明的修剪/加宽效果落成 batch/prefix 变化
//!   ——效果声明由 patch.proposal 的结构化字段承载（见 parse_effect），
//!   LLM 只产出声明，评分是纯算术（裁判确定性红线）。
//!
//! 「永不退化」落法：replay_verdict 返回 Confirm/Reject，transition 层
//! （cmd_patch_confirm_replay）拒掉 Reject——制度保证，不靠 LLM 自觉。

use crate::core::ast::Pipeline;
use crate::core::replay_eval::{beta_sweep, replay_score, ReplayTrace};
use crate::db;
use crate::L3_dsl::parser::parse_pipeline_file;
use std::collections::BTreeMap;

pub struct TreeNode {
    pub proc: String,
    pub impls: Vec<(String, String, i64)>, // (impl, status, latency_ms)
    pub attempts: usize,
    pub score: f64,
    pub status: String,
}

pub struct SessionTree {
    pub session: String,
    pub pipeline: String,
    pub recorded_at: String,
    pub nodes: Vec<TreeNode>,
    pub edges: Vec<(String, String)>, // (parent, child) 静态 needs 边
    pub v_proxy: f64,                 // Eq.1 二值代理（P2 前的占位评分）
}

/// runs → sessions（按 session 列分组；空 session 的旧行归 "legacy" 桶按 pipeline+时间聚）。
fn load_sessions(pipeline: Option<&str>) -> Vec<SessionTree> {
    let conn = db::open();
    let mut where_clause = String::from("WHERE session != ''");
    if let Some(p) = pipeline {
        where_clause.push_str(&format!(" AND pipeline = '{}'", p.replace('\'', "''")));
    }
    let mut stmt = conn
        .prepare(&format!(
            "SELECT session, pipeline, proc_name, impl_name, status, latency_ms, score, recorded_at
             FROM runs {where_clause} ORDER BY session, id"
        ))
        .unwrap();
    let rows: Vec<(String, String, String, String, String, i64, Option<f64>, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })
        .unwrap()
        .filter_map(|x| x.ok())
        .collect();

    let mut sessions: BTreeMap<String, Vec<(String, String, String, String, i64, Option<f64>, String)>> =
        BTreeMap::new();
    let mut pl_name = BTreeMap::new();
    let mut last_ts = BTreeMap::new();
    for (sess, pl, proc, imp, st, lat, sc, ts) in rows {
        pl_name.insert(sess.clone(), pl);
        last_ts.entry(sess.clone()).and_modify(|t: &mut String| *t = ts.clone()).or_insert(ts.clone());
        sessions.entry(sess.clone()).or_default().push((proc, imp, st, sess.clone(), lat, sc, ts));
    }

    sessions
        .into_iter()
        .map(|(sess, rows)| {
            let pipeline = pl_name.get(&sess).cloned().unwrap_or_default();
            build_tree(&sess, &pipeline, rows, last_ts.get(&sess).cloned().unwrap_or_default())
        })
        .collect()
}

fn build_tree(
    session: &str,
    pipeline: &str,
    rows: Vec<(String, String, String, String, i64, Option<f64>, String)>,
    recorded_at: String,
) -> SessionTree {
    // 同 proc 多行合并（重试/impl 降级 = attempts）
    let mut nodes: BTreeMap<String, TreeNode> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for (proc, imp, st, _s, lat, sc, _t) in rows {
        let e = nodes.entry(proc.clone()).or_insert_with(|| {
            order.push(proc.clone());
            TreeNode {
                proc: proc.clone(),
                impls: Vec::new(),
                attempts: 0,
                score: 0.0,
                status: "Ok".into(),
            }
        });
        e.attempts += 1;
        e.impls.push((imp, st.clone(), lat));
        if st != "Ok" {
            e.status = st.clone();
        }
        if let Some(v) = sc {
            e.score = v;
        } else if st == "Ok" {
            e.score = 1.0;
        }
    }
    let node_list: Vec<TreeNode> = order.iter().map(|n| nodes.remove(n).unwrap()).collect();
    // 二值代理评分：Ok=1.0，Fail 节点计 0（Score 列优先）
    let max_score = node_list.iter().map(|n| n.score).fold(0.0_f64, f64::max);
    let n_probes = node_list.iter().map(|n| n.attempts).sum::<usize>() as f64;
    let k_rounds = node_list.len().max(1) as f64;
    let v_proxy = max_score - 0.02 * n_probes + 0.01 * n_probes / k_rounds;

    SessionTree {
        session: session.to_string(),
        pipeline: pipeline.to_string(),
        recorded_at,
        nodes: node_list,
        edges: Vec::new(), // 由 CLI 层从 AST 填（需要管线文件）
        v_proxy,
    }
}

/// 静态拓扑边（needs）从管线 AST 提取——文件是拓扑权威源。
fn static_edges(pl: &Pipeline) -> Vec<(String, String)> {
    let mut edges = Vec::new();
    for proc in &pl.procs {
        for dep in &proc.needs {
            edges.push((dep.trim_start_matches('@').to_string(), proc.name.clone()));
        }
    }
    edges
}

pub fn cmd_tree(args: &[String]) -> Result<i32, String> {
    // ductile tree <pipeline> [--json] [--file <path.pipeline>]
    let mut json_out = false;
    let mut file: Option<String> = None;
    let mut pipeline: Option<String> = None;
    let mut it = args.iter();
    let mut first = true;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json_out = true,
            "--file" => {
                file = it.next().cloned();
            }
            other if first => {
                pipeline = Some(other.to_string());
                first = false;
            }
            _ => {}
        }
    }

    let mut trees = load_sessions(pipeline.as_deref());
    // 静态边注入（文件给了才注；多 session 共享同一拓扑）
    if let Some(f) = &file {
        if let Ok(pl) = parse_pipeline_file(f) {
            let edges = static_edges(&pl);
            for t in trees.iter_mut() {
                t.edges = edges.clone();
            }
        }
    }

    if trees.is_empty() {
        eprintln!(
            "no sessions found{} — runs 需 v0.20+ 引擎跑出（runs.session 列）",
            pipeline.map(|p| format!(" for pipeline '{p}'")).as_deref().unwrap_or("")
        );
        return Ok(1);
    }

    if json_out {
        // 手搓 JSON（项目零 serde 依赖纪律，与 hyper --json 同款）
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let items: Vec<String> = trees
            .iter()
            .map(|t| {
                let nodes: Vec<String> = t
                    .nodes
                    .iter()
                    .map(|n| {
                        let impls: Vec<String> = n
                            .impls
                            .iter()
                            .map(|(i, s, l)| {
                                format!(
                                    "{{\"impl\":\"{}\",\"status\":\"{}\",\"latency_ms\":{}}}",
                                    esc(i),
                                    esc(s),
                                    l
                                )
                            })
                            .collect();
                        format!(
                            "{{\"proc\":\"{}\",\"attempts\":{},\"status\":\"{}\",\"score\":{},\"impls\":[{}]}}",
                            esc(&n.proc),
                            n.attempts,
                            esc(&n.status),
                            n.score,
                            impls.join(",")
                        )
                    })
                    .collect();
                let edges: Vec<String> = t
                    .edges
                    .iter()
                    .map(|(a, b)| format!("\"{}->{}\"", esc(a), esc(b)))
                    .collect();
                format!(
                    "{{\"session\":\"{}\",\"pipeline\":\"{}\",\"recorded_at\":\"{}\",\"nodes\":[{}],\"edges\":[{}],\"v_proxy\":{:.4}}}",
                    esc(&t.session),
                    esc(&t.pipeline),
                    esc(&t.recorded_at),
                    nodes.join(","),
                    edges.join(","),
                    t.v_proxy
                )
            })
            .collect();
        println!("[{}]", items.join(",\n"));
        return Ok(0);
    }

    // 人读格式
    for t in &trees {
        println!(
            "session {}  [{}]  {}  ({} nodes)",
            t.session,
            t.pipeline,
            t.recorded_at,
            t.nodes.len()
        );
        for n in &t.nodes {
            let mark = if n.status == "Ok" { "✓" } else { "✗" };
            let impls: Vec<String> = n
                .impls
                .iter()
                .map(|(i, s, l)| format!("{i}:{s}({l}ms)"))
                .collect();
            println!(
                "  {mark} {} ×{}  score={:.2}  [{}]",
                n.proc,
                n.attempts,
                n.score,
                impls.join(", ")
            );
        }
        if !t.edges.is_empty() {
            let e: Vec<String> = t.edges.iter().map(|(a, b)| format!("{a}→{b}")).collect();
            println!("  edges: {}", e.join(" "));
        }
        println!("  V(β1=.02,β2=.01 proxy) = {:+.3}", t.v_proxy);
        println!();
    }
    Ok(0)
}

// ══════════════════════════════════════════════════════════════
// v0.20 Replay-RSI P3：patch 重放门（永不退化条款）
// ══════════════════════════════════════════════════════════════

/// patch 声明的重放效果（LLM 提议，结构化）。v0 可判定子集：
/// - SkipProcs{names}: 这些 proc 从不展开（历史树上 = 裁剪这些节点）
/// - ExtraWork{n}:     额外多跑 n 次尝试（尾部追加 n 个均值分节点）
/// 无声明 = NoEffect → 人审通道（纯 guide 文本不改重放分）。
#[derive(Debug, Clone, PartialEq)]
pub enum PatchEffect {
    SkipProcs(Vec<String>),
    ExtraWork(usize),
    NoEffect,
}

pub fn parse_effect(value: &str) -> PatchEffect {
    for line in value.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("replay_effect:") {
            let rest = rest.trim();
            if let Some(names) = rest.strip_prefix("skip_procs=") {
                let v: Vec<String> = names
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                return PatchEffect::SkipProcs(v);
            }
            if let Some(n) = rest.strip_prefix("extra_work=") {
                if let Ok(k) = n.parse::<usize>() {
                    return PatchEffect::ExtraWork(k);
                }
            }
        }
    }
    PatchEffect::NoEffect
}

/// 全历史重放对比：每 session 建 pi0/pi_new 两条轨迹，Eq.1 均分。
fn replay_compare(effect: &PatchEffect, beta1: f64, beta2: f64) -> Result<(f64, f64, usize), String> {
    let conn = db::open();
    let mut stmt = conn
        .prepare(
            "SELECT session, proc_name, score, status FROM runs
             WHERE session != '' ORDER BY session, id",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, Option<f64>, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map_err(|e| e.to_string())?
        .filter_map(|x| x.ok())
        .collect();

    let mut by_session: BTreeMap<String, Vec<(String, f64)>> = BTreeMap::new();
    for (sess, proc, sc, st) in rows {
        let score = sc.unwrap_or(if st == "Ok" { 1.0 } else { 0.0 });
        by_session.entry(sess).or_default().push((proc, score));
    }
    if by_session.is_empty() {
        return Err("no sessions in ledger — replay gate needs runs with session (v0.20+)".into());
    }

    let mut sum0 = 0.0;
    let mut sum_new = 0.0;
    for (_sess, nodes) in &by_session {
        let pi0 = ReplayTrace::serial(nodes.iter().map(|(_, s)| *s).collect());
        sum0 += replay_score(&pi0, beta1, beta2);
        let new_scores: Vec<f64> = match effect {
            PatchEffect::NoEffect => nodes.iter().map(|(_, s)| *s).collect(),
            PatchEffect::SkipProcs(skip) => nodes
                .iter()
                .filter(|(p, _)| !skip.contains(p))
                .map(|(_, s)| *s)
                .collect(),
            PatchEffect::ExtraWork(k) => {
                let mut v: Vec<f64> = nodes.iter().map(|(_, s)| *s).collect();
                let mean = v.iter().sum::<f64>() / v.len().max(1) as f64;
                v.extend(std::iter::repeat_n(mean, *k));
                v
            }
        };
        let pinew = ReplayTrace::serial(new_scores);
        sum_new += replay_score(&pinew, beta1, beta2);
    }
    let n = by_session.len();
    Ok((sum0 / n as f64, sum_new / n as f64, n))
}

pub enum ReplayVerdict {
    Confirm { v_pi0: f64, v_new: f64, n_sessions: usize },
    Reject { v_pi0: f64, v_new: f64, n_sessions: usize },
    HumanReview { reason: String },
}

pub fn replay_verdict(patch_value: &str, beta1: f64, beta2: f64) -> Result<ReplayVerdict, String> {
    let effect = parse_effect(patch_value);
    match effect {
        PatchEffect::NoEffect => Ok(ReplayVerdict::HumanReview {
            reason: "no replay_effect declaration — guide-text patches go to human review".into(),
        }),
        _ => {
            let (v_pi0, v_new, n) = replay_compare(&effect, beta1, beta2)?;
            if v_new > v_pi0 {
                Ok(ReplayVerdict::Confirm { v_pi0, v_new, n_sessions: n })
            } else {
                Ok(ReplayVerdict::Reject { v_pi0, v_new, n_sessions: n })
            }
        }
    }
}

/// CLI：ductile replay <patch_id> [beta1] [beta2]
/// exit 0=Confirm 2=HumanReview 3=Reject
pub fn cmd_replay(args: &[String]) -> Result<i32, String> {
    let Some(id_str) = args.first() else {
        eprintln!("usage: ductile replay <patch_id> [beta1] [beta2]");
        return Ok(1);
    };
    let id: i64 = id_str
        .parse()
        .map_err(|_| format!("patch id must be integer: {id_str}"))?;
    let beta1 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.02);
    let beta2 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0.01);

    let conn = db::open();
    let (value, status, field): (String, String, String) = match conn.query_row(
        "SELECT value, status, field FROM patches WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ) {
        Ok(v) => v,
        Err(_) => return Err(format!("patch #{id} not found")),
    };

    if status != "tentative" {
        eprintln!("patch #{id} status={status} — replay gate only applies to tentative patches");
        return Ok(1);
    }

    match replay_verdict(&value, beta1, beta2)? {
        ReplayVerdict::Confirm { v_pi0, v_new, n_sessions } => {
            println!(
                "REPLAY-CONFIRM patch#{id} [{field}]  V(pi0)={v_pi0:+.4} < V(new)={v_new:+.4}  over {n_sessions} sessions"
            );
            Ok(0)
        }
        ReplayVerdict::Reject { v_pi0, v_new, n_sessions } => {
            println!(
                "REPLAY-REJECT patch#{id} [{field}]  V(pi0)={v_pi0:+.4} >= V(new)={v_new:+.4}  over {n_sessions} sessions"
            );
            Ok(3)
        }
        ReplayVerdict::HumanReview { reason } => {
            println!("REPLAY-HUMAN patch#{id} [{field}]  {reason}");
            Ok(2)
        }
    }
}

#[cfg(test)]
mod replay_gate_tests {
    use super::*;

    #[test]
    fn parse_effect_variants() {
        assert_eq!(
            parse_effect("guide 文本\nreplay_effect: skip_procs=a, b,c\n"),
            PatchEffect::SkipProcs(vec!["a".into(), "b".into(), "c".into()])
        );
        assert_eq!(
            parse_effect("replay_effect: extra_work=3"),
            PatchEffect::ExtraWork(3)
        );
        assert_eq!(parse_effect("纯 guide 改动无声明"), PatchEffect::NoEffect);
        assert_eq!(parse_effect("replay_effect: extra_work=bad"), PatchEffect::NoEffect);
    }

    #[test]
    fn skip_direction_math() {
        // 跳过尾部 Fail（0 分）节点：省 N 罚、best 不变 → 必更优
        let full = ReplayTrace::serial(vec![1.0, 1.0, 0.0]);
        let skipped = ReplayTrace::serial(vec![1.0, 1.0]);
        assert!(replay_score(&skipped, 0.02, 0.01) > replay_score(&full, 0.02, 0.01));
        // 跳过唯一高分节点 → 必更差
        let hi = ReplayTrace::serial(vec![0.2, 0.9]);
        let hi_skipped = ReplayTrace::serial(vec![0.2]);
        assert!(replay_score(&hi_skipped, 0.02, 0.01) < replay_score(&hi, 0.02, 0.01));
    }
}
