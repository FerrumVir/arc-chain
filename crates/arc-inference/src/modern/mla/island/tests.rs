//! Island proofs on threads joined by in-memory links: every split, every
//! schedule and expert parallelism give the whole model's bytes; restarts
//! replay to the same bytes; tampering is caught. The same runtime over TCP
//! between separate processes is tested in `tests/island_processes.rs`.

use std::sync::Arc;
use std::thread::JoinHandle;

use super::commit::{AuditContext, Verdict, audit_all, audit_stage};
use super::coordinator::{Completion, Coordinator, Request, Schedule};
use super::even_cuts;
use super::expert::{ExpertPlacement, RemoteExperts, serve_experts};
use super::transport::{MemTransport, Transport};
use super::wire::{Frame, Item};
use super::worker::{Fault, StageWorker, WorkerConfig, serve};
use crate::modern::ModernError;
use crate::modern::arith::{self, Selection};
use crate::modern::mla::config::{ExpertFormat, MlaConfig};
use crate::modern::mla::model::{ExpertPool, MlaGeneration, StageModel};
use crate::modern::mla::package::StageSpec;
use crate::modern::mla::synthetic::{self, Router};
use crate::modern::model::GenerationRequest;

fn stage_model(c: &MlaConfig, a: usize, b: usize, router: Router) -> StageModel {
    let stage = StageSpec {
        first_layer: a,
        end_layer: b,
    };
    StageModel::from_owned(synthetic::package_bytes(c, stage, router).unwrap()).unwrap()
}

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

/// Requests with prompts of 1..=6 tokens, budgets of 1..=8 and some EOS ids.
fn requests(c: &MlaConfig, count: usize, seed: u64) -> Vec<Request> {
    let mut s = seed;
    (0..count)
        .map(|i| {
            let len = 1 + (lcg(&mut s) % 6) as usize;
            let prompt = (0..len)
                .map(|_| (lcg(&mut s) % c.vocab_size as u64) as u32)
                .collect();
            let max_tokens = 1 + (lcg(&mut s) % 8) as usize;
            let eos = if i % 3 == 0 {
                vec![(lcg(&mut s) % c.vocab_size as u64) as u32]
            } else {
                Vec::new()
            };
            let selection = if i % 2 == 0 {
                Selection::Rp64Argmax
            } else {
                Selection::Argmax
            };
            Request {
                id: 1000 + i as u64,
                prompt,
                max_tokens,
                eos,
                selection,
            }
        })
        .collect()
}

fn reference(model: &StageModel, r: &Request) -> MlaGeneration {
    model
        .generate(&GenerationRequest {
            prompt: &r.prompt,
            max_tokens: r.max_tokens,
            eos: &r.eos,
            selection: r.selection,
        })
        .unwrap()
}

fn assert_matches(reference: &MlaGeneration, got: &Completion, at: &str) {
    assert_eq!(got.error, None, "{at}");
    assert_eq!(got.tokens, reference.tokens, "{at}: tokens");
    assert_eq!(
        got.logits_hashes, reference.logits_hashes,
        "{at}: logits hashes"
    );
    assert_eq!(got.logits_digest(), reference.logits_digest, "{at}");
    assert_eq!(got.output_hash(), reference.output_hash, "{at}");
    assert!(
        got.ledger.complete(),
        "{at}: ledger {:?}",
        got.ledger.link_faults
    );
    assert_eq!(
        got.ledger.boundary_digests().unwrap(),
        reference.boundary_digests,
        "{at}: per-boundary commitments"
    );
}

/// An island of threads: one per stage, joined by in-memory links.
struct ThreadIsland {
    coordinator: Coordinator,
    workers: Vec<JoinHandle<Result<StageWorker, ModernError>>>,
}

impl ThreadIsland {
    fn start(
        c: &MlaConfig,
        cuts: &[usize],
        router: Router,
        configure: impl Fn(usize, &mut StageModel, &mut WorkerConfig),
    ) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let tag = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mem = MemTransport::new();
        let transport: Arc<dyn Transport> = Arc::new(mem.clone());
        let name = |s: usize| format!("island{tag}-stage{s}");
        let ring_end = format!("island{tag}-coordinator");
        let coordinator_listener = mem.listen(&ring_end).unwrap();
        let stages = cuts.len() - 1;
        let workers = (0..stages)
            .map(|s| {
                let mut model = stage_model(c, cuts[s], cuts[s + 1], router);
                let mut config = WorkerConfig::default();
                configure(s, &mut model, &mut config);
                let worker = StageWorker::new(model, config).unwrap();
                let listener = mem.listen(&name(s)).unwrap();
                let next = if s + 1 == stages {
                    ring_end.clone()
                } else {
                    name(s + 1)
                };
                let transport = transport.clone();
                std::thread::spawn(move || serve(worker, listener, transport, next))
            })
            .collect();
        let coordinator = Coordinator::new(transport, name(0), coordinator_listener, c.clone());
        Self {
            coordinator,
            workers,
        }
    }

    fn stop(mut self) -> Vec<StageWorker> {
        self.coordinator.shutdown().unwrap();
        self.workers
            .into_iter()
            .map(|w| w.join().unwrap().unwrap())
            .collect()
    }
}

const FORMATS: [(bool, ExpertFormat); 3] = [
    (false, ExpertFormat::Int8Dyadic),
    (true, ExpertFormat::Int8Dyadic),
    (true, ExpertFormat::Int4G32),
];

#[test]
fn even_cuts_cover_the_layers() {
    assert_eq!(even_cuts(4, 1), vec![0, 4]);
    assert_eq!(even_cuts(4, 2), vec![0, 2, 4]);
    assert_eq!(even_cuts(61, 4), vec![0, 16, 31, 46, 61]);
    assert_eq!(even_cuts(4, 9), vec![0, 1, 2, 3, 4]);
}

/// The acceptance test in threads: 1, 2, 3 and 4 stages, even and uneven
/// splits, three formats. Tokens, every logits hash and the hash at every
/// layer boundary of every position equal the whole model's generation.
#[test]
fn islands_match_the_whole_model_for_every_split() {
    for (lora, format) in FORMATS {
        let c = synthetic::tiny_config(lora, format);
        let whole = stage_model(&c, 0, c.n_layers, Router::Random);
        let reqs = requests(&c, 6, 7);
        let expected: Vec<MlaGeneration> = reqs.iter().map(|r| reference(&whole, r)).collect();
        // Exhaust all 2^(L-1) contiguous partitions of this model.
        for mask in 0..(1 << (c.n_layers - 1)) {
            let mut cuts = vec![0];
            cuts.extend((1..c.n_layers).filter(|i| mask & (1 << (i - 1)) != 0));
            cuts.push(c.n_layers);
            let mut island = ThreadIsland::start(&c, &cuts, Router::Random, |_, _, _| {});
            let schedule = Schedule {
                micro_batches: 2,
                concurrency: 4,
                ..Schedule::default()
            };
            let (done, stats) = island.coordinator.run(&reqs, &schedule).unwrap();
            for (r, got) in expected.iter().zip(&done) {
                assert_matches(
                    r,
                    got,
                    &format!("lora {lora} {format:?} cuts {cuts:?} id {}", got.id),
                );
                // Every stage committed, and its record root is defined.
                let ranges = got.ledger.stage_ranges();
                assert_eq!(ranges.len(), cuts.len() - 1);
                for (a, b) in ranges {
                    assert!(got.ledger.stage_root(got.id, a, b).is_some());
                }
            }
            assert_eq!(
                stats.generated_tokens,
                expected.iter().map(|g| g.tokens.len() as u64).sum::<u64>()
            );
            island.stop();
        }
    }
}

