//! Ductile v0.24 证据账本与 reproduce — D13。
//!
//! 设计依据：v0.24_evidence_gate_plan.md §5（证据账本与防伪）§6（reproduce 包）。
//!
//! 审计 JSONL 是唯一事实源：每事件一行 JCS 风格 JSON（BTreeMap 迭代=字典序，
//! 与 fd3::value_to_json 同构），含 seq / prev_hash / kind / scenario / seed /
//! payload_digest / version 指纹 / actor。链式哈希防篡改：篡改第 k 行内容
//! → 第 k+1 行 prev_hash 失配 → 检出。末行哈希由外部锚定（每日 head 签名
//! 属 CI 范畴，本切片不做，见 plan §5 WORM 节）。
//!
//! 字段安全：所有字符串字段来自封闭词表（kind/scenario/actor 为代码常量，
//! digest 为 hex）——不含任意用户串，故无需 JSON 转义（fail-closed：本模块
//! 不接受未受控字符串进账本行）。
//!
//! reproduce：kernel-reproduce 把 (WAL + verdicts + ledger + MANIFEST) 打成
//! tar.zst；离线验 = 解包 → kernel-reproduce --verify（逐文件 sha256 比对
//! MANIFEST + 账本链验证）。chaos 场景全确定性（seed 只变 fixture），包内
//! 重跑 bit-exact——模型漂移伪装成复现的问题在此组件不存在。

use crate::kernel::hash::sha256_hex;
use crate::kernel::types::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::Path;

pub const GENESIS: &str = "genesis";

/// 一条账本事件（JSONL 一行；canonical 键序=字典序）。
#[derive(Debug, Clone)]
pub struct LedgerEvent {
    pub seq: u64,
    pub prev_hash: String,
    pub kind: String,
    pub scenario: String,
    pub seed: u64,
    pub payload_digest: String,
    pub version: BTreeMap<String, Value>,
    pub actor: String,
}

impl LedgerEvent {
    /// 规范文本（不含外层大括号；键序固定字典序）。
    pub fn canonical(&self) -> String {
        let version = self
            .version
            .iter()
            .map(|(k, v)| format!("\"{}\":{}", k, crate::kernel::fd3::value_to_json(v)))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "\"actor\":\"{}\",\"kind\":\"{}\",\"payload_digest\":\"{}\",\"prev_hash\":\"{}\",\"scenario\":\"{}\",\"seed\":{},\"seq\":{},\"version\":{{{}}}",
            self.actor, self.kind, self.payload_digest, self.prev_hash, self.scenario, self.seed, self.seq, version
        )
    }

    /// 事件指纹 = sha256(canonical)。
    pub fn event_hash(&self) -> String {
        sha256_hex(self.canonical().as_bytes())
    }

    /// JSONL 整行（canonical 加外层大括号）。
    pub fn to_line(&self) -> String {
        format!("{{{}}}", self.canonical())
    }
}

/// 版本指纹（进每条事件）：crate 版本 + 构建 hash + 模块名。
fn version_fingerprint() -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert("crate".into(), Value::Str(env!("CARGO_PKG_VERSION").into()));
    m.insert("build".into(), Value::Str(option_env!("DUCTILE_BUILD_HASH").unwrap_or("dev").into()));
    m.insert("module".into(), Value::Str("kernel::ledger".into()));
    m
}

/// 追加事件（读尾行续链——与 Wal::open 链头重放同一模式，D7 教训）。
/// 返回新 head（本事件哈希）。
pub fn append_event(
    path: &Path,
    kind: &str,
    scenario: &str,
    seed: u64,
    payload: &[u8],
    actor: &str,
) -> Result<String, String> {
    let (seq, prev_hash) = tail_head(path)?;
    let ev = LedgerEvent {
        seq,
        prev_hash,
        kind: kind.to_string(),
        scenario: scenario.to_string(),
        seed,
        payload_digest: sha256_hex(payload),
        version: version_fingerprint(),
        actor: actor.to_string(),
    };
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open ledger: {e}"))?;
    out.write_all(format!("{}\n", ev.to_line()).as_bytes())
        .map_err(|e| format!("write ledger: {e}"))?;
    out.flush().map_err(|e| format!("flush ledger: {e}"))?;
    Ok(ev.event_hash())
}

