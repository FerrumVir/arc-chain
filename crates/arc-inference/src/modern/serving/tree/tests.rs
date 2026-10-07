use super::*;
use crate::modern::model::tests::tiny_model;
use crate::modern::serving::dense::DenseModel;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting<'a> {
    inner: &'a dyn BatchModel,
    calls: AtomicUsize,
}
impl BatchModel for Counting<'_> {
    fn vocab_size(&self) -> usize {
        self.inner.vocab_size()
    }
    fn max_positions(&self) -> usize {
        self.inner.max_positions()
    }
    fn kv_widths(&self) -> Vec<usize> {
        self.inner.kv_widths()
    }
    fn identity(&self) -> [u8; 32] {
        self.inner.identity()
    }
    fn forward_rows(&self, rows: &[Row], kvs: &mut [&mut SeqKv]) -> super::super::StepOutput {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.forward_rows(rows, kvs)
    }
}

// Deliberately oracle-fed ONLY in tests to exercise deep acceptance. Benchmark
// drafters never get the baseline continuation.
struct KnownHeads {
    prompt: usize,
    output: Vec<u32>,
    wrong_first: bool,
}
impl TreeDrafter for KnownHeads {
    fn propose(&self, context: &[u32], depth: usize) -> Result<DraftTree, ModernError> {
        let at = context.len() - self.prompt;
        let heads: Vec<_> = self
            .output
            .iter()
            .skip(at)
            .take(depth)
            .map(|&t| {
                if self.wrong_first {
                    vec![(t + 1) % 40, t]
                } else {
                    vec![t, (t + 1) % 40]
                }
            })
            .collect();
        head_tree(*context.last().unwrap(), &heads, 64)
    }
}

#[test]
fn exact_tokens_logits_and_committed_kv_across_trees_kernels_and_stops() {
    let model = tiny_model();
    let dense = DenseModel::new(&model, [0; 32]);
    let _guard = crate::canonical_simd::kernel_switch_guard();
    for fast in [false, true] {
        crate::canonical_simd::set_fast_canonical_kernel(fast);
        for selection in [Selection::Argmax, Selection::Rp64Argmax] {
            for prompt in [vec![1, 2, 3], vec![9, 3, 9, 3, 9], vec![0, 39, 18]] {
                for max_tokens in [1, 2, 7, 15] {
                    let request = GenerationRequest {
                        prompt: &prompt,
                        max_tokens,
                        eos: &[],
                        selection,
                    };
                    let reference = model.generate(&request).unwrap();
                    for eos in [
                        vec![],
                        vec![reference.tokens[0]],
                        vec![*reference.tokens.last().unwrap()],
                    ] {
                        let req = GenerationRequest {
                            eos: &eos,
                            ..request.clone()
                        };
                        let baseline = model.generate(&req).unwrap();
                        let mut reference_kv = model.new_cache();
                        for &t in prompt
                            .iter()
                            .chain(&baseline.tokens[..baseline.tokens.len() - 1])
                        {
                            model.forward(t, &mut reference_kv).unwrap();
                        }
                        let known = KnownHeads {
                            prompt: prompt.len(),
                            output: reference.tokens.clone(),
                            wrong_first: true,
                        };
                        for depth in [0, 1, 4, 12] {
                            for drafter in [&known as &dyn TreeDrafter, &LookupTree::default()] {
                                let out = generate_tree(&dense, &req, drafter, depth).unwrap();
                                assert_eq!(out.tokens, baseline.tokens);
                                assert_eq!(arith::tokens_hash(&out.tokens), baseline.output_hash);
                                assert_eq!(out.logits_hashes, baseline.logits_hashes);
                                assert_eq!(
                                    arith::logits_digest(&out.logits_hashes),
                                    baseline.logits_digest
                                );
                                assert_eq!(out.kv_digest, reference_kv.digest());
                            }
                        }
                    }
                }
            }
        }
    }
    crate::canonical_simd::set_fast_canonical_kernel(false);
}

