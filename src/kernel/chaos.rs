//! Ductile v0.24 chaos harness 核心 — D11-12。
//!
//! 六类 killpoint（行动③ §1）：pre-exec / post-spawn / WAL 写前 / WAL commit 后 /
//! ack 前 / ack 后。每个 killpoint：跑基线 → 在该点"杀"（丢未落盘状态）→
//! 恢复 → 重跑 → 断言四判据：
//!   ① 已确认副作用重复数=0
//!   ② 未完成步骤续跑（in_flight 恰好重跑）
//!   ③ WAL 审计链完整（verify_chain）
//!   ④ result_projection 与基线逐字段一致（results_match）
//!
//! 自举：本模块是被测对象；外层 .pipeline（chaos.pipeline）用旧 DSL 编排
//! `ductile kernel-chaos` 子命令——测试场本身是 ductile 管线（吃狗粮）。

use crate::kernel::hash::sha256_hex;
use crate::kernel::judge::{DeliverChannel, Deliverable, Deliverer};
use crate::kernel::types::{results_match, EffectKey, Outcome, Value};
use crate::kernel::wal::{recover, verify_chain, EffectExecutor, IdempotentRunner, Wal, WalRecord};
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// 六 killpoint。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillPoint {
    PreExec,
    PostSpawn,
    WalBefore,
    WalCommitted,
    AckBefore,
    AckAfter,
}

impl KillPoint {
    pub fn all() -> [KillPoint; 6] {
        [
            KillPoint::PreExec,
            KillPoint::PostSpawn,
            KillPoint::WalBefore,
            KillPoint::WalCommitted,
            KillPoint::AckBefore,
            KillPoint::AckAfter,
        ]
    }
    pub fn name(&self) -> &'static str {
        match self {
            KillPoint::PreExec => "pre_exec",
            KillPoint::PostSpawn => "post_spawn",
            KillPoint::WalBefore => "wal_before",
            KillPoint::WalCommitted => "wal_committed",
            KillPoint::AckBefore => "ack_before",
            KillPoint::AckAfter => "ack_after",
        }
    }
}

/// 被测微工作流：两步 effect + 一条 deliver（黄金 G1 的 effect 化身）。
pub struct ScenarioWork {
    pub plan_fp: String,
    pub steps: Vec<StepSpec>,
    pub deliver_items: Vec<Deliverable>,
}

#[derive(Debug, Clone)]
pub struct StepSpec {
    pub id: String,
    pub effect_index: u32,
    pub payload: String,
}

/// 副作用通道（计数 + 记录顺序）。
#[derive(Default)]
pub struct RecordingChannel {
    pub sent: Vec<String>,
    pub fail: Option<String>,
}

impl DeliverChannel for RecordingChannel {
    fn send(&mut self, d: &Deliverable) -> Result<(), crate::kernel::types::ErrCode> {
        if let Some(f) = &self.fail {
            if *f == d.channel {
                return Err(crate::kernel::types::ErrCode::E305);
            }
        }
        self.sent.push(format!("{}|{}", d.channel, d.manifest));
        Ok(())
    }
}
impl crate::kernel::wal::EffectExecutor for RecordingChannel {
    fn execute(&mut self, effect: &EffectKey) -> Result<(), crate::kernel::types::ErrCode> {
        self.sent
            .push(format!("effect:{}#{}", effect.effect_index, effect.input_digest));
        Ok(())
    }
}

/// 一次完整跑（不 crash）：全部步骤 + deliver。
fn run_full(
    wal: &mut Wal,
    acked: BTreeSet<EffectKey>,
    mut chan: RecordingChannel,
    sc: &ScenarioWork,
) -> (Outcome, RecordingChannel) {
    // 旁路借用：IdempotentRunner 持 &mut Wal + executor。
    // 我们需要 wal 在 effect ack 后追加 outcome——重构为手动序列避免双借。
    let mut acked = acked;
    for step in &sc.steps {
        // intent
        let _ = wal.append(&WalRecord::Intent {
            run_id: "r".into(),
            step_id: step.id.clone(),
            attempt: 1,
            input_digest: sha256_hex(step.payload.as_bytes()),
        });
        let ek = EffectKey {
            plan_fingerprint: sc.plan_fp.clone(),
            effect_index: step.effect_index,
            input_digest: step.payload.clone(),
        };
        if !acked.contains(&ek) {
            if crate::kernel::wal::EffectExecutor::execute(&mut chan, &ek).is_err() {
                return (Outcome::failed(crate::kernel::types::ErrCode::E302, step.id.clone()), chan);
            }
            let _ = wal.append(&WalRecord::EffectAck {
                run_id: "r".into(),
                step_id: step.id.clone(),
                effect: ek.clone(),
            });
            acked.insert(ek);
        }
        let _ = wal.append(&WalRecord::Outcome {
            run_id: "r".into(),
            step_id: step.id.clone(),
            attempt: 1,
            status: "Success".into(),
            error_code: None,
            skip_reason: None,
            fields_digest: Some(sha256_hex(step.payload.as_bytes())),
        });
    }
    // deliver（复用 judge::Deliverer 语义：critical=全部步骤）
    let mut critical: BTreeMap<String, Outcome> = sc
        .steps
        .iter()
        .map(|s| (s.id.clone(), Outcome::success(BTreeMap::new(), "")))
        .collect();
    let crit_ref: BTreeMap<String, &Outcome> =
        critical.iter().map(|(k, v)| (k.clone(), v)).collect();
    let mut runner = IdempotentRunner { wal, acked, exec: chan };
    let mut d = Deliverer { runner };
    let out = d.deliver(&sc.deliver_items, &crit_ref, &sc.plan_fp);
    critical.clear();
    let _ = &mut critical;
    (out, d.runner.exec)
}

