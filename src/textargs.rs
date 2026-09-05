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

/// 提取 body 第一个带引号字符串（转义感知）；无引号 → body 原样。
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
                        out.push(c);
                        esc = false;
                    } else if c == '\\' {
                        esc = true;
                    } else {
                        out.push(c);
                    }
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
    if path.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".into());
        format!("{}/{}", home, &path[2..])
    } else {
        path.to_string()
    }
}

/// djb2-xor 64 位哈希，8 位十六进制。
pub fn short_hash(s: &str) -> String {
    let mut hash: u64 = 5381;
    for c in s.chars() {
        hash = ((hash << 5).wrapping_add(hash)) ^ (c as u64);
    }
    format!("{:08x}", hash)
}
