//! MANIFEST.toon + binding v2 验收测试（T15 面；规格 §四.2-3）。

use ductile::kernel::binding::Envelope;
use ductile::kernel::ledger::{manifest_toon, verify_manifest_toon, write_manifest_toon};

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("dt-mftest-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn env() -> Envelope {
    Envelope {
        tenant_id: "t1".into(), issuer: "cli".into(), audience: "mlv".into(),
        subject: "s".into(), issued_at_ns: 1000, expires_at_ns: 9000,
        nonce: "n1".into(), contract_digest: "cd".into(), grant_digest: "gd".into(),
        binding_id: "b1".into(), revision: 3, payload_digest: "pd".into(),
        trust_root_version: 1, alg: "hmac-sha256-genesis-v1".into(),
    }
}

#[test]
fn t15_binding_envelope_canonical_toon() {
    // §四.2：binding 信封 body 改为 T；MAC 域常量不变仅字节换
    let e = env();
    // v2 canonical TOON（mlv_toon 层的通用平面对象编码）
    let t = ductile::kernel::mlv_toon::envelope_canonical_toon(
        &[
            ("alg".to_string(), e.alg.clone()),
            ("audience".to_string(), e.audience.clone()),
            ("binding_id".to_string(), e.binding_id.clone()),
            ("contract_digest".to_string(), e.contract_digest.clone()),
            ("expires_at_ns".to_string(), e.expires_at_ns.to_string()),
            ("grant_digest".to_string(), e.grant_digest.clone()),
            ("issued_at_ns".to_string(), e.issued_at_ns.to_string()),
            ("issuer".to_string(), e.issuer.clone()),
            ("nonce".to_string(), e.nonce.clone()),
            ("payload_digest".to_string(), e.payload_digest.clone()),
            ("revision".to_string(), e.revision.to_string()),
            ("subject".to_string(), e.subject.clone()),
            ("tenant_id".to_string(), e.tenant_id.clone()),
            ("trust_root_version".to_string(), e.trust_root_version.to_string()),
        ].into_iter().collect(),
    ).unwrap();
    let s = String::from_utf8(t).unwrap();
    // JCS 字典序=TOON Obj 字节序：alg 首行、trust_root_version 末行
    // （信封值域=string map——数字形串按 TOON 词法强制引号化，正确行为）
    assert!(s.starts_with("alg: hmac-sha256-genesis-v1\n"), "{s}");
    assert!(s.trim_end().ends_with("trust_root_version: \"1\""), "{s}");
    // MAC（v1 公式）与 v2 body 共存：envelope_digest 公式不变，仅 canonical 输入可换
    assert_eq!(e.envelope_digest().len(), 64);
}

#[test]
fn t15_manifest_toon_write_and_verify() {
    let d = tmpdir("mf");
    // 两个文件入 manifest
    std::fs::write(d.join("a.txt"), b"hello").unwrap();
    std::fs::create_dir_all(d.join("sub")).unwrap();
    std::fs::write(d.join("sub").join("b.bin"), [0u8, 1, 2]).unwrap();
    let entries = ductile::kernel::ledger::dir_manifest(&d).unwrap();
    assert_eq!(entries.len(), 2, "dir_manifest 应恰好两个文件（MANIFEST 未写）");

    // 内容快照：帧式（FT=2 header + FT=3 条目；codec 槽 0x02）
    let content = manifest_toon(&entries);
    assert_eq!(&content[8..10], &[2u8, 0x02][..], "header FT=2 + codec");
    let text = String::from_utf8_lossy(&content);
    assert!(text.contains("kind: reproduce_manifest"), "{text}");
    assert!(text.contains("crate: "), "{text}");
    assert!(text.contains("path: "), "{text}");

    // 原子写+verify 回环比对
    write_manifest_toon(&d, &entries).unwrap();
    let back = verify_manifest_toon(&d).unwrap();
    assert_eq!(back.len(), 2);
    let paths: Vec<&str> = back.iter().map(|e| e.path.as_str()).collect();
    assert!(paths.contains(&"a.txt") || paths.contains(&"./a.txt"), "{paths:?}");

    // 重复写=拒（显式迁移 only）
    assert!(write_manifest_toon(&d, &entries).is_err());

    // 篡改一个文件 → verify 拒
    std::fs::write(d.join("a.txt"), b"tampered").unwrap();
    assert!(verify_manifest_toon(&d).is_err());
    let _ = std::fs::remove_dir_all(&d);
}
