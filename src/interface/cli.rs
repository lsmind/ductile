//! CLI dispatch module — shared between binary and Python binding.

use crate::*;
use rusqlite::params;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub fn run(args: &[String]) -> Result<i32, String> {
    // 反馈单#5.2：--version / -V — 版本+构建信息（生产排查"我跑的是哪个构建"）
    if args.len() >= 2 && (args[1] == "--version" || args[1] == "-V") {
        println!(
            "ductile {} (build {})",
            env!("CARGO_PKG_VERSION"),
            option_env!("DUCTILE_BUILD_HASH").unwrap_or("dev")
        );
        return Ok(0);
    }
    if args.len() < 2 {
        print_usage();
        return Ok(1);
    }

    match args[1].as_str() {
        // Core
        "okr" if args.len() >= 3 => okr_compile(&args[2]),
        "check" if args.len() >= 3 => match parse_pipeline_file(&args[2]) {
            Err(e) => {
                eprintln!("{}", e);
                Ok(1)
            }
            Ok(pl) => {
                let errs = check_pipeline(&pl);
                if errs.is_empty() {
                    println!("Type check passed");
                    // v0.22 同构门禁（推模式）：check 通过后自动对 db 注册表查重。
                    // AGENTS.md 第 5 条"写新图前 hyper similar"从自觉变物理强制——
                    // 结构等价的图在 check 期就递到面前，不是等人想起去查。
                    // 语义：提示不阻断（复用是建议不是义务）；db 空则静默（无语料可查）。
                    iso_gate_print(&pl, &args[2]);
                    Ok(0)
                } else {
                    eprintln!("Type check errors:");
                    for e in &errs {
                        eprintln!("  {}", e);
                    }
                    Ok(1)
                }
            }
        },
        "run" if args.len() >= 3 => match split_run_args(&args[3..]) {
            Err(e) => {
                eprintln!("{}", e);
                Ok(1)
            }
            Ok((topic, policy_path, restrict)) => {
                if restrict {
                    std::env::set_var("DUCTILE_RESTRICT_SHELL", "1");
                }
                cmd_run(&args[2], &topic, policy_path.as_deref())
            }
        },
        "graph" if args.len() >= 3 => cmd_graph(&args[2]),
        "parse" if args.len() >= 3 => cmd_parse(&args[2]),

        // Hyper layer — guide graph generation (compile-time), not runtime cycles
        "hyper" if args.len() >= 3 => cmd_hyper(&args[2..]),

        // Database
        "import" if args.len() >= 3 => cmd_import(&args[2..]),
        "search" if args.len() >= 3 => cmd_search(&args[2]),
        "fts" if args.len() >= 3 => cmd_fts(&args[2]),
        "compose" if args.len() >= 5 => cmd_compose(&args[2], &args[3], &args[4..]),
        "db-stats" => cmd_db_stats(),
        // v0.20 Replay-RSI P1：账本发现树重建（Dream-RSI 式）
        "tree" => crate::L4_structure::replay::cmd_tree(&args[2..]),
        // v0.20 Replay-RSI P3：tentative patch 重放门（永不退化条款）
        "replay" if args.len() >= 3 => crate::L4_structure::replay::cmd_replay(&args[2..]),

        // v0.20 TUI 操作台（四视图：状态/日志库/蓝图/同构；纯读侧）
        "tui" => {
            let path = args.get(2).cloned();
            crate::interface::tui::run_tui(path)?;
            Ok(0)
        }

        // Discovery
        "discover" if args.len() >= 3 => cmd_discover(Some(&args[2])),
        "discover" => cmd_discover(None),
        "learn" if args.len() >= 3 => cmd_learn(&args[2]),
        "learn" => cmd_learn(""),

        // v0.8.1 harvest line
        "doctor" => cmd_doctor(),
        // v0.18.6 P1-3：cost_norm 测量面（PyroDash Eq.6 精神：actual/top-tier
        // 归一化）。只测量不排序——动 effective 公式前必须先盲评（选路环
        // 是唯一活着的自进化环，不能不测就改行为）。
        "cost" if args.len() >= 3 && args[2] == "report" => cmd_cost_report(),
        "wrap" if args.len() >= 5 => {
            // ductile wrap <tag> -- <cmd...>
            cmd_wrap(&args[2], &args[4..].join(" "))
        }
        "harvest" => {
            let days: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(7);
            cmd_harvest(days)
        }
        "grow" => {
            // grow [days] [top_import]
            let days: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
            let top: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(25);
            cmd_grow(days, top)
        }
        "scaffold" => {
            // scaffold [query...] — 列出/搜索已铸成的构式模板
            let q = args[2..].join(" ");
            cmd_scaffold(&q)
        }
        "promote" => {
            let (days, top, dry) = parse_promote_args(&args[2..]);
            cmd_promote(days, top, dry)
        }
        "degraded" if args.len() >= 3 && args[2] == "clear" && args.len() >= 4 => {
            Ok(if harvest::clear_degraded(&args[3]) {
                0
            } else {
                1
            })
        }
        "degraded" => {
            for f in harvest::list_degraded() {
                eprintln!("DEGRADED: {}", f);
            }
            Ok(0)
        }

        // Version
        // v0.19 探索环（explore-then-freeze）：curriculum 出题→沙箱探针→
        // 确定性裁判→固化三通道。--drs 只深探；--report 回看报告。
        "explore" if args.len() >= 4 && args[2] == "--report" => cmd_explore_report("", &args[3]),
        "explore" if args.len() >= 4 => {
            // ductile explore <file> <topic> [--drs]
            let drs_only = args.len() >= 5 && args[4] == "--drs";
            cmd_explore(&args[2], &args[3], drs_only)
        }
        "version" if args.len() >= 5 && args[2] == "save" => cmd_version_save(&args[3], &args[4]),
        "archive" if args.len() >= 2 => cmd_archive(),
        "version" if args.len() >= 4 && args[2] == "log" => cmd_version_log(&args[3]),
        "version" if args.len() >= 6 && args[2] == "diff" => {
            cmd_version_diff(&args[3], &args[4], &args[5])
        }

        // Hot patch: patch <pipeline> <proc> <impl> <field> <value>
        "patch" if args.len() >= 3 && args[2] == "list" => cmd_patch_list(),
        "patch" if args.len() >= 4 && args[2] == "clear" => cmd_patch_clear(&args[3]),
        // v0.18.6 P1-1：生命周期迁移（进化环生效/证伪的唯一通道）
        "patch" if args.len() >= 4 && args[2] == "confirm" => {
            cmd_patch_transition(&args[3], "confirmed")
        }
        "patch" if args.len() >= 4 && args[2] == "revert" => {
            cmd_patch_transition(&args[3], "reverted")
        }
        "patch" if args.len() >= 7 => {
            cmd_patch_set(&args[2], &args[3], &args[4], &args[5], &args[6])
        }

        // v0.12 script contract line — 脚本即 API
        // v0.15 canary 输入库（cognition spec §7 缺口 #2）
        "canary" if args.len() >= 3 && args[2] == "list" => {
            cmd_canary_list(args.get(3).map(|s| s.as_str()))
        }
        "canary" if args.len() >= 3 && args[2] == "add" && args.len() >= 6 => {
            cmd_canary_add(&args[3..])
        }
        "canary" if args.len() >= 3 && args[2] == "rm" && args.len() >= 4 => {
            cmd_canary_rm(&args[3])
        }
        "canary" if args.len() >= 3 && args[2] == "pass" && args.len() >= 5 => {
            cmd_canary_pass(&args[3], &args[4])
        }
        // v0.15 incident 一等实体（缺口 #3）
        "incident" if args.len() >= 3 && args[2] == "list" => {
            cmd_incident_list(args.get(3).map(|s| s.as_str()))
        }
        "incident" if args.len() >= 3 && args[2] == "close" && args.len() >= 5 => {
            cmd_incident_close(&args[3], &args[4..].join(" "))
        }
        // v0.18.6 P0-刀2：判别实验真重跑（canary 快照 → bridge → pass_rate → triage 回写）
        "incident" if args.len() >= 4 && args[2] == "triage" => cmd_incident_triage(&args[3]),
        // v0.15 L4 端到端复核（缺口 #4，冷启动 log-only）
        "l4" if args.len() >= 3 && args[2] == "list" => cmd_l4_list(),
        // v0.18.6 P1-2：盲评自动打标（校准闭环的标签注入通道）
        "l4" if args.len() >= 4 && args[2] == "calibrate" => {
            // v0.18.8：prefix/source 参数化（缺省 REGCHAIN/blind:regcheck3 旧语义兼容）
            let prefix = args.get(4).map(|s| s.as_str()).unwrap_or("REGCHAIN");
            let source = args.get(5).map(|s| s.as_str()).unwrap_or("blind:regcheck3");
            cmd_l4_calibrate(&args[3], prefix, source)
        }
        "l4" if args.len() >= 3 && args[2] == "status" => cmd_l4_status(),
        "l4" if args.len() >= 3 && args[2] == "review" && args.len() >= 6 => {
            cmd_l4_review(&args[3], &args[4], &args[5..].join(" "))
        }
        "l4" if args.len() >= 3 && args[2] == "label" && args.len() >= 5 => {
            cmd_l4_label(&args[3], &args[4])
        }
        // v0.15 判别实验搁置队列（缺口 #5）
        "shelve" if args.len() >= 3 && args[2] == "list" => {
            cmd_shelve_list(args.get(3).map(|s| s.as_str()))
        }
        "shelve" if args.len() >= 3 && args[2] == "resolve" && args.len() >= 5 => {
            cmd_shelve_resolve(&args[3], &args[4..].join(" "))
        }
        "script" if args.len() >= 3 && args[2] == "attach" && args.len() >= 4 => {
            cmd_script_attach(&args[3])
        }
        "script" if args.len() >= 3 && args[2] == "detach" && args.len() >= 4 => {
            cmd_script_detach(&args[3])
        }
        "script" if args.len() >= 3 && args[2] == "list" => cmd_script_list(),
        "script" if args.len() >= 3 && args[2] == "doctor" => cmd_script_doctor(),
        "script" if args.len() >= 3 && args[2] == "show" && args.len() >= 4 => {
            cmd_script_show(&args[3])
        }
        "script" if args.len() >= 3 && args[2] == "call" && args.len() >= 5 => {
            cmd_script_call(&args[3], &args[4])
        }

        // v0.24 kernel 切片 D12-13：chaos harness 自举 + 证据账本/reproduce
        "kernel-chaos" if args.len() >= 3 => cmd_kernel_chaos(&args[2..]),
        "kernel-ledger" if args.len() >= 3 => cmd_kernel_ledger(&args[2..]),
        "kernel-reproduce" if args.len() >= 3 => cmd_kernel_reproduce(&args[2..]),

        // v0.24 自组织治理超图 MLV（sonet 第四轮实施）
        "kernel-binding" if args.len() >= 4 => cmd_kernel_binding(&args[2..]),

        // v0.24 MLV v3.1 治理内核（旧动词输出字节冻结；新动词新格式）
        "mlv" if args.len() >= 3 => cmd_mlv(&args[2..]),

        // v0.23 深题手册：`ductile help <topic>` 把引擎行为写进二进制，
        // 终端即得——不再逼使用者进 Rust 源码排障（反馈单第 3 条）。
        "help" if args.len() >= 3 => cmd_help_topic(&args[2..].join(" ")),
        "help" => {
            print_usage();
            Ok(0)
        }

        _ => {
            print_usage();
            Ok(1)
        }
    }
}

// ── v0.24 kernel D12-13：chaos 自举 / 证据账本 / reproduce 包 ──────────

/// `ductile kernel-chaos <dir> [seeds]` — 六 killpoint × seeds 混沌注入。
/// 产物：dir/verdicts.jsonl + dir/ledger.jsonl（每判定一事件+汇总事件）。
/// exit 0 = 四判据全过（CHAOS-PASS）；任一违例 exit 1（fail-closed）。
fn cmd_kernel_chaos(args: &[String]) -> Result<i32, String> {
    let dir = &args[0];
    let seeds: u64 = match args.get(1) {
        Some(s) => s.parse().map_err(|_| format!("seeds must be a number, got '{s}'"))?,
        None => 1,
    };
    if seeds == 0 {
        return Err("seeds must be >= 1".into());
    }
    let base = std::path::PathBuf::from(dir);
    let verdicts =
        crate::kernel::chaos::chaos_all(&base, seeds).map_err(|e| format!("chaos: {e}"))?;
    let mut lines = String::new();
    let mut fails = 0usize;
    for v in &verdicts {
        let ok = v.duplicate_effects == 0 && v.chain_ok && v.results_match && v.resumed;
        if !ok {
            fails += 1;
        }
        lines.push_str(&format!(
            "{{\"killpoint\":\"{}\",\"seed\":{},\"duplicate_effects\":{},\"chain_ok\":{},\"results_match\":{},\"resumed\":{},\"side_effects_total\":{},\"pass\":{}}}\n",
            v.killpoint, v.seed, v.duplicate_effects, v.chain_ok, v.results_match, v.resumed, v.side_effects_total, ok
        ));
    }
    std::fs::write(base.join("verdicts.jsonl"), &lines).map_err(|e| e.to_string())?;
    // 证据账本：每判定一事件 + 汇总事件（payload=verdicts 全文，digest 入链）
    let ledger = base.join("ledger.jsonl");
    for v in &verdicts {
        let payload = format!(
            "{}|{}|{}|{}|{}|{}",
            v.killpoint, v.seed, v.duplicate_effects, v.chain_ok, v.results_match, v.resumed
        );
        crate::kernel::ledger::append_event(
            &ledger, "chaos_verdict", v.killpoint, v.seed, payload.as_bytes(), "kernel-chaos",
        )?;
    }
    let head = crate::kernel::ledger::append_event(
        &ledger, "chaos_run", "summary", seeds, lines.as_bytes(), "kernel-chaos",
    )?;
    println!(
        "chaos: {} verdicts ({} killpoints x {} seeds), fails={}",
        verdicts.len(),
        crate::kernel::chaos::KillPoint::all().len(),
        seeds,
        fails
    );
    println!("ledger head: {head}");
    if fails > 0 {
        eprintln!("CHAOS-FAIL: {fails} verdict(s) violated criteria");
        return Ok(1);
    }
    println!("CHAOS-PASS");
    Ok(0)
}

