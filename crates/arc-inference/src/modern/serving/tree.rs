//! Exact tree speculation over ENG-1's [`BatchModel`].
//!
//! Compile a parent-indexed tree to root-to-leaf sequences, then send ALL
//! sequences through ONE `forward_rows` call. A stage receives all rows at once;
//! no target call occurs per depth. Each path sees only the committed prefix
//! and its ancestors, in the reference attention order. This portable lowering
//! duplicates shared ancestors and prefix KV; it is not a shared-node attention
//! kernel. `verified_rows` exposes that cost rather than hiding it as speedup.
//!
//! Walk target argmax (or RP64) from the root, following a child only when its
//! token equals the selected token. Inductively every visited row has exactly
//! the greedy prefix, hence identical logits, tokens and KV. Rejected siblings
//! and their failures never enter the committed cache. A failure on the chosen
//! path is raised only if greedy would forward that token (not after EOS/limit).
//! Drafts affect work and acceptance only, never the computed function.

use super::{BatchModel, Row, SeqKv};
use crate::modern::ModernError;
use crate::modern::arith::{self, Selection};
use crate::modern::model::GenerationRequest;

const MAX_NODES: usize = 256;
const MAX_DEPTH: usize = 64;
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
        if tree.paths().iter().map(Vec::len).sum::<usize>() > MAX_ROWS {
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

    fn paths(&self) -> Vec<Vec<usize>> {
        let mut has_child = vec![false; self.nodes.len()];
        for node in &self.nodes[1..] {
            has_child[node.parent.unwrap()] = true;
        }
        has_child
            .iter()
            .enumerate()
            .filter(|(_, child)| !**child)
            .map(|(leaf, _)| {
                let mut path = vec![leaf];
                let mut at = leaf;
                while let Some(parent) = self.nodes[at].parent {
                    path.push(parent);
                    at = parent;
                }
                path.reverse();
                path
            })
            .collect()
    }
}

fn invalid(message: &str) -> ModernError {
    ModernError::Invalid(message.into())
}

/// Local drafts use only prompt + emitted output. The last context token is
/// the root. Depth excludes the root. No target output may be peeked ahead.
pub trait TreeDrafter {
    fn propose(&self, context: &[u32], depth: usize) -> Result<DraftTree, ModernError>;
}

/// Multiple n-gram matches become branches; common continuations share nodes.
/// Recent matches of the longest suffix are inserted first. No model needed.
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

