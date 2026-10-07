//! The serving layer's proofs: batch invariance, phase invariance, an
//! invisible prefix cache, exact speculative decoding, per-request failure
//! isolation, and the same scheduler driving a routed mixture-of-experts
//! fixture and a two-stage pipeline.
//!
//! Every comparison is against `ModernModel::forward` or
//! `ModernModel::generate`, the single-token path that the golden digests
//! pin, and is byte for byte: tokens, every logits hash and the KV digest.

use std::collections::HashMap;

use super::dense::{
    DenseFfn, DenseModel, FeedForward, dense_kv_widths, embed_rows, failed_step, final_logits,
    finish_step, forward_layers, layered_forward, silu_rows,
};
use super::gemm::project_rows;
use super::prefix::{PrefixCache, PrefixConfig};
use super::scheduler::{Completion, Request, Scheduler, SchedulerConfig};
use super::spec::{Drafter, PromptLookup, accept};
use super::{BatchModel, Failure, Row, SeqKv, StepOutput, check_rows};
use crate::modern::ModernError;
use crate::modern::arith::{self, DyadicMatrix, ONE, Selection, exp_q16};
use crate::modern::model::tests::{lcg_matrix, tiny_model};
use crate::modern::model::{GenerationRequest, ModernConfig, ModernLayer, ModernModel};
use crate::modern::tables::rope_tables;

const VOCAB: u32 = 64;

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

fn token(state: &mut u64) -> u32 {
    (lcg(state) % u64::from(VOCAB)) as u32
}

/// A SmolLM3-shaped model: GQA 4:2, NoPE on every 4th layer, vocabulary 64.
fn model_with(n_layers: usize, max_seq: usize, seed: u64) -> ModernModel {
    let config = ModernConfig {
        architecture: "smollm3".into(),
        n_layers,
        d_model: 32,
        n_heads: 4,
        n_kv_heads: 2,
        d_head: 8,
        d_ff: 48,
        vocab_size: VOCAB as usize,
        max_seq,
        rms_eps_q32: 4295,
        rope_theta: 5_000_000,
        rope_layers: (0..n_layers).map(|l| !(l + 1).is_multiple_of(4)).collect(),
    };
    let mut seed = seed;
    let (rope_cos, rope_sin) = rope_tables(config.rope_theta, config.d_head, max_seq).unwrap();
    let layers = (0..n_layers)
        .map(|l| ModernLayer {
            attn_norm: vec![ONE + 97 * l as i64; 32],
            wq: lcg_matrix(&mut seed, 32, 32),
            wk: lcg_matrix(&mut seed, 16, 32),
            wv: lcg_matrix(&mut seed, 16, 32),
            wo: lcg_matrix(&mut seed, 32, 32),
            ffn_norm: vec![ONE + 1000; 32],
            w_gate: lcg_matrix(&mut seed, 48, 32),
            w_up: lcg_matrix(&mut seed, 48, 32),
            w_down: lcg_matrix(&mut seed, 32, 48),
        })
        .collect();
    let model = ModernModel {
        embed: lcg_matrix(&mut seed, VOCAB as usize, 32),
        final_norm: vec![ONE; 32],
        rope_cos,
        rope_sin,
        layers,
        config,
    };
    model.validate().unwrap();
    model
}

fn random_requests(count: usize, seed: u64) -> Vec<Request> {
    let mut state = seed;
    (0..count)
        .map(|i| {
            let len = 1 + (lcg(&mut state) % 30) as usize;
            let prompt: Vec<u32> = (0..len).map(|_| token(&mut state)).collect();
            let max_tokens = 1 + (lcg(&mut state) % 12) as usize;
            let eos = if i.is_multiple_of(3) {
                vec![token(&mut state)]
            } else {
                Vec::new()
            };
            let selection = if i.is_multiple_of(2) {
                Selection::Argmax
            } else {
                Selection::Rp64Argmax
            };
            Request {
                id: i as u64,
                prompt,
                max_tokens,
                eos,
                selection,
            }
        })
        .collect()
}

/// `ModernModel::generate`: the golden path.
fn reference(model: &ModernModel, request: &Request) -> (Vec<u32>, Vec<[u8; 32]>) {
    let out = model
        .generate(&GenerationRequest {
            prompt: &request.prompt,
            max_tokens: request.max_tokens,
            eos: &request.eos,
            selection: request.selection,
        })
        .unwrap();
    (out.tokens, out.logits_hashes)
}

/// KV digest after plain decoding: prompt plus every token but the last.
fn replay_digest(model: &ModernModel, prompt: &[u32], tokens: &[u32]) -> [u8; 32] {
    let mut cache = model.new_cache();
    for &t in prompt.iter().chain(&tokens[..tokens.len() - 1]) {
        model.forward(t, &mut cache).unwrap();
    }
    cache.digest()
}

fn serve<M: BatchModel>(
    model: &M,
    config: SchedulerConfig,
    requests: &[Request],
) -> HashMap<u64, Completion> {
    let mut scheduler = Scheduler::new(model, config);
    for request in requests {
        scheduler.submit(request.clone()).unwrap();
    }
    scheduler.run().into_iter().map(|c| (c.id, c)).collect()
}

/// Tokens, logits hashes and (when recorded) the KV digest equal plain
/// decoding. Without `all_logits` the hashes start at the last prompt position.
fn assert_matches_reference(
    model: &ModernModel,
    request: &Request,
    completion: &Completion,
    all_logits: bool,
) {
    let generated = completion
        .result
        .as_ref()
        .unwrap_or_else(|e| panic!("request {} failed: {e}", request.id));
    let (tokens, hashes) = reference(model, request);
    assert_eq!(generated.tokens, tokens, "tokens of request {}", request.id);
    assert_eq!(generated.output_hash, arith::tokens_hash(&tokens));
    if all_logits {
        assert_eq!(generated.logits_hashes, hashes, "logits of {}", request.id);
    } else {
        assert_eq!(
            generated.logits_hashes[..],
            hashes[request.prompt.len() - 1..],
            "logits of {}",
            request.id
        );
    }
    if let Some(digest) = generated.kv_digest {
        assert_eq!(digest, replay_digest(model, &request.prompt, &tokens));
    }
}

fn config(max_running: usize) -> SchedulerConfig {
    SchedulerConfig {
        max_running,
        kv_digests: true,
        ..SchedulerConfig::default()
    }
}

