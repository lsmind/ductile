//! v0.12 脚本契约层 — 脚本即 API。
//!
//! 脚本不写入 .pipeline（DSL 只留 `script(name, k=v...)` 链接），本体注册在
//! ductile 脚本库（SQLite scripts 表），通过结构化契约头自描述：
//!
//! ```text
//! # ductile: v1
//! # name: resize_image
//! # desc: 等比缩放图片到目标宽度
//! # lang: python
//! # params: src(path, required), width(int, default=800)
//! # output: out(path), bytes(int)
//! # pure: false
//! # idempotent: true
//! # concurrency: safe
//! # effects: fs
//! # timeout: 60
//! # retries: 1
//! ```
//!
//! 契约头必须出现在脚本前部（首个非注释/非空行之前），全部用 `#` 前缀
//! （sh/python/powershell/ruby 通用）。解析 fail-closed：缺必填键、未知
//! lang、未知 concurrency 一律拒绝注册。
//!
//! 语义标注的编排意义：
//! - `pure` — 无声明外副作用；pure+idempotent+safe 才允许 e-graph CSE 熔合
//! - `idempotent` — f(f(x))==f(x)，重试安全
//! - `concurrency` — safe(随便并发) / exclusive(不许与自身并发) / serial(禁并发)
//! - `effects` — none/fs/net/process/system，声明副作用面（诚实声明是作者责任）

use crate::core::script_card::{Concurrency, ScriptCard};
use std::collections::BTreeMap;

/// lang → 解释器二进制。注册时校验，未知 lang fail-closed。
pub fn lang_interpreter(lang: &str) -> Result<&'static str, String> {
    match lang.trim() {
        "python" | "python3" => Ok("python3"),
        "sh" | "bash" => Ok("bash"),
        "powershell" | "pwsh" => Ok("pwsh"),
        "node" | "js" => Ok("node"),
        other => Err(format!(
            "unknown lang '{}' (known: python/sh/bash/powershell/node)",
            other
        )),
    }
}

/// CSE 熔合安全性：只有显式声明 纯 + 幂等 + 可并发 的脚本才允许
/// e-graph 同构熔合（两个同构调用合并为一次执行）。其余一律 no_cse。
/// v0.18.13 FOPT 承重：mcsm 坐标含 2（矛盾态）→ 强制 no_cse——
/// 措施表"2=隔离防传染"的物理强制：有未解矛盾的脚本不许与任何东西熔合
/// （熔合会传播失败模式）。1/3/4 不拦：1 稳态可熔，3 构造态 tentative
/// 观察（靠 canary），4 实践扩展允许（经验账本在积累证据）。
pub fn cse_safe(card: &ScriptCard) -> bool {
    let base = card.pure && card.idempotent && card.concurrency == Concurrency::Safe;
    if !base {
        return false;
    }
    if mcsm_has_conflict(&card.mcsm) {
        return false;
    }
    true
}

/// v0.18.13：mcsm 坐标任一维度 = 2（矛盾态）？
/// F(2)-O(1)-P(3)-T(2) → true（场域和目的都有矛盾）。
/// 空 mcsm（未标注）不拦——FOPT 是可选接口不是强制税。
pub fn mcsm_has_conflict(mcsm: &str) -> bool {
    if mcsm.is_empty() {
        return false;
    }
    // 已在 parse 时规范化为 F(n)-O(n)-P(n)-T(n)；防御性解析
    mcsm.contains("(2)")
}

