//! Promote — MDL 晋升门 (v0.9.0): harvest 候选 → 两部码检验 → 自动铸成 proc.
//!
//! KRC 物理移植 (V9b 同款): 候选命令被提升为库构件当且仅当两部码更省 —
//!   晋升成本 L_new = 字面(UTF-8) + 32b 选择码
//!   每次引用节省 S = (该命令的平均长度) − log2(词表增长后的引用码)
//!   净收益 G = count × S − L_new > 0 才晋升.
//! 防伪闸: 剥离已知前缀模式 (cd X && …) 只对可变残差计码; 单例永不晋升.
//! 账本: promotions 表 (可审计可回滚).

use crate::db;
use crate::harvest::{self, HarvestHit};
use rusqlite::{params, Connection};

/// 查询已铸成的构式模板 (来自 V26 生长).
pub fn list_scaffolds(q: &str) -> Result<Vec<(String, i64, i64, i64)>, String> {
    let conn = db_open()?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS scaffolds (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            text TEXT NOT NULL, save_b INTEGER, use_count INTEGER, lines INTEGER,
            source TEXT DEFAULT 'v26', imported_at TEXT,
            UNIQUE(text))",
        [],
    )
    .map_err(|e| e.to_string())?;
    let pat = format!("%{}%", q);
    let mut stmt = conn
        .prepare("SELECT text, save_b, use_count, lines FROM scaffolds WHERE text LIKE ?1 ORDER BY save_b DESC LIMIT 20")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![pat], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    Ok(rows)
}

pub struct PromotionVerdict {
    pub cmd: String,
    pub count: usize,
    pub lit_bits: f64,
    pub saving_per_use_bits: f64,
    pub gain_bits: f64,
    pub promoted: bool,
}

/// MDL gate: 一条候选命令的两部码裁决.
/// literal_bits = 8×UTF-8 字节 + 32 (选择码); ref_bits = log2(V+1) 当前词表引用成本.
pub fn mdl_gate(cmd: &str, count: usize, vocab_size: usize) -> PromotionVerdict {
    let lit_bits = 8.0 * cmd.len() as f64 + 32.0;
    let ref_bits = (vocab_size as f64 + 1.0).log2();
    // 引用节省: 命令文本本身不再逐字编码, 只付引用码 — 节省 ≈ 8×len − ref_bits
    let saving_per_use = 8.0 * cmd.len() as f64 - ref_bits;
    let gain = count as f64 * saving_per_use - lit_bits;
    PromotionVerdict {
        cmd: cmd.to_string(),
        count,
        lit_bits,
        saving_per_use_bits: saving_per_use,
        gain_bits: gain,
        promoted: gain > 0.0 && count >= 2,
    }
}

/// 剥离已知前缀模式, 只对残差计码 (防 "cd X && Y" 的 X 部分吃掉增益).
/// 残差 = 去掉第一个 "cd <dir> && " 后的剩余 — 该前缀是会话脚手架不是知识.
pub fn residual(cmd: &str) -> &str {
    if let Some(rest) = cmd.strip_prefix("cd ") {
        if let Some(and) = rest.find(" && ") {
            return &rest[and + 4..];
        }
    }
    cmd
}

fn db_open() -> Result<Connection, String> {
    db::init_db();
    Connection::open(db::db_path()).map_err(|e| e.to_string())
}

