//! E-graph: DAG construction, parallel group (topological layers), critical path.

use crate::ast::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
pub struct EGraph {
    pub nodes: BTreeSet<String>,
    pub edges: Vec<(String, String)>, // (from, to): from must complete before to
}

pub fn build_egraph(pl: &Pipeline) -> EGraph {
    let mut nodes = BTreeSet::new();
    let mut edges_raw = Vec::new();

    for proc in &pl.procs {
        nodes.insert(proc.name.clone());
        for impl_ in &proc.plan {
            for r in &impl_.refs {
                if nodes.contains(r) || pl.procs.iter().any(|p| &p.name == r) {
                    edges_raw.push((r.clone(), proc.name.clone()));
                }
            }
        }
    }

    let mut seen = BTreeSet::new();
    let mut edges = Vec::new();
    for (f, t) in edges_raw {
        if f != t && seen.insert((f.clone(), t.clone())) {
            edges.push((f, t));
        }
    }

    EGraph { nodes, edges }
}

pub fn parallel_groups(eg: &EGraph) -> Vec<Vec<String>> {
    let all_nodes: Vec<String> = eg.nodes.iter().cloned().collect();
    let mut remaining: BTreeSet<String> = all_nodes.iter().cloned().collect();
    let mut layers = Vec::new();

    while !remaining.is_empty() {
        let layer: Vec<String> = remaining
            .iter()
            .filter(|n| {
                eg.edges
                    .iter()
                    .filter(|(_, to)| to == *n)
                    .all(|(from, _)| !remaining.contains(from))
            })
            .cloned()
            .collect();

        if layer.is_empty() {
            layers.push(remaining.iter().cloned().collect());
            break;
        }

        for n in &layer {
            remaining.remove(n);
        }
        layers.push(layer);
    }
    layers
}

pub fn critical_path(eg: &EGraph) -> Vec<String> {
    let nodes: Vec<String> = eg.nodes.iter().cloned().collect();
    if nodes.is_empty() {
        return Vec::new();
    }

    let mut depth: BTreeMap<String, usize> = BTreeMap::new();
    for n in &nodes {
        depth.insert(n.clone(), 0);
    }

    let mut changed = true;
    while changed {
        changed = false;
        for (from, to) in &eg.edges {
            let new_d = depth.get(from).copied().unwrap_or(0) + 1;
            if new_d > depth.get(to).copied().unwrap_or(0) {
                depth.insert(to.clone(), new_d);
                changed = true;
            }
        }
    }

    let end = nodes
        .iter()
        .max_by_key(|n| depth.get(n.as_str()).copied().unwrap_or(0))
        .cloned()
        .unwrap_or_default();

    let mut path = vec![end.clone()];
    let mut current = end.clone();
    loop {
        let prev = eg
            .edges
            .iter()
            .filter(|(_, to)| *to == current)
            .max_by_key(|(from, _)| depth.get(from.as_str()).copied().unwrap_or(0));

        match prev {
            Some((from, _))
                if depth.get(from.as_str()).copied().unwrap_or(0)
                    < depth.get(current.as_str()).copied().unwrap_or(0) =>
            {
                path.push(from.clone());
                current = from.clone();
            }
            _ => break,
        }
    }
    path.reverse();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_pipeline_with_procs(proc_names: &[&str], refs: &[(&str, &str)]) -> Pipeline {
        let procs: Vec<Proc> = proc_names.iter().map(|name| Proc {
            name: (*name).into(),
            plan: refs.iter()
                .filter(|(from, to)| *to == *name)
                .map(|(from, _)| Impl {
                    name: format!("{}_impl", from),
                    tags: std::collections::BTreeSet::new(),
                    cost: Cost::default(),
                    enabled: true,
                    when: None,
                    refs: vec![(*from).into()],
                    body_text: format!("merge(@{})", from),
                    stub: false,
                    retry: 0,
                    ensure: vec![],
                    description: String::new(),
                })
                .collect::<Vec<_>>(),
            checks: vec![],
            deliver: false,
            foreach: None,
            foreach_var: String::new(),
            pick_by: "cost".into(), description: String::new(),
        }).collect();

        Pipeline {
            name: "test".into(),
            procs,
            weights: Weights::default(), description: String::new(),
        }
    }

    // ── Single node ──
    #[test]
    fn single_node_no_edges() {
        let pl = mk_pipeline_with_procs(&["a"], &[]);
        let eg = build_egraph(&pl);
        assert!(eg.nodes.contains("a"));
        assert!(eg.edges.is_empty());
    }

    // ── Linear chain a -> b -> c ──
    #[test]
    fn linear_chain() {
        let pl = mk_pipeline_with_procs(&["a", "b", "c"], &[("a", "b"), ("b", "c")]);
        let eg = build_egraph(&pl);
        assert_eq!(eg.edges.len(), 2);
        let groups = parallel_groups(&eg);
        assert_eq!(groups.len(), 3); // [a], [b], [c]
        assert_eq!(groups[0], vec!["a".to_string()]);
        assert_eq!(groups[1], vec!["b".to_string()]);
        assert_eq!(groups[2], vec!["c".to_string()]);
    }

    // ── Diamond: a -> b, a -> c, b -> d, c -> d ──
    #[test]
    fn diamond_dependency() {
        let pl = mk_pipeline_with_procs(
            &["a", "b", "c", "d"],
            &[("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")],
        );
        let eg = build_egraph(&pl);
        assert_eq!(eg.edges.len(), 4);
        let groups = parallel_groups(&eg);
        assert_eq!(groups.len(), 3); // [a], [b,c], [d]
        assert!(groups[1].contains(&"b".to_string()));
        assert!(groups[1].contains(&"c".to_string()));
        assert_eq!(groups[2], vec!["d".to_string()]);
    }

    // ── Parallel: a, b independent ──
    #[test]
    fn parallel_independent() {
        let pl = mk_pipeline_with_procs(&["a", "b"], &[]);
        let eg = build_egraph(&pl);
        let groups = parallel_groups(&eg);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 2);
    }

    // ── Critical path ──
    #[test]
    fn critical_path_linear() {
        let pl = mk_pipeline_with_procs(&["a", "b", "c"], &[("a", "b"), ("b", "c")]);
        let eg = build_egraph(&pl);
        let cp = critical_path(&eg);
        assert_eq!(cp.len(), 3);
        assert_eq!(cp[0], "a");
        assert_eq!(cp[2], "c");
    }

    #[test]
    fn critical_path_diamond() {
        let pl = mk_pipeline_with_procs(
            &["a", "b", "c", "d"],
            &[("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")],
        );
        let eg = build_egraph(&pl);
        let cp = critical_path(&eg);
        // Path length should be 4 (a->b->d or a->c->d)
        assert!(cp.len() >= 3);
        assert_eq!(cp.first(), Some(&"a".to_string()));
        assert_eq!(cp.last(), Some(&"d".to_string()));
    }

    // ── Empty pipeline ──
    #[test]
    fn empty_pipeline_egraph() {
        let pl = Pipeline {
            name: "empty".into(),
            procs: vec![],
            weights: Weights::default(), description: String::new(),
        };
        let eg = build_egraph(&pl);
        assert!(eg.nodes.is_empty());
        assert!(parallel_groups(&eg).is_empty());
        assert!(critical_path(&eg).is_empty());
    }
}
