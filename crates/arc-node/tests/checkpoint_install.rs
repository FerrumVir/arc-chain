use arc_consensus::view_change::{
    CheckpointEnvelope, CheckpointError, FinalityVote, FinalityVoteCollector, SnapshotIdentity,
    validator_set_hash,
};
use arc_consensus::{ConsensusDomain, STAKE_ARC, Validator, ValidatorSet};
use arc_crypto::{Hash256, KeyPair, hash_bytes};
use arc_state::StateDB;
use arc_state::snapshot::SnapshotPayload;
use arc_types::{Account, Address};

fn domain(tag: &[u8]) -> ConsensusDomain {
    ConsensusDomain::new(hash_bytes(tag), 1, 1)
}

fn committee(n: usize, salt: &str) -> (ValidatorSet, Vec<KeyPair>) {
    let keys: Vec<KeyPair> = (0..n)
        .map(|i| {
            KeyPair::from_ed25519_secret_bytes(
                &hash_bytes(format!("{salt}.validator.{i}").as_bytes()).0,
            )
        })
        .collect();
    let validators = keys
        .iter()
        .enumerate()
        .map(|(i, key)| Validator::new(key.address(), STAKE_ARC, i as u16).expect("valid"))
        .collect();
    (ValidatorSet::new(validators, 1), keys)
}

fn snapshot_bytes(height: u64) -> (Vec<u8>, Hash256, SnapshotPayload) {
    let mut payload = SnapshotPayload {
        height,
        accounts: vec![
            (
                hash_bytes(b"alice"),
                Account::new(hash_bytes(b"alice"), 900),
            ),
            (hash_bytes(b"bob"), Account::new(hash_bytes(b"bob"), 100)),
        ],
        ..Default::default()
    };
    payload.canonicalize();
    let (bytes, digest) = payload.encode();
    (bytes, digest, payload)
}

fn root_of(payload: &SnapshotPayload) -> Hash256 {
    let scratch = StateDB::with_genesis(&[]);
    scratch.install_durable_snapshot(payload);
    scratch.get_state_root()
}

fn certify(
    set: &ValidatorSet,
    keys: &[KeyPair],
    domain: &ConsensusDomain,
    height: u64,
    block: Hash256,
    state_root: Hash256,
) -> arc_consensus::view_change::FinalityCertificate {
    let set_hash = validator_set_hash(set);
    let tx_root = Hash256::ZERO;
    let mut collector = FinalityVoteCollector::new();
    let mut out = None;
    for key in keys {
        let vote =
            FinalityVote::sign(*domain, set_hash, height, block, state_root, tx_root, key).unwrap();
        if let Some(certificate) = collector.add(vote, domain, set).unwrap() {
            out = Some(certificate);
        }
    }
    out.expect("quorum reached")
}

fn envelope_for(
    set: &ValidatorSet,
    keys: &[KeyPair],
    domain: &ConsensusDomain,
    height: u64,
    state_root: Hash256,
    digest: Hash256,
) -> CheckpointEnvelope {
    CheckpointEnvelope {
        certificate: certify(
            set,
            keys,
            domain,
            height,
            hash_bytes(b"committed-block"),
            state_root,
        ),
        snapshot: SnapshotIdentity {
            height,
            state_root,
            digest,
        },
        anchor_hash: Hash256::ZERO,
        anchor_round: 0,
    }
}

#[test]
fn a_checkpoint_from_the_right_committee_verifies_and_installs() {
    let (set, keys) = committee(4, "install");
    let d = domain(b"arc.checkpoint.install");
    let (bytes, digest, payload) = snapshot_bytes(500);
    let root = root_of(&payload);
    let envelope = envelope_for(&set, &keys, &d, 500, root, digest);

    envelope
        .verify_payload(&bytes, &d, &set)
        .expect("a well-formed checkpoint from this committee must verify");

    let decoded = SnapshotPayload::decode(&bytes).expect("payload decodes");
    let state = StateDB::with_genesis(&[]);
    state.install_durable_snapshot(&decoded);
    assert_eq!(state.get_state_root(), envelope.snapshot.state_root);
    assert_eq!(state.height(), 500);
    assert_eq!(
        state.get_account(&hash_bytes(b"alice")).map(|a| a.balance),
        Some(900)
    );
}

#[test]
fn a_checkpoint_signed_by_a_different_committee_is_refused() {
    let (ours, _) = committee(4, "ours");
    let (theirs, their_keys) = committee(4, "theirs");
    let d = domain(b"arc.checkpoint.committee");
    let (bytes, digest, payload) = snapshot_bytes(500);
    let envelope = envelope_for(&theirs, &their_keys, &d, 500, root_of(&payload), digest);

    match envelope.verify_payload(&bytes, &d, &ours) {
        Err(CheckpointError::Certificate(_)) => {}
        other => panic!("a foreign committee must be refused, got {other:?}"),
    }
}