/// `ductile kernel-ledger <file> --verify` — 验证证据账本哈希链。
fn cmd_kernel_ledger(args: &[String]) -> Result<i32, String> {
    if !args.iter().any(|a| a == "--verify") {
        return Err("usage: ductile kernel-ledger <file> --verify".into());
    }
    let path = std::path::PathBuf::from(&args[0]);
    match crate::kernel::ledger::verify_ledger(&path) {
        Ok((n, head)) => {
            println!("LEDGER-OK events={n} head={head}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("LEDGER-BROKEN: {e}");
            Ok(1)
        }
    }
}

/// `ductile kernel-reproduce <dir> -o out.tar.zst`（打包）
/// `ductile kernel-reproduce --verify <dir>`（解包后离线验证）
fn cmd_kernel_reproduce(args: &[String]) -> Result<i32, String> {
    if args[0] == "--verify" {
        if args.len() < 2 {
            return Err("usage: ductile kernel-reproduce --verify <dir>".into());
        }
        let dir = std::path::PathBuf::from(&args[1]);
        match crate::kernel::ledger::verify_reproduce(&dir) {
            Ok((files, events, head)) => {
                println!("REPRODUCE-OK files={files} ledger_events={events} head={head}");
                Ok(0)
            }
            Err(e) => {
                eprintln!("REPRODUCE-BROKEN: {e}");
                Ok(1)
            }
        }
    } else {
        let dir = std::path::PathBuf::from(&args[0]);
        let out = args
            .iter()
            .position(|a| a == "-o")
            .and_then(|i| args.get(i + 1))
            .ok_or("usage: ductile kernel-reproduce <dir> -o out.tar.zst")?;
        if !dir.join("ledger.jsonl").exists() {
            return Err(format!(
                "no ledger.jsonl in {} — run `ductile kernel-chaos {}` first",
                dir.display(),
                dir.display()
            ));
        }
        // MANIFEST.json = dir 内全部产物逐文件 sha256（不含 MANIFEST 自身）
        let entries = crate::kernel::ledger::dir_manifest(&dir)
            .map_err(|e| format!("manifest: {e}"))?;
        std::fs::write(
            dir.join("MANIFEST.json"),
            crate::kernel::ledger::manifest_json(&entries),
        )
        .map_err(|e| e.to_string())?;
        // 打包走系统 tar --zstd（kernel 保持零新依赖）
        let status = std::process::Command::new("tar")
            .args([
                "--zstd",
                "-cf",
                out,
                "-C",
                dir.to_str().unwrap_or("."),
                ".",
            ])
            .status()
            .map_err(|e| format!("spawn tar: {e} (tar with zstd required)"))?;
        if !status.success() {
            return Err(format!("tar exited with {status}"));
        }
        let meta = std::fs::metadata(out).map_err(|e| e.to_string())?;
        println!(
            "REPRODUCE-PACKED {} bytes -> {} ({} files)",
            meta.len(),
            out,
            entries.len()
        );
        Ok(0)
    }
}

/// v0.24 自组织治理超图 MLV 子命令。
/// `ductile mlv <ledger> <verb> ...` — v3.1 治理内核。
fn cmd_mlv(args: &[String]) -> Result<i32, String> {
    use crate::kernel::gov::{ApplyOutcome, GovErr, GovRegistry};
    use crate::kernel::mlv::{effect_key, LedgerRecord, MlvOp, MlvState, DEFAULT_SKEW_NS, DOMAIN_RECORD, domain_hash, GENESIS_ROOT, DOMAIN_RECEIPT};
    let ledger_path = std::path::PathBuf::from(&args[0]);
    let verb = args[1].as_str();
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    fn emit_outcome(r: Result<ApplyOutcome, GovErr>, label: &str, rev: u64) -> Result<i32, String> {
        match r {
            Ok(ApplyOutcome::Committed { record_hash, .. }) => {
                println!("OK {label} rev={rev} record={record_hash}");
                Ok(0)
            }
            Ok(ApplyOutcome::Acked { result }) => {
                println!("ACK {label} rev={rev} code={}", result.get("code").unwrap_or(&"OK".into()));
                Ok(0)
            }
            Err(e) => {
                eprintln!("{} {}", e.code(), e.detail());
                Ok(1)
            }
        }
    }

    fn mkrec(seq: u64, op: MlvOp, binding: &str, rev: u64, from: Option<MlvState>, to: Option<MlvState>, prev: &str, request_key: &str, at: u64, payload: &str) -> LedgerRecord {
        // f7 冻结：request_digest=D(record域, op|request_key|payload)——payload 入 digest 不入 effect_key
        let digest = domain_hash(DOMAIN_RECORD, format!("{op:?}|{request_key}|{payload}").as_bytes());
        let env = crate::kernel::mlv::make_envelope(&op, binding, rev, request_key, &digest, at);
        let mac = env.get("mac").cloned().unwrap_or_default();
        LedgerRecord {
            schema: 1, seq, op,
            key_id: "genesis".into(), root_commitment: GENESIS_ROOT.into(),
            effect_key: effect_key(op, binding, rev, request_key).unwrap(),
            idempotency_scope: None, caller_id: Some("cli".into()),
            request_key: Some(request_key.into()),
            request_digest: digest,
            binding_id: Some(binding.into()), revision: Some(rev),
            from, to, before_record_hash: prev.into(), accepted_at_ns: at,
            nonce: Some(format!("nonce-{seq}-{at}")), envelope_digest: Some(mac), envelope: Some(env),
            payload: if payload.is_empty() { format!("{op:?}|{binding}|{rev}") } else { payload.to_string() }, registry_receipt: None,
            result: [("code".into(), "OK".into())].into_iter().collect(),
            record_hash: String::new(),
        }
    }

    let op_of = |v: &str| -> Result<MlvOp, String> {
        MlvOp::from_name(&v.to_uppercase().replace('-', "_")).map_err(|e| e.to_string())
    };

    match verb {
        "init" => {
            match GovRegistry::init(&ledger_path, now_ns) {
                Ok(reg) => {
                    let h = reg.ledger.head().unwrap_or_default();
                    println!("OK init v1 head={h}");
                    Ok(0)
                }
                Err(e) => { eprintln!("{} {}", e.code(), e.detail()); Ok(1) }
            }
        }
        "create" => {
            let binding = args[2].clone();
            let rev: u64 = args[3].parse().map_err(|_| "rev must be number")?;
            let rk = args.get(4).cloned().unwrap_or_else(|| format!("rk-{binding}-{rev}"));
            let mut reg = GovRegistry::open(&ledger_path).map_err(|e| format!("{} {}", e.code(), e.detail()))?;
            let prev = reg.head().map_err(|e| e.to_string())?;
            let seq = reg.record_count();
            let rec = mkrec(seq, MlvOp::CreateProposal, &binding, rev, None, Some(MlvState::Proposed), &prev, &rk, now_ns, "");
            emit_outcome(reg.submit(rec, now_ns), "create", rev)
        }
        "append" => {
            // f3 冻结规格：生产 CLI 去 fixture 化——直写账本路径，缺账本=ledger-missing
            // （夹具初始化由管线 harness 显式 ductile mlv <path> init 完成）
            let path = ledger_path.clone();
            let mut op = None; let mut binding = String::new(); let mut rev: u64 = 0;
            let mut rk = String::new(); let mut at = now_ns;
            let mut payload = String::new();
            let mut i = 2;
            while i + 1 < args.len() + 1 && i < args.len() {
                match args[i].as_str() {
                    "--op" if i + 1 < args.len() => { op = Some(args[i + 1].clone()); i += 2; }
                    "--binding" if i + 1 < args.len() => { binding = args[i + 1].clone(); i += 2; }
                    "--rev" if i + 1 < args.len() => { rev = args[i + 1].parse().map_err(|_| "rev number")?; i += 2; }
                    "--rk" if i + 1 < args.len() => { rk = args[i + 1].clone(); i += 2; }
                    "--at" if i + 1 < args.len() => { at = args[i + 1].parse().map_err(|_| "at number")?; i += 2; }
                    "--payload" if i + 1 < args.len() => { payload = args[i + 1].clone(); i += 2; }
                    _ => return Err(format!("bad flags near {}", args[i])),
                }
            }
            let op = MlvOp::from_name(&op.ok_or("--op required")?).map_err(|e| e.to_string())?;
            // f3：缺账本=ledger-missing（无自动 init；管线 harness 负责夹具）
            if !path.exists() {
                eprintln!("E424 ledger-missing");
                return Ok(1);
            }
            let mut reg = GovRegistry::open(&path).map_err(|e| format!("{} {}", e.code(), e.detail()))?;
            let prev = reg.head().map_err(|e| e.to_string())?;
            let seq = reg.record_count();
            let (from, to) = crate::kernel::gov::derive_edge(op, &binding, rev, &reg);
            let rec = mkrec(seq, op, &binding, rev, from, to, &prev, &rk, at, &payload);
            emit_outcome(reg.submit(rec, at), op.name().to_lowercase().as_str(), rev)
        }
        "registry-confirm" => {
            let binding = args[2].clone();
            let rev: u64 = args[3].parse().map_err(|_| "rev number")?;
            let exp_in: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(2 * DEFAULT_SKEW_NS);
            let mut reg = GovRegistry::open(&ledger_path).map_err(|e| format!("{} {}", e.code(), e.detail()))?;
            let prev = reg.head().map_err(|e| e.to_string())?;
            let seq = reg.record_count();
            let mut rec = mkrec(seq, MlvOp::RegistryConfirm, &binding, rev, None, None, &prev, "rk-confirm", now_ns, "");
            let mut rcpt = std::collections::BTreeMap::new();
            rcpt.insert("receipt_id".into(), format!("rcpt-{binding}-{rev}-{now_ns}"));
            rcpt.insert("issued_at_ns".into(), now_ns.to_string());
            rcpt.insert("expires_at_ns".into(), (now_ns + exp_in).to_string());
            rcpt.insert("root".into(), domain_hash(DOMAIN_RECEIPT, binding.as_bytes()));
            rec.registry_receipt = Some(rcpt);
            emit_outcome(reg.submit(rec, now_ns), "receipt", rev)
        }
        "begin" | "activate-begin" => {
            let binding = args[2].clone();
            let rev: u64 = args[3].parse().map_err(|_| "rev number")?;
            let mut reg = GovRegistry::open(&ledger_path).map_err(|e| format!("{} {}", e.code(), e.detail()))?;
            let prev = reg.head().map_err(|e| e.to_string())?;
            let seq = reg.record_count();
            let rec = mkrec(seq, MlvOp::ActivateBegin, &binding, rev, Some(MlvState::Decided), Some(MlvState::Activating), &prev, "rk-begin", now_ns, "");
            emit_outcome(reg.submit(rec, now_ns), "begin", rev)
        }
        "commit" | "activate-commit" => {
            let binding = args[2].clone();
            let rev: u64 = args[3].parse().map_err(|_| "rev number")?;
            let mut reg = GovRegistry::open(&ledger_path).map_err(|e| format!("{} {}", e.code(), e.detail()))?;
            let prev = reg.head().map_err(|e| e.to_string())?;
            let seq = reg.record_count();
            let (from, to) = crate::kernel::gov::derive_edge(MlvOp::ActivateCommit, &binding, rev, &reg);
            let rec = mkrec(seq, MlvOp::ActivateCommit, &binding, rev, from, to, &prev, "rk-commit", now_ns, "");
            emit_outcome(reg.submit(rec, now_ns), "commit", rev)
        }
        "abandon" => {
            let binding = args[2].clone();
            let rev: u64 = args[3].parse().map_err(|_| "rev number")?;
            let mut reg = GovRegistry::open(&ledger_path).map_err(|e| format!("{} {}", e.code(), e.detail()))?;
            let prev = reg.head().map_err(|e| e.to_string())?;
            let seq = reg.record_count();
            let rec = mkrec(seq, MlvOp::Abandon, &binding, rev, Some(MlvState::Activating), Some(MlvState::Decided), &prev, "rk-abandon", now_ns, "");
            emit_outcome(reg.submit(rec, now_ns), "abandon", rev)
        }
        "revoke" | "terminal" | "quarantine" | "investigate" | "repair" | "grant" | "decide" | "decision" => {
            let op = op_of(if verb == "decide" { "decision" } else { verb })?;
            let binding = args[2].clone();
            let rev: u64 = args[3].parse().map_err(|_| "rev number")?;
            let mut reg = GovRegistry::open(&ledger_path).map_err(|e| format!("{} {}", e.code(), e.detail()))?;
            let prev = reg.head().map_err(|e| e.to_string())?;
            let seq = reg.record_count();
            let (from, to) = crate::kernel::gov::derive_edge(op, &binding, rev, &reg);
            let rec = mkrec(seq, op, &binding, rev, from, to, &prev, &format!("rk-{verb}"), now_ns, "");
            emit_outcome(reg.submit(rec, now_ns), if verb == "decide" { "decision" } else { verb }, rev)
        }
        "verify" => {
            let reg = GovRegistry::open(&ledger_path).map_err(|e| format!("{} {}", e.code(), e.detail()))?;
            let head = reg.head().map_err(|e| e.to_string())?;
            println!("OK ledger head={head}");
            Ok(0)
        }
        "status" => {
            let binding = args[2].clone();
            let reg = GovRegistry::open(&ledger_path).map_err(|e| format!("{} {}", e.code(), e.detail()))?;
            match reg.state_of(&binding) {
                Some((rev, st)) => {
                    println!("binding={} current_rev={} state={} stop_gen={}", binding, rev, st.name(), reg.stop_gen(&binding));
                    Ok(0)
                }
                None => { eprintln!("binding not found: {binding}"); Ok(1) }
            }
        }
        _ => Err(format!("unknown mlv verb: {verb}")),
    }
}

/// `ductile kernel-binding <ledger> propose <binding_id> <revision> <actor>`
/// `ductile kernel-binding <ledger> advance <binding_id> <revision> <to_state> <actor>`
/// `ductile kernel-binding <ledger> status <binding_id>`
/// 一步一提交（head-CAS 由内部 head() 续接）；每步 exit 0/1 fail-closed。
fn cmd_kernel_binding(args: &[String]) -> Result<i32, String> {
    use crate::kernel::binding::{BindingRegistry, BindingState, BindingTransition, Phase};
    let ledger = std::path::PathBuf::from(&args[0]);
    let sub = args[1].as_str();
    let mut reg = BindingRegistry::open(&ledger).map_err(|e| format!("open: {e}"))?;
    match sub {
        "propose" => {
            if args.len() < 5 {
                return Err("usage: kernel-binding <ledger> propose <binding_id> <revision> <actor>".into());
            }
            let head = reg.head().map_err(|e| format!("head: {e:?}"))?;
            let t = BindingTransition {
                binding_id: args[2].clone(),
                revision: args[3].parse().map_err(|_| "revision must be number")?,
                from_state: BindingState::Proposed,
                to_state: BindingState::Proposed,
                effect_key: crate::kernel::types::EffectKey {
                    plan_fingerprint: format!("gov-proposal-{}", args[2]),
                    effect_index: 0,
                    input_digest: crate::kernel::hash::sha256_hex(
                        format!("{}|{}|propose", args[2], args[3]).as_bytes(),
                    ),
                },
                phase: Phase::Proposal,
                expected_head: head,
                actor: args[4].clone(),
            };
            let payload = format!("proposal|{}|{}", args[2], args[3]);
            match reg.commit(&t, payload.as_bytes()) {
                Ok(o) => {
                    println!("PROPOSED {} rev={} head={}", args[2], args[3], &o.new_head[..16]);
                    Ok(0)
                }
                Err(e) => {
                    eprintln!("PROPOSE-FAIL {:?} (E-code priority: E404>E421>E423>E424>E425>E422>E420)", e);
                    Ok(1)
                }
            }
        }
        "advance" => {
            if args.len() < 6 {
                return Err("usage: kernel-binding <ledger> advance <binding_id> <revision> <to_state> <actor>".into());
            }
            let binding_id = args[2].clone();
            let revision: u64 = args[3].parse().map_err(|_| "revision must be number")?;
            let to = crate::kernel::binding::state_from_name(&args[4])
                .map_err(|e| format!("unknown state: {e}"))?;
            let from = reg
                .state_of(&binding_id, revision)
                .ok_or("binding not found — propose first")?;
            let head = reg.head().map_err(|e| format!("head: {e:?}"))?;
            let phase = match to {
                BindingState::Granted => Phase::Grant,
                BindingState::Decided | BindingState::Rejected => Phase::Decision,
                _ => Phase::Activation,
            };
            let t = BindingTransition {
                binding_id: binding_id.clone(),
                revision,
                from_state: from,
                to_state: to,
                effect_key: crate::kernel::types::EffectKey {
                    plan_fingerprint: format!("gov-{}-{:?}", binding_id, phase),
                    effect_index: 0,
                    input_digest: crate::kernel::hash::sha256_hex(
                        format!("{}|{}|{:?}", binding_id, revision, to).as_bytes(),
                    ),
                },
                phase,
                expected_head: head,
                actor: args[5].clone(),
            };
            let payload = format!("advance|{}|{}|{}", binding_id, revision, to.name());
            match reg.commit(&t, payload.as_bytes()) {
                Ok(o) => {
                    println!("{} {} rev={} head={}", to.name(), binding_id, revision, &o.new_head[..16]);
                    Ok(0)
                }
                Err(e) => {
                    eprintln!("ADVANCE-FAIL {:?} (edge/state/phase/head rules)", e);
                    Ok(1)
                }
            }
        }
        "status" => {
            let binding_id = &args[2];
            match reg.current_revision(binding_id) {
                Some(r) => {
                    let st = reg.state_of(binding_id, r).map(|s| s.name()).unwrap_or("?");
                    println!(
                        "binding={} current_rev={} state={} stop_gen={}",
                        binding_id,
                        r,
                        st,
                        reg.stop_generation(binding_id)
                    );
                    Ok(0)
                }
                None => {
                    eprintln!("binding not found: {binding_id}");
                    Ok(1)
                }
            }
        }
        _ => Err("usage: kernel-binding <ledger> propose|advance|status ...".into()),
    }
}

/// v0.23 `ductile help <topic>`——深题手册。每条 = 一次真实排障的答案，
/// 内容与 SPEC 同源但面向"刚撞上坑的使用者"（现象 → 原因 → 修法）。
fn cmd_help_topic(topic_raw: &str) -> Result<i32, String> {
    // `ductile help <topic> src` — LSP 语义：概念 → 本版本源码定义位置。
    // 行号运行时解析（反映工作树，不写死）；版本 hash 编译期烤入，
    // git show 命令永远钉在构建版本上。
    let mut parts = topic_raw.split_whitespace();
    let topic = parts.next().unwrap_or("topics");
    let sub = parts.next().unwrap_or("");
    if sub == "src" {
        return cmd_help_src(topic);
    }
    if !sub.is_empty() {
        eprintln!(
            "unknown help subcommand '{}' — try: ductile help {} src",
            sub, topic
        );
        return Ok(1);
    }
    let text: Vec<&str> = match topic {
        "args" => vec![
            "ductile help args — 参数怎么从管线流进脚本（v0.23 参数通道）",
            "",
            "引擎传参通道由契约头 `# args:` 声明（默认 env，存量零迁移）：",
            "  env  (默认)  引擎设 DUCTILE_ARG_<NAME> 环境变量；脚本 os.environ 取",
            "  argv         引擎追加 --key=value 位置参数；脚本 sys.argv/\"$@\"/$1 取",
            "  both         双通道同传同值（迁移期兼容）",
            "",
            "attach 期 lint（fail-closed）：声明与脚本体读取方式错配 → 拒绝注册。",
            "现象（错配不拦时会发生的）：参数静默蒸发，脚本落回自身 default",
            "照常跑完——cmd=\"models\" 进去、status 出来，无任何报错。",
            "修法三选一：改脚本读声明通道 / 改声明匹配脚本 / '# args: xxx!' 跳 lint。",
            "",
            "排障口诀：参数没生效 → 先 `ductile script show <name>` 看 args 行，",
            "再 `ductile script call <name> \"k=v\"` 单发（绕开管线看脚本本体的回包）。",
            "env 通道细节：值支持 {topic}/@proc.field/契约 default；引擎注入前剥",
            "一层对称包围引号；嵌套 run 时外层 DUCTILE_ARG_* 会被剥（防污染）。",
        ],
        "quotes" => vec![
            "ductile help quotes — DSL 引号语义（三层引号必炸的地方）",
            "",
            "总原则：DSL 层引号 ≠ shell 层引号，各管各的，不要套娃。",
            "",
            "  script(x, text=\"a b\")        双引号内是字面值，空格安全",
            "  script(x, cfg='{\"a\":\"b\"}')   JSON 参数：外层单引号，内层双引号",
            "  run(\"grep '\\\"x\\\"' file\")      run() 内是 shell 语义，按 shell 规则转义",
            "",
            "三条铁律：",
            "  1. 禁止 echo '@ref' —— @ref 展开后含单引号会撕裂 shell 结构。",
            "     LLM/上游文本要落盘走 write 动词，要读用 read 或 .when 字段。",
            "  2. 三层同引号（\\\"'…'\\\"）必炸——外层换引号种类，别转义套娃。",
            "  3. 多行 @ref 必须整体引号包裹：echo \"@scan\" 也不行（同第 1 条），",
            "     用 write(from=\"@scan\", to=...) 或 read(from=...)。",
            "",
            "排障口诀：bash 语法错 + 消息里看到撕裂的引号 → 90% 是 @ref 进了",
            "echo/run 的引号结构；把数据边改成 write/read 动词，别修引号。",
        ],
        "errflow" => vec![
            "ductile help errflow — 错误分类与自动处置（十五类 → 策略）",
            "",
            "失败不是炸管线，是值：Left 值 §§FIELDS§§err=1§§err_code=…§§ 传播，",
            "关键链（.deliver 闭包）外的旁路失败可容忍。",
            "",
            "  timeout/ratelimit/memory/network → Retry（指数退避；关键节点预算×2）",
            "  auth/data/format/schema         → Switch（自动切备选 impl；auth=标死坏实现）",
            "  data 同输入必再错                → Reroute（跳过剩余重试直接换 impl）",
            "  cancelled/contract/dependency   → Escalate/Exit（fail-fast，别重试）",
            "",
            "排障口诀：先看 err_code 属哪类——Retry 类等它自己退避，Switch 类",
            "查备选 impl 是否存在（plan 里得先有 b 摆着），contract 类是你自己",
            "的契约红灯（参数/输出形状），改管线别改引擎。",
            "注意 err_msg 截断 200 字符：traceback 尾部常被切——单发复现用",
            "ductile script call，拿完整报错再建因果链。",
        ],
        "script" => vec![
            "ductile help script — 脚本契约速查（# ductile: v1 头）",
            "",
            "最小契约头（缺必填键拒绝注册）：",
            "  # ductile: v1",
            "  # name: my_tool            # 字母数字/_/-",
            "  # desc: 一句话",
            "  # lang: python             # python | bash | powershell",
            "  # params: text(str, required), n(int, default=10)",
            "  # output: n(int)",
            "  # pure: true               # 副作用标注",
            "  # idempotent: true",
            "  # concurrency: safe        # safe | serial | exclusive（flock 物理互斥）",
            "  # effects: none            # none | fs | net | system",
            "  # timeout: 60              # 可选 # retries: N",
            "  # args: env                # 可选 v0.23：env|argv|both（help args）",
            "",
            "输出协议：##DSL_RESULT\\nkey=value\\n##DSL_END（stderr 给人看）。",
            "bash 必须 set -euo pipefail（否则静默半成功）。",
            "契约卡存 attach 时的绝对路径——脚本挪窝要重新 attach",
            "（ductile script doctor 列 DEAD 路径）。",
        ],
        "topics" | "index" => vec![
            "ductile help <topic> — 深题手册（v0.23）",
            "",
            "  help args      参数怎么流进脚本：env/argv/both 通道 + 静默回落坑",
            "  help quotes    DSL 引号语义：三层引号、@ref 禁入 echo、JSON 参数写法",
            "  help errflow   错误分类：十五类 err_code → Retry/Switch/Reroute/Exit",
            "  help script    脚本契约头速查：必填键、DSL_RESULT、concurrency 档位",
            "",
            "命令总表：ductile（无参数）。动词全表与引擎语义：SPEC.md。",
        ],
        other => {
            eprintln!(
                "unknown help topic '{}' — try: args | quotes | errflow | script | topics",
                other
            );
            return Ok(1);
        }
    };
    for line in text {
        println!("{}", line);
    }
    Ok(0)
}

// ── v0.23 help <topic> src — 源码锚点表（LSP 式 go-to-definition）──

/// 每题挂 (文件, 符号, 职责一句话)。行号运行时 grep 解析——工作树改了
/// 位置跟着走；未命中退化为符号名（rg -n 仍可定位）。
fn cmd_help_src(topic: &str) -> Result<i32, String> {
    let hash = option_env!("DUCTILE_BUILD_HASH").unwrap_or("dev");
    println!(
        "ductile help {} src — 本版本源码锚点 (v{} @ {})",
        topic,
        env!("CARGO_PKG_VERSION"),
        hash
    );
    println!("  工作树行号实时解析; 钉版本看定义: git show {}:{{file}}", hash);
    println!();
    let anchors: &[(&str, &str, &str)] = match topic {
        "args" => &[
            ("src/core/script_card.rs", "enum ArgsChannel", "通道枚举与 parse（'!' 抑制语义）"),
            ("src/L2_orchestration/script.rs", "fn parse_contract", "契约头解析; args 键读入+lint 触发"),
            ("src/L2_orchestration/script.rs", "fn lint_args_channel", "双向 lint: env声明×argv读取 互拦"),
            ("src/L2_orchestration/steps.rs", "v0.23 参数通道 argv/both", "执行点: argv/both 时附加 --key=value"),
            ("src/L2_orchestration/steps.rs", "DUCTILE_ARG_", "env 注入 + 继承污染剥离"),
            ("src/L0_physical/db.rs", "args_channel", "scripts 表列与迁移"),
        ],
        "quotes" => &[
            ("src/L2_orchestration/script.rs", "fn parse_script_body", "DSL 侧引号感知 tokenizer（script(...) 参数）"),
            ("src/L2_orchestration/textargs.rs", "fn strip_wrapping_quotes", "env 注入前剥对称包围引号"),
            ("src/L2_orchestration/textargs.rs", "fn resolve_vars", "{topic}/@proc.field 解析"),
            ("src/L2_orchestration/steps.rs", "fn exec_write", "write 动词: @ref 落盘的安全通道"),
        ],
        "errflow" => &[
            ("src/L1_feedback/errflow.rs", "pub enum ErrCode", "十五类错误码定义"),
            ("src/L1_feedback/errflow.rs", "pub fn classify", "原始报错 → ErrCode 分类"),
            ("src/L1_feedback/errflow.rs", "pub fn strategy", "ErrCode → Retry/Switch/Reroute/Exit"),
            ("src/L1_feedback/errflow.rs", "pub fn respond", "分类 → 具体响应动作"),
            ("src/L1_feedback/errflow.rs", "fn delay_secs", "Retry 指数退避节奏"),
            ("src/L1_feedback/errflow.rs", "fn propagated", "Left 值跨 proc 传播（err_code 继承根因）"),
        ],
        "script" => &[
            ("src/core/script_card.rs", "pub struct ScriptCard", "契约卡数据结构"),
            ("src/L2_orchestration/script.rs", "fn parse_contract", "契约头解析与必填键校验"),
            ("src/L2_orchestration/script.rs", "pub fn lang_interpreter", "lang → 解释器映射"),
            ("src/L0_physical/db.rs", "fn script_attach_conn", "注册入库（含 args_channel）"),
            ("src/interface/cli.rs", "fn cmd_script_show", "契约卡展示（LLM 读这张卡）"),
        ],
        _ => {
            eprintln!(
                "no source map for '{}' — try: args | quotes | errflow | script",
                topic
            );
            return Ok(1);
        }
    };
    let root = std::env::current_dir().unwrap_or_default();
    for (file, needle, what) in anchors {
        let path = root.join(file);
        let mut found = String::new();
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Some(lno) = content.lines().position(|l| l.contains(needle)) {
                found = format!(":{}", lno + 1);
            }
        }
        println!("  {:<38} {:<34} {}", format!("{}{}", file, found), needle, what);
    }
    Ok(0)
}