#[test]
fn deep_branch_verification_is_one_batch_and_rejected_invalid_sibling_is_harmless() {
    let model = tiny_model();
    let dense = DenseModel::new(&model, [0; 32]);
    let request = GenerationRequest {
        prompt: &[1, 2, 3],
        max_tokens: 8,
        eos: &[],
        selection: Selection::Rp64Argmax,
    };
    let baseline = model.generate(&request).unwrap();
    let mut kv = dense.new_kv();
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
    dense.forward_rows(&rows, &mut [&mut kv]);
    let mut nodes = vec![Node {
        parent: None,
        token: baseline.tokens[0],
    }];
    for &token in &baseline.tokens[1..7] {
        let parent = nodes.len() - 1;
        nodes.push(Node {
            parent: Some(parent),
            token: u32::MAX,
        });
        nodes.push(Node {
            parent: Some(parent),
            token,
        });
    }
    let tree = DraftTree::new(nodes).unwrap();
    let count = Counting {
        inner: &dense,
        calls: AtomicUsize::new(0),
    };
    let out = verify_tree(
        &count,
        &tree,
        &mut kv,
        &baseline.tokens[..1],
        request.selection,
        &[],
        8,
    )
    .unwrap();
    assert_eq!(count.calls.load(Ordering::Relaxed), 1);
    assert_eq!(out.emitted, baseline.tokens[1..]);
    assert_eq!(out.logits_hashes, baseline.logits_hashes[3..]);
    assert!(out.verified_rows > out.logical_nodes); // ancestor duplication visible
    let mut plain = model.new_cache();
    for &t in request.prompt.iter().chain(&baseline.tokens[..7]) {
        model.forward(t, &mut plain).unwrap();
    }
    assert_eq!(kv.digest(), plain.digest());
}

struct FailAtPosition<'a> {
    inner: &'a dyn BatchModel,
    position: usize,
}
impl BatchModel for FailAtPosition<'_> {
    fn vocab_size(&self) -> usize {
        self.inner.vocab_size()
    }
    fn max_positions(&self) -> usize {
        self.inner.max_positions()
    }
    fn kv_widths(&self) -> Vec<usize> {
        self.inner.kv_widths()
    }
    fn identity(&self) -> [u8; 32] {
        self.inner.identity()
    }
    fn forward_rows(&self, rows: &[Row], kvs: &mut [&mut SeqKv]) -> super::super::StepOutput {
        let base: Vec<_> = kvs.iter().map(|kv| kv.len()).collect();
        let mut result = self.inner.forward_rows(rows, kvs);
        for seq in 0..kvs.len() {
            if let Some((offset, _)) = rows
                .iter()
                .filter(|r| r.seq == seq)
                .enumerate()
                .find(|(_, r)| r.position == self.position)
            {
                kvs[seq].rollback(base[seq] + offset);
                result.errors[seq] = Some(super::super::Failure {
                    kept: offset,
                    error: ModernError::Domain("injected row failure".into()),
                });
                for (i, _) in rows
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.seq == seq)
                    .skip(offset)
                {
                    result.logits[i] = None;
                }
            }
        }
        result
    }
}