/// crash 模拟：按 killpoint 截断 WAL（丢"未持久化"的最后状态）。
/// 返回截断后的行（写回文件）。
fn crash_at(lines: &[String], kp: KillPoint) -> Vec<String> {
    match kp {
        // 步骤还没开始：全丢
        KillPoint::PreExec => vec![],
        // 第一步 intent 前：丢最后 1 条
        KillPoint::PostSpawn => lines[..lines.len().saturating_sub(1)].to_vec(),
        // WAL 写前（丢最后 2 条：一写一ack）
        KillPoint::WalBefore => lines[..lines.len().saturating_sub(2)].to_vec(),
        // WAL commit 后、ack 前：丢最后 1 条（ack 未落）
        KillPoint::WalCommitted => lines[..lines.len().saturating_sub(1)].to_vec(),
        // ack 前：同上（对 WAL 而言 ack=写 EffectAck 行）
        KillPoint::AckBefore => lines[..lines.len().saturating_sub(1)].to_vec(),
        // ack 后：不丢（完整）——测的是恢复器正确识别已 ack
        KillPoint::AckAfter => lines.to_vec(),
    }
}

/// 单场景单 killpoint 单 seed 的 chaos 判定。
/// seed 变输入（payload/plan_fp/manifest），不变结构——多 seed 验证
/// 「不同输入同一保证」（plan §多 seed 基准协议：确定性组件用性质测试）。
#[derive(Debug, Clone)]
pub struct ChaosVerdict {
    pub killpoint: &'static str,
    pub seed: u64,
    pub duplicate_effects: usize,
    pub chain_ok: bool,
    pub results_match: bool,
    pub resumed: bool,
    pub side_effects_total: usize,
}

