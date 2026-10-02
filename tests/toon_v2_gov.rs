//! TOON v2 治理层 E2E（P2b；规格 §七 T11 的治理面）。
//!
//! 链路：keygen→init_ed25519_v2→submit（v2 验签+语义+append_v2）→
//! submit_trust_frame（v2 帧域）→reopen 重放（三态解码+信任前缀验签全确定性）。
//! 负例：v1 签名信封进 v2 链=拒；v2 链记录缺 sig=拒。

use ductile::kernel::mlv::{domain_hash, make_envelope, MlvOp, MlvState, DOMAIN_RECORD, GENESIS_ROOT};
use ductile::kernel::mlv_auth::{generate_keypair, TrustFrame};
use ductile::kernel::mlv_toon::sign_envelope_v2;
use ductile::kernel::mlv::effect_key;
use ductile::kernel::gov::{ApplyOutcome, GovRegistry};
use std::collections::BTreeMap;

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("ductile-govv2-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn v2_record(
    seq: u64, op: MlvOp, binding: &str, rev: u64,
    from: Option<MlvState>, to: Option<MlvState>, prev: &str,
    request_key: &str, at: u64, sk: &ed25519_dalek::SigningKey, key_id: &str,
) -> ductile::kernel::mlv::LedgerRecord {
    let digest = domain_hash(DOMAIN_RECORD, format!("{op:?}|{request_key}|p").as_bytes());
    let mut env = make_envelope(&op, binding, rev, request_key, &digest, at);
    env.insert("mac".into(), String::new());
    sign_envelope_v2(&mut env, sk, key_id, Some(0)).unwrap();
    ductile::kernel::mlv::LedgerRecord {
        schema: 1, seq, op,
        key_id: "genesis".into(), root_commitment: GENESIS_ROOT.into(),
        effect_key: effect_key(op, binding, rev, request_key).unwrap(),
        idempotency_scope: None, caller_id: Some("t".into()),
        request_key: Some(request_key.into()), request_digest: digest,
        binding_id: Some(binding.into()), revision: Some(rev),
        from, to, before_record_hash: prev.into(), accepted_at_ns: at,
        nonce: Some(format!("n-{seq}-{at}")), envelope_digest: Some(String::new()), envelope: Some(env),
        payload: "p".into(), registry_receipt: None,
        result: [("code".into(), "OK".into())].into_iter().collect(),
        record_hash: String::new(),
    }
}

