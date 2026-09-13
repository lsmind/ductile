//! Executor — runs a parsed Pipeline.
//!
//! Handles: cost-based impl selection, fallback, checks, foreach expansion,
//! retry with exponential backoff, variable resolution, shell command execution,
//! ##DSL_RESULT protocol, record (GCF) logging.

use crate::core::ast::*;
use crate::db;
use crate::egraph;
use crate::errflow;
use crate::ranking::rank_impls_named;
pub use crate::ranking::ImplPrefs;
pub use crate::steps::exec_script_call;
pub use crate::steps::known_functions;
use crate::steps::PipelineCtx;
use crate::steps::{is_probe_stub, step_registry};
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::time::Instant;

pub use crate::core::dslresult::{
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
    // v0.18.6 P1-1：只应用 confirmed。tentative 是待验证假设（doctor 处方），
    // 没过验证门不算数；reverted 是审计残留。进化环的生效路径唯一：
    // tentative →（canary/probe 验证）→ confirmed。
    let active: Vec<_> = patches.iter().filter(|p| p.status == "confirmed").collect();
    if active.is_empty() {
        eprintln!(
            "  [patch] {} patches skipped (tentative/reverted, not confirmed)",
            patches.len()
        );
        return pl.clone();
    }
    eprintln!(
        "  [patch] {}/{} patches applied to pipeline '{}' (confirmed only)",
        active.len(),
        patches.len(),
        pl.name
    );
    let mut cloned = pl.clone();
    for patch in &active {
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
    // v0.16 管线级 cwd/env（第三刀）：thread_local 中继到 steps 层的 Command 构造。
    // 不用 set_var 全局 env（AGENTS 规则 9：并行测试竞态）；不用签名穿线
    //（StepFn 签名动一发牵全身）。cwd 经 bash 一次性规范化（$VAR/$(...) 可用），
    // 失败即整流退出——cwd 错了后面每条命令都是错误目录，fail-closed。
    let _ctx_guard = PipelineCtx::set(pl);
    if let Some(poison) = _ctx_guard.as_ref().and_then(|g| g.poison()) {
        return ExecResult::Failed {
            error: poison.to_string(),
            partial: BTreeMap::new(),
        };
    }

    // Apply hot patches: clone pipeline, override fields from SQLite.
    // v0.11.1 Null Object：--policy 缺席不再走 Option 分支，统一为 Evaluator 多态调用。
    let mut pl = apply_patches(pl);
    crate::eval::evaluator(policy).apply(&mut pl, topic);
    // v0.18.6 P0-刀3：L4 复核常开。fail verdict 确定性记录（免费真信号）
    // 永远开；Success 侧真实 LLM 独立复核走 DUCTILE_L4_REVIEW=1 选入
    //（防引擎自证：回显 deliver 摘要记 pass 会污染校准集）。
    let l4_review_success = std::env::var("DUCTILE_L4_REVIEW")
        .map(|v| v == "1")
        .unwrap_or(false);

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
                    // v0.14 egraph 路径与默认路径同语义：Left 落库继续走（CSE 共享失败），
                    // 终局用 fatal_left 裁决；contract/Exit 才立即中止。
                    let rec = errflow::ErrorRecord::new(&proc.name, "-", &e, 0);
                    let enc = rec.encode();
                    eprintln!(
                        "  [{}] LEFT: {} ({})",
                        proc.name,
                        rec.message,
                        rec.code.code()
                    );
                    results.insert(proc.name.clone(), Value::Text(enc.clone()));
                    for (alias, rep_name) in &plan.aliases {
                        if rep_name == rep {
                            results.insert(alias.clone(), Value::Text(enc.clone()));
                        }
                    }
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
        // 与默认路径一致：仅关键链（deliver 闭包）上的 Left 致命
        if let Some(fatal_proc) = errflow::fatal_left(&pl, &results) {
            let root_msg = results
                .get(&fatal_proc)
                .and_then(|v| match v {
                    Value::Text(t) => crate::core::dslresult::extract_field("err_msg", t),
                    _ => None,
                })
                .unwrap_or_else(|| "unknown error".to_string());
            return ExecResult::Failed {
                error: format!("critical proc '{}' failed: {}", fatal_proc, root_msg),
                partial: results,
            };
        }
        if results.values().any(errflow::is_error_value) {
            eprintln!("  [pipeline] bypass failures tolerated — critical chain intact");
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
                            let surviving: Vec<crate::core::ast::Impl> = proc
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
                Value::Text(t) => crate::core::dslresult::extract_field("err_msg", t),
                _ => None,
            })
            .unwrap_or_else(|| "unknown error".to_string());
        // v0.15 L4 复核（log-only；enforcing 时 fail verdict 升格为管线失败）
        if let Some(l4_err) = l4_finalize(&pl, &results, Some(&root_msg), l4_review_success) {
            return ExecResult::Failed {
                error: format!(
                    "critical proc '{}' failed: {}; {}",
                    fatal_proc, root_msg, l4_err
                ),
                partial: results,
            };
        }
        ExecResult::Failed {
            error: format!("critical proc '{}' failed: {}", fatal_proc, root_msg),
            partial: results,
        }
    } else {
        // 旁路失败容忍：主链完好 → Success（旁路 Left 仍在 results 里可查）
        if results.values().any(errflow::is_error_value) {
            eprintln!("  [pipeline] bypass failures tolerated — critical chain intact");
        }
        // v0.15 L4 复核：Success 也记录（log-only 攒标签数据面）
        if let Some(l4_err) = l4_finalize(&pl, &results, None, l4_review_success) {
            return ExecResult::Failed {
                error: l4_err,
                partial: results,
            };
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
    // v0.17 auto-prompt：per-proc 图上下文中继（PipelineCtx 同款 thread_local）。
    // exec_llm 在 steps 层拿不到 Pipeline/Proc——穿签名会动 StepFn 全家，
    // 中继与 cwd/env 方案一致（不进全局 env，无并行竞态）。
    let _node_guard = crate::steps::NodeCtx::set(pl, proc);
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
    // v0.15 节点契约卡（cognition spec §7 P0）：执行后确定性校验。
    // L1 outputs 存在性 + L2 invariants 谓词（when.rs 求值器，@self.field 自引用）。
    // 违例 → "contract violation:" 前缀 → errflow Contract 类 → Exit（不重试：
    // 契约错不是瞬态错，重跑同一 impl 只会再违例）。
    let mut raw = match exec_proc_inner(proc, topic, params, results, pl) {
        Ok(v) => {
            if let Err(e) = check_contract(proc, &v) {
                record_incident(pl, proc, &e);
                return Err(e);
            }
            return Ok(v);
        }
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
                Ok(v) => {
                    if let Err(e) = check_contract(proc, &v) {
                        return Err(e);
                    }
                    return Ok(v);
                }
                Err(e) => raw = e,
            }
        }
    }
    Err(raw)
}

