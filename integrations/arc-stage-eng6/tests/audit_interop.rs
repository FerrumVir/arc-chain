//! Current ARC-68 contract over real loopback sockets. No model downloads.
use std::sync::Arc;

use arc_inference_eng6::modern::{
    arith::Selection,
    mla::{
        config::ExpertFormat,
        island::{
            commit::{AuditContext, Verdict, audit_all},
            coordinator::{Completion, Coordinator, Request, Schedule},
            transport::{TcpTransport, Transport},
            wire::{Frame, Revealed, Writer},
            worker::{StageWorker, WorkerConfig, serve},
        },
        model::StageModel,
        package::StageSpec,
        synthetic::{self, Router},
    },
    model::GenerationRequest,
};
use arc_stage_eng6::TunedTransport;

fn model(first_layer: usize, end_layer: usize) -> StageModel {
    StageModel::from_owned(
        synthetic::package_bytes(
            &synthetic::tiny_config(true, ExpertFormat::Int4G32),
            StageSpec {
                first_layer,
                end_layer,
            },
            Router::Random,
        )
        .unwrap(),
    )
    .unwrap()
}

fn transport(tuned: bool) -> Arc<dyn Transport> {
    if tuned {
        Arc::new(TunedTransport::default())
    } else {
        Arc::new(TcpTransport)
    }
}

// Both listener/connector directions are exercised, alongside an all-native
// control. The original requests stay outside every worker and received frame.
fn generate(tuned: [bool; 3], requests: &[Request]) -> Vec<(Completion, Vec<Revealed>)> {
    let transports = tuned.map(transport);
    let listeners: Vec<_> = transports
        .iter()
        .map(|t| t.listen("127.0.0.1:0").unwrap())
        .collect();
    let addresses: Vec<_> = listeners.iter().map(|l| l.address()).collect();
    let mut listeners = listeners.into_iter();
    let coordinator_listener = listeners.next().unwrap();
    let workers: Vec<_> = listeners
        .enumerate()
        .map(|(i, listener)| {
            let worker =
                StageWorker::new(model(i * 2, (i + 1) * 2), WorkerConfig::default()).unwrap();
            let transport = transports[i + 1].clone();
            let next = addresses[(i + 2) % 3].clone();
            std::thread::spawn(move || serve(worker, listener, transport, next))
        })
        .collect();
    let mut coordinator = Coordinator::new(
        transports[0].clone(),
        addresses[1].clone(),
        coordinator_listener,
        synthetic::tiny_config(true, ExpertFormat::Int4G32),
    );
    let (done, _) = coordinator
        .run(
            requests,
            &Schedule {
                concurrency: 2,
                micro_batches: 2,
                prefill_chunk: 2,
                forget_finished: false,
                ..Schedule::default()
            },
        )
        .unwrap();
    let result = done
        .into_iter()
        .map(|completion| {
            let reveals = coordinator.reveal(completion.id).unwrap();
            (completion, reveals)
        })
        .collect();
    coordinator.shutdown().unwrap();
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    result
}

// Opaque links carry the exact bytes; the native frame decoder is the protocol
// boundary. Round trips include malformed payloads so rejection is tested after
// crossing mixed native/tuned TCP, not merely against an in-memory fixture.
fn cross_wire(bytes: &[u8], tuned_listener: bool) -> Vec<u8> {
    let server = transport(tuned_listener);
    let client = transport(!tuned_listener);
    let mut listener = server.listen("127.0.0.1:0").unwrap();
    let address = listener.address();
    let peer = std::thread::spawn(move || {
        let mut link = listener.accept().unwrap();
        let bytes = link.recv().unwrap();
        link.send(&bytes).unwrap();
    });
    let mut link = client.connect(&address).unwrap();
    link.send(bytes).unwrap();
    let returned = link.recv().unwrap();
    peer.join().unwrap();
    assert_eq!(returned, bytes);
    returned
}

