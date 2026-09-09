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
        out.push(PathBuf::from(&home).join(".config").join("ductile").join("config.toml"));
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
    if (raw.starts_with('"') && raw.ends_with('"')) || (raw.starts_with('\'') && raw.ends_with('\''))
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
        Some(path) => load_llm_config_from_path(&path).unwrap_or_default().with_defaults(),
        None => LlmConfig::default().with_defaults(),
    }
}

pub fn load_llm_config_from_path(path: &Path) -> Result<LlmConfig, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let sections = parse_toml_sections(&text);
    Ok(llm_from_sections(&sections))
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
        fs::write(
            &path,
            "[llm]\napi_key = \"fromfile\"\nmodel = \"m1\"\n",
        )
        .unwrap();
        let cfg = load_llm_config_from_path(&path).unwrap();
        assert_eq!(cfg.api_key, "fromfile");
        assert_eq!(cfg.model, "m1");
        let _ = fs::remove_dir_all(&dir);
    }
}
