//! TOON v2 CLI ledger 组 E2E（P2c；规格 §六）。
//! 真进程黑盒：create/verify/convert 全动词+互斥负例（目标已存在/源已是 v2/缺文件）。

use std::process::Command;

fn ductile() -> String {
    let d = format!("{}/target/release/ductile", env!("CARGO_MANIFEST_DIR"));
    assert!(std::path::Path::new(&d).exists(), "release 二进制不存在——先 cargo build --release");
    d
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("dt-ledger-e2e-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(ductile()).args(args).output().expect("spawn ductile");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn ledger_cli_create_verify_roundtrip() {
    let d = tmpdir("cv");
    let p = d.join("led.v2");
    let keydir = d.join("keys");
    let (rk, _, _) = run(&["mlv", p.to_str().unwrap(), "keygen", "--out", keydir.to_str().unwrap()]);
    assert_eq!(rk, 0);
    let secret = {
        // keygen 输出 secret=<path>
        let (_, _out, _) = run(&["mlv", "x", "keygen", "--out", keydir.to_str().unwrap()]);
        // 第一次 keygen 已占用 key_id 目录名（同钥不同文件）；直接扫目录
        let mut s = None;
        for e in std::fs::read_dir(&keydir).unwrap() {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            if name.ends_with(".secret") { s = Some(keydir.join(name)); }
        }
        s.unwrap()
    };

    // create（v2=ed25519 强制；无钥=拒）
    let (rc0, _, err0) = run(&["ledger", "create", p.to_str().unwrap()]);
    assert_eq!(rc0, 1);
    assert!(err0.contains("--root-key"), "{err0}");

    let (rc, out, err) = run(&["ledger", "create", p.to_str().unwrap(), "--root-key", secret.to_str().unwrap()]);
    assert_eq!(rc, 0, "create rc={rc} err={err}");
    assert!(out.starts_with("OK ledger create v2 head=sha256:"), "{out}");

    // 首帧字节级断言：v2 帧头 = u64be‖FT=0‖0x02
    let data = std::fs::read(&p).unwrap();
    assert_eq!(data[8], 0x00, "FT=0 record");
    assert_eq!(data[9], 0x02, "codec slot=0x02");

    // verify 自动识别 v2
    let (rc2, out2, _) = run(&["ledger", "verify", p.to_str().unwrap()]);
    assert_eq!(rc2, 0);
    assert!(out2.contains("format=v2(toon)"), "{out2}");

    // 重复 create=拒（目标存在）
    let (rc3, _, err3) = run(&["ledger", "create", p.to_str().unwrap()]);
    assert_eq!(rc3, 1);
    assert!(err3.contains("target exists"), "{err3}");

    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn ledger_cli_ed25519_create_and_mlv_compat() {
    let d = tmpdir("ed");
    let p = d.join("gov.v2");
    let keydir = d.join("keys");

    // keygen 走既有 mlv 组（组间协作）
    let (rc, out, err) = run(&["mlv", p.to_str().unwrap(), "keygen", "--out", keydir.to_str().unwrap()]);
    assert_eq!(rc, 0, "{err}");
    let secret = out.lines().find_map(|l| l.strip_prefix("secret=")).unwrap().to_string();

    // create --root-key（ed25519 v2 链）
    let (rc2, out2, err2) = run(&["ledger", "create", p.to_str().unwrap(), "--root-key", &secret]);
    assert_eq!(rc2, 0, "{err2}");
    assert!(out2.contains("OK ledger create v2"), "{out2}");

    // verify：ed25519 v2 全验（H0/信任态/链）
    let (rc3, out3, _) = run(&["ledger", "verify", p.to_str().unwrap()]);
    assert_eq!(rc3, 0);
    assert!(out3.contains("format=v2(toon)"), "{out3}");

    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn ledger_cli_convert_v1_to_v2() {
    let d = tmpdir("conv");
    // v1 legacy 源链：库内直构造（P4b 后 mlv 写入动词已降级，CLI 面不再产 v1 链）
    let src = d.join("src.v1");
    {
        use ductile::kernel::mlv::{MlvLedger, MlvOp, LedgerRecord, make_envelope, effect_key, domain_hash, DOMAIN_RECORD};
        let (_led, genesis_h) = MlvLedger::init(&src, 1_000_000_000).unwrap();
        drop(_led); // 释放 EX 锁再 open（flock-reentrant 禁嵌套持锁）
        // v1 create 语义（照 cmd_mlv mkrec 无 signer 分支：make_envelope 生成 mac 信封）
        let mut led = MlvLedger::open(&src).unwrap();
        let now = 1_000_000_001u64;
        let (op, binding, rev, _rk) = (MlvOp::CreateProposal, "b1", 0u64, "rk-a");
        let payload = format!("{op:?}|{binding}|{rev}");
        let digest = domain_hash(DOMAIN_RECORD, format!("{op:?}||{payload}").as_bytes());
        let env = make_envelope(&op, binding, rev, "", &digest, now);
        let env_digest = env.get("mac").cloned().unwrap_or_default();
        let rec = LedgerRecord {
            schema: 1, seq: 1, op,
            key_id: "genesis".into(),
            root_commitment: ductile::kernel::mlv::GENESIS_ROOT.into(),
            effect_key: effect_key(op, binding, rev, "").unwrap(),
            idempotency_scope: None, caller_id: Some("cli".into()), request_key: Some("".into()),
            request_digest: digest,
            binding_id: Some(binding.into()), revision: Some(rev), from: None, to: Some(ductile::kernel::mlv::MlvState::Proposed),
            before_record_hash: genesis_h.clone(), accepted_at_ns: now,
            nonce: Some(format!("nonce-1-{now}")), envelope_digest: Some(env_digest), envelope: Some(env),
            payload: payload.clone(),
            registry_receipt: None,
            result: [("code".to_string(), "OK".to_string())].into_iter().collect(),
            record_hash: String::new(),
        };
        led.append(rec).unwrap();
    }

    // 源字节快照（convert 后必须零变化）
    let src_bytes = std::fs::read(&src).unwrap();

    let dst = d.join("dst.v2");
    // keygen 新钥（convert 重签用）
    let kd = d.join("keys");
    let (_, _kout, _) = run(&["mlv", "z", "keygen", "--out", kd.to_str().unwrap()]);
    let secret = {
        let mut s = None;
        for e in std::fs::read_dir(&kd).unwrap() {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            if name.ends_with(".secret") { s = Some(kd.join(name)); }
        }
        s.unwrap()
    };
    let (rc3, out3, err3) = run(&["ledger", "convert", "--in", src.to_str().unwrap(), "--out", dst.to_str().unwrap(), "--root-key", secret.to_str().unwrap()]);
    assert_eq!(rc3, 0, "{err3}");
    assert!(out3.contains("records=2"), "{out3}");

    // 源零变化
    assert_eq!(std::fs::read(&src).unwrap(), src_bytes, "convert must not touch source bytes");

    // 目标=v2 帧+verify 过+语义保留（2 记录）
    let data = std::fs::read(&dst).unwrap();
    assert_eq!(data[9], 0x02, "dst is v2");
    let (rc4, out4, err4) = run(&["ledger", "verify", dst.to_str().unwrap()]);
    assert_eq!(rc4, 0, "{err4}");
    assert!(out4.contains("format=v2(toon)"), "{out4}");
    // mlv status 兼容读 v2 链（三态 gov open）
    let (rc5, out5, _) = run(&["mlv", dst.to_str().unwrap(), "status", "b1"]);
    assert_eq!(rc5, 0);
    assert!(out5.contains("state=PROPOSED"), "{out5}");

    // v2 源再 convert=拒
    let dst2 = d.join("dst2.v2");
    let (rc6, _, err6) = run(&["ledger", "convert", "--in", dst.to_str().unwrap(), "--out", dst2.to_str().unwrap()]);
    assert_eq!(rc6, 1);
    assert!(err6.contains("already v2"), "{err6}");
    assert!(!dst2.exists(), "failed convert leaves no output");

    // 目标已存在=拒
    let (rc7, _, err7) = run(&["ledger", "convert", "--in", src.to_str().unwrap(), "--out", dst.to_str().unwrap()]);
    assert_eq!(rc7, 1);
    assert!(err7.contains("target exists"), "{err7}");

    // 缺源=稳定非零
    let (rc8, _, err8) = run(&["ledger", "convert", "--in", d.join("nope").to_str().unwrap(), "--out", d.join("x.v2").to_str().unwrap()]);
    assert_eq!(rc8, 2);
    assert!(err8.contains("ledger-missing"), "{err8}");

    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn ledger_cli_verify_missing_and_unknown_verb() {
    let d = tmpdir("neg");
    let (rc, _, err) = run(&["ledger", "verify", d.join("none.v2").to_str().unwrap()]);
    assert_eq!(rc, 2);
    assert!(err.contains("ledger-missing"), "{err}");
    let (rc2, _, _) = run(&["ledger", "frobnicate"]);
    assert_ne!(rc2, 0);
    let _ = std::fs::remove_dir_all(&d);
}
