//! E-graph: union-find e-classes + hashcons e-nodes + equality saturation.
//!
//! v0.10 重写。此前版本只是 DAG+拓扑分层（名为 e-graph 实为调度图）。
//! 现在补上真正的三件套：
//!   1. union-find 维护等价类（e-class）
//!   2. hashcons 的 e-node：proc 的每个 impl 投影为 (op, children)，
//!      op = body 检测到的函数名，children = @ref 的 canonical class id
//!   3. equality saturation（v0.10 为 union-only，不合成新节点，可证终止）：
//!        R1 同构合并: 两 class 的 canonical 节点集一致 → union
//!        R2 merge 扁平化: merge 节点的传递扁平子集相等 → union
//!           （涵盖交换律 merge(@a,@b)≡merge(@b,@a) 与嵌套折叠）
//!        R3 write/read 对消: read(from=P) ≡ write(to=P,content=@src) 的 src
//!           （即 write→read→下游 融合为 pass-through）
//!   提取器在 e-class 上做 per-class 最小静态 effective cost 选择，
//!   并给出 CSE 别名表（同 class 的 proc 只执行代表，其余复用结果）。
//!
//! 兼容层：build_egraph / parallel_groups / critical_path 保留原签名，
//! 内部改为在 e-class 压缩图上计算，executor/cli 不感知重构。

use crate::ast::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

// ── Union-find over e-class ids ──

#[derive(Debug, Clone, Default)]
pub struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    pub fn new() -> Self {
        UnionFind {
            parent: vec![],
            rank: vec![],
        }
    }

    pub fn make_set(&mut self) -> usize {
        let id = self.parent.len();
        self.parent.push(id);
        self.rank.push(0);
        id
    }

    /// 带路径压缩的查找（需要 &mut）。
    pub fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            let root = self.find(self.parent[x]);
            self.parent[x] = root;
        }
        self.parent[x]
    }

    /// 只读查找（不动路径），供只读遍历用。
    pub fn find_imm(&self, mut x: usize) -> usize {
        while self.parent[x] != x {
            x = self.parent[x];
        }
        x
    }

    pub fn union(&mut self, a: usize, b: usize) -> usize {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return ra;
        }
        if self.rank[ra] < self.rank[rb] {
            self.parent[ra] = rb;
            rb
        } else if self.rank[ra] > self.rank[rb] {
            self.parent[rb] = ra;
            ra
        } else {
            self.parent[rb] = ra;
            self.rank[ra] += 1;
            ra
        }
    }

    pub fn is_unified(&mut self, a: usize, b: usize) -> bool {
        self.find(a) == self.find(b)
    }

    pub fn len(&self) -> usize {
        self.parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }
}

// ── E-node / E-class ──

/// 一个 impl 的 e-graph 投影。
/// op = body 检测到的函数名（llm/search/write/read/merge/...），
/// children = 该 impl 引用的上游 proc 的 e-class id。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ENode {
    pub op: String,
    pub children: Vec<usize>,
    /// v0.11.1 裁判路由载体标记：impl 挂 .when() 即 true。when-载体节点不参与
    /// 任何熔合（同构/merge 扁平化/write-read 对消）——熔合会抹掉裁判依赖序，
    /// 导致 deliver 抢在 gen 前执行、判决落空。
    pub when_guard: bool,
}

/// E-class：等价 proc 的集合 + 其全部等价实现（e-nodes）+ 节点来源。
/// origins[i] 与 nodes[i] 对齐：(proc 名, impl 下标)，供提取器回查 Impl。
#[derive(Debug, Clone, Default)]
pub struct EClass {
    pub procs: Vec<String>,
    pub nodes: Vec<ENode>,
    pub origins: Vec<(String, usize)>,
}

/// 饱和后的完整 e-graph。
#[derive(Debug, Clone, Default)]
pub struct EGraph {
    /// 原始调度语义（proc 级）：from 必须先于 to 完成。
    pub nodes: BTreeSet<String>,
    pub edges: Vec<(String, String)>,

    // ── e-class 结构 ──
    pub classes: Vec<EClass>,
    pub uf: UnionFind,
    /// proc 名 → 初始 e-class id
    pub proc_class: BTreeMap<String, usize>,
    /// hashcons: e-node → 所在 canonical class id
    pub memo: HashMap<ENode, usize>,
    /// 融合规则命中计数（诊断用）
    pub fusion_hits: BTreeMap<&'static str, usize>,
}

impl EGraph {
    /// canonical class id（带压缩）。
    pub fn canon(&mut self, id: usize) -> usize {
        self.uf.find(id)
    }

    /// 只读版本。
    pub fn canon_imm(&self, id: usize) -> usize {
        self.uf.find_imm(id)
    }

    fn canon_node_imm(&self, node: &ENode) -> ENode {
        ENode {
            op: node.op.clone(),
            children: node.children.iter().map(|c| self.uf.find_imm(*c)).collect(),
            when_guard: node.when_guard,
        }
    }

    /// 添加 e-node（hashcons 去重，保留首个 origin）。
    fn add_node_raw(&mut self, class_id: usize, node: ENode, origin: (String, usize)) {
        let canon = self.uf.find(class_id);
        if !self.classes[canon].nodes.iter().any(|n| n == &node) {
            self.classes[canon].nodes.push(node.clone());
            self.classes[canon].origins.push(origin);
        }
        self.memo.insert(node, canon);
    }

