//! WAL v2（TOON）层 — TOON 迁移 P3a（规格 §四.1）。
//!
//! 裁定（docs/sonet_toon_spec.md §四.1）：
//! 1. 新 WAL 使用 v2 typed framing（u64be‖FT‖0x02‖T‖LF）；record hash 链公式
//!    不变（prev_hash 链=逐行 sha256），仅以 T 替代原 JSON 行。
//! 2. 旧 WAL 只读验证（wal.rs verify_chain/recover 原样保留=P3 兼容边界）；
//!    恢复生成新 v2 WAL，不向旧 WAL 追加。
//! 3. commit/fsync/追加序沿用；幂等（EffectKey 复用）语义不变。
//!
//! T 编码（每 WalRecord 一帧）：SchemaObj 固定序（与 v1 wal_to_json 字段序一致）：
//!   intent:  k,run,step,att,in,prev_hash
//!   effect:  k,run,step,ek,prev_hash
//!   outcome: k,run,step,att,st,err,skip,fd,prev_hash
//!   audit:   k,prev_hash

use crate::kernel::hash::sha256_hex;
use crate::kernel::toon::{encode_frame_v2, parse_toon_closed, toon_canonical, TVal};
use crate::kernel::types::EffectKey;
use crate::kernel::wal::{Recovered, WalRecord};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};

pub const FT_WAL: u8 = 0x00;

// ── 编码 ───────────────────────────────────────────────────────

fn opt_str(v: &Option<String>) -> TVal {
    match v { Some(s) => TVal::Str(s.clone()), None => TVal::Null }
}

/// WalRecord → canonical T（SchemaObj 保 v1 字段序）。
pub fn wal_record_to_tval(rec: &WalRecord, prev_hash: &str) -> TVal {
    match rec {
        WalRecord::Intent { run_id, step_id, attempt, input_digest } => TVal::SchemaObj(vec![
            ("k".into(), TVal::Str("intent".into())),
            ("run".into(), TVal::Str(run_id.clone())),
            ("step".into(), TVal::Str(step_id.clone())),
            ("att".into(), TVal::Num(*attempt as u64)),
            ("in".into(), TVal::Str(input_digest.clone())),
            ("prev_hash".into(), TVal::Str(prev_hash.into())),
        ]),
        WalRecord::EffectAck { run_id, step_id, effect } => TVal::SchemaObj(vec![
            ("k".into(), TVal::Str("effect".into())),
            ("run".into(), TVal::Str(run_id.clone())),
            ("step".into(), TVal::Str(step_id.clone())),
            ("ek".into(), TVal::Str(effect.to_string())),
            ("prev_hash".into(), TVal::Str(prev_hash.into())),
        ]),
        WalRecord::Outcome { run_id, step_id, attempt, status, error_code, skip_reason, fields_digest } => TVal::SchemaObj(vec![
            ("k".into(), TVal::Str("outcome".into())),
            ("run".into(), TVal::Str(run_id.clone())),
            ("step".into(), TVal::Str(step_id.clone())),
            ("att".into(), TVal::Num(*attempt as u64)),
            ("st".into(), TVal::Str(status.clone())),
            ("err".into(), opt_str(error_code)),
            ("skip".into(), opt_str(skip_reason)),
            ("fd".into(), opt_str(fields_digest)),
            ("prev_hash".into(), TVal::Str(prev_hash.into())),
        ]),
        WalRecord::Audit(a) => TVal::SchemaObj(vec![
            ("k".into(), TVal::Str("audit".into())),
            ("seq".into(), TVal::Num(a.seq)),
            ("run".into(), TVal::Str(a.run_id.clone())),
            ("step".into(), TVal::Str(a.step_id.clone())),
            ("att".into(), TVal::Num(a.attempt as u64)),
            ("scen".into(), opt_str(&a.scenario)),
            ("seed".into(), match a.seed { Some(n) => TVal::Num(n), None => TVal::Null }),
            ("fault".into(), opt_str(&a.fault)),
            ("st".into(), TVal::Str(a.outcome_status.to_string())),
            ("err".into(), a.error_code.as_ref().map(|e| TVal::Str(e.code().to_string())).unwrap_or(TVal::Null)),
            ("skip".into(), a.skip_reason.map(|r| TVal::Str(r.to_string())).unwrap_or(TVal::Null)),
            ("ek".into(), a.effect_key.as_ref().map(|e| TVal::Str(e.to_string())).unwrap_or(TVal::Null)),
            ("in".into(), opt_str(&a.input_digest)),
            ("out".into(), opt_str(&a.output_digest)),
            ("prev_hash".into(), TVal::Str(prev_hash.into())),
        ]),
    }
}

