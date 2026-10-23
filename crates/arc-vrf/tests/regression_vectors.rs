//! ARC-specific regression vectors.
//!
//! These are not conformance vectors (RFC 9381 Appendix B.3, ERC-2333 and the
//! Ethereum BLS vectors are checked elsewhere); they pin ARC's own choices: the
//! leader-election `alpha` layout and the proof-of-possession bytes for fixed,
//! obviously public test keys. `tests/vectors/arc_regression_vectors.json` was
//! produced by the ignored `print_vectors` test in the CI run it names, and
//! `committed_regression_vectors_reproduce` fails if the implementation ever
//! drifts from it.

use arc_vrf::{BlsSecretKey, Purpose, SecretKey, VrfInput};
use serde_json::{Value, json};

const COMMITTED_VECTORS: &str = include_str!("vectors/arc_regression_vectors.json");

fn ecvrf_cases() -> Vec<serde_json::Value> {
    let mut cases = Vec::new();
    for seed in 1..=3u8 {
        let sk = SecretKey::from_bytes([seed; 32]);
        let pk = sk.public_key();
        for (purpose, epoch, slot) in [
            (Purpose::LeaderElection, 1u64, 0u64),
            (Purpose::LeaderElection, 7, 11),
            (Purpose::CommitteeSampling, 7, 0),
            (Purpose::EpochSeedContribution, 8, 0),
        ] {
            let input = VrfInput {
                purpose,
                chain_id: [0x11; 32],
                epoch,
                slot,
                epoch_seed: [0x22; 32],
            };
            let alpha = input.alpha();
            let proof = sk.prove(&alpha).unwrap();
            let beta = pk.verify(&alpha, &proof).unwrap();
            cases.push(json!({
                "sk": hex::encode(sk.to_bytes()),
                "pk": hex::encode(pk.to_bytes()),
                "purpose": format!("{purpose:?}"),
                "chain_id": hex::encode(input.chain_id),
                "epoch": epoch,
                "slot": slot,
                "epoch_seed": hex::encode(input.epoch_seed),
                "alpha": hex::encode(alpha),
                "pi": hex::encode(proof.to_bytes()),
                "beta": hex::encode(beta.to_bytes()),
            }));
        }
    }
    cases
}

fn bls_pop_cases() -> Vec<serde_json::Value> {
    (1..=3u8)
        .map(|seed| {
            let ikm = [seed; 32];
            let sk = BlsSecretKey::key_gen(&ikm, &[]).unwrap();
            let pk = sk.public_key();
            let pop = sk.prove_possession();
            assert!(pk.verify_possession(&pop));
            json!({
                "ikm": hex::encode(ikm),
                "key_info": "",
                "sk": hex::encode(sk.to_bytes()),
                "pk": hex::encode(pk.to_bytes()),
                "pop": hex::encode(pop.to_bytes()),
                "message": hex::encode(b"ARC"),
                "signature": hex::encode(sk.sign(b"ARC").to_bytes()),
            })
        })
        .collect()
}

fn hex_value(value: &Value) -> Vec<u8> {
    hex::decode(value.as_str().expect("hex string")).expect("valid hex")
}

fn hex32(value: &Value) -> [u8; 32] {
    hex_value(value).try_into().expect("32 bytes")
}

fn purpose(name: &str) -> Purpose {
    match name {
        "LeaderElection" => Purpose::LeaderElection,
        "CommitteeSampling" => Purpose::CommitteeSampling,
        "EpochSeedContribution" => Purpose::EpochSeedContribution,
        other => panic!("unknown purpose {other}"),
    }
}

#[test]
fn committed_regression_vectors_reproduce() {
    let document: Value = serde_json::from_str(COMMITTED_VECTORS).unwrap();
    let run_url = document["generated_by"]["run_url"].as_str().unwrap();
    assert!(run_url.starts_with("https://github.com/FerrumVir/arc-chain/actions/runs/"));

    let ecvrf = document["ecvrf_leader_election"].as_array().unwrap();
    assert_eq!(ecvrf.len(), 12);
    for case in ecvrf {
        let sk = SecretKey::from_bytes(hex32(&case["sk"]));
        let pk = sk.public_key();
        assert_eq!(pk.to_bytes(), hex32(&case["pk"]));
        let input = VrfInput {
            purpose: purpose(case["purpose"].as_str().unwrap()),
            chain_id: hex32(&case["chain_id"]),
            epoch: case["epoch"].as_u64().unwrap(),
            slot: case["slot"].as_u64().unwrap(),
            epoch_seed: hex32(&case["epoch_seed"]),
        };
        let alpha = input.alpha();
        assert_eq!(alpha, hex_value(&case["alpha"]));
        let proof = sk.prove(&alpha).unwrap();
        assert_eq!(proof.to_bytes().to_vec(), hex_value(&case["pi"]));
        let beta = pk.verify(&alpha, &proof).unwrap();
        assert_eq!(beta.to_bytes().to_vec(), hex_value(&case["beta"]));
    }

    let bls = document["bls_proof_of_possession"].as_array().unwrap();
    assert_eq!(bls.len(), 3);
    for case in bls {
        let ikm = hex_value(&case["ikm"]);
        let key_info = hex_value(&case["key_info"]);
        let sk = BlsSecretKey::key_gen(&ikm, &key_info).unwrap();
        assert_eq!(sk.to_bytes().to_vec(), hex_value(&case["sk"]));
        let pk = sk.public_key();
        assert_eq!(pk.to_bytes().to_vec(), hex_value(&case["pk"]));
        let pop = sk.prove_possession();
        assert_eq!(pop.to_bytes().to_vec(), hex_value(&case["pop"]));
        assert!(pk.verify_possession(&pop));
        let message = hex_value(&case["message"]);
        let signature = sk.sign(&message);
        assert_eq!(signature.to_bytes().to_vec(), hex_value(&case["signature"]));
        assert!(pk.verify(&message, &signature));
    }

    // The printer and the committed file must agree exactly, so regenerating
    // the file is a no-op unless the implementation changed.
    assert_eq!(json!(ecvrf_cases()), document["ecvrf_leader_election"]);
    assert_eq!(json!(bls_pop_cases()), document["bls_proof_of_possession"]);
}

#[test]
#[ignore = "prints the ARC regression vectors; run with --ignored --nocapture"]
fn print_vectors() {
    let document = json!({
        "description": "ARC regression vectors generated by crates/arc-vrf/tests/regression_vectors.rs",
        "ecvrf_leader_election": ecvrf_cases(),
        "bls_proof_of_possession": bls_pop_cases(),
    });
    println!("ARC_VRF_REGRESSION_VECTORS_BEGIN");
    println!("{}", serde_json::to_string_pretty(&document).unwrap());
    println!("ARC_VRF_REGRESSION_VECTORS_END");
}
