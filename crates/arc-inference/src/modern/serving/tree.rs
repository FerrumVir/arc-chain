//! Exact tree speculation over ENG-1's [`BatchModel`].
//!
//! A drafter proposes a tree of candidate continuations; the target verifies
//! EVERY node in ONE [`BatchModel::forward_tree`] call, so a model split across
//! devices could traverse a pipeline once per tree (network unmeasured). Node `i`
//! sees the committed prefix and its own ancestors, in the reference attention
//! order. [`dense::DenseModel`] computes each node once with per-node attention
//! masks (shared ancestors are not duplicated); any other model falls back to
//! one sequence per root-to-leaf path in one `forward_rows` call.
//!
//! Walk the target's selection rule (argmax or RP64) from the root, following
//! a child only when its token equals the selected token. Inductively every
//! visited node has exactly the greedy prefix, hence identical logits, tokens
//! and KV. Only the visited path's KV is committed. Rejected siblings and
//! their failures never enter the cache. A failure on the chosen path is
//! raised only if greedy would forward that token (not after EOS/limit).
//! Drafts affect work and acceptance only, never the computed function.
//!
//! Drafters: [`LookupTree`] (n-gram/prompt lookup, for code and agent traffic),
//! [`RecycleTree`] (token recycling: the target's own top-k candidates from
//! earlier rows, no draft model, optionally merged with lookup), [`head_tree`]
//! (ranked candidate lists; no trained head) and [`LocalModelTree`] (a small local
//! model with the target's tokenizer).
//!
//! [`dense::DenseModel`]: super::dense::DenseModel

use super::TargetFeatures;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::time::Instant;

use super::{BatchModel, Row, SeqKv};
use crate::modern::ModernError;
use crate::modern::arith::{self, Selection};
use crate::modern::model::GenerationRequest;

const MAX_NODES: usize = 256;
const MAX_DEPTH: usize = 64;
/// Bound on the rows the path-lowering fallback materializes.
const MAX_ROWS: usize = 1024;

/// Node zero is the already emitted, not yet forwarded token. Every other
/// node refers to an earlier parent; sibling token ids must be distinct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub parent: Option<usize>,
    pub token: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftTree {
    nodes: Vec<Node>,
}

impl DraftTree {
    pub fn new(nodes: Vec<Node>) -> Result<Self, ModernError> {
        if nodes.is_empty() || nodes.len() > MAX_NODES || nodes[0].parent.is_some() {
            return Err(invalid("tree needs one root and at most 256 nodes"));
        }
        let mut depths = vec![0; nodes.len()];
        for (i, node) in nodes.iter().enumerate().skip(1) {
            let parent = node
                .parent
                .filter(|&p| p < i)
                .ok_or_else(|| invalid("tree parents must precede children"))?;
            depths[i] = depths[parent] + 1;
            if depths[i] > MAX_DEPTH
                || nodes[1..i]
                    .iter()
                    .any(|n| n.parent == node.parent && n.token == node.token)
            {
                return Err(invalid("tree is too deep or has duplicate siblings"));
            }
        }
        let tree = Self { nodes };
        if tree.expanded_rows() > MAX_ROWS {
            return Err(invalid("expanded tree exceeds 1024 verification rows"));
        }
        Ok(tree)
    }

