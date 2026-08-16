//! Executor — runs a parsed Pipeline.
//!
//! Handles: cost-based impl selection, fallback, checks, foreach expansion,
//! retry with exponential backoff, variable resolution, shell command execution,
//! ##DSL_RESULT protocol, record (GCF) logging.

use crate::ast::*;
use crate::db;
use crate::egraph;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

// ── Hot patches ──

/// Load patches from SQLite and apply them to a cloned pipeline.
/// Returns Cow-like: if no patches, returns original reference wrapped in Ok.
/// Supports overriding: enabled, cost.latency, cost.risk, cost.tokens, cost.money, retry, stub.
fn apply_patches(pl: &Pipeline) -> Pipeline {
    let patches = db::load_patches(&pl.name);
    if patches.is_empty() {
        return pl.clone();
    }
    eprintln!(
        "  [patch] {} patches applied to pipeline '{}'",
        patches.len(),
        pl.name
    );
    let mut cloned = pl.clone();
    for patch in &patches {
        for proc in &mut cloned.procs {
            if proc.name != patch.proc_name {
                continue;
            }
            for impl_ in &mut proc.plan {
                if impl_.name != patch.impl_name {
                    continue;
                }
                match patch.field.as_str() {
                    "enabled" => {
                        impl_.enabled = patch.value == "true" || patch.value == "1";
                        eprintln!(
                            "  [patch] {}.{}: enabled={}",
                            proc.name, impl_.name, impl_.enabled
                        );
                    }
                    "cost.latency" => {
                        impl_.cost.latency = patch.value.parse().unwrap_or(impl_.cost.latency);
                        eprintln!(
                            "  [patch] {}.{}: cost.latency={}",
                            proc.name, impl_.name, impl_.cost.latency
                        );
                    }
                    "cost.risk" => {
                        impl_.cost.risk = patch.value.parse().unwrap_or(impl_.cost.risk);
                        eprintln!(
                            "  [patch] {}.{}: cost.risk={}",
                            proc.name, impl_.name, impl_.cost.risk
                        );
                    }
                    "cost.tokens" => {
                        impl_.cost.tokens = patch.value.parse().unwrap_or(impl_.cost.tokens);
                        eprintln!(
                            "  [patch] {}.{}: cost.tokens={}",
                            proc.name, impl_.name, impl_.cost.tokens
                        );
                    }
                    "cost.money" => {
                        impl_.cost.money = patch.value.parse().unwrap_or(impl_.cost.money);
                        eprintln!(
                            "  [patch] {}.{}: cost.money={}",
                            proc.name, impl_.name, impl_.cost.money
                        );
                    }
                    "retry" => {
                        impl_.retry = patch.value.parse().unwrap_or(impl_.retry);
                        eprintln!(
                            "  [patch] {}.{}: retry={}",
                            proc.name, impl_.name, impl_.retry
                        );
                    }
                    "stub" => {
                        impl_.stub = patch.value == "true" || patch.value == "1";
                        eprintln!(
                            "  [patch] {}.{}: stub={}",
                            proc.name, impl_.name, impl_.stub
                        );
                    }
                    _ => {
                        eprintln!(
                            "  [patch] {}.{}: unknown field '{}'",
                            proc.name, impl_.name, patch.field
                        );
                    }
                }
            }
        }
    }
    cloned
}

pub fn exec_pipeline(topic: &str, params: &BTreeMap<String, String>, pl: &Pipeline) -> ExecResult {
    // Apply hot patches: clone pipeline, override fields from SQLite
    let pl = apply_patches(pl);
    let eg = egraph::build_egraph(&pl);
    let layers = egraph::parallel_groups(&eg);

    let mut results: BTreeMap<String, Value> = BTreeMap::new();
    let mut failed = false;
    let mut error_msg = String::new();

    for layer in &layers {
        if failed {
            break;
        }
        // Execute procs in this layer (serially for now — parallel via threads later)
        for proc_name in layer {
            if failed {
                break;
            }
            let proc = pl.procs.iter().find(|p| &p.name == proc_name);
            if let Some(proc) = proc {
                if proc.deliver {
                    continue;
                }

                match exec_proc(proc, topic, params, &results, &pl) {
                    Ok(val) => {
                        results.insert(proc.name.clone(), val);
                    }
                    Err(e) => {
                        failed = true;
                        error_msg = e;
                    }
                }
            }
        }
    }

    if failed {
        ExecResult::Failed(error_msg)
    } else {
        ExecResult::Success(results)
    }
}

