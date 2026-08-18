//! Ductile — declarative pipeline DSL engine.
//!
//! Core library: parse, typecheck, execute .pipeline files.
//! Python bindings via pyo3.

pub mod ast;
pub mod db;
pub mod egraph;
pub mod executor;
pub mod learn;
pub mod parser;
pub mod registry;
pub mod typecheck;
pub mod version;

pub mod cli;

pub use ast::*;
pub use egraph::{build_egraph, critical_path, parallel_groups, EGraph};
pub use executor::exec_pipeline;
pub use parser::{parse_pipeline, parse_pipeline_file, ParseError};
pub use typecheck::{check_pipeline, TypeError};

/// Char-boundary-safe truncation: never splits a multi-byte UTF-8 char.
/// Byte-slicing (`&s[..n]`) panics when n lands inside a CJK/emoji char —
/// this bit the daily-planning cron in production (executor retry log).
pub fn trunc_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod trunc_tests {
    use super::trunc_chars;

    #[test]
    fn ascii_unchanged() {
        assert_eq!(trunc_chars("hello world", 5), "hello");
        assert_eq!(trunc_chars("short", 200), "short");
    }

    #[test]
    fn cjk_boundary_backoff() {
        // 中 is 3 bytes; byte 4 splits the 2nd char -> must back off to 3
        assert_eq!(trunc_chars("中文测试", 4), "中");
        assert_eq!(trunc_chars("中文测试", 6), "中文");
        assert_eq!(trunc_chars("中文测试", 7), "中文");
    }

    #[test]
    fn mixed_content() {
        // "a中b" = 1+3+1 bytes; limit 2 -> "a"; limit 4 -> "a中"
        assert_eq!(trunc_chars("a中b", 2), "a");
        assert_eq!(trunc_chars("a中b", 4), "a中");
    }

    #[test]
    fn emoji_4byte() {
        assert_eq!(trunc_chars("a🎉b", 2), "a");
        assert_eq!(trunc_chars("a🎉b", 5), "a🎉");
    }

    #[test]
    fn zero_and_empty() {
        assert_eq!(trunc_chars("", 5), "");
        assert_eq!(trunc_chars("abc", 0), "");
    }
}

// ── pyo3 Python bindings ──

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::collections::BTreeMap;

/// Check a pipeline file: parse + typecheck.
#[pyfunction]
fn check(path: &str) -> PyResult<String> {
    let pl = parse_pipeline_file(path).map_err(|e| PyRuntimeError::new_err(format!("{}", e)))?;
    let errs = check_pipeline(&pl);
    if errs.is_empty() {
        Ok("Type check passed".to_string())
    } else {
        let msg = errs
            .iter()
            .map(|e| format!("  {}", e))
            .collect::<Vec<_>>()
            .join("\n");
        Err(PyRuntimeError::new_err(format!(
            "Type check errors:\n{}",
            msg
        )))
    }
}

/// Run a pipeline file with a topic and optional params.
#[pyfunction]
#[pyo3(signature = (path, topic="", params=None))]
fn run(path: &str, topic: &str, params: Option<BTreeMap<String, String>>) -> PyResult<String> {
    let pl = parse_pipeline_file(path).map_err(|e| PyRuntimeError::new_err(format!("{}", e)))?;
    let errs = check_pipeline(&pl);
    if !errs.is_empty() {
        let msg = errs
            .iter()
            .map(|e| format!("  {}", e))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(PyRuntimeError::new_err(format!(
            "Type check errors:\n{}",
            msg
        )));
    }
    let params = params.unwrap_or_default();
    match exec_pipeline(topic, &params, &pl) {
        ExecResult::Success(results) => {
            let mut out = format!("Pipeline executed successfully ({} procs)\n", results.len());
            for (k, v) in &results {
                let shown = match v {
                    Value::Text(t) => {
                        if t.len() > 200 {
                            format!("{}...", crate::trunc_chars(t, 200))
                        } else {
                            t.clone()
                        }
                    }
                    Value::File(f) => format!("<file: {}>", f),
                    Value::Null => "<null>".into(),
                };
                out.push_str(&format!("  {} => {}\n", k, shown));
            }
            Ok(out)
        }
        ExecResult::Failed(err) => {
            Err(PyRuntimeError::new_err(format!("Pipeline failed: {}", err)))
        }
    }
}

/// Parse a pipeline file and return structure info.
#[pyfunction]
fn parse(path: &str) -> PyResult<String> {
    let pl = parse_pipeline_file(path).map_err(|e| PyRuntimeError::new_err(format!("{}", e)))?;
    let tags = pl.computed_tags();
    let tag_str: Vec<String> = tags.iter().map(|t| format!("#{}", t)).collect();
    let mut out = format!("Pipeline: {}\nTags: {}\n\n", pl.name, tag_str.join(", "));
    for proc in &pl.procs {
        out.push_str(&format!(
            "  Proc: {} ({} impls){}\n",
            proc.name,
            proc.plan.len(),
            if proc.deliver { " [deliver]" } else { "" }
        ));
        for imp in &proc.plan {
            let imp_tags: Vec<String> = imp.tags.iter().map(|t| format!("#{}", t)).collect();
            out.push_str(&format!(
                "    Impl: {}{} cost(latency={}, risk={}, tokens={}, money={})\n",
                imp.name,
                if !imp_tags.is_empty() {
                    format!(" [{}]", imp_tags.join(", "))
                } else {
                    String::new()
                },
                imp.cost.latency,
                imp.cost.risk,
                imp.cost.tokens,
                imp.cost.money
            ));
        }
    }
    Ok(out)
}

/// Show e-graph structure: parallel groups and critical path.
#[pyfunction]
fn graph(path: &str) -> PyResult<String> {
    let pl = parse_pipeline_file(path).map_err(|e| PyRuntimeError::new_err(format!("{}", e)))?;
    let eg = build_egraph(&pl);
    let groups = parallel_groups(&eg);
    let cp = critical_path(&eg);
    let mut out = format!(
        "E-Graph:\n  Nodes: {}\n  Edges: {}\n\nParallel groups:\n",
        pl.procs.len(),
        eg.edges.len()
    );
    for g in &groups {
        out.push_str(&format!("  {:?}\n", g));
    }
    out.push_str(&format!("\nCritical path: {:?}\n", cp));
    Ok(out)
}

/// CLI entry point for `ductile` console script.
#[pyfunction]
fn cli_main(py: Python) -> PyResult<i32> {
    let sys = py.import("sys")?;
    let args: Vec<String> = sys.getattr("argv")?.extract()?;
    crate::cli::run(&args).map_err(PyRuntimeError::new_err)
}

#[pymodule]
fn ductile(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(check, m)?)?;
    m.add_function(wrap_pyfunction!(run, m)?)?;
    m.add_function(wrap_pyfunction!(parse, m)?)?;
    m.add_function(wrap_pyfunction!(graph, m)?)?;
    m.add_function(wrap_pyfunction!(cli_main, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