    pub fn root(token: u32) -> Self {
        Self {
            nodes: vec![Node {
                parent: None,
                token,
            }],
        }
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Depth of every node; the root is 0.
    pub fn depths(&self) -> Vec<usize> {
        let mut depths = vec![0; self.nodes.len()];
        for (i, node) in self.nodes.iter().enumerate().skip(1) {
            depths[i] = depths[node.parent.unwrap()] + 1;
        }
        depths
    }

    /// Nodes from the root to `node`, inclusive, in increasing order.
    pub fn path_to(&self, node: usize) -> Vec<usize> {
        let mut path = vec![node];
        let mut at = node;
        while let Some(parent) = self.nodes[at].parent {
            path.push(parent);
            at = parent;
        }
        path.reverse();
        path
    }

    /// Root-to-leaf paths, the sequences of the path-lowering fallback.
    pub fn paths(&self) -> Vec<Vec<usize>> {
        let mut has_child = vec![false; self.nodes.len()];
        for node in &self.nodes[1..] {
            has_child[node.parent.unwrap()] = true;
        }
        (0..self.nodes.len())
            .filter(|&i| !has_child[i])
            .map(|leaf| self.path_to(leaf))
            .collect()
    }

    /// Rows the path-lowering fallback sends: the sum of the path lengths.
    pub fn expanded_rows(&self) -> usize {
        self.paths().iter().map(Vec::len).sum()
    }

    fn child(&self, parent: usize, token: u32) -> Option<usize> {
        self.nodes
            .iter()
            .position(|n| n.parent == Some(parent) && n.token == token)
    }
}

/// Grows a tree while keeping the depth, leaf and expanded-row bookkeeping
/// incremental, so drafters can bound nodes, depth and fallback rows cheaply.
struct Builder {
    tree: DraftTree,
    depth: Vec<usize>,
    has_child: Vec<bool>,
    expanded: usize,
    max_nodes: usize,
}

impl Builder {
    fn new(root: u32, max_nodes: usize) -> Self {
        Self {
            tree: DraftTree::root(root),
            depth: vec![0],
            has_child: vec![false],
            expanded: 1,
            max_nodes: max_nodes.clamp(1, MAX_NODES),
        }
    }

    fn full(&self) -> bool {
        self.tree.nodes.len() >= self.max_nodes
    }

    /// The existing child, or a new one if every bound allows it.
    fn child(&mut self, parent: usize, token: u32) -> Option<usize> {
        if let Some(i) = self.tree.child(parent, token) {
            return Some(i);
        }
        let depth = self.depth[parent] + 1;
        let added = if self.has_child[parent] { depth + 1 } else { 1 };
        if self.full() || depth > MAX_DEPTH || self.expanded + added > MAX_ROWS {
            return None;
        }
        self.tree.nodes.push(Node {
            parent: Some(parent),
            token,
        });
        self.depth.push(depth);
        self.has_child.push(false);
        self.has_child[parent] = true;
        self.expanded += added;
        Some(self.tree.nodes.len() - 1)
    }
}

/// How a [`TreeOutput`]'s caches are laid out.
#[derive(Debug)]
enum Caches {
    /// The caller's cache holds the committed prefix, then one position per
    /// node in node order.
    InPlace { prefix: usize },
    /// One cache per root-to-leaf path; `at[node]` is (path, offset).
    Paths {
        caches: Vec<SeqKv>,
        at: Vec<(usize, usize)>,
    },
}

/// The result of [`BatchModel::forward_tree`].
#[derive(Debug)]
pub struct TreeOutput {
    /// Optional node-aligned final residuals; empty means unsupported.
    /// Rejected-node features are never passed to the next proposal.
    pub features: Vec<Option<TargetFeatures>>,
    /// Per node: its logits, if it and every ancestor succeeded.
    pub logits: Vec<Option<Vec<i64>>>,
    /// Per node: its own error, when every ancestor succeeded and it failed
    /// (the error the model raises for that token alone).
    pub errors: Vec<Option<ModernError>>,
    /// Rows the model computed for this tree.
    pub physical_rows: usize,
    caches: Caches,
}

impl TreeOutput {
    /// Close an in-place tree step: `kv` holds the committed prefix plus one
    /// appended position per node, in node order. A node's logits survive
    /// only when it and every ancestor succeeded.
    pub fn in_place(
        tree: &DraftTree,
        kv: &mut SeqKv,
        prefix: usize,
        mut logits: Vec<Option<Vec<i64>>>,
        mut errors: Vec<Option<ModernError>>,
    ) -> Self {
        let n = tree.nodes.len();
        if let Err(e) = kv.commit(prefix + n) {
            kv.rollback(prefix);
            let message = e.to_string();
            errors = (0..n)
                .map(|_| Some(ModernError::Invalid(message.clone())))
                .collect();
            logits = (0..n).map(|_| None).collect();
            return Self {
                features: Vec::new(),
                logits,
                errors,
                physical_rows: n,
                caches: Caches::InPlace { prefix },
            };
        }
        let mut valid = vec![false; n];
        for i in 0..n {
            let parent_ok = tree.nodes[i].parent.is_none_or(|p| valid[p]);
            valid[i] = parent_ok && errors[i].is_none();
            if !parent_ok {
                errors[i] = None;
            }
            if !valid[i] {
                logits[i] = None;
            }
        }
        Self {
            features: Vec::new(),
            logits,
            errors,
            physical_rows: n,
            caches: Caches::InPlace { prefix },
        }
    }

