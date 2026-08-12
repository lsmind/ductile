//! Ductile Parser — recursive-descent parser for .pipeline files.
//!
//! Grammar mirrors the Haskell Megaparsec parser.
//! Pipeline = header .proc+ (.check | .plan | .pick | .foreach | .deliver | .when | .retry | .ensure)

use crate::ast::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub msg: String,
    pub line_text: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Parse error:\n{}:{}:\n  |\n{:>2} | {}\n  | {:>width$}^\n{}\n",
               self.line, self.col, self.line, self.line_text, "",
               self.msg, width = self.col.saturating_sub(1))
    }
}

// ── Tokenizer: input -> lines with positions ──
struct Tokenizer<'a> {
    lines: Vec<&'a str>,
    line: usize,
    col: usize,
    // We don't track per-char; we work line-by-line, char-by-char within each line.
}

struct LineCursor<'a> {
    chars: Vec<char>,
    pos: usize,
    line_num: usize,
    raw: &'a str,
}

impl<'a> LineCursor<'a> {
    fn new(line: &'a str, line_num: usize) -> Self {
        LineCursor { chars: line.chars().collect(), pos: 0, line_num, raw: line }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied();
        if c.is_some() { self.pos += 1; }
        c
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() { self.advance(); }
            else { break; }
        }
    }

    fn starts_with(&self, s: &str) -> bool {
        let s_chars: Vec<char> = s.chars().collect();
        for (i, &c) in s_chars.iter().enumerate() {
            if self.chars.get(self.pos + i) != Some(&c) {
                return false;
            }
        }
        true
    }

    fn consume(&mut self, s: &str) -> bool {
        if self.starts_with(s) {
            self.pos += s.chars().count();
            true
        } else {
            false
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn col(&self) -> usize {
        self.pos + 1
    }

    fn remaining(&self) -> String {
        self.chars[self.pos..].iter().collect()
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
        return Err(ParseError { line: 1, col: 1, msg: "empty input".into(), line_text: String::new() });
    }

    // Parse header: Pipeline("name", effects=[...], min_level=L3)
    let header_line = lines[idx];
    let (pipeline, header_consumed) = parse_header(header_line, idx + 1)?;

    idx += 1;

    let mut procs = Vec::new();
    while idx < lines.len() {
        // Skip blanks/comments
        while idx < lines.len() && is_skippable(lines[idx]) {
            idx += 1;
        }
        if idx >= lines.len() { break; }

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
        name: pipeline.name,
        effects: pipeline.effects,
        min_level: pipeline.min_level,
        procs,
        weights: Weights::default(),
    })
}

struct HeaderResult {
    name: String,
    effects: Vec<Scope>,
    min_level: Level,
}

fn parse_header(line: &str, line_num: usize) -> Result<(HeaderResult, usize), ParseError> {
    let mut cur = LineCursor::new(line, line_num);

    // Accept both Pipeline( and pipeline(
    let lower: String = line.to_lowercase();
    if lower.trim_start().starts_with("pipeline(") || lower.trim_start().starts_with("pipeline (") {
        cur.skip_ws();
        cur.consume("Pipeline");
        if !cur.consume("(") {
            cur.consume(" (");
        }
    } else {
        return Err(ParseError {
            line: line_num, col: 1, msg: format!("unexpected {:?} — expecting \"Pipeline\"", &line[..line.len().min(20)]),
            line_text: line.into(),
        });
    }

    // Parse: "name", effects=[...], min_level=L3)
    // We extract the whole content between the first ( and matching )
    // Simpler: find name in quotes, effects list, min_level
    let rest = cur.remaining();
    let full_rest = if let Some(end) = find_matching_paren(&rest) {
        &rest[..end]
    } else {
        &rest[..]
    };

    let name = extract_quoted(full_rest).unwrap_or_default();

    // Parse effects
    let mut effects = Vec::new();
    if let Some(effects_start) = full_rest.find("effects=[") {
        let after_bracket = &full_rest[effects_start + 9..]; // after "effects=["
        if let Some(close) = after_bracket.find(']') {
            let inner = &after_bracket[..close];
            for part in inner.split(',') {
                let p = part.trim();
                if let Some(s) = Scope::from_str(p) {
                    effects.push(s);
                }
            }
        }
    }

    // Parse min_level
    let min_level = if let Some(ml_pos) = full_rest.find("min_level=") {
        let after = &full_rest[ml_pos + 10..];
        let level_str: String = after.chars().take_while(|c| c.is_alphanumeric()).collect();
        Level::from_str(&level_str).unwrap_or(Level::L0)
    } else {
        Level::L0
    };

    // If no effects declared, infer from procs later; for now default
    if effects.is_empty() {
        // Some pipelines omit effects — allow all
    }

    Ok((HeaderResult { name, effects, min_level }, 1))
}