#[test]
fn batched_projection_equals_one_row_at_a_time() {
    let _guard = crate::canonical_simd::kernel_switch_guard();
    let was = crate::canonical_simd::fast_canonical_kernel_enabled();
    let mut seed = 0x9e37_79b9_u64;
    // Edge magnitudes: zero, small, digit boundaries on both ISAs, past them
    // (scalar fallback for that row only), and a last row outside the
    // projection domain (an error for that row only).
    let magnitudes: [i64; 8] = [
        0,
        1,
        300,
        1 << 15,
        1 << 20,
        (1 << 31) - (1 << 15) - 1,
        1 << 31,
        3 << 31,
    ];
    for cols in [1usize, 15, 16, 17, 33, 64, 2049, 4111] {
        let m = lcg_matrix(&mut seed, 5, cols);
        let n = magnitudes.len() + 1;
        let mut xs = vec![1i64 << 57; n * cols];
        for (row, &limit) in xs.chunks_mut(cols).zip(&magnitudes) {
            for (c, value) in row.iter_mut().enumerate() {
                let draw = ((lcg(&mut seed) << 31) | lcg(&mut seed)) as i64;
                *value = if c == 0 {
                    limit
                } else if c == 1 {
                    -limit
                } else {
                    draw % (2 * limit + 1) - limit
                };
            }
        }
        for simd in [false, true] {
            crate::canonical_simd::set_fast_canonical_kernel(simd);
            let mut out = vec![0i64; n * 5];
            let mut errors: Vec<Option<ModernError>> = (0..n).map(|_| None).collect();
            project_rows(&m, &xs, &mut out, &mut errors);
            for (r, error) in errors.iter().enumerate() {
                let mut alone = [0i64; 5];
                let expected = arith::project(&m, &xs[r * cols..(r + 1) * cols], &mut alone);
                match (expected, error) {
                    (Ok(()), None) => {
                        assert_eq!(out[r * 5..(r + 1) * 5], alone, "cols {cols} row {r} {simd}")
                    }
                    (Err(a), Some(b)) => assert_eq!(a.to_string(), b.to_string()),
                    (a, b) => panic!("cols {cols} row {r}: alone {a:?}, batched {b:?}"),
                }
            }
            assert!(
                errors[n - 1].is_some(),
                "the last row is outside the domain"
            );
        }
    }
    crate::canonical_simd::set_fast_canonical_kernel(was);
}

#[test]
fn a_prompt_gives_the_same_bytes_in_one_prefill_in_chunks_and_token_by_token() {
    let _guard = crate::canonical_simd::kernel_switch_guard();
    let was = crate::canonical_simd::fast_canonical_kernel_enabled();
    let model = model_with(4, 96, 7);
    let dense = DenseModel::new(&model, [7; 32]);
    let mut state = 11u64;
    let prompt: Vec<u32> = (0..41).map(|_| token(&mut state)).collect();
    let mut cache = model.new_cache();
    let reference: Vec<[u8; 32]> = prompt
        .iter()
        .map(|&t| arith::logits_hash(&model.forward(t, &mut cache).unwrap()))
        .collect();
    let reference_kv = cache.digest();
    let splits: [&[usize]; 4] = [&[41], &[1, 7, 16, 3, 14], &[1; 41], &[40, 1]];
    for simd in [false, true] {
        crate::canonical_simd::set_fast_canonical_kernel(simd);
        for chunks in splits {
            let mut kv = dense.new_kv();
            let mut hashes = Vec::new();
            let mut position = 0usize;
            for &size in chunks {
                let rows: Vec<Row> = (position..position + size)
                    .map(|p| Row {
                        seq: 0,
                        token: prompt[p],
                        position: p,
                        logits: true,
                    })
                    .collect();
                let out = dense.forward_rows(&rows, &mut [&mut kv]);
                assert!(out.errors[0].is_none());
                for logits in &out.logits {
                    hashes.push(arith::logits_hash(logits.as_ref().unwrap()));
                }
                position += size;
            }
            assert_eq!(hashes, reference, "chunks {chunks:?} simd {simd}");
            assert_eq!(kv.digest(), reference_kv, "chunks {chunks:?} simd {simd}");
        }
    }
    crate::canonical_simd::set_fast_canonical_kernel(was);
}

#[test]
fn every_request_is_byte_identical_at_batch_1_8_and_32() {
    let model = model_with(3, 64, 21);
    let dense = DenseModel::new(&model, [21; 32]);
    let requests = random_requests(32, 5);
    let mut reversed = requests.clone();
    reversed.reverse();
    let shapes = [
        (1usize, 256usize, 64usize),
        (8, 40, 5),
        (32, 7, 3),
        (32, 512, 64),
    ];
    for (max_running, step_tokens, prefill_chunk) in shapes {
        for order in [&requests, &reversed] {
            let settings = SchedulerConfig {
                max_running,
                step_tokens,
                prefill_chunk,
                all_logits: true,
                kv_digests: true,
                ..SchedulerConfig::default()
            };
            let completions = serve(&dense, settings, order);
            assert_eq!(completions.len(), requests.len());
            for request in &requests {
                assert_matches_reference(&model, request, &completions[&request.id], true);
            }
        }
    }
}

