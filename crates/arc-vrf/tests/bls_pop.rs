//! BLS12-381 proof-of-possession tests: key generation against the ERC-2333
//! vectors, signature plumbing against the Ethereum `bls12-381-tests` vectors,
//! proof-of-possession semantics, and the rogue-key attack with its defeat.

use arc_vrf::bls_pop::{
    BLS_PUBLIC_KEY_LENGTH, BLS_SIGNATURE_LENGTH, MIN_IKM_LENGTH, POP_DST, aggregate_enrolled,
    aggregate_signatures,
};
use arc_vrf::{
    BlsPopError, BlsPublicKey, BlsSecretKey, BlsSignature, EnrolledBlsKey, ProofOfPossession,
};
use blst::min_pk::{AggregatePublicKey, PublicKey as RawPublicKey, SecretKey as RawSecretKey};
use serde_json::Value;

/// ERC-2333 test cases 0 to 3 (`derive_master_SK` = IETF `KeyGen` with empty `key_info`).
const ERC2333_VECTORS: &str = include_str!("vectors/erc2333_keygen.json");
/// Selected cases from ethereum/bls12-381-tests v0.1.2 (CC0-1.0).
const ETHEREUM_VECTORS: &str = include_str!("vectors/ethereum_bls12_381_tests.json");

fn hex_bytes(value: &Value) -> Vec<u8> {
    let text = value.as_str().expect("hex string");
    hex::decode(text.trim_start_matches("0x")).expect("valid hex")
}

fn array<const N: usize>(bytes: Vec<u8>) -> [u8; N] {
    bytes
        .try_into()
        .unwrap_or_else(|v: Vec<u8>| panic!("expected {N} bytes, got {}", v.len()))
}

/// Deterministic, obviously public test keying material.
fn test_key(seed: u8) -> BlsSecretKey {
    BlsSecretKey::key_gen(&[seed; MIN_IKM_LENGTH], &[]).unwrap()
}

#[test]
fn key_gen_rejects_short_ikm_and_accepts_32_bytes() {
    for len in [0usize, 1, 16, 31] {
        assert_eq!(
            BlsSecretKey::key_gen(&vec![1u8; len], &[]).unwrap_err(),
            BlsPopError::ShortIkm(len)
        );
    }
    assert!(BlsSecretKey::key_gen(&[1u8; 32], &[]).is_ok());
    assert!(BlsSecretKey::key_gen(&[1u8; 64], b"key info").is_ok());
}

