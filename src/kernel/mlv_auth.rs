//! MLV 账本入口 Ed25519 域分离签名（冻结规格 v1.1，docs/sonet_mlv_ed25519_spec.md）。
//!
//! 范围锁：不动 14-op 词表 / phase 唯一键 / effect_key / f4、f5 既有断言。
//! legacy 路径（无密钥信封）零改动；ed25519 模式为增量分流。
//!
//! 关键裁定（确认审后冻结）：
//! - sig/sig_key_id/sig_trust_seq 住信封内层（BTreeMap），不进 23 字段 CANON_ORDER。
//! - 信任帧=独立帧类型：ed25519 账本每帧 u64be(N)‖frame_type(1B)‖J‖LF；
//!   legacy 账本无该字节（J 以 '{' 开头，与 0x00/0x01 天然区分）。
//! - 业务动词签名供给=必选 --signing-key <file>；缺参/钥不在活动集/已吊销=硬错。
//! - 错误 detail 三分列：signature-invalid / signature-key-revoked / trust-chain-invalid
//!   （外层错误码均 E423-env，语义不变）。

use crate::kernel::hash::sha256;
use crate::kernel::mlv::{json_esc, parse_flat_object, LedgerRecord};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use std::collections::BTreeMap;
use std::path::Path;

// ── 域分隔常量（规格 §一/§二/§四）────────────────────────────

pub const DOMAIN_KEYID: &str = "MLV31-KEYID";
pub const DOMAIN_TRUST: &str = "MLV31-TRUST";
pub const DOMAIN_TRUST_FRAME: &str = "MLV31-TRUST-FRAME";
pub const DOMAIN_SIG: &str = "MLV31-SIG-v1";
pub const AUTH_SCHEMA: &str = "mlv_auth_v1";

/// E423-env detail 三分列（规格 §三.4）。
pub const DETAIL_SIG_INVALID: &str = "signature-invalid";
pub const DETAIL_KEY_REVOKED: &str = "signature-key-revoked";
pub const DETAIL_TRUST_CHAIN: &str = "trust-chain-invalid";

// ── 密钥模型（规格 §一）──────────────────────────────────────

/// key_digest = SHA256("MLV31-KEYID" ‖ pk)；key_id = digest[0..16] hex（32 字符）。
pub fn key_id_of(public_key: &[u8; 32]) -> (String, String) {
    let mut buf = Vec::with_capacity(DOMAIN_KEYID.len() + 32);
    buf.extend_from_slice(DOMAIN_KEYID.as_bytes());
    buf.extend_from_slice(public_key);
    let digest = sha256(&buf);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    (hex[..32].to_string(), hex)
}

/// 生成密钥对（getrandom 直依赖，规格 §〇.2）。
pub fn generate_keypair() -> Result<([u8; 32], [u8; 32]), String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| format!("getrandom: {e}"))?;
    let sk = SigningKey::from_bytes(&seed);
    Ok((seed, sk.verifying_key().to_bytes()))
}

