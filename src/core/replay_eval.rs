//! v0.20 Replay-RSI P2：重放评分引擎（core 纯函数）。
//!
//! Dream-RSI（arXiv 2609.14858）Eq.1 的 ductile 落法：
//!   V = max_score − β1·N + β2·N/max(1, k)
//!   max_score: 重放轨迹上最佳节点质量分（runs.score；空则 Ok=1/Fail=0 代理）
//!   N:         展开的生成-评估请求数（≈ runs 行数，重试计费）
//!   k:         决策轮数（每批并行 = 1 轮；串行 N 次 = N 轮）
//!
//! 前缀重放语义（论文 §3 Offline evaluation）：替代策略 = 在已记录树上
//! 选不同前缀停止点/不同批组合。本引擎对给定树枚举「截断前缀」轨迹族，
//! 返回每条轨迹的评分——P3 的策略改写环据此做证伪对比。
//!
//! β 三纪律（论文附录 B.2，防重放作弊）：
//!   1. 单次重放内 β 钉死
//!   2. 评估期：固定 β 网格扫描（beta_sweep）
//!   3. 跨轮调默认：只在提议下一版策略时（P3 责任，本层只提供 sweep 数据）
//!
//! 纯函数、零 I/O、零 LLM——可单测、可进 selftest 门禁（裁判确定性优先红线）。

/// 一个重放轨迹的评价输入。
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayTrace {
    /// 按展开顺序的节点分（score 列优先；Fail 无分时用 0.0 代理）。
    pub node_scores: Vec<f64>,
    /// 每个决策轮展开的节点数（批大小；串行 = 每轮 1）。
    /// 长度 = k（决策轮数）。各轮批大小之和 = node_scores.len()。
    pub batch_sizes: Vec<usize>,
}

impl ReplayTrace {
    /// 串行轨迹：每轮展开一个节点（波信息缺失时的保守解释）。
    pub fn serial(node_scores: Vec<f64>) -> Self {
        let n = node_scores.len();
        Self {
            node_scores,
            batch_sizes: vec![1; n],
        }
    }

    pub fn n_probes(&self) -> usize {
        self.node_scores.len()
    }

    pub fn k_rounds(&self) -> usize {
        self.batch_sizes.len()
    }

    /// 轨迹上达到的最佳分（空轨迹 = 0；论文 max_v∈T s_v）。
    pub fn best(&self) -> f64 {
        self.node_scores.iter().cloned().fold(0.0, f64::max)
    }

    /// 并行度 = N / k（论文 β2 项的每轮平均批量；串行=1，满批=W）。
    pub fn avg_batch(&self) -> f64 {
        if self.batch_sizes.is_empty() {
            return 0.0;
        }
        self.n_probes() as f64 / self.k_rounds() as f64
    }
}

/// Eq.1 评分。β 轮内钉死由调用方保证（本函数不记忆任何状态）。
pub fn replay_score(t: &ReplayTrace, beta1: f64, beta2: f64) -> f64 {
    let n = t.n_probes() as f64;
    let k = t.k_rounds().max(1) as f64;
    t.best() - beta1 * n + beta2 * n / k
}

/// β 扫描（纪律 2）：固定网格上评一条轨迹，返回 (β1, V) 曲线。
/// 用途：验证策略暴露真实的 attainment/work 权衡（曲线非退化），
/// 以及给 P3 的跨轮默认 β 选择提供证据。
pub fn beta_sweep(t: &ReplayTrace, grid: &[f64]) -> Vec<(f64, f64)> {
    grid.iter().map(|&b| (b, replay_score(t, b, 0.01))).collect()
}

/// 前缀轨迹族：对完整树枚举「在深度 d 截断」的所有前缀。
/// 论文语义：替代策略可以停在更早的点（少花钱）或走满（多探索）。
/// 返回 (截断深度, 轨迹) 序列——d=0（空）到 d=len（全树）。
pub fn prefix_family(node_scores: &[f64]) -> Vec<(usize, ReplayTrace)> {
    (0..=node_scores.len())
        .map(|d| {
            (
                d,
                ReplayTrace::serial(node_scores[..d].to_vec()),
            )
        })
        .collect()
}