/// 从脚本源码解析契约头。fail-closed：缺 `# ductile:` 起始行或任一必填键
/// 直接报错（列出全部缺失项），绝不带病注册。
pub fn parse_contract(source: &str, path: &str) -> Result<ScriptCard, String> {
    let mut kv: BTreeMap<String, String> = BTreeMap::new();
    let mut started = false;

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !started {
                continue;
            }
            break; // 头部后的空行结束契约区
        }
        if !trimmed.starts_with('#') {
            break; // 首个非注释行：契约区结束
        }
        let body = trimmed.trim_start_matches('#').trim();
        if let Some(rest) = body.strip_prefix("ductile:") {
            if rest.trim() == "v1" {
                started = true;
                continue;
            }
            return Err(format!(
                "unsupported contract version '{}' (want: ductile: v1)",
                rest.trim()
            ));
        }
        if started {
            if let Some(eq) = body.find(':') {
                let k = body[..eq].trim().to_lowercase();
                let v = body[eq + 1..].trim().to_string();
                if !k.is_empty() && !v.is_empty() {
                    kv.insert(k, v);
                }
            }
        }
    }

    if !started {
        return Err(format!(
            "{}: no contract header — first comment line must be `# ductile: v1`",
            path
        ));
    }

    // 必填键（fail-closed：一次报出全部缺失，省得作者补一轮错一轮）
    let required = [
        "name",
        "desc",
        "lang",
        "output",
        "pure",
        "idempotent",
        "concurrency",
        "effects",
    ];
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|k| !kv.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{}: contract header missing required keys: {}",
            path,
            missing.join(", ")
        ));
    }

    let lang = kv["lang"].clone();
    lang_interpreter(&lang)?; // 未知 lang 直接拒
    let concurrency = Concurrency::parse(&kv["concurrency"])?;

    let name = kv["name"].clone();
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        || name.is_empty()
    {
        return Err(format!(
            "{}: invalid script name '{}' (alphanumeric/_/- only)",
            path, name
        ));
    }

    let parse_bool = |k: &str| -> Result<bool, String> {
        match kv[k].as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            other => Err(format!(
                "{}: {} must be true/false, got '{}'",
                path, k, other
            )),
        }
    };
    let pure = parse_bool("pure")?;
    let idempotent = parse_bool("idempotent")?;

    let timeout_secs = kv
        .get("timeout")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300);
    let retries = kv
        .get("retries")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    // v0.18.11 MCSM/FOPT：可选键 mcsm —— F(n)-O(n)-P(n)-T(n) 认知坐标。
    // 声明了就校验（fail-closed：坐标垃圾拒绝入库），没声明默认空。
    let mcsm = match kv.get("mcsm") {
        Some(raw) => normalize_mcsm(raw, path)?,
        None => String::new(),
    };

    Ok(ScriptCard {
        name,
        path: path.to_string(),
        lang,
        desc: kv["desc"].clone(),
        params: kv.get("params").cloned().unwrap_or_default(),
        output: kv["output"].clone(),
        pure,
        idempotent,
        concurrency,
        effects: kv["effects"].clone(),
        timeout_secs,
        retries,
        mcsm,
    })
}

/// v0.18.11 校验并规范化 MCSM/FOPT 坐标。
/// 合法格式：`F(1)-O(2)-P(3)-T(4)`（字母 F/O/P/T 不区分大小写，数字 1-4）。
/// 数字不是价值高低——是该维度在机制成熟循环中的阶段：
/// 1=稳定通用机制 2=矛盾出现 3=构造新机制解决矛盾 4=实践扩展新机制，
/// 沉淀后回到 1'。F(1)-O(1)-P(1)-T(1)=稳态。
/// 返回规范化大写形式。见 docs/MCSM.md。
pub fn normalize_mcsm(raw: &str, path: &str) -> Result<String, String> {
    let s = raw.trim().to_uppercase();
    let parts: Vec<&str> = s.split('-').collect();
    let bad = || {
        format!(
            "{}: bad mcsm '{}' — expect F(1-4)-O(1-4)-P(1-4)-T(1-4), e.g. F(2)-O(1)-P(3)-T(2) (see docs/MCSM.md)",
            path, raw
        )
    };
    if parts.len() != 4 {
        return Err(bad());
    }
    let dims = [('F', 'O'), ('O', 'P'), ('P', 'T'), ('T', 'T')];
    let mut out = String::with_capacity(16);
    for (i, part) in parts.iter().enumerate() {
        let p = part.trim();
        let bytes = p.as_bytes();
        if bytes.len() != 4
            || bytes[0] != dims[i].0 as u8
            || bytes[1] != b'('
            || bytes[3] != b')'
            || !(b'1'..=b'4').contains(&bytes[2])
        {
            return Err(bad());
        }
        if i > 0 {
            out.push('-');
        }
        out.push_str(p);
    }
    Ok(out)
}

/// 从 `script(name, k=v, ...)` body 提取 (name, [(k, v), ...])。
/// 引号感知：值可含空格/逗号。首参数是无引号裸标识符或带引号字符串。
pub fn parse_script_body(body: &str) -> Result<(String, Vec<(String, String)>), String> {
    let open = body.find('(').ok_or("script body missing '('")?;
    let close = body.rfind(')').ok_or("script body missing closing ')'")?;
    if close < open {
        return Err("script body parens unbalanced".into());
    }
    let inner = &body[open + 1..close];

    // 引号感知 tokenizer（与 parser.rs measure 同款策略）
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quote: Option<char> = None;
    for c in inner.chars() {
        match in_quote {
            Some(q) => {
                if c == q {
                    in_quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => match c {
                '\'' | '"' => in_quote = Some(c),
                ',' => {
                    tokens.push(cur.trim().to_string());
                    cur = String::new();
                }
                _ => cur.push(c),
            },
        }
    }
    tokens.push(cur.trim().to_string());
    if in_quote.is_some() {
        return Err("script body has unbalanced quotes".into());
    }
    let tokens: Vec<String> = tokens.into_iter().filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
        return Err("script() requires a script name".into());
    }

    let name = tokens[0].clone();
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!("invalid script name '{}' in body", name));
    }

    let mut args = Vec::new();
    for tok in &tokens[1..] {
        if let Some(eq) = tok.find('=') {
            args.push((
                tok[..eq].trim().to_string(),
                tok[eq + 1..].trim().to_string(),
            ));
        } else {
            return Err(format!(
                "script arg '{}' is not k=v — script(name, key=value, ...)",
                tok
            ));
        }
    }
    Ok((name, args))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"#!/usr/bin/env python3
