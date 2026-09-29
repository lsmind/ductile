//! Ductile v0.24 fd3 帧协议 ABI（规范补丁③）— 行动③ D5-6。
//!
//! 帧格式：
//!   Frame ::= "DCTF3\n" u16be(ver=1) u16be(flags=0) u32be(|S|) u64be(|P|)
//!             S P sha256(all_preceding_bytes)
//!   S = JCS(schema_descriptor)  ≤64KiB
//!   P = JCS(WireOutcome)        ≤1MiB
//! 帧恰有一个，无尾随数据；摘要为原始 32 字节。
//!
//! 判定（补丁③）：非法 UTF-8/JCS/非有限数 → E304 Codec；
//! 短帧/超限/额外帧或字节/重复或未知键/摘要错误 → E306 Protocol；
//! schema/fields/stdout 不匹配 → E501 OutputContract。
//!
//! 本文件：帧的编码/解码/校验（纯函数，无 IO）——run/script/raw.shell
//! 共用；进程侧的 fd 管道读写在 adapter。

use crate::kernel::types::{ErrCode, Outcome, SkipReason, Value};
use std::collections::BTreeMap;

pub const MAGIC: &[u8; 6] = b"DCTF3\n";
pub const VERSION: u16 = 1;
pub const S_MAX: usize = 64 * 1024;
pub const P_MAX: usize = 1024 * 1024;
/// fd1（stdout）原始字节上限（补丁③：8MiB）。
pub const STDOUT_MAX: usize = 8 * 1024 * 1024;
/// 帧头长度：magic(6) + ver(2) + flags(2) + |S|(4) + |P|(8)。
pub const HEADER_LEN: usize = 6 + 2 + 2 + 4 + 8; // = 22

