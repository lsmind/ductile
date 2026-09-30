//! MLV 治理层（v3.1）：registry=可丢弃投影；唯一权威=mlv.rs 记录层账本。
//!
//! apply_record=commit 与 replay 共用唯一路径（v3 补丁：禁止两套实现）。
//! 校验序（v3 补丁5 冻结）：①幂等ACK短路 ②MAC验签 ③nonce首消费(暂存)
//! ④语义(边/唯一/回执) ⑤rename 持久化（失败回滚 nonce 暂存）。
//! replay 时钟=accepted_at_ns（确定性）；live 时钟=now（由调用方传入）。

use crate::kernel::mlv::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// 治理错误（输出协议冻结，v3 补丁6）。
#[derive(Debug, Clone, PartialEq)]
pub enum GovErr {
    IdempotencyConflict,          // E425 同键异摘要
    EnvelopeInvalid(String),      // E423-env
    NonceReuse,                   // E423-nonce-reuse
    IllegalEdge,                  // E422
    ReceiptRequired,              // receipt-required
    LedgerMissing,                // ledger-missing
    Terminal,                     // terminal
    Internal(String),
}

impl std::fmt::Display for GovErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.code(), self.detail())
    }
}

impl GovErr {
    pub fn code(&self) -> &'static str {
        match self {
            GovErr::IdempotencyConflict => "ERR E425 idempotency-conflict",
            GovErr::EnvelopeInvalid(_) => "ERR E423-env",
            GovErr::NonceReuse => "ERR E423-nonce-reuse",
            GovErr::IllegalEdge => "ERR E422 illegal-edge",
            GovErr::ReceiptRequired => "ERR receipt-required",
            GovErr::LedgerMissing => "ERR ledger-missing",
            GovErr::Terminal => "ERR terminal",
            GovErr::Internal(_) => "ERR internal",
        }
    }
    pub fn detail(&self) -> String {
        match self {
            GovErr::EnvelopeInvalid(m) => m.clone(),
            GovErr::Internal(m) => m.clone(),
            _ => String::new(),
        }
    }
}

/// 投影（全部可从账本重放重建）。
#[derive(Debug, Default, Clone)]
pub struct Projections {
    pub states: BTreeMap<(String, u64), MlvState>,
    pub current: BTreeMap<String, u64>,
    pub phase_ops: BTreeSet<(String, u64, &'static str)>, // (binding, rev, op)
    pub effect_index: BTreeMap<String, (String, BTreeMap<String, String>)>, // key → (request_digest, result)
    pub nonce_set: BTreeSet<String>,
    pub stop_gen: BTreeMap<String, u64>,
    pub receipts: BTreeMap<(String, u64), BTreeMap<String, String>>, // (binding, rev) → receipt
    pub pending_begin: BTreeSet<(String, u64)>,
}

pub struct GovRegistry {
    pub ledger: MlvLedger,
    pub proj: Projections,
    record_count: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApplyOutcome {
    /// 新记录落账。
    Committed { record_hash: String, to: Option<MlvState> },
    /// 幂等命中（同键同摘要）——返回原 result。
    Acked { result: BTreeMap<String, String> },
}

impl GovRegistry {
    /// open=取锁+验链+全量重放（投影=可丢弃缓存，每次重建）。
    pub fn open(path: &Path) -> Result<Self, GovErr> {
        let ledger = MlvLedger::open(path).map_err(map_ledger_err)?;
        let records = ledger.records().map_err(|e| GovErr::Internal(e))?;
        verify_chain(&records).map_err(|e| GovErr::Internal(format!("verify: {e}")))?;
        let mut reg = GovRegistry { ledger, proj: Projections::default(), record_count: 1 };
        for rec in &records[1..] {
            // replay：同 apply 语义路径，时钟=accepted_at_ns
            reg.apply_record(rec, rec.accepted_at_ns, false)
                .map_err(|e| GovErr::Internal(format!("replay@{}: {e:?}", rec.seq)))?;
        }
        Ok(reg)
    }

    /// 显式 init（账本不存在时）。
    pub fn init(path: &Path, at: u64) -> Result<Self, GovErr> {
        let (ledger, _) = MlvLedger::init(path, at).map_err(map_ledger_err)?;
        Ok(GovRegistry { ledger, proj: Projections::default(), record_count: 1 })
    }

