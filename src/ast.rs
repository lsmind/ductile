//! Ductile AST — type definitions for the pipeline DSL.
//!
//! Mirrors the Haskell AST.hs but uses Rust idioms (enums, Box, Vec).

use std::collections::BTreeMap;

// ── §1 Execution Level ──
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    L0, L1, L2, L3, L4, L5,
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Level {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "L0" => Some(Level::L0),
            "L1" => Some(Level::L1),
            "L2" => Some(Level::L2),
            "L3" => Some(Level::L3),
            "L4" => Some(Level::L4),
            "L5" => Some(Level::L5),
            _ => None,
        }
    }
}

// ── §10 Scope ──
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    Search, File, Image, Convert, Deliver,
    Memory, Terminal, GovRules, Analysis,
    GPU, Video, Audio, LLM,
}

impl Scope {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "Search" => Some(Scope::Search),
            "File" => Some(Scope::File),
            "Image" => Some(Scope::Image),
            "Convert" => Some(Scope::Convert),
            "Deliver" => Some(Scope::Deliver),
            "Memory" => Some(Scope::Memory),
            "Terminal" => Some(Scope::Terminal),
            "GovRules" => Some(Scope::GovRules),
            "Analysis" => Some(Scope::Analysis),
            "GPU" => Some(Scope::GPU),
            "Video" => Some(Scope::Video),
            "Audio" => Some(Scope::Audio),
            "LLM" => Some(Scope::LLM),
            _ => None,
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

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
        Cost { latency: 0, risk: 0.0, tokens: 0, money: 0.0 }
    }
}

impl Cost {
    pub fn is_negative(&self) -> bool {
        self.latency < 0 || self.risk < 0.0 || self.tokens < 0 || self.money < 0.0
    }
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
    pub level: Level,
    pub cost: Cost,
    pub enabled: bool,
    pub when: Option<String>,      // raw when-condition text
    pub refs: Vec<String>,         // body中引用的其他proc名
    pub body_text: String,         // 原始body文本（供executor解析和执行）
    pub stub: bool,
    pub retry: usize,              // v3.0: retry count
    pub ensure: Vec<Check>,        // v3.0: inline checks
}

#[derive(Debug, Clone)]
pub struct Check {
    pub cond: String,   // raw condition text (e.g. "result => has_results")
    pub msg: String,
}

#[derive(Debug, Clone)]
pub struct Proc {
    pub name: String,
    pub level: Level,
    pub scope: Option<Scope>,
    pub plan: Vec<Impl>,
    pub checks: Vec<Check>,
    pub deliver: bool,
    pub foreach: Option<String>,   // source proc name
    pub foreach_var: String,
    pub pick_by: String,           // pick strategy
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub name: String,
    pub effects: Vec<Scope>,
    pub min_level: Level,
    pub procs: Vec<Proc>,
    pub weights: Weights,
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
    Failed(String),
}

// ── RecentRuns: sliding window ──
#[derive(Debug, Clone, Default)]
pub struct RecentRuns {
    pub window: usize,
    pub fails: usize,
    pub consec_fail: usize,
}

pub const WINDOW_SIZE: usize = 20;

#[cfg(test)]
mod tests {
    use super::*;

    // ── Level ──
    #[test]
    fn level_from_str_valid() {
        assert_eq!(Level::from_str("L0"), Some(Level::L0));
        assert_eq!(Level::from_str("L3"), Some(Level::L3));
        assert_eq!(Level::from_str("L5"), Some(Level::L5));
    }

    #[test]
    fn level_from_str_invalid() {
        assert_eq!(Level::from_str("L6"), None);
        assert_eq!(Level::from_str("X"), None);
        assert_eq!(Level::from_str(""), None);
    }

    #[test]
    fn level_ordering() {
        assert!(Level::L0 < Level::L3);
        assert!(Level::L3 < Level::L5);
    }

    #[test]
    fn level_display() {
        assert_eq!(Level::L2.to_string(), "L2");
    }

    // ── Scope ──
    #[test]
    fn scope_from_str_valid() {
        assert_eq!(Scope::from_str("Search"), Some(Scope::Search));
        assert_eq!(Scope::from_str("File"), Some(Scope::File));
        assert_eq!(Scope::from_str("LLM"), Some(Scope::LLM));
        assert_eq!(Scope::from_str("GPU"), Some(Scope::GPU));
    }

    #[test]
    fn scope_from_str_invalid() {
        assert_eq!(Scope::from_str("search"), None); // case-sensitive
        assert_eq!(Scope::from_str("Network"), None);
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
        assert!(Cost { latency: -1, ..Default::default() }.is_negative());
        assert!(Cost { risk: -0.1, ..Default::default() }.is_negative());
        assert!(Cost { tokens: -1, ..Default::default() }.is_negative());
        assert!(Cost { money: -0.01, ..Default::default() }.is_negative());
    }

    #[test]
    fn cost_is_negative_false() {
        assert!(!Cost::default().is_negative());
        assert!(!Cost { latency: 100, risk: 0.5, tokens: 200, money: 1.0 }.is_negative());
    }

    // ── Weights ──
    #[test]
    fn weights_cost_total_basic() {
        let w = Weights::default();
        let c = Cost { latency: 1000, risk: 0.5, tokens: 10000, money: 2.0 };
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
}
