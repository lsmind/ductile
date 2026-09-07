//! Executor — runs a parsed Pipeline.
//!
//! Handles: cost-based impl selection, fallback, checks, foreach expansion,
//! retry with exponential backoff, variable resolution, shell command execution,
//! ##DSL_RESULT protocol, record (GCF) logging.

use crate::ast::*;
use crate::db;
use crate::egraph;
use crate::errflow;
use crate::ranking::rank_impls_named;
pub use crate::ranking::ImplPrefs;
pub use crate::steps::exec_script_call;
pub use crate::steps::known_functions;
use crate::steps::{is_probe_stub, step_registry};
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::time::Instant;

pub use crate::dslresult::{
    encode_structured_result, est_loss_field_coverage, extract_field, parse_dsl_result_block,
};
pub use crate::textargs::{
    detect_func, expand_tilde, extract_all_string_args, extract_first_string, extract_string_arg,
    resolve_vars, short_hash,
};

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

/// v0.11: policy 挂载点。None = 引擎默认评价（与 v0.10 行为一致）。
pub fn exec_pipeline(
    topic: &str,
    params: &BTreeMap<String, String>,
    pl: &Pipeline,
    policy: Option<&Policy>,
) -> ExecResult {
    // Apply hot patches: clone pipeline, override fields from SQLite.
    // v0.11.1 Null Object：--policy 缺席不再走 Option 分支，统一为 Evaluator 多态调用。
    let mut pl = apply_patches(pl);
    crate::eval::evaluator(policy).apply(&mut pl, topic);

    // v0.10: e-graph 提取模式（.pick(egraph) 或 DUCTILE_EGRAPH=1）。
    // 两层分工：e-graph 决定"谁跑"（class 代表/序/别名），
    // exec_proc 内部仍按历史排序决定"怎么跑"（impl 序 + retry）。
    let egraph_mode = pl.procs.iter().any(|p| p.pick_by == "egraph")
        || std::env::var("DUCTILE_EGRAPH")
            .map(|v| v == "1")
            .unwrap_or(false);

    if egraph_mode {
        let mut eg = egraph::build_egraph(&pl);
        let plan = egraph::extract_plan(&pl, &eg);
        eprintln!(
            "  [egraph] {} classes ({} procs) | fusion: {} | aliases: {}",
            eg.class_count(),
            pl.procs.len(),
            eg.fusion_hits
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join(" "),
            if plan.aliases.is_empty() {
                "-".into()
            } else {
                plan.aliases
                    .iter()
                    .map(|(a, r)| format!("{}→{}", a, r))
                    .collect::<Vec<_>>()
                    .join(",")
            }
        );
        let mut results: BTreeMap<String, Value> = BTreeMap::new();
        let mut alias_set: BTreeSet<String> = plan.aliases.keys().cloned().collect();
        for rep in &plan.order {
            let Some(proc) = pl.procs.iter().find(|p| &p.name == rep) else {
                continue;
            };
            if proc.deliver {
                continue;
            }
            // 别名成员不单独执行（结果由代表填充，CSE）
            if alias_set.contains(rep) {
                continue;
            }
            // 同 class 的下游 proc 已通过别名共享本结果（CSE）。
            match exec_proc(proc, topic, params, &results, &pl) {
                Ok(val) => {
                    let val = val.clone();
                    results.insert(proc.name.clone(), val.clone());
                    // CSE：同 class 别名共享
                    for (alias, rep_name) in &plan.aliases {
                        if rep_name == rep {
                            results.insert(alias.clone(), val.clone());
                        }
                    }
                }
                Err(e) => {
                    // v0.14 egraph 路径同语义：Left 落库继续走（CSE 共享"失败"事实）
                    let rec = errflow::ErrorRecord::new(&proc.name, "-", &e, 0);
                    let enc = rec.encode();
                    results.insert(proc.name.clone(), Value::Text(enc.clone()));
                    for (alias, rep_name) in &plan.aliases {
                        if rep_name == rep {
                            results.insert(alias.clone(), Value::Text(enc.clone()));
                        }
                    }
                    return ExecResult::Failed {
                        error: e,
                        partial: results,
                    };
                }
            }
        }
        return ExecResult::Success(results);
    }

    let eg = egraph::build_egraph(&pl);
    let layers = egraph::parallel_groups(&eg);

    let mut results: BTreeMap<String, Value> = BTreeMap::new();
    let mut first_native_error: Option<String> = None;

    for layer in &layers {
        // Execute procs in this layer (serially for now — parallel via threads later)
        for proc_name in layer {
            let proc = pl.procs.iter().find(|p| &p.name == proc_name);
            if let Some(proc) = proc {
                if proc.deliver {
                    continue;
                }

                // v0.14a3 下游响应策略（errflow::respond）：按死亡上游的错误分类决策。
                let mut all_refs: Vec<String> = Vec::new();
                for imp in &proc.plan {
                    for r in &imp.refs {
                        if !all_refs.contains(r) {
                            all_refs.push(r.clone());
                        }
                    }
                }
                if let Some(ref src_proc) = proc.foreach {
                    if !all_refs.contains(src_proc) {
                        all_refs.push(src_proc.clone());
                    }
                }
                let dead = errflow::dead_refs(&results, &all_refs);
                if !dead.is_empty() {
                    let code = results
                        .get(&dead[0])
                        .and_then(|v| match v {
                            Value::Text(t) => Some(errflow::code_of(t)),
                            _ => None,
                        })
                        .unwrap_or(errflow::ErrCode::Crash);
                    match errflow::respond(code) {
                        errflow::Response::Exit => {
                            // 创作错误（contract）：运行期不可恢复，整流退出（保留 partial）
                            eprintln!(
                                "  [pipeline] errflow: contract error in {} → exit flow",
                                dead[0]
                            );
                            let msg = format!(
                                "contract error in {}: unrecoverable, flow exited",
                                dead[0]
                            );
                            return ExecResult::Failed {
                                error: msg,
                                partial: results,
                            };
                        }
                        resp => {
                            // Ignore/Wait/Switch 对引用者的共同语义 = 方法切换：
                            // 只封锁真正引用死源的 impl，未引用的备选照跑（切换方法）。
                            // Wait 的"等待"由分层顺序天然提供（上游已吃满 Retry 预算）。
                            let surviving: Vec<crate::ast::Impl> = proc
                                .plan
                                .iter()
                                .filter(|imp| !imp.refs.iter().any(|r| dead.contains(r)))
                                .cloned()
                                .collect();
                            if !surviving.is_empty() && resp == errflow::Response::Switch {
                                eprintln!(
                                    "  [{}] errflow: switch method ({} impls avoid dead {})",
                                    proc.name,
                                    surviving.len(),
                                    dead.join(",")
                                );
                                let mut p2 = proc.clone();
                                p2.plan = surviving;
                                match exec_proc(&p2, topic, params, &results, &pl) {
                                    Ok(val) => {
                                        results.insert(proc.name.clone(), val);
                                        continue;
                                    }
                                    Err(e) => {
                                        let rec = errflow::ErrorRecord::new(&proc.name, "-", &e, 0);
                                        eprintln!(
                                            "  [{}] LEFT: {} ({})",
                                            proc.name,
                                            rec.message,
                                            rec.code.code()
                                        );
                                        results
                                            .insert(proc.name.clone(), Value::Text(rec.encode()));
                                        if first_native_error.is_none() {
                                            first_native_error = Some(e);
                                        }
                                        continue;
                                    }
                                }
                            }
                            // 无可切换方法 → 隐式传播 Left（无视类局部死亡的标准路径）
                            let up = results
                                .get(&dead[0])
                                .cloned()
                                .unwrap_or(Value::Text(String::new()));
                            let up_text = match &up {
                                Value::Text(t) => t.clone(),
                                _ => String::new(),
                            };
                            let rec =
                                errflow::ErrorRecord::propagated(&proc.name, &dead[0], &up_text);
                            eprintln!("  [{}] skipped: propagated from {}", proc.name, dead[0]);
                            results.insert(proc.name.clone(), Value::Text(rec.encode()));
                            if first_native_error.is_none() {
                                first_native_error =
                                    Some(format!("propagated from {}: {}", dead[0], rec.message));
                            }
                            continue;
                        }
                    }
                }

                match exec_proc(proc, topic, params, &results, &pl) {
                    Ok(val) => {
                        results.insert(proc.name.clone(), val);
                    }
                    Err(e) => {
                        // v0.14 错误值化：不再中止管线，Left 落 results 继续走，
                        // 终局判定决定成败（下游按隐式传播短路）。
                        let last_impl = proc
                            .plan
                            .iter()
                            .rev()
                            .find(|i| i.enabled)
                            .map(|i| i.name.clone())
                            .unwrap_or_else(|| "-".to_string());
                        let rec = errflow::ErrorRecord::new(&proc.name, &last_impl, &e, 0);
                        eprintln!(
                            "  [{}] LEFT: {} ({})",
                            proc.name,
                            rec.message,
                            rec.code.code()
                        );
                        results.insert(proc.name.clone(), Value::Text(rec.encode()));
                        if first_native_error.is_none() {
                            first_native_error = Some(e.clone());
                        }
                        // v0.14a3 Exit（respond 表）：contract = 创作错误，运行期不可恢复，
                        // 立即退出整个流程（无论下游是否引用——修：不能藏在下游 dead_refs 里）
                        if errflow::respond(rec.code) == errflow::Response::Exit {
                            eprintln!(
                                "  [pipeline] errflow: {} error in {} → exit flow",
                                rec.code.code(),
                                proc.name
                            );
                            return ExecResult::Failed {
                                error: format!(
                                    "{} error in {}: unrecoverable, flow exited",
                                    rec.code.code(),
                                    proc.name
                                ),
                                partial: results,
                            };
                        }
                    }
                }
            }
        }
    }

    // v0.14b 终局裁决（errflow::fatal_left）：只有关键 proc（deliver 引用闭包）上的
    // Left 才致命——旁路（日志/通知/监控类）失败不牵连主产出，流水线仍 Success。
    // 兼容：无 deliver 的管线 = 全部关键（v0.9 语义不变）。
    if let Some(fatal_proc) = errflow::fatal_left(&pl, &results) {
        let root_msg = results
            .get(&fatal_proc)
            .and_then(|v| match v {
                Value::Text(t) => crate::dslresult::extract_field("err_msg", t),
                _ => None,
            })
            .unwrap_or_else(|| "unknown error".to_string());
        ExecResult::Failed {
            error: format!("critical proc '{}' failed: {}", fatal_proc, root_msg),
            partial: results,
        }
    } else {
        // 旁路失败容忍：主链完好 → Success（旁路 Left 仍在 results 里可查）
        if results.values().any(errflow::is_error_value) {
            eprintln!("  [pipeline] bypass failures tolerated — critical chain intact");
        }
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
    // v0.14 内置策略（errflow::strategy）：先跑一轮裸尝试，失败按根因分类处置。
    // Retry 预算循环只调 exec_proc_inner（裸）——策略块放本层，inner 不得再进策略，
    // 否则递归重试预算永远从 1 重数（实测嵌套爆炸 bug）。
    // v0.14b 关键性（errflow::critical_set）：主链节点值得更努力——重试预算 ×2；
    // 旁路节点基础预算。零 DSL 面：关键性由 deliver 引用闭包自动判定。
    let critical = errflow::critical_set(pl).contains(&proc.name);
    let mut raw = match exec_proc_inner(proc, topic, params, results, pl) {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    let code = errflow::classify(&raw);
    if let errflow::Action::Retry { budget, backoff } = errflow::strategy_for(code, critical) {
        for attempt in 0..budget {
            let delay = backoff.delay_secs(attempt);
            eprintln!(
                "  [{}] errflow: {}{} → retry {}/{} in {}s",
                proc.name,
                code.code(),
                if critical { " (critical)" } else { "" },
                attempt + 1,
                budget,
                delay
            );
            std::thread::sleep(std::time::Duration::from_secs(delay));
            match exec_proc_inner(proc, topic, params, results, pl) {
                Ok(v) => return Ok(v),
                Err(e) => raw = e,
            }
        }
    }
    Err(raw)
}

fn exec_proc_inner(
    proc: &Proc,
    topic: &str,
    params: &BTreeMap<String, String>,
    results: &BTreeMap<String, Value>,
    pl: &Pipeline,
) -> Result<Value, String> {
    // Normal proc: rank impls, try in order
    let recent_map = load_recent_runs(&proc.name);
    let eligible: Vec<&Impl> = proc
        .plan
        .iter()
        .filter(|i| is_eligible(params, results, i))
        .collect();

    let ranked = rank_impls_named(
        &pl.weights,
        &recent_map,
        &eligible,
        &proc.pick_by,
        &proc.name,
    );

    let mut raw_err: Option<String> = None;
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
        // v0.11 谓词层退役：结果直接透传，质量门槛由独立 judge proc + .when 路由承担。
        let started = Instant::now();
        match run_impl_with_retry(impl_, topic, results) {
            Ok(val) => {
                let latency_ms = started.elapsed().as_millis() as i64;
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
                    latency_ms,
                );
                // v0.8 preference learning: 成功 ×1.1（LGuess 乘性更新的奖励半边）
                db::record_pref(&proc.name, &impl_.name, true);
                return Ok(val);
            }
            Err(err) => {
                let latency_ms = started.elapsed().as_millis() as i64;
                eprintln!("  [{}] failed: {}", proc.name, err);
                append_run(
                    &proc.name,
                    &impl_.name,
                    pid,
                    Status::Fail,
                    Some(&short_hash(&err)),
                    Some(&format!("{}.{}.step", proc.name, impl_.name)),
                    latency_ms,
                );
                db::record_pref(&proc.name, &impl_.name, false);
                // v0.14 根因透传：最终 Err 携带最后 impl 的原始错误（非包装串），
                // 分类层据此定 code，策略层据此决策。
                raw_err = Some(err);
                continue;
            }
        }
    }

    Err(raw_err.unwrap_or_else(|| format!("All paths failed for proc: {}", proc.name)))
}

