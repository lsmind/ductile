//! MLV v2（TOON）记录层 — TOON 迁移 P2 双读单写。
//!
//! 冻结裁定（docs/sonet_toon_spec.md v1.1 + 确认审#1/#2/#3；本文件为实现锚）：
//! 1. **record_hash / H0 / effect_key / MAC**：算法常量、域分隔串、拼接序、字段集、
//!    排除/置位规则全部不变；仅 canonical JSON 字节 J → canonical TOON 字节 T（§三.4）。
//!    不叠 0x02。
//! 2. **Ed25519 签名域**（信封+信任帧）：`DOMAIN_SIG ‖ u64be(len(T)) ‖ 0x02 ‖ T`
//!    （§三.6「FT 后插 codec 入签名域」+§四.2；len 沿帧 N 语义不计 codec、LF 不入域）。
//! 3. **物理帧 v2**：`u64be(N2) ‖ FT ‖ 0x02 ‖ T ‖ LF`，N2=len(T)（§1.3，toon.rs 已落）。
//! 4. **三态链格式**：首帧定模式（v1-legacy 8B 头 / v1-typed 9B / v2 10B），
//!    漂移=整链拒（§三.1-2）。
//! 5. **字段序**：LedgerRecord 23 字段 CANON_ORDER 恰为 UTF-8 字节序 → TVal::Obj；
//!    TrustFrame 8 字段与 auth 对象 5 字段为 v1 固定 canonical 序（非字节序）→
//!    TVal::SchemaObj（§1.1.3「固定 schema 对象按 schema 字段序输出，不排序」）。
//! 6. **auth schema 串保持 "mlv_auth_v1"**：格式版本由帧 codec 位判别，不改字符串
//!    （§1.1.3「由 schema 判别」——codec 即判别位；防 H0 公式输入漂移）。
//! 7. **链 ID**：本内核无独立 chain_id 字段，链身份=genesis record_hash/H0——v2
//!    canonical 字节不同 ⇒ 哈希天然不同，v2 不复用 v1 genesis（§三.4-5 由构造保证）。
//! 8. **v2 预期值禁止实现回填**：golden 一律测试内手写期望字节（fixture 驱动）。

use crate::kernel::hash::sha256;
use crate::kernel::mlv::{
    domain_hash, lock_path_of, LedgerRecord, MlvLedger, MlvLock, MlvOp, MlvState, CANON_ORDER,
    DOMAIN_RECORD, GENESIS_ROOT, INIT_EFFECT_KEY, ZERO_HASH,
};
use crate::kernel::mlv_auth::{
    TrustFrame, TrustState, AUTH_SCHEMA, DOMAIN_SIG, DOMAIN_TRUST,
};
use crate::kernel::toon::{encode_frame_v2, parse_toon_closed, toon_canonical, TVal};
use std::collections::BTreeMap;

pub const FT_RECORD: u8 = 0x00;
pub const FT_TRUST: u8 = 0x01;
pub const TOON_CODEC: u8 = 0x02;

// ── LedgerRecord ↔ TVal ─────────────────────────────────────────

fn opt_str(v: &Option<String>) -> TVal {
    match v { Some(s) => TVal::Str(s.clone()), None => TVal::Null }
}
fn opt_state(v: &Option<MlvState>) -> TVal {
    match v { Some(s) => TVal::Str(s.name().into()), None => TVal::Null }
}
fn opt_map(v: &Option<BTreeMap<String, String>>) -> TVal {
    match v {
        None => TVal::Null,
        Some(m) => TVal::Obj(m.iter().map(|(k, s)| (k.clone(), TVal::Str(s.clone()))).collect()),
    }
}

