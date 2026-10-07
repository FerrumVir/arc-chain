//! Islands of separate `arc-island stage` processes joined by TCP on
//! 127.0.0.1: the acceptance tests of the island runtime.
//!
//! * 1, 2 and 4 processes and uneven splits give the single process's tokens,
//!   logits hashes and per-boundary commitments;
//! * a stage process killed with SIGKILL mid-generation restarts from its
//!   activation log and the run finishes byte-identically;
//! * a tampering stage process is blamed by re-execution, and a lying
//!   commitment breaks the link check;
//! * a stage whose routed experts live in another process (expert
//!   parallelism) gives the same bytes.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use arc_inference::modern::arith::Selection;
use arc_inference::modern::mla::config::MlaConfig;
use arc_inference::modern::mla::island::commit::{AuditContext, Verdict, audit_all, audit_stage};
use arc_inference::modern::mla::island::coordinator::{Completion, Request, Schedule};
use arc_inference::modern::mla::island::even_cuts;
use arc_inference::modern::mla::island::process::ProcessIsland;
use arc_inference::modern::mla::model::{MlaGeneration, StageModel};
use arc_inference::modern::mla::package::StageSpec;
use arc_inference::modern::mla::synthetic::{self, Router};
use arc_inference::modern::model::GenerationRequest;

const EXE: &str = env!("CARGO_BIN_EXE_arc-island");

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("arc-island-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn package(dir: &Path, shape: &str) -> (PathBuf, MlaConfig, StageModel) {
    let c = synthetic::shape(shape).unwrap();
    let path = dir.join(format!("{shape}.arcspkg"));
    synthetic::write_package(&c, StageSpec::full(&c), Router::Random, &path).unwrap();
    let model = StageModel::open(&path).unwrap();
    (path, c, model)
}

fn requests(c: &MlaConfig, count: usize) -> Vec<Request> {
    let mut s = 42u64;
    let mut next = || {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        s >> 33
    };
    (0..count)
        .map(|i| Request {
            id: 100 + i as u64,
            prompt: (0..1 + next() % 5)
                .map(|_| (next() % c.vocab_size as u64) as u32)
                .collect(),
            max_tokens: 2 + (next() % 7) as usize,
            eos: Vec::new(),
            selection: if i % 2 == 0 {
                Selection::Rp64Argmax
            } else {
                Selection::Argmax
            },
        })
        .collect()
}

fn reference(model: &StageModel, reqs: &[Request]) -> Vec<MlaGeneration> {
    reqs.iter()
        .map(|r| {
            model
                .generate(&GenerationRequest {
                    prompt: &r.prompt,
                    max_tokens: r.max_tokens,
                    eos: &r.eos,
                    selection: r.selection,
                })
                .unwrap()
        })
        .collect()
}

fn assert_matches(expected: &[MlaGeneration], done: &[Completion], at: &str) {
    assert_eq!(expected.len(), done.len());
    for (g, c) in expected.iter().zip(done) {
        assert_eq!(c.error, None, "{at} id {}", c.id);
        assert_eq!(c.tokens, g.tokens, "{at} id {}: tokens", c.id);
        assert_eq!(c.logits_hashes, g.logits_hashes, "{at} id {}: logits", c.id);
        assert_eq!(
            c.ledger.boundary_digests().as_ref(),
            Some(&g.boundary_digests),
            "{at} id {}: per-boundary commitments",
            c.id
        );
    }
}

fn schedule(micro_batches: usize, concurrency: usize) -> Schedule {
    Schedule {
        micro_batches,
        concurrency,
        ..Schedule::default()
    }
}

/// The acceptance test: 1, 2 and 4 processes and uneven splits.
#[test]
fn islands_of_one_two_and_four_processes_match_the_single_process() {
    let scratch = Scratch::new("splits");
    for shape in ["tiny-lora", "tiny-i4"] {
        let (path, c, model) = package(&scratch.0, shape);
        let reqs = requests(&c, 6);
        let expected = reference(&model, &reqs);
        let mut splits: Vec<Vec<usize>> = [1, 2, 4]
            .iter()
            .map(|&n| even_cuts(c.n_layers, n))
            .collect();
        splits.push(vec![0, 1, 4]);
        splits.push(vec![0, 3, 4]);
        for cuts in splits {
            let mut island =
                ProcessIsland::launch(Path::new(EXE), &path, &cuts, &c, |_| Vec::new()).unwrap();
            for (g, b) in [(1, 1), (2, 6), (4, 3)] {
                let (done, _) = island
                    .coordinator
                    .run(
                        &reqs,
                        &Schedule {
                            forget_finished: true,
                            ..schedule(g, b)
                        },
                    )
                    .unwrap();
                assert_matches(
                    &expected,
                    &done,
                    &format!("{shape} cuts {cuts:?} G {g} B {b}"),
                );
            }
            let stats = island.shutdown().unwrap();
            assert_eq!(stats.len(), cuts.len() - 1);
            for s in &stats {
                assert!(s["positions"].as_u64().unwrap() > 0, "{s}");
            }
        }
    }
}