// ── v0.12 script contract line — 脚本即 API ──

// ── v0.15 canary / incident CLI（cognition spec §7 缺口 #2/#3）──

fn cmd_canary_list(proc_name: Option<&str>) -> Result<i32, String> {
    let conn = db::open_try()?;
    let rows = canary::list_canaries_conn(&conn, proc_name);
    if rows.is_empty() {
        println!(
            "(no canaries{})",
            proc_name.map(|p| format!(" for '{p}'")).unwrap_or_default()
        );
        return Ok(0);
    }
    for r in rows {
        let has_pass = canary::has_canary_pass_conn(&conn, &r.pipeline, &r.proc_name);
        println!(
            "#{} [{}] {}::{} expect=`{}` pass_record={} note={} input=`{}`",
            r.id,
            if has_pass { "PASS-REC" } else { "NO-REC" },
            r.pipeline,
            r.proc_name,
            r.expect,
            has_pass,
            r.note,
            crate::trunc_chars(&r.input, 60)
        );
    }
    Ok(0)
}

/// canary add <pipeline> <proc> <input> [expect] [note]
fn cmd_canary_add(rest: &[String]) -> Result<i32, String> {
    if rest.len() < 3 {
        return Err("canary add <pipeline> <proc> <input> [expect] [note...]".into());
    }
    let conn = db::open_try()?;
    let expect = rest.get(3).cloned().unwrap_or_default();
    let note = rest.get(4..).map(|v| v.join(" ")).unwrap_or_default();
    let id = canary::add_canary_conn(&conn, &rest[0], &rest[1], &rest[2], &expect, &note)?;
    println!(
        "canary #{} saved (expect=`{}`)",
        id,
        canary::normalize_expect(&expect)
    );
    Ok(0)
}

fn cmd_canary_rm(id_str: &str) -> Result<i32, String> {
    let id: i64 = id_str.parse().map_err(|_| format!("bad id: {id_str}"))?;
    let n = canary::rm_canary_conn(&db::open_try()?, id)?;
    println!("removed {n} canary");
    Ok(0)
}

/// canary pass <pipeline> <proc> — 手动登记一次 canary 通过（真跑由
/// canary run 子命令/工作流承担，这里先落硬门禁的通过记录面）。
fn cmd_canary_pass(pipeline: &str, proc_name: &str) -> Result<i32, String> {
    let conn = db::open_try()?;
    canary::record_canary_run_conn(&conn, pipeline, proc_name, true, "manual")?;
    println!("canary pass recorded: {pipeline}::{proc_name}");
    Ok(0)
}

fn cmd_incident_list(status: Option<&str>) -> Result<i32, String> {
    let rows = incident::list_incidents_conn(&db::open_try()?, status);
    if rows.is_empty() {
        println!(
            "(no incidents{})",
            status.map(|s| format!(" [{s}]")).unwrap_or_default()
        );
        return Ok(0);
    }
    for r in rows {
        println!(
            "#{} [{}]{} {}::{} code={} signals={} at={}",
            r.id,
            r.status,
            if r.triage.is_empty() {
                String::new()
            } else {
                format!(" (triage:{})", r.triage)
            },
            r.pipeline,
            r.proc_name,
            r.err_code,
            r.signals,
            r.created_at
        );
        println!("    {}", crate::trunc_chars(&r.evidence, 110));
    }
    Ok(0)
}

/// v0.18.6 P0-刀2：判别实验真重跑。
/// 读 incident → 取该节点全部 canary 快照 → 逐条经 L2 bridge 重跑 llm →
/// eval_expect 求值 pass_rate → classify_discriminant → 回写 incidents.triage。
/// 无 canary / 重跑全失败 → nocanary（判别证据未取得，禁止本地 patch）。
fn cmd_incident_triage(id: &str) -> Result<i32, String> {
    let id: i64 = id.parse().map_err(|_| format!("bad incident id: {id}"))?;
    let conn = db::open_try()?;
    let row: (String, String) = conn
        .query_row(
            "SELECT pipeline, proc_name FROM incidents WHERE id = ?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| format!("incident {id} not found: {e}"))?;
    let (pipeline, proc_name) = row;
    let canaries = canary::list_canaries_conn(&conn, Some(&proc_name));
    let targets: Vec<_> = canaries.iter().filter(|c| c.pipeline == pipeline).collect();
    if targets.is_empty() {
        let disc = incident::triage_incident_conn(&conn, id, "")?;
        println!("incident #{id}: no canary for {pipeline}::{proc_name} → {disc}");
        return Ok(0);
    }
    let mut passes = 0usize;
    let mut ran = 0usize;
    for c in &targets {
        if let Some(text) = crate::L2_orchestration::steps::replay_canary_llm(&c.input) {
            ran += 1;
            let ok = canary::eval_expect(&c.expect, &text);
            // 判别实验留痕：canary_runs 是 class2 vs 3/4 硬门禁的依据表
            let _ = canary::record_canary_run_conn(
                &conn,
                &c.pipeline,
                &c.proc_name,
                ok,
                &format!("incident-{} replay", id),
            );
            if ok {
                passes += 1;
            }
        }
    }
    if ran == 0 {
        let disc = incident::triage_incident_conn(&conn, id, "")?;
        println!("incident #{id}: replay failed → {disc} (no discriminant evidence)");
        return Ok(0);
    }
    let rate_f = passes as f64 / ran as f64;
    let disc = shelve::classify_discriminant(Some(rate_f))
        .to_label()
        .to_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    conn.execute(
        "UPDATE incidents SET triage = ?1, triage_at = ?2 WHERE id = ?3",
        rusqlite::params![disc, now, id],
    )
    .map_err(|e| format!("triage write failed: {e}"))?;
    println!(
        "incident #{id}: replay {}/{}, pass {}/{} (rate {:.2}) → {}",
        ran,
        targets.len(),
        passes,
        ran,
        rate_f,
        disc
    );
    Ok(0)
}

// ── v0.15 L4 端到端复核 CLI（缺口 #4）──

// ── v0.15 搁置队列 CLI（缺口 #5）──

fn cmd_shelve_list(status: Option<&str>) -> Result<i32, String> {
    print!("{}", shelve::render_shelved_conn(&db::open_try()?, status));
    Ok(0)
}

fn cmd_shelve_resolve(id: &str, resolution: &str) -> Result<i32, String> {
    let conn = db::open_try()?;
    let id: i64 = id.parse().map_err(|_| format!("bad shelved id: {id}"))?;
    shelve::resolve_shelved_conn(&conn, id, resolution)?;
    println!("shelved #{id} resolved: {resolution}");
    Ok(0)
}

fn cmd_l4_list() -> Result<i32, String> {
    print!("{}", l4::render_reviews_conn(&db::open_try()?, 20));
    Ok(0)
}

/// v0.18.6 P1-2：盲评自动打标（校准闭环）。
/// v0.18.8 泛化：标记前缀与校准源参数化——任何场景的 tally 产物都能回灌。
/// 读 tally 产物（如 /tmp/regchain_data/reg_tally.log 或任意 <PREFIX>-PASS/FAIL），
/// 用盲评 verdict（独立源：auto vs baseline 相对判断）给同一窗口的
/// l4_reviews 打标：<PREFIX>-PASS → ok，<PREFIX>-FAIL → bad。
/// 用法：ductile l4 calibrate <path> [prefix] [source]
///   prefix 默认 REGCHAIN（旧语义兼容），source 默认 blind:regcheck3。
///   场景侧（如命理）：ductile l4 calibrate tally_r9.log MINGLI blind:mingli
/// 同一 review 只打一次（有 label 的跳过）；打完打印 phase 变化。
/// 独立性：标签源（盲评相对判断）与 verdict 源（intent+deliver 绝对判断）
/// 不同源——这正是校准的意义：两个不同源的判断器的一致率。
fn cmd_l4_calibrate(path: &str, prefix: &str, source: &str) -> Result<i32, String> {
    let tally =
        std::fs::read_to_string(path).map_err(|e| format!("read tally log: {e} (先跑 tally)"))?;
    let pass_marker = format!("{prefix}-PASS");
    let fail_marker = format!("{prefix}-FAIL");
    let pass = tally.contains(&pass_marker);
    let fail = tally.contains(&fail_marker);
    if !pass && !fail {
        return Err(format!(
            "{path} 无 {pass_marker}/{fail_marker} 标记，不是 tally 产物（prefix={prefix}）"
        ));
    }
    let label = if pass { "ok" } else { "bad" };
    let conn = db::open_try()?;
    // 找未标注的 reviews（按时间窗不需要——regcheck3 窗口内落库的 review
    // 就是这次回归跑出来的；有 label 的跳过，幂等）
    let unlabeled: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT id FROM l4_reviews WHERE label = '' ORDER BY id")
            .map_err(|e| format!("query: {e}"))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .map_err(|e| format!("query: {e}"))?;
        rows.filter_map(|r| r.ok()).collect()
    };
    if unlabeled.is_empty() {
        println!("no unlabeled reviews — nothing to calibrate");
        println!("phase: {}", l4::phase_for_conn(&conn).as_str());
        return Ok(0);
    }
    let mut n = 0;
    for id in &unlabeled {
        l4::label_review_sourced_conn(&conn, *id, label, source)?;
        n += 1;
    }
    let phase = l4::phase_for_conn(&conn);
    let rate = l4::agreement_rate(&conn);
    println!(
        "✓ labeled {n} reviews '{label}' (source: {source}) from {path}",
        n = n,
        label = label,
        path = path
    );
    println!(
        "  labeled now: {}/{} | agreement: {} | phase: {}",
        count_labeled(&conn),
        count_reviews(&conn),
        match rate {
            Some(a) => format!("{:.2}", a),
            None => "n/a".to_string(),
        },
        phase.as_str()
    );
    Ok(0)
}