impl TreeDrafter for LookupTree {
    fn propose(&self, context: &[u32], depth: usize) -> Result<DraftTree, ModernError> {
        let root = *context
            .last()
            .ok_or_else(|| invalid("empty draft context"))?;
        let mut tree = DraftTree::root(root);
        let limit = self.max_nodes.clamp(1, MAX_NODES);
        for n in (self.min_ngram.max(1)..=self.max_ngram.min(context.len().saturating_sub(1))).rev()
        {
            let suffix = &context[context.len() - n..];
            for start in (0..context.len() - n).rev() {
                if !context[start..].starts_with(suffix) {
                    continue;
                }
                let mut parent = 0;
                for &token in context[start + n..].iter().take(depth.min(MAX_DEPTH)) {
                    if let Some(i) = tree
                        .nodes
                        .iter()
                        .position(|node| node.parent == Some(parent) && node.token == token)
                    {
                        parent = i;
                    } else {
                        if tree.nodes.len() == limit {
                            break;
                        }
                        tree.nodes.push(Node {
                            parent: Some(parent),
                            token,
                        });
                        // Bound actual materialized work, not just logical nodes.
                        if tree.paths().iter().map(Vec::len).sum::<usize>() > MAX_ROWS {
                            tree.nodes.pop();
                            break;
                        }
                        parent = tree.nodes.len() - 1;
                    }
                }
            }
        }
        Ok(tree)
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
    let mut tree = DraftTree::root(root);
    let mut frontier = vec![0];
    for head in heads.iter().take(MAX_DEPTH) {
        let mut next = Vec::new();
        for parent in frontier {
            for &token in head {
                if tree.nodes.len() >= max_nodes.clamp(1, MAX_NODES) {
                    return Ok(tree);
                }
                if tree
                    .nodes
                    .iter()
                    .any(|n| n.parent == Some(parent) && n.token == token)
                {
                    continue;
                }
                tree.nodes.push(Node {
                    parent: Some(parent),
                    token,
                });
                if tree.paths().iter().map(Vec::len).sum::<usize>() > MAX_ROWS {
                    tree.nodes.pop();
                    return Ok(tree);
                }
                next.push(tree.nodes.len() - 1);
            }
        }
        frontier = next;
    }
    Ok(tree)
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
    fn propose(&self, context: &[u32], depth: usize) -> Result<DraftTree, ModernError> {
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
    pub emitted: Vec<u32>,
    pub logits_hashes: Vec<[u8; 32]>,
    pub finished: bool,
    /// Physical rows sent in the single target verification call.
    pub verified_rows: usize,
    pub logical_nodes: usize,
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
    let paths = tree.paths();
    if paths.iter().any(|path| {
        base.checked_add(path.len())
            .is_none_or(|end| end > model.max_positions())
    }) {
        return Err(invalid("tree exceeds model context"));
    }
    let mut caches = vec![kv.clone(); paths.len()];
    let mut rows = Vec::new();
    let mut locations = vec![(0, 0, 0); tree.nodes.len()];
    for (seq, path) in paths.iter().enumerate() {
        for (offset, &node) in path.iter().enumerate() {
            locations[node] = (seq, offset, rows.len());
            rows.push(Row {
                seq,
                token: tree.nodes[node].token,
                position: base + offset,
                logits: true,
            });
        }
    }
    let mut refs: Vec<_> = caches.iter_mut().collect();
    let mut result = model.forward_rows(&rows, &mut refs);
    if result.logits.len() != rows.len() || result.errors.len() != paths.len() {
        return Err(invalid("model returned malformed tree result"));
    }
    let mut history = generated.to_vec();
    let mut emitted = Vec::new();
    let mut hashes = Vec::new();
    let mut node = 0;
    loop {
        let (seq, offset, row) = locations[node];
        if result.errors[seq]
            .as_ref()
            .is_some_and(|e| e.kept <= offset)
        {
            return Err(result.errors[seq].take().unwrap().error);
        }
        let logits = result.logits[row]
            .as_ref()
            .ok_or_else(|| invalid("missing visited logits"))?;
        if logits.len() != model.vocab_size() {
            return Err(invalid("wrong target logits width"));
        }
        let next = arith::select(logits, &history, selection)?;
        hashes.push(arith::logits_hash(logits));
        emitted.push(next);
        history.push(next);
        let finished = history.len() == max_tokens || eos.contains(&next);
        let child = tree
            .nodes
            .iter()
            .position(|n| n.parent == Some(node) && n.token == next);
        if finished || child.is_none() {
            let mut chosen = caches.swap_remove(seq);
            if chosen.len() < base + offset + 1 {
                return Err(invalid("target omitted visited KV"));
            }
            chosen.rollback(base + offset + 1);
            *kv = chosen;
            return Ok(TreeStep {
                emitted,
                logits_hashes: hashes,
                finished,
                verified_rows: rows.len(),
                logical_nodes: tree.nodes.len(),
            });
        }
        node = child.unwrap();
    }
}

#[derive(Debug)]
pub struct TreeGeneration {
    pub tokens: Vec<u32>,
    pub logits_hashes: Vec<[u8; 32]>,
    pub kv_digest: [u8; 32],
    /// Decode tree calls, excludes prompt prefill and its first selected token.
    pub verification_passes: usize,
    pub verified_rows: usize,
    pub logical_nodes: usize,
}

pub fn generate_tree(
    model: &dyn BatchModel,
    request: &GenerationRequest<'_>,
    drafter: &dyn TreeDrafter,
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
    let mut hashes = step
        .logits
        .iter()
        .map(|l| {
            l.as_ref()
                .map(|l| arith::logits_hash(l))
                .ok_or_else(|| invalid("missing prefill logits"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let first = arith::select(
        step.logits.last().unwrap().as_ref().unwrap(),
        &[],
        request.selection,
    )?;
    let mut tokens = vec![first];
    let (mut passes, mut physical, mut logical) = (0, 0, 0);
    while tokens.len() < request.max_tokens && !request.eos.contains(tokens.last().unwrap()) {
        let context: Vec<_> = request.prompt.iter().chain(&tokens).copied().collect();
        let budget = depth
            .min(MAX_DEPTH)
            .min(request.max_tokens - tokens.len() - 1);
        // Draft-only failures cannot cause a target refusal. Fall back to the
        // single root, whose verification still uses exactly the target rule.
        let proposed = drafter
            .propose(&context, budget)
            .unwrap_or_else(|_| DraftTree::root(*tokens.last().unwrap()));
        let tree = if proposed.nodes[0].token != *tokens.last().unwrap()
            || proposed.paths().iter().any(|p| p.len() > budget + 1)
        {
            DraftTree::root(*tokens.last().unwrap())
        } else {
            proposed
        };
        let verified = verify_tree(
            model,
            &tree,
            &mut kv,
            &tokens,
            request.selection,
            request.eos,
            request.max_tokens,
        )?;
        passes += 1;
        physical += verified.verified_rows;
        logical += verified.logical_nodes;
        tokens.extend(verified.emitted);
        hashes.extend(verified.logits_hashes);
    }
    Ok(TreeGeneration {
        tokens,
        logits_hashes: hashes,
        kv_digest: kv.digest(),
        verification_passes: passes,
        verified_rows: physical,
        logical_nodes: logical,
    })
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
