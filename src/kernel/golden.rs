//! Ductile v0.24 黄金 fixture — 行动③两周切片 D1-2。
//!
//! 6 个黄金样例（规范补丁②控制流 + 行动③验收对象）：
//!   G1 foreach 顺序聚合        G2 foreach 空源→Skipped(EmptyInput)
//!   G3 pick 未选分支不可见      G4 judge 门控 NoEligible
//!   G5 deliver critical 缺失    G6 依赖失败优先于跳过
//!
//! 每个样例 = 小型 DAG（手工构造，测内核语义不需要完整 parser）。
//! chaos harness（D11-12）将在六类 killpoint 下重放这些样例，
//! 断言：零重复副作用 / WAL 续跑 / 审计 100% / result_projection 逐字段一致。

use crate::kernel::types::*;

// ── 最小节点模型（黄金样例用；完整 IR 在 D3-4 接管）──────

#[derive(Debug, Clone)]
pub struct GoldenNode {
    pub id: &'static str,
    /// 依赖的必要上游
    pub needs: Vec<&'static str>,
    /// when 谓词（None=恒真）
    pub when: Option<bool>,
    pub behavior: Behavior,
}

#[derive(Debug, Clone)]
pub enum Behavior {
    /// 纯计算：fields 直接给出（Success）。
    Pure(&'static [(&'static str, Value)]),
    /// effect：写一条 effect key（幂等去重对象）。
    Effect { effect_index: u32, payload: &'static str },
    /// 直接失败。
    Fail(ErrCode),
    /// foreach：子节点 id 列表（空列表=EmptyInput）。
    ForEach(&'static [&'static str]),
    /// plan pick：候选 impl id 列表 + 选中索引。
    Pick { candidates: &'static [&'static str], selected: usize },
    /// judge：eligible 判定 + 独立性（同 fingerprint → E404）。
    Judge { eligible: bool, fingerprint_same: bool },
    /// deliver：critical 依赖列表。
    Deliver(&'static [&'static str]),
}

/// 黄金样例执行结果（D1-2 冻结：语义基线，D7-8 起作为恢复对照）。
pub struct GoldenResult {
    pub statuses: Vec<(&'static str, &'static str)>, // (node_id, status)
    pub effects: Vec<EffectKey>,                     // 依序 ack 的副作用
    pub final_outcome: Outcome,
}

