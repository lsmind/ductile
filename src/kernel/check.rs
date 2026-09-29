//! Ductile v0.24 表达式求值器 + 类型检查器 — 行动③ D3-4。
//!
//! 两遍策略（规范 §3 三遍的切片实现）：
//!   第一遍：parser 产出 AST（span 齐全）
//!   第二遍（本文件）：
//!     a) 类型检查：字面量精确定型/禁隐式转换/同型比较/谓词 bool/E202 未解析硬错
//!     b) 常量折叠求值：纯表达式在编译期可求值（谓词、静态 list 源）
//!
//! 判决兑现（审稿 B/A）：
//!   - 未解析变量绝不"原样保留"——编译期 E202 硬错
//!   - $item 越层 → E205
//!   - ==/!= 同型、序比较仅 str/int/float → E207
//!   - 谓词非 bool → E207
//!   - len/contains/unique/get 内建函数封闭签名
//!   - Diagnostic：code+span+expected/got+可复制 fix 命令

use crate::kernel::ast::*;
use crate::kernel::types::{ErrCode, RefPath, SkipReason, Value};
use std::collections::BTreeMap;
use std::fmt;

// ── Diagnostic（规范 §5：码+span+expected/got+fix）────────

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub code: ErrCode,
    pub span: Span,
    pub expected: String,
    pub got: String,
    pub message: String,
    /// 可复制修复命令（如 `ductile fix E202 file:12:3`）。
    pub fix: Option<String>,
}

impl Diagnostic {
    pub fn new(code: ErrCode, span: Span, expected: &str, got: &str, message: impl Into<String>) -> Self {
        Diagnostic {
            code,
            span,
            expected: expected.to_string(),
            got: got.to_string(),
            message: message.into(),
            fix: Some(format!("ductile fix {} {}:{}", code.code(), span.start, span.end)),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "error[{}]: {} (expected {}, got {}) at {}{}",
            self.code.code(),
            self.message,
            self.expected,
            self.got,
            self.span,
            self.fix.as_deref().map(|c| format!("; fix: `{}`", c)).unwrap_or_default()
        )
    }
}

pub type CheckResult<T> = Result<T, Diagnostic>;

// ── 类型环境 ─────────────────────────────────────────────

/// 词法作用域：$topic（若存在）、$item（foreach 子树内）、@node 输出 schema。
#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub topic: bool,
    /// item 类型（当前 foreach 体）
    pub item: Option<&'static str>,
    /// 已声明节点 id → 输出 schema（封闭 Record 或标量）
    pub nodes: BTreeMap<String, NodeSchema>,
}

/// 节点输出的可寻址 schema：
///   fields 命中 → 字段 schema；status/ok/stdout/meta/error/reason → 内建。
#[derive(Debug, Clone)]
pub struct NodeSchema {
    pub fields: Option<crate::kernel::types::Schema>,
}

/// 内建属性类型（补丁①属性表）。
fn builtin_attr(node: &str, attr: &str, fields: &Option<crate::kernel::types::Schema>) -> Option<&'static str> {
    match attr {
        "status" | "stdout" => Some("str"),
        "ok" => Some("bool"),
        "meta" => Some("map"),
        "error" | "reason" => Some("outcome_attr"), // 合法性运行期判（E317），类型层放行
        "fields" => fields.as_ref().map(|_| "fields"),
        _ => {
            // fields.k 直取：封闭 schema 才合法（补丁①）
            if let Some(crate::kernel::types::Schema::Record(rec)) = fields {
                if rec.contains_key(attr) {
                    return Some("field");
                }
            }
            let _ = node;
            None
        }
    }
}

// ── 表达式类型推断（严格、无隐式转换）────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Str,
    Int,
    Float,
    Bool,
    List(Box<Ty>),
    Map(Box<Ty>),
    /// 谓词上下文中的结果 bool；来自任何同型比较。
    PredBool,
    /// Outcome 属性（error/reason/fields.k）：类型层放行，运行期 E317 守卫。
    OutcomeAttr,
}

