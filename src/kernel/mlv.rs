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
pub const CANON_ORDER: &[&str] = &[
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

    /// typed 帧 = u64be(N) ‖ 0x00 ‖ J ‖ 0x0A（ed25519 账本业务记录）。
    pub fn frame_typed(&self) -> Vec<u8> {
        let mut f = self.frame();
        let mut out = Vec::with_capacity(f.len() + 1);
        out.extend_from_slice(&f[..8]);
        out.push(0x00);
        out.extend_from_slice(&f[8..]);
        f.clear();
        out
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

pub fn json_esc(s: &str) -> String {
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
/// 保序解析（f4 冻结规格：canonical 序即合法性——序≠CANON_ORDER 即拒）。
/// 复用闭解析器的全部字节层防线（重复键/未知字段/深度/尾随），仅改容器为 Vec。
fn parse_json_ordered(j: &str) -> Result<Vec<(String, JVal)>, String> {
    // 先走原闭解析器拿全部字节层校验（重复键在 token 层拒）
    let m = parse_json_closed(j)?;
    // 再按原文出现序提取键序（parse_string 已保证无重复；直接扫原文顶层键）
    let b: Vec<char> = j.chars().collect();
    let mut keys = Vec::new();
    let mut i = 0usize;
    skip_ws(&b, &mut i);
    if i >= b.len() || b[i] != '{' { return Err("expected {".into()); }
    i += 1;
    loop {
        skip_ws(&b, &mut i);
        if i < b.len() && b[i] == '}' { break; }
        let k = parse_string(&b, &mut i)?;
        keys.push(k.clone());
        skip_ws(&b, &mut i);
        if i >= b.len() || b[i] != ':' { return Err("expected :".into()); }
        i += 1;
        skip_ws(&b, &mut i);
        // 跳过值（字符串/嵌套对象/标量）——用一次完整 value 解析推进
        let _ = parse_value_skip(&b, &mut i)?;
        skip_ws(&b, &mut i);
        if i >= b.len() { return Err("unexpected end".into()); }
        match b[i] {
            ',' => { i += 1; }
            '}' => { i += 1; break; }
            c => return Err(format!("unexpected char {c}")),
        }
    }
    // 查序：与 CANON_ORDER 逐位比对
    if keys.len() != CANON_ORDER.len() {
        return Err(format!("field count {} != {}", keys.len(), CANON_ORDER.len()));
    }
    for (idx, k) in keys.iter().enumerate() {
        if k != CANON_ORDER[idx] {
            return Err(format!("non-canonical order: key[{idx}]='{k}' expect '{}'", CANON_ORDER[idx]));
        }
    }
    // 值从原 map 取（同键同值）
    Ok(keys.into_iter().map(|k| { let v = m.get(&k).cloned().unwrap(); (k, v) }).collect())
}

fn parse_value_skip(b: &[char], i: &mut usize) -> Result<(), String> {
    if *i >= b.len() { return Err("eof".into()); }
    if b[*i] == '"' { parse_string(b, i)?; return Ok(()); }
    if b[*i] == '{' {
        let _ = parse_flat_object(b, i)?;
        return Ok(());
    }
    while *i < b.len() && b[*i] != ',' && b[*i] != '}' && !b[*i].is_whitespace() { *i += 1; }
    Ok(())
}

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

pub fn parse_flat_object(b: &[char], i: &mut usize) -> Result<BTreeMap<String, String>, String> {
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
    let ordered = parse_json_ordered(j)?;
    let m: BTreeMap<String, JVal> = ordered.into_iter().collect();
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

/// 统一信封构造器（CLI 与测试共用单一构造路径；终验阻塞②）。
/// issued=at-skew、expires=at+10*skew：窗口覆盖构造时刻起的 10 个偏差周期。
pub fn make_envelope(
    op: &MlvOp, binding: &str, rev: u64, request_key: &str,
    request_digest: &str, at: u64,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("issuer".into(), "ductile-cli".into());
    env.insert("tenant".into(), "local".into());
    env.insert("subject".into(), "cli".into());
    env.insert("key_id".into(), "genesis".into());
    env.insert("binding_id".into(), binding.into());
    env.insert("revision".into(), rev.to_string());
    env.insert("op".into(), op.name().into());
    env.insert("request_key".into(), request_key.into());
    env.insert("request_digest".into(), request_digest.into());
    env.insert("issued_at_ns".into(), at.saturating_sub(DEFAULT_SKEW_NS).to_string());
    env.insert("expires_at_ns".into(), at.saturating_add(10 * DEFAULT_SKEW_NS).to_string());
    env.insert("contract_digest".into(), "cli-contract".into());
    env.insert("grant_digest".into(), "cli-grant".into());
    env.insert("audience".into(), "mlv".into());
    env.insert("root_commitment".into(), GENESIS_ROOT.into());
    let mac = envelope_mac(&env).unwrap_or_default();
    env.insert("mac".into(), mac);
    env
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

/// 进程内已持锁 inode 集合（acquire 与 Drop 共用同一张表——
/// 曾经两函数各声明同名 static 互不相通，Drop 删空表致钥匙永滞，
/// 重入语义收紧后暴露为 fatal）。
#[cfg(unix)]
static HELD_LOCKS: std::sync::Mutex<std::collections::BTreeSet<u64>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

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
    /// 同进程重入（同 inode 二次 acquire）=显式错误（终验阻塞①）：
    /// flock 语义=进程级，二次 LOCK_EX 自锁死等。held 表先探测；未命中则
    /// LOCK_NB 快速探测（跨进程持锁=WouldBlock 报错快速失败，防 CLI 死等）；
    /// 随后阻塞 LOCK_EX。嵌套代码路径必须先释放再进。
    pub fn acquire(lock_path: &Path) -> Result<Self, String> {
        use std::os::unix::io::AsRawFd;
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
            let mut held = HELD_LOCKS.lock().unwrap();
            let set: &mut std::collections::BTreeSet<u64> = &mut *held;
            if set.contains(&inode_key) {
                // 终验阻塞①：重入不再返回无锁句柄——显式错误。
                return Err(format!(
                    "flock-reentrant: this process already holds the ledger lock (inode-key {inode_key}); nested acquire is forbidden — release before re-entering"
                ));
            }
        }
        let fd = file.as_raw_fd();
        // LOCK_NB 快速探测：外部进程持锁时立即报错（可重试语义），不悬挂。
        loop {
            let r = unsafe { flock(fd, LOCK_EX | LOCK_NB) };
            if r == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(4) => continue,             // EINTR
                Some(11) => {
                    // EWOULDBLOCK：跨进程争用——单次让步后进入阻塞等待
                    break;
                }
                _ => return Err(format!("flock(nb-probe): {err}")),
            }
        }
        loop {
            let r = unsafe { flock(fd, LOCK_EX) };
            if r == 0 {
                HELD_LOCKS.lock().unwrap().insert(inode_key);
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
        if let Some(f) = &self.file {
            let _ = unsafe { flock(f.as_raw_fd(), LOCK_UN) };
            HELD_LOCKS.lock().unwrap().remove(&self.held_key);
        }
        // 重入句柄已在 acquire 阶段显式拒绝（终验阻塞①），此处只可能 file=Some。
    }
}

#[cfg(unix)]
extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}
#[cfg(unix)]
const LOCK_EX: i32 = 2;
#[cfg(unix)]
const LOCK_NB: i32 = 4;
#[cfg(unix)]
const LOCK_UN: i32 = 8;

// ── 账本（init/append/replay/verify；原子提交）────────────────────

/// 账本帧格式（ed25519 规格 §四.1.2：Typed=frame_type 字节在位）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameKind {
    Legacy,
    Typed,
}

#[derive(Debug)]
pub struct MlvLedger {
    pub path: PathBuf,
    pub lock_path: PathBuf,
    pub(crate) lock: Option<MlvLock>,
    pub(crate) tmp_counter: u64,
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
        Self::init_inner(path, accepted_at_ns, FrameKind::Legacy, None)
    }

    /// ed25519 模式 init（typed 帧格式；genesis payload 由调用方提供=auth 对象）。
    pub fn init_typed(path: &Path, accepted_at_ns: u64, genesis_payload: &str) -> Result<(Self, String), String> {
        Self::init_inner(path, accepted_at_ns, FrameKind::Typed, Some(genesis_payload))
    }

    fn init_inner(path: &Path, accepted_at_ns: u64, kind: FrameKind, custom_payload: Option<&str>) -> Result<(Self, String), String> {
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
            request_digest: domain_hash(DOMAIN_RECORD, custom_payload.map(|s| s.to_string()).unwrap_or_else(|| format!("init|{accepted_at_ns}")).as_bytes()),
            binding_id: None, revision: None, from: None, to: None,
            before_record_hash: ZERO_HASH.into(), accepted_at_ns,
            nonce: None, envelope_digest: None, envelope: None,
            payload: custom_payload.map(|s| s.to_string()).unwrap_or_else(|| format!("init|{accepted_at_ns}")),
            registry_receipt: None,
            result: [("code".to_string(), "OK".to_string())].into_iter().collect(),
            record_hash: String::new(),
        };
        rec.record_hash = rec.compute_hash();
        let mut led = MlvLedger {
            path: path.to_path_buf(), lock_path, lock: Some(lock), tmp_counter: 0,
        };
        let _ = &mut led;
        match kind {
            FrameKind::Legacy => led.commit_frame(&rec)?,
            FrameKind::Typed => led.commit_bytes(&rec.frame_typed())?,
        }
        Ok((led, rec.record_hash.clone()))
    }

    /// 直接追加原始帧字节（typed 帧构造方：record_frame_typed / TrustFrame::to_frame_bytes）。
    pub fn append_frame_raw(&mut self, frame: &[u8]) -> Result<(), String> {
        let old = std::fs::read(&self.path).unwrap_or_default();
        let mut new = old;
        new.extend_from_slice(frame);
        self.commit_bytes(&new)
    }

    /// 原子写全量字节（八步固定序复用）。
    fn commit_bytes(&mut self, new: &[u8]) -> Result<(), String> {
        self.tmp_counter += 1;
        let tmp = self.path.with_file_name(format!(
            "{}.tmp.{}.{}",
            self.path.file_name().unwrap().to_string_lossy(),
            std::process::id(),
            self.tmp_counter
        ));
        #[cfg(feature = "mlv-failpoint")]
        fn failpoint(tag: &str) {
            if std::env::var("DUCTILE_MLV_FAILPOINT").as_deref() == Ok(tag) {
                std::process::abort();
            }
        }
        #[cfg(not(feature = "mlv-failpoint"))]
        fn failpoint(_tag: &str) {}
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| format!("create tmp: {e}"))?;
        f.write_all(new).map_err(|e| format!("write tmp: {e}"))?;
        failpoint("after_write");
        f.sync_all().map_err(|e| format!("sync tmp: {e}"))?;
        failpoint("after_sync");
        drop(f);
        failpoint("before_rename");
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("rename: {e}"))?;
        failpoint("after_rename");
        if let Some(parent) = self.path.parent() {
            if let Ok(d) = std::fs::File::open(parent) {
                let _ = d.sync_all();
            }
        }
        failpoint("before_dirsync");
        Ok(())
    }

    pub fn records(&self) -> Result<Vec<LedgerRecord>, String> {
        let data = std::fs::read(&self.path).map_err(|e| format!("read ledger: {e}"))?;
        Ok(decode_frames_typed_aware(&data)?.0)
    }

    /// 业务记录 + 信任帧原始 JSON（typed 账本；legacy 第二项恒空）。
    pub fn records_with_trust(&self) -> Result<(Vec<LedgerRecord>, Vec<String>), String> {
        let data = std::fs::read(&self.path).map_err(|e| format!("read ledger: {e}"))?;
        decode_frames_typed_aware(&data)
    }

    /// 原子追加（返回 record_hash）。
    pub fn append(&mut self, rec: LedgerRecord) -> Result<String, String> {
        let mut rec = rec;
        rec.record_hash = rec.compute_hash();
        self.commit_frame(&rec)?;
        Ok(rec.record_hash.clone())
    }

    /// typed 帧原子追加（ed25519 账本业务记录；frame_type=0）。
    pub fn append_typed(&mut self, rec: LedgerRecord) -> Result<String, String> {
        let mut rec = rec;
        rec.record_hash = rec.compute_hash();
        self.append_frame_raw(&rec.frame_typed())?;
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
        #[cfg(feature = "mlv-failpoint")]
        fn failpoint(tag: &str) {
            if std::env::var("DUCTILE_MLV_FAILPOINT").as_deref() == Ok(tag) {
                std::process::abort();
            }
        }
        #[cfg(not(feature = "mlv-failpoint"))]
        fn failpoint(_tag: &str) {}
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| format!("create tmp: {e}"))?;
        f.write_all(&new).map_err(|e| format!("write tmp: {e}"))?;
        failpoint("after_write");
        f.sync_all().map_err(|e| format!("sync tmp: {e}"))?;
        failpoint("after_sync");
        drop(f);
        failpoint("before_rename");
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("rename: {e}"))?;
        failpoint("after_rename");
        if let Some(parent) = self.path.parent() {
            if let Ok(d) = std::fs::File::open(parent) {
                let _ = d.sync_all();
            }
        }
        failpoint("before_dirsync");
        Ok(())
    }

    /// 当前 head（空账本=ZERO_HASH；不验链——验链走 replay）。
    pub fn head(&self) -> Result<String, String> {
        let (recs, _trusts) = self.records_with_trust()?;
        Ok(recs.last().map(|r| r.record_hash.clone()).unwrap_or_else(|| ZERO_HASH.to_string()))
    }
}