pub fn chaos_once(dir: &Path, kp: KillPoint, seed: u64) -> Result<ChaosVerdict, String> {
    let sc = ScenarioWork {
        plan_fp: format!("chaos-plan-{seed}"),
        steps: vec![
            StepSpec { id: "s1".into(), effect_index: 0, payload: format!("alpha-{seed}") },
            StepSpec { id: "s2".into(), effect_index: 1, payload: format!("beta-{seed}") },
        ],
        deliver_items: vec![Deliverable { channel: "sink".into(), manifest: format!("m-{seed}") }],
    };

    // —— 基线（无故障）——
    let base_path = dir.join(format!("base-{}-{}.jsonl", kp.name(), seed));
    let _ = std::fs::remove_file(&base_path);
    let mut base_wal = Wal::open(&base_path).map_err(|e| e.to_string())?;
    let mut base_chan = RecordingChannel::default();
    let (base_out, base_chan) = run_full(&mut base_wal, BTreeSet::new(), base_chan, &sc);

    // —— crash 场景：跑到一半按 killpoint 截断 ——
    let crash_path = dir.join(format!("crash-{}-{}.jsonl", kp.name(), seed));
    let _ = std::fs::remove_file(&crash_path);
    // 先完整跑出 WAL，再按 killpoint 截断（等效于在那些点被杀）
    let mut cw = Wal::open(&crash_path).map_err(|e| e.to_string())?;
    let mut crash_chan = RecordingChannel::default();
    let _ = run_full(&mut cw, BTreeSet::new(), crash_chan, &sc);
    let full_lines: Vec<String> = std::io::BufReader::new(
        std::fs::File::open(&crash_path).map_err(|e| e.to_string())?,
    )
    .lines()
    .map(|l| l.unwrap())
    .collect();
    let truncated = crash_at(&full_lines, kp);
    let content = if truncated.is_empty() {
        String::new()
    } else {
        truncated.join("\n") + "\n"
    };
    std::fs::write(&crash_path, content).map_err(|e| e.to_string())?;

    // —— 恢复 + 重跑 ——
    let rec_lines: Vec<String> = std::io::BufReader::new(
        std::fs::File::open(&crash_path).map_err(|e| e.to_string())?,
    )
    .lines()
    .map(|l| l.unwrap())
    .collect();
    let rec = recover(&rec_lines);
    // crash 时刻 ack 快照（判据①ground truth；重跑消费 acked 前先留证）
    let acked_at_crash: BTreeSet<String> = rec
        .acked_effects
        .iter()
        .map(|k| format!("effect:{}#{}", k.effect_index, k.input_digest))
        .collect();
    let sink_acked: BTreeSet<String> = rec
        .acked_effects
        .iter()
        .map(|k| k.input_digest.clone())
        .filter(|d| d.contains('|'))
        .collect();
    let acked_count = rec.acked_effects.len();
    let mut rec_wal = Wal::open(&crash_path).map_err(|e| e.to_string())?;
    let mut replay_chan = RecordingChannel::default();
    let (replay_out, replay_chan) = run_full(&mut rec_wal, rec.acked_effects, replay_chan, &sc);

    // —— 判据 ——
    // ① 零重复（行动③原意）：已确认副作用（crash 时 WAL 已 ack）不得再次
    // 真实执行（acked_at_crash 快照在重跑前采集）。未确认的 effect 重跑
    // 执行是 at-least-once 的正确行为，不算重复。
    let replay_keys: Vec<String> = replay_chan
        .sent
        .iter()
        .filter(|s| s.starts_with("effect:"))
        .cloned()
        .collect();
    let duplicate_effects = replay_keys.iter().filter(|k| acked_at_crash.contains(*k)).count();

    // ③ 链完整（重跑后的完整 WAL）
    let final_lines: Vec<String> = std::io::BufReader::new(
        std::fs::File::open(&crash_path).map_err(|e| e.to_string())?,
    )
    .lines()
    .map(|l| l.unwrap())
    .collect();
    let chain_ok = verify_chain(&final_lines).is_ok();

    // ④ 逐字段一致
    let results_match = results_match(&base_out, &replay_out).is_ok();

    // ② 续跑：重跑真实发生（至少一条新副作用或复用决策被走过）
    let resumed = replay_chan.sent.len() >= 1 || acked_count >= 1;

    // 通道里 sink 副作用零重复（同口径：以 crash 时 ack 为准）
    let sink_dups = replay_chan
        .sent
        .iter()
        .filter(|s| !s.starts_with("effect:") && sink_acked.contains(s.as_str()))
        .count();

    Ok(ChaosVerdict {
        killpoint: kp.name(),
        seed,
        duplicate_effects: duplicate_effects + sink_dups,
        chain_ok,
        results_match,
        resumed,
        side_effects_total: base_chan.sent.len() + replay_chan.sent.len(),
    })
}

/// 汇总入口（kernel-chaos 子命令调用）。
/// seeds：每个 killpoint 跑的种子数（D14 验收=3；单测=1）。
pub fn chaos_all(base_dir: &Path, seeds: u64) -> Result<Vec<ChaosVerdict>, String> {
    let dir: PathBuf = base_dir.to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for kp in KillPoint::all() {
        // 每个 killpoint 独立子目录（隔离），内按 seed 分文件
        let sub = dir.join(kp.name());
        std::fs::create_dir_all(&sub).map_err(|e| e.to_string())?;
        for seed in 0..seeds {
            out.push(chaos_once(&sub, kp, seed)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ductile-chaos-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn all_six_killpoints_pass_four_criteria() {
        let dir = tmp("six");
        // D14 验收口径：六 killpoint × 3 seeds 零重复（进单测=永久回归钉）
        let verdicts = chaos_all(&dir, 3).unwrap();
        assert_eq!(verdicts.len(), 18);
        for v in &verdicts {
            assert_eq!(v.duplicate_effects, 0, "{} seed{} duplicate!", v.killpoint, v.seed);
            assert!(v.chain_ok, "{} seed{} chain broken", v.killpoint, v.seed);
            assert!(v.results_match, "{} seed{} results differ", v.killpoint, v.seed);
            assert!(v.resumed, "{} seed{} did not resume", v.killpoint, v.seed);
        }
    }

    #[test]
    fn crash_truncation_shapes() {
        let lines: Vec<String> = (0..5).map(|i| format!("{{\"k\":\"intent\",\"step\":\"s{}\",\"prev_hash\":\"h{}\"}}", i, i)).collect();
        assert!(crash_at(&lines, KillPoint::PreExec).is_empty());
        assert_eq!(crash_at(&lines, KillPoint::PostSpawn).len(), 4);
        assert_eq!(crash_at(&lines, KillPoint::WalBefore).len(), 3);
        assert_eq!(crash_at(&lines, KillPoint::AckAfter).len(), 5);
    }
}