#[test]
fn a_request_does_not_depend_on_its_neighbours() {
    let model = model_with(3, 64, 33);
    let dense = DenseModel::new(&model, [33; 32]);
    let target = Request {
        id: 1_000,
        prompt: vec![5, 9, 13, 2, 60, 7, 7, 31],
        max_tokens: 10,
        eos: Vec::new(),
        selection: Selection::Rp64Argmax,
    };
    let mut outputs = Vec::new();
    for (neighbours, seed) in [(0usize, 1u64), (7, 2), (31, 3), (31, 4)] {
        let mut batch = random_requests(neighbours, seed);
        batch.insert(neighbours / 2, target.clone());
        let settings = SchedulerConfig {
            step_tokens: 512,
            ..config(32)
        };
        let completions = serve(&dense, settings, &batch);
        let completion = &completions[&target.id];
        assert_matches_reference(&model, &target, completion, false);
        outputs.push(completion.result.clone());
    }
    assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn the_prefix_cache_never_changes_a_byte() {
    let model = model_with(3, 96, 45);
    let dense = DenseModel::new(&model, [45; 32]);
    let mut state = 99u64;
    let system: Vec<u32> = (0..37).map(|_| token(&mut state)).collect();
    let requests: Vec<Request> = (0..6usize)
        .map(|i| {
            let mut prompt = system.clone();
            prompt.extend((0..3 + i).map(|_| token(&mut state)));
            Request {
                id: i as u64,
                prompt,
                max_tokens: 8,
                eos: Vec::new(),
                selection: Selection::Rp64Argmax,
            }
        })
        .collect();
    let plain = serve(&dense, config(1), &requests);
    // 384 bytes per position here, so 8 KB holds about five 4-position blocks
    // and forces evictions.
    let caches = [
        (4usize, usize::MAX),
        (1, usize::MAX),
        (16, usize::MAX),
        (4, 8_000),
    ];
    for (block, capacity_bytes) in caches {
        for max_running in [1usize, 6] {
            let settings = SchedulerConfig {
                prefix: Some(PrefixConfig {
                    block,
                    capacity_bytes,
                }),
                ..config(max_running)
            };
            let mut scheduler = Scheduler::new(&dense, settings);
            let mut completions = HashMap::new();
            for request in &requests {
                scheduler.submit(request.clone()).unwrap();
                if max_running == 1 {
                    completions.extend(scheduler.run().into_iter().map(|c| (c.id, c)));
                }
            }
            completions.extend(scheduler.run().into_iter().map(|c| (c.id, c)));
            let mut hits = 0usize;
            for request in &requests {
                let cached = completions[&request.id].result.as_ref().unwrap();
                let fresh = plain[&request.id].result.as_ref().unwrap();
                assert_eq!(cached.tokens, fresh.tokens, "block {block}");
                assert_eq!(cached.logits_hashes, fresh.logits_hashes, "block {block}");
                assert_eq!(cached.kv_digest, fresh.kv_digest, "block {block}");
                assert_matches_reference(&model, request, &completions[&request.id], false);
                hits += cached.cached_tokens;
            }
            if max_running == 1 && capacity_bytes == usize::MAX {
                assert!(
                    hits >= 5 * (37 / block) * block,
                    "block {block}: {hits} hits"
                );
            }
        }
    }
}

#[test]
fn prefix_keys_commit_to_the_model_and_the_whole_prefix() {
    let settings = PrefixConfig {
        block: 2,
        capacity_bytes: usize::MAX,
    };
    let a = PrefixCache::new([1; 32], settings);
    let b = PrefixCache::new([2; 32], settings);
    assert_ne!(a.block_key(None, &[1, 2]), b.block_key(None, &[1, 2]));
    let root = a.block_key(None, &[1, 2]);
    let other = a.block_key(None, &[3, 4]);
    assert_ne!(
        a.block_key(Some(&root), &[5, 6]),
        a.block_key(Some(&other), &[5, 6])
    );
    // A cache never serves the last prompt position, and a miss copies nothing.
    let model = model_with(1, 32, 3);
    let dense = DenseModel::new(&model, [3; 32]);
    let mut cache = PrefixCache::new(dense.identity(), settings);
    let tokens = [1u32, 2, 3, 4, 5];
    let mut kv = dense.new_kv();
    let rows: Vec<Row> = tokens
        .iter()
        .enumerate()
        .map(|(p, &t)| Row {
            seq: 0,
            token: t,
            position: p,
            logits: false,
        })
        .collect();
    assert!(dense.forward_rows(&rows, &mut [&mut kv]).errors[0].is_none());
    cache.insert(&tokens, &kv);
    let mut fresh = dense.new_kv();
    assert_eq!(cache.lookup_into(&tokens, tokens.len() - 1, &mut fresh), 4);
    assert_eq!(fresh.export(0, 4), kv.export(0, 4));
    let mut miss = dense.new_kv();
    assert_eq!(cache.lookup_into(&[9, 9, 9], 2, &mut miss), 0);
    assert!(miss.is_empty());
}

/// Generated-prefix reuse (an agent's next turn re-sends the conversation) and
/// the KV-cache invariant of Vosti (arXiv 2609.38981): every block the cache
/// can serve equals recomputing its token prefix without the cache, with an
/// unbounded cache and with one small enough to evict.
#[test]
fn generated_prefixes_are_reused_and_every_cached_block_equals_recomputation() {
    let model = model_with(3, 128, 131);
    let dense = DenseModel::new(&model, [131; 32]);
    let first = Request {
        id: 0,
        prompt: (0..23u32).map(|i| (i * 7) % VOCAB).collect(),
        max_tokens: 12,
        eos: Vec::new(),
        selection: Selection::Argmax,
    };
    for capacity_bytes in [usize::MAX, 6_000] {
        let settings = SchedulerConfig {
            prefix: Some(PrefixConfig {
                block: 4,
                capacity_bytes,
            }),
            ..config(1)
        };
        let mut scheduler = Scheduler::new(&dense, settings);
        scheduler.submit(first.clone()).unwrap();
        let turn1 = scheduler.run().pop().unwrap();
        let out1 = turn1.result.unwrap().tokens;
        let mut prompt2 = first.prompt.clone();
        prompt2.extend_from_slice(&out1);
        prompt2.extend_from_slice(&[3, 1, 4, 1, 5]);
        let second = Request {
            id: 1,
            prompt: prompt2,
            max_tokens: 10,
            eos: Vec::new(),
            selection: Selection::Rp64Argmax,
        };
        scheduler.submit(second.clone()).unwrap();
        let turn2 = scheduler.run().pop().unwrap();
        assert_matches_reference(&model, &second, &turn2, false);
        let out2 = turn2.result.as_ref().unwrap();
        if capacity_bytes == usize::MAX {
            // Everything turn 1 computed (its prompt and every generated token
            // but the last) is reusable, in whole 4-position blocks.
            let reusable = first.prompt.len() + out1.len() - 1;
            assert_eq!(out2.cached_tokens, reusable / 4 * 4);
        }
        let cache = scheduler.prefix_cache_mut().unwrap();
        for (prompt, tokens) in [(&first.prompt, &out1), (&second.prompt, &out2.tokens)] {
            let sequence: Vec<u32> = prompt
                .iter()
                .chain(&tokens[..tokens.len() - 1])
                .copied()
                .collect();
            let mut served = dense.new_kv();
            let hit = cache.lookup_into(&sequence, sequence.len(), &mut served);
            if capacity_bytes == usize::MAX {
                assert_eq!(hit, sequence.len() / 4 * 4);
            }
            let mut recomputed = model.new_cache();
            for &t in &sequence[..hit] {
                model.forward(t, &mut recomputed).unwrap();
            }
            assert_eq!(served.len(), hit);
            assert_eq!(served.digest(), recomputed.digest());
        }
    }
}

/// A request that loses its cache mid-generation (preemption, a device
/// leaving an island) is rebuilt by prefilling its prompt and the tokens it
/// already generated. The rebuilt cache, the next logits and the next token
/// are the uninterrupted ones, whatever the chunking.
#[test]
fn recomputing_a_preempted_request_is_byte_identical() {
    let model = model_with(3, 96, 141);
    let dense = DenseModel::new(&model, [141; 32]);
    let request = Request {
        id: 0,
        prompt: (0..29u32).map(|i| (i * 13 + 5) % VOCAB).collect(),
        max_tokens: 20,
        eos: Vec::new(),
        selection: Selection::Rp64Argmax,
    };
    let (tokens, hashes) = reference(&model, &request);
    let p = request.prompt.len();
    for generated in [1usize, 7, tokens.len() - 1] {
        let sequence: Vec<u32> = request
            .prompt
            .iter()
            .chain(&tokens[..generated])
            .copied()
            .collect();
        for chunk in [sequence.len(), 5, 1] {
            let mut kv = dense.new_kv();
            let mut last = None;
            for start in (0..sequence.len()).step_by(chunk) {
                let end = (start + chunk).min(sequence.len());
                let rows: Vec<Row> = (start..end)
                    .map(|q| Row {
                        seq: 0,
                        token: sequence[q],
                        position: q,
                        logits: q + 1 == sequence.len(),
                    })
                    .collect();
                let out = dense.forward_rows(&rows, &mut [&mut kv]);
                assert!(out.errors[0].is_none());
                if let Some(Some(values)) = out.logits.last() {
                    last = Some(values.clone());
                }
            }
            let values = last.unwrap();
            assert_eq!(arith::logits_hash(&values), hashes[p + generated - 1]);
            let next = arith::select(&values, &tokens[..generated], request.selection).unwrap();
            assert_eq!(next, tokens[generated]);
            assert_eq!(
                kv.digest(),
                replay_digest(&model, &request.prompt, &tokens[..generated + 1])
            );
        }
    }
}

#[test]
fn speculative_decoding_emits_exactly_the_plain_greedy_tokens() {
    let model = model_with(3, 128, 57);
    let dense = DenseModel::new(&model, [57; 32]);
    let mut requests = random_requests(24, 8);
    for (i, request) in requests.iter_mut().enumerate() {
        if i.is_multiple_of(2) {
            let pattern = request.prompt.clone();
            request.prompt.extend_from_slice(&pattern);
            request
                .prompt
                .extend_from_slice(&pattern[..pattern.len() / 2]);
            request.max_tokens = 16;
        }
    }
    let golden = SchedulerConfig {
        all_logits: true,
        ..config(4)
    };
    let plain = serve(&dense, golden, &requests);
    let mut drafted = 0usize;
    for draft_tokens in [1usize, 3, 8] {
        for max_running in [1usize, 16] {
            let settings = SchedulerConfig {
                draft_tokens,
                all_logits: true,
                ..config(max_running)
            };
            let completions = serve(&dense, settings, &requests);
            for request in &requests {
                let spec = completions[&request.id].result.as_ref().unwrap();
                let base = plain[&request.id].result.as_ref().unwrap();
                assert_eq!(spec.tokens, base.tokens, "k {draft_tokens}");
                assert_eq!(spec.logits_hashes, base.logits_hashes, "k {draft_tokens}");
                assert_eq!(spec.kv_digest, base.kv_digest, "k {draft_tokens}");
                assert_matches_reference(&model, request, &completions[&request.id], true);
                drafted += spec.drafted;
            }
        }
    }
    assert!(drafted > 0, "prompt lookup never proposed a draft");
}

/// Proposes the true continuation, corrupting every `every`-th proposed
/// position, so verification sees full accepts, partial accepts and
/// immediate rejections.
struct Oracle {
    truths: Vec<Vec<u32>>,
    every: usize,
}

impl Drafter for Oracle {
    fn propose(&self, context: &[u32], max: usize) -> Vec<u32> {
        let Some(truth) = self
            .truths
            .iter()
            .find(|t| t.len() > context.len() && t.starts_with(context))
        else {
            return Vec::new();
        };
        truth[context.len()..]
            .iter()
            .take(max)
            .enumerate()
            .map(|(j, &t)| {
                if (context.len() + j).is_multiple_of(self.every) {
                    (t + 1) % VOCAB
                } else {
                    t
                }
            })
            .collect()
    }
}

/// Proposes ids outside the vocabulary and arbitrary context tokens.
struct Hostile;

impl Drafter for Hostile {
    fn propose(&self, context: &[u32], max: usize) -> Vec<u32> {
        (0..max)
            .map(|j| match (context.len() + j) % 3 {
                0 => u32::MAX,
                1 => VOCAB,
                _ => context[context.len() - 1 - j % context.len()],
            })
            .collect()
    }
}

#[test]
fn verification_is_exact_under_honest_corrupted_and_hostile_drafts() {
    let model = model_with(2, 96, 71);
    let dense = DenseModel::new(&model, [71; 32]);
    let requests = random_requests(16, 13);
    let truths: Vec<Vec<u32>> = requests
        .iter()
        .map(|request| {
            let mut truth = request.prompt.clone();
            truth.extend(reference(&model, request).0);
            truth
        })
        .collect();
    let drafters: [Box<dyn Drafter>; 5] = [
        Box::new(Oracle {
            truths: truths.clone(),
            every: usize::MAX,
        }),
        Box::new(Oracle {
            truths: truths.clone(),
            every: 3,
        }),
        Box::new(Oracle { truths, every: 1 }),
        Box::new(Hostile),
        Box::new(PromptLookup::default()),
    ];
    let mut accepted = 0usize;
    for drafter in drafters {
        let settings = SchedulerConfig {
            draft_tokens: 5,
            all_logits: true,
            ..config(8)
        };
        let mut scheduler = Scheduler::new(&dense, settings).with_drafter(drafter);
        for request in &requests {
            scheduler.submit(request.clone()).unwrap();
        }
        for completion in scheduler.run() {
            let request = &requests[completion.id as usize];
            assert_matches_reference(&model, request, &completion, true);
            accepted += completion.result.as_ref().unwrap().accepted;
        }
    }
    assert!(accepted > 0, "no draft was ever accepted");
}

#[test]
fn the_accept_rule_stops_where_plain_decoding_would() {
    let rows = |winners: &[usize]| -> Vec<Vec<i64>> {
        winners
            .iter()
            .map(|&w| {
                let mut logits = vec![0i64; 8];
                logits[w] = 100;
                logits
            })
            .collect()
    };
    let greedy = Selection::Argmax;
    // Every draft right: all rows used, plus the bonus token.
    let all = accept(&rows(&[3, 4, 5]), &[3, 4], &[], greedy, &[], 10).unwrap();
    assert_eq!(
        (all.emitted, all.rows_used, all.finished),
        (vec![3, 4, 5], 3, false)
    );
    // First draft wrong: one token, one row.
    let miss = accept(&rows(&[3, 4, 5]), &[6, 4], &[], greedy, &[], 10).unwrap();
    assert_eq!((miss.emitted, miss.rows_used), (vec![3], 1));
    // EOS in the middle ends the step even though the drafts were right.
    let eos = accept(&rows(&[3, 4, 5]), &[3, 4], &[], greedy, &[4], 10).unwrap();
    assert_eq!(
        (eos.emitted, eos.rows_used, eos.finished),
        (vec![3, 4], 2, true)
    );
    // max_tokens counts the tokens generated before the step.
    let limit = accept(&rows(&[3, 4, 5]), &[3, 4], &[1, 1], greedy, &[], 3).unwrap();
    assert_eq!((limit.emitted, limit.finished), (vec![3], true));
    assert!(accept(&[], &[], &[], greedy, &[], 3).is_err());
}

#[test]
fn prompt_lookup_continues_the_latest_match() {
    let drafter = PromptLookup::default();
    assert_eq!(drafter.propose(&[1, 2, 3, 4, 1, 2], 3), vec![3, 4, 1]);
    assert_eq!(drafter.propose(&[1, 2, 3, 4, 1, 2], 1), vec![3]);
    // The longer suffix "8 9" wins over the more recent "9".
    assert_eq!(drafter.propose(&[8, 9, 5, 9, 6, 8, 9], 2), vec![5, 9]);
    assert!(drafter.propose(&[1, 2, 3], 4).is_empty());
    assert!(drafter.propose(&[7, 7], 0).is_empty());
}

#[test]
fn a_failing_sequence_does_not_disturb_the_batch() {
    let model = model_with(2, 24, 81);
    let dense = DenseModel::new(&model, [81; 32]);
    let mut kvs: Vec<SeqKv> = (0..3).map(|_| dense.new_kv()).collect();
    let filler: Vec<Row> = (0..22usize)
        .map(|p| Row {
            seq: 0,
            token: p as u32,
            position: p,
            logits: false,
        })
        .collect();
    assert!(
        dense.forward_rows(&filler, &mut [&mut kvs[1]]).errors[0].is_none(),
        "filler"
    );
    let row = |seq: usize, token: u32, position: usize| Row {
        seq,
        token,
        position,
        logits: true,
    };
    // Sequence 1 runs past its 24-position context; 0 and 2 are ordinary.
    let rows = [
        row(0, 3, 0),
        row(0, 4, 1),
        row(0, 5, 2),
        row(1, 9, 22),
        row(1, 9, 23),
        row(1, 9, 24),
        row(2, 7, 0),
        row(2, 8, 1),
    ];
    let out = {
        let mut refs: Vec<&mut SeqKv> = kvs.iter_mut().collect();
        dense.forward_rows(&rows, &mut refs)
    };
    assert!(out.errors[0].is_none() && out.errors[2].is_none());
    // Sequence 1 fails at its third row, position 24. The two rows before it
    // are kept, exactly as plain decoding computes them; the failing row is
    // dropped.
    assert!(matches!(
        &out.errors[1],
        Some(Failure {
            kept: 2,
            error: ModernError::Domain(_)
        })
    ));
    assert_eq!(kvs[1].len(), 24, "the rows before the failure stay");
    assert_eq!((kvs[0].len(), kvs[2].len()), (3, 2));
    let survivors: [(usize, &[u32], usize); 2] = [(0, &[3, 4, 5], 0), (2, &[7, 8], 6)];
    for (seq, tokens, first) in survivors {
        let mut cache = model.new_cache();
        for (offset, &t) in tokens.iter().enumerate() {
            let alone = model.forward(t, &mut cache).unwrap();
            assert_eq!(
                out.logits[first + offset].as_deref(),
                Some(alone.as_slice())
            );
        }
        assert_eq!(kvs[seq].digest(), cache.digest());
    }
    let mut cache = model.new_cache();
    for p in 0..22u32 {
        model.forward(p, &mut cache).unwrap();
    }
    for (offset, &t) in [9u32, 9].iter().enumerate() {
        let alone = model.forward(t, &mut cache).unwrap();
        assert_eq!(out.logits[3 + offset].as_deref(), Some(alone.as_slice()));
    }
    assert_eq!(kvs[1].digest(), cache.digest());
    assert!(out.logits[5].is_none());
}

/// Fails every sequence that feeds `poison`: a stand-in for a domain error
/// deep inside one request.
struct Poisoned<'a> {
    inner: DenseModel<'a>,
    poison: u32,
}