    /// Make the root-to-`node` path the committed continuation of `kv`, the
    /// cache passed to [`BatchModel::forward_tree`].
    pub fn commit(self, tree: &DraftTree, node: usize, kv: &mut SeqKv) -> Result<(), ModernError> {
        let path = tree.path_to(node);
        match self.caches {
            Caches::InPlace { prefix } => kv.keep_path(prefix, &path),
            Caches::Paths { mut caches, at } => {
                let (seq, offset) = at[node];
                let mut chosen = caches.swap_remove(seq);
                if chosen.len() < kv.len() + offset + 1 {
                    return Err(invalid("target omitted visited KV"));
                }
                chosen.rollback(kv.len() + offset + 1);
                *kv = chosen;
                Ok(())
            }
        }
    }

    /// Leave `kv` exactly as it was before the tree step.
    pub fn discard(self, kv: &mut SeqKv) {
        if let Caches::InPlace { prefix } = self.caches {
            kv.rollback(prefix);
        }
    }
}

/// The rows of a tree step: node `i` at position `prefix + depth(i)`, and the
/// stored positions it attends to after the prefix (`prefix + ancestor` for
/// every ancestor, then `prefix + i`). Pipeline stages share one plan.
pub struct TreePlan {
    pub prefix: usize,
    pub rows: Vec<Row>,
    pub tails: Vec<Vec<usize>>,
}

impl TreePlan {
    pub fn new(tree: &DraftTree, prefix: usize) -> Self {
        let depths = tree.depths();
        let rows = tree
            .nodes
            .iter()
            .zip(&depths)
            .map(|(node, depth)| Row {
                seq: 0,
                token: node.token,
                position: prefix + depth,
                logits: true,
            })
            .collect();
        let tails = (0..tree.nodes.len())
            .map(|i| tree.path_to(i).into_iter().map(|j| prefix + j).collect())
            .collect();
        Self {
            prefix,
            rows,
            tails,
        }
    }
}

/// The portable [`BatchModel::forward_tree`]: one sequence per root-to-leaf
/// path, all in one `forward_rows` call. Shared ancestors and the prefix KV
/// are duplicated; `physical_rows` reports that cost.
pub fn forward_tree_by_paths<M: BatchModel + ?Sized>(
    model: &M,
    tree: &DraftTree,
    kv: &mut SeqKv,
) -> TreeOutput {
    let base = kv.len();
    let paths = tree.paths();
    let mut caches = vec![kv.clone(); paths.len()];
    let mut rows = Vec::new();
    let mut at = vec![(0, 0); tree.nodes.len()];
    let mut row_of = vec![0; tree.nodes.len()];
    for (seq, path) in paths.iter().enumerate() {
        for (offset, &node) in path.iter().enumerate() {
            at[node] = (seq, offset);
            row_of[node] = rows.len();
            rows.push(Row {
                seq,
                token: tree.nodes[node].token,
                position: base + offset,
                logits: true,
            });
        }
    }
    let mut refs: Vec<_> = caches.iter_mut().collect();
    let mut step = model.forward_rows(&rows, &mut refs);
    let n = tree.nodes.len();
    let mut logits: Vec<Option<Vec<i64>>> = (0..n).map(|_| None).collect();
    let mut errors: Vec<Option<ModernError>> = (0..n).map(|_| None).collect();
    if step.logits.len() != rows.len() || step.errors.len() != paths.len() {
        for error in &mut errors {
            *error = Some(invalid("model returned malformed tree result"));
        }
    } else {
        for node in 0..n {
            let (seq, offset) = at[node];
            match &step.errors[seq] {
                Some(failure) if failure.kept == offset => {
                    errors[node] = Some(ModernError::Invalid(String::new()));
                }
                Some(failure) if failure.kept < offset => {}
                _ => logits[node] = step.logits[row_of[node]].take(),
            }
        }
        // Move each failing node's real error out of its sequence's report.
        for node in 0..n {
            if errors[node].is_some() {
                let (seq, _) = at[node];
                if let Some(failure) = step.errors[seq].take() {
                    errors[node] = Some(failure.error);
                }
            }
        }
    }
    let features = if step.features.len() == rows.len() {
        row_of
            .iter()
            .enumerate()
            .map(|(node, &r)| logits[node].as_ref().and_then(|_| step.features[r].take()))
            .collect()
    } else {
        Vec::new()
    };
    TreeOutput {
        features,
        logits,
        errors,
        physical_rows: rows.len(),
        caches: Caches::Paths { caches, at },
    }
}

fn invalid(message: &str) -> ModernError {
    ModernError::Invalid(message.into())
}

/// Local drafts use only prompt + emitted output, plus whatever the target
/// already returned for earlier rows ([`Self::observe`]). The last context
/// token is the root. Depth excludes the root. No target output may be
/// peeked ahead.
pub trait TreeDrafter {
    /// Final residual of the last committed row (before the pending root).
    /// On the first proposal this is the last prompt row. Later it is the
    /// final visited node of the preceding successful verification. A head
    /// combines it with the pending token in `context.last()`. No future or
    /// rejected-row features are provided. Models without the hook pass None.
    fn propose_with_features(
        &mut self,
        context: &[u32],
        depth: usize,
        _features: Option<&TargetFeatures>,
    ) -> Result<DraftTree, ModernError> {
        self.propose(context, depth)
    }

