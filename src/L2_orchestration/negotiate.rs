//! v0.18.16 上下文协商（declare-then-run）——docs/v0.18_context_negotiation_spec.md
//!
//! 动机：v0.17 auto-prompt 是纯"推"模型，盲评两处输分（s1 约束截断 -15、
//! s3 审计缺清单 -7.3）都是注入不足——引擎猜错了模型需要什么。本模块把
//! guessing 换成 asking：模型声明缺什么（enough=false + missing），引擎
//! 按解析表补什么，prompt 追加式积累（只增不减），每轮 LLM 调用无状态。
//!
//! 协商默认关（[agents.x] negotiate=true 开启；NEGOTIATE=0/1 env 全局覆盖）。
//! 循环在引擎不在模型：预算、解析、失败分类全在 Rust 侧，可单测可回归。

use std::cell::RefCell;

use crate::core::dslresult;

/// 单条缺口声明：{"ref": "@req.constraints", "why": "选型需要预算边界"}
#[derive(Debug, Clone, PartialEq)]
pub struct MissingReq {
    pub ref_str: String,
    pub why: String,
}

/// 协商一轮的摘要（落 runs.negotiation JSON 数组）
#[derive(Debug, Clone)]
pub struct RoundSummary {
    pub round: u32,
    pub prompt_len: usize,
    pub missing: Vec<MissingReq>,
    pub resolved: Vec<String>,
    pub unavailable: Vec<String>,
}

/// 协议注入文本：告诉模型可以声明缺口（无它模型不知道有此通道）。
/// 只注入一次（prompt_0 追加，后续轮次继承在场）。
pub fn protocol_note() -> String {
    "\n\n# 上下文协商协议\n\
如果你判断当前信息不足以产出高质量结果，可以在输出的 JSON 对象中只输出：\n\
{\"enough\": false, \"missing\": [{\"ref\": \"<引用>\", \"why\": \"<一句话用途>\"}]}\n\
可用的引用形态：\n\
- @proc.field —— 上游节点的结构化字段（如 @req.constraints）\n\
- @proc —— 上游节点的完整输出文本\n\
- topic —— 本次任务的主题输入全文\n\
- script:<name> —— 已注册脚本的契约卡（参数/输出/不变量）\n\
规则：enough=false 时不得输出业务字段；每条 missing 必须带 ref 和 why；\n\
信息足够时照常输出全部契约字段（可省略 enough）。最多协商 3 轮，请优先\
声明最关键的缺口。"
        .to_string()
}

/// 从 LLM 输出的结构化字段里识别协商回合。
/// 返回 Some(missing) = 模型声明不足（enough=false 且 missing 非空）；
/// None = 正常产出（enough=true/缺省，或违约走调用方 fail 路径）。
pub fn parse_missing(encoded: &str) -> Option<Vec<MissingReq>> {
    let enough = dslresult::extract_field("enough", encoded).unwrap_or_default();
    let enough = enough.trim().to_lowercase();
    if !(enough == "false" || enough == "0") {
        return None;
    }
    let raw = dslresult::extract_field("missing", encoded).unwrap_or_default();
    if raw.trim().is_empty() {
        return None; // enough=false 但 missing 空 = 违约，调用方按 fail 处理
    }
    Some(parse_missing_json(&raw).unwrap_or_default())
}

/// missing 字段的 JSON 解析（手写——仓库依赖极简主义不引 serde_json；
/// 只提取数组内各对象的 "ref"/"why" 字符串值，解析失败容忍降级空表）
fn parse_missing_json(raw: &str) -> Option<Vec<MissingReq>> {
    let start = raw.find('[')?;
    let end = raw.rfind(']')?;
    if end <= start {
        return Some(Vec::new());
    }
    let slice = &raw[start..=end];
    let mut out = Vec::new();
    // 逐对象提取：找 {"ref": "...", "why": "..."} 形态
    let mut rest = slice;
    while let Some(obj_start) = rest.find('{') {
        let obj_end = match rest[obj_start..].find('}') {
            Some(e) => obj_start + e,
            None => break,
        };
        let obj = &rest[obj_start..=obj_end];
        let ref_str = extract_json_str_field(obj, "ref").unwrap_or_default();
        let why = extract_json_str_field(obj, "why").unwrap_or_default();
        if !ref_str.is_empty() {
            out.push(MissingReq { ref_str, why });
        }
        rest = &rest[obj_end..];
    }
    Some(out)
}

