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

/// Encode an ExecResult as the `run_json` payload.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // P4a：API 面全 TOON（规格 §五.1；JSON 出口删除——零外部消费者实证）
    m.add_function(wrap_pyfunction!(scripts_toon, m)?)?;
    m.add_function(wrap_pyfunction!(procs_toon, m)?)?;
    m.add_function(wrap_pyfunction!(runs_toon, m)?)?;
    m.add_function(wrap_pyfunction!(db_stats_toon, m)?)?;
    m.add_function(wrap_pyfunction!(pipeline_toon, m)?)?;
    m.add_function(wrap_pyfunction!(run_toon, m)?)?;
    m.add_function(wrap_pyfunction!(script_call_toon, m)?)?;
    m.add_function(wrap_pyfunction!(hyper_similar_toon, m)?)?;
    m.add_function(wrap_pyfunction!(hyper_nodes_toon, m)?)?;
    Ok(())
}

// ── TOON 出口层（P4a；规格 §五.1：API 响应=canonical TOON，删 JSON 面）──────
//
// 9 个 pyo3 出口全换 TOON：scripts_toon/procs_toon/runs_toon/db_stats_toon/
// pipeline_toon/run_toon/script_call_toon/hyper_similar_toon/hyper_nodes_toon。
// 编码=kernel::toon::toon_canonical（唯一规范入口）；ExecResult 值域
// Text/File/Null 全部有 TOON 标量对应。

use crate::kernel::toon::{toon_canonical, TVal};

fn tv_str(s: &str) -> TVal { TVal::Str(s.to_string()) }

pub fn exec_result_toon(r: &ExecResult) -> String {
    match r {
        ExecResult::Success(results) => {
            let m: std::collections::BTreeMap<String, TVal> = results
                .iter()
                .map(|(k, v)| (k.clone(), match v {
                    crate::core::ast::Value::Text(t) => TVal::Str(t.clone()),
                    crate::core::ast::Value::File(f) => TVal::Str(f.clone()),
                    crate::core::ast::Value::Null => TVal::Null,
                }))
                .collect();
            let obj = TVal::Obj([
                ("ok".to_string(), TVal::Bool(true)),
                ("results".to_string(), TVal::Obj(m)),
            ].into_iter().collect());
            String::from_utf8(toon_canonical(&obj).unwrap_or_default()).unwrap_or_default()
        }
        ExecResult::Failed { error, partial } => {
            let code = partial
                .values()
                .find_map(|v| match v {
                    crate::core::ast::Value::Text(t) => {
                        crate::core::dslresult::extract_field("err_code", t)
                    }
                    _ => None,
                });
            let m: std::collections::BTreeMap<String, TVal> = partial
                .iter()
                .map(|(k, v)| (k.clone(), match v {
                    crate::core::ast::Value::Text(t) => TVal::Str(t.clone()),
                    crate::core::ast::Value::File(f) => TVal::Str(f.clone()),
                    crate::core::ast::Value::Null => TVal::Null,
                }))
                .collect();
            let obj = TVal::Obj([
                ("ok".to_string(), TVal::Bool(false)),
                ("error".to_string(), TVal::Str(error.clone())),
                ("err_code".to_string(), code.map(TVal::Str).unwrap_or(TVal::Null)),
                ("partial".to_string(), TVal::Obj(m)),
            ].into_iter().collect());
            String::from_utf8(toon_canonical(&obj).unwrap_or_default()).unwrap_or_default()
        }
    }
}

pub fn scripts_toon_core() -> String {
    let cards = db::script_list();
    // 数组=table（同构对象数组）：name/lang/desc/path/params/output/enabled/concurrency
    let rows: Vec<Vec<TVal>> = cards.iter().map(|c| vec![
        tv_str(&c.name), tv_str(&c.lang), tv_str(&c.desc), tv_str(&c.path),
        tv_str(&c.params), tv_str(&c.output),
        TVal::Bool(c.pure), TVal::Bool(c.idempotent),
        tv_str(&format!("{:?}", c.concurrency)),
    ]).collect();
    let t = TVal::Table {
        cols: vec!["name".into(), "lang".into(), "desc".into(), "path".into(),
                   "params".into(), "output".into(), "pure".into(), "idempotent".into(), "concurrency".into()],
        rows,
    };
    let obj = TVal::Obj([("scripts".to_string(), t)].into_iter().collect());
    String::from_utf8(toon_canonical(&obj).unwrap_or_default()).unwrap_or_default()
}

#[pyfunction]
pub fn scripts_toon() -> PyResult<String> {
    Ok(scripts_toon_core())
}