# ductile: v1
# name: resize_image
# desc: 等比缩放图片
# lang: python
# params: src(path, required), width(int, default=800)
# output: out(path), bytes(int)
# pure: false
# idempotent: true
# concurrency: safe
# effects: fs
# timeout: 60
print("hi")
"#;

    #[test]
    fn parse_good_contract() {
        let card = parse_contract(GOOD, "/tmp/resize.py").unwrap();
        assert_eq!(card.name, "resize_image");
        assert_eq!(card.lang, "python");
        assert!(!card.pure);
        assert!(card.idempotent);
        assert_eq!(card.concurrency, Concurrency::Safe);
        assert_eq!(card.timeout_secs, 60);
        assert_eq!(card.retries, 0);
        assert!(!cse_safe(&card)); // pure=false → 不许 CSE
        assert_eq!(card.mcsm, ""); // 未声明 mcsm → 空（可选键）
    }

    #[test]
    fn mcsm_field_parsed_and_normalized() {
        // v0.18.11：mcsm 契约键 — F-O-P-T 认知坐标进卡
        let src = GOOD.replace("# timeout: 60", "# timeout: 60\n# mcsm: F(2)-o(1)-P(3)-t(4)");
        let card = parse_contract(&src, "/tmp/x.py").unwrap();
        assert_eq!(card.mcsm, "F(2)-O(1)-P(3)-T(4)", "小写规范化大写");
        // 非法坐标 fail-closed
        for bad in ["F(9)-O(1)-P(3)-T(4)", "f(2)-x(1)-P(3)-T(4)", "F(2)-O(1)-P(3)", "F(2)O(1)P(3)T(4)"] {
            let src = GOOD.replace("# timeout: 60", format!("# timeout: 60\n# mcsm: {bad}").as_str());
            assert!(parse_contract(&src, "/tmp/x.py").is_err(), "应拒绝: {bad}");
        }
    }

    #[test]
    fn pure_safe_script_allows_cse() {
        let src = GOOD.replace("pure: false", "pure: true");
        let card = parse_contract(&src, "/tmp/x.py").unwrap();
        assert!(cse_safe(&card));
    }

    #[test]
    fn missing_keys_listed_at_once() {
        let src = "# ductile: v1\n# name: x\nprint(1)\n";
        let err = parse_contract(src, "/tmp/x.py").unwrap_err();
        assert!(err.contains("missing required keys"), "{}", err);
        assert!(err.contains("desc") && err.contains("lang"), "{}", err);
    }

    #[test]
    fn no_header_is_hard_error() {
        let err = parse_contract("print(1)\n", "/tmp/x.py").unwrap_err();
        assert!(err.contains("no contract header"));
    }

    #[test]
    fn unknown_lang_and_concurrency_rejected() {
        let bad = GOOD.replace("lang: python", "lang: cobol");
        assert!(parse_contract(&bad, "/x").is_err());
        let bad2 = GOOD.replace("concurrency: safe", "concurrency: whatever");
        assert!(parse_contract(&bad2, "/x").is_err());
    }

    #[test]
    fn bad_bool_rejected() {
        let bad = GOOD.replace("pure: false", "pure: yes");
        let err = parse_contract(&bad, "/x").unwrap_err();
        assert!(err.contains("must be true/false"));
    }

    #[test]
    fn script_body_tokenizer() {
        let (name, args) =
            parse_script_body("script(resize, src=\"a b.png\", width=1024)").unwrap();
        assert_eq!(name, "resize");
        assert_eq!(args[0], ("src".into(), "a b.png".into()));
        assert_eq!(args[1], ("width".into(), "1024".into()));
    }

    #[test]
    fn script_body_unbalanced_quotes_hard_error() {
        assert!(parse_script_body("script(x, a=\"unclosed)").is_err());
        assert!(parse_script_body("script x").is_err());
    }

    #[test]
    fn interpreter_map() {
        assert_eq!(lang_interpreter("python").unwrap(), "python3");
        assert_eq!(lang_interpreter("bash").unwrap(), "bash");
        assert_eq!(lang_interpreter("powershell").unwrap(), "pwsh");
        assert!(lang_interpreter("cobol").is_err());
    }
}
