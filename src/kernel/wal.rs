//! Ductile v0.24 执行检查点 + WAL 恢复 — D7-8。
//!
//! 行动③判据：
//!   1. 已确认副作用重复数=0（EffectKey 唯一约束）
//!   2. 未完成节点从最后持久化检查点续跑
//!   3. WAL 可恢复；审计 prev_hash 链完整
//!   4. result_projection 与无故障基线逐字段一致
//!
//! 本文件：检查点日志（append-only JSONL + fsync）+ 恢复器。
//! 每步三相位：intent（执行前落盘）→ effect-ack（副作用确认）→
//! outcome（终态）。恢复时：
//!   - 有 outcome → 跳过（不重跑）
//!   - 有 intent 无 outcome → 该步重跑，但 effect-ack 已存在的
//!     EffectKey 直接复用（零重复副作用）
//!   - 无 intent → 未开始

use crate::kernel::hash::sha256_hex;
use crate::kernel::types::{AuditEvent, EffectKey, ErrCode};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};

// ── WAL 记录 ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum WalRecord {
    /// 步骤开始（执行前；crash 后此步需要重跑）。
    Intent { run_id: String, step_id: String, attempt: u32, input_digest: String },
    /// 副作用已确认（EffectKey 落账；重跑时同 key 直接复用）。
    EffectAck { run_id: String, step_id: String, effect: EffectKey },
    /// 步骤终态（crash 后此步跳过）。
    Outcome { run_id: String, step_id: String, attempt: u32, status: String, error_code: Option<String>, skip_reason: Option<String>, fields_digest: Option<String> },
    /// 审计事件（链式 prev_hash）。
    Audit(AuditEvent),
}

#[derive(Debug, Clone)]
pub struct ChainState {
    pub last_hash: String,
    pub seq: u64,
}

impl Default for ChainState {
    fn default() -> Self {
        ChainState { last_hash: "0".repeat(64), seq: 0 }
    }
}

/// WAL 写入器：每记录一行 JSON + fsync（crash-safe）。
pub struct Wal {
    file: std::fs::File,
    pub chain: ChainState,
}

impl Wal {
    pub fn open(path: &std::path::Path) -> std::io::Result<Self> {
        // 追加模式；已有链状态由 recover 读出后传入 chain
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        // 续链：append 到已有 WAL 时，链头=最后一行的 sha256
        // （否则新行 prev_hash 从 0×64 起 → verify_chain 断）
        let mut chain = ChainState::default();
        if let Ok(f) = std::fs::File::open(path) {
            let mut last_line = String::new();
            for line in std::io::BufReader::new(f).lines().flatten() {
                if !line.trim().is_empty() {
                    last_line = line;
                }
            }
            if !last_line.is_empty() {
                chain.last_hash = sha256_hex(last_line.as_bytes());
            }
        }
        Ok(Wal { file, chain })
    }

    pub fn append(&mut self, rec: &WalRecord) -> std::io::Result<String> {
        let line = wal_to_json(rec, &self.chain.last_hash);
        self.chain.seq += 1;
        self.chain.last_hash = sha256_hex(line.as_bytes());
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.sync_data()?; // fsync：crash-safe
        Ok(self.chain.last_hash.clone())
    }
}

/// 审计链完整性校验：逐行重算 prev_hash 链。
pub fn verify_chain(lines: &[String]) -> Result<(), usize> {
    let mut expect_prev = "0".repeat(64);
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue; // 空行不参与链（防御历史文件）
        }
        if !line.contains(&format!("\"prev_hash\":\"{}\"", expect_prev)) {
            return Err(i);
        }
        expect_prev = sha256_hex(line.as_bytes());
    }
    Ok(())
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn wal_to_json(rec: &WalRecord, prev_hash: &str) -> String {
    match rec {
        WalRecord::Intent { run_id, step_id, attempt, input_digest } => format!(
            "{{\"k\":\"intent\",\"run\":\"{}\",\"step\":\"{}\",\"att\":{},\"in\":\"{}\",\"prev_hash\":\"{}\"}}",
            json_escape(run_id), json_escape(step_id), attempt, input_digest, prev_hash
        ),
        WalRecord::EffectAck { run_id, step_id, effect } => format!(
            "{{\"k\":\"effect\",\"run\":\"{}\",\"step\":\"{}\",\"ek\":\"{}\",\"prev_hash\":\"{}\"}}",
            json_escape(run_id), json_escape(step_id), effect, prev_hash
        ),
        WalRecord::Outcome { run_id, step_id, attempt, status, error_code, skip_reason, fields_digest } => format!(
            "{{\"k\":\"outcome\",\"run\":\"{}\",\"step\":\"{}\",\"att\":{},\"st\":\"{}\",\"err\":{},\"skip\":{},\"fd\":{},\"prev_hash\":\"{}\"}}",
            json_escape(run_id),
            json_escape(step_id),
            attempt,
            json_escape(status),
            error_code.as_deref().map(|c| format!("\"{}\"", c)).unwrap_or_else(|| "null".into()),
            skip_reason.as_deref().map(|c| format!("\"{}\"", json_escape(c))).unwrap_or_else(|| "null".into()),
            fields_digest.as_deref().map(|c| format!("\"{}\"", c)).unwrap_or_else(|| "null".into()),
            prev_hash
        ),
        WalRecord::Audit(_) => format!("{{\"k\":\"audit\",\"prev_hash\":\"{}\"}}", prev_hash),
    }
}