    /// 合并两个 class：节点集取并（去重），memo 重指。
    /// 返回合并后的 canonical id。
    fn merge_classes(&mut self, a: usize, b: usize) -> usize {
        let ra = self.uf.find(a);
        let rb = self.uf.find(b);
        if ra == rb {
            return ra;
        }
        let moved: Vec<ENode> = self.classes[rb].nodes.drain(..).collect();
        let moved_origins: Vec<(String, usize)> = self.classes[rb].origins.drain(..).collect();
        let moved_procs = std::mem::take(&mut self.classes[rb].procs);
        self.classes[ra].procs.extend(moved_procs);
        for (node, origin) in moved.into_iter().zip(moved_origins) {
            self.add_node_raw(ra, node, origin);
        }
        self.uf.union(ra, rb)
    }

    /// canonical class 数（诊断用）。
    pub fn class_count(&self) -> usize {
        (0..self.classes.len())
            .filter(|i| self.uf.find_imm(*i) == *i)
            .count()
    }
}

// ── 构造：从 Pipeline 建立 e-graph ──

/// 从 impl.body_text 提取 op（与 executor::detect_func 同语义）。
pub(crate) fn body_op(body: &str) -> String {
    let body = body.trim();
    if let Some(pos) = body.find('(') {
        let head: String = body[..pos]
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !head.is_empty() {
            return head;
        }
    }
    "call".to_string()
}

/// 提取 body 中全部 @name 引用（保序，不去重）。
pub(crate) fn body_refs(body: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' && i + 1 < chars.len() && chars[i + 1].is_alphanumeric() {
            let start = i + 1;
            let mut end = start;
            while end < chars.len()
                && (chars[end].is_alphanumeric() || chars[end] == '_' || chars[end] == '-')
            {
                end += 1;
            }
            refs.push(chars[start..end].iter().collect());
            i = end;
        } else {
            i += 1;
        }
    }
    refs
}

/// 提取 body 中 key="value" / key = 'value' 形式的引号参数值（容忍空白）。
pub(crate) fn extract_quoted_arg(body: &str, key: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(rel) = body[search_from..].find(key) {
        let start = search_from + rel;
        let end = start + key.len();
        search_from = end;
        // key 必须是独立词（不能是别的标识符的后缀）
        let prev_ok = body[..start]
            .chars()
            .last()
            .map(|c| !c.is_alphanumeric() && c != '_')
            .unwrap_or(true);
        if !prev_ok {
            continue;
        }
        let mut i = end;
        let bytes = body.as_bytes();
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let q = body[i..].chars().next()?;
        if q != '"' && q != '\'' {
            continue;
        }
        let mut out = String::new();
        for c in body[i + q.len_utf8()..].chars() {
            if c == q {
                return Some(out);
            }
            out.push(c);
        }
    }
    None
}

pub fn build_egraph(pl: &Pipeline) -> EGraph {
    let mut eg = EGraph {
        nodes: BTreeSet::new(),
        edges: Vec::new(),
        classes: vec![],
        uf: UnionFind::new(),
        proc_class: BTreeMap::new(),
        memo: HashMap::new(),
        fusion_hits: BTreeMap::new(),
    };

    // 1. 每个 proc 一个初始 class
    for proc in &pl.procs {
        let id = eg.uf.make_set();
        eg.classes.push(EClass {
            procs: vec![proc.name.clone()],
            nodes: vec![],
            origins: vec![],
        });
        eg.proc_class.insert(proc.name.clone(), id);
        eg.nodes.insert(proc.name.clone());
    }

    // 2. 每个 impl 投影为 e-node；同时保留 proc 级依赖边
    let mut edges_seen = BTreeSet::new();
    for proc in &pl.procs {
        let class_id = eg.proc_class[&proc.name];
        for (idx, impl_) in proc.plan.iter().enumerate() {
            // v0.11.1 重构：以 parser 计算的 refs 为准（含 .when() 裁判路由依赖），
            // 并集 body 重提取——手工构造的测试 AST 可能 refs 为空。
            let mut refs = impl_.refs.clone();
            for r in body_refs(&impl_.body_text) {
                if !refs.contains(&r) {
                    refs.push(r);
                }
            }
            for r in &refs {
                if pl.procs.iter().any(|p| &p.name == r) && r != &proc.name {
                    if edges_seen.insert((r.clone(), proc.name.clone())) {
                        eg.edges.push((r.clone(), proc.name.clone()));
                    }
                }
            }
            let children: Vec<usize> = refs
                .iter()
                .filter_map(|r| eg.proc_class.get(r).copied())
                .collect();
            let node = ENode {
                op: body_op(&impl_.body_text),
                children,
                when_guard: impl_.when.is_some(),
            };
            eg.add_node_raw(class_id, node.clone(), (proc.name.clone(), idx));
        }
    }

    // 3. equality saturation（union-only，到不动点）
    saturate(&mut eg, pl);
    eg
}

/// 融合规则到不动点。每条规则只做 class union（不合成新节点），
/// canonical class 数单调下降 → 必然终止。
fn saturate(eg: &mut EGraph, pl: &Pipeline) {
    for _ in 0..64 {
        let mut changed = false;
        changed |= union_isomorphic_classes(eg);
        changed |= flatten_merge_nodes(eg);
        changed |= cancel_write_read(eg, pl);
        if !changed {
            break;
        }
    }
}

// ── R1: 同构合并 ──

/// v0.11.1：class 是否含裁判路由载体节点（.when 挂身的 impl 投影）。
fn class_has_when_guard(eg: &EGraph, cid: usize) -> bool {
    let canon = eg.uf.find_imm(cid);
    eg.classes[canon].nodes.iter().any(|n| n.when_guard)
}