pub fn script_call_toon_core(
    name: &str,
    args: Option<BTreeMap<String, String>>,
) -> Result<String, String> {
    if db::script_get(name).is_none() {
        let known: Vec<String> = db::script_list().iter().map(|c| c.name.clone()).collect();
        let hint = if known.is_empty() {
            format!("script '{}' not attached — register first: ductile script attach <file>", name)
        } else {
            format!("script '{}' not attached. Attached: {}", name, known.join(", "))
        };
        let obj = TVal::Obj([
            ("ok".to_string(), TVal::Bool(false)),
            ("error".to_string(), TVal::Str(hint)),
        ].into_iter().collect());
        return Ok(String::from_utf8(toon_canonical(&obj).unwrap_or_default()).unwrap_or_default());
    }
    let kv: Vec<String> = args
        .unwrap_or_default()
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", k, v))
        .collect();
    let body = format!(
        "script({}{})",
        name,
        if kv.is_empty() { String::new() } else { format!(", {}", kv.join(", ")) }
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
            let text = match &v {
                crate::core::ast::Value::Text(t) => t.as_str(),
                _ => "",
            };
            match decode_internal_fields(text)
                .or_else(|| crate::core::dslresult::parse_dsl_result_block(text))
            {
                Some(fields) => {
                    let m: std::collections::BTreeMap<String, TVal> = fields
                        .into_iter()
                        .map(|(k, val)| (k, TVal::Str(val)))
                        .collect();
                    let obj = TVal::Obj([
                        ("ok".to_string(), TVal::Bool(true)),
                        ("fields".to_string(), TVal::Obj(m)),
                    ].into_iter().collect());
                    Ok(String::from_utf8(toon_canonical(&obj).unwrap_or_default()).unwrap_or_default())
                }
                None => {
                    let obj = TVal::Obj([
                        ("ok".to_string(), TVal::Bool(true)),
                        ("result".to_string(), match &v {
                            crate::core::ast::Value::Text(t) => TVal::Str(t.clone()),
                            crate::core::ast::Value::File(f) => TVal::Str(f.clone()),
                            crate::core::ast::Value::Null => TVal::Null,
                        }),
                    ].into_iter().collect());
                    Ok(String::from_utf8(toon_canonical(&obj).unwrap_or_default()).unwrap_or_default())
                }
            }
        }
        Err(e) => {
            let obj = TVal::Obj([
                ("ok".to_string(), TVal::Bool(false)),
                ("error".to_string(), TVal::Str(e)),
            ].into_iter().collect());
            Ok(String::from_utf8(toon_canonical(&obj).unwrap_or_default()).unwrap_or_default())
        }
    }
}

#[pyfunction]
#[pyo3(signature = (name, args=None))]
pub fn script_call_toon(name: &str, args: Option<BTreeMap<String, String>>) -> PyResult<String> {
    script_call_toon_core(name, args).map_err(pyerr)
}

pub fn run_toon_core(
    path: &str,
    topic: &str,
    params: Option<BTreeMap<String, String>>,
    policy: Option<&str>,
) -> Result<String, String> {
    let pl = parse_pipeline_file(path).map_err(|e| e.to_string())?;
    let errs = crate::typecheck::check_pipeline(&pl);
    if !errs.is_empty() {
        let msg = errs.iter().map(|e| format!("  {}", e)).collect::<Vec<_>>().join("\n");
        return Err(format!("Type check errors:\n{}", msg));
    }
    let policy_opt = match policy {
        Some(p) => Some(parse_policy_file(p).map_err(|e| e.to_string())?),
        None => None,
    };
    let params = params.unwrap_or_default();
    let result = exec_pipeline(topic, &params, &pl, policy_opt.as_ref());
    Ok(exec_result_toon(&result))
}

#[pyfunction]
#[pyo3(signature = (path, topic="", params=None, policy=None))]
pub fn run_toon(
    path: &str,
    topic: &str,
    params: Option<BTreeMap<String, String>>,
    policy: Option<&str>,
) -> PyResult<String> {
    run_toon_core(path, topic, params, policy).map_err(pyerr)
}

// ── TOON 出口（续）：procs/runs/db_stats/pipeline/hyper ──────────────

