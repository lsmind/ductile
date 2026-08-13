//! Ductile Parser — recursive-descent parser for .pipeline files.
//!
//! v0.4: No level/scope in header or proc. Tags (#tag) only on leaf impls.
//! Pipeline header: Pipeline("name") — no effects, no min_level.

use crate::ast::*;
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub msg: String,
    pub line_text: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Parse error:\n{}:{}:\n  |  \n{:>2} | {}\n  | {:>width$}^\n{}\n",
            self.line,
            self.col,
            self.line,
            self.line_text,
            "",
            self.msg,
            width = self.col.saturating_sub(1)
        )
    }
}

// Check if line is blank or comment
fn is_skippable(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty() || trimmed.starts_with("//")
}

// ── Top-level parse ──
pub fn parse_pipeline(input: &str) -> Result<Pipeline, ParseError> {
    let lines: Vec<&str> = input.lines().collect();
    let mut idx = 0;

    // Skip blanks/comments
    while idx < lines.len() && is_skippable(lines[idx]) {
        idx += 1;
    }
    if idx >= lines.len() {
        return Err(ParseError {
            line: 1,
            col: 1,
            msg: "empty input".into(),
            line_text: String::new(),
        });
    }

    // Parse header: Pipeline("name") or Pipeline("name", "desc")
    let header_line = lines[idx];
    let (name, description) = parse_header(header_line, idx + 1)?;

    idx += 1;

    let mut procs = Vec::new();
    while idx < lines.len() {
        // Skip blanks/comments
        while idx < lines.len() && is_skippable(lines[idx]) {
            idx += 1;
        }
        if idx >= lines.len() {
            break;
        }

        let line = lines[idx];
        let trimmed = line.trim();

        // .proc line
        if trimmed.starts_with(".proc(") || trimmed.starts_with(".proc (") {
            let (proc, consumed) = parse_proc(&lines, idx)?;
            idx = consumed;
            procs.push(proc);
            continue;
        }

        // Unknown line — skip
        idx += 1;
    }

    Ok(Pipeline {
        name,
        description,
        procs,
        weights: Weights::default(),
    })
}

fn parse_header(line: &str, line_num: usize) -> Result<(String, String), ParseError> {
    let lower: String = line.to_lowercase();
    if lower.trim_start().starts_with("pipeline(") || lower.trim_start().starts_with("pipeline (") {
        // OK
    } else {
        return Err(ParseError {
            line: line_num,
            col: 1,
            msg: format!(
                "unexpected {:?} — expecting \"Pipeline\"",
                &line[..line.len().min(20)]
            ),
            line_text: line.into(),
        });
    }

    // Extract quoted strings: first is name, optional second is description
    let quoted = extract_all_quoted(line);
    let name = quoted.first().cloned().unwrap_or_default();
    let description = quoted.get(1).cloned().unwrap_or_default();
    Ok((name, description))
}

fn extract_all_quoted(s: &str) -> Vec<String> {
    let mut result = Vec::new();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '\"' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end] != '\"' {
                end += 1;
            }
            if end <= bytes.len() {
                result.push(bytes[start..end].iter().collect());
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    result
}

fn find_matching_paren(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.chars().enumerate() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_quoted(s: &str) -> Option<String> {
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '"' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end] != '"' {
                end += 1;
            }
            if end <= bytes.len() {
                return Some(bytes[start..end].iter().collect());
            }
        }
        i += 1;
    }
    None
}

