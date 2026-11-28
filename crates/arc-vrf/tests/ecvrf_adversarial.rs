//! Black-box adversarial tests of the RFC 9381 ECVRF API: uniqueness,
//! forgery, malleability, wrong keys and inputs, and the small-order and
//! non-canonical public keys that `ECVRF_validate_key` must reject.

use arc_vrf::ecvrf::{OUTPUT_LENGTH, PROOF_LENGTH, SECRET_KEY_LENGTH};
use arc_vrf::{Proof, PublicKey, Purpose, SecretKey, VrfError, VrfInput};
use curve25519_dalek::constants::EIGHT_TORSION;
use curve25519_dalek::edwards::CompressedEdwardsY;
use serde_json::Value;

/// The eight small-order points and the two non-canonical encodings from
/// RFC 9381 Section 5.4.5, as committed data.
const SMALL_ORDER_VECTORS: &str = include_str!("vectors/edwards25519_small_order_points.json");

/// The group order `q` (RFC 8032 Section 5.1: `2^252 + 27742317777372353535851937790883648493`),
/// little-endian.
const GROUP_ORDER_LE: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
];

fn keypair(seed: u8) -> (SecretKey, PublicKey) {
    let sk = SecretKey::from_bytes([seed; SECRET_KEY_LENGTH]);
    let pk = sk.public_key();
    (sk, pk)
}

fn add_le(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut carry = 0u16;
    for ((digit, x), y) in out.iter_mut().zip(a).zip(b) {
        let sum = u16::from(*x) + u16::from(*y) + carry;
        *digit = (sum & 0xff) as u8;
        carry = sum >> 8;
    }
    assert_eq!(carry, 0, "sum must fit in 32 bytes");
    out
}

#[test]
fn prove_is_deterministic_and_the_output_is_unique_per_key_and_input() {
    let inputs: [&[u8]; 4] = [b"", b"a", b"leader election", &[0u8; 100]];
    for seed in 1..=8u8 {
        let (sk, pk) = keypair(seed);
        for alpha in inputs {
            let first = sk.prove(alpha).unwrap();
            let second = sk.prove(alpha).unwrap();
            assert_eq!(
                first.to_bytes(),
                second.to_bytes(),
                "prove is deterministic"
            );
            let beta_first = pk.verify(alpha, &first).unwrap();
            let beta_second = pk.verify(alpha, &second).unwrap();
            assert_eq!(
                beta_first, beta_second,
                "same (pk, alpha) maps to the same beta"
            );
            assert_eq!(first.to_hash(), beta_first);
            assert_eq!(beta_first.as_bytes().len(), OUTPUT_LENGTH);
            assert_eq!(beta_first.as_ref(), &beta_first.to_bytes()[..]);
        }
    }
}

#[test]
fn different_inputs_and_different_keys_give_different_outputs_and_proofs() {
    let (sk1, pk1) = keypair(1);
    let (sk2, pk2) = keypair(2);
    let alpha = b"same input";
    let proof1 = sk1.prove(alpha).unwrap();
    let proof2 = sk2.prove(alpha).unwrap();
    assert_ne!(proof1.to_bytes(), proof2.to_bytes());
    assert_ne!(
        pk1.verify(alpha, &proof1).unwrap(),
        pk2.verify(alpha, &proof2).unwrap()
    );

    let proof_other = sk1.prove(b"same input!").unwrap();
    assert_ne!(proof1.to_bytes(), proof_other.to_bytes());
    assert_ne!(
        pk1.verify(alpha, &proof1).unwrap(),
        pk1.verify(b"same input!", &proof_other).unwrap()
    );
}

#[test]
fn generated_keys_round_trip_and_verify() {
    let sk = SecretKey::generate(&mut rand::rngs::OsRng);
    let pk = sk.public_key();
    let pk_again = PublicKey::from_bytes(pk.to_bytes()).unwrap();
    assert_eq!(pk, pk_again);
    let alpha = b"generated";
    let proof = Proof::from_slice(&sk.prove(alpha).unwrap().to_bytes()).unwrap();
    assert!(pk_again.verify(alpha, &proof).is_ok());
    assert_eq!(SecretKey::from_bytes(sk.to_bytes()).public_key(), pk);
}