pub fn procs_toon_core(query: &str) -> String {
    let rows = if query.trim().is_empty() {
        db::all_procs()
    } else {
        db::search_procs(query)
    };
    let entries: Vec<TVal> = rows.iter().map(|r| {
        let tags: Vec<TVal> = r.tags.iter().map(|t| TVal::Str(t.clone())).collect();
        TVal::Obj([
            ("name".to_string(), TVal::Str(r.name.clone())),
            ("pipeline".to_string(), TVal::Str(r.pipeline.clone())),
            ("description".to_string(), TVal::Str(r.description.clone())),
            ("tags".to_string(), TVal::Arr(tags)),
            ("impl_count".to_string(), TVal::Num(r.impl_count as u64)),
            ("is_deliver".to_string(), TVal::Bool(r.is_deliver)),
        ].into_iter().collect())
    }).collect();
    let obj = TVal::Obj([("procs".to_string(), TVal::Arr(entries))].into_iter().collect());
    String::from_utf8(toon_canonical(&obj).unwrap_or_default()).unwrap_or_default()
}

#[pyfunction]
pub fn procs_toon(query: &str) -> PyResult<String> {
    Ok(procs_toon_core(query))
}

pub fn runs_toon_core(proc_name: &str, limit: usize) -> String {
    let rows = db::recent_runs_limit(proc_name, limit);
    let entries: Vec<TVal> = rows.iter().map(|r| {
        TVal::Obj([
            ("proc".to_string(), TVal::Str(r.proc_name.clone())),
            ("impl".to_string(), TVal::Str(r.impl_name.clone())),
            ("status".to_string(), TVal::Str(r.status.clone())),
            ("latency_ms".to_string(), TVal::Num(r.latency_ms as u64)),
            ("recorded_at".to_string(), TVal::Str(r.recorded_at.clone())),
        ].into_iter().collect())
    }).collect();
    let obj = TVal::Obj([("runs".to_string(), TVal::Arr(entries))].into_iter().collect());
    String::from_utf8(toon_canonical(&obj).unwrap_or_default()).unwrap_or_default()
}

#[pyfunction]
pub fn runs_toon(proc_name: &str, limit: usize) -> PyResult<String> {
    Ok(runs_toon_core(proc_name, limit))
}

pub fn db_stats_toon_core() -> String {
    let (pipelines, procs, runs, compositions) = db::db_stats();
    let obj = TVal::Obj([
        ("pipelines".to_string(), TVal::Num(pipelines as u64)),
        ("procs".to_string(), TVal::Num(procs as u64)),
        ("runs".to_string(), TVal::Num(runs as u64)),
        ("compositions".to_string(), TVal::Num(compositions as u64)),
    ].into_iter().collect());
    String::from_utf8(toon_canonical(&obj).unwrap_or_default()).unwrap_or_default()
}

#[pyfunction]
pub fn db_stats_toon() -> PyResult<String> {
    Ok(db_stats_toon_core())
}

/// Parse a .pipeline file into structural TOON.
pub fn pipeline_toon_core(path: &str) -> Result<String, String> {
    let pl = parse_pipeline_file(path).map_err(|e| e.to_string())?;
    // 结构面：procs 列表（name/desc/impl_count）+ proc 名数组
    let procs: Vec<TVal> = pl.procs.iter().map(|p| {
        TVal::Obj([
            ("name".to_string(), TVal::Str(p.name.clone())),
            ("desc".to_string(), TVal::Str(p.description.clone())),
            ("impls".to_string(), TVal::Num(p.plan.len() as u64)),
            ("deliver".to_string(), TVal::Bool(p.deliver)),
        ].into_iter().collect())
    }).collect();
    let obj = TVal::Obj([("procs".to_string(), TVal::Arr(procs))].into_iter().collect());
    Ok(String::from_utf8(toon_canonical(&obj).unwrap_or_default()).unwrap_or_default())
}

#[pyfunction]
pub fn pipeline_toon(path: &str) -> PyResult<String> {
    pipeline_toon_core(path).map_err(pyerr)
}

pub fn hyper_similar_toon_core(
    query_path: &str,
    roots: Option<Vec<String>>,
) -> Result<String, String> {
    // hyper 层结构化查询→TOON 重编码（json 中间层不暴露）
    let j = crate::hyper::similar_json(query_path, &roots.unwrap_or_default())?;
    json_str_to_toon(&j)
}

#[pyfunction]
#[pyo3(signature = (query_path, roots=None))]
pub fn hyper_similar_toon(query_path: &str, roots: Option<Vec<String>>) -> PyResult<String> {
    hyper_similar_toon_core(query_path, roots).map_err(pyerr)
}

pub fn hyper_nodes_toon_core(
    query: &str,
    roots: Option<Vec<String>>,
    role: Option<String>,
    op: Option<String>,
) -> Result<String, String> {
    let roots = roots.unwrap_or_default();
    let filter = if role.is_some() || op.is_some() {
        Some(crate::hyper::NodeQuery { role, op, in_arity: None, gated: None })
    } else {
        None
    };
    let j = crate::hyper::nodes_json(query, &roots, filter.as_ref())?;
    json_str_to_toon(&j)
}

