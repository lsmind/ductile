//! Type checker — validates pipeline structure.

use crate::ast::*;

#[derive(Debug, Clone, PartialEq)]
pub enum TypeError {
    LevelExceeds(String, String, Level, Level),
    EffectNotDeclared(String, Scope),
    NoEnabledImpl(String),
    CostNegative(String, String),
    EmptyPlan(String),
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeError::LevelExceeds(proc, impl_name, l1, l2) => {
                write!(f, "[{}] impl '{}' level {} exceeds min_level {}", proc, impl_name, l1, l2)
            }
            TypeError::EffectNotDeclared(proc, scope) => {
                write!(f, "[{}] scope {} not declared in effects", proc, scope)
            }
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
    pl.procs.iter().flat_map(|proc| check_proc(&pl.effects, pl.min_level, proc)).collect()
}

fn check_proc(effects: &[Scope], min_lvl: Level, proc: &Proc) -> Vec<TypeError> {
    let mut errs = Vec::new();

    // Scope check
    if let Some(s) = proc.scope {
        if !effects.contains(&s) {
            errs.push(TypeError::EffectNotDeclared(proc.name.clone(), s));
        }
    }

    // Level check (deliver/foreach exempt)
    let is_deliver = proc.deliver;
    let is_foreach = proc.foreach.is_some();
    if !is_deliver && !is_foreach && proc.level > min_lvl {
        errs.push(TypeError::LevelExceeds(proc.name.clone(), "(proc)".into(), proc.level, min_lvl));
    }

    // Plan check
    if is_deliver {
        // deliver proc can have empty plan
    } else if proc.plan.is_empty() {
        errs.push(TypeError::EmptyPlan(proc.name.clone()));
    } else {
        for impl_ in &proc.plan {
            // Level check for impl
            if impl_.level > min_lvl {
                errs.push(TypeError::LevelExceeds(
                    proc.name.clone(), impl_.name.clone(), impl_.level, min_lvl));
            }
            // Cost check
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

    fn mk_pipeline(min_level: Level, effects: Vec<Scope>, procs: Vec<Proc>) -> Pipeline {
        Pipeline {
            name: "test".into(),
            effects,
            min_level,
            procs,
            weights: Weights::default(),
        }
    }

    fn mk_proc(name: &str, level: Level, scope: Option<Scope>, plan: Vec<Impl>) -> Proc {
        Proc {
            name: name.into(),
            level,
            scope,
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
            level: Level::L0,
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
        let pl = mk_pipeline(
            Level::L3,
            vec![Scope::File],
            vec![mk_proc("p", Level::L1, Some(Scope::File),
                vec![mk_impl("a", Cost::default())])],
        );
        assert!(check_pipeline(&pl).is_empty());
    }

    // ── Level exceeds ──
    #[test]
    fn level_exceeds_min() {
        let pl = mk_pipeline(
            Level::L2,
            vec![Scope::File],
            vec![mk_proc("p", Level::L4, Some(Scope::File),
                vec![mk_impl("a", Cost::default())])],
        );
        let errs = check_pipeline(&pl);
        assert!(errs.iter().any(|e| matches!(e, TypeError::LevelExceeds(_, _, _, _))));
    }

    // ── Effect not declared ──
    #[test]
    fn effect_not_declared() {
        let pl = mk_pipeline(
            Level::L3,
            vec![Scope::File],
            vec![mk_proc("p", Level::L0, Some(Scope::Search),
                vec![mk_impl("a", Cost::default())])],
        );
        let errs = check_pipeline(&pl);
        assert!(errs.iter().any(|e| matches!(e, TypeError::EffectNotDeclared(_, _))));
    }

    // ── Negative cost ──
    #[test]
    fn negative_cost_detected() {
        let pl = mk_pipeline(
            Level::L3,
            vec![Scope::File],
            vec![mk_proc("p", Level::L0, Some(Scope::File),
                vec![mk_impl("bad", Cost { latency: -1, ..Default::default() })])],
        );
        let errs = check_pipeline(&pl);
        assert!(errs.iter().any(|e| matches!(e, TypeError::CostNegative(_, _))));
    }

    // ── Empty plan ──
    #[test]
    fn empty_plan_errors() {
        let pl = mk_pipeline(
            Level::L3,
            vec![Scope::File],
            vec![mk_proc("p", Level::L0, Some(Scope::File), vec![])],
        );
        let errs = check_pipeline(&pl);
        assert!(errs.iter().any(|e| matches!(e, TypeError::EmptyPlan(_))));
    }

    // ── Deliver proc exempt from empty plan ──
    #[test]
    fn deliver_exempt_from_empty_plan() {
        let mut p = mk_proc("out", Level::L0, None, vec![]);
        p.deliver = true;
        let pl = mk_pipeline(Level::L3, vec![Scope::Deliver], vec![p]);
        let errs = check_pipeline(&pl);
        assert!(errs.is_empty());
    }

    // ── Foreach proc exempt from level check ──
    #[test]
    fn foreach_exempt_from_level() {
        let mut p = mk_proc("fan", Level::L5, None,
            vec![mk_impl("x", Cost::default())]);
        p.foreach = Some("src".into());
        let pl = mk_pipeline(Level::L1, vec![], vec![p]);
        let errs = check_pipeline(&pl);
        // foreach is exempt from level but not from empty plan
        assert!(!errs.iter().any(|e| matches!(e, TypeError::LevelExceeds(_, _, _, _))));
    }

    // ── TypeError display ──
    #[test]
    fn type_error_display() {
        let e = TypeError::EmptyPlan("proc1".into());
        assert!(e.to_string().contains("proc1"));
        assert!(e.to_string().contains("empty plan"));
    }
}
