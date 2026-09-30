//! Ductile v0.24 judge 桩 + deliver outbox — D9-10。
//!
//! judge（规范 §4）：mode=rule（确定性 evaluator，本切片）；
//!   fingerprint 相同（judge 模型==producer 模型）→ E404 JudgeIndependent。
//! deliver（规范 §4+补丁④）：critical 集=所选路径必要生产者闭包；
//!   幂等 outbox=副作用按 EffectKey 走 WAL ack——重复 deliver 零重复副作用。

use crate::kernel::types::{ErrCode, Outcome, SkipReason, Value};
use crate::kernel::wal::{EffectExecutor, IdempotentRunner, Wal};
use std::collections::{BTreeMap, BTreeSet};

// ── judge：确定性规则桩 ──────────────────────────────────

/// 裁判策略：确定性谓词（分数阈值等），由 policy 文件编译而来。
#[derive(Debug, Clone)]
pub enum Rule {
    FieldGe { field: String, threshold: i64 },
    FieldLe { field: String, threshold: i64 },
    StatusIs { status: String },
    All(Vec<Rule>),
    Any(Vec<Rule>),
}

/// judge 输入：被评节点的 Outcome。
pub struct JudgeInput<'a> {
    pub outcome: &'a Outcome,
}

/// 独立性检查：producer fingerprint == judge fingerprint → E404（禁自判）。
pub fn check_independence(producer_fp: &str, judge_fp: &str) -> Result<(), ErrCode> {
    if producer_fp == judge_fp {
        Err(ErrCode::E404)
    } else {
        Ok(())
    }
}

/// 确定性规则求值（无 LLM、无随机；同输入同结论）。
pub fn eval_rule(rule: &Rule, input: &JudgeInput) -> bool {
    match rule {
        Rule::FieldGe { field, threshold } => match input.outcome.field(field) {
            Ok(Value::Int(v)) => *v >= *threshold,
            Ok(Value::Float(v)) => *v >= *threshold as f64,
            _ => false,
        },
        Rule::FieldLe { field, threshold } => match input.outcome.field(field) {
            Ok(Value::Int(v)) => *v <= *threshold,
            Ok(Value::Float(v)) => *v <= *threshold as f64,
            _ => false,
        },
        Rule::StatusIs { status } => input.outcome.status() == status,
        Rule::All(rules) => rules.iter().all(|r| eval_rule(r, input)),
        Rule::Any(rules) => rules.iter().any(|r| eval_rule(r, input)),
    }
}

/// judge 桩：规则 + 独立性 → Outcome{eligible,score,reason}（规范 §2 judge 签名）。
pub fn judge_stub(
    rule: &Rule,
    input: &JudgeInput,
    producer_fp: &str,
    judge_fp: &str,
) -> Outcome {
    if let Err(e) = check_independence(producer_fp, judge_fp) {
        return Outcome::failed(e, "judge fingerprint == producer（禁自判）");
    }
    let eligible = eval_rule(rule, input);
    let mut fields = BTreeMap::new();
    fields.insert("eligible".to_string(), Value::Bool(eligible));
    fields.insert(
        "score".to_string(),
        Value::Float(if eligible { 1.0 } else { 0.0 }),
    );
    fields.insert(
        "reason".to_string(),
        Value::Str(if eligible { "rule satisfied".into() } else { "rule not satisfied".into() }),
    );
    Outcome::success(fields, "")
}

// ── deliver：critical 闭包 + 幂等 outbox ─────────────────

/// 交付物：目标通道 + 内容摘要。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Deliverable {
    pub channel: String,
    pub manifest: String,
}

/// deliver 的 effect key 派生（幂等锚点）。
pub fn deliver_effect_key(d: &Deliverable, plan_fp: &str) -> crate::kernel::types::EffectKey {
    crate::kernel::types::EffectKey {
        plan_fingerprint: plan_fp.to_string(),
        effect_index: 0,
        input_digest: format!("{}|{}", d.channel, d.manifest),
    }
}

/// 真实交付通道（测试用计数实现；生产=feishu/file/registry）。
pub trait DeliverChannel {
    fn send(&mut self, d: &Deliverable) -> Result<(), ErrCode>;
}

/// 幂等 deliver：WAL ack 的 EffectKey + critical 前置。
/// E: EffectExecutor（如 ChannelAsExecutor<C>）。
pub struct Deliverer<'a, E: crate::kernel::wal::EffectExecutor> {
    pub runner: IdempotentRunner<'a, E>,
}