impl BatchModel for Poisoned<'_> {
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

    fn forward_rows(&self, rows: &[Row], kvs: &mut [&mut SeqKv]) -> StepOutput {
        let base: Vec<usize> = kvs.iter().map(|kv| kv.len()).collect();
        let mut out = self.inner.forward_rows(rows, kvs);
        for row in rows {
            if row.token == self.poison && out.errors[row.seq].is_none() {
                out.errors[row.seq] = Some(Failure {
                    kept: 0,
                    error: ModernError::Domain("poisoned".into()),
                });
                kvs[row.seq].rollback(base[row.seq]);
            }
        }
        for (row, slot) in rows.iter().zip(out.logits.iter_mut()) {
            if out.errors[row.seq].is_some() {
                *slot = None;
            }
        }
        out
    }
}

#[test]
fn a_failing_request_leaves_every_other_request_untouched() {
    let model = model_with(2, 64, 91);
    let mut requests = random_requests(20, 17);
    let outputs: Vec<Vec<u32>> = requests.iter().map(|r| reference(&model, r).0).collect();
    // A poison token that no request generates.
    let poison = (0..VOCAB)
        .rev()
        .find(|t| outputs.iter().all(|tokens| !tokens.contains(t)))
        .unwrap();
    for (i, request) in requests.iter_mut().enumerate() {
        request.prompt.retain(|&t| t != poison);
        if request.prompt.is_empty() {
            request.prompt.push((poison + 1) % VOCAB);
        }
        if i.is_multiple_of(4) {
            let middle = request.prompt.len() / 2;
            request.prompt.insert(middle, poison);
        }
    }
    let poisoned = Poisoned {
        inner: DenseModel::new(&model, [91; 32]),
        poison,
    };
    for max_running in [1usize, 20] {
        let completions = serve(&poisoned, config(max_running), &requests);
        for (i, request) in requests.iter().enumerate() {
            let completion = &completions[&request.id];
            if i.is_multiple_of(4) {
                assert!(completion.result.is_err(), "request {i} should fail");
            } else if !reference(&model, request).0.contains(&poison) {
                assert_matches_reference(&model, request, completion, false);
            }
        }
    }
}