impl Ty {
    pub fn name(&self) -> String {
        match self {
            Ty::Str => "str".into(),
            Ty::Int => "int".into(),
            Ty::Float => "float".into(),
            Ty::Bool | Ty::PredBool => "bool".into(),
            Ty::List(t) => format!("list<{}>", t.name()),
            Ty::Map(t) => format!("map<{}>", t.name()),
            Ty::OutcomeAttr => "outcome_attr".into(),
        }
    }
    fn same(&self, other: &Ty) -> bool {
        self.name() == other.name()
    }
}

fn lit_ty(l: &Lit) -> Ty {
    match l {
        Lit::Str(_) => Ty::Str,
        Lit::Int(_) => Ty::Int,
        Lit::Float(_) => Ty::Float,
        Lit::Bool(_) => Ty::Bool,
    }
}

/// 内建函数封闭签名（补丁①）：
///   len(str|list<T>|map<T>)→int；contains(list<T>,T)→bool；
///   unique(list<T>)→bool；get(map<T>,str)→T
fn check_call(name: &str, args: &[&Expr], scope: &Scope, span: Span) -> CheckResult<Ty> {
    let arg_ty = |i: usize| -> CheckResult<Ty> { infer(args[i], scope) };
    match name {
        "len" => {
            if args.len() != 1 {
                return Err(Diagnostic::new(ErrCode::E201, span, "1 arg", &format!("{} args", args.len()), "len/1"));
            }
            let t = arg_ty(0)?;
            match t {
                Ty::Str | Ty::Int | Ty::Map(_) => Err(Diagnostic::new(
                    ErrCode::E207, span, "str|list|map", &t.name(), "len 需要序列类型",
                )),
                Ty::List(_) => Ok(Ty::Int),
                other => Err(Diagnostic::new(ErrCode::E207, span, "sequence", &other.name(), "len 类型不符")),
            }
        }
        "contains" => {
            if args.len() != 2 {
                return Err(Diagnostic::new(ErrCode::E201, span, "2 args", &format!("{} args", args.len()), "contains/2"));
            }
            let t0 = arg_ty(0)?;
            let t1 = arg_ty(1)?;
            match t0 {
                Ty::List(elem) => {
                    if elem.same(&t1) {
                        Ok(Ty::Bool)
                    } else {
                        Err(Diagnostic::new(ErrCode::E207, span, &elem.name(), &t1.name(), "contains 元素类型不符"))
                    }
                }
                other => Err(Diagnostic::new(ErrCode::E207, span, "list", &other.name(), "contains/1 需要 list")),
            }
        }
        "unique" => {
            if args.len() != 1 {
                return Err(Diagnostic::new(ErrCode::E201, span, "1 arg", &format!("{} args", args.len()), "unique/1"));
            }
            let t = arg_ty(0)?;
            match t {
                Ty::List(_) => Ok(Ty::Bool),
                other => Err(Diagnostic::new(ErrCode::E207, span, "list", &other.name(), "unique/1 需要 list")),
            }
        }
        "get" => {
            if args.len() != 2 {
                return Err(Diagnostic::new(ErrCode::E201, span, "2 args", &format!("{} args", args.len()), "get/2"));
            }
            let t0 = arg_ty(0)?;
            let t1 = arg_ty(1)?;
            if !t1.same(&Ty::Str) {
                return Err(Diagnostic::new(ErrCode::E207, span, "str", &t1.name(), "get/2 需要 str 键"));
            }
            match t0 {
                Ty::Map(elem) => Ok(*elem),
                other => Err(Diagnostic::new(ErrCode::E207, span, "map", &other.name(), "get/1 需要 map")),
            }
        }
        _ => Err(Diagnostic::new(
            ErrCode::E202,
            span,
            "known builtin: len/contains/unique/get",
            name,
            "未知函数（fail-closed，无自由函数）",
        )),
    }
}