    fn propose(&mut self, context: &[u32], depth: usize) -> Result<DraftTree, ModernError>;

    /// The target's logits after feeding `token`, for every prompt position
    /// and every verified node (accepted or not), in order. Drafters that
    /// learn from the target's own candidates use this; it never changes
    /// what the target computes.
    fn observe(&mut self, _token: u32, _logits: &[i64]) {}
}

/// Multiple n-gram matches become branches; common continuations share nodes.
/// Recent matches of the longest suffix are inserted first. No model needed.
#[derive(Debug, Clone, Copy)]
pub struct LookupTree {
    pub min_ngram: usize,
    pub max_ngram: usize,
    pub max_nodes: usize,
}

impl Default for LookupTree {
    fn default() -> Self {
        Self {
            min_ngram: 1,
            max_ngram: 4,
            max_nodes: 32,
        }
    }
}

impl LookupTree {
    fn grow(&self, builder: &mut Builder, context: &[u32], depth: usize) {
        for n in (self.min_ngram.max(1)..=self.max_ngram.min(context.len().saturating_sub(1))).rev()
        {
            let suffix = &context[context.len() - n..];
            for start in (0..context.len() - n).rev() {
                if !context[start..].starts_with(suffix) {
                    continue;
                }
                let mut parent = 0;
                for &token in context[start + n..].iter().take(depth.min(MAX_DEPTH)) {
                    match builder.child(parent, token) {
                        Some(i) => parent = i,
                        None => break,
                    }
                }
            }
        }
    }
}

impl TreeDrafter for LookupTree {
    fn propose(&mut self, context: &[u32], depth: usize) -> Result<DraftTree, ModernError> {
        let root = *context
            .last()
            .ok_or_else(|| invalid("empty draft context"))?;
        let mut builder = Builder::new(root, self.max_nodes);
        self.grow(&mut builder, context, depth);
        Ok(builder.tree)
    }
}

/// Token recycling: a draft tree built from the target's OWN top-k candidates.
///
/// Every time the target returns logits after a token (prompt rows and every
/// verified node, accepted or rejected), the `top_k` highest-ranked next tokens
/// are stored as that token's successors, replacing older ones. A proposal
/// grows the tree best-first from the root along stored successors, a
/// successor of rank `r` costing `rank_cost[r]` (default `r + 1`), until
/// `max_nodes`. No draft model,
/// no training and no extra target work: the candidates are by-products of
/// rows the target computed anyway. For a sharded model the last stage only
/// has to return `top_k` ids per row. With `lookup` set, n-gram branches are
/// placed first and recycling fills the rest of the budget. Ties break by
/// cost, then depth, then insertion order, so proposals are deterministic.
///
/// Token Recycling follows Luo et al., "Turning Trash into Treasure:
/// Accelerating Inference of Large Language Models with Token Recycling"
/// (arXiv 2408.08696); the tree shape here is a cost-ordered best-first
/// search rather than the paper's static template.
#[derive(Debug, Clone)]
pub struct RecycleTree {
    pub top_k: usize,
    pub max_nodes: usize,
    pub lookup: Option<LookupTree>,
    /// Cost of a successor by rank (the last entry repeats); a path's cost is
    /// the sum along it. Lower costs are drafted first.
    pub rank_cost: Vec<usize>,
    successors: HashMap<u32, Vec<u32>>,
}

impl RecycleTree {
    pub fn new(top_k: usize, max_nodes: usize, lookup: Option<LookupTree>) -> Self {
        Self {
            top_k: top_k.clamp(1, 16),
            max_nodes,
            lookup,
            rank_cost: (1..=16).collect(),
            successors: HashMap::new(),
        }
    }