/// v0.15 incident 落库（旁路写入，失败静默——事故记录不能搞死主管线）。
fn record_incident(pl: &Pipeline, proc: &Proc, err: &str) {
    if let Ok(conn) = crate::db::open_try() {
        let code = errflow::classify(err).code().to_string();
        let id = crate::incident::record_incident_conn(&conn, &pl.name, &proc.name, &code, err, "");
        // v0.18.6 P0-刀2：incident 落库即自动 triage（判别自动化——归因闸
        // 从"可调用"变"必经"）。旁路静默，失败不搞死主管线。
        // latest_result_text 留空 = 无输出快照可比，判 nocanary/按 canary 谓词对空文本求值。
        let _ = crate::incident::triage_incident_conn(&conn, id, "");
    }
}

/// v0.15 L4 端到端复核（cognition spec §7 缺口 #4）：管线收尾 log-only 记录。
/// v0.18.6 P0-刀3：常开（拆 DUCTILE_L4 环境门——焊死的门等于没有门）。
/// - fatal 有值 → fail verdict 确定性记录（免费真信号，不请 LLM）
/// - fatal None + review_success → 独立会话 LLM 真复核（review_prompt，
///   不带管线内部细节——产出者不自证清白，防橡皮图章）
/// - fatal None + !review_success → pass verdict 落库（evidence 标注
///   "no independent review"，校准者一眼可辨，不冒充真复核）
/// 旁路写入（open_try 失败静默）——复核记录不能搞死主管线。
/// enforcing 阶段（≥8 标签且一致率≥70%）升格：fail verdict 回写为 pipeline 错误。
fn l4_finalize(
    pl: &Pipeline,
    results: &BTreeMap<String, Value>,
    fatal: Option<&str>,
    review_success: bool,
) -> Option<String> {
    // cfg!(test) 守卫：cargo test 继承 shell 的 DUCTILE_L4=1 时曾把单测的
    // exec_pipeline 复核写进真库（环境泄漏脚枪）
    if cfg!(test) {
        return None;
    }
    let Ok(conn) = crate::db::open_try() else {
        return None;
    };
    // deliver proc 本身不执行（executor 跳过）；摘要取 deliver 引用的 proc 值
    let deliver_target: Vec<String> = pl
        .procs
        .iter()
        .filter(|p| p.deliver)
        .flat_map(|p| p.deliver_refs.clone())
        .collect();
    let deliver_summary: String = results
        .iter()
        .filter(|(k, _)| deliver_target.contains(k))
        .map(|(k, v)| {
            format!(
                "{}: {}",
                k,
                v.as_text().chars().take(2000).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    let (verdict, evidence) = match fatal {
        Some(msg) => (
            "fail",
            format!("critical: {}", msg.chars().take(2000).collect::<String>()),
        ),
        None if review_success => {
            // 独立会话真复核：意图 = topic（exec_pipeline 无独立 intent 字段，
            // topic 即任务意图的最接近代理）+ deliver 摘要，不带管线内部细节。
            let intent = pl.description.chars().take(2000).collect::<String>();
            let prompt = crate::l4::review_prompt(&intent, &deliver_summary);
            match crate::L2_orchestration::steps::replay_canary_llm(&prompt) {
                Some(out) => {
                    let first = out
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("");
                    let (v, e) = if first.to_lowercase().starts_with("fail") {
                        ("fail", first.chars().take(2000).collect::<String>())
                    } else {
                        ("pass", first.chars().take(2000).collect::<String>())
                    };
                    (v, format!("[independent review] {e}"))
                }
                None => (
                    "pass",
                    format!(
                        "[no independent review: bridge unavailable] {}",
                        deliver_summary.chars().take(1800).collect::<String>()
                    ),
                ),
            }
        }
        None => (
            "pass",
            if deliver_summary.is_empty() {
                "no deliver proc; all procs completed".to_string()
            } else {
                format!(
                    "[no independent review] {}",
                    deliver_summary.chars().take(1800).collect::<String>()
                )
            },
        ),
    };
    let _ = crate::l4::record_review_conn(&conn, &pl.name, verdict, &evidence, None);
    eprintln!("  [l4] review recorded: {} (log-only)", verdict);
    // enforcing 阶段：fail verdict 不再容忍
    if verdict == "fail" && crate::l4::phase_for_conn(&conn) == crate::l4::L4Phase::Enforcing {
        return Some(format!(
            "L4 enforcing: end-to-end review failed — {}",
            evidence
        ));
    }
    None
}

/// v0.15 契约校验（cognition spec §7 P0）。纯函数：proc 契约 × 结果值 → Ok/Err。
/// 错误消息以 "contract violation:" 开头（errflow PAT_CONTRACT 判定链首位命中）。
pub fn check_contract(proc: &Proc, value: &Value) -> Result<(), String> {
    let c = &proc.contract;
    if c.outputs.is_empty() && c.invariants.is_empty() {
        return Ok(());
    }
    // 裸文本结果无字段可查：fail-closed——声明了契约就必须有结构化输出
    let text = match value {
        Value::Text(t) => t.as_str(),
        _ => {
            return Err(format!(
                "contract violation: proc '{}' declared outputs/invariants but result is not text-structured",
                proc.name
            ))
        }
    };
    // L1 outputs：字段存在性
    for f in &c.outputs {
        if crate::core::dslresult::extract_field(f, text).is_none() {
            return Err(format!(
                "contract violation: proc '{}' missing required output field '{}'",
                proc.name, f
            ));
        }
    }
    // L2 invariants：when.rs 求值器，@self.field 自引用本 proc 结果
    let mut self_results: BTreeMap<String, Value> = BTreeMap::new();
    self_results.insert(proc.name.clone(), value.clone());
    for inv in &c.invariants {
        // @self → 本 proc 名（语法糖：契约谓词天然自指）
        let cond = inv.replace("@self.", &format!("@{}.", proc.name));
        if !crate::when::eval_cond_str(&cond, &BTreeMap::new(), &self_results) {
            return Err(format!(
                "contract violation: proc '{}' invariant failed: {}",
                proc.name, inv
            ));
        }
    }
    Ok(())
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
                // v0.15 incident 一等实体：失败信号束成事故候选落库（聚合去重）
                record_incident(pl, proc, &err);
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
                // v0.14d：format/schema 同属 Reroute 族（确定性坏换路径）。
                if matches!(
                    errflow::classify(&err),
                    errflow::ErrCode::Data | errflow::ErrCode::Format | errflow::ErrCode::Schema
                ) {
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
            cwd: None,
            env: vec![],
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
                contract: Default::default(),
                deliver: false,
                needs: vec![],
                constraint_fields: vec![],
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