pub fn infer(e: &Expr, scope: &Scope) -> CheckResult<Ty> {
    match e {
        Expr::Lit(l, _) => Ok(lit_ty(l)),
        Expr::List(items, span) => {
            // 列表字面量全同质；空列表须由上下文定型（这里返回占位报错——
            // 谓词上下文不该出现裸空列表）
            let mut elem: Option<Ty> = None;
            for item in items {
                let t = infer(item, scope)?;
                match &elem {
                    None => elem = Some(t),
                    Some(prev) if prev.same(&t) => {}
                    Some(prev) => {
                        return Err(Diagnostic::new(
                            ErrCode::E201,
                            e.span(),
                            &prev.name(),
                            &t.name(),
                            "list 元素不同质",
                        ))
                    }
                }
            }
            match elem {
                Some(t) => Ok(Ty::List(Box::new(t))),
                None => Err(Diagnostic::new(
                    ErrCode::E205,
                    *span,
                    "非空列表或上下文定型",
                    "[]",
                    "空列表须由上下文定型（谓词中不允许裸空列表）",
                )),
            }
        }
        Expr::Var(kind, span) => match kind {
            VarKind::Topic => {
                if scope.topic {
                    Ok(Ty::Str)
                } else {
                    Err(Diagnostic::new(
                        ErrCode::E202,
                        *span,
                        "$topic（管线声明 topic 输入）",
                        "$topic",
                        "本管线未声明 topic 输入，$topic 未解析——绝不静默保留",
                    ))
                }
            }
            VarKind::Item => {
                if let Some(t) = scope.item {
                    // item 类型切片期固定为 str；完整版由 foreach source 静态类型驱动
                    let _ = t;
                    Ok(Ty::Str)
                } else {
                    Err(Diagnostic::new(
                        ErrCode::E205,
                        *span,
                        "$item（仅在 foreach 体内）",
                        "$item",
                        "$item 越出 foreach 词法子树",
                    ))
                }
            }
            VarKind::Hash(inner) => {
                let t = infer(inner, scope)?;
                if t.same(&Ty::Str) {
                    Ok(Ty::Str)
                } else {
                    Err(Diagnostic::new(ErrCode::E201, *span, "str", &t.name(), "$hash 参数必须是 str"))
                }
            }
        },
        Expr::Path(path, span) => {
            // @id 必须在作用域（前向引用允许但此处 nodes 表在构图后填——
            // 切片期：检查 id 存在性 + 属性合法性）
            check_ref(path, scope, *span)
        }
        Expr::Call { name, args, span } => {
            let refs: Vec<&Expr> = args.iter().collect();
            check_call(name, &refs, scope, *span)
        }
        Expr::Cmp { op, lhs, rhs, span } => {
            let lt = infer(lhs, scope)?;
            let rt = infer(rhs, scope)?;
            if !lt.same(&rt) {
                return Err(Diagnostic::new(
                    ErrCode::E201,
                    *span,
                    &lt.name(),
                    &rt.name(),
                    format!("{} 两侧类型不同（禁隐式转换）", op.symbol()),
                ));
            }
            if op.is_ordering()
                && !matches!(lt, Ty::Str | Ty::Int | Ty::Float)
            {
                return Err(Diagnostic::new(
                    ErrCode::E207,
                    *span,
                    "str|int|float",
                    &lt.name(),
                    "序比较仅限 str/int/float",
                ));
            }
            Ok(Ty::PredBool)
        }
        Expr::Not(inner, span) => {
            let t = infer(inner, scope)?;
            if t.same(&Ty::Bool) || t.same(&Ty::PredBool) {
                Ok(Ty::PredBool)
            } else {
                Err(Diagnostic::new(ErrCode::E207, *span, "bool", &t.name(), "! 需要 bool"))
            }
        }
        Expr::And(l, r, span) | Expr::Or(l, r, span) => {
            let lt = infer(l, scope)?;
            let rt = infer(r, scope)?;
            let opname = if matches!(e, Expr::And(..)) { "&&" } else { "||" };
            for (t, side) in [(lt, "左"), (rt, "右")] {
                if !(t.same(&Ty::Bool) || t.same(&Ty::PredBool)) {
                    return Err(Diagnostic::new(
                        ErrCode::E207,
                        *span,
                        "bool",
                        &t.name(),
                        format!("{} {}侧需 bool", opname, side),
                    ));
                }
            }
            Ok(Ty::PredBool)
        }
    }
}

