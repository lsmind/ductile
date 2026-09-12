//! Ranking — impl 排序与偏好学习。
//!
//! 从 executor.rs 拆出的选路层：乘性偏好权重（LGuess 式，成功 ×α/失败 ÷β，
//! clamp 上下界）、滑动窗口失败惩罚（连败 3 次 → ∞）、静态/成本排序、
//! 近期运行统计（load_recent_runs）。排序公式：effective = base×(1+penalty)/w。

use crate::core::ast::*;
use crate::db;
use std::collections::BTreeMap;

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

pub fn rank_impls_named<'a>(
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