// ── Parse a single .proc block ──
fn parse_proc(lines: &[&str], start_idx: usize) -> Result<(Proc, usize), ParseError> {
    let mut idx = start_idx;
    let line = lines[idx];
    let _line_num = idx + 1;

    // .proc("name") — just the name, no level/scope
    let name = extract_quoted(line).unwrap_or_default();

    idx += 1;

    let mut plan: Vec<Impl> = Vec::new();
    let mut checks: Vec<Check> = Vec::new();
    let mut is_deliver = false;
    let mut foreach_src: Option<String> = None;
    let mut foreach_var = String::new();
    let mut pick_by = "cost + history".to_string();
    let mut description = String::new();

    // Parse proc body: .plan(...) .pick .check(...) .foreach(...) .deliver(...) .desc(...)
    while idx < lines.len() {
        let raw = lines[idx];
        let trimmed = raw.trim();

        // Skip blank lines and comments inside proc
        if trimmed.is_empty() || trimmed.starts_with("//") {
            idx += 1;
            continue;
        }

        // .desc("text") — proc description
        if trimmed.starts_with(".desc(") || trimmed.starts_with(".desc (") {
            description = extract_quoted(trimmed).unwrap_or_default();
            idx += 1;
            continue;
        }

        // .plan( — multi-line impl block
        if trimmed.starts_with(".plan(") || trimmed.starts_with(".plan (") {
            let (impls, next_idx) = parse_plan_block(lines, idx, &name)?;
            plan.extend(impls);
            idx = next_idx;
            continue;
        }

        // .pick or .pick(by=...)
        if trimmed.starts_with(".pick") {
            if let Some(by_start) = trimmed.find("by=") {
                let after = &trimmed[by_start + 3..];
                let by_val: String = after
                    .chars()
                    .skip_while(|c| *c == '"' || *c == ' ')
                    .take_while(|c| *c != '"' && *c != ')' && *c != ',')
                    .collect::<String>()
                    .trim()
                    .to_string();
                if !by_val.is_empty() {
                    pick_by = by_val;
                }
            }
            idx += 1;
            continue;
        }

        // .check(result => predicate, "message")
        if trimmed.starts_with(".check(") || trimmed.starts_with(".check (") {
            let check = parse_check_line(trimmed)?;
            checks.push(check);
            idx += 1;
            continue;
        }

        // .foreach(source=@proc, var=item)
        if trimmed.starts_with(".foreach(") || trimmed.starts_with(".foreach (") {
            let (src, var) = parse_foreach_line(trimmed)?;
            foreach_src = Some(src);
            foreach_var = var;
            idx += 1;
            continue;
        }

        // .deliver(media=[@proc])
        if trimmed.starts_with(".deliver(") || trimmed.starts_with(".deliver (") {
            is_deliver = true;
            idx += 1;
            continue;
        }

        // Next .proc or end — stop
        if trimmed.starts_with(".proc(") || trimmed.starts_with(".proc (") {
            break;
        }

        // Unknown line — skip
        idx += 1;
    }

    Ok((
        Proc {
            name,
            description: description.clone(),
            plan,
            checks,
            deliver: is_deliver,
            foreach: foreach_src,
            foreach_var,
            pick_by,
        },
        idx,
    ))
}

// ── Parse .plan(...) block — extract impl entries ──
fn parse_plan_block(
    lines: &[&str],
    start_idx: usize,
    proc_name: &str,
) -> Result<(Vec<Impl>, usize), ParseError> {
    let mut depth = 0i32;
    let mut buf = String::new();
    let mut idx = start_idx;
    let mut started = false;

    while idx < lines.len() {
        let line = lines[idx];
        for c in line.chars() {
            if c == '(' {
                depth += 1;
                started = true;
            }
            if c == ')' {
                depth -= 1;
                if started && depth == 0 {
                    buf.push(c);
                    break;
                }
            }
            buf.push(c);
        }
        buf.push('\n');
        idx += 1;
        if started && depth <= 0 {
            break;
        }
    }

    let impls = parse_impl_entries(&buf, proc_name)?;
    Ok((impls, idx))
}