/// ENG-1's batch-invariance proofs, for islands: a request's bytes do not
/// depend on the concurrency, the number of micro-batches in flight, the
/// prefill chunking, or which other requests share its micro-batch.
#[test]
fn every_request_is_byte_identical_at_any_concurrency_and_micro_batch_depth() {
    let c = synthetic::tiny_config(true, ExpertFormat::Int8Dyadic);
    let whole = stage_model(&c, 0, c.n_layers, Router::Random);
    let reqs = requests(&c, 14, 99);
    let expected: Vec<MlaGeneration> = reqs.iter().map(|r| reference(&whole, r)).collect();
    // Sequence ids are reused across runs, so finished sequences' logs are
    // forgotten (a stage refuses to reopen a sequence it still holds).
    let mut island = ThreadIsland::start(&c, &[0, 1, 2, 4], Router::Random, |_, _, _| {});
    for (micro_batches, concurrency, prefill_chunk) in [
        (1, 1, 0),
        (1, 8, 0),
        (2, 4, 1),
        (3, 7, 2),
        (4, 14, 0),
        (8, 14, 3),
    ] {
        let schedule = Schedule {
            micro_batches,
            concurrency,
            prefill_chunk,
            forget_finished: true,
            ..Schedule::default()
        };
        let (done, _) = island.coordinator.run(&reqs, &schedule).unwrap();
        for (r, got) in expected.iter().zip(&done) {
            assert_matches(
                r,
                got,
                &format!(
                    "G {micro_batches} B {concurrency} chunk {prefill_chunk} id {}",
                    got.id
                ),
            );
        }
    }
    // One request alone, then among neighbours in reverse order.
    let alone = island
        .coordinator
        .run(
            &reqs[5..6],
            &Schedule {
                forget_finished: true,
                ..Schedule::default()
            },
        )
        .unwrap()
        .0;
    let mut reversed = reqs.clone();
    reversed.reverse();
    let crowded = island
        .coordinator
        .run(
            &reversed,
            &Schedule {
                micro_batches: 3,
                concurrency: 14,
                forget_finished: true,
                ..Schedule::default()
            },
        )
        .unwrap()
        .0;
    let among = crowded.iter().find(|c| c.id == reqs[5].id).unwrap();
    assert_eq!(alone[0].tokens, among.tokens);
    assert_eq!(alone[0].logits_hashes, among.logits_hashes);
    assert_eq!(
        alone[0].ledger.boundary_digests(),
        among.ledger.boundary_digests()
    );
    island.stop();
}

/// A routed-tie router (every key tied) through a 4-stage island: routing
/// ties resolve the same way on every stage of every layout.
#[test]
fn routing_ties_are_resolved_identically_across_stages() {
    let c = synthetic::tiny_config(true, ExpertFormat::Int4G32);
    for router in [Router::Paired, Router::Flat] {
        let whole = stage_model(&c, 0, c.n_layers, router);
        let reqs = requests(&c, 4, 5);
        let mut island = ThreadIsland::start(&c, &[0, 1, 2, 3, 4], router, |_, _, _| {});
        let (done, _) = island
            .coordinator
            .run(
                &reqs,
                &Schedule {
                    micro_batches: 2,
                    concurrency: 4,
                    ..Schedule::default()
                },
            )
            .unwrap();
        for (r, got) in reqs.iter().zip(&done) {
            assert_matches(
                &reference(&whole, r),
                got,
                &format!("{router:?} id {}", r.id),
            );
        }
        island.stop();
    }
}

/// Refused requests fail alone, with `generate`'s reasons, and never reach
/// the ring; their neighbours are untouched.
#[test]
fn refused_requests_fail_alone() {
    let c = synthetic::tiny_config(false, ExpertFormat::Int8Dyadic);
    let whole = stage_model(&c, 0, c.n_layers, Router::Random);
    let mut reqs = requests(&c, 3, 11);
    let bad = |id: u64, prompt: Vec<u32>, max_tokens: usize| Request {
        id,
        prompt,
        max_tokens,
        eos: vec![],
        selection: Selection::Argmax,
    };
    reqs.insert(1, bad(1, vec![], 3));
    reqs.insert(2, bad(2, vec![1; 20], 5));
    reqs.push(bad(3, vec![1, 50], 2));
    let mut island = ThreadIsland::start(&c, &[0, 2, 4], Router::Random, |_, _, _| {});
    let (done, _) = island
        .coordinator
        .run(
            &reqs,
            &Schedule {
                micro_batches: 2,
                concurrency: 3,
                ..Schedule::default()
            },
        )
        .unwrap();
    for (r, got) in reqs.iter().zip(&done) {
        let generated = whole.generate(&GenerationRequest {
            prompt: &r.prompt,
            max_tokens: r.max_tokens,
            eos: &r.eos,
            selection: r.selection,
        });
        match generated {
            Ok(g) => assert_matches(&g, got, &format!("id {}", r.id)),
            Err(e) => {
                let message = got.error.as_deref().expect("a refusal");
                assert!(
                    e.to_string().contains(message),
                    "id {}: {e} vs {message}",
                    r.id
                );
                assert!(got.tokens.is_empty());
            }
        }
    }
    island.stop();
}

