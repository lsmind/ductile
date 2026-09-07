//! Ductile Parser — recursive-descent parser for .pipeline files.
//!
//! v0.4: No level/scope in header or proc. Tags (#tag) only on leaf impls.
//! Pipeline header: Pipeline("name") — no effects, no min_level.

use crate::ast::*;
use std::collections::BTreeMap;
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
pub(crate) fn is_skippable(line: &str) -> bool {
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
                crate::trunc_chars(line, 20)
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

pub(crate) fn extract_all_quoted(s: &str) -> Vec<String> {
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

pub(crate) fn find_matching_paren(s: &str) -> Option<usize> {
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

pub(crate) fn extract_quoted(s: &str) -> Option<String> {
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
    let mut deliver_refs: Vec<String> = Vec::new();
    let mut foreach_src: Option<String> = None;
    let mut foreach_var = String::new();
    let mut pick_by = "cost + history".to_string();
    let mut description = String::new();
    let mut proc_when: Option<String> = None;

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

        // .when(cond) — proc 级块状裁判路由（v0.11.1）：作用于该 proc 全部 impls。
        // 此前独立行 .when 落进 Unknown-line 分支被静默丢弃 → @gen.score < 80 路由失效。
        if trimmed.starts_with(".when(") {
            let after = &trimmed[5..];
            let close = find_matching_paren(after).ok_or_else(|| ParseError {
                line: idx + 1,
                col: 1,
                msg: "unbalanced parens in block .when(...)".into(),
                line_text: raw.to_string(),
            })?;
            let inner = after[1..close].trim().to_string();
            if inner.is_empty() {
                return Err(ParseError {
                    line: idx + 1,
                    col: 1,
                    msg: "block .when() requires a condition".into(),
                    line_text: raw.to_string(),
                });
            }
            proc_when = Some(inner);
            idx += 1;
            continue;
        }

        // .pick / .pick(by=...) / .pick(egraph) / .pick(static)
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
            } else if trimmed.starts_with(".pick(") && trimmed.ends_with(')') {
                // 裸策略词：.pick(egraph) / .pick(static)
                let inner = trimmed[".pick(".len()..trimmed.len() - 1].trim();
                if !inner.is_empty()
                    && inner
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_' || c == ' ')
                {
                    pick_by = inner.split_whitespace().next().unwrap().to_string();
                }
            }
            idx += 1;
            continue;
        }

        // .check(result => predicate, "message")
        if trimmed.starts_with(".check(") || trimmed.starts_with(".check (") {
            // v0.11 谓词层退役：裁判与生产分离。质量门槛改用独立 judge proc + .when 路由。
            eprintln!(
                "[v0.11] warning: .check() retired — predicate layer removed, quality gates now live in judge procs (.when routing). Line ignored: {}",
                crate::trunc_chars(trimmed, 60)
            );
            let _ = parse_check_line(trimmed)?; // 仍解析以保留语法错误检查
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

        // .deliver(media=[@proc]) — v0.14b：解析引用（关键节点集的根，此前被丢弃）
        if trimmed.starts_with(".deliver(") || trimmed.starts_with(".deliver (") {
            is_deliver = true;
            let inner = trimmed
                .trim_start_matches(".deliver(")
                .trim_start_matches(".deliver (")
                .trim_end_matches(')')
                .trim();
            for r in extract_refs(inner) {
                if !deliver_refs.contains(&r) {
                    deliver_refs.push(r);
                }
            }
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

    // v0.11.1：块级 .when 下推到未持有内联 when 的全部 impls，裁判 @ref 并入 refs
    //（egraph 据此建 gen→deliver 边，保证裁判先于路由消费者执行）。
    if let Some(cond) = &proc_when {
        for imp in plan.iter_mut() {
            if imp.when.is_none() {
                imp.when = Some(cond.clone());
            }
            for r in extract_refs(cond) {
                if !imp.refs.contains(&r) {
                    imp.refs.push(r);
                }
            }
        }
    }

    Ok((
        Proc {
            name,
            description: description.clone(),
            plan,
            checks,
            deliver: is_deliver,
            deliver_refs,
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

            // v0.11.1 重构：.when() 里的 @ref 也算依赖——裁判路由 `.when(@gate.score < 80)`
            // 需要 gate → 本 proc 的 DAG 边，否则 deliver 会先于 gate 执行，判决必然落空。
            let mut refs = extract_refs(&body_text);
            if let Some(w) = &when {
                for r in extract_refs(w) {
                    if !refs.contains(&r) {
                        refs.push(r);
                    }
                }
            }

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
            // v0.11: .plan() 内每个条目必须是 `name -> body`。
            // 旧版把无名条目静默命名为 path_N，产生 <noop> 幽灵 impl 污染数据流
            // （实测：weights(rd=0.5) 被吃成 path_1 并假成功）。现在硬错误。
            return Err(ParseError {
                line: 0,
                col: 0,
                msg: format!(
                    "plan entry without `name -> body` form: {:?} — every entry needs an impl name",
                    crate::trunc_chars(entry, 60)
                ),
                line_text: entry.to_string(),
            });
        }
    }

    let _ = proc_name;
    Ok(impls)
}

pub(crate) fn find_arrow(s: &str) -> Option<usize> {
    s.find(" -> ").or_else(|| s.find("->"))
}

pub(crate) fn split_impl_entries(text: &str) -> Vec<String> {
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

    // v0.11: .cost() 退役——评价移入 .eval 策略文件（--policy 挂载）。
    // 兼容处理：打警告并从 body 剥离，Cost 用 default（真实 cost 来源见 executor::apply_policy）。
    if let Some(cost_pos) = body.find(".cost(") {
        eprintln!(
            "[v0.11] warning: .cost() retired — costs move to .eval policy file (--policy). Declaration ignored."
        );
        let after = &body[cost_pos..];
        if let Some(close) = find_matching_paren(after) {
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
            // v0.11 谓词层退役：.ensure 同 .check 一并退役。
            eprintln!(
                "[v0.11] warning: .ensure() retired — predicate layer removed. Modifier ignored."
            );
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
                    body = format!("{}{}", &body[..e_pos], &after[char_close + 1..]);
                    continue;
                }
                break;
            }
            break;
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

pub(crate) fn extract_refs(body: &str) -> Vec<String> {
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

pub(crate) fn extract_last_quoted(s: &str) -> Option<String> {
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

// ── v0.11 Policy (.eval) 解析 ──
//
// 评价与流程分离：.pipeline 只描述流程；权重 / cost 来源写在这里，运行时 --policy 挂载。
// 格式（行式，`#` 或 `//` 注释，空行忽略）：
//   weights latency=0.001 risk=10.0 money=1.0 tokens=0.0001
//   fail_closed = true
//   <proc>.<impl> cost latency=measure("/abs/bench.sh {topic}") risk=0.05
// 值语法：纯数字 → Direct(f64)；measure("命令") → Measure(cmd)。

pub(crate) fn parse_policy_value(raw: &str) -> Result<CostValue, String> {
    let raw = raw.trim();
    if let Some(open) = raw.find("measure(") {
        let rest = &raw[open + "measure(".len()..];
        let close = rest
            .rfind(')')
            .ok_or_else(|| format!("measure( missing ')': {}", raw))?;
        let cmd = extract_quoted(rest[..close].trim())
            .ok_or_else(|| format!("measure(\"...\") expects a quoted command: {}", raw))?;
        if cmd.trim().is_empty() {
            return Err(format!("measure command is empty: {}", raw));
        }
        return Ok(CostValue::Measure(cmd));
    }
    raw.parse::<f64>().map(CostValue::Direct).map_err(|_| {
        format!(
            "bad cost value {:?} — expect number or measure(\"cmd\")",
            raw
        )
    })
}

pub(crate) fn apply_policy_field(
    spec: &mut CostSpec,
    field: &str,
    val_raw: &str,
) -> Result<(), String> {
    let v = parse_policy_value(val_raw)?;
    match field {
        "latency" => spec.latency = Some(v),
        "risk" => spec.risk = Some(v),
        "tokens" => spec.tokens = Some(v),
        "money" => spec.money = Some(v),
        other => return Err(format!("unknown cost field {:?}", other)),
    }
    Ok(())
}

pub fn parse_policy(input: &str) -> Result<Policy, String> {
    let mut policy = Policy {
        weights: Weights::default(),
        costs: BTreeMap::new(),
        fail_closed: true,
    };
    for (i, line) in input.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        let lineno = i + 1;

        // weights <field>=<f64> ...
        if let Some(rest) = trimmed.strip_prefix("weights") {
            let rest = rest.trim();
            if !rest.is_empty() && !rest.starts_with('=') && rest.contains('=') {
                for part in rest.split_whitespace() {
                    let (k, v) = part.split_once('=').ok_or_else(|| {
                        format!(
                            ".eval:{}: weights expects key=value, got {:?}",
                            lineno, part
                        )
                    })?;
                    let f = v.parse::<f64>().map_err(|_| {
                        format!(
                            ".eval:{}: weights.{} must be a number, got {:?}",
                            lineno, k, v
                        )
                    })?;
                    match k {
                        "latency" => policy.weights.latency = f,
                        "risk" => policy.weights.risk = f,
                        "tokens" => policy.weights.tokens = f,
                        "money" => policy.weights.money = f,
                        other => {
                            return Err(format!(
                                ".eval:{}: unknown weight {:?} (v0.11: rd 已移除)",
                                lineno, other
                            ))
                        }
                    }
                }
                continue;
            }
            return Err(format!(
                ".eval:{}: malformed weights line: {:?}",
                lineno, trimmed
            ));
        }

        // fail_closed = true|false
        if let Some(rest) = trimmed.strip_prefix("fail_closed") {
            let v = rest.trim().trim_start_matches('=').trim();
            policy.fail_closed = match v {
                "true" => true,
                "false" => false,
                other => {
                    return Err(format!(
                        ".eval:{}: fail_closed expects true/false, got {:?}",
                        lineno, other
                    ))
                }
            };
            continue;
        }

        // <proc>.<impl> cost <field>=<value> ...
        let mut parts = trimmed.split_whitespace();
        let key = parts.next().unwrap_or_default();
        if key.contains('.') && !key.contains('=') {
            let verb = parts.next().unwrap_or_default();
            if verb != "cost" {
                return Err(format!(
                    ".eval:{}: expecting {{proc}}.{{impl}} cost ..., got {:?}",
                    lineno, trimmed
                ));
            }
            let mut spec = CostSpec::default();
            // 引号感知分词：measure("cmd with spaces {topic}") 不可按空白截断。
            let args_raw: String = parts.collect::<Vec<_>>().join(" ");
            for (field, val_raw) in
                parse_cost_args(&args_raw).map_err(|e| format!(".eval:{}: {}", lineno, e))?
            {
                apply_policy_field(&mut spec, &field, &val_raw)
                    .map_err(|e| format!(".eval:{}: {}", lineno, e))?;
            }
            policy.costs.insert(key.to_string(), spec);
            continue;
        }

        return Err(format!(
            ".eval:{}: unrecognized line {:?} — see SPEC v0.11 policy format",
            lineno, trimmed
        ));
    }
    Ok(policy)
}

/// cost 行参数的引号感知分词：`latency=measure("a b") risk=0.05` →
/// [("latency", `measure("a b")`), ("risk", "0.05")]。
pub(crate) fn parse_cost_args(args: &str) -> Result<Vec<(String, String)>, String> {
    let chars: Vec<char> = args.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        // 字段名
        let start = i;
        while i < chars.len() && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        let field: String = chars[start..i].iter().collect();
        if i >= chars.len() || chars[i] != '=' {
            return Err(format!("expects field=value, got {:?}", field));
        }
        i += 1; // 跳过 '='
                // 值：双引号内的空白不算分隔符
        let val_start = i;
        let mut in_quote = false;
        while i < chars.len() {
            let c = chars[i];
            if c == '"' {
                in_quote = !in_quote;
            } else if !in_quote && c.is_whitespace() {
                break;
            }
            i += 1;
        }
        if in_quote {
            return Err(format!("unterminated quote after field {:?}", field));
        }
        let val: String = chars[val_start..i].iter().collect();
        out.push((field, val));
    }
    Ok(out)
}

pub fn parse_policy_file(path: &str) -> Result<Policy, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {}", path, e))?;
    parse_policy(&content)
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
        // v0.11: .cost() 退役——声明被剥离，cost 用 default；评价在 .eval 策略文件。
        assert_eq!(p.plan[0].cost.latency, 0);
        assert!(p.plan[0].tags.contains("file"));
        assert_eq!(p.plan[1].name, "pricey");
        assert_eq!(p.plan[1].cost.latency, 0);
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

    // ── v0.11.1 块级 .when（proc 级裁判路由）──

    #[test]
    fn block_when_pushes_to_all_impls() {
        let input = r#"Pipeline("t")

.proc("gen")
  .plan(high -> run("echo 'score=85'"))

.proc("deliver")
  .plan(x -> run("echo DELIVERED"))
  .when(@gen.score < 80)
"#;
        let pl = parse_pipeline(input).unwrap();
        let deliver = pl.procs.iter().find(|p| p.name == "deliver").unwrap();
        assert_eq!(deliver.plan.len(), 1);
        let imp = &deliver.plan[0];
        assert_eq!(imp.when.as_deref(), Some("@gen.score < 80"));
        // 裁判 @ref 入 refs → egraph 建边的前提
        assert!(imp.refs.contains(&"gen".to_string()));
    }

    #[test]
    fn block_when_empty_condition_hard_error() {
        let input = r#"Pipeline("t")

.proc("deliver")
  .plan(x -> run("echo hi"))
  .when()
"#;
        let err = parse_pipeline(input).unwrap_err();
        assert!(err.msg.contains("requires a condition"));
    }

    #[test]
    fn block_when_unbalanced_parens_hard_error() {
        let input = r#"Pipeline("t")

.proc("deliver")
  .plan(x -> run("echo hi"))
  .when(@gen.score < 80
"#;
        let err = parse_pipeline(input).unwrap_err();
        assert!(err.msg.contains("unbalanced parens"));
    }

    #[test]
    fn inline_when_not_overridden_by_block() {
        // 内联 when 优先：块级下推只填 when.is_none() 的 impl。
        let input = r#"Pipeline("t")

.proc("deliver")
  .plan(x -> run("echo a").when(mode == "fast"), y -> run("echo b"))
  .when(@gen.score < 80)

.proc("gen")
  .plan(g -> run("echo 'score=70'"))
"#;
        let pl = parse_pipeline(input).unwrap();
        let deliver = pl.procs.iter().find(|p| p.name == "deliver").unwrap();
        assert_eq!(deliver.plan[0].when.as_deref(), Some("mode == \"fast\""));
        assert_eq!(deliver.plan[1].when.as_deref(), Some("@gen.score < 80"));
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
        // v0.11 谓词层退役：.check 解析但不入 AST（警告+忽略），门槛改独立 judge proc + .when。
        assert_eq!(pl.procs[0].checks.len(), 0);
    }

    // ── v0.11 Policy (.eval) parsing ──
    #[test]
    fn parse_policy_basic() {
        let input = r#"
// comment line
weights latency=0.002 risk=5.0 tokens=0.001 money=2.0
fail_closed = true
render.sdxl cost latency=measure("/abs/bench_sdxl.sh {topic}") risk=0.05
render.flux  cost latency=120.0
"#;
        let pol = parse_policy(input).unwrap();
        assert!((pol.weights.latency - 0.002).abs() < 1e-12);
        assert!((pol.weights.risk - 5.0).abs() < 1e-12);
        assert!((pol.weights.money - 2.0).abs() < 1e-12);
        assert_eq!(pol.costs.len(), 2);
        let spec = &pol.costs["render.sdxl"];
        assert_eq!(
            spec.latency,
            Some(CostValue::Measure("/abs/bench_sdxl.sh {topic}".into()))
        );
        assert_eq!(spec.risk, Some(CostValue::Direct(0.05)));
        assert_eq!(
            pol.costs["render.flux"].latency,
            Some(CostValue::Direct(120.0))
        );
    }

    #[test]
    fn parse_policy_rejects_garbage() {
        assert!(parse_policy("weights latency=abc").is_err());
        assert!(
            parse_policy("weights rd=1.0").is_err(),
            "rd removed in v0.11"
        );
        assert!(parse_policy("foo bar baz").is_err());
        assert!(parse_policy("a.b spend latency=1.0").is_err());
        assert!(parse_policy("a.b cost latency=measure(unclosed)").is_err());
        assert!(parse_policy("a.b cost latency=1.0 extra=oops").is_err());
    }

    #[test]
    fn parse_plan_rejects_unnamed_entry() {
        // 幽灵 impl 陷阱回归测试：weights(rd=0.5) 旧版被吃成 path_1 假成功
        let input = r#"Pipeline("t")

.proc("p")
  .plan(
    ok -> read("x"),
    weights(rd = 0.5)
  )
"#;
        assert!(parse_pipeline(input).is_err());
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
        // v0.11 谓词层退役：.ensure 同 .check，警告+忽略。
        assert_eq!(pl.procs[0].plan[0].ensure.len(), 0);
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

// ── v0.12.1 解析原语直接单测（纯函数，此前仅经集成路径间接覆盖）──

#[cfg(test)]
mod prim_tests {
    use super::*;

    // ── is_skippable ──

    #[test]
    fn skippable_blank_and_comment() {
        assert!(is_skippable(""));
        assert!(is_skippable("   "));
        assert!(is_skippable("  // comment"));
        assert!(!is_skippable(".proc(\"x\")"));
    }

    // ── extract_quoted / extract_all_quoted / extract_last_quoted ──

    #[test]
    fn quoted_first_and_last() {
        assert_eq!(extract_quoted(r#"a "first" b"#), Some("first".into()));
        assert_eq!(extract_quoted("no quotes"), None);
        assert_eq!(extract_last_quoted(r#""a" mid "b""#), Some("b".into()));
        assert_eq!(extract_last_quoted("none"), None);
    }

    #[test]
    fn all_quoted_multiple_and_unterminated() {
        assert_eq!(extract_all_quoted(r#"x="1" y="2""#), vec!["1", "2"]);
        // 未闭合引号：取到串尾
        let out = extract_all_quoted(r#""abc"#);
        assert_eq!(out, vec!["abc"]);
    }

    #[test]
    fn quoted_multibyte_safe() {
        // 中文按字符收集，不按字节错切
        assert_eq!(extract_quoted(r#"k="你好""#), Some("你好".into()));
    }

    // ── find_matching_paren ──

    #[test]
    fn matching_paren_nested() {
        assert_eq!(find_matching_paren("fn(a, g(b)) tail"), Some(10));
        assert_eq!(find_matching_paren("(outer (inner))"), Some(14));
        assert_eq!(find_matching_paren("no paren"), None);
        assert_eq!(find_matching_paren("((unbalanced"), None);
    }

    // ── find_arrow / split_impl_entries ──

    #[test]
    fn arrow_spaced_and_tight() {
        assert_eq!(find_arrow("a -> b"), Some(1)); // " -> " 模式含前导空格，始于 1
        assert_eq!(find_arrow("a ->b"), Some(2)); // 紧凑回退 "->" 始于 2
        assert_eq!(find_arrow("a-> b"), Some(1)); // 紧凑形态 "->" 在索引 1
        assert_eq!(find_arrow("no arrow"), None);
    }

    #[test]
    fn split_entries_depth_aware() {
        // 括号内的逗号不切分；换行折叠为空格
        let text = "run(\"a, b\"),\n llm(x=1)";
        let out = split_impl_entries(text);
        assert_eq!(out.len(), 2, "{:?}", out);
        assert!(out[0].contains("a, b"));
        assert!(out[1].contains("llm"));
    }

    #[test]
    fn split_entries_trailing_empty_dropped() {
        let out = split_impl_entries("a, b,");
        assert_eq!(
            out.len(),
            2,
            "trailing comma must not yield empty entry: {:?}",
            out
        );
    }

    // ── extract_refs ──

    #[test]
    fn refs_dedup_and_charset() {
        let body = "merge(@a, @b, @a-x) then @a again";
        let refs = extract_refs(body);
        assert_eq!(refs, vec!["a", "b", "a-x"]);
        // @a-x 后的 @a 不重复（a 已在首位）
    }

    #[test]
    fn refs_none_when_no_at() {
        assert!(extract_refs("plain body").is_empty());
    }

    // ── parse_policy_value / apply_policy_field ──

    #[test]
    fn policy_value_direct_number() {
        assert_eq!(parse_policy_value("5000.0"), Ok(CostValue::Direct(5000.0)));
        assert_eq!(parse_policy_value(" 42 "), Ok(CostValue::Direct(42.0)));
    }

    #[test]
    fn policy_value_measure_forms() {
        match parse_policy_value("measure(\"bench.sh {topic}\")").unwrap() {
            CostValue::Measure(cmd) => assert_eq!(cmd, "bench.sh {topic}"),
            other => panic!("{:?}", other),
        }
        // 尾随垃圾：rfind(')') 取最后一个 —— measure("a(b)")
        match parse_policy_value("measure(\"echo (x) 1.0\")").unwrap() {
            CostValue::Measure(cmd) => assert!(cmd.contains("(x)"), "{}", cmd),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn policy_value_errors() {
        // measure 缺右括号
        assert!(parse_policy_value("measure(\"cmd").is_err());
        // measure 命令为空
        assert!(parse_policy_value("measure(\"  \")").is_err());
        // 非数字非 measure
        assert!(parse_policy_value("fast").is_err());
    }

    #[test]
    fn policy_field_routing_and_unknown() {
        let mut spec = CostSpec::default();
        assert!(apply_policy_field(&mut spec, "latency", "100").is_ok());
        assert_eq!(spec.latency, Some(CostValue::Direct(100.0)));
        assert!(apply_policy_field(&mut spec, "risk", "0.5").is_ok());
        assert!(apply_policy_field(&mut spec, "money", "0.01").is_ok());
        // 未知字段
        assert!(apply_policy_field(&mut spec, "rd", "1.0").is_err());
        // 坏值
        assert!(apply_policy_field(&mut spec, "latency", "x").is_err());
    }

    // ── parse_cost_args ──

    #[test]
    fn cost_args_pairs_and_errors() {
        // 语义确认：value 段按空白/逗号截断 —— "10," 的逗号归入 value
        let out = parse_cost_args("latency=10, risk=0.5").unwrap();
        assert_eq!(out[0].0, "latency");
        assert_eq!(out[1], ("risk".to_string(), "0.5".to_string()));
        assert!(parse_cost_args("bogus").is_err());
    }
}