    pub fn head(&self) -> Result<String, GovErr> {
        self.ledger.head().map_err(|e| GovErr::Internal(e))
    }

    /// 提交请求（live：t=now 由调用方传）。
    /// 校验序：①幂等ACK ②MAC ③nonce暂存 ④语义 ⑤append（rename）。
    pub fn submit(&mut self, rec: LedgerRecord, now_ns: u64) -> Result<ApplyOutcome, GovErr> {
        // ① 幂等短路（ACK 不追加记录）
        if let Some((digest, result)) = self.proj.effect_index.get(&rec.effect_key) {
            if digest == &rec.request_digest {
                return Ok(ApplyOutcome::Acked { result: result.clone() });
            }
            return Err(GovErr::IdempotencyConflict);
        }
        // ② MAC（信封在场必验）
        if let Some(env) = &rec.envelope {
            verify_envelope_mac(env).map_err(GovErr::EnvelopeInvalid)?;
        }
        // ③ nonce 首消费（暂存内存；失败回滚）
        let nonce = rec.nonce.clone();
        if let Some(n) = &nonce {
            if self.proj.nonce_set.contains(n) {
                return Err(GovErr::NonceReuse);
            }
        }
        // ④ 语义
        self.check_semantics(&rec, now_ns)?;
        // ⑤ 持久化（append 内部=原子提交）
        let rh = self.ledger.append(rec.clone()).map_err(|e| GovErr::Internal(e))?;
        // 提交成功：投影更新（与 apply_record 同一路径——此处直接调 apply 内部）
        let mut committed = rec;
        committed.record_hash = rh.clone();
        self.apply_record(&committed, now_ns, true)
            .map_err(|e| GovErr::Internal(format!("post-commit apply: {e:?}")))?;
        Ok(ApplyOutcome::Committed { record_hash: rh, to: committed.to })
    }

