//! Ductile config.toml — flat section parser (no serde/toml crate).
//!
//! Lookup order for file path:
//!   1. `$DUCTILE_CONFIG`
//!   2. `./config.toml` / `./ductile.toml`
//!   3. `$HOME/.config/ductile/config.toml`
//!   4. `$HOME/.local/share/ductile/config.toml` (and Windows USERPROFILE)
//!
//! Value precedence for LLM (applied by bridge + exec_llm):
//!   CLI / llm() args > process env > config.toml [llm] > built-in defaults

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
}

/// v0.16 llm agent：[agents.<name>] 段 → llm(<name>, ...) 裸首参引用。
/// model/system/schema/timeout 均可省（缺省回落 [llm] / 实参）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentConfig {
    pub model: String,
    pub system: String,
    pub schema: String,
    pub timeout_secs: u64,
    /// v0.16.1 智力阶梯：tiers = "light,medium,high"（升序）。空 = 旧单模型路径。
    pub tiers: Vec<String>,
    /// v0.17 auto-prompt：操作指南（开放动作/检索方式/示例说明），
    /// prompt 缺省合成时注入"# 可用动作与检索方式"段。
    pub guide: String,
}

/// v0.16.1 命名模型档：[models.<tier>] —— 档位是语义能力级（light/high），
/// 不是裸模型名。model 必填；base_url/api_key/timeout 可选（缺省回落 [llm]）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelTier {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub timeout_secs: u64,
    /// v0.17：思考型模型 token 预算（35B 思考烧 9k+ tokens 的实测教训）。
    /// 缺省 0 = 不注入（沿用环境变量 OPENAI_MAX_TOKENS）。
    pub max_tokens: u64,
}

/// 档位表：tier 名 → ModelTier。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TiersConfig {
    pub tiers: BTreeMap<String, ModelTier>,
}

/// agents 配置集合：name → AgentConfig（[agents.planner] 形态，点分嵌套展开）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentsConfig {
    pub agents: BTreeMap<String, AgentConfig>,
}

impl AgentsConfig {
    pub fn get(&self, name: &str) -> Option<&AgentConfig> {
        self.agents.get(name)
    }
}

impl LlmConfig {
    pub fn with_defaults(self) -> Self {
        LlmConfig {
            base_url: if self.base_url.is_empty() {
                "https://api.openai.com/v1".into()
            } else {
                self.base_url
            },
            api_key: self.api_key,
            model: if self.model.is_empty() {
                "gpt-4o-mini".into()
            } else {
                self.model
            },
            timeout_secs: if self.timeout_secs == 0 {
                120
            } else {
                self.timeout_secs
            },
        }
    }
}

/// Candidate config file paths (first existing wins via [`find_config_path`]).
pub fn config_search_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("DUCTILE_CONFIG") {
        let t = p.trim();
        if !t.is_empty() {
            out.push(PathBuf::from(t));
        }
    }
    out.push(PathBuf::from("config.toml"));
    out.push(PathBuf::from("ductile.toml"));
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    if !home.is_empty() {
        out.push(
            PathBuf::from(&home)
                .join(".config")
                .join("ductile")
                .join("config.toml"),
        );
        out.push(
            PathBuf::from(&home)
                .join(".local")
                .join("share")
                .join("ductile")
                .join("config.toml"),
        );
    }
    out
}

pub fn find_config_path() -> Option<PathBuf> {
    config_search_paths().into_iter().find(|p| p.is_file())
}

/// Parse a minimal TOML subset: `[section]` + `key = "str"|'str'|number|true|false`.
pub fn parse_toml_sections(text: &str) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current = String::new();
    for raw in text.lines() {
        let line = strip_toml_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current = line[1..line.len() - 1].trim().to_string();
            sections.entry(current.clone()).or_default();
            continue;
        }
        if let Some((k, v)) = split_toml_kv(&line) {
            sections.entry(current.clone()).or_default().insert(k, v);
        }
    }
    sections
}