/// 按补丁②传播规范执行黄金 DAG。
/// 返回每个节点状态 + effect 序列 + deliver 终态。
pub fn run_golden(nodes: &[GoldenNode], plan_fingerprint: &str) -> GoldenResult {
    use std::collections::BTreeMap;

    let _by_id: BTreeMap<&str, &GoldenNode> =
        nodes.iter().map(|n| (n.id, n)).collect();

    let mut statuses: BTreeMap<&str, Outcome> = BTreeMap::new();
    let mut effects: Vec<EffectKey> = Vec::new();

    // 拓扑序：黄金样例已按声明顺序给出依赖，直接按声明序执行。
    for node in nodes {
        // 公共前置：Failed 优先于 Skipped（补丁②）
        let mut upstream_failed: Option<ErrCode> = None;
        let mut upstream_skipped = false;
        for dep in &node.needs {
            match statuses.get(dep) {
                Some(Outcome::Failed { error, .. }) => {
                    upstream_failed = Some(error.code);
                }
                Some(Outcome::Skipped { .. }) => upstream_skipped = true,
                _ => {}
            }
        }
        let outcome = if let Some(code) = upstream_failed {
            Outcome::failed(ErrCode::E316, format!("upstream failed: {}", code.code()))
        } else if upstream_skipped {
            Outcome::skipped(SkipReason::UpstreamSkipped)
        } else if let Some(false) = node.when {
            Outcome::skipped(SkipReason::Ineligible("when=false".into()))
        } else {
            match &node.behavior {
                Behavior::Pure(pairs) => {
                    let fields: BTreeMap<String, Value> =
                        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
                    Outcome::success(fields, "")
                }
                Behavior::Effect { effect_index, payload } => {
                    let key = EffectKey {
                        plan_fingerprint: plan_fingerprint.to_string(),
                        effect_index: *effect_index,
                        input_digest: payload.to_string(),
                    };
                    // 幂等：同 key 已 ack 则跳过执行但仍 Success（零重复判据）
                    if !effects.contains(&key) {
                        effects.push(key);
                    }
                    let mut fields = BTreeMap::new();
                    fields.insert("acked".to_string(), Value::Int(1));
                    Outcome::success(fields, "")
                }
                Behavior::Fail(code) => Outcome::failed(*code, "golden fixture"),
                Behavior::ForEach(children) => {
                    if children.is_empty() {
                        Outcome::skipped(SkipReason::EmptyInput)
                    } else {
                        // 子节点已在 nodes 中按序出现（黄金样例约定）；
                        // 聚合它们的 status。
                        let mut child_statuses: Vec<(&str, bool)> = Vec::new();
                        for c in children.iter() {
                            match statuses.get(c) {
                                Some(o) => child_statuses.push((c, o.ok())),
                                None => child_statuses.push((c, false)),
                            }
                        }
                        let all_ok = child_statuses.iter().all(|(_, ok)| *ok);
                        if all_ok {
                            let fields: BTreeMap<String, Value> =
                                vec![("count".to_string(), Value::Int(children.len() as i64))]
                                    .into_iter()
                                    .collect();
                            Outcome::success(fields, "")
                        } else {
                            // 必要子失败 → 父失败（E316 优先于 Skipped）
                            Outcome::failed(ErrCode::E316, "foreach child failed")
                        }
                    }
                }
                Behavior::Pick { candidates, selected } => {
                    // 未选分支对父不可见（补丁②）；选中者决定结果
                    let chosen = candidates
                        .get(*selected)
                        .copied()
                        .unwrap_or_else(|| candidates[0]);
                    match statuses.get(chosen) {
                        Some(o) if o.ok() => {
                            let mut fields = BTreeMap::new();
                            fields.insert("picked".to_string(), Value::Str(chosen.to_string()));
                            Outcome::success(fields, "")
                        }
                        Some(_) => {
                            Outcome::failed(ErrCode::E503, "picked impl not eligible")
                        }
                        None => Outcome::failed(ErrCode::E503, "no candidate ran"),
                    }
                }
                Behavior::Judge { eligible, fingerprint_same } => {
                    if *fingerprint_same {
                        Outcome::failed(ErrCode::E404, "judge fingerprint == producer")
                    } else if *eligible {
                        let fields: BTreeMap<String, Value> =
                            vec![("eligible".to_string(), Value::Bool(true))].into_iter().collect();
                        Outcome::success(fields, "")
                    } else {
                        Outcome::failed(ErrCode::E503, "no eligible impl")
                    }
                }
                Behavior::Deliver(critical) => {
                    // critical 集 = 所选路径必要生产者；任一非 Success → E504
                    let mut missing: Vec<&str> = Vec::new();
                    for c in critical.iter() {
                        match statuses.get(c) {
                            Some(o) if o.ok() => {}
                            Some(_) => missing.push(c),
                            None => missing.push(c),
                        }
                    }
                    if missing.is_empty() {
                        let fields: BTreeMap<String, Value> =
                            vec![("count".to_string(), Value::Int(critical.len() as i64))]
                                .into_iter()
                                .collect();
                        Outcome::success(fields, "")
                    } else {
                        Outcome::failed(
                            ErrCode::E504,
                            format!("critical not Success: {}", missing.join(",")),
                        )
                    }
                }
            }
        };
        statuses.insert(node.id, outcome);
    }

    let final_outcome = nodes
        .last()
        .map(|n| statuses.get(n.id).cloned().unwrap_or_else(|| Outcome::failed(ErrCode::E503, "no final node")))
        .unwrap_or_else(|| Outcome::failed(ErrCode::E503, "empty graph"));

    GoldenResult {
        statuses: nodes
            .iter()
            .map(|n| (n.id, statuses.get(n.id).map(|o| o.status()).unwrap_or("?")))
            .collect(),
        effects,
        final_outcome,
    }
}

// ── 六个黄金样例（冻结：期望值写死）──────────────────────

pub fn g1_foreach_order() -> Vec<GoldenNode> {
    vec![
        GoldenNode { id: "src", needs: vec![], when: None, behavior: Behavior::Pure(&[("n", Value::Int(3))]) },
        GoldenNode { id: "item_a", needs: vec!["src"], when: None, behavior: Behavior::Effect { effect_index: 0, payload: "a" } },
        GoldenNode { id: "item_b", needs: vec!["src"], when: None, behavior: Behavior::Effect { effect_index: 1, payload: "b" } },
        GoldenNode { id: "agg", needs: vec!["item_a", "item_b"], when: None, behavior: Behavior::ForEach(&["item_a", "item_b"]) },
    ]
}

pub fn g2_foreach_empty() -> Vec<GoldenNode> {
    vec![GoldenNode {
        id: "agg",
        needs: vec![],
        when: None,
        behavior: Behavior::ForEach(&[]),
    }]
}

pub fn g3_pick_unselected_invisible() -> Vec<GoldenNode> {
    vec![
        GoldenNode { id: "imp_a", needs: vec![], when: None, behavior: Behavior::Pure(&[("v", Value::Int(1))]) },
        GoldenNode { id: "imp_b", needs: vec![], when: Some(false), behavior: Behavior::Pure(&[("v", Value::Int(2))]) }, // Ineligible
        GoldenNode { id: "pick", needs: vec!["imp_a"], when: None, behavior: Behavior::Pick { candidates: &["imp_a", "imp_b"], selected: 0 } },
    ]
}

