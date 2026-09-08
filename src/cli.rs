//! CLI dispatch module — shared between binary and Python binding.

use crate::*;
use rusqlite::params;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

pub fn run(args: &[String]) -> Result<i32, String> {
    if args.len() < 2 {
        print_usage();
        return Ok(1);
    }

    match args[1].as_str() {
        // Core
        "check" if args.len() >= 3 => match parse_pipeline_file(&args[2]) {
            Err(e) => {
                eprintln!("{}", e);
                Ok(1)
            }
            Ok(pl) => {
                let errs = check_pipeline(&pl);
                if errs.is_empty() {
                    println!("Type check passed");
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

        // Database
        "import" if args.len() >= 3 => cmd_import(&args[2..]),
        "search" if args.len() >= 3 => cmd_search(&args[2]),
        "fts" if args.len() >= 3 => cmd_fts(&args[2]),
        "compose" if args.len() >= 5 => cmd_compose(&args[2], &args[3], &args[4..]),
        "db-stats" => cmd_db_stats(),

        // Discovery
        "discover" if args.len() >= 3 => cmd_discover(Some(&args[2])),
        "discover" => cmd_discover(None),
        "learn" if args.len() >= 3 => cmd_learn(&args[2]),
        "learn" => cmd_learn(""),

        // v0.8.1 harvest line
        "doctor" => cmd_doctor(),
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
        "version" if args.len() >= 5 && args[2] == "save" => cmd_version_save(&args[3], &args[4]),
        "version" if args.len() >= 4 && args[2] == "log" => cmd_version_log(&args[3]),
        "version" if args.len() >= 6 && args[2] == "diff" => {
            cmd_version_diff(&args[3], &args[4], &args[5])
        }

        // Hot patch: patch <pipeline> <proc> <impl> <field> <value>
        "patch" if args.len() >= 3 && args[2] == "list" => cmd_patch_list(),
        "patch" if args.len() >= 4 && args[2] == "clear" => cmd_patch_clear(&args[3]),
        "patch" if args.len() >= 7 => {
            cmd_patch_set(&args[2], &args[3], &args[4], &args[5], &args[6])
        }

        // v0.12 script contract line — 脚本即 API
        "script" if args.len() >= 3 && args[2] == "attach" && args.len() >= 4 => {
            cmd_script_attach(&args[3])
        }
        "script" if args.len() >= 3 && args[2] == "detach" && args.len() >= 4 => {
            cmd_script_detach(&args[3])
        }
        "script" if args.len() >= 3 && args[2] == "list" => cmd_script_list(),
        "script" if args.len() >= 3 && args[2] == "show" && args.len() >= 4 => {
            cmd_script_show(&args[3])
        }
        "script" if args.len() >= 3 && args[2] == "call" && args.len() >= 5 => {
            cmd_script_call(&args[3], &args[4])
        }

        _ => {
            print_usage();
            Ok(1)
        }
    }
}

// ── v0.12 script contract line — 脚本即 API ──

fn cmd_script_attach(path: &str) -> Result<i32, String> {
    let source =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {}", path, e))?;
    let card = crate::script::parse_contract(&source, path)?;
    crate::db::script_attach(&card)?;
    println!(
        "attached: {} (lang={}, pure={}, idempotent={}, concurrency={}, effects={})",
        card.name,
        card.lang,
        card.pure,
        card.idempotent,
        card.concurrency.as_str(),
        card.effects
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
    eprintln!();
    eprintln!("Database:");
    eprintln!("  import <dir|file>      Import pipelines into SQLite");
    eprintln!("  search \"query\"         Search proc library (LIKE)");
    eprintln!("  fts \"query\"            BM25 full-text search (relevance ranked)");
    eprintln!("  compose <name> <desc> <#tag1> <#tag2>...  Assemble from library");
    eprintln!("  db-stats               Show database statistics");
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
    eprintln!("Script contracts (v0.12 — 脚本即 API):");
    eprintln!("  script attach <file>    Register script (parses # ductile: contract header)");
    eprintln!("  script list             Show attached scripts");
    eprintln!("  script show <name>      Show contract card (LLM reads this, not the script)");
    eprintln!("  script call <name> \"k=v, k=v\"   One-off invoke (debug)");
    eprintln!("  script detach <name>    Unregister");
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
                        .map(|i| (prefs.get(&proc.name, &i.name), i.name.as_str()))
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
    let mut eg = build_egraph(&pl);
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
                    if fp.extension().map(|e| e == "pipeline").unwrap_or(false) {
                        all_files.push(fp.to_string_lossy().to_string());
                    }
                }
            }
        } else if path.is_file() {
            all_files.push(p.clone());
        }
    }
    if all_files.is_empty() {
        println!("No .pipeline files found.");
        return Ok(0);
    }
    let mut ok = 0;
    let mut errs = 0;
    for f in &all_files {
        match db::import_pipeline_file(f) {
            Ok(name) => {
                println!("  ✓ {} ({})", name, f);
                ok += 1;
            }
            Err(e) => {
                println!("  ✗ {} ({})", e, f);
                errs += 1;
            }
        }
    }
    println!("\nImported {} pipelines ({} errors)", ok, errs);
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
    println!("Proc Registry — Isomorphism Discovery");
    println!("=====================================");
    println!("\nTotal registered procs: {}", all_procs.len());
    println!("Isomorphic groups (≥2 pipelines): {}\n", groups.len());
    if groups.is_empty() {
        println!("No isomorphic groups found.");
    } else {
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

fn cmd_patch_set(
    pipeline: &str,
    proc_name: &str,
    impl_name: &str,
    field: &str,
    value: &str,
) -> Result<i32, String> {
    db::set_patch(pipeline, proc_name, impl_name, field, value);
    println!(
        "✓ Patched: {}.{}.{} = {}",
        pipeline, proc_name, impl_name, field
    );
    println!("  {} = {}", field, value);
    println!("\nNext `ductile run` will use this override. Source file not modified.");
    Ok(0)
}

fn cmd_patch_list() -> Result<i32, String> {
    let patches = db::all_patches();
    if patches.is_empty() {
        println!("No patches set.");
        return Ok(0);
    }
    println!("Active patches ({}):\n", patches.len());
    for p in &patches {
        println!(
            "  {}.{}.{} = {}",
            p.pipeline, p.proc_name, p.impl_name, p.field
        );
        println!("    → {}", p.value);
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
        let (topic, policy, restrict) =
            split_run_args(&s(&["hello", "--restrict-shell"])).unwrap();
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