pub fn wal_record_canonical_v2(rec: &WalRecord, prev_hash: &str) -> Result<Vec<u8>, String> {
    toon_canonical(&wal_record_to_tval(rec, prev_hash))
}

/// v2 帧 = u64be(N)‖FT‖0x02‖T‖LF。
pub fn wal_frame_v2(rec: &WalRecord, prev_hash: &str) -> Result<Vec<u8>, String> {
    Ok(encode_frame_v2(FT_WAL, &wal_record_canonical_v2(rec, prev_hash)?))
}

// ── 解码（闭合：parse→schema 校验→重编码逐字节比对）──────────────

fn tget<'a>(m: &'a BTreeMap<String, TVal>, k: &str) -> Result<&'a TVal, String> {
    m.get(k).ok_or_else(|| format!("v2 wal missing field: {k}"))
}
fn tstr(v: &TVal, k: &str) -> Result<String, String> {
    match v { TVal::Str(s) => Ok(s.clone()), _ => Err(format!("v2 wal field {k} must be string")) }
}
fn tnum(v: &TVal, k: &str) -> Result<u64, String> {
    match v { TVal::Num(n) => Ok(*n), _ => Err(format!("v2 wal field {k} must be number")) }
}
fn topt_str(v: &TVal, k: &str) -> Result<Option<String>, String> {
    match v { TVal::Str(s) => Ok(Some(s.clone())), TVal::Null => Ok(None), _ => Err(format!("v2 wal field {k} must be string or null")) }
}

/// 解析 v2 WAL 帧 body（T）→ WalRecord + prev_hash。序漂移/非 canonical 拒。
pub fn parse_wal_record_v2(t: &[u8]) -> Result<(WalRecord, String), String> {
    use crate::kernel::types::AuditEvent;
    let root = parse_toon_closed(t)?;
    let m = match &root { TVal::Obj(m) => m, _ => return Err("v2 wal root must be object".into()) };
    let k = tstr(tget(m, "k")?, "k")?;
    let prev = tstr(tget(m, "prev_hash")?, "prev_hash")?;
    let rec = match k.as_str() {
        "intent" => {
            if m.len() != 6 { return Err("v2 wal intent field count".into()); }
            WalRecord::Intent {
                run_id: tstr(tget(m, "run")?, "run")?,
                step_id: tstr(tget(m, "step")?, "step")?,
                attempt: u32::try_from(tnum(tget(m, "att")?, "att")?).map_err(|_| "att overflow")?,
                input_digest: tstr(tget(m, "in")?, "in")?,
            }
        }
        "effect" => {
            if m.len() != 5 { return Err("v2 wal effect field count".into()); }
            let ek = tstr(tget(m, "ek")?, "ek")?;
            WalRecord::EffectAck {
                run_id: tstr(tget(m, "run")?, "run")?,
                step_id: tstr(tget(m, "step")?, "step")?,
                effect: parse_effect_key(&ek).ok_or("bad ek")?,
            }
        }
        "outcome" => {
            if m.len() != 9 { return Err("v2 wal outcome field count".into()); }
            WalRecord::Outcome {
                run_id: tstr(tget(m, "run")?, "run")?,
                step_id: tstr(tget(m, "step")?, "step")?,
                attempt: u32::try_from(tnum(tget(m, "att")?, "att")?).map_err(|_| "att overflow")?,
                status: tstr(tget(m, "st")?, "st")?,
                error_code: topt_str(tget(m, "err")?, "err")?,
                skip_reason: topt_str(tget(m, "skip")?, "skip")?,
                fields_digest: topt_str(tget(m, "fd")?, "fd")?,
            }
        }
        "audit" => {
            if m.len() != 15 { return Err("v2 wal audit field count".into()); }
            WalRecord::Audit(AuditEvent {
                seq: tnum(tget(m, "seq")?, "seq")?,
                run_id: tstr(tget(m, "run")?, "run")?,
                step_id: tstr(tget(m, "step")?, "step")?,
                attempt: u32::try_from(tnum(tget(m, "att")?, "att")?).map_err(|_| "att overflow")?,
                scenario: topt_str(tget(m, "scen")?, "scen")?,
                seed: match tget(m, "seed")? { TVal::Num(n) => Some(*n), TVal::Null => None, _ => return Err("seed".into()) },
                fault: topt_str(tget(m, "fault")?, "fault")?,
                outcome_status: Box::leak(tstr(tget(m, "st")?, "st")?.into_boxed_str()),
                error_code: None, // ErrCode 反查由消费方按需补充（parse 层不引入错误码表依赖）
                skip_reason: None,
                effect_key: match tget(m, "ek")? { TVal::Str(s) => Some(parse_effect_key(&s).ok_or("bad ek")?), TVal::Null => None, _ => return Err("ek".into()) },
                input_digest: topt_str(tget(m, "in")?, "in")?,
                output_digest: topt_str(tget(m, "out")?, "out")?,
                prev_hash: prev.clone(),
            })
        }
        _ => return Err(format!("v2 wal unknown kind: {k}")),
    };
    // canonical 双闸
    let re = wal_record_canonical_v2(&rec, &prev)?;
    if re != t {
        return Err("v2 wal not canonical".into());
    }
    Ok((rec, prev))
}