    fn cost(&self, rank: usize) -> usize {
        self.rank_cost
            .get(rank)
            .or(self.rank_cost.last())
            .copied()
            .unwrap_or(rank + 1)
    }

    /// Forget every stored successor (a cold start).
    pub fn clear(&mut self) {
        self.successors.clear();
    }

    /// Tokens with stored successors.
    pub fn known_tokens(&self) -> usize {
        self.successors.len()
    }
}

/// The `k` highest logits' token ids, highest first, lower id first on ties.
pub fn top_k(logits: &[i64], k: usize) -> Vec<u32> {
    let mut best: Vec<(i64, u32)> = Vec::with_capacity(k + 1);
    for (token, &value) in logits.iter().enumerate() {
        if best.len() == k && best.last().is_some_and(|&(v, _)| value <= v) {
            continue;
        }
        let at = best.partition_point(|&(v, _)| v >= value);
        best.insert(at, (value, token as u32));
        best.truncate(k);
    }
    best.into_iter().map(|(_, t)| t).collect()
}

impl TreeDrafter for RecycleTree {
    fn propose(&mut self, context: &[u32], depth: usize) -> Result<DraftTree, ModernError> {
        let root = *context
            .last()
            .ok_or_else(|| invalid("empty draft context"))?;
        let mut builder = Builder::new(root, self.max_nodes);
        if let Some(lookup) = &self.lookup {
            lookup.grow(&mut builder, context, depth);
        }
        let depth = depth.min(MAX_DEPTH);
        // (cost, depth, order) -> (parent, token)
        let mut heap = BinaryHeap::new();
        let mut order = 0usize;
        let this = &*self;
        let mut seed = |heap: &mut BinaryHeap<_>, node: usize, cost: usize, builder: &Builder| {
            if builder.depth[node] >= depth {
                return;
            }
            if let Some(next) = this.successors.get(&builder.tree.nodes[node].token) {
                for (rank, &token) in next.iter().enumerate() {
                    let step = this.cost(rank);
                    heap.push(Reverse((
                        cost + step,
                        builder.depth[node] + 1,
                        order,
                        node,
                        token,
                    )));
                    order += 1;
                }
            }
        };
        for node in 0..builder.tree.nodes.len() {
            seed(&mut heap, node, 0, &builder);
        }
        while !builder.full() {
            let Some(Reverse((cost, _, _, parent, token))) = heap.pop() else {
                break;
            };
            if builder.tree.child(parent, token).is_some() {
                continue;
            }
            let Some(node) = builder.child(parent, token) else {
                continue;
            };
            seed(&mut heap, node, cost, &builder);
        }
        Ok(builder.tree)
    }

    fn observe(&mut self, token: u32, logits: &[i64]) {
        self.successors.insert(token, top_k(logits, self.top_k));
    }
}

/// Medusa-style Cartesian tree from ranked per-depth candidate heads. Heads
/// may come from an external local model; this implements the tree layout,
/// not trained Medusa/EAGLE weights. Breadth-first truncation bounds work.
pub fn head_tree(
    root: u32,
    heads: &[Vec<u32>],
    max_nodes: usize,
) -> Result<DraftTree, ModernError> {
    let mut builder = Builder::new(root, max_nodes);
    let mut frontier = vec![0];
    for head in heads.iter().take(MAX_DEPTH) {
        let mut next = Vec::new();
        for parent in frontier {
            for &token in head {
                if builder.tree.child(parent, token).is_some() {
                    continue;
                }
                match builder.child(parent, token) {
                    Some(i) => next.push(i),
                    None => return Ok(builder.tree),
                }
            }
        }
        frontier = next;
    }
    Ok(builder.tree)
}

/// Optional local draft model. Greedy rollout supplies top-k candidates at
/// each depth to `head_tree`; off-rollout combinations remain mere proposals.
/// Target and draft MUST use identical token ids. This reference drafter
/// rebuilds its prefix each call; its elapsed time belongs in end-to-end cost.
pub struct LocalModelTree<'a> {
    pub model: &'a dyn BatchModel,
    pub top_k: usize,
    pub max_nodes: usize,
}