fn exec_proc(
    proc: &Proc,
    topic: &str,
    params: &BTreeMap<String, String>,
    results: &BTreeMap<String, Value>,
    pl: &Pipeline,
) -> Result<Value, String> {
    // Handle foreach
    if let Some(ref src) = proc.foreach {
        return exec_foreach_proc(proc, src, topic, params, results, pl);
    }

    // Normal proc: rank impls, try in order
    let recent_map = load_recent_runs(&proc.name);
    let eligible: Vec<&Impl> = proc
        .plan
        .iter()
        .filter(|i| is_eligible(params, results, i))
        .collect();

    let ranked = rank_impls(&pl.weights, &recent_map, &eligible, &proc.pick_by);

    for (rank, impl_) in ranked.iter().enumerate() {
        let pid = (b'A' + rank as u8) as char;
        if impl_.stub {
            eprintln!("  [{}] STUB: {}", proc.name, impl_.name);
            return Ok(Value::Text(format!("[STUB:{}]", impl_.name)));
        }
        if !impl_.enabled {
            continue;
        }

        eprintln!("  [{}] trying: {}", proc.name, impl_.name);
        match run_impl_with_retry(impl_, topic, results) {
            Ok(val) => {
                // Run checks
                let all_checks: Vec<&Check> =
                    proc.checks.iter().chain(impl_.ensure.iter()).collect();
                match run_checks(&all_checks, &val) {
                    Ok(val) => {
                        let out_text = match &val {
                            Value::Text(t) => t.clone(),
                            _ => String::new(),
                        };
                        append_run_rd(
                            &proc.name,
                            &impl_.name,
                            pid,
                            Status::Ok,
                            None,
                            None,
                            &out_text,
                        );
                        return Ok(val);
                    }
                    Err(chk_err) => {
                        eprintln!("  [{}] check failed: {}", proc.name, chk_err);
                        append_run(
                            &proc.name,
                            &impl_.name,
                            pid,
                            Status::Fail,
                            Some(&short_hash(&chk_err)),
                            Some(&format!("{}.{}.check", proc.name, impl_.name)),
                        );
                        continue;
                    }
                }
            }
            Err(err) => {
                eprintln!("  [{}] failed: {}", proc.name, err);
                append_run(
                    &proc.name,
                    &impl_.name,
                    pid,
                    Status::Fail,
                    Some(&short_hash(&err)),
                    Some(&format!("{}.{}.step", proc.name, impl_.name)),
                );
                continue;
            }
        }
    }

    Err(format!("All paths failed for proc: {}", proc.name))
}

fn exec_foreach_proc(
    proc: &Proc,
    src_proc: &str,
    topic: &str,
    _params: &BTreeMap<String, String>,
    results: &BTreeMap<String, Value>,
    pl: &Pipeline,
) -> Result<Value, String> {
    let var_name = &proc.foreach_var;
    let src_val = results
        .get(src_proc)
        .ok_or_else(|| format!("foreach source not found: @{}", src_proc))?;

    let items: Vec<String> = match src_val {
        Value::Text(t) => t
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|s| s.to_string())
            .collect(),
        _ => vec![format!("{:?}", src_val)],
    };

    eprintln!(
        "  [{}] foreach over {} ({} items, var={})",
        proc.name,
        src_proc,
        items.len(),
        var_name
    );

    let mut sub_results: Vec<Result<Value, String>> = Vec::new();
    let recent_map = load_recent_runs(&proc.name);
    let eligible: Vec<&Impl> = proc.plan.iter().collect();
    let ranked = rank_impls(&pl.weights, &recent_map, &eligible, &proc.pick_by);

    for item in &items {
        let clean_item = item.split('|').next().unwrap_or(item).trim().to_string();
        let preview: String = clean_item.chars().take(80).collect();
        eprintln!("    -> item: {}", preview);

        let mut item_results = results.clone();
        item_results.insert(var_name.clone(), Value::Text(clean_item.clone()));

        let mut found = false;
        for (rank, impl_) in ranked.iter().enumerate() {
            let pid = (b'A' + rank as u8) as char;
            if impl_.stub {
                sub_results.push(Ok(Value::Text(format!(
                    "[STUB:{}:{}]",
                    impl_.name,
                    &clean_item.chars().take(40).collect::<String>()
                ))));
                found = true;
                break;
            }
            if !impl_.enabled {
                continue;
            }

            let expanded = Impl {
                body_text: impl_
                    .body_text
                    .replace(&format!("{{{}}}", var_name), &clean_item),
                ..(*impl_).clone()
            };

            match run_impl_with_retry(&expanded, topic, &item_results) {
                Ok(val) => {
                    append_run(&proc.name, &impl_.name, pid, Status::Ok, None, None);
                    sub_results.push(Ok(val));
                    found = true;
                    break;
                }
                Err(err) => {
                    append_run(
                        &proc.name,
                        &impl_.name,
                        pid,
                        Status::Fail,
                        Some(&short_hash(&err)),
                        Some(&format!("{}.{}.foreach", proc.name, impl_.name)),
                    );
                }
            }
        }
        if !found {
            return Err(format!(
                "All paths failed for foreach item in: {}",
                proc.name
            ));
        }
    }

    // Aggregate
    let parts: Vec<String> = sub_results
        .iter()
        .filter_map(|r| match r {
            Ok(Value::Text(t)) => Some(t.clone()),
            _ => None,
        })
        .collect();
    Ok(Value::Text(parts.join("\n---\n---\n")))
}

// ── Impl execution ──

fn run_impl_with_retry(
    impl_: &Impl,
    topic: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    if impl_.retry == 0 {
        return run_impl_steps(impl_, topic, results);
    }

    let mut attempt = 0;
    loop {
        match run_impl_steps(impl_, topic, results) {
            Ok(val) => return Ok(val),
            Err(err) => {
                if attempt < impl_.retry {
                    let delay = 1u64 << (attempt + 1); // 2s, 4s, 8s...
                    eprintln!(
                        "    -> retry {}/{} after {}s ({})",
                        attempt + 1,
                        impl_.retry,
                        delay,
                        &err[..err.len().min(80)]
                    );
                    std::thread::sleep(std::time::Duration::from_secs(delay));
                    attempt += 1;
                } else {
                    return Err(err);
                }
            }
        }
    }
}