fn parse_effect_key(s: &str) -> Option<EffectKey> {
    let parts: Vec<&str> = s.split('#').collect();
    if parts.len() != 3 { return None; }
    Some(EffectKey {
        plan_fingerprint: parts[0].to_string(),
        effect_index: parts[1].parse().ok()?,
        input_digest: parts[2].to_string(),
    })
}

// ── v2 WAL 写入器（追加模式+fsync；链公式同 v1）───────────────────

pub struct WalV2 {
    file: std::fs::File,
    pub last_hash: String,
    pub seq: u64,
}

impl WalV2 {
    pub fn open(path: &std::path::Path) -> Result<Self, String> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("open wal v2: {e}"))?;
        let mut last_hash = "0".repeat(64);
        let mut seq = 0u64;
        if let Ok(f) = std::fs::File::open(path) {
            // 读全部 v2 帧取末帧 T 的 sha256 续链
            let data: Vec<u8> = {
                let mut buf = Vec::new();
                let mut br = BufReader::new(f);
                br.read_to_end(&mut buf).map_err(|e| e.to_string())?;
                buf
            };
            let mut i = 0usize;
            let mut last_t: Option<Vec<u8>> = None;
            while i < data.len() {
                if data.len() - i < 10 { return Err("truncated v2 wal frame".into()); }
                let n64 = u64::from_be_bytes(data[i..i + 8].try_into().unwrap());
                let n = usize::try_from(n64).map_err(|_| "overflow")?;
                let end = i.checked_add(10).and_then(|v| v.checked_add(n)).and_then(|v| v.checked_add(1))
                    .ok_or("overflow")?;
                if data.len() < end || data[end - 1] != b'\n' { return Err("bad v2 wal boundary".into()); }
                if data[i + 8] != FT_WAL || data[i + 9] != 0x02 {
                    return Err("v2 wal frame format drift".into());
                }
                last_t = Some(data[i + 10..i + 10 + n].to_vec());
                seq += 1;
                i = end;
            }
            if let Some(t) = last_t {
                last_hash = sha256_hex(&t);
            }
        }
        Ok(WalV2 { file, last_hash, seq })
    }

    /// 追加一条（原子序：算帧→写→fsync）；返回新链哈希。
    pub fn append(&mut self, rec: &WalRecord) -> Result<String, String> {
        let frame = wal_frame_v2(rec, &self.last_hash)?;
        let t_len = frame.len() - 11; // 8 len + FT + codec + LF
        let t = frame[10..10 + t_len].to_vec();
        self.seq += 1;
        self.last_hash = sha256_hex(&t);
        self.file.write_all(&frame).map_err(|e| e.to_string())?;
        self.file.sync_data().map_err(|e| e.to_string())?;
        Ok(self.last_hash.clone())
    }
}

// ── v2 WAL 只读验证+恢复（新 v2；旧 JSON WAL 走 wal.rs 原函数）────────

/// v2 WAL 全量解码+链验证。返回 (记录列表, 每帧 T)——供 recover_v2。
pub fn read_wal_v2(path: &std::path::Path) -> Result<Vec<(WalRecord, String)>, String> {
    let data = std::fs::read(path).map_err(|e| format!("read wal v2: {e}"))?;
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut expect_prev = "0".repeat(64);
    while i < data.len() {
        if data.len() - i < 10 { return Err("truncated v2 wal frame".into()); }
        let n64 = u64::from_be_bytes(data[i..i + 8].try_into().unwrap());
        let n = usize::try_from(n64).map_err(|_| "overflow")?;
        if n > 16 * 1024 * 1024 { return Err("v2 wal frame exceeds 16MiB".into()); }
        let end = i.checked_add(10).and_then(|v| v.checked_add(n)).and_then(|v| v.checked_add(1))
            .ok_or("overflow")?;
        if data.len() < end || data[end - 1] != b'\n' { return Err("bad v2 wal boundary".into()); }
        if data[i + 8] != FT_WAL { return Err("bad v2 wal FT".into()); }
        if data[i + 9] != 0x02 { return Err("v2 wal mixed with v1 rejected".into()); }
        let t = &data[i + 10..i + 10 + n];
        let (rec, prev) = parse_wal_record_v2(t)?;
        if prev != expect_prev {
            return Err(format!("v2 wal chain break at frame {}", out.len()));
        }
        expect_prev = sha256_hex(t);
        out.push((rec, prev));
        i = end;
    }
    Ok(out)
}