fn find_matching_paren(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.chars().enumerate() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 { return Some(i); }
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
    let line_num = idx + 1;

    // .proc("name", level=L1, scope=Search)  or  .proc("name", level=L1)
    let (name, level, scope) = parse_proc_header(line, line_num)?;

    idx += 1;

    let mut plan: Vec<Impl> = Vec::new();
    let mut checks: Vec<Check> = Vec::new();
    let mut is_deliver = false;
    let mut foreach_src: Option<String> = None;
    let mut foreach_var = String::new();
    let mut pick_by = "cost + history".to_string();

    // Parse proc body: .plan(...) .pick .check(...) .foreach(...) .deliver(...)
    while idx < lines.len() {
        let raw = lines[idx];
        let trimmed = raw.trim();

        // Skip blank lines and comments inside proc
        if trimmed.is_empty() || trimmed.starts_with("//") {
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
            // Extract by=... if present
            if let Some(by_start) = trimmed.find("by=") {
                let after = &trimmed[by_start + 3..];
                // Strip quotes and trailing )
                let by_val: String = after.chars()
                    .skip_while(|c| *c == '"' || *c == ' ')
                    .take_while(|c| *c != '"' && *c != ')' && *c != ',')
                    .collect::<String>().trim().to_string();
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

    Ok((Proc {
        name,
        level,
        scope,
        plan,
        checks,
        deliver: is_deliver,
        foreach: foreach_src,
        foreach_var,
        pick_by,
    }, idx))
}

fn parse_proc_header(line: &str, line_num: usize) -> Result<(String, Level, Option<Scope>), ParseError> {
    // .proc("name", level=L1, scope=Search)
    let name = extract_quoted(line).unwrap_or_default();

    let level = if let Some(pos) = line.find("level=") {
        let after = &line[pos + 6..];
        let level_str: String = after.chars().take_while(|c| c.is_alphanumeric()).collect();
        Level::from_str(&level_str).unwrap_or(Level::L0)
    } else {
        Level::L0
    };

    let scope = if let Some(pos) = line.find("scope=") {
        let after = &line[pos + 6..];
        let scope_str: String = after.chars()
            .skip_while(|c| *c == ' ')
            .take_while(|c| c.is_alphanumeric())
            .collect();
        Scope::from_str(&scope_str)
    } else {
        None
    };

    Ok((name, level, scope))
}

// ── Parse .plan(...) block — extract impl entries ──
fn parse_plan_block(lines: &[&str], start_idx: usize, proc_name: &str)
    -> Result<(Vec<Impl>, usize), ParseError>
{
    // Collect all text from .plan( to matching )
    let mut depth = 0i32;
    let mut buf = String::new();
    let mut idx = start_idx;
    let mut started = false;

    while idx < lines.len() {
        let line = lines[idx];
        for c in line.chars() {
            if c == '(' { depth += 1; started = true; }
            if c == ')' {
                depth -= 1;
                if started && depth == 0 {
                    // End of .plan()
                    buf.push(c);
                    // Remainder of this line after ) is outside .plan
                    break;
                }
            }
            buf.push(c);
        }
        buf.push('\n');
        idx += 1;
        if started && depth <= 0 { break; }
    }

    // Now parse impl entries from buf
    // Format: name -> body.cost(...) , name -> body.cost(...)
    // Split by commas at depth 0 (not inside parens)
    let impls = parse_impl_entries(&buf, proc_name)?;

    Ok((impls, idx))
}

fn parse_impl_entries(text: &str, proc_name: &str) -> Result<Vec<Impl>, ParseError> {
    // Split by " -> " to find impl name + body pairs
    // Then split body by commas at depth 0 to separate entries
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

    // Split into entries: each entry is "name -> body_with_cost"
    // We split by commas that are at paren-depth 0 AND follow a complete "name -> body.cost(...)" unit
    let entries = split_impl_entries(inner);

    let mut impls = Vec::new();
    for entry in &entries {
        let entry = entry.trim();
        if entry.is_empty() { continue; }

        // Find " -> " separator
        if let Some(arrow_pos) = find_arrow(entry) {
            let name = entry[..arrow_pos].trim().to_string();
            let body_with_cost = entry[arrow_pos + 4..].trim(); // skip " -> "

            // Parse .cost(...) from body
            let (body_text, cost, retry, ensure, when, enabled, stub) =
                extract_cost_and_modifiers(body_with_cost, &name);

            // Extract @refs from body
            let refs = extract_refs(&body_text);

            impls.push(Impl {
                name,
                level: Level::L0, // will be set by proc level
                cost,
                enabled,
                when,
                refs,
                body_text,
                stub,
                retry,
                ensure,
            });
        } else {
            // No arrow — single body (unnamed). Use body as both name and content.
            let (body_text, cost, retry, ensure, when, enabled, stub) =
                extract_cost_and_modifiers(entry, "unnamed");

            let refs = extract_refs(&body_text);
            impls.push(Impl {
                name: format!("path_{}", impls.len() + 1),
                level: Level::L0,
                cost,
                enabled,
                when,
                refs,
                body_text,
                stub,
                retry,
                ensure,
            });
        }
    }

    // Set impl levels from proc level — we don't have it here, set to L0
    // (typecheck will handle level validation)
    let _ = proc_name;

    Ok(impls)
}

fn find_arrow(s: &str) -> Option<usize> {
    s.find(" -> ").or_else(|| s.find("->"))
}

fn split_impl_entries(text: &str) -> Vec<String> {
    // Split by commas at paren depth 0, but only after a complete impl unit
    let mut entries = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();

    for c in text.chars() {
        match c {
            '(' => { depth += 1; current.push(c); }
            ')' => { depth -= 1; current.push(c); }
            ',' if depth == 0 => {
                entries.push(current.clone());
                current.clear();
            }
            '\n' => { current.push(' '); } // flatten newlines
            _ => { current.push(c); }
        }
    }
    if !current.trim().is_empty() {
        entries.push(current);
    }
    entries
}

fn extract_cost_and_modifiers(
    text: &str,
    _impl_name: &str,
) -> (String, Cost, usize, Vec<Check>, Option<String>, bool, bool) {
    // Returns (body_text, cost, retry, ensure, when, enabled, stub)
    let mut cost = Cost::default();
    let mut body = text.trim().to_string();
    let mut retry = 0;
    let mut ensure = Vec::new();
    let mut when = None;
    let mut enabled = true;
    let mut stub = false;

    // Extract .cost(latency=N, risk=N, tokens=N, money=N)
    if let Some(cost_pos) = body.find(".cost(") {
        let after = &body[cost_pos..];
        if let Some(close) = find_matching_paren(after) {
            let cost_inner = &after[6..close]; // skip ".cost("
            for field in cost_inner.split(',') {
                let field = field.trim();
                if let Some(eq) = field.find('=') {
                    let key = field[..eq].trim();
                    let val = field[eq + 1..].trim();
                    match key {
                        "latency" => { cost.latency = val.parse().unwrap_or(0); }
                        "risk" => { cost.risk = val.parse().unwrap_or(0.0); }
                        "tokens" => { cost.tokens = val.parse().unwrap_or(0); }
                        "money" => { cost.money = val.parse().unwrap_or(0.0); }
                        _ => {}
                    }
                }
            }
            // Remove .cost(...) from body
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
                // Use char_indices to find the true close position (avoid mid-multibyte panics)
                let char_close = after.char_indices().nth(close).map(|(b, _)| b).unwrap_or(after.len());
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

    // Disabled?
    if body.contains(".disabled") {
        enabled = false;
        body = body.replace(".disabled", "");
    }

    // Stub?
    if body.contains(".stub") {
        stub = true;
        body = body.replace(".stub", "");
    }

    (body.trim().to_string(), cost, retry, ensure, when, enabled, stub)
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
    // .check(result => has_results, "message")
    let after = &line[line.find('(').unwrap_or(6) + 1..];
    let inner = after.trim_end_matches(')').trim();

    // Split: condition part, message part (last quoted string)
    let msg = extract_last_quoted(inner).unwrap_or_default();
    let cond = if let Some(pos) = inner.rfind(',') {
        inner[..pos].trim().to_string()
    } else {
        inner.to_string()
    };

    Ok(Check { cond, msg })
}

fn parse_foreach_line(line: &str) -> Result<(String, String), ParseError> {
    // .foreach(source=@decompose, var=subtask)
    let after = &line[line.find('(').unwrap_or(9) + 1..];
    let inner = after.trim_end_matches(')').trim();

    let mut src = String::new();
    let mut var = "item".to_string();

    for part in inner.split(',') {
        let part = part.trim();
        if part.starts_with("source=") {
            let val = &part[7..];
            // Strip @ and quotes
            src = val.trim_matches(|c| c == '@' || c == '"' || c == ' ').to_string();
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
    let content = std::fs::read_to_string(path)
        .map_err(|e| ParseError {
            line: 0, col: 0, msg: format!("cannot read file: {}", e), line_text: String::new()
        })?;
    parse_pipeline(&content)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Header parsing ──
    #[test]
    fn parse_simple_header() {
        let input = r#"Pipeline("test", effects=[Search, File], min_level=L3)"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.name, "test");
        assert_eq!(pl.min_level, Level::L3);
        assert!(pl.effects.contains(&Scope::Search));
        assert!(pl.effects.contains(&Scope::File));
    }

    #[test]
    fn parse_header_no_effects() {
        let input = r#"Pipeline("bare", min_level=L1)"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.name, "bare");
        assert!(pl.effects.is_empty());
    }

    #[test]
    fn parse_header_lowercase() {
        let input = r#"pipeline("lower", effects=[Search], min_level=L0)"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.name, "lower");
    }

    // ── Empty input ──
    #[test]
    fn parse_empty_input_errors() {
        assert!(parse_pipeline("").is_err());
        assert!(parse_pipeline("\n\n// comment only").is_err());
    }

    // ── Single proc ──
    #[test]
    fn parse_single_proc() {
        let input = r#"Pipeline("t", effects=[File], min_level=L4)

.proc("fetch", level=L0, scope=File)
  .plan(
    cheap  -> read("input.txt").cost(latency=10, risk=0, tokens=0, money=0),
    pricey -> read("input.txt").cost(latency=500, risk=0, tokens=0, money=0)
  )
  .pick(by=cost + history)
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs.len(), 1);
        let p = &pl.procs[0];
        assert_eq!(p.name, "fetch");
        assert_eq!(p.level, Level::L0);
        assert_eq!(p.scope, Some(Scope::File));
        assert_eq!(p.plan.len(), 2);
        assert_eq!(p.plan[0].name, "cheap");
        assert_eq!(p.plan[0].cost.latency, 10);
        assert_eq!(p.plan[1].name, "pricey");
        assert_eq!(p.plan[1].cost.latency, 500);
        assert_eq!(p.pick_by, "cost + history");
    }

    // ── Multiple procs ──
    #[test]
    fn parse_two_procs() {
        let input = r#"Pipeline("t", effects=[Search, File], min_level=L3)

.proc("search", level=L0, scope=Search)
  .plan(
    web -> web_search(query="{topic}").cost(latency=200, risk=0.1, tokens=0, money=0)
  )

.proc("save", level=L1, scope=File)
  .plan(
    write -> write(to="out.txt", content=@search).cost(latency=5, risk=0, tokens=0, money=0)
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
        let input = r#"Pipeline("t", effects=[Deliver], min_level=L0)

.proc("output", level=L0)
  .deliver(media=[@search])
"#;
        let pl = parse_pipeline(input).unwrap();
        assert!(pl.procs[0].deliver);
    }

    // ── Foreach ──
    #[test]
    fn parse_foreach() {
        let input = r#"Pipeline("t", effects=[File], min_level=L0)

.proc("items", level=L0, scope=File)
  .plan(
    read -> read("list.txt").cost(latency=1, risk=0, tokens=0, money=0)
  )

.proc("process", level=L0)
  .foreach(source=@items, var=subtask)
  .plan(
    handle -> write(to="{subtask}.txt", content="done").cost(latency=1, risk=0, tokens=0, money=0)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs[1].foreach, Some("items".into()));
        assert_eq!(pl.procs[1].foreach_var, "subtask");
    }

    // ── Check ──
    #[test]
    fn parse_check() {
        let input = r#"Pipeline("t", effects=[Search], min_level=L0)

.proc("s", level=L0, scope=Search)
  .plan(
    web -> web_search(query="AI").cost(latency=100, risk=0.1, tokens=0, money=0)
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
        let input = r#"Pipeline("t", effects=[File], min_level=L0)

.proc("a", level=L0, scope=File)
  .plan(
    r -> read("x.txt").cost(latency=1, risk=0, tokens=0, money=0)
  )

.proc("b", level=L0)
  .plan(
    merge -> merge(@a, @a).cost(latency=1, risk=0, tokens=0, money=0)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert!(pl.procs[1].plan[0].refs.contains(&"a".to_string()));
    }

    // ── Modifiers: disabled, stub, retry, when ──
    #[test]
    fn parse_disabled_impl() {
        let input = r#"Pipeline("t", effects=[File], min_level=L0)

.proc("p", level=L0, scope=File)
  .plan(
    on  -> read("a.txt").cost(latency=1, risk=0, tokens=0, money=0),
    off -> read("b.txt").cost(latency=1, risk=0, tokens=0, money=0).disabled
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert!(pl.procs[0].plan[0].enabled);
        assert!(!pl.procs[0].plan[1].enabled);
    }

    #[test]
    fn parse_stub_impl() {
        let input = r#"Pipeline("t", effects=[File], min_level=L0)

.proc("p", level=L0, scope=File)
  .plan(
    fake -> read("x").cost(latency=0, risk=0, tokens=0, money=0).stub
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert!(pl.procs[0].plan[0].stub);
    }

    #[test]
    fn parse_retry_impl() {
        let input = r#"Pipeline("t", effects=[File], min_level=L0)

.proc("p", level=L0, scope=File)
  .plan(
    r -> read("x").cost(latency=1, risk=0, tokens=0, money=0).retry(n=3)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs[0].plan[0].retry, 3);
    }

    #[test]
    fn parse_when_impl() {
        let input = r#"Pipeline("t", effects=[File], min_level=L0)

.proc("p", level=L0, scope=File)
  .plan(
    a -> read("a").cost(latency=1, risk=0, tokens=0, money=0),
    b -> read("b").cost(latency=1, risk=0, tokens=0, money=0).when(mode == "deep")
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert!(pl.procs[0].plan[1].when.is_some());
    }

    // ── Comments and blank lines ──
    #[test]
    fn parse_with_comments() {
        let input = r#"// This is a comment
Pipeline("t", effects=[File], min_level=L0)

// Another comment
.proc("p", level=L0, scope=File)
  .plan(
    // inline comment
    r -> read("x").cost(latency=1, risk=0, tokens=0, money=0)
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs.len(), 1);
        assert_eq!(pl.procs[0].name, "p");
    }

    // ── ensure modifier ──
    #[test]
    fn parse_ensure_modifier() {
        let input = r#"Pipeline("t", effects=[Search], min_level=L0)

.proc("p", level=L0, scope=Search)
  .plan(
    s -> web_search(query="AI").cost(latency=100, risk=0.1, tokens=0, money=0)
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
        let input = r#"Pipeline("t", effects=[File], min_level=L0)

.proc("p", level=L0, scope=File)
  .plan(
    r -> read("x").cost(latency=1, risk=0, tokens=0, money=0)
  )
  .pick
"#;
        let pl = parse_pipeline(input).unwrap();
        assert_eq!(pl.procs[0].pick_by, "cost + history");
    }
}