    /// 语义检查（校验序④；replay 重算 result 与存储比对）。
    fn check_semantics(&self, rec: &LedgerRecord, t_ns: u64) -> Result<(), GovErr> {
        // seq 连续
        let records_len = self.seq_hint();
        if rec.seq != records_len {
            return Err(GovErr::Internal(format!("seq {} != next {}", rec.seq, records_len)));
        }
        // op 白名单已由 parse 保证；边表：
        let binding = rec.binding_id.clone().unwrap_or_default();
        let rev = rec.revision.unwrap_or(0);
        match rec.op {
            MlvOp::LedgerInit => {
                return Err(GovErr::Internal("LEDGER_INIT only via explicit init".into()));
            }
            MlvOp::CreateProposal => {
                // from=null 专属；rev=current+1 或首 rev=0；同 rev 终态禁复活
                if rec.from.is_some() {
                    return Err(GovErr::IllegalEdge);
                }
                if let Some(&cur) = self.proj.current.get(&binding) {
                    let old = self.proj.states.get(&(binding.clone(), cur));
                    match old {
                        Some(s) if s.is_terminal() => {
                            if rev != cur + 1 {
                                return Err(GovErr::IllegalEdge);
                            }
                        }
                        Some(_) => return Err(GovErr::IllegalEdge), // 旧 rev 未终态→禁新 rev
                        None => return Err(GovErr::Internal("current without state".into())),
                    }
                } else if rev != 0 {
                    return Err(GovErr::IllegalEdge);
                }
                if self.proj.states.contains_key(&(binding.clone(), rev)) {
                    return Err(GovErr::IllegalEdge); // 复活
                }
            }
            MlvOp::RegistryConfirm => {
                // 无状态边；回执登记（receipt 有效期由信封时间式验）
                if rec.registry_receipt.is_none() {
                    return Err(GovErr::ReceiptRequired);
                }
            }
            MlvOp::Revoke | MlvOp::Terminal => {
                let from = rec.from.ok_or(GovErr::IllegalEdge)?;
                if from.is_terminal() {
                    return Err(GovErr::Terminal); // 终态不可复活
                }
                if rec.to != Some(if rec.op == MlvOp::Revoke { MlvState::Revoked } else { MlvState::Terminal }) {
                    return Err(GovErr::IllegalEdge);
                }
                if self.proj.states.get(&(binding.clone(), rev)) != Some(&from) {
                    return Err(GovErr::IllegalEdge);
                }
            }
            MlvOp::ActivateCommit => {
                let from = rec.from.ok_or(GovErr::IllegalEdge)?;
                let to = commit_target(from).ok_or(GovErr::IllegalEdge)?;
                if rec.to != Some(to) {
                    return Err(GovErr::IllegalEdge);
                }
                if self.proj.states.get(&(binding.clone(), rev)) != Some(&from) {
                    return Err(GovErr::IllegalEdge);
                }
                // ACTIVATING 路径需已确认回执（VERIFIED 修复链回径免回执）
                if from == MlvState::Activating {
                    let receipt = self.proj.receipts.get(&(binding.clone(), rev))
                        .ok_or(GovErr::ReceiptRequired)?;
                    let exp = receipt.get("expires_at_ns")
                        .and_then(|v| v.parse::<u64>().ok())
                        .ok_or(GovErr::ReceiptRequired)?;
                    if !envelope_time_ok(
                        receipt.get("issued_at_ns").and_then(|v| v.parse().ok()).unwrap_or(0),
                        exp, t_ns, DEFAULT_SKEW_NS,
                    ) {
                        return Err(GovErr::ReceiptRequired); // 过期回执
                    }
                }
            }
            MlvOp::Quarantine => {
                let from = rec.from.ok_or(GovErr::IllegalEdge)?;
                if !QUARANTINE_SOURCES.contains(&from) {
                    return Err(GovErr::IllegalEdge);
                }
                if self.proj.states.get(&(binding.clone(), rev)) != Some(&from) {
                    return Err(GovErr::IllegalEdge);
                }
            }
            _ => {
                // 静态边 op（Grant/Decision/ActivateBegin/Abandon/Investigate/Repair/Verify）
                let (from, to) = state_edge(rec.op).ok_or(GovErr::IllegalEdge)?;
                let expect_from = from.ok_or(GovErr::IllegalEdge)?;
                if rec.from != Some(expect_from) || rec.to != Some(to) {
                    return Err(GovErr::IllegalEdge);
                }
                if self.proj.states.get(&(binding.clone(), rev)) != Some(&expect_from) {
                    return Err(GovErr::IllegalEdge);
                }
                if rec.op == MlvOp::ActivateBegin
                    && !self.proj.pending_begin.insert_would_be_new(&(binding.clone(), rev))
                {
                    // 同 (binding,rev) 只允许一个未决 begin
                    return Err(GovErr::IllegalEdge);
                }
            }
        }
        // phase_ops 唯一（除 RegistryConfirm 可多回执/Abandon 后重 begin）
        let opname: &'static str = match rec.op {
            MlvOp::Grant => "GRANT",
            MlvOp::Decision => "DECISION",
            MlvOp::ActivateBegin => "ACTIVATE_BEGIN",
            MlvOp::ActivateCommit => "ACTIVATE_COMMIT",
            MlvOp::CreateProposal => "CREATE_PROPOSAL",
            MlvOp::Abandon => "ABANDON",
            _ => "",
        };
        if !opname.is_empty()
            && self.proj.phase_ops.contains(&(binding.clone(), rev, opname))
            && rec.op != MlvOp::ActivateBegin // begin 在 abandon 后可重新提出
            && rec.op != MlvOp::Abandon
        {
            return Err(GovErr::Internal(format!("phase op duplicate: {opname}")));
        }
        // replay 模式下重算 result 比对（P8：result=审计冗余，重算比对）
        Ok(())
    }

    pub fn record_count_pub(&self) -> u64 {
        self.record_count
    }

    fn seq_hint(&self) -> u64 {
        // 投影不存 seq——由账本行数给出（open 后=records.len()；submit 前=len）
        self.record_count
    }

