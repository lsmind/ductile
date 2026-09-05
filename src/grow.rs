//! Grow — native MDL construction growth (v0.9.2, learn v2 substrate).
//!
//! V26 物理的 Rust 移植: 在命令全文语料上做行级 greedy pair-merge,
//! 增益门 > 0 (与 promote.rs / V26 [L2G] 同账法):
//!   gain = c·(occ_a + occ_b − occ_ab) − (8·bytes(ab) + 32)
//! 长出的多行构式入 scaffolds 表 (source='grown').
//!
//! 物理依据: 脚手架住在行间 (V26: top-10 构式 10/10 多行, ratio 0.0311),
//! 首行归一化 (harvest v1) 恰好毁掉它们.

use crate::db;
use crate::harvest;
use rusqlite::{params, Connection};
use std::collections::HashMap;

pub struct GrowthReport {
    pub calls: usize,
    pub distinct: usize,
    pub bits_raw: f64,
    pub bits_tp: f64,
    pub rounds: usize,
    pub imported: usize,
}

struct Interner {
    map: HashMap<String, u32>,
    strings: Vec<String>,
}

impl Interner {
    fn new() -> Self {
        Interner {
            map: HashMap::new(),
            strings: Vec::new(),
        }
    }
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = self.strings.len() as u32;
        self.strings.push(s.to_string());
        self.map.insert(s.to_string(), id);
        id
    }
    fn get(&self, id: u32) -> &str {
        &self.strings[id as usize]
    }
    fn len(&self) -> usize {
        self.strings.len()
    }
}

/// 全语料生长. 返回报告; 构式写 scaffolds (source='grown').
pub fn grow(days: u32, top_import: usize) -> Result<GrowthReport, String> {
    // ── 1. 全文命令计数 (不做首行归一化) ──
    let cmds = harvest::harvest_full_counts(days)?;
    let calls: usize = cmds.values().map(|c| c.calls as usize).sum();
    let distinct = cmds.len();
    if calls < 200 {
        return Err(format!("corpus too small: {calls} calls in {days}d"));
    }

    // ── 2. 行级 token 语料 + interning ──
    let mut ir = Interner::new();
    let sentinel = ir.intern("\u{0}");
    let mut seqs: Vec<Vec<u32>> = Vec::with_capacity(calls);
    let mut bits_raw = 0f64;
    for (cmd, c) in &cmds {
        let lines: Vec<&str> = cmd.split('\n').filter(|l| !l.trim().is_empty()).collect();
        if lines.is_empty() {
            continue;
        }
        for l in &lines {
            bits_raw += 8.0 * l.len() as f64 * c.calls as f64;
        }
        for _ in 0..c.calls {
            let mut s: Vec<u32> = lines.iter().map(|l| ir.intern(l)).collect();
            s.push(sentinel);
            seqs.push(s);
        }
    }

    // ── 3. greedy pair-merge, 增益门 > 0 ──
    let mut dict_bits = 0f64;
    let mut rounds = 0usize;
    loop {
        let mut freq: HashMap<u32, f64> = HashMap::new();
        for s in &seqs {
            for &t in s {
                *freq.entry(t).or_insert(0.0) += 1.0;
            }
        }
        let n_total: f64 = freq.values().sum();
        let mut pairs: HashMap<(u32, u32), f64> = HashMap::new();
        for s in &seqs {
            for w in s.windows(2) {
                if w[0] == sentinel || w[1] == sentinel {
                    continue;
                }
                *pairs.entry((w[0], w[1])).or_insert(0.0) += 1.0;
            }
        }
        let occ = |c: f64| -((c / n_total).log2());
        let mut cands: Vec<(f64, u32, u32)> = Vec::new();
        for ((a, b), c) in &pairs {
            let merged = format!("{}\n{}", ir.get(*a), ir.get(*b));
            let gain =
                c * (occ(freq[a]) + occ(freq[b]) - occ(*c)) - (8.0 * merged.len() as f64 + 32.0);
            if gain > 0.0 {
                cands.push((gain, *a, *b));
            }
        }
        if cands.is_empty() || rounds >= 60 {
            break;
        }
        cands.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));
        cands.truncate(100);
        let mut merge_map: HashMap<(u32, u32), u32> = HashMap::new();
        for (_, a, b) in &cands {
            let text = format!("{}\n{}", ir.get(*a), ir.get(*b));
            dict_bits += 8.0 * text.len() as f64 + 32.0;
            let id = ir.intern(&text);
            merge_map.insert((*a, *b), id);
        }
        for s in seqs.iter_mut() {
            let mut out: Vec<u32> = Vec::with_capacity(s.len());
            let mut i = 0;
            while i < s.len() {
                if i + 1 < s.len() && s[i] != sentinel && s[i + 1] != sentinel {
                    if let Some(&m) = merge_map.get(&(s[i], s[i + 1])) {
                        out.push(m);
                        i += 2;
                        continue;
                    }
                }
                out.push(s[i]);
                i += 1;
            }
            *s = out;
        }
        rounds += 1;
    }

    // ── 4. 终态: 多行构式按节省排序 → scaffolds ──
    let mut freq: HashMap<u32, f64> = HashMap::new();
    for s in &seqs {
        for &t in s {
            *freq.entry(t).or_insert(0.0) += 1.0;
        }
    }
    let n_total: f64 = freq.values().sum();
    let mut savings: Vec<(f64, f64, u32)> = Vec::new(); // (save_b, count, id)
    for (&id, &c) in &freq {
        let text = ir.get(id);
        if text.contains('\n') {
            let raw_u = 8.0 * text.len() as f64;
            let occ_u = -((c / n_total).log2());
            let sav = c * (raw_u - occ_u);
            if sav > 0.0 {
                savings.push((sav, c, id));
            }
        }
    }
    savings.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));

    let bits_occ: f64 = freq
        .iter()
        .map(|(&id, &c)| {
            let occ_u = -((c / n_total).log2());
            c * occ_u
        })
        .sum();
    let bits_tp = bits_occ + dict_bits;

    let mut conn = db_open()?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS scaffolds (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            text TEXT NOT NULL, save_b INTEGER, use_count INTEGER, lines INTEGER,
            source TEXT DEFAULT 'grown', imported_at TEXT,
            UNIQUE(text))",
        [],
    )
    .map_err(|e| e.to_string())?;
    let mut imported = 0usize;
    let mut junk_filtered = 0usize;
    for (sav, c, id) in savings.iter().take(top_import) {
        let text = ir.get(*id);
        if is_junk_construct(text) {
            junk_filtered += 1;
            continue;
        }
        let n_lines = text.split('\n').count() as i64;
        conn.execute(
            "INSERT OR IGNORE INTO scaffolds (text, save_b, use_count, lines, source, imported_at)
             VALUES (?1, ?2, ?3, ?4, 'grown', datetime('now'))",
            params![text, *sav as i64, *c as i64, n_lines],
        )
        .map_err(|e| e.to_string())?;
        imported += 1;
    }
    let _ = junk_filtered; // 报告字段留待 GrowthReport 扩展; 先行为对齐 Python 守门
    Ok(GrowthReport {
        calls,
        distinct,
        bits_raw,
        bits_tp,
        rounds,
        imported,
    })
}