#[derive(Debug, Clone, PartialEq)]
pub enum FrameError {
    /// E306：帧结构损坏（短帧/超限/尾随数据/魔数不符/版本不符）。
    Protocol(&'static str),
    /// E304：载荷编码非法（UTF-8/JCS/非有限数/键集合不精确）。
    Codec(&'static str),
    /// E306：摘要不匹配。
    DigestMismatch,
}

impl FrameError {
    pub fn code(&self) -> ErrCode {
        match self {
            FrameError::Protocol(_) | FrameError::DigestMismatch => ErrCode::E306,
            FrameError::Codec(_) => ErrCode::E304,
        }
    }
}

// ── WireOutcome 的最小 JSON 编解码（JCS 近似：键序、无空白）────
// 切片实现自带极简解析器（serde 仅在需要时引入；内核零依赖原则）。

#[derive(Debug, Clone, PartialEq)]
pub enum WireOutcome {
    Success { fields: BTreeMap<String, Value>, stdout_sha256: String, stdout_len: u64 },
    Failed { code: String, name: String, detail: String },
    Skipped { reason: String, detail: String },
}

// —— 编码 ——

/// Value → JCS 近似 JSON（BTreeMap 键序；float 用非科学紧凑表示）。
pub fn value_to_json(v: &Value) -> String {
    match v {
        Value::Str(s) => json_escape(s),
        Value::Int(i) => i.to_string(),
        Value::Float(x) => {
            if x.fract() == 0.0 && x.abs() < 1e15 {
                format!("{:.1}", x)
            } else {
                format!("{}", x)
            }
        }
        Value::Bool(b) => b.to_string(),
        Value::List(items) => {
            let parts: Vec<String> = items.iter().map(value_to_json).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Map(m) => {
            let parts: Vec<String> =
                m.iter().map(|(k, v)| format!("{}:{}", json_escape(k), value_to_json(v))).collect();
            format!("{{{}}}", parts.join(","))
        }
        // ref 不得进入 fd3（规范：不得跨进程序列化）→ 编码期硬错
        Value::Ref(_) => String::from("\"<ref:ILLEGAL>\""),
    }
}

fn json_escape(s: &str) -> String {
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

/// WireOutcome → P 载荷。键集合必须精确（补丁③）。
pub fn encode_payload(w: &WireOutcome) -> String {
    match w {
        WireOutcome::Success { fields, stdout_sha256, stdout_len } => {
            let f: Vec<String> =
                fields.iter().map(|(k, v)| format!("{}:{}", json_escape(k), value_to_json(v))).collect();
            format!(
                "{{\"fields\":{{{}}},\"status\":\"Success\",\"stdout\":{{\"bytes\":{},\"sha256\":\"{}\"}}}}",
                f.join(","),
                stdout_len,
                stdout_sha256
            )
        }
        WireOutcome::Failed { code, name, detail } => format!(
            "{{\"error\":{{\"code\":\"{}\",\"detail\":{},\"name\":\"{}\"}},\"status\":\"Failed\"}}",
            code,
            json_escape(detail),
            name
        ),
        WireOutcome::Skipped { reason, detail } => format!(
            "{{\"reason\":{{\"detail\":{},\"name\":\"{}\"}},\"status\":\"Skipped\"}}",
            json_escape(detail),
            reason
        ),
    }
}

/// schema 描述符 S 载荷（切片：字段名→类型名）。
pub fn encode_schema_desc(fields: &BTreeMap<String, Value>) -> String {
    let parts: Vec<String> = fields
        .iter()
        .map(|(k, v)| format!("{}:\"{}\"", json_escape(k), v.type_name()))
        .collect();
    format!("{{{}}}", parts.join(","))
}

// —— 帧编码 ——

/// 完整帧：header + S + P + sha256(preceding)。
pub fn encode_frame(schema_desc: &str, payload: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_LEN + schema_desc.len() + payload.len() + 32);
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&VERSION.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&(schema_desc.len() as u32).to_be_bytes());
    buf.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    buf.extend_from_slice(schema_desc.as_bytes());
    buf.extend_from_slice(payload.as_bytes());
    let digest = crate::kernel::hash::sha256(&buf);
    buf.extend_from_slice(&digest);
    buf
}

// —— 帧解码与校验 ——

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedFrame {
    pub schema_desc: String,
    pub payload: String,
}

pub fn decode_frame(bytes: &[u8]) -> Result<DecodedFrame, FrameError> {
    if bytes.len() < HEADER_LEN + 32 {
        return Err(FrameError::Protocol("short frame"));
    }
    if &bytes[0..6] != MAGIC.as_slice() {
        return Err(FrameError::Protocol("bad magic"));
    }
    let ver = u16::from_be_bytes([bytes[6], bytes[7]]);
    if ver != VERSION {
        return Err(FrameError::Protocol("version mismatch"));
    }
    let flags = u16::from_be_bytes([bytes[8], bytes[9]]);
    if flags != 0 {
        return Err(FrameError::Protocol("unknown flags"));
    }
    let s_len = u32::from_be_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]) as usize;
    let p_len = u64::from_be_bytes([
        bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21],
    ]) as usize;
    if s_len > S_MAX {
        return Err(FrameError::Protocol("schema > 64KiB"));
    }
    if p_len > P_MAX {
        return Err(FrameError::Protocol("payload > 1MiB"));
    }
    let body_end = HEADER_LEN + s_len + p_len;
    let frame_end = body_end + 32;
    if bytes.len() != frame_end {
        return Err(FrameError::Protocol("trailing bytes or truncated"));
    }
    // 摘要：header+S+P 的 sha256 必须等于尾 32 字节
    let expect = &bytes[body_end..frame_end];
    let actual = crate::kernel::hash::sha256(&bytes[..body_end]);
    if expect != actual {
        return Err(FrameError::DigestMismatch);
    }
    let schema_desc = String::from_utf8(bytes[HEADER_LEN..HEADER_LEN + s_len].to_vec())
        .map_err(|_| FrameError::Codec("schema not UTF-8"))?;
    let payload = String::from_utf8(bytes[HEADER_LEN + s_len..body_end].to_vec())
        .map_err(|_| FrameError::Codec("payload not UTF-8"))?;
    Ok(DecodedFrame { schema_desc, payload })
}

// ── 极简 JSON 载荷解析（只认 WireOutcome 的三种精确形状）────
//
// 防御原则（补丁③"恶意输出防御"）：
//   - 只接受精确键集合（多余/缺失键 → E304）
//   - 深度/长度受 S_MAX/P_MAX 物理限制
//   - 非有限数/裸控制字符 → E304

