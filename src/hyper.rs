//! Hypernetwork: generate runnable `.pipeline` graphs from typed hedges; `check` validates shape.
//! Kinds: `chain` | `gate` (explicit ports) | `bundle` | `xor`. Runtime runs pipelines only.
//! Legacy `.stage` desugars to vertices + hedges.

use crate::ast::Pipeline;
use crate::parser::{parse_pipeline, parse_pipeline_file, ParseError};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

// ── AST ──

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HyperRequire {
    pub judge: bool,
    pub min_impls: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HyperRole {
    Default,
    Judge,
    Source,
    Sink,
}

impl HyperRole {
    fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "default" | "worker" => Ok(HyperRole::Default),
            "judge" | "gate" => Ok(HyperRole::Judge),
            "source" | "ingest" => Ok(HyperRole::Source),
            "sink" | "deliver" | "out" => Ok(HyperRole::Sink),
            other => Err(format!("unknown role {:?}", other)),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            HyperRole::Default => "default",
            HyperRole::Judge => "judge",
            HyperRole::Source => "source",
            HyperRole::Sink => "sink",
        }
    }
}

/// Hyperedge incidence kind (what a multi-vertex edge *means* when projected).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HedgeKind {
    Chain,
    Bundle,
    Gate,
    Xor,
}