fn parse_impl_entries(text: &str, proc_name: &str) -> Result<Vec<Impl>, ParseError> {
    let text = text.trim();

    // Remove leading ".plan(" prefix
    let after_plan = if let Some(pos) = text.find('(') {
        &text[pos + 1..]
    } else {
        text
    };

    // Remove trailing ")"
    let inner = after_plan.trim_end();
    let inner = inner.strip_suffix(')').unwrap_or(inner).trim();

    let entries = split_impl_entries(inner);

    let mut impls = Vec::new();
    for entry in &entries {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        if let Some(arrow_pos) = find_arrow(entry) {
            let name = entry[..arrow_pos].trim().to_string();
            let body_with_cost = entry[arrow_pos + 4..].trim();

            let (body_text, cost, retry, ensure, when, enabled, stub, tags) =
                extract_cost_and_modifiers(body_with_cost, &name);

            let refs = extract_refs(&body_text);

            impls.push(Impl {
                name,
                tags,
                cost,
                enabled,
                when,
                refs,
                body_text,
                stub,
                retry,
                ensure,
                description: String::new(),
            });
        } else {
            let (body_text, cost, retry, ensure, when, enabled, stub, tags) =
                extract_cost_and_modifiers(entry, "unnamed");

            let refs = extract_refs(&body_text);
            impls.push(Impl {
                name: format!("path_{}", impls.len() + 1),
                tags,
                cost,
                enabled,
                when,
                refs,
                body_text,
                stub,
                retry,
                ensure,
                description: String::new(),
            });
        }
    }

    let _ = proc_name;
    Ok(impls)
}

fn find_arrow(s: &str) -> Option<usize> {
    s.find(" -> ").or_else(|| s.find("->"))
}

fn split_impl_entries(text: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();

    for c in text.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                entries.push(current.clone());
                current.clear();
            }
            '\n' => {
                current.push(' ');
            }
            _ => {
                current.push(c);
            }
        }
    }
    if !current.trim().is_empty() {
        entries.push(current);
    }
    entries
}

/// Extract .cost(), .retry(), .ensure(), .when(), .disabled, .stub, and .tags(#a, #b)
/// Returns (body_text, cost, retry, ensure, when, enabled, stub, tags)
fn extract_cost_and_modifiers(
    text: &str,
    _impl_name: &str,
) -> (
    String,
    Cost,
    usize,
    Vec<Check>,
    Option<String>,
    bool,
    bool,
    BTreeSet<String>,
) {
    let mut cost = Cost::default();
    let mut body = text.trim().to_string();
    let mut retry = 0;
    let mut ensure = Vec::new();
    let mut when = None;
    let mut enabled = true;
    let mut stub = false;
    let mut tags = BTreeSet::new();

    // Extract .tags(#a, #b, ...) or .tags(a, b)
    if let Some(tags_pos) = body.find(".tags(") {
        let after = &body[tags_pos..];
        if let Some(close) = find_matching_paren(after) {
            let tags_inner = &after[6..close]; // skip ".tags("
            for part in tags_inner.split(',') {
                let tag = part.trim().trim_start_matches('#').trim().to_string();
                if !tag.is_empty() {
                    tags.insert(tag);
                }
            }
            body = format!("{}{}", &body[..tags_pos], &after[close + 1..]);
        }
    }

    // Extract .cost(latency=N, risk=N, tokens=N, money=N)
    if let Some(cost_pos) = body.find(".cost(") {
        let after = &body[cost_pos..];
        if let Some(close) = find_matching_paren(after) {
            let cost_inner = &after[6..close];
            for field in cost_inner.split(',') {
                let field = field.trim();
                if let Some(eq) = field.find('=') {
                    let key = field[..eq].trim();
                    let val = field[eq + 1..].trim();
                    match key {
                        "latency" => {
                            cost.latency = val.parse().unwrap_or(0);
                        }
                        "risk" => {
                            cost.risk = val.parse().unwrap_or(0.0);
                        }
                        "tokens" => {
                            cost.tokens = val.parse().unwrap_or(0);
                        }
                        "money" => {
                            cost.money = val.parse().unwrap_or(0.0);
                        }
                        _ => {}
                    }
                }
            }
            body = format!("{}{}", &body[..cost_pos], &after[close + 1..]);
        }
    }

    // Extract .retry(n=N)
    if let Some(r_pos) = body.find(".retry(") {
        let after = &body[r_pos..];
        if let Some(close) = find_matching_paren(after) {
            let inner = &after[7..close];
            if let Some(eq) = inner.find('=') {
                let val = &inner[eq + 1..].trim();
                retry = val.parse().unwrap_or(0);
            }
            body = format!("{}{}", &body[..r_pos], &after[close + 1..]);
        }
    }

    // Extract .ensure(cond, "msg")
    loop {
        if let Some(e_pos) = body.find(".ensure(") {
            let prefix = ".ensure(";
            let after = &body[e_pos..];
            if let Some(close) = find_matching_paren(after) {
                let char_close = after
                    .char_indices()
                    .nth(close)
                    .map(|(b, _)| b)
                    .unwrap_or(after.len());
                let inner_start = prefix.len();
                if char_close >= inner_start {
                    let inner = &after[inner_start..char_close];
                    if let Some(comma) = inner.rfind(',') {
                        let cond = inner[..comma].trim().to_string();
                        let mut msg = inner[comma + 1..].trim().to_string();
                        msg = msg.trim_matches('"').to_string();
                        ensure.push(Check { cond, msg });
                    }
                    body = format!("{}{}", &body[..e_pos], &after[char_close + 1..]);
                    continue;
                }
            }
        }
        break;
    }

    // Extract .when(cond)
    if let Some(w_pos) = body.find(".when(") {
        let after = &body[w_pos..];
        if let Some(close) = find_matching_paren(after) {
            let inner = after[6..close].trim().to_string();
            when = Some(inner);
            body = format!("{}{}", &body[..w_pos], &after[close + 1..]);
        }
    }

    if body.contains(".disabled") {
        enabled = false;
        body = body.replace(".disabled", "");
    }

    if body.contains(".stub") {
        stub = true;
        body = body.replace(".stub", "");
    }

    (
        body.trim().to_string(),
        cost,
        retry,
        ensure,
        when,
        enabled,
        stub,
        tags,
    )
}

