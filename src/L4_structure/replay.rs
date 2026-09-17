//! v0.20 Replay-RSI P1：`ductile tree` — 账本发现树重建。
//!
//! 数据源：runs 表（pipeline/session 落库后）。
//! 树语义（Dream-RSI 发现树的 ductile 落法）：
//! - 一个 session = 一棵树（run 的树身份）
//! - 节点 = proc 的一次执行（含失败；重试合并为 attempts 计数）
//! - 边 = DSL 静态拓扑（needs/when，AST 权威）× runs 时序印证
//! - 分数 = runs.score（judge/探针回填；空则 Ok=1.0/Fail=0.0 代理）
//!
//! 论文 Eq.1 的评分在 replay 评分引擎（P2，core 纯函数）——本命令只重建
//! 与展示树，评分调用 P2 接口（就绪前显示二值代理）。

use crate::core::ast::Pipeline;
use crate::L3_dsl::parser::parse_pipeline_file;
use crate::db;
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