#[test]
fn serving_reproduces_generate_on_the_tiny_model() {
    let model = tiny_model();
    let dense = DenseModel::new(&model, [0; 32]);
    let prompts: [&[u32]; 3] = [&[1, 2, 3], &[39, 0, 17, 5], &[8]];
    let requests: Vec<Request> = prompts
        .iter()
        .zip(0u64..)
        .map(|(prompt, id)| Request {
            id,
            prompt: prompt.to_vec(),
            max_tokens: 6,
            eos: Vec::new(),
            selection: Selection::Rp64Argmax,
        })
        .collect();
    let settings = SchedulerConfig {
        all_logits: true,
        draft_tokens: 3,
        ..config(3)
    };
    let completions = serve(&dense, settings, &requests);
    for request in &requests {
        assert_matches_reference(&model, request, &completions[&request.id], true);
    }
}

/// One expert of the mixture-of-experts fixture: a SwiGLU block.
struct Expert {
    gate: DyadicMatrix,
    up: DyadicMatrix,
    down: DyadicMatrix,
}

/// A routed mixture-of-experts feed-forward block: top-k experts per row by
/// integer router score (lowest index on ties), Q16 softmax weights over the
/// chosen experts, and one batched projection per expert over just the rows
/// routed to it. The grouping depends on the batch; no value does.
struct MoeFfn {
    routers: Vec<DyadicMatrix>,
    experts: Vec<Vec<Expert>>,
    top_k: usize,
}

impl MoeFfn {
    fn new(seed: &mut u64, layers: usize, experts: usize, top_k: usize) -> Self {
        Self {
            routers: (0..layers).map(|_| lcg_matrix(seed, experts, 32)).collect(),
            experts: (0..layers)
                .map(|_| {
                    (0..experts)
                        .map(|_| Expert {
                            gate: lcg_matrix(seed, 24, 32),
                            up: lcg_matrix(seed, 24, 32),
                            down: lcg_matrix(seed, 32, 24),
                        })
                        .collect()
                })
                .collect(),
            top_k,
        }
    }

    /// The chosen experts and their Q16 weights, for one row's scores.
    fn route(&self, scores: &[i64]) -> Vec<(usize, i64)> {
        let mut order: Vec<usize> = (0..scores.len()).collect();
        order.sort_by_key(|&e| (std::cmp::Reverse(scores[e]), e));
        order.truncate(self.top_k);
        let best = scores[order[0]];
        let weights: Vec<i64> = order.iter().map(|&e| exp_q16(scores[e] - best)).collect();
        let total: i64 = weights.iter().sum();
        order
            .into_iter()
            .zip(weights)
            .map(|(e, w)| (e, (w << 16) / total))
            .collect()
    }
}

impl FeedForward for MoeFfn {
    fn forward(
        &self,
        layer: usize,
        normed: &[i64],
        out: &mut [i64],
        errors: &mut [Option<ModernError>],
    ) {
        let d = 32;
        let experts = &self.experts[layer];
        let n = errors.len();
        let mut scores = vec![0i64; n * experts.len()];
        project_rows(&self.routers[layer], normed, &mut scores, errors);
        let routes: Vec<Vec<(usize, i64)>> = scores
            .chunks(experts.len())
            .zip(errors.iter())
            .map(|(row, error)| {
                if error.is_some() {
                    Vec::new()
                } else {
                    self.route(row)
                }
            })
            .collect();
        out.fill(0);
        for (e, expert) in experts.iter().enumerate() {
            let members: Vec<(usize, i64)> = routes
                .iter()
                .enumerate()
                .filter_map(|(r, route)| {
                    route
                        .iter()
                        .find(|&&(chosen, _)| chosen == e)
                        .map(|&(_, w)| (r, w))
                })
                .collect();
            if members.is_empty() {
                continue;
            }
            let f = expert.gate.rows;
            let mut xs = vec![0i64; members.len() * d];
            for (slot, &(r, _)) in xs.chunks_mut(d).zip(&members) {
                slot.copy_from_slice(&normed[r * d..(r + 1) * d]);
            }
            let mut member_errors: Vec<Option<ModernError>> =
                members.iter().map(|_| None).collect();
            let mut gate = vec![0i64; members.len() * f];
            let mut up = vec![0i64; members.len() * f];
            let mut y = vec![0i64; members.len() * d];
            project_rows(&expert.gate, &xs, &mut gate, &mut member_errors);
            project_rows(&expert.up, &xs, &mut up, &mut member_errors);
            silu_rows(&mut gate, &up, f, &mut member_errors);
            project_rows(&expert.down, &gate, &mut y, &mut member_errors);
            for ((&(r, w), values), error) in members.iter().zip(y.chunks(d)).zip(member_errors) {
                if let Some(error) = error {
                    if errors[r].is_none() {
                        errors[r] = Some(error);
                    }
                    continue;
                }
                for (o, &v) in out[r * d..(r + 1) * d].iter_mut().zip(values) {
                    *o += ((i128::from(v) * i128::from(w)) >> 16) as i64;
                }
            }
        }
    }
}

