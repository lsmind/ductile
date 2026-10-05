// T17（规格 §五.4 TOON-CLOSED-1）：`.when` 闭输出门。
// 五情形：成功 / 未知字段 / 非法转义 / 非 canonical 输出 / 一次修复上限。
// 失败不得进入 judge 或账本（fail-closed：无输出文件、无半解析状态）。
//
// 裁判单一源 = `ductile toon` CLI（cmd_toon_stdin → parse_toon_closed →
// canonical 重编码），bridge 与本测试同走此口。

use std::io::Write;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ductile")
}

/// 跑 `ductile toon`，返回 (exit_code, stdout, stderr)。
fn judge(input: &str) -> (i32, String, String) {
    let mut child = Command::new(bin())
        .arg("toon")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ductile");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Python 侧 closed_toon_fields 的等价复刻（测试内联，验证同一协议：
/// 首判 → 拒则剥 fence 修复一次 → 终判 fail-closed）。
fn closed_repair_once(content: &str) -> (bool, bool) {
    let text = format!("{}\n", content.trim());
    let (code, _out, err) = judge(&text);
    if code == 0 {
        return (true, false);
    }
    let _ = err;
    // 一次修复：剥 code fence
    let mut repaired = content.trim().to_string();
    if repaired.starts_with("```") {
        repaired = repaired
            .trim_start_matches("```")
            .trim_start_matches(|c: char| c.is_ascii_alphabetic())
            .trim()
            .to_string();
    }
    if repaired.ends_with("```") {
        repaired = repaired[..repaired.len() - 3].trim().to_string();
    }
    let repaired = format!("{}\n", repaired);
    if repaired == text {
        return (false, true); // 无可修复差异 → fail closed（不再重判）
    }
    let (code2, _, _) = judge(&repaired);
    (code2 == 0, true)
}

#[test]
fn t17_case1_success_outputs_canonical() {
    let (code, out, err) = judge("title: \"hello world\"\ncount: 3\n");
    assert_eq!(code, 0, "accept canonical: {err}");
    // 裁判实证：`"hello world"` 重编码为裸 hello world——最小引号规则
    //（无空格歧义定界时裸串即 canonical；case1 输入的引号形式被吸收）。
    assert_eq!(out, "count: 3\ntitle: hello world\n", "canonical re-encode");
    // 裁判输出本身也必须是 canonical TOON（恰好一个末尾 LF）
    assert!(out.ends_with('\n') && !out.ends_with("\n\n"));
}

#[test]
fn t17_case2_unknown_field_rejected() {
    // 分层裁定：closed parser 层未知键=合法动态键（词法无辜）；schema 闭合
    // 在 `--schema` 白名单门（E461）。两层都验证：无 schema 过、带 schema 拒。
    let (code, _, _) = judge("title: ok\nextra: 1\n");
    assert_eq!(code, 0, "no-schema: unknown key is legal dynamic key");

    let mut child = Command::new(bin()).args(["toon", "--schema", "title,count"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().expect("spawn");
    child.stdin.as_mut().unwrap().write_all(b"title: ok\nextra: 1\n").unwrap();
    let out = child.wait_with_output().expect("wait");
    assert_ne!(out.status.code().unwrap_or(-1), 0, "E461 unknown field");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("E461") && err.contains("extra"), "err names field: {err}");
    // stdout 空 = 无半解析产物（不进 judge/账本）
    assert!(out.stdout.is_empty(), "no partial output");
}

#[test]
fn t17_case3_bad_escape_rejected() {
    // 大写 hex \u 转义被拒（§1.2.3）
    let (code, _, err) = judge("s: \"a\\u0041b\"\n");
    // 小写 hex 合法（ASCII 转义）；换大写必须拒
    assert_eq!(code, 0, "lowercase \\u of ASCII allowed: {err}");
    let (code2, _, _) = judge("s: \"a\\u004Ab\"\n");
    assert_ne!(code2, 0, "uppercase hex must be rejected");
    // 非 ASCII 转义拒绝
    let (code3, _, _) = judge("s: \"\\u4e2d\"\n");
    assert_ne!(code3, 0, "\\u of non-ASCII must be rejected");
}

#[test]
fn t17_case4_non_canonical_rejected() {
    // 带空格裸串是 canonical（最小引号：定界歧义才引号化）——真非 canonical 用例：
    // 数字形裸串必须引号化（否则词法歧义——会解析成数字）
    let (code, _, _) = judge("v: 42\n");
    assert_eq!(code, 0, "bare number is canonical");
    // 字符串形数字必须引号化（否则歧义成数字）——引号形即 canonical，原样保持
    let (code_n, out_n, _) = judge("v: \"42\"\n");
    assert_eq!(code_n, 0, "quoted numeric string is canonical");
    assert_eq!(out_n, "v: \"42\"\n", "type preservation: quotes kept");
    // 尾随空格
    let (code2, _, _) = judge("k: 1 \n");
    assert_ne!(code2, 0, "trailing space rejected");
    // 多余末尾 LF
    let (code3, _, _) = judge("k: 1\n\n");
    assert_ne!(code3, 0, "extra trailing LF rejected");
    // 前导零
    let (code4, _, _) = judge("k: 007\n");
    assert_ne!(code4, 0, "leading zero rejected");
}

#[test]
fn t17_case5_one_repair_limit_fail_closed() {
    // fence 包裹 → 首判拒、剥 fence 修复后过
    let (ok, repaired) = closed_repair_once("```\ntitle: \"fenced\"\n```");
    assert!(ok, "fenced output repaired once and accepted");
    assert!(repaired, "repair path taken");

    // 垃圾输出 → 首判拒、修复无差异 → fail closed
    let (ok2, _) = closed_repair_once("total garbage without colons");
    assert!(!ok2, "garbage fails closed after one repair attempt");

    // 修复后仍非 canonical（fence 剥了但内容本身坏）→ fail closed
    let (ok3, repaired3) = closed_repair_once("```\nk: 007\n```");
    assert!(!ok3, "repaired-but-noncanonical still fails closed");
    assert!(repaired3);
}

#[test]
fn t17_failure_produces_no_output_or_ledger_write() {
    // 失败路径的物理证明：exit≠0 时 stdout 必须为空（不回显原文、无半解析产物）。
    // stdout=空 ⇒ bridge 无 ##DSL_RESULT 字段可产出 ⇒ 不进 judge/账本。
    let (code, out, _) = judge("k: 007\n"); // 前导零=非 canonical
    assert_ne!(code, 0);
    assert!(out.is_empty(), "no partial output on rejection");
    let (code2, out2, _) = judge("k: [unclosed\n");
    assert_ne!(code2, 0);
    assert!(out2.is_empty(), "no partial output on parse error");
}
