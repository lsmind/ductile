//! Textargs — impl body 的纯文本解析原语。
//!
//! 从 executor.rs 拆出的无副作用函数族：函数名识别（detect_func）、
//! 变量解析（{topic}/{hash(topic)}/@proc.field）、字符串实参提取
//! （extract_string_arg / extract_first_string / extract_all_string_args）。
//! 全部为纯函数，无 I/O、无全局状态——单元测试的理想单元。

use crate::ast::Value;
use crate::dslresult::extract_field;
use std::collections::BTreeMap;

// ── Function detection ──

/// 识别 impl body 开头的函数调用名。无调用形态 → 空串。
pub fn detect_func(body: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut acc = String::new();
    let mut found_paren = false;

    for &c in &chars {
        if c == '(' {
            if !acc.is_empty() {
                found_paren = true;
                break;
            }
            return String::new();
        }
        if c.is_alphanumeric() || c == '_' {
            acc.push(c);
        } else if !acc.is_empty() {
            // Accumulated identifier but next char isn't (
            return String::new();
        }
    }
    if found_paren {
        acc
    } else {
        String::new()
    }
}

// ── Variable resolution ──

/// {topic}/{hash(topic)}/@proc/@proc.field 解析。未知 @ref 原样保留。
pub fn resolve_vars(text: &str, topic: &str, results: &BTreeMap<String, Value>) -> String {
    let t1 = text.replace("{topic}", topic);
    let t2 = t1.replace("{hash(topic)}", &short_hash(topic));

    // Replace @procname references
    let mut result = String::new();
    let chars: Vec<char> = t2.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            let start = i + 1;
            let mut end = start;
            while end < chars.len()
                && (chars[end].is_alphanumeric() || chars[end] == '_' || chars[end] == '-')
            {
                end += 1;
            }
            if end > start {
                let name: String = chars[start..end].iter().collect();

                // Check for .field suffix
                let mut field = None;
                let mut after = end;
                if after < chars.len() && chars[after] == '.' {
                    let f_start = after + 1;
                    let mut f_end = f_start;
                    while f_end < chars.len()
                        && (chars[f_end].is_alphanumeric() || chars[f_end] == '_')
                    {
                        f_end += 1;
                    }
                    field = Some(chars[f_start..f_end].iter().collect::<String>());
                    after = f_end;
                }

                match results.get(&name) {
                    Some(Value::Text(v)) => {
                        if let Some(fld) = field {
                            if let Some(fv) = extract_field(&fld, v) {
                                result.push_str(&fv);
                            } else {
                                result.push_str(&format!("<no field:{}.{}>", name, fld));
                            }
                        } else {
                            result.push_str(v);
                        }
                    }
                    Some(v) => result.push_str(v.as_text()),
                    None => {
                        result.push('@');
                        result.push_str(&name);
                    }
                }
                i = after;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

// ── String arg extraction ──

/// key="value" 或 key=value 提取。无匹配 → 空串。
pub fn extract_string_arg(key: &str, body: &str) -> String {
    // key="value" or key=value
    let pattern_q = format!("{}=\"", key);
    if let Some(pos) = body.find(&pattern_q) {
        let after = &body[pos + pattern_q.len()..];
        return after.chars().take_while(|c| *c != '"').collect();
    }
    let pattern_r = format!("{}=", key);
    if let Some(pos) = body.find(&pattern_r) {
        // Ensure it's not inside a longer key (e.g., "content" matching "cont")
        let before = if pos > 0 { &body[..pos] } else { "" };
        let last_char = before.chars().last();
        if let Some(c) = last_char {
            if c.is_alphanumeric() || c == '_' {
                return String::new();
            }
        }
        let after = &body[pos + pattern_r.len()..];
        return after
            .chars()
            .take_while(|c| *c != ',' && *c != ')' && *c != ' ')
            .collect();
    }
    String::new()
}

/// v0.16 llm agent 引用：提取动词调用括号内的**第一个裸标识符实参**。
/// `llm(planner, prompt="...")` → "planner"；`llm(prompt="...", model="x")` → None
///（首参是 k=v 或引号串都不是 agent 名）。标识符后必须跟 `,` 或 `)`。
pub fn extract_first_bare_arg(body: &str) -> Option<String> {
    // 定位动词调用的开括号（与 detect_func 同位）
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i] != '(' {
        i += 1;
    }
    if i >= chars.len() {
        return None;
    }
    i += 1; // past '('
    // 跳过空白
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    // 收集标识符字符
    let start = i;
    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
        i += 1;
    }
    if i == start {
        return None; // 首字符非标识符（引号/k=v 值等）
    }
    let ident: String = chars[start..i].iter().collect();
    // 跳过空白后必须是 , 或 )——排除 `key =` 形态（k=v 首参）
    let mut j = i;
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
    }
    if j < chars.len() && (chars[j] == ',' || chars[j] == ')') {
        Some(ident)
    } else {
        None
    }
}
pub fn extract_first_string(body: &str) -> String {
    // Escape-aware: a DSL string literal may contain \" — the first raw '"'
    // after the opener must not be preceded by an odd number of backslashes.
    let chars: Vec<char> = body.chars().collect();
    let Some(open) = chars.iter().position(|&c| c == '"') else {
        return body.to_string();
    };
    let mut i = open + 1;
    while i < chars.len() {
        if chars[i] == '"' {
            let mut backslashes = 0;
            let mut j = i;
            while j > 0 && chars[j - 1] == '\\' {
                backslashes += 1;
                j -= 1;
            }
            if backslashes % 2 == 0 {
                // real terminator — also unescape \" and \\
                let raw: String = chars[open + 1..i].iter().collect();
                let mut out = String::with_capacity(raw.len());
                let mut esc = false;
                for c in raw.chars() {
                    if esc {
                        // Only \" and \\ are DSL escapes; keep other \X literal
                        // so Windows paths like C:\Users\... survive parsing.
                        if c != '"' && c != '\\' {
                            out.push('\\');
                        }
                        out.push(c);
                        esc = false;
                    } else if c == '\\' {
                        esc = true;
                    } else {
                        out.push(c);
                    }
                }
                if esc {
                    out.push('\\');
                }
                return out;
            }
        }
        i += 1;
    }
    body.to_string()
}

