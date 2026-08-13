//! CLI dispatch module — shared between binary and Python binding.

use crate::*;
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
            Err(e) => { eprintln!("{}", e); Ok(1) }
            Ok(pl) => {
                let errs = check_pipeline(&pl);
                if errs.is_empty() { println!("Type check passed"); Ok(0) }
                else {
                    eprintln!("Type check errors:");
                    for e in &errs { eprintln!("  {}", e); }
                    Ok(1)
                }
            }
        },
        "run" if args.len() >= 3 => {
            let topic = if args.len() > 3 { args[3..].join(" ") } else { String::new() };
            cmd_run(&args[2], &topic)
        }
        "graph" if args.len() >= 3 => cmd_graph(&args[2]),
        "parse" if args.len() >= 3 => cmd_parse(&args[2]),

        // Database
        "import" if args.len() >= 3 => cmd_import(&args[2..]),
        "search" if args.len() >= 3 => cmd_search(&args[2]),
        "compose" if args.len() >= 5 => cmd_compose(&args[2], &args[3], &args[4..]),
        "db-stats" => cmd_db_stats(),

        // Discovery
        "discover" if args.len() >= 3 => cmd_discover(Some(&args[2])),
        "discover" => cmd_discover(None),
        "learn" if args.len() >= 3 => cmd_learn(&args[2]),
        "learn" => cmd_learn(""),

        // Version
        "version" if args.len() >= 5 && args[2] == "save" => cmd_version_save(&args[3], &args[4]),
        "version" if args.len() >= 4 && args[2] == "log" => cmd_version_log(&args[3]),
        "version" if args.len() >= 6 && args[2] == "diff" => cmd_version_diff(&args[3], &args[4], &args[5]),

        _ => { print_usage(); Ok(1) }
    }
}

fn print_usage() {
    eprintln!("Ductile v{} — declarative pipeline DSL", env!("CARGO_PKG_VERSION"));
    eprintln!();
    eprintln!("Usage: ductile <command> <file.pipeline> [args]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  check <file>           Parse + type check");
    eprintln!("  run   <file> [topic]   Parse + check + execute");
    eprintln!("  graph <file>           Show e-graph structure");
    eprintln!("  parse <file>           Parse only (show structure)");
    eprintln!();
    eprintln!("Database:");
    eprintln!("  import <dir|file>      Import pipelines into SQLite");
    eprintln!("  search \"query\"         Search proc library");
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
}

// ── run ──

fn cmd_run(path: &str, topic_str: &str) -> Result<i32, String> {
    let pl = match parse_pipeline_file(path) {
        Err(e) => { eprintln!("{}", e); return Ok(1); }
        Ok(pl) => pl,
    };
    let errs = check_pipeline(&pl);
    if !errs.is_empty() {
        eprintln!("Type check errors:");
        for e in &errs { eprintln!("  {}", e); }
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
            println!("  {} ≅ {} [{}] — {}",
                m.local_proc, m.remote.name, m.remote.pipeline, m.remote.description);
        }
        println!();
    }

    match exec_pipeline(&topic, &params, &pl) {
        ExecResult::Success(results) => {
            println!();
            println!("Pipeline executed successfully");
            println!("  Procs completed: {}", results.len());
            for (k, v) in &results {
                let shown = match v {
                    Value::Text(t) => if t.len() > 200 { format!("{}...", &t[..200]) } else { t.clone() },
                    Value::File(f) => format!("<file: {}>", f),
                    Value::Null => "<null>".to_string(),
                };
                println!("    {} => {}", k, shown);
            }
            Ok(0)
        }
        ExecResult::Failed(err) => { eprintln!("Pipeline failed: {}", err); Ok(1) }
    }
}

// ── graph ──

fn cmd_graph(path: &str) -> Result<i32, String> {
    let pl = match parse_pipeline_file(path) { Err(e) => { eprintln!("{}", e); return Ok(1); } Ok(pl) => pl };
    let eg = build_egraph(&pl);
    let groups = parallel_groups(&eg);
    let cp = critical_path(&eg);
    println!("E-Graph:\n  Nodes: {}\n  Edges: {}\n", pl.procs.len(), eg.edges.len());
    println!("Parallel groups:");
    for g in &groups { println!("  {:?}", g); }
    println!("\nCritical path: {:?}", cp);
    Ok(0)
}

// ── parse ──