fn count_labeled(conn: &rusqlite::Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM l4_reviews WHERE label IN ('ok','bad')",
        [],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

fn count_reviews(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM l4_reviews", [], |r| r.get(0))
        .unwrap_or(0)
}

fn cmd_l4_status() -> Result<i32, String> {
    let conn = db::open_try()?;
    let phase = l4::phase_for_conn(&conn);
    let rate = l4::agreement_rate(&conn);
    println!(
        "L4 phase: {} | agreement: {} | min_labeled: {} | min_agreement: {}",
        phase.as_str(),
        match rate {
            Some(a) => format!("{:.2}", a),
            None => "n/a".to_string(),
        },
        l4::L4_ENFORCE_MIN_LABELED,
        l4::L4_ENFORCE_MIN_AGREEMENT
    );
    Ok(0)
}

fn cmd_l4_review(pipeline: &str, verdict: &str, evidence: &str) -> Result<i32, String> {
    let conn = db::open_try()?;
    let id = l4::record_review_conn(&conn, pipeline, verdict, evidence, None)?;
    let phase = l4::phase_for_conn(&conn);
    println!(
        "l4 review #{} recorded [{}] phase={} (log-only 阶段不拦截)",
        id,
        verdict,
        phase.as_str()
    );
    Ok(0)
}

fn cmd_l4_label(id: &str, label: &str) -> Result<i32, String> {
    let conn = db::open_try()?;
    let id: i64 = id.parse().map_err(|_| format!("bad review id: {id}"))?;
    l4::label_review_conn(&conn, id, label)?;
    let phase = l4::phase_for_conn(&conn);
    let rate = l4::agreement_rate(&conn);
    println!(
        "l4 review #{} labeled {} | phase={} agreement={}",
        id,
        label,
        phase.as_str(),
        match rate {
            Some(a) => format!("{:.2}", a),
            None => "n/a".to_string(),
        }
    );
    Ok(0)
}

fn cmd_incident_close(id_str: &str, resolution: &str) -> Result<i32, String> {
    let id: i64 = id_str.parse().map_err(|_| format!("bad id: {id_str}"))?;
    incident::close_incident_conn(&db::open_try()?, id, resolution)?;
    println!("incident #{id} closed: {resolution}");
    Ok(0)
}

fn cmd_script_attach(path: &str) -> Result<i32, String> {
    let source =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {}", path, e))?;
    let card = crate::script::parse_contract(&source, path)?;
    crate::db::script_attach(&card)?;
    println!(
        "attached: {} (lang={}, pure={}, idempotent={}, concurrency={}, effects={}, args={})",
        card.name,
        card.lang,
        card.pure,
        card.idempotent,
        card.concurrency.as_str(),
        card.effects,
        card.args_channel
    );
    Ok(0)
}

fn cmd_script_detach(name: &str) -> Result<i32, String> {
    if crate::db::script_detach(name) {
        println!("detached: {}", name);
        Ok(0)
    } else {
        eprintln!("script '{}' not found", name);
        Ok(1)
    }
}

fn cmd_script_list() -> Result<i32, String> {
    let scripts = crate::db::script_list();
    if scripts.is_empty() {
        println!("no scripts attached — ductile script attach <file>");
        return Ok(0);
    }
    println!(
        "{:<18} {:<8} {:<5} {:<11} {:<10} {}",
        "NAME", "LANG", "PURE", "IDEMPOTENT", "CONCUR", "EFFECTS"
    );
    for c in &scripts {
        println!(
            "{:<18} {:<8} {:<5} {:<11} {:<10} {}",
            c.name,
            c.lang,
            c.pure,
            c.idempotent,
            c.concurrency.as_str(),
            c.effects
        );
    }
    Ok(0)
}

/// v0.19 审计①：死契约诊断——逐条 stat 契约卡路径，列出不存在的。
/// 只读不删（删之前先看见）；死路径 exit 1（可接 devcycle 门禁）。
/// 修复：ductile script detach <name> 或重新 attach 真实文件。
fn cmd_script_doctor() -> Result<i32, String> {
    let scripts = crate::db::script_list();
    if scripts.is_empty() {
        println!("no scripts attached — nothing to check");
        return Ok(0);
    }
    let mut dead = 0;
    println!("{:<18} {:<6} {}", "NAME", "STATE", "PATH");
    for c in &scripts {
        let state = if std::path::Path::new(&c.path).exists() {
            "ok"
        } else {
            dead += 1;
            "DEAD"
        };
        println!("{:<18} {:<6} {}", c.name, state, c.path);
    }
    if dead > 0 {
        println!(
            "\n{} dead contract(s) — fix: ductile script detach <name> (unregister) \
             or re-attach the real file",
            dead
        );
        return Ok(1);
    }
    println!("\nall {} contracts alive", scripts.len());
    Ok(0)
}

fn cmd_script_show(name: &str) -> Result<i32, String> {
    let c = match crate::db::script_get(name) {
        Some(c) => c,
        None => {
            eprintln!("script '{}' not found", name);
            return Ok(1);
        }
    };
    println!("script: {}", c.name);
    println!("  desc:         {}", c.desc);
    println!("  path:         {}", c.path);
    println!("  lang:         {}", c.lang);
    println!(
        "  params:       {}",
        if c.params.is_empty() {
            "(none)"
        } else {
            &c.params
        }
    );
    println!("  output:       {}", c.output);
    println!("  pure:         {}", c.pure);
    println!("  idempotent:   {}", c.idempotent);
    println!("  concurrency:  {}", c.concurrency.as_str());
    println!("  effects:      {}", c.effects);
    println!("  timeout:      {}s", c.timeout_secs);
    println!("  retries:      {}", c.retries);
    println!("  args:         {} (v0.23 参数通道: env=DUCTILE_ARG_* | argv=--key=value | both)", c.args_channel);
    if !c.mcsm.is_empty() {
        println!("  mcsm:         {}", c.mcsm);
        if !c.mcsm_note.is_empty() {
            // v0.19.x 实例级注解：直接展示具体指称（读卡即知，不用翻文档）
            for part in c.mcsm_note.split(" | ") {
                println!("                {}", part);
            }
        } else {
            println!("                (F场域 O本体 P现象 T目的；1稳定 2矛盾 3构造新机制 4实践扩展 — docs/MCSM.md)");
        }
    }
    println!("  cse_safe:     {}", crate::script::cse_safe(&c));
    Ok(0)
}

/// 单发调用（绕过 pipeline，调试用）：ductile script call <name> "k=v, k=v"
fn cmd_script_call(name: &str, kv: &str) -> Result<i32, String> {
    let body = build_script_call_body(name, kv);
    let impl_ = Impl {
        name: "cli_call".into(),
        description: String::new(),
        tags: BTreeSet::new(),
        cost: Cost::default(),
        enabled: true,
        when: None,
        refs: Vec::new(),
        body_text: body.clone(),
        stub: false,
        retry: 0,
        ensure: Vec::new(),
    };
    match crate::executor::exec_script_call(&impl_, "", &body, &BTreeMap::new()) {
        Ok(v) => {
            println!("{:?}", v);
            Ok(0)
        }
        Err(e) => {
            eprintln!("{}", e);
            Ok(1)
        }
    }
}

fn print_usage() {
    eprintln!(
        "Ductile v{} — declarative pipeline DSL",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!();
    eprintln!("Usage: ductile <command> <file.pipeline> [args]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  check <file>           Parse + type check");
    eprintln!("  run   <file> [topic] [--policy f.eval] [--restrict-shell]");
    eprintln!("                         Parse + check + execute");
    eprintln!("                         --restrict-shell blocks run/sh/spawn (or set DUCTILE_RESTRICT_SHELL=1)");
    eprintln!("  graph <file>           Show e-graph structure");
    eprintln!("  parse <file>           Parse only (show structure)");
    eprintln!("  kernel-chaos <dir> [seeds]       Chaos harness: 6 killpoints x seeds (WAL-backed, fail-closed)");
    eprintln!("  kernel-ledger <file> --verify    Verify evidence ledger hash chain");
    eprintln!("  kernel-reproduce <dir> -o <out.tar.zst>   Pack reproduce bundle");
    eprintln!("  kernel-reproduce --verify <dir>  Verify unpacked reproduce bundle offline");
    eprintln!("  hyper build <f.hyper> [-o out.pipeline]  Emit pipeline from hyper layer");
    eprintln!("  hyper check <f.hyper> <f.pipeline>       Check pipeline vs hyper constraints");
    eprintln!("  hyper parse <f.hyper>  Show hyper stages / requires");
    eprintln!("  hyper similar <f.hyper|f.pipeline> [--json] [dirs]  Workflow structural reuse");
    eprintln!("  hyper nodes <f>[:node]|--role X [--op Y] [--json]  Node/proc reuse lookup");
    eprintln!();
    eprintln!("Database:");
    eprintln!("  import <dir|file>      Import pipelines into SQLite");
    eprintln!("  search \"query\"         Search proc library (LIKE)");
    eprintln!("  fts \"query\"            BM25 full-text search (relevance ranked)");
    eprintln!("  compose <name> <desc> <#tag1> <#tag2>...  Assemble from library");
    eprintln!("  db-stats               Show database statistics");
    eprintln!();
    eprintln!("Console:");
    eprintln!("  tui [file.pipeline]    Interactive TUI (status/data/blueprint/isomorph)");
    eprintln!();
    eprintln!("Discovery:");
    eprintln!("  discover [file]        Show isomorphic proc groups");
    eprintln!("  learn [dir]            Discover compressive abstractions");
    eprintln!();
    eprintln!("Version:");
    eprintln!("  version save <file> \"desc\"   Save version snapshot");
    eprintln!("  version log  <file>          Show version history");
    eprintln!("  version diff <file> v1 v2    Diff two versions");
    eprintln!();
    eprintln!("Patch (hot override, no file edit):");
    eprintln!("  patch <pipeline> <proc> <impl> <field> <value>");
    eprintln!("  patch list");
    eprintln!("  patch clear <pipeline>");
    eprintln!();
    eprintln!("Deep help (v0.23 — 深题手册, 现象→原因→修法):");
    eprintln!("  help args              参数通道 env/argv/both + 静默回落坑");
    eprintln!("  help quotes            DSL 引号语义 (三层引号/@ref 禁入 echo)");
    eprintln!("  help errflow           错误分类 Retry/Switch/Reroute/Exit");
    eprintln!("  help script            脚本契约头速查");
    eprintln!();
    eprintln!("Script contracts (v0.12 — 脚本即 API):");
    eprintln!("  script attach <file>    Register script (parses # ductile: contract header)");
    eprintln!("  script list             Show attached scripts");
    eprintln!("  script doctor           Check all contract paths (DEAD = file missing)");
    eprintln!("  script show <name>      Show contract card (LLM reads this, not the script)");
    eprintln!("  script call <name> \"k=v, k=v\"   One-off invoke (debug)");
    eprintln!("  script detach <name>    Unregister");
    eprintln!();
    eprintln!("Explore (v0.19 探索环):");
    eprintln!(
        "  explore <file> <topic> [--drs]    Curriculum 出题→沙箱探针→确定性裁判→incidents 固化"
    );
    eprintln!("  explore --report <id>       只读检索冻结报告");
    eprintln!();
    eprintln!("Maintenance (v0.19 审计五件):");
    eprintln!("  archive                 Snapshot db (wal_checkpoint + copy, keep=10)");
    eprintln!();
    eprintln!("DSL verbs (SPEC §2/§5 — help 只列入口，动词全表见 SPEC):");
    eprintln!("  run/sh/write/read/llm/gate/judge/spawn/cp/ftp ... + proc 修饰符");
    eprintln!("  .when .needs .trust .deliver .constraint .pick .tags .desc .retry");
}

// ── run ──

fn cmd_run(path: &str, topic_str: &str, policy_path: Option<&str>) -> Result<i32, String> {
    let pl = match parse_pipeline_file(path) {
        Err(e) => {
            eprintln!("{}", e);
            return Ok(1);
        }
        Ok(pl) => pl,
    };
    // v0.11: 评价策略挂载（.eval 文件）。None = 引擎默认。
    let policy = match policy_path {
        Some(p) => match parse_policy_file(p) {
            Ok(pol) => {
                println!("Policy: {} (weights + {} cost specs)", p, pol.costs.len());
                Some(pol)
            }
            Err(e) => {
                eprintln!("Policy error in {}:\n  {}", p, e);
                return Ok(1);
            }
        },
        None => None,
    };
    let errs = check_pipeline(&pl);
    if !errs.is_empty() {
        eprintln!("Type check errors:");
        for e in &errs {
            eprintln!("  {}", e);
        }
        return Ok(1);
    }
    let (topic, params) = parse_topic_params(topic_str);
    println!("Pipeline: {} | Topic: {}", pl.name, topic);
    println!();

    // Auto-import to DB
    db::import_pipeline(&pl, path);

    // Isomorphism hints
    let registry = registry::load_all_entries();
    let matches = registry::find_isomorphic_matches(&pl.name, &registry, &pl);
    if !matches.is_empty() {
        println!("Isomorphism hints:");
        for m in &matches {
            println!(
                "  {} ≅ {} [{}] — {}",
                m.local_proc, m.remote.name, m.remote.pipeline, m.remote.description
            );
        }
        println!();
    }

    match exec_pipeline(&topic, &params, &pl, policy.as_ref()) {
        ExecResult::Success(results) => {
            println!();
            // v0.8.1: learning visibility — what the prefs chose this run
            for proc in &pl.procs {
                if proc.plan.len() >= 2 {
                    let prefs = executor::ImplPrefs::load(std::path::Path::new(""));
                    let mut ws: Vec<(f64, &str)> = proc
                        .plan
                        .iter()
                        .map(|i| (prefs.get(&pl.name, &proc.name, &i.name), i.name.as_str()))
                        .collect();
                    ws.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                    println!(
                        "  [learned] {}: {} (w={:.2}) leads over {} (w={:.2})",
                        proc.name, ws[0].1, ws[0].0, ws[1].1, ws[1].0
                    );
                }
            }
            println!("Pipeline executed successfully");
            println!("  Procs completed: {}", results.len());
            for (k, v) in &results {
                let shown = match v {
                    Value::Text(t) => {
                        if t.len() > 200 {
                            format!("{}...", crate::trunc_chars(t, 200))
                        } else {
                            t.clone()
                        }
                    }
                    Value::File(f) => format!("<file: {}>", f),
                    Value::Null => "<null>".to_string(),
                };
                println!("    {} => {}", k, shown);
            }
            Ok(0)
        }
        ExecResult::Failed { error, partial } => {
            eprintln!("Pipeline failed: {}", error);
            if !partial.is_empty() {
                eprintln!("  [partial] {} procs completed/marked:", partial.len());
                for (k, v) in &partial {
                    let is_err = crate::errflow::is_error_value(v);
                    let tag = if is_err { "LEFT" } else { "OK  " };
                    eprintln!("    [{}] {} => {:?}", tag, k, v);
                }
            }
            harvest::set_degraded(&pl.name, &error);
            eprintln!(
                "[degraded] flag set — fix then: ductile degraded clear {}",
                pl.name
            );
            Ok(1)
        }
    }
}

// ── graph ──

fn cmd_graph(path: &str) -> Result<i32, String> {
    let pl = match parse_pipeline_file(path) {
        Err(e) => {
            eprintln!("{}", e);
            return Ok(1);
        }
        Ok(pl) => pl,
    };
    let eg = build_egraph(&pl);
    let groups = parallel_groups(&eg);
    let cp = critical_path(&eg);
    let plan = extract_plan(&pl, &eg);
    println!(
        "E-Graph:\n  Nodes: {}\n  Edges: {}\n  E-classes: {}\n",
        pl.procs.len(),
        eg.edges.len(),
        eg.class_count()
    );
    if !eg.fusion_hits.is_empty() {
        let f: Vec<String> = eg
            .fusion_hits
            .iter()
            .map(|(k, v)| format!("{}×{}", k, v))
            .collect();
        println!("  Fusion rules fired: {}", f.join(", "));
    }
    // e-class 明细（仅多成员 class 展示）
    for id in 0..eg.classes.len() {
        if eg.uf.find_imm(id) != id || eg.classes[id].procs.len() < 2 {
            continue;
        }
        println!("  [class] {{{}}}", eg.classes[id].procs.join(", "));
    }
    if !plan.aliases.is_empty() {
        let a: Vec<String> = plan
            .aliases
            .iter()
            .map(|(x, r)| format!("{}→{}", x, r))
            .collect();
        println!("  CSE aliases: {}", a.join(", "));
    }
    println!("\nParallel groups:");
    for g in &groups {
        println!("  {:?}", g);
    }
    println!("\nCritical path: {:?}", cp);
    println!("\nExtracted plan (static):");
    for name in &plan.order {
        if let Some((p, i)) = plan.picks.get(name) {
            let cost = plan.costs.get(name).copied().unwrap_or(0.0);
            println!("  {} <- {}::{} (cost {:.3})", name, p, i, cost);
        }
    }
    Ok(0)
}

// ── parse ──

fn cmd_parse(path: &str) -> Result<i32, String> {
    let pl = match parse_pipeline_file(path) {
        Err(e) => {
            eprintln!("{}", e);
            return Ok(1);
        }
        Ok(pl) => pl,
    };
    println!("Pipeline: {}", pl.name);
    if !pl.description.is_empty() {
        println!("Description: {}", pl.description);
    }
    let tags = pl.computed_tags();
    if !tags.is_empty() {
        let tag_str: Vec<String> = tags.iter().map(|t| format!("#{}", t)).collect();
        println!("Tags: {}", tag_str.join(", "));
    }
    println!();
    for proc in &pl.procs {
        println!(
            "  Proc: {} ({} impls){}",
            proc.name,
            proc.plan.len(),
            if proc.deliver { " [deliver]" } else { "" }
        );
        if !proc.description.is_empty() {
            println!("    Desc: {}", proc.description);
        }
        for imp in &proc.plan {
            let tag_str: Vec<String> = imp.tags.iter().map(|t| format!("#{}", t)).collect();
            print!("    Impl: {}", imp.name);
            if !tag_str.is_empty() {
                print!(" [{}]", tag_str.join(", "));
            }
            println!();
            if !imp.description.is_empty() {
                println!("      Desc: {}", imp.description);
            }
        }
        for chk in &proc.checks {
            println!("    Check: \"{}\"", chk.msg);
        }
    }

    // Isomorphism hints
    let registry = registry::load_all_entries();
    let matches = registry::find_isomorphic_matches(&pl.name, &registry, &pl);
    if !matches.is_empty() {
        println!("\nIsomorphism hints:");
        for m in &matches {
            println!(
                "  {} ≅ {} [{}] — {}",
                m.local_proc, m.remote.name, m.remote.pipeline, m.remote.description
            );
        }
    }
    Ok(0)
}

// ── hyper (graph-generation guide layer) ──

fn cmd_hyper(args: &[String]) -> Result<i32, String> {
    if args.is_empty() {
        eprintln!("Usage: ductile hyper <build|check|parse> ...");
        return Ok(1);
    }
    match args[0].as_str() {
        "parse" if args.len() >= 2 => {
            let h = hyper::parse_hyper_file(&args[1]).map_err(|e| e.to_string())?;
            println!("HyperGraph: {}", h.name);
            if !h.goal.is_empty() {
                println!("Goal: {}", h.goal);
            }
            println!(
                "Require: judge={} min_impls={}",
                h.require.judge, h.require.min_impls
            );
            println!("Hypergraph key: {}", h.hypergraph_key());
            println!("Vertices ({}):", h.vertices.len());
            for v in &h.vertices {
                let tags: Vec<String> = v.tags.iter().map(|t| format!("#{}", t)).collect();
                println!(
                    "  {} role={} tags=[{}]",
                    v.name,
                    v.role.as_str(),
                    tags.join(",")
                );
            }
            println!("Hyperedges ({}):", h.hedges.len());
            for e in &h.hedges {
                if e.kind == crate::hyper::HedgeKind::Gate {
                    println!(
                        "  {} kind=gate judge={:?} producers={:?} consumers={:?}",
                        e.name,
                        e.judge.as_deref().unwrap_or("?"),
                        e.producers,
                        e.consumers
                    );
                } else {
                    println!(
                        "  {} kind={} members={:?}",
                        e.name,
                        e.kind.as_str(),
                        e.members
                    );
                }
            }
            println!("Projected DAG stages:");
            for s in &h.stages {
                println!("  {} after={:?} gated_by={:?}", s.name, s.after, s.gated_by);
            }
            if let Some(d) = &h.deliver {
                println!("Deliver: @{}", d);
            }
            Ok(0)
        }
        "build" if args.len() >= 2 => {
            let (out_path, hyper_path) = parse_hyper_build_args(&args[1..])?;
            let h = hyper::parse_hyper_file(hyper_path).map_err(|e| e.to_string())?;
            // Validate emit before write
            let text = hyper::emit_pipeline(&h);
            let pl = parse_pipeline(&text).map_err(|e| format!("emitted pipeline parse: {}", e))?;
            let terrs = check_pipeline(&pl);
            if !terrs.is_empty() {
                eprintln!("Emitted pipeline failed typecheck:");
                for e in &terrs {
                    eprintln!("  {}", e);
                }
                return Ok(1);
            }
            let herrs = hyper::check_pipeline_against(&h, &pl);
            if !herrs.is_empty() {
                eprintln!("Emitted pipeline failed hyper check:");
                for e in &herrs {
                    eprintln!("  {}", e);
                }
                return Ok(1);
            }
            match out_path {
                Some(path) => {
                    hyper::write_pipeline_file(&h, path)?;
                    println!("Wrote {} (from {})", path, hyper_path);
                }
                None => print!("{}", text),
            }
            Ok(0)
        }
        "check" if args.len() >= 3 => {
            let h = hyper::parse_hyper_file(&args[1]).map_err(|e| e.to_string())?;
            let pl = parse_pipeline_file(&args[2]).map_err(|e| e.to_string())?;
            let errs = hyper::check_pipeline_against(&h, &pl);
            if errs.is_empty() {
                println!("Hyper check passed ({} stages)", h.stages.len());
                Ok(0)
            } else {
                eprintln!("Hyper check failed:");
                for e in &errs {
                    eprintln!("  {}", e);
                }
                Ok(1)
            }
        }
        "similar" if args.len() >= 2 => {
            let (as_json, query_path, roots) = parse_hyper_similar_args(&args[1..])?;
            if as_json {
                let raw = hyper::similar_json(query_path, &roots)?;
                println!("{}", raw);
                return Ok(0);
            }
            let (qsig, qname) = if query_path.ends_with(".hyper") {
                let h = hyper::parse_hyper_file(query_path).map_err(|e| e.to_string())?;
                (hyper::struct_sig_from_hyper(&h), h.name)
            } else if query_path.ends_with(".pipeline") {
                let pl = parse_pipeline_file(query_path).map_err(|e| e.to_string())?;
                (hyper::struct_sig_from_pipeline(&pl), pl.name)
            } else {
                return Err("hyper similar expects .hyper or .pipeline".into());
            };
            let scan = if roots.is_empty() {
                vec![
                    "examples".into(),
                    "examples/hyper".into(),
                    "examples/scripts".into(),
                ]
            } else {
                roots
            };
            println!("Query: {} ({})", qname, query_path);
            println!("Structure key: {}", qsig.structure_key());
            println!(
                "Scan: {:?} (+ db registry: pipelines.source_file ∪ hyper_graphs)",
                scan
            );
            println!("(Match key = role+edges+gates; tags are soft rank only)\n");
            let hits = hyper::find_similar(&qsig, &qname, &scan)?;
            let mut shown = 0;
            for h in &hits {
                if h.path.replace('\\', "/") == query_path.replace('\\', "/") {
                    continue; // skip self
                }
                // Show structural hits and near (same roles); hide tag-only noise
                if !h.structure_match && !h.note.starts_with("same role sequence") {
                    continue;
                }
                let mark = if h.structure_match {
                    "✓ ISO"
                } else {
                    "~ near"
                };
                println!(
                    "{} [{}] {} ({})  tag_jaccard={:.2}",
                    mark, h.kind, h.name, h.path, h.tag_jaccard
                );
                println!("    {}", h.note);
                if h.structure_match {
                    println!("    → LLM: reuse_action=reuse_pipeline  path={}", h.path);
                } else {
                    println!("    → LLM: reuse_action=adapt_topology  path={}", h.path);
                }
                shown += 1;
                if shown >= 20 {
                    break;
                }
            }
            if shown == 0 {
                println!("No structural reuse candidates in scan roots.");
            }
            Ok(0)
        }
        "nodes" => cmd_hyper_nodes(&args[1..]),
        _ => {
            eprintln!("Usage:");
            eprintln!("  ductile hyper build <file.hyper> [-o out.pipeline]");
            eprintln!("  ductile hyper check <file.hyper> <file.pipeline>");
            eprintln!("  ductile hyper parse <file.hyper>");
            eprintln!("  ductile hyper similar <file.hyper|file.pipeline> [--json] [dirs]");
            eprintln!("  ductile hyper nodes <file>[:node] [--json] [dirs]");
            eprintln!("  ductile hyper nodes --role judge [--op run] [--json] [dirs]");
            Ok(1)
        }
    }
}

fn cmd_hyper_nodes(args: &[String]) -> Result<i32, String> {
    let opts = parse_hyper_nodes_args(args)?;
    let scan = if opts.roots.is_empty() {
        vec![
            "examples".into(),
            "examples/hyper".into(),
            "examples/scripts".into(),
        ]
    } else {
        opts.roots.clone()
    };
    let filter = if opts.role.is_some() || opts.op.is_some() {
        Some(hyper::NodeQuery {
            role: opts.role.clone(),
            op: opts.op.clone(),
            in_arity: opts.in_arity,
            gated: opts.gated,
        })
    } else {
        None
    };

    if opts.as_json {
        let raw = hyper::nodes_json(opts.query.as_deref().unwrap_or(""), &scan, filter.as_ref())?;
        println!("{}", raw);
        return Ok(0);
    }

    if let Some(f) = &filter {
        if opts.query.is_none() {
            let hits = hyper::find_nodes_by_filter(f, &scan)?;
            println!(
                "Node filter: role={:?} op={:?}  ({} hits)",
                f.role,
                f.op,
                hits.len()
            );
            for h in hits.iter().take(30) {
                println!(
                    "  [{}] {}:{}  key={}  {}",
                    h.kind, h.path, h.node_name, h.node_key, h.body_preview
                );
            }
            if hits.is_empty() {
                println!("No nodes matched.");
            }
            return Ok(0);
        }
    }

    let q = opts
        .query
        .as_deref()
        .ok_or_else(|| "hyper nodes needs <file>[:node] or --role/--op".to_string())?;
    let (path, node_opt) = {
        if let Some(i) = q.rfind(':') {
            let (left, right) = q.split_at(i);
            let node = &right[1..];
            if !node.is_empty()
                && !node.contains('/')
                && !node.contains('\\')
                && !node.contains('.')
                && (left.ends_with(".pipeline") || left.ends_with(".hyper"))
            {
                (left, Some(node))
            } else {
                (q, None)
            }
        } else {
            (q, None)
        }
    };

    let targets: Vec<(String, hyper::NodeSig, String)> = if path.ends_with(".pipeline") {
        let pl = parse_pipeline_file(path).map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for proc in &pl.procs {
            if proc.deliver || proc.name == "deliver" {
                continue;
            }
            if let Some(n) = node_opt {
                if proc.name != n {
                    continue;
                }
            }
            let sig = hyper::node_sig_from_proc(proc);
            v.push((proc.name.clone(), sig, path.to_string()));
        }
        if v.is_empty() {
            return Err(format!("no matching proc in {}", path));
        }
        v
    } else if path.ends_with(".hyper") {
        let h = hyper::parse_hyper_file(path).map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for s in &h.stages {
            if let Some(n) = node_opt {
                if s.name != n {
                    continue;
                }
            }
            let sig = hyper::node_sig_from_stage(s);
            v.push((s.name.clone(), sig, path.to_string()));
        }
        if v.is_empty() {
            return Err(format!("no matching stage in {}", path));
        }
        v
    } else {
        return Err("hyper nodes expects .hyper or .pipeline".into());
    };

    for (name, sig, p) in &targets {
        println!("Node {}:{}  key={}", p, name, sig.node_key());
        let hits = hyper::find_similar_nodes(sig, &scan, Some((p, name)))?;
        let mut shown = 0;
        for h in &hits {
            if !h.structure_match && !h.note.starts_with("same role+op") {
                continue;
            }
            let mark = if h.structure_match {
                "✓ NODE"
            } else {
                "~ near"
            };
            println!(
                "  {} [{}] {}:{}  tag={:.2}",
                mark, h.kind, h.path, h.node_name, h.tag_jaccard
            );
            println!("      {}", h.note);
            if !h.body_preview.is_empty() {
                println!("      body: {}", h.body_preview);
            }
            if h.structure_match {
                println!("      → LLM: reuse_action=reuse_node");
            } else {
                println!("      → LLM: reuse_action=adapt_ports");
            }
            shown += 1;
            if shown >= 15 {
                break;
            }
        }
        if shown == 0 {
            println!("  (no node reuse candidates)");
        }
        println!();
    }
    Ok(0)
}

#[derive(Default)]
struct HyperNodesOpts {
    as_json: bool,
    query: Option<String>,
    roots: Vec<String>,
    role: Option<String>,
    op: Option<String>,
    in_arity: Option<usize>,
    gated: Option<bool>,
}

fn parse_hyper_nodes_args(args: &[String]) -> Result<HyperNodesOpts, String> {
    let mut opts = HyperNodesOpts::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--json" {
            opts.as_json = true;
            i += 1;
            continue;
        }
        if a == "--role" {
            opts.role = Some(args.get(i + 1).ok_or("--role needs a value")?.clone());
            i += 2;
            continue;
        }
        if a == "--op" {
            opts.op = Some(args.get(i + 1).ok_or("--op needs a value")?.clone());
            i += 2;
            continue;
        }
        if a == "--in" {
            opts.in_arity = Some(
                args.get(i + 1)
                    .ok_or("--in needs a number")?
                    .parse()
                    .map_err(|_| "bad --in")?,
            );
            i += 2;
            continue;
        }
        if a == "--gated" {
            let v = args.get(i + 1).ok_or("--gated needs true|false")?;
            opts.gated = Some(v == "true" || v == "1");
            i += 2;
            continue;
        }
        if opts.query.is_none() && !a.starts_with("--") {
            if a.ends_with(".hyper")
                || a.ends_with(".pipeline")
                || a.contains(".pipeline:")
                || a.contains(".hyper:")
            {
                opts.query = Some(a.clone());
            } else {
                opts.roots.push(a.clone());
            }
            i += 1;
            continue;
        }
        if !a.starts_with("--") {
            opts.roots.push(a.clone());
            i += 1;
            continue;
        }
        return Err(format!("unknown arg {}", a));
    }
    Ok(opts)
}

