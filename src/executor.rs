//! Executor — runs a parsed Pipeline.
//!
//! Handles: cost-based impl selection, fallback, checks, foreach expansion,
//! retry with exponential backoff, variable resolution, shell command execution,
//! ##DSL_RESULT protocol, record (GCF) logging.

use crate::ast::*;
use crate::db;
use crate::egraph;
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use crate::steps::{is_probe_stub, step_registry};
pub use crate::steps::known_functions;
pub use crate::steps::exec_script_call;
use std::time::Instant;

pub use crate::textargs::{detect_func, resolve_vars, extract_string_arg, extract_first_string, expand_tilde, short_hash, extract_all_string_args};
pub use crate::dslresult::{parse_dsl_result_block, encode_structured_result, extract_field, est_loss_field_coverage};

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
                Err(e) => return ExecResult::Failed(e),
            }
        }
        return ExecResult::Success(results);
    }

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

    let ranked = rank_impls_named(
        &pl.weights,
        &recent_map,
        &eligible,
        &proc.pick_by,
        &proc.name,
    );

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

// ── Ranking ──

/// v0.8 preference learning 常数（LGuess 式乘性权重，AdaBoost 同族）。
pub const PREF_ALPHA: f64 = 1.1; // 成功 ×1.1
pub const PREF_BETA: f64 = 1.5; // 失败 ÷1.5（不对称：坏消息更重）
pub const PREF_MAX: f64 = 20.0; // clamp 上界（最多打 95 折）
pub const PREF_MIN: f64 = 0.05; // clamp 下界（最多涨 20 倍）

/// 每 (proc, impl) 的学习偏好权重，SQLite 持久化（impl_prefs 表）。
/// w>1 = 历史可靠 → effective cost 除以 w（打折）；w<1 = 不可靠 → 变贵。
#[derive(Debug, Default)]
pub struct ImplPrefs {
    map: BTreeMap<(String, String), f64>,
}

impl ImplPrefs {
    pub fn load(dir: &std::path::Path) -> Self {
        let _ = dir;
        let mut map = BTreeMap::new();
        for (p, i, w) in db::load_impl_prefs() {
            map.insert((p, i), w);
        }
        ImplPrefs { map }
    }

    pub fn get(&self, proc_name: &str, impl_name: &str) -> f64 {
        self.map
            .get(&(proc_name.to_string(), impl_name.to_string()))
            .copied()
            .unwrap_or(1.0)
    }

    /// 乘性更新并持久化。成功 ×α / 失败 ÷β，clamp [PREF_MIN, PREF_MAX]。
    pub fn update(&mut self, proc_name: &str, impl_name: &str, success: bool) {
        let key = (proc_name.to_string(), impl_name.to_string());
        let w = self.map.get(&key).copied().unwrap_or(1.0);
        let w = if success {
            w * PREF_ALPHA
        } else {
            w / PREF_BETA
        };
        let w = w.clamp(PREF_MIN, PREF_MAX);
        self.map.insert(key, w);
        db::upsert_impl_pref(proc_name, impl_name, w);
    }
}

#[cfg(test)]
fn rank_impls<'a>(
    weights: &Weights,
    recent: &BTreeMap<String, RecentRuns>,
    impls: &[&'a Impl],
    pick_by: &str,
) -> Vec<&'a Impl> {
    rank_impls_named(weights, recent, impls, pick_by, "")
}

fn rank_impls_named<'a>(
    weights: &Weights,
    recent: &BTreeMap<String, RecentRuns>,
    impls: &[&'a Impl],
    pick_by: &str,
    proc_name: &str,
) -> Vec<&'a Impl> {
    let prefs = ImplPrefs::load(std::path::Path::new(""));
    rank_impls_pref(weights, recent, impls, pick_by, &prefs, proc_name)
}

/// 排序核心：effective = base×(1+penalty)/w + rd。
/// w>1（历史可靠）打折，w<1 变贵；penalty 纪律惩罚保留；
/// pick_by="static" 忽略学习偏好（纯静态 cost 基准模式）。
fn rank_impls_pref<'a>(
    weights: &Weights,
    recent: &BTreeMap<String, RecentRuns>,
    impls: &[&'a Impl],
    pick_by: &str,
    prefs: &ImplPrefs,
    proc_name: &str,
) -> Vec<&'a Impl> {
    let static_mode = pick_by == "static";
    let mut ranked: Vec<(f64, &'a Impl)> = impls
        .iter()
        .map(|i| {
            let base = weights.cost_total(&i.cost);
            let penalty = impl_penalty(recent, &i.name);
            let w = if static_mode {
                1.0
            } else {
                prefs.get(proc_name, &i.name).max(1e-6)
            };
            (base * (1.0 + penalty) / w, *i)
        })
        .collect();
    ranked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    ranked.into_iter().map(|(_, imp)| imp).collect()
}