/// A worker that crashes and restarts on its log rebuilds every open
/// sequence's cache by replay, and continues byte for byte as if it had
/// never stopped. A torn last record is dropped; a log that does not
/// reproduce its commitments is refused.
#[test]
fn a_restarted_worker_replays_its_log_and_continues_byte_identically() {
    let c = synthetic::tiny_config(true, ExpertFormat::Int4G32);
    let dir = std::env::temp_dir().join(format!("arc-island-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = || WorkerConfig {
        log_dir: Some(dir.clone()),
        fault: None,
    };
    let stage = || stage_model(&c, 1, 3, Router::Random);
    // Boundary-1 inputs for two sequences, from the first layer.
    let first = stage_model(&c, 0, 1, Router::Random);
    let mut feeder = StageWorker::new(first, WorkerConfig::default()).unwrap();
    let mut step = |seq: u64, start: u32, tokens: Vec<u32>| -> Item {
        let item = Item::new(seq, start, 3, Selection::Rp64Argmax, tokens);
        match feeder
            .process(Frame::Step {
                id: 0,
                items: vec![item],
            })
            .unwrap()
        {
            Frame::Step { mut items, .. } => {
                let mut item = items.remove(0);
                item.commits.clear();
                item
            }
            _ => unreachable!(),
        }
    };
    let batches = [
        vec![step(1, 0, vec![4, 9, 2]), step(2, 0, vec![7])],
        vec![step(1, 3, vec![11]), step(2, 1, vec![3, 3])],
        vec![step(1, 4, vec![30]), step(2, 3, vec![8])],
    ];
    let run = |worker: &mut StageWorker, items: &[Item]| -> Vec<Item> {
        match worker
            .process(Frame::Step {
                id: 0,
                items: items.to_vec(),
            })
            .unwrap()
        {
            Frame::Step { items, .. } => items,
            _ => unreachable!(),
        }
    };
    // Uninterrupted reference, without a log.
    let mut steady = StageWorker::new(stage(), WorkerConfig::default()).unwrap();
    let expected: Vec<Vec<Item>> = batches.iter().map(|b| run(&mut steady, b)).collect();
    // Two batches, crash (drop without shutdown), restart, third batch.
    let mut before = StageWorker::new(stage(), config()).unwrap();
    assert_eq!(run(&mut before, &batches[0]), expected[0]);
    assert_eq!(run(&mut before, &batches[1]), expected[1]);
    drop(before);
    let log = dir.join("stage-1-3.arclog");
    // A torn record at the tail (a crash mid-write) is ignored.
    let mut bytes = std::fs::read(&log).unwrap();
    let intact = bytes.len();
    bytes.extend_from_slice(&[200, 0, 0, 0, 1, 2, 3]);
    std::fs::write(&log, &bytes).unwrap();
    let mut after = StageWorker::new(stage(), config()).unwrap();
    assert_eq!(after.stats.replayed_positions, 7);
    assert_eq!(after.cached_positions(1), Some(4));
    assert_eq!(after.cached_positions(2), Some(3));
    assert_eq!(std::fs::metadata(&log).unwrap().len() as usize, intact);
    assert_eq!(run(&mut after, &batches[2]), expected[2]);
    drop(after);
    // A different model cannot adopt this log.
    let other = stage_model(&c, 1, 3, Router::Paired);
    let refused = StageWorker::new(other, config());
    assert!(
        matches!(&refused, Err(ModernError::Invalid(m)) if m.contains("does not reproduce")),
        "{:?}",
        refused.err()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A stage that alters its output and commits the altered hash keeps every
/// link consistent; re-executing each stage from its revealed log, with only
/// that stage's weights, blames exactly that stage at that position.
#[test]
fn a_tampered_stage_is_rejected_by_re_execution() {
    let c = synthetic::tiny_config(true, ExpertFormat::Int8Dyadic);
    let whole = stage_model(&c, 0, c.n_layers, Router::Random);
    let cuts = [0, 1, 3, 4];
    let reqs = requests(&c, 3, 21);
    let victim = reqs[1].id;
    let honest: Vec<MlaGeneration> = reqs.iter().map(|r| reference(&whole, r)).collect();
    let mut island = ThreadIsland::start(&c, &cuts, Router::Random, |s, _, config| {
        if s == 1 {
            config.fault = Some(Fault::TamperOutput {
                seq: victim,
                position: 2,
            });
        }
    });
    let (done, _) = island
        .coordinator
        .run(
            &reqs,
            &Schedule {
                micro_batches: 2,
                concurrency: 3,
                ..Schedule::default()
            },
        )
        .unwrap();
    let verifiers: Vec<StageModel> = cuts
        .windows(2)
        .map(|w| stage_model(&c, w[0], w[1], Router::Random))
        .collect();
    let verifier_refs: Vec<&StageModel> = verifiers.iter().collect();
    for (r, got) in reqs.iter().zip(&done) {
        // The lie is consistent: every link agrees.
        assert!(got.ledger.complete(), "id {}", r.id);
        let revealed = island.coordinator.reveal(r.id).unwrap();
        assert_eq!(revealed.len(), 3);
        let verdicts = audit_all(
            &got.ledger,
            &revealed,
            &verifier_refs,
            &AuditContext::new(r, &got.tokens),
        )
        .unwrap();
        for ((a, b), verdict) in verdicts {
            if r.id == victim && (a, b) == (1, 3) {
                assert_eq!(
                    verdict,
                    Verdict::Fault {
                        position: 2,
                        boundary: 3
                    }
                );
            } else {
                assert!(
                    matches!(verdict, Verdict::Valid { .. }),
                    "id {} [{a}, {b}): {verdict:?}",
                    r.id
                );
            }
        }
        if r.id != victim {
            let i = reqs.iter().position(|q| q.id == r.id).unwrap();
            assert_matches(&honest[i], got, "an honest neighbour");
        } else {
            assert_ne!(
                got.ledger.boundary_digests().unwrap()[3],
                honest[1].boundary_digests[3]
            );
        }
        // An altered reveal is caught at its first altered position.
        let mut forged = revealed[2].clone();
        forged.inputs[c.d_model] += 1;
        assert_eq!(
            audit_stage(
                &verifiers[2],
                &forged,
                got.ledger.stage_commits(3, 4).unwrap(),
                &AuditContext::new(r, &got.tokens)
            ),
            Verdict::InputMismatch { position: 1 }
        );
    }
    island.stop();
}

/// The last stage emits a token other than its sampler's. Every activation
/// and logits hash it commits is honest, so every link agrees; re-executing
/// it and re-applying the selection rule to the committed logits blames it
/// at that position. A token altered on its way back to the ring is caught
/// too (the committed token differs from the token fed next).
#[test]
fn a_wrong_token_from_the_last_stage_is_rejected_by_re_selection() {
    let c = synthetic::tiny_config(true, ExpertFormat::Int4G32);
    let whole = stage_model(&c, 0, c.n_layers, Router::Random);
    let request = |id: u64, prompt: Vec<u32>, selection| Request {
        id,
        prompt,
        max_tokens: 6,
        eos: vec![],
        selection,
    };
    let reqs = vec![
        request(1, vec![3, 17, 5], Selection::Rp64Argmax),
        request(2, vec![41, 2], Selection::Argmax),
    ];
    // Position 3 is where request 1's second token is selected.
    let (victim, position) = (1u64, 3usize);
    let cuts = [0, 2, 3, 4];
    let mut island = ThreadIsland::start(&c, &cuts, Router::Random, |s, _, config| {
        if s == 2 {
            config.fault = Some(Fault::WrongToken {
                seq: victim,
                position: position as u32,
            });
        }
    });
    let (done, _) = island
        .coordinator
        .run(
            &reqs,
            &Schedule {
                micro_batches: 2,
                concurrency: 2,
                ..Schedule::default()
            },
        )
        .unwrap();
    let honest = reference(&whole, &reqs[0]);
    let got = &done[0];
    // The lie is in the emitted token only: every link agrees, and the
    // wrong token became the next position's input.
    assert!(got.ledger.complete());
    assert_eq!(got.tokens[0], honest.tokens[0]);
    assert_eq!(got.tokens[1], (honest.tokens[1] + 1) % c.vocab_size as u32);
    let verifiers: Vec<StageModel> = cuts
        .windows(2)
        .map(|w| stage_model(&c, w[0], w[1], Router::Random))
        .collect();
    let verifier_refs: Vec<&StageModel> = verifiers.iter().collect();
    let revealed = island.coordinator.reveal(victim).unwrap();
    for ((a, b), verdict) in audit_all(
        &got.ledger,
        &revealed,
        &verifier_refs,
        &AuditContext::new(&reqs[0], &got.tokens),
    )
    .unwrap()
    {
        if (a, b) == (3, 4) {
            assert_eq!(
                verdict,
                Verdict::WrongToken {
                    position,
                    committed: got.tokens[1],
                    expected: honest.tokens[1],
                }
            );
        } else {
            assert!(
                matches!(verdict, Verdict::Valid { .. }),
                "[{a}, {b}): {verdict:?}"
            );
        }
    }
    // Every logits hash and selection is in the last stage's record.
    let head = got.ledger.stage_commits(3, 4).unwrap();
    assert!(head.iter().all(|p| p.logits.is_some()));
    assert_eq!(head[2].selected, Some(got.tokens[0]));
    // An honest neighbour verifies, including its selections; changing a
    // token it was fed after one it emitted is caught.
    assert_matches(
        &reference(&whole, &reqs[1]),
        &done[1],
        "the honest neighbour",
    );
    let revealed = island.coordinator.reveal(2).unwrap();
    let commits = done[1].ledger.stage_commits(3, 4).unwrap();
    assert!(matches!(
        audit_stage(
            &verifiers[2],
            &revealed[2],
            commits,
            &AuditContext::new(&reqs[1], &done[1].tokens)
        ),
        Verdict::Valid { .. }
    ));
    let mut forged = revealed[2].clone();
    forged.tokens[2] = (forged.tokens[2] + 1) % c.vocab_size as u32;
    assert_eq!(
        audit_stage(
            &verifiers[2],
            &forged,
            commits,
            &AuditContext::new(&reqs[1], &done[1].tokens)
        ),
        Verdict::ForwardMismatch { position: 1 }
    );
    island.stop();
}

/// A stage that sends honest activations but commits a different output
/// hash breaks the link check at that position, and its own audit fails.
#[test]
fn a_lying_commitment_breaks_the_link_check() {
    let c = synthetic::tiny_config(false, ExpertFormat::Int8Dyadic);
    let whole = stage_model(&c, 0, c.n_layers, Router::Random);
    let reqs = requests(&c, 2, 3);
    let victim = reqs[0].id;
    let mut island = ThreadIsland::start(&c, &[0, 2, 4], Router::Random, |s, _, config| {
        if s == 0 {
            config.fault = Some(Fault::LieInCommit {
                seq: victim,
                position: 0,
            });
        }
    });
    let (done, _) = island.coordinator.run(&reqs, &Schedule::default()).unwrap();
    let got = &done[0];
    // The data was honest, so the tokens are too...
    assert_eq!(got.tokens, reference(&whole, &reqs[0]).tokens);
    // ...but the commitment chain is broken at boundary 2, position 0.
    assert!(!got.ledger.complete());
    assert_eq!(got.ledger.link_faults.len(), 1);
    let fault = &got.ledger.link_faults[0];
    assert_eq!((fault.position, fault.boundary), (0, 2));
    assert_eq!((fault.upstream, fault.downstream), ((0, 2), (2, 4)));
    let revealed = island.coordinator.reveal(victim).unwrap();
    let stage0 = stage_model(&c, 0, 2, Router::Random);
    assert_eq!(
        audit_stage(
            &stage0,
            &revealed[0],
            got.ledger.stage_commits(0, 2).unwrap(),
            &AuditContext::new(&reqs[0], &got.tokens)
        ),
        Verdict::Fault {
            position: 0,
            boundary: 2
        }
    );
    assert_matches(&reference(&whole, &reqs[1]), &done[1], "the other request");
    island.stop();
}

/// Expert parallelism: a stage's routed experts on 2 or 3 devices (expert
/// servers on other threads) inside a pipeline, and the whole model as one
/// stage with experts on 3 devices: the bytes of the single device.
#[test]
fn expert_parallel_stages_are_byte_identical() {
    for (lora, format) in FORMATS {
        let c = synthetic::tiny_config(lora, format);
        let whole = stage_model(&c, 0, c.n_layers, Router::Random);
        let reqs = requests(&c, 4, 17);
        let expected: Vec<MlaGeneration> = reqs.iter().map(|r| reference(&whole, r)).collect();
        for (cuts, devices) in [(vec![0, 2, 4], 2usize), (vec![0, 4], 3)] {
            let mem = MemTransport::new();
            let tag = format!("experts-{lora}-{format:?}-{devices}");
            let addresses: Vec<String> = (0..devices).map(|d| format!("{tag}-{d}")).collect();
            // Device 0 is the stage itself; devices 1.. serve the last stage's layers.
            let (a, b) = (cuts[cuts.len() - 2], cuts[cuts.len() - 1]);
            for address in &addresses[1..] {
                let listener = mem.listen(address).unwrap();
                let server = Arc::new(stage_model(&c, a, b, Router::Random));
                std::thread::spawn(move || serve_experts(server, listener));
            }
            let pools = std::sync::Mutex::new(Vec::new());
            let mut island = ThreadIsland::start(&c, &cuts, Router::Random, |s, model, _| {
                if s == cuts.len() - 2 {
                    let pool = Arc::new(
                        RemoteExperts::connect_placed(
                            &mem,
                            ExpertPlacement { devices, local: 0 },
                            &addresses,
                            Some(
                                (0..c.n_routed_experts)
                                    .map(|e| (e / 3 + 1) % devices)
                                    .collect(),
                            ),
                        )
                        .unwrap(),
                    );
                    pools.lock().unwrap().push(pool.clone());
                    model.set_expert_pool(Some(pool as Arc<dyn ExpertPool>));
                }
            });
            let (done, _) = island
                .coordinator
                .run(
                    &reqs,
                    &Schedule {
                        micro_batches: 2,
                        concurrency: 4,
                        ..Schedule::default()
                    },
                )
                .unwrap();
            for (r, got) in expected.iter().zip(&done) {
                assert_matches(r, got, &format!("{tag} cuts {cuts:?}"));
            }
            let calls: u64 = pools
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.calls.load(std::sync::atomic::Ordering::Relaxed))
                .sum();
            assert!(calls > 0, "{tag}: no expert went remote");
            island.stop();
        }
    }
}

/// Pings travel the ring untouched.
#[test]
fn pings_travel_the_ring() {
    let c = synthetic::tiny_config(false, ExpertFormat::Int8Dyadic);
    let mut island = ThreadIsland::start(&c, &[0, 1, 2, 4], Router::Random, |_, _, _| {});
    for payload in [0, 100, 70_000] {
        assert!(island.coordinator.ping(payload).unwrap() >= 0.0);
    }
    let workers = island.stop();
    assert!(workers.iter().all(|w| w.stats.frames >= 3));
}

#[test]
fn logits_digests_match_generate_on_a_prompt_only_budget() {
    // max_tokens = 1: the token comes from the prompt's last position only.
    let c = synthetic::tiny_config(true, ExpertFormat::Int8Dyadic);
    let whole = stage_model(&c, 0, c.n_layers, Router::Random);
    let r = Request {
        id: 5,
        prompt: vec![1, 2, 3, 4],
        max_tokens: 1,
        eos: vec![],
        selection: Selection::Rp64Argmax,
    };
    let mut island = ThreadIsland::start(&c, &[0, 2, 4], Router::Random, |_, _, _| {});
    let (done, _) = island
        .coordinator
        .run(std::slice::from_ref(&r), &Schedule::default())
        .unwrap();
    let g = reference(&whole, &r);
    assert_matches(&g, &done[0], "prompt only");
    assert_eq!(
        done[0].logits_digest(),
        arith::logits_digest(&g.logits_hashes)
    );
    island.stop();
}

#[test]
fn tree_one_pass_matches_separate_paths_and_preserves_prefix() {
    use super::wire::TreeNode;
    for (lora, format) in FORMATS {
        let c = synthetic::tiny_config(lora, format);
        for mask in 0..8 {
            let mut cuts = vec![0];
            cuts.extend((1..4).filter(|i| mask & (1 << (i - 1)) != 0));
            cuts.push(4);
            let make = || {
                cuts.windows(2)
                    .map(|r| {
                        StageWorker::new(
                            stage_model(&c, r[0], r[1], Router::Random),
                            WorkerConfig::default(),
                        )
                        .unwrap()
                    })
                    .collect::<Vec<_>>()
            };
            let mut workers = make();
            let prefix = Frame::Step {
                id: 0,
                items: vec![Item::new(7, 0, 2, Selection::Rp64Argmax, vec![3, 9])],
            };
            let mut frame = prefix.clone();
            for w in &mut workers {
                frame = w.process(frame).unwrap();
            }
            // Two siblings plus a grandchild: sibling KV must never leak.
            let nodes = vec![
                TreeNode {
                    parent: None,
                    item: Item::new(100, 2, 2, Selection::Rp64Argmax, vec![4]),
                },
                TreeNode {
                    parent: None,
                    item: Item::new(101, 2, 2, Selection::Rp64Argmax, vec![8]),
                },
                TreeNode {
                    parent: Some(0),
                    item: Item::new(102, 3, 2, Selection::Rp64Argmax, vec![6]),
                },
            ];
            let mut tree = Frame::Tree {
                id: 1,
                prefix: 7,
                nodes,
            };
            assert_eq!(Frame::decode(&tree.encode()).unwrap(), tree);
            for w in &mut workers {
                tree = w.process(tree).unwrap();
            }
            let Frame::Tree { nodes, .. } = tree else {
                panic!("tree");
            };
            for (node, path) in nodes.iter().zip([vec![4], vec![8], vec![4, 6]]) {
                let mut refs = make();
                let mut frame = prefix.clone();
                for w in &mut refs {
                    frame = w.process(frame).unwrap();
                }
                for (j, token) in path.iter().enumerate() {
                    frame = Frame::Step {
                        id: j as u64 + 1,
                        items: vec![Item::new(
                            7,
                            j as u32 + 2,
                            2,
                            Selection::Rp64Argmax,
                            vec![*token],
                        )],
                    };
                    for w in &mut refs {
                        frame = w.process(frame).unwrap();
                    }
                }
                let Frame::Step { items, .. } = frame else {
                    panic!("step");
                };
                assert_eq!(node.item.hidden, items[0].hidden);
                assert_eq!(node.item.commits, items[0].commits);
            }
            // The public ENG-8 coordinator API sends the whole tree in one
            // frame, leaves the live prefix usable, then closes it explicitly.
            let mut island = ThreadIsland::start(&c, &cuts, Router::Random, |_, _, _| {});
            island
                .coordinator
                .forward_batch(
                    0,
                    vec![Item::new(7, 0, 2, Selection::Rp64Argmax, vec![3, 9])],
                )
                .unwrap();
            let input_nodes = nodes
                .iter()
                .map(|node| super::wire::TreeNode {
                    parent: node.parent,
                    item: Item::new(
                        node.item.seq,
                        node.item.start,
                        2,
                        Selection::Rp64Argmax,
                        node.item.tokens.clone(),
                    ),
                })
                .collect();
            assert_eq!(
                island.coordinator.verify_tree(1, 7, input_nodes).unwrap(),
                nodes
            );
            island.coordinator.close_sequences(vec![7], true).unwrap();
            island.stop();
            for w in &workers {
                assert_eq!(w.cached_positions(7), Some(2));
                for id in [100, 101, 102] {
                    assert_eq!(w.cached_positions(id), None);
                }
            }
            // Malformed trees fail before changing any live cache.
            let bad = Frame::Tree {
                id: 4,
                prefix: 7,
                nodes: vec![TreeNode {
                    parent: Some(0),
                    item: Item::new(200, 2, 2, Selection::Rp64Argmax, vec![5]),
                }],
            };
            assert!(workers[0].process(bad).is_err());
            assert_eq!(workers[0].cached_positions(7), Some(2));
        }
    }
}

#[test]
fn replica_replays_after_lost_reply_and_rejects_changed_replay() {
    use super::replica::{ReplicaConnector, ReplicatedStage, StageSession};
    struct Session {
        worker: StageWorker,
        calls: usize,
        lose: bool,
        corrupt: bool,
        unreachable: bool,
    }
    impl StageSession for Session {
        fn exchange(&mut self, input: &[u8]) -> Result<Vec<u8>, ModernError> {
            if self.unreachable {
                return Err(ModernError::Io("replay transport failed".into()));
            }
            self.calls += 1;
            let output = self.worker.process(Frame::decode(input)?)?;
            // Worker has advanced KV, but its reply is lost during churn.
            if self.lose && self.calls == 2 {
                return Err(ModernError::Io("lost in-flight reply".into()));
            }
            let mut bytes = output.encode();
            if self.corrupt {
                *bytes.last_mut().unwrap() ^= 1;
            }
            Ok(bytes)
        }
    }
    struct Connector {
        c: MlaConfig,
    }
    impl ReplicaConnector for Connector {
        fn connect(&self, endpoint: &str) -> Result<Box<dyn StageSession>, ModernError> {
            if endpoint == "unreachable" {
                return Err(ModernError::Io("connect failed".into()));
            }
            Ok(Box::new(Session {
                worker: StageWorker::new(
                    stage_model(&self.c, 0, 4, Router::Random),
                    WorkerConfig::default(),
                )?,
                calls: 0,
                lose: endpoint == "lost",
                corrupt: endpoint == "bad",
                unreachable: endpoint == "replay-error",
            }))
        }
    }
    let c = synthetic::tiny_config(true, ExpertFormat::Int4G32);
    let mut replica = ReplicatedStage::new(
        Arc::new(Connector { c: c.clone() }),
        vec!["lost".into(), "bad".into(), "healthy".into()],
        1 << 20,
    )
    .unwrap();
    let mut reference = StageWorker::new(
        stage_model(&c, 0, 4, Router::Random),
        WorkerConfig::default(),
    )
    .unwrap();
    for pos in 0..5 {
        // Two streams in every frame; frame IDs may be reused by the scheduler.
        let frame = Frame::Step {
            id: 0,
            items: [7, 8]
                .iter()
                .map(|&seq| Item::new(seq, pos, 1, Selection::Argmax, vec![pos + 3]))
                .collect(),
        };
        assert_eq!(
            replica.process(frame.clone()).unwrap(),
            reference.process(frame).unwrap()
        );
    }
    assert_eq!(replica.failovers, 2);
    assert_eq!(replica.replayed_frames, 1);
    assert_eq!(replica.divergent_replays, 1);
    for (spares, divergences, reason) in [
        (
            vec!["bad", "bad"],
            2,
            "all 2 remaining replicas diverged during replay",
        ),
        (
            vec!["unreachable", "unreachable"],
            0,
            "no reachable remaining replica",
        ),
        (
            vec!["replay-error", "replay-error"],
            0,
            "no reachable remaining replica",
        ),
        (
            vec!["bad", "unreachable"],
            1,
            "1 divergent replays and 1 unreachable replicas",
        ),
    ] {
        let mut relay = ReplicatedStage::new(
            Arc::new(Connector { c: c.clone() }),
            std::iter::once("lost")
                .chain(spares)
                .map(str::to_string)
                .collect(),
            1 << 20,
        )
        .unwrap();
        let frame = |position| Frame::Step {
            id: 0,
            items: vec![Item::new(7, position, 1, Selection::Argmax, vec![3])],
        };
        relay.process(frame(0)).unwrap();
        let error = relay.process(frame(1)).unwrap_err().to_string();
        assert!(error.contains(reason), "{error}");
        assert_eq!(relay.divergent_replays, divergences);
        assert!(
            relay
                .process(Frame::Ping {
                    id: 1,
                    payload: vec![]
                })
                .unwrap_err()
                .to_string()
                .contains("permanently refused")
        );
        println!("replay exhaustion: {error}; divergent_replays={divergences}");
    }
    let mut full =
        ReplicatedStage::new(Arc::new(Connector { c }), vec!["healthy".into()], 1).unwrap();
    assert!(
        full.process(Frame::Ping {
            id: 0,
            payload: vec![]
        })
        .is_err()
    );
}

#[test]
fn replica_identity_and_transport_timeout_fail_closed() {
    use super::replica::{
        ReplicaConnector, TransportConnector, serve_session_with, stage_identity,
    };
    use super::transport::{DeadlineTcpTransport, TcpTransport};
    use std::time::Duration;
    let c = synthetic::tiny_config(true, ExpertFormat::Int4G32);
    for wrong_identity in [true, false] {
        let model = stage_model(&c, 0, 4, Router::Random);
        let mut identity = stage_identity(&model);
        if wrong_identity {
            identity[0] ^= 1;
        }
        let mut listener = TcpTransport.listen("127.0.0.1:0").unwrap();
        let address = listener.address();
        let server = std::thread::spawn(move || {
            let link = listener.accept().unwrap();
            let worker = StageWorker::new(model, WorkerConfig::default()).unwrap();
            let _ = serve_session_with(worker, link, &mut |_| {
                std::thread::sleep(Duration::from_millis(500));
                true
            });
        });
        let connector = TransportConnector {
            transport: Arc::new(DeadlineTcpTransport {
                timeout: Duration::from_millis(200),
            }),
            stage_id: identity,
        };
        let result = connector.connect(&address);
        if wrong_identity {
            assert!(result.is_err());
        } else {
            let mut session = result.unwrap();
            let input = Frame::Ping {
                id: 0,
                payload: vec![],
            }
            .encode();
            assert!(
                session.exchange(&input).is_err(),
                "slow reply must time out"
            );
            drop(session);
        }
        server.join().unwrap();
    }
}

mod trusted_audit_cases {
    use crate as inference;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/audit_context_cases.rs"
    ));

    #[test]
    fn native_worker_metadata_attacks_require_trusted_context() {
        use super::{StageWorker, WorkerConfig};
        let mut workers = [
            StageWorker::new(model(0, 2), WorkerConfig::default()).unwrap(),
            StageWorker::new(model(2, 4), WorkerConfig::default()).unwrap(),
        ];
        exercise(|s, frame| workers[s].process(frame).unwrap());
    }
}