/// A forced stage restart: SIGKILL one stage process mid-generation, start
/// it again on the same address, and finish the run. The new process replays
/// its activation log (checking every hash it committed before the crash),
/// the upstream stage reconnects, and every byte equals the uninterrupted
/// single process.
#[test]
fn a_killed_stage_restarts_from_its_log_and_finishes_byte_identically() {
    let scratch = Scratch::new("restart");
    let (path, c, model) = package(&scratch.0, "tiny-lora");
    let reqs = requests(&c, 3);
    let expected = reference(&model, &reqs);
    let logs = scratch.0.join("logs");
    let cuts = even_cuts(c.n_layers, 4);
    for victim in [0usize, 2, 3] {
        let dir = logs.join(format!("victim-{victim}"));
        let mut island = ProcessIsland::launch(Path::new(EXE), &path, &cuts, &c, |s| {
            vec![
                "--log-dir".into(),
                dir.join(format!("s{s}")).display().to_string(),
            ]
        })
        .unwrap();
        let mut restarted = None;
        let ProcessIsland {
            stages,
            coordinator,
        } = &mut island;
        let (done, _) = coordinator
            .run_with(&reqs, &schedule(1, 3), &mut |steps| {
                if steps == 3 {
                    stages[victim].kill();
                    stages[victim].restart()?;
                    restarted = Some(stages[victim].hello.clone());
                }
                Ok(())
            })
            .unwrap();
        let hello = restarted.expect("the stage was restarted");
        assert!(
            hello["replayed_positions"].as_u64().unwrap() > 0,
            "victim {victim}: {hello}"
        );
        assert_matches(&expected, &done, &format!("restart of stage {victim}"));
        island.shutdown().unwrap();
    }
}

