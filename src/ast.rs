//! Ductile AST — type definitions for the pipeline DSL.
//!
//! v0.4: Level/Scope replaced by tag system. Tags live only on leaf impls.
//! Pipeline effects auto-computed as union of all impl tags.

use std::collections::{BTreeMap, BTreeSet};

// ── §1 Tag (replaces Level + Scope) ──
// Tags are free-form strings. By convention:
//   #network, #file, #gpu, #llm, #search, #terminal, #memory, #deliver ...
// Pipeline effects = union of all impl tags (auto-computed, not declared).

// ── §9 Cost ──
#[derive(Debug, Clone, PartialEq)]
pub struct Cost {
    pub latency: i64,
    pub risk: f64,
    pub tokens: i64,
    pub money: f64,
}

impl Default for Cost {
    fn default() -> Self {
        Cost {
            latency: 0,
            risk: 0.0,
            tokens: 0,
            money: 0.0,
        }
    }
}

impl Cost {
    pub fn is_negative(&self) -> bool {
        self.latency < 0 || self.risk < 0.0 || self.tokens < 0 || self.money < 0.0
    }
}

// ── v0.11 Policy：评价与流程分离 ──
/// cost 值来源：直接数值，或测试方法（链接测试脚本，运行时实测）。
#[derive(Debug, Clone, PartialEq)]
pub enum CostValue {
    Direct(f64),
    /// 测试命令（bash -c 执行；可含 {topic} 占位符），stdout 中第一个浮点数即值。
    Measure(String),
}

/// 单个 `proc.impl` 的 cost 规格：字段 → 来源。未声明的字段 = 0。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostSpec {
    pub latency: Option<CostValue>,
    pub risk: Option<CostValue>,
    pub tokens: Option<CostValue>,
    pub money: Option<CostValue>,
}

/// 评价策略（.eval 文件）。.pipeline 只描述流程；权重与 cost 来源全在这里，
/// 运行时 `ductile run x.pipeline "topic" --policy p.eval` 挂载。
/// 原则：不能又当运动员又当裁判——流程文件不自带判罚。
#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub weights: Weights,
    /// key = "proc.impl"
    pub costs: BTreeMap<String, CostSpec>,
    /// v0.11 恒 fail-closed（未知函数/条件 = 硬失败）；字段保留为格式前瞻。
    pub fail_closed: bool,
}

// ── Weights ──
#[derive(Debug, Clone)]
pub struct Weights {
    pub tokens: f64,
    pub latency: f64,
    pub risk: f64,
    pub money: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Weights {
            tokens: 0.0001,
            latency: 0.001,
            risk: 10.0,
            money: 1.0,
        }
    }
}

impl Weights {
    pub fn cost_total(&self, c: &Cost) -> f64 {
        self.tokens * c.tokens as f64
            + self.latency * c.latency as f64
            + self.risk * c.risk
            + self.money * c.money
    }
}

// ── §3 Impl / Proc / Pipeline ──
#[derive(Debug, Clone)]
pub struct Impl {
    pub name: String,
    pub description: String, // v0.4.1: human-readable description (.desc("..."))
    pub tags: BTreeSet<String>, // v0.4: replaces level + scope
    pub cost: Cost,
    pub enabled: bool,
    pub when: Option<String>, // raw when-condition text
    pub refs: Vec<String>,    // body中引用的其他proc名
    pub body_text: String,    // 原始body文本（供executor解析和执行）
    pub stub: bool,
    pub retry: usize,       // retry count
    pub ensure: Vec<Check>, // inline checks
}

#[derive(Debug, Clone)]
pub struct Check {
    pub cond: String, // raw condition text (e.g. "result => has_results")
    pub msg: String,
}