#[test]
fn coordinator_rejects_substituted_return_metadata_without_harming_neighbour() {
    use super::worker::Downstream;
    let c = synthetic::tiny_config(true, ExpertFormat::Int4G32);
    let reqs = vec![
        Request {
            id: 1,
            prompt: vec![0, 17, 5],
            max_tokens: 3,
            eos: vec![],
            selection: Selection::Rp64Argmax,
        },
        Request {
            id: 2,
            prompt: vec![0, 17, 5],
            max_tokens: 3,
            eos: vec![],
            selection: Selection::Rp64Argmax,
        },
    ];
    let whole = stage_model(&c, 0, 4, Router::Random);
    let expected = reference(&whole, &reqs[1]);
    let mem: Arc<dyn Transport> = Arc::new(MemTransport::new());
    let mut listener = mem.listen("metadata-stage").unwrap();
    let returns = mem.listen("metadata-coordinator").unwrap();
    let transport = mem.clone();
    let worker = std::thread::spawn(move || {
        let mut stage = StageWorker::new(whole, WorkerConfig::default()).unwrap();
        let mut link = listener.accept().unwrap();
        let mut downstream = Downstream::new(transport, "metadata-coordinator".into());
        loop {
            let input = Frame::decode(&link.recv().unwrap()).unwrap();
            let stop = matches!(input, Frame::Shutdown);
            let mut output = stage.process(input).unwrap();
            if let Frame::Step { items, .. } = &mut output {
                for item in items.iter_mut().filter(|item| item.seq == 1) {
                    item.selection = Selection::Argmax;
                }
            }
            downstream.send(&output.encode()).unwrap();
            if stop {
                break;
            }
        }
    });
    let mut coordinator = Coordinator::new(mem, "metadata-stage".into(), returns, c);
    let (done, _) = coordinator
        .run(
            &reqs,
            &Schedule {
                concurrency: 2,
                ..Schedule::default()
            },
        )
        .unwrap();
    assert!(
        done[0]
            .error
            .as_ref()
            .unwrap()
            .contains("trusted request/input metadata")
    );
    assert!(done[0].tokens.is_empty());
    assert_matches(&expected, &done[1], "honest neighbour");
    coordinator.shutdown().unwrap();
    worker.join().unwrap();
}

