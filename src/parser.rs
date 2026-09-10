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

// Check if line is blank or comment. Shebang (`#!`) lines are skipped too —
// they let a .pipeline be `chmod +x`'d and run directly via
// `#!/path/to/ductile run` (Linux passes the script path as argv[2] of the
// interpreter, landing exactly on the `run <file>` dispatch).
pub(crate) fn is_skippable(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("#!")
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
    let (name, description, cwd, env) = parse_header(header_line, idx + 1)?;

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
        cwd,
        env,
    })
}

/// v0.16 头部 k=v 参数提取：cwd="..." 与 env=["K=V", "K2=V2"]。
/// env 值按引号块切分，每块再剥一层引号。env 引号块内还含逗号也原样保留
/// （split(',') 会切断 "A=1, B=2"——块提取不受影响）。
fn parse_header_kv(line: &str) -> (Option<String>, Vec<String>) {
    let mut cwd = None;
    let mut env: Vec<String> = vec![];
    if let Some(p) = line.find("cwd=") {
        // cwd="..." — 取 cwd= 后第一个引号块
        let after = &line[p + 4..];
        if let Some(q1) = after.find('"') {
            let rest = &after[q1 + 1..];
            if let Some(q2) = rest.find('"') {
                cwd = Some(rest[..q2].to_string());
            }
        }
    }
    if let Some(p) = line.find("env=[") {
        let after = &line[p + 5..];
        if let Some(br) = after.find(']') {
            let inner = &after[..br];
            // 引号块切分：连续引号对之间的内容
            let bytes: Vec<char> = inner.chars().collect();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == '"' {
                    let start = i + 1;
                    let mut end = start;
                    while end < bytes.len() && bytes[end] != '"' {
                        end += 1;
                    }
                    let v: String = bytes[start..end].iter().collect();
                    if !v.trim().is_empty() {
                        env.push(v);
                    }
                    i = end + 1;
                } else {
                    i += 1;
                }
            }
        }
    }
    (cwd, env)
}