/// keygen 落盘：目录 0700，.secret 0600（32B seed），.pub 0644（32B pk）；
/// 临时文件原子发布；同名已存在=硬错（规格 §一.3-4）。
pub fn keygen_write(keydir: &Path) -> Result<(String, std::path::PathBuf, std::path::PathBuf), String> {
    use std::os::unix::fs::PermissionsExt;
    let (seed, pk) = generate_keypair()?;
    let (key_id, keyid64) = key_id_of(&pk);
    std::fs::create_dir_all(keydir).map_err(|e| format!("mkdir: {e}"))?;
    std::fs::set_permissions(keydir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("chmod dir: {e}"))?;
    let secret = keydir.join(format!("ed25519-{keyid64}.secret"));
    let public = keydir.join(format!("ed25519-{keyid64}.pub"));
    if secret.exists() || public.exists() {
        return Err(format!("key file already exists: {}", secret.display()));
    }
    let tmp_s = keydir.join(format!(".tmp.{}.secret.{}", std::process::id(), keyid64));
    let tmp_p = keydir.join(format!(".tmp.{}.pub.{}", std::process::id(), keyid64));
    std::fs::write(&tmp_s, seed).map_err(|e| format!("write secret: {e}"))?;
    std::fs::set_permissions(&tmp_s, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod secret: {e}"))?;
    std::fs::write(&tmp_p, pk).map_err(|e| format!("write pub: {e}"))?;
    std::fs::set_permissions(&tmp_p, std::fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("chmod pub: {e}"))?;
    std::fs::rename(&tmp_s, &secret).map_err(|e| format!("rename secret: {e}"))?;
    std::fs::rename(&tmp_p, &public).map_err(|e| format!("rename pub: {e}"))?;
    Ok((key_id, secret, public))
}

/// 读 .secret（32B seed）→ SigningKey；并校验文件权限位（fail-closed）。
pub fn load_signing_key(path: &Path) -> Result<(SigningKey, String), String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| format!("read key meta: {e}"))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!("signing key permissions too open: {:o} (want 0600)", meta.permissions().mode() & 0o777));
    }
    let seed = std::fs::read(path).map_err(|e| format!("read key: {e}"))?;
    let seed: [u8; 32] = seed.as_slice().try_into().map_err(|_| "secret must be exactly 32 bytes".to_string())?;
    let sk = SigningKey::from_bytes(&seed);
    let (key_id, _) = key_id_of(&sk.verifying_key().to_bytes());
    Ok((sk, key_id))
}

// ── 认证对象与 H0（规格 §二.1）───────────────────────────────

/// auth 对象 canonical 序（固定）：schema, mode, root_key_id, root_public_key [, trust_hash]。
pub fn auth_object_json(root_key_id: &str, root_public_key_hex: &str, trust_hash: &str) -> String {
    format!(
        "{{\"schema\":\"{AUTH_SCHEMA}\",\"mode\":\"ed25519\",\"root_key_id\":\"{root_key_id}\",\"root_public_key\":\"{root_public_key_hex}\",\"trust_hash\":\"{trust_hash}\"}}"
    )
}