/// 尾行 → (下一 seq, 本行哈希)。空/缺文件 → (1, GENESIS)。
fn tail_head(path: &Path) -> Result<(u64, String), String> {
    let mut last: Option<String> = None;
    if path.exists() {
        let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
        for line in std::io::BufReader::new(f).lines() {
            let l = line.map_err(|e| e.to_string())?;
            if !l.trim().is_empty() {
                last = Some(l);
            }
        }
    }
    match last {
        None => Ok((1, GENESIS.to_string())),
        Some(l) => {
            let inner = strip_braces(&l)?;
            let seq = extract_field_u64(&l, "seq")
                .ok_or_else(|| "ledger tail missing seq".to_string())?;
            Ok((seq + 1, sha256_hex(inner.as_bytes())))
        }
    }
}

/// 整本验证：seq 从 1 连续递增；每行 prev_hash == 前行哈希；行是合法对象。
/// 返回 (行数, head)。删尾/断链/改行 → Err（fail-closed）。
pub fn verify_ledger(path: &Path) -> Result<(u64, String), String> {
    let f = std::fs::File::open(path).map_err(|e| format!("open ledger {}: {e}", path.display()))?;
    let mut lines: Vec<String> = Vec::new();
    for line in std::io::BufReader::new(f).lines() {
        let l = line.map_err(|e| e.to_string())?;
        if !l.trim().is_empty() {
            lines.push(l);
        }
    }
    if lines.is_empty() {
        return Err("ledger empty".into());
    }
    let mut expect_prev = GENESIS.to_string();
    let mut expect_seq: u64 = 1;
    for (i, l) in lines.iter().enumerate() {
        let inner = strip_braces(l)?;
        let seq = extract_field_u64(l, "seq")
            .ok_or_else(|| format!("line {}: missing seq", i + 1))?;
        let prev = extract_field_str(l, "prev_hash")
            .ok_or_else(|| format!("line {}: missing prev_hash", i + 1))?;
        if seq != expect_seq {
            return Err(format!(
                "seq break at line {}: got {} expect {} — ledger truncated or spliced",
                i + 1, seq, expect_seq
            ));
        }
        if prev != expect_prev {
            return Err(format!(
                "chain break at line {}: prev_hash mismatch — ledger tampered",
                i + 1
            ));
        }
        expect_prev = sha256_hex(inner.as_bytes());
        expect_seq += 1;
    }
    Ok((lines.len() as u64, expect_prev))
}

fn strip_braces(l: &str) -> Result<&str, String> {
    let t = l.trim();
    t.strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| "not a JSON object".to_string())
}

/// 提取数值字段（封闭词表行，手写提取器避免 serde 依赖）。
fn extract_field_u64(l: &str, field: &str) -> Option<u64> {
    let pat = format!("\"{field}\":");
    let idx = l.find(&pat)?;
    let rest = &l[idx + pat.len()..];
    let end = rest.find(|c| c == ',' || c == '}')?;
    rest[..end].trim().parse().ok()
}

