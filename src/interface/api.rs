//! Rich Python-facing API (pyo3) — structured JSON returns for
//! agents (LangChain adapters), the web frontend, and notebooks.
//!
//! Design:
//! - Introspection (scripts/procs/pipeline/runs/db_stats) never raises on empty state.
//! - `run_json` raises only on *authoring* errors (parse/typecheck/policy);
//!   *execution* failure returns `{"ok":false,"error":...}` so agents can route on it.
//! - All JSON building goes through small pure functions (unit-tested below).

use crate::core::ast::ExecResult;
use crate::core::ast::Pipeline;
use crate::core::script_card::ScriptCard;
use crate::db::{self, ProcRow, RunRow};
use crate::egraph::{build_egraph, critical_path, parallel_groups};
use crate::executor::exec_pipeline;
use crate::parser::{parse_pipeline_file, parse_policy_file};
use crate::L2_orchestration::script::cse_safe;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

fn pyerr<E: ToString>(e: E) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

// ─────────────────────────────────────────────────────────────
// Pure JSON encoding helpers (unit-tested)
// ─────────────────────────────────────────────────────────────

/// JSON string escaping (control chars, quotes, backslash; UTF-8 passthrough).
pub fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Split a declaration list on *top-level* commas only — nesting-aware,
/// so `text(str, required), n(int, default=10)` yields two entries.
pub fn split_top_level(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' | '[' | '{' => {
                depth += 1;
                cur.push(c);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => {
                let t = cur.trim().to_string();
                if !t.is_empty() {
                    out.push(t);
                }
                cur.clear();
            }
            c => cur.push(c),
        }
    }
    let t = cur.trim().to_string();
    if !t.is_empty() {
        out.push(t);
    }
    out
}

#[derive(Debug, PartialEq)]
pub struct ParamDecl {
    pub name: String,
    pub ptype: String,
    pub required: bool,
    pub default: Option<String>,
}

/// Parse one params declaration: `text(str, required)` / `n(int, default=10)` / `x(str)`.
pub fn parse_param_decl(entry: &str) -> ParamDecl {
    let e = entry.trim();
    let (name, inner) = match e.find('(') {
        Some(i) if e.ends_with(')') => {
            (e[..i].trim().to_string(), e[i + 1..e.len() - 1].to_string())
        }
        _ => (e.to_string(), String::new()),
    };
    let mut parts = split_top_level(&inner);
    let ptype = if parts.is_empty() {
        String::new()
    } else {
        parts.remove(0).trim().to_string()
    };
    let mut required = false;
    let mut default = None;
    for m in &parts {
        let m = m.trim();
        if m == "required" {
            required = true;
        } else if let Some(v) = m.strip_prefix("default=") {
            default = Some(v.trim().to_string());
        }
    }
    ParamDecl {
        name,
        ptype,
        required,
        default,
    }
}

