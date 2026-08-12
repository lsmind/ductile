//! CLI dispatch module — shared between binary and Python binding.

use crate::*;
use std::collections::BTreeMap;

pub fn run(args: &[String]) -> Result<i32, String> {
    if args.len() < 2 {
        print_usage();
        return Ok(1);
    }

    match args[1].as_str() {
        "check" if args.len() >= 3 => {
            match parse_pipeline_file(&args[2]) {
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
            }
        }
        "run" if args.len() >= 3 => {
            let topic = if args.len() > 3 { args[3..].join(" ") } else { String::new() };
            cmd_run(&args[2], &topic)
        }
        "graph" if args.len() >= 3 => {
            cmd_graph(&args[2])
        }
        "parse" if args.len() >= 3 => {
            cmd_parse(&args[2])
        }
        _ => {
            print_usage();
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
    eprintln!("  run   <file> [topic]   Parse + check + execute");
    eprintln!("  graph <file>           Show e-graph structure");
    eprintln!("  parse <file>           Parse only (show structure)");
}

fn cmd_run(path: &str, topic_str: &str) -> Result<i32, String> {
    let pl = match parse_pipeline_file(path) {
        Err(e) => {
            eprintln!("{}", e);
            return Ok(1);
        }
        Ok(pl) => pl,
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
    if !params.is_empty() {
        let pstr: Vec<String> = params.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
        println!("Params: {}", pstr.join(", "));
    }
    println!();

    match exec_pipeline(&topic, &params, &pl) {
        ExecResult::Success(results) => {
            println!();
            println!("Pipeline executed successfully");
            println!("  Procs completed: {}", results.len());
            for (k, v) in &results {
                let shown = match v {
                    Value::Text(t) => {
                        if t.len() > 200 { format!("{}...", &t[..200]) } else { t.clone() }
                    }
                    Value::File(f) => format!("<file: {}>", f),
                    Value::Null => "<null>".to_string(),
                };
                println!("    {} => {}", k, shown);
            }
            Ok(0)
        }
        ExecResult::Failed(err) => {
            eprintln!("Pipeline failed: {}", err);
            Ok(1)
        }
    }
}

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

    println!("E-Graph:");
    println!("  Nodes: {}", pl.procs.len());
    println!("  Edges: {}", eg.edges.len());
    println!();
    println!("Parallel groups:");
    for g in &groups {
        println!("  {:?}", g);
    }
    println!();
    println!("Critical path: {:?}", cp);
    Ok(0)
}

fn cmd_parse(path: &str) -> Result<i32, String> {
    let pl = match parse_pipeline_file(path) {
        Err(e) => {
            eprintln!("{}", e);
            return Ok(1);
        }
        Ok(pl) => pl,
    };

    println!("Pipeline: {}", pl.name);
    println!("Effects: {:?}", pl.effects);
    println!("Min level: {}", pl.min_level);
    println!();
    for proc in &pl.procs {
        println!(
            "  Proc: {} {} ({} impls){}",
            proc.name,
            proc.level,
            proc.plan.len(),
            if proc.deliver { " [deliver]" } else { "" }
        );
        for imp in &proc.plan {
            println!(
                "    Impl: {} cost(latency={}, risk={}, tokens={}, money={}){}",
                imp.name,
                imp.cost.latency,
                imp.cost.risk,
                imp.cost.tokens,
                imp.cost.money,
                if !imp.refs.is_empty() {
                    format!(" refs={:?}", imp.refs)
                } else {
                    String::new()
                }
            );
        }
        for chk in &proc.checks {
            println!("    Check: \"{}\"", chk.msg);
        }
    }
    Ok(0)
}

fn parse_topic_params(input: &str) -> (String, BTreeMap<String, String>) {
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