/// The fixture model: the dense attention path with routed experts.
struct MoeModel<'a> {
    base: &'a ModernModel,
    ffn: MoeFfn,
}

impl BatchModel for MoeModel<'_> {
    fn vocab_size(&self) -> usize {
        self.base.config.vocab_size
    }

    fn max_positions(&self) -> usize {
        self.base.config.max_seq
    }

    fn kv_widths(&self) -> Vec<usize> {
        dense_kv_widths(self.base)
    }

    fn identity(&self) -> [u8; 32] {
        [0xe0; 32]
    }

    fn forward_rows(&self, rows: &[Row], kvs: &mut [&mut SeqKv]) -> StepOutput {
        layered_forward(self.base, rows, kvs, &self.ffn)
    }
}

#[test]
fn mixture_of_experts_routing_is_batch_invariant() {
    let base = model_with(3, 96, 101);
    let mut seed = 202u64;
    let moe = MoeModel {
        base: &base,
        ffn: MoeFfn::new(&mut seed, 3, 6, 2),
    };
    let requests = random_requests(24, 31);
    // Unbatched reference: one request at a time, one token per step.
    let alone = SchedulerConfig {
        step_tokens: 1,
        all_logits: true,
        ..config(1)
    };
    let reference = serve(&moe, alone, &requests);
    let batched = [
        SchedulerConfig {
            all_logits: true,
            ..config(24)
        },
        SchedulerConfig {
            step_tokens: 9,
            prefill_chunk: 4,
            all_logits: true,
            draft_tokens: 4,
            ..config(8)
        },
    ];
    for settings in batched {
        let completions = serve(&moe, settings, &requests);
        for request in &requests {
            let got = completions[&request.id].result.as_ref().unwrap();
            let want = reference[&request.id].result.as_ref().unwrap();
            assert_eq!(got.tokens, want.tokens, "request {}", request.id);
            assert_eq!(got.logits_hashes, want.logits_hashes);
            assert_eq!(got.kv_digest, want.kv_digest);
        }
    }
    let cached = SchedulerConfig {
        prefix: Some(PrefixConfig {
            block: 4,
            capacity_bytes: usize::MAX,
        }),
        ..config(1)
    };
    let completions = serve(&moe, cached, &requests);
    for request in &requests {
        let got = completions[&request.id].result.as_ref().unwrap();
        let want = reference[&request.id].result.as_ref().unwrap();
        assert_eq!(got.tokens, want.tokens);
        assert_eq!(got.kv_digest, want.kv_digest);
    }
}

/// The dense model cut into two stages, as on two devices of an island: each
/// stage owns the KV planes of its layers, and only hidden states cross the
/// cut. Rows go through in two micro-batches.
struct Pipeline<'a> {
    model: &'a ModernModel,
    cut: usize,
}

impl BatchModel for Pipeline<'_> {
    fn vocab_size(&self) -> usize {
        self.model.config.vocab_size
    }

    fn max_positions(&self) -> usize {
        self.model.config.max_seq
    }

    fn kv_widths(&self) -> Vec<usize> {
        dense_kv_widths(self.model)
    }

    fn identity(&self) -> [u8; 32] {
        [0xb0; 32]
    }

    fn forward_rows(&self, rows: &[Row], kvs: &mut [&mut SeqKv]) -> StepOutput {
        let base = match check_rows(rows, kvs) {
            Ok(base) => base,
            Err(e) => return failed_step(rows.len(), kvs.len(), &e),
        };
        let d = self.model.config.d_model;
        let layers = self.model.config.n_layers;
        let ffn = DenseFfn(self.model);
        let mut errors: Vec<Option<ModernError>> = rows.iter().map(|_| None).collect();
        let mut hidden = embed_rows(self.model, rows, &mut errors);
        // Micro-batches split at a sequence boundary near the middle.
        let split = (rows.len() / 2..rows.len())
            .find(|&i| i > 0 && rows[i].seq != rows[i - 1].seq)
            .unwrap_or(rows.len());
        for range in [0..split, split..rows.len()] {
            if range.is_empty() {
                continue;
            }
            let micro_rows = &rows[range.clone()];
            let micro_errors = &mut errors[range.clone()];
            let stage_one = &mut hidden[range.start * d..range.end * d];
            forward_layers(
                self.model,
                0..self.cut,
                micro_rows,
                kvs,
                stage_one,
                micro_errors,
                &ffn,
            );
            // The wire between the two devices: hidden states only.
            let mut stage_two = stage_one.to_vec();
            forward_layers(
                self.model,
                self.cut..layers,
                micro_rows,
                kvs,
                &mut stage_two,
                micro_errors,
                &ffn,
            );
            stage_one.copy_from_slice(&stage_two);
        }
        let logits = final_logits(self.model, rows, &hidden, &mut errors);
        finish_step(rows, kvs, &base, errors, logits)
    }
}

#[test]
fn a_two_stage_pipeline_matches_the_whole_model() {
    let model = model_with(4, 96, 111);
    let dense = DenseModel::new(&model, [111; 32]);
    let pipeline = Pipeline {
        model: &model,
        cut: 2,
    };
    let requests = random_requests(16, 41);
    let settings = SchedulerConfig {
        step_tokens: 64,
        prefill_chunk: 7,
        all_logits: true,
        draft_tokens: 3,
        ..config(16)
    };
    let whole = serve(&dense, settings, &requests);
    let split = serve(&pipeline, settings, &requests);
    for request in &requests {
        assert_eq!(
            whole[&request.id].result, split[&request.id].result,
            "request {}",
            request.id
        );
        assert_matches_reference(&model, request, &split[&request.id], true);
    }
}

#[test]
fn refusals_match_generate_and_stay_with_their_request() {
    let model = model_with(1, 16, 5);
    let dense = DenseModel::new(&model, [5; 32]);
    let mut scheduler = Scheduler::new(&dense, config(4));
    let refused = [
        Request {
            id: 1,
            prompt: Vec::new(),
            max_tokens: 2,
            eos: Vec::new(),
            selection: Selection::Argmax,
        },
        Request {
            id: 2,
            prompt: vec![1; 12],
            max_tokens: 5,
            eos: Vec::new(),
            selection: Selection::Argmax,
        },
        Request {
            id: 3,
            prompt: vec![VOCAB],
            max_tokens: 1,
            eos: Vec::new(),
            selection: Selection::Argmax,
        },
    ];
    for request in refused {
        let generate = model.generate(&GenerationRequest {
            prompt: &request.prompt,
            max_tokens: request.max_tokens,
            eos: &request.eos,
            selection: request.selection,
        });
        let submit = scheduler.submit(request);
        assert_eq!(
            generate.map(|_| ()).map_err(|e| e.to_string()),
            submit.map_err(|e| e.to_string())
        );
    }
    let fine = Request {
        id: 4,
        prompt: vec![1, 2],
        max_tokens: 3,
        eos: Vec::new(),
        selection: Selection::Argmax,
    };
    scheduler.submit(fine.clone()).unwrap();
    let completions = scheduler.run();
    assert_eq!(completions.len(), 1);
    assert_matches_reference(&model, &fine, &completions[0], false);
}

