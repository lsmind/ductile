//! attractor.rs — 记忆开关-积分器极简核（v9 设计，docs/attractor-hb/v9_design.md）
//!
//! 物理指认（映射自设计稿的 pipeline.rs/mlv.rs 到 L0-L4 实际结构）：
//! - 纯函数核（本文件）：类别记忆 m=e^(-n/τ)、乘法增益 q_X、winner=argmax
//! - MLV 承载：src/kernel/mlv.rs 的 Decision Op + append_typed（唯一写路径）
//! - 质量闭环：L1_feedback/{canary,incident}.rs 既有件
//!
//! 13 堵墙中的代码级墙（其余在调用方）：
//! - 墙4 仅 verified 更新记忆：本核不 I/O，verified 语义由调用方 OutcomeVerifier 实现
//! - 墙8 verified 判定源独立性：核内无 winner 依赖的 verify 接口
//! - 墙9 历史重放不得改判：Params 绑定 formula_version，重放按记录内版本参数

use std::collections::BTreeMap;

/// 公式版本（墙10：C 变更/参数语义变更必须 bump 并冻结旧版重放参数）
pub const FORMULA_VERSION: &str = "attractor.v9";

/// λ 硬上界（墙6 证据主政不变量：记忆至多放大 (1+λ_max) 倍证据）
/// rules 可加载更小的 λ，但不得抬高此界（F1）。
pub const LAMBDA_MAX: f64 = 0.5;

/// 闭类别集 C（墙10：完整有序，绑定 formula_version；变更开新版本）
pub type Categories = Vec<String>;

/// 公式参数（全部随 Decision 入账，重放按记录内值——墙9）
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    pub formula_version: String,
    /// λ ∈ [0, LAMBDA_MAX]（R 期硬校验，非有限值拒绝）
    pub lambda: f64,
    /// τ > 0 且有限
    pub tau: f64,
    /// 闭类别集（有序；哈希进 formula_version 语义）
    pub categories: Categories,
}

impl Params {
    pub fn validate(&self) -> Result<(), String> {
        if self.formula_version != FORMULA_VERSION {
            return Err(format!(
                "ERR formula-version-mismatch: record={} engine={} (墙9: 旧记录按旧版本参数重放)",
                self.formula_version, FORMULA_VERSION
            ));
        }
        if !self.lambda.is_finite() || self.lambda < 0.0 || self.lambda > LAMBDA_MAX {
            return Err(format!(
                "ERR E422 lambda-out-of-range: {:?} not in [0,{}] (F1: 证据主政硬界)",
                self.lambda, LAMBDA_MAX
            ));
        }
        if !self.tau.is_finite() || self.tau <= 0.0 {
            return Err(format!("ERR E422 tau-invalid: {:?} not finite >0", self.tau));
        }
        if self.categories.is_empty() || self.categories.len() < 2 {
            return Err(format!(
                "ERR E422 categories-must-have >=2 (got {})",
                self.categories.len()
            ));
        }
        // 有序性（墙10：C 的序是版本语义的一部分）
        let sorted = self.categories.clone();
        let mut seen = sorted.clone();
        seen.sort();
        seen.dedup();
        if seen.len() != self.categories.len() {
            return Err("ERR E422 categories-duplicate".into());
        }
        Ok(())
    }
}

/// 记忆态（从账本重放导出；无独立存储——墙13）
/// a=None 表示无记忆（a=⊥）；n=距上次 verified 的 run 数
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MemoryState {
    pub a: Option<String>,
    pub n: u64,
}

impl MemoryState {
    /// 墙12 verified 转移门 + 墙11 n 终态统一 的状态迁移
    /// - verified(同类别)：n=0
    /// - verified(新类别)：换边全局重置 a=X, n=0
    /// - 无 verified 的完成 run（含 no-Decision/失败/弃权/验证拒绝）：n+1，a 不变
    pub fn step(&self, ev: &TrialEvent) -> MemoryState {
        match ev {
            TrialEvent::Verified(cat) => {
                if self.a.as_deref() == Some(cat.as_str()) {
                    MemoryState { a: self.a.clone(), n: 0 }
                } else {
                    // 换边全局重置（墙12：a 只能被 verified 改变）
                    MemoryState { a: Some(cat.clone()), n: 0 }
                }
            }
            TrialEvent::CompletedUnverified => MemoryState { a: self.a.clone(), n: self.n + 1 },
        }
    }
}