/// 两 canonical class 的节点集（canon 化、排序去重后）完全一致 → union。
fn union_isomorphic_classes(eg: &mut EGraph) -> bool {
    let mut changed = false;
    let mut round = true;
    while round {
        round = false;
        let ids: Vec<usize> = (0..eg.classes.len())
            .filter(|i| eg.uf.find_imm(*i) == *i && !eg.classes[*i].nodes.is_empty())
            .collect();
        let mut sig_map: HashMap<Vec<ENode>, usize> = HashMap::new();
        for id in ids {
            let mut sig: Vec<ENode> = eg.classes[id]
                .nodes
                .iter()
                .map(|n| eg.canon_node_imm(n))
                .collect();
            sig.sort();
            sig.dedup();
            if sig.is_empty() {
                continue; // 空节点 class 不参与同构合并
            }
            if let Some(&other) = sig_map.get(&sig) {
                if eg.uf.find_imm(id) != eg.uf.find_imm(other) {
                    // v0.11.1：任一侧含 when-载体 → 不熔合（裁判路由 impl 语义不等价）。
                    if class_has_when_guard(eg, id) || class_has_when_guard(eg, other) {
                        continue;
                    }
                    eg.merge_classes(other, id);
                    *eg.fusion_hits.entry("isomorphic_union").or_insert(0) += 1;
                    changed = true;
                    round = true;
                    break; // 结构已变，重扫
                }
            } else {
                sig_map.insert(sig, id);
            }
        }
    }
    changed
}

// ── R2: merge 扁平化 ──

/// merge 节点的传递扁平子集：对每个 child class，
/// 若它是纯 merge class（全部节点都是 merge 且各节点扁平集一致）则递归展开，
/// 否则视为自身。guard 防环，memo 防重复计算。
fn class_flat_or_self(
    eg: &EGraph,
    cid: usize,
    memo: &mut HashMap<usize, BTreeSet<usize>>,
    guard: &mut HashSet<usize>,
) -> BTreeSet<usize> {
    let canon = eg.uf.find_imm(cid);
    if let Some(s) = memo.get(&canon) {
        return s.clone();
    }
    if !guard.insert(canon) {
        return BTreeSet::from([canon]); // 环：保守视为自身
    }
    let nodes = &eg.classes[canon].nodes;
    let pure = !nodes.is_empty() && nodes.iter().all(|n| n.op == "merge");
    let result = if !pure {
        BTreeSet::from([canon])
    } else {
        let mut agreed: Option<BTreeSet<usize>> = None;
        let mut consistent = true;
        for n in nodes {
            let mut out = BTreeSet::new();
            for &c in &n.children {
                out.extend(class_flat_or_self(eg, c, memo, guard));
            }
            match &agreed {
                None => agreed = Some(out),
                Some(a) if *a == out => {}
                _ => {
                    consistent = false;
                    break;
                }
            }
        }
        if consistent {
            agreed.unwrap_or_else(|| BTreeSet::from([canon]))
        } else {
            BTreeSet::from([canon])
        }
    };
    guard.remove(&canon);
    memo.insert(canon, result.clone());
    result
}

fn merge_node_flat_set(
    eg: &EGraph,
    node: &ENode,
    memo: &mut HashMap<usize, BTreeSet<usize>>,
    guard: &mut HashSet<usize>,
) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    for &c in &node.children {
        out.extend(class_flat_or_self(eg, c, memo, guard));
    }
    out
}

/// 所有 merge 节点按扁平子集分组，组内 union。
/// 涵盖：交换律（merge(@a,@b) ≡ merge(@b,@a)）、
/// 嵌套折叠（merge(merge(a,b),c) ≡ merge(a,b,c)）、退化 merge(@x) ≡ x。
fn flatten_merge_nodes(eg: &mut EGraph) -> bool {
    let mut changed = false;
    loop {
        let mut sigs: Vec<(BTreeSet<usize>, usize)> = Vec::new();
        for cid in 0..eg.classes.len() {
            if eg.uf.find_imm(cid) != cid {
                continue;
            }
            for node in &eg.classes[cid].nodes {
                if node.op != "merge" {
                    continue;
                }
                let mut memo = HashMap::new();
                let mut guard = HashSet::new();
                let flat = merge_node_flat_set(eg, node, &mut memo, &mut guard);
                sigs.push((flat, cid));
            }
        }
        let mut groups: HashMap<BTreeSet<usize>, usize> = HashMap::new();
        let mut acted = false;
        'pass: for (sig, cid) in sigs {
            // 退化 merge(@x)：扁平集恰为 {x} → 与 x 同 class
            if sig.len() == 1 {
                let only = *sig.iter().next().unwrap();
                if eg.uf.find_imm(cid) != eg.uf.find_imm(only) {
                    // v0.11.1：when-载体不参与退化合并。
                    if class_has_when_guard(eg, cid) || class_has_when_guard(eg, only) {
                        continue;
                    }
                    eg.merge_classes(cid, only);
                    *eg.fusion_hits.entry("merge_flatten").or_insert(0) += 1;
                    changed = true;
                    acted = true;
                    break 'pass;
                }
                continue;
            }
            match groups.get(&sig) {
                Some(&other) => {
                    if eg.uf.find_imm(cid) != eg.uf.find_imm(other) {
                        // v0.11.1：when-载体不参与扁平化合并。
                        if class_has_when_guard(eg, cid) || class_has_when_guard(eg, other) {
                            continue;
                        }
                        eg.merge_classes(other, cid);
                        *eg.fusion_hits.entry("merge_flatten").or_insert(0) += 1;
                        changed = true;
                        acted = true;
                        break 'pass; // 结构已变，重算
                    }
                }
                None => {
                    groups.insert(sig, cid);
                }
            }
        }
        if !acted {
            break;
        }
    }
    changed
}

