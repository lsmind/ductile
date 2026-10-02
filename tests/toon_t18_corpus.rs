// T18（规格 §五.6）：LLM 发布门的确定性烟测。
// CI 纪律：720 次实测是发布门（scripts/t18_run_gate.py 真调 LLM）；
// CI 只跑固定 8 卡 ×1 次、断言字节级 golden，不调 LLM——不得以抽样替代发布门。
//
// 本测试验证的确定性面：
// 1. 240 卡语料全部过 closed parser（字节级——逐卡 re-encode 相等由裁判保证）；
// 2. 8 类 ×30 张分层结构精确；
// 3. 每卡 schema/oracle 结构完整（expect_ 键集 == schema 键集）；
// 4. 语料确定性：种子重建两次哈希一致；
// 5. 运行器语法可用（py_compile）。

use std::collections::BTreeSet;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ductile")
}

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn corpus_dir() -> std::path::PathBuf {
    repo_root().join("tests/fixtures/t18_corpus")
}

fn judge(input: &str, schema: &str) -> Result<String, String> {
    use std::io::Write;
    let mut child = Command::new(bin())
        .args(["toon", "--schema", schema])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 从裁判 canonical 输出取顶层字段（k: v 行）。
fn fields_of(canonical: &str) -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();
    for line in canonical.lines() {
        if line.starts_with("  ") || line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(": ") {
            m.insert(k.to_string(), v.to_string());
        }
    }
    m
}

fn unquote(v: &str) -> &str {
    let v = v.trim();
    v.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(v)
}

#[test]
fn t18_corpus_all_240_parse_closed() {
    let dir = corpus_dir();
    let files: Vec<_> = std::fs::read_dir(&dir)
        .expect("t18_corpus dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "toon").unwrap_or(false))
        .collect();
    assert_eq!(files.len(), 240, "exactly 240 cards");
    let mut bad = Vec::new();
    for f in &files {
        let raw = std::fs::read_to_string(f).unwrap();
        // 语料键全集做白名单（class/card_id/schema/input + expect_*）
        let mut allow: Vec<&str> = vec!["class", "card_id", "schema", "input"];
        for l in raw.lines() {
            if let Some(rest) = l.strip_prefix("expect_") {
                let key = format!("expect_{}", rest.split(':').next().unwrap());
                allow.push(Box::leak(key.into_boxed_str()));
            }
        }
        if let Err(e) = judge(&raw, &allow.join(",")) {
            bad.push(format!("{}: {}", f.file_name().unwrap().to_string_lossy(), e));
        }
    }
    assert!(bad.is_empty(), "cards failing closed parse: {bad:#?}");
}

#[test]
fn t18_corpus_structure_8x30() {
    let dir = corpus_dir();
    let mut per_class: std::collections::BTreeMap<String, usize> = Default::default();
    for e in std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(cls) = name.split('-').next() {
            *per_class.entry(cls.to_string()).or_default() += 1;
        }
    }
    assert_eq!(per_class.len(), 8, "8 classes: {per_class:?}");
    for (cls, n) in &per_class {
        assert_eq!(*n, 30, "class {cls} must have 30 cards, got {n}");
    }
    // 类名与生成器一致（冻结）
    let expect: BTreeSet<&str> = [
        "script", "storyboard", "ledger", "audit", "subtitle", "task", "review", "release",
    ]
    .into_iter()
    .collect();
    let got: BTreeSet<&str> = per_class.keys().map(|s| s.as_str()).collect();
    assert_eq!(got, expect, "class set frozen");
}

#[test]
fn t18_card_schema_oracle_coherent() {
    // 每卡：expect_ 键集 == schema 键集（oracle 完备性）
    let dir = corpus_dir();
    for e in std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()) {
        let raw = std::fs::read_to_string(e.path()).unwrap();
        let allow: Vec<&str> = {
            let mut a: Vec<&str> = vec!["class", "card_id", "schema", "input"];
            for l in raw.lines() {
                if let Some(rest) = l.strip_prefix("expect_") {
                    let key = format!("expect_{}", rest.split(':').next().unwrap());
                    a.push(Box::leak(key.into_boxed_str()));
                }
            }
            a
        };
        let canon = judge(&raw, &allow.join(",")).expect("parse");
        let f = fields_of(&canon);
        // canonical 输出键序=字节序，schema 声明序可不同——断言集合相等
        let schema: BTreeSet<&str> = unquote(f["schema"].trim()).split(',').collect();
        let expects: BTreeSet<&str> = f
            .keys()
            .filter(|k| k.starts_with("expect_"))
            .map(|k| &k["expect_".len()..])
            .collect();
        assert_eq!(
            schema, expects,
            "card {:?}: schema keys must equal oracle keys",
            e.path()
        );
    }
}

#[test]
fn t18_corpus_deterministic_regeneration() {
    // 确定性：重新生成 → 全量哈希一致（种子冻结证明）
    let before: Vec<(String, u64)> = {
        let mut v = Vec::new();
        use std::hash::{Hash, Hasher};
        for e in std::fs::read_dir(corpus_dir()).unwrap().filter_map(|e| e.ok()) {
            let raw = std::fs::read(e.path()).unwrap();
            let mut h = std::collections::hash_map::DefaultHasher::new();
            raw.hash(&mut h);
            v.push((e.file_name().to_string_lossy().into_owned(), h.finish()));
        }
        v.sort();
        v
    };
    let out = Command::new("python3")
        .arg(repo_root().join("scripts/t18_gen_corpus.py"))
        .output()
        .expect("regen corpus");
    assert!(out.status.success(), "gen failed: {}", String::from_utf8_lossy(&out.stderr));
    let after: Vec<(String, u64)> = {
        let mut v = Vec::new();
        use std::hash::{Hash, Hasher};
        for e in std::fs::read_dir(corpus_dir()).unwrap().filter_map(|e| e.ok()) {
            let raw = std::fs::read(e.path()).unwrap();
            let mut h = std::collections::hash_map::DefaultHasher::new();
            raw.hash(&mut h);
            v.push((e.file_name().to_string_lossy().into_owned(), h.finish()));
        }
        v.sort();
        v
    };
    assert_eq!(before, after, "corpus must regenerate byte-identical (frozen seeds)");
}

#[test]
fn t18_gate_runner_syntax_ok() {
    // 运行器与生成器语法可用（发布门工具完整性）
    for script in ["scripts/t18_run_gate.py", "scripts/t18_gen_corpus.py"] {
        let out = Command::new("python3")
            .arg("-m")
            .arg("py_compile")
            .arg(repo_root().join(script))
            .output()
            .expect("py_compile");
        assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
    }
}
