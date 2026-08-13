//! Ductile — declarative pipeline DSL engine.
//!
//! Core library: parse, typecheck, execute .pipeline files.
//! Python bindings via pyo3.

pub mod ast;
pub mod parser;
pub mod typecheck;
pub mod egraph;
pub mod executor;
pub mod db;
pub mod registry;
pub mod learn;
pub mod version;

pub mod cli;

pub use ast::*;
pub use parser::{parse_pipeline, parse_pipeline_file, ParseError};
pub use typecheck::{check_pipeline, TypeError};
pub use egraph::{build_egraph, parallel_groups, critical_path, EGraph};
pub use executor::exec_pipeline;

// ── pyo3 Python bindings ──

use pyo3::prelude::*;
use pyo3::exceptions::PyRuntimeError;
use std::collections::BTreeMap;

/// Check a pipeline file: parse + typecheck.
#[pyfunction]
fn check(path: &str) -> PyResult<String> {
    let pl = parse_pipeline_file(path)
        .map_err(|e| PyRuntimeError::new_err(format!("{}", e)))?;
    let errs = check_pipeline(&pl);
    if errs.is_empty() {
        Ok("Type check passed".to_string())
    } else {
        let msg = errs.iter()
            .map(|e| format!("  {}", e))
            .collect::<Vec<_>>()
            .join("\n");
        Err(PyRuntimeError::new_err(format!("Type check errors:\n{}", msg)))
    }
}

/// Run a pipeline file with a topic and optional params.
#[pyfunction]
#[pyo3(signature = (path, topic="", params=None))]
fn run(path: &str, topic: &str, params: Option<BTreeMap<String, String>>) -> PyResult<String> {
    let pl = parse_pipeline_file(path)
        .map_err(|e| PyRuntimeError::new_err(format!("{}", e)))?;
    let errs = check_pipeline(&pl);
    if !errs.is_empty() {
        let msg = errs.iter()
            .map(|e| format!("  {}", e))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(PyRuntimeError::new_err(format!("Type check errors:\n{}", msg)));
    }
    let params = params.unwrap_or_default();
    match exec_pipeline(topic, &params, &pl) {
        ExecResult::Success(results) => {
            let mut out = format!("Pipeline executed successfully ({} procs)\n", results.len());
            for (k, v) in &results {
                let shown = match v {
                    Value::Text(t) => {
                        if t.len() > 200 { format!("{}...", &t[..200]) } else { t.clone() }
                    }
                    Value::File(f) => format!("<file: {}>", f),
                    Value::Null => "<null>".into(),
                };
                out.push_str(&format!("  {} => {}\n", k, shown));
            }
            Ok(out)
        }
        ExecResult::Failed(err) => Err(PyRuntimeError::new_err(format!("Pipeline failed: {}", err))),
    }
}

/// Parse a pipeline file and return structure info.
#[pyfunction]
fn parse(path: &str) -> PyResult<String> {
    let pl = parse_pipeline_file(path)
        .map_err(|e| PyRuntimeError::new_err(format!("{}", e)))?;
    let tags = pl.computed_tags();
    let tag_str: Vec<String> = tags.iter().map(|t| format!("#{}", t)).collect();
    let mut out = format!(
        "Pipeline: {}\nTags: {}\n\n",
        pl.name, tag_str.join(", ")
    );
    for proc in &pl.procs {
        out.push_str(&format!(
            "  Proc: {} ({} impls){}\n",
            proc.name, proc.plan.len(),
            if proc.deliver { " [deliver]" } else { "" }
        ));
        for imp in &proc.plan {
            let imp_tags: Vec<String> = imp.tags.iter().map(|t| format!("#{}", t)).collect();
            out.push_str(&format!(
                "    Impl: {}{} cost(latency={}, risk={}, tokens={}, money={})\n",
                imp.name,
                if !imp_tags.is_empty() { format!(" [{}]", imp_tags.join(", ")) } else { String::new() },
                imp.cost.latency, imp.cost.risk, imp.cost.tokens, imp.cost.money
            ));
        }
    }
    Ok(out)
}

/// Show e-graph structure: parallel groups and critical path.
#[pyfunction]
fn graph(path: &str) -> PyResult<String> {
    let pl = parse_pipeline_file(path)
        .map_err(|e| PyRuntimeError::new_err(format!("{}", e)))?;
    let eg = build_egraph(&pl);
    let groups = parallel_groups(&eg);
    let cp = critical_path(&eg);
    let mut out = format!(
        "E-Graph:\n  Nodes: {}\n  Edges: {}\n\nParallel groups:\n",
        pl.procs.len(), eg.edges.len()
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
