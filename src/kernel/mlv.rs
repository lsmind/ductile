//! Ductile v0.24 治理超图 MLV 记录层 — v3.1（sonet_mlv_redesign_v3.md 冻结版）。
//!
//! 实现外援终审「可施工」版：封闭 14-Op 词表/11 态机（终态 REVOKED/TERMINAL 不可复活）、
//! 23 字段 LedgerRecord、域分隔哈希 D(L,B)、flock FFI、原子提交（tmp→sync→rename→
//! 父目录 sync）、显式 init、全量重放（replay 时钟=accepted_at_ns）、幂等键排除
//! digest、信封四组声明+唯一时间式、回执门禁。
//!
//! **codec 修正**：外援二轮曾判 serde_json 依赖违规，被实施方以「仓库已有依赖」
//! 误证推翻——实际 Cargo.toml 无 serde_json。本模块回归手写封闭 codec（沿 ledger.rs
//! 手写提取器传统）：canonical=固定字节序 23 字段（与 BTreeMap 字节序一致，F 条）、
//! 严格 JSON 微解析器（重复键/未知字段 parse 阶段拒——比 serde 更严，G 条）、
//! 标准转义。依赖维持 std+sha2。

use crate::kernel::hash::sha256_hex;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

// ── 域标签（v3 补丁3 冻结）────────────────────────────────────────

pub const DOMAIN_EFFECT: &str = "MLV/v3/effect";
pub const DOMAIN_RECEIPT: &str = "MLV/v3/receipt";
pub const DOMAIN_RECORD: &str = "MLV/v3/record";
pub const DOMAIN_MAC: &str = "MLV/v3/mac";

/// 域分隔哈希 D(L,B)=SHA256(L ‖ 0x00 ‖ u64be(len B) ‖ B)。
pub fn domain_hash(domain: &str, body: &[u8]) -> String {
    let mut buf = Vec::with_capacity(domain.len() + 1 + 8 + body.len());
    buf.extend_from_slice(domain.as_bytes());
    buf.push(0x00);
    buf.extend_from_slice(&(body.len() as u64).to_be_bytes());
    buf.extend_from_slice(body);
    format!("sha256:{}", sha256_hex(&buf))
}

/// genesis 前驱（首记录 before_record_hash）。
pub const ZERO_HASH: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// genesis 根（HMAC 钥；MLV 编译常量模拟预装根）。
pub const GENESIS_ROOT: &str =
    "ductile-genesis-root-v1-0000000000000000000000000000000000000000";

/// 默认时钟偏差（300s；唯一配置源，v3.1 E 条）。
pub const DEFAULT_SKEW_NS: u64 = 300_000_000_000;

// ── Op 封闭词表（14）──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MlvOp {
    LedgerInit, CreateProposal, Grant, Decision, ActivateBegin, ActivateCommit,
    RegistryConfirm, Abandon, Revoke, Quarantine, Investigate, Repair, Verify, Terminal,
}

pub const MLV_OPS: &[MlvOp] = &[
    MlvOp::LedgerInit, MlvOp::CreateProposal, MlvOp::Grant, MlvOp::Decision,
    MlvOp::ActivateBegin, MlvOp::ActivateCommit, MlvOp::RegistryConfirm,
    MlvOp::Abandon, MlvOp::Revoke, MlvOp::Quarantine, MlvOp::Investigate,
    MlvOp::Repair, MlvOp::Verify, MlvOp::Terminal,
];

impl MlvOp {
    pub fn name(&self) -> &'static str {
        match self {
            MlvOp::LedgerInit => "LEDGER_INIT",
            MlvOp::CreateProposal => "CREATE_PROPOSAL",
            MlvOp::Grant => "GRANT",
            MlvOp::Decision => "DECISION",
            MlvOp::ActivateBegin => "ACTIVATE_BEGIN",
            MlvOp::ActivateCommit => "ACTIVATE_COMMIT",
            MlvOp::RegistryConfirm => "REGISTRY_CONFIRM",
            MlvOp::Abandon => "ABANDON",
            MlvOp::Revoke => "REVOKE",
            MlvOp::Quarantine => "QUARANTINE",
            MlvOp::Investigate => "INVESTIGATE",
            MlvOp::Repair => "REPAIR",
            MlvOp::Verify => "VERIFY",
            MlvOp::Terminal => "TERMINAL",
        }
    }
    pub fn from_name(s: &str) -> Result<Self, String> {
        for op in MLV_OPS {
            if op.name() == s {
                return Ok(*op);
            }
        }
        Err(format!("unknown op: {s}"))
    }
    /// 无状态边（LEDGER_INIT/REGISTRY_CONFIRM）。
    pub fn has_state_edge(&self) -> bool {
        !matches!(self, MlvOp::LedgerInit | MlvOp::RegistryConfirm)
    }
}

// ── 11 态状态机（终态 REVOKED/TERMINAL）───────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MlvState {
    Proposed, Granted, Decided, Activating, Active,
    Quarantined, Investigating, Repairing, Verified, Revoked, Terminal,
}