/// 23 字段 → TVal（hash_override 空串=排除 record_hash 自身，与 v1 canonical_without_hash 对称）。
pub fn record_to_tval(rec: &LedgerRecord, hash_override: &str) -> TVal {
    let mut m: BTreeMap<String, TVal> = BTreeMap::new();
    m.insert("accepted_at_ns".into(), TVal::Num(rec.accepted_at_ns));
    m.insert("before_record_hash".into(), TVal::Str(rec.before_record_hash.clone()));
    m.insert("binding_id".into(), opt_str(&rec.binding_id));
    m.insert("caller_id".into(), opt_str(&rec.caller_id));
    m.insert("effect_key".into(), TVal::Str(rec.effect_key.clone()));
    m.insert("envelope".into(), opt_map(&rec.envelope));
    m.insert("envelope_digest".into(), opt_str(&rec.envelope_digest));
    m.insert("from".into(), opt_state(&rec.from));
    m.insert("idempotency_scope".into(), opt_str(&rec.idempotency_scope));
    m.insert("key_id".into(), TVal::Str(rec.key_id.clone()));
    m.insert("nonce".into(), opt_str(&rec.nonce));
    m.insert("op".into(), TVal::Str(rec.op.name().into()));
    m.insert("payload".into(), TVal::Str(rec.payload.clone()));
    m.insert("record_hash".into(), TVal::Str(hash_override.into()));
    m.insert("registry_receipt".into(), opt_map(&rec.registry_receipt));
    m.insert("request_digest".into(), TVal::Str(rec.request_digest.clone()));
    m.insert("request_key".into(), opt_str(&rec.request_key));
    m.insert("result".into(), opt_map(&Some(rec.result.clone())));
    m.insert("revision".into(), match rec.revision { Some(r) => TVal::Num(r), None => TVal::Null });
    m.insert("root_commitment".into(), TVal::Str(rec.root_commitment.clone()));
    m.insert("schema".into(), TVal::Num(rec.schema as u64));
    m.insert("seq".into(), TVal::Num(rec.seq));
    m.insert("to".into(), opt_state(&rec.to));
    TVal::Obj(m)
}

/// canonical TOON 字节（无 record_hash 版=哈希输入；含=帧体）。
pub fn canonical_v2(rec: &LedgerRecord, hash_override: &str) -> Result<Vec<u8>, String> {
    toon_canonical(&record_to_tval(rec, hash_override))
}

/// v2 record_hash = D(MLV/v3/record, canonical_without_hash_v2)（公式不变，字节换 T）。
pub fn record_hash_v2(rec: &LedgerRecord) -> Result<String, String> {
    Ok(domain_hash(DOMAIN_RECORD, &canonical_v2(rec, "")?))
}

/// 业务记录 v2 帧（FT=0）。
pub fn record_frame_v2(rec: &LedgerRecord) -> Result<Vec<u8>, String> {
    Ok(encode_frame_v2(FT_RECORD, &canonical_v2(rec, &rec.record_hash)?))
}

// ── 闭合解析（T → LedgerRecord；schema 闭合 + 序 canonical 双闸）──────────

fn tval_str(v: &TVal, field: &str) -> Result<String, String> {
    match v { TVal::Str(s) => Ok(s.clone()), _ => Err(format!("v2 field {field} must be string")) }
}
fn tval_opt_str(v: &TVal, field: &str) -> Result<Option<String>, String> {
    match v { TVal::Str(s) => Ok(Some(s.clone())), TVal::Null => Ok(None), _ => Err(format!("v2 field {field} must be string or null")) }
}
fn tval_num(v: &TVal, field: &str) -> Result<u64, String> {
    match v { TVal::Num(n) => Ok(*n), _ => Err(format!("v2 field {field} must be number")) }
}
fn tval_opt_num(v: &TVal, field: &str) -> Result<Option<u64>, String> {
    match v { TVal::Num(n) => Ok(Some(*n)), TVal::Null => Ok(None), _ => Err(format!("v2 field {field} must be number or null")) }
}
fn tval_opt_state(v: &TVal, field: &str) -> Result<Option<MlvState>, String> {
    match v {
        TVal::Str(s) => MlvState::from_name(s).map(Some).map_err(|e| format!("v2 field {field}: {e}")),
        TVal::Null => Ok(None),
        _ => Err(format!("v2 field {field} must be state string or null")),
    }
}
fn tval_opt_map(v: &TVal, field: &str) -> Result<Option<BTreeMap<String, String>>, String> {
    match v {
        TVal::Null => Ok(None),
        TVal::Obj(m) => {
            let mut out = BTreeMap::new();
            for (k, val) in m {
                match val {
                    TVal::Str(s) => { out.insert(k.clone(), s.clone()); }
                    _ => return Err(format!("v2 field {field}[{k}] must be flat string map")),
                }
            }
            Ok(Some(out))
        }
        _ => Err(format!("v2 field {field} must be flat object or null")),
    }
}

