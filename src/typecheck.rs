//! Type checker — validates pipeline structure.
//!
//! Checks:
//!   - NoEnabledImpl: proc has at least one enabled impl
//!   - CostNegative: impl has negative cost
//!   - EmptyPlan: non-deliver proc has empty plan
//!   - UnknownFunction: enabled impl body calls a non-registered function (fail-closed)

use crate::ast::*;
use crate::steps::{is_probe_stub, known_functions};
use crate::textargs::detect_func;

#[derive(Debug, Clone, PartialEq)]
pub enum TypeError {
    NoEnabledImpl(String),
    CostNegative(String, String),
    EmptyPlan(String),
    UnknownFunction(String, String, String),
    /// v0.18.4：.when 条件解析失败提前到 check 期（此前 run 才炸——fail-late，
    /// 外部反馈单#1：check 绿灯 + run 崩溃的"链式名解析漏检"同款病根）。
    BadWhenCondition(String, String, String, String),
    /// v0.18.5：script() body 静态校验提前到 check 期（此前 run 才炸——
    /// 外部反馈单#5.1：`.args()`/`.env()`/`.timeout()` 链式后缀被并入
    /// 脚本名（"invalid script name 'x)  .args(k=v'"），两次生产事故同族，
    /// check 绿灯 + run 崩溃的 fail-late）。
    BadScriptBody(String, String, String, String),
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
            TypeError::UnknownFunction(proc, impl_name, func) => {
                write!(
                    f,
                    "[{}] impl '{}' unknown function '{}' — fail-closed",
                    proc, impl_name, func
                )
            }
            TypeError::BadWhenCondition(proc, impl_name, cond, msg) => {
                write!(
                    f,
                    "[{}] impl '{}' bad .when({}) — {} — fail-closed at check",
                    proc, impl_name, cond, msg
                )
            }
            TypeError::BadScriptBody(proc, impl_name, body, msg) => {
                write!(
                    f,
                    "[{}] impl '{}' bad script body '{}' — {} — fail-closed at check (反馈单#5.1: 链式 .args/.env/.timeout 须内联为 script(name, k=v))",
                    proc, impl_name, body, msg
                )
            }
        }
    }
}

pub fn check_pipeline(pl: &Pipeline) -> Vec<TypeError> {
    let known: std::collections::BTreeSet<&str> = known_functions().into_iter().collect();
    pl.procs
        .iter()
        .flat_map(|proc| check_proc(proc, &known))
        .collect()
}

fn check_proc(proc: &Proc, known: &std::collections::BTreeSet<&str>) -> Vec<TypeError> {
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
                errs.push(TypeError::CostNegative(
                    proc.name.clone(),
                    impl_.name.clone(),
                ));
            }
            // v0.18.4：.when 条件静态校验——parse_when 是纯函数，check 期即可
            // 全量解析。裸 @proc（缺 .field）、空算子数等此前 run 才暴露
            // （fail-late），现在 check 期拦截。
            if let Some(cond) = &impl_.when {
                if let Err(msg) = crate::when::parse_when(cond) {
                    errs.push(TypeError::BadWhenCondition(
                        proc.name.clone(),
                        impl_.name.clone(),
                        cond.clone(),
                        msg,
                    ));
                }
            }
            // v0.18.5（反馈单#5.1）：script() body 静态校验——parse_script_body
            // 是纯函数，check 期即可全量解析。链式 .args()/.env()/.timeout()
            // 后缀被并入脚本名、非 k=v 参数等此前 run 才暴露（fail-late，
            // S0BP/S0BQ 两次生产事故同款），现在 check 期拦截。
            let func = detect_func(&impl_.body_text);
            if func == "script" {
                if let Err(msg) = crate::script::parse_script_body(&impl_.body_text) {
                    errs.push(TypeError::BadScriptBody(
                        proc.name.clone(),
                        impl_.name.clone(),
                        impl_.body_text.trim().to_string(),
                        msg,
                    ));
                }
            }
            if impl_.enabled && !impl_.stub {
                let func = detect_func(&impl_.body_text);
                if !func.is_empty() && !is_probe_stub(&func) && !known.contains(func.as_str()) {
                    errs.push(TypeError::UnknownFunction(
                        proc.name.clone(),
                        impl_.name.clone(),
                        func,
                    ));
                }
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
            cwd: None,
            env: vec![],
            description: String::new(),
        }
    }

    fn mk_proc(name: &str, plan: Vec<Impl>) -> Proc {
        Proc {
            name: name.into(),
            plan,
            checks: vec![],
            contract: Default::default(),
            deliver: false,
            needs: vec![],
            constraint_fields: vec![],
            deliver_refs: vec![],
            foreach: None,
            foreach_var: String::new(),
            pick_by: "cost".into(),
            description: String::new(),
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
            body_text: "run(\"true\")".into(),
            stub: false,
            retry: 0,
            ensure: vec![],
            description: String::new(),
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
            vec![mk_impl(
                "bad",
                Cost {
                    latency: -1,
                    ..Default::default()
                },
            )],
        )]);
        let errs = check_pipeline(&pl);
        assert!(errs
            .iter()
            .any(|e| matches!(e, TypeError::CostNegative(_, _))));
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
        assert!(errs
            .iter()
            .any(|e| matches!(e, TypeError::NoEnabledImpl(_))));
    }

    // ── Unknown function ──
    #[test]
    fn unknown_function_detected() {
        let mut i = mk_impl("bad", Cost::default());
        i.body_text = "not_a_real_fn(x=1)".into();
        let pl = mk_pipeline(vec![mk_proc("p", vec![i])]);
        let errs = check_pipeline(&pl);
        assert!(
            errs.iter()
                .any(|e| matches!(e, TypeError::UnknownFunction(_, _, _))),
            "{:?}",
            errs
        );
    }

    // ── v0.18.4 .when 静态校验（反馈单#1：fail-late → fail-fast）──
    #[test]
    fn bare_when_ref_caught_at_check() {
        // .when(@arm) 裸引用（缺 .field）——此前 run 期才炸（All paths failed），现在 check 期拦截
        let mut i = mk_impl("guarded", Cost::default());
        i.when = Some("@arm".into());
        let pl = mk_pipeline(vec![mk_proc("probe", vec![i])]);
        let errs = check_pipeline(&pl);
        assert!(
            errs.iter()
                .any(|e| matches!(e, TypeError::BadWhenCondition(..))),
            "bare @ref must fail check: {:?}",
            errs
        );
    }

    #[test]
    fn valid_when_condition_passes_check() {
        let mut i = mk_impl("guarded", Cost::default());
        i.when = Some("@gate.score >= 0.75".into());
        let pl = mk_pipeline(vec![mk_proc("p", vec![i])]);
        let errs = check_pipeline(&pl);
        assert!(!errs
            .iter()
            .any(|e| matches!(e, TypeError::BadWhenCondition(..))));
    }

    // ── TypeError display ──
    #[test]
    fn type_error_display() {
        let e = TypeError::EmptyPlan("proc1".into());
        assert!(e.to_string().contains("proc1"));
        assert!(e.to_string().contains("empty plan"));
    }
}