fn run_impl_steps(
    impl_: &Impl,
    topic: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let body = &impl_.body_text;
    let func = detect_func(body);

    match func.as_str() {
        "mcp_search" | "search" => exec_search(impl_, topic, body, results, true),
        "web_search" => exec_search(impl_, topic, body, results, false),
        "llm" => exec_llm(impl_, topic, body, results),
        "merge" => exec_merge(impl_, body, results),
        "write" => exec_write(impl_, topic, body, results),
        "read" => exec_read(impl_, body),
        "run" | "sh" => exec_run(impl_, topic, body, results),
        "cache" => Err("cache miss".into()),
        _ => Ok(Value::Text(format!("<noop: {}>", func))),
    }
}

fn detect_func(body: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut acc = String::new();
    let mut found_paren = false;

    for &c in &chars {
        if c == '(' {
            if !acc.is_empty() {
                found_paren = true;
                break;
            }
            return String::new();
        }
        if c.is_alphanumeric() || c == '_' {
            acc.push(c);
        } else if !acc.is_empty() {
            // Accumulated identifier but next char isn't (
            return String::new();
        }
    }
    if found_paren {
        acc
    } else {
        String::new()
    }
}

// ── Variable resolution ──

fn resolve_vars(text: &str, topic: &str, results: &BTreeMap<String, Value>) -> String {
    let t1 = text.replace("{topic}", topic);
    let t2 = t1.replace("{hash(topic)}", &short_hash(topic));

    // Replace @procname references
    let mut result = String::new();
    let chars: Vec<char> = t2.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            let start = i + 1;
            let mut end = start;
            while end < chars.len()
                && (chars[end].is_alphanumeric() || chars[end] == '_' || chars[end] == '-')
            {
                end += 1;
            }
            if end > start {
                let name: String = chars[start..end].iter().collect();

                // Check for .field suffix
                let mut field = None;
                let mut after = end;
                if after < chars.len() && chars[after] == '.' {
                    let f_start = after + 1;
                    let mut f_end = f_start;
                    while f_end < chars.len()
                        && (chars[f_end].is_alphanumeric() || chars[f_end] == '_')
                    {
                        f_end += 1;
                    }
                    field = Some(chars[f_start..f_end].iter().collect::<String>());
                    after = f_end;
                }

                match results.get(&name) {
                    Some(Value::Text(v)) => {
                        if let Some(fld) = field {
                            if let Some(fv) = extract_field(&fld, v) {
                                result.push_str(&fv);
                            } else {
                                result.push_str(&format!("<no field:{}.{}>", name, fld));
                            }
                        } else {
                            result.push_str(v);
                        }
                    }
                    Some(v) => result.push_str(v.as_text()),
                    None => {
                        result.push('@');
                        result.push_str(&name);
                    }
                }
                i = after;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

// ── String arg extraction ──

fn extract_string_arg(key: &str, body: &str) -> String {
    // key="value" or key=value
    let pattern_q = format!("{}=\"", key);
    if let Some(pos) = body.find(&pattern_q) {
        let after = &body[pos + pattern_q.len()..];
        return after.chars().take_while(|c| *c != '"').collect();
    }
    let pattern_r = format!("{}=", key);
    if let Some(pos) = body.find(&pattern_r) {
        // Ensure it's not inside a longer key (e.g., "content" matching "cont")
        let before = if pos > 0 { &body[..pos] } else { "" };
        let last_char = before.chars().last();
        if let Some(c) = last_char {
            if c.is_alphanumeric() || c == '_' {
                return String::new();
            }
        }
        let after = &body[pos + pattern_r.len()..];
        return after
            .chars()
            .take_while(|c| *c != ',' && *c != ')' && *c != ' ')
            .collect();
    }
    String::new()
}

fn extract_first_string(body: &str) -> String {
    if let Some(pos) = body.find('"') {
        let after = &body[pos + 1..];
        if let Some(end) = after.find('"') {
            return after[..end].to_string();
        }
    }
    body.to_string()
}

// ── Built-in executors ──

fn exec_search(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
    is_mcp: bool,
) -> Result<Value, String> {
    let query = extract_string_arg("query", body);
    let resolved = resolve_vars(&query, topic, results);
    let func_name = if is_mcp { "mcp_search" } else { "web_search" };
    eprintln!("    -> {}: \"{}\"", func_name, resolved);

    let bridge = find_bridge("search_bridge.py");
    let mode = if is_mcp { "mcp_search" } else { "web_search" };
    let engine = extract_string_arg("engine", body);

    let output = Command::new("python3")
        .arg(&bridge)
        .arg(mode)
        .arg(&resolved)
        .args(if engine.is_empty() {
            vec![]
        } else {
            vec![&engine]
        })
        .output()
        .map_err(|e| format!("search bridge launch failed: {}", e))?;

    if output.status.success() {
        Ok(Value::Text(
            String::from_utf8_lossy(&output.stdout).to_string(),
        ))
    } else {
        Err(format!(
            "search failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(200)
                .collect::<String>()
        ))
    }
}

fn exec_llm(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let prompt = extract_string_arg("input", body);
    let template = extract_string_arg("template", body);
    let count = extract_string_arg("count", body);
    let resolved_prompt = resolve_vars(&prompt, topic, results);
    eprintln!(
        "    -> llm: template={}, input.len={}",
        template,
        resolved_prompt.len()
    );

    let bridge = find_bridge("llm_bridge.py");
    let count_arg = if count.is_empty() { "5" } else { &count };

    let output = Command::new("python3")
        .arg(&bridge)
        .arg(&resolved_prompt)
        .arg(&template)
        .arg(count_arg)
        .output()
        .map_err(|e| format!("llm bridge launch failed: {}", e))?;

    if output.status.success() {
        Ok(Value::Text(
            String::from_utf8_lossy(&output.stdout).to_string(),
        ))
    } else {
        Err(format!(
            "llm failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(200)
                .collect::<String>()
        ))
    }
}

fn exec_merge(
    impl_: &Impl,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let refs = &impl_.refs;
    let parts: Vec<String> = refs
        .iter()
        .filter(|r| *r != "merge")
        .filter_map(|r| results.get(r))
        .map(|v| v.as_text().to_string())
        .collect();
    let has_dedup = body.contains("dedup");
    let merged = if has_dedup {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for p in &parts {
            if seen.insert(p.clone()) {
                out.push(p.clone());
            }
        }
        out.join("\n")
    } else {
        parts.join("\n---\n")
    };
    eprintln!(
        "    -> merge: {} sources, {} chars",
        parts.len(),
        merged.len()
    );
    Ok(Value::Text(merged))
}

fn exec_write(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let to_path = extract_string_arg("to", body);
    let content = extract_string_arg("content", body);
    let resolved_path = resolve_vars(&to_path, topic, results);
    let resolved_content = resolve_vars(&content, topic, results);
    let expanded = expand_tilde(&resolved_path);
    eprintln!("    -> write: {}", expanded);

    let path = Path::new(&expanded);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, &resolved_content).map_err(|e| format!("write failed: {}", e))?;
    Ok(Value::File(expanded))
}

fn exec_read(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = extract_string_arg("from", body);
    let path = if path.is_empty() {
        extract_first_string(body)
    } else {
        path
    };

    if path.starts_with("cache:") {
        return Err("cache miss".into());
    }

    let expanded = expand_tilde(&path);
    if !Path::new(&expanded).exists() {
        return Err(format!("file not found: {}", expanded));
    }
    let content = fs::read_to_string(&expanded).map_err(|e| format!("read failed: {}", e))?;
    Ok(Value::Text(content))
}

fn exec_run(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let cmd_raw = extract_first_string(body);
    let cmd = resolve_vars(&cmd_raw, topic, results);
    eprintln!("    -> run: {}", cmd);

    let output = Command::new("bash")
        .arg("-c")
        .arg(&cmd)
        .output()
        .map_err(|e| format!("run failed: {}", e))?;

    if !output.status.success() {
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "run failed (exit {:?}): {}",
            output.status.code(),
            &stderr_text.chars().take(300).collect::<String>()
        ));
    }

    let stdout_text = String::from_utf8_lossy(&output.stdout).to_string();

    // Parse ##DSL_RESULT block
    if let Some(kvs) = parse_dsl_result_block(&stdout_text) {
        if !kvs.is_empty() {
            eprintln!("    -> result: {} fields", kvs.len());
            return Ok(Value::Text(encode_structured_result(&kvs, &stdout_text)));
        }
    }

    // Raw stdout (truncated)
    let trimmed = if stdout_text.len() > 5000 {
        format!("{}...[truncated]", &stdout_text[..5000])
    } else {
        stdout_text
    };
    Ok(Value::Text(trimmed))
}