impl TreeDrafter for LocalModelTree<'_> {
    fn propose(&mut self, context: &[u32], depth: usize) -> Result<DraftTree, ModernError> {
        let root = *context
            .last()
            .ok_or_else(|| invalid("empty draft context"))?;
        if depth == 0 || self.top_k == 0 {
            return Ok(DraftTree::root(root));
        }
        let mut kv = self.model.new_kv();
        let rows: Vec<_> = context
            .iter()
            .enumerate()
            .map(|(position, &token)| Row {
                seq: 0,
                token,
                position,
                logits: position + 1 == context.len(),
            })
            .collect();
        let mut step = self.model.forward_rows(&rows, &mut [&mut kv]);
        if let Some(failure) = step.errors[0].take() {
            return Err(failure.error);
        }
        let mut logits = step
            .logits
            .pop()
            .flatten()
            .ok_or_else(|| invalid("draft model omitted logits"))?;
        let mut heads = Vec::new();
        for at in 0..depth.min(MAX_DEPTH) {
            let mut ranked: Vec<_> = (0..logits.len()).collect();
            ranked.sort_by(|&a, &b| logits[b].cmp(&logits[a]).then(a.cmp(&b)));
            if ranked.is_empty() {
                return Err(invalid("empty draft vocabulary"));
            }
            let head: Vec<u32> = ranked
                .into_iter()
                .take(self.top_k)
                .map(|t| t as u32)
                .collect();
            let token = head[0];
            heads.push(head);
            if at + 1 == depth || kv.len() >= self.model.max_positions() {
                break;
            }
            let row = Row {
                seq: 0,
                token,
                position: kv.len(),
                logits: true,
            };
            let mut step = self.model.forward_rows(&[row], &mut [&mut kv]);
            if let Some(failure) = step.errors[0].take() {
                return Err(failure.error);
            }
            logits = step
                .logits
                .pop()
                .flatten()
                .ok_or_else(|| invalid("draft model omitted logits"))?;
        }
        head_tree(root, &heads, self.max_nodes)
    }
}

#[derive(Debug)]
pub struct TreeStep {
    pub accepted_features: Option<TargetFeatures>,
    pub emitted: Vec<u32>,
    pub logits_hashes: Vec<[u8; 32]>,
    pub finished: bool,
    /// Rows the target computed in the single verification call.
    pub verified_rows: usize,
    pub logical_nodes: usize,
    /// Rows the path-lowering fallback would send for the same tree.
    pub expanded_rows: usize,
}

/// Verify without mutating `kv` on any error. Commit only the greedy path.
/// `generated.last()` must be the tree root; the cache excludes that root.
pub fn verify_tree(
    model: &dyn BatchModel,
    tree: &DraftTree,
    kv: &mut SeqKv,
    generated: &[u32],
    selection: Selection,
    eos: &[u32],
    max_tokens: usize,
) -> Result<TreeStep, ModernError> {
    verify_tree_observed(
        model,
        tree,
        kv,
        generated,
        selection,
        eos,
        max_tokens,
        &mut |_, _| {},
    )
}