#[test]
fn wrong_public_key_and_wrong_input_are_rejected() {
    let (sk, pk) = keypair(3);
    let (_, other_pk) = keypair(4);
    let alpha = b"the right input";
    let proof = sk.prove(alpha).unwrap();
    assert!(pk.verify(alpha, &proof).is_ok());
    assert_eq!(
        other_pk.verify(alpha, &proof),
        Err(VrfError::VerificationFailed)
    );
    assert_eq!(
        pk.verify(b"the wrong input", &proof),
        Err(VrfError::VerificationFailed)
    );
    assert_eq!(
        pk.verify(b"the right input\0", &proof),
        Err(VrfError::VerificationFailed)
    );
    assert_eq!(pk.verify(b"", &proof), Err(VrfError::VerificationFailed));
}

#[test]
fn every_single_bit_flip_in_the_proof_is_rejected() {
    let (sk, pk) = keypair(5);
    let alpha = b"bit flips";
    let pi = sk.prove(alpha).unwrap().to_bytes();
    assert!(pk.verify(alpha, &Proof::from_bytes(&pi).unwrap()).is_ok());
    let mut decoded_but_rejected = 0usize;
    let mut rejected_at_decoding = 0usize;
    for (index, _) in pi.iter().enumerate() {
        for bit in 0..8u8 {
            let mut tampered = pi;
            tampered[index] ^= 1u8 << bit;
            match Proof::from_bytes(&tampered) {
                Err(VrfError::InvalidProof) => rejected_at_decoding += 1,
                Err(other) => panic!("byte {index} bit {bit}: unexpected error {other:?}"),
                Ok(proof) => {
                    assert_eq!(
                        pk.verify(alpha, &proof),
                        Err(VrfError::VerificationFailed),
                        "byte {index} bit {bit} verified"
                    );
                    decoded_but_rejected += 1;
                }
            }
        }
    }
    assert_eq!(
        decoded_but_rejected + rejected_at_decoding,
        PROOF_LENGTH * 8
    );
    // Flipping bits of c (bytes 32..48) always decodes, so at least those cases reached verify.
    assert!(decoded_but_rejected >= 16 * 8);
}

#[test]
fn proof_with_non_canonical_scalar_s_is_rejected() {
    // s + q encodes the same scalar with different bytes; ECVRF_decode_proof must
    // reject it (RFC 9381 Section 5.4.4 step 8), or proofs would be malleable.
    let (sk, pk) = keypair(6);
    let alpha = b"scalar malleability";
    let pi = sk.prove(alpha).unwrap().to_bytes();
    let mut s = [0u8; 32];
    s.copy_from_slice(&pi[48..]);
    let mut malleated = pi;
    malleated[48..].copy_from_slice(&add_le(&s, &GROUP_ORDER_LE));
    assert_ne!(malleated, pi);
    assert_eq!(Proof::from_bytes(&malleated), Err(VrfError::InvalidProof));
    assert!(pk.verify(alpha, &Proof::from_bytes(&pi).unwrap()).is_ok());
}

#[test]
fn proof_with_non_canonical_gamma_encoding_is_rejected() {
    // Gamma = identity, canonically encoded, decodes (and then fails to verify);
    // the same point encoded as y = 1 + p, or with the sign bit set on x = 0,
    // must be rejected at decoding.
    let (sk, pk) = keypair(7);
    let alpha = b"gamma encoding";
    let pi = sk.prove(alpha).unwrap().to_bytes();

    let mut canonical_identity = pi;
    canonical_identity[..32].copy_from_slice(&{
        let mut identity = [0u8; 32];
        identity[0] = 1;
        identity
    });
    let decoded = Proof::from_bytes(&canonical_identity).unwrap();
    assert_eq!(
        pk.verify(alpha, &decoded),
        Err(VrfError::VerificationFailed)
    );

    let mut non_canonical_identity = pi;
    non_canonical_identity[..32].copy_from_slice(&{
        let mut identity_plus_p = [0xffu8; 32];
        identity_plus_p[0] = 0xee;
        identity_plus_p[31] = 0x7f;
        identity_plus_p
    });
    assert_eq!(
        Proof::from_bytes(&non_canonical_identity),
        Err(VrfError::InvalidProof)
    );

    let mut negative_zero_x = canonical_identity;
    negative_zero_x[31] |= 0x80;
    assert_eq!(
        Proof::from_bytes(&negative_zero_x),
        Err(VrfError::InvalidProof)
    );
}