fn strip_toml_comment(line: &str) -> String {
    // Only treat # outside quotes as comment.
    let mut out = String::new();
    let mut in_s = false;
    let mut in_d = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' && !in_s {
            in_d = !in_d;
            out.push(c);
            continue;
        }
        if c == '\'' && !in_d {
            in_s = !in_s;
            out.push(c);
            continue;
        }
        if c == '#' && !in_s && !in_d {
            break;
        }
        out.push(c);
    }
    out
}

fn split_toml_kv(line: &str) -> Option<(String, String)> {
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let raw_val = line[eq + 1..].trim();
    let val = parse_toml_value(raw_val)?;
    Some((key.to_string(), val))
}

fn parse_toml_value(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Some(String::new());
    }
    if (raw.starts_with('"') && raw.ends_with('"'))
        || (raw.starts_with('\'') && raw.ends_with('\''))
    {
        let inner = &raw[1..raw.len() - 1];
        // minimal unescape for \"
        return Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"));
    }
    // bare number / bool / ident
    Some(raw.to_string())
}

pub fn llm_from_sections(sections: &BTreeMap<String, BTreeMap<String, String>>) -> LlmConfig {
    let empty = BTreeMap::new();
    let llm = sections.get("llm").unwrap_or(&empty);
    let get = |k: &str| llm.get(k).cloned().unwrap_or_default();
    let timeout_secs = get("timeout_secs")
        .parse::<u64>()
        .or_else(|_| get("timeout").parse::<u64>())
        .unwrap_or(0);
    LlmConfig {
        base_url: first_nonempty(&[get("base_url"), get("api_base"), get("url")]),
        api_key: first_nonempty(&[get("api_key"), get("key")]),
        model: first_nonempty(&[get("model"), get("default_model")]),
        timeout_secs,
    }
    .with_defaults()
}

fn first_nonempty(vals: &[String]) -> String {
    vals.iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_default()
}

pub fn load_llm_config() -> LlmConfig {
    match find_config_path() {
        Some(path) => load_llm_config_from_path(&path)
            .unwrap_or_default()
            .with_defaults(),
        None => LlmConfig::default().with_defaults(),
    }
}

pub fn load_llm_config_from_path(path: &Path) -> Result<LlmConfig, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let sections = parse_toml_sections(&text);
    Ok(llm_from_sections(&sections))
}

/// v0.16 [agents.<name>] 段 → AgentsConfig（[agents.planner] 点分形态）。
/// 字段同 [llm] 的 agent 子集：model/system/schema/timeout_secs（system/schema
/// 内联 \n 解析时展开为真换行）。
/// v0.16.1 新增：tiers = "light,medium,high"（逗号分隔升序阶梯）。
pub fn agents_from_sections(sections: &BTreeMap<String, BTreeMap<String, String>>) -> AgentsConfig {
    let mut out = AgentsConfig::default();
    for (sec, kv) in sections {
        let Some(name) = sec.strip_prefix("agents.") else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let get = |k: &str| kv.get(k).cloned().unwrap_or_default();
        let agent = AgentConfig {
            model: get("model"),
            system: get("system").replace("\\n", "\n"),
            schema: get("schema"),
            timeout_secs: get("timeout_secs").parse().unwrap_or(0),
            tiers: get("tiers")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            guide: get("guide").replace("\\n", "\n"),
        };
        out.agents.insert(name.to_string(), agent);
    }
    out
}

pub fn load_agents_config() -> AgentsConfig {
    match find_config_path() {
        Some(path) => match fs::read_to_string(&path) {
            Ok(text) => agents_from_sections(&parse_toml_sections(&text)),
            Err(_) => AgentsConfig::default(),
        },
        None => AgentsConfig::default(),
    }
}

/// v0.16.1 [models.<tier>] 段 → TiersConfig（[models.light] 形态）。
/// tier 名与 [agents.x].tiers 引用对应；model 必填，base_url/api_key/timeout_secs
/// 可选（缺省回落 [llm]）。
pub fn tiers_from_sections(sections: &BTreeMap<String, BTreeMap<String, String>>) -> TiersConfig {
    let mut out = TiersConfig::default();
    for (sec, kv) in sections {
        let Some(name) = sec.strip_prefix("models.") else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let get = |k: &str| kv.get(k).cloned().unwrap_or_default();
        let model = get("model");
        if model.is_empty() {
            continue; // model 必填，空档跳过（fail-safe）
        }
        let tier = ModelTier {
            model,
            base_url: get("base_url"),
            api_key: get("api_key"),
            timeout_secs: get("timeout_secs").parse().unwrap_or(0),
            max_tokens: get("max_tokens").parse().unwrap_or(0),
        };
        out.tiers.insert(name.to_string(), tier);
    }
    out
}

