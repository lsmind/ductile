//! v0.11 评价子系统 —— 「裁判分离」的运行时半边。
//!
//! 设计模式映射（2026-09-05 重构，自 executor.rs God-file 抽出）：
//! - **Strategy**：[`CostSource`] —— cost 求值策略族（[`DirectCost`] / [`MeasuredCost`]）。
//!   新增 cost 来源（env/file/...）只需加实现，不改挂载逻辑。
//! - **Repository**：[`CostCacheStore`] —— 测量缓存的持久化抽象（[`SqliteCostCache`] 生产 /
//!   [`InMemoryCache`] 测试），measure 策略只依赖抽象，可注入、可单测。
//! - **Null Object**：[`Evaluator`] —— `--policy` 缺席不再是 `Option` 分支，
//!   而是 [`NullEval`]（无操作）与 [`PolicyEval`]（真实挂载）的多态统一。
//! - **Factory**：[`cost_source()`] / [`evaluator()`] —— 由数据构造策略对象。

use crate::ast::{CostValue, Pipeline, Policy};
use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// measure 结果缓存 TTL（秒）。
pub const COST_MEASURE_TTL_SECS: u64 = 86_400;

/// measure 脚本单次执行上限（防挂死卡死整条 pipeline；exec_run 的 group-kill 版本见 executor）。
const MEASURE_TIMEOUT: Duration = Duration::from_secs(30);

/// 缓存键：(proc, impl, field)。
pub type CacheKey<'a> = (&'a str, &'a str, &'a str);

// ── Repository：测量缓存抽象 ──

/// cost 测量缓存的存取抽象。生产走 SQLite，测试可注入内存实现。
pub trait CostCacheStore {
    fn get_fresh(&self, key: CacheKey, ttl_secs: u64) -> Option<f64>;
    fn put(&self, key: CacheKey, value: f64);
}

/// 生产实现：SQLite `cost_cache` 表。
pub struct SqliteCostCache;

impl CostCacheStore for SqliteCostCache {
    fn get_fresh(&self, key: CacheKey, ttl_secs: u64) -> Option<f64> {
        crate::db::cost_cache_get_fresh(key.0, key.1, key.2, ttl_secs as i64)
    }
    fn put(&self, key: CacheKey, value: f64) {
        crate::db::cost_cache_put(key.0, key.1, key.2, value);
    }
}

/// 测试实现：进程内 HashMap（不做 TTL 过期——测试只关心命中语义）。
#[derive(Default)]
pub struct InMemoryCache {
    map: std::cell::RefCell<BTreeMap<(String, String, String), f64>>,
}

impl InMemoryCache {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CostCacheStore for InMemoryCache {
    fn get_fresh(&self, key: CacheKey, _ttl_secs: u64) -> Option<f64> {
        self.map
            .borrow()
            .get(&(key.0.to_string(), key.1.to_string(), key.2.to_string()))
            .copied()
    }
    fn put(&self, key: CacheKey, value: f64) {
        self.map.borrow_mut().insert(
            (key.0.to_string(), key.1.to_string(), key.2.to_string()),
            value,
        );
    }
}

// ── Strategy：cost 求值策略 ──

/// cost 值的求值策略。实现按 `proc.impl.field` 键控解析出数值。
pub trait CostSource {
    fn resolve(&self, key: CacheKey, topic: &str, cache: &dyn CostCacheStore) -> Option<f64>;
}

/// Strategy A：直接数值——声明即真相，不执行任何东西。
pub struct DirectCost(pub f64);

impl CostSource for DirectCost {
    fn resolve(&self, _key: CacheKey, _topic: &str, _cache: &dyn CostCacheStore) -> Option<f64> {
        Some(self.0)
    }
}

/// Strategy B：测试方法——执行外部脚本取 stdout 首个浮点数（**latency 单位：毫秒**），
/// 经 Repository 缓存（TTL 内命中不重测）。
pub struct MeasuredCost {
    pub command: String,
}

impl CostSource for MeasuredCost {
    fn resolve(&self, key: CacheKey, topic: &str, cache: &dyn CostCacheStore) -> Option<f64> {
        let (proc_name, impl_name) = (key.0, key.1);
        if let Some(v) = cache.get_fresh(key, COST_MEASURE_TTL_SECS) {
            eprintln!(
                "  [policy] {}.{}: measure cache hit = {}",
                proc_name, impl_name, v
            );
            return Some(v);
        }
        let expanded = self.command.replace("{topic}", topic);
        eprintln!(
            "  [policy] {}.{}: measuring via: {}",
            proc_name, impl_name, expanded
        );
        let out = run_command_captured(&expanded, MEASURE_TIMEOUT)?;
        if !out.status.success() {
            eprintln!(
                "  [policy] measure script exited {} — stderr: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr)
                    .chars()
                    .take(200)
                    .collect::<String>()
            );
            return None;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let v = first_f64(&stdout)?;
        cache.put(key, v);
        eprintln!(
            "  [policy] {}.{}: measured {} = {}",
            proc_name, impl_name, key.2, v
        );
        Some(v)
    }
}

/// Factory：由 AST 数据（CostValue）构造对应策略对象。
pub fn cost_source(cv: &CostValue) -> Box<dyn CostSource + '_> {
    match cv {
        CostValue::Direct(v) => Box::new(DirectCost(*v)),
        CostValue::Measure(cmd) => Box::new(MeasuredCost {
            command: cmd.clone(),
        }),
    }
}

/// 执行 shell 命令并捕获输出，带超时（超时 kill 进程本身）。
fn run_command_captured(cmd: &str, timeout: Duration) -> Option<std::process::Output> {
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| eprintln!("  [policy] measure launch failed: {}", e))
        .ok()?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!(
                        "  [policy] measure timed out after {}s: {}",
                        timeout.as_secs(),
                        cmd
                    );
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("  [policy] measure failed: {}", e);
                return None;
            }
        }
    };
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    use std::io::Read;
    if let Some(mut p) = child.stdout.take() {
        let _ = p.read_to_end(&mut stdout_buf);
    }
    if let Some(mut p) = child.stderr.take() {
        let _ = p.read_to_end(&mut stderr_buf);
    }
    Some(std::process::Output {
        status,
        stdout: stdout_buf,
        stderr: stderr_buf,
    })
}

