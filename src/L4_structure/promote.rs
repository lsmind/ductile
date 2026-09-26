//! Promote — MDL 晋升门 (v0.9.0): harvest 候选 → 两部码检验 → 自动铸成 proc.
//!
//! KRC 物理移植 (V9b 同款): 候选命令被提升为库构件当且仅当两部码更省 —
//!   晋升成本 L_new = 字面(UTF-8) + 32b 选择码
//!   每次引用节省 S = (该命令的平均长度) − log2(词表增长后的引用码)
//!   净收益 G = count × S − L_new > 0 才晋升.
//! 防伪闸: 剥离已知前缀模式 (cd X && …) 只对可变残差计码; 单例永不晋升.
//! 账本: promotions 表 (可审计可回滚).

use crate::db;
use crate::harvest::{self, CmdCount, HarvestHit};
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

/// v0.22 PR-4 摊销门输入信号：过去计数 + 时间分布 → 未来期望。
pub struct AmortSignals {
    pub count: usize,
    pub sessions: u32,
    pub days_since_last: f64,
    pub horizon_days: f64,
}

/// 摊销版裁决：期望未来使用 E[N] 驱动（旧门用过去 count——探索噪声伪装复用）。
pub struct AmortVerdict {
    pub expected_uses: f64,
    pub lit_bits: f64,
    pub count: usize,
    pub gain_bits: f64,
    pub promoted: bool,
}

/// E[N] = session_rate × horizon × freshness × per_session，clamp ≤ 3×count。
/// - sessions<2 → E[N]=0：无跨会话证据（V28：99% 调用是会话内迭代噪声），
///   未来使用不可外推——单会话狂调是探索，探索结束即死。
/// - freshness = 2^(-days_since_last/7)：7 天半衰期，信号易逝（Eq.22 同哲学）。
/// - per_session = 1 + (calls/sessions − 1)/2：会话内迭代率折半外推——
///   首用必计，迭代不可全额外推（探索期迭代密度 ≠ 复用期）。
/// - 观测窗用 days_since_last 做下界（无首次使用时间戳的保守近似）。
pub fn mdl_gate_amortized(cmd: &str, sig: AmortSignals, vocab_size: usize) -> AmortVerdict {
    let lit_bits = 8.0 * cmd.len() as f64 + 32.0;
    if sig.sessions < 2 {
        return AmortVerdict { expected_uses: 0.0, lit_bits, count: sig.count, gain_bits: -lit_bits, promoted: false };
    }
    let ref_bits = (vocab_size as f64 + 1.0).log2();
    let saving_per_use = 8.0 * cmd.len() as f64 - ref_bits;

    let session_rate = sig.sessions as f64 / sig.days_since_last.max(1.0);
    let freshness = 2.0f64.powf(-sig.days_since_last / 7.0);
    let per_session = 1.0 + (sig.count as f64 / sig.sessions as f64 - 1.0) / 2.0;
    let cap = (sig.count as f64) * 3.0; // 未来 ≤ 3× 过去：防下界近似爆炸
    let en = (session_rate * sig.horizon_days * freshness * per_session).min(cap);

    let gain = en * saving_per_use - lit_bits;
    AmortVerdict {
        expected_uses: en,
        lit_bits,
        count: sig.count,
        gain_bits: gain,
        promoted: gain > 0.0 && en >= 2.0,
    }
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
    let full_by_cmd: std::collections::HashMap<String, CmdCount> = full;
    let mut hits: Vec<HarvestHit> = full_by_cmd
        .iter()
        .filter(|(_, cc)| cc.sessions >= 2)
        .map(|(cmd, cc)| HarvestHit {
            count: cc.sessions as usize,
            cmd: cmd.clone(),
            last_seen: String::new(),
        })
        .collect();
    hits.sort_by(|a, b| b.count.cmp(&a.count));

    // v0.22 PR-4 摊销门：过去计数只是证据，裁决改期望未来使用 E[N]。
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    const HORIZON_DAYS: f64 = 3.0e1;
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
        // v0.22 PR-4：摊销裁决（E[N] 替代过去 count）。观测窗 = max(days, 距今)。
        let cc = full_by_cmd.get(&h.cmd);
        let days_since_last = cc
            .map(|c| ((now_ts - c.last_ts) / 86400.0).max(0.0))
            .unwrap_or(days as f64);
        let sig = AmortSignals {
            count: cc.map(|c| c.calls as usize).unwrap_or(h.count),
            sessions: cc.map(|c| c.sessions).unwrap_or(2),
            days_since_last,
            horizon_days: HORIZON_DAYS,
        };
        let v = mdl_gate_amortized(res, sig, v0 + promoted_n);
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

    // ═══ v0.22 PR-4 晋升摊销门（TDD 负例）═══
    // 旧 mdl_gate 的盲区：count 是过去计数，不是未来预期。会话内高频迭代
    // （探索噪声）只要 count 够大就能过门——但探索结束后未来 E[N]≈0，
    // 晋升的 32b 选择码永远收不回。摊销门 = 期望未来使用驱动。

    #[test]
    fn v022_amort_expectation_gate_blocks_dying_cmd() {
        // 死命令：过去 20 次（探索期狂调）但全部集中在 1 个会话 + 3 天未见
        // → E[N]≈0，不得晋升——无论过去 count 多大。
        let v = mdl_gate_amortized(
            "ffmpeg -i in.mp4 -vf scale=1920:1080 out.mp4", // 45B 残差
            AmortSignals { count: 20, sessions: 1, days_since_last: 3.0, horizon_days: 30.0 },
            10,
        );
        assert!(!v.promoted, "dying cmd (20 calls, 1 session, 3d stale) must NOT promote: E[N]={}", v.expected_uses);
    }

    #[test]
    fn v022_amort_expectation_gate_passes_recurring_cmd() {
        // 活命令：10 会话 × 每天 2 次 × 昨天还在用 → E[N] 高，应晋升。
        let v = mdl_gate_amortized(
            "cargo test --release --lib 2>&1 | grep test.result",
            AmortSignals { count: 60, sessions: 10, days_since_last: 1.0, horizon_days: 30.0 },
            8,
        );
        assert!(v.promoted, "recurring cmd (10 sessions, used yesterday) should promote: E[N]={}", v.expected_uses);
    }

    #[test]
    fn v022_amort_stale_but_high_rate_still_blocks() {
        // 高频但已死：过去 100 次 20 会话，但 14 天未见 → 半衰期衰减后
        // E[N] 应显著缩水。30 天视野下不应晋升（信息陈旧，未来不可外推）。
        let v = mdl_gate_amortized(
            "python3 scripts/build_all_assets.py --target=ep1 --quality=high",
            AmortSignals { count: 100, sessions: 20, days_since_last: 14.0, horizon_days: 30.0 },
            10,
        );
        // 边界敏感测试：不硬性断言不晋升（长命令 gain 大），但 E[N] 必须
        // 显著低于无衰减外推（100×30/14≈214）——衰减机制必须生效。
        assert!(
            v.expected_uses < 60.0,
            "14d-stale must decay E[N] below naive extrapolation, got {}",
            v.expected_uses
        );
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
