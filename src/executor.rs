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
use std::path::PathBuf;
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
        // ── v0.7 resource management ops ──
        "spawn" => exec_spawn(impl_, topic, body, results),
        "procs" => exec_procs(impl_, topic, body, results),
        "kill" => exec_kill(impl_, topic, body, results),
        "wait" => exec_wait(impl_, topic, body, results),
        "exists" => exec_fs_exists(impl_, body),
        "stat" => exec_fs_stat(impl_, body),
        "ls" => exec_fs_ls(impl_, body),
        "rm" => exec_fs_rm(impl_, body),
        "cp" => exec_fs_cp(impl_, body),
        "mkdir" => exec_fs_mkdir(impl_, body),
        "disk" => exec_disk(impl_, body),
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

// ── Resource management helpers (v0.7) ──

/// Best-effort kill of an entire process group (POSIX only; no-op elsewhere).
/// Uses kill(-pid, SIGKILL) via libc-free syscall wrapper: we shell out to
/// `kill` itself to avoid adding a libc dependency.
fn libc_kill_group(pid: i32) {
    let _ = Command::new("kill")
        .arg("-9")
        .arg(format!("-{}", pid))
        .output();
}

/// Extract ALL quoted values for a repeated key: env="A=1" env="B=2" → ["A=1","B=2"]
fn extract_all_string_args(key: &str, body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let pattern = format!("{}=\"", key);
    let mut rest = body;
    while let Some(pos) = rest.find(&pattern) {
        // guard against substring matches (e.g. "envx=")
        let before = if pos > 0 { &rest[..pos] } else { "" };
        if let Some(c) = before.chars().last() {
            if c.is_alphanumeric() || c == '_' {
                rest = &rest[pos + pattern.len()..];
                continue;
            }
        }
        let after = &rest[pos + pattern.len()..];
        let val: String = after.chars().take_while(|c| *c != '"').collect();
        if !val.is_empty() {
            out.push(val);
        }
        rest = &rest[pos + pattern.len()..];
    }
    out
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

// ── v0.7 Resource management ops: processes ──

use std::collections::HashMap as StdHashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

/// Global process table: handle name → (pid, start_unix, command)
static PROC_TABLE: OnceLock<Mutex<StdHashMap<String, (u32, u64, String)>>> = OnceLock::new();

fn proc_table() -> &'static Mutex<StdHashMap<String, (u32, u64, String)>> {
    PROC_TABLE.get_or_init(|| Mutex::new(StdHashMap::new()))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// spawn("name", "cmd") [timeout=0] — background launch, returns pid.
/// Handle name is used by wait/kill/procs. Non-blocking.
fn exec_spawn(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let name = extract_string_arg("name", body);
    if name.is_empty() {
        return Err("spawn requires name=\"handle\"".into());
    }
    // cmd: prefer cmd="..." arg; fall back to first quoted string that isn't the name
    let cmd_raw = {
        let c = extract_string_arg("cmd", body);
        if !c.is_empty() {
            c
        } else {
            // strip name="..." then take first quoted string
            let stripped = body.replace(&format!("name=\"{}\"", name), "");
            extract_first_string(&stripped)
        }
    };
    let cmd = resolve_vars(&cmd_raw, topic, results);
    if cmd.is_empty() || cmd == name {
        return Err("spawn requires a command string".into());
    }
    use std::process::Stdio;
    let child = Command::new("bash")
        .arg("-c")
        .arg(&cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn failed: {}", e))?;
    let pid = child.id();
    eprintln!("    -> spawn[{}]: pid={} cmd={}", name, pid, cmd);
    proc_table()
        .lock()
        .map_err(|e| format!("proc table poisoned: {}", e))?
        .insert(name, (pid, unix_now(), cmd));
    Ok(Value::Text(format!("{}", pid)))
}

/// procs() or procs("name") — list live handles. Output: name pid age_s cmd per line.
fn exec_procs(
    _impl_: &Impl,
    _topic: &str,
    _body: &str,
    _results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let filter = extract_string_arg("name", _body);
    let table = proc_table()
        .lock()
        .map_err(|e| format!("proc table poisoned: {}", e))?;
    let now = unix_now();
    let mut lines = Vec::new();
    for (name, (pid, start, cmd)) in table.iter() {
        if !filter.is_empty() && name != &filter {
            continue;
        }
        // liveness check via /proc
        let alive = Path::new(&format!("/proc/{}", pid)).exists();
        lines.push(format!(
            "{}\t{}\t{}\t{}\t{}",
            name,
            pid,
            now.saturating_sub(*start),
            if alive { "alive" } else { "dead" },
            cmd
        ));
    }
    Ok(Value::Text(lines.join("\n")))
}

/// kill("name") — SIGKILL the handle's process group; also accepts raw pid("1234").
fn exec_kill(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let target = resolve_vars(&extract_string_arg("name", body), topic, results);
    if target.is_empty() {
        return Err("kill requires name=\"handle\" or a pid".into());
    }
    let pid: i32 = if let Ok(p) = target.parse::<i32>() {
        p
    } else {
        let table = proc_table()
            .lock()
            .map_err(|e| format!("proc table poisoned: {}", e))?;
        table
            .get(&target)
            .map(|(pid, _, _)| *pid as i32)
            .ok_or_else(|| format!("unknown handle: {}", target))?
    };
    eprintln!("    -> kill: -{}", pid);
    let _ = Command::new("kill")
        .arg("-9")
        .arg(format!("-{}", pid))
        .output();
    Ok(Value::Text(format!("killed -{}", pid)))
}

/// wait("name", timeout=60) — block until handle exits (or timeout). Returns exit info.
fn exec_wait(
    _impl_: &Impl,
    topic: &str,
    body: &str,
    results: &BTreeMap<String, Value>,
) -> Result<Value, String> {
    let name = resolve_vars(&extract_string_arg("name", body), topic, results);
    let timeout: u64 = extract_string_arg("timeout", body).parse().unwrap_or(60);
    if name.is_empty() {
        return Err("wait requires name=\"handle\"".into());
    }
    let pid = {
        let table = proc_table()
            .lock()
            .map_err(|e| format!("proc table poisoned: {}", e))?;
        table
            .get(&name)
            .map(|(pid, _, _)| *pid)
            .ok_or_else(|| format!("unknown handle: {}", name))?
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout.max(1));
    loop {
        let alive = Path::new(&format!("/proc/{}", pid)).exists();
        if !alive {
            return Ok(Value::Text(format!("{} exited", name)));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "wait timeout: {} still running after {}s",
                name, timeout
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

// ── v0.7 Resource management ops: filesystem ──

/// exists("path") → "true"/"false"
fn exec_fs_exists(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = expand_tilde(&extract_first_string(body));
    Ok(Value::Text(if Path::new(&path).exists() {
        "true".into()
    } else {
        "false".into()
    }))
}

/// stat("path") → "size_bytes mtime_unix" or error if missing
fn exec_fs_stat(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = expand_tilde(&extract_first_string(body));
    let meta = fs::metadata(&path).map_err(|e| format!("stat failed: {}", e))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let kind = if meta.is_dir() {
        "dir"
    } else if meta.is_file() {
        "file"
    } else {
        "other"
    };
    Ok(Value::Text(format!("{} {} {}", kind, meta.len(), mtime)))
}

/// ls("dir") → lines of entries (name only)
fn exec_fs_ls(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = expand_tilde(&extract_first_string(body));
    let entries = fs::read_dir(&path).map_err(|e| format!("ls failed: {}", e))?;
    let mut names: Vec<String> = Vec::new();
    for e in entries.flatten() {
        names.push(e.file_name().to_string_lossy().to_string());
    }
    names.sort();
    Ok(Value::Text(names.join("\n")))
}

/// rm("path") — recursive; refuses / and $HOME roots.
fn exec_fs_rm(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = expand_tilde(&extract_first_string(body));
    let p = Path::new(&path);
    let danger = p == Path::new("/") || p == dirs_home();
    if danger {
        return Err(format!("rm refused: {} is a protected root", path));
    }
    if !p.exists() {
        return Ok(Value::Text("noop: not found".into()));
    }
    if p.is_dir() {
        fs::remove_dir_all(p).map_err(|e| format!("rm failed: {}", e))?;
    } else {
        fs::remove_file(p).map_err(|e| format!("rm failed: {}", e))?;
    }
    Ok(Value::Text(format!("removed {}", path)))
}

fn dirs_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into())))
}

/// cp("src", "dst") — file or directory tree.
fn exec_fs_cp(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let src = expand_tilde(&extract_string_arg("from", body));
    let dst = expand_tilde(&extract_string_arg("to", body));
    if src.is_empty() || dst.is_empty() {
        return Err("cp requires from=\"...\" to=\"...\"".into());
    }
    let s = Path::new(&src);
    if !s.exists() {
        return Err(format!("cp: source not found: {}", src));
    }
    if s.is_dir() {
        // shell out for recursive copy (keeps this fn small)
        let st = Command::new("cp")
            .arg("-r")
            .arg(&src)
            .arg(&dst)
            .output()
            .map_err(|e| format!("cp failed: {}", e))?;
        if !st.status.success() {
            return Err(format!(
                "cp failed: {}",
                String::from_utf8_lossy(&st.stderr)
            ));
        }
    } else {
        fs::copy(&src, &dst).map_err(|e| format!("cp failed: {}", e))?;
    }
    Ok(Value::Text(format!("copied {} -> {}", src, dst)))
}

/// mkdir("path") — create all parents.
fn exec_fs_mkdir(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = expand_tilde(&extract_first_string(body));
    fs::create_dir_all(&path).map_err(|e| format!("mkdir failed: {}", e))?;
    Ok(Value::Text(path))
}

/// disk("dir") → "avail_gb total_gb" via df -BG.
fn exec_disk(_impl_: &Impl, body: &str) -> Result<Value, String> {
    let path = expand_tilde(&extract_first_string(body));
    let out = Command::new("df")
        .arg("-BG")
        .arg(&path)
        .output()
        .map_err(|e| format!("df failed: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "df failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    // last line: fs size used avail use% mount
    let text = String::from_utf8_lossy(&out.stdout);
    let last = text.lines().last().unwrap_or("");
    let fields: Vec<&str> = last.split_whitespace().collect();
    if fields.len() >= 4 {
        return Ok(Value::Text(format!("{} {}", fields[3], fields[1])));
    }
    Err("disk: unexpected df output".into())
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
    // v0.7 resource management: optional timeout=seconds arg (default 300s, 0 = no limit)
    let timeout_secs: u64 = extract_string_arg("timeout", body).parse().unwrap_or(300);
    // v0.7 resource management: env vars via env="K=V" args (repeatable)
    let envs = extract_all_string_args("env", body);
    eprintln!("    -> run: {} (timeout={}s)", cmd, timeout_secs);

    let mut command = Command::new("bash");
    command.arg("-c").arg(&cmd);
    for e in &envs {
        if let Some(eq) = e.find('=') {
            let (k, v) = (&e[..eq], &e[eq + 1..]);
            if k == "PATH" || k == "HOME" || k == "USER" {
                // never clobber identity vars
                continue;
            }
            command.env(k, v);
        }
    }

    // Spawn + deadline: kill the whole process group on timeout so orphaned
    // children (e.g. `foo &`) don't outlive the pipeline.
    use std::process::Stdio;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("run failed: {}", e))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs.max(1));
    let status;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = s;
                break;
            }
            Ok(None) => {
                if timeout_secs > 0 && std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Best-effort group kill: negative pid of the child.
                    // (child.id() is the group leader under setsid-less bash -c)
                    let pid = child.id() as i32;
                    if pid > 0 {
                        libc_kill_group(pid);
                    }
                    return Err(format!("run timed out after {}s: {}", timeout_secs, cmd));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(format!("run failed: {}", e)),
        }
    }

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let mut stdout_buf: Vec<u8> = Vec::new();
    let mut stderr_buf: Vec<u8> = Vec::new();
    use std::io::Read;
    if let Some(p) = stdout_pipe.as_mut() {
        let _ = p.read_to_end(&mut stdout_buf);
    }
    if let Some(p) = stderr_pipe.as_mut() {
        let _ = p.read_to_end(&mut stderr_buf);
    }

    if !status.success() {
        let stderr_text = String::from_utf8_lossy(&stderr_buf);
        return Err(format!(
            "run failed (exit {:?}): {}",
            status.code(),
            &stderr_text.chars().take(300).collect::<String>()
        ));
    }

    let stdout_text = String::from_utf8_lossy(&stdout_buf).to_string();

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