fn parse_hyper_build_args(args: &[String]) -> Result<(Option<&str>, &str), String> {
    // hyper build <file.hyper> [-o out.pipeline]
    let mut out: Option<&str> = None;
    let mut hyper_path: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-o" || args[i] == "--output" {
            let p = args
                .get(i + 1)
                .ok_or_else(|| format!("{} needs a path", args[i]))?;
            out = Some(p.as_str());
            i += 2;
            continue;
        }
        if hyper_path.is_none() {
            hyper_path = Some(args[i].as_str());
            i += 1;
            continue;
        }
        return Err(format!("unexpected arg {:?}", args[i]));
    }
    let hyper_path = hyper_path.ok_or_else(|| "hyper build needs <file.hyper>".to_string())?;
    Ok((out, hyper_path))
}

fn parse_hyper_similar_args(args: &[String]) -> Result<(bool, &str, Vec<String>), String> {
    let mut as_json = false;
    let mut query: Option<&str> = None;
    let mut roots = Vec::new();
    for a in args {
        if a == "--json" {
            as_json = true;
            continue;
        }
        if query.is_none() {
            query = Some(a.as_str());
        } else {
            roots.push(a.clone());
        }
    }
    let query =
        query.ok_or_else(|| "hyper similar needs <file.hyper|file.pipeline>".to_string())?;
    Ok((as_json, query, roots))
}