/// Asynchronous pipelined speculation (`speculative.rs`): tokens, logits
/// hashes and every stage's commitment at every position equal plain
/// decoding's for every drafter, acceptance pattern, depth, pass width and
/// split; rollbacks truncate in place, replay from the log and skip queued
/// dead work.
mod speculation {
    use std::collections::{BTreeSet, HashMap, VecDeque};
    use std::sync::mpsc::channel;

    use super::*;
    use crate::modern::mla::island::speculative::{
        Drafter, NgramDrafter, Script, ScriptedDrafter, SpecConfig,
    };
    use crate::modern::mla::island::worker::{
        SUPERSEDED, WorkerStats, serve_from, skip_superseded,
    };

    /// Proposes nothing, too many tokens, out-of-vocabulary tokens or plain
    /// noise, in turn: the coordinator must survive any drafter.
    struct Garbage {
        state: u64,
        vocab: u32,
    }

    impl Drafter for Garbage {
        fn name(&self) -> String {
            "garbage".into()
        }

        fn draft(&mut self, _context: &[u32], _prompt_len: usize, max: usize) -> Vec<u32> {
            let vocab = u64::from(self.vocab);
            match lcg(&mut self.state) % 4 {
                0 => Vec::new(),
                1 => (0..max + 3)
                    .map(|_| (lcg(&mut self.state) % vocab) as u32)
                    .collect(),
                2 => vec![1, self.vocab + 7, 2],
                _ => (0..max)
                    .map(|_| (lcg(&mut self.state) % vocab) as u32)
                    .collect(),
            }
        }
    }