/// 从单层 JSON 对象文本中提取 "key": "value" 的 value（容忍空白/转义）
fn extract_json_str_field(obj: &str, key: &str) -> Option<String> {
    let pat = format!("\"{}\"", key);
    let kpos = obj.find(&pat)?;
    let colon = obj[kpos + pat.len()..].find(':')? + kpos + pat.len();
    let after = &obj[colon + 1..];
    let q1 = after.find('"')?;
    // 转义感知扫描字符串值
    let mut out = String::new();
    let mut chars = after[q1 + 1..].char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                if let Some((_, n)) = chars.next() {
                    out.push(match n {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                }
            }
            '"' => return Some(out),
            _ => {
                let _ = i;
                out.push(c);
            }
        }
    }
    Some(out)
}

/// 解析一条 ref，返回补充文本。Err = 解析失败（调用方转 unavailable 告知）。
/// 窗口纪律（规格 2.2）：@proc.field 走字段窗口 2000；@proc/topic 全文；
/// script: 契约卡全文。给不给、给多少由引擎决定——模型只能声明。
pub fn resolve_ref(
    ref_str: &str,
    topic: &str,
    results: &std::collections::BTreeMap<String, crate::core::ast::Value>,
) -> Result<String, String> {
    let s = ref_str.trim();
    if s == "topic" {
        return Ok(topic.to_string());
    }
    if let Some(name) = s.strip_prefix("script:") {
        return match crate::db::script_get(name) {
            Some(card) => Ok(format!(
                "# script:{} 契约卡\nname={}\ndesc={}\nparams={}\noutput={}\npure={} idempotent={} concurrency={}",
                name, card.name, card.desc, card.params, card.output, card.pure, card.idempotent,
                format!("{:?}", card.concurrency)
            )),
            None => Err(format!("script:{} 未注册", name)),
        };
    }
    if let Some(rest) = s.strip_prefix('@') {
        if let Some((proc, field)) = rest.split_once('.') {
            let val = results
                .get(proc)
                .ok_or_else(|| format!("@{} 不存在", proc))?;
            let text = val.as_text();
            let v = dslresult::extract_field(field, &text)
                .ok_or_else(|| format!("@{}.{} 字段不存在", proc, field))?;
            let win: String = v.chars().take(2000).collect();
            return Ok(win);
        }
        let val = results
            .get(rest)
            .ok_or_else(|| format!("@{} 不存在", rest))?;
        return Ok(val.as_text().to_string());
    }
    Err(format!(
        "不可解析的引用形态 '{}'（支持 @proc.field / @proc / topic / script:name）",
        s
    ))
}

/// prompt 里是否已提供过该 ref（补充块头部带 ref 标记）。
/// already-provided 防重复讨要；同一 ref 第二次讨要直接 unavailable。
pub fn already_provided(prompt: &str, ref_str: &str) -> bool {
    let marker = format!("[{}]", ref_str.trim());
    prompt.contains(&format!("## 补充 {}", marker))
}

/// 构造追加块（规格 2.3：追加不重建——prompt_r+1 = prompt_r + 本块）
pub fn supplement_block(round: u32, items: &[(String, String)]) -> String {
    let mut out = format!("\n\n# 补充信息（协商第 {} 轮）\n", round);
    for (ref_str, content) in items {
        out.push_str(&format!("## 补充 [{}]\n{}\n\n", ref_str.trim(), content));
    }
    out
}

/// 协商日志 JSON（落 runs.negotiation；手写序列化）
pub fn negotiation_log(summaries: &[RoundSummary], final_prompt_len: usize) -> String {
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
    }
    let rounds: Vec<String> = summaries
        .iter()
        .map(|s| {
            let missing: Vec<String> = s
                .missing
                .iter()
                .map(|m| format!("{{\"ref\":\"{}\",\"why\":\"{}\"}}", esc(&m.ref_str), esc(&m.why)))
                .collect();
            let resolved: Vec<String> = s.resolved.iter().map(|r| format!("\"{}\"", esc(r))).collect();
            let unavailable: Vec<String> =
                s.unavailable.iter().map(|r| format!("\"{}\"", esc(r))).collect();
            format!(
                "{{\"round\":{},\"prompt_len\":{},\"missing\":[{}],\"resolved\":[{}],\"unavailable\":[{}]}}",
                s.round, s.prompt_len, missing.join(","), resolved.join(","), unavailable.join(",")
            )
        })
        .collect();
    format!(
        "{{\"rounds\":[{}],\"final_prompt_len\":{}}}",
        rounds.join(","),
        final_prompt_len
    )
}