/// A dyadic matrix with one `mu` and one `k` for every row.
fn plane_matrix(rows: usize, cols: usize, q: Vec<i8>, mu: i32, k: u8) -> DyadicMatrix {
    DyadicMatrix {
        rows,
        cols,
        q,
        mu: vec![mu; rows],
        k: vec![k; rows],
    }
}

fn zero_matrix(rows: usize, cols: usize) -> DyadicMatrix {
    plane_matrix(rows, cols, vec![0; rows * cols], 0, 16)
}

/// The two-token, two-dimensional model of the ARC-54 review (appendix A).
/// `embed` holds the rows of token 0 and token 1, which are also the LM head;
/// every other weight is zero, so a token's hidden state is its embedding.
/// With `value_overflow`, the value projection reads axis 1 only, scaled so
/// that a token with a non-zero axis-1 embedding leaves the KV `i32` domain
/// while a token on axis 0 alone never does.
fn two_token_model(embed: [i8; 4], final_norm: [i64; 2], value_overflow: bool) -> ModernModel {
    let model = ModernModel {
        config: ModernConfig {
            architecture: "llama".into(),
            n_layers: 1,
            d_model: 2,
            n_heads: 1,
            n_kv_heads: 1,
            d_head: 2,
            d_ff: 2,
            vocab_size: 2,
            max_seq: 16,
            rms_eps_q32: 4295,
            rope_theta: 10_000,
            rope_layers: vec![false],
        },
        embed: plane_matrix(2, 2, embed.to_vec(), 1 << 30, 30),
        final_norm: final_norm.to_vec(),
        rope_cos: vec![ONE as i32; 16],
        rope_sin: vec![0; 16],
        layers: vec![ModernLayer {
            attn_norm: vec![ONE; 2],
            wq: zero_matrix(2, 2),
            wk: zero_matrix(2, 2),
            wv: if value_overflow {
                plane_matrix(2, 2, vec![0, 127, 0, 127], 1 << 30, 16)
            } else {
                zero_matrix(2, 2)
            },
            wo: zero_matrix(2, 2),
            ffn_norm: vec![ONE; 2],
            w_gate: zero_matrix(2, 2),
            w_up: zero_matrix(2, 2),
            w_down: zero_matrix(2, 2),
        }],
    };
    model.validate().unwrap();
    model
}

/// Proposes a fixed pattern, whatever the context.
#[derive(Clone)]
struct Pattern(Vec<u32>);

impl Drafter for Pattern {
    fn propose(&self, _: &[u32], max: usize) -> Vec<u32> {
        self.0.iter().copied().take(max).collect()
    }
}

/// The error `generate` returns for `request`, as text.
fn reference_error(model: &ModernModel, request: &Request) -> String {
    model
        .generate(&GenerationRequest {
            prompt: &request.prompt,
            max_tokens: request.max_tokens,
            eos: &request.eos,
            selection: request.selection,
        })
        .expect_err("the reference refuses")
        .to_string()
}

fn assert_fails_like_reference(model: &ModernModel, request: &Request, completion: &Completion) {
    let expected = reference_error(model, request);
    assert_eq!(
        completion.result.as_ref().err().map(String::as_str),
        Some(expected.as_str()),
        "request {}",
        request.id
    );
}

fn serve_one<M: BatchModel>(
    model: &M,
    settings: SchedulerConfig,
    drafter: Pattern,
    request: &Request,
) -> Completion {
    let mut scheduler = Scheduler::new(model, settings).with_drafter(Box::new(drafter));
    scheduler.submit(request.clone()).unwrap();
    let mut completions = scheduler.run();
    assert_eq!(completions.len(), 1);
    completions.pop().unwrap()
}

/// ARC-54 finding 1: a drafted token in the vocabulary whose forward pass
/// leaves the profile's domain used to fail the whole request, although plain
/// decoding rejects that token and never computes it. The review's
/// counterexample: a drafter that always proposes the bad token, at every
/// draft bound, in default and golden mode, with the prefix cache off and on.
#[test]
fn a_rejected_draft_outside_the_domain_never_fails_the_request() {
    let model = two_token_model([1, 0, 0, 1], [ONE; 2], true);
    assert!(matches!(
        model.forward(1, &mut model.new_cache()),
        Err(ModernError::Domain(_))
    ));
    let dense = DenseModel::new(&model, [0xa5; 32]);
    let request = Request {
        id: 1,
        prompt: vec![0, 0],
        max_tokens: 4,
        eos: Vec::new(),
        selection: Selection::Argmax,
    };
    assert_eq!(reference(&model, &request).0, vec![0, 0, 0, 0]);
    let caches = [
        None,
        Some(PrefixConfig {
            block: 1,
            capacity_bytes: usize::MAX,
        }),
    ];
    for draft_tokens in [0usize, 1, 2, 4] {
        for all_logits in [false, true] {
            for prefix in caches {
                let settings = SchedulerConfig {
                    draft_tokens,
                    all_logits,
                    prefix,
                    ..config(1)
                };
                let mut scheduler =
                    Scheduler::new(&dense, settings).with_drafter(Box::new(Pattern(vec![1; 8])));
                // Twice: the second run reuses the first's prompt block when
                // the cache is on.
                for round in 0..2 {
                    scheduler.submit(request.clone()).unwrap();
                    let completion = scheduler.run().pop().unwrap();
                    assert_matches_reference(&model, &request, &completion, all_logits);
                    let generated = completion.result.as_ref().unwrap();
                    assert_eq!(generated.accepted, 0, "the bad token is never accepted");
                    assert_eq!(
                        generated.drafted > 0,
                        draft_tokens > 0,
                        "bound {draft_tokens}"
                    );
                    if prefix.is_some() && !all_logits && round == 1 {
                        assert_eq!(generated.cached_tokens, 1, "the second run reuses a block");
                    }
                }
            }
        }
    }
}

/// A draft that fails after several accepted drafts is dropped together with
/// the rows after it; the accepted rows stand and decoding continues.
#[test]
fn a_failing_draft_after_accepted_drafts_is_ignored() {
    let model = two_token_model([1, 0, 0, 1], [ONE; 2], true);
    let dense = DenseModel::new(&model, [0xa6; 32]);
    let request = Request {
        id: 2,
        prompt: vec![0],
        max_tokens: 8,
        eos: Vec::new(),
        selection: Selection::Argmax,
    };
    assert_eq!(reference(&model, &request).0, vec![0; 8]);
    for all_logits in [false, true] {
        let settings = SchedulerConfig {
            draft_tokens: 8,
            all_logits,
            ..config(1)
        };
        let drafter = Pattern(vec![0, 0, 1, 1, 1, 1, 1, 1]);
        let completion = serve_one(&dense, settings, drafter, &request);
        assert_matches_reference(&model, &request, &completion, all_logits);
        let generated = completion.result.as_ref().unwrap();
        assert!(generated.accepted >= 4, "accepted {}", generated.accepted);
        assert!(
            generated.drafted > generated.accepted,
            "a draft failed: drafted {} accepted {}",
            generated.drafted,
            generated.accepted
        );
    }
}