impl MlvState {
    pub fn name(&self) -> &'static str {
        match self {
            MlvState::Proposed => "PROPOSED",
            MlvState::Granted => "GRANTED",
            MlvState::Decided => "DECIDED",
            MlvState::Activating => "ACTIVATING",
            MlvState::Active => "ACTIVE",
            MlvState::Quarantined => "QUARANTINED",
            MlvState::Investigating => "INVESTIGATING",
            MlvState::Repairing => "REPAIRING",
            MlvState::Verified => "VERIFIED",
            MlvState::Revoked => "REVOKED",
            MlvState::Terminal => "TERMINAL",
        }
    }
    pub fn from_name(s: &str) -> Result<Self, String> {
        Ok(match s {
            "PROPOSED" => MlvState::Proposed,
            "GRANTED" => MlvState::Granted,
            "DECIDED" => MlvState::Decided,
            "ACTIVATING" => MlvState::Activating,
            "ACTIVE" => MlvState::Active,
            "QUARANTINED" => MlvState::Quarantined,
            "INVESTIGATING" => MlvState::Investigating,
            "REPAIRING" => MlvState::Repairing,
            "VERIFIED" => MlvState::Verified,
            "REVOKED" => MlvState::Revoked,
            "TERMINAL" => MlvState::Terminal,
            _ => return Err(format!("unknown state: {s}")),
        })
    }
    pub fn is_terminal(&self) -> bool {
        matches!(self, MlvState::Revoked | MlvState::Terminal)
    }
}

/// QUARANTINE 源态精化（v3.1 A②：{ACTIVE, VERIFIED}）。
pub const QUARANTINE_SOURCES: &[MlvState] = &[MlvState::Active, MlvState::Verified];

/// 逐 Op 边表（静态边）。REVOKE/TERMINAL 源态动态（任意非终态）。
/// 返回 (Option<from>, to)；LEDGER_INIT/REGISTRY_CONFIRM 无边。
pub fn state_edge(op: MlvOp) -> Option<(Option<MlvState>, MlvState)> {
    match op {
        MlvOp::CreateProposal => Some((None, MlvState::Proposed)),
        MlvOp::Grant => Some((Some(MlvState::Proposed), MlvState::Granted)),
        MlvOp::Decision => Some((Some(MlvState::Granted), MlvState::Decided)),
        MlvOp::ActivateBegin => Some((Some(MlvState::Decided), MlvState::Activating)),
        MlvOp::Abandon => Some((Some(MlvState::Activating), MlvState::Decided)),
        MlvOp::Quarantine => None,  // 源态枚举 {ACTIVE,VERIFIED}，动态查
        MlvOp::Investigate => Some((Some(MlvState::Quarantined), MlvState::Investigating)),
        MlvOp::Repair => Some((Some(MlvState::Investigating), MlvState::Repairing)),
        MlvOp::Verify => Some((Some(MlvState::Repairing), MlvState::Verified)),
        _ => None, // LEDGER_INIT/RegistryConfirm/Revoke/Terminal/ActivateCommit 特判
    }
}

/// ACTIVATE_COMMIT 双径：ACTIVATING→ACTIVE（需回执）／VERIFIED→ACTIVE（修复链回径）。
pub fn commit_target(from: MlvState) -> Option<MlvState> {
    match from {
        MlvState::Activating | MlvState::Verified => Some(MlvState::Active),
        _ => None,
    }
}

// ── 23 字段 LedgerRecord ──────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct LedgerRecord {
    pub schema: u32,
    pub seq: u64,
    pub op: MlvOp,
    pub key_id: String,
    pub root_commitment: String,
    pub effect_key: String,
    pub idempotency_scope: Option<String>,
    pub caller_id: Option<String>,
    pub request_key: Option<String>,
    pub request_digest: String,
    pub binding_id: Option<String>,
    pub revision: Option<u64>,
    pub from: Option<MlvState>,
    pub to: Option<MlvState>,
    pub before_record_hash: String,
    pub accepted_at_ns: u64,
    pub nonce: Option<String>,
    pub envelope_digest: Option<String>,
    pub envelope: Option<BTreeMap<String, String>>,
    pub payload: String,
    pub registry_receipt: Option<BTreeMap<String, String>>,
    pub result: BTreeMap<String, String>,
    pub record_hash: String,
}

/// canonical 字段序（UTF-8 字节序=F 条 BTreeMap 序）。
const CANON_ORDER: &[&str] = &[
    "accepted_at_ns", "before_record_hash", "binding_id", "caller_id", "effect_key",
    "envelope", "envelope_digest", "from", "idempotency_scope", "key_id", "nonce",
    "op", "payload", "record_hash", "registry_receipt", "request_digest",
    "request_key", "result", "revision", "root_commitment", "schema", "seq", "to",
];