fn parse_header(line: &str, line_num: usize) -> Result<(String, String, Option<String>, Vec<String>), ParseError> {
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

    // Extract quoted strings: first is name, optional second is description.
    // cwd=/env= 的引号块也混在 quoted 流里——先剥掉 k=v 区域再取 name/desc，
    // 否则 Pipeline("x", cwd="/a") 会把 "/a" 当成 description。
    let kv_start = line
        .find("cwd=")
        .or_else(|| line.find("env=["))
        .unwrap_or(line.len());
    let desc_zone = &line[..kv_start];
    let quoted = extract_all_quoted(desc_zone);
    let name = quoted.first().cloned().unwrap_or_default();
    let description = quoted.get(1).cloned().unwrap_or_default();
    let (cwd, env) = parse_header_kv(line);
    Ok((name, description, cwd, env))
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

/// 返回**字节**索引（调用方全部拿它做字符串切片）。
/// v0.14.2 fix：旧版返回 char_indices 序号，调用方当字节索引用——
/// 多字节字符（中文/→/…）之后偏移 N-1 字节，strip 尾巴残留垃圾
/// （impl .desc 含 "→" 时残留 `SS")`，issue #1 深层根因）。
/// 现在直接在字节维度扫括号深度，索引即字节，语义对齐。
pub(crate) fn find_matching_paren(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
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

    // v0.16 单 impl 内联糖：.proc("name", verb(...))
    // 候选 = name 后剥掉「, 」到行尾的整段（不做括号截断——修饰符在动词调用
    // 之后，截断会静默丢弃 .when/.retry），再剥 .proc( 自身的闭括号。
    // 候选必须以已知动词调用开头；箭头缺席才激活，`name -> body` 形态留给
    // .plan 的多路选择。
    let mut inline_body: Option<String> = None;
    if find_arrow(line).is_none() {
        // 找带引号的完整形态（"name"），跳过闭引号后再剥「, 」——
        // 只跳 name 长度会停在闭引号上，候选头部残留 `", run(...)`。
        let quoted_name = format!("\"{}\"", name);
        if let Some(p) = line.find(&quoted_name) {
            let mut c = line[p + quoted_name.len()..]
                .trim_start()
                .trim_start_matches(',')
                .trim_start();
            if let Some(stripped) = c.strip_suffix(')') {
                c = stripped.trim_end();
            }
            let candidate = c.to_string();
            let func = crate::textargs::detect_func(&candidate);
            if !func.is_empty() && crate::steps::known_functions().contains(&func.as_str()) {
                inline_body = Some(candidate);
            }
        }
    }

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
    let mut contract: Option<crate::ast::Contract> = None;

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

        // .contract(outputs="a,b", invariants="...", invariants="...")
        // v0.15 节点契约卡（cognition spec §7 P0）：outputs/invariants 可重复，
        // outputs 逗号分隔多字段。解析后入 Proc.contract，执行后校验。
        if trimmed.starts_with(".contract(") || trimmed.starts_with(".contract (") {
            let after = trimmed
                .find(".contract(")
                .map(|p| &trimmed[p + ".contract(".len()..])
                .unwrap_or_else(|| {
                    let p = trimmed.find(".contract (").unwrap();
                    &trimmed[p + ".contract (".len()..]
                });
            let close = find_matching_paren(&format!("({}", after))
                .map(|i| i.saturating_sub(1))
                .ok_or_else(|| ParseError {
                    line: idx + 1,
                    col: 1,
                    msg: "unbalanced parens in .contract(...)".into(),
                    line_text: raw.to_string(),
                })?;
            let inner = &after[..close];
            let mut outputs: Vec<String> = Vec::new();
            let mut invariants: Vec<String> = Vec::new();
            for part in split_kv_args(inner) {
                let (k, v) = part;
                match k.as_str() {
                    "outputs" => {
                        for f in v.split(',') {
                            let f = f.trim();
                            if !f.is_empty() {
                                outputs.push(f.to_string());
                            }
                        }
                    }
                    "invariants" => {
                        let v = v.trim();
                        if !v.is_empty() {
                            invariants.push(v.to_string());
                        }
                    }
                    _ => {
                        return Err(ParseError {
                            line: idx + 1,
                            col: 1,
                            msg: format!(
                                "unknown .contract() key {:?} — expected outputs= or invariants=",
                                k
                            ),
                            line_text: raw.to_string(),
                        });
                    }
                }
            }
            contract = Some(crate::ast::Contract { outputs, invariants });
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

    // v0.16 单 impl 内联糖脱糖：.proc("x", verb(...)) ≡ .plan(verb -> verb(...))。
    // impl 名 = 动词名；tags 缺席时按动词推导（#run/#write/…——第二刀）；
    // 块级 .when 语义不变（下推）；.plan 与内联共存时 .plan 追加（防呆：单
    // impl 糖下再写 .plan 属于自相矛盾，追加而不是硬错，兼容生成器输出）。
    if let Some(body) = inline_body {
        let (body_text, _cost, retry, ensure, when, enabled, stub, tags, description) =
            extract_cost_and_modifiers(&body, &name);
        let func = crate::textargs::detect_func(&body_text);
        // 第二刀：手写 tags 为空时按动词推导（语义域标记如 #git/#gate 仍可手写叠加）
        let mut tags = tags;
        if tags.is_empty() && !func.is_empty() {
            tags.insert(func.clone());
        }
        let mut refs = extract_refs(&body_text);
        if let Some(w) = &when {
            for r in extract_refs(w) {
                if !refs.contains(&r) {
                    refs.push(r);
                }
            }
        }
        let inline_impl = Impl {
            name: func,
            tags,
            cost: _cost,
            enabled,
            when: when.or(proc_when.clone()),
            refs,
            body_text,
            stub,
            retry,
            ensure,
            description,
        };
        plan.insert(0, inline_impl);
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
            contract: contract.unwrap_or_default(),
            deliver: is_deliver,
            deliver_refs,
            foreach: foreach_src,
            foreach_var,
            pick_by,
        },
        idx,
    ))
}