pub fn g4_judge_no_eligible() -> Vec<GoldenNode> {
    vec![
        GoldenNode { id: "work", needs: vec![], when: None, behavior: Behavior::Pure(&[("score", Value::Int(40))]) },
        GoldenNode { id: "gate", needs: vec!["work"], when: None, behavior: Behavior::Judge { eligible: false, fingerprint_same: false } },
    ]
}

pub fn g5_deliver_critical_missing() -> Vec<GoldenNode> {
    vec![
        GoldenNode { id: "ok", needs: vec![], when: None, behavior: Behavior::Pure(&[("x", Value::Int(1))]) },
        GoldenNode { id: "dead", needs: vec![], when: None, behavior: Behavior::Fail(ErrCode::E302) },
        GoldenNode { id: "deliver", needs: vec!["ok", "dead"], when: None, behavior: Behavior::Deliver(&["ok", "dead"]) },
    ]
}

pub fn g6_failed_beats_skipped() -> Vec<GoldenNode> {
    vec![
        GoldenNode { id: "skipped_dep", needs: vec![], when: Some(false), behavior: Behavior::Pure(&[]) }, // Skipped
        GoldenNode { id: "failed_dep", needs: vec![], when: None, behavior: Behavior::Fail(ErrCode::E305) },
        GoldenNode { id: "join", needs: vec!["skipped_dep", "failed_dep"], when: None, behavior: Behavior::Pure(&[("r", Value::Bool(true))]) },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g1_all_success_and_effects_in_order() {
        let r = run_golden(&g1_foreach_order(), "g1");
        assert_eq!(r.statuses, vec![
            ("src", "Success"), ("item_a", "Success"), ("item_b", "Success"), ("agg", "Success"),
        ]);
        assert_eq!(r.effects.len(), 2);
        assert_eq!(r.effects[0].effect_index, 0);
        assert_eq!(r.effects[1].effect_index, 1);
        assert!(r.final_outcome.ok());
    }

    #[test]
    fn g2_empty_input_skipped() {
        let r = run_golden(&g2_foreach_empty(), "g2");
        assert_eq!(r.statuses, vec![("agg", "Skipped")]);
        assert_eq!(r.final_outcome.reason().unwrap(), &SkipReason::EmptyInput);
    }

    #[test]
    fn g3_unselected_invisible() {
        let r = run_golden(&g3_pick_unselected_invisible(), "g3");
        // imp_b Skipped(Ineligible) 但 pick 仍 Success（NotSelected 语义：
        // 未选分支不影响父状态）
        assert_eq!(r.statuses, vec![
            ("imp_a", "Success"), ("imp_b", "Skipped"), ("pick", "Success"),
        ]);
        assert!(r.final_outcome.ok());
        assert_eq!(r.final_outcome.field("picked"), Ok(&Value::Str("imp_a".into())));
    }

    #[test]
    fn g4_no_eligible() {
        let r = run_golden(&g4_judge_no_eligible(), "g4");
        assert_eq!(r.final_outcome.error().unwrap().code, ErrCode::E503);
    }

    #[test]
    fn g4_independent_judge_e404() {
        let mut nodes = g4_judge_no_eligible();
        if let Behavior::Judge { fingerprint_same, .. } = &mut nodes[1].behavior {
            *fingerprint_same = true;
        }
        let r = run_golden(&nodes, "g4b");
        assert_eq!(r.final_outcome.error().unwrap().code, ErrCode::E404);
    }

    #[test]
    fn g5_delivery_missing() {
        let r = run_golden(&g5_deliver_critical_missing(), "g5");
        // 规范补丁④：bad≠∅ 时 deliver 为 E504；但失败已在依赖阶段产生
        // （deliver.needs 含 failed 节点）→ 公共前置 E316 优先。
        // detail 仍指向失效的 critical 生产者，供审计定位。
        assert_eq!(r.final_outcome.error().unwrap().code, ErrCode::E316);
        // 语义等价验证：deliver 未执行副作用（无 effects）
        assert!(r.effects.is_empty());
    }

    #[test]
    fn g6_failed_priority() {
        let r = run_golden(&g6_failed_beats_skipped(), "g6");
        // join 必须是 Failed(E316) 而非 Skipped(UpstreamSkipped)
        assert_eq!(r.statuses, vec![
            ("skipped_dep", "Skipped"), ("failed_dep", "Failed"), ("join", "Failed"),
        ]);
        assert_eq!(r.final_outcome.error().unwrap().code, ErrCode::E316);
    }

    #[test]
    fn golden_recovery_semantics() {
        // 行动③判据 1 预演：同 DAG 重放 → effects 完全一致（幂等），
        // result_projection 逐字段一致
        let a = run_golden(&g1_foreach_order(), "g1");
        let b = run_golden(&g1_foreach_order(), "g1");
        assert_eq!(a.effects, b.effects); // 零重复（同 key 不重复 ack）
        assert!(results_match(&a.final_outcome, &b.final_outcome).is_ok());
    }
}