impl HedgeKind {
    fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "chain" | "path" | "flow" => Ok(HedgeKind::Chain),
            "bundle" | "and" | "co" => Ok(HedgeKind::Bundle),
            "gate" | "judge" => Ok(HedgeKind::Gate),
            "xor" | "alt" | "or" => Ok(HedgeKind::Xor),
            other => Err(format!(
                "unknown hedge kind {:?} (chain|bundle|gate|xor)",
                other
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            HedgeKind::Chain => "chain",
            HedgeKind::Bundle => "bundle",
            HedgeKind::Gate => "gate",
            HedgeKind::Xor => "xor",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Vertex {
    pub name: String,
    pub role: HyperRole,
    pub tags: BTreeSet<String>,
}

/// A hyperedge: typed incidence over vertices.
///
/// - `chain` / `bundle` / `xor`: use `members` (order matters for chain/xor slot).
/// - `gate`: use explicit `judge` + `producers` + `consumers` (members mirrors union for keying).
#[derive(Debug, Clone)]
pub struct Hyperedge {
    pub name: String,
    pub kind: HedgeKind,
    pub members: Vec<String>,
    pub judge: Option<String>,
    pub producers: Vec<String>,
    pub consumers: Vec<String>,
}

/// Projected DAG node (execution view). Derived from the hypergraph.
#[derive(Debug, Clone)]
pub struct HyperStage {
    pub name: String,
    pub role: HyperRole,
    pub tags: BTreeSet<String>,
    pub after: Vec<String>,
    pub gated_by: Option<String>,
    /// From `xor` (slot) and/or `.require(min_impls=)`; emit/check use this per stage.
    pub min_impls: usize,
}

/// Parsed hypergraph + its DAG projection (`stages`).
#[derive(Debug, Clone)]
pub struct HyperSpec {
    pub name: String,
    pub goal: String,
    pub require: HyperRequire,
    pub vertices: Vec<Vertex>,
    pub hedges: Vec<Hyperedge>,
    /// DAG projection used by emit/check (always filled).
    pub stages: Vec<HyperStage>,
    pub deliver: Option<String>,
}

impl HyperSpec {
    /// Ordered/typed incidence key (name-independent). Not classical undirected hypergraph iso.
    pub fn hypergraph_key(&self) -> String {
        // Count only projected stages' roles (xor alts suppressed) so key matches execution shape.
        let mut role_counts: BTreeMap<&str, usize> = BTreeMap::new();
        for s in &self.stages {
            *role_counts.entry(s.role.as_str()).or_insert(0) += 1;
        }
        let vpart: Vec<String> = role_counts
            .iter()
            .map(|(r, n)| format!("{}×{}", r, n))
            .collect();

        let role_of = |name: &str| -> &str {
            self.vertices
                .iter()
                .find(|v| v.name == name)
                .map(|v| v.role.as_str())
                .unwrap_or("?")
        };

        let mut eparts: Vec<String> = self
            .hedges
            .iter()
            .map(|h| match h.kind {
                HedgeKind::Chain => {
                    let roles: Vec<&str> = h.members.iter().map(|m| role_of(m)).collect();
                    format!("chain:{}", roles.join(">"))
                }
                HedgeKind::Gate => {
                    let j = h.judge.as_deref().map(role_of).unwrap_or("?");
                    let mut ps: Vec<&str> = h.producers.iter().map(|m| role_of(m)).collect();
                    ps.sort_unstable();
                    let mut cs: Vec<&str> = h.consumers.iter().map(|m| role_of(m)).collect();
                    cs.sort_unstable();
                    format!("gate:j={}|p={}|c={}", j, ps.join("+"), cs.join("+"))
                }
                HedgeKind::Bundle => {
                    let mut s: Vec<&str> = h.members.iter().map(|m| role_of(m)).collect();
                    s.sort_unstable();
                    format!("bundle:{}", s.join("+"))
                }
                HedgeKind::Xor => {
                    // slot role first, then sorted alt roles (alts may be suppressed from V)
                    let slot = h.members.first().map(|m| role_of(m)).unwrap_or("?");
                    let mut alts: Vec<&str> =
                        h.members.iter().skip(1).map(|m| role_of(m)).collect();
                    alts.sort_unstable();
                    format!(
                        "xor:slot={}|alts={}|n={}",
                        slot,
                        alts.join("+"),
                        h.members.len()
                    )
                }
            })
            .collect();
        eparts.sort();
        format!(
            "V=[{}]|E=[{}]|judge={}",
            vpart.join(","),
            eparts.join(";"),
            if self.require.judge
                || self
                    .vertices
                    .iter()
                    .any(|v| matches!(v.role, HyperRole::Judge))
            {
                1
            } else {
                0
            }
        )
    }
}

fn hedge_simple(name: String, kind: HedgeKind, members: Vec<String>) -> Hyperedge {
    Hyperedge {
        name,
        kind,
        members,
        judge: None,
        producers: Vec::new(),
        consumers: Vec::new(),
    }
}

fn hedge_gate(
    name: String,
    judge: String,
    producers: Vec<String>,
    consumers: Vec<String>,
) -> Hyperedge {
    let mut members = Vec::new();
    for p in &producers {
        if !members.iter().any(|m| m == p) {
            members.push(p.clone());
        }
    }
    if !members.iter().any(|m| m == &judge) {
        members.push(judge.clone());
    }
    for c in &consumers {
        if !members.iter().any(|m| m == c) {
            members.push(c.clone());
        }
    }
    Hyperedge {
        name,
        kind: HedgeKind::Gate,
        members,
        judge: Some(judge),
        producers,
        consumers,
    }
}

#[derive(Debug, Clone)]
pub struct HyperError {
    pub line: usize,
    pub msg: String,
}

impl std::fmt::Display for HyperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line > 0 {
            write!(f, "hyper L{}: {}", self.line, self.msg)
        } else {
            write!(f, "hyper: {}", self.msg)
        }
    }
}

// ── Parse ──

pub fn parse_hyper(input: &str) -> Result<HyperSpec, HyperError> {
    let mut name: Option<String> = None;
    let mut goal = String::new();
    let mut require = HyperRequire {
        judge: false,
        min_impls: 1,
    };
    let mut vertices: Vec<Vertex> = Vec::new();
    let mut hedges: Vec<Hyperedge> = Vec::new();
    let mut legacy_stages: Vec<HyperStage> = Vec::new();
    let mut deliver: Option<String> = None;

    for (idx, raw) in input.lines().enumerate() {
        let line_no = idx + 1;
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line
            .strip_prefix("HyperGraph(")
            .or_else(|| line.strip_prefix("Hyper("))
        {
            name = Some(extract_first_quoted(rest).ok_or_else(|| HyperError {
                line: line_no,
                msg: "Hyper[Graph](\"name\") needs a quoted name".into(),
            })?);
            continue;
        }

        if !line.starts_with('.') {
            return Err(HyperError {
                line: line_no,
                msg: format!(
                    "expected HyperGraph(...) or .directive, got {:?}",
                    trunc(&line, 48)
                ),
            });
        }

        let (directive, args) = split_directive(&line).map_err(|m| HyperError {
            line: line_no,
            msg: m,
        })?;

        match directive {
            "goal" => {
                goal = extract_first_quoted(&args).ok_or_else(|| HyperError {
                    line: line_no,
                    msg: ".goal(\"...\") needs a quoted string".into(),
                })?;
            }
            "require" => {
                apply_require(&mut require, &args).map_err(|m| HyperError {
                    line: line_no,
                    msg: m,
                })?;
            }
            "vertex" | "v" => {
                vertices.push(parse_vertex(&args).map_err(|m| HyperError {
                    line: line_no,
                    msg: m,
                })?);
            }
            "hedge" | "edge" | "hyperedge" => {
                hedges.push(parse_hedge(&args).map_err(|m| HyperError {
                    line: line_no,
                    msg: m,
                })?);
            }
            "stage" => {
                // Legacy sugar → vertex + deferred chain/gate synthesis
                let st = parse_stage(&args).map_err(|m| HyperError {
                    line: line_no,
                    msg: m,
                })?;
                if !vertices.iter().any(|v| v.name == st.name) {
                    vertices.push(Vertex {
                        name: st.name.clone(),
                        role: st.role.clone(),
                        tags: st.tags.clone(),
                    });
                }
                legacy_stages.push(st);
            }
            "deliver" => {
                deliver = Some(parse_deliver_arg(&args).map_err(|m| HyperError {
                    line: line_no,
                    msg: m,
                })?);
            }
            other => {
                return Err(HyperError {
                    line: line_no,
                    msg: format!("unknown directive .{}", other),
                });
            }
        }
    }

    let name = name.ok_or_else(|| HyperError {
        line: 0,
        msg: "missing HyperGraph(\"name\") or Hyper(\"name\") header".into(),
    })?;

    if vertices.is_empty() {
        return Err(HyperError {
            line: 0,
            msg: "hypergraph needs at least one .vertex(...) or .stage(...)".into(),
        });
    }

    // Desugar legacy after=/gated_by= into hyperedges when author didn't write .hedge
    if hedges.is_empty() && !legacy_stages.is_empty() {
        hedges = synthesize_hedges_from_stages(&legacy_stages);
    }

    validate_hypergraph(&vertices, &hedges, deliver.as_deref())?;

    let mut stages = project_stages(&vertices, &hedges)?;
    if stages.is_empty() {
        return Err(HyperError {
            line: 0,
            msg: "projection produced no stages".into(),
        });
    }

    // If require.judge, ensure a judge vertex exists or a gate hedge exists
    if require.judge {
        let has_j = vertices.iter().any(|v| matches!(v.role, HyperRole::Judge))
            || hedges.iter().any(|h| h.kind == HedgeKind::Gate);
        if !has_j {
            return Err(HyperError {
                line: 0,
                msg: "require.judge=true but no judge vertex / gate hyperedge".into(),
            });
        }
    }

    // Author require.min_impls is a floor for every projected stage; xor only raises its slot.
    let floor = require.min_impls.max(1);
    for s in &mut stages {
        s.min_impls = s.min_impls.max(floor);
    }

    validate_spec_refs(&name, &stages, deliver.as_deref())?;

    Ok(HyperSpec {
        name,
        goal,
        require,
        vertices,
        hedges,
        stages,
        deliver,
    })
}

pub fn parse_hyper_file(path: &str) -> Result<HyperSpec, HyperError> {
    let content = std::fs::read_to_string(path).map_err(|e| HyperError {
        line: 0,
        msg: format!("cannot read {}: {}", path, e),
    })?;
    parse_hyper(&content)
}

fn synthesize_hedges_from_stages(stages: &[HyperStage]) -> Vec<Hyperedge> {
    let mut hedges = Vec::new();

    // 1) Explicit gate ports from gated_by= (producers = after \ {judge})
    let mut covered_data: BTreeSet<(String, String)> = BTreeSet::new();
    for s in stages {
        if let Some(g) = &s.gated_by {
            let producers: Vec<String> = s.after.iter().filter(|a| *a != g).cloned().collect();
            for p in &producers {
                // producers→judge is owned by the gate hedge; do not also emit chain
                covered_data.insert((p.clone(), g.clone()));
            }
            hedges.push(hedge_gate(
                format!("gate_{}", s.name),
                g.clone(),
                producers,
                vec![s.name.clone()],
            ));
        }
    }

    // 2) Remaining after= edges as chain pairs (skip gate-owned producer→judge)
    let mut pairs: Vec<(String, String)> = Vec::new();
    for s in stages {
        for a in &s.after {
            let e = (a.clone(), s.name.clone());
            if covered_data.contains(&e) {
                continue;
            }
            pairs.push(e);
        }
    }

    // 3) Merge unique-successor pairs into maximal chains (aligns with explicit kind=chain)
    for chain in merge_chain_pairs(&pairs) {
        if chain.len() >= 2 {
            hedges.push(hedge_simple(
                format!("chain_{}", hedges.len() + 1),
                HedgeKind::Chain,
                chain,
            ));
        }
    }

    // If nothing at all, linear chain in declaration order
    if hedges.is_empty() && stages.len() >= 2 {
        hedges.push(hedge_simple(
            "main_chain".into(),
            HedgeKind::Chain,
            stages.iter().map(|s| s.name.clone()).collect(),
        ));
    }
    hedges
}

/// Merge (u→v) pairs into maximal paths where each node has ≤1 successor and ≤1 predecessor
/// among the pair set. Branching edges stay as length-2 chains.
fn merge_chain_pairs(pairs: &[(String, String)]) -> Vec<Vec<String>> {
    let mut succ: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut pred: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (a, b) in pairs {
        succ.entry(a.clone()).or_default().push(b.clone());
        pred.entry(b.clone()).or_default().push(a.clone());
    }

    // Unique-successor / unique-predecessor links that can be path-merged
    let mut uniq_succ: BTreeMap<String, String> = BTreeMap::new();
    let mut uniq_pred: BTreeMap<String, String> = BTreeMap::new();
    let mut leftover: Vec<(String, String)> = Vec::new();
    for (a, b) in pairs {
        let out_ok = succ.get(a).map(|v| v.len() == 1).unwrap_or(false);
        let in_ok = pred.get(b).map(|v| v.len() == 1).unwrap_or(false);
        if out_ok && in_ok {
            uniq_succ.insert(a.clone(), b.clone());
            uniq_pred.insert(b.clone(), a.clone());
        } else {
            leftover.push((a.clone(), b.clone()));
        }
    }

    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut chains: Vec<Vec<String>> = Vec::new();

    // Starts: nodes with uniq_succ but no uniq_pred
    let mut starts: Vec<String> = uniq_succ
        .keys()
        .filter(|k| !uniq_pred.contains_key(k.as_str()))
        .cloned()
        .collect();
    starts.sort();
    for start in starts {
        if used.contains(&start) {
            continue;
        }
        let mut path = vec![start.clone()];
        used.insert(start.clone());
        let mut cur = start;
        while let Some(nxt) = uniq_succ.get(&cur) {
            if used.contains(nxt) {
                break;
            }
            path.push(nxt.clone());
            used.insert(nxt.clone());
            cur = nxt.clone();
        }
        if path.len() >= 2 {
            chains.push(path);
        }
    }

    // Any remaining uniq edges not consumed (cycles / fragments)
    for (a, b) in &uniq_succ {
        if !used.contains(a) || !used.contains(b) {
            if !used.contains(a) && !used.contains(b) {
                chains.push(vec![a.clone(), b.clone()]);
                used.insert(a.clone());
                used.insert(b.clone());
            }
        }
    }

    // Branching leftovers as pairwise chains
    leftover.sort();
    leftover.dedup();
    for (a, b) in leftover {
        chains.push(vec![a, b]);
    }
    chains
}

fn validate_hypergraph(
    vertices: &[Vertex],
    hedges: &[Hyperedge],
    deliver: Option<&str>,
) -> Result<(), HyperError> {
    let names: BTreeSet<&str> = vertices.iter().map(|v| v.name.as_str()).collect();
    let mut seen = BTreeSet::new();
    for v in vertices {
        if !seen.insert(v.name.as_str()) {
            return Err(HyperError {
                line: 0,
                msg: format!("duplicate vertex {:?}", v.name),
            });
        }
    }
    for h in hedges {
        match h.kind {
            HedgeKind::Gate => {
                let judge = h.judge.as_ref().ok_or_else(|| HyperError {
                    line: 0,
                    msg: format!(
                        "gate hedge {:?} needs judge=… (explicit ports; no last-member heuristic)",
                        h.name
                    ),
                })?;
                if h.consumers.is_empty() {
                    return Err(HyperError {
                        line: 0,
                        msg: format!("gate hedge {:?} needs consumers=…", h.name),
                    });
                }
                for m in h
                    .producers
                    .iter()
                    .chain(std::iter::once(judge))
                    .chain(h.consumers.iter())
                {
                    if !names.contains(m.as_str()) {
                        return Err(HyperError {
                            line: 0,
                            msg: format!("hyperedge {:?} port {:?} unknown vertex", h.name, m),
                        });
                    }
                }
            }
            HedgeKind::Chain => {
                if h.members.len() < 2 {
                    return Err(HyperError {
                        line: 0,
                        msg: format!("chain hedge {:?} needs ≥2 members", h.name),
                    });
                }
                for m in &h.members {
                    if !names.contains(m.as_str()) {
                        return Err(HyperError {
                            line: 0,
                            msg: format!("hyperedge {:?} member {:?} unknown vertex", h.name, m),
                        });
                    }
                }
            }
            HedgeKind::Bundle | HedgeKind::Xor => {
                if h.members.is_empty() {
                    return Err(HyperError {
                        line: 0,
                        msg: format!("hyperedge {:?} has no members", h.name),
                    });
                }
                if h.kind == HedgeKind::Xor && h.members.len() < 2 {
                    return Err(HyperError {
                        line: 0,
                        msg: format!(
                            "xor hedge {:?} needs ≥2 members (slot=first, rest=alts)",
                            h.name
                        ),
                    });
                }
                for m in &h.members {
                    if !names.contains(m.as_str()) {
                        return Err(HyperError {
                            line: 0,
                            msg: format!("hyperedge {:?} member {:?} unknown vertex", h.name, m),
                        });
                    }
                }
            }
        }
    }
    if let Some(d) = deliver {
        if !names.contains(d) {
            return Err(HyperError {
                line: 0,
                msg: format!(".deliver({:?}) unknown vertex", d),
            });
        }
    }
    Ok(())
}

/// Project hypergraph → DAG stages (pairwise after= / gated_by=).
pub fn project_stages(
    vertices: &[Vertex],
    hedges: &[Hyperedge],
) -> Result<Vec<HyperStage>, HyperError> {
    let mut after: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut gated: BTreeMap<String, String> = BTreeMap::new();
    let mut slot_min_impls: BTreeMap<String, usize> = BTreeMap::new();

    for v in vertices {
        after.entry(v.name.clone()).or_default();
    }

    // Vertices that appear only as xor alts (not slot, not in other hedges) are suppressed.
    let mut in_other: BTreeSet<String> = BTreeSet::new();
    for h in hedges {
        match h.kind {
            HedgeKind::Xor => {
                if let Some(slot) = h.members.first() {
                    in_other.insert(slot.clone());
                }
            }
            HedgeKind::Chain | HedgeKind::Bundle => {
                for m in &h.members {
                    in_other.insert(m.clone());
                }
            }
            HedgeKind::Gate => {
                if let Some(j) = &h.judge {
                    in_other.insert(j.clone());
                }
                for m in h.producers.iter().chain(h.consumers.iter()) {
                    in_other.insert(m.clone());
                }
            }
        }
    }

    let mut suppressed: BTreeSet<String> = BTreeSet::new();
    for h in hedges {
        if h.kind != HedgeKind::Xor {
            continue;
        }
        let slot = h.members.first().cloned().ok_or_else(|| HyperError {
            line: 0,
            msg: format!("xor hedge {:?} empty", h.name),
        })?;
        slot_min_impls.insert(slot.clone(), h.members.len().max(1));
        for alt in h.members.iter().skip(1) {
            if !in_other.contains(alt) {
                suppressed.insert(alt.clone());
            }
        }
    }

    for h in hedges {
        match h.kind {
            HedgeKind::Chain => {
                for w in h.members.windows(2) {
                    after.entry(w[1].clone()).or_default().insert(w[0].clone());
                }
            }
            HedgeKind::Bundle => {
                // Constraint-only at project time; enforced in check_pipeline_against.
            }
            HedgeKind::Gate => {
                let judge = h.judge.clone().ok_or_else(|| HyperError {
                    line: 0,
                    msg: format!("gate hedge {:?} missing judge=", h.name),
                })?;
                if h.consumers.is_empty() {
                    return Err(HyperError {
                        line: 0,
                        msg: format!("gate hedge {:?} missing consumers=", h.name),
                    });
                }
                // producers → judge (data so judge can @ref producers)
                for p in &h.producers {
                    after.entry(judge.clone()).or_default().insert(p.clone());
                }
                // consumers: when-gate only — do NOT add judge as data after=
                for c in &h.consumers {
                    gated.insert(c.clone(), judge.clone());
                }
            }
            HedgeKind::Xor => {
                // Slot kept; alts suppressed above. No extra DAG edges.
            }
        }
    }

    // Preserve vertex declaration order; drop xor-only alts
    let stages = vertices
        .iter()
        .filter(|v| !suppressed.contains(&v.name))
        .map(|v| HyperStage {
            name: v.name.clone(),
            role: v.role.clone(),
            tags: v.tags.clone(),
            after: after
                .get(&v.name)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default(),
            gated_by: gated.get(&v.name).cloned(),
            min_impls: slot_min_impls.get(&v.name).copied().unwrap_or(1),
        })
        .collect();
    Ok(stages)
}

fn parse_vertex(args: &str) -> Result<Vertex, String> {
    let name = extract_first_quoted(args)
        .ok_or_else(|| ".vertex(\"name\", ...) needs a quoted name".to_string())?;
    let mut role = HyperRole::Default;
    let mut tags = BTreeSet::new();
    let after_name = skip_first_quoted(args);
    for part in merge_tag_continuations(split_top_commas(after_name)) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((k, v)) = part.split_once('=') {
            match k.trim() {
                "role" => role = HyperRole::parse(v.trim())?,
                "tags" => tags = parse_tag_list(v.trim()),
                other => return Err(format!("unknown vertex key {:?}", other)),
            }
        } else if part.starts_with('#') {
            for t in parse_tag_list(part) {
                tags.insert(t);
            }
        } else {
            return Err(format!("bad vertex arg {:?}", trunc(part, 40)));
        }
    }
    Ok(Vertex { name, role, tags })
}

