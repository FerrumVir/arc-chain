// Shared adversarial fixtures, executed with native workers and TCP processes.
// The including module supplies `inference` as either this crate or arc_inference.
use inference::modern::arith::Selection;
use inference::modern::mla::config::{ExpertFormat, MlaConfig};
use inference::modern::mla::island::commit::{
    AuditContext, Ledger, Verdict, audit_all, audit_stage,
};
use inference::modern::mla::island::coordinator::Request;
use inference::modern::mla::island::wire::{Frame, Item};
use inference::modern::mla::model::StageModel;
use inference::modern::mla::package::StageSpec;
use inference::modern::mla::synthetic::{self, Router};
use inference::modern::model::GenerationRequest;

pub fn config() -> MlaConfig {
    synthetic::tiny_config(true, ExpertFormat::Int4G32)
}

pub fn model(a: usize, b: usize) -> StageModel {
    StageModel::from_owned(
        synthetic::package_bytes(
            &config(),
            StageSpec {
                first_layer: a,
                end_layer: b,
            },
            Router::Random,
        )
        .unwrap(),
    )
    .unwrap()
}

pub fn exercise(mut process: impl FnMut(usize, Frame) -> Frame) {
    let c = config();
    let whole = model(0, 4);
    let verifiers = [model(0, 2), model(2, 4)];
    let refs: Vec<_> = verifiers.iter().collect();
    let requests: Vec<_> = [7, 8, 9]
        .iter()
        .map(|&id| Request {
            id,
            prompt: vec![0, 17, 5],
            max_tokens: 12,
            eos: vec![],
            selection: if id == 9 {
                Selection::Argmax
            } else {
                Selection::Rp64Argmax
            },
        })
        .collect();
    let expected = whole
        .generate(&GenerationRequest {
            prompt: &requests[0].prompt,
            max_tokens: 12,
            eos: &[],
            selection: Selection::Rp64Argmax,
        })
        .unwrap();
    let mut ledgers = std::array::from_fn::<_, 3, _>(|_| Ledger::new(c.n_layers));
    let mut outputs = [Vec::new(), Vec::new(), Vec::new()];
    let mut next = std::array::from_fn::<_, 3, _>(|i| requests[i].prompt.clone());
    let mut start = 0;
    for step in 0..12 {
        let n = next[0].len();
        let frame = Frame::Step {
            id: step,
            items: requests
                .iter()
                .enumerate()
                .map(|(i, r)| Item::new(r.id, start, 3, r.selection, next[i].clone()))
                .collect(),
        };
        let Frame::Step { mut items, .. } = process(0, frame) else {
            panic!("step");
        };
        // Exact independent-review reproducer: native head changes the rule,
        // with every activation/logit/commit still otherwise honest.
        items[0].selection = Selection::Argmax;
        let Frame::Step { items, .. } = process(1, Frame::Step { id: step, items }) else {
            panic!("step");
        };
        for (i, item) in items.iter().enumerate() {
            assert_eq!(item.error, None);
            ledgers[i].record(start as usize, n, &item.commits);
            let token = item.commits.last().unwrap().selected.unwrap();
            outputs[i].push(token);
            next[i] = vec![token];
        }
        start += n as u32;
    }
    assert_eq!(
        outputs[0],
        vec![28, 14, 14, 14, 14, 26, 2, 14, 28, 12, 3, 10]
    );
    assert_eq!(
        expected.tokens,
        vec![28, 14, 14, 14, 14, 26, 2, 12, 2, 32, 3, 10]
    );
    assert_eq!(outputs[1], expected.tokens, "honest neighbour unchanged");
    assert_eq!(outputs[2], outputs[0], "honest Argmax control");
    for i in 0..3 {
        let request = &requests[i];
        assert!(ledgers[i].complete());
        let first = process(
            0,
            Frame::Reveal {
                seq: request.id,
                stages: vec![],
            },
        );
        let Frame::Reveal { stages, .. } = process(1, first) else {
            panic!("reveal");
        };
        let trusted = AuditContext::new(request, &outputs[i]);
        let verdicts = audit_all(&ledgers[i], &stages, &refs, &trusted).unwrap();
        assert!(matches!(verdicts[0].1, Verdict::Valid { .. }));
        if i == 0 {
            assert_eq!(
                verdicts[1].1,
                Verdict::RequestMismatch { field: "selection" }
            );
            println!("native sampler substitution: {verdicts:?}");
            let mut restored = stages.clone();
            restored[1].selection = request.selection;
            assert_eq!(
                audit_all(&ledgers[i], &restored, &refs, &trusted).unwrap()[1].1,
                Verdict::WrongToken {
                    position: 9,
                    committed: 14,
                    expected: 12
                }
            );
        } else {
            assert!(
                verdicts
                    .iter()
                    .all(|(_, v)| matches!(v, Verdict::Valid { .. }))
            );
        }
        // Each attack is rejected before a self-consistent reveal can redefine
        // the request. It need not change activations or logits to be malicious.
        for field in [
            "sequence",
            "prompt boundary",
            "prompt tokens",
            "selection",
            "history",
        ] {
            let mut forged = stages.clone();
            // Isolate this attack from the victim's original sampler mismatch.
            forged[1].selection = request.selection;
            match field {
                "sequence" => forged[1].seq += 100,
                "prompt boundary" => forged[1].prompt_len = 1,
                "prompt tokens" => forged[1].tokens[0] ^= 1,
                "selection" => {
                    let other = if request.selection == Selection::Argmax {
                        Selection::Rp64Argmax
                    } else {
                        Selection::Argmax
                    };
                    forged[0].selection = other;
                    forged[1].selection = other;
                }
                "history" => {
                    forged[1].selection = request.selection;
                    forged[1].tokens[3] ^= 1;
                }
                _ => unreachable!(),
            }
            let verdicts = audit_all(&ledgers[i], &forged, &refs, &trusted).unwrap();
            let expected = if field == "history" {
                Verdict::ForwardMismatch { position: 2 }
            } else {
                Verdict::RequestMismatch { field }
            };
            assert_eq!(verdicts[1].1, expected, "{field}");
            assert!(
                !verdicts
                    .iter()
                    .all(|(_, v)| matches!(v, Verdict::Valid { .. })),
                "{field}"
            );
            println!("sequence {} forged {field}: {verdicts:?}", request.id);
        }
        for accepted in [&[][..], &outputs[i][..11]] {
            let absent = AuditContext::new(request, accepted);
            let verdicts = audit_all(&ledgers[i], &stages, &refs, &absent).unwrap();
            assert!(
                verdicts
                    .iter()
                    .all(|(_, v)| matches!(v, Verdict::Refused(_)))
            );
        }
        assert!(audit_all(&ledgers[i], &[], &refs, &trusted).is_err());
        assert!(audit_all(&ledgers[i], &stages[..1], &refs, &trusted).is_err());
        assert!(
            audit_all(
                &ledgers[i],
                &[stages[0].clone(), stages[0].clone()],
                &refs,
                &trusted
            )
            .is_err()
        );
        assert!(audit_all(&Ledger::new(4), &[], &refs, &trusted).is_err());
        if i == 1 {
            let mut commits = ledgers[i].stage_commits(2, 4).unwrap().to_vec();
            commits[0].logits.as_mut().unwrap()[0] ^= 1;
            assert_eq!(
                audit_stage(&verifiers[1], &stages[1], &commits, &trusted),
                Verdict::LogitsMismatch { position: 0 }
            );
            let mut commits = ledgers[i].stage_commits(2, 4).unwrap().to_vec();
            commits.last_mut().unwrap().selected = None;
            assert!(matches!(
                audit_stage(&verifiers[1], &stages[1], &commits, &trusted),
                Verdict::Refused(_)
            ));
            let mut inconsistent = outputs[i].clone();
            *inconsistent.last_mut().unwrap() ^= 1;
            let verdicts = audit_all(
                &ledgers[i],
                &stages,
                &refs,
                &AuditContext::new(request, &inconsistent),
            )
            .unwrap();
            assert_eq!(verdicts[1].1, Verdict::ForwardMismatch { position: 13 });
        }
    }
    // Incomplete prefill may be audited, but no output/history may be invented.
    let req = Request {
        id: 20,
        ..requests[0].clone()
    };
    let mut frame = Frame::Step {
        id: 20,
        items: vec![Item::new(req.id, 0, 3, req.selection, vec![0])],
    };
    for stage in 0..2 {
        frame = process(stage, frame);
    }
    let Frame::Step { items, .. } = frame else {
        panic!("step");
    };
    let mut ledger = Ledger::new(4);
    ledger.record(0, 1, &items[0].commits);
    let mut frame = Frame::Reveal {
        seq: req.id,
        stages: vec![],
    };
    for stage in 0..2 {
        frame = process(stage, frame);
    }
    let Frame::Reveal { stages, .. } = frame else {
        panic!("reveal");
    };
    assert!(
        audit_all(&ledger, &stages, &refs, &AuditContext::new(&req, &[]))
            .unwrap()
            .iter()
            .all(|(_, v)| matches!(v, Verdict::Valid { positions: 1 }))
    );
}