#[pyfunction]
#[pyo3(signature = (query="", roots=None, role=None, op=None))]
pub fn hyper_nodes_toon(
    query: &str,
    roots: Option<Vec<String>>,
    role: Option<String>,
    op: Option<String>,
) -> PyResult<String> {
    hyper_nodes_toon_core(query, roots, role, op).map_err(pyerr)
}

/// 内部 JSON 字符串→TOON（bridge 函数：hyper 层遗留 json 输出收编；
/// P4 后续轮次把 hyper 层原生 TOON 化后删除）。
fn json_str_to_toon(j: &str) -> Result<String, String> {
    // 极小 bridge：数组/对象/标量的宽松转换（hyper 输出形状有限）
    let v = parse_loose_json(j)?;
    let t = toon_canonical(&v).map_err(|e| e.to_string())?;
    String::from_utf8(t).map_err(|e| e.to_string())
}

fn parse_loose_json(j: &str) -> Result<TVal, String> {
    let b: Vec<char> = j.chars().collect();
    let mut i = 0usize;
    let v = parse_loose_value(&b, &mut i)?;
    skip_ws(&b, &mut i);
    if i != b.len() {
        return Err("trailing".into());
    }
    Ok(v)
}

fn skip_ws(b: &[char], i: &mut usize) {
    while *i < b.len() && b[*i].is_whitespace() { *i += 1; }
}

fn parse_loose_value(b: &[char], i: &mut usize) -> Result<TVal, String> {
    skip_ws(b, i);
    if *i >= b.len() { return Err("eof".into()); }
    match b[*i] {
        '{' => {
            *i += 1;
            let mut m = std::collections::BTreeMap::new();
            skip_ws(b, i);
            if *i < b.len() && b[*i] == '}' { *i += 1; return Ok(TVal::Obj(m)); }
            loop {
                skip_ws(b, i);
                let k = parse_loose_string(b, i)?;
                skip_ws(b, i);
                if *i >= b.len() || b[*i] != ':' { return Err("expect :".into()); }
                *i += 1;
                let v = parse_loose_value(b, i)?;
                if m.insert(k, v).is_some() { return Err("dup key".into()); }
                skip_ws(b, i);
                match b.get(*i) {
                    Some(',') => { *i += 1; }
                    Some('}') => { *i += 1; break; }
                    _ => return Err("expect , or }".into()),
                }
            }
            Ok(TVal::Obj(m))
        }
        '[' => {
            *i += 1;
            let mut items = Vec::new();
            skip_ws(b, i);
            if *i < b.len() && b[*i] == ']' { *i += 1; return Ok(TVal::Arr(items)); }
            loop {
                let v = parse_loose_value(b, i)?;
                items.push(v);
                skip_ws(b, i);
                match b.get(*i) {
                    Some(',') => { *i += 1; }
                    Some(']') => { *i += 1; break; }
                    _ => return Err("expect , or ]".into()),
                }
            }
            Ok(TVal::Arr(items))
        }
        '"' => Ok(TVal::Str(parse_loose_string(b, i)?)),
        't' => { expect_lit(b, i, "true")?; Ok(TVal::Bool(true)) }
        'f' => { expect_lit(b, i, "false")?; Ok(TVal::Bool(false)) }
        'n' => { expect_lit(b, i, "null")?; Ok(TVal::Null) }
        _ => {
            let start = *i;
            while *i < b.len() && (b[*i].is_ascii_digit() || b[*i] == '-') { *i += 1; }
            let s: String = b[start..*i].iter().collect();
            if s.is_empty() { return Err("bad value".into()); }
            let n: u64 = s.parse().map_err(|_| "num")?;
            Ok(TVal::Num(n))
        }
    }
}

fn parse_loose_string(b: &[char], i: &mut usize) -> Result<String, String> {
    if *i >= b.len() || b[*i] != '"' { return Err("expect string".into()); }
    *i += 1;
    let mut out = String::new();
    while *i < b.len() {
        match b[*i] {
            '\\' => {
                *i += 1;
                match b.get(*i) {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(c) => return Err(format!("bad esc {c}")),
                    None => return Err("eof esc".into()),
                }
                *i += 1;
            }
            '"' => { *i += 1; return Ok(out); }
            c => { out.push(c); *i += 1; }
        }
    }
    Err("unterminated".into())
}

fn expect_lit(b: &[char], i: &mut usize, lit: &str) -> Result<(), String> {
    for c in lit.chars() {
        if b.get(*i) != Some(&c) { return Err(format!("expect {lit}")); }
        *i += 1;
    }
    Ok(())
}