// ── 协商日志的跨层传递（steps → executor → db insert）──
// exec_llm 协商壳在返回前 stash，append_run 落库时 take——时序对齐
// （壳返回时本节点 runs 行尚不存在，直接 UPDATE MAX(id) 会挂到别的节点上）。
thread_local! {
    static PENDING_LOG: RefCell<Option<String>> = RefCell::new(None);
}

pub fn stash_log(log: String) {
    PENDING_LOG.with(|c| *c.borrow_mut() = Some(log));
}

pub fn take_log() -> Option<String> {
    PENDING_LOG.with(|c| c.borrow_mut().take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ast::Value;
    use std::collections::BTreeMap;

    fn enc(fields: &[(&str, &str)]) -> String {
        let kvs: Vec<(String, String)> = fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        crate::core::dslresult::encode_structured_result(&kvs, "raw")
    }

    // ── 协议识别 ──
    #[test]
    fn enough_false_with_missing_is_negotiation() {
        let e = enc(&[
            ("enough", "false"),
            ("missing", r#"[{"ref":"@req.constraints","why":"选型需要预算"}]"#),
        ]);
        let m = parse_missing(&e).expect("应识别为协商回合");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].ref_str, "@req.constraints");
        assert_eq!(m[0].why, "选型需要预算");
    }

    #[test]
    fn enough_true_or_absent_is_normal() {
        assert!(parse_missing(&enc(&[("enough", "true"), ("x", "1")])).is_none());
        assert!(parse_missing(&enc(&[("score", "85")])).is_none());
    }

    #[test]
    fn enough_false_empty_missing_is_violation_not_negotiation() {
        // 违约：enough=false 但 missing 空 → None（调用方按 fail-closed 处理）
        assert!(parse_missing(&enc(&[("enough", "false")])).is_none());
    }

    // ── 解析表 ──
    #[test]
    fn resolve_topic_field_proc_and_script() {
        let mut results = BTreeMap::new();
        results.insert(
            "req".into(),
            Value::Text(enc(&[("constraints", "预算500元内，别太复杂"), ("title", "x")])),
        );
        // topic 全文
        assert_eq!(resolve_ref("topic", "主题ABC", &results).unwrap(), "主题ABC");
        // @proc.field 窗口 2000
        let v = resolve_ref("@req.constraints", "t", &results).unwrap();
        assert!(v.contains("预算500元"));
        // @proc 全文
        let full = resolve_ref("@req", "t", &results).unwrap();
        assert!(full.contains("constraints"));
        // 不存在 → Err
        assert!(resolve_ref("@nope.field", "t", &results).is_err());
        assert!(resolve_ref("@req.nofield", "t", &results).is_err());
        // 非法形态 → Err
        assert!(resolve_ref("随便什么", "t", &results).is_err());
    }

    #[test]
    fn resolve_field_window_truncated_at_2000_chars() {
        let mut results = BTreeMap::new();
        let long = "字".repeat(5000);
        results.insert(
            "big".into(),
            Value::Text(enc(&[("data", &long)])),
        );
        let v = resolve_ref("@big.data", "t", &results).unwrap();
        assert_eq!(v.chars().count(), 2000);
    }

    // ── 追加不重建 + already-provided ──
    #[test]
    fn supplement_appends_and_marker_detects_provided() {
        let p0 = "基础prompt".to_string();
        let block = supplement_block(1, &[("@req.constraints".into(), "预算500元内".into())]);
        let p1 = format!("{}{}", p0, block);
        // 只增不减：p0 原文仍在
        assert!(p1.starts_with(p0.as_str()));
        // 已提供检测：同一 ref 第二次讨要应被识别
        assert!(already_provided(&p1, "@req.constraints"));
        assert!(!already_provided(&p1, "@req.title"));
        // 第二轮追加后两块都在场
        let p2 = format!("{}{}", p1, supplement_block(2, &[("@req.title".into(), "标题X".into())]));
        assert!(already_provided(&p2, "@req.constraints"));
        assert!(already_provided(&p2, "@req.title"));
    }

    // ── 协商日志 ──
    #[test]
    fn negotiation_log_json_shape() {
        let s = vec![RoundSummary {
            round: 1,
            prompt_len: 800,
            missing: vec![MissingReq {
                ref_str: "@req.constraints".into(),
                why: "预算".into(),
            }],
            resolved: vec!["@req.constraints".into()],
            unavailable: vec![],
        }];
        let j = negotiation_log(&s, 1200);
        assert!(j.contains("\"round\":1"), "{}", j);
        assert!(j.contains("\"ref\":\"@req.constraints\""));
        assert!(j.contains("\"final_prompt_len\":1200"));
    }
}