fn check_ref(path: &RefPath, scope: &Scope, span: Span) -> CheckResult<Ty> {
    // 根节点必须已声明（E202）
    let node = scope.nodes.get(&path.id).ok_or_else(|| {
        Diagnostic::new(
            ErrCode::E202,
            span,
            "已声明节点",
            &format!("@{}", path.id),
            format!("未解析引用 @{}——绝不静默保留原样文本", path.id),
        )
    })?;
    if path.fields.is_empty() {
        // 裸 @node：切片期不允许（outcome 不是值；用属性访问）
        return Err(Diagnostic::new(
            ErrCode::E203,
            span,
            "@id.field 形式",
            &path.to_string(),
            "裸 @id 不是值；请访问属性（.status/.ok/.fields.k）",
        ));
    }
    let first = &path.fields[0];
    match builtin_attr(&path.id, first, &node.fields) {
        Some("str") => Ok(Ty::Str),
        Some("bool") => Ok(Ty::Bool),
        Some("map") => Ok(Ty::Map(Box::new(Ty::Str))), // meta: map<str> 切片近似
        Some("fields") => {
            if path.fields.len() == 1 {
                Ok(Ty::Map(Box::new(Ty::Str)))
            } else {
                // @x.fields.k → k 必须在封闭 schema（E203）
                let k = &path.fields[1];
                if let Some(crate::kernel::types::Schema::Record(rec)) = &node.fields {
                    if rec.contains_key(k) {
                        Ok(field_ty(rec.get(k).unwrap()))
                    } else {
                        Err(Diagnostic::new(
                            ErrCode::E203,
                            span,
                            "schema 封闭字段",
                            k,
                            format!("@{}.fields.{} 不在封闭 schema（拼错即硬错）", path.id, k),
                        ))
                    }
                } else {
                    Err(Diagnostic::new(ErrCode::E203, span, "封闭 schema", &path.to_string(), "无封闭 fields"))
                }
            }
        }
        Some("field") => {
            // 直取 @x.k（补丁①允许 fields.k 的 schema 命中）
            if let Some(crate::kernel::types::Schema::Record(rec)) = &node.fields {
                Ok(field_ty(rec.get(first).unwrap()))
            } else {
                Err(Diagnostic::new(ErrCode::E203, span, "封闭 schema", &path.to_string(), "无封闭 fields"))
            }
        }
        Some("outcome_attr") => Ok(Ty::OutcomeAttr),
        _ => Err(Diagnostic::new(
            ErrCode::E203,
            span,
            "status|ok|stdout|meta|fields|error|reason|<schema字段>",
            first,
            format!("@{}.{} 未知属性（封闭 schema，无静默空串）", path.id, first),
        )),
    }
}

fn field_ty(s: &crate::kernel::types::Schema) -> Ty {
    use crate::kernel::types::Schema as S;
    match s {
        S::Str => Ty::Str,
        S::Int => Ty::Int,
        S::Float => Ty::Float,
        S::Bool => Ty::Bool,
        S::List(t) => Ty::List(Box::new(field_ty(t))),
        S::Map(t) => Ty::Map(Box::new(field_ty(t))),
        S::Record(_) => Ty::Map(Box::new(Ty::Str)), // 嵌套记录切片近似
    }
}

// ── 谓词检查入口（when/contract invariant）──────────────

/// 谓词必须是 bool（E207），且不能是 OutcomeAttr（error/reason 的合法性
/// 是运行期属性——谓词里用它们必须经 .ok/.status 归约）。
pub fn check_predicate(e: &Expr, scope: &Scope) -> CheckResult<()> {
    let t = infer(e, scope)?;
    if matches!(t, Ty::PredBool | Ty::Bool) {
        Ok(())
    } else {
        Err(Diagnostic::new(
            ErrCode::E207,
            e.span(),
            "bool（谓词）",
            &t.name(),
            ".when 条件必须是布尔表达式",
        ))
    }
}

// ── 常量折叠求值（纯表达式编译期求值；谓词=确定性）────────

pub struct EvalCtx<'a> {
    pub topic: Option<&'a str>,
    pub item: Option<&'a str>,
    /// 运行期节点字段（fields.k 当前值）——编译期求值只用于纯谓词，
    /// 含 Path 的表达式走运行期（此处返回 None）。
    pub runtime: BTreeMap<String, Value>,
}