pub fn load_tiers_config() -> TiersConfig {
    match find_config_path() {
        Some(path) => match fs::read_to_string(&path) {
            Ok(text) => tiers_from_sections(&parse_toml_sections(&text)),
            Err(_) => TiersConfig::default(),
        },
        None => TiersConfig::default(),
    }
}

// ── v0.16.1 档位匹配（纯函数，确定性）────────────────────────────
//
// 三信号优先序：
//   1. tier= 实参（手动指定，最高）
//   2. 复杂度打分（schema 字段数 + prompt/system 长度）
//   3. 阶梯只有一档 = 钉死该档
// 失败升级（执行层）：档位 i 桥失败 → 自动升 i+1，直到阶梯顶。

/// 复杂度分数（确定性）：0 = 最轻。
/// - schema 字段数：>=6 +2，>=3 +1（重抽取）
/// - prompt 长度：>4000 +2，>1200 +1（长上下文推理）
/// - system 长度：>400 +1（重角色）
pub fn complexity_score(prompt_len: usize, system_len: usize, schema_fields: usize) -> u32 {
    let mut s = 0u32;
    if schema_fields >= 6 {
        s += 2;
    } else if schema_fields >= 3 {
        s += 1;
    }
    if prompt_len > 4000 {
        s += 2;
    } else if prompt_len > 1200 {
        s += 1;
    }
    if system_len > 400 {
        s += 1;
    }
    s
}

/// 分数 → 阶梯起点下标：0-1 → 0档，2-3 → 1档，4+ → 2档（钳到阶梯内）。
pub fn score_to_tier_index(score: u32, ladder_len: usize) -> usize {
    if ladder_len == 0 {
        return 0;
    }
    let idx = match score {
        0..=1 => 0,
        2..=3 => 1,
        _ => 2,
    };
    idx.min(ladder_len - 1)
}

/// 解析 agent 的阶梯并选定起始档。
/// 返回 (tier 名列表, 起始下标)。fail-closed：阶梯引用未定义档位 / tier= 实参
/// 不在阶梯内 → 硬错误并列出可用档位。
pub fn resolve_tier_start(
    agent: &AgentConfig,
    tiers_cfg: &TiersConfig,
    prompt_len: usize,
    system_len: usize,
    schema_fields: usize,
    tier_arg: Option<&str>,
) -> Result<(Vec<String>, usize), String> {
    // 阶梯完整性校验（fail-closed：引用未定义档位 = 配置漂移，硬错）
    for t in &agent.tiers {
        if !tiers_cfg.tiers.contains_key(t) {
            let known: Vec<String> = tiers_cfg.tiers.keys().cloned().collect();
            return Err(format!(
                "agent ladder references undefined tier '{}' — define [models.{}] in config.toml. Known tiers: [{}]",
                t,
                t,
                if known.is_empty() { "(none)".into() } else { known.join(", ") }
            ));
        }
    }
    let ladder = agent.tiers.clone();
    if ladder.is_empty() {
        return Ok((ladder, 0));
    }
    // 信号 1：tier= 实参
    if let Some(want) = tier_arg {
        return match ladder.iter().position(|t| t == want) {
            Some(i) => Ok((ladder, i)),
            None => Err(format!(
                "tier '{}' not in agent ladder [{}] — pick one of the ladder or drop tier=",
                want,
                ladder.join(", ")
            )),
        };
    }
    // 信号 2：复杂度打分（单档阶梯自然钳到 0）
    let score = complexity_score(prompt_len, system_len, schema_fields);
    let idx = score_to_tier_index(score, ladder.len());
    Ok((ladder, idx))
}