// ── import ──

fn cmd_import(paths: &[String]) -> Result<i32, String> {
    db::init_db();
    let mut all_files = Vec::new();
    for p in paths {
        let path = std::path::Path::new(p);
        if path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for e in entries.filter_map(|e| e.ok()) {
                    let fp = e.path();
                    let ext_ok = fp
                        .extension()
                        .map(|e| e == "pipeline" || e == "hyper")
                        .unwrap_or(false);
                    if ext_ok {
                        all_files.push(fp.to_string_lossy().to_string());
                    }
                }
            }
        } else if path.is_file() {
            all_files.push(p.clone());
        }
    }
    if all_files.is_empty() {
        println!("No .pipeline/.hyper files found.");
        return Ok(0);
    }
    // v0.19.1：目录导入模式（无 .pipeline 逐个 import 时）顺带注册
    // hyper_graphs——similar 语料统一进 db。单文件 import 仍走
    // import_pipeline_file（run/check 已自动 import pipeline 语义不变）。
    let dir_mode = paths.iter().any(|p| std::path::Path::new(p).is_dir());
    let conn = if dir_mode { Some(db::open()) } else { None };
    let mut ok = 0;
    let mut errs = 0;
    for f in &all_files {
        if f.ends_with(".hyper") {
            match &conn {
                Some(c) => match db::import_graph_file(c, f) {
                    Ok((name, _kind)) => {
                        println!("  ✓ {} ({})", name, f);
                        ok += 1;
                    }
                    Err(e) => {
                        println!("  ✗ {} ({})", e, f);
                        errs += 1;
                    }
                },
                None => {
                    // 单文件模式传 .hyper：也注册（命令语义就是"收进库"）
                    let c = db::open();
                    match db::import_graph_file(&c, f) {
                        Ok((name, _kind)) => {
                            println!("  ✓ {} ({})", name, f);
                            ok += 1;
                        }
                        Err(e) => {
                            println!("  ✗ {} ({})", e, f);
                            errs += 1;
                        }
                    }
                }
            }
            continue;
        }
        match db::import_pipeline_file(f) {
            Ok(name) => {
                // pipeline 也注册进 hyper_graphs（similar 语料）
                if let Some(c) = &conn {
                    let _ = db::import_graph_file(c, f);
                } else {
                    let c = db::open();
                    let _ = db::import_graph_file(&c, f);
                }
                println!("  ✓ {} ({})", name, f);
                ok += 1;
            }
            Err(e) => {
                println!("  ✗ {} ({})", e, f);
                errs += 1;
            }
        }
    }
    println!("\nImported {} graphs ({} errors)", ok, errs);
    Ok(0)
}

// ── search ──

fn cmd_search(query: &str) -> Result<i32, String> {
    db::init_db();
    let results = db::search_procs(query);
    if results.is_empty() {
        println!("No matching procs found.");
        return Ok(0);
    }
    println!("Search \"{}\" — {} results:\n", query, results.len());
    for p in &results {
        let tag_str: Vec<String> = p.tags.iter().map(|t| format!("#{}", t)).collect();
        print!("  {}", p.name);
        if !tag_str.is_empty() {
            print!(" {{{{{}}}}}", tag_str.join(", "));
        }
        println!();
        if !p.description.is_empty() {
            println!("    {}", p.description);
        }
        println!("    [{}, {} impls]", p.pipeline, p.impl_count);
    }
    Ok(0)
}

// ── compose ──

fn cmd_compose(name: &str, desc: &str, tag_args: &[String]) -> Result<i32, String> {
    db::init_db();
    let all_procs = db::all_procs();
    println!("Composition Assembly");
    println!("====================");
    println!("Name: {}\nDescription: {}\n", name, desc);

    let mut proc_names = Vec::new();
    for (i, tag_query) in tag_args.iter().enumerate() {
        let tags: BTreeSet<String> = tag_query
            .trim_start_matches('#')
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        let candidates: Vec<&db::ProcRow> = all_procs
            .iter()
            .filter(|p| tags.is_subset(&p.tags))
            .collect();
        println!(
            "Step {} {{{}}}:",
            i + 1,
            tags.iter()
                .map(|t| format!("#{}", t))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if candidates.is_empty() {
            println!("  ✗ No matching procs");
        } else {
            for c in candidates.iter().take(3) {
                println!("  → {} [{}] {}", c.name, c.pipeline, c.description);
            }
            let chosen = candidates[0];
            println!("  ✓ Selected: {} [{}]", chosen.name, chosen.pipeline);
            proc_names.push(chosen.name.clone());
        }
        println!();
    }
    db::save_composition(name, desc, &proc_names);
    println!("Saved to compositions.");
    Ok(0)
}

// ── db-stats ──

fn cmd_db_stats() -> Result<i32, String> {
    db::init_db();
    let (pipelines, procs, runs, comps) = db::db_stats();
    println!("Ductile SQLite Database");
    println!("=======================");
    println!("  Pipelines:     {}", pipelines);
    println!("  Procs:         {}", procs);
    println!("  Run records:   {}", runs);
    println!("  Compositions:  {}", comps);
    println!("  Location:      {:?}", db::db_path());
    Ok(0)
}

// ── discover ──

fn cmd_discover(file: Option<&str>) -> Result<i32, String> {
    if let Some(path) = file {
        // Import then discover
        match parse_pipeline_file(path) {
            Err(e) => {
                eprintln!("{}", e);
                return Ok(1);
            }
            Ok(pl) => db::import_pipeline(&pl, path),
        }
    }
    let groups = db::isomorphic_groups();
    let all_procs = db::all_procs();
    let with_tags: Vec<&db::ProcRow> = all_procs.iter().filter(|p| !p.tags.is_empty()).collect();
    println!("Proc Registry — Tag overlap groups (soft hint, not structural iso)");
    println!("=====================================");
    println!("\nTotal registered procs: {}", all_procs.len());
    println!("Tag-identical groups (≥2 pipelines): {}\n", groups.len());
    if groups.is_empty() {
        println!("No tag-overlap groups found.");
        println!("For structural reuse: ductile hyper similar <file.hyper>");
    } else {
        println!("(Reliable reuse → ductile hyper similar; tags here are retrieval only)\n");
        for g in &groups {
            let tag_str: Vec<String> = g.tags.iter().map(|t| format!("#{}", t)).collect();
            println!("\n  {{{}}} — {} procs", tag_str.join(", "), g.members.len());
            for m in &g.members {
                println!("    {} — {} [{}]", m.name, m.description, m.pipeline);
            }
        }
    }
    println!("\nAll procs by tag signature:");
    println!("---------------------------");
    for p in &with_tags {
        let tag_str: Vec<String> = p.tags.iter().map(|t| format!("#{}", t)).collect();
        println!(
            "  {{{}}} {} — {} [{}]",
            tag_str.join(", "),
            p.name,
            p.description,
            p.pipeline
        );
    }
    Ok(0)
}

// ── learn ──

fn cmd_learn(arg: &str) -> Result<i32, String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let dirs: Vec<&str> = if arg.is_empty() {
        vec![
            Box::leak(format!("{}/projects/ductile/pipelines", home).into_boxed_str()),
            Box::leak(format!("{}/projects/ductile/experiments", home).into_boxed_str()),
        ]
    } else {
        vec![arg]
    };

    let result = learn::learn(&dirs);
    println!("Library Learning (Frequency Compression)");
    println!("=========================================");
    println!("\nPipelines scanned: {}", result.total_pipelines);
    println!("Tag sequences: {}", result.total_sequences);
    println!("Total tokens: {}\n", result.total_tokens);

    if result.repeats.is_empty() {
        println!("No compressive patterns found.");
    } else {
        println!("Discovered patterns:");
        println!("---------------------");
        for r in &result.repeats {
            println!(
                "  [×{}] {} → found in: {}",
                r.count,
                r.tags.join(" → "),
                r.pipelines.join(", ")
            );
        }
    }
    println!("\nCompression summary:");
    println!("  Original:   {} tokens", result.total_tokens);
    println!("  Compressed: {} tokens", result.compressed_size);
    println!("  Ratio:      {:.1}%", result.compression_ratio * 100.0);
    // v0.9.3: learn 的知识源已升级为命令语料 (V26/V28 物理) — 指路 grow
    println!("\n[note] learn 只扫 .pipeline 文件的 tag 序列; 真正的构式生长在命令全文语料上:");
    println!("       ductile grow 60 25    # MDL 构式生长 (脚手架住在行间, 见 SPEC §2.12)");
    println!("       ductile promote 60 30 # 跨会话晋升门 (sessions>=2 = 复用证据)");
    Ok(0)
}

// ── version ──

// ── v0.19 explore：探索环 CLI（P1 接线）──────────────────────────────

use crate::L4_structure::explore::{
    parse_curriculum_json, run_explore_loop, ExploreBudget, ExploreReport, ExploreStop, Stages,
    TaskOutcome,
};