// ── R3: write/read 对消 ──

/// read(from=P) ≡ write(to=P, content=@src) 的 src → read class 并入 src class。
/// 路径按原文精确匹配（含 {topic} 等占位符原样比较，保守不误消）。
/// write 的 content 无 @ref（字面量）时不消（无 class 可并）。
fn cancel_write_read(eg: &mut EGraph, pl: &Pipeline) -> bool {
    let mut changed = false;
    loop {
        // 收集 reads / writes（经 origins 回查 impl body）
        let mut reads: Vec<(usize, String)> = Vec::new(); // (class, path)
        let mut writes: Vec<(usize, String, Option<String>)> = Vec::new(); // (class, path, src)
        for cid in 0..eg.classes.len() {
            if eg.uf.find_imm(cid) != cid {
                continue;
            }
            for (ni, node) in eg.classes[cid].nodes.iter().enumerate() {
                let Some((pname, idx)) = eg.classes[cid].origins.get(ni).cloned() else {
                    continue;
                };
                let Some(proc) = pl.procs.iter().find(|p| p.name == pname) else {
                    continue;
                };
                let Some(impl_) = proc.plan.get(idx) else {
                    continue;
                };
                match node.op.as_str() {
                    "read" | "read_file" => {
                        if let Some(p) = extract_quoted_arg(&impl_.body_text, "from") {
                            reads.push((cid, p));
                        }
                    }
                    "write" => {
                        if let Some(p) = extract_quoted_arg(&impl_.body_text, "to") {
                            let src = body_refs(&impl_.body_text)
                                .into_iter()
                                .find(|r| eg.proc_class.contains_key(r));
                            writes.push((cid, p, src));
                        }
                    }
                    _ => {}
                }
            }
        }

        let mut acted = false;
        'outer: for (rc, rpath) in &reads {
            for (wc, wpath, wsrc) in &writes {
                if rpath == wpath && eg.uf.find_imm(*rc) != eg.uf.find_imm(*wc) {
                    if let Some(src) = wsrc {
                        let sc = eg.proc_class[src];
                        if eg.uf.find_imm(*rc) != eg.uf.find_imm(sc) {
                            // v0.11.1：when-载体不参与 write-read 对消。
                            if class_has_when_guard(eg, *rc)
                                || class_has_when_guard(eg, *wc)
                                || class_has_when_guard(eg, sc)
                            {
                                continue;
                            }
                            eg.merge_classes(*rc, sc);
                            *eg.fusion_hits.entry("write_read_cancel").or_insert(0) += 1;
                            changed = true;
                            acted = true;
                            break 'outer;
                        }
                    }
                }
            }
        }
        if !acted {
            break;
        }
    }
    changed
}

// ── 提取器：per-class 最优 impl 选择 ──

/// 提取结果：执行计划（静态）。
#[derive(Debug, Clone, Default)]
pub struct ExtractedPlan {
    /// 执行序（class 拓扑序）：每 class 的代表 proc 名（即被选中实现的所属 proc）。
    pub order: Vec<String>,
    /// 代表 proc 名 → (选中 proc 名, 选中 impl 名)。
    pub picks: BTreeMap<String, (String, String)>,
    /// 代表 proc 名 → 预计静态 effective cost。
    pub costs: BTreeMap<String, f64>,
    /// CSE 别名：同 class 其他 proc 名 → 代表 proc 名。
    pub aliases: BTreeMap<String, String>,
}

/// 静态 effective cost（提取层）：仅声明成本。
/// 历史惩罚/偏好/RD 附加费留在运行时（executor::rank_impls_pref），
/// 两层分工：e-graph 选"谁"（class 代表），历史选"怎么跑"（impl 排序）。
pub fn effective_cost(weights: &Weights, impl_: &Impl) -> f64 {
    weights.cost_total(&impl_.cost)
}

