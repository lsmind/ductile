//! v0.11.1 `.when()` 条件路由 —— **Interpreter 模式**。
//!
//! SPEC 承诺的裁判路由 `.when(@gate.score < 80)` 此前从未实现：
//! 旧 `eval_when` 只支持 params 上的字符串 `==`/`!=`（`_results` 参数带下划线未用），
//! `@proc.field` 数值比较是空话。本模块把它落地为文法→AST→求值的正规解释器：
//!
//! ```text
//! cond   := operand OP operand | operand          （单 operand = 存在性）
//! operand := "literal" | number | param_name | @proc.field
//! OP     := == | != | >= | <= | > | <
//! ```
//!
//! 判定原则（fail-closed）：
//! - **裁判引用 `@gate.score`**：gate 未跑出该字段 → 条件不成立（不过审不放行）；
//!   数值比较双端必须都能解析为 f64，任一端缺失/非数值 → 不成立。
//! - **参数引用 `mode`**：与旧语义一致（缺 param = 不成立）。
//! - 空条件 = 恒真（无路由声明）。
//!
//! 比较双方先用精确字符串判等；数值比较才走 f64 解析（`score=85` vs `85.0` 也能比）。

use std::collections::BTreeMap;

use crate::ast::Value;

/// 比较算子（文法 OP）。
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Eq,
    Ne,
    Ge,
    Le,
    Gt,
    Lt,
}

impl Op {
    fn eval(&self, lhs: f64, rhs: f64) -> bool {
        match self {
            Op::Eq => lhs == rhs,
            Op::Ne => lhs != rhs,
            Op::Ge => lhs >= rhs,
            Op::Le => lhs <= rhs,
            Op::Gt => lhs > rhs,
            Op::Lt => lhs < rhs,
        }
    }
    fn as_str(&self) -> &'static str {
        match self {
            Op::Eq => "==",
            Op::Ne => "!=",
            Op::Ge => ">=",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Lt => "<",
        }
    }
}

/// 操作数（文法 operand）。
#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    /// 引号字面量（"deep"）
    Lit(String),
    /// 数值字面量（80 / 0.75）
    Num(f64),
    /// 裸参数名（mode）——查 CLI params
    Param(String),
    /// 裁判引用（@gate.score）——查上游 proc 的 DSL_RESULT 字段
    ResultField { proc: String, field: String },
}

/// 条件 AST（文法 cond）。
#[derive(Debug, Clone)]
pub enum Cond {
    /// 单操作数：存在性判定
    Exists(Operand),
    /// 二元比较
    Cmp(Box<Operand>, Op, Box<Operand>),
}

/// 求值上下文：CLI params + 上游 proc 结果。
pub struct CondCtx<'a> {
    pub params: &'a BTreeMap<String, String>,
    pub results: &'a BTreeMap<String, Value>,
}

impl Operand {
    /// 解析为字符串（用于 Eq/Ne 与存在性）。
    /// Param 缺失 → None；ResultField 缺失/非结构化 → None。
    fn as_string(&self, ctx: &CondCtx) -> Option<String> {
        match self {
            Operand::Lit(s) => Some(s.clone()),
            Operand::Num(n) => Some(fmt_num(*n)),
            Operand::Param(name) => ctx.params.get(name).cloned(),
            Operand::ResultField { proc, field } => result_field(ctx.results, proc, field),
        }
    }

    /// 解析为数值（用于数值比较）。非数值 → None。
    fn as_number(&self, ctx: &CondCtx) -> Option<f64> {
        match self {
            Operand::Lit(s) => s.trim().parse::<f64>().ok(),
            Operand::Num(n) => Some(*n),
            Operand::Param(name) => ctx.params.get(name).and_then(|v| v.trim().parse().ok()),
            Operand::ResultField { proc, field } => {
                result_field(ctx.results, proc, field).and_then(|v| v.trim().parse().ok())
            }
        }
    }
}

/// DSL_RESULT 编码字段提取（§§FIELDS§§k=v... 协议；裸文本也可解析 k=v 形态）。
/// 与 executor::extract_field 解耦：这里只做轻量查找，避免环依赖。
fn result_field(results: &BTreeMap<String, Value>, proc_name: &str, field: &str) -> Option<String> {
    let val = results.get(proc_name)?;
    let text = match val {
        Value::Text(t) => t,
        _ => return None,
    };
    extract_encoded_field(text, field)
}