fn cmd_parse(path: &str) -> Result<i32, String> {
    let pl = match parse_pipeline_file(path) { Err(e) => { eprintln!("{}", e); return Ok(1); } Ok(pl) => pl };
    println!("Pipeline: {}", pl.name);
    if !pl.description.is_empty() { println!("Description: {}", pl.description); }
    let tags = pl.computed_tags();
    if !tags.is_empty() {
        let tag_str: Vec<String> = tags.iter().map(|t| format!("#{}", t)).collect();
        println!("Tags: {}", tag_str.join(", "));
    }
    println!();
    for proc in &pl.procs {
        println!("  Proc: {} ({} impls){}", proc.name, proc.plan.len(),
            if proc.deliver { " [deliver]" } else { "" });
        if !proc.description.is_empty() { println!("    Desc: {}", proc.description); }
        for imp in &proc.plan {
            let tag_str: Vec<String> = imp.tags.iter().map(|t| format!("#{}", t)).collect();
            print!("    Impl: {}", imp.name);
            if !tag_str.is_empty() { print!(" [{}]", tag_str.join(", ")); }
            println!();
            if !imp.description.is_empty() { println!("      Desc: {}", imp.description); }
        }
        for chk in &proc.checks { println!("    Check: \"{}\"", chk.msg); }
    }

    // Isomorphism hints
    let registry = registry::load_all_entries();
    let matches = registry::find_isomorphic_matches(&pl.name, &registry, &pl);
    if !matches.is_empty() {
        println!("\nIsomorphism hints:");
        for m in &matches {
            println!("  {} ≅ {} [{}] — {}",
                m.local_proc, m.remote.name, m.remote.pipeline, m.remote.description);
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
    if all_files.is_empty() { println!("No .pipeline files found."); return Ok(0); }
    let mut ok = 0;
    let mut errs = 0;
    for f in &all_files {
        match db::import_pipeline_file(f) {
            Ok(name) => { println!("  ✓ {} ({})", name, f); ok += 1; }
            Err(e) => { println!("  ✗ {} ({})", e, f); errs += 1; }
        }
    }
    println!("\nImported {} pipelines ({} errors)", ok, errs);
    Ok(0)
}

// ── search ──

fn cmd_search(query: &str) -> Result<i32, String> {
    db::init_db();
    let results = db::search_procs(query);
    if results.is_empty() { println!("No matching procs found."); return Ok(0); }
    println!("Search \"{}\" — {} results:\n", query, results.len());
    for p in &results {
        let tag_str: Vec<String> = p.tags.iter().map(|t| format!("#{}", t)).collect();
        print!("  {}", p.name);
        if !tag_str.is_empty() { print!(" {{{{{}}}}}", tag_str.join(", ")); }
        println!();
        if !p.description.is_empty() { println!("    {}", p.description); }
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
        let candidates: Vec<&db::ProcRow> = all_procs.iter()
            .filter(|p| tags.is_subset(&p.tags))
            .collect();
        println!("Step {} {{{}}}:", i + 1, tags.iter().map(|t| format!("#{}", t)).collect::<Vec<_>>().join(", "));
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
            Err(e) => { eprintln!("{}", e); return Ok(1); }
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
        println!("  {{{}}} {} — {} [{}]", tag_str.join(", "), p.name, p.description, p.pipeline);
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
            println!("  [×{}] {} → found in: {}",
                r.count,
                r.tags.join(" → "),
                r.pipelines.join(", "));
        }
    }
    println!("\nCompression summary:");
    println!("  Original:   {} tokens", result.total_tokens);
    println!("  Compressed: {} tokens", result.compressed_size);
    println!("  Ratio:      {:.1}%", result.compression_ratio * 100.0);
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
        Some(log) => { println!("{}", log); Ok(0) }
        None => { println!("No versions recorded."); Ok(0) }
    }
}

fn cmd_version_diff(path: &str, v1: &str, v2: &str) -> Result<i32, String> {
    let pl = parse_pipeline_file(path).map_err(|e| format!("{}", e))?;
    let v1n: usize = v1.parse().map_err(|_| "invalid version number")?;
    let v2n: usize = v2.parse().map_err(|_| "invalid version number")?;
    match version::diff_versions(&pl.name, v1n, v2n) {
        Some(diff) => { println!("{}", diff); Ok(0) }
        None => { println!("Version(s) not found."); Ok(0) }
    }
}

// ── helpers ──

fn parse_topic_params(input: &str) -> (String, BTreeMap<String, String>) {
    let parts: Vec<&str> = input.split("--").collect();
    let topic = parts[0].trim().to_string();
    let topic = if topic.is_empty() { "AI".into() } else { topic };
    let mut params = BTreeMap::new();
    for p in &parts[1..] {
        if let Some(eq) = p.find('=') {
            let k = p[..eq].trim().to_string();
            let v = p[eq + 1..].trim().to_string();
            if !k.is_empty() { params.insert(k, v); }
        }
    }
    (topic, params)
}