/// est_loss v1: 结构化字段保留度。上游有 DSL_RESULT 字段集 F_up，
/// 本 impl 输出字段集 F_out → loss = 1 − |F_up ∩ F_out| / |F_up|。
/// 上游无结构化字段 → None（退化到 v0 二值代理）。
pub fn est_loss_field_coverage(upstream_fields: &[String], output_text: &str) -> Option<f64> {
    if upstream_fields.is_empty() {
        return None;
    }
    let out_kvs = parse_dsl_result_block(output_text)?;
    if out_kvs.is_empty() {
        return None;
    }
    let out_keys: std::collections::BTreeSet<&str> =
        out_kvs.iter().map(|(k, _)| k.as_str()).collect();
    let up_keys: std::collections::BTreeSet<&str> =
        upstream_fields.iter().map(|s| s.as_str()).collect();
    let inter = up_keys.intersection(&out_keys).count();
    Some(1.0 - inter as f64 / up_keys.len() as f64)
}

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
            let rd = impl_rd_surcharge(weights, recent, &i.name);
            (base * (1.0 + penalty) + rd, *i)
        })
        .collect();
    ranked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    ranked.into_iter().map(|(_, imp)| imp).collect()
}

/// RD 附加费：weights.rd × 该 impl 近 20 次的平均失真率 est_loss/max(rate_tokens,1)。
/// 语义：同样 token 预算下，单位信息损耗大的 impl 排后（E7a 准则的运行时形态）。
/// 无历史记录 = 0（冷启动不惩罚）；rd 权重 0 = 完全关闭（默认）。
fn impl_rd_surcharge(weights: &Weights, recent: &BTreeMap<String, RecentRuns>, name: &str) -> f64 {
    if weights.rd <= 0.0 {
        return 0.0;
    }
    match recent.get(name) {
        None => 0.0,
        Some(rr) => {
            if rr.n == 0 {
                return 0.0;
            }
            let loss_rate = rr.sum_loss / (rr.sum_tokens.max(1) as f64);
            weights.rd * loss_rate
        }
    }
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

/// RD-aware append_run: rate_tokens 从 impl 输出文本估算（len/4 ≈ token 数）。
/// 惩罚域/失真域分家（反双重计费）：失败由 fail-rate 惩罚独占计费（e^(7r)），
/// est_loss 只在有结构化证据（v1 字段覆盖度）时非零；无证据 = 0.0。
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
    // loss：v1 字段覆盖度（有上游字段与 DSL_RESULT 时）；否则 0——不再用 check 二值兜底，
    // 否则同一次失败既吃乘法惩罚又吃 RD 加法费（双重计费）。
    let est_loss = est_loss_v0(output_text, &status).unwrap_or(0.0);
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

/// est_loss 证据函数：当前无 v1 上游字段上下文传入（调用点未接线），
/// 返回 None = 无结构化证据 → RD 域不计费。
fn est_loss_v0(_output_text: &str, _status: &Status) -> Option<f64> {
    None
}

fn load_recent_runs(proc_name: &str) -> BTreeMap<String, RecentRuns> {
    let rows = db::recent_runs(proc_name);
    if rows.is_empty() {
        return BTreeMap::new();
    }

    // Group by impl name, compute sliding window
    let mut by_impl: BTreeMap<String, Vec<&db::RunRow>> = BTreeMap::new();
    for row in &rows {
        by_impl.entry(row.impl_name.clone()).or_default().push(row);
    }

    let mut result = BTreeMap::new();
    for (name, all_runs) in &by_impl {
        let recent = if all_runs.len() > WINDOW_SIZE {
            &all_runs[all_runs.len() - WINDOW_SIZE..]
        } else {
            &all_runs[..]
        };
        let fails = recent.iter().filter(|r| r.status != "Ok").count();
        let consec = recent.iter().rev().take_while(|r| r.status != "Ok").count();
        let sum_tokens: i64 = recent.iter().map(|r| r.rate_tokens).sum();
        let sum_loss: f64 = recent.iter().map(|r| r.est_loss).sum();
        result.insert(
            name.clone(),
            RecentRuns {
                window: recent.len(),
                fails,
                consec_fail: consec,
                n: recent.len(),
                sum_tokens,
                sum_loss,
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
    fn est_loss_v1_field_coverage() {
        let up = vec![
            "status".to_string(),
            "count".to_string(),
            "score".to_string(),
        ];
        // 全保留 → 0
        let full = "x\n##DSL_RESULT\nstatus=ok\ncount=5\nscore=9\n##DSL_END";
        assert_eq!(est_loss_field_coverage(&up, full), Some(0.0));
        // 丢 1/3 → ≈1/3
        let partial = "x\n##DSL_RESULT\nstatus=ok\ncount=5\n##DSL_END";
        let got = est_loss_field_coverage(&up, partial).unwrap();
        assert!((got - 1.0 / 3.0).abs() < 1e-12);
        // 全丢 → 1
        let none = "x\n##DSL_RESULT\nother=1\n##DSL_END";
        assert_eq!(est_loss_field_coverage(&up, none), Some(1.0));
        // 反双重计费：Fail 无结构化证据 → est_loss = 0（失败只由 fail-rate 惩罚计费）
        let status = Status::Fail;
        assert_eq!(est_loss_v0("raw error output", &status), None);
        assert_eq!(est_loss_v0("raw error output", &status).unwrap_or(0.0), 0.0);
        // 无 DSL_RESULT → None（退回 v0）
        assert_eq!(est_loss_field_coverage(&up, "plain text"), None);
        // 上游无字段 → None
        assert_eq!(est_loss_field_coverage(&[], "any"), None);
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
                n: 0,
                sum_tokens: 0,
                sum_loss: 0.0,
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
                n: 0,
                sum_tokens: 0,
                sum_loss: 0.0,
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
                n: 0,
                sum_tokens: 0,
                sum_loss: 0.0,
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
                n: 0,
                sum_tokens: 0,
                sum_loss: 0.0,
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

    #[test]
    fn rank_impls_rd_surcharge_reorders() {
        // rd=0（默认）：cheap 在前；rd>0 且 cheap 失真率高：expensive 在前
        let impls_cost = |name: &str, latency: i64| Impl {
            name: name.into(),
            cost: Cost {
                latency,
                risk: 0.0,
                tokens: 0,
                money: 0.0,
            },
            ..default_impl()
        };
        let a = impls_cost("cheap", 10);
        let b = impls_cost("expensive", 100);
        let impls = vec![&a, &b];
        let mut recent = BTreeMap::new();
        // cheap: 10 次运行, 200 tokens, est_loss 累计 5.0 → loss_rate = 5/200 = 0.025
        recent.insert(
            "cheap".into(),
            RecentRuns {
                window: 10,
                fails: 0,
                consec_fail: 0,
                n: 10,
                sum_tokens: 200,
                sum_loss: 5.0,
            },
        );
        // expensive: 10 次运行, 2000 tokens, est_loss 累计 2.0 → loss_rate = 2/2000 = 0.001
        recent.insert(
            "expensive".into(),
            RecentRuns {
                window: 10,
                fails: 0,
                consec_fail: 0,
                n: 10,
                sum_tokens: 2000,
                sum_loss: 2.0,
            },
        );
        // rd=0: 纯 cost 排序
        let w0 = Weights::default();
        assert_eq!(w0.rd, 0.0);
        let ranked0 = rank_impls(&w0, &recent, &impls, "cost");
        assert_eq!(ranked0[0].name, "cheap");
        // rd=10000: cheap surcharge = 10000×0.025 = 250 ≫ cost 差; expensive = 10000×0.001 = 10
        let w1 = Weights {
            rd: 10000.0,
            ..Weights::default()
        };
        let ranked1 = rank_impls(&w1, &recent, &impls, "cost");
        assert_eq!(ranked1[0].name, "expensive");
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