pub fn params_json(decl: &str) -> String {
    let items: Vec<String> = split_top_level(decl)
        .iter()
        .map(|e| {
            let p = parse_param_decl(e);
            format!(
                "{{\"name\":\"{}\",\"type\":\"{}\",\"required\":{},\"default\":{}}}",
                escape_json(&p.name),
                escape_json(&p.ptype),
                p.required,
                match &p.default {
                    Some(d) => format!("\"{}\"", escape_json(d)),
                    None => "null".to_string(),
                }
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Parse one output declaration: `words(int)` → (name, type).
pub fn parse_out_decl(entry: &str) -> (String, String) {
    let e = entry.trim();
    match e.find('(') {
        Some(i) if e.ends_with(')') => (
            e[..i].trim().to_string(),
            e[i + 1..e.len() - 1].trim().to_string(),
        ),
        _ => (e.to_string(), String::new()),
    }
}

pub fn output_json(decl: &str) -> String {
    let items: Vec<String> = split_top_level(decl)
        .iter()
        .map(|e| {
            let (n, t) = parse_out_decl(e);
            format!(
                "{{\"name\":\"{}\",\"type\":\"{}\"}}",
                escape_json(&n),
                escape_json(&t)
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Decode the internal `§§FIELDS§§k=v§§...§§RAW§§...` encoding into pairs.
/// Non-encoded text → None.
pub fn decode_internal_fields(text: &str) -> Option<Vec<(String, String)>> {
    let rest = text.strip_prefix("§§FIELDS§§")?;
    let mut out = Vec::new();
    for part in rest.split("§§") {
        if part == "RAW" {
            break;
        }
        if let Some(eq) = part.find('=') {
            out.push((part[..eq].to_string(), part[eq + 1..].to_string()));
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn script_card_json(c: &ScriptCard) -> String {
    format!(
        "{{\"name\":\"{}\",\"lang\":\"{}\",\"desc\":\"{}\",\"path\":\"{}\",\"params\":{},\"output\":{},\"pure\":{},\"idempotent\":{},\"concurrency\":\"{}\",\"effects\":\"{}\",\"cse_safe\":{},\"timeout_secs\":{},\"retries\":{}}}",
        escape_json(&c.name),
        escape_json(&c.lang),
        escape_json(&c.desc),
        escape_json(&c.path),
        params_json(&c.params),
        output_json(&c.output),
        c.pure,
        c.idempotent,
        c.concurrency.as_str(),
        escape_json(&c.effects),
        cse_safe(c),
        c.timeout_secs,
        c.retries
    )
}

pub fn proc_row_json(r: &ProcRow) -> String {
    let tags: Vec<String> = r
        .tags
        .iter()
        .map(|t| format!("\"{}\"", escape_json(t)))
        .collect();
    format!(
        "{{\"name\":\"{}\",\"pipeline\":\"{}\",\"description\":\"{}\",\"tags\":[{}],\"impl_count\":{},\"is_deliver\":{}}}",
        escape_json(&r.name),
        escape_json(&r.pipeline),
        escape_json(&r.description),
        tags.join(","),
        r.impl_count,
        r.is_deliver
    )
}

pub fn run_row_json(r: &RunRow) -> String {
    format!(
        "{{\"proc\":\"{}\",\"impl\":\"{}\",\"status\":\"{}\",\"latency_ms\":{},\"recorded_at\":\"{}\"}}",
        escape_json(&r.proc_name),
        escape_json(&r.impl_name),
        escape_json(&r.status),
        r.latency_ms,
        escape_json(&r.recorded_at)
    )
}

pub fn value_json(v: &crate::core::ast::Value) -> String {
    use crate::core::ast::Value;
    match v {
        Value::Text(t) => format!("{{\"type\":\"text\",\"value\":\"{}\"}}", escape_json(t)),
        Value::File(f) => format!("{{\"type\":\"file\",\"path\":\"{}\"}}", escape_json(f)),
        Value::Null => "{\"type\":\"null\"}".to_string(),
    }
}

/// Encode an ExecResult as the `run_json` payload.
pub fn exec_result_json(r: &ExecResult) -> String {
    match r {
        ExecResult::Success(results) => {
            let items: Vec<String> = results
                .iter()
                .map(|(k, v)| format!("\"{}\":{}", escape_json(k), value_json(v)))
                .collect();
            format!("{{\"ok\":true,\"results\":{{{}}}}}", items.join(","))
        }
        ExecResult::Failed { error, partial } => {
            // v0.14：失败是数据——partial 保留现场 + 首个根因的 err_code。
            let code = partial
                .values()
                .find_map(|v| match v {
                    crate::core::ast::Value::Text(t) => {
                        crate::core::dslresult::extract_field("err_code", t)
                    }
                    _ => None,
                })
                .unwrap_or_default();
            let mut parts: Vec<String> = Vec::new();
            for (k, v) in partial {
                let entry = match v {
                    crate::core::ast::Value::Text(t) if t.starts_with("§§FIELDS§§") => {
                        match decode_internal_fields(&t) {
                            Some(fields) => {
                                let items: Vec<String> = fields
                                    .iter()
                                    .map(|(fk, fv)| {
                                        format!("\"{}\":\"{}\"", escape_json(fk), escape_json(fv))
                                    })
                                    .collect();
                                format!("\"{}\":{{{}}}", escape_json(&k), items.join(","))
                            }
                            None => format!(
                                "\"{}\":{}",
                                escape_json(&k),
                                value_json(&crate::core::ast::Value::Text(t.clone()))
                            ),
                        }
                    }
                    other => format!("\"{}\":{}", escape_json(&k), value_json(&other)),
                };
                parts.push(entry);
            }
            format!(
                "{{\"ok\":false,\"error\":\"{}\",\"err_code\":\"{}\",\"partial\":{{{}}}}}",
                escape_json(error),
                escape_json(&code),
                parts.join(",")
            )
        }
    }
}

pub fn pipeline_json_of(pl: &Pipeline) -> String {
    let eg = build_egraph(pl);
    let groups = parallel_groups(&eg);
    let cp = critical_path(&eg);
    let mut procs = Vec::new();
    for p in &pl.procs {
        let impls: Vec<String> = p
            .plan
            .iter()
            .map(|imp| {
                let tags: Vec<String> = imp.tags.iter().map(|t| format!("\"{}\"", escape_json(t))).collect();
                let refs: Vec<String> = imp.refs.iter().map(|r| format!("\"{}\"", escape_json(r))).collect();
                format!(
                    "{{\"name\":\"{}\",\"desc\":\"{}\",\"tags\":[{}],\"when\":{},\"refs\":[{}],\"retry\":{},\"enabled\":{}}}",
                    escape_json(&imp.name),
                    escape_json(&imp.description),
                    tags.join(","),
                    match &imp.when {
                        Some(w) => format!("\"{}\"", escape_json(w)),
                        None => "null".to_string(),
                    },
                    refs.join(","),
                    imp.retry,
                    imp.enabled
                )
            })
            .collect();
        procs.push(format!(
            "{{\"name\":\"{}\",\"deliver\":{},\"impls\":[{}]}}",
            escape_json(&p.name),
            p.deliver,
            impls.join(",")
        ));
    }
    let g: Vec<String> = groups
        .iter()
        .map(|g| {
            format!(
                "[{}]",
                g.iter()
                    .map(|n| format!("\"{}\"", escape_json(n)))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .collect();
    let c: Vec<String> = cp
        .iter()
        .map(|n| format!("\"{}\"", escape_json(n)))
        .collect();
    format!(
        "{{\"name\":\"{}\",\"procs\":[{}],\"parallel_groups\":[{}],\"critical_path\":[{}]}}",
        escape_json(&pl.name),
        procs.join(","),
        g.join(","),
        c.join(",")
    )
}

// ─────────────────────────────────────────────────────────────
// pyo3 surface
// ─────────────────────────────────────────────────────────────

/// All registered script contract cards as JSON (agents read this, not script bodies).
pub fn scripts_json_core() -> String {
    let cards = db::script_list();
    let items: Vec<String> = cards.iter().map(script_card_json).collect();
    format!("[{}]", items.join(","))
}

#[pyfunction]
pub fn scripts_json() -> PyResult<String> {
    Ok(scripts_json_core())
}

/// Proc library as JSON. `query=""` lists all; otherwise FTS-ish search.
pub fn procs_json_core(query: &str) -> String {
    let rows = if query.trim().is_empty() {
        db::all_procs()
    } else {
        db::search_procs(query)
    };
    let items: Vec<String> = rows.iter().map(proc_row_json).collect();
    format!("[{}]", items.join(","))
}

#[pyfunction]
pub fn procs_json(query: &str) -> PyResult<String> {
    Ok(procs_json_core(query))
}

/// Recent runs of one proc as JSON (newest first), at most `limit`.
pub fn runs_json_core(proc_name: &str, limit: usize) -> String {
    let rows = db::recent_runs_limit(proc_name, limit);
    let items: Vec<String> = rows.iter().map(run_row_json).collect();
    format!("[{}]", items.join(","))
}

#[pyfunction]
pub fn runs_json(proc_name: &str, limit: usize) -> PyResult<String> {
    Ok(runs_json_core(proc_name, limit))
}

/// Library stats as JSON: {"pipelines":N,"procs":N,"runs":N,"compositions":N}.
pub fn db_stats_json_core() -> String {
    let (pipelines, procs, runs, compositions) = db::db_stats();
    format!(
        "{{\"pipelines\":{},\"procs\":{},\"runs\":{},\"compositions\":{}}}",
        pipelines, procs, runs, compositions
    )
}

#[pyfunction]
pub fn db_stats_json() -> PyResult<String> {
    Ok(db_stats_json_core())
}

/// Parse a .pipeline file into structural JSON (procs/impls/when/refs + egraph plan).
/// Pure-Rust core (serve/CLI call this; the pyo3 wrapper below is gc-droppable
/// outside Python — extension-module must not leak into bin/test link graphs).
pub fn pipeline_json_core(path: &str) -> Result<String, String> {
    let pl = parse_pipeline_file(path).map_err(|e| e.to_string())?;
    Ok(pipeline_json_of(&pl))
}

#[pyfunction]
pub fn pipeline_json(path: &str) -> PyResult<String> {
    pipeline_json_core(path).map_err(pyerr)
}

/// Run a pipeline; returns structured JSON.
/// Raises on authoring errors (parse/typecheck/policy); execution failure
/// is reported as `{"ok":false,"error":...}` for the caller to route on.
/// Pure-Rust core: authoring errors → Err(String); execution failure is
/// encoded as {"ok":false} in the Ok payload (failure is data, not exception).
pub fn run_json_core(
    path: &str,
    topic: &str,
    params: Option<BTreeMap<String, String>>,
    policy: Option<&str>,
) -> Result<String, String> {
    let pl = parse_pipeline_file(path).map_err(|e| e.to_string())?;
    let errs = crate::typecheck::check_pipeline(&pl);
    if !errs.is_empty() {
        let msg = errs
            .iter()
            .map(|e| format!("  {}", e))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("Type check errors:\n{}", msg));
    }
    let policy_opt = match policy {
        Some(p) => Some(parse_policy_file(p).map_err(|e| e.to_string())?),
        None => None,
    };
    let params = params.unwrap_or_default();
    let result = exec_pipeline(topic, &params, &pl, policy_opt.as_ref());
    Ok(exec_result_json(&result))
}

#[pyfunction]
#[pyo3(signature = (path, topic="", params=None, policy=None))]
pub fn run_json(
    path: &str,
    topic: &str,
    params: Option<BTreeMap<String, String>>,
    policy: Option<&str>,
) -> PyResult<String> {
    run_json_core(path, topic, params, policy).map_err(pyerr)
}

/// One-off script invoke. Unregistered name / authoring errors return
/// {"ok":false,"error":...} (agents route on it); only internal panics raise.
pub fn script_call_json_core(
    name: &str,
    args: Option<BTreeMap<String, String>>,
) -> Result<String, String> {
    if db::script_get(name).is_none() {
        let known: Vec<String> = db::script_list().iter().map(|c| c.name.clone()).collect();
        let hint = if known.is_empty() {
            format!(
                "script '{}' not attached — register first: ductile script attach <file>",
                name
            )
        } else {
            format!(
                "script '{}' not attached. Attached: {}",
                name,
                known.join(", ")
            )
        };
        return Ok(format!(
            "{{\"ok\":false,\"error\":\"{}\"}}",
            escape_json(&hint)
        ));
    }
    let kv: Vec<String> = args
        .unwrap_or_default()
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", k, v))
        .collect();
    let body = format!(
        "script({}{})",
        name,
        if kv.is_empty() {
            String::new()
        } else {
            format!(", {}", kv.join(", "))
        }
    );
    let impl_ = crate::core::ast::Impl {
        name: "api_call".into(),
        description: String::new(),
        tags: BTreeSet::new(),
        cost: crate::core::ast::Cost::default(),
        enabled: true,
        when: None,
        refs: Vec::new(),
        body_text: body.clone(),
        stub: false,
        retry: 0,
        ensure: Vec::new(),
    };
    match crate::steps::exec_script_call(&impl_, "", &body, &BTreeMap::new()) {
        Ok(v) => {
            // ##DSL_RESULT 块解码为干净字段（agent/前端不读 §§FIELDS§§ 内部编码）
            let text = match &v {
                crate::core::ast::Value::Text(t) => t.as_str(),
                _ => "",
            };
            match decode_internal_fields(text)
                .or_else(|| crate::core::dslresult::parse_dsl_result_block(text))
            {
                Some(fields) => {
                    let items: Vec<String> = fields
                        .iter()
                        .map(|(k, val)| format!("\"{}\":\"{}\"", escape_json(k), escape_json(val)))
                        .collect();
                    Ok(format!(
                        "{{\"ok\":true,\"fields\":{{{}}}}}",
                        items.join(",")
                    ))
                }
                None => Ok(format!("{{\"ok\":true,\"result\":{}}}", value_json(&v))),
            }
        }
        Err(e) => Ok(format!(
            "{{\"ok\":false,\"error\":\"{}\"}}",
            escape_json(&e)
        )),
    }
}

#[pyfunction]
#[pyo3(signature = (name, args=None))]
pub fn script_call_json(name: &str, args: Option<BTreeMap<String, String>>) -> PyResult<String> {
    script_call_json_core(name, args).map_err(pyerr)
}

/// Structural reuse lookup for LLM graph builders (`hyper similar --json`).
pub fn hyper_similar_json_core(
    query_path: &str,
    roots: Option<Vec<String>>,
) -> Result<String, String> {
    let roots = roots.unwrap_or_default();
    crate::hyper::similar_json(query_path, &roots)
}

#[pyfunction]
#[pyo3(signature = (query_path, roots=None))]
pub fn hyper_similar_json(query_path: &str, roots: Option<Vec<String>>) -> PyResult<String> {
    hyper_similar_json_core(query_path, roots).map_err(pyerr)
}

/// Node/proc reuse lookup (`hyper nodes --json`).
pub fn hyper_nodes_json_core(
    query: &str,
    roots: Option<Vec<String>>,
    role: Option<String>,
    op: Option<String>,
) -> Result<String, String> {
    let roots = roots.unwrap_or_default();
    let filter = if role.is_some() || op.is_some() {
        Some(crate::hyper::NodeQuery {
            role,
            op,
            in_arity: None,
            gated: None,
        })
    } else {
        None
    };
    crate::hyper::nodes_json(query, &roots, filter.as_ref())
}

#[pyfunction]
#[pyo3(signature = (query="", roots=None, role=None, op=None))]
pub fn hyper_nodes_json(
    query: &str,
    roots: Option<Vec<String>>,
    role: Option<String>,
    op: Option<String>,
) -> PyResult<String> {
    hyper_nodes_json_core(query, roots, role, op).map_err(pyerr)
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(scripts_json, m)?)?;
    m.add_function(wrap_pyfunction!(procs_json, m)?)?;
    m.add_function(wrap_pyfunction!(runs_json, m)?)?;
    m.add_function(wrap_pyfunction!(db_stats_json, m)?)?;
    m.add_function(wrap_pyfunction!(pipeline_json, m)?)?;
    m.add_function(wrap_pyfunction!(run_json, m)?)?;
    m.add_function(wrap_pyfunction!(script_call_json, m)?)?;
    m.add_function(wrap_pyfunction!(hyper_similar_json, m)?)?;
    m.add_function(wrap_pyfunction!(hyper_nodes_json, m)?)?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────
// Unit tests (pure Rust — no Python interpreter needed)
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ast::Value;
    use crate::core::script_card::{Concurrency, ScriptCard};
    use std::collections::BTreeSet;

    fn card(name: &str, params: &str, pure: bool, conc: Concurrency) -> ScriptCard {
        ScriptCard {
            name: name.into(),
            path: "/tmp/x.sh".into(),
            lang: "bash".into(),
            desc: "d".into(),
            params: params.into(),
            output: "words(int)".into(),
            pure,
            idempotent: pure,
            concurrency: conc,
            effects: "none".into(),
            timeout_secs: 10,
            retries: 0,
        }
    }

    #[test]
    fn decode_internal_fields_pairs_and_raw_boundary() {
        let enc = "§§FIELDS§§words=3§§lines=1§§RAW§§##DSL_RESULT\nwords=3";
        let f = decode_internal_fields(enc).unwrap();
        assert_eq!(
            f,
            vec![("words".into(), "3".into()), ("lines".into(), "1".into())]
        );
        // RAW 段里的 k=v 不得混入字段
        assert_eq!(
            decode_internal_fields("§§FIELDS§§a=1§§RAW§§b=2"),
            Some(vec![("a".into(), "1".into())])
        );
        assert_eq!(decode_internal_fields("plain text"), None);
        assert_eq!(decode_internal_fields("§§FIELDS§§§§RAW§§x"), None);
    }

    #[test]
    fn escape_json_specials() {
        assert_eq!(escape_json("a\"b\\c\n"), "a\\\"b\\\\c\\n");
        assert_eq!(escape_json("中文\n"), "中文\\n"); // UTF-8 passthrough
        assert_eq!(escape_json("\u{1}"), "\\u0001");
    }

    #[test]
    fn split_top_level_nesting_aware() {
        assert_eq!(
            split_top_level("text(str, required), n(int, default=10)"),
            vec!["text(str, required)", "n(int, default=10)"]
        );
        assert_eq!(split_top_level(""), Vec::<String>::new());
        assert_eq!(split_top_level("a, , b"), vec!["a", "b"]);
        // deep nesting survives
        assert_eq!(
            split_top_level("f(a(b, c), d), g"),
            vec!["f(a(b, c), d)", "g"]
        );
    }

    #[test]
    fn parse_param_decl_forms() {
        assert_eq!(
            parse_param_decl("text(str, required)"),
            ParamDecl {
                name: "text".into(),
                ptype: "str".into(),
                required: true,
                default: None
            }
        );
        assert_eq!(
            parse_param_decl("n(int, default=10)"),
            ParamDecl {
                name: "n".into(),
                ptype: "int".into(),
                required: false,
                default: Some("10".into())
            }
        );
        assert_eq!(
            parse_param_decl("x(str)"),
            ParamDecl {
                name: "x".into(),
                ptype: "str".into(),
                required: false,
                default: None
            }
        );
    }

    #[test]
    fn params_json_shape() {
        let j = params_json("text(str, required), n(int, default=10)");
        assert!(j.contains("\"name\":\"text\""));
        assert!(j.contains("\"required\":true"));
        assert!(j.contains("\"default\":\"10\""));
        assert_eq!(params_json(""), "[]");
    }

    #[test]
    fn output_json_shape() {
        let j = output_json("words(int), lines(int)");
        assert!(j.contains("\"name\":\"words\",\"type\":\"int\""));
        assert_eq!(output_json(""), "[]");
    }

    #[test]
    fn script_card_json_cse_flag() {
        let safe = script_card_json(&card("a", "x(str)", true, Concurrency::Safe));
        assert!(safe.contains("\"cse_safe\":true"));
        let excl = script_card_json(&card("b", "x(str)", true, Concurrency::Exclusive));
        assert!(excl.contains("\"cse_safe\":false"));
        let impure = script_card_json(&card("c", "x(str)", false, Concurrency::Safe));
        assert!(impure.contains("\"cse_safe\":false"));
    }

    #[test]
    fn exec_result_json_ok_and_fail() {
        let mut m = BTreeMap::new();
        m.insert("p".to_string(), Value::Text("v".into()));
        m.insert("f".to_string(), Value::File("/tmp/a".into()));
        m.insert("n".to_string(), Value::Null);
        let ok = exec_result_json(&ExecResult::Success(m));
        assert!(ok.starts_with("{\"ok\":true"));
        assert!(ok.contains("\"type\":\"file\",\"path\":\"/tmp/a\""));
        assert!(ok.contains("\"type\":\"null\""));
        let fail = exec_result_json(&ExecResult::Failed {
            error: "boom \"x\"".into(),
            partial: BTreeMap::new(),
        });
        assert!(fail.starts_with("{\"ok\":false,\"error\":\"boom \\\"x\\\"\",\"err_code\":\"\""));
        // v0.14：带 Left 的 partial 解码为干净对象
        let mut p = BTreeMap::new();
        let rec = crate::errflow::ErrorRecord::new(
            "render",
            "ffmpeg",
            "run timed out after 5s: ffmpeg",
            2,
        );
        p.insert(
            "render".to_string(),
            crate::core::ast::Value::Text(rec.encode()),
        );
        p.insert(
            "search".to_string(),
            crate::core::ast::Value::Text("hits".into()),
        );
        let fail2 = exec_result_json(&ExecResult::Failed {
            error: "All paths failed for proc: render".into(),
            partial: p,
        });
        assert!(fail2.contains("\"err_code\":\"timeout\""));
        assert!(fail2.contains("\"render\":{\"err\":\"1\""));
        assert!(fail2.contains("\"err_msg\":\"run timed out after 5s: ffmpeg\""));
        assert!(fail2.contains("\"search\":{\"type\":\"text\",\"value\":\"hits\"}"));
    }

    #[test]
    fn proc_row_and_run_row_json() {
        let pr = ProcRow {
            id: 1,
            name: "gen".into(),
            pipeline: "demo".into(),
            description: "d".into(),
            tags: ["a".to_string(), "b".to_string()].into_iter().collect(),
            impl_count: 2,
            is_deliver: false,
        };
        let j = proc_row_json(&pr);
        assert!(j.contains("\"tags\":[\"a\",\"b\"]"));
        assert!(j.contains("\"impl_count\":2"));

        let rr = RunRow {
            proc_name: "gen".into(),
            impl_name: "w".into(),
            status: "Ok".into(),
            latency_ms: 42,
            recorded_at: "t".into(),
            rate_tokens: 0,
            est_loss: 0.0,
        };
        let j = run_row_json(&rr);
        assert!(j.contains("\"latency_ms\":42"));
    }
}