fn extract_refs(body: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() {
                let c = chars[end];
                if c.is_alphanumeric() || c == '_' || c == '-' {
                    end += 1;
                } else {
                    break;
                }
            }
            if end > start {
                let name: String = chars[start..end].iter().collect();
                if !refs.contains(&name) {
                    refs.push(name);
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    refs
}

fn parse_check_line(line: &str) -> Result<Check, ParseError> {
    let after = &line[line.find('(').unwrap_or(6) + 1..];
    let inner = after.trim_end_matches(')').trim();

    let msg = extract_last_quoted(inner).unwrap_or_default();
    let cond = if let Some(pos) = inner.rfind(',') {
        inner[..pos].trim().to_string()
    } else {
        inner.to_string()
    };

    Ok(Check { cond, msg })
}

fn parse_foreach_line(line: &str) -> Result<(String, String), ParseError> {
    let after = &line[line.find('(').unwrap_or(9) + 1..];
    let inner = after.trim_end_matches(')').trim();

    let mut src = String::new();
    let mut var = "item".to_string();

    for part in inner.split(',') {
        let part = part.trim();
        if part.starts_with("source=") {
            let val = &part[7..];
            src = val
                .trim_matches(|c| c == '@' || c == '"' || c == ' ')
                .to_string();
        } else if part.starts_with("var=") {
            let val = &part[4..];
            var = val.trim_matches(|c| c == '"' || c == ' ').to_string();
        }
    }

    Ok((src, var))
}

fn extract_last_quoted(s: &str) -> Option<String> {
    let mut result = None;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && chars[end] != '"' {
                end += 1;
            }
            if end <= chars.len() {
                result = Some(chars[start..end].iter().collect());
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    result
}

pub fn parse_pipeline_file(path: &str) -> Result<Pipeline, ParseError> {
    let content = std::fs::read_to_string(path).map_err(|e| ParseError {
        line: 0,
        col: 0,
        msg: format!("cannot read file: {}", e),
        line_text: String::new(),
    })?;
    parse_pipeline(&content)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Header parsing ──
    #[test]
    fn parse_simple_header() {
        let input = r#"Pipeline("test")"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.name, "test");
    }

    #[test]
    fn parse_header_lowercase() {
        let input = r#"pipeline("lower")"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.name, "lower");
    }

    // ── Empty input ──
    #[test]
    fn parse_empty_input_errors() {
        assert!(parse_pipeline("").is_err());
        assert!(parse_pipeline("\n\n// comment only").is_err());
    }

    // ── Single proc with tags ──
    #[test]
    fn parse_single_proc_with_tags() {
        let input = r#"Pipeline("t")

.proc("fetch")
  .plan(
    cheap  -> read("input.txt").tags(#file).cost(latency=10, risk=0, tokens=0, money=0),
    pricey -> read("input.txt").tags(#file).cost(latency=500, risk=0, tokens=0, money=0)
  )
  .pick(by=cost + history)
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs.len(), 1);
        let p = &pl.procs[0];
        assert_eq!(p.name, "fetch");
        assert_eq!(p.plan.len(), 2);
        assert_eq!(p.plan[0].name, "cheap");
        assert_eq!(p.plan[0].cost.latency, 10);
        assert!(p.plan[0].tags.contains("file"));
        assert_eq!(p.plan[1].name, "pricey");
        assert_eq!(p.plan[1].cost.latency, 500);
        assert!(p.plan[1].tags.contains("file"));
        assert_eq!(p.pick_by, "cost + history");
    }

    // ── Multiple tags on one impl ──
    #[test]
    fn parse_multiple_tags() {
        let input = r#"Pipeline("t")

.proc("search")
  .plan(
    web -> web_search(query="AI").tags(#search, #network).cost(latency=200, risk=0.1, tokens=0, money=0)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        let tags = &pl.procs[0].plan[0].tags;
        assert_eq!(tags.len(), 2);
        assert!(tags.contains("search"));
        assert!(tags.contains("network"));
    }

    // ── No tags is OK ──
    #[test]
    fn parse_no_tags() {
        let input = r#"Pipeline("t")

.proc("p")
  .plan(
    r -> read("x").cost(latency=1, risk=0, tokens=0, money=0)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert!(pl.procs[0].plan[0].tags.is_empty());
    }

    // ── Tags without # prefix ──
    #[test]
    fn parse_tags_without_hash() {
        let input = r#"Pipeline("t")

.proc("p")
  .plan(
    r -> read("x").tags(file, network).cost(latency=1, risk=0, tokens=0, money=0)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert!(pl.procs[0].plan[0].tags.contains("file"));
        assert!(pl.procs[0].plan[0].tags.contains("network"));
    }

    // ── Multiple procs ──
    #[test]
    fn parse_two_procs() {
        let input = r#"Pipeline("t")

.proc("search")
  .plan(
    web -> web_search(query="{topic}").tags(#search).cost(latency=200, risk=0.1, tokens=0, money=0)
  )

.proc("save")
  .plan(
    write -> write(to="out.txt", content=@search).tags(#file).cost(latency=5, risk=0, tokens=0, money=0)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs.len(), 2);
        assert_eq!(pl.procs[0].name, "search");
        assert_eq!(pl.procs[1].name, "save");
    }

    // ── Deliver ──
    #[test]
    fn parse_deliver() {
        let input = r#"Pipeline("t")

.proc("output")
  .deliver(media=[@search])
"#;
        let pl = parse_pipeline(input).unwrap();
        assert!(pl.procs[0].deliver);
    }

    // ── Foreach ──
    #[test]
    fn parse_foreach() {
        let input = r#"Pipeline("t")

.proc("items")
  .plan(
    read -> read("list.txt").tags(#file).cost(latency=1, risk=0, tokens=0, money=0)
  )

.proc("process")
  .foreach(source=@items, var=subtask)
  .plan(
    handle -> write(to="{subtask}.txt", content="done").tags(#file).cost(latency=1, risk=0, tokens=0, money=0)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs[1].foreach, Some("items".into()));
        assert_eq!(pl.procs[1].foreach_var, "subtask");
    }

    // ── Check ──
    #[test]
    fn parse_check() {
        let input = r#"Pipeline("t")

.proc("s")
  .plan(
    web -> web_search(query="AI").tags(#search).cost(latency=100, risk=0.1, tokens=0, money=0)
  )
  .check(result => has_results, "no search results")
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs[0].checks.len(), 1);
        assert_eq!(pl.procs[0].checks[0].msg, "no search results");
    }

    // ── Refs extraction ──
    #[test]
    fn parse_refs_from_body() {
        let input = r#"Pipeline("t")

.proc("a")
  .plan(
    r -> read("x.txt").tags(#file).cost(latency=1, risk=0, tokens=0, money=0)
  )

.proc("b")
  .plan(
    merge -> merge(@a, @a).tags(#file).cost(latency=1, risk=0, tokens=0, money=0)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert!(pl.procs[1].plan[0].refs.contains(&"a".to_string()));
    }

    // ── Modifiers: disabled, stub, retry, when ──
    #[test]
    fn parse_disabled_impl() {
        let input = r#"Pipeline("t")

.proc("p")
  .plan(
    on  -> read("a.txt").tags(#file).cost(latency=1, risk=0, tokens=0, money=0),
    off -> read("b.txt").tags(#file).cost(latency=1, risk=0, tokens=0, money=0).disabled
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert!(pl.procs[0].plan[0].enabled);
        assert!(!pl.procs[0].plan[1].enabled);
    }

    #[test]
    fn parse_stub_impl() {
        let input = r#"Pipeline("t")

.proc("p")
  .plan(
    fake -> read("x").tags(#file).cost(latency=0, risk=0, tokens=0, money=0).stub
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert!(pl.procs[0].plan[0].stub);
    }

    #[test]
    fn parse_retry_impl() {
        let input = r#"Pipeline("t")

.proc("p")
  .plan(
    r -> read("x").tags(#file).cost(latency=1, risk=0, tokens=0, money=0).retry(n=3)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs[0].plan[0].retry, 3);
    }

    #[test]
    fn parse_when_impl() {
        let input = r#"Pipeline("t")

.proc("p")
  .plan(
    a -> read("a").tags(#file).cost(latency=1, risk=0, tokens=0, money=0),
    b -> read("b").tags(#file).cost(latency=1, risk=0, tokens=0, money=0).when(mode == "deep")
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert!(pl.procs[0].plan[1].when.is_some());
    }

    // ── Comments and blank lines ──
    #[test]
    fn parse_with_comments() {
        let input = r#"// This is a comment
Pipeline("t")

// Another comment
.proc("p")
  .plan(
    // inline comment
    r -> read("x").tags(#file).cost(latency=1, risk=0, tokens=0, money=0)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs.len(), 1);
        assert_eq!(pl.procs[0].name, "p");
    }

    // ── ensure modifier ──
    #[test]
    fn parse_ensure_modifier() {
        let input = r#"Pipeline("t")

.proc("p")
  .plan(
    s -> web_search(query="AI").tags(#search).cost(latency=100, risk=0.1, tokens=0, money=0)
      .ensure(result => not_empty, "empty result")
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs[0].plan[0].ensure.len(), 1);
        assert_eq!(pl.procs[0].plan[0].ensure[0].msg, "empty result");
    }

    // ── pick default ──
    #[test]
    fn parse_pick_default() {
        let input = r#"Pipeline("t")

.proc("p")
  .plan(
    r -> read("x").tags(#file).cost(latency=1, risk=0, tokens=0, money=0)
  )
  .pick
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs[0].pick_by, "cost + history");
    }

    // ── computed_tags from parsed pipeline ──
    #[test]
    fn computed_tags_from_parsed() {
        let input = r#"Pipeline("t")

.proc("search")
  .plan(
    web -> web_search(query="AI").tags(#search, #network).cost(latency=200, risk=0.1, tokens=0, money=0)
  )

.proc("save")
  .plan(
    out -> write(to="out.txt", content=@search).tags(#file).cost(latency=5, risk=0, tokens=0, money=0)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        let tags = pl.computed_tags();
        assert_eq!(tags.len(), 3);
        assert!(tags.contains("search"));
        assert!(tags.contains("network"));
        assert!(tags.contains("file"));
    }
}