/// [`verify_tree`], handing `observe` every node's token and logits in node
/// order once the step succeeded (for [`TreeDrafter::observe`]).
#[allow(clippy::too_many_arguments)]
pub fn verify_tree_observed(
    model: &dyn BatchModel,
    tree: &DraftTree,
    kv: &mut SeqKv,
    generated: &[u32],
    selection: Selection,
    eos: &[u32],
    max_tokens: usize,
    observe: &mut dyn FnMut(u32, &[i64]),
) -> Result<TreeStep, ModernError> {
    if generated.last() != Some(&tree.nodes[0].token)
        || generated.len() >= max_tokens
        || generated.last().is_some_and(|t| eos.contains(t))
    {
        return Err(invalid("verification needs a pending nonterminal root"));
    }
    if kv.widths() != model.kv_widths() {
        return Err(invalid("wrong cache shape"));
    }
    let base = kv.len();
    let deepest = tree.depths().into_iter().max().unwrap_or(0);
    if base
        .checked_add(deepest + 1)
        .is_none_or(|end| end > model.max_positions())
    {
        return Err(invalid("tree exceeds model context"));
    }
    let mut result = model.forward_tree(tree, kv);
    let n = tree.nodes.len();
    if result.logits.len() != n || result.errors.len() != n {
        result.discard(kv);
        return Err(invalid("model returned malformed tree result"));
    }
    let mut history = generated.to_vec();
    let mut emitted = Vec::new();
    let mut hashes = Vec::new();
    let mut node = 0;
    loop {
        if let Some(error) = result.errors[node].take() {
            result.discard(kv);
            return Err(error);
        }
        let next = match result.logits[node].as_ref() {
            None => Err(invalid("missing visited logits")),
            Some(logits) if logits.len() != model.vocab_size() => {
                Err(invalid("wrong target logits width"))
            }
            Some(logits) => {
                hashes.push(arith::logits_hash(logits));
                arith::select(logits, &history, selection)
            }
        };
        let next = match next {
            Ok(next) => next,
            Err(e) => {
                result.discard(kv);
                return Err(e);
            }
        };
        emitted.push(next);
        history.push(next);
        let finished = history.len() == max_tokens || eos.contains(&next);
        let child = tree.child(node, next);
        if finished || child.is_none() {
            let observed_logits = std::mem::take(&mut result.logits);
            let accepted_features = result.features.get_mut(node).and_then(Option::take);
            let physical = result.physical_rows;
            if let Err(error) = result.commit(tree, node, kv) {
                kv.rollback(base);
                return Err(error);
            }
            for (i, logits) in observed_logits.iter().enumerate() {
                if let Some(logits) = logits {
                    observe(tree.nodes[i].token, logits);
                }
            }
            return Ok(TreeStep {
                accepted_features,
                emitted,
                logits_hashes: hashes,
                finished,
                verified_rows: physical,
                logical_nodes: n,
                expanded_rows: tree.expanded_rows(),
            });
        }
        node = child.unwrap();
    }
}

#[derive(Debug)]
pub struct TreeGeneration {
    /// Wall time: prefill includes KV setup, prompt forward, hashing/observe,
    /// and first selection. Decode includes draft, verification, commit and
    /// bookkeeping, excluding prefill. Subtimes are contained in decode.
    pub prefill_seconds: f64,
    pub decode_seconds: f64,
    pub draft_seconds: f64,
    pub verify_seconds: f64,
    pub tokens: Vec<u32>,
    pub logits_hashes: Vec<[u8; 32]>,
    pub kv_digest: [u8; 32],
    /// Decode tree calls, excludes prompt prefill and its first selected token.
    pub verification_passes: usize,
    pub verified_rows: usize,
    pub logical_nodes: usize,
    pub expanded_rows: usize,
}