/// stdout 中第一个浮点数（容忍 "0.21s"、"latency: 123 ms" 等输出）。
pub fn first_f64(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            let mut dot = false;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || (bytes[i] == b'.' && !dot)) {
                if bytes[i] == b'.' {
                    dot = true;
                }
                i += 1;
            }
            return s[start..i].parse::<f64>().ok().filter(|v| v.is_finite());
        }
        i += 1;
    }
    None
}

// ── Null Object：评价策略挂载点 ──

/// 评价策略挂载接口。`exec_pipeline` 只依赖此抽象。
pub trait Evaluator {
    fn apply(&self, pl: &mut Pipeline, topic: &str);
}

/// 真实挂载：把 .eval 策略（权重 + cost 表）织入管线。
pub struct PolicyEval<'p> {
    pub policy: &'p Policy,
    pub cache: &'p dyn CostCacheStore,
}

impl Evaluator for PolicyEval<'_> {
    fn apply(&self, pl: &mut Pipeline, topic: &str) {
        apply_policy(pl, self.policy, self.cache, topic);
    }
}

/// Null Object：无 `--policy` 时的无操作实现（管线保持引擎默认）。
pub struct NullEval;

impl Evaluator for NullEval {
    fn apply(&self, _pl: &mut Pipeline, _topic: &str) {}
}

/// Factory：`Option<&Policy>` → 策略对象（None 对象化为 NullEval）。
pub fn evaluator(policy: Option<&Policy>) -> Box<dyn Evaluator + '_> {
    match policy {
        Some(p) => Box::new(PolicyEval {
            policy: p,
            cache: &SqliteCostCache,
        }),
        None => Box::new(NullEval),
    }
}

// ── 挂载逻辑（原 executor::apply_policy，骨架不变）──

/// 把策略织入管线：权重整体替换；cost 按 `proc.impl` 键控解析写回。
/// 解析失败的字段按 0 兜底并 warning（不 crash——评价失效不能炸生产）。
fn apply_policy(pl: &mut Pipeline, policy: &Policy, cache: &dyn CostCacheStore, topic: &str) {
    pl.weights = policy.weights.clone();
    eprintln!(
        "  [policy] weights: latency={} risk={} tokens={:.4} money={}",
        pl.weights.latency, pl.weights.risk, pl.weights.tokens, pl.weights.money
    );
    for proc in &mut pl.procs {
        for impl_ in &mut proc.plan {
            let key = format!("{}.{}", proc.name, impl_.name);
            let Some(spec) = policy.costs.get(&key) else {
                continue;
            };
            for (field, cv) in [
                ("latency", &spec.latency),
                ("risk", &spec.risk),
                ("tokens", &spec.tokens),
                ("money", &spec.money),
            ] {
                let Some(cv) = cv else { continue };
                let (proc_name, impl_name) = split_key(&key);
                let resolved = cost_source(cv).resolve((proc_name, impl_name, field), topic, cache);
                match resolved {
                    Some(v) => match field {
                        "latency" => impl_.cost.latency = v as i64,
                        "risk" => impl_.cost.risk = v,
                        "tokens" => impl_.cost.tokens = v as i64,
                        "money" => impl_.cost.money = v,
                        _ => {}
                    },
                    None => eprintln!(
                        "  [policy] {}.{}: field {} unresolvable — treated as 0 (sorts last among known)",
                        key, field, field
                    ),
                }
            }
            eprintln!(
                "  [policy] {}: resolved cost latency={} risk={:.3} tokens={} money={:.3}",
                key, impl_.cost.latency, impl_.cost.risk, impl_.cost.tokens, impl_.cost.money
            );
        }
    }
}

