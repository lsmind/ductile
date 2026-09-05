//! Dslresult — ##DSL_RESULT 结构化结果协议。
//!
//! 从 executor.rs 拆出的协议层：外部脚本在 stdout 末尾输出
//! `##DSL_RESULT\nk=v\n##DSL_END` 块，引擎解析为字段对；
//! 内部编码为 `§§FIELDS§§k1=v1§§RAW§§原始stdout`，下游用
//! `@proc.field` 提取。est_loss_field_coverage 用字段保留度
//! 度量上游信息损耗。全部纯函数。

use std::collections::BTreeSet;

/// 解析 stdout 中的 ##DSL_RESULT 块 → (key, value) 列表。
/// 无块或块内无有效行 → None。
pub fn parse_dsl_result_block(stdout: &str) -> Option<Vec<(String, String)>> {
    let mut in_block = false;
    let mut kvs = Vec::new();
    for line in stdout.lines() {
        if line.starts_with("##DSL_RESULT") {
            in_block = true;
            continue;
        }
        if line.starts_with("##DSL_END") {
            in_block = false;
            continue;
        }
        if in_block {
            if let Some(eq) = line.find('=') {
                let k = line[..eq].trim().to_string();
                let v = line[eq + 1..].trim().to_string();
                if !k.is_empty() && !v.is_empty() {
                    kvs.push((k, v));
                }
            }
        }
    }
    if kvs.is_empty() {
        None
    } else {
        Some(kvs)
    }
}

/// 字段对 + 原始 stdout → 内部编码 `§§FIELDS§§..§§RAW§§..`。
pub fn encode_structured_result(kvs: &[(String, String)], raw: &str) -> String {
    let fields: Vec<String> = kvs.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
    format!("§§FIELDS§§{}§§RAW§§{}", fields.join("§§"), raw)
}