/// v0.15 节点契约卡（cognition spec §7 P0）：执行后校验的确定性信号源。
/// outputs — L1 形式层：##DSL_RESULT 必须携带的字段（存在性）。
/// invariants — L2 语义层：跨字段谓词（when.rs 求值器，`@self.field` 自引用）。
///   违例 → Err 带 "contract violation:" 前缀 → errflow Contract 类 → Exit。
#[derive(Debug, Clone, Default)]
pub struct Contract {
    pub outputs: Vec<String>,
    pub invariants: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Proc {
    pub name: String,
    pub description: String, // v0.4.1: human-readable description (.desc("...")
    pub plan: Vec<Impl>,
    pub checks: Vec<Check>,
    pub contract: Contract, // v0.15: .contract(outputs=..., invariants=...)
    pub deliver: bool,
    pub foreach: Option<String>, // source proc name
    /// v0.14b：deliver 引用（`.deliver(@report)` 的 @report）——关键节点集的根。
    /// 旧版只置 is_deliver 丢弃参数，关键性判定无从谈起（实测 bug）。
    pub deliver_refs: Vec<String>,
    pub foreach_var: String,
    pub pick_by: String, // pick strategy
}

#[derive(Debug, Clone, Default)]
pub struct Pipeline {
    pub name: String,
    pub description: String, // v0.4.1: optional Pipeline("name", "desc")
    pub procs: Vec<Proc>,
    pub weights: Weights,
    // v0.16 管线级执行环境（第三刀）：Pipeline(..., cwd="...", env=["K=V", ...])
    // cwd 由 executor 在执行前 chdir（相对路径相对 pipeline 文件所在目录）；
    // env 注入所有子进程（PATH/HOME/USER 身份变量永不覆盖）。
    // 默认（None/空）= v0.15 行为：继承调用方 CWD，不注入。
    pub cwd: Option<String>,
    pub env: Vec<String>,
}

impl Pipeline {
    /// Auto-compute effects/tags as the union of all leaf impl tags.
    pub fn computed_tags(&self) -> BTreeSet<String> {
        let mut tags = BTreeSet::new();
        for proc in &self.procs {
            for impl_ in &proc.plan {
                for t in &impl_.tags {
                    tags.insert(t.clone());
                }
            }
        }
        tags
    }
}

/// Compute tags for a single proc: union of all its impl tags.
pub fn proc_tags(proc: &Proc) -> BTreeSet<String> {
    let mut tags = BTreeSet::new();
    for impl_ in &proc.plan {
        for t in &impl_.tags {
            tags.insert(t.clone());
        }
    }
    tags
}

// ── §11 Status / Record ──
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Fail,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Ok => write!(f, "Ok"),
            Status::Fail => write!(f, "Fail"),
        }
    }
}

// ── Runtime Values ──
#[derive(Debug, Clone)]
pub enum Value {
    Text(String),
    File(String),
    Null,
}

impl Value {
    pub fn as_text(&self) -> &str {
        match self {
            Value::Text(t) => t,
            Value::File(f) => f,
            Value::Null => "",
        }
    }

    pub fn len(&self) -> usize {
        self.as_text().len()
    }
}

// ── ExecResult ──
#[derive(Debug)]
pub enum ExecResult {
    Success(BTreeMap<String, Value>),
    /// v0.14：失败携带部分结果——失败是数据。partial 含全部已完成 proc 的值
    /// （Left 错误值为 §§FIELDS§§err=1§§ 编码文本），debug/agent 不再丢失现场。
    Failed {
        error: String,
        partial: BTreeMap<String, Value>,
    },
}

// ── RecentRuns: sliding window ──
#[derive(Debug, Clone, Default)]
pub struct RecentRuns {
    pub window: usize,
    pub fails: usize,
    pub consec_fail: usize,
    /// RD 统计：样本数、rate_tokens 累计、est_loss 累计
    pub n: usize,
    pub sum_tokens: i64,
    pub sum_loss: f64,
}