pub fn generate_tree(
    model: &dyn BatchModel,
    request: &GenerationRequest<'_>,
    drafter: &mut dyn TreeDrafter,
    depth: usize,
) -> Result<TreeGeneration, ModernError> {
    if request.prompt.is_empty()
        || request.max_tokens == 0
        || request
            .prompt
            .len()
            .checked_add(request.max_tokens)
            .is_none_or(|n| n > model.max_positions())
    {
        return Err(invalid(
            "generation needs a nonempty prompt and a bounded output",
        ));
    }
    let prefill_start = Instant::now();
    let mut kv = model.new_kv();
    let rows: Vec<_> = request
        .prompt
        .iter()
        .enumerate()
        .map(|(position, &token)| Row {
            seq: 0,
            token,
            position,
            logits: true,
        })
        .collect();
    let mut step = model.forward_rows(&rows, &mut [&mut kv]);
    if let Some(failure) = step.errors[0].take() {
        return Err(failure.error);
    }
    let mut hashes = Vec::with_capacity(rows.len() + request.max_tokens);
    for (row, logits) in rows.iter().zip(&step.logits) {
        let logits = logits
            .as_ref()
            .ok_or_else(|| invalid("missing prefill logits"))?;
        hashes.push(arith::logits_hash(logits));
        drafter.observe(row.token, logits);
    }
    let first = arith::select(
        step.logits.last().unwrap().as_ref().unwrap(),
        &[],
        request.selection,
    )?;
    let mut features = step.features.last_mut().and_then(Option::take);
    drop(step);
    let mut tokens = vec![first];
    let prefill_seconds = prefill_start.elapsed().as_secs_f64();
    let decode_start = Instant::now();
    let (mut draft_seconds, mut verify_seconds) = (0.0, 0.0);
    let (mut passes, mut physical, mut logical, mut expanded) = (0, 0, 0, 0);
    while tokens.len() < request.max_tokens && !request.eos.contains(tokens.last().unwrap()) {
        let draft_start = Instant::now();
        let context: Vec<_> = request.prompt.iter().chain(&tokens).copied().collect();
        let budget = depth
            .min(MAX_DEPTH)
            .min(request.max_tokens - tokens.len() - 1);
        // Draft-only failures cannot cause a target refusal. Fall back to the
        // single root, whose verification still uses exactly the target rule.
        let proposed = drafter
            .propose_with_features(&context, budget, features.as_ref())
            .unwrap_or_else(|_| DraftTree::root(*tokens.last().unwrap()));
        let tree = if proposed.nodes[0].token != *tokens.last().unwrap()
            || proposed.depths().into_iter().any(|d| d > budget)
        {
            DraftTree::root(*tokens.last().unwrap())
        } else {
            proposed
        };
        draft_seconds += draft_start.elapsed().as_secs_f64();
        let verify_start = Instant::now();
        let verified = verify_tree_observed(
            model,
            &tree,
            &mut kv,
            &tokens,
            request.selection,
            request.eos,
            request.max_tokens,
            &mut |token, logits| drafter.observe(token, logits),
        )?;
        verify_seconds += verify_start.elapsed().as_secs_f64();
        features = verified.accepted_features;
        passes += 1;
        physical += verified.verified_rows;
        logical += verified.logical_nodes;
        expanded += verified.expanded_rows;
        tokens.extend(verified.emitted);
        hashes.extend(verified.logits_hashes);
    }
    let decode_seconds = decode_start.elapsed().as_secs_f64();
    Ok(TreeGeneration {
        prefill_seconds,
        decode_seconds,
        draft_seconds,
        verify_seconds,
        tokens,
        logits_hashes: hashes,
        kv_digest: kv.digest(),
        verification_passes: passes,
        verified_rows: physical,
        logical_nodes: logical,
        expanded_rows: expanded,
    })
}

/// Exact acceptance replay for prompt lookup ONLY. Proposals see only prompt
/// plus emitted tokens; the recorded greedy continuation is used exclusively
/// to walk each proposed tree. This cannot replay recycling or feature heads:
/// their inputs include target results for rejected nodes or hidden states.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LookupReplay {
    pub passes: usize,
    pub nodes: usize,
    pub expanded_rows: usize,
}

pub fn replay_lookup(
    prompt: &[u32],
    output: &[u32],
    max_tokens: usize,
    lookup: LookupTree,
    depth: usize,
) -> Result<LookupReplay, ModernError> {
    if prompt.is_empty() || output.is_empty() || output.len() > max_tokens {
        return Err(invalid("replay requires a complete bounded greedy output"));
    }
    let mut drafter = lookup;
    let mut totals = LookupReplay::default();
    let mut at = 1;
    while at < output.len() {
        let context: Vec<_> = prompt.iter().chain(&output[..at]).copied().collect();
        let budget = depth.min(MAX_DEPTH).min(max_tokens - at - 1);
        let tree = drafter.propose(&context, budget)?;
        totals.passes += 1;
        totals.nodes += tree.nodes.len();
        totals.expanded_rows += tree.expanded_rows();
        let mut node = 0;
        loop {
            let child = tree.child(node, output[at]);
            at += 1;
            if at == output.len() || child.is_none() {
                break;
            }
            node = child.unwrap();
        }
    }
    Ok(totals)
}

/// Projection only: assumes one traversal of `hops` one-way links per tree
/// pass. `compute_ms` must include tree compute, drafting, serialization and
/// any return-path latency not counted in hops. No network speed is measured.
pub fn projected_tokens_per_second(
    tokens_per_pass: f64,
    hops: usize,
    hop_ms: f64,
    compute_ms: f64,
) -> Result<f64, ModernError> {
    if !tokens_per_pass.is_finite()
        || tokens_per_pass <= 0.0
        || hops == 0
        || !hop_ms.is_finite()
        || hop_ms <= 0.0
        || !compute_ms.is_finite()
        || compute_ms < 0.0
    {
        return Err(invalid("invalid projection inputs"));
    }
    Ok(1000.0 * tokens_per_pass / (hops as f64 * hop_ms + compute_ms))
}

#[cfg(test)]
mod tests;