/// 解析 v2 业务记录：parse_toon_closed → 闭合 schema 校验（23 字段+类型封闭）→
/// 重编码 canonical 必须逐字节等于输入（序漂移/非 canonical 全拒）。
pub fn parse_record_v2(t: &[u8]) -> Result<LedgerRecord, String> {
    let root = parse_toon_closed(t)?;
    let m = match &root { TVal::Obj(m) => m, _ => return Err("v2 record root must be object".into()) };
    if m.len() != CANON_ORDER.len() {
        return Err(format!("v2 field count {} != 23", m.len()));
    }
    for k in m.keys() {
        if !CANON_ORDER.contains(&k.as_str()) {
            return Err(format!("v2 unknown field: {k}"));
        }
    }
    let get = |k: &str| m.get(k).ok_or_else(|| format!("v2 missing field: {k}"));
    let op = MlvOp::from_name(&tval_str(get("op")?, "op")?)?;
    let mut rec = LedgerRecord {
        accepted_at_ns: tval_num(get("accepted_at_ns")?, "accepted_at_ns")?,
        before_record_hash: tval_str(get("before_record_hash")?, "before_record_hash")?,
        binding_id: tval_opt_str(get("binding_id")?, "binding_id")?,
        caller_id: tval_opt_str(get("caller_id")?, "caller_id")?,
        effect_key: tval_str(get("effect_key")?, "effect_key")?,
        envelope: tval_opt_map(get("envelope")?, "envelope")?,
        envelope_digest: tval_opt_str(get("envelope_digest")?, "envelope_digest")?,
        from: tval_opt_state(get("from")?, "from")?,
        idempotency_scope: tval_opt_str(get("idempotency_scope")?, "idempotency_scope")?,
        key_id: tval_str(get("key_id")?, "key_id")?,
        nonce: tval_opt_str(get("nonce")?, "nonce")?,
        op,
        payload: tval_str(get("payload")?, "payload")?,
        record_hash: tval_str(get("record_hash")?, "record_hash")?,
        registry_receipt: tval_opt_map(get("registry_receipt")?, "registry_receipt")?,
        request_digest: tval_str(get("request_digest")?, "request_digest")?,
        request_key: tval_opt_str(get("request_key")?, "request_key")?,
        result: tval_opt_map(get("result")?, "result")?.unwrap_or_default(),
        revision: tval_opt_num(get("revision")?, "revision")?,
        root_commitment: tval_str(get("root_commitment")?, "root_commitment")?,
        schema: u32::try_from(tval_num(get("schema")?, "schema")?).map_err(|_| "v2 schema overflow")?,
        seq: tval_num(get("seq")?, "seq")?,
        to: tval_opt_state(get("to")?, "to")?,
    };
    if matches!(m.get("result"), Some(TVal::Null)) {
        return Err("v2 field result must be object (not null)".into());
    }
    // 序 canonical 双闸：重编码逐字节等于输入
    let re = canonical_v2(&rec, &rec.record_hash)?;
    if re != t {
        return Err("v2 record not canonical (field order/bytes drift)".into());
    }
    // 哈希一致性
    let want = record_hash_v2(&rec)?;
    if rec.record_hash != want {
        return Err("v2 record_hash mismatch".into());
    }
    Ok(rec)
}

// ── 三态链解码 ───────────────────────────────────────────────────

/// 链帧格式（首帧裁定；混链=整链拒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFormat {
    V1Legacy,
    V1Typed,
    V2,
}

