//! TOON v2 账本层验收测试（T09/T11/T12/T13；规格 §七）。
//!
//! golden 纪律（§三.7/硬门12）：v2 预期值禁止由被测实现回填——
//! T 的期望字节全部手写在测试内（TOON 文本可读，直接写死）；
//! record_hash 期望=冻结 v1 公式 domain_hash(DOMAIN_RECORD, 手写T) 推导，
//! 不调用被测的 record_to_tval/canonical_v2。

use ductile::kernel::mlv::{domain_hash, LedgerRecord, MlvLedger, MlvOp, DOMAIN_RECORD};
use ductile::kernel::mlv_auth::{TrustFrame, generate_keypair};
use ductile::kernel::mlv_toon::*;
use ductile::kernel::toon::{toon_canonical, TVal};
use std::collections::BTreeMap;

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("ductile-toonv2-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 固定 golden 记录（全部字段值手写定死）。
fn golden_record() -> LedgerRecord {
    let rd = format!("sha256:{}", "ab".repeat(32));
    LedgerRecord {
        schema: 1, seq: 0, op: MlvOp::LedgerInit,
        key_id: "genesis".into(),
        root_commitment: "ductile-genesis-root-v1-0000000000000000000000000000000000000000".into(),
        effect_key: "MLV-INIT-0000-fixed-constant-key".into(),
        idempotency_scope: None, caller_id: None, request_key: None,
        request_digest: rd,
        binding_id: None, revision: None, from: None, to: None,
        before_record_hash: format!("sha256:{}", "0".repeat(64)),
        accepted_at_ns: 1,
        nonce: None, envelope_digest: None, envelope: None,
        payload: "init-payload-v2-golden".into(),
        registry_receipt: None,
        result: [("code".to_string(), "OK".to_string())].into_iter().collect(),
        record_hash: String::new(),
    }
}

/// 手写期望 T（record_hash 排除=空串占位；字段序=字节序=CANON_ORDER）。
fn golden_t_no_hash() -> String {
    let mut s = String::new();
    s.push_str("accepted_at_ns: 1\n");
    s.push_str("before_record_hash: \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n");
    s.push_str("binding_id: null\n");
    s.push_str("caller_id: null\n");
    s.push_str("effect_key: MLV-INIT-0000-fixed-constant-key\n");
    s.push_str("envelope: null\n");
    s.push_str("envelope_digest: null\n");
    s.push_str("from: null\n");
    s.push_str("idempotency_scope: null\n");
    s.push_str("key_id: genesis\n");
    s.push_str("nonce: null\n");
    s.push_str("op: LEDGER_INIT\n");
    s.push_str("payload: init-payload-v2-golden\n");
    s.push_str("record_hash: \"\"\n");
    s.push_str("registry_receipt: null\n");
    s.push_str(&format!("request_digest: \"sha256:{}\"\n", "ab".repeat(32)));
    s.push_str("request_key: null\n");
    s.push_str("result:\n  code: OK\n");
    s.push_str("revision: null\n");
    s.push_str("root_commitment: ductile-genesis-root-v1-0000000000000000000000000000000000000000\n");
    s.push_str("schema: 1\n");
    s.push_str("seq: 0\n");
    s.push_str("to: null\n");
    s
}

// ── T09：v2 golden 逐字节 ───────────────────────────────────────

#[test]
fn t09_canonical_v2_golden_bytes() {
    let rec = golden_record();
    let got = canonical_v2(&rec, "").unwrap();
    let want = golden_t_no_hash().into_bytes();
    assert_eq!(got, want, "canonical TOON bytes must equal handwritten golden");
}

#[test]
fn t09_record_hash_from_formula() {
    let rec = golden_record();
    // 期望=冻结 v1 公式作用于手写 T（不是调被测映射）
    let want = domain_hash(DOMAIN_RECORD, golden_t_no_hash().as_bytes());
    assert_eq!(record_hash_v2(&rec).unwrap(), want);
}

#[test]
fn t09_v2_hash_differs_from_v1() {
    let rec = golden_record();
    // 相同语义值：canonical 字节不同 ⇒ v1/v2 哈希不同（§三.5 预期）
    assert_ne!(record_hash_v2(&rec).unwrap(), rec.compute_hash());
}

#[test]
fn t09_frame_v2_layout() {
    let mut rec = golden_record();
    rec.record_hash = record_hash_v2(&rec).unwrap();
    let f = record_frame_v2(&rec).unwrap();
    let t = canonical_v2(&rec, &rec.record_hash).unwrap();
    assert_eq!(&f[0..8], &(t.len() as u64).to_be_bytes(), "u64be(N) header");
    assert_eq!(f[8], 0x00, "FT=0 record");
    assert_eq!(f[9], 0x02, "codec slot");
    assert_eq!(&f[10..f.len() - 1], &t[..], "body == T");
    assert_eq!(*f.last().unwrap(), b'\n', "terminating LF");
    assert_eq!(f.len(), 10 + t.len() + 1);
}

// ── T11：v2 全链路（create/append/readback/verify/tamper）─────────

fn business_record(seq: u64, prev: &str, env: BTreeMap<String, String>) -> LedgerRecord {
    LedgerRecord {
        schema: 1, seq, op: MlvOp::Verify,
        key_id: "k1".into(),
        root_commitment: "ductile-genesis-root-v1-0000000000000000000000000000000000000000".into(),
        effect_key: format!("ek-{seq}"),
        idempotency_scope: None, caller_id: None, request_key: None,
        request_digest: format!("sha256:{}", "c".repeat(64)),
        binding_id: None, revision: None, from: None, to: None,
        before_record_hash: prev.into(),
        accepted_at_ns: 1000 + seq,
        nonce: None, envelope_digest: None, envelope: Some(env),
        payload: format!("p{seq}"),
        registry_receipt: None,
        result: [("code".to_string(), "OK".to_string())].into_iter().collect(),
        record_hash: String::new(),
    }
}

#[test]
fn t11_v2_full_chain() {
    let d = tmpdir("t11");
    let path = d.join("led.v2");
    let (led, ghash) = MlvLedger::init_v2(&path, 42, "init|42").unwrap();
    assert!(!ghash.is_empty());
    drop(led);

    // 重开：三态解码=v2；帧1=genesis
    let mut led = MlvLedger::open(&path).unwrap();
    let (fmt, frames) = led.records_v2().unwrap();
    assert_eq!(fmt, FrameFormat::V2);
    assert_eq!(frames.len(), 1);
    match &frames[0] {
        LedgerFrame::Record(r) => {
            assert_eq!(r.op, MlvOp::LedgerInit);
            assert_eq!(r.record_hash, ghash);
        }
        _ => panic!("frame 0 must be record"),
    }

    // 追加带签名信封的业务记录（链验证开启：before 必须接 genesis）
    let (seed, _pk) = generate_keypair().unwrap();
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    let mut env = BTreeMap::new();
    env.insert("alg".into(), "test".into());
    env.insert("issued_at_ns".into(), "42".into());
    env.insert("expires_at_ns".into(), "9".into());
    sign_envelope_v2(&mut env, &sk, "kid-1", None).unwrap();
    let rec = business_record(1, &ghash, env);
    let h1 = led.append_v2(rec.clone()).unwrap();

    // 读回：解析+canonical 双闸+哈希一致全过
    let (fmt2, frames2) = led.records_v2().unwrap();
    assert_eq!(fmt2, FrameFormat::V2);
    assert_eq!(frames2.len(), 2);
    match &frames2[1] {
        LedgerFrame::Record(r) => {
            assert_eq!(r.record_hash, h1);
            assert_eq!(r.envelope.as_ref().unwrap().get("sig").unwrap(), rec.envelope.as_ref().unwrap().get("sig").unwrap());
        }
        _ => panic!("frame 1 must be record"),
    }

    // 篡改链：before_record_hash 断链 → 整链拒（decode 内链验证：prev 衔接）
    let bad = business_record(2, "sha256:deadbeef", BTreeMap::new());
    led.append_v2(bad).unwrap();
    let err = led.records_v2().unwrap_err();
    assert!(err.contains("chain break") || err.contains("seq break"), "got: {err}");
    let _ = std::fs::remove_dir_all(&d);
}

// ── T12：混链与错误模式 ─────────────────────────────────────────

#[test]
fn t12_mixed_chain_rejected_both_directions() {
    let d = tmpdir("t12a");
    // v2 链 + 追加 v1 typed 帧 → 拒
    let path = d.join("a.v2");
    let (mut led, g) = MlvLedger::init_v2(&path, 1, "x").unwrap();
    let v1_frame = {
        let rec = business_record(1, &g, BTreeMap::new());
        let mut rec = rec;
        rec.record_hash = rec.compute_hash(); // v1 哈希路径
        rec.frame_typed()
    };
    led.append_frame_raw(&v1_frame).unwrap();
    assert!(led.records_v2().is_err(), "v2 chain + v1 typed frame must be rejected");

    // v1 typed 链 + 追加 v2 帧 → 整链拒（v1 解码器按 9B 头算边界，v2 帧多 1 字节必炸）
    let p2 = d.join("b.v1");
    let (mut l2, g2) = MlvLedger::init_typed(&p2, 1, "x").unwrap();
    let mut rec = business_record(1, &g2, BTreeMap::new());
    rec.record_hash = rec.compute_hash();
    let v2f = record_frame_v2(&rec).unwrap();
    l2.append_frame_raw(&v2f).unwrap();
    assert!(l2.records_v2().is_err(), "v1 chain + v2 frame must fail whole-chain");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn t12_unknown_codec_rejected() {
    let d = tmpdir("t12b");
    let path = d.join("c.v2");
    let (led, g) = MlvLedger::init_v2(&path, 1, "x").unwrap();
    drop(led);
    let mut data = std::fs::read(&path).unwrap();
    let _ = g;
    // codec 槽改为 0x54（明令禁用值）→ 拒
    data[9] = 0x54;
    assert!(decode_ledger_tri(&data).is_err());
    // codec 槽改为 0x00 → 非法（既非 0x02 也非 '{'）→ 拒
    data[9] = 0x00;
    assert!(decode_ledger_tri(&data).is_err());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn t12_truncated_and_trailing_rejected() {
    let d = tmpdir("t12c");
    let path = d.join("d.v2");
    let (led, _) = MlvLedger::init_v2(&path, 1, "x").unwrap();
    drop(led);
    let data = std::fs::read(&path).unwrap();
    // 截断
    assert!(decode_ledger_tri(&data[..data.len() - 3]).is_err());
    // 尾随字节
    let mut t = data.clone();
    t.push(0x00);
    assert!(decode_ledger_tri(&t).is_err());
    // 错终止符
    let mut w = data.clone();
    let n = u64::from_be_bytes(w[0..8].try_into().unwrap()) as usize;
    w[10 + n - 1 + 1 - 1] = b'X'; // 帧尾 LF 位
    assert!(decode_ledger_tri(&w).is_err());
    let _ = std::fs::remove_dir_all(&d);
}

// ── T13：Ed25519 v2（0x02 入签名域）────────────────────────────

#[test]
fn t13_envelope_sig_v2_roundtrip_and_tamper() {
    let (seed, pk) = generate_keypair().unwrap();
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    let mut env = BTreeMap::new();
    env.insert("binding_id".into(), "b1".into());
    env.insert("op".into(), "VERIFY".into());
    env.insert("issued_at_ns".into(), "7".into());
    sign_envelope_v2(&mut env, &sk, "kid-1", Some(3)).unwrap();
    assert!(verify_envelope_sig_v2(&env, &pk).is_ok());

    // 篡改任一受保护字节 → 失败
    let mut bad = env.clone();
    bad.insert("op".into(), "GRANT".into());
    assert!(verify_envelope_sig_v2(&bad, &pk).is_err());
    let mut bad2 = env.clone();
    bad2.insert("extra".into(), "x".into());
    assert!(verify_envelope_sig_v2(&bad2, &pk).is_err());

    // v2 签名 ≠ v1 签名（消息布局不同：0x02 叠加）——互验必挂
    let v1_msg = ductile::kernel::mlv_auth::signing_message(&env);
    let v2_msg = signing_message_v2(&env).unwrap();
    assert_ne!(v1_msg, v2_msg);
    assert_eq!(v2_msg[DOMAIN_SIG_LEN() + 8], 0x02, "codec byte in signing domain");
}

fn DOMAIN_SIG_LEN() -> usize {
    ductile::kernel::mlv_auth::DOMAIN_SIG.len()
}

#[test]
fn t13_trust_frame_v2_sig() {
    let (seed, pk) = generate_keypair().unwrap();
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    let mut tf = TrustFrame {
        kind: "TRUST_ROTATE".into(), key_id: "root".into(),
        new_key_id: "k2".into(), new_pk: "ab".repeat(32),
        trust_seq: 1, sig: String::new(),
        prev_trust_hash: format!("sha256:{}", "0".repeat(64)),
    };
    sign_trust_frame_v2(&mut tf, &sk).unwrap();
    assert!(verify_trust_frame_sig_v2(&tf, &pk).is_ok());
    // 篡改
    let mut bad = tf.clone();
    bad.new_key_id = "k3".into();
    assert!(verify_trust_frame_sig_v2(&bad, &pk).is_err());

    // 帧=FT1+0x02；解析回读+canonical 序闸
    let bytes = trust_frame_v2_bytes(&tf).unwrap();
    assert_eq!(bytes[8], 0x01);
    assert_eq!(bytes[9], 0x02);
    let back = parse_trust_frame_v2(&bytes[10..bytes.len() - 1]).unwrap();
    assert_eq!(back, tf);

    // 字段序漂移（schema 放最后=非 v1 canonical 序）→ parse 拒
    let mut drifty = String::new();
    drifty.push_str("kind: TRUST_ROTATE\n");
    drifty.push_str("key_id: root\n");
    drifty.push_str("new_key_id: k2\n");
    drifty.push_str("new_pk: ");
    drifty.push_str(&"ab".repeat(32));
    drifty.push('\n');
    drifty.push_str("prev_trust_hash: \"sha256:");
    drifty.push_str(&"0".repeat(64));
    drifty.push_str("\"\n");
    drifty.push_str("schema: mlv_auth_v1\n");
    drifty.push_str("sig: ");
    drifty.push_str(&tf.sig);
    drifty.push('\n');
    drifty.push_str("trust_seq: 1\n");
    assert!(parse_trust_frame_v2(drifty.as_bytes()).is_err(), "field order drift must be rejected");
}

#[test]
fn t13_h0_v2_and_auth_object() {
    let d = tmpdir("t13");
    let path = d.join("e.v2");
    // ed25519 v2 建链：payload=auth TOON 文本
    let (seed, _pk) = generate_keypair().unwrap();
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    let pk_hex: String = sk.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let (kid, _) = ductile::kernel::mlv_auth::key_id_of(&sk.verifying_key().to_bytes());
    let h0 = compute_h0_v2(&kid, &pk_hex).unwrap();
    // H0 公式不变仅字节换：v1 H0 ≠ v2 H0（确定性+区分）
    let h0_v1 = ductile::kernel::mlv_auth::compute_h0(&kid, &pk_hex);
    assert_ne!(h0, h0_v1);
    assert!(h0.starts_with("sha256:") && h0.len() == 7 + 64);

    let auth_t = auth_object_toon(&kid, &pk_hex, &h0).unwrap();
    assert!(auth_t.starts_with(b"schema: mlv_auth_v1\n"), "auth schema order is v1 canonical (not byte order)");
    // 解析回平面对象
    let m = parse_auth_payload_toon(std::str::from_utf8(&auth_t).unwrap()).unwrap();
    assert_eq!(m.get("schema").unwrap(), "mlv_auth_v1");
    assert_eq!(m.get("trust_hash").unwrap(), &h0);

    let (led, g) = MlvLedger::init_v2(&path, 7, std::str::from_utf8(&auth_t).unwrap()).unwrap();
    drop(led);
    let data = std::fs::read(&path).unwrap();
    let (fmt, frames) = decode_ledger_tri(&data).unwrap();
    assert_eq!(fmt, FrameFormat::V2);
    assert_eq!(frames.len(), 1);
    match &frames[0] {
        LedgerFrame::Record(r) => {
            assert_eq!(r.record_hash, g);
            assert!(parse_auth_payload_toon(&r.payload).is_some(), "genesis payload is auth TOON");
        }
        _ => panic!("genesis must be record"),
    }
    let _ = std::fs::remove_dir_all(&d);
}

// ── parse 负例（闭合 schema 门）─────────────────────────────────

#[test]
fn t09_parse_rejects_noncanonical_bodies() {
    let base = golden_t_no_hash();
    // 未知字段
    let mut x = base.clone();
    x.push_str("zzz_extra: 1\n");
    // 注意：插入位置无所谓——BTreeMap 排序后字段数 24 → 拒
    assert!(parse_record_v2(x.as_bytes()).is_err());
    // 缺字段（删一行）
    let mut y = base.clone();
    y.replace_range(0..y.find('\n').unwrap() + 1, "");
    assert!(parse_record_v2(y.as_bytes()).is_err());
    // 尾随空格
    let mut z = base.clone();
    z.replace_range(z.len() - 1.., " \n");
    assert!(parse_record_v2(z.as_bytes()).is_err());
    // CR
    let mut c = base.clone();
    c.insert(5, '\r');
    assert!(parse_record_v2(c.as_bytes()).is_err());
    // 空字节=空对象（23 字段缺失）→ 拒
    assert!(parse_record_v2(b"").is_err());
}

#[test]
fn t09_record_tval_mapping_symmetry() {
    // LedgerRecord → TVal → LedgerRecord 全保真（真哈希过哈希闸）
    let mut rec = golden_record();
    rec.record_hash = record_hash_v2(&rec).unwrap();
    let t = record_to_tval(&rec, &rec.record_hash);
    let back = parse_record_v2(&toon_canonical(&t).unwrap()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(back, rec);
}
