//! attractor v9 D3 试点 cell：黑盒 CLI 真进程闭环（test-only）。
//!
//! 数学速查（λ=0.5, τ=8, 记忆在 B, g=(A=0.85, B=0.6)）：
//!   B 保持条件：0.6·(1+0.5·m) > 0.85 ⟺ m > 0.8333 ⟺ n < 8·ln(1.2) ≈ 1.46
//!   → n=0,1 时 B 拉住；n≥2 时 A 翻案（证据主政）
//! 关键断言：tie 冷启动→no-Decision / 记忆形成 / 满血拉住 / 衰减翻案 /
//!           弱记忆破平（tie 输入但 a 在场）/ 热路径粗门。

use std::process::Command;
use std::time::Instant;

fn ductile() -> String {
    let d = format!("{}/target/release/ductile", env!("CARGO_MANIFEST_DIR"));
    assert!(std::path::Path::new(&d).exists(), "release 二进制不存在——先 cargo build --release");
    d
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("dt-att-d3-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(ductile()).args(args).output().expect("spawn ductile");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

struct Cell {
    ledger: std::path::PathBuf,
    secret: std::path::PathBuf,
}

fn setup(tag: &str) -> Cell {
    let d = tmpdir(tag);
    let led = d.join("cell.mlv");
    let keydir = d.join("keys");
    let (_, _, _) = run(&["mlv", "x", "keygen", "--out", keydir.to_str().unwrap()]);
    let secret = {
        let mut s = None;
        for e in std::fs::read_dir(&keydir).unwrap() {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            if name.ends_with(".secret") { s = Some(keydir.join(name)); }
        }
        s.unwrap()
    };
    let (rc, out, err) = run(&["ledger", "create", led.to_str().unwrap(), "--root-key", secret.to_str().unwrap()]);
    assert_eq!(rc, 0, "create rc={rc} out={out} err={err}");
    Cell { ledger: led, secret }
}

fn decide(c: &Cell, ga: f64, gb: f64, verdict: Option<&str>) -> (i32, String) {
    let ev = format!("A={ga},B={gb}");
    let mut args = vec![
        "attractor".to_string(), "decide".to_string(),
        c.ledger.to_str().unwrap().to_string(), "cell".to_string(), "0".to_string(),
        "0.5".to_string(), "8.0".to_string(), "A,B".to_string(), ev,
    ];
    if let Some(v) = verdict { args.push(v.to_string()); }
    args.push("--signing-key".to_string());
    args.push(c.secret.to_str().unwrap().to_string());
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (rc, out, err) = run(&refs);
    if rc != 0 { eprintln!("decide rc={rc} err={err}"); }
    (rc, out)
}

fn field(out: &str, k: &str) -> String {
    for line in out.lines() {
        if let Some(v) = line.strip_prefix(&format!("{k}: ")) { return v.trim().to_string(); }
    }
    String::new()
}

#[test]
fn d3_pilot_cell_full_cycle() {
    let c = setup("cycle");

    // ── 阶段0：冷启动 tie → no-Decision（无记忆，平票禁随机破平）──
    {
        let (rc, out) = decide(&c, 1.0, 1.0, None);
        assert_eq!(rc, 0);
        assert!(out.contains("no-Decision"), "冷启动平票应 no-Decision: {out}");
        assert_eq!(field(&out, "a"), "-");
    }

    // ── 阶段1：B 稳赢 5 run 全 verified → a=B 满血 ──
    let mut last_a = String::new();
    for i in 0..5 {
        let (rc, out) = decide(&c, 0.4, 0.6, if i == 0 { None } else { Some("verified") });
        assert_eq!(rc, 0, "run{i}");
        assert_eq!(field(&out, "winner"), "B", "run{i} 应 B 赢: {out}");
        last_a = field(&out, "a");
    }
    assert_eq!(last_a, "B", "五连 verified 后 a 应=B");

    // ── 阶段2：满血记忆拉住（0.6·1.5=0.9 > 0.85，反证不足）──
    {
        let (rc, out) = decide(&c, 0.85, 0.6, Some("verified"));
        assert_eq!(rc, 0);
        assert_eq!(field(&out, "winner"), "B", "满血记忆应拉住 B（0.9>0.85）: {out}");
        assert_eq!(field(&out, "a"), "B");
        assert_eq!(field(&out, "n"), "0");
    }

    // ── 阶段3：rejected 流 → n 推进，n=1 仍拉住（m=0.8825, q_B=0.865>0.85）──
    {
        let (rc, out) = decide(&c, 0.85, 0.6, Some("rejected"));
        assert_eq!(rc, 0);
        assert_eq!(field(&out, "winner"), "B", "n=1 衰减中仍拉住: {out}");
        assert_eq!(field(&out, "n"), "1");
    }
    // ── 阶段4：n=2 → m=e^(-0.25)=0.779, q_B=0.834<0.85 → A 翻案（证据主政）──
    {
        let (rc, out) = decide(&c, 0.85, 0.6, Some("rejected"));
        assert_eq!(rc, 0);
        assert_eq!(field(&out, "winner"), "A", "n=2 衰减后证据主政翻案: {out}");
        assert_eq!(field(&out, "a"), "B", "a 只被 verified 改变，rejected 不动 a");
        assert_eq!(field(&out, "n"), "2");
    }
    // ── 阶段5：继续 rejected → n 递增，A 持续赢 ──
    for i in 3..6 {
        let (rc, out) = decide(&c, 0.85, 0.6, Some("rejected"));
        assert_eq!(rc, 0, "rejected n={i}");
        assert_eq!(field(&out, "winner"), "A");
        assert_eq!(field(&out, "n"), i.to_string());
    }

    // ── 阶段6：弱记忆破平——tie 证据 (1.0,1.0) 但 a=B 在场 → B 赢（非平票）──
    {
        let (rc, out) = decide(&c, 1.0, 1.0, Some("rejected"));
        assert_eq!(rc, 0);
        assert_eq!(field(&out, "winner"), "B", "a=B 弱记忆应破平（m>0）: {out}");
        assert!(!out.contains("no-Decision"));
    }

    // ── 阶段7：同输入不同 run 槽位 = 合法新 run（run-slot 幂等语义）──
    {
        let (rc, out) = decide(&c, 1.0, 1.0, Some("rejected"));
        assert_eq!(rc, 0, "同输入不同槽位应合法入账: {out}");
    }

    // ── 投影一致性 ──
    {
        let (rc, out, _) = run(&["attractor", "show", c.ledger.to_str().unwrap(), "cell"]);
        assert_eq!(rc, 0, "{out}");
        assert_eq!(field(&out, "a"), "B");
        // runs：1(tie)+5+1+1+1+3+1+1 = 14
        assert_eq!(field(&out, "runs"), "14", "run 计数: {out}");
    }
}

#[test]
fn d3_hotpath_p99_under_35ms() {
    // CLI 20 run 链总耗时 < 2s（粗门；纯核 decide 是 O(|C|) 纯函数，微秒级，
    // p99≤35ms 的精基准由 lib 层 replay+decide 承担，进程开销不属热路径）
    let c = setup("hot");
    let t0 = Instant::now();
    for i in 0..20 {
        let v = if i == 0 { None } else { Some("verified") };
        let (rc, _) = decide(&c, 0.4, 0.6, v);
        assert_eq!(rc, 0, "hot run{i}");
    }
    let dt = t0.elapsed();
    assert!(dt.as_millis() < 2000, "20 run 链 {dt:?} 超 2s 粗门");
}