/// 全局计划提取：class 级 Kahn 拓扑（确定性），逐 class 选最小静态 cost 的 impl。
/// 跳过：deliver proc 的 impl、disabled impl、空 class。
pub fn extract_plan(pl: &Pipeline, eg: &EGraph) -> ExtractedPlan {
    let canon_ids: Vec<usize> = (0..eg.classes.len())
        .filter(|i| eg.uf.find_imm(*i) == *i && !eg.classes[*i].nodes.is_empty())
        .collect();

    // class 级依赖：to 依赖 from
    let mut class_dep: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
    for (from, to) in &eg.edges {
        if let (Some(f), Some(t)) = (eg.proc_class.get(from), eg.proc_class.get(to)) {
            let (f, t) = (eg.uf.find_imm(*f), eg.uf.find_imm(*t));
            if f != t {
                class_dep.entry(t).or_default().insert(f);
            }
        }
    }

    // Kahn 拓扑（BTreeSet 队列保证确定性）
    let mut indeg: BTreeMap<usize, usize> = canon_ids
        .iter()
        .map(|&c| (c, class_dep.get(&c).map_or(0, |s| s.len())))
        .collect();
    let mut ready: BTreeSet<usize> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(c, _)| *c)
        .collect();
    let mut topo: Vec<usize> = Vec::new();
    while let Some(&c) = ready.iter().next() {
        ready.remove(&c);
        topo.push(c);
        let dependents: Vec<usize> = indeg
            .keys()
            .filter(|d| class_dep.get(*d).map_or(false, |s| s.contains(&c)))
            .copied()
            .collect();
        for d in dependents {
            if let Some(x) = indeg.get_mut(&d) {
                *x -= 1;
                if *x == 0 {
                    ready.insert(d);
                }
            }
        }
    }
    // 环防御：把剩余 class 按字典序追加（与旧 parallel_groups 的宽容策略一致）
    if topo.len() < canon_ids.len() {
        let done: HashSet<usize> = topo.iter().copied().collect();
        let mut rest: Vec<usize> = canon_ids
            .iter()
            .filter(|c| !done.contains(c))
            .copied()
            .collect();
        rest.sort_unstable();
        topo.extend(rest);
    }

    // 逐 class 选最小静态 cost 的 (proc, impl)
    let mut order = Vec::new();
    let mut picks = BTreeMap::new();
    let mut costs = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    let mut members: BTreeMap<usize, Vec<String>> = BTreeMap::new();

    for &c in &topo {
        members.insert(c, eg.classes[c].procs.clone());
        let mut best: Option<(String, String, f64)> = None; // (proc, impl, cost)
                                                            // 候选 = class 内全部成员 proc 的全部 impl。
                                                            // （e-node 去重只作用于图结构；提取候选以 proc 为准，不受影响）
                                                            //
                                                            // 两层优先：
                                                            //   1. 非 read 节点优先——对消 class 里 read 是缓存/恢复路径，
                                                            //      不是值的定义（write/read cancel 后定义在源头）。
                                                            //      纯 read class（外部文件输入）不受影响。
                                                            //   2. 同层内按 (cost, proc, impl) 字典序取最小（确定性）。
        let mut pool: Vec<(&Proc, &Impl)> = Vec::new();
        for m in &eg.classes[c].procs {
            let Some(proc) = pl.procs.iter().find(|p| &p.name == m) else {
                continue;
            };
            if proc.deliver {
                continue;
            }
            for impl_ in &proc.plan {
                if impl_.enabled {
                    pool.push((proc, impl_));
                }
            }
        }
        let has_non_read = pool
            .iter()
            .any(|(_, i)| !matches!(body_op(&i.body_text).as_str(), "read" | "read_file"));
        for (proc, impl_) in pool {
            let op = body_op(&impl_.body_text);
            if has_non_read && matches!(op.as_str(), "read" | "read_file") {
                continue;
            }
            let cost = effective_cost(&pl.weights, impl_);
            let better = match &best {
                None => true,
                Some((bp, bi, bc)) => (cost, &proc.name, &impl_.name) < (*bc, bp, bi),
            };
            if better {
                best = Some((proc.name.clone(), impl_.name.clone(), cost));
            }
        }
        if let Some((pname, iname, cost)) = best {
            order.push(pname.clone());
            picks.insert(pname.clone(), (pname.clone(), iname.clone()));
            costs.insert(pname.clone(), cost);
            // 同 class 其余 proc → 别名（复用代表结果，不重复执行）
            for m in &eg.classes[c].procs {
                if m != &pname {
                    aliases.insert(m.clone(), pname.clone());
                }
            }
        }
    }

    ExtractedPlan {
        order,
        picks,
        costs,
        aliases,
    }
}

// ── 兼容层：调度视图（在 proc 级图上，语义与 v0.9 一致）──