/// 从 `§§FIELDS§§k=v...§§RAW§§...` 或裸 `k=v` 文本中取字段。
pub fn extract_encoded_field(text: &str, field: &str) -> Option<String> {
    // 1) 结构化编码形态
    if let Some(rest) = text.strip_prefix("§§FIELDS§§") {
        let section = rest.split("§§RAW§§").next().unwrap_or(rest);
        for kv in section.split("§§") {
            if let Some((k, v)) = kv.split_once('=') {
                if k == field {
                    return Some(v.to_string());
                }
            }
        }
        return None;
    }
    // 2) 裸 stdout 兜底：行内按空白分词找 `k=v` token（judge proc 可直接 print score=85）
    for line in text.lines() {
        for token in line.split_whitespace() {
            if let Some((k, v)) = token.split_once('=') {
                if k.trim() == field && !v.is_empty() {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}

impl Cond {
    /// 求值（fail-closed 语义见模块注释）。
    pub fn eval(&self, ctx: &CondCtx) -> bool {
        match self {
            Cond::Exists(op) => match op {
                // 裸参数保持旧语义：值 == "true" 才成立（verbose=true/false 开关形态）
                Operand::Param(name) => ctx.params.get(name).map(|v| v == "true").unwrap_or(false),
                // 裁判字段/字面量：存在即成立
                _ => op.as_string(ctx).is_some(),
            },
            Cond::Cmp(lhs, op, rhs) => {
                // 数值优先：双端都是数值 → 数值比较（85 vs 85.0 可比）。
                // 否则只对 Eq/Ne 退化为字符串比较；序比较缺数值一律 fail-closed(false)。
                match (lhs.as_number(ctx), rhs.as_number(ctx)) {
                    (Some(l), Some(r)) => op.eval(l, r),
                    _ => match op {
                        Op::Eq => lhs.as_string(ctx) == rhs.as_string(ctx),
                        Op::Ne => lhs.as_string(ctx) != rhs.as_string(ctx),
                        _ => false,
                    },
                }
            }
        }
    }
}

// ── Parser（文法 → AST）──

/// 解析 when 条件文本。空/空白 → None（恒真）。语法非法 → Err。
pub fn parse_when(cond: &str) -> Result<Option<Cond>, String> {
    // 只在整条条件被成对引号包裹时剥外层（旧 `.when("fast")` 形态）。
    // 不能用 trim_matches——它会把 `mode == "deep"` 的右操作数收尾引号啃掉。
    let trimmed = cond.trim();
    let cond = if trimmed.len() >= 2
        && trimmed.starts_with('"')
        && trimmed.ends_with('"')
        && !trimmed[1..trimmed.len() - 1].contains('"')
    {
        trimmed[1..trimmed.len() - 1].trim()
    } else {
        trimmed
    };
    if cond.is_empty() {
        return Ok(None);
    }
    // 按算子长度降序找（>= 优先于 >）
    for (pat, op) in [
        ("==", Op::Eq),
        ("!=", Op::Ne),
        (">=", Op::Ge),
        ("<=", Op::Le),
        (">", Op::Gt),
        ("<", Op::Lt),
    ] {
        if let Some(pos) = cond.find(pat) {
            let lhs = parse_operand(cond[..pos].trim())?;
            let rhs = parse_operand(cond[pos + pat.len()..].trim())?;
            return Ok(Some(Cond::Cmp(Box::new(lhs), op, Box::new(rhs))));
        }
    }
    // 无算子：裸存在性（param 或 @proc.field）
    Ok(Some(Cond::Exists(parse_operand(cond)?)))
}

fn parse_operand(s: &str) -> Result<Operand, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty operand in .when() condition".into());
    }
    // 引号字面量
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        return Ok(Operand::Lit(s[1..s.len() - 1].to_string()));
    }
    // 裁判引用 @proc.field
    if let Some(rest) = s.strip_prefix('@') {
        let (proc, field) = rest.split_once('.').ok_or_else(|| {
            format!(
                ".when: '@{}' missing .field — judge refs need @proc.field form",
                rest
            )
        })?;
        if proc.is_empty() || field.is_empty() {
            return Err(format!(".when: bad judge ref '@{}'", rest));
        }
        return Ok(Operand::ResultField {
            proc: proc.to_string(),
            field: field.to_string(),
        });
    }
    // 数值
    if let Ok(n) = s.parse::<f64>() {
        return Ok(Operand::Num(n));
    }
    // 裸参数名：合法标识符
    if s.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Ok(Operand::Param(s.to_string()));
    }
    Err(format!(".when: cannot parse operand {:?}", s))
}