#[test]
fn a_checkpoint_for_a_different_chain_is_refused() {
    let (set, keys) = committee(4, "chain");
    let ours = domain(b"arc.chain.ours");
    let theirs = domain(b"arc.chain.theirs");
    let (bytes, digest, payload) = snapshot_bytes(500);
    let envelope = envelope_for(&set, &keys, &theirs, 500, root_of(&payload), digest);

    match envelope.verify_payload(&bytes, &ours, &set) {
        Err(CheckpointError::Certificate(_)) => {}
        other => panic!("a foreign chain domain must be refused, got {other:?}"),
    }
}

#[test]
fn payload_bytes_the_certificate_does_not_authorise_are_refused() {
    let (set, keys) = committee(4, "payload");
    let d = domain(b"arc.checkpoint.payload");
    let (bytes, digest, payload) = snapshot_bytes(500);
    let envelope = envelope_for(&set, &keys, &d, 500, root_of(&payload), digest);

    let (other_bytes, _, _) = snapshot_bytes(501);
    match envelope.verify_payload(&other_bytes, &d, &set) {
        Err(CheckpointError::DigestMismatch { .. }) => {}
        other => panic!("substituted bytes must be refused, got {other:?}"),
    }

    let mut tampered = bytes.clone();
    let middle = tampered.len() / 2;
    tampered[middle] ^= 0xff;
    assert!(matches!(
        envelope.verify_payload(&tampered, &d, &set),
        Err(CheckpointError::DigestMismatch { .. })
    ));
}

#[test]
fn a_certificate_below_quorum_authorises_nothing() {
    let (set, keys) = committee(7, "quorum");
    let d = domain(b"arc.checkpoint.quorum");
    let (bytes, digest, payload) = snapshot_bytes(500);
    let root = root_of(&payload);

    let mut certificate = certify(&set, &keys, &d, 500, hash_bytes(b"committed-block"), root);
    certificate.votes.truncate(1);
    let envelope = CheckpointEnvelope {
        certificate,
        snapshot: SnapshotIdentity {
            height: 500,
            state_root: root,
            digest,
        },
        anchor_hash: Hash256::ZERO,
        anchor_round: 0,
    };
    match envelope.verify_payload(&bytes, &d, &set) {
        Err(CheckpointError::Certificate(_)) => {}
        other => panic!("a sub-quorum certificate must be refused, got {other:?}"),
    }
}

#[test]
fn an_envelope_naming_a_state_root_the_payload_does_not_produce_is_caught_on_install() {
    let (set, keys) = committee(4, "root");
    let d = domain(b"arc.checkpoint.root");
    let (bytes, digest, _) = snapshot_bytes(500);
    let lying_root = hash_bytes(b"a-root-this-payload-does-not-produce");
    let envelope = envelope_for(&set, &keys, &d, 500, lying_root, digest);

    envelope
        .verify_payload(&bytes, &d, &set)
        .expect("the envelope is internally consistent");

    let decoded = SnapshotPayload::decode(&bytes).expect("decodes");
    let state = StateDB::with_genesis(&[]);
    state.install_durable_snapshot(&decoded);
    assert_ne!(
        state.get_state_root(),
        envelope.snapshot.state_root,
        "the post-install state-root check is what makes this detectable"
    );
}

#[test]
fn an_envelope_whose_certificate_covers_a_different_height_is_refused() {
    let (set, keys) = committee(4, "height");
    let d = domain(b"arc.checkpoint.height");
    let (bytes, digest, payload) = snapshot_bytes(500);
    let root = root_of(&payload);
    let mut envelope = envelope_for(&set, &keys, &d, 500, root, digest);
    envelope.snapshot.height = 900;

    match envelope.verify_payload(&bytes, &d, &set) {
        Err(CheckpointError::HeightMismatch { expected, found }) => {
            assert_eq!(expected, 500);
            assert_eq!(found, 900);
        }
        other => panic!("a height mismatch must be refused, got {other:?}"),
    }
}

#[test]
fn installing_a_checkpoint_replaces_state_wholesale_not_partially() {
    let (bytes, _, _) = snapshot_bytes(500);
    let decoded = SnapshotPayload::decode(&bytes).expect("decodes");

    let stale: Address = hash_bytes(b"stale-account");
    let state = StateDB::with_genesis(&[(stale, 12_345)]);
    assert_eq!(state.get_account(&stale).map(|a| a.balance), Some(12_345));

    state.install_durable_snapshot(&decoded);
    assert_eq!(
        state.get_account(&hash_bytes(b"alice")).map(|a| a.balance),
        Some(900),
        "the checkpoint's accounts must be present"
    );
    let root_after = state.get_state_root();
    let clean = StateDB::with_genesis(&[]);
    clean.install_durable_snapshot(&decoded);
    assert_ne!(
        root_after,
        clean.get_state_root(),
        "installing over existing state does not produce the checkpoint's root, \
         which is exactly what the node's post-install root check detects"
    );
}