/// 从内部编码中提取字段值。非编码文本 / 字段缺席 → None。
pub fn extract_field(field: &str, text: &str) -> Option<String> {
    if !text.starts_with("§§FIELDS§§") {
        return None;
    }
    let rest = &text["§§FIELDS§§".len()..];
    for part in rest.split("§§") {
        // split("§§") 后段内不可能再含 "§§"，原 starts_with("RAW§§") 是死分支
        // （扫描会漏进 RAW 区把原始 stdout 里的 k=v 误当字段）——修正为整段相等。
        if part == "RAW" {
            break;
        }
        if let Some(eq) = part.find('=') {
            let k = &part[..eq];
            let v = &part[eq + 1..];
            if k == field {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// est_loss v1: 结构化字段保留度。上游有 DSL_RESULT 字段集 F_up，
/// 本 impl 输出字段集 F_out → loss = 1 − |F_up ∩ F_out| / |F_up|。
/// 上游无结构化字段 → None（退化到 v0 二值代理）。
pub fn est_loss_field_coverage(upstream_fields: &[String], output_text: &str) -> Option<f64> {
    if upstream_fields.is_empty() {
        return None;
    }
    let out_kvs = parse_dsl_result_block(output_text)?;
    if out_kvs.is_empty() {
        return None;
    }
    let out_keys: BTreeSet<&str> = out_kvs.iter().map(|(k, _)| k.as_str()).collect();
    let up_keys: BTreeSet<&str> = upstream_fields.iter().map(|s| s.as_str()).collect();
    let inter = up_keys.intersection(&out_keys).count();
    Some(1.0 - inter as f64 / up_keys.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_dsl_result_block ──
    #[test]
    fn parse_dsl_result_basic() {
        let stdout = "some output\n##DSL_RESULT\nstatus=ok\ncount=5\n##DSL_END\nmore output";
        let kvs = parse_dsl_result_block(stdout).unwrap();
        assert_eq!(kvs.len(), 2);
        assert_eq!(kvs[0], ("status".into(), "ok".into()));
        assert_eq!(kvs[1], ("count".into(), "5".into()));
    }

    #[test]
    fn est_loss_v1_field_coverage() {
        let up = vec![
            "status".to_string(),
            "count".to_string(),
            "score".to_string(),
        ];
        // 全保留 → 0
        let full = "x\n##DSL_RESULT\nstatus=ok\ncount=5\nscore=9\n##DSL_END";
        assert_eq!(est_loss_field_coverage(&up, full), Some(0.0));
        // 丢 1/3 → ≈1/3
        let partial = "x\n##DSL_RESULT\nstatus=ok\ncount=5\n##DSL_END";
        let got = est_loss_field_coverage(&up, partial).unwrap();
        assert!((got - 1.0 / 3.0).abs() < 1e-12);
        // 全丢 → 1
        let none = "x\n##DSL_RESULT\nother=1\n##DSL_END";
        assert_eq!(est_loss_field_coverage(&up, none), Some(1.0));
        // 无 DSL_RESULT → None（退回 v0）
        assert_eq!(est_loss_field_coverage(&up, "plain text"), None);
        // 上游无字段 → None
        assert_eq!(est_loss_field_coverage(&[], "any"), None);
    }

    #[test]
    fn parse_dsl_result_empty_block() {
        let stdout = "##DSL_RESULT\n##DSL_END";
        assert!(parse_dsl_result_block(stdout).is_none());
    }

    #[test]
    fn parse_dsl_result_no_block() {
        assert!(parse_dsl_result_block("just regular output").is_none());
    }

    // ── encode/extract structured result ──
    #[test]
    fn structured_result_roundtrip() {
        let kvs = vec![
            ("status".into(), "ok".into()),
            ("count".into(), "42".into()),
        ];
        let encoded = encode_structured_result(&kvs, "raw output here");
        assert!(encoded.starts_with("§§FIELDS§§"));
        assert!(encoded.contains("status=ok"));
        assert!(encoded.contains("§§RAW§§raw output here"));
    }

    #[test]
    fn extract_field_from_structured() {
        let kvs = vec![("status".into(), "ok".into())];
        let encoded = encode_structured_result(&kvs, "raw");
        let field = extract_field("status", &encoded);
        assert_eq!(field, Some("ok".into()));
    }

    #[test]
    fn extract_field_missing() {
        let kvs = vec![("status".into(), "ok".into())];
        let encoded = encode_structured_result(&kvs, "raw");
        assert!(extract_field("missing", &encoded).is_none());
    }

    #[test]
    fn extract_field_from_unstructured() {
        assert!(extract_field("x", "plain text").is_none());
    }


    // ── 新增边界用例 ──

    #[test]
    fn parse_value_containing_equals() {
        let stdout = "##DSL_RESULT\npath=/tmp/a=b.txt\n##DSL_END";
        let kvs = parse_dsl_result_block(stdout).unwrap();
        assert_eq!(kvs[0].1, "/tmp/a=b.txt");
    }

    #[test]
    fn parse_skips_empty_kv_lines() {
        let stdout = "##DSL_RESULT\nempty=\n=v\nnoline\nok=1\n##DSL_END";
        let kvs = parse_dsl_result_block(stdout).unwrap();
        assert_eq!(kvs.len(), 1);
        assert_eq!(kvs[0], ("ok".into(), "1".into()));
    }

    #[test]
    fn parse_trims_whitespace() {
        let stdout = "##DSL_RESULT\n  key  =  value  \n##DSL_END";
        let kvs = parse_dsl_result_block(stdout).unwrap();
        assert_eq!(kvs[0], ("key".into(), "value".into()));
    }

    #[test]
    fn extract_field_stops_at_raw_marker() {
        let kvs = vec![("a".into(), "1".into())];
        let encoded = encode_structured_result(&kvs, "xx§§RAW§§b=9");
        assert_eq!(extract_field("a", &encoded), Some("1".into()));
        assert!(extract_field("b", &encoded).is_none());
    }
}