/// 提取重复键的全部值：env="A" env="B" → ["A","B"]。
pub fn extract_all_string_args(key: &str, body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let pattern = format!("{}=\"", key);
    let mut rest = body;
    while let Some(pos) = rest.find(&pattern) {
        // guard against substring matches (e.g. "envx=")
        let before = if pos > 0 { &rest[..pos] } else { "" };
        if let Some(c) = before.chars().last() {
            if c.is_alphanumeric() || c == '_' {
                rest = &rest[pos + pattern.len()..];
                continue;
            }
        }
        let after = &rest[pos + pattern.len()..];
        let val: String = after.chars().take_while(|c| *c != '"').collect();
        if !val.is_empty() {
            out.push(val);
        }
        rest = &rest[pos + pattern.len()..];
    }
    out
}

// ── Helpers ──

/// ~/ 前缀展开为 $HOME。
pub fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") || path.starts_with("~\\") {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| {
                if cfg!(windows) {
                    r"C:\Users\Public".into()
                } else {
                    "/home/user".into()
                }
            });
        format!("{}{}{}", home, std::path::MAIN_SEPARATOR, &path[2..])
    } else {
        path.to_string()
    }
}

/// 文件系统路径：tilde 展开；Windows 上将 `/tmp` 与 `/tmp/...` 映射到系统临时目录。
/// （引擎 `write`/`read`/`cp` 走原生路径；Git Bash 内的 `/tmp` 仍由 bash 自己解析。）
/// v0.16：管线级 cwd 下相对路径解析到 cwd（Pipeline(cwd=...) 对 fs 动词同样生效；
/// run() 走 Command::current_dir，fs 动词没有子进程——在这里统一锚定）。
pub fn expand_fs_path(path: &str) -> String {
    let p = expand_tilde(path);
    #[cfg(windows)]
    {
        let norm = p.replace('\\', "/");
        if norm == "/tmp" || norm.starts_with("/tmp/") {
            let rest = norm.trim_start_matches("/tmp").trim_start_matches('/');
            let mut t = std::env::temp_dir();
            if !rest.is_empty() {
                t.push(rest);
            }
            return t.to_string_lossy().into_owned();
        }
    }
    // v0.16：相对路径 + 管线 cwd 存在 → 锚到 cwd（绝对路径/无 cwd 原样返回）
    if !p.starts_with('/') && !p.starts_with('~') {
        if let Some(dir) = crate::steps::PipelineCtx::cwd() {
            return format!("{}/{}", dir.trim_end_matches('/'), p);
        }
    }
    p
}