fn parse_hedge(args: &str) -> Result<Hyperedge, String> {
    // chain/bundle/xor: .hedge("flow", kind=chain, load, extract, report)
    // gate: .hedge("quality", kind=gate, judge=gate, producers=extract, consumers=report)
    // multi: producers=a+b  or producers="a,b"
    let name = extract_first_quoted(args)
        .ok_or_else(|| ".hedge(\"name\", kind=..., …) needs a name".to_string())?;
    let rest = skip_first_quoted(args);
    let mut kind = HedgeKind::Chain;
    let mut members = Vec::new();
    let mut judge: Option<String> = None;
    let mut producers: Vec<String> = Vec::new();
    let mut consumers: Vec<String> = Vec::new();
    for part in split_top_commas(rest) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((k, v)) = part.split_once('=') {
            match k.trim() {
                "kind" => kind = HedgeKind::parse(v.trim())?,
                "judge" => judge = Some(strip_at(v.trim().trim_matches(|c| c == '"' || c == '\''))),
                "producers" | "producer" => {
                    producers.extend(parse_name_list(v.trim()));
                }
                "consumers" | "consumer" => {
                    consumers.extend(parse_name_list(v.trim()));
                }
                other => return Err(format!("unknown hedge key {:?}", other)),
            }
        } else {
            members.push(strip_at(part.trim_matches(|c| c == '"' || c == '\'')));
        }
    }
    match kind {
        HedgeKind::Gate => {
            if judge.is_none() || consumers.is_empty() {
                return Err(format!(
                    "gate hedge {:?} needs judge=… and consumers=… (producers= optional; positional members rejected)",
                    name
                ));
            }
            if !members.is_empty() {
                return Err(format!(
                    "gate hedge {:?} must use ports (judge=/producers=/consumers=), not bare member list",
                    name
                ));
            }
            Ok(hedge_gate(name, judge.unwrap(), producers, consumers))
        }
        HedgeKind::Chain | HedgeKind::Bundle | HedgeKind::Xor => {
            if judge.is_some() || !producers.is_empty() || !consumers.is_empty() {
                return Err(format!(
                    "hedge {:?} kind={} uses member list, not gate ports",
                    name,
                    kind.as_str()
                ));
            }
            if members.is_empty() {
                return Err(format!("hedge {:?} needs member vertices", name));
            }
            Ok(hedge_simple(name, kind, members))
        }
    }
}

fn skip_first_quoted(args: &str) -> &str {
    if let Some(q1) = args.find('"') {
        let rest = &args[q1 + 1..];
        if let Some(q2) = rest.find('"') {
            return rest[q2 + 1..].trim().trim_start_matches(',').trim();
        }
    }
    args
}

fn validate_spec_refs(
    _name: &str,
    stages: &[HyperStage],
    deliver: Option<&str>,
) -> Result<(), HyperError> {
    let names: BTreeSet<&str> = stages.iter().map(|s| s.name.as_str()).collect();
    for s in stages {
        for a in &s.after {
            if !names.contains(a.as_str()) {
                return Err(HyperError {
                    line: 0,
                    msg: format!("stage {:?} after={:?} unknown", s.name, a),
                });
            }
        }
        if let Some(g) = &s.gated_by {
            if !names.contains(g.as_str()) {
                return Err(HyperError {
                    line: 0,
                    msg: format!("stage {:?} gated_by={:?} unknown", s.name, g),
                });
            }
        }
    }
    if let Some(d) = deliver {
        if !names.contains(d) {
            return Err(HyperError {
                line: 0,
                msg: format!(".deliver({:?}) unknown stage", d),
            });
        }
    }
    Ok(())
}

fn strip_comment(line: &str) -> &str {
    if let Some(i) = line.find("//") {
        &line[..i]
    } else {
        line
    }
}

fn trunc(s: &str, n: usize) -> String {
    crate::trunc_chars(s, n).to_string()
}

fn extract_first_quoted(s: &str) -> Option<String> {
    let start = s.find('"')?;
    let rest = &s[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn split_directive(line: &str) -> Result<(&str, String), String> {
    // .goal("...")  or .require(...)  or .stage("n", ...)
    let body = line.strip_prefix('.').unwrap_or(line);
    let open = body
        .find('(')
        .ok_or_else(|| format!("directive missing '(': {}", line))?;
    let name = body[..open].trim();
    let mut depth = 0i32;
    let mut end = None;
    for (i, ch) in body.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end.ok_or_else(|| format!("unclosed '(': {}", line))?;
    let args = body[open + 1..end].trim().to_string();
    Ok((name, args))
}

fn apply_require(req: &mut HyperRequire, args: &str) -> Result<(), String> {
    for part in split_top_commas(args) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = part.split_once('=').ok_or_else(|| {
            format!(
                ".require expects key=value pairs, got {:?}",
                trunc(part, 40)
            )
        })?;
        let k = k.trim();
        let v = v.trim();
        match k {
            "judge" => req.judge = parse_bool(v)?,
            "min_impls" => {
                req.min_impls = v
                    .parse::<usize>()
                    .map_err(|_| format!("bad min_impls {:?}", v))?;
                if req.min_impls == 0 {
                    return Err("min_impls must be >= 1".into());
                }
            }
            other => return Err(format!("unknown require key {:?}", other)),
        }
    }
    Ok(())
}

fn parse_bool(v: &str) -> Result<bool, String> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        other => Err(format!("bad bool {:?}", other)),
    }
}

fn parse_stage(args: &str) -> Result<HyperStage, String> {
    let name = extract_first_quoted(args)
        .ok_or_else(|| ".stage(\"name\", ...) needs a quoted stage name".to_string())?;
    let mut role = HyperRole::Default;
    let mut tags = BTreeSet::new();
    let mut after = Vec::new();
    let mut gated_by = None;

    // Skip the quoted name, parse remaining key=value / tags=#a,#b
    let after_name = if let Some(q1) = args.find('"') {
        let rest = &args[q1 + 1..];
        if let Some(q2) = rest.find('"') {
            rest[q2 + 1..].trim().trim_start_matches(',').trim()
        } else {
            ""
        }
    } else {
        args
    };

    for part in merge_tag_continuations(split_top_commas(after_name)) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((k, v)) = part.split_once('=') {
            let k = k.trim();
            let v = v.trim();
            match k {
                "role" => role = HyperRole::parse(v)?,
                "tags" => tags = parse_tag_list(v),
                "after" => after = parse_name_list(v),
                "gated_by" | "gate" => gated_by = Some(strip_at(v)),
                other => return Err(format!("unknown stage key {:?}", other)),
            }
        } else if part.starts_with('#') || part.contains('#') {
            for t in parse_tag_list(part) {
                tags.insert(t);
            }
        } else {
            return Err(format!("bad stage arg {:?}", trunc(part, 40)));
        }
    }

    Ok(HyperStage {
        name,
        role,
        tags,
        after,
        gated_by,
        min_impls: 1,
    })
}

fn merge_tag_continuations(parts: Vec<String>) -> Vec<String> {
    // tags=#read,#file is split by commas into ["tags=#read", "#file"] — rejoin.
    let mut out = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        let p = parts[i].trim().to_string();
        if p.starts_with("tags=") {
            let mut blob = p;
            while i + 1 < parts.len() {
                let n = parts[i + 1].trim();
                if n.starts_with('#') && !n.contains('=') {
                    blob.push(',');
                    blob.push_str(n);
                    i += 1;
                } else {
                    break;
                }
            }
            out.push(blob);
        } else {
            out.push(p);
        }
        i += 1;
    }
    out
}

fn parse_deliver_arg(args: &str) -> Result<String, String> {
    if let Some(q) = extract_first_quoted(args) {
        return Ok(q);
    }
    let t = args.trim().trim_start_matches('@');
    if t.is_empty() {
        return Err(".deliver needs a stage name".into());
    }
    // deliver(report) or deliver(@report)
    let name = t
        .trim_matches(|c| c == ')' || c == '(' || c == '"' || c == ' ')
        .to_string();
    if name.is_empty() {
        return Err(".deliver needs a stage name".into());
    }
    Ok(name)
}

