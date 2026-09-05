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
        if part.starts_with("RAW§§") {
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