/// djb2-xor 64 位哈希，8 位十六进制。
pub fn short_hash(s: &str) -> String {
    let mut hash: u64 = 5381;
    for c in s.chars() {
        hash = ((hash << 5).wrapping_add(hash)) ^ (c as u64);
    }
    format!("{:08x}", hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── detect_func ──
    #[test]
    fn detect_func_known() {
        assert_eq!(detect_func("read(\"file.txt\")"), "read");
        assert_eq!(detect_func("web_search(query=\"AI\")"), "web_search");
        assert_eq!(detect_func("write(to=\"out\")"), "write");
        assert_eq!(detect_func("merge(@a, @b)"), "merge");
        assert_eq!(detect_func("llm(input=\"x\")"), "llm");
        assert_eq!(detect_func("sh(\"echo hi\")"), "sh");
        assert_eq!(detect_func("run(\"script\")"), "run");
    }

    #[test]
    fn detect_func_unknown() {
        assert_eq!(detect_func("noop"), "");
        assert_eq!(detect_func(""), "");
        assert_eq!(detect_func("123"), "");
    }

    // ── resolve_vars ──
    #[test]
    fn resolve_topic_var() {
        let results = BTreeMap::new();
        let out = resolve_vars("search({topic})", "AI", &results);
        assert_eq!(out, "search(AI)");
    }

    #[test]
    fn resolve_proc_ref() {
        let mut results = BTreeMap::new();
        results.insert("source".into(), Value::Text("result data".into()));
        let out = resolve_vars("process(@source)", "AI", &results);
        assert_eq!(out, "process(result data)");
    }

    #[test]
    fn resolve_unresolved_ref_left_as_is() {
        let results = BTreeMap::new();
        let out = resolve_vars("@nonexistent", "AI", &results);
        assert_eq!(out, "@nonexistent");
    }

    #[test]
    fn resolve_hash_topic() {
        let results = BTreeMap::new();
        let out = resolve_vars("{hash(topic)}", "test", &results);
        // Should be replaced with a hash, not the original {hash(topic)}
        assert_ne!(out, "{hash(topic)}");
        assert!(!out.is_empty());
    }

    // ── extract_string_arg ──
    #[test]
    fn extract_string_arg_quoted() {
        assert_eq!(extract_string_arg("query", r#"query="AI news""#), "AI news");
        assert_eq!(
            extract_string_arg("to", r#"to="/tmp/out.txt""#),
            "/tmp/out.txt"
        );
    }

    #[test]
    fn extract_string_arg_unquoted() {
        assert_eq!(extract_string_arg("n", "n=5"), "5");
    }

    #[test]
    fn extract_string_arg_missing() {
        assert_eq!(extract_string_arg("missing", "query=\"AI\""), "");
    }

    // ── v0.16 llm agent 裸首参 ──

    #[test]
    fn first_bare_arg_agent_name() {
        assert_eq!(
            extract_first_bare_arg("llm(planner, prompt=\"hi\")"),
            Some("planner".into())
        );
        // 空白容忍
        assert_eq!(
            extract_first_bare_arg("llm( judge , prompt=\"hi\")"),
            Some("judge".into())
        );
    }

    #[test]
    fn first_bare_arg_none_for_kv_or_quoted() {
        // 首参 k=v → None
        assert_eq!(extract_first_bare_arg("llm(prompt=\"hi\", model=\"x\")"), None);
        // 首参引号串 → None
        assert_eq!(extract_first_bare_arg("llm(\"plain prompt\")"), None);
        // 空 → None
        assert_eq!(extract_first_bare_arg("llm()"), None);
    }

    #[test]
    fn extract_string_arg_no_partial_match() {
        // "content" should not match partial "cont"
        assert_eq!(extract_string_arg("cont", r#"content="hello""#), "");
    }

    // ── short_hash ──
    #[test]
    fn short_hash_deterministic() {
        let h1 = short_hash("test");
        let h2 = short_hash("test");
        assert_eq!(h1, h2);
        assert!(h1.len() >= 8); // at least 8 hex chars (djb2 can overflow 8)
    }

    #[test]
    fn short_hash_different_inputs() {
        assert_ne!(short_hash("a"), short_hash("b"));
    }

    // ── expand_tilde ──
    #[test]
    fn expand_tilde_with_slash() {
        let expanded = expand_tilde("~/test");
        assert!(!expanded.contains('~'));
        assert!(
            expanded.ends_with("/test") || expanded.ends_with("\\test"),
            "{}",
            expanded
        );
    }

    #[test]
    fn expand_tilde_no_tilde() {
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
    }

    // ── 新增边界用例 ──

    #[test]
    fn detect_func_leading_space() {
        assert_eq!(detect_func("  run(\"x\")"), "run");
    }

    #[test]
    fn detect_func_paren_first() {
        assert_eq!(detect_func("(\"x\")"), "");
    }

    #[test]
    fn detect_func_word_then_space() {
        assert_eq!(detect_func("echo hi ("), "");
    }

    #[test]
    fn resolve_proc_field_from_structured() {
        let encoded =
            crate::dslresult::encode_structured_result(&[("score".into(), "85".into())], "raw");
        let mut results = BTreeMap::new();
        results.insert("gate".into(), Value::Text(encoded));
        assert_eq!(resolve_vars("@gate.score", "t", &results), "85");
    }

    #[test]
    fn resolve_proc_field_missing_keeps_marker() {
        let encoded = crate::dslresult::encode_structured_result(&[("x".into(), "1".into())], "r");
        let mut results = BTreeMap::new();
        results.insert("gate".into(), Value::Text(encoded));
        let out = resolve_vars("@gate.score", "t", &results);
        assert!(out.contains("<no field:gate.score>"), "{}", out);
    }

    #[test]
    fn extract_first_string_escapes() {
        // DSL 字符串内的 \" 转义：终止符不应被奇数反斜杠提前截断
        let body = "sh(\"echo \\\"hi\\\"\")";
        assert_eq!(extract_first_string(body), "echo \"hi\"");
    }

    #[test]
    fn extract_first_string_keeps_windows_path_backslashes() {
        let body = r#"mkdir("C:\Users\foo\bar")"#;
        assert_eq!(extract_first_string(body), r"C:\Users\foo\bar");
    }

    #[test]
    fn extract_first_string_no_quotes_returns_body() {
        assert_eq!(extract_first_string("plain"), "plain");
    }

    #[test]
    fn extract_string_arg_unquoted_stops_at_delims() {
        assert_eq!(extract_string_arg("n", "n=5,x=2"), "5");
        assert_eq!(extract_string_arg("n", "n=5)"), "5");
    }

    #[test]
    fn extract_all_string_args_multiple() {
        let body = r#"run("x", env="A=1", env="B=2")"#;
        assert_eq!(extract_all_string_args("env", body), vec!["A=1", "B=2"]);
    }

    #[test]
    fn extract_all_string_args_substring_guard() {
        let body = r#"run("x", envx="A=1")"#;
        assert!(extract_all_string_args("env", body).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn expand_fs_path_maps_tmp_on_windows() {
        let p = expand_fs_path("/tmp/ductile_win_path_probe.txt");
        assert!(!p.starts_with("/tmp"), "got {}", p);
        assert!(p.contains("ductile_win_path_probe.txt"), "got {}", p);
    }

    #[cfg(not(windows))]
    #[test]
    fn expand_fs_path_keeps_tmp_on_unix() {
        assert_eq!(
            expand_fs_path("/tmp/ductile_unix_path_probe.txt"),
            "/tmp/ductile_unix_path_probe.txt"
        );
    }

    #[test]
    fn expand_tilde_only_prefix() {
        assert_eq!(expand_tilde("/a/~b"), "/a/~b");
    }
}