pub const WINDOW_SIZE: usize = 20;

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_tags(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── Cost ──
    #[test]
    fn cost_default_is_zero() {
        let c = Cost::default();
        assert_eq!(c.latency, 0);
        assert_eq!(c.risk, 0.0);
        assert_eq!(c.tokens, 0);
        assert_eq!(c.money, 0.0);
    }

    #[test]
    fn cost_is_negative_true() {
        assert!(Cost {
            latency: -1,
            ..Default::default()
        }
        .is_negative());
        assert!(Cost {
            risk: -0.1,
            ..Default::default()
        }
        .is_negative());
        assert!(Cost {
            tokens: -1,
            ..Default::default()
        }
        .is_negative());
        assert!(Cost {
            money: -0.01,
            ..Default::default()
        }
        .is_negative());
    }

    #[test]
    fn cost_is_negative_false() {
        assert!(!Cost::default().is_negative());
        assert!(!Cost {
            latency: 100,
            risk: 0.5,
            tokens: 200,
            money: 1.0
        }
        .is_negative());
    }

    // ── Weights ──
    #[test]
    fn weights_cost_total_basic() {
        let w = Weights::default();
        let c = Cost {
            latency: 1000,
            risk: 0.5,
            tokens: 10000,
            money: 2.0,
        };
        // 0.001*1000 + 10.0*0.5 + 0.0001*10000 + 1.0*2.0 = 1 + 5 + 1 + 2 = 9
        let total = w.cost_total(&c);
        assert!((total - 9.0).abs() < 0.001);
    }

    #[test]
    fn weights_cost_total_zero() {
        let w = Weights::default();
        let total = w.cost_total(&Cost::default());
        assert!(total.abs() < 0.001);
    }

    // ── Value ──
    #[test]
    fn value_as_text() {
        assert_eq!(Value::Text("hello".into()).as_text(), "hello");
        assert_eq!(Value::File("/tmp/x.txt".into()).as_text(), "/tmp/x.txt");
        assert_eq!(Value::Null.as_text(), "");
    }

    #[test]
    fn value_len() {
        assert_eq!(Value::Text("hello".into()).len(), 5);
        assert_eq!(Value::Null.len(), 0);
    }

    // ── Status ──
    #[test]
    fn status_display() {
        assert_eq!(Status::Ok.to_string(), "Ok");
        assert_eq!(Status::Fail.to_string(), "Fail");
    }

    // ── RecentRuns ──
    #[test]
    fn recent_runs_default() {
        let rr = RecentRuns::default();
        assert_eq!(rr.window, 0);
        assert_eq!(rr.fails, 0);
        assert_eq!(rr.consec_fail, 0);
    }

    // ── computed_tags ──
    #[test]
    fn computed_tags_union_of_impl_tags() {
        let pl = Pipeline {
            name: "test".into(),
            description: String::new(),
            procs: vec![
                Proc {
                    name: "a".into(),
                    description: String::new(),
                    plan: vec![Impl {
                        name: "x".into(),
                        tags: mk_tags(&["search", "network"]),
                        ..default_impl()
                    }],
                    checks: vec![],
                contract: Default::default(),
                    deliver: false,
                    deliver_refs: vec![],
                    foreach: None,
                    foreach_var: String::new(),
                    pick_by: "cost".into(),
                },
                Proc {
                    name: "b".into(),
                    description: String::new(),
                    plan: vec![Impl {
                        name: "y".into(),
                        tags: mk_tags(&["file", "network"]),
                        ..default_impl()
                    }],
                    checks: vec![],
                contract: Default::default(),
                    deliver: false,
                    deliver_refs: vec![],
                    foreach: None,
                    foreach_var: String::new(),
                    pick_by: "cost".into(),
                },
            ],
            weights: Weights::default(),
            cwd: None,
            env: vec![],
        };
        let tags = pl.computed_tags();
        assert_eq!(tags.len(), 3);
        assert!(tags.contains("search"));
        assert!(tags.contains("file"));
        assert!(tags.contains("network"));
    }

    #[test]
    fn computed_tags_empty_when_no_tags() {
        let pl = Pipeline {
            name: "bare".into(),
            description: String::new(),
            procs: vec![Proc {
                name: "a".into(),
                description: String::new(),
                plan: vec![Impl {
                    name: "x".into(),
                    ..default_impl()
                }],
                checks: vec![],
                contract: Default::default(),
                deliver: false,
                deliver_refs: vec![],
                foreach: None,
                foreach_var: String::new(),
                pick_by: "cost".into(),
            }],
            weights: Weights::default(),
            cwd: None,
            env: vec![],
        };
        assert!(pl.computed_tags().is_empty());
    }

    fn default_impl() -> Impl {
        Impl {
            name: String::new(),
            description: String::new(),
            tags: BTreeSet::new(),
            cost: Cost::default(),
            enabled: true,
            when: None,
            refs: vec![],
            body_text: String::new(),
            stub: false,
            retry: 0,
            ensure: vec![],
        }
    }
}
