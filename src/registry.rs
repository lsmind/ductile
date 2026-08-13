//! Registry — proc 注册 + 同构发现。
//!
//! 基于 tag 集合匹配：tag 集相同的 proc 视为同构。
//! parse/run/discover 时提示用户可复用。
//! 注册数据统一走 SQLite（db::import_pipeline 自动注册）。

use crate::ast::*;
use crate::db;
use std::collections::BTreeSet;

// ── Types ──

#[derive(Debug, Clone)]
pub struct ProcEntry {
    pub name: String,
    pub description: String,
    pub tags: BTreeSet<String>,
    pub pipeline: String,
    pub impl_count: usize,
}

#[derive(Debug, Clone)]
pub struct IsomorphismGroup {
    pub tags: BTreeSet<String>,
    pub members: Vec<ProcEntry>,
}

#[derive(Debug, Clone)]
pub struct IsoMatch {
    pub local_proc: String,
    pub local_tags: BTreeSet<String>,
    pub remote: ProcEntry,
}

// ── Extract entries from pipeline ──

pub fn extract_entries(pl: &Pipeline) -> Vec<ProcEntry> {
    pl.procs
        .iter()
        .filter(|p| !p.deliver && !p.plan.is_empty())
        .map(|proc| ProcEntry {
            name: proc.name.clone(),
            description: proc.description.clone(),
            tags: proc_tags(proc),
            pipeline: pl.name.clone(),
            impl_count: proc.plan.len(),
        })
        .collect()
}

// ── Find isomorphic groups ──

pub fn find_isomorphic_groups(entries: &[ProcEntry]) -> Vec<IsomorphismGroup> {
    let with_tags: Vec<&ProcEntry> = entries.iter().filter(|e| !e.tags.is_empty()).collect();

    // Group by tags
    let mut groups: std::collections::BTreeMap<Vec<String>, Vec<ProcEntry>> =
        std::collections::BTreeMap::new();
    for e in &with_tags {
        let key: Vec<String> = e.tags.iter().cloned().collect();
        groups.entry(key).or_default().push((*e).clone());
    }

    groups
        .into_iter()
        .filter(|(_, members)| members.len() >= 2)
        .map(|(tags, members)| IsomorphismGroup {
            tags: tags.into_iter().collect(),
            members,
        })
        .collect()
}

// ── Cross-pipeline isomorphic matching ──

pub fn find_isomorphic_matches(
    cur_pipeline: &str,
    registry: &[ProcEntry],
    pl: &Pipeline,
) -> Vec<IsoMatch> {
    let mut matches = Vec::new();
    for proc in &pl.procs {
        if proc.deliver {
            continue;
        }
        let tags = proc_tags(proc);
        if tags.is_empty() {
            continue;
        }
        for entry in registry {
            if entry.pipeline == cur_pipeline {
                continue; // exclude self
            }
            if entry.tags == tags {
                matches.push(IsoMatch {
                    local_proc: proc.name.clone(),
                    local_tags: tags.clone(),
                    remote: entry.clone(),
                });
            }
        }
    }
    matches
}

// ── Load all entries from DB ──

pub fn load_all_entries() -> Vec<ProcEntry> {
    let procs = db::all_procs();
    procs
        .into_iter()
        .map(|p| ProcEntry {
            name: p.name,
            description: p.description,
            tags: p.tags,
            pipeline: p.pipeline,
            impl_count: p.impl_count as usize,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Cost;

    fn mk_pipeline(name: &str) -> Pipeline {
        Pipeline {
            name: name.into(),
            description: String::new(),
            procs: vec![],
            weights: Weights::default(),
        }
    }

    fn mk_proc(name: &str, tags: &[&str]) -> Proc {
        Proc {
            name: name.into(),
            description: String::new(),
            plan: vec![Impl {
                name: "impl1".into(),
                description: String::new(),
                tags: tags.iter().map(|t| t.to_string()).collect(),
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
        }
    }

    #[test]
    fn extract_entries_basic() {
        let pl = Pipeline {
            name: "p1".into(),
            description: String::new(),
            procs: vec![mk_proc("search", &["search", "web"])],
            weights: Weights::default(),
        };
        let entries = extract_entries(&pl);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "search");
        assert_eq!(entries[0].tags.len(), 2);
    }

    #[test]
    fn find_iso_groups_2plus() {
        let entries = vec![
            ProcEntry {
                name: "a".into(),
                description: String::new(),
                tags: vec!["x".into()].into_iter().collect(),
                pipeline: "p1".into(),
                impl_count: 1,
            },
            ProcEntry {
                name: "b".into(),
                description: String::new(),
                tags: vec!["x".into()].into_iter().collect(),
                pipeline: "p2".into(),
                impl_count: 1,
            },
            ProcEntry {
                name: "c".into(),
                description: String::new(),
                tags: vec!["y".into()].into_iter().collect(),
                pipeline: "p1".into(),
                impl_count: 1,
            },
        ];
        let groups = find_isomorphic_groups(&entries);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members.len(), 2);
    }

    #[test]
    fn find_iso_matches_excludes_self() {
        let registry = vec![ProcEntry {
            name: "search".into(),
            description: "web search".into(),
            tags: vec!["search".into(), "web".into()].into_iter().collect(),
            pipeline: "p1".into(),
            impl_count: 1,
        }];
        let pl_same = Pipeline {
            name: "p1".into(),
            description: String::new(),
            procs: vec![mk_proc("search", &["search", "web"])],
            weights: Weights::default(),
        };
        let matches = find_isomorphic_matches("p1", &registry, &pl_same);
        assert!(matches.is_empty()); // same pipeline excluded
    }
}