// ── 恢复器 ───────────────────────────────────────────────

/// 从 WAL 行流重建恢复态。
#[derive(Debug, Default)]
pub struct Recovered {
    /// 已终态步骤（重跑时跳过）。
    pub done: std::collections::BTreeSet<String>,
    /// 已确认副作用（重跑时同 key 复用——零重复）。
    pub acked_effects: std::collections::BTreeSet<EffectKey>,
    /// 有 intent 无 outcome（crash 中断点；需要重跑）。
    pub in_flight: Vec<(String, u32)>,
}

pub fn recover(lines: &[String]) -> Recovered {
    let mut r = Recovered::default();
    let mut intent_seen: BTreeMap<String, u32> = BTreeMap::new();
    for line in lines {
        if line.contains("\"k\":\"intent\"") {
            if let (Some(run), Some(step), Some(att)) =
                (grab(line, "run"), grab(line, "step"), grab_num(line, "att"))
            {
                let _ = run;
                intent_seen.insert(step.clone(), att);
            }
        } else if line.contains("\"k\":\"effect\"") {
            if let (Some(step), Some(ek)) = (grab(line, "step"), grab(line, "ek")) {
                let _ = step;
                if let Some(key) = parse_effect_key(&ek) {
                    r.acked_effects.insert(key);
                }
            }
        } else if line.contains("\"k\":\"outcome\"") {
            if let Some(step) = grab(line, "step") {
                r.done.insert(step.clone());
                intent_seen.remove(&step);
            }
        }
    }
    r.in_flight = intent_seen.into_iter().map(|(s, a)| (s, a)).collect();
    r
}

fn grab(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\":\"", key);
    let i = line.find(&pat)? + pat.len();
    let rest = &line[i..];
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(out),
            _ => out.push(c),
        }
    }
    None
}