/// 杂质构式守门 (2026-09-01 effect_flow 真发现, 与 Python effect_flow.py 同规则):
/// grow 的语料清洗会放进三类杂质 — 裸终止符尾(print(...)\nPY)、泛型内联开头
/// (python3 - <<PY)、全短行. 它们是"语法模板"不是"工作流构式", 对新语料的
/// 命中是假效应 (任何 python 内联都命中). 三规则与 exp/effect_flow.py 逐条对齐.
fn is_junk_construct(text: &str) -> bool {
    // 1. 任一行是裸 heredoc 终止符 (PY/PYEOF/EOF/END)
    for line in text.lines() {
        let t = line.trim();
        if t == "PY" || t == "PYEOF" || t == "EOF" || t == "END" || t == "PYEOF'" {
            return true;
        }
    }
    // 2. 首行是泛型内联开头 (python3 - <<..., python3 -c "...")
    let first = text.lines().next().unwrap_or("").trim_start();
    if first.starts_with("python3 - <<")
        || first.starts_with("python - <<")
        || first.starts_with("python3 -c '")
        || first.starts_with("python3 -c \"")
        || first == "python3"
        || first == "python"
    {
        return true;
    }
    // 3. 全短行: 没有一行 ≥20 字符 → 无实质内容
    let has_substance = text.lines().any(|l| l.trim().len() >= 20);
    if !has_substance {
        return true;
    }
    false
}

fn db_open() -> Result<Connection, String> {
    db::init_db();
    Connection::open(db::db_path()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod junk_gate_tests {
    use super::is_junk_construct;

    #[test]
    fn terminator_tail_is_junk() {
        assert!(is_junk_construct("print(\"patched\")\nPY"));
        assert!(is_junk_construct("echo hi\nPYEOF"));
    }
    #[test]
    fn generic_opener_is_junk() {
        assert!(is_junk_construct(
            "python3 - <<'PY'\nimport json\nreal content line here ok"
        ));
        assert!(is_junk_construct(
            "python3 -c \"\nimport json\nreal content line here ok"
        ));
    }
    #[test]
    fn short_lines_all_junk() {
        assert!(is_junk_construct("a = 1\nb = 2\nc = 3"));
    }
    #[test]
    fn real_construct_passes() {
        assert!(!is_junk_construct("import pathlib\nimport json\nd = json.loads(pathlib.Path('ledger.json').read_text())  # workflow"));
        assert!(!is_junk_construct(
            "const { chromium } = require('playwright');\n(async () => {"
        ));
    }
    #[test]
    fn py_inside_code_line_not_junk() {
        // "PY" 出现在行中但不是裸终止符行 → 不误杀
        assert!(!is_junk_construct(
            "import PYutils  # PY in middle of a real line here"
        ));
    }
}