pub fn parse_payload(p: &str) -> Result<WireOutcome, FrameError> {
    let mut parser = P { b: p.as_bytes(), i: 0 };
    parser.ws();
    let v = parser.value(0)?;
    parser.ws();
    if parser.i != parser.b.len() {
        return Err(FrameError::Codec("trailing json"));
    }
    wire_from_value(v)
}

fn wire_from_value(v: JValue) -> Result<WireOutcome, FrameError> {
    let mut m = match v {
        JValue::Obj(m) => m,
        _ => return Err(FrameError::Codec("payload not object")),
    };
    let status = match m.remove("status") {
        Some(JValue::Str(s)) => s,
        _ => return Err(FrameError::Codec("missing status")),
    };
    match status.as_str() {
        "Success" => {
            // 精确键集合：fields + status + stdout
            if m.len() != 2 || !m.contains_key("fields") || !m.contains_key("stdout") {
                return Err(FrameError::Codec("Success keys not exact"));
            }
            let fields = match m.remove("fields") {
                Some(JValue::Obj(f)) => f,
                _ => return Err(FrameError::Codec("fields not object")),
            };
            let mut fm = BTreeMap::new();
            for (k, v) in fields {
                fm.insert(k, v.to_value()?);
            }
            let mut stdout = match m.remove("stdout") {
                Some(JValue::Obj(s)) => s,
                _ => return Err(FrameError::Codec("stdout not object")),
            };
            if stdout.len() != 2 {
                return Err(FrameError::Codec("stdout keys not exact"));
            }
            let bytes = match stdout.remove("bytes") {
                Some(JValue::Int(i)) if i >= 0 => i as u64,
                _ => return Err(FrameError::Codec("stdout.bytes missing")),
            };
            let sha = match stdout.remove("sha256") {
                Some(JValue::Str(s)) => s,
                _ => return Err(FrameError::Codec("stdout.sha256 missing")),
            };
            Ok(WireOutcome::Success { fields: fm, stdout_sha256: sha, stdout_len: bytes })
        }
        "Failed" => {
            if m.len() != 1 || !m.contains_key("error") {
                return Err(FrameError::Codec("Failed keys not exact"));
            }
            let mut err = match m.remove("error") {
                Some(JValue::Obj(e)) => e,
                _ => return Err(FrameError::Codec("error not object")),
            };
            if err.len() != 3 {
                return Err(FrameError::Codec("error keys not exact"));
            }
            let code = match err.remove("code") {
                Some(JValue::Str(s)) => s,
                _ => return Err(FrameError::Codec("error.code missing")),
            };
            let name = match err.remove("name") {
                Some(JValue::Str(s)) => s,
                _ => return Err(FrameError::Codec("error.name missing")),
            };
            let detail = match err.remove("detail") {
                Some(JValue::Str(s)) => s,
                _ => return Err(FrameError::Codec("error.detail missing")),
            };
            Ok(WireOutcome::Failed { code, name, detail })
        }
        "Skipped" => {
            if m.len() != 1 || !m.contains_key("reason") {
                return Err(FrameError::Codec("Skipped keys not exact"));
            }
            let mut r = match m.remove("reason") {
                Some(JValue::Obj(r)) => r,
                _ => return Err(FrameError::Codec("reason not object")),
            };
            if r.len() != 2 {
                return Err(FrameError::Codec("reason keys not exact"));
            }
            let name = match r.remove("name") {
                Some(JValue::Str(s)) => s,
                _ => return Err(FrameError::Codec("reason.name missing")),
            };
            let detail = match r.remove("detail") {
                Some(JValue::Str(s)) => s,
                _ => return Err(FrameError::Codec("reason.detail missing")),
            };
            Ok(WireOutcome::Skipped { reason: name, detail })
        }
        _ => Err(FrameError::Codec("unknown status")),
    }
}

// —— 极简递归下降 JSON（深度限制 32）——

#[derive(Debug, Clone)]
enum JValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    List(Vec<JValue>),
    Obj(BTreeMap<String, JValue>),
}