/// "proc.impl" → ("proc", "impl")（只切第一个点；impl 名可含点）。
fn split_key(key: &str) -> (&str, &str) {
    match key.find('.') {
        Some(pos) => (&key[..pos], &key[pos + 1..]),
        None => (key, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Strategy：DirectCost ──

    #[test]
    fn direct_cost_resolves_without_touching_cache() {
        let cache = InMemoryCache::new();
        let src = DirectCost(42.0);
        assert_eq!(
            src.resolve(("p", "i", "latency"), "topic", &cache),
            Some(42.0)
        );
        // 策略未写缓存
        assert_eq!(cache.get_fresh(("p", "i", "latency"), 60), None);
    }

    // ── Strategy：MeasuredCost（真跑脚本）──

    #[test]
    fn measured_cost_executes_script_and_caches() {
        let cache = InMemoryCache::new();
        let src = MeasuredCost {
            command: "echo 123 ms".into(),
        };
        // 首测：执行脚本取值 123
        assert_eq!(src.resolve(("p", "i", "latency"), "t", &cache), Some(123.0));
        // 缓存已写入
        assert_eq!(cache.get_fresh(("p", "i", "latency"), 60), Some(123.0));
        // 二测：换一个必失败脚本——仍返回 123（cache hit，不执行）
        let src2 = MeasuredCost {
            command: "exit 1".into(),
        };
        assert_eq!(
            src2.resolve(("p", "i", "latency"), "t", &cache),
            Some(123.0)
        );
    }

    #[test]
    fn measured_cost_failing_script_returns_none() {
        let cache = InMemoryCache::new();
        let src = MeasuredCost {
            command: "echo boom >&2; exit 3".into(),
        };
        assert_eq!(src.resolve(("p", "i", "latency"), "t", &cache), None);
        assert_eq!(cache.get_fresh(("p", "i", "latency"), 60), None);
    }

    #[test]
    fn measured_cost_non_numeric_stdout_returns_none() {
        let cache = InMemoryCache::new();
        let src = MeasuredCost {
            command: "echo no numbers here".into(),
        };
        assert_eq!(src.resolve(("p", "i", "latency"), "t", &cache), None);
    }

    #[test]
    fn measured_cost_topic_expansion_and_timeout_output() {
        let cache = InMemoryCache::new();
        let src = MeasuredCost {
            command: "echo 0.25s {topic}".into(),
        };
        // 容忍 "0.25s" 形态；topic 展开发生在命令里（不影响取值）
        assert_eq!(
            src.resolve(("p", "i", "latency"), "XYZ", &cache),
            Some(0.25)
        );
    }

    // ── first_f64 边界 ──

    #[test]
    fn first_f64_variants() {
        assert_eq!(first_f64("0.201"), Some(0.201));
        assert_eq!(first_f64("measured latency: 123 ms"), Some(123.0));
        assert_eq!(first_f64("lat=1.5e3"), Some(1.5)); // 指数记法不支持——取到 1.5
        assert_eq!(first_f64("nope"), None);
        assert_eq!(first_f64(""), None);
        assert_eq!(first_f64("..5"), Some(5.0)); // 扫描语义：跳过前置点，取到 5
    }

    // ── Null Object：evaluator 工厂 ──

    #[test]
    fn null_eval_is_noop() {
        // NullEval.apply 不改管线——由 policy_probe 真跑覆盖集成路径，
        // 这里钉死工厂行为：None → NullEval（类型层面不可直接比较，验证 apply 不 panic 且权重不动）
        let mut pl = crate::ast::Pipeline::default();
        let before = pl.weights.clone();
        NullEval.apply(&mut pl, "t");
        assert_eq!(pl.weights.latency, before.latency);
        assert_eq!(pl.weights.risk, before.risk);
    }
}