/// When plain decoding itself would feed the failing token next, the request
/// fails with plain decoding's error. A decision taken before that row (EOS,
/// the output length) ends the request normally, failing row or not.
#[test]
fn a_draft_plain_decoding_would_feed_fails_exactly_like_plain_decoding() {
    // Token 0 predicts itself once; then the repetition penalty hands the
    // argmax to token 1, whose value projection leaves the KV domain.
    let model = two_token_model([21, 0, 20, 1], [ONE; 2], true);
    let dense = DenseModel::new(&model, [0xa7; 32]);
    let doomed = Request {
        id: 3,
        prompt: vec![0],
        max_tokens: 4,
        eos: Vec::new(),
        selection: Selection::Rp64Argmax,
    };
    let expected = reference_error(&model, &doomed);
    assert!(expected.contains("KV value outside i32"), "{expected}");
    for draft_tokens in [0usize, 2] {
        for all_logits in [false, true] {
            let settings = SchedulerConfig {
                draft_tokens,
                all_logits,
                ..config(1)
            };
            let completion = serve_one(&dense, settings, Pattern(vec![1; 8]), &doomed);
            assert_fails_like_reference(&model, &doomed, &completion);
        }
    }
    let saved = [
        Request {
            id: 4,
            prompt: vec![0],
            max_tokens: 4,
            eos: vec![1],
            selection: Selection::Rp64Argmax,
        },
        Request {
            id: 5,
            prompt: vec![0],
            max_tokens: 2,
            eos: Vec::new(),
            selection: Selection::Rp64Argmax,
        },
    ];
    for request in &saved {
        assert_eq!(reference(&model, request).0, vec![0, 1]);
        for draft_tokens in [0usize, 2] {
            let settings = SchedulerConfig {
                draft_tokens,
                ..config(1)
            };
            let completion = serve_one(&dense, settings, Pattern(vec![1; 8]), request);
            assert_matches_reference(&model, request, &completion, false);
        }
    }
}

/// A prompt token outside the domain is refused with `generate`'s error, in
/// whichever prefill chunk it falls and whether or not a prefix is reused,
/// and the other requests of the batch are untouched.
#[test]
fn a_prompt_token_outside_the_domain_is_refused_like_generate() {
    let model = two_token_model([1, 0, 0, 1], [ONE; 2], true);
    let dense = DenseModel::new(&model, [0xa8; 32]);
    let refused: Vec<Request> = [vec![1u32], vec![0, 1, 0], vec![0, 0, 0, 1]]
        .into_iter()
        .enumerate()
        .map(|(i, prompt)| Request {
            id: 10 + i as u64,
            prompt,
            max_tokens: 3,
            eos: Vec::new(),
            selection: Selection::Argmax,
        })
        .collect();
    let fine = Request {
        id: 20,
        prompt: vec![0, 0, 0],
        max_tokens: 3,
        eos: Vec::new(),
        selection: Selection::Argmax,
    };
    let caches = [
        None,
        Some(PrefixConfig {
            block: 1,
            capacity_bytes: usize::MAX,
        }),
    ];
    for prefill_chunk in [1usize, 2, 64] {
        for all_logits in [false, true] {
            for prefix in caches {
                let settings = SchedulerConfig {
                    prefill_chunk,
                    all_logits,
                    prefix,
                    ..config(4)
                };
                let mut scheduler = Scheduler::new(&dense, settings);
                // The fine request first, so its blocks are cached when the
                // refused ones run again.
                for round in 0..2 {
                    scheduler.submit(fine.clone()).unwrap();
                    for request in &refused {
                        scheduler.submit(request.clone()).unwrap();
                    }
                    let completions: HashMap<u64, Completion> =
                        scheduler.run().into_iter().map(|c| (c.id, c)).collect();
                    for request in &refused {
                        assert_fails_like_reference(&model, request, &completions[&request.id]);
                    }
                    assert_matches_reference(&model, &fine, &completions[&fine.id], all_logits);
                    if round == 1 && prefix.is_some() && !all_logits {
                        let reused = completions[&refused[2].id].result.is_err();
                        assert!(reused, "the refused request still fails after a prefix hit");
                    }
                }
            }
        }
    }
}

/// ARC-54 finding 2: `generate` runs the LM head at every prompt position, so
/// a prompt position whose logits choose no token can still refuse. Serving
/// runs the same head checks on those rows and refuses identically, in the
/// default mode as in golden mode, with the prefix cache off and on.
#[test]
fn prompt_rows_without_logits_run_the_same_head_checks_as_generate() {
    // A huge final-norm gain on axis 1: the LM head's input check fails at
    // token 1's position, whose logits nobody reads, and nowhere else.
    let model = two_token_model([1, 0, 0, 1], [ONE, 1 << 60], false);
    let dense = DenseModel::new(&model, [0xa9; 32]);
    let refused = Request {
        id: 30,
        prompt: vec![1, 0],
        max_tokens: 1,
        eos: Vec::new(),
        selection: Selection::Argmax,
    };
    let expected = reference_error(&model, &refused);
    assert!(
        expected.contains("projection input magnitude"),
        "{expected}"
    );
    let fine = Request {
        id: 31,
        prompt: vec![0, 0],
        max_tokens: 1,
        eos: Vec::new(),
        selection: Selection::Argmax,
    };
    assert_eq!(reference(&model, &fine).0, vec![0]);
    let caches = [
        None,
        Some(PrefixConfig {
            block: 1,
            capacity_bytes: usize::MAX,
        }),
    ];
    for all_logits in [false, true] {
        for prefix in caches {
            let settings = SchedulerConfig {
                all_logits,
                prefix,
                ..config(2)
            };
            let mut scheduler = Scheduler::new(&dense, settings);
            scheduler.submit(refused.clone()).unwrap();
            scheduler.submit(fine.clone()).unwrap();
            let completions: HashMap<u64, Completion> =
                scheduler.run().into_iter().map(|c| (c.id, c)).collect();
            assert_fails_like_reference(&model, &refused, &completions[&refused.id]);
            assert_matches_reference(&model, &fine, &completions[&fine.id], all_logits);
        }
    }
}

#[test]
fn tree_generation_survives_pipeline_partition_and_microbatch_boundaries() {
    use super::tree::{LookupTree, generate_tree};
    let model = model_with(4, 96, 113);
    let dense = DenseModel::new(&model, [113; 32]);
    let pipeline = Pipeline {
        model: &model,
        cut: 2,
    };
    for req in random_requests(8, 47) {
        let request = GenerationRequest {
            prompt: &req.prompt,
            max_tokens: req.max_tokens,
            eos: &req.eos,
            selection: req.selection,
        };
        let whole = generate_tree(&dense, &request, &LookupTree::default(), 8).unwrap();
        let split = generate_tree(&pipeline, &request, &LookupTree::default(), 8).unwrap();
        let plain = model.generate(&request).unwrap();
        assert_eq!(split.tokens, plain.tokens);
        assert_eq!(split.logits_hashes, plain.logits_hashes);
        assert_eq!(split.tokens, whole.tokens);
        assert_eq!(split.kv_digest, whole.kv_digest);
        assert_eq!(split.verification_passes, whole.verification_passes);
    }
}