/// curriculum 真调用：[agents.curriculum] 配置 → llm bridge 单发。
fn curriculum_llm_call(goal_ctx: &str) -> Result<String, String> {
    let agents = crate::L3_dsl::config::load_agents_config();
    let agent = match agents.get("curriculum") {
        Some(a) => a.clone(),
        None => {
            return Err(
                "explore: config.toml 缺 [agents.curriculum]（出题角色必须显式配置）".into(),
            )
        }
    };
    let llm = crate::L3_dsl::config::load_llm_config();
    let bridge = crate::L2_orchestration::steps::find_bridge("llm_bridge.py");
    if bridge.is_empty() {
        return Err(
            "explore: llm_bridge.py 不在搜索路径（repo/bridge/ 或 ~/.local/share/ductile/bridge/）"
                .into(),
        );
    }
    let out = std::process::Command::new("python3")
        .arg(&bridge)
        .arg("--prompt")
        .arg(goal_ctx)
        .arg("--model")
        .arg(&agent.model)
        .arg("--system")
        .arg(&agent.system)
        .arg("--schema")
        .arg(&agent.schema)
        .env("OPENAI_BASE_URL", &llm.base_url)
        .env("OPENAI_API_KEY", &llm.api_key)
        .env("OPENAI_TIMEOUT_SECS", llm.timeout_secs.to_string())
        .env(
            "OPENAI_MAX_TOKENS",
            std::env::var("OPENAI_MAX_TOKENS").unwrap_or_else(|_| "6000".into()),
        )
        .output()
        .map_err(|e| format!("explore: bridge 启动失败: {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "explore: bridge exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 探针执行：plan 若是 .pipeline 路径 → DUCTILE_DATA 沙箱真跑；
/// 否则原样回给裁判（自然语言步骤由 judge 描述判据）。
fn explore_execute(task: &crate::L4_structure::explore::ExploreTask) -> Result<String, String> {
    let plan = task.plan.trim();
    // cmd: 前缀 = 沙箱 shell 探针（curriculum code-as-policy 形态）
    if let Some(cmd) = plan.strip_prefix("cmd:") {
        // 沙箱数据目录仍建（DUCTILE_DATA 隔离锚点），但 cwd 不再指沙箱——
        // P3 自举实锤：cwd=空沙箱使 cargo test 无 target/锁可依，输出被
        // 编译错误淹没，判据"含 test result: ok"必然假 Fail。cwd 回到调用
        // 现场，探针命令自己负责绝对路径。
        let sandbox = std::env::temp_dir().join(format!(
            "ductile_explore_cmd_{}_{}",
            std::process::id(),
            task.id.replace('/', "_")
        ));
        std::fs::create_dir_all(&sandbox).map_err(|e| format!("sandbox: {}", e))?;
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(cmd.trim())
            .current_dir(std::env::current_dir().unwrap_or_else(|_| "/tmp".into()))
            .env("DUCTILE_DATA", &sandbox)
            .output()
            .map_err(|e| format!("探针命令失败: {}", e))?;
        return Ok(format!(
            "exit={}\n{}{}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    if plan.ends_with(".pipeline") && std::path::Path::new(plan).exists() {
        let sandbox = std::env::temp_dir().join(format!(
            "ductile_explore_{}_{}",
            std::process::id(),
            task.id.replace('/', "_")
        ));
        std::fs::create_dir_all(&sandbox).map_err(|e| format!("sandbox: {}", e))?;
        let out = std::process::Command::new(
            std::env::current_exe().unwrap_or_else(|_| "ductile".into()),
        )
        .arg("run")
        .arg(plan)
        .arg(&task.goal)
        .env("DUCTILE_DATA", &sandbox)
        .output()
        .map_err(|e| format!("探针执行失败: {}", e))?;
        return Ok(format!(
            "exit={}\n{}{}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(format!(
        "plan(非管线,未执行): {}\njudge: {}",
        task.plan, task.judge
    ))
}

/// 确定性裁判：judge 描述 + 产物 → 判定。
/// 判据语言（P1 子集，逐步扩）：`exit 0` / `exit N` / `含 <substr>` / `不含 <substr>`。
fn explore_judge(task: &crate::L4_structure::explore::ExploreTask, artifact: &str) -> TaskOutcome {
    let j = task.judge.trim();
    let mut checks: Vec<(bool, String)> = Vec::new();
    for clause in j.split(';') {
        let c = clause.trim();
        if c.is_empty() {
            continue;
        }
        if c == "exit 0" {
            let ok = artifact
                .lines()
                .next()
                .map(|l| l == "exit=0")
                .unwrap_or(false);
            checks.push((ok, format!("exit0({})", ok)));
        } else if let Some(n) = c.strip_prefix("exit ") {
            let want = format!("exit={}", n.trim());
            let ok = artifact.lines().next().map(|l| l == want).unwrap_or(false);
            checks.push((ok, format!("exit{}({})", n.trim(), ok)));
        } else if let Some(s) = c.strip_prefix("含 ") {
            let s = s.trim().trim_matches(['\'', '"']);
            let ok = artifact.contains(s);
            checks.push((ok, format!("contains({:?},{})", s, ok)));
        } else if let Some(s) = c.strip_prefix("不含 ") {
            let s = s.trim().trim_matches(['\'', '"']);
            let ok = !artifact.contains(s);
            checks.push((ok, format!("excludes({:?},{})", s, ok)));
        } else {
            return TaskOutcome::Undecidable {
                reason: format!("判据子句不可解析: {:?}", c),
            };
        }
    }
    if checks.is_empty() {
        return TaskOutcome::Undecidable {
            reason: "判据为空".into(),
        };
    }
    let evidence = checks
        .iter()
        .map(|(_, e)| e.clone())
        .collect::<Vec<_>>()
        .join(",");
    if checks.iter().all(|(ok, _)| *ok) {
        TaskOutcome::Pass { evidence }
    } else {
        TaskOutcome::Fail { evidence }
    }
}

/// 固化（P2）：Fail → incidents 真表行，三元组 schema：
/// action=探针命令 / condition=判据 / consequence=实际产物信号（evidence 字段）。
/// pipeline 维度记 explore:<topic>，proc_name 记题 id——与运行时故障同一张表，
/// 同 (pipeline, proc, err_code) 聚合去重（record_incident_conn 语义不变）。
fn consolidate(
    report: &mut ExploreReport,
    tasks: &std::collections::BTreeMap<String, crate::L4_structure::explore::ExploreTask>,
) {
    let conn = crate::L0_physical::db::open();
    let mut ids = Vec::new();
    for (id, (_, outcome)) in report.results.iter() {
        if let TaskOutcome::Fail { evidence } = outcome {
            let task = tasks.get(id);
            let action = task.map(|t| t.plan.clone()).unwrap_or_default();
            let condition = task.map(|t| t.judge.clone()).unwrap_or_default();
            let triple = format!(
                "action: {}\ncondition: {}\nconsequence: {}",
                action, condition, evidence
            );
            // 直插不走 record_incident_conn：它会用 evidence_snapshot 重写
            // evidence（只留 err/fields 头），explore 的三元组必须完整保留。
            // 聚合去重语义与原函数一致：同 (pipeline, proc, err_code) open → 更新。
            let pipeline_dim = format!("explore:{}", report.topic);
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT id FROM incidents WHERE pipeline=?1 AND proc_name=?2 AND err_code=?3 AND status='open'",
                    rusqlite::params![pipeline_dim, id, "explore_finding"],
                    |r| r.get(0),
                )
                .ok();
            let row_id = match existing {
                Some(rid) => {
                    let _ = conn.execute(
                        "UPDATE incidents SET evidence=?1, created_at=?2 WHERE id=?3",
                        rusqlite::params![triple, crate::L0_physical::time::now_ts(), rid],
                    );
                    rid
                }
                None => {
                    let _ = conn.execute(
                        "INSERT INTO incidents (pipeline, proc_name, signals, err_code, evidence, status, created_at) VALUES (?1,?2,?3,?4,?5,'open',?6)",
                        rusqlite::params![
                            pipeline_dim,
                            id,
                            "explore,judge-fail",
                            "explore_finding",
                            triple,
                            crate::L0_physical::time::now_ts()
                        ],
                    );
                    conn.last_insert_rowid()
                }
            };
            ids.push(format!("incident#{}[{}]", row_id, id));
        }
    }
    report.consolidated = ids;
}

fn explore_report_json(r: &ExploreReport) -> String {
    let mut out = String::from("{\n");
    out.push_str(&format!("  \"topic\": \"{}\",\n", r.topic));
    out.push_str(&format!(
        "  \"waves_run\": {}, \"deep_steps_run\": {},\n",
        r.waves_run, r.deep_steps_run
    ));
    out.push_str(&format!(
        "  \"stop_reason\": \"{}\",\n",
        match &r.stop_reason {
            Some(ExploreStop::Budget) => "Budget".into(),
            Some(ExploreStop::CurriculumStop) => "CurriculumStop".into(),
            Some(ExploreStop::FailClosed(e)) => format!("FailClosed({})", e),
            None => "-".into(),
        }
    ));
    out.push_str("  \"results\": {\n");
    for (i, (id, (kind, o))) in r.results.iter().enumerate() {
        let (oc, ev) = match o {
            TaskOutcome::Pass { evidence } => ("Pass", evidence.clone()),
            TaskOutcome::Fail { evidence } => ("Fail", evidence.clone()),
            TaskOutcome::Undecidable { reason } => ("Undecidable", reason.clone()),
        };
        let comma = if i + 1 < r.results.len() { "," } else { "" };
        out.push_str(&format!(
            "    \"{}\": [\"{:?}\", \"{}\", \"{}\"]{}\n",
            id,
            kind,
            oc,
            ev.replace('"', "'"),
            comma
        ));
    }
    out.push_str("  },\n");
    out.push_str(&format!("  \"consolidated\": {},\n", r.consolidated.len()));
    out.push_str(&format!("  \"has_findings\": {},\n", r.has_findings()));
    out.push_str(&format!("  \"frozen\": {}\n", r.frozen));
    out.push('}');
    out
}

fn cmd_explore(path: &str, topic: &str, drs_only: bool) -> Result<i32, String> {
    // 门禁 5：探针执行全沙箱。探索循环对目标管线先做静态检查（parse+check），
    // 把结果喂 curriculum 当首轮记忆概况。
    let pl = match parse_pipeline_file(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("explore: {}", e);
            return Ok(1);
        }
    };
    let errs = check_pipeline(&pl);
    let header = format!(
        "目标管线 {}\nproc 数 {}\n静态检查 {}\n主题: {}",
        path,
        pl.procs.len(),
        if errs.is_empty() {
            "通过".to_string()
        } else {
            format!("{:?}", errs)
        },
        topic
    );

    let stages = if drs_only {
        Stages {
            brs: false,
            drs: true,
        }
    } else {
        Stages::default()
    };
    let mut last_gap = String::new();

    let mut curriculum =
        |r: &ExploreReport| -> Result<crate::L4_structure::explore::CurriculumOutput, String> {
            let mut ctx = format!(
                "{}\n\n已探索: 波 {} / 深步 {} / 题数 {}\n上轮缺口: {}\n\n产出下一批探针 JSON。",
                header,
                r.waves_run,
                r.deep_steps_run,
                r.results.len(),
                if last_gap.is_empty() { "-" } else { &last_gap }
            );
            for (id, (kind, o)) in r.results.iter().take(8) {
                ctx.push_str(&format!(
                    "\n- {} {:?}: {}",
                    id,
                    kind,
                    match o {
                        TaskOutcome::Pass { .. } => "Pass".into(),
                        TaskOutcome::Fail { evidence } => format!("Fail({})", evidence),
                        TaskOutcome::Undecidable { reason } => format!("Undecidable({})", reason),
                    }
                ));
            }
            let raw = curriculum_llm_call(&ctx)?;
            let out = parse_curriculum_json(&raw)?;
            last_gap = out.gap.clone();
            Ok(out)
        };
    let mut task_store: std::collections::BTreeMap<
        String,
        crate::L4_structure::explore::ExploreTask,
    > = std::collections::BTreeMap::new();
    let mut execute = |t: &crate::L4_structure::explore::ExploreTask| {
        task_store.insert(t.id.clone(), t.clone());
        explore_execute(t)
    };
    let mut judge = |t: &crate::L4_structure::explore::ExploreTask, a: &str| explore_judge(t, a);

    eprintln!(
        "explore: BRS={} DRS={} budget={}波/{}步",
        stages.brs, stages.drs, 8, 12
    );
    let mut report = run_explore_loop(
        topic,
        &ExploreBudget::default(),
        &stages,
        &mut curriculum,
        &mut execute,
        &mut judge,
    );
    consolidate(&mut report, &task_store);
    let json = explore_report_json(&report);
    println!("{}", json);

    // freeze（门禁 6）：报告写入持久位置后置 frozen，二次检索走只读路径。
    // 目录 ~/.local/share/ductile/explore/（跟随主库 DUCTILE_DATA 数据根）。
    let data_root = std::env::var("DUCTILE_DATA").unwrap_or_else(|_| {
        format!(
            "{}/.local/share/ductile",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    let explore_dir = std::path::Path::new(&data_root).join("explore");
    let _ = std::fs::create_dir_all(&explore_dir);
    let stamp = crate::L0_physical::time::now_ts().replace(['-', ':', ' '], "");
    // 文件名卫生（P3 自举实锤：中文长 topic 超 255 字节文件名上限 → os error 36）。
    // 只保留 [A-Za-z0-9_-]，截 40；清洗后为空回退 "topic"。
    let safe: String = topic
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(40)
        .collect();
    let safe = if safe.is_empty() {
        "topic".to_string()
    } else {
        safe
    };
    let report_name = format!("{}_{}.json", safe, stamp);
    let report_path = explore_dir.join(&report_name);
    std::fs::write(&report_path, &json).map_err(|e| format!("freeze: {}", e))?;
    eprintln!("explore report (frozen): {}", report_path.display());
    eprintln!(
        "explore report id: {}",
        report_name.trim_end_matches(".json")
    );
    Ok(0)
}

fn cmd_explore_report(path: &str, id: &str) -> Result<i32, String> {
    let _ = path;
    // 只读检索冻结报告（门禁 6：本路径零写——无 UPDATE/INSERT，只 println）。
    let data_root = std::env::var("DUCTILE_DATA").unwrap_or_else(|_| {
        format!(
            "{}/.local/share/ductile",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    let explore_dir = std::path::Path::new(&data_root).join("explore");
    let mut hits: Vec<std::path::PathBuf> = std::fs::read_dir(&explore_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().starts_with(id))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    hits.sort();
    if hits.is_empty() {
        eprintln!(
            "explore report: 无匹配 {} 的冻结报告（目录 {}）",
            id,
            explore_dir.display()
        );
        return Ok(1);
    }
    for h in &hits {
        let body = std::fs::read_to_string(h).unwrap_or_default();
        println!("== {} ==\n{}", h.display(), body);
    }
    Ok(0)
}

/// v0.19 审计⑤：库快照——wal_checkpoint(TRUNCATE) 折叠 WAL 后整库拷贝到
/// `$DUCTILE_DATA/archive/`，保留最近 ARCHIVE_KEEP 份（时间戳命名，超出淘汰
/// 最旧）。SQLite 单点无归档的最低限度物理保障。
const ARCHIVE_KEEP: usize = 10;

fn cmd_archive() -> Result<i32, String> {
    use std::path::PathBuf;
    let db = db::db_path();
    if !db.exists() {
        return Err(format!("no database at {}", db.display()));
    }
    // 折叠 WAL 进主库，拷贝才是完整态
    let conn = rusqlite::Connection::open(&db).map_err(|e| format!("open: {e}"))?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .map_err(|e| format!("checkpoint: {e}"))?;
    drop(conn);
    let mut dir: PathBuf = db.clone();
    dir.pop();
    dir.push("archive");
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
    let stamp = crate::L0_physical::time::now_ts().replace(['-', ':', ' '], "");
    let dest = dir.join(format!("ductile_{}.db", stamp));
    std::fs::copy(&db, &dest).map_err(|e| format!("copy: {e}"))?;
    // retention: 最旧淘汰
    let mut snaps: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().starts_with("ductile_"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    snaps.sort();
    while snaps.len() > ARCHIVE_KEEP {
        let oldest = snaps.remove(0);
        let _ = std::fs::remove_file(&oldest);
    }
    println!(
        "archived: {} ({} bytes, keep={})",
        dest.display(),
        std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0),
        ARCHIVE_KEEP
    );
    Ok(0)
}

fn cmd_version_save(path: &str, desc: &str) -> Result<i32, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("{}", e))?;
    let pl = parse_pipeline_file(path).map_err(|e| format!("{}", e))?;
    let new_v = version::save_version(&pl.name, &content, desc);
    println!("Saved version v{}", new_v);
    Ok(0)
}

fn cmd_version_log(path: &str) -> Result<i32, String> {
    let pl = parse_pipeline_file(path).map_err(|e| format!("{}", e))?;
    match version::read_log(&pl.name) {
        Some(log) => {
            println!("{}", log);
            Ok(0)
        }
        None => {
            println!("No versions recorded.");
            Ok(0)
        }
    }
}

fn cmd_version_diff(path: &str, v1: &str, v2: &str) -> Result<i32, String> {
    let pl = parse_pipeline_file(path).map_err(|e| format!("{}", e))?;
    let v1n: usize = v1.parse().map_err(|_| "invalid version number")?;
    let v2n: usize = v2.parse().map_err(|_| "invalid version number")?;
    match version::diff_versions(&pl.name, v1n, v2n) {
        Some(diff) => {
            println!("{}", diff);
            Ok(0)
        }
        None => {
            println!("Version(s) not found.");
            Ok(0)
        }
    }
}

// ── patch ──

/// v0.18.6 P1-1：patch 生命周期迁移。tentative → confirmed（验证通过，生效）
/// 或 → reverted（证伪/回滚，保留审计痕迹）。id 可用 `patch list` 查。
fn cmd_patch_transition(id: &str, to: &str) -> Result<i32, String> {
    let id: i64 = id.parse().map_err(|_| format!("bad patch id: {id}"))?;
    // v0.20 Replay-RSI：tentative → confirmed 的重放门（永不退化条款）。
    // 带效果声明的 patch 必须重放分严格更优才放行——制度保证，不靠自觉。
    // 无声明（纯 guide 文本）→ 人审通道，门不拦（REPLAY-HUMAN）。
    if to == "confirmed" {
        let conn = db::open_try()?;
        let status: String = conn
            .query_row("SELECT status FROM patches WHERE id=?1", [id], |r| r.get(0))
            .unwrap_or_default();
        if status == "tentative" {
            let value: String = conn
                .query_row("SELECT value FROM patches WHERE id=?1", [id], |r| r.get(0))
                .map_err(|_| format!("patch #{id} not found"))?;
            match crate::L4_structure::replay::replay_verdict(&value, 0.02, 0.01) {
                Ok(crate::L4_structure::replay::ReplayVerdict::Reject {
                    v_pi0,
                    v_new,
                    n_sessions,
                }) => {
                    println!(
                        "✗ replay gate REJECTED patch #{id}: V(pi0)={v_pi0:+.4} >= V(new)={v_new:+.4} over {n_sessions} sessions"
                    );
                    println!("  confirm blocked — patch stays tentative (revert it or change the effect)");
                    return Ok(3);
                }
                Ok(crate::L4_structure::replay::ReplayVerdict::Confirm {
                    v_pi0,
                    v_new,
                    n_sessions,
                }) => {
                    println!(
                        "✓ replay gate passed: V(pi0)={v_pi0:+.4} < V(new)={v_new:+.4} over {n_sessions} sessions"
                    );
                }
                Ok(crate::L4_structure::replay::ReplayVerdict::RejectContract { reason }) => {
                    println!("✗ replay gate CONTRACT-REJECTED patch #{id}: {reason}");
                    println!("  confirm blocked — patch stays tentative (revert it or change the effect)");
                    return Ok(3);
                }
                Ok(crate::L4_structure::replay::ReplayVerdict::HumanReview { reason }) => {
                    println!("· replay gate: human review ({reason})");
                }
                Err(e) => {
                    // 重放不可算（无 session 数据等）→ fail-closed：不拦人工 confirm，
                    // 但明示门未评估（与 contract 缺席不阻断同款语义）。
                    println!("· replay gate unevaluated: {e}");
                }
            }
        }
    }
    let conn = db::open_try()?;
    db::transition_patch_conn(&conn, id, to)?;
    println!("✓ patch #{} → {}", id, to);
    if to == "confirmed" {
        println!("  now active: next `ductile run` applies it");
    } else {
        println!("  reverted: kept for audit, no longer applied");
    }
    Ok(0)
}

fn cmd_patch_set(
    pipeline: &str,
    proc_name: &str,
    impl_name: &str,
    field: &str,
    value: &str,
) -> Result<i32, String> {
    // v0.18.5 出处标注：默认 human；进化环（doctor 处方经 rx_apply）等
    // 程序化写 patch 的路径须显式声明 origin（如 llm:qwen3.8:27b）。
    // DUCTILE_PATCH_ORIGIN 是给脚本/agent 的注入通道，人手敲命令不受影响。
    let origin = std::env::var("DUCTILE_PATCH_ORIGIN").unwrap_or_else(|_| "human".into());
    // v0.18.6 P1-1：生命周期。人手敲 = confirmed（历史语义不变）；
    // 进化环（doctor 处方）经 DUCTILE_PATCH_STATUS=tentative 写入待验证假设，
    // 验证通过才 transition 到 confirmed 生效。
    let status = std::env::var("DUCTILE_PATCH_STATUS").unwrap_or_else(|_| "confirmed".into());
    let st = match status.as_str() {
        "tentative" => "tentative",
        "reverted" => "reverted",
        _ => "confirmed",
    };
    db::set_patch(pipeline, proc_name, impl_name, field, value, &origin, st);
    println!(
        "✓ Patched: {}.{}.{} = {}",
        pipeline, proc_name, impl_name, field
    );
    println!("  {} = {}", field, value);
    println!("  origin: {} | status: {}", origin, st);
    println!("\nNext `ductile run` will use this override. Source file not modified.");
    Ok(0)
}

fn cmd_patch_list() -> Result<i32, String> {
    let patches = db::all_patches();
    if patches.is_empty() {
        println!("No patches set.");
        return Ok(0);
    }
    let active = patches.iter().filter(|p| p.status == "confirmed").count();
    println!(
        "Patches ({} total, {} confirmed active):\n",
        patches.len(),
        active
    );
    for p in &patches {
        println!(
            "  #{} {}.{}.{} = {}",
            p.id, p.pipeline, p.proc_name, p.impl_name, p.field
        );
        println!("    → {}", p.value);
        println!("    origin: {} | status: {}", p.origin, p.status);
    }
    Ok(0)
}

fn cmd_patch_clear(pipeline: &str) -> Result<i32, String> {
    // Delete all patches for a pipeline
    db::init_db();
    let conn = db::open();
    conn.execute("DELETE FROM patches WHERE pipeline = ?1", params![pipeline])
        .ok();
    println!("✓ Cleared all patches for pipeline: {}", pipeline);
    Ok(0)
}

// ── fts (BM25 full-text search) ──

fn cmd_fts(query: &str) -> Result<i32, String> {
    db::init_db();
    // Try FTS search; fall back to LIKE if FTS index not built yet
    let results = db::search_fts(query, 20);
    if results.is_empty() {
        println!("No results for \"{}\" (BM25).", query);
        return Ok(0);
    }
    println!("BM25 search \"{}\" — {} results:\n", query, results.len());
    for r in &results {
        let tag_str: Vec<String> = r.tags.iter().map(|t| format!("#{t}")).collect();
        print!("  {}", r.name);
        if !tag_str.is_empty() {
            print!(" {{{}}}", tag_str.join(", "));
        }
        println!("  [score: {:.2}]", r.bm25_score);
        if !r.description.is_empty() {
            println!("    {}", r.description);
        }
        println!("    [{}, {} impls]", r.pipeline, r.impl_count);
    }
    Ok(0)
}

// ── helpers ──

// ── 纯参数解析（无 I/O，可单测）──

/// run 子命令参数：--policy <file> / --restrict-shell 任意位置，其余 token 拼 topic。
pub fn split_run_args(args: &[String]) -> Result<(String, Option<String>, bool), String> {
    let mut policy_path: Option<String> = None;
    let mut restrict = false;
    let mut topic_parts: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--policy" {
            if i + 1 >= args.len() {
                return Err("--policy requires a .eval file path".into());
            }
            policy_path = Some(args[i + 1].clone());
            i += 2;
        } else if args[i] == "--restrict-shell" {
            restrict = true;
            i += 1;
        } else {
            topic_parts.push(&args[i]);
            i += 1;
        }
    }
    Ok((topic_parts.join(" "), policy_path, restrict))
}

/// promote 参数：[days] [top] [--dry]，按**位置**区分（1st→days, 2nd→top）。
/// 原实现按类型分支派发——但任何 u32 都能 parse 成 usize，第二个数字
/// 永远命中 days 分支覆盖之，top 从未生效（promote 30 10 实为 days=10
/// top=40 默认）。位置语义修复，单测 promote_positional_and_flag 钉死。
pub fn parse_promote_args(args: &[String]) -> (u32, usize, bool) {
    let mut days: u32 = 7;
    let mut top: usize = 40;
    let mut dry = false;
    let mut positional = 0usize;
    for a in args {
        if a == "--dry" {
            dry = true;
        } else if positional == 0 {
            if let Ok(d) = a.parse::<u32>() {
                days = d;
                positional += 1;
            }
        } else if positional == 1 {
            if let Ok(t) = a.parse::<usize>() {
                top = t;
                positional += 1;
            }
        }
    }
    (days, top, dry)
}

/// script call 的 DSL body：空 kv → script(name)，否则 script(name, kv)。
pub fn build_script_call_body(name: &str, kv: &str) -> String {
    if kv.trim().is_empty() {
        format!("script({})", name)
    } else {
        format!("script({}, {})", name, kv)
    }
}

pub fn parse_topic_params(input: &str) -> (String, BTreeMap<String, String>) {
    let parts: Vec<&str> = input.split("--").collect();
    let topic = parts[0].trim().to_string();
    let topic = if topic.is_empty() { "AI".into() } else { topic };
    let mut params = BTreeMap::new();
    for p in &parts[1..] {
        if let Some(eq) = p.find('=') {
            let k = p[..eq].trim().to_string();
            let v = p[eq + 1..].trim().to_string();
            if !k.is_empty() {
                params.insert(k, v);
            }
        }
    }
    (topic, params)
}

// ── v0.8.1: doctor / wrap / harvest ──

/// v0.18.6 P1-3：cost_norm 测量面。聚合 runs（近窗口）按 proc × impl 统计
/// 成功样本的 avg_latency_ms / avg_tokens，再按 proc 组内归一化：
/// cost_norm = impl 均值 / 组内最优均值（1.0 = 该 proc 下最便宜）。
/// PyroDash Eq.6 的精神：跨档位比较看相对成本而非绝对成本。
/// 纯只读报告——是否进 effective 公式等盲评数据积累后再定。
fn cmd_cost_report() -> Result<i32, String> {
    let conn = db::open_try()?;
    // 聚合：最近 500 条成功 run，按 (proc, impl) 聚合
    let mut stmt = conn
        .prepare(
            "SELECT proc_name, impl_name,
                    COUNT(*), AVG(latency_ms), AVG(rate_tokens)
             FROM runs WHERE status = 'Ok'
             GROUP BY proc_name, impl_name
             ORDER BY proc_name, impl_name",
        )
        .map_err(|e| format!("query: {e}"))?;
    let rows: Vec<(String, String, i64, f64, f64)> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, f64>(4)?,
            ))
        })
        .map_err(|e| format!("query: {e}"))?
        .filter_map(|r| r.ok())
        .collect();
    if rows.is_empty() {
        println!("no successful runs recorded — nothing to report");
        return Ok(0);
    }
    // 组内最优（latency 与 tokens 各自归一；综合 = 两者的几何均值）
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<(String, i64, f64, f64)>> = BTreeMap::new();
    for (p_, i_, n, lat, tok) in &rows {
        groups
            .entry(p_.clone())
            .or_default()
            .push((i_.clone(), *n, *lat, *tok));
    }
    println!("cost_norm report (successful runs only, lower = cheaper):\n");
    let mut grand = 0.0;
    let mut cnt = 0;
    for (proc, impls) in &groups {
        let best_lat = impls.iter().map(|x| x.2).fold(f64::INFINITY, f64::min);
        let best_tok = impls.iter().map(|x| x.3).fold(f64::INFINITY, f64::min);
        println!("proc '{}':", proc);
        for (name, n, lat, tok) in impls {
            let n_lat = if best_lat > 0.0 { lat / best_lat } else { 1.0 };
            let n_tok = if best_tok > 0.0 { tok / best_tok } else { 1.0 };
            let norm = (n_lat * n_tok).sqrt();
            grand += norm;
            cnt += 1;
            println!(
                "  {:<24} n={:<4} lat={:<8.0}ms tok={:<8.0} norm={:.2}x",
                name, n, lat, tok, norm
            );
        }
        println!();
    }
    println!(
        "impls: {} | mean norm: {:.2}x",
        cnt,
        grand / cnt.max(1) as f64
    );
    Ok(0)
}

fn cmd_doctor() -> Result<i32, String> {
    let report = harvest::doctor();
    println!("Ductile doctor");
    println!("==============");
    for (name, ok, detail) in &report.checks {
        let mark = if *ok { "OK " } else { "FIX" };
        println!("  [{}] {} — {}", mark, name, detail);
    }
    Ok(if report.all_ok() { 0 } else { 1 })
}

fn cmd_wrap(tag: &str, cmd: &str) -> Result<i32, String> {
    let (code, note) = harvest::wrap_and_run(cmd, tag)?;
    eprintln!("[wrap] exit={} | {}", code, note);
    Ok(code)
}

fn cmd_grow(days: u32, top: usize) -> Result<i32, String> {
    println!(
        "MDL construction growth (last {}d, import top {}) [learn v2]",
        days, top
    );
    match grow::grow(days, top) {
        Err(e) => {
            eprintln!("grow failed: {}", e);
            Ok(1)
        }
        Ok(rep) => {
            println!(
                "  corpus: {} calls / {} distinct commands",
                rep.calls, rep.distinct
            );
            println!(
                "  rounds: {}   two-part: {:.0} b vs raw {:.0} b  (ratio {:.4})",
                rep.rounds,
                rep.bits_tp,
                rep.bits_raw,
                rep.bits_tp / rep.bits_raw
            );
            println!("  scaffolds imported: {}", rep.imported);
            Ok(0)
        }
    }
}

fn cmd_scaffold(q: &str) -> Result<i32, String> {
    use crate::promote;
    match promote::list_scaffolds(q) {
        Err(e) => {
            eprintln!("scaffold failed: {}", e);
            Ok(1)
        }
        Ok(rows) => {
            println!("Scaffolds ({} matches):", rows.len());
            for (text, save_b, uses, lines) in rows.iter().take(20) {
                println!(
                    "  [{:>7}b x{:<4} L{}] {}",
                    save_b,
                    uses,
                    lines,
                    text.split('\n')
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(70)
                        .collect::<String>()
                );
            }
            Ok(0)
        }
    }
}

fn cmd_promote(days: u32, top: usize, dry: bool) -> Result<i32, String> {
    println!(
        "MDL promotion gate{} (last {}d, top {})",
        if dry { " [DRY RUN]" } else { "" },
        days,
        top
    );
    println!("=================================================");
    match promote::promote(days, top, dry) {
        Err(e) => {
            eprintln!("promote failed: {}", e);
            Ok(1)
        }
        Ok((promoted, evaluated)) => {
            println!("-------------------------------------------------");
            if dry {
                println!("[DRY] evaluated {} candidates (no writes)", evaluated);
            } else {
                println!("promoted {} / evaluated {} candidates", promoted, evaluated);
            }
            Ok(0)
        }
    }
}

fn cmd_harvest(days: u32) -> Result<i32, String> {
    match harvest::harvest(days) {
        Err(e) => {
            eprintln!("harvest failed: {}", e);
            Ok(1)
        }
        Ok(hits) => {
            println!("Harvested from last {}d (candidates: {})", days, hits.len());
            println!("=================================================");
            for h in hits.iter().take(30) {
                println!("  [x{}] {}  (last: {})", h.count, h.cmd, h.last_seen);
            }
            if hits.is_empty() {
                println!("  (no repeated commands found)");
            } else {
                println!("\nNext: ductile wrap <tag> -- <cmd>  to promote into the library");
            }
            Ok(0)
        }
    }
}

// ── v0.22 同构门禁（check 期推模式查重）──
//
// 把 AGENTS.md 第 5 条"写新图前 hyper similar"焊进物理结构：check 通过即
// 自动扫 db 注册表，结构键精确等价 → stderr 提示可复用。拉模式变推模式。
// 分层：interface 调 L4（struct_sig）+ L0（registered_graph_files），合法下行。
fn iso_gate_print(pl: &crate::core::ast::Pipeline, src_path: &str) {
    use crate::L4_structure::hyper::struct_sig_from_pipeline;
    let qsig = struct_sig_from_pipeline(pl);
    let qkey = qsig.structure_key();
    let conn = crate::db::open();
    let canon = |p: &str| -> String {
        let a = std::fs::canonicalize(p)
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or_else(|_| p.to_string());
        a
    };
    let self_canon = canon(src_path);
    let mut hits: Vec<(String, String)> = Vec::new();
    for (name, path) in crate::db::registered_graph_files(&conn) {
        if path.ends_with(".hyper") {
            continue; // 超图走 hyper check，不在此比对
        }
        if canon(&path) == self_canon {
            continue; // 自身（重 check 自己）不算
        }
        let Ok(other) = crate::parser::parse_pipeline_file(&path) else {
            continue; // 死路径跳过（doctor 语义）
        };
        if struct_sig_from_pipeline(&other).structure_key() == qkey {
            hits.push((name, path));
        }
    }
    if !hits.is_empty() {
        eprintln!("[iso-gate] 结构等价图已存在（复用优先，AGENTS.md 第 5 条）：");
        for (name, path) in hits.iter().take(5) {
            eprintln!("  ≅ {} — {}", name, path);
        }
        eprintln!("[iso-gate] 若确为新场景，忽略本提示；否则考虑复用/参数化而不是新写。");
    }
}

// ── v0.22 OKR 编译器：NL 目标 → KR 树 → .hyper 草稿（人审后 hyper build）──
//
// OKR ↔ ductile 映射：Objective=goal / KR=stage+contract / Initiative=vertex。
// 职责分层（节点平等原则）：LLM 只做"分解提议"（origin=llm），结构与门禁
// 由引擎判定——生成的 .hyper 必须过 parse 才落盘，人审后才能 build。
// 兜底：llm 不可用 → 退出码 2，不吐半成品。
fn okr_compile(objective: &str) -> Result<i32, String> {
    let prompt = format!(
        r#"你是管线架构师。把目标分解为 OKR 超图草稿。
目标：{objective}

硬性文法（违反=引擎拒绝落盘）：
- 第一行 HyperGraph("名")，名用小写下划线
- .goal("一句话目标")
- .require(judge=true, min_impls=1)
- 每个关键结果 KR 一行 .vertex("kr名", role=source|default|judge|sink, tags=#标签)
  KR 名必须可判定（如 kr_score_ge_80 不是"提高质量"）
- .hedge("flow", kind=chain, 依序列出全部 vertex)
- 若有 judge vertex，加 .hedge("quality", kind=gate, judge=该vertex, producers=数据生产者, consumers=受门禁者)
- .deliver(sink名)
- 全部 8-14 行，不加任何解释

只输出 .hyper 内容本身，不要代码围栏。"#,
    );
    // llm 桥直调（与 exec_llm 同源路径）；失败 fail-closed
    let bridge = crate::steps::find_bridge("llm_bridge.py");
    if bridge.is_empty() || !std::path::Path::new(&bridge).exists() {
        return Err("llm bridge not found — OKR 编译器依赖 llm()".into());
    }
    let out = std::process::Command::new("python3")
        .arg(&bridge)
        .arg("--prompt")
        .arg(&prompt)
        .output()
        .map_err(|e| format!("llm bridge launch failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "llm failed: {}",
            String::from_utf8_lossy(&out.stderr).chars().take(300).collect::<String>()
        ));
    }
    let raw = String::from_utf8_lossy(&out.stdout).to_string();
    // 剥可能的代码围栏与前后空行
    let body = raw
        .trim()
        .trim_start_matches("```hyper")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
        .to_string();
    // ##DSL_RESULT 污染防护：截到 HyperGraph 块止
    let body = if let Some(p) = body.find("##DSL_RESULT") {
        body[..p].trim().to_string()
    } else {
        body
    };
    // fail-closed：必须 parse 得过才落盘（LLM 提议 ≠ 结构合法）
    crate::L4_structure::hyper::parse_hyper(&body).map_err(|e| format!("LLM 草稿不合文法（{e}）— 不落盘"))?;
    let fname = format!("okr_{}.hyper", crate::L4_structure::hyper::parse_hyper(&body).unwrap().name);
    std::fs::write(&fname, format!("// OKR 编译草稿（origin=llm，人审后 ductile hyper build {fname} -o <out>.pipeline）\n{body}\n"))
        .map_err(|e| format!("write {fname}: {e}"))?;
    println!("OKR 草稿已落盘：{fname}（LLM 提议，未经人审禁止 build）");
    println!("下一步：人审 → ductile hyper build {fname} -o <out>.pipeline → ductile check");
    Ok(0)
}

// ── v0.12.1 参数解析与分发单测 ──

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // ── split_run_args ──

    #[test]
    fn run_args_policy_extracted() {
        let (topic, policy, restrict) =
            split_run_args(&s(&["my", "topic", "--policy", "p.eval"])).unwrap();
        assert_eq!(topic, "my topic");
        assert_eq!(policy.as_deref(), Some("p.eval"));
        assert!(!restrict);
    }

    #[test]
    fn run_args_policy_first() {
        let (topic, policy, _) = split_run_args(&s(&["--policy", "p.eval", "hello"])).unwrap();
        assert_eq!(topic, "hello");
        assert_eq!(policy.as_deref(), Some("p.eval"));
    }

    #[test]
    fn run_args_no_policy() {
        let (topic, policy, restrict) = split_run_args(&s(&["just", "topic"])).unwrap();
        assert_eq!(topic, "just topic");
        assert!(policy.is_none());
        assert!(!restrict);
    }

    #[test]
    fn run_args_policy_missing_value_err() {
        assert!(split_run_args(&s(&["t", "--policy"])).is_err());
    }

    #[test]
    fn run_args_policy_consumes_next_token() {
        // --policy 后跟的 token 不会被误当 topic
        let (topic, _, _) =
            split_run_args(&s(&["--policy", "a.eval", "x", "--policy", "b.eval"])).unwrap();
        assert_eq!(topic, "x");
    }

    #[test]
    fn run_args_restrict_shell_flag() {
        let (topic, policy, restrict) = split_run_args(&s(&["hello", "--restrict-shell"])).unwrap();
        assert_eq!(topic, "hello");
        assert!(policy.is_none());
        assert!(restrict);
    }

    // ── parse_promote_args ──

    #[test]
    fn promote_defaults() {
        assert_eq!(parse_promote_args(&[]), (7, 40, false));
    }

    #[test]
    fn promote_positional_and_flag() {
        assert_eq!(
            parse_promote_args(&s(&["30", "10", "--dry"])),
            (30, 10, true)
        );
    }

    #[test]
    fn promote_only_flag() {
        assert_eq!(parse_promote_args(&s(&["--dry"])), (7, 40, true));
    }

    // ── build_script_call_body ──

    #[test]
    fn script_call_body_empty_kv() {
        assert_eq!(build_script_call_body("report", ""), "script(report)");
        assert_eq!(build_script_call_body("report", "   "), "script(report)");
    }

    #[test]
    fn script_call_body_with_kv() {
        assert_eq!(
            build_script_call_body("report", "topic=x"),
            "script(report, topic=x)"
        );
    }

    // ── parse_topic_params ──

    #[test]
    fn topic_params_split() {
        let (topic, params) = parse_topic_params("量子计算 --mode=fast --n=3");
        assert_eq!(topic, "量子计算");
        assert_eq!(params.get("mode").unwrap(), "fast");
        assert_eq!(params.get("n").unwrap(), "3");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn topic_params_empty_topic_defaults_ai() {
        let (topic, params) = parse_topic_params("--mode=deep");
        assert_eq!(topic, "AI");
        assert_eq!(params.get("mode").unwrap(), "deep");
    }

    #[test]
    fn topic_params_no_eq_skipped() {
        let (_, params) = parse_topic_params("t --flag --k=v");
        assert_eq!(params.len(), 1);
        assert!(params.contains_key("k"));
    }

    #[test]
    fn topic_params_empty_key_skipped() {
        let (_, params) = parse_topic_params("t -- =v");
        assert!(params.is_empty());
    }

    // ── 分发冒烟：usage 退出码 ──

    #[test]
    fn dispatch_no_args_usage() {
        let args: Vec<String> = vec![];
        assert_eq!(run(&args).unwrap(), 1);
        assert_eq!(run(&s(&["ductile"])).unwrap(), 1);
        assert_eq!(run(&s(&["ductile", "no-such-cmd"])).unwrap(), 1);
    }
}