/// 永不退化条款（论文 §3 Policy selection）：候选集含 π0，
/// 胜者分数 ≥ π0 分数。返回 (胜者索引, 是否与 π0 并列)。
pub fn select_never_worse(scores: &[f64]) -> (usize, bool) {
    debug_assert!(!scores.is_empty(), "候选集非空（π0 必在）");
    let mut best_i = 0;
    let mut best_v = f64::NEG_INFINITY;
    for (i, &v) in scores.iter().enumerate() {
        if v > best_v {
            best_v = v;
            best_i = i;
        }
    }
    (best_i, best_i == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eq1_basic_arithmetic() {
        // 5 节点全 Ok（分=1），串行：V = 1 − .02*5 + .01*5/5 = 0.91
        let t = ReplayTrace::serial(vec![1.0; 5]);
        assert!((replay_score(&t, 0.02, 0.01) - 0.91).abs() < 1e-9);
        // 并行 1 轮 5 节点：V = 1 − .1 + .01*5/1 = 0.95
        let t2 = ReplayTrace {
            node_scores: vec![1.0; 5],
            batch_sizes: vec![5],
        };
        assert!((replay_score(&t2, 0.02, 0.01) - 0.95).abs() < 1e-9);
        // 并行度奖励生效：同 N 下批越大 V 越高
        assert!(replay_score(&t2, 0.02, 0.01) > replay_score(&t, 0.02, 0.01));
    }

    #[test]
    fn empty_and_single() {
        let e = ReplayTrace::serial(vec![]);
        assert_eq!(replay_score(&e, 0.02, 0.01), 0.0);
        let s = ReplayTrace::serial(vec![0.5]);
        // 0.5 - 0.02 + 0.01 = 0.49
        assert!((replay_score(&s, 0.02, 0.01) - 0.49).abs() < 1e-9);
    }

    #[test]
    fn best_ignores_order_failures_scored() {
        // 失败节点 0 分仍占 N（工作已花），但 best 只看最大
        let t = ReplayTrace::serial(vec![0.0, 0.8, 0.0, 0.9]);
        assert!((t.best() - 0.9).abs() < 1e-9);
        assert_eq!(t.n_probes(), 4);
    }

    #[test]
    fn beta_sweep_monotone_down_in_beta1() {
        let t = ReplayTrace::serial(vec![1.0; 10]);
        let curve = beta_sweep(&t, &[0.0, 0.01, 0.02, 0.05]);
        for w in curve.windows(2) {
            assert!(w[1].1 <= w[0].1, "β1 越大罚越重，V 单调不增");
        }
    }

    #[test]
    fn prefix_family_full_coverage() {
        let fam = prefix_family(&[0.3, 0.9, 0.5]);
        assert_eq!(fam.len(), 4);
        assert_eq!(fam[0].1.n_probes(), 0);
        assert_eq!(fam[3].1.n_probes(), 3);
        // 存在中间截断优于全树的可能（β1 罚 > 边际收益时）
        let full = replay_score(&fam[3].1, 0.2, 0.01);
        let cut = replay_score(&fam[2].1, 0.2, 0.01);
        // [0.3,0.9] 截断：0.9-0.4+0.005=0.505；全树 [0.3,0.9,0.5]: 0.9-0.6+0.005=0.305
        assert!(cut > full);
    }

    #[test]
    fn select_never_worse_guarantees_pi0() {
        // 全部更差 → 选 π0（索引 0）
        let (i, tie) = select_never_worse(&[0.5, 0.4, 0.3]);
        assert_eq!((i, tie), (0, true));
        // 有更优 → 选优
        let (i2, _) = select_never_worse(&[0.5, 0.7]);
        assert_eq!(i2, 1);
        // 并列 → 保守取 π0（先到先得，i=0 已是 π0 时不变）
        let (i3, tie3) = select_never_worse(&[0.6, 0.6]);
        assert_eq!((i3, tie3), (0, true));
    }

    #[test]
    fn avg_batch_semantics() {
        let t = ReplayTrace {
            node_scores: vec![1.0; 6],
            batch_sizes: vec![2, 2, 2],
        };
        assert!((t.avg_batch() - 2.0).abs() < 1e-9);
        assert_eq!(t.k_rounds(), 3);
    }
}