// ── ##DSL_RESULT protocol ──

fn parse_dsl_result_block(stdout: &str) -> Option<Vec<(String, String)>> {
    let mut in_block = false;
    let mut kvs = Vec::new();
    for line in stdout.lines() {
        if line.starts_with("##DSL_RESULT") {
            in_block = true;
            continue;
        }
        if line.starts_with("##DSL_END") {
            in_block = false;
            continue;
        }
        if in_block {
            if let Some(eq) = line.find('=') {
                let k = line[..eq].trim().to_string();
                let v = line[eq + 1..].trim().to_string();
                if !k.is_empty() && !v.is_empty() {
                    kvs.push((k, v));
                }
            }
        }
    }
    if kvs.is_empty() {
        None
    } else {
        Some(kvs)
    }
}

fn encode_structured_result(kvs: &[(String, String)], raw: &str) -> String {
    let fields: Vec<String> = kvs.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
    format!("§§FIELDS§§{}§§RAW§§{}", fields.join("§§"), raw)
}

fn extract_field(field: &str, text: &str) -> Option<String> {
    if !text.starts_with("§§FIELDS§§") {
        return None;
    }
    let rest = &text["§§FIELDS§§".len()..];
    for part in rest.split("§§") {
        if part.starts_with("RAW§§") {
            break;
        }
        if let Some(eq) = part.find('=') {
            let k = &part[..eq];
            let v = &part[eq + 1..];
            if k == field {
                return Some(v.to_string());
            }
        }
    }
    None
}

// ── Checks ──

fn run_checks(checks: &[&Check], val: &Value) -> Result<Value, String> {
    for c in checks {
        if !eval_cond(&c.cond, val) {
            return Err(c.msg.clone());
        }
    }
    Ok(val.clone())
}