/// .contract(...) 内的 k=v 参数切分：引号感知（值可含逗号/空格），
/// 无引号值取到下一个 ` key=` 或结尾。返回 (key, value) 序列。
fn split_kv_args(inner: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let chars: Vec<char> = inner.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // skip whitespace/commas
        while i < chars.len() && (chars[i] == ' ' || chars[i] == ',') {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        // key = identifier chars up to '='
        let kstart = i;
        while i < chars.len() && chars[i] != '=' && chars[i] != ',' && chars[i] != ' ' {
            i += 1;
        }
        if i >= chars.len() || chars[i] != '=' {
            // malformed or trailing junk — skip one char to guarantee progress
            i = kstart + 1;
            continue;
        }
        let key: String = chars[kstart..i].iter().collect();
        i += 1; // past '='
        // skip spaces
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        if i < chars.len() && chars[i] == '"' {
            // quoted value: take until closing quote (escape-aware)
            i += 1;
            let vstart = i;
            let mut esc = false;
            while i < chars.len() {
                if esc {
                    esc = false;
                    i += 1;
                    continue;
                }
                if chars[i] == '\\' {
                    esc = true;
                    i += 1;
                    continue;
                }
                if chars[i] == '"' {
                    break;
                }
                i += 1;
            }
            let val: String = chars[vstart..i.min(chars.len())].iter().collect();
            if i < chars.len() {
                i += 1; // past closing quote
            }
            // DSL 转义还原（与 extract_first_string 同语义）：\"→"，\\→\，
            // 其余 \X 保字面量（Windows 路径兼容）
            let mut unescaped = String::with_capacity(val.len());
            let mut esc = false;
            for ch in val.chars() {
                if esc {
                    if ch != '"' && ch != '\\' {
                        unescaped.push('\\');
                    }
                    unescaped.push(ch);
                    esc = false;
                } else if ch == '\\' {
                    esc = true;
                } else {
                    unescaped.push(ch);
                }
            }
            out.push((key, unescaped));
        } else {
            // bare value: until ',' (no nesting in this context)
            let vstart = i;
            while i < chars.len() && chars[i] != ',' {
                i += 1;
            }
            let val: String = chars[vstart..i].iter().collect();
            out.push((key, val.trim().to_string()));
        }
    }
    out
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
    // v0.14.3 fix: 深度扫描必须跳过 DSL 字符串字面量内的括号。
    // 旧版裸扫：`run("case x in h*) ... *) ...")` 里字符串内的 `)` 会把
    // depth 提前打到 0，plan 块在字符串中间截断，后续文本成孤儿行
    // （devcycle.pipeline 路由 crash 根因；awk '{print $1}'、$() 同类）。
    // 转义感知：字符串内的 \" 不闭串（与 extract_first_string 同语义）。
    let mut in_string = false;
    let mut esc = false;

    while idx < lines.len() {
        let line = lines[idx];
        for c in line.chars() {
            if esc {
                buf.push(c);
                esc = false;
                continue;
            }
            match c {
                '\\' if in_string => {
                    buf.push(c);
                    esc = true;
                }
                '"' => {
                    in_string = !in_string;
                    buf.push(c);
                }
                '(' if !in_string => {
                    depth += 1;
                    started = true;
                    buf.push(c);
                }
                ')' if !in_string => {
                    depth -= 1;
                    if started && depth == 0 {
                        buf.push(c);
                        break;
                    }
                    buf.push(c);
                }
                _ => buf.push(c),
            }
        }
        buf.push('\n');
        idx += 1;
        if started && depth <= 0 {
            break;
        }
    }

    // 串未闭而 plan 块结束（depth 归 0 或行耗尽）= DSL 语法错误（fail-closed），
    // 不再静默截断产生垃圾 body。注：depth==0 正常闭合点串必已闭（引号内的
    // ) 不减深度），所以这里抓的是「字符串跨过 plan 边界还没闭合」的形态。
    if in_string {
        return Err(ParseError {
            line: idx,
            col: 1,
            msg: "unbalanced quotes inside .plan(...) — a DSL string literal is never closed".into(),
            line_text: lines[idx.saturating_sub(1)].to_string(),
        });
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

            let (body_text, cost, retry, ensure, when, enabled, stub, tags, description) =
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
                description,
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

/// Extract .cost(), .retry(), .ensure(), .when(), .disabled, .stub, .tags, and .desc()
/// Returns (body_text, cost, retry, ensure, when, enabled, stub, tags, description)
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
    String,
) {
    let mut cost = Cost::default();
    let mut body = text.trim().to_string();
    let mut retry = 0;
    let mut ensure = Vec::new();
    let mut when = None;
    let mut enabled = true;
    let mut stub = false;
    let mut tags = BTreeSet::new();
    let mut description = String::new();

    // impl 级 .desc("text")：剥离去 description 字段。
    // 此前从未剥离——尾部残留 `.desc(` 会被 script() 的 rfind(')') 探进去，
    // tokenizer 吞掉 `.desc(` 后的文本到值里（issue #1：静默脏值）；
    // 且 Impl.description 从未被解析（永远是空串）。两处一并修。
    if let Some(d_pos) = body.find(".desc(") {
        let after = &body[d_pos..];
        if let Some(close) = find_matching_paren(after) {
            let inner = &after[6..close];
            // 取第一个引号对内的文本；裸文本也接受
            description = inner.trim().to_string();
            if let Some(stripped) = description.strip_prefix('"') {
                description = stripped.split('"').next().unwrap_or("").to_string();
            }
            body = format!("{}{}", &body[..d_pos], &after[close + 1..]);
        }
    }

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
                // find_matching_paren 返回字节索引（v0.14.2 语义），但它是相对
                // after 的——close+1 可能正好落在多字节字符中间或越界（末尾
                // 中文标点形态实测 panic start byte index out of bounds）。
                // 剥离用 char 边界钳制：close+1 不在边界上就退到下一个边界。
                let strip_at = (close + 1).min(after.len());
                let strip_at = after
                    .char_indices()
                    .map(|(b, _)| b)
                    .find(|b| *b >= strip_at)
                    .unwrap_or(after.len());
                let inner_start = prefix.len();
                if strip_at >= inner_start {
                    body = format!("{}{}", &body[..e_pos], &after[strip_at..]);
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
        description,
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
    fn parse_impl_desc_stripped_from_body() {
        // issue #1 回归：impl 级 .desc 此前不剥离，尾部 `.desc(` 被 script() 的
        // rfind(')') 探进去吞值。修后 body_text 干净、description 有值。
        let input = r#"Pipeline("t")
.proc("v")
  .plan(
    v -> script(s0_probe3, a="v=@armA.ratio", b="r=1", c="tail")
      .tags(#t)
      .desc("armA < armB 且 roundtrip=1 → PASS")
  )
"#;
        let pl = parse_pipeline(input).unwrap();
        let imp = &pl.procs[0].plan[0];
        assert!(
            !imp.body_text.contains(".desc"),
            "body leaked: {}",
            imp.body_text
        );
        assert_eq!(
            imp.body_text.trim(),
            r#"script(s0_probe3, a="v=@armA.ratio", b="r=1", c="tail")"#
        );
        assert_eq!(imp.description, "armA < armB 且 roundtrip=1 → PASS");
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

    #[test]
    fn plan_block_parens_inside_string_literal_do_not_close_block() {
        // v0.14.3: 字符串字面量内的 ) 不得把 .plan 深度打到 0。
        // devcycle.pipeline 路由 crash 根因：`case "$TOPIC" in start:*) ... *)`
        let src = "Pipeline(\"t\", \"case regression\")\n  .proc(\"route\")\n    .plan(\n      r -> run(\"case hello in h*) echo MATCH ;; *) echo OTHER ;; esac\")\n    )\n  .proc(\"deliver\")\n    .deliver(@route)\n";
        let pl = parse_pipeline(src).unwrap();
        assert_eq!(pl.procs.len(), 2);
        let route = &pl.procs[0];
        assert_eq!(route.plan.len(), 1);
        // body 必须完整到达 esac，未被字符串内的 ) 截断
        assert!(route.plan[0].body_text.contains("esac"), "body truncated: {:?}", route.plan[0].body_text);
        assert!(route.plan[0].body_text.contains("*)"), "case wildcard arm lost");
    }

    #[test]
    fn plan_block_unterminated_string_is_hard_error() {
        // 串未闭而 plan 结束 → fail-closed 硬错误，不静默截断
        let src = "Pipeline(\"t\", \"unterminated\")\n  .proc(\"p\")\n    .plan(\n      x -> run(\"echo never-closed)\n    )\n";
        assert!(parse_pipeline(src).is_err());
    }

    #[test]
    fn contract_card_parse_outputs_and_invariants() {
        // v0.15 节点契约卡：outputs 逗号展开、invariants 可重复、引号值转义还原
        let src = "Pipeline(\"t\")\n  .proc(\"judge\")\n    .plan(j -> run(\"echo ok\"))\n    .contract(outputs=\"score, note\", invariants=\"@self.score >= 80\", invariants=\"@self.city != \\\"\\\"\")\n";
        let pl = parse_pipeline(src).unwrap();
        let p = &pl.procs[0];
        assert_eq!(p.contract.outputs, vec!["score", "note"]);
        assert_eq!(p.contract.invariants.len(), 2);
        assert_eq!(p.contract.invariants[0], "@self.score >= 80");
        assert_eq!(p.contract.invariants[1], "@self.city != \"\"");
    }

    #[test]
    fn contract_card_unknown_key_is_hard_error() {
        let src = "Pipeline(\"t\")\n  .proc(\"p\")\n    .plan(x -> run(\"echo ok\"))\n    .contract(bogus=\"x\")\n";
        let err = parse_pipeline(src).unwrap_err();
        assert!(err.msg.contains("unknown .contract() key"), "got: {}", err.msg);
    }

    #[test]
    fn contract_check_l1_missing_field_and_l2_invariant() {
        // executor::check_contract 纯函数级：L1 缺字段 / L2 谓词违例 / 全通过
        use crate::executor::check_contract;
        use crate::ast::Contract;
        let mk = |outputs: Vec<&str>, invariants: Vec<&str>| crate::ast::Proc {
            name: "p".into(),
            description: String::new(),
            plan: vec![],
            checks: vec![],
            contract: Contract {
                outputs: outputs.into_iter().map(String::from).collect(),
                invariants: invariants.into_iter().map(String::from).collect(),
            },
            deliver: false,
            deliver_refs: vec![],
            foreach: None,
            foreach_var: String::new(),
            pick_by: String::new(),
        };
        // L1 通过 + L2 通过
        let ok_val = Value::Text("§§FIELDS§§score=85§§note=x§§RAW§§raw".into());
        assert!(check_contract(&mk(vec!["score"], vec!["@self.score >= 80"]), &ok_val).is_ok());
        // L1 缺字段
        let missing = Value::Text("§§FIELDS§§other=1§§RAW§§raw".into());
        let e = check_contract(&mk(vec!["path"], vec![]), &missing).unwrap_err();
        assert!(e.starts_with("contract violation:"), "got: {}", e);
        // L2 违例（截断事故形态：score=10 合法整数但 < 80）
        let trunc = Value::Text("§§FIELDS§§score=10§§RAW§§raw".into());
        let e = check_contract(&mk(vec!["score"], vec!["@self.score >= 80"]), &trunc).unwrap_err();
        assert!(e.contains("invariant failed"), "got: {}", e);
        // 裸文本 + 声明了契约 → fail-closed
        let bare = Value::Text("plain output".into());
        assert!(check_contract(&mk(vec!["score"], vec![]), &bare).is_err());
    }

    // ── v0.16 三刀：单 impl 内联糖 / tags 动词推导 / 管线级 cwd+env ──

    #[test]
    fn inline_impl_sugar_desugars_to_plan() {
        // 第一刀：.proc("x", verb(...)) ≡ .plan(verb -> verb(...))
        let src = "Pipeline(\"t\")\n  .proc(\"analyze\", script(word_stats, text=\"{topic}\"))\n";
        let pl = parse_pipeline(src).unwrap();
        assert_eq!(pl.procs.len(), 1);
        assert_eq!(pl.procs[0].plan.len(), 1);
        let imp = &pl.procs[0].plan[0];
        assert_eq!(imp.name, "script");
        assert_eq!(imp.body_text, "script(word_stats, text=\"{topic}\")");
        // 第二刀：tags 按动词推导
        assert!(imp.tags.contains("script"), "tags: {:?}", imp.tags);
    }

    #[test]
    fn inline_impl_sugar_preserves_trailing_modifiers() {
        // 修饰符在动词调用之后：.when/.retry 必须存活（截断=静默丢门禁）
        let src = "Pipeline(\"t\")\n  .proc(\"push\", run(\"echo push\").retry(n=2).when(mode == \"fast\"))\n";
        let pl = parse_pipeline(src).unwrap();
        let imp = &pl.procs[0].plan[0];
        assert_eq!(imp.name, "run");
        assert_eq!(imp.retry, 2, "retry lost — body: {:?}", imp.body_text);
        assert_eq!(
            imp.when.as_deref(),
            Some("mode == \"fast\""),
            "when lost — body: {:?}",
            imp.body_text
        );
        // body 应只剩裸调用
        assert_eq!(imp.body_text, "run(\"echo push\")");
    }

    #[test]
    fn inline_impl_sugar_block_when_still_applies() {
        // 块级 .when 下推不因内联糖失效
        let src = "Pipeline(\"t\")\n  .proc(\"deliver\", run(\"echo DELIVERED\"))\n    .when(@gen.score < 80)\n";
        let pl = parse_pipeline(src).unwrap();
        let imp = &pl.procs[0].plan[0];
        assert_eq!(imp.when.as_deref(), Some("@gen.score < 80"));
        // 裁判 @ref 并入 refs（egraph 建边依赖）
        assert!(imp.refs.contains(&"gen".to_string()), "refs: {:?}", imp.refs);
    }

    #[test]
    fn inline_impl_sugar_string_with_paren_not_truncated() {
        // 字符串字面量内的括号/引号不截断 body（与 .plan 深度扫描同语义）
        let src = "Pipeline(\"t\")\n  .proc(\"x\", run(\"echo 'case (a) *)'\"))\n";
        let pl = parse_pipeline(src).unwrap();
        let imp = &pl.procs[0].plan[0];
        assert!(
            imp.body_text.contains("case (a) *)"),
            "body truncated: {:?}",
            imp.body_text
        );
    }

    #[test]
    fn inline_impl_sugar_ignored_for_unknown_func_and_arrow() {
        // 未知动词 → 不激活（fail-closed 交给执行层报错，不产生幽灵 impl）
        let src = "Pipeline(\"t\")\n  .proc(\"p\", bogus_verb(\"x\"))\n";
        let pl = parse_pipeline(src).unwrap();
        assert!(pl.procs[0].plan.is_empty(), "should not desugar unknown verb");
        // 箭头形态 → 留给 .plan 语义
        let src2 = "Pipeline(\"t\")\n  .proc(\"p\", a -> run(\"echo x\"))\n";
        let pl2 = parse_pipeline(src2).unwrap();
        assert!(pl2.procs[0].plan.is_empty());
    }

    #[test]
    fn header_cwd_env_parsed() {
        // 第三刀：Pipeline(..., cwd="...", env=["K=V", ...])
        let src = "Pipeline(\"ship\", \"desc\", cwd=\"$HOME/projects/ductile\", env=[\"A=1\", \"B=two words\"])\n";
        let pl = parse_pipeline(src).unwrap();
        assert_eq!(pl.name, "ship");
        assert_eq!(pl.description, "desc");
        assert_eq!(pl.cwd.as_deref(), Some("$HOME/projects/ductile"));
        assert_eq!(pl.env, vec!["A=1".to_string(), "B=two words".to_string()]);
    }

    #[test]
    fn header_cwd_alone_no_description_pollution() {
        // cwd 的引号块不能漏进 description
        let src = "Pipeline(\"ship\", cwd=\"/tmp\")\n";
        let pl = parse_pipeline(src).unwrap();
        assert_eq!(pl.name, "ship");
        assert_eq!(pl.description, "");
        assert_eq!(pl.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn header_without_kv_unchanged() {
        // 旧形态零回归：无 cwd/env 时行为与 v0.15 完全一致
        let src = "Pipeline(\"a\", \"b\")\n";
        let pl = parse_pipeline(src).unwrap();
        assert_eq!(pl.name, "a");
        assert_eq!(pl.description, "b");
        assert!(pl.cwd.is_none());
        assert!(pl.env.is_empty());
    }
}