pub fn parallel_groups(eg: &EGraph) -> Vec<Vec<String>> {
    let remaining: BTreeSet<String> = eg.nodes.iter().cloned().collect();
    let mut remaining = remaining;
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

// ── tests ──

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_impl(name: &str, body: &str, latency: i64) -> Impl {
        Impl {
            name: name.into(),
            description: String::new(),
            tags: BTreeSet::new(),
            cost: Cost {
                latency,
                ..Default::default()
            },
            enabled: true,
            when: None,
            refs: vec![],
            body_text: body.into(),
            stub: false,
            retry: 0,
            ensure: vec![],
        }
    }

    fn mk_proc(name: &str, impls: Vec<Impl>) -> Proc {
        Proc {
            name: name.into(),
            description: String::new(),
            plan: impls,
            checks: vec![],
            contract: Default::default(),
            deliver: false,
            deliver_refs: vec![],
            foreach: None,
            foreach_var: String::new(),
            pick_by: "cost + history".into(),
        }
    }

    fn mk_pipeline(procs: Vec<Proc>) -> Pipeline {
        Pipeline {
            name: "t".into(),
            description: String::new(),
            procs,
            weights: Weights::default(),
            cwd: None,
            env: vec![],
        }
    }

    // ── UnionFind ──

    #[test]
    fn union_find_basic() {
        let mut uf = UnionFind::new();
        let a = uf.make_set();
        let b = uf.make_set();
        let c = uf.make_set();
        assert!(!uf.is_unified(a, b));
        uf.union(a, b);
        assert!(uf.is_unified(a, b));
        assert!(!uf.is_unified(a, c));
        assert_eq!(uf.find_imm(a), uf.find_imm(b));
    }

    #[test]
    fn union_find_chain_compression() {
        let mut uf = UnionFind::new();
        let ids: Vec<usize> = (0..5).map(|_| uf.make_set()).collect();
        uf.union(ids[0], ids[1]);
        uf.union(ids[1], ids[2]);
        uf.union(ids[2], ids[3]);
        assert_eq!(uf.find_imm(ids[0]), uf.find_imm(ids[3]));
        assert_ne!(uf.find_imm(ids[0]), uf.find_imm(ids[4]));
    }

    // ── 构造投影 ──

    #[test]
    fn enode_projection_op_and_children() {
        let pl = mk_pipeline(vec![
            mk_proc("src", vec![mk_impl("s1", "web_search(query=\"x\")", 10)]),
            mk_proc(
                "down",
                vec![mk_impl("d1", "llm(input=@src, template=\"t\")", 20)],
            ),
        ]);
        let eg = build_egraph(&pl);
        assert_eq!(eg.classes[eg.proc_class["src"]].nodes[0].op, "web_search");
        let dclass = eg.proc_class["down"];
        assert_eq!(eg.classes[dclass].nodes[0].op, "llm");
        assert_eq!(
            eg.classes[dclass].nodes[0].children,
            vec![eg.canon_imm(eg.proc_class["src"])]
        );
        assert_eq!(eg.edges, vec![("src".to_string(), "down".to_string())]);
    }

    #[test]
    fn origins_align_with_nodes() {
        let pl = mk_pipeline(vec![mk_proc(
            "p",
            vec![
                mk_impl("a", "run(\"ls\")", 1),
                mk_impl("b", "sh(\"pwd\")", 2),
            ],
        )]);
        let eg = build_egraph(&pl);
        let c = eg.proc_class["p"];
        assert_eq!(
            eg.classes[c].origins,
            vec![("p".to_string(), 0), ("p".to_string(), 1)]
        );
    }

    // ── R1 同构合并 ──

    #[test]
    fn isomorphic_procs_union() {
        let pl = mk_pipeline(vec![
            mk_proc("src", vec![mk_impl("s1", "web_search(query=\"x\")", 10)]),
            mk_proc(
                "dup1",
                vec![mk_impl("d1", "llm(input=@src, template=\"a\")", 20)],
            ),
            mk_proc(
                "dup2",
                vec![mk_impl("d2", "llm(input=@src, template=\"b\")", 30)],
            ),
        ]);
        let mut eg = build_egraph(&pl);
        // dup1 与 dup2 唯一节点都是 llm([src]) → 同一 class
        assert!(eg.is_unified_pub(eg.proc_class["dup1"], eg.proc_class["dup2"]));
        assert_eq!(eg.fusion_hits.get("isomorphic_union"), Some(&1));
        assert_eq!(eg.class_count(), 2); // src + {dup1,dup2}
    }

    // ── R2 merge 扁平化 ──

    #[test]
    fn merge_commutativity_unions() {
        let pl = mk_pipeline(vec![
            mk_proc("a", vec![mk_impl("a1", "web_search(query=\"x\")", 10)]),
            mk_proc("b", vec![mk_impl("b1", "mcp_search(query=\"y\")", 10)]),
            mk_proc("m1", vec![mk_impl("m1i", "merge(@a, @b, dedup)", 5)]),
            mk_proc("m2", vec![mk_impl("m2i", "merge(@b, @a)", 5)]),
        ]);
        let mut eg = build_egraph(&pl);
        assert!(eg.is_unified_pub(eg.proc_class["m1"], eg.proc_class["m2"]));
        assert_eq!(eg.fusion_hits.get("merge_flatten"), Some(&1));
    }

    #[test]
    fn nested_merge_flattens_to_three_way() {
        let pl = mk_pipeline(vec![
            mk_proc("a", vec![mk_impl("a1", "web_search(query=\"x\")", 10)]),
            mk_proc("b", vec![mk_impl("b1", "mcp_search(query=\"y\")", 10)]),
            mk_proc("c", vec![mk_impl("c1", "run(\"echo z\")", 10)]),
            mk_proc("inner", vec![mk_impl("i1", "merge(@a, @b)", 5)]),
            mk_proc("outer", vec![mk_impl("o1", "merge(@inner, @c)", 5)]),
            mk_proc("threeway", vec![mk_impl("t1", "merge(@a, @b, @c)", 5)]),
        ]);
        let mut eg = build_egraph(&pl);
        // outer = merge(merge(a,b),c) ≡ merge(a,b,c) ≡ threeway
        assert!(eg.is_unified_pub(eg.proc_class["outer"], eg.proc_class["threeway"]));
    }

    #[test]
    fn degenerate_merge_unions_with_child() {
        let pl = mk_pipeline(vec![
            mk_proc("a", vec![mk_impl("a1", "web_search(query=\"x\")", 10)]),
            mk_proc("m", vec![mk_impl("m1", "merge(@a)", 0)]),
        ]);
        let mut eg = build_egraph(&pl);
        // merge(@a) ≡ a
        assert!(eg.is_unified_pub(eg.proc_class["m"], eg.proc_class["a"]));
    }

    // ── R3 write/read 对消 ──

    #[test]
    fn write_read_cancellation() {
        let pl = mk_pipeline(vec![
            mk_proc(
                "gen",
                vec![mk_impl("g1", "llm(template=\"write doc\")", 50)],
            ),
            mk_proc(
                "writer",
                vec![mk_impl(
                    "w1",
                    "write(to=\"out/{topic}.md\", content=@gen)",
                    5,
                )],
            ),
            mk_proc(
                "reader",
                vec![mk_impl("r1", "read(from=\"out/{topic}.md\")", 3)],
            ),
        ]);
        let mut eg = build_egraph(&pl);
        // read(P) ≡ write 的 content 来源 @gen
        assert!(eg.is_unified_pub(eg.proc_class["reader"], eg.proc_class["gen"]));
        assert_eq!(eg.fusion_hits.get("write_read_cancel"), Some(&1));
    }

    #[test]
    fn write_read_different_paths_no_cancel() {
        let pl = mk_pipeline(vec![
            mk_proc("gen", vec![mk_impl("g1", "llm(template=\"doc\")", 50)]),
            mk_proc(
                "writer",
                vec![mk_impl("w1", "write(to=\"out/a.md\", content=@gen)", 5)],
            ),
            mk_proc("reader", vec![mk_impl("r1", "read(from=\"out/b.md\")", 3)]),
        ]);
        let mut eg = build_egraph(&pl);
        assert!(!eg.is_unified_pub(eg.proc_class["reader"], eg.proc_class["gen"]));
    }

    // ── v0.11.1 裁判路由守卫 ──

    #[test]
    fn when_carrier_blocks_isomorphic_fusion() {
        // gen 与 deliver 的 impl 均为 echo 形态（同构），但 deliver 挂 .when(@gen.score<80)
        // ——熔合会抹掉裁判依赖序，必须拒绝。
        let mut deliver = mk_impl("x", "run(\"echo DELIVERED\")", 1);
        deliver.when = Some("@gen.score < 80".into());
        deliver.refs = vec!["gen".into()];
        let pl = mk_pipeline(vec![
            mk_proc("gen", vec![mk_impl("high", "run(\"echo 'score=85'\")", 1)]),
            mk_proc("deliver", vec![deliver]),
        ]);
        let mut eg = build_egraph(&pl);
        assert_eq!(eg.fusion_hits.get("isomorphic_union"), None);
        assert!(!eg.is_unified_pub(eg.proc_class["gen"], eg.proc_class["deliver"]));
        // 裁判边保留 → 分层正确：gen 先于 deliver
        let groups = parallel_groups(&eg);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], vec!["gen".to_string()]);
        assert_eq!(groups[1], vec!["deliver".to_string()]);
    }

    #[test]
    fn no_when_still_fuses_isomorphic() {
        // 对照组：同构 impl 双方均无 when → 熔合照常（CSE 能力不受影响）。
        let pl = mk_pipeline(vec![
            mk_proc("a", vec![mk_impl("x", "run(\"echo SAME\")", 1)]),
            mk_proc("b", vec![mk_impl("y", "run(\"echo SAME\")", 1)]),
        ]);
        let eg = build_egraph(&pl);
        assert_eq!(eg.fusion_hits.get("isomorphic_union"), Some(&1));
    }

    #[test]
    fn when_carrier_blocks_write_read_cancel() {
        // write 侧挂 when → 对消被守卫拒绝（对消会隐式改写裁判消费者的数据源）。
        let mut writer = mk_impl("w1", "write(to=\"out/{topic}.md\", content=@gen)", 5);
        writer.when = Some("@gen.score < 80".into());
        writer.refs = vec!["gen".into()];
        let pl = mk_pipeline(vec![
            mk_proc(
                "gen",
                vec![mk_impl("g1", "llm(template=\"write doc\")", 50)],
            ),
            mk_proc("writer", vec![writer]),
            mk_proc(
                "reader",
                vec![mk_impl("r1", "read(from=\"out/{topic}.md\")", 3)],
            ),
        ]);
        let mut eg = build_egraph(&pl);
        assert_eq!(eg.fusion_hits.get("write_read_cancel"), None);
        assert!(!eg.is_unified_pub(eg.proc_class["reader"], eg.proc_class["gen"]));
    }

    // ── 提取器 ──

    #[test]
    fn extract_picks_min_cost_and_aliases() {
        let pl = mk_pipeline(vec![
            mk_proc("src", vec![mk_impl("s1", "web_search(query=\"x\")", 10)]),
            mk_proc(
                "dup1",
                vec![
                    mk_impl("cheap", "llm(input=@src, template=\"a\")", 10),
                    mk_impl("dear", "llm(input=@src, template=\"b\")", 900),
                ],
            ),
            mk_proc(
                "dup2",
                vec![mk_impl("d2", "llm(input=@src, template=\"c\")", 500)],
            ),
        ]);
        let eg = build_egraph(&pl);
        let plan = extract_plan(&pl, &eg);
        // dup1≡dup2 同 class，代表应是 dup1（其 cheap impl 全局最便宜）
        assert!(plan.picks.contains_key("dup1"));
        assert_eq!(
            plan.picks["dup1"],
            ("dup1".to_string(), "cheap".to_string())
        );
        assert_eq!(plan.aliases.get("dup2").map(String::as_str), Some("dup1"));
        // 拓扑序：src 在 dup1 前
        let pos_src = plan.order.iter().position(|n| n == "src").unwrap();
        let pos_dup = plan.order.iter().position(|n| n == "dup1").unwrap();
        assert!(pos_src < pos_dup);
    }

    #[test]
    fn extract_skips_disabled_and_deliver() {
        let mut deliver_proc = mk_proc("final", vec![mk_impl("f1", "read(from=\"x.md\")", 1)]);
        deliver_proc.deliver = true;
        let pl = mk_pipeline(vec![
            mk_proc(
                "p",
                vec![
                    mk_impl("off", "run(\"a\")", 1),
                    mk_impl("on", "run(\"b\")", 2),
                ],
            ),
            deliver_proc,
        ]);
        let mut patched = pl.clone();
        patched.procs[0].plan[0].enabled = false;
        let eg = build_egraph(&patched);
        let plan = extract_plan(&patched, &eg);
        assert_eq!(plan.picks["p"], ("p".to_string(), "on".to_string()));
        assert!(!plan.picks.contains_key("final"));
    }

    #[test]
    fn extraction_is_deterministic() {
        let pl = mk_pipeline(vec![
            mk_proc("a", vec![mk_impl("a1", "web_search(query=\"x\")", 10)]),
            mk_proc("b", vec![mk_impl("b1", "run(\"echo @a\")", 10)]),
            mk_proc("c", vec![mk_impl("c1", "merge(@a, @b)", 1)]),
        ]);
        let eg1 = build_egraph(&pl);
        let eg2 = build_egraph(&pl);
        let p1 = extract_plan(&pl, &eg1);
        let p2 = extract_plan(&pl, &eg2);
        assert_eq!(p1.order, p2.order);
        assert_eq!(p1.picks, p2.picks);
        assert_eq!(p1.aliases, p2.aliases);
    }

    // ── 饱和终止 ──

    #[test]
    fn saturation_terminates_on_self_merge() {
        // merge 引用自己（病态输入）：guard 应防住无限递归
        let pl = mk_pipeline(vec![
            mk_proc("a", vec![mk_impl("a1", "web_search(query=\"x\")", 10)]),
            mk_proc("m", vec![mk_impl("m1", "merge(@m, @a)", 1)]),
        ]);
        let eg = build_egraph(&pl); // 不 panic、不死循环即通过
        assert_eq!(eg.class_count() >= 1, true);
    }

    // ── 兼容层 ──

    #[test]
    fn parallel_groups_layers() {
        let pl = mk_pipeline(vec![
            mk_proc("a", vec![mk_impl("a1", "web_search(query=\"x\")", 10)]),
            mk_proc("b", vec![mk_impl("b1", "run(\"echo\")", 10)]),
            mk_proc("c", vec![mk_impl("c1", "merge(@a, @b)", 1)]),
        ]);
        let eg = build_egraph(&pl);
        let groups = parallel_groups(&eg);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2); // a, b
        assert_eq!(groups[1], vec!["c".to_string()]);
    }

    #[test]
    fn critical_path_chain() {
        let pl = mk_pipeline(vec![
            mk_proc("a", vec![mk_impl("a1", "web_search(query=\"x\")", 10)]),
            mk_proc("b", vec![mk_impl("b1", "run(\"echo @a\")", 10)]),
            mk_proc("c", vec![mk_impl("c1", "run(\"echo @b\")", 10)]),
        ]);
        let eg = build_egraph(&pl);
        assert_eq!(critical_path(&eg), vec!["a", "b", "c"]);
    }

    #[test]
    fn empty_pipeline_egraph() {
        let pl = mk_pipeline(vec![]);
        let eg = build_egraph(&pl);
        assert!(eg.nodes.is_empty());
        assert!(parallel_groups(&eg).is_empty());
        assert!(critical_path(&eg).is_empty());
        let plan = extract_plan(&pl, &eg);
        assert!(plan.order.is_empty());
    }

    // ── 辅助 ──

    impl EGraph {
        /// 测试用：union-find 判等（公开包装）。
        pub fn is_unified_pub(&mut self, a: usize, b: usize) -> bool {
            self.uf.is_unified(a, b)
        }
    }

    #[test]
    fn quoted_arg_extraction() {
        assert_eq!(
            extract_quoted_arg("read(from=\"out/x.md\")", "from"),
            Some("out/x.md".to_string())
        );
        assert_eq!(
            extract_quoted_arg("write(to = 'a b.md', content=@x)", "to"),
            Some("a b.md".to_string())
        );
        assert_eq!(extract_quoted_arg("read(from=\"x\")", "to"), None);
    }
}