#[test]
fn gov_v2_full_lifecycle() {
    let d = tmpdir("full");
    let path = d.join("gov.v2");
    // keygen 落盘（0700/0600/0644 原子）
    let keydir = d.join("keys");
    let (key_id, secret, _pub) = ductile::kernel::mlv_auth::keygen_write(&keydir).unwrap();
    let (seed, _pk) = generate_keypair().unwrap();
    let _ = seed;
    let sk = ed25519_dalek::SigningKey::from_bytes(&{
        let b = std::fs::read(&secret).unwrap();
        let mut a = [0u8; 32];
        a.copy_from_slice(&b);
        a
    });

    // v2 init（auth TOON genesis）
    let (mut reg, ghash) = GovRegistry::init_ed25519_v2(&path, 1_000_000, &secret).unwrap();
    assert!(!ghash.is_empty());
    assert_eq!(reg.frame_fmt, ductile::kernel::mlv_toon::FrameFormat::V2);

    // 业务提交：CREATE_PROPOSAL（from=null 专属首事件）
    let rec = v2_record(1, MlvOp::CreateProposal, "b1", 0, None, Some(MlvState::Proposed), &ghash, "rk1", 1_100_000, &sk, &key_id);
    let out = reg.submit(rec.clone(), 1_100_000).unwrap();
    match out {
        ApplyOutcome::Committed { record_hash, .. } => assert!(!record_hash.is_empty()),
        _ => panic!("expected committed"),
    }

    // 幂等重放：同键同摘要 → Acked
    let out2 = reg.submit(rec.clone(), 1_100_000).unwrap();
    assert!(matches!(out2, ApplyOutcome::Acked { .. }));

    // 信任帧：ROTATE（v2 帧域签名）
    let (seed2, _pk2) = generate_keypair().unwrap();
    let new_pk_hex: String = ed25519_dalek::SigningKey::from_bytes(&seed2).verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let (new_kid, _) = ductile::kernel::mlv_auth::key_id_of(&ed25519_dalek::SigningKey::from_bytes(&seed2).verifying_key().to_bytes());
    let mut tf = TrustFrame {
        kind: "TRUST_ROTATE".into(), key_id: key_id.clone(),
        new_key_id: new_kid.clone(), new_pk: new_pk_hex,
        trust_seq: 1, sig: String::new(),
        prev_trust_hash: reg.trust.chain_hash.clone(),
    };
    ductile::kernel::mlv_toon::sign_trust_frame_v2(&mut tf, &sk).unwrap();
    reg.submit_trust_frame(tf).unwrap();
    assert_eq!(reg.trust.trust_seq, 1);
    assert!(reg.trust.active.contains_key(&new_kid));

    // 新钥继续提交（信任前缀=rotate 后）
    let sk2 = ed25519_dalek::SigningKey::from_bytes(&seed2);
    let head = reg.head().unwrap();
    let rec2 = v2_record(2, MlvOp::Grant, "b1", 0, Some(MlvState::Proposed), Some(MlvState::Granted), &head, "rk2", 1_200_000, &sk2, &new_kid);
    let out3 = reg.submit(rec2, 1_200_000).unwrap();
    assert!(matches!(out3, ApplyOutcome::Committed { .. }));

    // reopen：全量重放（三态+v2 域验签+链验证确定性）
    drop(reg);
    let reg2 = GovRegistry::open(&path).unwrap();
    assert_eq!(reg2.frame_fmt, ductile::kernel::mlv_toon::FrameFormat::V2);
    assert_eq!(reg2.trust.trust_seq, 1);
    assert!(reg2.trust.active.contains_key(&new_kid));
    // 旧钥 rotate 后仍在 active 集（可吊销保留历史），但被新信任态约束

    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn gov_v2_rejects_v1_signature_envelope() {
    let d = tmpdir("mixsig");
    let path = d.join("m.v2");
    let keydir = d.join("keys");
    let (key_id, secret, _pub) = ductile::kernel::mlv_auth::keygen_write(&keydir).unwrap();
    let seed_bytes = std::fs::read(&secret).unwrap();
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&seed_bytes);
    let sk = ed25519_dalek::SigningKey::from_bytes(&seed);

    let (mut reg, ghash) = GovRegistry::init_ed25519_v2(&path, 1_000_000, &secret).unwrap();
    // v1 签名域的信封（无 0x02）→ v2 链必拒
    let digest = domain_hash(DOMAIN_RECORD, b"x|rk|p");
    let mut env = make_envelope(&MlvOp::CreateProposal, "b1", 0, "rk", &digest, 1_100_000);
    env.insert("mac".into(), String::new());
    ductile::kernel::mlv_auth::sign_envelope(&mut env, &sk, &key_id, Some(0)).unwrap();
    let rec = ductile::kernel::mlv::LedgerRecord {
        schema: 1, seq: 1, op: MlvOp::CreateProposal,
        key_id: "genesis".into(), root_commitment: GENESIS_ROOT.into(),
        effect_key: effect_key(MlvOp::CreateProposal, "b1", 0, "rk").unwrap(),
        idempotency_scope: None, caller_id: Some("t".into()),
        request_key: Some("rk".into()), request_digest: digest,
        binding_id: Some("b1".into()), revision: Some(0),
        from: None, to: Some(MlvState::Proposed), before_record_hash: ghash.clone(), accepted_at_ns: 1_100_000,
        nonce: Some("n1".into()), envelope_digest: Some(String::new()), envelope: Some(env),
        payload: "p".into(), registry_receipt: None,
        result: [("code".into(), "OK".into())].into_iter().collect(),
        record_hash: String::new(),
    };
    let err = reg.submit(rec, 1_100_000).unwrap_err();
    assert!(err.to_string().contains("E423"), "v1 sig in v2 chain must be E423, got: {err}");
    // 零写入：链上仍只有 genesis
    let (fmt, frames) = reg.ledger.records_v2().unwrap();
    assert_eq!(fmt, ductile::kernel::mlv_toon::FrameFormat::V2);
    assert_eq!(frames.len(), 1, "failed submit must leave zero frames");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn gov_v1_ledgers_still_open_cleanly() {
    // 三态 open 回归：v1 typed 与 legacy 链经 gov::open 照常打开（零破坏）
    let d = tmpdir("v1compat");
    // legacy
    let p1 = d.join("l.v1");
    let (led, g) = ductile::kernel::mlv::MlvLedger::init(&p1, 1_000_000).unwrap();
    drop(led);
    let r1 = GovRegistry::open(&p1).unwrap();
    assert_eq!(r1.frame_fmt, ductile::kernel::mlv_toon::FrameFormat::V1Legacy);
    assert_eq!(r1.frames_typed, false);
    // typed（ed25519 v1）：合法 typed genesis 必须 payload=auth 对象——用 init_ed25519 系
    let keydir = d.join("keys");
    let (_kid, secret, _pub) = ductile::kernel::mlv_auth::keygen_write(&keydir).unwrap();
    let p2 = d.join("t.v1");
    let (_r, g2) = GovRegistry::init_ed25519(&p2, 1_000_000, &secret).unwrap();
    drop(_r);
    let r2 = GovRegistry::open(&p2).unwrap();
    assert_eq!(r2.frame_fmt, ductile::kernel::mlv_toon::FrameFormat::V1Typed);
    assert_eq!(r2.trust.toon, false);
    let _ = (g, g2);
    let _ = std::fs::remove_dir_all(&d);
}