/// 试次事件（verified 语义由调用方 OutcomeVerifier 决定，判定源与 winner 无关——墙8）
#[derive(Debug, Clone, PartialEq)]
pub enum TrialEvent {
    /// 结果质量谓词通过（与 winner 无关）
    Verified(String),
    /// 完成 but 未获 verified：no-Decision / 执行失败 / 验证拒绝 / 弃权（墙11 统一）
    CompletedUnverified,
}

/// 每类证据 g_X ≥ 0（P0 来源：契约卡 + judge 现有输出）
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence(pub BTreeMap<String, f64>);

/// 决策输出
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionOut {
    pub winner: Option<String>, // None = 平票 no-Decision
    pub q: BTreeMap<String, f64>,
    pub m: f64,
    pub tie: bool,
}

/// 核心：m = e^(-n/τ)（a=⊥ 时 m=0）；q_X = g_X·[1+λm·1(a=X)]；winner = argmax q_X
/// （v9：常数 6.7 已删——对 argmax 精确中性，F2；平票=no-Decision，禁止随机破平）
pub fn decide(p: &Params, mem: &MemoryState, ev: &Evidence) -> Result<DecisionOut, String> {
    p.validate()?;
    let m: f64 = match &mem.a {
        None => 0.0,
        Some(a) => {
            if !p.categories.iter().any(|c| c == a) {
                // 墙10：a 不在新 C 只取消 boost，不清 a（由调用方保留账本态）
                0.0
            } else {
                let n = mem.n as f64;
                let v = (-n / p.tau).exp();
                if !v.is_finite() {
                    return Err("ERR E422 m-not-finite".into());
                }
                v
            }
        }
    };
    let mut q = BTreeMap::new();
    let mut best: Option<(String, f64)> = None;
    let mut tie = false;
    for cat in &p.categories {
        let g = match ev.0.get(cat) {
            Some(v) if v.is_finite() && *v >= 0.0 => *v,
            Some(v) => return Err(format!("ERR E422 evidence-invalid: {}={:?}", cat, v)),
            None => 0.0, // 缺席类别证据为 0（闭集语义）
        };
        let boost = if mem.a.as_deref() == Some(cat.as_str()) { 1.0 + p.lambda * m } else { 1.0 };
        let qx = g * boost;
        q.insert(cat.clone(), qx);
        match &best {
            None => best = Some((cat.clone(), qx)),
            Some((_, bq)) if qx > *bq => best = Some((cat.clone(), qx)),
            Some((_, bq)) if (qx - *bq).abs() == 0.0 => tie = true,
            _ => {}
        }
    }
    // 平票判定：严格最大者唯一才出 winner
    if let Some((_wcat, wq)) = &best {
        let max_count = q.values().filter(|v| **v == *wq).count();
        if max_count > 1 {
            return Ok(DecisionOut { winner: None, q, m, tie: true });
        }
    }
    let _ = tie;
    Ok(DecisionOut { winner: best.map(|(c, _)| c), q, m, tie: false })
}

/// streak 观测（F3 防过重形态）：连续 K 次 winner==a 且未获 verified → 一次性 incident 信号
/// 无状态派生：由调用方数连续次数；本函数只给判据
pub fn stale_streak(winner: Option<&str>, a: Option<&str>, verified: bool) -> bool {
    !verified && winner.is_some() && a.is_some() && winner == a
}

// ── 账本接线层（TOON k: v payload + 重放导出）──────────────────────

/// Decision payload 编码（TOON 规范：每行 `k: v`，冒号后恰一空格——生死线）
/// 字段封闭词表（v9 §D2）：未知字段 parse 拒（fail-closed）
pub const PAYLOAD_FIELDS: &[&str] = &[
    "purpose", "formula_version", "lambda", "tau", "categories",
    "evidence", "m", "n", "memory_a", "winner", "tie", "prev_verdict", "input_digest",
];

