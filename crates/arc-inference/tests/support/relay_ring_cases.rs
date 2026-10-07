// Actual relay CLI processes in a coordinator ring, with owned finite replicas.
use arc_inference::modern::mla::island::{
    coordinator::Coordinator,
    process::StageProcess,
    replica::stage_identity,
    transport::{DeadlineTcpTransport, Transport},
};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct RelayRing {
    coordinator: Coordinator,
    children: Vec<StageProcess>,
    primaries: Vec<usize>,
}

impl RelayRing {
    fn launch(
        path: &Path,
        config: &MlaConfig,
        spares: bool,
        drop_at: usize,
        journal_mib: usize,
    ) -> Self {
        let transport: Arc<dyn Transport> = Arc::new(DeadlineTcpTransport {
            timeout: Duration::from_secs(3),
        });
        let listener = transport.listen("127.0.0.1:0").unwrap();
        let mut next = listener.address();
        let mut children = Vec::new();
        let mut primaries = Vec::new();
        for (a, b) in [(2, 4), (0, 2)] {
            let model = StageModel::open_range(
                path,
                Some(StageSpec {
                    first_layer: a,
                    end_layer: b,
                }),
            )
            .unwrap();
            let mut endpoints = Vec::new();
            for replica in 0..if spares { 2 } else { 1 } {
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
                        "--drop-reply-at".into(),
                        if replica == 0 { drop_at } else { 0 }.to_string(),
                        "--threads".into(),
                        "1".into(),
                    ],
                    "127.0.0.1:0",
                )
                .unwrap();
                endpoints.push(child.address.clone());
                if replica == 0 {
                    primaries.push(children.len());
                }
                children.push(child);
            }
            let relay = StageProcess::spawn_command(
                Path::new(EXE),
                "relay",
                vec![
                    "--next".into(),
                    next,
                    "--replicas".into(),
                    endpoints.join(","),
                    "--stage-id".into(),
                    hex::encode(stage_identity(&model)),
                    "--timeout-ms".into(),
                    "3000".into(),
                    "--journal-mib".into(),
                    journal_mib.to_string(),
                ],
                "127.0.0.1:0",
            )
            .unwrap();
            next = relay.address.clone();
            children.push(relay);
        }
        let mut coordinator = Coordinator::new(transport, next, listener, config.clone());
        coordinator.timeout = Duration::from_secs(5);
        Self {
            coordinator,
            children,
            primaries,
        }
    }

    fn shutdown(mut self) {
        self.coordinator.shutdown().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        for child in &mut self.children {
            loop {
                if let Some(status) = child.child.try_wait().unwrap() {
                    assert!(status.success());
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "child did not exit after Shutdown"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        for child in self.children {
            child.finish().unwrap();
        }
    }
}

#[test]
fn coordinator_ring_relay_cli_recovers_killed_sessions_and_lost_executed_replies() {
    let scratch = Scratch::new("relay-ring");
    let (path, c, whole) = package(&scratch.0, "tiny-i4");
    let reqs: Vec<_> = [Selection::Rp64Argmax, Selection::Argmax]
        .into_iter()
        .enumerate()
        .map(|(i, selection)| Request {
            id: 7 + i as u64,
            prompt: vec![0, 17, 5],
            max_tokens: 12,
            eos: vec![],
            selection,
        })
        .collect();
    let expected = reference(&whole, &reqs);
    let verifiers: Vec<_> = [(0, 2), (2, 4)]
        .into_iter()
        .map(|(a, b)| {
            StageModel::open_range(
                &path,
                Some(StageSpec {
                    first_layer: a,
                    end_layer: b,
                }),
            )
            .unwrap()
        })
        .collect();
    let refs: Vec<_> = verifiers.iter().collect();
    let schedule = Schedule {
        concurrency: 2,
        micro_batches: 1,
        prefill_chunk: 1,
        forget_finished: false,
        ..Schedule::default()
    };
    let mut control = RelayRing::launch(&path, &c, false, 0, 8);
    let (baseline, _) = control.coordinator.run(&reqs, &schedule).unwrap();
    assert_matches(&expected, &baseline, "honest relay control");
    control.shutdown();

    for lost_reply in [false, true] {
        // In the lost-reply case, both primaries execute frame 3, then exit
        // without replying. In the kill case, kill both sessions only after
        // two acknowledged frames at a quiescent boundary, before next input.
        let mut ring = RelayRing::launch(&path, &c, true, if lost_reply { 3 } else { 0 }, 8);
        let start = Instant::now();
        let mut killed = false;
        let (done, _) = ring
            .coordinator
            .run_with(&reqs, &schedule, &mut |steps| {
                if !lost_reply && !killed && steps == 2 {
                    for &i in &ring.primaries {
                        ring.children[i].kill();
                    }
                    killed = true;
                }
                Ok(())
            })
            .unwrap();
        assert!(lost_reply || killed);
        assert_matches(&expected, &done, "both streams after relay churn");
        for ((request, got), original) in reqs.iter().zip(&done).zip(&baseline) {
            assert!(got.ledger.complete());
            assert_eq!(got.tokens, original.tokens);
            assert_eq!(got.logits_hashes, original.logits_hashes);
            assert_eq!(got.logits_digest(), original.logits_digest());
            assert_eq!(got.output_hash(), original.output_hash());
            assert_eq!(
                got.ledger.boundary_digests(),
                original.ledger.boundary_digests()
            );
            for (a, b) in [(0, 2), (2, 4)] {
                assert_eq!(
                    got.ledger.stage_commits(a, b),
                    original.ledger.stage_commits(a, b)
                );
                assert_eq!(
                    got.ledger.stage_root(request.id, a, b),
                    original.ledger.stage_root(request.id, a, b)
                );
            }
            let revealed = ring.coordinator.reveal(request.id).unwrap();
            let verdicts = audit_all(
                &got.ledger,
                &revealed,
                &refs,
                &AuditContext::new(request, &got.tokens),
            )
            .unwrap();
            assert_eq!(verdicts.len(), 2);
            assert!(
                verdicts
                    .iter()
                    .all(|(_, v)| matches!(v, Verdict::Valid { .. }))
            );
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "bounded recovery and audit"
        );
        println!(
            "relay ring lost_reply={lost_reply}: exact tokens/logits/Ledger/roots and both audits; elapsed {:?}",
            start.elapsed()
        );
        // All primaries really exited; exact outputs after frame 3 therefore
        // require the CLI relay's spare-session replay path.
        for &i in &ring.primaries {
            assert!(ring.children[i].child.try_wait().unwrap().is_some());
        }
        if !lost_reply {
            // Killed primaries have nonzero exit codes; remove only these owned
            // already-reaped children before the clean shutdown assertion.
            for &i in ring.primaries.iter().rev() {
                ring.children.remove(i);
            }
        }
        ring.shutdown();
    }
}

#[test]
fn coordinator_ring_relay_exhaustion_is_terminal_even_for_smaller_frames() {
    let scratch = Scratch::new("relay-exhaustion");
    let (path, c, _) = package(&scratch.0, "tiny-i4");
    let reqs: Vec<_> = [7, 8]
        .into_iter()
        .map(|id| Request {
            id,
            prompt: vec![3],
            max_tokens: 4,
            eos: vec![],
            selection: Selection::Argmax,
        })
        .collect();
    for journal_exhaustion in [false, true] {
        let mut ring =
            RelayRing::launch(&path, &c, false, if journal_exhaustion { 0 } else { 2 }, 1);
        let schedule = Schedule {
            concurrency: 2,
            micro_batches: 1,
            // First two-stream frame fits; the second exceeds the 1 MiB
            // journal. No oversized network frame or model failure is needed.
            pad_bytes_per_position: if journal_exhaustion { 400_000 } else { 0 },
            ..Schedule::default()
        };
        let start = Instant::now();
        let error = ring
            .coordinator
            .run(&reqs, &schedule)
            .unwrap_err()
            .to_string();
        let reason = if journal_exhaustion {
            "journal capacity exceeded"
        } else {
            "replicas exhausted"
        };
        assert!(error.contains(reason), "{error}");
        // Smaller frames used to slip through after a journal-capacity refusal.
        // Neither control traffic nor a fresh request can produce success now.
        for _ in 0..2 {
            let refused = ring.coordinator.ping(0).unwrap_err().to_string();
            assert!(refused.contains("permanently refused"), "{refused}");
        }
        assert!(ring.coordinator.run(&reqs, &Schedule::default()).is_err());
        assert!(start.elapsed() < Duration::from_secs(20));
        println!(
            "relay ring terminal {reason}: {error}; small-frame/retry refused; elapsed {:?}",
            start.elapsed()
        );
        // Unrecoverable runs cannot circulate a successful Shutdown. Drop owns
        // and kills/reaps exactly these child processes; no crash recovery claim.
        drop(ring);
    }
}