impl<'a, E: crate::kernel::wal::EffectExecutor> Deliverer<'a, E> {
    /// critical 集（补丁④：所选路径必要生产者）全部 Success 才发；
    /// 否则 E504 DeliveryMissing。
    pub fn deliver(
        &mut self,
        items: &[Deliverable],
        critical: &BTreeMap<String, &Outcome>,
        plan_fp: &str,
    ) -> Outcome {
        // critical 前置：任一非 Success → E504
        for (id, out) in critical {
            if !out.ok() {
                return Outcome::failed(
                    ErrCode::E504,
                    format!("critical '{}' is {}", id, out.status()),
                );
            }
        }
        // 幂等发送
        let mut count = 0;
        for d in items {
            let ek = deliver_effect_key(d, plan_fp);
            match self.runner.run_effect(&ek) {
                Ok(true) => count += 1, // 真发
                Ok(false) => {}         // 复用（零重复）
                Err(e) => {
                    return Outcome::failed(e, format!("deliver to {} failed", d.channel))
                }
            }
        }
        // count=本次新发送数 → meta（执行轨迹，恢复对比不参与）；
        // fields 只留 manifest_digest（result 语义：交付内容指纹）
        let mut fields = BTreeMap::new();
        fields.insert(
            "manifest_digest".to_string(),
            Value::Str(crate::kernel::hash::sha256_hex(
                items.iter().map(|d| format!("{}|{}", d.channel, d.manifest)).collect::<String>().as_bytes(),
            )),
        );
        let mut meta = BTreeMap::new();
        meta.insert("newly_sent".to_string(), Value::Int(count as i64));
        Outcome::Success { fields, stdout: String::new(), meta }
    }
}

// 交付通道的 effect 适配器（显式包装，避免 blanket impl 与其他
// EffectExecutor 冲突——chaos RecordingChannel 双 trait 实证）。
pub struct ChannelAsExecutor<C: DeliverChannel>(pub C);