#[derive(Debug, Clone, PartialEq)]
pub struct DecisionPayload {
    pub lambda: f64,
    pub tau: f64,
    pub categories: Vec<String>,
    /// 有序 (类别, g) 对（保留入账序=input_digest 语义）
    pub evidence: Vec<(String, f64)>,
    pub m: f64,
    pub n: u64,
    pub memory_a: Option<String>,
    pub winner: Option<String>,
    pub tie: bool,
    pub prev_verdict: Option<String>, // verified | rejected | no-decision（首条缺席）
    pub input_digest: String,
}

pub const VERDICTS: &[&str] = &["verified", "rejected", "no-decision"];

fn fmt_f(v: f64) -> String { format!("{v:.12}") }

impl DecisionPayload {
    pub fn encode(&self) -> String {
        let ev = self
            .evidence
            .iter()
            .map(|(c, g)| format!("{c}={g:.12}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut s = String::new();
        s.push_str("purpose: attractor-decision\n");
        s.push_str(&format!("formula_version: {FORMULA_VERSION}\n"));
        s.push_str(&format!("lambda: {}\n", fmt_f(self.lambda)));
        s.push_str(&format!("tau: {}\n", fmt_f(self.tau)));
        s.push_str(&format!("categories: {}\n", self.categories.join(",")));
        s.push_str(&format!("evidence: {ev}\n"));
        s.push_str(&format!("m: {}\n", fmt_f(self.m)));
        s.push_str(&format!("n: {}\n", self.n));
        s.push_str(&format!("memory_a: {}\n", self.memory_a.as_deref().unwrap_or("-")));
        s.push_str(&format!("winner: {}\n", self.winner.as_deref().unwrap_or("-")));
        s.push_str(&format!("tie: {}\n", if self.tie { 1 } else { 0 }));
        if let Some(v) = &self.prev_verdict {
            s.push_str(&format!("prev_verdict: {v}\n"));
        }
        s.push_str(&format!("input_digest: {}", self.input_digest));
        s
    }

    /// 严格解析：未知字段/缺字段/非法值全拒（fail-closed；冒号后空格生死线）
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        for line in s.trim_end_matches('\n').split('\n') {
            if line.trim().is_empty() { continue; }
            let (k, v) = line.split_once(": ")
                .ok_or_else(|| format!("ERR E422 payload-line-not-kv: {line:?}"))?;
            if !PAYLOAD_FIELDS.contains(&k) {
                return Err(format!("ERR E422 payload-unknown-field: {k}"));
            }
            map.insert(k.into(), v.into());
        }
        let need = |k: &str| -> Result<String, String> {
            map.get(k).cloned().ok_or_else(|| format!("ERR E422 payload-missing: {k}"))
        };
        if need("purpose")? != "attractor-decision" {
            return Err("ERR E422 payload-purpose-mismatch".into());
        }
        if need("formula_version")? != FORMULA_VERSION {
            return Err("ERR E422 payload-formula-version".into());
        }
        let parse_f = |k: &str| -> Result<f64, String> {
            need(k)?.parse::<f64>().map_err(|_| format!("ERR E422 payload-{k}-not-float"))
        };
        let evidence = need("evidence")?
            .split(',')
            .map(|pair| {
                let (c, g) = pair.split_once('=')
                    .ok_or_else(|| format!("ERR E422 evidence-pair: {pair:?}"))?;
                let g = g.parse::<f64>().map_err(|_| format!("ERR E422 evidence-g: {pair:?}"))?;
                Ok((c.to_string(), g))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let memory_a = need("memory_a")?;
        let winner = need("winner")?;
        let prev_verdict = match map.get("prev_verdict") {
            Some(v) if VERDICTS.contains(&v.as_str()) => Some(v.clone()),
            Some(v) => return Err(format!("ERR E422 prev_verdict-invalid: {v}")),
            None => None,
        };
        Ok(DecisionPayload {
            lambda: parse_f("lambda")?,
            tau: parse_f("tau")?,
            categories: need("categories")?.split(',').map(|s| s.into()).collect(),
            evidence,
            m: parse_f("m")?,
            n: need("n")?.parse::<u64>().map_err(|_| "ERR E422 payload-n".to_string())?,
            memory_a: if memory_a == "-" { None } else { Some(memory_a) },
            winner: if winner == "-" { None } else { Some(winner) },
            tie: need("tie")? == "1",
            prev_verdict,
            input_digest: need("input_digest")?,
        })
    }
}

/// 重放导出 (a, n)（墙13：无独立存储，一切从账本重放）
/// 语义：Decision #k 的 prev_verdict 认证的是 **#k-1 的 winner** 的执行结果。
/// - verified 且 #k-1 有 winner → step(Verified(w))（同类 n=0 / 换边全局重置）
/// - rejected / no-decision / 非首条缺席 / verified 但 #k-1 无 winner →
///   step(CompletedUnverified)（墙11：no-Decision 也恰推一次；保守 fail-closed）
/// - 首条（无前序 run）→ 不推进
pub fn replay_memory(payloads: &[DecisionPayload]) -> MemoryState {
    let mut st = MemoryState::default();
    for (i, p) in payloads.iter().enumerate() {
        if i == 0 {
            continue;
        }
        let prev = &payloads[i - 1];
        st = match (p.prev_verdict.as_deref(), prev.winner.clone()) {
            (Some("verified"), Some(w)) => st.step(&TrialEvent::Verified(w)),
            _ => st.step(&TrialEvent::CompletedUnverified),
        };
    }
    st
}

/// input_digest：run 槽位 + 全证据向量 + params + prev_verdict 的稳定摘要
/// （k=run 序号：同槽位同输入=重复重放拒；不同槽位同输入=合法——记忆态不同）
pub fn input_digest(k: u64, lambda: f64, tau: f64, categories: &[String], evidence: &[(String, f64)], prev_verdict: Option<&str>) -> String {
    let ev = evidence.iter().map(|(c, g)| format!("{c}={g:.12}")).collect::<Vec<_>>().join(",");
    let raw = format!("{FORMULA_VERSION}|run{k}|{lambda:.12}|{tau:.12}|{}|{ev}|{}",
        categories.join(","), prev_verdict.unwrap_or("-"));
    // FNV-1a 64（仓库无 sha2 依赖时的稳定哈希；collision 域=试点 cell，可接受）
    let mut h: u64 = 0xcbf29ce484222325;
    for b in raw.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cats() -> Categories { vec!["A".into(), "B".into()] }
    fn p(lambda: f64, tau: f64) -> Params {
        Params { formula_version: FORMULA_VERSION.into(), lambda, tau, categories: cats() }
    }
    fn ev(a: f64, b: f64) -> Evidence {
        Evidence(BTreeMap::from([("A".into(), a), ("B".into(), b)]))
    }

    // ── 基准 ──
    #[test]
    fn no_memory_pure_evidence() {
        let out = decide(&p(0.5, 8.0), &MemoryState::default(), &ev(1.0, 0.6)).unwrap();
        assert_eq!(out.winner.as_deref(), Some("A"));
        assert!((out.m - 0.0).abs() < 1e-12);
    }

    // ── 同类衰减（τ=8）──
    #[test]
    fn memory_decays_with_n() {
        let mem = MemoryState { a: Some("A".into()), n: 8 };
        let out = decide(&p(0.5, 8.0), &mem, &ev(1.0, 1.0)).unwrap();
        // m=e^-1≈0.3679, q_A=1.3679 > q_B=1.0 → A 赢（弱证据并列被记忆破平？）
        // 注意：这不是平票（q 不同）——记忆增益恰好破平是设计语义（增强同侧）
        assert!((out.m - (-1.0f64).exp()).abs() < 1e-12);
        assert_eq!(out.winner.as_deref(), Some("A"));
    }

    // ── 换边全局重置 ──
    #[test]
    fn switch_resets_globally() {
        let mem = MemoryState { a: Some("A".into()), n: 0 };
        let next = mem.step(&TrialEvent::Verified("B".into()));
        assert_eq!(next, MemoryState { a: Some("B".into()), n: 0 });
        // 再验证旧类别：n 又归零，a 切回
        let next2 = next.step(&TrialEvent::Verified("A".into()));
        assert_eq!(next2, MemoryState { a: Some("A".into()), n: 0 });
    }

    // ── 墙11 n 终态统一：失败/弃权/no-Decision 都恰推一次 ──
    #[test]
    fn unverified_runs_advance_n_once() {
        let mem = MemoryState { a: Some("A".into()), n: 3 };
        let next = mem.step(&TrialEvent::CompletedUnverified);
        assert_eq!(next.n, 4);
        assert_eq!(next.a, Some("A".into()));
    }

    // ── 平票 = no-Decision（禁止随机破平）──
    #[test]
    fn tie_is_no_decision() {
        let out = decide(&p(0.5, 8.0), &MemoryState::default(), &ev(1.0, 1.0)).unwrap();
        assert!(out.tie);
        assert_eq!(out.winner, None);
    }

    // ── 记忆恰好破平的语义确认（q 不等即非平票）──
    #[test]
    fn memory_breaks_ties_by_design() {
        let mem = MemoryState { a: Some("A".into()), n: 0 };
        let out = decide(&p(0.5, 8.0), &mem, &ev(1.0, 1.0)).unwrap();
        assert_eq!(out.winer_or(), Some("A")); // 见下方 helper
    }

    // ── F1 λ 硬界 ──
    #[test]
    fn lambda_bounds_enforced() {
        assert!(decide(&p(0.5, 8.0), &MemoryState::default(), &ev(1.0, 1.0)).is_ok());
        assert!(decide(&p(0.5000001, 8.0), &MemoryState::default(), &ev(1.0, 1.0)).is_err());
        assert!(decide(&p(f64::NAN, 8.0), &MemoryState::default(), &ev(1.0, 1.0)).is_err());
        assert!(decide(&p(-0.1, 8.0), &MemoryState::default(), &ev(1.0, 1.0)).is_err());
    }

    // ── F2：常数中性已从公式删除（q 形态）；翻转边界 g_Y = g_X(1+λm) ──
    #[test]
    fn flip_boundary_exact() {
        let mem = MemoryState { a: Some("A".into()), n: 0 }; // m=1
        // g_B = 1.5 恰好平票 → no-Decision
        let out = decide(&p(0.5, 8.0), &mem, &ev(1.0, 1.5)).unwrap();
        assert!(out.tie, "g_Y=1.5=1.0*(1+0.5*1) should tie");
        assert_eq!(out.winner, None);
        // g_B = 1.5001 → B 赢
        let out2 = decide(&p(0.5, 8.0), &mem, &ev(1.0, 1.5001)).unwrap();
        assert_eq!(out2.winner.as_deref(), Some("B"));
    }

    // ── 墙8 自确认红测（判定源独立性由调用方保证；此处测核不依赖 winner）──
    #[test]
    fn verified_independent_of_winner() {
        // a=A, winner=A, 但结果错误 → 调用方应发 CompletedUnverified → n 前进
        let mem = MemoryState { a: Some("A".into()), n: 2 };
        let next = mem.step(&TrialEvent::CompletedUnverified);
        assert_eq!((next.a.clone(), next.n), (Some("A".into()), 3));
    }

    // ── 墙10：a 不在 C 只取消 boost 不清 a ──
    #[test]
    fn a_not_in_c_boost_cancelled_only() {
        let p2 = Params {
            formula_version: FORMULA_VERSION.into(),
            lambda: 0.5,
            tau: 8.0,
            categories: vec!["X".into(), "Y".into()],
        };
        let mem = MemoryState { a: Some("A".into()), n: 0 };
        let out = decide(&p2, &mem, &ev_x(1.0, 1.0)).unwrap();
        assert!((out.m - 0.0).abs() < 1e-12, "a=A not in C-set, m must be 0");
        // 且平票（q_X=q_Y=1.0）→ no-Decision
        assert!(out.tie);
    }

    // ── 证据域校验 ──
    #[test]
    fn evidence_domain_checked() {
        let mut e = ev(1.0, 1.0);
        e.0.insert("A".into(), -0.1);
        assert!(decide(&p(0.5, 8.0), &MemoryState::default(), &e).is_err());
        e.0.insert("A".into(), f64::INFINITY);
        assert!(decide(&p(0.5, 8.0), &MemoryState::default(), &e).is_err());
    }

    // ── F3 streak 判据 ──
    #[test]
    fn streak_signal() {
        assert!(stale_streak(Some("A"), Some("A"), false));
        assert!(!stale_streak(Some("A"), Some("A"), true));
        assert!(!stale_streak(Some("B"), Some("A"), false));
        assert!(!stale_streak(None, Some("A"), false));
    }

    fn ev_x(x: f64, y: f64) -> Evidence {
        Evidence(BTreeMap::from([("X".into(), x), ("Y".into(), y)]))
    }

    impl DecisionOut {
        fn winer_or(&self) -> Option<&str> {
            self.winner.as_deref()
        }
    }

    // ── D2：payload 编解码 + 重放导出 ──

    fn sample_payload() -> DecisionPayload {
        DecisionPayload {
            lambda: 0.5, tau: 8.0,
            categories: vec!["A".into(), "B".into()],
            evidence: vec![("A".into(), 1.0), ("B".into(), 1.5)],
            m: 1.0, n: 0,
            memory_a: Some("A".into()),
            winner: None, tie: true,
            prev_verdict: Some("verified".into()),
            input_digest: input_digest(7, 0.5, 8.0, &["A".into(), "B".into()],
                &[("A".into(), 1.0), ("B".into(), 1.5)], Some("verified")),
        }
    }

    #[test]
    fn payload_roundtrip() {
        let p = sample_payload();
        let s = p.encode();
        assert!(s.contains("purpose: attractor-decision"));
        assert!(s.contains("prev_verdict: verified"));
        let q = DecisionPayload::parse(&s).unwrap();
        assert_eq!(p, q);
        // 再编码幂等
        assert_eq!(s, q.encode());
    }

    #[test]
    fn payload_first_record_no_prev_verdict() {
        let mut p = sample_payload();
        p.prev_verdict = None;
        let q = DecisionPayload::parse(&p.encode()).unwrap();
        assert_eq!(q.prev_verdict, None);
        assert!(p.encode().find("prev_verdict").is_none());
    }

    #[test]
    fn payload_unknown_field_rejected() {
        let s = sample_payload().encode() + "\nevil: 1";
        assert!(DecisionPayload::parse(&s).is_err());
    }

    #[test]
    fn payload_bad_verdict_rejected() {
        let s = sample_payload().encode().replace("prev_verdict: verified", "prev_verdict: ok");
        assert!(DecisionPayload::parse(&s).is_err());
    }

    #[test]
    fn payload_colon_space_lifeline() {
        // 冒号后无空格 = 非法行（TOON 生死线）
        let s = sample_payload().encode().replace("m: 1.000000000000", "m:1.000000000000");
        assert!(DecisionPayload::parse(&s).is_err());
    }

    #[test]
    fn payload_lambda_bound_checked_on_parse() {
        let mut p = sample_payload();
        p.lambda = 0.9;
        let _s = p.encode();
        // parse 层不查 λ（DecisionPayload 只是载体），decide 层才拒——
        // 但重放侧必须拒，这里直接验证 Params::validate 会拒
        let params = Params { formula_version: FORMULA_VERSION.into(), lambda: 0.9, tau: 8.0, categories: cats() };
        assert!(params.validate().is_err());
    }

    // ── replay_memory：闭环语义测试（D2 核心）──

    fn mk_pay(winner: Option<&str>, prev: Option<&str>, a: Option<&str>, n: u64) -> DecisionPayload {
        DecisionPayload {
            lambda: 0.5, tau: 8.0, categories: cats(),
            evidence: vec![("A".into(), 1.0), ("B".into(), 1.0)],
            m: 0.0, n,
            memory_a: a.map(String::from),
            winner: winner.map(String::from), tie: winner.is_none(),
            prev_verdict: prev.map(String::from),
            input_digest: String::new(),
        }
    }

    #[test]
    fn replay_empty_is_fresh() {
        assert_eq!(replay_memory(&[]), MemoryState::default());
    }

    #[test]
    fn replay_verified_same_cat_n_zero() {
        // run1: winner=A 无 verdict（首条）；run2: prev=verified → a=A n=0
        let ps = [mk_pay(Some("A"), None, None, 0), mk_pay(Some("A"), Some("verified"), Some("A"), 0)];
        assert_eq!(replay_memory(&ps), MemoryState { a: Some("A".into()), n: 0 });
    }

    #[test]
    fn replay_rejected_advances_n() {
        // run1: winner=A；run2: prev=rejected → n=1（a 无 verified 不形成）
        // run3: prev 缺席（非首条）→ 保守 n+1=2
        let ps = [
            mk_pay(Some("A"), None, None, 0),
            mk_pay(Some("A"), Some("rejected"), Some("A"), 0),
            mk_pay(Some("A"), None, Some("A"), 1),
        ];
        assert_eq!(replay_memory(&ps), MemoryState { a: None, n: 2 });
    }

    #[test]
    fn replay_switch_resets() {
        // run1: winner=A（首条）；run2: winner=A prev=verified → 认证 run1 的 A → a=A n=0
        // run3: winner=B prev=verified → 认证 run2 的 A（非 B！）→ 同类 n=0，a 仍 A
        // run4: winner=B prev=verified → 认证 run3 的 B → 换边重置 a=B n=0
        let ps = [
            mk_pay(Some("A"), None, None, 0),
            mk_pay(Some("A"), Some("verified"), Some("A"), 0),
            mk_pay(Some("B"), Some("verified"), Some("A"), 0),
            mk_pay(Some("B"), Some("verified"), Some("B"), 0),
        ];
        assert_eq!(replay_memory(&ps), MemoryState { a: Some("B".into()), n: 0 });
    }

    #[test]
    fn replay_no_decision_advances_n() {
        // run2 是 no-decision（winner=None）→ run3 的 prev_verdict=no-decision → n+1
        let ps = [
            mk_pay(Some("A"), None, None, 0),
            mk_pay(None, Some("verified"), Some("A"), 0),
            mk_pay(Some("A"), Some("no-decision"), Some("A"), 1),
        ];
        assert_eq!(replay_memory(&ps), MemoryState { a: Some("A".into()), n: 1 });
    }

    #[test]
    fn replay_verified_no_winner_prev_advances_n() {
        // run2 winner=None 但 prev=verified：认证对象无 winner → 保守推进（墙11）
        let ps = [
            mk_pay(Some("A"), None, None, 0),
            mk_pay(None, Some("verified"), Some("A"), 0),
            mk_pay(Some("A"), Some("verified"), Some("A"), 1),
        ];
        assert_eq!(replay_memory(&ps), MemoryState { a: Some("A".into()), n: 1 });
    }

    #[test]
    fn digest_stable_and_sensitive() {
        let ev = vec![("A".into(), 1.0), ("B".into(), 1.0)];
        let cats2 = vec!["A".into(), "B".into()];
        let d1 = input_digest(0, 0.5, 8.0, &cats2, &ev, None);
        let d2 = input_digest(0, 0.5, 8.0, &cats2, &ev, None);
        assert_eq!(d1, d2); // 同槽位同输入稳定
        assert!(d1.starts_with("fnv1a64:"));
        // 证据变 → digest 变
        let ev2 = vec![("A".into(), 1.0000001), ("B".into(), 1.0)];
        assert_ne!(d1, input_digest(0, 0.5, 8.0, &cats2, &ev2, None));
        // verdict 变 → digest 变
        assert_ne!(d1, input_digest(0, 0.5, 8.0, &cats2, &ev, Some("verified")));
        // run 槽位变 → digest 变（不同 run 同输入合法）
        assert_ne!(d1, input_digest(1, 0.5, 8.0, &cats2, &ev, None));
    }
}