/// 便捷入口：解析 + 求值（executor 兼容层用）。
/// 解析失败 = fail-closed false（并留 stderr 提示——坏条件不该静默放行）。
pub fn eval_cond_str(
    cond: &str,
    params: &BTreeMap<String, String>,
    results: &BTreeMap<String, Value>,
) -> bool {
    match parse_when(cond) {
        Ok(Some(c)) => c.eval(&CondCtx { params, results }),
        Ok(None) => true,
        Err(e) => {
            eprintln!("  [when] bad condition {:?}: {} — fail-closed", cond, e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(params: &[(&str, &str)], results: &[(&str, &str)]) -> CondCtx<'static> {
        // 泄漏以获得 'static —— 测试专用，量小无碍
        let p: BTreeMap<String, String> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let r: BTreeMap<String, Value> = results
            .iter()
            .map(|(k, v)| (k.to_string(), Value::Text(v.to_string())))
            .collect();
        CondCtx {
            params: Box::leak(Box::new(p)),
            results: Box::leak(Box::new(r)),
        }
    }

    // ── 裁判路由主路径：@gate.score < 80 ──

    #[test]
    fn judge_route_below_threshold() {
        let c = ctx(&[], &[("gate", "§§FIELDS§§score=72§§RAW§§ok")]);
        let cond = parse_when("@gate.score < 80").unwrap().unwrap();
        assert!(cond.eval(&c)); // 72 < 80 → 走返工路径
    }

    #[test]
    fn judge_route_above_threshold_blocks() {
        let c = ctx(&[], &[("gate", "§§FIELDS§§score=85§§RAW§§ok")]);
        let cond = parse_when("@gate.score < 80").unwrap().unwrap();
        assert!(!cond.eval(&c));
    }

    #[test]
    fn judge_score_fractional_compare() {
        let c = ctx(&[], &[("gate", "score=0.85")]);
        let cond = parse_when("@gate.score >= 0.75").unwrap().unwrap();
        assert!(cond.eval(&c));
    }

    // ── fail-closed：裁判缺席 = 不放行 ──

    #[test]
    fn judge_missing_proc_fails_closed() {
        let c = ctx(&[], &[]);
        let cond = parse_when("@gate.score < 80").unwrap().unwrap();
        assert!(!cond.eval(&c));
    }

    #[test]
    fn judge_missing_field_fails_closed() {
        let c = ctx(&[], &[("gate", "§§FIELDS§§other=1§§RAW§§x")]);
        let cond = parse_when("@gate.score < 80").unwrap().unwrap();
        assert!(!cond.eval(&c));
    }

    #[test]
    fn judge_non_numeric_field_fails_closed_on_order_cmp() {
        let c = ctx(&[], &[("gate", "§§FIELDS§§score=high")]);
        let cond = parse_when("@gate.score < 80").unwrap().unwrap();
        assert!(!cond.eval(&c));
    }

    #[test]
    fn bad_condition_string_fails_closed() {
        let c = ctx(&[], &[]);
        // @ref 缺 .field → 解析错误 → false（不放行）
        assert!(!eval_cond_str(
            "@gate",
            &c.params.clone(),
            &c.results.clone()
        ));
    }

    // ── 参数条件（旧语义兼容）──

    #[test]
    fn param_eq_literal() {
        let c = ctx(&[("mode", "deep")], &[]);
        let cond = parse_when("mode == \"deep\"").unwrap().unwrap();
        assert!(cond.eval(&c));
    }

    #[test]
    fn param_ne_literal() {
        let c = ctx(&[("mode", "fast")], &[]);
        let cond = parse_when("mode != \"deep\"").unwrap().unwrap();
        assert!(cond.eval(&c));
    }

    #[test]
    fn param_missing_fails_closed() {
        let c = ctx(&[], &[]);
        let cond = parse_when("mode == \"deep\"").unwrap().unwrap();
        assert!(!cond.eval(&c));
    }

    #[test]
    fn bare_param_true_semantics() {
        let c = ctx(&[("verbose", "true")], &[]);
        let cond = parse_when("verbose").unwrap().unwrap();
        assert!(cond.eval(&c));
        let c2 = ctx(&[("verbose", "false")], &[]);
        assert!(!cond.eval(&c2)); // 存在但非 "true"？——v1: 存在即真（与旧版差异钉死在测试）
    }

    // ── Eq 字符串退化 ──

    #[test]
    fn eq_string_field() {
        let c = ctx(&[], &[("gen", "§§FIELDS§§status=ok§§RAW§§x")]);
        let cond = parse_when("@gen.status == \"ok\"").unwrap().unwrap();
        assert!(cond.eval(&c));
    }

    #[test]
    fn numeric_vs_string_field_eq() {
        // score=85 vs 85（数值字面量）→ 数值路径可比
        let c = ctx(&[], &[("gate", "§§FIELDS§§score=85")]);
        let cond = parse_when("@gate.score == 85").unwrap().unwrap();
        assert!(cond.eval(&c));
    }

    // ── parser 边界 ──

    #[test]
    fn empty_cond_is_none() {
        assert!(parse_when("").unwrap().is_none());
        assert!(parse_when("  ").unwrap().is_none());
        assert!(parse_when("\"\"").unwrap().is_none());
    }

    #[test]
    fn operators_parse_in_priority_order() {
        // >= 不被 > 吃掉
        let c = ctx(&[("n", "5")], &[]);
        let cond = parse_when("n >= 5").unwrap().unwrap();
        assert!(cond.eval(&c));
        let cond2 = parse_when("n <= 5").unwrap().unwrap();
        assert!(cond2.eval(&c));
    }

    #[test]
    fn extract_encoded_field_forms() {
        assert_eq!(
            extract_encoded_field("§§FIELDS§§a=1§§b=2§§RAW§§raw", "b"),
            Some("2".into())
        );
        assert_eq!(
            extract_encoded_field("plain k=v\ntail", "k"),
            Some("v".into())
        );
        assert_eq!(extract_encoded_field("nothing", "k"), None);
    }
}