#[derive(Debug, Clone)]
pub enum LedgerFrame {
    Record(LedgerRecord),
    Trust(TrustFrame),
}

/// 三态全量解码：v2 走 TOON 闭合解析；v1 两态复用既有 JSON 路径（只读兼容）。
/// 任何帧与首帧格式不一致 → 整链失败。
pub fn decode_ledger_tri(data: &[u8]) -> Result<(FrameFormat, Vec<LedgerFrame>), String> {
    use crate::kernel::mlv_auth::decode_frames_mixed;
    use crate::kernel::mlv::decode_frames_typed_aware;
    // 首帧格式判定：+8 字节='{' → v1 legacy；0x00/0x01 → typed 族（+9 codec 槽定 v1/v2）
    if data.is_empty() {
        return Err("empty ledger".into());
    }
    if data.len() < 9 {
        return Err("truncated frame header".into());
    }
    let first = data[8];
    let fmt = match first {
        b'{' => FrameFormat::V1Legacy,
        FT_RECORD | FT_TRUST => {
            if data.len() < 10 { return Err("truncated typed frame header".into()); }
            match data[9] {
                TOON_CODEC => FrameFormat::V2,
                b'{' => FrameFormat::V1Typed,
                other => return Err(format!("bad codec slot {other:#x} at frame 0")),
            }
        }
        other => return Err(format!("bad frame head {other:#x} at frame 0")),
    };
    match fmt {
        FrameFormat::V2 => {
            let mut out = Vec::new();
            let mut i = 0usize;
            let mut records_seen = 0usize;
            let mut expect_prev = ZERO_HASH.to_string();
            while i < data.len() {
                let hdr = 10usize;
                if data.len() - i < hdr {
                    return Err(format!("truncated v2 frame header at {i}"));
                }
                // 逐帧再验格式位（首帧已定 V2；漂移=拒）
                if data[i + 8] != FT_RECORD && data[i + 8] != FT_TRUST {
                    return Err(format!("bad v2 frame_type at {i}"));
                }
                if data[i + 9] != TOON_CODEC {
                    return Err(format!("v1/v2 mixed chain rejected at frame offset {i}"));
                }
                let n64 = u64::from_be_bytes(data[i..i + 8].try_into().unwrap());
                let n = usize::try_from(n64).map_err(|_| "frame length overflow")?;
                if n > 16 * 1024 * 1024 {
                    return Err("v2 frame body exceeds 16MiB".into());
                }
                let end = i.checked_add(hdr).and_then(|v| v.checked_add(n)).and_then(|v| v.checked_add(1))
                    .ok_or("frame boundary overflow")?;
                if data.len() < end || data[end - 1] != 0x0A {
                    return Err(format!("bad v2 frame boundary at {i}"));
                }
                let t = &data[i + hdr..i + hdr + n];
                if data[i + 8] == FT_TRUST {
                    out.push(LedgerFrame::Trust(parse_trust_frame_v2(t)?));
                } else {
                    let rec = parse_record_v2(t)?;
                    // 链验证（T12：错误模式整链失败）：seq 连续 + prev 衔接 + 首帧 genesis
                    if records_seen == 0 {
                        if rec.op != MlvOp::LedgerInit {
                            return Err("v2 first record must be LEDGER_INIT".into());
                        }
                        if rec.before_record_hash != ZERO_HASH {
                            return Err("v2 genesis before_record_hash must be ZERO_HASH".into());
                        }
                    } else {
                        if rec.seq != records_seen as u64 {
                            return Err(format!("v2 seq break: got {} expect {}", rec.seq, records_seen));
                        }
                        if rec.before_record_hash != expect_prev {
                            return Err(format!("v2 chain break at seq {}", rec.seq));
                        }
                    }
                    expect_prev = rec.record_hash.clone();
                    records_seen += 1;
                    out.push(LedgerFrame::Record(rec));
                }
                i = end;
            }
            Ok((FrameFormat::V2, out))
        }
        FrameFormat::V1Typed => {
            let (_, frames) = decode_frames_mixed(data)?;
            Ok((FrameFormat::V1Typed, frames.into_iter().map(|f| match f {
                crate::kernel::mlv_auth::LedgerFrame::Record(r) => LedgerFrame::Record(r),
                crate::kernel::mlv_auth::LedgerFrame::Trust(t) => LedgerFrame::Trust(t),
            }).collect()))
        }
        FrameFormat::V1Legacy => {
            let (recs, _trusts) = decode_frames_typed_aware(data)?;
            if !_trusts.is_empty() {
                return Err("trust frames in legacy ledger".into());
            }
            Ok((FrameFormat::V1Legacy, recs.into_iter().map(LedgerFrame::Record).collect()))
        }
    }
}