/// Fill missing OPENAI_* into a Command's environment from config (does not override existing env).
pub fn apply_llm_env_from_config(cmd: &mut std::process::Command, cfg: &LlmConfig) {
    let set_if_absent = |cmd: &mut std::process::Command, key: &str, val: &str| {
        if val.is_empty() {
            return;
        }
        if std::env::var_os(key).is_none() {
            cmd.env(key, val);
        }
    };
    set_if_absent(cmd, "OPENAI_BASE_URL", &cfg.base_url);
    set_if_absent(cmd, "OPENAI_API_KEY", &cfg.api_key);
    set_if_absent(cmd, "OPENAI_MODEL", &cfg.model);
    if cfg.timeout_secs > 0 && std::env::var_os("OPENAI_TIMEOUT_SECS").is_none() {
        cmd.env("OPENAI_TIMEOUT_SECS", cfg.timeout_secs.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_llm_section_strings() {
        let text = r#"
# comment
[llm]
base_url = "http://127.0.0.1:11434/v1"
api_key = "ollama"
model = 'qwen2.5'
timeout_secs = 90

[other]
x = 1
"#;
        let secs = parse_toml_sections(text);
        let cfg = llm_from_sections(&secs);
        assert_eq!(cfg.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(cfg.api_key, "ollama");
        assert_eq!(cfg.model, "qwen2.5");
        assert_eq!(cfg.timeout_secs, 90);
    }

    #[test]
    fn comment_inside_quotes_preserved() {
        let text = r#"
[llm]
api_key = "abc#def"
"#;
        let cfg = llm_from_sections(&parse_toml_sections(text));
        assert_eq!(cfg.api_key, "abc#def");
    }

    #[test]
    fn aliases_api_base_and_key() {
        let text = r#"
[llm]
api_base = "https://example.com/v1"
key = "secret"
default_model = "glm-5"
"#;
        let cfg = llm_from_sections(&parse_toml_sections(text));
        assert_eq!(cfg.base_url, "https://example.com/v1");
        assert_eq!(cfg.api_key, "secret");
        assert_eq!(cfg.model, "glm-5");
    }

    #[test]
    fn defaults_when_empty_section() {
        let cfg = llm_from_sections(&BTreeMap::new());
        assert_eq!(cfg.base_url, "https://api.openai.com/v1");
        assert_eq!(cfg.model, "gpt-4o-mini");
        assert_eq!(cfg.timeout_secs, 120);
        assert!(cfg.api_key.is_empty());
    }

    #[test]
    fn load_from_temp_file() {
        let dir = std::env::temp_dir().join(format!("ductile-cfg-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        fs::write(&path, "[llm]\napi_key = \"fromfile\"\nmodel = \"m1\"\n").unwrap();
        let cfg = load_llm_config_from_path(&path).unwrap();
        assert_eq!(cfg.api_key, "fromfile");
        assert_eq!(cfg.model, "m1");
        let _ = fs::remove_dir_all(&dir);
    }

    // ── v0.16 [agents.<name>] ──

    #[test]
    fn agents_section_parsed() {
        let text = r#"
[llm]
model = "default-m"

[agents.planner]
model = "qwen3.8:27b"
system = "你是规划助手。\n输出短计划。"
schema = "summary,kind"
timeout_secs = 300

[agents.judge]
model = "gpt-4o-mini"
"#;
        let secs = parse_toml_sections(text);
        let agents = agents_from_sections(&secs);
        let planner = agents.get("planner").expect("planner parsed");
        assert_eq!(planner.model, "qwen3.8:27b");
        // \n 展开
        assert!(
            planner.system.contains('\n'),
            "system: {:?}",
            planner.system
        );
        assert_eq!(planner.schema, "summary,kind");
        assert_eq!(planner.timeout_secs, 300);
        let judge = agents.get("judge").expect("judge parsed");
        assert_eq!(judge.model, "gpt-4o-mini");
        assert_eq!(judge.system, "");
        assert_eq!(agents.get("nonexistent"), None);
    }

    #[test]
    fn agents_ignored_without_prefix() {
        let text = "[agents]\nmodel = \"x\"\n\n[other]\ny = \"1\"\n";
        let agents = agents_from_sections(&parse_toml_sections(text));
        // [agents] 裸段（无 .name）不产出 agent
        assert!(agents.agents.is_empty());
    }

    // ── v0.16.1 档位阶梯 ──

    #[test]
    fn tiers_section_parsed() {
        let text = r#"
[models.light]
model = "lfm2.5-8b"

[models.medium]
model = "qwen3.8:27b"
timeout_secs = 300

[models.high]
model = "ornith-1.5:35b"
base_url = "http://gpu-box:11434/v1"
"#;
        let tiers = tiers_from_sections(&parse_toml_sections(text));
        assert_eq!(tiers.tiers.len(), 3);
        assert_eq!(tiers.tiers["light"].model, "lfm2.5-8b");
        assert_eq!(tiers.tiers["light"].timeout_secs, 0); // 缺省回落 [llm]
        assert_eq!(tiers.tiers["medium"].timeout_secs, 300);
        assert_eq!(tiers.tiers["high"].base_url, "http://gpu-box:11434/v1");
    }

    #[test]
    fn tiers_model_required() {
        // model 缺失的档被跳过（fail-safe，不产出半档）
        let text = "[models.broken]\ntimeout_secs = 60\n";
        let tiers = tiers_from_sections(&parse_toml_sections(text));
        assert!(tiers.tiers.is_empty());
    }

    #[test]
    fn agent_tiers_ladder_parsed() {
        let text = "[agents.planner]\ntiers = \"light, medium,high\"\n";
        let agents = agents_from_sections(&parse_toml_sections(text));
        assert_eq!(
            agents.get("planner").unwrap().tiers,
            vec!["light", "medium", "high"]
        );
    }

    #[test]
    fn complexity_scoring() {
        // 轻任务：短 prompt、无 schema
        assert_eq!(complexity_score(50, 0, 0), 0);
        // 中等：3 字段 schema
        assert_eq!(complexity_score(100, 100, 3), 1);
        // 重：7 字段 schema + 长 prompt
        assert_eq!(complexity_score(5000, 100, 7), 4);
        // 长 system 也加分
        assert_eq!(complexity_score(100, 500, 0), 1);
    }

    #[test]
    fn tier_index_clamped() {
        assert_eq!(score_to_tier_index(0, 3), 0);
        assert_eq!(score_to_tier_index(2, 3), 1);
        assert_eq!(score_to_tier_index(4, 3), 2);
        // 两档阶梯：重任务钳到顶
        assert_eq!(score_to_tier_index(4, 2), 1);
        // 单档阶梯：永远 0
        assert_eq!(score_to_tier_index(99, 1), 0);
    }

    #[test]
    fn resolve_tier_undefined_hard_error() {
        let agent = AgentConfig {
            tiers: vec!["light".into(), "ghost".into()],
            ..Default::default()
        };
        let tiers = TiersConfig {
            tiers: [(
                "light".to_string(),
                ModelTier {
                    model: "m".into(),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
        };
        let err = resolve_tier_start(&agent, &tiers, 10, 10, 0, None).unwrap_err();
        assert!(err.contains("undefined tier 'ghost'"), "{err}");
    }

    #[test]
    fn resolve_tier_arg_selects_index() {
        let agent = AgentConfig {
            tiers: vec!["light".into(), "medium".into(), "high".into()],
            ..Default::default()
        };
        let mk = |m: &str| ModelTier {
            model: m.into(),
            ..Default::default()
        };
        let tiers = TiersConfig {
            tiers: [
                ("light".to_string(), mk("m1")),
                ("medium".to_string(), mk("m2")),
                ("high".to_string(), mk("m3")),
            ]
            .into_iter()
            .collect(),
        };
        // tier= 手动指定
        let (_, i) = resolve_tier_start(&agent, &tiers, 0, 0, 0, Some("high")).unwrap();
        assert_eq!(i, 2);
        // tier= 不在阶梯内 → 硬错误
        assert!(resolve_tier_start(&agent, &tiers, 0, 0, 0, Some("ultra")).is_err());
        // 复杂度选择：轻 → 0
        let (_, i) = resolve_tier_start(&agent, &tiers, 50, 0, 0, None).unwrap();
        assert_eq!(i, 0);
        // 重 → 2
        let (_, i) = resolve_tier_start(&agent, &tiers, 5000, 500, 7, None).unwrap();
        assert_eq!(i, 2);
    }
}