#[test]
fn gamma_shifted_by_a_small_order_point_is_rejected() {
    // Gamma + T for a torsion point T hashes to the same beta (the cofactor
    // multiplication in proof_to_hash clears T), which is exactly why the proof
    // must not verify: otherwise one input would have several valid proofs.
    let (sk, pk) = keypair(8);
    let alpha = b"torsion";
    let proof = sk.prove(alpha).unwrap();
    let pi = proof.to_bytes();
    let mut gamma_bytes = [0u8; 32];
    gamma_bytes.copy_from_slice(&pi[..32]);
    let gamma = CompressedEdwardsY(gamma_bytes).decompress().unwrap();
    for torsion in &EIGHT_TORSION[1..] {
        let shifted = (gamma + torsion).compress().to_bytes();
        let mut tampered = pi;
        tampered[..32].copy_from_slice(&shifted);
        let decoded = Proof::from_bytes(&tampered).expect("a valid point encoding decodes");
        assert_eq!(
            pk.verify(alpha, &decoded),
            Err(VrfError::VerificationFailed)
        );
        assert_eq!(
            decoded.to_hash(),
            proof.to_hash(),
            "cofactor clearing makes beta unique"
        );
    }
}

#[test]
fn small_order_and_non_canonical_public_keys_are_rejected() {
    let document: Value = serde_json::from_str(SMALL_ORDER_VECTORS).unwrap();
    let cases = document["cases"].as_array().unwrap();
    assert_eq!(
        cases.len(),
        14,
        "7 y-values from RFC 9381 Section 5.4.5, both sign bits"
    );
    for case in cases {
        let encoding: [u8; 32] = hex::decode(case["encoding"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let expected = match case["expected"].as_str().unwrap() {
            "small_order" => VrfError::SmallOrderPublicKey,
            "invalid_encoding" => VrfError::InvalidPublicKey,
            other => panic!("unknown expectation {other}"),
        };
        assert_eq!(
            PublicKey::from_bytes(encoding),
            Err(expected),
            "{}",
            case["description"].as_str().unwrap()
        );
    }
    // A point that is not on the curve at all.
    assert_eq!(
        PublicKey::from_bytes([2u8; 32]).unwrap_err(),
        VrfError::InvalidPublicKey
    );
}

#[test]
fn proof_length_must_be_exactly_80_bytes() {
    let (sk, _) = keypair(9);
    let pi = sk.prove(b"length").unwrap().to_bytes();
    assert!(Proof::from_slice(&pi).is_ok());
    assert_eq!(Proof::from_slice(&pi[..79]), Err(VrfError::InvalidProof));
    assert_eq!(
        Proof::from_slice(&[&pi[..], &[0u8][..]].concat()),
        Err(VrfError::InvalidProof)
    );
    assert_eq!(Proof::from_slice(&[]), Err(VrfError::InvalidProof));
}

#[test]
fn a_leader_election_proof_does_not_verify_for_any_other_purpose_epoch_or_slot() {
    let (sk, pk) = keypair(10);
    let base = VrfInput {
        purpose: Purpose::LeaderElection,
        chain_id: [1u8; 32],
        epoch: 42,
        slot: 7,
        epoch_seed: [2u8; 32],
    };
    let proof = sk.prove(&base.alpha()).unwrap();
    let beta = pk.verify(&base.alpha(), &proof).unwrap();

    let mut other_chain = base;
    other_chain.chain_id[31] ^= 1;
    let mut other_seed = base;
    other_seed.epoch_seed[0] ^= 1;
    let variants = [
        VrfInput {
            purpose: Purpose::CommitteeSampling,
            ..base
        },
        VrfInput {
            purpose: Purpose::EpochSeedContribution,
            ..base
        },
        VrfInput { epoch: 43, ..base },
        VrfInput { slot: 8, ..base },
        other_chain,
        other_seed,
    ];
    for variant in variants {
        assert_eq!(
            pk.verify(&variant.alpha(), &proof),
            Err(VrfError::VerificationFailed)
        );
        let own_proof = sk.prove(&variant.alpha()).unwrap();
        assert_ne!(pk.verify(&variant.alpha(), &own_proof).unwrap(), beta);
    }
}