impl LedgerRecord {
    fn field_json(&self, name: &str, hash_override: &str) -> Option<String> {
        Some(match name {
            "accepted_at_ns" => self.accepted_at_ns.to_string(),
            "before_record_hash" => json_esc(&self.before_record_hash),
            "binding_id" => json_esc_opt(&self.binding_id),
            "caller_id" => json_esc_opt(&self.caller_id),
            "effect_key" => json_esc(&self.effect_key),
            "envelope" => json_esc_map(&self.envelope),
            "envelope_digest" => json_esc_opt(&self.envelope_digest),
            "from" => json_esc_opt_state(&self.from),
            "idempotency_scope" => json_esc_opt(&self.idempotency_scope),
            "key_id" => json_esc(&self.key_id),
            "nonce" => json_esc_opt(&self.nonce),
            "op" => json_esc(self.op.name()),
            "payload" => json_esc(&self.payload),
            "record_hash" => json_esc(hash_override),
            "registry_receipt" => json_esc_map(&self.registry_receipt),
            "request_digest" => json_esc(&self.request_digest),
            "request_key" => json_esc_opt(&self.request_key),
            "result" => json_esc_map(&Some(self.result.clone())),
            "revision" => match self.revision { Some(r) => r.to_string(), None => "null".into() },
            "root_commitment" => json_esc(&self.root_commitment),
            "schema" => self.schema.to_string(),
            "seq" => self.seq.to_string(),
            "to" => json_esc_opt_state(&self.to),
            _ => return None,
        })
    }

    /// canonical JSON（无空白；键=字节序；record_hash 以空串占位=排除自身）。
    pub fn canonical_without_hash(&self) -> String {
        let fields: Vec<String> = CANON_ORDER
            .iter()
            .filter_map(|k| Some((k, self.field_json(k, "")?)))
            .map(|(k, v)| format!("\"{k}\":{v}"))
            .collect();
        format!("{{{}}}", fields.join(","))
    }

    pub fn compute_hash(&self) -> String {
        domain_hash(DOMAIN_RECORD, self.canonical_without_hash().as_bytes())
    }

    /// 帧 = u64be(N) ‖ J ‖ 0x0A（N=len(J)，仅定界不入哈希）。
    pub fn frame(&self) -> Vec<u8> {
        let fields: Vec<String> = CANON_ORDER
            .iter()
            .filter_map(|k| Some((k, self.field_json(k, &self.record_hash)?)))
            .map(|(k, v)| format!("\"{k}\":{v}"))
            .collect();
        let j = format!("{{{}}}", fields.join(","));
        let mut f = Vec::with_capacity(8 + j.len() + 1);
        f.extend_from_slice(&(j.len() as u64).to_be_bytes());
        f.extend_from_slice(j.as_bytes());
        f.push(0x0A);
        f
    }

    pub fn to_line(&self) -> String {
        String::from_utf8(self.frame()).unwrap()
    }

    pub fn decode_frame(frame: &[u8]) -> Result<Self, String> {
        if frame.len() < 9 || *frame.last().unwrap() != 0x0A {
            return Err("bad frame tail".into());
        }
        let n = u64::from_be_bytes(frame[..8].try_into().unwrap()) as usize;
        if frame.len() != 8 + n + 1 {
            return Err("bad frame length".into());
        }
        parse_record_json(std::str::from_utf8(&frame[8..8 + n]).map_err(|e| e.to_string())?)
    }
}

// 手写 codec：转义 + 微解析器（封闭类型：string/u64/null/扁平 string map）