/// 纯表达式求值：Lit/List/Var(有值)/Hash/Call/比较/布尔组合。
/// 含 Path → None（调用方走运行期路径）。
pub fn eval_const(e: &Expr, ctx: &EvalCtx) -> Option<Value> {
    match e {
        Expr::Lit(l, _) => Some(l.to_value()),
        Expr::List(items, _) => {
            let mut out = Vec::new();
            for i in items {
                out.push(eval_const(i, ctx)?);
            }
            Some(Value::List(out))
        }
        Expr::Var(VarKind::Topic, _) => ctx.topic.map(|t| Value::Str(t.to_string())),
        Expr::Var(VarKind::Item, _) => ctx.item.map(|t| Value::Str(t.to_string())),
        Expr::Var(VarKind::Hash(inner), _) => {
            let v = eval_const(inner, ctx)?;
            let digest = crate::kernel::hash::sha256_hex(&canonical_bytes(&v));
            Some(Value::Str(digest))
        }
        Expr::Path(..) => None, // 运行期
        Expr::Call { name, args, .. } => {
            let mut vals = Vec::new();
            for a in args {
                vals.push(eval_const(a, ctx)?);
            }
            crate::kernel::evalfns::apply(name, &vals)
        }
        Expr::Cmp { op, lhs, rhs, .. } => {
            let l = eval_const(lhs, ctx)?;
            let r = eval_const(rhs, ctx)?;
            crate::kernel::evalfns::cmp_value(*op, &l, &r).map(Value::Bool)
        }
        Expr::Not(inner, _) => {
            match eval_const(inner, ctx)? {
                Value::Bool(b) => Some(Value::Bool(!b)),
                _ => None,
            }
        }
        Expr::And(l, r, _) => {
            match (eval_const(l, ctx)?, eval_const(r, ctx)?) {
                (Value::Bool(a), Value::Bool(b)) => Some(Value::Bool(a && b)),
                _ => None,
            }
        }
        Expr::Or(l, r, _) => {
            match (eval_const(l, ctx)?, eval_const(r, ctx)?) {
                (Value::Bool(a), Value::Bool(b)) => Some(Value::Bool(a || b)),
                _ => None,
            }
        }
    }
}

fn canonical_bytes(v: &Value) -> Vec<u8> {
    // JCS 近似：BTreeMap 已键序；紧凑分隔。切片用规范化字节串。
    match v {
        Value::Str(s) => format!("\"{}\"", s).into_bytes(),
        Value::Int(i) => i.to_string().into_bytes(),
        Value::Float(x) => format!("{}", x).into_bytes(),
        Value::Bool(b) => b.to_string().into_bytes(),
        Value::List(items) => {
            let mut out = vec![b'['];
            for (i, item) in items.iter().enumerate() {
                if i > 0 { out.push(b','); }
                out.extend_from_slice(&canonical_bytes(item));
            }
            out.push(b']');
            out
        }
        Value::Map(m) => {
            let mut out = vec![b'{'];
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 { out.push(b','); }
                out.extend_from_slice(format!("\"{}\":", k).as_bytes());
                out.extend_from_slice(&canonical_bytes(v));
            }
            out.push(b'}');
            out
        }
        Value::Ref(p) => format!("\"@{}\"", p).into_bytes(),
    }
}

// ── 管线级检查（节点 id 唯一、分支契约一致 E206、谓词检查）──