    /// Never drafts: the pipeline holds only verified tokens.
    struct Silent;

    impl Drafter for Silent {
        fn name(&self) -> String {
            "silent".into()
        }

        fn draft(&mut self, _context: &[u32], _prompt_len: usize, _max: usize) -> Vec<u32> {
            Vec::new()
        }
    }

    /// (passes in flight, positions per pass): plain decoding, synchronous
    /// chains, and pipelined passes of one and several positions.
    const SHAPES: [(usize, usize); 7] = [(1, 1), (1, 2), (1, 4), (2, 1), (4, 1), (3, 2), (6, 3)];

    /// Prompts of 1..=6 tokens (every other one repeated, so n-grams match),
    /// budgets up to the context, some EOS ids and both selection rules.
    fn spec_requests(c: &MlaConfig, count: usize, seed: u64) -> Vec<Request> {
        let mut s = seed;
        (0..count)
            .map(|i| {
                let len = 1 + (lcg(&mut s) % 6) as usize;
                let mut prompt: Vec<u32> = (0..len)
                    .map(|_| (lcg(&mut s) % c.vocab_size as u64) as u32)
                    .collect();
                if i % 2 == 1 {
                    prompt.extend_from_within(..);
                }
                let room = (c.max_seq - prompt.len()) as u64;
                let max_tokens = 1 + (lcg(&mut s) % room) as usize;
                let eos = if i % 4 == 3 {
                    vec![(lcg(&mut s) % c.vocab_size as u64) as u32]
                } else {
                    Vec::new()
                };
                let selection = if i % 2 == 1 {
                    Selection::Argmax
                } else {
                    Selection::Rp64Argmax
                };
                Request {
                    id: 2000 + i as u64,
                    prompt,
                    max_tokens,
                    eos,
                    selection,
                }
            })
            .collect()
    }

    fn truth(reqs: &[Request], expected: &[MlaGeneration]) -> HashMap<u64, Vec<u32>> {
        reqs.iter()
            .zip(expected)
            .map(|(r, g)| (r.id, g.tokens.clone()))
            .collect()
    }