fn json_esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
fn json_esc_opt(v: &Option<String>) -> String {
    match v { Some(s) => json_esc(s), None => "null".into() }
}
fn json_esc_opt_state(v: &Option<MlvState>) -> String {
    match v { Some(s) => json_esc(s.name()), None => "null".into() }
}
fn json_esc_map(v: &Option<BTreeMap<String, String>>) -> String {
    match v {
        None => "null".into(),
        Some(m) => {
            let inner: Vec<String> = m.iter()
                .map(|(k, val)| format!("{}:{}", json_esc(k), json_esc(val)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
    }
}

/// 封闭 JSON 值（本模块全部字段类型）。
#[derive(Debug, Clone, PartialEq)]
pub enum JVal {
    S(String),
    N(u64),
    Null,
    Obj(BTreeMap<String, String>),
}

/// 严格微解析器：单层对象+扁平子对象；重复键 parse 阶段拒（G 条）。
pub fn parse_json_closed(j: &str) -> Result<BTreeMap<String, JVal>, String> {
    let b: Vec<char> = j.chars().collect();
    let mut i = 0usize;
    let out = parse_object(&b, &mut i, 0)?;
    if i != b.len() {
        return Err(format!("trailing bytes at {i}"));
    }
    Ok(out)
}

fn parse_object(b: &[char], i: &mut usize, depth: u32) -> Result<BTreeMap<String, JVal>, String> {
    if depth > 2 {
        return Err("nesting too deep (max 2)".into());
    }
    let mut m = BTreeMap::new();
    skip_ws(b, i);
    if *i >= b.len() || b[*i] != '{' {
        return Err("expected {".into());
    }
    *i += 1;
    skip_ws(b, i);
    if *i < b.len() && b[*i] == '}' {
        *i += 1;
        return Ok(m);
    }
    loop {
        skip_ws(b, i);
        let key = parse_string(b, i)?;
        skip_ws(b, i);
        if *i >= b.len() || b[*i] != ':' {
            return Err("expected :".into());
        }
        *i += 1;
        skip_ws(b, i);
        let val = if *i < b.len() && b[*i] == '{' {
            JVal::Obj(parse_flat_object(b, i)?)
        } else if *i < b.len() && b[*i] == '"' {
            JVal::S(parse_string(b, i)?)
        } else {
            // 数值或 null
            let start = *i;
            while *i < b.len() && b[*i] != ',' && b[*i] != '}' && !b[*i].is_whitespace() {
                *i += 1;
            }
            let tok: String = b[start..*i].iter().collect();
            if tok == "null" {
                JVal::Null
            } else {
                JVal::N(tok.parse::<u64>().map_err(|_| format!("bad number: {tok}"))?)
            }
        };
        if m.contains_key(&key) {
            return Err(format!("duplicate key: {key}"));
        }
        m.insert(key, val);
        skip_ws(b, i);
        if *i >= b.len() {
            return Err("unexpected end".into());
        }
        match b[*i] {
            ',' => { *i += 1; }
            '}' => { *i += 1; return Ok(m); }
            c => return Err(format!("unexpected char {c}")),
        }
    }
}

fn parse_flat_object(b: &[char], i: &mut usize) -> Result<BTreeMap<String, String>, String> {
    let mut m = BTreeMap::new();
    if *i >= b.len() || b[*i] != '{' {
        return Err("expected {".into());
    }
    *i += 1;
    skip_ws(b, i);
    if *i < b.len() && b[*i] == '}' {
        *i += 1;
        return Ok(m);
    }
    loop {
        skip_ws(b, i);
        let key = parse_string(b, i)?;
        skip_ws(b, i);
        if *i >= b.len() || b[*i] != ':' {
            return Err("expected :".into());
        }
        *i += 1;
        skip_ws(b, i);
        if *i >= b.len() || b[*i] != '"' {
            return Err("flat object value must be string".into());
        }
        let val = parse_string(b, i)?;
        if m.contains_key(&key) {
            return Err(format!("duplicate key: {key}"));
        }
        m.insert(key, val);
        skip_ws(b, i);
        match b.get(*i) {
            Some(',') => { *i += 1; }
            Some('}') => { *i += 1; return Ok(m); }
            _ => return Err("expected , or }".into()),
        }
    }
}

fn parse_string(b: &[char], i: &mut usize) -> Result<String, String> {
    if *i >= b.len() || b[*i] != '"' {
        return Err("expected string".into());
    }
    *i += 1;
    let mut out = String::new();
    while *i < b.len() {
        match b[*i] {
            '"' => { *i += 1; return Ok(out); }
            '\\' => {
                *i += 1;
                match b.get(*i) {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some('b') => out.push('\u{0008}'),
                    Some('f') => out.push('\u{000C}'),
                    Some('u') => {
                        let hex: String = b[*i + 1..*i + 5].iter().collect();
                        let cp = u32::from_str_radix(&hex, 16)
                            .map_err(|_| format!("bad \\u escape: {hex}"))?;
                        out.push(char::from_u32(cp).ok_or("bad codepoint")?);
                        *i += 4;
                    }
                    _ => return Err("bad escape".into()),
                }
                *i += 1;
            }
            c => { out.push(c); *i += 1; }
        }
    }
    Err("unterminated string".into())
}

fn skip_ws(b: &[char], i: &mut usize) {
    while *i < b.len() && b[*i].is_whitespace() {
        *i += 1;
    }
}

/// 严格解析 LedgerRecord（23 字段全到场、无未知、类型封闭）。
pub fn parse_record_json(j: &str) -> Result<LedgerRecord, String> {
    let m = parse_json_closed(j)?;
    if m.len() != CANON_ORDER.len() {
        return Err(format!("field count {} != 23", m.len()));
    }
    for k in m.keys() {
        if !CANON_ORDER.contains(&k.as_str()) {
            return Err(format!("unknown field: {k}"));
        }
    }
    let gs = |k: &str| -> Result<String, String> {
        match m.get(k) {
            Some(JVal::S(s)) => Ok(s.clone()),
            _ => Err(format!("field {k} must be string")),
        }
    };
    let gn = |k: &str| -> Result<u64, String> {
        match m.get(k) {
            Some(JVal::N(n)) => Ok(*n),
            _ => Err(format!("field {k} must be number")),
        }
    };
    let gopt = |k: &str| -> Result<Option<String>, String> {
        match m.get(k) {
            Some(JVal::Null) | None => Ok(None),
            Some(JVal::S(s)) => Ok(Some(s.clone())),
            _ => Err(format!("field {k} must be string or null")),
        }
    };
    let gopt_state = |k: &str| -> Result<Option<MlvState>, String> {
        match m.get(k) {
            Some(JVal::Null) | None => Ok(None),
            Some(JVal::S(s)) => MlvState::from_name(s).map(Some),
            _ => Err(format!("field {k} must be state string or null")),
        }
    };
    let gopt_map = |k: &str| -> Result<Option<BTreeMap<String, String>>, String> {
        match m.get(k) {
            Some(JVal::Null) | None => Ok(None),
            Some(JVal::Obj(o)) => Ok(Some(o.clone())),
            _ => Err(format!("field {k} must be flat object or null")),
        }
    };
    Ok(LedgerRecord {
        accepted_at_ns: gn("accepted_at_ns")?,
        before_record_hash: gs("before_record_hash")?,
        binding_id: gopt("binding_id")?,
        caller_id: gopt("caller_id")?,
        effect_key: gs("effect_key")?,
        envelope: gopt_map("envelope")?,
        envelope_digest: gopt("envelope_digest")?,
        from: gopt_state("from")?,
        idempotency_scope: gopt("idempotency_scope")?,
        key_id: gs("key_id")?,
        nonce: gopt("nonce")?,
        op: MlvOp::from_name(&gs("op")?)?,
        payload: gs("payload")?,
        record_hash: gs("record_hash")?,
        registry_receipt: gopt_map("registry_receipt")?,
        request_digest: gs("request_digest")?,
        request_key: gopt("request_key")?,
        result: gopt_map("result")?.ok_or("result must be present")?,
        revision: match m.get("revision") {
            Some(JVal::Null) | None => None,
            Some(JVal::N(n)) => Some(*n),
            _ => return Err("revision must be number or null".into()),
        },
        root_commitment: gs("root_commitment")?,
        schema: gn("schema")? as u32,
        seq: gn("seq")?,
        to: gopt_state("to")?,
    })
}

// ── 幂等键（v3 补丁5：排除 request_digest）────────────────────────

/// effect_key=SHA256(effect域|op|binding|rev|request_key)，0x1f 分隔；字段含 0x1f 拒。
pub fn effect_key(op: MlvOp, binding_id: &str, revision: u64, request_key: &str) -> Result<String, String> {
    for s in [op.name(), binding_id, &revision.to_string(), request_key] {
        if s.contains('\u{1f}') {
            return Err("0x1f forbidden in effect key fields".into());
        }
    }
    let mut buf = Vec::new();
    buf.extend_from_slice(DOMAIN_EFFECT.as_bytes());
    buf.push(0x1f);
    buf.extend_from_slice(op.name().as_bytes());
    buf.push(0x1f);
    buf.extend_from_slice(binding_id.as_bytes());
    buf.push(0x1f);
    buf.extend_from_slice(revision.to_string().as_bytes());
    buf.push(0x1f);
    buf.extend_from_slice(request_key.as_bytes());
    Ok(format!("sha256:{}", sha256_hex(&buf)))
}

/// LEDGER_INIT 固定常量键。
pub const INIT_EFFECT_KEY: &str = "MLV-INIT-0000-fixed-constant-key";

// ── 信封（v3.1 E：四组声明+唯一时间式+mac）────────────────────────

/// 唯一时间式：(issued-skew ≤ t) ∧ (t < expires+skew)。
pub fn envelope_time_ok(issued_at_ns: u64, expires_at_ns: u64, t_ns: u64, skew_ns: u64) -> bool {
    let lower = issued_at_ns.saturating_sub(skew_ns);
    let upper = expires_at_ns.saturating_add(skew_ns);
    t_ns >= lower && t_ns < upper
}

/// 受保护声明全集（15 声明+mac）。
pub const ENVELOPE_CLAIMS: &[&str] = &[
    "issuer", "tenant", "subject", "key_id",
    "binding_id", "revision", "op", "request_key", "request_digest",
    "issued_at_ns", "expires_at_ns",
    "contract_digest", "grant_digest", "audience", "root_commitment", "mac",
];

pub fn envelope_mac(env: &BTreeMap<String, String>) -> Result<String, String> {
    const REQ: &[&str] = &[
        "issuer", "tenant", "subject", "key_id",
        "binding_id", "revision", "op", "request_key", "request_digest",
        "issued_at_ns", "expires_at_ns",
        "contract_digest", "grant_digest", "audience", "root_commitment",
    ];
    for k in REQ {
        if !env.contains_key(*k) {
            return Err(format!("envelope missing claim: {k}"));
        }
    }
    // mac=D(mac域, 根 ‖ 除 mac 外全声明按字节序)
    let mut parts = Vec::new();
    for (k, v) in env {
        if k == "mac" { continue; }
        parts.push(format!("\"{k}\":\"{v}\""));
    }
    let body = format!("{}|{}", GENESIS_ROOT, parts.join(","));
    Ok(domain_hash(DOMAIN_MAC, body.as_bytes()))
}

pub fn verify_envelope_mac(env: &BTreeMap<String, String>) -> Result<(), String> {
    let got = env.get("mac").ok_or("envelope missing mac")?;
    let want = envelope_mac(env)?;
    if got != &want {
        return Err("envelope mac mismatch".into());
    }
    Ok(())
}

// ── flock FFI（Linux；EINTR 重试；cfg(unix)）──────────────────────

#[cfg(unix)]
pub struct MlvLock {
    file: Option<std::fs::File>,
    held_key: u64,
}

#[cfg(unix)]
impl std::fmt::Debug for MlvLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MlvLock(<held>)")
    }
}

#[cfg(unix)]
impl MlvLock {
    /// 阻塞 EX 锁；.lock sidecar 独立 inode 永不 rename。
    /// 同进程重入（同 inode 二次 acquire）：flock 语义=进程级，二次 LOCK_EX
    /// 自锁死等 → 全局已持锁表拦截，返回 Reentrant（不重复上锁，Drop 时由
    /// 首个持有者释放）。
    pub fn acquire(lock_path: &Path) -> Result<Self, String> {
        use std::os::unix::io::AsRawFd;
        use std::sync::Mutex;
        static HELD: Mutex<Option<std::collections::BTreeSet<u64>>> = Mutex::new(None);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|e| format!("open lock {}: {e}", lock_path.display()))?;
        // inode 唯一键（dev+ino）
        use std::os::unix::fs::MetadataExt;
        let md = file.metadata().map_err(|e| e.to_string())?;
        let inode_key = md.dev() ^ md.ino();
        {
            let mut held = HELD.lock().unwrap();
            let set = held.get_or_insert_with(Default::default);
            if set.contains(&inode_key) {
                return Ok(MlvLock { file: None, held_key: inode_key });
            }
        }
        let fd = file.as_raw_fd();
        loop {
            let r = unsafe { flock(fd, LOCK_EX) };
            if r == 0 {
                let mut held = HELD.lock().unwrap();
                held.get_or_insert_with(Default::default).insert(inode_key);
                return Ok(MlvLock { file: Some(file), held_key: inode_key });
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(4) {
                continue; // EINTR
            }
            return Err(format!("flock: {err}"));
        }
    }
}

#[cfg(unix)]
impl Drop for MlvLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        use std::sync::Mutex;
        static HELD: Mutex<Option<std::collections::BTreeSet<u64>>> = Mutex::new(None);
        if let Some(f) = &self.file {
            let _ = unsafe { flock(f.as_raw_fd(), LOCK_UN) };
            let mut held = HELD.lock().unwrap();
            if let Some(set) = held.as_mut() {
                set.remove(&self.held_key);
            }
        }
        // 重入句柄（file=None）：不释放——由首个持有者负责
    }
}