/// 当前词表规模: wrapped_cmds 的 distinct tag + 已晋升 proc 名.
fn vocab_size(conn: &Connection) -> usize {
    let n_cmds: i64 = conn
        .query_row("SELECT COUNT(DISTINCT tag) FROM wrapped_cmds", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    let n_procs: i64 = conn
        .query_row("SELECT COUNT(*) FROM procs", [], |r| r.get(0))
        .unwrap_or(0);
    (n_cmds + n_procs).max(2) as usize
}

pub fn promote(days: u32, top: usize, dry_run: bool) -> Result<(usize, usize), String> {
    // v0.9.3: 跨会话判据 — 会话内重复=迭代(探索), 跨会话重复=复用(知识)
    let full = harvest::harvest_full_counts(days)?;
    let mut hits: Vec<HarvestHit> = full
        .into_iter()
        .filter(|(_, cc)| cc.sessions >= 2)
        .map(|(cmd, cc)| HarvestHit {
            count: cc.sessions as usize,
            cmd,
            last_seen: String::new(),
        })
        .collect();
    hits.sort_by(|a, b| b.count.cmp(&a.count));
    let mut conn = db_open()?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS promotions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tag TEXT NOT NULL, cmd TEXT NOT NULL,
            count INTEGER, gain_bits REAL, promoted_at TEXT,
            UNIQUE(tag, cmd)
        )",
        [],
    )
    .map_err(|e| e.to_string())?;

    // 已在库中的命令跳过 (重复晋升无意义)
    let known: std::collections::HashSet<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT cmd FROM wrapped_cmds UNION SELECT DISTINCT cmd FROM promotions",
            )
            .map_err(|e| e.to_string())?;
        let v = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .flatten()
            .collect();
        v
    };

    let mut promoted_n = 0usize;
    let mut evaluated = 0usize;
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut tag_used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let v0 = vocab_size(&conn);
    for h in hits.iter().take(top) {
        let res = residual(&h.cmd);
        if res.len() < 6 {
            continue; // 残差太短: 脚手架噪声
        }
        if h.cmd.contains('…') || res.contains('…') {
            continue; // 截断件不可重放 — 拒绝 (装置病①修复)
        }
        // 近重复键: 剥离分隔符差异 (&& vs ; vs 空格) 后的前48字符
        let norm_key: String = res
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '/' || *c == '.' || *c == '-' || *c == '_')
            .take(48)
            .collect();
        if seen_keys.contains(&norm_key) {
            continue; // 近重复变体 (装置病②修复)
        }
        if known.contains(&h.cmd) {
            continue;
        }
        evaluated += 1;
        seen_keys.insert(norm_key);
        let v = PromotionVerdict {
            cmd: h.cmd.clone(),
            ..mdl_gate(res, h.count, v0 + promoted_n)
        };
        let tag = auto_tag(res, &mut tag_used);
        println!(
            "  [{:>7.0}b {:+9.0}b net] x{:<3} #{} ← {}",
            v.lit_bits,
            v.gain_bits,
            v.count,
            tag,
            harvest::trunc_str(res, 58)
        );
        if v.promoted && !dry_run {
            // 铸成: _harvested pipeline 下的 proc + wrapped_cmds 入库 (不执行)
            let pid: i64 = conn
                .query_row(
                    "SELECT id FROM pipelines WHERE name='_harvested'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if pid == 0 {
                conn.execute(
                    "INSERT OR IGNORE INTO pipelines (name, description, source_file, imported_at) VALUES ('_harvested', 'MDL 自动晋升 (v0.9.0 promote)', NULL, datetime('now'))",
                    [],
                )
                .map_err(|e| e.to_string())?;
                let pid2: i64 = conn
                    .query_row(
                        "SELECT id FROM pipelines WHERE name='_harvested'",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                insert_proc(&mut conn, &tag, pid2, &h.cmd)?;
            } else {
                insert_proc(&mut conn, &tag, pid, &h.cmd)?;
            }
            conn.execute(
                "INSERT OR IGNORE INTO wrapped_cmds (tag, cmd, exit_code, recorded_at) VALUES (?1, ?2, NULL, datetime('now'))",
                params![tag, h.cmd],
            )
            .map_err(|e| e.to_string())?;
            conn.execute(
                "INSERT OR IGNORE INTO promotions (tag, cmd, count, gain_bits, promoted_at) VALUES (?1, ?2, ?3, ?4, datetime('now'))",
                params![tag, h.cmd, h.count as i64, v.gain_bits],
            )
            .map_err(|e| e.to_string())?;
            promoted_n += 1;
        }
    }
    Ok((promoted_n, evaluated))
}

fn insert_proc(conn: &mut Connection, tag: &str, pid: i64, cmd: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO procs (name, pipeline_id, description, tags, impl_count, is_deliver)
         SELECT ?1, ?2, ?3, ?4, 1, 0
         WHERE NOT EXISTS (SELECT 1 FROM procs WHERE name=?1 AND pipeline_id=?2)",
        params![
            tag,
            pid,
            format!("MDL promoted (+gain): {}", harvest::trunc_str(cmd, 50)),
            format!("{},harvest", tag)
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 从残差自动起 tag: 取第一个词法词 (≤18 字符, [a-z0-9-]).
fn auto_tag(res: &str, used: &mut std::collections::HashSet<String>) -> String {
    let lowered = res.to_lowercase();
    let word: String = lowered
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let mut t = if word.is_empty() || word.len() < 3 {
        "auto".to_string()
    } else {
        word.chars().take(18).collect::<String>()
    };
    // 防撞名: 传入已用集, 撞则加序号
    while used.contains(&t) {
        t = format!(
            "{}-{}",
            word.chars().take(14).collect::<String>(),
            used.len() + 1
        );
        if t.len() > 20 {
            t.truncate(20);
        }
    }
    used.insert(t.clone());
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdl_gate_singleton_never_promotes() {
        let v = mdl_gate("echo hello world this is long enough", 1, 10);
        assert!(!v.promoted, "count=1 must not promote");
    }

    #[test]
    fn mdl_gate_frequent_short_cmd_may_promote() {
        // 长命令 × 多次引用：增益应为正
        let cmd = "cargo test --release --lib 2>&1 | grep test.result";
        let v = mdl_gate(cmd, 5, 8);
        assert!(v.gain_bits > 0.0, "gain={}", v.gain_bits);
        assert!(v.promoted);
    }

    #[test]
    fn residual_strips_cd_prefix() {
        assert_eq!(residual("cd /tmp && ls -la"), "ls -la");
        assert_eq!(residual("echo hi"), "echo hi");
        assert_eq!(residual("cd only"), "cd only");
    }

    #[test]
    fn auto_tag_from_residual_word() {
        let mut used = std::collections::HashSet::new();
        assert_eq!(auto_tag("cargo test --lib", &mut used), "cargo");
        // 撞名加序号
        let t2 = auto_tag("cargo build", &mut used);
        assert!(t2.starts_with("cargo"), "{}", t2);
        assert_ne!(t2, "cargo");
    }
}

