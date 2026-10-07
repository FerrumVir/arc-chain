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
use arc_inference::modern::mla::island::commit::{Verdict, audit_all};
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
        let verdicts = audit_all(&got.ledger, &revealed, &verifier_refs).unwrap();
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