/// A stage process that alters one output (and commits the altered hash) is
/// blamed at that position by re-executing each stage, from its revealed
/// log, with only that stage's weights; one that lies in its commitment
/// breaks the link check.
#[test]
fn tampered_and_lying_stage_processes_are_rejected() {
    let scratch = Scratch::new("tamper");
    let (path, c, model) = package(&scratch.0, "tiny-lora");
    let reqs = requests(&c, 2);
    let expected = reference(&model, &reqs);
    let victim = reqs[0].id;
    let cuts = vec![0, 1, 3, 4];
    let verifiers: Vec<StageModel> = cuts
        .windows(2)
        .map(|w| {
            StageModel::open_range(
                &path,
                Some(StageSpec {
                    first_layer: w[0],
                    end_layer: w[1],
                }),
            )
            .unwrap()
        })
        .collect();
    let verifier_refs: Vec<&StageModel> = verifiers.iter().collect();

    let mut island = ProcessIsland::launch(Path::new(EXE), &path, &cuts, &c, |s| {
        if s == 1 {
            vec!["--fault".into(), format!("tamper:{victim}:1")]
        } else {
            Vec::new()
        }
    })
    .unwrap();
    let (done, _) = island.coordinator.run(&reqs, &schedule(2, 2)).unwrap();
    for (r, got) in reqs.iter().zip(&done) {
        assert!(
            got.ledger.complete(),
            "a consistent lie passes the link check"
        );
        let revealed = island.coordinator.reveal(r.id).unwrap();
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
                        position: 1,
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
        // A log altered after the fact (the reveal of a separate process)
        // no longer hashes to what the stage committed.
        assert!(revealed[2].tokens.len() > 1);
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
    assert_matches(&expected[1..], &done[1..], "the honest neighbour");
    island.shutdown().unwrap();

    let mut island = ProcessIsland::launch(Path::new(EXE), &path, &cuts, &c, |s| {
        if s == 1 {
            vec!["--fault".into(), format!("lie:{victim}:0")]
        } else {
            Vec::new()
        }
    })
    .unwrap();
    let (done, _) = island.coordinator.run(&reqs, &schedule(1, 2)).unwrap();
    assert_eq!(done[0].tokens, expected[0].tokens, "the data was honest");
    let faults = &done[0].ledger.link_faults;
    assert_eq!(faults.len(), 1, "{faults:?}");
    assert_eq!((faults[0].position, faults[0].boundary), (0, 3));
    assert_eq!((faults[0].upstream, faults[0].downstream), ((1, 3), (3, 4)));
    assert_matches(&expected[1..], &done[1..], "the honest neighbour");
    island.shutdown().unwrap();
}

/// Expert parallelism across processes: the last stage's odd routed experts
/// live in an `arc-island experts` process.
#[test]
fn a_stage_with_experts_in_another_process_matches_the_single_process() {
    let scratch = Scratch::new("experts");
    let (path, c, model) = package(&scratch.0, "tiny-i4");
    let reqs = requests(&c, 4);
    let expected = reference(&model, &reqs);
    let cuts = vec![0, 2, 4];
    let mut server = Command::new(EXE)
        .args(["experts", "--package"])
        .arg(&path)
        .args(["--layers", "2:4", "--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut hello = String::new();
    BufReader::new(server.stdout.take().unwrap())
        .read_line(&mut hello)
        .unwrap();
    let hello: serde_json::Value = serde_json::from_str(hello.trim()).unwrap();
    let address = hello["listening"].as_str().unwrap().to_string();
    let mut island = ProcessIsland::launch(Path::new(EXE), &path, &cuts, &c, |s| {
        if s == 1 {
            vec![
                "--experts-at".into(),
                format!("local,{address}"),
                "--expert-device".into(),
                "0".into(),
            ]
        } else {
            Vec::new()
        }
    })
    .unwrap();
    let (done, _) = island.coordinator.run(&reqs, &schedule(2, 4)).unwrap();
    assert_matches(&expected, &done, "expert parallel");
    island.shutdown().unwrap();
    let _ = server.kill();
    let _ = server.wait();
}

/// A last-stage process that emits a token other than its sampler's: every
/// link agrees, and the audit's re-selection blames it at that position.
#[test]
fn a_wrong_token_from_the_last_stage_process_is_rejected() {
    let scratch = Scratch::new("wrong-token");
    let (path, c, model) = package(&scratch.0, "tiny-i4");
    let reqs = vec![
        Request {
            id: 7,
            prompt: vec![3, 17, 5],
            max_tokens: 5,
            eos: Vec::new(),
            selection: Selection::Rp64Argmax,
        },
        Request {
            id: 8,
            prompt: vec![41, 2],
            max_tokens: 4,
            eos: Vec::new(),
            selection: Selection::Argmax,
        },
    ];
    let expected = reference(&model, &reqs);
    let cuts = vec![0, 2, 4];
    let mut island = ProcessIsland::launch(Path::new(EXE), &path, &cuts, &c, |s| {
        if s == 1 {
            vec!["--fault".into(), "wrong-token:7:3".into()]
        } else {
            Vec::new()
        }
    })
    .unwrap();
    let (done, _) = island.coordinator.run(&reqs, &schedule(2, 2)).unwrap();
    assert!(done[0].ledger.complete(), "every hash is honest");
    assert_eq!(done[0].tokens[0], expected[0].tokens[0]);
    assert_ne!(done[0].tokens[1], expected[0].tokens[1]);
    let verifiers: Vec<StageModel> = cuts
        .windows(2)
        .map(|w| {
            StageModel::open_range(
                &path,
                Some(StageSpec {
                    first_layer: w[0],
                    end_layer: w[1],
                }),
            )
            .unwrap()
        })
        .collect();
    let verifier_refs: Vec<&StageModel> = verifiers.iter().collect();
    for (r, got) in reqs.iter().zip(&done) {
        let revealed = island.coordinator.reveal(r.id).unwrap();
        for ((a, b), verdict) in audit_all(
            &got.ledger,
            &revealed,
            &verifier_refs,
            &AuditContext::new(r, &got.tokens),
        )
        .unwrap()
        {
            if r.id == 7 && (a, b) == (2, 4) {
                assert_eq!(
                    verdict,
                    Verdict::WrongToken {
                        position: 3,
                        committed: got.tokens[1],
                        expected: expected[0].tokens[1],
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
    }
    assert_matches(&expected[1..], &done[1..], "the honest neighbour");
    island.shutdown().unwrap();
}

#[test]
fn regional_replicas_recover_lost_inflight_replies_with_heterogeneous_speeds() {
    use arc_inference::modern::mla::island::process::StageProcess;
    use arc_inference::modern::mla::island::replica::{
        ReplicatedStage, TransportConnector, stage_identity,
    };
    use arc_inference::modern::mla::island::transport::DeadlineTcpTransport;
    use arc_inference::modern::mla::island::wire::{Frame, Item};
    use arc_inference::modern::mla::island::worker::{StageWorker, WorkerConfig};
    use std::sync::Arc;
    use std::time::Duration;

    let scratch = Scratch::new("regional-replicas");
    let (path, c, _) = package(&scratch.0, "tiny-i4");
    let cuts = [0, 1, 3, 4];
    let mut children = Vec::new();
    let mut stages = Vec::new();
    let mut references = Vec::new();
    for (index, range) in cuts.windows(2).enumerate() {
        let spec = Some(StageSpec {
            first_layer: range[0],
            end_layer: range[1],
        });
        let model = StageModel::open_range(&path, spec).unwrap();
        let identity = stage_identity(&model);
        references.push(StageWorker::new(model, WorkerConfig::default()).unwrap());
        let mut endpoints = Vec::new();
        for primary in [true, false] {
            let args = vec![
                "--package".into(),
                path.display().to_string(),
                "--layers".into(),
                format!("{}:{}", range[0], range[1]),
                "--sessions".into(),
                "1".into(),
                "--drop-reply-at".into(),
                if primary { "2" } else { "0" }.into(),
                // Slow nodes are legitimate, not changed arithmetic.
                "--reply-delay-ms".into(),
                (index * 2).to_string(),
                "--threads".into(),
                "1".into(),
            ];
            let process =
                StageProcess::spawn_command(Path::new(EXE), "replica", args, "127.0.0.1:0")
                    .unwrap();
            endpoints.push(process.address.clone());
            children.push(process);
        }
        stages.push(
            ReplicatedStage::new(
                Arc::new(TransportConnector {
                    transport: Arc::new(DeadlineTcpTransport {
                        timeout: Duration::from_secs(5),
                    }),
                    stage_id: identity,
                }),
                endpoints,
                1 << 20,
            )
            .unwrap(),
        );
    }
    for position in 0..6 {
        let mut frame = Frame::Step {
            id: 0,
            items: [10, 11]
                .iter()
                .map(|&seq| Item::new(seq, position, 1, Selection::Rp64Argmax, vec![position + 3]))
                .collect(),
        };
        let mut expected = frame.clone();
        for (stage, reference) in stages.iter_mut().zip(&mut references) {
            frame = stage.process(frame).unwrap();
            expected = reference.process(expected).unwrap();
            assert_eq!(
                frame, expected,
                "every stage and every output byte after churn"
            );
        }
    }
    for stage in &mut stages {
        assert_eq!(stage.failovers, 1);
        assert_eq!(stage.replayed_frames, 1);
        stage.process(Frame::Shutdown).unwrap();
    }
    drop(stages);
    for child in children {
        child.finish().unwrap();
    }
    assert_eq!(c.n_layers, 4);
}

mod trusted_audit_cases {
    use arc_inference as inference;
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/audit_context_cases.rs"
    ));
}

#[test]
fn separate_process_metadata_attacks_require_trusted_context() {
    use arc_inference::modern::mla::island::process::StageProcess;
    use arc_inference::modern::mla::island::replica::{
        ReplicaConnector, TransportConnector, stage_identity,
    };
    use arc_inference::modern::mla::island::transport::DeadlineTcpTransport;
    use arc_inference::modern::mla::island::wire::Frame;
    use std::sync::Arc;
    use std::time::Duration;
    let scratch = Scratch::new("trusted-audits");
    let (path, _, _) = package(&scratch.0, "tiny-i4");
    let mut children = Vec::new();
    let mut sessions = Vec::new();
    for (a, b) in [(0, 2), (2, 4)] {
        let child = StageProcess::spawn_command(
            Path::new(EXE),
            "replica",
            vec![
                "--package".into(),
                path.display().to_string(),
                "--layers".into(),
                format!("{a}:{b}"),
                "--sessions".into(),
                "1".into(),
                "--threads".into(),
                "1".into(),
            ],
            "127.0.0.1:0",
        )
        .unwrap();
        let connector = TransportConnector {
            transport: Arc::new(DeadlineTcpTransport {
                timeout: Duration::from_secs(5),
            }),
            stage_id: stage_identity(&trusted_audit_cases::model(a, b)),
        };
        sessions.push(connector.connect(&child.address).unwrap());
        children.push(child);
    }
    trusted_audit_cases::exercise(|s, frame| {
        Frame::decode(&sessions[s].exchange(&frame.encode()).unwrap()).unwrap()
    });
    for session in &mut sessions {
        session.exchange(&Frame::Shutdown.encode()).unwrap();
    }
    drop(sessions);
    for child in children {
        child.finish().unwrap();
    }
}

mod relay_ring_cases {
    use super::*;
    include!("support/relay_ring_cases.rs");
}