#[cfg(unix)]
extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}
#[cfg(unix)]
const LOCK_EX: i32 = 2;
#[cfg(unix)]
const LOCK_UN: i32 = 8;

// ── 账本（init/append/replay/verify；原子提交）────────────────────

#[derive(Debug)]
pub struct MlvLedger {
    pub path: PathBuf,
    pub lock_path: PathBuf,
    lock: Option<MlvLock>,
    tmp_counter: u64,
}

impl MlvLedger {
    /// 打开既有账本（缺失=Err ledger-missing）；全程持 EX 锁。
    pub fn open(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Err("ledger-missing".into());
        }
        let lock_path = lock_path_of(path);
        let lock = MlvLock::acquire(&lock_path)?;
        Ok(MlvLedger { path: path.to_path_buf(), lock_path, lock: Some(lock), tmp_counter: 0 })
    }

    /// 显式 init：唯一可建账本的入口；LEDGER_INIT 首行；re-init 必拒。
    pub fn init(path: &Path, accepted_at_ns: u64) -> Result<(Self, String), String> {
        if path.exists() {
            return Err("ledger already initialized (re-init rejected)".into());
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create dir: {e}"))?;
            }
        }
        let lock_path = lock_path_of(path);
        let lock = MlvLock::acquire(&lock_path)?;
        let mut rec = LedgerRecord {
            schema: 1, seq: 0, op: MlvOp::LedgerInit,
            key_id: "genesis".into(), root_commitment: GENESIS_ROOT.into(),
            effect_key: INIT_EFFECT_KEY.into(),
            idempotency_scope: None, caller_id: None, request_key: None,
            request_digest: domain_hash(DOMAIN_RECORD, format!("init|{accepted_at_ns}").as_bytes()),
            binding_id: None, revision: None, from: None, to: None,
            before_record_hash: ZERO_HASH.into(), accepted_at_ns,
            nonce: None, envelope_digest: None, envelope: None,
            payload: format!("init|{accepted_at_ns}"),
            registry_receipt: None,
            result: [("code".to_string(), "OK".to_string())].into_iter().collect(),
            record_hash: String::new(),
        };
        rec.record_hash = rec.compute_hash();
        let mut led = MlvLedger {
            path: path.to_path_buf(), lock_path, lock: Some(lock), tmp_counter: 0,
        };
        led.commit_frame(&rec)?;
        Ok((led, rec.record_hash.clone()))
    }

    pub fn records(&self) -> Result<Vec<LedgerRecord>, String> {
        let data = std::fs::read(&self.path).map_err(|e| format!("read ledger: {e}"))?;
        decode_frames(&data)
    }

    /// 原子追加（返回 record_hash）。
    pub fn append(&mut self, rec: LedgerRecord) -> Result<String, String> {
        let mut rec = rec;
        rec.record_hash = rec.compute_hash();
        self.commit_frame(&rec)?;
        Ok(rec.record_hash.clone())
    }

    /// 八步固定序：复制旧+新 → tmp(create_new) → write_all → sync → rename → 父目录 sync。
    fn commit_frame(&mut self, rec: &LedgerRecord) -> Result<(), String> {
        let old = std::fs::read(&self.path).unwrap_or_default();
        let mut new = old;
        new.extend_from_slice(&rec.frame());
        self.tmp_counter += 1;
        let tmp = self.path.with_file_name(format!(
            "{}.tmp.{}.{}",
            self.path.file_name().unwrap().to_string_lossy(),
            std::process::id(),
            self.tmp_counter
        ));
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| format!("create tmp: {e}"))?;
        f.write_all(&new).map_err(|e| format!("write tmp: {e}"))?;
        f.sync_all().map_err(|e| format!("sync tmp: {e}"))?;
        drop(f);
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("rename: {e}"))?;
        if let Some(parent) = self.path.parent() {
            if let Ok(d) = std::fs::File::open(parent) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    }

    /// 当前 head（空账本=ZERO_HASH；不验链——验链走 replay）。
    pub fn head(&self) -> Result<String, String> {
        let recs = self.records()?;
        Ok(recs.last().map(|r| r.record_hash.clone()).unwrap_or_else(|| ZERO_HASH.to_string()))
    }
}