pub fn check_pipeline(p: &Pipeline24) -> Result<Vec<Diagnostic>, Vec<Diagnostic>> {
    let mut diags = Vec::new();
    let mut scope = Scope { topic: p.has_topic, item: None, nodes: BTreeMap::new() };

    // 节点 id 唯一性
    let mut seen = std::collections::BTreeSet::new();
    for n in &p.nodes {
        if !seen.insert(n.id.as_str()) {
            diags.push(Diagnostic::new(
                ErrCode::E204,
                n.span,
                "唯一节点 id",
                &n.id,
                format!("节点 id '{}' 重复", n.id),
            ));
        }
    }

    // 逐节点：分支非空 + 分支契约一致 + 谓词检查
    for n in &p.nodes {
        if n.branches.is_empty() && n.foreach.is_none() {
            diags.push(Diagnostic::new(
                ErrCode::E101,
                n.span,
                "至少一个 impl 或 foreach",
                "空节点",
                format!("节点 '{}' 无分支", n.id),
            ));
            continue;
        }
        // 谓词（foreach 子作用域）
        let sub_scope = if let Some(fe) = &n.foreach {
            let mut s = scope.clone();
            s.item = Some("str");
            for b in &fe.body {
                check_node_branches(b, &mut s, &mut diags);
            }
            // foreach source 必须可推断为 list
            if let Err(d) = infer(&fe.source, &scope).map(|t| {
                if !matches!(t, Ty::List(_)) {
                    Err(Diagnostic::new(
                        ErrCode::E201,
                        fe.span,
                        "list<T>",
                        &t.name(),
                        "foreach source 必须是 list",
                    ))
                } else {
                    Ok(())
                }
            }) {
                if let Err(d) = Result::<(), Diagnostic>::Err(d) {
                    diags.push(d);
                }
            }
            s
        } else {
            scope.clone()
        };
        for b in &n.branches {
            if let Some(w) = &b.when {
                if let Err(d) = check_predicate(w, &sub_scope) {
                    diags.push(d);
                }
            }
        }
        // 注册节点输出 schema（供下游 @ref）
        scope.nodes.insert(
            n.id.clone(),
            NodeSchema { fields: n.contract.clone() },
        );
    }

    if diags.is_empty() {
        Ok(diags)
    } else {
        Err(diags)
    }
}