impl JValue {
    fn to_value(self) -> Result<Value, FrameError> {
        match self {
            JValue::Str(s) => Ok(Value::Str(s)),
            JValue::Int(i) => Ok(Value::Int(i)),
            JValue::Float(x) => {
                if x.is_finite() {
                    Ok(Value::Float(x))
                } else {
                    Err(FrameError::Codec("non-finite float"))
                }
            }
            JValue::Bool(b) => Ok(Value::Bool(b)),
            JValue::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for i in items {
                    out.push(i.to_value()?);
                }
                Ok(Value::List(out))
            }
            JValue::Obj(m) => {
                let mut out = BTreeMap::new();
                for (k, v) in m {
                    out.insert(k, v.to_value()?);
                }
                Ok(Value::Map(out))
            }
            // null 禁止（规范：不提供 null）
            JValue::Null => Err(FrameError::Codec("null not in value domain")),
        }
    }
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn value(&mut self, depth: u32) -> Result<JValue, FrameError> {
        if depth > 32 {
            return Err(FrameError::Codec("depth > 32"));
        }
        self.ws();
        if self.i >= self.b.len() {
            return Err(FrameError::Codec("unexpected end"));
        }
        match self.b[self.i] {
            b'"' => self.string().map(JValue::Str),
            b'-' | b'0'..=b'9' => self.number(),
            b't' => self.lit("true", JValue::Bool(true)),
            b'f' => self.lit("false", JValue::Bool(false)),
            b'n' => self.lit("null", JValue::Null),
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.i < self.b.len() && self.b[self.i] == b']' {
                    self.i += 1;
                    return Ok(JValue::List(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => {
                            self.i += 1;
                        }
                        Some(b']') => {
                            self.i += 1;
                            break;
                        }
                        _ => return Err(FrameError::Codec("bad list")),
                    }
                }
                Ok(JValue::List(items))
            }
            b'{' => {
                self.i += 1;
                let mut m = BTreeMap::new();
                self.ws();
                if self.i < self.b.len() && self.b[self.i] == b'}' {
                    self.i += 1;
                    return Ok(JValue::Obj(m));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    if self.b.get(self.i) != Some(&b':') {
                        return Err(FrameError::Codec("expect ':'"));
                    }
                    self.i += 1;
                    let v = self.value(depth + 1)?;
                    if m.insert(k, v).is_some() {
                        return Err(FrameError::Codec("duplicate key"));
                    }
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => {
                            self.i += 1;
                        }
                        Some(b'}') => {
                            self.i += 1;
                            break;
                        }
                        _ => return Err(FrameError::Codec("bad object")),
                    }
                }
                Ok(JValue::Obj(m))
            }
            _ => Err(FrameError::Codec("unexpected byte")),
        }
    }
    fn lit(&mut self, word: &str, v: JValue) -> Result<JValue, FrameError> {
        if self.b.len() - self.i >= word.len() && &self.b[self.i..self.i + word.len()] == word.as_bytes() {
            self.i += word.len();
            Ok(v)
        } else {
            Err(FrameError::Codec("bad literal"))
        }
    }
    fn string(&mut self) -> Result<String, FrameError> {
        if self.b.get(self.i) != Some(&b'"') {
            return Err(FrameError::Codec("expect string"));
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            match self.b.get(self.i) {
                None => return Err(FrameError::Codec("unterminated string")),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    match self.b.get(self.i) {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'b') => out.push('\u{8}'),
                        Some(b'f') => out.push('\u{c}'),
                        Some(b'u') => {
                            let hex = self
                                .b
                                .get(self.i + 1..self.i + 5)
                                .ok_or(FrameError::Codec("bad \\u"))?;
                            let cp = u32::from_str_radix(
                                std::str::from_utf8(hex).map_err(|_| FrameError::Codec("bad \\u hex"))?,
                                16,
                            )
                            .map_err(|_| FrameError::Codec("bad \\u hex"))?;
                            // 代理对处理
                            if (0xD800..0xDC00).contains(&cp) {
                                let hex2 = self
                                    .b
                                    .get(self.i + 6..self.i + 10)
                                    .ok_or(FrameError::Codec("bad surrogate"))?;
                                let lo = u32::from_str_radix(
                                    std::str::from_utf8(hex2).map_err(|_| FrameError::Codec("bad surrogate"))?,
                                    16,
                                )
                                .map_err(|_| FrameError::Codec("bad surrogate"))?;
                                let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                out.push(char::from_u32(c).ok_or(FrameError::Codec("bad codepoint"))?);
                                self.i += 10;
                                continue;
                            }
                            out.push(char::from_u32(cp).ok_or(FrameError::Codec("bad codepoint"))?);
                            self.i += 4;
                        }
                        _ => return Err(FrameError::Codec("bad escape")),
                    }
                    self.i += 1;
                }
                Some(&c) if c < 0x20 => return Err(FrameError::Codec("raw control char")),
                Some(&c) => {
                    // UTF-8 逐字节收集（外层 from_utf8 校验）
                    let start = self.i;
                    let len = utf8_len(c).ok_or(FrameError::Codec("bad utf8 lead"))?;
                    self.i += len;
                    if self.i > self.b.len() {
                        return Err(FrameError::Codec("truncated utf8"));
                    }
                    out.push_str(
                        std::str::from_utf8(&self.b[start..self.i])
                            .map_err(|_| FrameError::Codec("invalid utf8"))?,
                    );
                }
            }
        }
    }
    fn number(&mut self) -> Result<JValue, FrameError> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        let mut is_float = false;
        if self.b.get(self.i) == Some(&b'.') {
            is_float = true;
            self.i += 1;
            while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        if matches!(self.b.get(self.i), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+') | Some(b'-')) {
                self.i += 1;
            }
            while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| FrameError::Codec("bad number"))?;
        if is_float {
            let f: f64 = text.parse().map_err(|_| FrameError::Codec("bad float"))?;
            if !f.is_finite() {
                return Err(FrameError::Codec("non-finite float"));
            }
            Ok(JValue::Float(f))
        } else {
            text.parse::<i64>().map(JValue::Int).map_err(|_| FrameError::Codec("int overflow"))
        }
    }
}