/// H0 = SHA256("MLV31-TRUST" ‖ len_u64be("mlv_auth_v1") ‖ auth_without_trust_hash)。
/// auth_without_trust_hash 按 canonical 序（schema,mode,root_key_id,root_public_key）。
pub fn compute_h0(root_key_id: &str, root_public_key_hex: &str) -> String {
    let body = format!(
        "{{\"schema\":\"{AUTH_SCHEMA}\",\"mode\":\"ed25519\",\"root_key_id\":\"{root_key_id}\",\"root_public_key\":\"{root_public_key_hex}\"}}"
    );
    let schema_len = AUTH_SCHEMA.len() as u64;
    let mut buf = Vec::new();
    buf.extend_from_slice(DOMAIN_TRUST.as_bytes());
    buf.extend_from_slice(&schema_len.to_be_bytes());
    buf.extend_from_slice(body.as_bytes());
    let digest = sha256(&buf);
    format!("sha256:{}", digest.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// genesis payload 解析：payload 解析出 schema="mlv_auth_v1" → ed25519 模式。
pub fn parse_auth_payload(payload: &str) -> Option<BTreeMap<String, String>> {
    let b: Vec<char> = payload.chars().collect();
    let mut i = 0usize;
    let m = parse_flat_object(&b, &mut i).ok()?;
    if m.get("schema").map(|s| s.as_str()) == Some(AUTH_SCHEMA) {
        Some(m)
    } else {
        None
    }
}

// ── 信封签名（规格 §二.2-2.3）────────────────────────────────

/// 信封 canonical 字节：BTreeMap 序（与既有信封序列化一致）序列化，排除 sig 字段。
fn envelope_canon_bytes(env: &BTreeMap<String, String>) -> Vec<u8> {
    let mut parts = Vec::new();
    for (k, v) in env {
        if k == "sig" {
            continue;
        }
        parts.push(format!("\"{}\":\"{}\"", json_esc(k), json_esc(v)));
    }
    format!("{{{}}}", parts.join(",")).into_bytes()
}

/// signing_message = "MLV31-SIG-v1" ‖ len_u64be(envelope_canon_bytes) ‖ envelope_canon_bytes。
pub fn signing_message(env: &BTreeMap<String, String>) -> Vec<u8> {
    let canon = envelope_canon_bytes(env);
    let mut msg = Vec::with_capacity(DOMAIN_SIG.len() + 8 + canon.len());
    msg.extend_from_slice(DOMAIN_SIG.as_bytes());
    msg.extend_from_slice(&(canon.len() as u64).to_be_bytes());
    msg.extend_from_slice(&canon);
    msg
}

/// 签名信封：插入 sig/sig_key_id（/sig_trust_seq），mac 置空串（规格 §二.2.3-4）。
/// 声明字段（sig_key_id/sig_trust_seq）先落位再计算签名消息——它们属于签名域；
/// sig 自身在 envelope_canon_bytes 中排除（防自引用）。
pub fn sign_envelope(
    env: &mut BTreeMap<String, String>,
    sk: &SigningKey,
    key_id: &str,
    trust_seq: Option<u64>,
) -> Result<(), String> {
    env.insert("sig_key_id".into(), key_id.into());
    if let Some(ts) = trust_seq {
        env.insert("sig_trust_seq".into(), ts.to_string());
    }
    let msg = signing_message(env);
    let sig = sk.sign(&msg).to_bytes();
    let sig_hex: String = sig.iter().map(|b| format!("{b:02x}")).collect();
    env.insert("sig".into(), sig_hex);
    Ok(())
}

/// 验签（ed25519 模式）。detail 三分列由调用方按信任态补充细分。
pub fn verify_envelope_sig(
    env: &BTreeMap<String, String>,
    public_key: &[u8; 32],
) -> Result<(), String> {
    let sig_hex = env.get("sig").ok_or_else(|| format!("{DETAIL_SIG_INVALID}: envelope missing sig"))?;
    let sig_bytes = hex_decode64(sig_hex)
        .ok_or_else(|| format!("{DETAIL_SIG_INVALID}: sig must be 128 hex chars"))?;
    let sig = Signature::from_bytes(&sig_bytes);
    let vk = VerifyingKey::from_bytes(public_key).map_err(|_| format!("{DETAIL_SIG_INVALID}: bad public key"))?;
    let msg = signing_message(env);
    vk.verify(&msg, &sig).map_err(|_| format!("{DETAIL_SIG_INVALID}: Ed25519 verify failed"))?;
    Ok(())
}

pub fn hex_decode64(s: &str) -> Option<[u8; 64]> {
    if s.len() != 128 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 64];
    for i in 0..64 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// mlv_toon v2 路径复用（同一实现的 pub 别名）。
pub fn hex_decode64_pub(s: &str) -> Option<[u8; 64]> {
    hex_decode64(s)
}

pub fn hex_decode32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

// ── 信任帧（规格 §四）────────────────────────────────────────

pub const TRUST_FRAME_ROTATE: &str = "TRUST_ROTATE";
pub const TRUST_FRAME_REVOKE: &str = "TRUST_REVOKE";

/// 信任帧 canonical 字段序（规格 §四.1.3）：
/// schema, kind, key_id, new_key_id, new_pk, trust_seq, sig, prev_trust_hash
#[derive(Debug, Clone, PartialEq)]
pub struct TrustFrame {
    pub kind: String,          // TRUST_ROTATE | TRUST_REVOKE
    pub key_id: String,        // 签发钥
    pub new_key_id: String,    // ROTATE：新钥；REVOKE：空串
    pub new_pk: String,        // ROTATE：新公钥 hex64；REVOKE：空串
    pub trust_seq: u64,        // ROTATE=轮换后版本；REVOKE=生效序号
    pub sig: String,           // 128 hex
    pub prev_trust_hash: String,
}

impl TrustFrame {
    pub fn to_json(&self) -> String {
        // trust_seq 以字符串承载（parse_flat_object 封闭类型=字符串值；canonical 自洽）
        format!(
            "{{\"schema\":\"{AUTH_SCHEMA}\",\"kind\":\"{}\",\"key_id\":\"{}\",\"new_key_id\":\"{}\",\"new_pk\":\"{}\",\"trust_seq\":\"{}\",\"sig\":\"{}\",\"prev_trust_hash\":\"{}\"}}",
            self.kind, self.key_id, self.new_key_id, self.new_pk, self.trust_seq, self.sig, self.prev_trust_hash
        )
    }

    pub fn to_frame_bytes(&self) -> Vec<u8> {
        let j = self.to_json();
        let mut f = Vec::with_capacity(9 + j.len());
        f.extend_from_slice(&(j.len() as u64).to_be_bytes());
        f.push(1u8); // frame_type = 1（信任帧）
        f.extend_from_slice(j.as_bytes());
        f.push(b'\n');
        f
    }

    pub fn from_json(j: &str) -> Result<Self, String> {
        let b: Vec<char> = j.chars().collect();
        let mut i = 0usize;
        let m = parse_flat_object(&b, &mut i)?;
        // canonical 字段序即合法性：逐位比对生成序
        let expect_prefix = format!(
            "{{\"schema\":\"{AUTH_SCHEMA}\",\"kind\":\"{}\"",
            m.get("kind").cloned().unwrap_or_default()
        );
        if !j.starts_with(&expect_prefix) {
            return Err("trust frame non-canonical order".into());
        }
        let kind = m.get("kind").cloned().unwrap_or_default();
        if kind != TRUST_FRAME_ROTATE && kind != TRUST_FRAME_REVOKE {
            return Err(format!("trust frame kind invalid: {kind}"));
        }
        Ok(TrustFrame {
            kind,
            key_id: m.get("key_id").cloned().ok_or("key_id missing")?,
            new_key_id: m.get("new_key_id").cloned().unwrap_or_default(),
            new_pk: m.get("new_pk").cloned().unwrap_or_default(),
            // trust_seq 规范语法锁死：0|[1-9][0-9]*（无前导零/无符号/无空白），超界=拒（终审#1）
            trust_seq: m
                .get("trust_seq")
                .and_then(|v| {
                    let ok = !v.is_empty()
                        && v.bytes().all(|b| b.is_ascii_digit())
                        && (v.len() == 1 || !v.starts_with('0'));
                    if ok { v.parse::<u64>().ok() } else { None }
                })
                .ok_or("trust_seq must be canonical decimal")?,
            sig: {
                let s = m.get("sig").cloned().ok_or("sig missing")?;
                let ok = s.len() == 128 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
                if ok { s } else { return Err("sig must be 128 lowercase hex".into()) }
            },
            prev_trust_hash: m.get("prev_trust_hash").cloned().ok_or("prev_trust_hash missing")?,
        })
    }

    /// 帧信任哈希（链式）：SHA256("MLV31-TRUST-FRAME" ‖ len_u64be(J) ‖ J)。
    pub fn trust_hash(&self) -> String {
        let j = self.to_json();
        let mut buf = Vec::new();
        buf.extend_from_slice(DOMAIN_TRUST_FRAME.as_bytes());
        buf.extend_from_slice(&(j.len() as u64).to_be_bytes());
        buf.extend_from_slice(j.as_bytes());
        let digest = sha256(&buf);
        format!("sha256:{}", digest.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }
}

/// 信任态（open 全量重放重建；规格 §四.4）。
#[derive(Debug, Default, Clone)]
pub struct TrustState {
    pub active: BTreeMap<String, [u8; 32]>,       // key_id → pk（含已吊销但保留的公钥历史）
    pub revoked: BTreeMap<String, u64>,           // key_id → 生效 trust_seq
    pub trust_seq: u64,
    pub chain_hash: String,                       // 当前链哈希（genesis 后=H0）
    pub mode_ed25519: bool,
    pub root_key_id: String,
    /// 信任帧签名域版本（false=v1 J 域；true=v2 TOON 域+0x02）。由 genesis 链格式定。
    pub toon: bool,
}

impl TrustState {
    pub fn legacy() -> Self {
        TrustState::default()
    }

    pub fn from_genesis_auth(auth: &BTreeMap<String, String>) -> Result<Self, String> {
        let root_key_id = auth.get("root_key_id").ok_or("auth missing root_key_id")?;
        let pk_hex = auth.get("root_public_key").ok_or("auth missing root_public_key")?;
        let pk = hex_decode32(pk_hex).ok_or("root_public_key must be 64 hex chars")?;
        // H0 自洽校验（防 genesis 篡改）
        let want_h0 = compute_h0(root_key_id, pk_hex);
        let got_h0 = auth.get("trust_hash").ok_or("auth missing trust_hash")?;
        if &want_h0 != got_h0 {
            return Err(format!("{DETAIL_TRUST_CHAIN}: genesis trust_hash mismatch (H0)"));
        }
        // root_key_id 与公钥自洽（key_id 推导一致性）
        let (derived, _) = key_id_of(&pk);
        if &derived != root_key_id {
            return Err(format!("{DETAIL_TRUST_CHAIN}: root_key_id does not derive from root_public_key"));
        }
        let mut active = BTreeMap::new();
        active.insert(root_key_id.clone(), pk);
        Ok(TrustState {
            active,
            revoked: BTreeMap::new(),
            trust_seq: 0,
            chain_hash: want_h0,
            mode_ed25519: true,
            root_key_id: root_key_id.clone(),
            toon: false,
        })
    }

    /// v2（TOON）模式构造入口：H0 用 v2 公式校验，toon=true（信任帧验签域分流）。
    pub fn from_genesis_auth_toon(auth: &BTreeMap<String, String>) -> Result<Self, String> {
        let root_key_id = auth.get("root_key_id").ok_or("auth missing root_key_id")?;
        let pk_hex = auth.get("root_public_key").ok_or("auth missing root_public_key")?;
        let pk = hex_decode32(pk_hex).ok_or("root_public_key must be 64 hex chars")?;
        let want_h0 = crate::kernel::mlv_toon::compute_h0_v2(root_key_id, pk_hex)?;
        let got_h0 = auth.get("trust_hash").ok_or("auth missing trust_hash")?;
        if &want_h0 != got_h0 {
            return Err(format!("{DETAIL_TRUST_CHAIN}: genesis trust_hash mismatch (H0 v2)"));
        }
        let (derived, _) = key_id_of(&pk);
        if &derived != root_key_id {
            return Err(format!("{DETAIL_TRUST_CHAIN}: root_key_id does not derive from root_public_key"));
        }
        let mut active = BTreeMap::new();
        active.insert(root_key_id.clone(), pk);
        Ok(TrustState {
            active,
            revoked: BTreeMap::new(),
            trust_seq: 0,
            chain_hash: want_h0,
            mode_ed25519: true,
            root_key_id: root_key_id.clone(),
            toon: true,
        })
    }

    pub fn is_active(&self, key_id: &str) -> bool {
        self.active.contains_key(key_id) && !self.is_revoked(key_id, self.trust_seq)
    }

    /// 该钥在序号 seq 时点是否已吊销。
    pub fn is_revoked(&self, key_id: &str, seq: u64) -> bool {
        self.revoked.get(key_id).map(|eff| seq >= *eff).unwrap_or(false)
    }

    pub fn pk_of(&self, key_id: &str) -> Option<&[u8; 32]> {
        self.active.get(key_id)
    }

    /// 应用一条信任帧（重放与 live 共用；全部 fail-closed）。
    pub fn apply_trust_frame(&mut self, tf: &TrustFrame) -> Result<(), String> {
        // 链哈希衔接
        if tf.prev_trust_hash != self.chain_hash {
            return Err(format!("{DETAIL_TRUST_CHAIN}: prev_trust_hash mismatch (reorder/deletion?)"));
        }
        // 签发钥验证（ROTATE=单钥；REVOKE=任意活动钥）
        self.verify_frame_signer(tf)?;
        match tf.kind.as_str() {
            TRUST_FRAME_ROTATE => {
                // version 单调：trust_seq == 当前+1
                if tf.trust_seq != self.trust_seq + 1 {
                    return Err(format!("{DETAIL_TRUST_CHAIN}: rotate version {} != {}+1", tf.trust_seq, self.trust_seq));
                }
                let new_pk = hex_decode32(&tf.new_pk)
                    .ok_or_else(|| format!("{DETAIL_TRUST_CHAIN}: new_pk must be 64 hex chars"))?;
                let (derived, _) = key_id_of(&new_pk);
                if derived != tf.new_key_id {
                    return Err(format!("{DETAIL_TRUST_CHAIN}: new_key_id does not derive from new_pk"));
                }
                self.active.insert(tf.new_key_id.clone(), new_pk);
                self.trust_seq = tf.trust_seq;
            }
            TRUST_FRAME_REVOKE => {
                // 生效序号单调：> 当前 trust_seq
                if tf.trust_seq <= self.trust_seq {
                    return Err(format!("{DETAIL_TRUST_CHAIN}: revoke effective seq {} <= current {}", tf.trust_seq, self.trust_seq));
                }
                // 吊销目标=key_id 字段（规格 §四.3：tombstone 以 key_id+生效 trust_seq 标识）；
                // 已吊销钥重复吊销=拒（tombstone 不可变）。
                if !self.active.contains_key(&tf.key_id) {
                    return Err(format!("{DETAIL_TRUST_CHAIN}: revoke target not enrolled"));
                }
                if self.revoked.contains_key(&tf.key_id) {
                    return Err(format!("{DETAIL_TRUST_CHAIN}: key already revoked"));
                }
                self.revoked.insert(tf.key_id.clone(), tf.trust_seq);
                self.trust_seq = tf.trust_seq;
            }
            _ => unreachable!(),
        }
        self.chain_hash = tf.trust_hash();
        Ok(())
    }

    /// 吊销帧签名验证：签发钥=任意活动未吊销钥（规格 §四.3）。
    /// ROTATE 由 tf.key_id（当前活动根）单钥验证；REVOKE 对活动钥集逐一尝试
    /// （验证结果确定：活动集在当前位置确定，Ed25519 验签确定）。
    fn verify_frame_signer(&self, tf: &TrustFrame) -> Result<(), String> {
        if tf.kind == TRUST_FRAME_ROTATE {
            let pk = *self.active.get(&tf.key_id)
                .ok_or_else(|| format!("{DETAIL_TRUST_CHAIN}: rotate signer not enrolled"))?;
            if self.is_revoked(&tf.key_id, self.trust_seq) {
                return Err(format!("{DETAIL_KEY_REVOKED}: rotate signer revoked"));
            }
            return if self.toon {
                crate::kernel::mlv_toon::verify_trust_frame_sig_v2(tf, &pk)
            } else {
                verify_trust_frame_sig(tf, &pk)
            };
        }
        // REVOKE：任意活动未吊销钥
        let mut last_err = format!("{DETAIL_SIG_INVALID}: revoke has no valid active signer");
        for (kid, pk) in &self.active {
            if self.is_revoked(kid, self.trust_seq) {
                continue;
            }
            let ok = if self.toon {
                crate::kernel::mlv_toon::verify_trust_frame_sig_v2(tf, pk).is_ok()
            } else {
                verify_trust_frame_sig(tf, pk).is_ok()
            };
            if ok {
                return Ok(());
            }
            last_err = format!("{DETAIL_SIG_INVALID}: active signer {kid} verify failed");
        }
        Err(last_err)
    }
}

/// 信任帧签名：消息=域前缀‖len‖canonical J（排除 sig 自身）。
pub fn trust_frame_signing_message(tf: &TrustFrame) -> Vec<u8> {
    let mut bare = tf.clone();
    bare.sig = String::new();
    let j = bare.to_json();
    let mut msg = Vec::with_capacity(DOMAIN_SIG.len() + 8 + j.len());
    msg.extend_from_slice(DOMAIN_SIG.as_bytes());
    msg.extend_from_slice(&(j.len() as u64).to_be_bytes());
    msg.extend_from_slice(j.as_bytes());
    msg
}

pub fn sign_trust_frame(tf: &mut TrustFrame, sk: &SigningKey) -> Result<(), String> {
    let msg = trust_frame_signing_message(tf);
    let sig = sk.sign(&msg).to_bytes();
    tf.sig = sig.iter().map(|b| format!("{b:02x}")).collect();
    Ok(())
}

fn verify_trust_frame_sig(tf: &TrustFrame, pk: &[u8; 32]) -> Result<(), String> {
    let sig_bytes = hex_decode64(&tf.sig)
        .ok_or_else(|| format!("{DETAIL_SIG_INVALID}: trust frame sig must be 128 hex chars"))?;
    let sig = Signature::from_bytes(&sig_bytes);
    let vk = VerifyingKey::from_bytes(pk).map_err(|_| format!("{DETAIL_SIG_INVALID}: bad trust signer key"))?;
    let msg = trust_frame_signing_message(tf);
    vk.verify(&msg, &sig).map_err(|_| format!("{DETAIL_SIG_INVALID}: trust frame Ed25519 verify failed"))?;
    Ok(())
}

/// 业务记录验签（重放按历史信任前缀；live 按当前态）——
/// 返回 Err(detail) 三分列细分。
pub fn verify_business_sig(
    env: &BTreeMap<String, String>,
    state_at_position: &TrustState,
) -> Result<(), String> {
    let sig_key_id = env.get("sig_key_id")
        .ok_or_else(|| format!("{DETAIL_SIG_INVALID}: envelope missing sig_key_id"))?;
    let pk = state_at_position.pk_of(sig_key_id)
        .ok_or_else(|| format!("{DETAIL_TRUST_CHAIN}: signing key not enrolled: {sig_key_id}"))?;
    // sig_trust_seq 声明（若在）：规范十进制语法（终审#1）且不得高于验证时点 trust_seq；
    // 声明了但语法非法=拒（不是静默跳过——防畸形声明混入）
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
    verify_envelope_sig(env, pk)
}

// ── 混合帧解码（规格 §四.1.2）────────────────────────────────
//
// ed25519 账本：u64be(N)‖frame_type(1B)‖J(N B)‖LF（N=len(J)）
// legacy 账本：u64be(N)‖J(N B)‖LF
// 判定：u64be 后首字节 0x00/0x01=typed；'{'=legacy；其余=拒。
// 全账本一致：首帧定格式，后续帧 frame_type 不得漂移（legacy 帧内 J 以 '{' 开头
// 与 0x00/0x01 天然区分；typed 账本出现裸 '{' 头=格式漂移拒）。

#[derive(Debug, Clone, PartialEq)]
pub enum LedgerFrame {
    Record(LedgerRecord),
    Trust(TrustFrame),
}

pub fn decode_frames_mixed(data: &[u8]) -> Result<(bool, Vec<LedgerFrame>), String> {
    use crate::kernel::mlv::{parse_record_json, LedgerRecord};
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut typed: Option<bool> = None;
    while i < data.len() {
        if data.len() - i < 9 {
            return Err("truncated frame header".into());
        }
        let n64 = u64::from_be_bytes(data[i..i + 8].try_into().unwrap());
        let n = usize::try_from(n64).map_err(|_| "frame length overflow")?;
        let head = data[i + 8];
        let (is_typed, ft, jstart) = match head {
            0x00 | 0x01 => (true, head, i + 9),
            b'{' => (false, 0u8, i + 8),
            _ => return Err("bad frame head (expected frame_type or '{')".into()),
        };
        match typed {
            None => typed = Some(is_typed),
            Some(t) if t != is_typed => {
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
        let j = std::str::from_utf8(&data[jstart..jstart + n]).map_err(|e| e.to_string())?;
        if is_typed {
            if ft == 0x01 {
                out.push(LedgerFrame::Trust(TrustFrame::from_json(j)?));
            } else {
                out.push(LedgerFrame::Record(parse_record_json(j)?));
            }
        } else {
            out.push(LedgerFrame::Record(parse_record_json(j)?));
        }
        i = end;
    }
    Ok((typed.unwrap_or(false), out))
}

/// 业务记录 typed 帧字节（frame_type=0；ed25519 账本专用）。
pub fn record_frame_typed(rec: &LedgerRecord) -> Vec<u8> {
    let mut f = rec.frame();
    // frame()= u64be(N)‖J‖LF → 在 u64be 后插入 0x00
    let mut out = Vec::with_capacity(f.len() + 1);
    out.extend_from_slice(&f[..8]);
    out.push(0x00);
    out.extend_from_slice(&f[8..]);
    f.clear();
    out
}