#[test]
fn key_gen_matches_the_erc2333_master_key_vectors() {
    let document: Value = serde_json::from_str(ERC2333_VECTORS).unwrap();
    let cases = document["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 4);
    for case in cases {
        let seed = hex_bytes(&case["seed"]);
        let expected: [u8; 32] = array(hex_bytes(&case["master_sk_be"]));
        let sk = BlsSecretKey::key_gen(&seed, &[]).unwrap();
        assert_eq!(
            sk.to_bytes(),
            expected,
            "ERC-2333 test case {}",
            case["case"]
        );
        assert_eq!(
            BlsSecretKey::from_bytes(&expected).unwrap().to_bytes(),
            expected
        );
    }
}

#[test]
fn key_gen_is_deterministic_and_key_info_separates_keys() {
    let a = BlsSecretKey::key_gen(&[9u8; 32], &[]).unwrap();
    let b = BlsSecretKey::key_gen(&[9u8; 32], &[]).unwrap();
    let c = BlsSecretKey::key_gen(&[9u8; 32], b"other").unwrap();
    assert_eq!(a.to_bytes(), b.to_bytes());
    assert_ne!(a.to_bytes(), c.to_bytes());
    assert_eq!(a.public_key(), b.public_key());
    assert_ne!(a.public_key(), c.public_key());
}

#[test]
fn secret_key_bytes_must_be_a_non_zero_scalar_below_the_order() {
    assert_eq!(
        BlsSecretKey::from_bytes(&[0u8; 32]).unwrap_err(),
        BlsPopError::InvalidSecretKey
    );
    assert_eq!(
        BlsSecretKey::from_bytes(&[0xffu8; 32]).unwrap_err(),
        BlsPopError::InvalidSecretKey
    );
    let mut one = [0u8; 32];
    one[31] = 1;
    assert!(BlsSecretKey::from_bytes(&one).is_ok());
}

#[test]
fn ethereum_sign_vectors_reproduce_byte_exactly() {
    let document: Value = serde_json::from_str(ETHEREUM_VECTORS).unwrap();
    let cases = document["sign"].as_array().unwrap();
    assert!(cases.len() >= 4);
    for case in cases {
        let privkey: [u8; 32] = array(hex_bytes(&case["input"]["privkey"]));
        let message = hex_bytes(&case["input"]["message"]);
        match case["output"].as_str() {
            None => {
                assert_eq!(
                    BlsSecretKey::from_bytes(&privkey).unwrap_err(),
                    BlsPopError::InvalidSecretKey,
                    "{}",
                    case["name"]
                );
            }
            Some(_) => {
                let expected: [u8; BLS_SIGNATURE_LENGTH] = array(hex_bytes(&case["output"]));
                let sk = BlsSecretKey::from_bytes(&privkey).unwrap();
                let signature = sk.sign(&message);
                assert_eq!(signature.to_bytes(), expected, "{}", case["name"]);
                assert!(sk.public_key().verify(&message, &signature));
                assert_eq!(BlsSignature::from_bytes(&expected).unwrap(), signature);
            }
        }
    }
}

#[test]
fn ethereum_verify_vectors_agree() {
    let document: Value = serde_json::from_str(ETHEREUM_VECTORS).unwrap();
    let cases = document["verify"].as_array().unwrap();
    assert!(cases.len() >= 5);
    for case in cases {
        let expected = case["output"].as_bool().unwrap();
        let pubkey = hex_bytes(&case["input"]["pubkey"]);
        let message = hex_bytes(&case["input"]["message"]);
        let signature = hex_bytes(&case["input"]["signature"]);
        let verified = match (
            BlsPublicKey::from_bytes(&array::<BLS_PUBLIC_KEY_LENGTH>(pubkey)),
            BlsSignature::from_bytes(&array::<BLS_SIGNATURE_LENGTH>(signature)),
        ) {
            (Ok(pk), Ok(sig)) => pk.verify(&message, &sig),
            _ => false,
        };
        assert_eq!(verified, expected, "{}", case["name"]);
    }
}

#[test]
fn ethereum_g1_deserialization_vectors_agree() {
    let document: Value = serde_json::from_str(ETHEREUM_VECTORS).unwrap();
    let cases = document["deserialization_G1"].as_array().unwrap();
    assert!(cases.len() >= 10);
    for case in cases {
        let decodes = case["output"].as_bool().unwrap();
        let bytes = hex_bytes(&case["input"]["pubkey"]);
        // Pure decoding (what the vector specifies), straight from blst.
        assert_eq!(
            RawPublicKey::uncompress(&bytes).is_ok(),
            decodes,
            "{}",
            case["name"]
        );
        // ARC's constructor additionally rejects the identity, so it accepts a
        // strict subset of what decodes.
        let arc_accepts = case["arc_from_bytes_ok"].as_bool().unwrap();
        let result = if bytes.len() == BLS_PUBLIC_KEY_LENGTH {
            BlsPublicKey::from_bytes(&array::<BLS_PUBLIC_KEY_LENGTH>(bytes)).is_ok()
        } else {
            false
        };
        assert_eq!(result, arc_accepts, "{}", case["name"]);
        assert!(!arc_accepts || decodes);
    }
}

#[test]
fn a_valid_proof_of_possession_verifies_and_enrolls() {
    let sk = test_key(1);
    let pk = sk.public_key();
    let pop = sk.prove_possession();
    assert!(pk.verify_possession(&pop));
    assert_eq!(sk.prove_possession(), pop, "PopProve is deterministic");
    let decoded = ProofOfPossession::from_bytes(pop.as_bytes()).unwrap();
    assert_eq!(decoded, pop);
    let enrolled = EnrolledBlsKey::enroll(pk, &decoded).unwrap();
    assert_eq!(*enrolled.public_key(), pk);
}

#[test]
fn a_proof_of_possession_from_another_key_fails() {
    let alice = test_key(2);
    let bob = test_key(3);
    let bob_pop = bob.prove_possession();
    assert!(bob.public_key().verify_possession(&bob_pop));
    assert!(!alice.public_key().verify_possession(&bob_pop));
    assert_eq!(
        EnrolledBlsKey::enroll(alice.public_key(), &bob_pop).unwrap_err(),
        BlsPopError::InvalidProofOfPossession
    );
}

#[test]
fn a_proof_of_possession_and_a_message_signature_are_not_interchangeable() {
    // Wrong DST in both directions: the message tag and the proof tag differ.
    let sk = test_key(4);
    let pk = sk.public_key();

    // A message signature over the public key bytes is not a proof of possession.
    let signature_over_pk = sk.sign(pk.as_bytes());
    let as_proof = ProofOfPossession::from_bytes(signature_over_pk.as_bytes()).unwrap();
    assert!(!pk.verify_possession(&as_proof));
    assert_eq!(
        EnrolledBlsKey::enroll(pk, &as_proof).unwrap_err(),
        BlsPopError::InvalidProofOfPossession
    );

    // A proof of possession is not a message signature over the public key bytes.
    let pop = sk.prove_possession();
    let as_signature = BlsSignature::from_bytes(pop.as_bytes()).unwrap();
    assert!(!pk.verify(pk.as_bytes(), &as_signature));
    assert_ne!(pop.to_bytes(), signature_over_pk.to_bytes());
}

#[test]
fn every_single_bit_flip_in_a_proof_of_possession_is_rejected() {
    let sk = test_key(5);
    let pk = sk.public_key();
    let pop = sk.prove_possession().to_bytes();
    // Every byte takes a while through pairings; one bit per byte keeps the test fast.
    for (index, bit) in (0..BLS_SIGNATURE_LENGTH).zip((0..8u8).cycle()) {
        let mut tampered = pop;
        tampered[index] ^= 1u8 << bit;
        match ProofOfPossession::from_bytes(&tampered) {
            Err(BlsPopError::InvalidSignature) => {}
            Err(other) => panic!("byte {index} bit {bit}: unexpected error {other:?}"),
            Ok(proof) => assert!(
                !pk.verify_possession(&proof),
                "byte {index} bit {bit} verified"
            ),
        }
    }
}

#[test]
fn identity_and_malformed_public_keys_and_signatures_are_rejected() {
    // The identity in G1: compressed infinity flag set (0xc0), everything else zero.
    let mut g1_infinity = [0u8; BLS_PUBLIC_KEY_LENGTH];
    g1_infinity[0] = 0xc0;
    assert_eq!(
        BlsPublicKey::from_bytes(&g1_infinity).unwrap_err(),
        BlsPopError::InvalidPublicKey
    );
    // Compression flag missing.
    assert_eq!(
        BlsPublicKey::from_bytes(&[0u8; BLS_PUBLIC_KEY_LENGTH]).unwrap_err(),
        BlsPopError::InvalidPublicKey
    );
    // The identity in G2.
    let mut g2_infinity = [0u8; BLS_SIGNATURE_LENGTH];
    g2_infinity[0] = 0xc0;
    assert_eq!(
        BlsSignature::from_bytes(&g2_infinity).unwrap_err(),
        BlsPopError::InvalidSignature
    );
    assert_eq!(
        ProofOfPossession::from_bytes(&g2_infinity).unwrap_err(),
        BlsPopError::InvalidSignature
    );
    assert_eq!(
        BlsSignature::from_bytes(&[0xffu8; BLS_SIGNATURE_LENGTH]).unwrap_err(),
        BlsPopError::InvalidSignature
    );
}

#[test]
fn aggregation_of_enrolled_keys_verifies_and_rejects_empty_input() {
    let keys: Vec<BlsSecretKey> = (10..14u8).map(test_key).collect();
    let enrolled: Vec<EnrolledBlsKey> = keys
        .iter()
        .map(|sk| EnrolledBlsKey::enroll(sk.public_key(), &sk.prove_possession()).unwrap())
        .collect();
    let message = b"finality vote for block 1234";
    let signatures: Vec<BlsSignature> = keys.iter().map(|sk| sk.sign(message)).collect();

    let aggregate_key = aggregate_enrolled(&enrolled).unwrap();
    let aggregate_signature = aggregate_signatures(&signatures).unwrap();
    assert!(aggregate_key.verify(message, &aggregate_signature));
    assert!(!aggregate_key.verify(b"a different message", &aggregate_signature));
    // A signature missing from the aggregate fails.
    let partial = aggregate_signatures(&signatures[1..]).unwrap();
    assert!(!aggregate_key.verify(message, &partial));
    // A key missing from the aggregate fails.
    let partial_key = aggregate_enrolled(&enrolled[1..]).unwrap();
    assert!(!partial_key.verify(message, &aggregate_signature));

    assert_eq!(
        aggregate_enrolled(&[]).unwrap_err(),
        BlsPopError::EmptyAggregate
    );
    assert_eq!(
        aggregate_signatures(&[]).unwrap_err(),
        BlsPopError::EmptyAggregate
    );
    assert_eq!(
        aggregate_enrolled(&enrolled[..1]).unwrap(),
        *enrolled[0].public_key()
    );
}

#[test]
fn the_rogue_key_attack_works_without_proofs_of_possession_and_fails_with_them() {
    let victim = test_key(20);
    let attacker = test_key(21);
    let pk_victim = victim.public_key();
    let pk_attacker = attacker.public_key();

    // The attacker publishes pk_rogue = pk_attacker - pk_victim, a perfectly
    // valid-looking subgroup point whose discrete logarithm nobody knows.
    let raw_attacker = RawPublicKey::from_bytes(pk_attacker.as_bytes()).unwrap();
    let raw_victim = RawPublicKey::from_bytes(pk_victim.as_bytes()).unwrap();
    let mut rogue = AggregatePublicKey::from_public_key(&raw_attacker);
    rogue.sub_aggregate(&AggregatePublicKey::from_public_key(&raw_victim));
    let rogue_bytes = rogue.to_public_key().compress();
    let pk_rogue = BlsPublicKey::from_bytes(&rogue_bytes).unwrap();
    assert_ne!(pk_rogue, pk_attacker);
    assert_ne!(pk_rogue, pk_victim);

    // 1. Without proofs of possession the attack succeeds: the naive aggregate of
    //    (pk_rogue, pk_victim) is pk_attacker, so the attacker's lone signature
    //    passes as a two-party aggregate signature that the victim never made.
    let raw_rogue = RawPublicKey::from_bytes(&rogue_bytes).unwrap();
    let naive_aggregate = AggregatePublicKey::aggregate(&[&raw_rogue, &raw_victim], true)
        .unwrap()
        .to_public_key()
        .compress();
    assert_eq!(naive_aggregate, pk_attacker.to_bytes());
    let message = b"slash the victim";
    let forged = attacker.sign(message);
    assert!(
        BlsPublicKey::from_bytes(&naive_aggregate)
            .unwrap()
            .verify(message, &forged),
        "the rogue-key forgery verifies against the naive aggregate"
    );

    // 2. With proofs of possession the rogue key cannot be enrolled. Every proof
    //    the attacker can produce fails PopVerify for pk_rogue:
    //    a. the attacker's own (valid) proof is for pk_attacker, not pk_rogue;
    let own_proof = attacker.prove_possession();
    assert!(pk_attacker.verify_possession(&own_proof));
    assert!(!pk_rogue.verify_possession(&own_proof));
    //    b. signing the rogue key's bytes under the proof tag with the attacker's key;
    let raw_attacker_sk = RawSecretKey::from_bytes(&attacker.to_bytes()).unwrap();
    let signed_rogue_bytes = raw_attacker_sk.sign(&rogue_bytes, POP_DST, &[]).compress();
    let forged_proof = ProofOfPossession::from_bytes(&signed_rogue_bytes).unwrap();
    assert!(!pk_rogue.verify_possession(&forged_proof));
    //    c. replaying the victim's published proof.
    let victim_proof = victim.prove_possession();
    assert!(!pk_rogue.verify_possession(&victim_proof));
    for proof in [&own_proof, &forged_proof, &victim_proof] {
        assert_eq!(
            EnrolledBlsKey::enroll(pk_rogue, proof).unwrap_err(),
            BlsPopError::InvalidProofOfPossession
        );
    }

    // 3. Honest enrolment and aggregation still work, and the forgery does not
    //    verify against the honest aggregate.
    let enrolled_victim = EnrolledBlsKey::enroll(pk_victim, &victim_proof).unwrap();
    let enrolled_attacker = EnrolledBlsKey::enroll(pk_attacker, &own_proof).unwrap();
    let honest_aggregate = aggregate_enrolled(&[enrolled_victim, enrolled_attacker]).unwrap();
    let honest_signature =
        aggregate_signatures(&[victim.sign(message), attacker.sign(message)]).unwrap();
    assert!(honest_aggregate.verify(message, &honest_signature));
    assert!(!honest_aggregate.verify(message, &forged));
}