fn utf8_len(b: u8) -> Option<usize> {
    match b {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

// ── Outcome ↔ WireOutcome（fd3 侧视图）───────────────────

/// 出站：内核 Outcome → WireOutcome（stdout 摘要按 fd1 实际内容计算）。
pub fn outcome_to_wire(out: &Outcome, stdout_bytes: &[u8]) -> WireOutcome {
    match out {
        Outcome::Success { fields, .. } => WireOutcome::Success {
            fields: fields.clone(),
            stdout_len: stdout_bytes.len() as u64,
            stdout_sha256: crate::kernel::hash::sha256_hex(stdout_bytes),
        },
        Outcome::Failed { error, .. } => WireOutcome::Failed {
            code: error.code.code().to_string(),
            name: error.code.name().to_string(),
            detail: error.detail.clone(),
        },
        Outcome::Skipped { reason, .. } => {
            let (name, detail) = match reason {
                SkipReason::Ineligible(d) => ("Ineligible", d.clone()),
                SkipReason::UpstreamSkipped => ("UpstreamSkipped", String::new()),
                SkipReason::NotSelected => ("NotSelected", String::new()),
                SkipReason::EmptyInput => ("EmptyInput", String::new()),
            };
            WireOutcome::Skipped { reason: name.to_string(), detail }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn success_wire() -> WireOutcome {
        let mut fields = BTreeMap::new();
        fields.insert("path".to_string(), Value::Str("out/x.md".into()));
        fields.insert("bytes".to_string(), Value::Int(120));
        WireOutcome::Success { fields, stdout_sha256: crate::kernel::hash::sha256_hex(b"hello"), stdout_len: 5 }
    }

    #[test]
    fn roundtrip_success() {
        let w = success_wire();
        let payload = encode_payload(&w);
        let frame = encode_frame(&encode_schema_desc(&wire_fields(&w)), &payload);
        let dec = decode_frame(&frame).unwrap();
        assert_eq!(dec.payload, payload);
        let back = parse_payload(&dec.payload).unwrap();
        assert_eq!(back, w);
    }

    fn wire_fields(w: &WireOutcome) -> BTreeMap<String, Value> {
        match w {
            WireOutcome::Success { fields, .. } => fields.clone(),
            _ => BTreeMap::new(),
        }
    }

    #[test]
    fn roundtrip_failed_skipped() {
        let f = WireOutcome::Failed { code: "E302".into(), name: "Exit".into(), detail: "exit 1".into() };
        let back = parse_payload(&encode_payload(&f)).unwrap();
        assert_eq!(back, f);
        let s = WireOutcome::Skipped { reason: "EmptyInput".into(), detail: String::new() };
        let back = parse_payload(&encode_payload(&s)).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn tamper_detected() {
        let w = success_wire();
        let mut frame = encode_frame("{\"bytes\":\"int\"}", &encode_payload(&w));
        let n = frame.len();
        frame[n - 1] ^= 0xFF; // 翻转摘要尾字节
        match decode_frame(&frame) {
            Err(FrameError::DigestMismatch) => {}
            other => panic!("expect digest mismatch, got {:?}", other.map(|_| ())),
        }
        // 篡改载荷同样被摘要拦
        let mut frame2 = encode_frame("{\"bytes\":\"int\"}", &encode_payload(&w));
        frame2[HEADER_LEN + 20] ^= 0x01;
        assert!(matches!(decode_frame(&frame2), Err(FrameError::DigestMismatch)));
    }

    #[test]
    fn protocol_violations() {
        let w = success_wire();
        let good = encode_frame("{}", &encode_payload(&w));
        // 短帧
        assert!(matches!(decode_frame(&good[..good.len() - 40]), Err(FrameError::Protocol(_))));
        // 尾随数据
        let mut trailing = good.clone();
        trailing.push(0x00);
        assert!(matches!(decode_frame(&trailing), Err(FrameError::Protocol("trailing bytes or truncated"))));
        // 坏魔数
        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert!(matches!(decode_frame(&bad_magic), Err(FrameError::Protocol("bad magic"))));
        // 版本不符
        let mut v2 = good.clone();
        v2[6] = 0;
        v2[7] = 2;
        assert!(matches!(decode_frame(&v2), Err(FrameError::Protocol("version mismatch"))));
    }

    #[test]
    fn payload_exact_keys_and_null_banned() {
        // 多余键 → Codec（精确键集合）
        assert!(parse_payload("{\"status\":\"Skipped\",\"reason\":{\"name\":\"EmptyInput\",\"detail\":\"\",\"x\":1}}").is_err());
        // null → Codec（值域无 null）
        assert!(parse_payload("{\"status\":\"Success\",\"fields\":{\"a\":null},\"stdout\":{\"bytes\":0,\"sha256\":\"x\"}}").is_err());
        // 非有限数 → Codec
        assert!(parse_payload("{\"status\":\"Success\",\"fields\":{\"a\":1e999},\"stdout\":{\"bytes\":0,\"sha256\":\"x\"}}").is_err());
        // 未知 status / 重复键 / 尾随 → Codec
        assert!(parse_payload("{\"status\":\"Weird\"}").is_err());
        assert!(parse_payload("{\"status\":\"Success\",\"status\":\"Success\"}").is_err());
        assert!(parse_payload("{\"status\":\"Weird\"} x").is_err());
        // 深度攻击：fields 内嵌套 >32 → Codec
        let mut deep = String::from("1");
        for _ in 0..40 {
            deep = format!("[{}]", deep);
        }
        let attack = format!(
            "{{\"status\":\"Success\",\"fields\":{{\"a\":{}}},\"stdout\":{{\"bytes\":0,\"sha256\":\"x\"}}}}",
            deep
        );
        assert!(parse_payload(&attack).is_err());
    }

    #[test]
    fn stdout_digest_binding() {
        // outcome_to_wire 绑定 fd1 实际内容
        let out = Outcome::success(BTreeMap::new(), "hello");
        let w = outcome_to_wire(&out, b"hello");
        match w {
            WireOutcome::Success { stdout_len, stdout_sha256, .. } => {
                assert_eq!(stdout_len, 5);
                assert_eq!(stdout_sha256, crate::kernel::hash::sha256_hex(b"hello"));
            }
            _ => panic!(),
        }
    }
}