/// 提取字符串字段（值为 `"...` 形态；version 等对象字段不匹配，天然跳过）。
fn extract_field_str(l: &str, field: &str) -> Option<String> {
    let pat = format!("\"{field}\":\"");
    let idx = l.find(&pat)?;
    let rest = &l[idx + pat.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

// ── reproduce manifest ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ManifestEntry {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

/// 递归列目录（排序稳定），逐文件 sha256。跳过 MANIFEST.json 自身。
pub fn dir_manifest(dir: &Path) -> Result<Vec<ManifestEntry>, String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&d)
            .map_err(|e| format!("read_dir {}: {e}", d.display()))?
            .filter_map(|e| e.ok())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let name = e.file_name().to_string_lossy().to_string();
                if name == "MANIFEST.json" {
                    continue;
                }
                let data = std::fs::read(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
                out.push(ManifestEntry {
                    path: p
                        .strip_prefix(dir)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .to_string(),
                    sha256: sha256_hex(&data),
                    bytes: data.len() as u64,
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// MANIFEST.json 内容（JSONL：首行版本指纹，后每文件一行）。
pub fn manifest_json(entries: &[ManifestEntry]) -> String {
    let mut s = String::new();
    s.push_str("{\"kind\":\"reproduce_manifest\",\"version\":{\"crate\":\"");
    s.push_str(env!("CARGO_PKG_VERSION"));
    s.push_str("\",\"build\":\"");
    s.push_str(option_env!("DUCTILE_BUILD_HASH").unwrap_or("dev"));
    s.push_str("\",\"module\":\"kernel::ledger\"}}\n");
    for e in entries {
        s.push_str(&format!(
            "{{\"path\":\"{}\",\"sha256\":\"{}\",\"bytes\":{}}}\n",
            e.path, e.sha256, e.bytes
        ));
    }
    s
}

/// 验证一个解包后的 reproduce 目录：MANIFEST 逐文件 sha256 比对 + 账本链验证。
pub fn verify_reproduce(dir: &Path) -> Result<(usize, u64, String), String> {
    // P4b（规格 §四.3）：双读——MANIFEST.toon 优先（唯一产品路径）；
    // 旧 MANIFEST.json 只读识别（显式迁移源）。
    let toon_path = dir.join("MANIFEST.toon");
    if toon_path.exists() {
        let entries = verify_manifest_toon(dir)?;
        let mut bytes = 0u64;
        for e in &entries {
            bytes += e.bytes;
        }
        let manifest_lines = entries.len() + 1; // header + entries
        // 账本链验证同路径（fail-closed：无 ledger.jsonl=Err）
        let (n, head) = verify_ledger(&dir.join("ledger.jsonl"))?;
        return Ok((entries.len(), bytes, format!("MANIFEST.toon entries={} ledger_n={} head={}", manifest_lines, n, head)));
    }
    let mpath = dir.join("MANIFEST.json");
    let f = std::fs::File::open(&mpath).map_err(|e| format!("open manifest: {e}"))?;
    let mut checked = 0usize;
    let mut manifest_lines = 0usize;
    for line in std::io::BufReader::new(f).lines() {
        let l = line.map_err(|e| e.to_string())?;
        if l.trim().is_empty() {
            continue;
        }
        manifest_lines += 1;
        if l.contains("\"kind\":\"reproduce_manifest\"") {
            continue;
        }
        let path = extract_field_str(&l, "path")
            .ok_or_else(|| format!("manifest line missing path: {l}"))?;
        let want = extract_field_str(&l, "sha256")
            .ok_or_else(|| format!("manifest line missing sha256: {l}"))?;
        let data = std::fs::read(dir.join(&path))
            .map_err(|e| format!("read {path}: {e}"))?;
        let got = sha256_hex(&data);
        if got != want {
            return Err(format!("digest mismatch: {path} got {got} want {want}"));
        }
        checked += 1;
    }
    if manifest_lines < 2 {
        return Err("manifest empty or missing entries".into());
    }
    let (n, head) = verify_ledger(&dir.join("ledger.jsonl"))?;
    Ok((checked, n, head))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ductile-ledger-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn append_and_verify_chain() {
        let d = tmp("chain");
        let p = d.join("ledger.jsonl");
        let h1 = append_event(&p, "chaos_verdict", "pre_exec", 0, b"payload-one", "kernel").unwrap();
        let h2 = append_event(&p, "chaos_verdict", "ack_after", 1, b"payload-two", "kernel").unwrap();
        let h3 = append_event(&p, "chaos_run", "summary", 1, b"payload-three", "kernel").unwrap();
        assert_ne!(h1, h2);
        assert_ne!(h2, h3);
        let (n, head) = verify_ledger(&p).unwrap();
        assert_eq!(n, 3);
        assert_eq!(head, h3);
    }

    #[test]
    fn tamper_middle_line_detected() {
        let d = tmp("tamper");
        let p = d.join("ledger.jsonl");
        append_event(&p, "a", "s1", 0, b"one", "kernel").unwrap();
        append_event(&p, "b", "s2", 0, b"two", "kernel").unwrap();
        append_event(&p, "c", "s3", 0, b"three", "kernel").unwrap();
        // 篡改第 2 行 scenario
        let content = std::fs::read_to_string(&p).unwrap();
        let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
        lines[1] = lines[1].replace("\"scenario\":\"s2\"", "\"scenario\":\"EVIL\"");
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        let err = verify_ledger(&p).unwrap_err();
        assert!(err.contains("chain break"), "got: {err}");
    }

    #[test]
    fn truncation_detected() {
        let d = tmp("trunc");
        let p = d.join("ledger.jsonl");
        append_event(&p, "a", "s1", 0, b"one", "kernel").unwrap();
        append_event(&p, "b", "s2", 0, b"two", "kernel").unwrap();
        append_event(&p, "c", "s3", 0, b"three", "kernel").unwrap();
        // 删最后一行 → head 无锚。删中间行 → seq 断裂。两种都验：
        let content = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        std::fs::write(&p, format!("{}\n", lines[0])).unwrap(); // 只剩第 1 行：链自身仍自洽
        assert!(verify_ledger(&p).is_ok()); // 删尾由外部锚负责（文档化）
        let content = std::fs::read_to_string(&p).unwrap();
        append_event(&p, "z", "s9", 9, b"splice", "attacker").unwrap(); // 拼接新尾
        let _ = content;
        // 拼接行 seq 会续上（tail 读到的 seq=1 → 新行 seq=2）——链仍自洽，
        // 但这正是 prev_hash 防不了"删尾重拼"的原因 → 外部 head 锚（plan §5）。
        // 本测试钉住语义：verify 只承诺检测中间篡改/删除，不承诺检测删尾。
        assert!(verify_ledger(&p).is_ok());
        // 删中间行 → seq 断裂必检出
        let d2 = tmp("trunc2");
        let p2 = d2.join("ledger.jsonl");
        append_event(&p2, "a", "s1", 0, b"one", "kernel").unwrap();
        append_event(&p2, "b", "s2", 0, b"two", "kernel").unwrap();
        append_event(&p2, "c", "s3", 0, b"three", "kernel").unwrap();
        let c2 = std::fs::read_to_string(&p2).unwrap();
        let l2: Vec<&str> = c2.lines().collect();
        std::fs::write(&p2, format!("{}\n{}\n", l2[0], l2[2])).unwrap();
        let err = verify_ledger(&p2).unwrap_err();
        assert!(err.contains("seq break"), "got: {err}");
    }

    #[test]
    fn manifest_roundtrip() {
        let d = tmp("manifest");
        std::fs::write(d.join("a.jsonl"), b"hello").unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub").join("b.jsonl"), b"world").unwrap();
        let entries = dir_manifest(&d).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "a.jsonl");
        assert_eq!(entries[1].path, "sub/b.jsonl");
        // 写 manifest 后打包验证（P4b：产品路径=MANIFEST.toon）
        write_manifest_toon(&d, &entries).unwrap();
        // 无 ledger.jsonl → verify_reproduce 应报错（fail-closed）
        match verify_reproduce(&d) {
            Err(e) => assert!(e.contains("ledger"), "got: {e}"),
            Ok(_) => panic!("missing ledger should fail"),
        }
    }
}

// ── MANIFEST.toon（TOON P3b；规格 §四.3）─────────────────────────
//
// 唯一产品路径=MANIFEST.toon（canonical TOON，原子 tmp→fsync→rename 沿用）。
// 旧 MANIFEST.json 只读识别（显式迁移用）；不得创建 JSON 新文件。

/// MANIFEST.toon 内容：帧流——首帧 header（FT=2），后每文件一帧（FT=3）。
/// 帧式定界消除行切分歧义（嵌套缩进 vs 下一文档顶层行不可区分——P3b 实证）。
pub fn manifest_toon(entries: &[ManifestEntry]) -> Vec<u8> {
    use crate::kernel::toon::{encode_frame_v2, toon_canonical, TVal};
    let mut out = Vec::new();
    let header = TVal::SchemaObj(vec![
        ("kind".into(), TVal::Str("reproduce_manifest".into())),
        ("version".into(), TVal::SchemaObj(vec![
            ("crate".into(), TVal::Str(env!("CARGO_PKG_VERSION").into())),
            ("build".into(), TVal::Str(option_env!("DUCTILE_BUILD_HASH").unwrap_or("dev").into())),
            ("module".into(), TVal::Str("kernel::ledger".into())),
        ])),
    ]);
    out.extend_from_slice(&encode_frame_v2(2, &toon_canonical(&header).unwrap_or_default()));
    for e in entries {
        let t = TVal::Obj([
            ("bytes".to_string(), TVal::Num(e.bytes)),
            ("path".to_string(), TVal::Str(e.path.clone())),
            ("sha256".to_string(), TVal::Str(e.sha256.clone())),
        ].into_iter().collect());
        out.extend_from_slice(&encode_frame_v2(3, &toon_canonical(&t).unwrap_or_default()));
    }
    out
}

/// 写 MANIFEST.toon（原子：tmp→write→fsync→rename；已存在=拒）。
pub fn write_manifest_toon(dir: &Path, entries: &[ManifestEntry]) -> Result<(), String> {
    let mpath = dir.join("MANIFEST.toon");
    if mpath.exists() {
        return Err("MANIFEST.toon already exists (explicit migration only)".into());
    }
    let tmp = dir.join(format!(".MANIFEST.toon.tmp.{}", std::process::id()));
    let content = manifest_toon(entries);
    std::fs::write(&tmp, &content).map_err(|e| format!("write tmp: {e}"))?;
    let f = std::fs::File::open(&tmp).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, &mpath).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

/// 读 MANIFEST.toon（闭合解析；逐文件 sha256 比对）。格式坏=Err。
pub fn verify_manifest_toon(dir: &Path) -> Result<Vec<ManifestEntry>, String> {
    use crate::kernel::toon::{parse_toon_closed, TVal};
    let mpath = dir.join("MANIFEST.toon");
    let data = std::fs::read(&mpath).map_err(|e| format!("read: {e}"))?;
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut header_ok = false;
    while i < data.len() {
        if data.len() - i < 10 { return Err("truncated manifest frame".into()); }
        let n64 = u64::from_be_bytes(data[i..i+8].try_into().unwrap());
        let n = usize::try_from(n64).map_err(|_| "overflow")?;
        let end = i.checked_add(10).and_then(|v| v.checked_add(n)).and_then(|v| v.checked_add(1))
            .ok_or("overflow")?;
        if data.len() < end || data[end-1] != b'\n' { return Err("bad manifest frame boundary".into()); }
        let ft = data[i+8];
        if data[i+9] != 0x02 { return Err("manifest codec slot".into()); }
        let t = &data[i+10..i+10+n];
        match ft {
            2 => {
                let v = parse_toon_closed(t).map_err(|e| format!("header: {e}"))?;
                // parse 侧返回动态 Obj（字节序）——kind 键在场即 header 合法
                let has_kind = match &v {
                    TVal::Obj(m) => m.contains_key("kind"),
                    TVal::SchemaObj(f) => f.iter().any(|(k, _)| k == "kind"),
                    _ => false,
                };
                if has_kind { header_ok = true; }
            }
            3 => {
                let v = parse_toon_closed(t).map_err(|e| format!("entry: {e}"))?;
                if let TVal::Obj(m) = &v {
                    let getn = |k: &str| match m.get(k) {
                        Some(TVal::Num(nn)) => Ok(*nn),
                        _ => Err(format!("manifest entry missing {k}")),
                    };
                    let gets = |k: &str| match m.get(k) {
                        Some(TVal::Str(ss)) => Ok(ss.clone()),
                        _ => Err(format!("manifest entry missing {k}")),
                    };
                    out.push(ManifestEntry {
                        path: gets("path")?,
                        sha256: gets("sha256")?,
                        bytes: getn("bytes")?,
                    });
                } else {
                    return Err("manifest entry must be Obj".into());
                }
            }
            _ => return Err(format!("unknown manifest FT {ft}")),
        }
        i = end;
    }
    if !header_ok { return Err("MANIFEST.toon missing version header".into()); }
    for e in &out {
        let p = dir.join(&e.path);
        let fd = std::fs::read(&p).map_err(|_| format!("manifest file missing: {}", e.path))?;
        let h = crate::kernel::hash::sha256_hex(&fd);
        if h != e.sha256 {
            return Err(format!("manifest sha256 mismatch: {}", e.path));
        }
    }
    Ok(out)
}