fn check_node_branches(n: &Node, scope: &mut Scope, diags: &mut Vec<Diagnostic>) {
    for b in &n.branches {
        if let Some(w) = &b.when {
            if let Err(d) = check_predicate(w, scope) {
                diags.push(d);
            }
        }
    }
    scope.nodes.insert(n.id.clone(), NodeSchema { fields: n.contract.clone() });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::types::Schema as S;

    fn scope_with_fields() -> Scope {
        let fields = S::Record(BTreeMap::from([
            ("score".to_string(), S::Int),
            ("path".to_string(), S::Str),
        ]));
        Scope {
            topic: true,
            item: None,
            nodes: BTreeMap::from([("judge".to_string(), NodeSchema { fields: Some(fields) })]),
        }
    }

    fn expr_ok(e: &Expr) -> Ty {
        infer(e, &scope_with_fields()).unwrap()
    }
    fn expr_err(e: &Expr) -> ErrCode {
        infer(e, &scope_with_fields()).unwrap_err().code
    }

    fn s(v: &str) -> Expr {
        Expr::Lit(Lit::Str(v.into()), Span::new(0, 1))
    }
    fn i(v: i64) -> Expr {
        Expr::Lit(Lit::Int(v), Span::new(0, 1))
    }
    fn b(v: bool) -> Expr {
        Expr::Lit(Lit::Bool(v), Span::new(0, 1))
    }
    fn path(p: &str) -> Expr {
        let parts: Vec<&str> = p.split('.').collect();
        Expr::Path(
            RefPath { id: parts[0].to_string(), fields: parts[1..].iter().map(|x| x.to_string()).collect() },
            Span::new(0, 2),
        )
    }
    fn cmp(op: CmpOp, l: Expr, r: Expr) -> Expr {
        let sp = Span::merge(l.span(), r.span());
        Expr::Cmp { op, lhs: Box::new(l), rhs: Box::new(r), span: sp }
    }
    fn and_(l: Expr, r: Expr) -> Expr {
        let sp = Span::merge(l.span(), r.span());
        Expr::And(Box::new(l), Box::new(r), sp)
    }

    #[test]
    fn types_precise_no_coercion() {
        // 精确定型
        assert_eq!(expr_ok(&i(1)), Ty::Int);
        assert_eq!(expr_ok(&s("x")), Ty::Str);
        // 禁隐式转换："1" == 1 → E201（审稿 B：双轨变量伪装成功的死穴）
        assert_eq!(expr_err(&cmp(CmpOp::Eq, s("1"), i(1))), ErrCode::E201);
        // 同型比较 OK → bool
        assert_eq!(expr_ok(&cmp(CmpOp::Eq, i(1), i(2))), Ty::PredBool);
        // 序比较仅 str/int/float：bool < bool → E207
        assert_eq!(expr_err(&cmp(CmpOp::Lt, b(true), b(false))), ErrCode::E207);
    }

    #[test]
    fn unresolved_vars_hard_error() {
        // $topic 存在
        let ok = Expr::Var(VarKind::Topic, Span::new(0, 6));
        assert_eq!(expr_ok(&ok), Ty::Str);
        // $item 越层 → E205（fail-closed，绝不原样保留）
        let item = Expr::Var(VarKind::Item, Span::new(0, 5));
        assert_eq!(expr_err(&item), ErrCode::E205);
        // 无 topic 管线用 $topic → E202
        let mut sc = scope_with_fields();
        sc.topic = false;
        let e = infer(&ok, &sc).unwrap_err();
        assert_eq!(e.code, ErrCode::E202);
        assert!(e.message.contains("绝不静默保留"));
        assert!(e.fix.is_some());
    }

    #[test]
    fn ref_strict_resolution() {
        // 封闭 schema 命中
        assert_eq!(expr_ok(&path("judge.fields.score")), Ty::Int);
        assert_eq!(expr_ok(&path("judge.ok")), Ty::Bool);
        assert_eq!(expr_ok(&path("judge.status")), Ty::Str);
        // 拼错字段 → E203 硬错（stauts）
        assert_eq!(expr_err(&path("judge.stauts")), ErrCode::E203);
        // 未知节点 → E202
        assert_eq!(expr_err(&path("nothere.ok")), ErrCode::E202);
        // 裸 @id → E203
        assert_eq!(expr_err(&Expr::Path(RefPath { id: "judge".into(), fields: vec![] }, Span::new(0, 6))), ErrCode::E203);
    }

    #[test]
    fn builtin_calls_closed() {
        use crate::kernel::ast::Expr::Call;
        let mk = |name: &str, args: Vec<Expr>| Expr::Call {
            name: name.to_string(),
            args,
            span: Span::new(0, 8),
        };
        // len/1 list → int
        let list = Expr::List(vec![s("a"), s("b")], Span::new(0, 5));
        assert_eq!(expr_ok(&mk("len", vec![list.clone()])), Ty::Int);
        // len int → E207
        assert_eq!(expr_err(&mk("len", vec![i(3)])), ErrCode::E207);
        // contains(list,str) 同型 → bool
        assert_eq!(expr_ok(&mk("contains", vec![list.clone(), s("a")])), Ty::Bool);
        // contains 不同型 → E207
        assert_eq!(expr_err(&mk("contains", vec![list, i(1)])), ErrCode::E207);
        // 未知函数 → E202 fail-closed
        assert_eq!(expr_err(&mk("freestyle", vec![i(1)])), ErrCode::E202);
    }

    #[test]
    fn predicate_must_be_bool() {
        // 谓词 int → E207
        let sc = scope_with_fields();
        let e = check_predicate(&i(1), &scope_with_fields()).unwrap_err();
        assert_eq!(e.code, ErrCode::E207);
        // 合法谓词
        let good = and_(
            cmp(CmpOp::Ge, path("judge.fields.score"), i(80)),
            cmp(CmpOp::Eq, path("judge.status"), s("Success")),
        );
        assert!(check_predicate(&good, &scope_with_fields()).is_ok());
    }

    #[test]
    fn const_fold_eval() {
        let ctx = EvalCtx { topic: Some("轨道众生"), item: None, runtime: BTreeMap::new() };
        // $hash("x") 确定性
        let h1 = eval_const(
            &Expr::Var(VarKind::Hash(Box::new(s("x"))), Span::new(0, 10)),
            &ctx,
        ).unwrap();
        let h2 = eval_const(
            &Expr::Var(VarKind::Hash(Box::new(s("x"))), Span::new(0, 10)),
            &ctx,
        ).unwrap();
        assert_eq!(h1, h2);
        if let Value::Str(hex) = h1 {
            assert_eq!(hex.len(), 64);
            assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        } else {
            panic!("hash 必须 str");
        }
        // 纯比较折叠
        let c = eval_const(&cmp(CmpOp::Lt, i(1), i(2)), &ctx).unwrap();
        assert_eq!(c, Value::Bool(true));
        // Path → None（运行期）
        assert!(eval_const(&path("judge.ok"), &ctx).is_none());
    }
}
