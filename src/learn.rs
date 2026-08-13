//! Learn — 静态扫描 .pipeline 文件，发现频率压缩模式。
//!
//! 不依赖执行记录。直接从 tag 序列结构发现重复模式。
//! 输出：重复 tag 序列 + 压缩率。

use crate::ast::*;
use crate::parser::parse_pipeline_file;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ── Types ──

#[derive(Debug, Clone)]
pub struct RepeatPattern {
    pub tags: Vec<String>,
    pub count: usize,
    pub pipelines: Vec<String>,
}

#[derive(Debug)]
pub struct LearnResult {
    pub total_pipelines: usize,
    pub total_sequences: usize,
    pub total_tokens: usize,
    pub repeats: Vec<RepeatPattern>,
    pub compressed_size: usize,
    pub compression_ratio: f64,
}

// ── Extract tag sequences from a pipeline ──

pub fn extract_tag_seqs(pl: &Pipeline) -> Vec<Vec<String>> {
    pl.procs
        .iter()
        .filter(|p| !p.deliver && !p.plan.is_empty())
        .map(|proc| proc_tags(proc).into_iter().collect())
        .collect()
}

// ── Find exact-repeat tag sequences ──

pub fn find_repeats(corpus: &[(Vec<String>, String)]) -> Vec<RepeatPattern> {
    let mut freq: BTreeMap<String, (Vec<String>, usize, Vec<String>)> = BTreeMap::new();

    for (seq, pipeline_name) in corpus {
        if seq.len() < 2 {
            continue;
        }
        let key = seq.join(" → ");
        let entry = freq.entry(key).or_insert_with(|| (seq.clone(), 0, vec![]));
        entry.1 += 1;
        if !entry.2.contains(pipeline_name) {
            entry.2.push(pipeline_name.clone());
        }
    }

    freq.into_iter()
        .filter(|(_, (_, count, _))| *count >= 2)
        .map(|(_, (tags, count, pipelines))| RepeatPattern {
            tags,
            count,
            pipelines,
        })
        .collect()
}

// ── Scan directory for .pipeline files ──

pub fn scan_dir(dir: &str) -> (Vec<(Vec<String>, String)>, Vec<String>) {
    let path = Path::new(dir);
    if !path.is_dir() {
        return (vec![], vec![]);
    }

    let mut corpus = Vec::new();
    let mut names = Vec::new();

    let entries = std::fs::read_dir(path).unwrap_or_else(|_| panic!("Cannot read {}", dir));
    let mut pipe_files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "pipeline").unwrap_or(false))
        .collect();
    pipe_files.sort();

    for fpath in pipe_files {
        let fpath_str = fpath.to_string_lossy().to_string();
        match parse_pipeline_file(&fpath_str) {
            Ok(pl) => {
                let seqs = extract_tag_seqs(&pl);
                for seq in seqs {
                    corpus.push((seq, pl.name.clone()));
                }
                names.push(pl.name);
            }
            Err(_) => continue,
        }
    }

    (corpus, names)
}

pub fn scan_dirs(dirs: &[&str]) -> (Vec<(Vec<String>, String)>, Vec<String>) {
    let mut all_corpus = Vec::new();
    let mut all_names = Vec::new();
    for dir in dirs {
        let (c, n) = scan_dir(dir);
        all_corpus.extend(c);
        all_names.extend(n);
    }
    (all_corpus, all_names)
}

// ── Main entry point ──

pub fn learn(dirs: &[&str]) -> LearnResult {
    let (corpus, names) = scan_dirs(dirs);
    let total_tokens: usize = corpus.iter().map(|(seq, _)| seq.len()).sum();

    let repeats = find_repeats(&corpus);

    // Compute compressed size
    let tokens_saved: usize = repeats.iter().map(|r| (r.count - 1) * r.tags.len()).sum();
    let compressed_size = total_tokens.saturating_sub(tokens_saved);
    let ratio = if total_tokens == 0 {
        1.0
    } else {
        compressed_size as f64 / total_tokens as f64
    };

    LearnResult {
        total_pipelines: names.len(),
        total_sequences: corpus.len(),
        total_tokens,
        repeats,
        compressed_size,
        compression_ratio: ratio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_tag_seqs_basic() {
        use crate::ast::Cost;
        let pl = Pipeline {
            name: "test".into(),
            description: String::new(),
            procs: vec![
                Proc {
                    name: "a".into(),
                    description: String::new(),
                    plan: vec![Impl {
                        name: "impl_a".into(),
                        description: String::new(),
                        tags: vec!["search".into(), "web".into()].into_iter().collect(),
                        cost: Cost::default(),
                        enabled: true,
                        when: None,
                        refs: vec![],
                        body_text: String::new(),
                        stub: false,
                        retry: 0,
                        ensure: vec![],
                    }],
                    checks: vec![],
                    deliver: false,
                    foreach: None,
                    foreach_var: String::new(),
                    pick_by: "cost".into(),
                },
                Proc {
                    name: "b".into(),
                    description: String::new(),
                    plan: vec![Impl {
                        name: "impl_b".into(),
                        description: String::new(),
                        tags: vec!["llm".into(), "parse".into()].into_iter().collect(),
                        cost: Cost::default(),
                        enabled: true,
                        when: None,
                        refs: vec![],
                        body_text: String::new(),
                        stub: false,
                        retry: 0,
                        ensure: vec![],
                    }],
                    checks: vec![],
                    deliver: false,
                    foreach: None,
                    foreach_var: String::new(),
                    pick_by: "cost".into(),
                },
            ],
            weights: Weights::default(),
        };
        let seqs = extract_tag_seqs(&pl);
        assert_eq!(seqs.len(), 2);
        // Tags are from BTreeSet, so sorted
        let first: Vec<String> = seqs[0].clone();
        assert!(first.contains(&"search".to_string()));
        assert!(first.contains(&"web".to_string()));
    }

    #[test]
    fn find_repeats_counts_correctly() {
        let corpus = vec![
            (vec!["search".into(), "web".into()], "p1".into()),
            (vec!["search".into(), "web".into()], "p2".into()),
            (vec!["search".into(), "web".into()], "p3".into()),
            (vec!["llm".into(), "parse".into()], "p1".into()),
            (vec!["llm".into(), "parse".into()], "p2".into()),
            (vec!["unique".into()], "p3".into()), // too short, skipped
        ];
        let repeats = find_repeats(&corpus);
        assert_eq!(repeats.len(), 2);
        // search→web appears 3 times
        let sw = repeats
            .iter()
            .find(|r| r.tags.contains(&"search".to_string()))
            .unwrap();
        assert_eq!(sw.count, 3);
        assert_eq!(sw.pipelines.len(), 3);
    }

    #[test]
    fn find_repeats_no_singletons() {
        let corpus = vec![
            (vec!["a".into(), "b".into()], "p1".into()),
            (vec!["c".into(), "d".into()], "p2".into()),
        ];
        let repeats = find_repeats(&corpus);
        assert!(repeats.is_empty()); // all unique
    }
}
