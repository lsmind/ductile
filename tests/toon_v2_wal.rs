//! WAL v2（TOON）验收测试（T14 WAL 面；规格 §四.1+§七 T14）。
//!
//! 覆盖：v2 回环（编码→解码语义等价）、链公式（逐帧 sha256 续链）、
//! 崩溃恢复语义（done/acked/in_flight 与 v1 recover 等价）、
//! v1→v2 迁移（原子+源零变化+目标已存在拒）、混链拒、帧边界负例。

use ductile::kernel::types::EffectKey;
use ductile::kernel::wal::{Recovered, WalRecord};
use ductile::kernel::wal_toon::*;

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("dt-walv2-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ek(idx: u32) -> EffectKey {
    EffectKey { plan_fingerprint: "plan-abc".into(), effect_index: idx, input_digest: "d1".into() }
}

#[test]
fn t14_wal_v2_roundtrip_and_chain() {
    let d = tmpdir("rt");
    let p = d.join("w.v2");
    let mut w = WalV2::open(&p).unwrap();
    let h1 = w.append(&WalRecord::Intent { run_id: "r1".into(), step_id: "s1".into(), attempt: 1, input_digest: "in1".into() }).unwrap();
    let h2 = w.append(&WalRecord::EffectAck { run_id: "r1".into(), step_id: "s1".into(), effect: ek(0) }).unwrap();
    let h3 = w.append(&WalRecord::Outcome { run_id: "r1".into(), step_id: "s1".into(), attempt: 1, status: "Success".into(), error_code: None, skip_reason: None, fields_digest: Some("fd".into()) }).unwrap();
    assert_ne!(h1, h2);
    assert_ne!(h2, h3);
    assert_eq!(w.seq, 3);
    drop(w);

    // 重开=续链（append 模式链头=末帧 T 哈希）
    let mut w2 = WalV2::open(&p).unwrap();
    assert_eq!(w2.last_hash, h3);
    let h4 = w2.append(&WalRecord::Intent { run_id: "r1".into(), step_id: "s2".into(), attempt: 1, input_digest: "in2".into() }).unwrap();
    drop(w2);

    // 全量读+链验证
    let recs = read_wal_v2(&p).unwrap();
    assert_eq!(recs.len(), 4);
    // 语义等价回读
    match &recs[0].0 {
        WalRecord::Intent { run_id, step_id, attempt, input_digest } => {
            assert_eq!((run_id.as_str(), step_id.as_str(), *attempt, input_digest.as_str()), ("r1", "s1", 1, "in1"));
        }
        _ => panic!("rec0"),
    }
    match &recs[3].0 {
        WalRecord::Intent { step_id, .. } => assert_eq!(step_id, "s2"),
        _ => panic!("rec3"),
    }
    // 链哈希=帧 T 的 sha256（公式不变）
    let data = std::fs::read(&p).unwrap();
    let n0 = u64::from_be_bytes(data[0..8].try_into().unwrap()) as usize;
    let t0 = &data[10..10 + n0];
    assert_eq!(ductile::kernel::hash::sha256_hex(t0), h1);
    let _ = h4;
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn t14_wal_v2_crash_recovery_semantics() {
    let d = tmpdir("cr");
    let p = d.join("w.v2");
    let mut w = WalV2::open(&p).unwrap();
    // s1 全三相（done）；s2 有 intent+effect 无 outcome（crash 中断=重跑+效果复用）
    w.append(&WalRecord::Intent { run_id: "r".into(), step_id: "s1".into(), attempt: 1, input_digest: "a".into() }).unwrap();
    w.append(&WalRecord::EffectAck { run_id: "r".into(), step_id: "s1".into(), effect: ek(0) }).unwrap();
    w.append(&WalRecord::Outcome { run_id: "r".into(), step_id: "s1".into(), attempt: 1, status: "Success".into(), error_code: None, skip_reason: None, fields_digest: None }).unwrap();
    w.append(&WalRecord::Intent { run_id: "r".into(), step_id: "s2".into(), attempt: 1, input_digest: "b".into() }).unwrap();
    w.append(&WalRecord::EffectAck { run_id: "r".into(), step_id: "s2".into(), effect: ek(1) }).unwrap();
    drop(w);

    let recs = read_wal_v2(&p).unwrap();
    let r: Recovered = recover_v2(&recs);
    assert!(r.done.contains("s1"), "s1 done");
    assert!(!r.done.contains("s2"), "s2 not done");
    assert!(r.acked_effects.contains(&ek(0)));
    assert!(r.acked_effects.contains(&ek(1)));
    assert_eq!(r.in_flight.len(), 1);
    assert_eq!(r.in_flight[0].0, "s2");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn t14_wal_v1_to_v2_migration() {
    let d = tmpdir("mig");
    // v1 WAL（旧写入器）
    let src = d.join("w.v1");
    let mut w1 = ductile::kernel::wal::Wal::open(&src).unwrap();
    w1.append(&WalRecord::Intent { run_id: "r".into(), step_id: "s1".into(), attempt: 1, input_digest: "x".into() }).unwrap();
    w1.append(&WalRecord::EffectAck { run_id: "r".into(), step_id: "s1".into(), effect: ek(0) }).unwrap();
    w1.append(&WalRecord::Outcome { run_id: "r".into(), step_id: "s1".into(), attempt: 1, status: "Success".into(), error_code: None, skip_reason: None, fields_digest: None }).unwrap();
    drop(w1);
    let src_bytes = std::fs::read(&src).unwrap();

    // 迁移
    let dst = d.join("w.v2");
    let n = migrate_wal_v1_to_v2(&src, &dst).unwrap();
    assert_eq!(n, 3);
    // 源零变化
    assert_eq!(std::fs::read(&src).unwrap(), src_bytes);
    // 目标=v2 且恢复语义等价
    let recs = read_wal_v2(&dst).unwrap();
    assert_eq!(recs.len(), 3);
    let r = recover_v2(&recs);
    assert!(r.done.contains("s1"));
    assert_eq!(r.in_flight.len(), 0);

    // 目标已存在=拒
    assert!(migrate_wal_v1_to_v2(&src, &dst).is_err());
    // 二次迁移（同源同目标名不同路径）幂等性：新目标可再建
    let dst2 = d.join("w2.v2");
    assert_eq!(migrate_wal_v1_to_v2(&src, &dst2).unwrap(), 3);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn t14_wal_v2_rejects_v1_mixed_and_bad_frames() {
    let d = tmpdir("mix");
    let p = d.join("w.v2");
    let mut w = WalV2::open(&p).unwrap();
    w.append(&WalRecord::Intent { run_id: "r".into(), step_id: "s1".into(), attempt: 1, input_digest: "x".into() }).unwrap();
    drop(w);
    // 追加 v1 JSON 行（无帧头）→ read 拒（残尾非帧头）
    let mut data = std::fs::read(&p).unwrap();
    data.extend_from_slice(b"{\"k\":\"intent\",\"run\":\"r\"}\n");
    let p2 = d.join("mixed.v2");
    std::fs::write(&p2, &data).unwrap();
    assert!(read_wal_v2(&p2).is_err(), "v2 WAL + v1 line must fail whole-file");

    // 截断
    let p3 = d.join("trunc.v2");
    let good = std::fs::read(&p).unwrap();
    std::fs::write(&p3, &good[..good.len() - 3]).unwrap();
    assert!(read_wal_v2(&p3).is_err());

    // 错终止符
    let mut bad = good.clone();
    let last = bad.len() - 1;
    bad[last] = b'X';
    let p4 = d.join("badterm.v2");
    std::fs::write(&p4, &bad).unwrap();
    assert!(read_wal_v2(&p4).is_err());

    // 链断：改 prev_hash 字段（重编码闸会拒——非 canonical）
    let p5 = d.join("chainbreak.v2");
    let mut cb = good.clone();
    // 改帧体一字节（0 帧的 in 字段值首字符）
    let _n0 = u64::from_be_bytes(cb[0..8].try_into().unwrap()) as usize;
    cb[10 + 5] = b'Q';
    std::fs::write(&p5, &cb).unwrap();
    assert!(read_wal_v2(&p5).is_err());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn t14_wal_v2_audit_full_fidelity() {
    // v2 audit=15 字段完整编码（v1 只写 k+prev_hash）
    let a = ductile::kernel::types::AuditEvent {
        seq: 7, run_id: "r".into(), step_id: "s".into(), attempt: 2,
        scenario: Some("smoke".into()), seed: Some(42), fault: None,
        outcome_status: "Success", error_code: None, skip_reason: None,
        effect_key: Some(ek(3)), input_digest: Some("in".into()), output_digest: Some("out".into()),
        prev_hash: "0".repeat(64),
    };
    let t = wal_record_canonical_v2(&WalRecord::Audit(a.clone()), &"0".repeat(64)).unwrap();
    let s = String::from_utf8(t.clone()).unwrap();
    assert!(s.contains("scen: smoke"), "{s}");
    assert!(s.contains("seed: 42"), "{s}");
    // ek 含 '#'（TOON 分隔符）→ 强制引号化；prev_hash 数字开头同
    assert!(s.contains("ek: \"plan-abc#3#d1\""), "{s}");
    // 回读（audit 完整往返：结构字段逐一对）
    let (back, prev) = parse_wal_record_v2(&t).unwrap();
    assert_eq!(prev, "0".repeat(64));
    match back {
        WalRecord::Audit(a2) => {
            assert_eq!(a2.seq, 7);
            assert_eq!(a2.run_id, "r");
            assert_eq!(a2.scenario.as_deref(), Some("smoke"));
            assert_eq!(a2.seed, Some(42));
            assert_eq!(a2.effect_key, Some(ek(3)));
            assert_eq!(a2.output_digest.as_deref(), Some("out"));
        }
        _ => panic!("audit"),
    }
}