fn exec_foreach_proc(
    proc: &Proc,
    src_proc: &str,
    topic: &str,
    params: &BTreeMap<String, String>,
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
    // v0.11.1：foreach 子 proc 也走 when 裁判过滤（此前整组绕过 is_eligible）。
    let eligible: Vec<&Impl> = proc
        .plan
        .iter()
        .filter(|i| is_eligible(params, results, i))
        .collect();
    let ranked = rank_impls_named(
        &pl.weights,
        &recent_map,
        &eligible,
        &proc.pick_by,
        &proc.name,
    );

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

            let started = Instant::now();
            match run_impl_with_retry(&expanded, topic, &item_results) {
                Ok(val) => {
                    let latency_ms = started.elapsed().as_millis() as i64;
                    append_run(
                        &proc.name,
                        &impl_.name,
                        pid,
                        Status::Ok,
                        None,
                        None,
                        latency_ms,
                    );
                    sub_results.push(Ok(val));
                    found = true;
                    break;
                }
                Err(err) => {
                    let latency_ms = started.elapsed().as_millis() as i64;
                    append_run(
                        &proc.name,
                        &impl_.name,
                        pid,
                        Status::Fail,
                        Some(&short_hash(&err)),
                        Some(&format!("{}.{}.foreach", proc.name, impl_.name)),
                        latency_ms,
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
                // v0.14 Reroute（errflow 策略）：数据类错误同输入必再错，
                // 立即放弃当前 impl 剩余重试预算，让位下一备选路径。
                if errflow::classify(&err) == errflow::ErrCode::Data {
                    eprintln!(
                        "    -> errflow: data error → reroute (skip {} retries)",
                        impl_.retry - attempt
                    );
                    return Err(err);
                }
                if attempt < impl_.retry {
                    let delay = 1u64 << (attempt + 1); // 2s, 4s, 8s...
                    eprintln!(
                        "    -> retry {}/{} after {}s ({})",
                        attempt + 1,
                        impl_.retry,
                        delay,
                        crate::trunc_chars(&err, 80)
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

    if is_probe_stub(&func) {
        return Err("cache miss".into());
    }

    match step_registry().get(func.as_str()) {
        // v0.11 fail-closed：未知函数不再假成功（旧 <noop> 会把垃圾当结果污染下游并落库 Ok）。
        // Err 触发正常降级；清单由注册表自动生成。
        Some(f) => f(impl_, topic, body, results),
        None => {
            let known = known_functions().join("/");
            if func.is_empty() {
                Err("impl body has no recognizable function call".into())
            } else {
                Err(format!(
                    "unknown function '{}' — fail-closed (v0.11). Known: {}",
                    func, known
                ))
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
    results: &BTreeMap<String, Value>,
    cond: &str,
) -> bool {
    // v0.11.1 Interpreter 模式：条件 → AST（when::Cond）→ 求值。
    // 落地 SPEC 承诺的裁判路由 .when(@gate.score < 80)；fail-closed：坏条件/缺席裁判不放行。
    crate::when::eval_cond_str(cond, params, results)
}

// ── Record I/O (SQLite) ──

fn append_run(
    proc_name: &str,
    impl_name: &str,
    pid: char,
    status: Status,
    _err_hash: Option<&str>,
    _err_at: Option<&str>,
    latency_ms: i64,
) {
    append_run_rd(
        proc_name, impl_name, pid, status, _err_hash, _err_at, "", latency_ms,
    );
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
    latency_ms: i64,
) {
    let status_str = match status {
        Status::Ok => "Ok",
        Status::Fail => "Fail",
    };
    // rate 代理：输出字符数 / 4（英文 ~4 char/token 的粗估；中文偏保守）
    let rate_tokens = (output_text.chars().count() as i64) / 4;
    // v0.11: est_loss 死路移除（原 est_loss_v0 恒返 None，从未接线）。
    // latency_ms 从 exec_proc/exec_foreach_proc 的 Instant 实测传入——这是
    // 「cost 从测量来」的地基：后续可按 runs 表实测 EMA 排序。
    db::record_run_rd(
        proc_name,
        impl_name,
        "",
        status_str,
        latency_ms,
        _err_hash,
        _err_at,
        rate_tokens,
        0.0,
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

#[cfg(test)]
mod tests {
    use super::*;

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
                deliver_refs: vec![],
                foreach: None,
                foreach_var: String::new(),
                pick_by: "cost".into(),
                description: String::new(),
            }],
        };
        let params = BTreeMap::new();
        let result = exec_pipeline("test", &params, &pl, None);
        match result {
            ExecResult::Success(map) => {
                assert!(map.contains_key("p"));
                assert!(map["p"].as_text().contains("STUB"));
            }
            ExecResult::Failed { error, .. } => panic!("stub should succeed: {}", error),
        }
    }
}