fn parse_tag_list(v: &str) -> BTreeSet<String> {
    v.split(',')
        .map(|t| t.trim().trim_start_matches('#').to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn parse_name_list(v: &str) -> Vec<String> {
    let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
    v.split(|c| c == '+' || c == ',')
        .map(|t| strip_at(t.trim()))
        .filter(|t| !t.is_empty())
        .collect()
}

fn strip_at(s: &str) -> String {
    s.trim().trim_start_matches('@').to_string()
}

fn split_top_commas(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut in_str = false;
    for ch in s.chars() {
        match ch {
            '"' if !in_str => in_str = true,
            '"' if in_str => in_str = false,
            '(' | '[' if !in_str => depth += 1,
            ')' | ']' if !in_str => depth -= 1,
            ',' if !in_str && depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

// ── Emit ──

/// Emit a skeleton `.pipeline` guided by the hyper spec.
/// Bodies are deterministic stubs chosen from stage tags/role so `ductile check` passes.
pub fn emit_pipeline(spec: &HyperSpec) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "// Generated by ductile hyper — topology from Hyper(\"{}\"); fill real impls as needed.\n",
        spec.name
    ));
    out.push_str("// Regenerate: ductile hyper build <file.hyper> -o <file.pipeline>\n");
    if spec.goal.is_empty() {
        out.push_str(&format!("Pipeline(\"{}\")\n", spec.name));
    } else {
        out.push_str(&format!(
            "Pipeline(\"{}\", \"{}\")\n",
            spec.name,
            escape_quotes(&spec.goal)
        ));
    }

    let min_impls_default = spec.require.min_impls.max(1);
    let prev_default = default_after_chain(spec);

    for stage in &spec.stages {
        let after = if stage.after.is_empty() {
            prev_default.get(&stage.name).cloned().unwrap_or_default()
        } else {
            stage.after.clone()
        };
        let min_impls = stage.min_impls.max(min_impls_default);

        out.push_str(&format!("  .proc(\"{}\")\n", stage.name));
        out.push_str(&format!(
            "    .desc(\"hyper stage {} role={}\")\n",
            stage.name,
            stage.role.as_str()
        ));
        if let Some(g) = &stage.gated_by {
            out.push_str(&format!("    .when(@{}.score >= 80)\n", g));
        }
        out.push_str("    .plan(\n");

        let bodies = stub_bodies(stage, &after, min_impls);
        for (i, (iname, body, tags)) in bodies.iter().enumerate() {
            let tag_s = if tags.is_empty() {
                String::new()
            } else {
                format!(
                    ".tags({})",
                    tags.iter()
                        .map(|t| format!("#{}", t))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let stub_flag = if i > 0 { ".stub" } else { "" };
            let comma = if i + 1 < bodies.len() { "," } else { "" };
            out.push_str(&format!(
                "      {} -> {}{}{}{}\n",
                iname, body, tag_s, stub_flag, comma
            ));
        }
        out.push_str("    )\n");
    }

    let deliver = resolve_deliver(spec);
    out.push_str(&format!("  .proc(\"deliver\")\n"));
    out.push_str(&format!("    .deliver(@{})\n", deliver));
    out
}

fn default_after_chain(spec: &HyperSpec) -> BTreeMap<String, Vec<String>> {
    // If a stage omits after=, chain linearly in declaration order (except first).
    let mut m = BTreeMap::new();
    let mut prev: Option<String> = None;
    for s in &spec.stages {
        if s.after.is_empty() {
            if let Some(p) = &prev {
                m.insert(s.name.clone(), vec![p.clone()]);
            }
        }
        prev = Some(s.name.clone());
    }
    m
}

fn resolve_deliver(spec: &HyperSpec) -> String {
    if let Some(d) = &spec.deliver {
        return d.clone();
    }
    spec.stages
        .iter()
        .rev()
        .find(|s| matches!(s.role, HyperRole::Sink))
        .or_else(|| spec.stages.last())
        .map(|s| s.name.clone())
        .unwrap_or_else(|| "out".into())
}

fn stub_bodies(
    stage: &HyperStage,
    after: &[String],
    min_impls: usize,
) -> Vec<(String, String, BTreeSet<String>)> {
    let tags = if stage.tags.is_empty() {
        default_tags_for_role(&stage.role)
    } else {
        stage.tags.clone()
    };
    let primary = primary_body(stage, after, &tags);
    let mut out = vec![("primary".into(), primary, tags.clone())];
    while out.len() < min_impls {
        let n = out.len();
        let name = if n == 1 {
            "fallback".to_string()
        } else {
            format!("alt{}", n)
        };
        // Fallback: cheap stub read/write so ranking/errflow has another path.
        let body = if tags.iter().any(|t| t == "write" || t == "file") && after.is_empty() {
            "write(to=\"/tmp/ductile_hyper_fallback.txt\", content=\"fallback\")".into()
        } else if let Some(a) = after.first() {
            format!(
                "write(to=\"/tmp/ductile_hyper_{}_fb.txt\", content=@{})",
                stage.name, a
            )
        } else {
            "read(from=\"{topic}\")".into()
        };
        out.push((name, body, tags.clone()));
    }
    out
}

fn default_tags_for_role(role: &HyperRole) -> BTreeSet<String> {
    let mut t = BTreeSet::new();
    match role {
        HyperRole::Judge => {
            t.insert("judge".into());
            t.insert("gate".into());
        }
        HyperRole::Source => {
            t.insert("read".into());
            t.insert("file".into());
        }
        HyperRole::Sink => {
            t.insert("write".into());
            t.insert("file".into());
        }
        HyperRole::Default => {
            t.insert("work".into());
        }
    }
    t
}

fn primary_body(stage: &HyperStage, after: &[String], tags: &BTreeSet<String>) -> String {
    if matches!(stage.role, HyperRole::Judge) || tags.iter().any(|t| t == "judge" || t == "gate") {
        // Deterministic judge stub; embed @deps so DAG edges exist for after=.
        let deps = after
            .iter()
            .map(|a| format!("@{}", a))
            .collect::<Vec<_>>()
            .join(" ");
        return format!(
            "run(\"printf '%s\\n' '##DSL_RESULT' 'score=100' 'deps={}' '##DSL_END'\")",
            deps
        );
    }
    if tags.iter().any(|t| t == "llm" || t == "extract") {
        let src = after
            .first()
            .map(|a| format!("@{}", a))
            .unwrap_or_else(|| "\"{topic}\"".into());
        return format!(
            "llm(prompt={}, system=\"extract\", schema=\"title,url,topic\")",
            src
        );
    }
    if tags.iter().any(|t| t == "write") {
        let content = after
            .first()
            .map(|a| format!("@{}", a))
            .unwrap_or_else(|| "\"ok\"".into());
        return format!(
            "write(to=\"/tmp/ductile_hyper_{}.txt\", content={})",
            stage.name, content
        );
    }
    if tags.iter().any(|t| t == "read" || t == "file" || t == "io") {
        return "read(from=\"{topic}\")".into();
    }
    // Generic worker: pass through upstream or topic.
    if let Some(a) = after.first() {
        format!(
            "write(to=\"/tmp/ductile_hyper_{}.txt\", content=@{})",
            stage.name, a
        )
    } else {
        "read(from=\"{topic}\")".into()
    }
}

fn escape_quotes(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn write_pipeline_file(spec: &HyperSpec, path: &str) -> Result<(), String> {
    let text = emit_pipeline(spec);
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    std::fs::write(path, text).map_err(|e| e.to_string())
}

// ── Check generated / hand-written pipeline against hyper constraints ──

pub fn check_pipeline_against(spec: &HyperSpec, pl: &Pipeline) -> Vec<String> {
    let mut errs = Vec::new();
    let proc_names: BTreeSet<&str> = pl.procs.iter().map(|p| p.name.as_str()).collect();

    for stage in &spec.stages {
        if !proc_names.contains(stage.name.as_str()) {
            errs.push(format!("missing proc for hyper stage {:?}", stage.name));
            continue;
        }
        let proc = pl.procs.iter().find(|p| p.name == stage.name).unwrap();
        let enabled = proc.plan.iter().filter(|i| i.enabled).count();
        let need = stage.min_impls.max(spec.require.min_impls.max(1));
        if enabled < need {
            errs.push(format!(
                "stage {:?} has {} enabled impl(s), need min_impls={}",
                stage.name, enabled, need
            ));
        }
        let after = if stage.after.is_empty() {
            default_after_chain(spec)
                .get(&stage.name)
                .cloned()
                .unwrap_or_default()
        } else {
            stage.after.clone()
        };
        for dep in &after {
            let has = proc.plan.iter().any(|i| i.refs.iter().any(|r| r == dep))
                || proc.plan.iter().any(|i| {
                    i.when
                        .as_ref()
                        .map(|w| w.contains(&format!("@{}", dep)))
                        .unwrap_or(false)
                });
            // Also accept block-level when only for gated_by; for after, require body/@ref.
            let body_ref = proc.plan.iter().any(|i| {
                i.body_text.contains(&format!("@{}", dep)) || i.refs.iter().any(|r| r == dep)
            });
            if !body_ref && !has {
                errs.push(format!(
                    "stage {:?} should depend on {:?} via @ref (after=)",
                    stage.name, dep
                ));
            }
        }
        if let Some(g) = &stage.gated_by {
            let gated = proc.plan.iter().any(|i| {
                i.when
                    .as_ref()
                    .map(|w| w.contains(&format!("@{}", g)))
                    .unwrap_or(false)
            });
            if !gated {
                errs.push(format!(
                    "stage {:?} should be gated_by {:?} (.when(@{}.…))",
                    stage.name, g, g
                ));
            }
        }
    }

    // bundle: every member must exist as a proc (co-occurrence)
    for h in &spec.hedges {
        if h.kind != HedgeKind::Bundle {
            continue;
        }
        for m in &h.members {
            if !proc_names.contains(m.as_str()) {
                errs.push(format!(
                    "bundle {:?} requires proc {:?} (co-occurrence)",
                    h.name, m
                ));
            }
        }
    }

    if spec.require.judge {
        let has_judge_stage = spec
            .stages
            .iter()
            .any(|s| matches!(s.role, HyperRole::Judge));
        if !has_judge_stage {
            errs.push("require.judge=true but no stage with role=judge".into());
        } else {
            for s in spec
                .stages
                .iter()
                .filter(|s| matches!(s.role, HyperRole::Judge))
            {
                if !proc_names.contains(s.name.as_str()) {
                    errs.push(format!("judge stage {:?} missing in pipeline", s.name));
                }
            }
        }
    }

    let deliver = resolve_deliver(spec);
    let has_deliver = pl.procs.iter().any(|p| {
        p.deliver && (p.deliver_refs.iter().any(|r| r == &deliver) || p.name == "deliver")
    });
    // Accept either .proc("deliver").deliver(@x) or deliver flag on target.
    let deliver_ok = has_deliver
        || pl
            .procs
            .iter()
            .any(|p| p.deliver && p.deliver_refs.iter().any(|r| r == &deliver))
        || pl.procs.iter().any(|p| {
            p.name == "deliver"
                && (p.deliver
                    || p.deliver_refs.iter().any(|r| r == &deliver)
                    || p.plan.iter().any(|i| i.body_text.contains(&deliver)))
        });
    if !deliver_ok {
        // Soft: many pipelines mark deliver on a terminal proc.
        let terminal = pl.procs.iter().any(|p| p.deliver);
        if !terminal {
            errs.push(format!(
                "pipeline should deliver @{} (or mark a deliver proc)",
                deliver
            ));
        }
    }

    errs
}

/// Parse hyper + emit + parse_pipeline round-trip helper (for CLI/tests).
pub fn build_and_parse(spec: &HyperSpec) -> Result<Pipeline, String> {
    let text = emit_pipeline(spec);
    parse_pipeline(&text).map_err(|e: ParseError| e.to_string())
}

// ── Structural isomorphism (reuse index) ──
//
// Tags alone are a soft hint (easy collisions). Reuse matching keys off
// role + normalized edges + gate pattern; tag Jaccard only ranks hits.

/// Name-independent structural fingerprint of a hyper / inferred pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructSig {
    /// Topo-ordered roles (source|judge|sink|default).
    pub roles: Vec<&'static str>,
    /// Edges as (from_idx, to_idx) in topo order.
    pub edges: Vec<(usize, usize)>,
    /// Gate edges: consumer_idx gated by judge_idx.
    pub gates: Vec<(usize, usize)>,
    pub require_judge: bool,
    /// Per-stage tag sets (aligned with roles) — soft only.
    pub tags: Vec<BTreeSet<String>>,
    /// When present, true hypergraph isomorphism key (V roles × hedges).
    pub hypergraph_key: Option<String>,
}

impl StructSig {
    /// DAG projection key (pairwise edges/gates) — used vs pipelines.
    pub fn dag_key(&self) -> String {
        let roles = self.roles.join(">");
        let mut edges = self.edges.clone();
        edges.sort_unstable();
        let e: Vec<String> = edges.iter().map(|(a, b)| format!("{}-{}", a, b)).collect();
        let mut gates = self.gates.clone();
        gates.sort_unstable();
        let g: Vec<String> = gates.iter().map(|(c, j)| format!("{}<-{}", c, j)).collect();
        format!(
            "roles={}|edges={}|gates={}|judge={}",
            roles,
            e.join(","),
            g.join(","),
            if self.require_judge { 1 } else { 0 }
        )
    }

    /// Isomorphism key: hypergraph key when both sides are hypergraphs; else DAG key.
    pub fn structure_key(&self) -> String {
        self.hypergraph_key
            .clone()
            .unwrap_or_else(|| self.dag_key())
    }
}

#[derive(Debug, Clone)]
pub struct SimilarHit {
    pub path: String,
    pub name: String,
    pub kind: &'static str, // "hyper" | "pipeline"
    pub structure_match: bool,
    pub tag_jaccard: f64,
    pub structure_key: String,
    pub note: String,
}

pub fn struct_sig_from_hyper(spec: &HyperSpec) -> StructSig {
    // Prefer true hypergraph key; still expose DAG projection fields for near-match UX.
    let chain = default_after_chain(spec);
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, s) in spec.stages.iter().enumerate() {
        index.insert(s.name.as_str(), i);
    }

    let roles: Vec<&'static str> = spec.stages.iter().map(|s| s.role.as_str()).collect();
    let mut edges = Vec::new();
    let mut gates = Vec::new();
    let mut tags = Vec::new();

    for (i, s) in spec.stages.iter().enumerate() {
        let after = if s.after.is_empty() {
            chain.get(&s.name).cloned().unwrap_or_default()
        } else {
            s.after.clone()
        };
        for dep in &after {
            if let Some(&j) = index.get(dep.as_str()) {
                edges.push((j, i));
            }
        }
        if let Some(g) = &s.gated_by {
            if let Some(&j) = index.get(g.as_str()) {
                gates.push((i, j));
            }
        }
        tags.push(s.tags.clone());
    }

    let require_judge = spec.require.judge
        || spec
            .vertices
            .iter()
            .any(|v| matches!(v.role, HyperRole::Judge))
        || spec.hedges.iter().any(|h| h.kind == HedgeKind::Gate);

    // Prefer true hypergraph key for isomorphism when hedges exist.
    StructSig {
        roles,
        edges,
        gates,
        require_judge,
        tags,
        hypergraph_key: Some(spec.hypergraph_key()),
    }
}

/// Infer a structural sig from an executable pipeline (for reuse against .hyper).
pub fn struct_sig_from_pipeline(pl: &Pipeline) -> StructSig {
    let procs: Vec<&crate::ast::Proc> = pl
        .procs
        .iter()
        .filter(|p| !p.deliver && p.name != "deliver")
        .collect();
    let index: BTreeMap<&str, usize> = procs
        .iter()
        .enumerate()
        .map(|(i, p)| (p.name.as_str(), i))
        .collect();

    let mut roles = Vec::new();
    let mut edges = Vec::new();
    let mut gates = Vec::new();
    let mut tags_v = Vec::new();

    for (i, proc) in procs.iter().enumerate() {
        let tags = crate::ast::proc_tags(proc);
        let role = infer_role(proc, &tags);
        roles.push(role.as_str());
        tags_v.push(tags);

        // Separate when-gate judges from data @refs so .when(@j.score) does not
        // invent a spurious data edge judge→consumer (hyper gate projection doesn't).
        let mut gate_refs: BTreeSet<String> = BTreeSet::new();
        let mut refs: BTreeSet<String> = BTreeSet::new();
        for imp in &proc.plan {
            if let Some(w) = &imp.when {
                for r in crate::parser::extract_refs(w) {
                    if w.contains(".score") || w.contains(".ok") {
                        gate_refs.insert(r.clone());
                        if let Some(&j) = index.get(r.as_str()) {
                            if !gates.contains(&(i, j)) {
                                gates.push((i, j));
                            }
                        }
                    } else {
                        refs.insert(r);
                    }
                }
            }
        }
        for imp in &proc.plan {
            for r in &imp.refs {
                if !gate_refs.contains(r) {
                    refs.insert(r.clone());
                }
            }
        }
        for r in refs {
            if let Some(&j) = index.get(r.as_str()) {
                if j != i {
                    edges.push((j, i));
                }
            }
        }
    }

    let require_judge = roles.iter().any(|r| *r == "judge") || !gates.is_empty();

    StructSig {
        roles,
        edges,
        gates,
        require_judge,
        tags: tags_v,
        hypergraph_key: None,
    }
}

fn infer_role(proc: &crate::ast::Proc, tags: &BTreeSet<String>) -> HyperRole {
    let has = |k: &str| tags.iter().any(|t| t == k);
    if has("judge") || has("gate") {
        return HyperRole::Judge;
    }
    if has("write") && !has("read") {
        return HyperRole::Sink;
    }
    if (has("read") || has("io") || has("file")) && proc.plan.iter().all(|i| i.refs.is_empty()) {
        return HyperRole::Source;
    }
    if has("write") {
        return HyperRole::Sink;
    }
    HyperRole::Default
}

fn tag_jaccard(a: &[BTreeSet<String>], b: &[BTreeSet<String>]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    // Align by index when lengths match; else compare bag-union Jaccard.
    if a.len() == b.len() {
        let mut sum = 0.0;
        for (x, y) in a.iter().zip(b.iter()) {
            sum += set_jaccard(x, y);
        }
        return sum / a.len() as f64;
    }
    let mut ua = BTreeSet::new();
    let mut ub = BTreeSet::new();
    for s in a {
        ua.extend(s.iter().cloned());
    }
    for s in b {
        ub.extend(s.iter().cloned());
    }
    set_jaccard(&ua, &ub)
}

fn set_jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

pub fn score_similar(
    query: &StructSig,
    cand: &StructSig,
    path: &str,
    name: &str,
    kind: &'static str,
) -> SimilarHit {
    let both_hyper = query.hypergraph_key.is_some() && cand.hypergraph_key.is_some();
    let (qk, ck) = if both_hyper {
        (
            query.hypergraph_key.clone().unwrap(),
            cand.hypergraph_key.clone().unwrap(),
        )
    } else {
        (query.dag_key(), cand.dag_key())
    };
    let structure_match = qk == ck;
    let tag_jaccard = tag_jaccard(&query.tags, &cand.tags);
    let note = if structure_match {
        if both_hyper {
            "ordered typed hypergraph isomorphic (incidence key)".into()
        } else if kind == "pipeline" || query.hypergraph_key.is_some() {
            "DAG projection isomorphic (not hypergraph iso — pipeline has no hyperedges)".into()
        } else if tag_jaccard >= 0.5 {
            "structure isomorphic; tags agree (soft)".into()
        } else {
            "structure isomorphic; tags diverge (ok — tags are soft)".into()
        }
    } else if query.roles == cand.roles {
        "same role sequence; edge/gate/hyperedge pattern differs".into()
    } else {
        "not isomorphic (role/edge mismatch) — tag overlap ignored for reuse".into()
    };
    SimilarHit {
        path: path.into(),
        name: name.into(),
        kind,
        structure_match,
        tag_jaccard,
        structure_key: ck,
        note,
    }
}

/// Scan paths for `.hyper` / `.pipeline` and rank structural reuse candidates.
pub fn find_similar(
    query: &StructSig,
    _query_name: &str,
    roots: &[String],
) -> Result<Vec<SimilarHit>, String> {
    let mut hits = Vec::new();
    let mut files = Vec::new();
    for root in roots {
        collect_graph_files(Path::new(root), &mut files)?;
    }
    files.sort();
    files.dedup();

    for path in files {
        let path_str = path.to_string_lossy().to_string();
        if path.extension().and_then(|e| e.to_str()) == Some("hyper") {
            let h = match parse_hyper_file(&path_str) {
                Ok(h) => h,
                Err(_) => continue, // skip unreadable / intentionally-invalid fixtures
            };
            let sig = struct_sig_from_hyper(&h);
            hits.push(score_similar(query, &sig, &path_str, &h.name, "hyper"));
        } else if path.extension().and_then(|e| e.to_str()) == Some("pipeline") {
            let pl = match parse_pipeline_file(&path_str) {
                Ok(pl) => pl,
                Err(_) => continue,
            };
            let sig = struct_sig_from_pipeline(&pl);
            hits.push(score_similar(query, &sig, &path_str, &pl.name, "pipeline"));
        }
    }

    hits.sort_by(|a, b| {
        b.structure_match
            .cmp(&a.structure_match)
            .then(
                b.tag_jaccard
                    .partial_cmp(&a.tag_jaccard)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.path.cmp(&b.path))
    });
    Ok(hits)
}

/// Agent-facing JSON for LLM graph builders.
/// `reuse_action` tells the model what to do with each hit.
pub fn similar_report_json(
    query_path: &str,
    query_name: &str,
    query_sig: &StructSig,
    hits: &[SimilarHit],
) -> String {
    let esc = |s: &str| {
        let mut out = String::new();
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    };
    let mut out = String::from("{");
    out.push_str(&format!(
        "\"query_path\":\"{}\",\"query_name\":\"{}\",\"dag_key\":\"{}\",",
        esc(query_path),
        esc(query_name),
        esc(&query_sig.dag_key())
    ));
    match &query_sig.hypergraph_key {
        Some(hk) => out.push_str(&format!("\"hypergraph_key\":\"{}\",", esc(hk))),
        None => out.push_str("\"hypergraph_key\":null,"),
    }
    out.push_str(&format!(
        "\"structure_key\":\"{}\",",
        esc(&query_sig.structure_key())
    ));
    out.push_str(
        "\"match_rule\":\"hyper↔hyper: hypergraph_key (ordered typed incidence); hyper↔pipeline or pipeline↔pipeline: dag_key only; tags soft rank; never claim hypergraph iso for pipeline hits\",",
    );
    out.push_str(
        "\"agent_instruction\":\"Before writing a new .hyper/.pipeline: if any hit has isomorphic=true, REUSE that path (hyper check + patch impl bodies) instead of inventing topology. If reuse_action=adapt_topology, keep role sequence and fix edges/gates. Never treat tag overlap alone as permission to reuse.\",",
    );
    out.push_str("\"hits\":[");
    let mut first = true;
    for h in hits {
        if h.path.replace('\\', "/") == query_path.replace('\\', "/") {
            continue;
        }
        if !h.structure_match && !h.note.starts_with("same role sequence") {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        let reuse = if h.structure_match {
            "reuse_pipeline"
        } else {
            "adapt_topology"
        };
        out.push_str(&format!(
            "{{\"isomorphic\":{},\"reuse_action\":\"{}\",\"kind\":\"{}\",\"name\":\"{}\",\"path\":\"{}\",\"tag_jaccard\":{:.4},\"structure_key\":\"{}\",\"note\":\"{}\"}}",
            h.structure_match,
            reuse,
            h.kind,
            esc(&h.name),
            esc(&h.path),
            h.tag_jaccard,
            esc(&h.structure_key),
            esc(&h.note)
        ));
    }
    out.push_str("]}");
    out
}

/// Resolve query file + scan roots → JSON report (CLI / Python / agents).
pub fn similar_json(query_path: &str, roots: &[String]) -> Result<String, String> {
    let (qsig, qname) = if query_path.ends_with(".hyper") {
        let h = parse_hyper_file(query_path).map_err(|e| e.to_string())?;
        (struct_sig_from_hyper(&h), h.name)
    } else if query_path.ends_with(".pipeline") {
        let pl = parse_pipeline_file(query_path).map_err(|e| e.to_string())?;
        (struct_sig_from_pipeline(&pl), pl.name)
    } else {
        return Err("query must be .hyper or .pipeline".into());
    };
    let scan = if roots.is_empty() {
        vec![
            "examples".into(),
            "examples/hyper".into(),
            "examples/scripts".into(),
        ]
    } else {
        roots.to_vec()
    };
    let hits = find_similar(&qsig, &qname, &scan)?;
    Ok(similar_report_json(query_path, &qname, &qsig, &hits))
}

// ── Node-level reuse (proc / stage) ──
//
// Workflow iso reuses whole graphs; node iso reuses a single stage/proc.
// Key = role + op family + in-arity + gated + min_impls_bucket — tags soft only.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSig {
    pub role: &'static str,
    /// Primary builtin/script family: read|write|llm|run|script|search|merge|unknown
    pub op: String,
    /// Distinct data @refs (inbound arity).
    pub in_arity: usize,
    pub gated: bool,
    /// 1 = single path; 2 = has fallback pressure (≥2 impls)
    pub impl_bucket: u8,
    pub tags: BTreeSet<String>,
}

impl NodeSig {
    pub fn node_key(&self) -> String {
        format!(
            "role={}|op={}|in={}|gated={}|impls={}",
            self.role,
            self.op,
            self.in_arity,
            if self.gated { 1 } else { 0 },
            self.impl_bucket
        )
    }
}

#[derive(Debug, Clone)]
pub struct NodeHit {
    pub path: String,
    pub graph_name: String,
    pub node_name: String,
    pub kind: &'static str, // hyper-stage | pipeline-proc
    pub structure_match: bool,
    pub tag_jaccard: f64,
    pub node_key: String,
    pub note: String,
    pub body_preview: String,
}

pub fn node_sig_from_stage(stage: &HyperStage) -> NodeSig {
    let tags = if stage.tags.is_empty() {
        default_tags_for_role(&stage.role)
    } else {
        stage.tags.clone()
    };
    let op = op_from_tags_role(&tags, &stage.role);
    let in_arity = stage.after.len();
    NodeSig {
        role: stage.role.as_str(),
        op,
        in_arity,
        gated: stage.gated_by.is_some(),
        impl_bucket: if stage.min_impls >= 2 { 2 } else { 1 },
        tags,
    }
}

pub fn node_sig_from_proc(proc: &crate::ast::Proc) -> NodeSig {
    let tags = crate::ast::proc_tags(proc);
    let enabled: Vec<_> = proc.plan.iter().filter(|i| i.enabled).collect();
    let op = enabled
        .first()
        .map(|i| op_from_body(&i.body_text))
        .filter(|o| o != "unknown")
        .unwrap_or_else(|| {
            let role_guess = infer_role(proc, &tags);
            op_from_tags_role(&tags, &role_guess)
        });
    let mut refs = BTreeSet::new();
    let mut gated = false;
    for imp in &enabled {
        for r in &imp.refs {
            refs.insert(r.clone());
        }
        if let Some(w) = &imp.when {
            gated = true;
            for r in crate::parser::extract_refs(w) {
                if !(w.contains(".score") || w.contains(".ok")) {
                    refs.insert(r);
                }
            }
        }
    }
    let in_arity = refs.len();
    // Prefer body-derived role when tags are missing/misleading
    let role = {
        let from_tags = infer_role(proc, &tags);
        if matches!(from_tags, HyperRole::Default) {
            match op.as_str() {
                "read" | "ls" | "stat" | "exists" if in_arity == 0 => HyperRole::Source,
                "write" => HyperRole::Sink,
                "run" if tags.iter().any(|t| t == "judge" || t == "gate") => HyperRole::Judge,
                _ => from_tags,
            }
        } else {
            from_tags
        }
    };
    let impl_bucket = if enabled.len() >= 2 { 2 } else { 1 };
    NodeSig {
        role: role.as_str(),
        op,
        in_arity,
        gated,
        impl_bucket,
        tags,
    }
}

fn op_from_body(body: &str) -> String {
    // Scan for known_fn( even if modifiers precede (shouldn't) or body is dirty
    let bytes = body.as_bytes();
    let names = [
        "llm", "read", "write", "run", "script", "search", "merge", "spawn", "exists", "mkdir",
        "cp", "ls", "stat",
    ];
    for name in names {
        let mut start = 0;
        while let Some(rel) = body[start..].find(name) {
            let i = start + rel;
            let after = i + name.len();
            let boundary_ok = i == 0
                || matches!(
                    bytes.get(i - 1).copied().unwrap_or(b' '),
                    b' ' | b'\t' | b'\n' | b'(' | b',' | b'>' | b'='
                );
            if boundary_ok && body[after..].starts_with('(') {
                return name.to_string();
            }
            start = i + 1;
        }
    }
    "unknown".into()
}

fn op_from_tags_role(tags: &BTreeSet<String>, role: &HyperRole) -> String {
    let has = |k: &str| tags.iter().any(|t| t == k);
    if has("llm") || has("extract") {
        return "llm".into();
    }
    if has("write") {
        return "write".into();
    }
    if has("read") || has("io") {
        return "read".into();
    }
    if has("judge") || has("gate") || matches!(role, HyperRole::Judge) {
        return "run".into();
    }
    if has("search") {
        return "search".into();
    }
    match role {
        HyperRole::Source => "read".into(),
        HyperRole::Sink => "write".into(),
        HyperRole::Judge => "run".into(),
        HyperRole::Default => "unknown".into(),
    }
}

fn body_preview_proc(proc: &crate::ast::Proc) -> String {
    proc.plan
        .iter()
        .find(|i| i.enabled)
        .map(|i| crate::trunc_chars(&i.body_text, 80).to_string())
        .unwrap_or_default()
}

pub fn score_node(
    query: &NodeSig,
    cand: &NodeSig,
    path: &str,
    graph: &str,
    node: &str,
    kind: &'static str,
    preview: &str,
) -> NodeHit {
    let qk = query.node_key();
    let ck = cand.node_key();
    let structure_match = qk == ck;
    let tag_jaccard = set_jaccard(&query.tags, &cand.tags);
    let note = if structure_match {
        "node isomorphic — reuse this proc/stage impl".into()
    } else if query.role == cand.role && query.op == cand.op {
        "same role+op; arity/gate/impls differ — adapt ports".into()
    } else if query.role == cand.role {
        "same role only — weak hint".into()
    } else {
        "not a node match".into()
    };
    NodeHit {
        path: path.into(),
        graph_name: graph.into(),
        node_name: node.into(),
        kind,
        structure_match,
        tag_jaccard,
        node_key: ck,
        note,
        body_preview: preview.into(),
    }
}

#[derive(Debug, Clone, Default)]
pub struct NodeQuery {
    pub role: Option<String>,
    pub op: Option<String>,
    pub in_arity: Option<usize>,
    pub gated: Option<bool>,
}

impl NodeQuery {
    fn matches_filter(&self, sig: &NodeSig) -> bool {
        if let Some(r) = &self.role {
            if sig.role != r.as_str() {
                return false;
            }
        }
        if let Some(o) = &self.op {
            if sig.op != *o {
                return false;
            }
        }
        if let Some(a) = self.in_arity {
            if sig.in_arity != a {
                return false;
            }
        }
        if let Some(g) = self.gated {
            if sig.gated != g {
                return false;
            }
        }
        true
    }
}

/// Enumerate all nodes under roots (hyper stages + pipeline procs).
pub fn collect_nodes(roots: &[String]) -> Result<Vec<(NodeSig, NodeHit)>, String> {
    let mut files = Vec::new();
    for root in roots {
        collect_graph_files(Path::new(root), &mut files)?;
    }
    files.sort();
    files.dedup();
    let mut out = Vec::new();
    for path in files {
        let path_str = path.to_string_lossy().to_string();
        if path.extension().and_then(|e| e.to_str()) == Some("hyper") {
            let h = match parse_hyper_file(&path_str) {
                Ok(h) => h,
                Err(_) => continue,
            };
            for s in &h.stages {
                let sig = node_sig_from_stage(s);
                let hit = score_node(&sig, &sig, &path_str, &h.name, &s.name, "hyper-stage", "");
                out.push((sig, hit));
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("pipeline") {
            let pl = match parse_pipeline_file(&path_str) {
                Ok(pl) => pl,
                Err(_) => continue,
            };
            for proc in &pl.procs {
                if proc.deliver || proc.name == "deliver" {
                    continue;
                }
                let sig = node_sig_from_proc(proc);
                let preview = body_preview_proc(proc);
                let hit = score_node(
                    &sig,
                    &sig,
                    &path_str,
                    &pl.name,
                    &proc.name,
                    "pipeline-proc",
                    &preview,
                );
                out.push((sig, hit));
            }
        }
    }
    Ok(out)
}

pub fn find_similar_nodes(
    query: &NodeSig,
    roots: &[String],
    exclude_path_node: Option<(&str, &str)>,
) -> Result<Vec<NodeHit>, String> {
    let catalog = collect_nodes(roots)?;
    let mut hits = Vec::new();
    for (sig, meta) in catalog {
        if let Some((ep, en)) = exclude_path_node {
            if meta.path.replace('\\', "/") == ep.replace('\\', "/") && meta.node_name == en {
                continue;
            }
        }
        hits.push(score_node(
            query,
            &sig,
            &meta.path,
            &meta.graph_name,
            &meta.node_name,
            meta.kind,
            &meta.body_preview,
        ));
    }
    hits.sort_by(|a, b| {
        b.structure_match
            .cmp(&a.structure_match)
            .then(
                b.tag_jaccard
                    .partial_cmp(&a.tag_jaccard)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.path.cmp(&b.path))
            .then(a.node_name.cmp(&b.node_name))
    });
    Ok(hits)
}

pub fn find_nodes_by_filter(filter: &NodeQuery, roots: &[String]) -> Result<Vec<NodeHit>, String> {
    let catalog = collect_nodes(roots)?;
    let mut hits: Vec<NodeHit> = catalog
        .into_iter()
        .filter(|(sig, _)| filter.matches_filter(sig))
        .map(|(sig, meta)| {
            let mut h = meta;
            h.node_key = sig.node_key();
            h.structure_match = true; // matched filter
            h.note = "matched node query filter".into();
            h
        })
        .collect();
    hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.node_name.cmp(&b.node_name)));
    Ok(hits)
}

pub fn nodes_report_json(
    query_label: &str,
    query_key: &str,
    hits: &[NodeHit],
    mode: &str,
) -> String {
    let esc = |s: &str| {
        let mut out = String::new();
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out
    };
    let mut out = String::from("{");
    out.push_str(&format!(
        "\"mode\":\"{}\",\"query\":\"{}\",\"node_key\":\"{}\",",
        esc(mode),
        esc(query_label),
        esc(query_key)
    ));
    out.push_str(
        "\"match_rule\":\"node_key = role + op + in_arity + gated + impl_bucket; tags soft only\",",
    );
    out.push_str(
        "\"agent_instruction\":\"When building a graph node: call node reuse before writing a new proc. isomorphic=true → reuse_action=reuse_node (copy/adapt that proc's plan). same role+op → adapt_ports. Prefer pipeline-proc hits with body_preview. Tags alone are not reuse permission. For whole-graph reuse use hyper similar.\",",
    );
    out.push_str("\"hits\":[");
    let mut first = true;
    let mut n = 0;
    for h in hits {
        let keep = h.structure_match || h.note.starts_with("same role+op") || mode == "filter";
        if !keep {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        let reuse = if h.structure_match && mode != "filter" {
            "reuse_node"
        } else if h.note.starts_with("same role+op") {
            "adapt_ports"
        } else if mode == "filter" {
            "candidate_node"
        } else {
            "inspect"
        };
        out.push_str(&format!(
            "{{\"isomorphic\":{},\"reuse_action\":\"{}\",\"kind\":\"{}\",\"graph\":\"{}\",\"node\":\"{}\",\"path\":\"{}\",\"tag_jaccard\":{:.4},\"node_key\":\"{}\",\"body_preview\":\"{}\",\"note\":\"{}\"}}",
            h.structure_match,
            reuse,
            h.kind,
            esc(&h.graph_name),
            esc(&h.node_name),
            esc(&h.path),
            h.tag_jaccard,
            esc(&h.node_key),
            esc(&h.body_preview),
            esc(&h.note)
        ));
        n += 1;
        if n >= 40 {
            break;
        }
    }
    out.push_str("]}");
    out
}

/// `query` = path, or `path:node`, or empty when using filter-only via NodeQuery.
pub fn nodes_json(
    query: &str,
    roots: &[String],
    filter: Option<&NodeQuery>,
) -> Result<String, String> {
    let scan = if roots.is_empty() {
        vec![
            "examples".into(),
            "examples/hyper".into(),
            "examples/scripts".into(),
        ]
    } else {
        roots.to_vec()
    };

    if let Some(f) = filter {
        if f.role.is_some() || f.op.is_some() || f.in_arity.is_some() || f.gated.is_some() {
            let hits = find_nodes_by_filter(f, &scan)?;
            let label = format!("{:?}", f);
            return Ok(nodes_report_json(&label, "filter", &hits, "filter"));
        }
    }

    let (path, node_opt) = split_path_node(query);
    if path.is_empty() {
        return Err(
            "hyper nodes needs <file.hyper|file.pipeline>[:node] or --role/--op filter".into(),
        );
    }

    if path.ends_with(".hyper") {
        let h = parse_hyper_file(path).map_err(|e| e.to_string())?;
        if let Some(n) = node_opt {
            let stage = h
                .stages
                .iter()
                .find(|s| s.name == n)
                .ok_or_else(|| format!("stage {:?} not in {}", n, path))?;
            let sig = node_sig_from_stage(stage);
            let hits = find_similar_nodes(&sig, &scan, Some((path, n)))?;
            return Ok(nodes_report_json(
                &format!("{}:{}", path, n),
                &sig.node_key(),
                &hits,
                "similar",
            ));
        }
        // all stages
        let mut combined = String::from("{\"mode\":\"list\",\"query\":\"");
        combined.push_str(&path.replace('\\', "\\\\").replace('"', "\\\""));
        combined.push_str("\",\"nodes\":[");
        let mut first = true;
        for s in &h.stages {
            let sig = node_sig_from_stage(s);
            let hits = find_similar_nodes(&sig, &scan, Some((path, &s.name)))?;
            let one = nodes_report_json(
                &format!("{}:{}", path, s.name),
                &sig.node_key(),
                &hits,
                "similar",
            );
            if !first {
                combined.push(',');
            }
            first = false;
            combined.push_str(&one);
        }
        combined.push_str("]}");
        return Ok(combined);
    }

    if path.ends_with(".pipeline") {
        let pl = parse_pipeline_file(path).map_err(|e| e.to_string())?;
        if let Some(n) = node_opt {
            let proc = pl
                .procs
                .iter()
                .find(|p| p.name == n)
                .ok_or_else(|| format!("proc {:?} not in {}", n, path))?;
            let sig = node_sig_from_proc(proc);
            let hits = find_similar_nodes(&sig, &scan, Some((path, n)))?;
            return Ok(nodes_report_json(
                &format!("{}:{}", path, n),
                &sig.node_key(),
                &hits,
                "similar",
            ));
        }
        let mut combined = String::from("{\"mode\":\"list\",\"query\":\"");
        combined.push_str(&path.replace('\\', "\\\\").replace('"', "\\\""));
        combined.push_str("\",\"nodes\":[");
        let mut first = true;
        for proc in &pl.procs {
            if proc.deliver || proc.name == "deliver" {
                continue;
            }
            let sig = node_sig_from_proc(proc);
            let hits = find_similar_nodes(&sig, &scan, Some((path, &proc.name)))?;
            let one = nodes_report_json(
                &format!("{}:{}", path, proc.name),
                &sig.node_key(),
                &hits,
                "similar",
            );
            if !first {
                combined.push(',');
            }
            first = false;
            combined.push_str(&one);
        }
        combined.push_str("]}");
        return Ok(combined);
    }

    Err("query must be .hyper/.pipeline or path:node".into())
}

fn split_path_node(query: &str) -> (&str, Option<&str>) {
    // Windows paths have drive colon — only split on last ":node" if suffix has no slash/backslash
    if let Some(i) = query.rfind(':') {
        let (left, right) = query.split_at(i);
        let node = &right[1..];
        if !node.is_empty()
            && !node.contains('/')
            && !node.contains('\\')
            && !node.contains('.')
            && (left.ends_with(".pipeline") || left.ends_with(".hyper"))
        {
            return (left, Some(node));
        }
    }
    (query, None)
}

fn collect_graph_files(root: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<(), String> {
    if root.is_file() {
        if let Some(ext) = root.extension().and_then(|e| e.to_str()) {
            if ext == "hyper" || ext == "pipeline" {
                out.push(root.to_path_buf());
            }
        }
        return Ok(());
    }
    if !root.is_dir() {
        return Ok(());
    }
    // v0.15 fix: 扫描根本身不可读 → Err（调用方给的是明确路径，读不到是真错误）；
    // 但**子目录**不可读只跳过（/tmp 下 systemd-private-* 是常态，
    // 一个权限目录毒死整条 similar 扫描 = drill [4] 挂点根因）。
    let entries =
        std::fs::read_dir(root).map_err(|e| format!("read_dir {}: {}", root.display(), e))?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "target" || name == ".git" || name == "node_modules" {
                continue;
            }
            // 不可读子目录：跳过不递归（symlink 环与权限目录都归这类）
            let readable = std::fs::read_dir(&p);
            if readable.is_err() {
                continue;
            }
            collect_graph_files(&p, out)?;
        } else {
            collect_graph_files(&p, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check_pipeline;

    const SAMPLE: &str = r#"
Hyper("unstructured_extract")
  .goal("blurb -> title,url,topic")
  .require(judge=true, min_impls=2)
  .stage("load", role=source, tags=#read,#file)
  .stage("extract", tags=#llm,#extract, after=load)
  .stage("gate", role=judge, tags=#gate, after=extract)
  .stage("report", role=sink, tags=#write,#file, after=extract, gated_by=gate)
  .deliver(report)
"#;

    #[test]
    fn parse_tags_with_commas() {
        let h = parse_hyper(
            r#"
Hyper("t")
  .stage("load", tags=#read,#file)
"#,
        )
        .unwrap();
        assert!(h.stages[0].tags.contains("read"));
        assert!(h.stages[0].tags.contains("file"));
    }

    #[test]
    fn parse_sample_hyper() {
        let h = parse_hyper(SAMPLE).expect("parse");
        assert_eq!(h.name, "unstructured_extract");
        assert!(h.require.judge);
        assert_eq!(h.require.min_impls, 2);
        assert_eq!(h.stages.len(), 4);
        assert_eq!(h.stages[3].gated_by.as_deref(), Some("gate"));
        assert_eq!(h.deliver.as_deref(), Some("report"));
    }

    #[test]
    fn emit_round_trip_typechecks() {
        let h = parse_hyper(SAMPLE).unwrap();
        let text = emit_pipeline(&h);
        let pl = parse_pipeline(&text).expect(&text);
        let errs = check_pipeline(&pl);
        assert!(errs.is_empty(), "typecheck: {:?} text=\n{}", errs, text);
        let hyper_errs = check_pipeline_against(&h, &pl);
        assert!(
            hyper_errs.is_empty(),
            "hyper check: {:?} text=\n{}",
            hyper_errs,
            text
        );
    }

    #[test]
    fn reject_unknown_after() {
        let bad = r#"
Hyper("x")
  .stage("a", tags=#read)
  .stage("b", after=missing)
"#;
        assert!(parse_hyper(bad).is_err());
    }

    #[test]
    fn check_catches_missing_fallback() {
        let h = parse_hyper(SAMPLE).unwrap();
        let thin = r#"
Pipeline("unstructured_extract")
  .proc("load").plan(p -> read(from="{topic}"))
  .proc("extract").plan(p -> llm(prompt=@load, schema="title"))
  .proc("gate").plan(g -> run("echo ok"))
  .proc("report").when(@gate.score >= 80).plan(w -> write(to="o.txt", content=@extract))
  .proc("deliver").deliver(@report)
"#;
        let pl = parse_pipeline(thin).unwrap();
        let errs = check_pipeline_against(&h, &pl);
        assert!(
            errs.iter().any(|e| e.contains("min_impls")),
            "errs={:?}",
            errs
        );
    }

    #[test]
    fn structure_iso_ignores_names_and_tags() {
        let a = parse_hyper(
            r#"
Hyper("a")
  .require(judge=true)
  .stage("load", role=source, tags=#read)
  .stage("work", tags=#llm, after=load)
  .stage("gate", role=judge, tags=#gate, after=work)
  .stage("out", role=sink, tags=#write, after=work, gated_by=gate)
"#,
        )
        .unwrap();
        // Same structure, different names + tags → still isomorphic
        let b = parse_hyper(
            r#"
Hyper("b")
  .require(judge=true)
  .stage("ingest", role=source, tags=#web,#http)
  .stage("nlp", tags=#gpu, after=ingest)
  .stage("judge", role=judge, tags=#qa, after=nlp)
  .stage("save", role=sink, tags=#s3, after=nlp, gated_by=judge)
"#,
        )
        .unwrap();
        let sa = struct_sig_from_hyper(&a);
        let sb = struct_sig_from_hyper(&b);
        assert_eq!(sa.structure_key(), sb.structure_key());
        let hit = score_similar(&sa, &sb, "b.hyper", "b", "hyper");
        assert!(hit.structure_match);
        assert!(
            hit.tag_jaccard < 0.5,
            "tags should diverge: {}",
            hit.tag_jaccard
        );
    }

    #[test]
    fn tag_collision_is_not_structure_match() {
        // Same tags, different topology → must NOT structure-match
        let a = parse_hyper(
            r#"
Hyper("linear")
  .stage("x", role=source, tags=#llm,#search)
  .stage("y", role=sink, tags=#write, after=x)
"#,
        )
        .unwrap();
        let b = parse_hyper(
            r#"
Hyper("branched")
  .stage("x", role=source, tags=#llm,#search)
  .stage("m", tags=#llm,#search, after=x)
  .stage("y", role=sink, tags=#write, after=m)
"#,
        )
        .unwrap();
        let sa = struct_sig_from_hyper(&a);
        let sb = struct_sig_from_hyper(&b);
        assert_ne!(sa.structure_key(), sb.structure_key());
        let hit = score_similar(&sa, &sb, "b.hyper", "branched", "hyper");
        assert!(!hit.structure_match);
    }

    #[test]
    fn node_iso_same_role_op_arity() {
        let pl = parse_pipeline(
            r#"
Pipeline("a")
  .proc("load")
    .plan(
      p -> read(from="x.txt")
    )
"#,
        )
        .unwrap();
        assert!(!pl.procs.is_empty(), "procs={}", pl.procs.len());
        let load = node_sig_from_proc(&pl.procs[0]);
        assert_eq!(
            load.op, "read",
            "body={:?} tags={:?}",
            pl.procs[0].plan[0].body_text, pl.procs[0].plan[0].tags
        );
        assert_eq!(load.in_arity, 0);
        assert_eq!(load.role, "source");

        let pl2 = parse_pipeline(
            r#"
Pipeline("b")
  .proc("ingest")
    .plan(
      p -> read(from="y.txt")
    )
"#,
        )
        .unwrap();
        let ingest = node_sig_from_proc(&pl2.procs[0]);
        assert_eq!(load.node_key(), ingest.node_key());
    }

    #[test]
    fn hypergraph_hedges_project_and_key() {
        let h = parse_hyper(
            r#"
HyperGraph("g")
  .require(judge=true)
  .vertex("a", role=source)
  .vertex("b", role=default)
  .vertex("j", role=judge)
  .vertex("c", role=sink)
  .hedge("flow", kind=chain, a, b, c)
  .hedge("q", kind=gate, judge=j, producers=b, consumers=c)
  .deliver(c)
"#,
        )
        .unwrap();
        assert_eq!(h.vertices.len(), 4);
        assert_eq!(h.hedges.len(), 2);
        assert!(h.hedges.iter().any(|e| e.kind == HedgeKind::Chain));
        assert!(h.hedges.iter().any(|e| e.kind == HedgeKind::Gate));
        let c = h.stages.iter().find(|s| s.name == "c").unwrap();
        assert_eq!(c.gated_by.as_deref(), Some("j"));
        assert!(c.after.iter().any(|x| x == "b"));
        // gate must NOT force judge as data after= on consumer
        assert!(!c.after.iter().any(|x| x == "j"));
        let j = h.stages.iter().find(|s| s.name == "j").unwrap();
        assert!(j.after.iter().any(|x| x == "b"), "producers→judge");
        let key = h.hypergraph_key();
        assert!(key.contains("chain:"), "{}", key);
        assert!(key.contains("gate:"), "{}", key);

        // Rename vertices, same roles/hedge pattern → same hypergraph key
        let h2 = parse_hyper(
            r#"
HyperGraph("g2")
  .require(judge=true)
  .vertex("in", role=source)
  .vertex("mid", role=default)
  .vertex("judge", role=judge)
  .vertex("out", role=sink)
  .hedge("flow", kind=chain, in, mid, out)
  .hedge("q", kind=gate, judge=judge, producers=mid, consumers=out)
  .deliver(out)
"#,
        )
        .unwrap();
        assert_eq!(h.hypergraph_key(), h2.hypergraph_key());
    }

    #[test]
    fn legacy_desugar_matches_explicit_hypergraph_key() {
        let legacy = parse_hyper(
            r#"
Hyper("legacy")
  .require(judge=true)
  .stage("ingest", role=source)
  .stage("nlp", after=ingest)
  .stage("judge", role=judge, after=nlp)
  .stage("save", role=sink, after=nlp, gated_by=judge)
"#,
        )
        .unwrap();
        let explicit = parse_hyper(
            r#"
HyperGraph("explicit")
  .require(judge=true)
  .vertex("ingest", role=source)
  .vertex("nlp", role=default)
  .vertex("judge", role=judge)
  .vertex("save", role=sink)
  .hedge("flow", kind=chain, ingest, nlp, save)
  .hedge("q", kind=gate, judge=judge, producers=nlp, consumers=save)
  .deliver(save)
"#,
        )
        .unwrap();
        assert_eq!(
            legacy.hypergraph_key(),
            explicit.hypergraph_key(),
            "legacy={}\nexplicit={}\nlegacy hedges={:?}\nexplicit hedges={:?}",
            legacy.hypergraph_key(),
            explicit.hypergraph_key(),
            legacy
                .hedges
                .iter()
                .map(|h| format!("{}:{:?}", h.kind.as_str(), h.members))
                .collect::<Vec<_>>(),
            explicit
                .hedges
                .iter()
                .map(|h| format!("{}:{:?}", h.kind.as_str(), h.members))
                .collect::<Vec<_>>(),
        );
        let save = legacy.stages.iter().find(|s| s.name == "save").unwrap();
        assert_eq!(save.gated_by.as_deref(), Some("judge"));
        assert!(!save.after.iter().any(|a| a == "judge"));
    }

    #[test]
    fn emit_pipeline_dag_matches_hyper_projection() {
        let h = parse_hyper(
            r#"
HyperGraph("g")
  .require(judge=true)
  .vertex("a", role=source)
  .vertex("b", role=default)
  .vertex("j", role=judge)
  .vertex("c", role=sink)
  .hedge("flow", kind=chain, a, b, c)
  .hedge("q", kind=gate, judge=j, producers=b, consumers=c)
  .deliver(c)
"#,
        )
        .unwrap();
        let pl = build_and_parse(&h).unwrap();
        let sh = struct_sig_from_hyper(&h);
        let sp = struct_sig_from_pipeline(&pl);
        assert_eq!(
            sh.dag_key(),
            sp.dag_key(),
            "hyper dag={}\npipeline dag={}\nemit=\n{}",
            sh.dag_key(),
            sp.dag_key(),
            emit_pipeline(&h)
        );
        // when-gate must not invent judge→consumer data edge
        assert!(
            !sp.edges.iter().any(|&(from, to)| {
                sp.roles.get(from) == Some(&"judge") && sp.roles.get(to) == Some(&"sink")
            }),
            "spurious judge→sink edge: {:?}",
            sp.edges
        );
    }

    #[test]
    fn gate_rejects_positional_members() {
        let err = parse_hyper(
            r#"
HyperGraph("bad")
  .vertex("a")
  .vertex("j", role=judge)
  .vertex("c")
  .hedge("q", kind=gate, a, j, c)
"#,
        )
        .unwrap_err();
        assert!(
            err.msg.contains("ports") || err.msg.contains("judge="),
            "{}",
            err.msg
        );
    }

    #[test]
    fn xor_suppresses_alts_one_slot() {
        let h = parse_hyper(
            r#"
HyperGraph("x")
  .vertex("live", role=default, tags=#llm)
  .vertex("stub", role=default, tags=#stub)
  .vertex("out", role=sink, tags=#write)
  .hedge("alts", kind=xor, live, stub)
  .hedge("flow", kind=chain, live, out)
  .deliver(out)
"#,
        )
        .unwrap();
        let names: Vec<_> = h.stages.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"live"));
        assert!(names.contains(&"out"));
        assert!(
            !names.contains(&"stub"),
            "xor alt must not be a stage: {:?}",
            names
        );
        let live = h.stages.iter().find(|s| s.name == "live").unwrap();
        assert!(live.min_impls >= 2, "slot min_impls={}", live.min_impls);
    }

    #[test]
    fn bundle_enforced_in_check() {
        let h = parse_hyper(
            r#"
HyperGraph("b")
  .vertex("a", role=source)
  .vertex("c", role=sink)
  .hedge("flow", kind=chain, a, c)
  .hedge("pack", kind=bundle, a, c)
  .deliver(c)
"#,
        )
        .unwrap();
        let pl = build_and_parse(&h).unwrap();
        assert!(check_pipeline_against(&h, &pl).is_empty());

        // Drop proc "a" from a minimal pipeline → bundle fails
        let broken = parse_pipeline(
            r#"
Pipeline("broken")
  .proc("c")
    .plan(p -> write(to="/tmp/x", content="ok"))
  .proc("deliver")
    .deliver(@c)
"#,
        )
        .unwrap();
        let errs = check_pipeline_against(&h, &broken);
        assert!(
            errs.iter().any(|e| e.contains("bundle") && e.contains("a")),
            "{:?}",
            errs
        );
    }
}
