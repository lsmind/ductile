//! Ductile v0.24 AST + 表达式文法（规范 §3 + 补丁① EBNF）— 行动③ D3-4。
//!
//! 与存量 core::ast 并存（sidecar 策略）：本 AST 只服务 v0.24 前端，
//! 全部节点带 Span，供 Diagnostic 定位与 AST codemod（migrate）复用。
//!
//! 文法（补丁①）：
//!   Var    ::= "$topic" | "$item" | "$hash" "(" Expr ")"
//!   Path   ::= "@" Id {"." Id}
//!   Atom   ::= Lit | ListLit | Var | Path | Call | "(" Expr ")"
//!   Cmp    ::= Atom [("=="|"!="|"<"|"<="|">"|">=") Atom]
//!   Not    ::= "!" Not | Cmp
//!   And    ::= Not {"&&" Not}
//!   Expr   ::= And {"||" And}
//! 类型规则：字面量精确定型、禁隐式转换；==/!= 同型；序比较仅 str/int/float；
//! 谓词必须 bool（E207）；未解析=编译期硬错（E202），绝不原样保留。

use crate::kernel::types::{ErrCode, RefPath, Schema, Value};
use std::collections::BTreeMap;
use std::fmt;

// ── Span：诊断与 codemod 的定位基础 ──────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: u32, end: u32) -> Self {
        Span { start, end }
    }
    pub fn merge(a: Span, b: Span) -> Self {
        Span { start: a.start.min(b.start), end: a.end.max(b.end) }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

// ── 表达式 AST ───────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl Lit {
    pub fn type_name(&self) -> &'static str {
        match self {
            Lit::Str(_) => "str",
            Lit::Int(_) => "int",
            Lit::Float(_) => "float",
            Lit::Bool(_) => "bool",
        }
    }
    pub fn to_value(&self) -> Value {
        match self {
            Lit::Str(s) => Value::Str(s.clone()),
            Lit::Int(i) => Value::Int(*i),
            Lit::Float(x) => Value::Float(*x),
            Lit::Bool(b) => Value::Bool(*b),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum VarKind {
    /// `$topic:str` —— 根输入，全图可见。
    Topic,
    /// `$item:T` —— 仅 foreach 词法子树内可见（E205 越层硬错）。
    Item,
    /// `$hash(Expr)` —— SHA-256 规范编码 → 64 位小写十六进制 str。
    Hash(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit(Lit, Span),
    List(Vec<Expr>, Span),
    Var(VarKind, Span),
    Path(RefPath, Span),
    Call { name: String, args: Vec<Expr>, span: Span },
    Cmp { op: CmpOp, lhs: Box<Expr>, rhs: Box<Expr>, span: Span },
    Not(Box<Expr>, Span),
    And(Box<Expr>, Box<Expr>, Span),
    Or(Box<Expr>, Box<Expr>, Span),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub fn symbol(&self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
    /// 序比较仅 str/int/float（补丁①类型规则）。
    pub fn is_ordering(&self) -> bool {
        matches!(self, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge)
    }
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Lit(_, s)
            | Expr::List(_, s)
            | Expr::Var(_, s)
            | Expr::Path(_, s)
            | Expr::Not(_, s)
            | Expr::And(_, _, s)
            | Expr::Or(_, _, s) => *s,
            Expr::Call { span, .. } | Expr::Cmp { span, .. } => *span,
        }
    }
}

// ── v0.24 节点 IR（切片子集；完整动词签名表逐步补齐）──────

/// 动词调用：名称 + 具名参数（表达式）。类型检查器按 verb 表验
/// 参数与输出 schema 的绑定（E201/E206）。
#[derive(Debug, Clone, PartialEq)]
pub struct VerbCall {
    pub verb: String,
    pub args: BTreeMap<String, Expr>,
    pub span: Span,
}

/// plan 分支：一个 impl 候选。分支间输出 schema 必须一致（E206）。
#[derive(Debug, Clone, PartialEq)]
pub struct PlanBranch {
    pub impl_name: String,
    pub call: VerbCall,
    /// 本分支谓词（None=恒真）。
    pub when: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ForEachSpec {
    /// 静态 list 来源（表达式）。
    pub source: Expr,
    pub body: Vec<Node>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: String,
    pub span: Span,
    pub branches: Vec<PlanBranch>,
    pub foreach: Option<ForEachSpec>,
    /// 输出契约（封闭 schema；分支不一致 → E206）。
    pub contract: Option<Schema>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pipeline24 {
    pub nodes: Vec<Node>,
    /// 主题存在标志：$topic 合法性的根。
    pub has_topic: bool,
}