// ── v0.12.1 egraph 原语直接单测 ──

#[cfg(test)]
mod prim_tests {
    use super::*;

    // ── body_op ──

    #[test]
    fn op_head_extraction() {
        assert_eq!(body_op(r#"run("echo hi")"#), "run");
        assert_eq!(body_op(r#"web_search(query="AI")"#), "web_search");
        // 前导空白
        assert_eq!(body_op(r#"  llm(x=1)"#), "llm");
    }

    #[test]
    fn op_fallbacks() {
        // 无括号 → call；head 只有符号字符 → call
        assert_eq!(body_op("just text"), "call");
        assert_eq!(body_op("(x)"), "call");
        assert_eq!(body_op(""), "call");
    }

    // ── body_refs ──

    #[test]
    fn refs_ordered_no_dedup() {
        // 与 parser::extract_refs 不同：保序不去重（enode 子项需要重复子项）
        assert_eq!(body_refs("f(@a, @b, @a)"), vec!["a", "b", "a"]);
    }

    #[test]
    fn refs_requires_alnum_after_at() {
        // @ 后非字母数字（如 @/ @空格）不构成引用
        assert!(body_refs("email@ host @/x").is_empty());
    }

    #[test]
    fn refs_charset_includes_dash_underscore() {
        assert_eq!(body_refs("@a-b @c_d"), vec!["a-b", "c_d"]);
    }

    // ── extract_quoted_arg ──

    #[test]
    fn quoted_arg_double_and_single_quotes() {
        assert_eq!(
            extract_quoted_arg(r#"to="out.txt""#, "to"),
            Some("out.txt".into())
        );
        assert_eq!(
            extract_quoted_arg(r#"to = 'single'"#, "to"),
            Some("single".into())
        );
    }

    #[test]
    fn quoted_arg_tolerates_spacing() {
        assert_eq!(
            extract_quoted_arg(r#"key  =  "v""#, "key"),
            Some("v".into())
        );
    }

    #[test]
    fn quoted_arg_word_boundary() {
        // key 不能是更长标识符的后缀：搜 "to" 不得命中 "into"
        assert_eq!(
            extract_quoted_arg(r#"into="x" to="y""#, "to"),
            Some("y".into())
        );
    }

    #[test]
    fn quoted_arg_missing_or_unquoted() {
        assert_eq!(extract_quoted_arg(r#"to=out"#, "to"), None); // 无引号
        assert_eq!(extract_quoted_arg(r#"other="x""#, "to"), None); // 无此键
        assert_eq!(extract_quoted_arg(r#"to="#, "to"), None); // 等号后无值
    }
}
