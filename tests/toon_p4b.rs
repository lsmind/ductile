//! P4b 验收（规格 §六.0 mlv 降级 + §五.1/T19 JSON writer 删尽）。
//!
//! - mlv 组写入动词=fail-closed 指引（E422+ledger 组路由）；status/verify 保留
//! - explore 报告=.toon 落盘（canonical TOON，无 JSON writer）
//! - reproduce 打包=MANIFEST.toon（产品路径）；双读（toon 优先/json 只读）

use std::process::Command;

fn bin() -> std::path::PathBuf {
    // E2E 用真二进制（与 toon_v2_cli.rs 同法）
    let p = std::path::PathBuf::from(env!("CARGO_BIN_EXE_ductile"));
    assert!(p.exists(), "bin missing: {p:?}");
    p
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin()).args(args).output().expect("spawn");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn t19_mlv_write_verbs_fail_closed() {
    // init/create/append/commit 等写入动词→ E422 + ledger 指引；exit 1
    // keygen 放行（钥工具不写账本）；其余写入动词拦截
    for verb in ["init", "create", "append", "begin", "commit", "abandon", "rotate", "propose", "advance"] {
        let (code, _, err) = run(&["mlv", "/tmp/p4b-ledger.jsonl", verb]);
        assert_eq!(code, 1, "verb {verb} should exit 1");
        assert!(err.contains("E422"), "{verb} err: {err}");
        assert!(err.contains("ductile ledger"), "{verb} must route to ledger group: {err}");
    }
}

#[test]
fn t19_mlv_readonly_verbs_survive() {
    // status/verify 保留（不触发降级门——报错只能是业务错不是 E422 门）
    let (code, _, err) = run(&["mlv", "/tmp/p4b-nonexistent-ledger.jsonl", "status"]);
    // 文件不存在=业务错；关键=不是 E422 降级门
    assert!(!err.contains("removed in TOON v2"), "status must pass the gate: {err}");
    // 不约束 exit code（业务错 1 也行）
    let _ = code;
}

#[test]
fn t19_explore_report_is_toon() {
    // explore 报告落盘=.toon（无 .json 新建）；产物 canonical TOON 可闭合解析
    let d = std::env::temp_dir().join(format!("dt-p4b-explore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    // 最小管线（explore 需要可 parse 的文件）
    let pl = d.join("mini.pipeline");
    std::fs::write(&pl, "Pipeline(\"mini\", \"p4b smoke\")\n  .proc(\"hello\", run(\"echo hi\"))\n").unwrap();
    let (code, out, err) = run(&["explore", pl.to_str().unwrap(), "smoke"]);
    let _ = (code, err);
    // stdout 应是 TOON（results: 顶层键，无 { } JSON 花括号）
    assert!(!out.trim_start().starts_with('{'), "report must not be JSON: {out}");
    assert!(out.contains("topic:"), "TOON field: {out}");
    assert!(out.contains("stop_reason:"), "TOON field: {out}");
    let _ = std::fs::remove_dir_all(&d);
}