/// RD 附加费：weights.rd × 该 impl 近 20 次的平均失真率 est_loss/max(rate_tokens,1)。
/// 语义：同样 token 预算下，单位信息损耗大的 impl 排后（E7a 准则的运行时形态）。
/// 无历史记录 = 0（冷启动不惩罚）；rd 权重 0 = 完全关闭（默认）。
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

    // ── v0.8 preference learning (LGuess-style multiplicative weights) ──

    #[test]
    fn pref_update_success_increases_weight() {
        let mut p = ImplPrefs::default();
        let w0 = p.get("procA", "implA");
        p.update("procA", "implA", true);
        let w1 = p.get("procA", "implA");
        assert!(w1 > w0, "success must increase weight");
        assert!((w1 - w0 * PREF_ALPHA).abs() < 1e-9);
        // untouched impl stays at 1.0
        assert_eq!(p.get("procA", "implB"), 1.0);
    }

    #[test]
    fn pref_update_failure_decreases_weight() {
        let mut p = ImplPrefs::default();
        p.update("procA", "implA", false);
        let w1 = p.get("procA", "implA");
        assert!(w1 < 1.0, "failure must decrease weight");
        assert!((w1 - 1.0 / PREF_BETA).abs() < 1e-9);
    }

    #[test]
    fn pref_update_clamps() {
        let mut p = ImplPrefs::default();
        for _ in 0..1000 {
            p.update("procA", "implA", true);
        }
        assert_eq!(p.get("procA", "implA"), PREF_MAX);
        let mut q = ImplPrefs::default();
        for _ in 0..1000 {
            q.update("procA", "implA", false);
        }
        assert_eq!(q.get("procA", "implA"), PREF_MIN);
    }

    #[test]
    fn pref_roundtrip_sqlite() {
        let dir = std::env::temp_dir().join(format!("ductile-pref-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut p = ImplPrefs::load(&dir);
        p.update("pX", "iX", true);
        p.update("pX", "iX", true);
        let w = p.get("pX", "iX");
        // fresh load from same dir must see the persisted weight
        let q = ImplPrefs::load(&dir);
        assert_eq!(q.get("pX", "iX"), w);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rank_divides_by_learned_weight() {
        // reliable-but-expensive impl should win once its learned weight is high
        let weights = Weights::default();
        let mut recent = BTreeMap::new();
        let mut prefs = ImplPrefs::default();
        let expensive = mk_impl("expensive", 10_000);
        let cheap = mk_impl("cheap", 1_000);
        // 30 consecutive successes on expensive → w clamped at PREF_MAX
        for _ in 0..30 {
            prefs.update("procA", "expensive", true);
        }
        let impls = vec![&cheap, &expensive];
        let ranked = rank_impls_pref(&weights, &recent, &impls, "cost", &prefs, "procA");
        assert_eq!(ranked[0].name, "expensive");
        // static mode ignores prefs: cheap must win
        let ranked_static = rank_impls_pref(&weights, &recent, &impls, "static", &prefs, "procA");
        assert_eq!(ranked_static[0].name, "cheap");
    }

    #[test]
    fn rank_default_pref_is_noop() {
        // with no history/prefs, ranking must equal pure cost order
        let weights = Weights::default();
        let recent = BTreeMap::new();
        let prefs = ImplPrefs::default();
        let a = mk_impl("a", 5_000);
        let b = mk_impl("b", 2_000);
        let impls = vec![&a, &b];
        let ranked = rank_impls_pref(&weights, &recent, &impls, "cost", &prefs, "procA");
        assert_eq!(ranked[0].name, "b");
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

    fn mk_impl(name: &str, latency: i64) -> Impl {
        Impl {
            name: name.into(),
            cost: Cost {
                latency,
                risk: 0.0,
                tokens: 0,
                money: 0.0,
            },
            ..default_impl()
        }
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
        let result = exec_pipeline("test", &params, &pl, None);
        match result {
            ExecResult::Success(map) => {
                assert!(map.contains_key("p"));
                assert!(map["p"].as_text().contains("STUB"));
            }
            ExecResult::Failed(e) => panic!("stub should succeed: {}", e),
        }
    }
}