/// v2 恢复（语义同 wal.rs recover；结构化解析替代 substring grab）。
pub fn recover_v2(records: &[(WalRecord, String)]) -> Recovered {
    let mut r = Recovered::default();
    let mut intent_seen: BTreeMap<String, u32> = BTreeMap::new();
    for (rec, _) in records {
        match rec {
            WalRecord::Intent { step_id, attempt, .. } => {
                intent_seen.insert(step_id.clone(), *attempt);
            }
            WalRecord::EffectAck { effect, .. } => {
                r.acked_effects.insert(effect.clone());
            }
            WalRecord::Outcome { step_id, .. } => {
                r.done.insert(step_id.clone());
                intent_seen.remove(step_id);
            }
            WalRecord::Audit(_) => {}
        }
    }
    r.in_flight = intent_seen.into_iter().collect();
    r
}

/// 旧 JSON WAL（只读）→ 恢复 → 写成全新 v2 WAL（规格 §四.1「恢复原子生成新 v2」）。
pub fn migrate_wal_v1_to_v2(src: &std::path::Path, dst: &std::path::Path) -> Result<usize, String> {
    if dst.exists() {
        return Err("target exists".into());
    }
    let lines: Vec<String> = std::io::BufReader::new(
        std::fs::File::open(src).map_err(|e| format!("open src: {e}"))?,
    )
    .lines()
    .flatten()
    .filter(|l| !l.trim().is_empty())
    .collect();
    crate::kernel::wal::verify_chain(&lines).map_err(|i| format!("v1 chain break at {i}"))?;
    // v1 行→WalRecord（复用 wal.rs recover 的字段抽取；语义等价重编码 v2）
    let mut w = WalV2::open(dst)?;
    let mut count = 0usize;
    for line in &lines {
        let rec = v1_line_to_record(line).ok_or("unrecognized v1 wal line")?;
        w.append(&rec)?;
        count += 1;
    }
    Ok(count)
}

fn v1_line_to_record(line: &str) -> Option<WalRecord> {
    // 最小充分解析（v1 wal_to_json 的逆；audit 的 seq/event/detail 从行内抓取）
    if line.contains("\"k\":\"intent\"") {
        let att: u32 = grab_num(line, "att")?;
        return Some(WalRecord::Intent {
            run_id: grab(line, "run")?,
            step_id: grab(line, "step")?,
            attempt: att,
            input_digest: grab(line, "in")?,
        });
    }
    if line.contains("\"k\":\"effect\"") {
        return Some(WalRecord::EffectAck {
            run_id: grab(line, "run")?,
            step_id: grab(line, "step")?,
            effect: parse_effect_key(&grab(line, "ek")?)?,
        });
    }
    if line.contains("\"k\":\"outcome\"") {
        let err = grab(line, "err");
        let skip = grab(line, "skip");
        let fd = grab(line, "fd");
        return Some(WalRecord::Outcome {
            run_id: grab(line, "run")?,
            step_id: grab(line, "step")?,
            attempt: grab_num(line, "att")?,
            status: grab(line, "st")?,
            error_code: err,
            skip_reason: skip,
            fields_digest: fd,
        });
    }
    if line.contains("\"k\":\"audit\"") {
        // v1 audit 行只有 k+prev_hash（细节未入 v1 WAL——TOON 版完整化）
        return Some(WalRecord::Audit(crate::kernel::types::AuditEvent {
            seq: 0, run_id: String::new(), step_id: String::new(), attempt: 0,
            scenario: None, seed: None, fault: None,
            outcome_status: "audit",
            error_code: None, skip_reason: None, effect_key: None,
            input_digest: None, output_digest: None,
            prev_hash: String::new(),
        }));
    }
    None
}

fn grab(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
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
    let pat = format!("\"{key}\":");
    let i = line.find(&pat)? + pat.len();
    let rest = &line[i..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}