fn eval_cond(cond: &str, val: &Value) -> bool {
    // Parse "result => predicate" format
    let cond = cond.trim();

    // Extract predicate name after "=>"
    let pred_name = if let Some(arrow) = cond.find("=>") {
        cond[arrow + 2..].trim()
    } else {
        cond
    };

    // Split predicate name and args
    let (name, _args_str): (&str, &str) = if let Some(open) = pred_name.find('(') {
        (
            &pred_name[..open],
            pred_name[open + 1..].trim_end_matches(')'),
        )
    } else {
        (pred_name, "")
    };

    let text = val.as_text();

    // Structured result?
    let is_structured = text.starts_with("§§FIELDS§§");
    let raw = if is_structured {
        extract_raw(text)
    } else {
        text.to_string()
    };

    match name {
        "has_results" | "has_summary" | "has_content" | "valid_output" => raw.len() > 20,
        "not_empty" => !raw.is_empty(),
        "has_items" => raw.lines().filter(|l| !l.trim().is_empty()).count() >= 2,
        "has_citations" => raw.contains("[1]") || raw.contains("[链接") || raw.contains("来源"),
        "has_date" => (2020..=2030).any(|y| raw.contains(&y.to_string())),
        "no_error" => !raw.contains("ERROR") && !raw.contains("Error") && !raw.contains("error"),
        "all_has_url" => raw.contains("http"),
        "file_exists" | "has_keywords" => raw.len() > 10,
        "has_field" if is_structured => {
            // Extract field name from args
            true // simplified
        }
        _ => true, // unknown predicate: pass
    }
}

fn extract_raw(text: &str) -> String {
    if let Some(pos) = text.find("§§RAW§§") {
        text[pos + "§§RAW§§".len()..].to_string()
    } else {
        text.to_string()
    }
}

// ── Ranking ──

fn rank_impls<'a>(
    weights: &Weights,
    recent: &BTreeMap<String, RecentRuns>,
    impls: &[&'a Impl],
    _pick_by: &str,
) -> Vec<&'a Impl> {
    let mut ranked: Vec<(f64, &'a Impl)> = impls
        .iter()
        .map(|i| {
            let base = weights.cost_total(&i.cost);
            let penalty = impl_penalty(recent, &i.name);
            (base * (1.0 + penalty), *i)
        })
        .collect();
    ranked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    ranked.into_iter().map(|(_, imp)| imp).collect()
}

fn impl_penalty(recent: &BTreeMap<String, RecentRuns>, name: &str) -> f64 {
    match recent.get(name) {
        None => 0.0,
        Some(rr) => {
            if rr.window == 0 {
                return 0.0;
            }
            if rr.consec_fail >= 3 {
                return f64::INFINITY;
            }
            if rr.window < 3 {
                return 0.0;
            }
            let failure_rate = rr.fails as f64 / rr.window as f64;
            if failure_rate <= 0.1 {
                0.0
            } else {
                (7.0 * failure_rate).exp() - 1.0
            }
        }
    }
}

// ── When condition ──

fn is_eligible(
    params: &BTreeMap<String, String>,
    results: &BTreeMap<String, Value>,
    impl_: &Impl,
) -> bool {
    match &impl_.when {
        None => true,
        Some(cond) => eval_when(params, results, cond),
    }
}

fn eval_when(
    params: &BTreeMap<String, String>,
    _results: &BTreeMap<String, Value>,
    cond: &str,
) -> bool {
    let cond = cond.trim().trim_matches('"');
    if cond.is_empty() {
        return true;
    }

    // var == "value"
    if let Some(pos) = cond.find("==") {
        let lhs = cond[..pos].trim();
        let rhs = cond[pos + 2..].trim().trim_matches('"');
        return params.get(lhs).map(|v| v == rhs).unwrap_or(false);
    }
    // var != "value"
    if let Some(pos) = cond.find("!=") {
        let lhs = cond[..pos].trim();
        let rhs = cond[pos + 2..].trim().trim_matches('"');
        return params.get(lhs).map(|v| v != rhs).unwrap_or(false);
    }
    // bare var: exists and is "true"
    params.get(cond).map(|v| v == "true").unwrap_or(false)
}

// ── Record I/O (SQLite) ──

fn append_run(
    proc_name: &str,
    impl_name: &str,
    pid: char,
    status: Status,
    _err_hash: Option<&str>,
    _err_at: Option<&str>,
) {
    append_run_rd(proc_name, impl_name, pid, status, _err_hash, _err_at, "");
}

/// RD-aware append_run: rate_tokens 从 impl 输出文本估算（len/4 ≈ token 数），
/// est_loss v0 = check 结果二值代理（Ok=0.0, Fail=1.0）。
fn append_run_rd(
    proc_name: &str,
    impl_name: &str,
    pid: char,
    status: Status,
    _err_hash: Option<&str>,
    _err_at: Option<&str>,
    output_text: &str,
) {
    let status_str = match status {
        Status::Ok => "Ok",
        Status::Fail => "Fail",
    };
    // rate 代理：输出字符数 / 4（英文 ~4 char/token 的粗估；中文偏保守）
    let rate_tokens = (output_text.chars().count() as i64) / 4;
    // loss 代理 v0：check 失败 = 全失真；通过 = 0（后续版本接入语义距离）
    let est_loss = if status == Status::Fail { 1.0 } else { 0.0 };
    db::record_run_rd(
        proc_name,
        impl_name,
        "",
        status_str,
        0,
        _err_hash,
        _err_at,
        rate_tokens,
        est_loss,
    );
    // Keep pid logging for backwards compat in stderr
    let _ = pid;
}

fn load_recent_runs(proc_name: &str) -> BTreeMap<String, RecentRuns> {
    let rows = db::recent_runs(proc_name);
    if rows.is_empty() {
        return BTreeMap::new();
    }

    // Group by impl name, compute sliding window
    let mut by_impl: BTreeMap<String, Vec<bool>> = BTreeMap::new();
    for row in &rows {
        let ok = row.status == "Ok";
        by_impl.entry(row.impl_name.clone()).or_default().push(ok);
    }

    let mut result = BTreeMap::new();
    for (name, all_runs) in &by_impl {
        let recent = if all_runs.len() > WINDOW_SIZE {
            &all_runs[all_runs.len() - WINDOW_SIZE..]
        } else {
            &all_runs[..]
        };
        let fails = recent.iter().filter(|&&ok| !ok).count();
        let consec = recent.iter().rev().take_while(|&&ok| !ok).count();
        result.insert(
            name.clone(),
            RecentRuns {
                window: recent.len(),
                fails,
                consec_fail: consec,
            },
        );
    }
    result
}