fn lock_path_of(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    path.with_file_name(format!("{name}.lock"))
}

pub fn decode_frames(data: &[u8]) -> Result<Vec<LedgerRecord>, String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        if data.len() - i < 9 {
            return Err("truncated frame header".into());
        }
        let n = u64::from_be_bytes(data[i..i + 8].try_into().unwrap()) as usize;
        if data.len() < i + 8 + n + 1 || data[i + 8 + n] != 0x0A {
            return Err("bad frame boundary".into());
        }
        let j = &data[i + 8..i + 8 + n];
        out.push(parse_record_json(std::str::from_utf8(j).map_err(|e| e.to_string())?)?);
        i += 8 + n + 1;
    }
    Ok(out)
}

/// 整链结构验证：首行 LEDGER_INIT；seq 从 0 连续；prev 链；record_hash 自洽。
pub fn verify_chain(records: &[LedgerRecord]) -> Result<String, String> {
    if records.is_empty() {
        return Err("ledger empty".into());
    }
    if records[0].op != MlvOp::LedgerInit {
        return Err("first record must be LEDGER_INIT".into());
    }
    let mut expect_prev = ZERO_HASH.to_string();
    for (i, r) in records.iter().enumerate() {
        if r.seq != i as u64 {
            return Err(format!("seq break at {i}: got {} expect {i}", r.seq));
        }
        if r.before_record_hash != expect_prev {
            return Err(format!("chain break at {i}: prev mismatch"));
        }
        let want = r.compute_hash();
        if r.record_hash != want {
            return Err(format!("record_hash mismatch at {i}"));
        }
        expect_prev = r.record_hash.clone();
    }
    Ok(expect_prev)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ductile-mlv-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn init_rec(at: u64) -> LedgerRecord {
        let mut r = LedgerRecord {
            schema: 1, seq: 0, op: MlvOp::LedgerInit,
            key_id: "genesis".into(), root_commitment: GENESIS_ROOT.into(),
            effect_key: INIT_EFFECT_KEY.into(),
            idempotency_scope: None, caller_id: None, request_key: None,
            request_digest: domain_hash(DOMAIN_RECORD, b"init|1"),
            binding_id: None, revision: None, from: None, to: None,
            before_record_hash: ZERO_HASH.into(), accepted_at_ns: at,
            nonce: None, envelope_digest: None, envelope: None,
            payload: "init|1".into(), registry_receipt: None,
            result: [("code".into(), "OK".into())].into_iter().collect(),
            record_hash: String::new(),
        };
        r.record_hash = r.compute_hash();
        r
    }

    #[test]
    fn golden_first_record() {
        // T04 黄金向量（钉死 codec/域/字段序——任何漂移炸这里）
        let rec = init_rec(1);
        assert_eq!(
            rec.record_hash,
            "sha256:f98bdb1fc4ef4ad24d07ea81f37c3ab01e28556290212fddeef0d6109658e18f"
        );
    }

    #[test]
    fn frame_roundtrip_and_bad_lp() {
        let rec = init_rec(7);
        let f = rec.frame();
        let back = LedgerRecord::decode_frame(&f).unwrap();
        assert_eq!(back.seq, 0);
        assert_eq!(back.op, MlvOp::LedgerInit);
        assert_eq!(back.record_hash, rec.record_hash);
        let mut bad = f.clone();
        bad[7] ^= 0xFF;
        assert!(LedgerRecord::decode_frame(&bad).is_err());
    }

    #[test]
    fn parse_rejects_unknown_and_dup() {
        // 未知字段 → 拒（计数 24≠23）：从帧中取纯 JSON 段再改
        let rec = init_rec(1);
        let f = rec.frame();
        let n = u64::from_be_bytes(f[..8].try_into().unwrap()) as usize;
        let j: String = String::from_utf8(f[8..8 + n].to_vec()).unwrap();
        let mut j2 = j.clone();
        j2.pop(); // }
        j2.push_str(",\"evil\":1}");
        assert!(parse_record_json(&j2).is_err());
        // 原 JSON 可解析
        assert!(parse_record_json(&j).is_ok());
        // 重复键 → 拒（parse 阶段，G 条）
        let j2 = "{\"schema\":1,\"schema\":2}";
        assert!(parse_json_closed(j2).is_err());
    }

    #[test]
    fn init_reject_and_missing() {
        let d = tmpdir("init");
        let p = d.join("ledger.jsonl");
        assert_eq!(MlvLedger::open(&p).unwrap_err(), "ledger-missing");
        let (_led, h) = MlvLedger::init(&p, 42).unwrap();
        assert!(h.starts_with("sha256:"));
        assert!(MlvLedger::init(&p, 43).is_err());
        let led = MlvLedger::open(&p).unwrap();
        let recs = led.records().unwrap();
        assert_eq!(recs.len(), 1);
        let head = verify_chain(&recs).unwrap();
        assert_eq!(head, h);
    }

    #[test]
    fn time_formula_boundaries() {
        let skew: u64 = 300;
        assert!(envelope_time_ok(1000, 2000, 1500, skew));
        assert!(!envelope_time_ok(1000, 2000, 2000 + skew, skew));     // 上界严格
        assert!(envelope_time_ok(1000, 2000, 2000 + skew - 1, skew));
        assert!(envelope_time_ok(1000, 2000, 1000 - skew, skew));      // 下界含等号
        assert!(!envelope_time_ok(1000, 2000, 1000 - skew - 1, skew));
    }

    #[test]
    fn effect_key_rules() {
        let a = effect_key(MlvOp::Grant, "b1", 0, "rk1").unwrap();
        let b = effect_key(MlvOp::Grant, "b1", 0, "rk1").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, effect_key(MlvOp::Grant, "b1", 0, "rk2").unwrap());
        // 0x1f 注入拒
        assert!(effect_key(MlvOp::Grant, "b\u{1f}", 0, "rk").is_err());
    }

    #[test]
    fn mac_full_and_reject() {
        let mut env: BTreeMap<String, String> = [
            ("issuer", "i"), ("tenant", "t"), ("subject", "s"), ("key_id", "k"),
            ("binding_id", "b1"), ("revision", "0"), ("op", "GRANT"),
            ("request_key", "rk"), ("request_digest", "d"),
            ("issued_at_ns", "1"), ("expires_at_ns", "9"),
            ("contract_digest", "c"), ("grant_digest", "g"),
            ("audience", "ductile-mlv"), ("root_commitment", GENESIS_ROOT),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let mac = envelope_mac(&env).unwrap();
        env.insert("mac".into(), mac);
        assert!(verify_envelope_mac(&env).is_ok());
        env.insert("subject".into(), "EVIL".into());
        assert!(verify_envelope_mac(&env).is_err());
        // 缺声明 → envelope_mac 报缺
        let mut bad = env.clone();
        bad.remove("audience");
        assert!(envelope_mac(&bad).is_err());
    }

    #[test]
    fn append_two_and_chain() {
        let d = tmpdir("two");
        let p = d.join("ledger.jsonl");
        let (mut led, h) = MlvLedger::init(&p, 1).unwrap();
        let ek = effect_key(MlvOp::CreateProposal, "b1", 0, "rk").unwrap();
        let rec = LedgerRecord {
            schema: 1, seq: 1, op: MlvOp::CreateProposal,
            key_id: "genesis".into(), root_commitment: GENESIS_ROOT.into(),
            effect_key: ek, idempotency_scope: None,
            caller_id: Some("actor-llm".into()), request_key: Some("rk".into()),
            request_digest: domain_hash(DOMAIN_RECORD, b"req"),
            binding_id: Some("b1".into()), revision: Some(0),
            from: None, to: Some(MlvState::Proposed),
            before_record_hash: h, accepted_at_ns: 2,
            nonce: Some("n1".into()), envelope_digest: None, envelope: None,
            payload: "proposal|b1|0".into(), registry_receipt: None,
            result: [("code".into(), "OK".into())].into_iter().collect(),
            record_hash: String::new(),
        };
        let h2 = led.append(rec).unwrap();
        assert!(h2.starts_with("sha256:"));
        let recs = led.records().unwrap();
        assert_eq!(recs.len(), 2);
        let head = verify_chain(&recs).unwrap();
        assert_eq!(head, h2);
    }
}