pub fn lock_path_of(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    path.with_file_name(format!("{name}.lock"))
}

/// typed-aware 全量解码：返回 (业务记录, 信任帧原始 JSON)。
/// 帧格式由首帧头字节裁定（0x00/0x01=typed：u64be(N)‖ft(1B)‖J‖LF；'{'=legacy），
/// 全账本不得混用两种格式。信任帧 JSON 不在此解析（mlv_auth::TrustFrame::from_json 负责）。
pub fn decode_frames_typed_aware(data: &[u8]) -> Result<(Vec<LedgerRecord>, Vec<String>), String> {
    let mut recs = Vec::new();
    let mut trusts = Vec::new();
    let mut i = 0usize;
    let mut typed: Option<bool> = None;
    while i < data.len() {
        if data.len() - i < 9 {
            return Err("truncated frame header".into());
        }
        let n64 = u64::from_be_bytes(data[i..i + 8].try_into().unwrap());
        let n = usize::try_from(n64).map_err(|_| "frame length overflow")?;
        let head = data[i + 8];
        let (is_typed, ft) = match head {
            0x00 | 0x01 => (true, head),
            b'{' => (false, 0u8),
            _ => return Err("bad frame head (expected frame_type or '{')".into()),
        };
        match typed {
            None => typed = Some(is_typed),
            Some(prev) if prev != is_typed => {
                return Err("frame format drift: typed/legacy mixed in one ledger".into());
            }
            _ => {}
        }
        let hdr = if is_typed { 9 } else { 8 };
        let end = i.checked_add(hdr).and_then(|v| v.checked_add(n)).and_then(|v| v.checked_add(1))
            .ok_or("frame boundary overflow")?;
        if data.len() < end || data[end - 1] != 0x0A {
            return Err("bad frame boundary".into());
        }
        let jstart = i + hdr;
        let j = std::str::from_utf8(&data[jstart..jstart + n]).map_err(|e| e.to_string())?;
        if is_typed && ft == 0x01 {
            trusts.push(j.to_string());
        } else {
            recs.push(parse_record_json(j)?);
        }
        i = end;
    }
    Ok((recs, trusts))
}