#[test]
fn chosen_failure_is_transactional_but_eos_and_limit_do_not_forward_it() {
    let mut model = tiny_model();
    for gain in &mut model.final_norm {
        *gain = -*gain;
    }
    let dense = DenseModel::new(&model, [0; 32]);
    let mut kv = dense.new_kv();
    let root = 3;
    let mut reference = model.new_cache();
    let next = arith::select(
        &model.forward(root, &mut reference).unwrap(),
        &[root],
        Selection::Argmax,
    )
    .unwrap();
    assert_ne!(root, next);
    let failing = FailAtPosition {
        inner: &dense,
        position: 1,
    };
    let tree = DraftTree::new(vec![
        Node {
            parent: None,
            token: root,
        },
        Node {
            parent: Some(0),
            token: next,
        },
    ])
    .unwrap();
    let before = kv.clone();
    assert!(verify_tree(&failing, &tree, &mut kv, &[root], Selection::Argmax, &[], 5).is_err());
    assert_eq!(kv, before);
    for (eos, limit) in [(vec![next], 5), (vec![], 2)] {
        let out = verify_tree(
            &failing,
            &tree,
            &mut kv,
            &[root],
            Selection::Argmax,
            &eos,
            limit,
        )
        .unwrap();
        assert_eq!(out.emitted, vec![next]);
        assert!(out.finished);
        assert_eq!(kv.digest(), reference.digest());
        kv = before.clone();
    }
}

#[test]
fn malformed_trees_context_limits_and_projection_inputs_refuse() {
    assert!(DraftTree::new(vec![]).is_err());
    assert!(
        DraftTree::new(vec![Node {
            parent: Some(0),
            token: 0
        }])
        .is_err()
    );
    for parent in [None, Some(1), Some(5)] {
        assert!(
            DraftTree::new(vec![
                Node {
                    parent: None,
                    token: 0
                },
                Node { parent, token: 1 }
            ])
            .is_err()
        );
    }
    assert!(
        DraftTree::new(vec![
            Node {
                parent: None,
                token: 0
            },
            Node {
                parent: Some(0),
                token: 1
            },
            Node {
                parent: Some(0),
                token: 1
            }
        ])
        .is_err()
    );
    let nodes = (0usize..66)
        .map(|i| Node {
            parent: i.checked_sub(1),
            token: 0,
        })
        .collect();
    assert!(DraftTree::new(nodes).is_err());
    let model = tiny_model();
    let dense = DenseModel::new(&model, [0; 32]);
    for (prompt, max_tokens) in [(vec![], 1), (vec![1], 0), (vec![1; 31], 2), (vec![40], 1)] {
        let request = GenerationRequest {
            prompt: &prompt,
            max_tokens,
            eos: &[],
            selection: Selection::Argmax,
        };
        assert!(generate_tree(&dense, &request, &LookupTree::default(), 8).is_err());
    }
    assert!((projected_tokens_per_second(8.0, 8, 10.0, 20.0).unwrap() - 80.0).abs() < 1e-12);
    for (tokens, hops, hop_ms, compute) in [
        (f64::NAN, 8, 10.0, 0.0),
        (1.0, 0, 10.0, 0.0),
        (1.0, 8, 0.0, 0.0),
        (1.0, 8, 10.0, -1.0),
    ] {
        assert!(projected_tokens_per_second(tokens, hops, hop_ms, compute).is_err());
    }
}

#[test]
fn lookup_forks_matches_and_local_draft_keeps_target_bytes() {
    let lookup = LookupTree::default();
    let tree = lookup.propose(&[1, 2, 7, 8, 1, 2, 9, 10, 1, 2], 2).unwrap();
    assert!(
        tree.nodes
            .iter()
            .any(|n| n.parent == Some(0) && n.token == 7)
    );
    assert!(
        tree.nodes
            .iter()
            .any(|n| n.parent == Some(0) && n.token == 9)
    );
    let model = tiny_model();
    let dense = DenseModel::new(&model, [0; 32]);
    let draft = LocalModelTree {
        model: &dense,
        top_k: 2,
        max_nodes: 32,
    };
    let request = GenerationRequest {
        prompt: &[1, 2, 3],
        max_tokens: 12,
        eos: &[],
        selection: Selection::Argmax,
    };
    let out = generate_tree(&dense, &request, &draft, 5).unwrap();
    let baseline = model.generate(&request).unwrap();
    assert_eq!(out.tokens, baseline.tokens);
    assert_eq!(out.logits_hashes, baseline.logits_hashes);
    assert!(out.verification_passes < 11);
}