// ── TrustFrame v2（SchemaObj 保 v1 固定序）────────────────────────

/// v1 canonical 序：schema,kind,key_id,new_key_id,new_pk,trust_seq,sig,prev_trust_hash。
/// trust_seq v1 以字符串承载（parse_flat_object 封闭串值）——v2 沿字符串（对称，防哈希输入漂移）。
pub fn trust_frame_to_tval(tf: &TrustFrame) -> TVal {
    TVal::SchemaObj(vec![
        ("schema".into(), TVal::Str(AUTH_SCHEMA.into())),
        ("kind".into(), TVal::Str(tf.kind.clone())),
        ("key_id".into(), TVal::Str(tf.key_id.clone())),
        ("new_key_id".into(), TVal::Str(tf.new_key_id.clone())),
        ("new_pk".into(), TVal::Str(tf.new_pk.clone())),
        ("trust_seq".into(), TVal::Str(tf.trust_seq.to_string())),
        ("sig".into(), TVal::Str(tf.sig.clone())),
        ("prev_trust_hash".into(), TVal::Str(tf.prev_trust_hash.clone())),
    ])
}

pub fn trust_frame_canonical_v2(tf: &TrustFrame) -> Result<Vec<u8>, String> {
    toon_canonical(&trust_frame_to_tval(tf))
}

/// 信任帧 v2 帧（FT=1）。
pub fn trust_frame_v2_bytes(tf: &TrustFrame) -> Result<Vec<u8>, String> {
    Ok(encode_frame_v2(FT_TRUST, &trust_frame_canonical_v2(tf)?))
}

pub fn parse_trust_frame_v2(t: &[u8]) -> Result<TrustFrame, String> {
    let root = parse_toon_closed(t)?;
    let m = match &root { TVal::Obj(m) => m, _ => return Err("v2 trust root must be object".into()) };
    let want = ["kind", "key_id", "new_key_id", "new_pk", "prev_trust_hash", "schema", "sig", "trust_seq"];
    if m.len() != want.len() {
        return Err(format!("v2 trust field count {} != 8", m.len()));
    }
    for k in m.keys() {
        if !want.contains(&k.as_str()) {
            return Err(format!("v2 trust unknown field: {k}"));
        }
    }
    if m.get("schema") != Some(&TVal::Str(AUTH_SCHEMA.into())) {
        return Err("v2 trust schema mismatch".into());
    }
    let s = |k: &str| -> Result<String, String> {
        match m.get(k) { Some(TVal::Str(v)) => Ok(v.clone()), _ => Err(format!("v2 trust field {k} must be string")) }
    };
    let trust_seq: u64 = s("trust_seq")?.parse().map_err(|_| "v2 trust trust_seq must be canonical decimal".to_string())?;
    let tf = TrustFrame {
        kind: s("kind")?, key_id: s("key_id")?, new_key_id: s("new_key_id")?,
        new_pk: s("new_pk")?, trust_seq, sig: s("sig")?, prev_trust_hash: s("prev_trust_hash")?,
    };
    // 序 canonical 双闸（固定序≠字节序——重编码按 v1 canonical 序比对）
    let re = trust_frame_canonical_v2(&tf)?;
    if re != t {
        return Err("v2 trust frame not canonical (field order drift)".into());
    }
    Ok(tf)
}