#[test]
fn native_generation_and_verifier_owned_audits_survive_mixed_tcp() {
    let requests: Vec<_> = [Selection::Rp64Argmax, Selection::Argmax]
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
    let baseline = generate([false; 3], &requests);
    let verifiers = [model(0, 2), model(2, 4)];
    let refs: Vec<_> = verifiers.iter().collect();
    for tuned in [[true, false, true], [false, true, false]] {
        for ((got, revealed), (control, _)) in generate(tuned, &requests).into_iter().zip(&baseline)
        {
            let request = requests.iter().find(|r| r.id == got.id).unwrap();
            let reference = model(0, 4)
                .generate(&GenerationRequest {
                    prompt: &request.prompt,
                    max_tokens: request.max_tokens,
                    eos: &request.eos,
                    selection: request.selection,
                })
                .unwrap();
            assert_eq!(got.error, None);
            assert_eq!(got.tokens, reference.tokens);
            assert_eq!(got.logits_hashes, reference.logits_hashes);
            assert_eq!(got.logits_digest(), reference.logits_digest);
            assert_eq!(got.output_hash(), reference.output_hash);
            assert!(got.ledger.complete());
            assert_eq!(
                got.ledger.boundary_digests().unwrap(),
                reference.boundary_digests
            );
            assert_eq!(got.tokens, control.tokens);
            assert_eq!(got.logits_hashes, control.logits_hashes);
            for (first, end) in [(0, 2), (2, 4)] {
                assert_eq!(
                    got.ledger.stage_commits(first, end),
                    control.ledger.stage_commits(first, end)
                );
                assert_eq!(
                    got.ledger.stage_root(got.id, first, end),
                    control.ledger.stage_root(got.id, first, end)
                );
                assert!(got.ledger.stage_root(got.id, first, end).is_some());
            }
            let trusted = AuditContext::new(request, &got.tokens);
            let other = requests.iter().find(|r| r.id != got.id).unwrap();
            assert!(
                audit_all(
                    &got.ledger,
                    &revealed,
                    &refs,
                    &AuditContext::new(other, &got.tokens)
                )
                .unwrap()
                .iter()
                .all(|(_, v)| matches!(v, Verdict::RequestMismatch { field: "sequence" }))
            );
            assert!(
                audit_all(&got.ledger, &revealed, &refs, &trusted)
                    .unwrap()
                    .iter()
                    .all(|(_, v)| matches!(v, Verdict::Valid { .. }))
            );
            assert!(revealed.iter().all(|r| r.seq == request.id));
            let bytes = Frame::Reveal {
                seq: request.id,
                stages: revealed.clone(),
            }
            .encode();
            assert_eq!(bytes[0], 8);
            assert_eq!(
                Frame::decode(&cross_wire(&bytes, tuned[0])).unwrap(),
                Frame::Reveal {
                    seq: request.id,
                    stages: revealed.clone()
                }
            );

            // Each single-field substitution crosses the transport before audit.
            // The other stage stays honest and Valid; no prior mutation can mask
            // the current regression. Context never comes from the reveal.
            for stage in 0..2 {
                for field in [
                    "sequence",
                    "selection",
                    "prompt boundary",
                    "prompt tokens",
                    "history",
                ] {
                    let mut forged = revealed.clone();
                    match field {
                        "sequence" => forged[stage].seq += 1,
                        "selection" => {
                            forged[stage].selection = if request.selection == Selection::Argmax {
                                Selection::Rp64Argmax
                            } else {
                                Selection::Argmax
                            }
                        }
                        "prompt boundary" => forged[stage].prompt_len = 1,
                        "prompt tokens" => forged[stage].tokens[0] ^= 1,
                        "history" => forged[stage].tokens[3] ^= 1,
                        _ => unreachable!(),
                    }
                    let wire = Frame::Reveal {
                        seq: request.id,
                        stages: forged,
                    }
                    .encode();
                    let Frame::Reveal { stages, .. } =
                        Frame::decode(&cross_wire(&wire, tuned[0])).unwrap()
                    else {
                        panic!("reveal")
                    };
                    let verdicts = audit_all(&got.ledger, &stages, &refs, &trusted).unwrap();
                    let expected = if field == "history" {
                        Verdict::ForwardMismatch { position: 2 }
                    } else {
                        Verdict::RequestMismatch { field }
                    };
                    assert_eq!(verdicts[stage].1, expected);
                    assert!(matches!(verdicts[1 - stage].1, Verdict::Valid { .. }));
                }
            }
            for accepted in [&[][..], &got.tokens[..11]] {
                assert!(
                    audit_all(
                        &got.ledger,
                        &revealed,
                        &refs,
                        &AuditContext::new(request, accepted)
                    )
                    .unwrap()
                    .iter()
                    .all(|(_, v)| matches!(v, Verdict::Refused(_)))
                );
            }
            let mut final_changed = got.tokens.clone();
            *final_changed.last_mut().unwrap() ^= 1;
            assert_eq!(
                audit_all(
                    &got.ledger,
                    &revealed,
                    &refs,
                    &AuditContext::new(request, &final_changed)
                )
                .unwrap()[1]
                    .1,
                Verdict::ForwardMismatch { position: 13 }
            );
            assert!(audit_all(&got.ledger, &revealed[..1], &refs, &trusted).is_err());
            assert!(
                audit_all(
                    &got.ledger,
                    &[revealed[0].clone(), revealed[0].clone()],
                    &refs,
                    &trusted
                )
                .is_err()
            );
        }
    }
}

#[test]
fn legacy_and_malformed_reveals_fail_closed_after_native_transport() {
    let request = Request {
        id: 7,
        prompt: vec![0, 17, 5],
        max_tokens: 2,
        eos: vec![],
        selection: Selection::Rp64Argmax,
    };
    let (_, stages) = generate([true, false, true], &[request]).pop().unwrap();
    // Actual old layout: stage entries have no sequence field at all.
    let mut old = Writer::default();
    old.u8(3);
    old.u64(7);
    old.count(stages.len());
    for stage in &stages {
        old.u32(stage.first_layer);
        old.u32(stage.end_layer);
        old.u32(stage.prompt_len);
        old.selection(stage.selection);
        old.u32s(&stage.tokens);
        old.acts(&stage.inputs);
    }
    let bytes = Frame::Reveal { seq: 7, stages }.encode();
    assert_eq!(bytes[0], 8);
    for tuned_listener in [false, true] {
        let mut legacy = bytes.clone();
        legacy[0] = 3;
        let legacy_empty = [
            vec![3],
            7u64.to_le_bytes().to_vec(),
            0u32.to_le_bytes().to_vec(),
        ]
        .concat();
        let mut trailing = bytes.clone();
        trailing.push(0);
        for malformed in [legacy, legacy_empty, old.bytes.clone(), trailing] {
            assert!(Frame::decode(&cross_wire(&malformed, tuned_listener)).is_err());
        }
        // Every truncation of the nonempty sequence-bound Reveal is refused.
        // Use a single TCP connection for the entire corpus.
        let mut corpus = Vec::new();
        for n in 0..bytes.len() {
            corpus.extend_from_slice(&(n as u32).to_le_bytes());
            corpus.extend_from_slice(&bytes[..n]);
        }
        let returned = cross_wire(&corpus, tuned_listener);
        let mut at = 0;
        for n in 0..bytes.len() {
            assert_eq!(
                u32::from_le_bytes(returned[at..at + 4].try_into().unwrap()) as usize,
                n
            );
            at += 4;
            assert!(Frame::decode(&returned[at..at + n]).is_err(), "prefix {n}");
            at += n;
        }
    }
}