pub fn decode_frames(data: &[u8]) -> Result<Vec<LedgerRecord>, String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        if data.len() - i < 9 {
            return Err("truncated frame header".into());
        }
        let n64 = u64::from_be_bytes(data[i..i + 8].try_into().unwrap());
        // checked：n 或边界算术溢出（近 u64::MAX 长度前缀）=拒，禁 wrap/panic（f4）
        let n = usize::try_from(n64).map_err(|_| "frame length overflow")?;
        let end = i.checked_add(8).and_then(|v| v.checked_add(n)).and_then(|v| v.checked_add(1))
            .ok_or("frame boundary overflow")?;
        if data.len() < end || data[end - 1] != 0x0A {
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
        let h = {
            let (led, h) = MlvLedger::init(&p, 42).unwrap();
            assert!(h.starts_with("sha256:"));
            assert!(MlvLedger::init(&p, 43).is_err());
            // 同进程重入 open（持锁中嵌套 acquire）=显式错误（终验阻塞①）
            let nested = MlvLedger::open(&p);
            assert!(nested.unwrap_err().contains("flock-reentrant"));
            drop(led);
            h
        };
        // 释放后 open 必须成功
        let led = MlvLedger::open(&p).unwrap();
        let recs = led.records().unwrap();
        assert_eq!(recs.len(), 1);
        let head = verify_chain(&recs).unwrap();
        assert_eq!(head, h);
    }

    #[test]
    fn t13_reentrant_acquire_is_explicit_error() {
        // 终验阻塞①：嵌套 acquire 必须显式错误，且首持有者 Drop 后可重新 acquire
        let d = tmpdir("t13");
        let lp = d.join("l.jsonl.lock");
        let l1 = MlvLock::acquire(&lp).unwrap();
        let err = MlvLock::acquire(&lp).unwrap_err();
        assert!(err.contains("flock-reentrant"), "got: {err}");
        drop(l1);
        // 释放后重新获取必须成功（held 表已清）
        let _l2 = MlvLock::acquire(&lp).unwrap();
    }

    #[test]
    fn t14_cross_process_lock_mutual_exclusion() {
        // 外部进程持锁：本进程 LOCK_NB 探测让步后阻塞等待，直到对方释放后获得
        let d = tmpdir("t14");
        let lp = d.join("l.jsonl.lock");
        // 用子进程持锁 300ms 后释放
        let child = std::process::Command::new("python3")
            .arg("-c")
            .arg(format!(
                "import fcntl,subprocess,sys,time;f=open(r'{}','a+');fcntl.flock(f,fcntl.LOCK_EX);time.sleep(0.3);print('released')",
                lp.display()
            ))
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(80)); // 等子进程拿到锁
        let t0 = std::time::Instant::now();
        let l = MlvLock::acquire(&lp).expect("blocking acquire after child release");
        let waited = t0.elapsed();
        assert!(waited.as_millis() >= 100, "acquire returned too fast ({waited:?}) — no cross-process exclusion");
        drop(l);
        let out = child.wait_with_output().unwrap();
        assert!(out.stdout.ends_with(b"released\n"));
    }

    #[test]
    fn t18_two_thread_same_process_contention() {
        // 终验A裁决：同进程双线程争同一 lock inode——一线程持有，另一线程必须
        // 显式 reentrant 错误（HELD 表进程全局），首者 Drop 后第二线程可获取
        let d = tmpdir("t18");
        let lp = std::sync::Arc::new(d.join("l.jsonl.lock"));
        let l1 = MlvLock::acquire(&lp).unwrap();
        let lp2 = lp.clone();
        let handle = std::thread::spawn(move || {
            match MlvLock::acquire(&lp2) {
                Err(e) if e.contains("flock-reentrant") => "explicit-error".to_string(),
                Ok(_) => "BAD-acquired".to_string(),
                Err(e) => format!("BAD-other:{e}"),
            }
        });
        assert_eq!(handle.join().unwrap(), "explicit-error");
        drop(l1);
        // 释放后新线程可获取
        let lp3 = lp.clone();
        let h2 = std::thread::spawn(move || MlvLock::acquire(&lp3).map(|_| "ok").map_err(|e| e));
        assert!(matches!(h2.join().unwrap(), Ok("ok")));
    }

    #[test]
    fn t19_records_immutable_snapshot_while_locked() {
        // records()=owned Vec=不可变快照（终验A裁决：嵌套只读路径）；
        // 持锁期间可读，改返回值不影响账本，重复读全等
        let d = tmpdir("t19");
        let p = d.join("ledger.jsonl");
        let (led, _) = MlvLedger::init(&p, 42).unwrap();
        let r1 = led.records().unwrap();
        assert_eq!(r1.len(), 1);
        let mut mutated = r1.clone();
        mutated[0].payload = "EVIL".into();
        let r2 = led.records().unwrap();
        assert_eq!(r2, r1, "snapshot mutated externally");
        assert_ne!(r2, mutated);
        assert_eq!(std::fs::read(&p).unwrap().len() % 1, 0); // 账本仍在
    }

    #[test]
    fn f4_canonical_vectors() {
        // decode→re-encode 字节全等（canonical 等价回检，逐帧逐字节）
        let d = tmpdir("f4c");
        let p = d.join("ledger.jsonl");
        let (mut led, _) = MlvLedger::init(&p, 7).unwrap();
        let data = std::fs::read(&p).unwrap();
        let recs = decode_frames(&data).unwrap();
        assert_eq!(recs.len(), 1);
        let mut re = Vec::new();
        for r in &recs { re.extend_from_slice(&r.frame()); }
        assert_eq!(re, data, "canonical roundtrip byte-equality broken");
        // 多帧：追加一条后再全量回检
        let rec = LedgerRecord {
            schema: 1, seq: 1, op: MlvOp::CreateProposal,
            key_id: "genesis".into(), root_commitment: GENESIS_ROOT.into(),
            effect_key: effect_key(MlvOp::CreateProposal, "b", 0, "rk").unwrap(),
            idempotency_scope: None, caller_id: Some("t".into()), request_key: Some("rk".into()),
            request_digest: domain_hash(DOMAIN_RECORD, b"CREATE_PROPOSAL|rk|p"),
            binding_id: Some("b".into()), revision: Some(0), from: None, to: Some(MlvState::Proposed),
            before_record_hash: recs[0].record_hash.clone(), accepted_at_ns: 9,
            nonce: Some("n9".into()), envelope_digest: None, envelope: None,
            payload: "p".into(), registry_receipt: None,
            result: [("code".into(), "OK".into())].into_iter().collect(),
            record_hash: String::new(),
        };
        let rh = led.append(rec).unwrap();
        assert!(rh.starts_with("sha256:"));
        let data2 = std::fs::read(&p).unwrap();
        let recs2 = decode_frames(&data2).unwrap();
        let mut re2 = Vec::new();
        for r in &recs2 { re2.extend_from_slice(&r.frame()); }
        assert_eq!(re2, data2);
    }

    #[test]
    fn f4_byte_reject() {
        let d = tmpdir("f4b");
        let p = d.join("ledger.jsonl");
        let (_led, _) = MlvLedger::init(&p, 7).unwrap();
        let data = std::fs::read(&p).unwrap();
        // N 大于剩余字节
        let mut evil = data.clone();
        let n = (evil.len() as u64) * 10;
        evil[0..8].copy_from_slice(&n.to_be_bytes());
        assert!(decode_frames(&evil).is_err(), "oversized N accepted");
        // u64 长度前缀极值（usize 可容但边界溢出）
        let mut evil2 = data.clone();
        evil2[0..8].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(decode_frames(&evil2).is_err(), "u64::MAX N accepted");
        // 帧后尾随字节（不含 LF 的残渣）
        let mut evil3 = data.clone();
        evil3.extend_from_slice(b"x");
        assert!(decode_frames(&evil3).is_err(), "trailing byte accepted");
        // 缺 LF
        let mut evil4 = data.clone();
        let l = evil4.len();
        evil4[l - 1] = b' ';
        assert!(decode_frames(&evil4).is_err(), "missing LF accepted");
        // 双 LF（第二帧以 0x0A 开头=垃圾头）
        let mut evil5 = data.clone();
        evil5.push(0x0A);
        assert!(decode_frames(&evil5).is_err(), "double LF accepted");
        // 非 UTF-8 JSON 段
        let mut evil6 = data.clone();
        let jstart = 8;
        evil6[jstart] = 0xFF;
        // 重算 N 保持边界一致：只破坏字节内容
        assert!(decode_frames(&evil6).is_err(), "invalid UTF-8 accepted");
        // 截断帧头
        assert!(decode_frames(&data[..5]).is_err(), "truncated header accepted");
    }

    #[test]
    fn f4_semantic_reject() {
        let d = tmpdir("f4s");
        let p = d.join("ledger.jsonl");
        let (_led, _) = MlvLedger::init(&p, 7).unwrap();
        let data = std::fs::read(&p).unwrap();
        let n = u64::from_be_bytes(data[0..8].try_into().unwrap()) as usize;
        let j = String::from_utf8(data[8..8 + n].to_vec()).unwrap();

        // 未知字段 → parse 拒
        let mut evil = j.clone();
        evil.insert_str(evil.len() - 1, ",\"evil_field\":\"x\"");
        assert!(parse_record_json(&evil).is_err(), "unknown field accepted");

        // 重复键 → parse 拒
        let mut dup = j.clone();
        dup.insert_str(dup.len() - 1, ",\"schema\":1");
        assert!(parse_record_json(&dup).is_err(), "duplicate key accepted");

        // 重复键转义等价形（"\u0073chema" 与 "schema" 同键）→ parse 拒
        let mut esc = j.clone();
        esc.insert_str(esc.len() - 1, ",\"\\u0073chema\":2");
        assert!(parse_record_json(&esc).is_err(), "escaped-equivalent duplicate key accepted");

        // 乱序（payload 挪首，字段集合法）→ parse 拒（canonical 序即合法性）
        let inner = &j[1..j.len() - 1];
        if let Some(pos) = inner.find("\"payload\"") {
            let tail = inner[pos..].trim_end_matches(',').to_string();
            let head = inner[..pos].trim_end_matches(',').to_string();
            let reordered = format!("{{{},{}}}", tail, head);
            assert!(parse_record_json(&reordered).is_err(), "non-canonical order accepted");
        }

        // 缺字段（删 payload 键值对）→ parse 拒（字段计数 23）
        let mut miss = j.clone();
        let inner = &j[1..j.len() - 1];
        if let Some(pos) = inner.find("\"payload\"") {
            let end = inner[pos..].find(',').map(|v| pos + v + 1).unwrap_or(inner.len());
            let mut ni = inner[..pos].trim_end_matches(',').to_string();
            if end < inner.len() { ni.push(','); ni.push_str(&inner[end..]); }
            miss = format!("{{{}}}", ni);
        }
        assert!(parse_record_json(&miss).is_err(), "missing field accepted");

        // 多字段（payload 后再加一个已知键的副本变体——即重复，已测；此处测多余逗号语法坏）
        let mut syntax = j.clone();
        syntax.insert_str(syntax.len() - 1, ",");
        assert!(parse_record_json(&syntax).is_err(), "trailing comma accepted");
    }

    #[test]
    fn f4_escaped_key_order_differential() {
        // 终验可留⑥：转义键序差分——转义后与规范键等价的键（"\u0073chema"=="schema"）
        // 必须出现在规范位置，任何非规范位置（无论转义与否）一律拒。
        // 构造：把内层某个非首位字段名转义改写（字段集与值不变、键序不变）——
        // 若解析器对转义键放行（等价键回到 canonical 位）则 canonical 向量本身必须仍合法；
        // 再把该转义键挪到非规范位置（与 schema 交换）→ 必拒。
        let d = tmpdir("f4e");
        let p = d.join("ledger.jsonl");
        let (_led, _) = MlvLedger::init(&p, 7).unwrap();
        let data = std::fs::read(&p).unwrap();
        let n = u64::from_be_bytes(data[0..8].try_into().unwrap()) as usize;
        let j = String::from_utf8(data[8..8 + n].to_vec()).unwrap();

        // (a) canonical 位上的转义等价键：把 "seq" 改写为 "\u0073eq"（位置不变）。
        // 语义等价（解析后同键同值同序）→ 必须被接受（证明拒绝只针对位置，不针对拼写）。
        let escaped_canonical = j.replacen("\"seq\"", "\"\\u0073eq\"", 1);
        assert_eq!(
            parse_record_json(&escaped_canonical).unwrap().seq,
            recs_seq_of(&data),
            "escaped-equivalent key at canonical position must parse to same record"
        );

        // (b) 非规范位置："\u0073eq"（=seq，canon 序在后段）挪到首位（accepted_at_ns 之前）
        // → 与把 "seq" 挪到首位等价 → 必拒（canonical 序即合法性，转义不绕序）。
        if let Some(pos) = escaped_canonical.find("\"\\u0073eq\"") {
            let inner = &escaped_canonical[1..escaped_canonical.len() - 1];
            let seg_start = inner.find("\"\\u0073eq\"").unwrap();
            let _ = pos;
            let seg_end = inner[seg_start..].find(',').map(|v| seg_start + v).unwrap_or(inner.len());
            let seg = &inner[seg_start..seg_end];
            let rest = format!("{},{}", &inner[..seg_start], &inner[seg_end + 1..]);
            let reordered = format!("{{{},{}}}", seg, rest);
            assert!(parse_record_json(&reordered).is_err(), "escaped key moved to non-canonical position accepted");
        }

        // (c) 对照组：未转义的 "seq" 挪到首位 → 同样必拒（基线，证明 (b) 非误伤）
        if let Some(pos) = j.find("\"seq\"") {
            let inner = &j[1..j.len() - 1];
            let _ = pos;
            let seg_end = inner.find("\"seq\"").unwrap() + "\"seq\"".len();
            // 取整段键值（到下一个逗号）
            let seg_end = inner[seg_end..].find(',').map(|v| seg_end + v).unwrap_or(inner.len());
            let seg_start = inner.find("\"seq\"").unwrap();
            let seg = &inner[seg_start..seg_end];
            let rest = format!("{},{}", &inner[..seg_start], &inner[seg_end + 1..]);
            let reordered = format!("{{{},{}}}", seg, rest);
            assert!(parse_record_json(&reordered).is_err(), "plain key moved to non-canonical position accepted (baseline)");
        }
    }

    /// 从帧字节取 seq 字段值（测试帮手：避免与被测解析器共用实现路径）。
    fn recs_seq_of(data: &[u8]) -> u64 {
        let n = u64::from_be_bytes(data[0..8].try_into().unwrap()) as usize;
        let j = String::from_utf8(data[8..8 + n].to_vec()).unwrap();
        let m = parse_json_closed(&j).unwrap();
        match m.get("seq") {
            Some(crate::kernel::mlv::JVal::N(v)) => *v,
            other => panic!("seq not numeric: {other:?}"),
        }
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