fn grab_num(line: &str, key: &str) -> Option<u32> {
    let pat = format!("\"{}\":", key);
    let i = line.find(&pat)? + pat.len();
    let rest = &line[i..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn parse_effect_key(s: &str) -> Option<EffectKey> {
    // plan#idx#digest
    let parts: Vec<&str> = s.split('#').collect();
    if parts.len() == 3 {
        Some(EffectKey {
            plan_fingerprint: parts[0].to_string(),
            effect_index: parts[1].parse().ok()?,
            input_digest: parts[2].to_string(),
        })
    } else {
        None
    }
}

// ── 幂等 effect 执行器（outbox 语义）─────────────────────

/// effect 执行抽象：真实世界副作用由调用方实现；返回值=已执行。
pub trait EffectExecutor {
    fn execute(&mut self, effect: &EffectKey) -> Result<(), ErrCode>;
}

/// 幂等包装：acked 的 key 直接复用，不重复执行（判据 1）。
pub struct IdempotentRunner<'a, E: EffectExecutor> {
    pub wal: &'a mut Wal,
    pub acked: std::collections::BTreeSet<EffectKey>,
    pub exec: E,
}

impl<'a, E: EffectExecutor> IdempotentRunner<'a, E> {
    pub fn run_effect(&mut self, effect: &EffectKey) -> Result<bool, ErrCode> {
        if self.acked.contains(effect) {
            return Ok(false); // 复用：零重复
        }
        self.exec.execute(effect)?;
        self.wal
            .append(&WalRecord::EffectAck { run_id: String::new(), step_id: String::new(), effect: effect.clone() })
            .map_err(|_| ErrCode::E402)?;
        self.acked.insert(effect.clone());
        Ok(true) // 真实执行了
    }
}

// ── 恢复语义端到端测试（黄金判据）────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    struct CountingExec {
        count: std::cell::Cell<u32>,
        fail_on: Option<EffectKey>,
    }

    impl EffectExecutor for CountingExec {
        fn execute(&mut self, effect: &EffectKey) -> Result<(), ErrCode> {
            if let Some(f) = &self.fail_on {
                if f == effect {
                    return Err(ErrCode::E302);
                }
            }
            self.count.set(self.count.get() + 1);
            Ok(())
        }
    }

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ductile-wal-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn wal_roundtrip_and_recover() {
        let dir = tmpdir();
        let path = dir.join("wal1.jsonl");
        let _ = std::fs::remove_file(&path);

        let mut wal = Wal::open(&path).unwrap();
        let ek = EffectKey { plan_fingerprint: "p1".into(), effect_index: 0, input_digest: "d0".into() };
        wal.append(&WalRecord::Intent {
            run_id: "r1".into(),
            step_id: "step_a".into(),
            attempt: 1,
            input_digest: "in0".into(),
        }).unwrap();
        wal.append(&WalRecord::EffectAck { run_id: "r1".into(), step_id: "step_a".into(), effect: ek.clone() }).unwrap();
        wal.append(&WalRecord::Outcome {
            run_id: "r1".into(),
            step_id: "step_a".into(),
            attempt: 1,
            status: "Success".into(),
            error_code: None,
            skip_reason: None,
            fields_digest: Some("f0".into()),
        }).unwrap();
        // step_b 只到 intent（模拟 crash）
        wal.append(&WalRecord::Intent {
            run_id: "r1".into(),
            step_id: "step_b".into(),
            attempt: 1,
            input_digest: "in1".into(),
        }).unwrap();

        // 恢复
        let lines: Vec<String> = std::io::BufReader::new(std::fs::File::open(&path).unwrap())
            .lines()
            .map(|l| l.unwrap())
            .collect();
        let rec = recover(&lines);
        assert!(rec.done.contains("step_a"));
        assert_eq!(rec.in_flight.len(), 1);
        assert_eq!(rec.in_flight[0].0, "step_b");
        assert!(rec.acked_effects.contains(&ek));
        // 审计链完整
        assert!(verify_chain(&lines).is_ok());
    }

    #[test]
    fn chain_tamper_detected() {
        let dir = tmpdir();
        let path = dir.join("wal2.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut wal = Wal::open(&path).unwrap();
        for i in 0..3 {
            wal.append(&WalRecord::Intent {
                run_id: "r".into(),
                step_id: format!("s{}", i),
                attempt: 1,
                input_digest: "x".into(),
            }).unwrap();
        }
        let lines: Vec<String> = std::io::BufReader::new(std::fs::File::open(&path).unwrap())
            .lines()
            .map(|l| l.unwrap())
            .collect();
        assert!(verify_chain(&lines).is_ok());
        // 篡改第2行：其自身内容变 → 其 hash 变 → 第3行 prev_hash 断（Err(2)）
        let mut tampered = lines.clone();
        tampered[1] = tampered[1].replace("s1", "EVIL");
        assert_eq!(verify_chain(&tampered), Err(2));
        // 删掉第1行（链断）
        let truncated: Vec<String> = lines[1..].to_vec();
        assert_eq!(verify_chain(&truncated), Err(0));
    }

    #[test]
    fn idempotent_zero_duplicate_effects() {
        // 判据 1 端到端：crash → 恢复 → 重跑 → 副作用零重复
        let dir = tmpdir();
        let path = dir.join("wal3.jsonl");
        let _ = std::fs::remove_file(&path);

        let ek = EffectKey { plan_fingerprint: "p".into(), effect_index: 7, input_digest: "dd".into() };
        let mut wal = Wal::open(&path).unwrap();
        // 第一次跑：执行 + ack
        {
            let exec = CountingExec { count: std::cell::Cell::new(0), fail_on: None };
            let mut runner = IdempotentRunner { wal: &mut wal, acked: BTreeSet::new(), exec };
            assert!(runner.run_effect(&ek).unwrap()); // 真执行
            assert_eq!(runner.exec.count.get(), 1);
        }
        // crash：重启进程，恢复 acked，重跑同一 effect
        {
            let lines: Vec<String> = std::io::BufReader::new(std::fs::File::open(&path).unwrap())
                .lines()
                .map(|l| l.unwrap())
                .collect();
            let rec = recover(&lines);
            let mut wal2 = Wal::open(&path).unwrap();
            let exec = CountingExec { count: std::cell::Cell::new(0), fail_on: None };
            let mut runner = IdempotentRunner { wal: &mut wal2, acked: rec.acked_effects, exec };
            assert!(!runner.run_effect(&ek).unwrap()); // 复用，零重复
            assert_eq!(runner.exec.count.get(), 0); // 没有第二次真实执行
        }
    }

    #[test]
    fn effect_failure_no_ack() {
        // 失败的 effect 不 ack——重跑时才重试（fail-closed，不装成功）
        let dir = tmpdir();
        let path = dir.join("wal4.jsonl");
        let _ = std::fs::remove_file(&path);
        let ek = EffectKey { plan_fingerprint: "p".into(), effect_index: 0, input_digest: "x".into() };
        let mut wal = Wal::open(&path).unwrap();
        let exec = CountingExec { count: std::cell::Cell::new(0), fail_on: Some(ek.clone()) };
        let mut runner = IdempotentRunner { wal: &mut wal, acked: BTreeSet::new(), exec };
        assert!(runner.run_effect(&ek).is_err());
        assert!(runner.acked.is_empty()); // 未 ack
    }
}