// ── Helpers ──

fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".into());
        format!("{}/{}", home, &path[2..])
    } else {
        path.to_string()
    }
}

fn short_hash(s: &str) -> String {
    let mut hash: u64 = 5381;
    for c in s.chars() {
        hash = ((hash << 5).wrapping_add(hash)) ^ (c as u64);
    }
    format!("{:08x}", hash)
}

fn find_bridge(script: &str) -> String {
    // Search order: current dir, ~/.local/share/ductile/bridge/, project-local
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let candidates = [
        format!("./{}", script),
        format!("./bridge/{}", script),
        format!("{}/.local/share/ductile/bridge/{}", home, script),
        format!("/usr/local/share/ductile/bridge/{}", script),
    ];
    for c in &candidates {
        if Path::new(c).exists() {
            return c.clone();
        }
    }
    // Fallback: return the home path (will fail with a clear error)
    format!("{}/.local/share/ductile/bridge/{}", home, script)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── detect_func ──
    #[test]
    fn detect_func_known() {
        assert_eq!(detect_func("read(\"file.txt\")"), "read");
        assert_eq!(detect_func("web_search(query=\"AI\")"), "web_search");
        assert_eq!(detect_func("write(to=\"out\")"), "write");
        assert_eq!(detect_func("merge(@a, @b)"), "merge");
        assert_eq!(detect_func("llm(input=\"x\")"), "llm");
        assert_eq!(detect_func("sh(\"echo hi\")"), "sh");
        assert_eq!(detect_func("run(\"script\")"), "run");
    }

    #[test]
    fn detect_func_unknown() {
        assert_eq!(detect_func("noop"), "");
        assert_eq!(detect_func(""), "");
        assert_eq!(detect_func("123"), "");
    }

    // ── resolve_vars ──
    #[test]
    fn resolve_topic_var() {
        let results = BTreeMap::new();
        let out = resolve_vars("search({topic})", "AI", &results);
        assert_eq!(out, "search(AI)");
    }

    #[test]
    fn resolve_proc_ref() {
        let mut results = BTreeMap::new();
        results.insert("source".into(), Value::Text("result data".into()));
        let out = resolve_vars("process(@source)", "AI", &results);
        assert_eq!(out, "process(result data)");
    }

    #[test]
    fn resolve_unresolved_ref_left_as_is() {
        let results = BTreeMap::new();
        let out = resolve_vars("@nonexistent", "AI", &results);
        assert_eq!(out, "@nonexistent");
    }

    #[test]
    fn resolve_hash_topic() {
        let results = BTreeMap::new();
        let out = resolve_vars("{hash(topic)}", "test", &results);
        // Should be replaced with a hash, not the original {hash(topic)}
        assert_ne!(out, "{hash(topic)}");
        assert!(!out.is_empty());
    }

    // ── extract_string_arg ──
    #[test]
    fn extract_string_arg_quoted() {
        assert_eq!(extract_string_arg("query", r#"query="AI news""#), "AI news");
        assert_eq!(
            extract_string_arg("to", r#"to="/tmp/out.txt""#),
            "/tmp/out.txt"
        );
    }

    #[test]
    fn extract_string_arg_unquoted() {
        assert_eq!(extract_string_arg("n", "n=5"), "5");
    }

    #[test]
    fn extract_string_arg_missing() {
        assert_eq!(extract_string_arg("missing", "query=\"AI\""), "");
    }

    #[test]
    fn extract_string_arg_no_partial_match() {
        // "content" should not match partial "cont"
        assert_eq!(extract_string_arg("cont", r#"content="hello""#), "");
    }

    // ── short_hash ──
    #[test]
    fn short_hash_deterministic() {
        let h1 = short_hash("test");
        let h2 = short_hash("test");
        assert_eq!(h1, h2);
        assert!(h1.len() >= 8); // at least 8 hex chars (djb2 can overflow 8)
    }

    #[test]
    fn short_hash_different_inputs() {
        assert_ne!(short_hash("a"), short_hash("b"));
    }

    // ── expand_tilde ──
    #[test]
    fn expand_tilde_with_slash() {
        let expanded = expand_tilde("~/test");
        assert!(!expanded.contains('~'));
        assert!(expanded.ends_with("/test"));
    }

    #[test]
    fn expand_tilde_no_tilde() {
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
    }

    // ── eval_cond (check predicates) ──
    #[test]
    fn eval_cond_has_results_long() {
        let val = Value::Text("This is a long enough result text!".into());
        assert!(eval_cond("result => has_results", &val));
    }

    #[test]
    fn eval_cond_has_results_short() {
        let val = Value::Text("short".into());
        assert!(!eval_cond("result => has_results", &val));
    }

    #[test]
    fn eval_cond_not_empty() {
        assert!(eval_cond("result => not_empty", &Value::Text("x".into())));
        assert!(!eval_cond("result => not_empty", &Value::Text("".into())));
    }

    #[test]
    fn eval_cond_has_items() {
        let val = Value::Text("line1\nline2\nline3".into());
        assert!(eval_cond("result => has_items", &val));
    }

    #[test]
    fn eval_cond_has_items_too_few() {
        let val = Value::Text("only one".into());
        assert!(!eval_cond("result => has_items", &val));
    }

    #[test]
    fn eval_cond_has_date() {
        let val = Value::Text("Published in 2024".into());
        assert!(eval_cond("result => has_date", &val));
    }

    #[test]
    fn eval_cond_no_error() {
        assert!(eval_cond(
            "result => no_error",
            &Value::Text("all good".into())
        ));
        assert!(!eval_cond(
            "result => no_error",
            &Value::Text("ERROR occurred".into())
        ));
    }

    #[test]
    fn eval_cond_unknown_predicate_passes() {
        assert!(eval_cond(
            "result => unknown_pred",
            &Value::Text("x".into())
        ));
    }

    // ── eval_when ──
    #[test]
    fn eval_when_eq_true() {
        let mut params = BTreeMap::new();
        params.insert("mode".into(), "deep".into());
        let results = BTreeMap::new();
        assert!(eval_when(&params, &results, "mode == \"deep\""));
    }

    #[test]
    fn eval_when_eq_false() {
        let mut params = BTreeMap::new();
        params.insert("mode".into(), "shallow".into());
        let results = BTreeMap::new();
        assert!(!eval_when(&params, &results, "mode == \"deep\""));
    }

    #[test]
    fn eval_when_neq() {
        let mut params = BTreeMap::new();
        params.insert("mode".into(), "shallow".into());
        let results = BTreeMap::new();
        assert!(eval_when(&params, &results, "mode != \"deep\""));
    }

    #[test]
    fn eval_when_missing_var() {
        let params = BTreeMap::new();
        let results = BTreeMap::new();
        assert!(!eval_when(&params, &results, "mode == \"deep\""));
    }

    #[test]
    fn eval_when_empty_passes() {
        let params = BTreeMap::new();
        let results = BTreeMap::new();
        assert!(eval_when(&params, &results, ""));
    }

    // ── parse_dsl_result_block ──
    #[test]
    fn parse_dsl_result_basic() {
        let stdout = "some output\n##DSL_RESULT\nstatus=ok\ncount=5\n##DSL_END\nmore output";
        let kvs = parse_dsl_result_block(stdout).unwrap();
        assert_eq!(kvs.len(), 2);
        assert_eq!(kvs[0], ("status".into(), "ok".into()));
        assert_eq!(kvs[1], ("count".into(), "5".into()));
    }

    #[test]
    fn parse_dsl_result_empty_block() {
        let stdout = "##DSL_RESULT\n##DSL_END";
        assert!(parse_dsl_result_block(stdout).is_none());
    }

    #[test]
    fn parse_dsl_result_no_block() {
        assert!(parse_dsl_result_block("just regular output").is_none());
    }

    // ── encode/extract structured result ──
    #[test]
    fn structured_result_roundtrip() {
        let kvs = vec![
            ("status".into(), "ok".into()),
            ("count".into(), "42".into()),
        ];
        let encoded = encode_structured_result(&kvs, "raw output here");
        assert!(encoded.starts_with("§§FIELDS§§"));
        assert!(encoded.contains("status=ok"));
        assert!(encoded.contains("§§RAW§§raw output here"));
    }

    #[test]
    fn extract_field_from_structured() {
        let kvs = vec![("status".into(), "ok".into())];
        let encoded = encode_structured_result(&kvs, "raw");
        let field = extract_field("status", &encoded);
        assert_eq!(field, Some("ok".into()));
    }

    #[test]
    fn extract_field_missing() {
        let kvs = vec![("status".into(), "ok".into())];
        let encoded = encode_structured_result(&kvs, "raw");
        assert!(extract_field("missing", &encoded).is_none());
    }

    #[test]
    fn extract_field_from_unstructured() {
        assert!(extract_field("x", "plain text").is_none());
    }

    // ── impl_penalty ──
    #[test]
    fn impl_penalty_no_history() {
        let recent = BTreeMap::new();
        assert_eq!(impl_penalty(&recent, "a"), 0.0);
    }

    #[test]
    fn impl_penalty_low_failure_rate() {
        let mut recent = BTreeMap::new();
        recent.insert(
            "a".into(),
            RecentRuns {
                window: 10,
                fails: 1,
                consec_fail: 0,
            },
        );
        assert_eq!(impl_penalty(&recent, "a"), 0.0); // 0.1 rate = no penalty
    }

    #[test]
    fn impl_penalty_consec_3_failures() {
        let mut recent = BTreeMap::new();
        recent.insert(
            "a".into(),
            RecentRuns {
                window: 5,
                fails: 3,
                consec_fail: 3,
            },
        );
        assert!(impl_penalty(&recent, "a").is_infinite());
    }

    #[test]
    fn impl_penalty_high_failure() {
        let mut recent = BTreeMap::new();
        recent.insert(
            "a".into(),
            RecentRuns {
                window: 10,
                fails: 8,
                consec_fail: 0,
            },
        );
        let p = impl_penalty(&recent, "a");
        assert!(p > 0.0);
    }

    // ── rank_impls ──
    #[test]
    fn rank_impls_cheapest_first() {
        let weights = Weights::default();
        let recent = BTreeMap::new();
        let a = Impl {
            name: "expensive".into(),
            cost: Cost {
                latency: 1000,
                risk: 0.5,
                tokens: 0,
                money: 0.0,
            },
            ..default_impl()
        };
        let b = Impl {
            name: "cheap".into(),
            cost: Cost {
                latency: 10,
                risk: 0.0,
                tokens: 0,
                money: 0.0,
            },
            ..default_impl()
        };
        let impls = vec![&a, &b];
        let ranked = rank_impls(&weights, &recent, &impls, "cost");
        assert_eq!(ranked[0].name, "cheap");
        assert_eq!(ranked[1].name, "expensive");
    }

    #[test]
    fn rank_impls_penalty_affects_order() {
        let weights = Weights::default();
        let mut recent = BTreeMap::new();
        // Penalize "cheap" heavily
        recent.insert(
            "cheap".into(),
            RecentRuns {
                window: 10,
                fails: 8,
                consec_fail: 0,
            },
        );
        let a = Impl {
            name: "cheap".into(),
            cost: Cost {
                latency: 10,
                risk: 0.0,
                tokens: 0,
                money: 0.0,
            },
            ..default_impl()
        };
        let b = Impl {
            name: "expensive".into(),
            cost: Cost {
                latency: 100,
                risk: 0.0,
                tokens: 0,
                money: 0.0,
            },
            ..default_impl()
        };
        let impls = vec![&a, &b];
        let ranked = rank_impls(&weights, &recent, &impls, "cost");
        // With penalty, expensive should come first
        assert_eq!(ranked[0].name, "expensive");
    }

    fn default_impl() -> Impl {
        Impl {
            name: String::new(),
            tags: std::collections::BTreeSet::new(),
            cost: Cost::default(),
            enabled: true,
            when: None,
            refs: vec![],
            body_text: String::new(),
            stub: false,
            retry: 0,
            ensure: vec![],
            description: String::new(),
        }
    }

    // ── exec_read with missing file ──
    #[test]
    fn exec_read_missing_file() {
        let impl_ = Impl {
            body_text: r#"read(from="/nonexistent/ductile_test_file.txt")"#.into(),
            ..default_impl()
        };
        let result = exec_read(&impl_, &impl_.body_text);
        assert!(result.is_err());
    }

    #[test]
    fn exec_read_cache_miss() {
        let impl_ = Impl {
            body_text: r#"read(from="cache:abc")"#.into(),
            ..default_impl()
        };
        let result = exec_read(&impl_, &impl_.body_text);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "cache miss");
    }

    // ── exec_write + exec_read roundtrip ──
    #[test]
    fn exec_write_then_read() {
        let path = "/tmp/ductile_test_roundtrip.txt";
        let _ = std::fs::remove_file(path);

        // Write
        let write_impl = Impl {
            body_text: format!(r#"write(to="{}", content="hello ductile")"#, path),
            ..default_impl()
        };
        let results = BTreeMap::new();
        let w = exec_write(&write_impl, "test", &write_impl.body_text, &results);
        assert!(w.is_ok());

        // Read back
        let read_impl = Impl {
            body_text: format!(r#"read(from="{}")"#, path),
            ..default_impl()
        };
        let r = exec_read(&read_impl, &read_impl.body_text);
        assert!(r.is_ok());
        match r.unwrap() {
            Value::Text(t) => assert_eq!(t, "hello ductile"),
            _ => panic!("expected Text"),
        }

        let _ = std::fs::remove_file(path);
    }

    // ── exec_merge ──
    #[test]
    fn exec_merge_basic() {
        let mut results = BTreeMap::new();
        results.insert("a".into(), Value::Text("data_a".into()));
        results.insert("b".into(), Value::Text("data_b".into()));
        let impl_ = Impl {
            refs: vec!["a".into(), "b".into()],
            body_text: "merge(@a, @b)".into(),
            ..default_impl()
        };
        let result = exec_merge(&impl_, &impl_.body_text, &results);
        assert!(result.is_ok());
        let text = result.unwrap().as_text().to_string();
        assert!(text.contains("data_a"));
        assert!(text.contains("data_b"));
    }

    #[test]
    fn exec_merge_dedup() {
        let mut results = BTreeMap::new();
        results.insert("a".into(), Value::Text("same".into()));
        results.insert("b".into(), Value::Text("same".into()));
        let impl_ = Impl {
            refs: vec!["a".into(), "b".into()],
            body_text: "merge(@a, @b, dedup)".into(),
            ..default_impl()
        };
        let result = exec_merge(&impl_, &impl_.body_text, &results);
        let text = result.unwrap().as_text().to_string();
        // With dedup, should not contain duplicate
        assert_eq!(text.matches("same").count(), 1);
    }

    // ── exec_pipeline with stub ──
    #[test]
    fn exec_pipeline_stub_proc() {
        let pl = Pipeline {
            name: "stub_test".into(),
            weights: Weights::default(),
            description: String::new(),
            procs: vec![Proc {
                name: "p".into(),
                plan: vec![Impl {
                    name: "stub_impl".into(),
                    tags: std::collections::BTreeSet::new(),
                    cost: Cost::default(),
                    enabled: true,
                    when: None,
                    refs: vec![],
                    body_text: "read(\"x\")".into(),
                    stub: true,
                    retry: 0,
                    ensure: vec![],
                    description: String::new(),
                }],
                checks: vec![],
                deliver: false,
                foreach: None,
                foreach_var: String::new(),
                pick_by: "cost".into(),
                description: String::new(),
            }],
        };
        let params = BTreeMap::new();
        let result = exec_pipeline("test", &params, &pl);
        match result {
            ExecResult::Success(map) => {
                assert!(map.contains_key("p"));
                assert!(map["p"].as_text().contains("STUB"));
            }
            ExecResult::Failed(e) => panic!("stub should succeed: {}", e),
        }
    }
}