    /// What a drafter's script guarantees about the drafts it sends.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Forces {
        /// Nothing: always right, or not scripted.
        Nothing,
        /// Some drafts are wrong.
        Rejections,
        /// Every draft is wrong.
        EveryDraft,
    }

    fn drafters(
        c: &MlaConfig,
        reqs: &[Request],
        expected: &[MlaGeneration],
    ) -> Vec<(Box<dyn Drafter>, Forces)> {
        let truth = truth(reqs, expected);
        let scripted = |script: Script| -> Box<dyn Drafter> {
            Box::new(ScriptedDrafter::new(truth.clone(), c.vocab_size, script))
        };
        let case = |drafter: Box<dyn Drafter>, forces: Forces| (drafter, forces);
        vec![
            case(scripted(Script::Pattern(vec![true])), Forces::Nothing),
            case(scripted(Script::Pattern(vec![false])), Forces::EveryDraft),
            case(
                scripted(Script::Pattern(vec![true, false])),
                Forces::Rejections,
            ),
            case(
                scripted(Script::Pattern(vec![true, true, false])),
                Forces::Rejections,
            ),
            case(
                scripted(Script::Rate { rate: 0.5, seed: 3 }),
                Forces::Rejections,
            ),
            case(Box::<NgramDrafter>::default(), Forces::Nothing),
            case(
                Box::new(Garbage {
                    state: 11,
                    vocab: c.vocab_size as u32,
                }),
                Forces::Nothing,
            ),
            case(Box::new(Silent), Forces::Nothing),
        ]
    }

    /// The correctness gate: on every split (all of them for one format),
    /// every drafter and every shape gives the single process's tokens,
    /// logits hashes and boundary digests, and a ledger equal to plain
    /// decoding's on the same ring at every position of every stage. It also
    /// proves speculation ran: drafters whose script makes drafts wrong cause
    /// rejections, rollbacks and (with passes in flight) cancelled passes,
    /// and their rejections land at every row that carries drafts.
    #[test]
    fn speculation_matches_plain_decoding_for_any_drafter_depth_and_split() {
        for (f, (lora, format)) in FORMATS.into_iter().enumerate() {
            let c = synthetic::tiny_config(lora, format);
            let whole = stage_model(&c, 0, c.n_layers, Router::Random);
            let mut reqs = spec_requests(&c, 6, 31 + f as u64);
            // One answer that runs to the end of the context without EOS, so
            // every scripted pattern reaches every row of every shape.
            let prompt = vec![5, 6, 7];
            reqs.push(Request {
                id: 2900,
                max_tokens: c.max_seq - prompt.len(),
                prompt,
                eos: Vec::new(),
                selection: Selection::Argmax,
            });
            let expected: Vec<MlaGeneration> = reqs.iter().map(|r| reference(&whole, r)).collect();
            let masks: Vec<u32> = if f == 0 {
                (0..1 << (c.n_layers - 1)).collect()
            } else {
                vec![0, 0b010, 0b111]
            };
            for mask in masks {
                let mut cuts = vec![0];
                cuts.extend((1..c.n_layers).filter(|i| mask & (1 << (i - 1)) != 0));
                cuts.push(c.n_layers);
                let mut island = ThreadIsland::start(&c, &cuts, Router::Random, |_, _, _| {});
                let plain_schedule = Schedule {
                    forget_finished: true,
                    ..Schedule::default()
                };
                let (plain, _) = island.coordinator.run(&reqs, &plain_schedule).unwrap();
                for (g, got) in expected.iter().zip(&plain) {
                    assert_matches(g, got, &format!("plain cuts {cuts:?} id {}", got.id));
                }
                for (depth, rows) in SHAPES {
                    // Rows at which a forced rejection was observed, over drafters.
                    let mut rejected_rows = BTreeSet::new();
                    for (mut drafter, forces) in drafters(&c, &reqs, &expected) {
                        let config = SpecConfig {
                            depth,
                            rows,
                            forget_finished: true,
                            ..SpecConfig::default()
                        };
                        let at = format!(
                            "{format:?} lora {lora} cuts {cuts:?} D {depth} R {rows} {}",
                            drafter.name()
                        );
                        let (done, stats, spec) = island
                            .coordinator
                            .run_speculative(&reqs, &config, drafter.as_mut())
                            .unwrap();
                        for ((g, p), got) in expected.iter().zip(&plain).zip(&done) {
                            assert_matches(g, got, &format!("{at} id {}", got.id));
                            assert_eq!(got.ledger, p.ledger, "{at} id {}: commitments", got.id);
                        }
                        let tokens: u64 = expected.iter().map(|g| g.tokens.len() as u64).sum();
                        assert_eq!(stats.generated_tokens, tokens, "{at}");
                        assert!(spec.max_in_flight <= depth as u64, "{at}: {spec:?}");
                        // Prove the drafts reached the ring: every rejection is
                        // attributed to the row that carried it and rolls the
                        // stages back.
                        let attributed: u64 = spec.rejected_rows.iter().sum();
                        assert_eq!(attributed, spec.rejected, "{at}: {spec:?}");
                        assert!(spec.rollbacks >= spec.rejected, "{at}: {spec:?}");
                        if depth == 1 {
                            // A synchronous pass carries the verified token at
                            // row 0, and nothing is on the ring behind it.
                            let row0 = spec.rejected_rows.first().copied().unwrap_or(0);
                            assert_eq!(row0, 0, "{at}: {spec:?}");
                            assert_eq!(spec.cancelled_passes, 0, "{at}: {spec:?}");
                        }
                        if rows == 1 && depth == 1 {
                            assert_eq!(spec.drafted, 0, "{at}: plain decoding never drafts");
                        } else if forces != Forces::Nothing {
                            // The script makes drafts wrong: they must have been
                            // rejected and rolled back, and with passes in
                            // flight, later passes must have been cancelled.
                            assert!(spec.rejected > 0 && spec.rollbacks > 0, "{at}: {spec:?}");
                            if depth > 1 {
                                assert!(spec.cancelled_passes > 0, "{at}: {spec:?}");
                            }
                            for (row, &n) in spec.rejected_rows.iter().enumerate() {
                                if n > 0 {
                                    rejected_rows.insert(row);
                                }
                            }
                        }
                    }
                    if rows > 1 || depth > 1 {
                        // Forced rejections landed at every row that carries
                        // drafts: rows 1.. of a synchronous pass, every row of a
                        // pipelined one.
                        let first = usize::from(depth == 1);
                        let every_row: BTreeSet<usize> = (first..rows).collect();
                        assert_eq!(
                            rejected_rows, every_row,
                            "cuts {cuts:?} D {depth} R {rows}: rows with forced rejections"
                        );
                    }
                }
                island.stop();
            }
        }
    }

    /// After speculative answers every stage holds exactly the verified
    /// positions: each stage's log re-executes to its commitments, and the
    /// rejected drafts were rolled back or skipped.
    #[test]
    fn speculative_answers_pass_every_stage_audit() {
        let c = synthetic::tiny_config(true, ExpertFormat::Int4G32);
        let whole = stage_model(&c, 0, c.n_layers, Router::Random);
        let cuts = [0, 1, 3, 4];
        let verifiers: Vec<StageModel> = cuts
            .windows(2)
            .map(|w| stage_model(&c, w[0], w[1], Router::Random))
            .collect();
        let verifier_refs: Vec<&StageModel> = verifiers.iter().collect();
        let reqs = spec_requests(&c, 6, 77);
        let expected: Vec<MlaGeneration> = reqs.iter().map(|r| reference(&whole, r)).collect();
        let mut drafter = ScriptedDrafter::new(
            truth(&reqs, &expected),
            c.vocab_size,
            Script::Rate { rate: 0.6, seed: 9 },
        );
        let mut island = ThreadIsland::start(&c, &cuts, Router::Random, |_, _, _| {});
        let config = SpecConfig {
            depth: 4,
            rows: 2,
            ..SpecConfig::default()
        };
        let (done, _, spec) = island
            .coordinator
            .run_speculative(&reqs, &config, &mut drafter)
            .unwrap();
        assert!(
            spec.accepted > 0 && spec.rejected > 0 && spec.rollbacks > 0,
            "{spec:?}"
        );
        for ((r, g), got) in reqs.iter().zip(&expected).zip(&done) {
            assert_matches(g, got, &format!("id {}", r.id));
            let revealed = island.coordinator.reveal(r.id).unwrap();
            let verdicts = audit_all(
                &got.ledger,
                &revealed,
                &verifier_refs,
                &AuditContext::new(r, &got.tokens),
            )
            .unwrap();
            for ((a, b), verdict) in verdicts {
                assert!(
                    matches!(verdict, Verdict::Valid { .. }),
                    "id {} [{a}, {b}): {verdict:?}",
                    r.id
                );
            }
        }
        let workers = island.stop();
        let discarded: u64 = workers
            .iter()
            .map(|w| w.stats.rolled_back_positions + w.stats.skipped_positions)
            .sum();
        assert!(discarded > 0);
    }

    fn through(workers: &mut [StageWorker], frame: Frame) -> Frame {
        workers
            .iter_mut()
            .fold(frame, |frame, w| w.process(frame).unwrap())
    }

    fn one(start: u32, token: u32) -> Frame {
        Frame::Step {
            id: u64::from(start),
            items: vec![Item::new(5, start, 2, Selection::Rp64Argmax, vec![token])],
        }
    }

    /// A rollback truncates every stage in place and is logged: a restarted
    /// stage replays items and rollbacks to the same state, and continuing
    /// gives the bytes of a ring that never saw the rejected draft.
    #[test]
    fn rollbacks_truncate_in_place_and_replay_from_the_log() {
        let c = synthetic::tiny_config(true, ExpertFormat::Int8Dyadic);
        let dir = std::env::temp_dir().join(format!("arc-island-rollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logged = || WorkerConfig {
            log_dir: Some(dir.clone()),
            fault: None,
        };
        let ring = |log: bool| -> Vec<StageWorker> {
            [(0, 1), (1, 3), (3, 4)]
                .into_iter()
                .map(|(a, b)| {
                    let config = if log && a == 1 {
                        logged()
                    } else {
                        WorkerConfig::default()
                    };
                    StageWorker::new(stage_model(&c, a, b, Router::Random), config).unwrap()
                })
                .collect()
        };
        let prefill = Frame::Step {
            id: 0,
            items: vec![Item::new(5, 0, 2, Selection::Rp64Argmax, vec![3, 9])],
        };
        let mut reference = ring(false);
        let expected: Vec<Frame> = [prefill.clone(), one(2, 4), one(3, 7), one(4, 8)]
            .into_iter()
            .map(|frame| through(&mut reference, frame))
            .collect();
        let mut spec = ring(true);
        assert_eq!(through(&mut spec, prefill), expected[0]);
        assert_eq!(through(&mut spec, one(2, 4)), expected[1]);
        through(&mut spec, one(3, 40));
        through(&mut spec, one(4, 41));
        assert_eq!(spec[1].cached_positions(5), Some(5));
        let rollback = Frame::Rollback { seq: 5, keep: 3 };
        assert_eq!(through(&mut spec, rollback.clone()), rollback);
        for w in &spec {
            assert_eq!(w.cached_positions(5), Some(3));
            assert_eq!(w.stats.rolled_back_positions, 2);
        }
        assert_eq!(through(&mut spec, one(3, 7)), expected[2]);
        // Crash the logged stage and restart it from its log.
        let restarted = StageWorker::new(stage_model(&c, 1, 3, Router::Random), logged()).unwrap();
        assert_eq!(restarted.cached_positions(5), Some(4));
        spec[1] = restarted;
        assert_eq!(through(&mut spec, one(4, 8)), expected[3]);
        // Every stage's log holds only the verified positions.
        let reveal = || Frame::Reveal {
            seq: 5,
            stages: Vec::new(),
        };
        assert_eq!(
            through(&mut spec, reveal()),
            through(&mut reference, reveal())
        );
        // Unknown sequences are ignored; growing or a closed cache is refused.
        assert!(
            spec[0]
                .process(Frame::Rollback { seq: 99, keep: 0 })
                .is_ok()
        );
        assert!(
            spec[0]
                .process(Frame::Rollback { seq: 5, keep: 9 })
                .is_err()
        );
        assert_eq!(spec[0].cached_positions(5), Some(5));
        spec[0]
            .process(Frame::Close {
                seqs: vec![5],
                forget: false,
            })
            .unwrap();
        assert!(
            spec[0]
                .process(Frame::Rollback { seq: 5, keep: 1 })
                .is_err()
        );
        drop(spec);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Early cancellation: an item that a rollback queued behind it discards
    /// is marked superseded and passed on uncomputed; other sequences and
    /// positions before the kept length are untouched.
    #[test]
    fn queued_rollbacks_mark_the_items_they_discard() {
        let item = |seq: u64, start: u32| {
            let mut item = Item::new(seq, start, 1, Selection::Argmax, vec![1]);
            item.hidden = vec![1, 2];
            item.pad = 9;
            item
        };
        let mut frame = Frame::Step {
            id: 1,
            items: vec![item(7, 5), item(7, 6), item(8, 6)],
        };
        let queued: VecDeque<Result<Frame, ModernError>> = [
            Ok(Frame::Rollback { seq: 7, keep: 6 }),
            Ok(Frame::Rollback { seq: 8, keep: 7 }),
            Err(ModernError::Invalid("undecodable".into())),
        ]
        .into_iter()
        .collect();
        let mut stats = WorkerStats::default();
        skip_superseded(&mut frame, &queued, &mut stats);
        let Frame::Step { items, .. } = &frame else {
            panic!("step");
        };
        assert_eq!(items[0], item(7, 5));
        assert_eq!(items[1].error.as_deref(), Some(SUPERSEDED));
        assert!(items[1].hidden.is_empty() && items[1].pad == 0);
        assert_eq!(items[2], item(8, 6));
        assert_eq!((stats.skipped_items, stats.skipped_positions), (1, 1));
    }

    /// The serve loop itself: stale drafts, the rollback that discards them
    /// and the replacement are all queued before the stage looks at the
    /// first draft. It skips both drafts, computes the replacement exactly,
    /// and forwards every frame in order.
    #[test]
    fn a_stage_skips_work_a_queued_rollback_discards() {
        let c = synthetic::tiny_config(false, ExpertFormat::Int8Dyadic);
        let fresh = || {
            StageWorker::new(
                stage_model(&c, 0, c.n_layers, Router::Random),
                WorkerConfig::default(),
            )
            .unwrap()
        };
        let prefill = Frame::Step {
            id: 0,
            items: vec![Item::new(5, 0, 2, Selection::Rp64Argmax, vec![3, 9])],
        };
        let mut reference = fresh();
        let mut worker = fresh();
        for frame in [prefill, one(2, 4)] {
            reference.process(frame.clone()).unwrap();
            worker.process(frame).unwrap();
        }
        let expected = reference.process(one(3, 7)).unwrap();
        let (tx, rx) = channel();
        let rollback = Frame::Rollback { seq: 5, keep: 3 };
        for frame in [
            one(3, 40),
            one(4, 41),
            rollback.clone(),
            one(3, 7),
            Frame::Shutdown,
        ] {
            tx.send(frame.encode()).unwrap();
        }
        let mem = MemTransport::new();
        let mut out = mem.listen("skip-next").unwrap();
        let worker = serve_from(worker, rx, Arc::new(mem.clone()), "skip-next".into()).unwrap();
        let mut link = out.accept().unwrap();
        let got: Vec<Frame> = (0..5)
            .map(|_| Frame::decode(&link.recv().unwrap()).unwrap())
            .collect();
        for (frame, start) in got[..2].iter().zip([3, 4]) {
            let Frame::Step { items, .. } = frame else {
                panic!("step");
            };
            assert_eq!(items[0].start, start);
            assert_eq!(items[0].error.as_deref(), Some(SUPERSEDED));
            assert!(items[0].commits.is_empty());
        }
        assert_eq!(got[2], rollback);
        assert_eq!(got[3], expected);
        assert_eq!(got[4], Frame::Shutdown);
        assert_eq!(worker.stats.skipped_items, 2);
        assert_eq!(worker.stats.rolled_back_positions, 0);
        assert_eq!(worker.cached_positions(5), Some(4));
    }

    #[test]
    fn speculation_refuses_empty_shapes_and_repeated_ids() {
        let c = synthetic::tiny_config(false, ExpertFormat::Int8Dyadic);
        let mut island = ThreadIsland::start(&c, &[0, 4], Router::Random, |_, _, _| {});
        let reqs = spec_requests(&c, 2, 5);
        for (depth, rows) in [(0, 1), (1, 0)] {
            let config = SpecConfig {
                depth,
                rows,
                ..SpecConfig::default()
            };
            assert!(
                island
                    .coordinator
                    .run_speculative(&reqs, &config, &mut Silent)
                    .is_err()
            );
        }
        let twice = [reqs[0].clone(), reqs[0].clone()];
        assert!(
            island
                .coordinator
                .run_speculative(&twice, &SpecConfig::default(), &mut Silent)
                .is_err()
        );
        island.stop();
    }
}