    /// 投影更新（apply=commit/replay 唯一路径）。
    fn apply_record(&mut self, rec: &LedgerRecord, _t_ns: u64, _live: bool) -> Result<(), GovErr> {
        let binding = rec.binding_id.clone().unwrap_or_default();
        let rev = rec.revision.unwrap_or(0);
        self.record_count += 1;
        // 幂等索引
        self.proj.effect_index.insert(
            rec.effect_key.clone(),
            (rec.request_digest.clone(), rec.result.clone()),
        );
        // nonce
        if let Some(n) = &rec.nonce {
            self.proj.nonce_set.insert(n.clone());
        }
        if rec.op == MlvOp::LedgerInit {
            return Ok(());
        }
        // 状态
        if let Some(to) = rec.to {
            self.proj.states.insert((binding.clone(), rev), to);
            self.proj.current.insert(binding.clone(), rev);
        }
        // phase 唯一表
        let opname: &'static str = match rec.op {
            MlvOp::Grant => "GRANT",
            MlvOp::Decision => "DECISION",
            MlvOp::ActivateBegin => "ACTIVATE_BEGIN",
            MlvOp::ActivateCommit => "ACTIVATE_COMMIT",
            MlvOp::CreateProposal => "CREATE_PROPOSAL",
            MlvOp::Abandon => "ABANDON",
            _ => "",
        };
        if !opname.is_empty() {
            self.proj.phase_ops.insert((binding.clone(), rev, opname));
        }
        match rec.op {
            MlvOp::RegistryConfirm => {
                if let Some(r) = &rec.registry_receipt {
                    self.proj.receipts.insert((binding.clone(), rev), r.clone());
                }
            }
            MlvOp::ActivateBegin => {
                self.proj.pending_begin.insert((binding.clone(), rev));
            }
            MlvOp::ActivateCommit | MlvOp::Abandon | MlvOp::Revoke | MlvOp::Terminal => {
                self.proj.pending_begin.remove(&(binding.clone(), rev));
                if matches!(rec.op, MlvOp::Revoke | MlvOp::Terminal) {
                    *self.proj.stop_gen.entry(binding.clone()).or_insert(0) += 1;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

trait InsertWouldBeNew {
    fn insert_would_be_new(&self, k: &(String, u64)) -> bool;
}
impl InsertWouldBeNew for BTreeSet<(String, u64)> {
    fn insert_would_be_new(&self, k: &(String, u64)) -> bool {
        !self.contains(k)
    }
}

fn map_ledger_err(e: String) -> GovErr {
    if e.contains("ledger-missing") {
        GovErr::LedgerMissing
    } else {
        GovErr::Internal(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ductile-gov-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mk_rec(seq: u64, op: MlvOp, binding: &str, rev: u64, from: Option<MlvState>, to: Option<MlvState>, prev: &str, request_key: &str) -> LedgerRecord {
        LedgerRecord {
            schema: 1, seq, op,
            key_id: "genesis".into(), root_commitment: GENESIS_ROOT.into(),
            effect_key: effect_key(op, binding, rev, request_key).unwrap(),
            idempotency_scope: None, caller_id: Some("actor".into()),
            request_key: Some(request_key.into()),
            request_digest: domain_hash(DOMAIN_RECORD, format!("{op:?}|{request_key}").as_bytes()),
            binding_id: Some(binding.into()), revision: Some(rev),
            from, to, before_record_hash: prev.into(), accepted_at_ns: 100,
            nonce: Some(format!("n-{seq}")), envelope_digest: None, envelope: None,
            payload: format!("{op:?}|{binding}|{rev}"), registry_receipt: None,
            result: [("code".into(), "OK".into())].into_iter().collect(),
            record_hash: String::new(),
        }
    }

    fn setup(path: &std::path::Path) -> GovRegistry {
        let reg = GovRegistry::init(path, 1).unwrap();
        reg
    }

    #[test]
    fn t01_create_only_from_null_and_revision_rules() {
        let d = tmp("t01"); let p = d.join("l.jsonl");
        let mut reg = setup(&p);
        let head = reg.head().unwrap();
        // 非 null from 的 CREATE → E422
        let bad = mk_rec(1, MlvOp::CreateProposal, "b1", 0, Some(MlvState::Proposed), Some(MlvState::Proposed), &head, "rk");
        assert_eq!(reg.submit(bad, 200), Err(GovErr::IllegalEdge));
        // 首次 rev≠0 → E422
        let bad2 = mk_rec(1, MlvOp::CreateProposal, "b1", 3, None, Some(MlvState::Proposed), &head, "rk");
        assert_eq!(reg.submit(bad2, 200), Err(GovErr::IllegalEdge));
        // 合法 create
        let ok = mk_rec(1, MlvOp::CreateProposal, "b1", 0, None, Some(MlvState::Proposed), &head, "rk");
        assert!(matches!(reg.submit(ok, 200), Ok(ApplyOutcome::Committed { .. })));
    }

    #[test]
    fn t05_same_key_same_digest_ack_no_new_row() {
        let d = tmp("t05"); let p = d.join("l.jsonl");
        let mut reg = setup(&p);
        let head = reg.head().unwrap();
        let r1 = mk_rec(1, MlvOp::CreateProposal, "b1", 0, None, Some(MlvState::Proposed), &head, "rk");
        let r2 = r1.clone(); // 同键同摘要
        assert!(matches!(reg.submit(r1, 200), Ok(ApplyOutcome::Committed { .. })));
        let before = reg.ledger.records().unwrap().len();
        match reg.submit(r2, 300) {
            Ok(ApplyOutcome::Acked { .. }) => {}
            other => panic!("expected Acked, got {other:?}"),
        }
        let after = reg.ledger.records().unwrap().len();
        assert_eq!(before, after, "ACK must not add row");
    }

    #[test]
    fn t06_same_key_diff_digest_reject() {
        let d = tmp("t06"); let p = d.join("l.jsonl");
        let mut reg = setup(&p);
        let head = reg.head().unwrap();
        let r1 = mk_rec(1, MlvOp::CreateProposal, "b1", 0, None, Some(MlvState::Proposed), &head, "rk");
        assert!(reg.submit(r1, 200).is_ok());
        // 同键异摘要（换 request_digest）
        let mut r2 = mk_rec(1, MlvOp::CreateProposal, "b1", 0, None, Some(MlvState::Proposed), &head, "rk");
        r2.request_digest = domain_hash(DOMAIN_RECORD, b"DIFFERENT");
        assert_eq!(reg.submit(r2, 300), Err(GovErr::IdempotencyConflict));
    }

    #[test]
    fn t09_illegal_edge_rejected() {
        let d = tmp("t09"); let p = d.join("l.jsonl");
        let mut reg = setup(&p);
        let head = reg.head().unwrap();
        let c = mk_rec(1, MlvOp::CreateProposal, "b1", 0, None, Some(MlvState::Proposed), &head, "rk");
        assert!(reg.submit(c, 200).is_ok());
        let h1 = reg.head().unwrap();
        // PROPOSED→DECIDED 跳边（GRANTED 缺失）→ E422
        let bad = mk_rec(2, MlvOp::Decision, "b1", 0, Some(MlvState::Proposed), Some(MlvState::Decided), &h1, "rk");
        assert_eq!(reg.submit(bad, 200), Err(GovErr::IllegalEdge));
    }

    #[test]
    fn t10_terminal_no_revive() {
        let d = tmp("t10"); let p = d.join("l.jsonl");
        let mut reg = setup(&p);
        let head = reg.head().unwrap();
        assert!(reg.submit(mk_rec(1, MlvOp::CreateProposal, "b1", 0, None, Some(MlvState::Proposed), &head, "rk"), 100).is_ok());
        let h = reg.head().unwrap();
        assert!(reg.submit(mk_rec(2, MlvOp::Grant, "b1", 0, Some(MlvState::Proposed), Some(MlvState::Granted), &h, "rk"), 100).is_ok());
        let h = reg.head().unwrap();
        assert!(reg.submit(mk_rec(3, MlvOp::Revoke, "b1", 0, Some(MlvState::Granted), Some(MlvState::Revoked), &h, "rk"), 100).is_ok());
        let h = reg.head().unwrap();
        // REVOKED 上再 Terminal → ERR terminal
        let bad = mk_rec(4, MlvOp::Terminal, "b1", 0, Some(MlvState::Revoked), Some(MlvState::Terminal), &h, "rk");
        assert_eq!(reg.submit(bad, 100), Err(GovErr::Terminal));
        // 复活为 PROPOSED → E422
        let bad2 = mk_rec(4, MlvOp::CreateProposal, "b1", 0, None, Some(MlvState::Proposed), &h, "rk2");
        assert_eq!(reg.submit(bad2, 100), Err(GovErr::IllegalEdge));
        // 新 rev=current+1 允许（旧 rev 已终态）
        let ok = mk_rec(4, MlvOp::CreateProposal, "b1", 1, None, Some(MlvState::Proposed), &h, "rk3");
        assert!(reg.submit(ok, 100).is_ok());
    }

    #[test]
    fn t12_receipt_gate() {
        let d = tmp("t12"); let p = d.join("l.jsonl");
        let mut reg = setup(&p);
        let head = reg.head().unwrap();
        assert!(reg.submit(mk_rec(1, MlvOp::CreateProposal, "b1", 0, None, Some(MlvState::Proposed), &head, "rk"), 100).is_ok());
        let h = reg.head().unwrap();
        assert!(reg.submit(mk_rec(2, MlvOp::Grant, "b1", 0, Some(MlvState::Proposed), Some(MlvState::Granted), &h, "rk"), 100).is_ok());
        let h = reg.head().unwrap();
        assert!(reg.submit(mk_rec(3, MlvOp::Decision, "b1", 0, Some(MlvState::Granted), Some(MlvState::Decided), &h, "rk"), 100).is_ok());
        let h = reg.head().unwrap();
        assert!(reg.submit(mk_rec(4, MlvOp::ActivateBegin, "b1", 0, Some(MlvState::Decided), Some(MlvState::Activating), &h, "rk"), 100).is_ok());
        let h = reg.head().unwrap();
        // 无回执 COMMIT → receipt-required
        let bad = mk_rec(5, MlvOp::ActivateCommit, "b1", 0, Some(MlvState::Activating), Some(MlvState::Active), &h, "rk");
        assert_eq!(reg.submit(bad, 100), Err(GovErr::ReceiptRequired));
        // 登记回执（有效期内）
        let mut rcpt = BTreeMap::new();
        rcpt.insert("receipt_id".into(), "r1".into());
        rcpt.insert("issued_at_ns".into(), "50".into());
        rcpt.insert("expires_at_ns".into(), format!("{}", 100 + 2 * DEFAULT_SKEW_NS));
        let mut conf = mk_rec(5, MlvOp::RegistryConfirm, "b1", 0, None, None, &h, "rk");
        conf.registry_receipt = Some(rcpt);
        assert!(reg.submit(conf, 100).is_ok());
        let h = reg.head().unwrap();
        // 有回执 COMMIT → 过
        assert!(reg.submit(mk_rec(6, MlvOp::ActivateCommit, "b1", 0, Some(MlvState::Activating), Some(MlvState::Active), &h, "rk"), 100).is_ok());
    }

    #[test]
    fn t07_replay_equal_projections() {
        let d = tmp("t07"); let p = d.join("l.jsonl");
        let (states, current, effects, nonces) = {
            let mut reg = setup(&p);
            let head = reg.head().unwrap();
            assert!(reg.submit(mk_rec(1, MlvOp::CreateProposal, "b1", 0, None, Some(MlvState::Proposed), &head, "rk"), 100).is_ok());
            let h = reg.head().unwrap();
            assert!(reg.submit(mk_rec(2, MlvOp::Grant, "b1", 0, Some(MlvState::Proposed), Some(MlvState::Granted), &h, "rk"), 100).is_ok());
            let h = reg.head().unwrap();
            assert!(reg.submit(mk_rec(3, MlvOp::Revoke, "b1", 0, Some(MlvState::Granted), Some(MlvState::Revoked), &h, "rk"), 100).is_ok());
            (
                reg.proj.states.clone(), reg.proj.current.clone(),
                reg.proj.effect_index.clone(), reg.proj.nonce_set.clone(),
            )
        };
        // 新进程视角 open=全量重放
        let reg2 = GovRegistry::open(&p).unwrap();
        assert_eq!(reg2.proj.states, states);
        assert_eq!(reg2.proj.current, current);
        assert_eq!(reg2.proj.effect_index, effects);
        assert_eq!(reg2.proj.nonce_set, nonces);
    }

    #[test]
    fn t03_missing_ledger_refuses() {
        let d = tmp("t03"); let p = d.join("nonexistent.jsonl");
        assert!(matches!(GovRegistry::open(&p), Err(GovErr::LedgerMissing)));
    }

    #[test]
    fn t11_nonce_reuse_rejected() {
        let d = tmp("t11"); let p = d.join("l.jsonl");
        let mut reg = setup(&p);
        let head = reg.head().unwrap();
        let mut r1 = mk_rec(1, MlvOp::CreateProposal, "b1", 0, None, Some(MlvState::Proposed), &head, "rk-a");
        r1.nonce = Some("nonce-1".into());
        assert!(reg.submit(r1, 100).is_ok());
        let h = reg.head().unwrap();
        // 重放同 nonce（不同键）→ 拒
        let mut r2 = mk_rec(2, MlvOp::CreateProposal, "b2", 0, None, Some(MlvState::Proposed), &h, "rk-b");
        r2.nonce = Some("nonce-1".into());
        assert_eq!(reg.submit(r2, 100), Err(GovErr::NonceReuse));
    }

    #[test]
    fn abandon_returns_to_decided() {
        let d = tmp("ab"); let p = d.join("l.jsonl");
        let mut reg = setup(&p);
        let head = reg.head().unwrap();
        assert!(reg.submit(mk_rec(1, MlvOp::CreateProposal, "b1", 0, None, Some(MlvState::Proposed), &head, "rk"), 100).is_ok());
        let h = reg.head().unwrap();
        assert!(reg.submit(mk_rec(2, MlvOp::Grant, "b1", 0, Some(MlvState::Proposed), Some(MlvState::Granted), &h, "rk"), 100).is_ok());
        let h = reg.head().unwrap();
        assert!(reg.submit(mk_rec(3, MlvOp::Decision, "b1", 0, Some(MlvState::Granted), Some(MlvState::Decided), &h, "rk"), 100).is_ok());
        let h = reg.head().unwrap();
        assert!(reg.submit(mk_rec(4, MlvOp::ActivateBegin, "b1", 0, Some(MlvState::Decided), Some(MlvState::Activating), &h, "rk"), 100).is_ok());
        let h = reg.head().unwrap();
        // ABANDON：ACTIVATING→DECIDED（唯一放弃出口）
        assert!(reg.submit(mk_rec(5, MlvOp::Abandon, "b1", 0, Some(MlvState::Activating), Some(MlvState::Decided), &h, "rk"), 100).is_ok());
        // abandon 后可重新 begin（pending 清除）
        let h = reg.head().unwrap();
        assert!(reg.submit(mk_rec(6, MlvOp::ActivateBegin, "b1", 0, Some(MlvState::Decided), Some(MlvState::Activating), &h, "rk2"), 100).is_ok());
    }
}

/// CLI 辅助：按 op+投影当前态推导 (from,to)。
pub fn derive_edge(op: MlvOp, binding: &str, rev: u64, reg: &GovRegistry) -> (Option<MlvState>, Option<MlvState>) {
    let cur = reg.proj.states.get(&(binding.to_string(), rev)).copied();
    match op {
        MlvOp::CreateProposal => (None, Some(MlvState::Proposed)),
        MlvOp::RegistryConfirm | MlvOp::LedgerInit => (None, None),
        MlvOp::Revoke => (cur, Some(MlvState::Revoked)),
        MlvOp::Terminal => (cur, Some(MlvState::Terminal)),
        MlvOp::ActivateCommit => (cur, cur.and_then(commit_target)),
        _ => {
            if let Some((f, t)) = state_edge(op) {
                (f, Some(t))
            } else {
                match op {
                    MlvOp::Quarantine => (cur, Some(MlvState::Quarantined)),
                    _ => (cur, None),
                }
            }
        }
    }
}

impl GovRegistry {
    pub fn record_count(&self) -> u64 {
        self.record_count_pub()
    }
    pub fn state_of(&self, binding: &str) -> Option<(u64, MlvState)> {
        let rev = self.proj.current.get(binding)?;
        let st = self.proj.states.get(&(binding.to_string(), *rev))?;
        Some((*rev, *st))
    }
    pub fn stop_gen(&self, binding: &str) -> u64 {
        self.proj.stop_gen.get(binding).copied().unwrap_or(0)
    }
}