impl<C: DeliverChannel> EffectExecutor for ChannelAsExecutor<C> {
    fn execute(&mut self, effect: &crate::kernel::types::EffectKey) -> Result<(), ErrCode> {
        // deliver effect key 的 input_digest 编码 channel|manifest
        let parts: Vec<&str> = effect.input_digest.splitn(2, '|').collect();
        if parts.len() == 2 {
            self.0.send(&Deliverable {
                channel: parts[0].to_string(),
                manifest: parts[1].to_string(),
            })
        } else {
            Err(ErrCode::E312)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::wal::Wal;
    use std::cell::Cell;

    // —— judge ——

    fn scored(n: i64) -> Outcome {
        let mut f = BTreeMap::new();
        f.insert("score".to_string(), Value::Int(n));
        Outcome::success(f, "")
    }

    #[test]
    fn judge_rule_deterministic() {
        let rule = Rule::FieldGe { field: "score".into(), threshold: 80 };
        let hi = scored(90);
        let lo = scored(40);
        let j1 = judge_stub(&rule, &JudgeInput { outcome: &hi }, "producer-a", "judge-x");
        let j2 = judge_stub(&rule, &JudgeInput { outcome: &hi }, "producer-a", "judge-x");
        assert!(j1.ok() && j2.ok());
        assert_eq!(j1.field("eligible"), Ok(&Value::Bool(true)));
        // 同输入同结论（确定性桩）
        assert_eq!(j1.fields(), j2.fields());
        let j3 = judge_stub(&rule, &JudgeInput { outcome: &lo }, "producer-a", "judge-x");
        assert_eq!(j3.field("eligible"), Ok(&Value::Bool(false)));
    }

    #[test]
    fn judge_self_judgment_banned() {
        let rule = Rule::StatusIs { status: "Success".into() };
        let o = Outcome::success(BTreeMap::new(), "");
        let j = judge_stub(&rule, &JudgeInput { outcome: &o }, "fp-1", "fp-1");
        assert_eq!(j.error().unwrap().code, ErrCode::E404);
        assert!(j.error().unwrap().detail.contains("自判"));
    }

    #[test]
    fn judge_composite_rules() {
        let rule = Rule::All(vec![
            Rule::FieldGe { field: "score".into(), threshold: 80 },
            Rule::StatusIs { status: "Success".into() },
        ]);
        let good = scored(85);
        let j = judge_stub(&rule, &JudgeInput { outcome: &good }, "p", "j");
        assert_eq!(j.field("eligible"), Ok(&Value::Bool(true)));
        // Skipped 的 outcome 不 eligible
        let skipped = Outcome::skipped(SkipReason::EmptyInput);
        let j2 = judge_stub(&rule, &JudgeInput { outcome: &skipped }, "p", "j");
        assert_eq!(j2.field("eligible"), Ok(&Value::Bool(false)));
    }

    // —— deliver ——

    struct CountingChannel {
        sent: Cell<u32>,
        fail_channel: Option<String>,
    }

    impl DeliverChannel for CountingChannel {
        fn send(&mut self, d: &Deliverable) -> Result<(), ErrCode> {
            if let Some(f) = &self.fail_channel {
                if *f == d.channel {
                    return Err(ErrCode::E305);
                }
            }
            self.sent.set(self.sent.get() + 1);
            Ok(())
        }
    }

    fn wal_tmp(tag: &str) -> (std::path::PathBuf, Wal) {
        let d = std::env::temp_dir().join(format!("ductile-deliver-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("wal.jsonl");
        let _ = std::fs::remove_file(&p);
        let wal = Wal::open(&p).unwrap();
        (d, wal)
    }

    #[test]
    fn deliver_idempotent_across_crash() {
        let items = vec![Deliverable { channel: "feishu".into(), manifest: "m1".into() }];
        let mut critical = BTreeMap::new();
        let ok_out = Outcome::success(BTreeMap::new(), "");
        critical.insert("build".to_string(), &ok_out);

        // 第一次：真发
        let (dir, wal) = wal_tmp("a");
        let mut wal = wal;
        let ch = CountingChannel { sent: Cell::new(0), fail_channel: None };
        {
            let mut d = Deliverer {
                runner: IdempotentRunner { wal: &mut wal, acked: BTreeSet::new(), exec: ChannelAsExecutor(ch) },
            };
            let out = d.deliver(&items, &critical, "plan-1");
            assert!(out.ok());
            assert_eq!(out.meta().get("newly_sent"), Some(&Value::Int(1)));
            assert_eq!(d.runner.exec.0.sent.get(), 1);
        }
        // crash → 恢复 → 重发：零重复
        let lines: Vec<String> = std::io::BufRead::lines(std::io::BufReader::new(
            std::fs::File::open(dir.join("wal.jsonl")).unwrap(),
        ))
        .map(|l| l.unwrap())
        .collect();
        let rec = crate::kernel::wal::recover(&lines);
        let mut wal2 = Wal::open(&dir.join("wal.jsonl")).unwrap();
        let ch2 = CountingChannel { sent: Cell::new(0), fail_channel: None };
        {
            let mut d = Deliverer {
                runner: IdempotentRunner { wal: &mut wal2, acked: rec.acked_effects, exec: ChannelAsExecutor(ch2) },
            };
            let out = d.deliver(&items, &critical, "plan-1");
            assert!(out.ok());
            assert_eq!(out.meta().get("newly_sent"), Some(&Value::Int(0))); // 零新发
            assert_eq!(d.runner.exec.0.sent.get(), 0); // 通道零调用（零重复副作用）
        }
    }

    #[test]
    fn deliver_blocked_when_critical_fails() {
        let items = vec![Deliverable { channel: "x".into(), manifest: "m".into() }];
        let mut critical = BTreeMap::new();
        let bad_out = Outcome::failed(ErrCode::E302, "boom");
        critical.insert("build".to_string(), &bad_out);
        let (_dir, wal) = wal_tmp("b");
        let mut wal = wal;
        let ch = CountingChannel { sent: Cell::new(0), fail_channel: None };
        let mut d = Deliverer {
            runner: IdempotentRunner { wal: &mut wal, acked: BTreeSet::new(), exec: ChannelAsExecutor(ch) },
        };
        let out = d.deliver(&items, &critical, "p");
        assert_eq!(out.error().unwrap().code, ErrCode::E504);
        assert_eq!(d.runner.exec.0.sent.get(), 0); // 危险步骤零接触（负例）
    }

    #[test]
    fn deliver_blocked_when_critical_skipped() {
        let items = vec![Deliverable { channel: "x".into(), manifest: "m".into() }];
        let skipped = Outcome::skipped(SkipReason::Ineligible("gated".into()));
        let mut critical = BTreeMap::new();
        critical.insert("gate".to_string(), &skipped);
        let (_dir, wal) = wal_tmp("c");
        let mut wal = wal;
        let ch = CountingChannel { sent: Cell::new(0), fail_channel: None };
        let mut d = Deliverer {
            runner: IdempotentRunner { wal: &mut wal, acked: BTreeSet::new(), exec: ChannelAsExecutor(ch) },
        };
        let out = d.deliver(&items, &critical, "p");
        assert_eq!(out.error().unwrap().code, ErrCode::E504);
        assert_eq!(d.runner.exec.0.sent.get(), 0);
    }

    #[test]
    fn deliver_channel_failure_is_error_not_silent() {
        let items = vec![Deliverable { channel: "dead".into(), manifest: "m".into() }];
        let mut critical = BTreeMap::new();
        let ok_out = Outcome::success(BTreeMap::new(), "");
        critical.insert("ok".to_string(), &ok_out);
        let (_dir, wal) = wal_tmp("d");
        let mut wal = wal;
        let ch = CountingChannel { sent: Cell::new(0), fail_channel: Some("dead".into()) };
        let mut d = Deliverer {
            runner: IdempotentRunner { wal: &mut wal, acked: BTreeSet::new(), exec: ChannelAsExecutor(ch) },
        };
        let out = d.deliver(&items, &critical, "p");
        assert!(!out.ok());
        assert_eq!(out.error().unwrap().code, ErrCode::E305);
    }
}