// ── auth 对象 / H0 v2 ────────────────────────────────────────────

/// auth 对象 v1 canonical 序（SchemaObj）：schema,mode,root_key_id,root_public_key[,trust_hash]。
pub fn auth_object_to_tval(root_key_id: &str, root_public_key_hex: &str, trust_hash: &str) -> TVal {
    TVal::SchemaObj(vec![
        ("schema".into(), TVal::Str(AUTH_SCHEMA.into())),
        ("mode".into(), TVal::Str("ed25519".into())),
        ("root_key_id".into(), TVal::Str(root_key_id.into())),
        ("root_public_key".into(), TVal::Str(root_public_key_hex.into())),
        ("trust_hash".into(), TVal::Str(trust_hash.into())),
    ])
}

pub fn auth_object_toon(root_key_id: &str, root_public_key_hex: &str, trust_hash: &str) -> Result<Vec<u8>, String> {
    toon_canonical(&auth_object_to_tval(root_key_id, root_public_key_hex, trust_hash))
}

/// H0 v2 = SHA256("MLV31-TRUST" ‖ u64be(len("mlv_auth_v1")) ‖ T_auth_no_trust_hash)。
/// 公式不变，仅 body JSON→TOON（§三.4；不叠 0x02）。
pub fn compute_h0_v2(root_key_id: &str, root_public_key_hex: &str) -> Result<String, String> {
    let body = auth_object_toon(root_key_id, root_public_key_hex, "")?;
    let mut buf = Vec::new();
    buf.extend_from_slice(DOMAIN_TRUST.as_bytes());
    buf.extend_from_slice(&(AUTH_SCHEMA.len() as u64).to_be_bytes());
    buf.extend_from_slice(&body);
    let digest = sha256(&buf);
    Ok(format!("sha256:{}", digest.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

/// v2 genesis payload（auth 对象 TOON 文本）解析 → 平面对象。
pub fn parse_auth_payload_toon(payload: &str) -> Option<BTreeMap<String, String>> {
    let root = parse_toon_closed(payload.as_bytes()).ok()?;
    match root {
        TVal::Obj(m) => {
            if m.get("schema") == Some(&TVal::Str(AUTH_SCHEMA.into())) {
                let mut out = BTreeMap::new();
                for (k, v) in m {
                    match v { TVal::Str(s) => { out.insert(k, s); }, _ => return None }
                }
                Some(out)
            } else { None }
        }
        _ => None,
    }
}

// ── 信封 Ed25519 v2（codec 入签名域）──────────────────────────────

/// 信封 canonical TOON（BTreeMap 序=v1 envelope_canon_bytes 序；排除 sig 自身）。
pub fn envelope_canonical_toon(env: &BTreeMap<String, String>) -> Result<Vec<u8>, String> {
    let m: BTreeMap<String, TVal> = env.iter()
        .filter(|(k, _)| k.as_str() != "sig")
        .map(|(k, v)| (k.clone(), TVal::Str(v.clone())))
        .collect();
    toon_canonical(&TVal::Obj(m))
}

/// v2 签名消息 = "MLV31-SIG-v1" ‖ u64be(len(T)) ‖ 0x02 ‖ T（裁定 #2）。
pub fn signing_message_v2(env: &BTreeMap<String, String>) -> Result<Vec<u8>, String> {
    let t = envelope_canonical_toon(env)?;
    let mut msg = Vec::with_capacity(DOMAIN_SIG.len() + 8 + 1 + t.len());
    msg.extend_from_slice(DOMAIN_SIG.as_bytes());
    msg.extend_from_slice(&(t.len() as u64).to_be_bytes());
    msg.push(TOON_CODEC);
    msg.extend_from_slice(&t);
    Ok(msg)
}

pub fn sign_envelope_v2(
    env: &mut BTreeMap<String, String>,
    sk: &ed25519_dalek::SigningKey,
    key_id: &str,
    trust_seq: Option<u64>,
) -> Result<(), String> {
    use ed25519_dalek::Signer;
    env.insert("sig_key_id".into(), key_id.into());
    if let Some(ts) = trust_seq {
        env.insert("sig_trust_seq".into(), ts.to_string());
    }
    let msg = signing_message_v2(env)?;
    let sig = sk.sign(&msg);
    let sig_hex: String = sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    env.insert("sig".into(), sig_hex);
    Ok(())
}

pub fn verify_envelope_sig_v2(
    env: &BTreeMap<String, String>,
    public_key: &[u8; 32],
) -> Result<(), String> {
    use ed25519_dalek::{Signature, Verifier};
    let sig_hex = env.get("sig").ok_or("signature-invalid: envelope missing sig")?;
    let sb = crate::kernel::mlv_auth::hex_decode64(sig_hex)
        .ok_or("signature-invalid: sig must be 128 hex chars")?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(public_key)
        .map_err(|_| "signature-invalid: bad public key")?;
    let msg = signing_message_v2(env)?;
    vk.verify(&msg, &Signature::from_bytes(&sb))
        .map_err(|_| "signature-invalid: Ed25519 verify failed".to_string())
}

/// 信任帧 v2 签名消息（同裁定 #2：T=排除 sig 后的 canonical——sig 置空串编码）。
pub fn trust_frame_signing_message_v2(tf: &TrustFrame) -> Result<Vec<u8>, String> {
    let mut bare = tf.clone();
    bare.sig = String::new();
    let t = trust_frame_canonical_v2(&bare)?;
    let mut msg = Vec::with_capacity(DOMAIN_SIG.len() + 8 + 1 + t.len());
    msg.extend_from_slice(DOMAIN_SIG.as_bytes());
    msg.extend_from_slice(&(t.len() as u64).to_be_bytes());
    msg.push(TOON_CODEC);
    msg.extend_from_slice(&t);
    Ok(msg)
}

pub fn sign_trust_frame_v2(tf: &mut TrustFrame, sk: &ed25519_dalek::SigningKey) -> Result<(), String> {
    use ed25519_dalek::Signer;
    let msg = trust_frame_signing_message_v2(tf)?;
    let sig = sk.sign(&msg);
    tf.sig = sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    Ok(())
}

pub fn verify_trust_frame_sig_v2(tf: &TrustFrame, pk: &[u8; 32]) -> Result<(), String> {
    use ed25519_dalek::{Signature, Verifier};
    let sb = crate::kernel::mlv_auth::hex_decode64(&tf.sig)
        .ok_or("signature-invalid: trust frame sig must be 128 hex chars")?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(pk)
        .map_err(|_| "signature-invalid: bad trust signer key")?;
    let msg = trust_frame_signing_message_v2(tf)?;
    vk.verify(&msg, &Signature::from_bytes(&sb))
        .map_err(|_| "signature-invalid: trust frame Ed25519 verify failed".to_string())
}

// ── v2 写入路径（复用八步原子提交；v1 writer 原样保留=P2 兼容边界）────────

impl MlvLedger {
    /// v2 显式 init：唯一 v2 建链入口；genesis payload=auth TOON（ed25519）或自定义串。
    pub fn init_v2(path: &std::path::Path, accepted_at_ns: u64, genesis_payload: &str) -> Result<(Self, String), String> {
        if path.exists() {
            return Err("ledger already initialized (re-init rejected)".into());
        }
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
            }
        }
        let lock_path = lock_path_of(path);
        let lock = MlvLock::acquire(&lock_path)?;
        let mut rec = LedgerRecord {
            schema: 1, seq: 0, op: MlvOp::LedgerInit,
            key_id: "genesis".into(), root_commitment: GENESIS_ROOT.into(),
            effect_key: INIT_EFFECT_KEY.into(),
            idempotency_scope: None, caller_id: None, request_key: None,
            request_digest: domain_hash(DOMAIN_RECORD, genesis_payload.as_bytes()),
            binding_id: None, revision: None, from: None, to: None,
            before_record_hash: ZERO_HASH.into(), accepted_at_ns,
            nonce: None, envelope_digest: None, envelope: None,
            payload: genesis_payload.into(), registry_receipt: None,
            result: [("code".to_string(), "OK".to_string())].into_iter().collect(),
            record_hash: String::new(),
        };
        rec.record_hash = record_hash_v2(&rec)?;
        let mut led = MlvLedger { path: path.to_path_buf(), lock_path, lock: Some(lock), tmp_counter: 0 };
        led.append_frame_raw(&record_frame_v2(&rec)?)?;
        Ok((led, rec.record_hash.clone()))
    }

    /// v2 业务记录原子追加（FT=0；返回 record_hash）。
    pub fn append_v2(&mut self, mut rec: LedgerRecord) -> Result<String, String> {
        rec.record_hash = record_hash_v2(&rec)?;
        self.append_frame_raw(&record_frame_v2(&rec)?)?;
        Ok(rec.record_hash.clone())
    }

    /// v2 信任帧原子追加（FT=1）。
    pub fn append_trust_v2(&mut self, tf: &TrustFrame) -> Result<(), String> {
        self.append_frame_raw(&trust_frame_v2_bytes(tf)?)
    }

    /// v2 全量读（本文件路径；v1 读走既有 records()）。
    pub fn records_v2(&self) -> Result<(FrameFormat, Vec<LedgerFrame>), String> {
        let data = std::fs::read(&self.path).map_err(|e| format!("read ledger: {e}"))?;
        decode_ledger_tri(&data)
    }
}

// ── TrustState v2（H0 自洽用 v2 公式；结构体复用=格式中立）─────────

/// v2 genesis auth → TrustState（toon=true：信任帧验签域=v2）。
pub fn trust_state_from_genesis_auth_v2(auth: &BTreeMap<String, String>) -> Result<TrustState, String> {
    TrustState::from_genesis_auth_toon(auth)
}

/// v2 业务记录验签（trust 检查逻辑=verify_business_sig；签名域=v2）。
pub fn verify_business_sig_v2(
    env: &BTreeMap<String, String>,
    state_at_position: &TrustState,
) -> Result<(), String> {
    use crate::kernel::mlv_auth::{DETAIL_KEY_REVOKED, DETAIL_TRUST_CHAIN};
    let sig_key_id = env.get("sig_key_id")
        .ok_or_else(|| format!("{DETAIL_TRUST_CHAIN}: envelope missing sig_key_id"))?;
    let pk = state_at_position.pk_of(sig_key_id)
        .ok_or_else(|| format!("{DETAIL_TRUST_CHAIN}: signing key not enrolled: {sig_key_id}"))?;
    if let Some(declared_raw) = env.get("sig_trust_seq") {
        let canon = !declared_raw.is_empty()
            && declared_raw.bytes().all(|b| b.is_ascii_digit())
            && (declared_raw.len() == 1 || !declared_raw.starts_with('0'));
        let declared = if canon { declared_raw.parse::<u64>().ok() } else { None };
        match declared {
            Some(d) if d <= state_at_position.trust_seq => {}
            _ => return Err(format!(
                "{DETAIL_TRUST_CHAIN}: sig_trust_seq invalid or ahead: {declared_raw} vs position trust_seq {}",
                state_at_position.trust_seq
            )),
        }
    }
    let pos_seq = state_at_position.trust_seq;
    if state_at_position.is_revoked(sig_key_id, pos_seq) {
        return Err(format!("{DETAIL_KEY_REVOKED}: {sig_key_id} revoked at seq {pos_seq}"));
    }
    verify_envelope_sig_v2(env, pk)
}

/// v2 链验证（verify_chain 的 v2 版：哈希用 record_hash_v2；其余语义同 v1）。
pub fn verify_chain_v2(records: &[LedgerRecord]) -> Result<String, String> {
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
        let want = record_hash_v2(r)?;
        if r.record_hash != want {
            return Err(format!("record_hash mismatch at {i}"));
        }
        expect_prev = r.record_hash.clone();
    }
    Ok(expect_prev)
}
