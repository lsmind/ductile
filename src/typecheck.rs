//! Type checker — validates pipeline structure.
//!
//! v0.4: No level or scope checks. Only checks:
//!   - NoEnabledImpl: proc has at least one enabled impl
//!   - CostNegative: impl has negative cost
//!   - EmptyPlan: non-deliver proc has empty plan

use crate::ast::*;

#[derive(Debug, Clone, PartialEq)]
pub enum TypeError {
    NoEnabledImpl(String),
    CostNegative(String, String),
    EmptyPlan(String),
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeError::NoEnabledImpl(proc) => {
                write!(f, "[{}] no enabled impl", proc)
            }
            TypeError::CostNegative(proc, impl_name) => {
                write!(f, "[{}] impl '{}' has negative cost", proc, impl_name)
            }
            TypeError::EmptyPlan(proc) => {
                write!(f, "[{}] empty plan", proc)
            }
        }
    }
}

pub fn check_pipeline(pl: &Pipeline) -> Vec<TypeError> {
    pl.procs.iter().flat_map(|proc| check_proc(proc)).collect()
}

fn check_proc(proc: &Proc) -> Vec<TypeError> {
    let mut errs = Vec::new();

    // Plan check
    if proc.deliver {
        // deliver proc can have empty plan
    } else if proc.plan.is_empty() {
        errs.push(TypeError::EmptyPlan(proc.name.clone()));
    } else {
        let has_enabled = proc.plan.iter().any(|i| i.enabled);
        if !has_enabled {
            errs.push(TypeError::NoEnabledImpl(proc.name.clone()));
        }
        for impl_ in &proc.plan {
            if impl_.cost.is_negative() {
                errs.push(TypeError::CostNegative(proc.name.clone(), impl_.name.clone()));
            }
        }
    }

    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn mk_pipeline(procs: Vec<Proc>) -> Pipeline {
        Pipeline {
            name: "test".into(),
            procs,
            weights: Weights::default(),
        }
    }

    fn mk_proc(name: &str, plan: Vec<Impl>) -> Proc {
        Proc {
            name: name.into(),
            plan,
            checks: vec![],
            deliver: false,
            foreach: None,
            foreach_var: String::new(),
            pick_by: "cost".into(),
        }
    }

    fn mk_impl(name: &str, cost: Cost) -> Impl {
        Impl {
            name: name.into(),
            tags: BTreeSet::new(),
            cost,
            enabled: true,
            when: None,
            refs: vec![],
            body_text: "noop()".into(),
            stub: false,
            retry: 0,
            ensure: vec![],
        }
    }

    // ── Valid pipeline ──
    #[test]
    fn valid_pipeline_no_errors() {
        let pl = mk_pipeline(vec![mk_proc("p", vec![mk_impl("a", Cost::default())])]);
        assert!(check_pipeline(&pl).is_empty());
    }

    // ── Negative cost ──
    #[test]
    fn negative_cost_detected() {
        let pl = mk_pipeline(vec![mk_proc(
            "p",
            vec![mk_impl("bad", Cost { latency: -1, ..Default::default() })],
        )]);
        let errs = check_pipeline(&pl);
        assert!(errs.iter().any(|e| matches!(e, TypeError::CostNegative(_, _))));
    }

    // ── Empty plan ──
    #[test]
    fn empty_plan_errors() {
        let pl = mk_pipeline(vec![mk_proc("p", vec![])]);
        let errs = check_pipeline(&pl);
        assert!(errs.iter().any(|e| matches!(e, TypeError::EmptyPlan(_))));
    }

    // ── Deliver proc exempt from empty plan ──
    #[test]
    fn deliver_exempt_from_empty_plan() {
        let mut p = mk_proc("out", vec![]);
        p.deliver = true;
        let pl = mk_pipeline(vec![p]);
        let errs = check_pipeline(&pl);
        assert!(errs.is_empty());
    }

    // ── No enabled impl ──
    #[test]
    fn no_enabled_impl_errors() {
        let mut i = mk_impl("disabled", Cost::default());
        i.enabled = false;
        let pl = mk_pipeline(vec![mk_proc("p", vec![i])]);
        let errs = check_pipeline(&pl);
        assert!(errs.iter().any(|e| matches!(e, TypeError::NoEnabledImpl(_))));
    }

    // ── TypeError display ──
    #[test]
    fn type_error_display() {
        let e = TypeError::EmptyPlan("proc1".into());
        assert!(e.to_string().contains("proc1"));
        assert!(e.to_string().contains("empty plan"));
    }
}
