//! attractor 纯核热路径基准（v9 D3 门：p99 ≤ 35ms）。
//! 测的是内核责任面：payload parse + replay + decide——不含进程启动/磁盘/链验签
//! （那些是 CLI 一次性成本，不是热路径责任面）。

use crate::kernel::attractor::*;

fn bench_params() -> Params {
    Params { formula_version: FORMULA_VERSION.into(), lambda: 0.5, tau: 8.0,
             categories: vec!["A".into(), "B".into()] }
}

/// 构造 200 run 账本流（encode→parse 全真路径）
fn bench_payloads(n: usize) -> Vec<DecisionPayload> {
    let mut ps = Vec::new();
    for k in 0..n {
        let p = DecisionPayload {
            lambda: 0.5, tau: 8.0, categories: vec!["A".into(), "B".into()],
            evidence: vec![("A".into(), 0.4), ("B".into(), 0.6)],
            m: 0.0, n: 0, memory_a: None,
            winner: Some("B".into()), tie: false,
            prev_verdict: if k == 0 { None } else { Some("verified".into()) },
            input_digest: format!("fnv1a64:{k:016x}"),
        };
        ps.push(DecisionPayload::parse(&p.encode()).unwrap());
    }
    ps
}

#[test]
fn hotpath_replay_decide_p99_under_35ms() {
    let payloads = bench_payloads(200);
    let params = bench_params();
    let ev = Evidence([("A".into(), 0.4), ("B".into(), 0.6)].into_iter().collect());
    // 预热
    let _ = replay_memory(&payloads);
    let _ = decide(&params, &MemoryState::default(), &ev).unwrap();
    // 1000 次全链（replay 200 run + decide）
    let mut lat = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let t0 = std::time::Instant::now();
        let mem = replay_memory(&payloads);
        let out = decide(&params, &mem, &ev).unwrap();
        lat.push(t0.elapsed().as_nanos() as f64 / 1e6);
        assert_eq!(out.winner.as_deref(), Some("B"));
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = lat[499];
    let p99 = lat[989];
    let mx = lat[999];
    eprintln!("HOTPATH p50={:.4}ms p99={:.4}ms max={:.4}ms (200-run replay + decide, N=1000)", p50, p99, mx);
    assert!(p99 <= 35.0, "p99={:.4}ms exceeds 35ms gate (200-run replay+decide)", p99);
}
