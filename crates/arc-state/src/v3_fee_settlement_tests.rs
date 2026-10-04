//! Fixtures for protocol-v3 block-level fee settlement.
//!
//! `v3_launch_rule_goldens` pins the block hashes, state roots, treasury
//! effects and exact state-WAL operation stream of a short recovered-v3
//! history executed under the launch rule, where every transfer credits the
//! shared fee treasury itself. The pinned values were recorded by CI on a
//! commit whose execution and admission code was unmodified `main`
//! (4070114c). Any later change must reproduce them byte for byte whenever
//! block-level fee settlement is not active.
//!
//! `reference_greedy_selection` is a verbatim copy of the quadratic greedy
//! loop the consensus proposal and commit paths used on that commit. It is the
//! oracle for the incremental admission builder that replaces it.

use super::*;
use arc_crypto::KeyPair;

const FIXTURE_CHAIN_ID: &str = "0x415243";
const FIXTURE_GENESIS: &[u8] = b"arc-v3-fee-settlement-fixture-genesis";
const FIXTURE_TIMESTAMP_MS: u64 = 1_790_000_000_000;

/// Launch-rule transcript recorded on unmodified `main` logic: printed by CI
/// run 37232853669 (commit a76309a1, identical on ubuntu-latest and
/// macos-15) before any fee-settlement code existed. See the module
/// documentation; never edit these lines by hand to make a change pass.
const V3_LAUNCH_RULE_GOLDENS: &[&str] = &[
    "h1 block=b3cf52a1ec680112eca34d2c69d3748640ac097cce75c97230aec6128f6ae311 root=37b02b247c092466e57fb82ef3ca2f4cf85d5aab400a47ff825bec3dc396c629 txs=1 ok=1",
    "h2 block=f58dc06a33449d456b2e1a9fe3c85525bf99d398c82cd32637951374af46d43f root=37b02b247c092466e57fb82ef3ca2f4cf85d5aab400a47ff825bec3dc396c629 txs=0 ok=",
    "h3 block=ca2f23736b51b5c887f472ff1a374dfe267917d9597ef3d93ba9ca7dafc7e9b6 root=dbac698695bff798df3aa1283ff4366132d9af226b949c65ee2e223cea6896f8 txs=1 ok=1",
    "h4 block=d814b6eef245150d855dc4bfbe20671ab82dc37d40338abaf2898a6cb42e2793 root=4866a3ea9657751e5aceddcef3074f4a850173843639206fc786b498ba0ee867 txs=1 ok=1",
    "h5 block=35afa6ef0939721764d58102e9338dc23373c4578ed01284448c1db94642fe7d root=554f3c8971afba630f5f853b6655fa3006a10f836d5a4a967768b42b1e781042 txs=1 ok=1",
    "h6 block=a8cf21bd357ce72511d60a6550c2aa8ecbe19b40d6bb72d35a7faf23a9092437 root=554f3c8971afba630f5f853b6655fa3006a10f836d5a4a967768b42b1e781042 txs=0 ok=",
    "h7 block=a41bfeb2ac7947974a8ec7f37d2c95bca4d66236f71a45a3d46842524bc82319 root=194879e12a875891d05e9b62b539bf433a378e2eb498180ad627fbe0eeb86abb txs=1 ok=1",
    "treasury balance=111 history=5",
    "sender0 balance=1000266 nonce=2",
    "sender1 balance=999743 nonce=1",
    "sender2 balance=999998 nonce=1",
    "sender3 balance=999498 nonce=1",
    "select 497979821be51b1dd22792e8b6a19fd1cf359b3d8d24a8684dbff6cc41f317f4",
    "defer 506f4711754b68c3ed1c56f360ea8dc89ec2f185a5761d4ae9507c510e7d230b",
    "defer 506f4711754b68c3ed1c56f360ea8dc89ec2f185a5761d4ae9507c510e7d230b",
    "defer 60c15d7087d76671981254c2387032a839493e2489070409c1a85225e86ecd31",
    "defer b0b10e71bf4e0efbc44fc8411f385ab08fec39b27c6cbcd256fdc4ef0c976560",
    "defer dc1877c1fa875485e3daf3a43067697b6d2759c31b844dc9da8367710bf070a2",
    "defer fc9cd8746f1c2bcfc1d93c09a770d20a4a0642f556ca7aee5f81f5e32ac2f8e2",
    "wal=0fe3e6090190579f0df4296d8287447cea4cc3e3a1ec4ec1c04fc9bd10b2635b",
];

fn fixture_key(label: &str, index: u64) -> KeyPair {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-v3-fee-settlement-fixture-key-v1");
    hasher.update(label.as_bytes());
    hasher.update(&index.to_le_bytes());
    KeyPair::from_ed25519_secret_bytes(hasher.finalize().as_bytes())
}

fn fixture_address(label: &str, index: u64) -> Address {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-v3-fee-settlement-fixture-address-v1");
    hasher.update(label.as_bytes());
    hasher.update(&index.to_le_bytes());
    Hash256(*hasher.finalize().as_bytes())
}

fn fixture_dir(name: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "arc-state-v3-fees-{name}-{}-{unique}",
        std::process::id()
    ))
}

/// Bind a state to the fixed fixture recovery domain, which makes it execute
/// and admit under protocol v3 exactly as a recovered chain does.
fn bind_fixture_recovery_domain(state: &StateDB) {
    *state.recovery_context.write() = Some(recovery::RecoveryContext::new(
        FIXTURE_CHAIN_ID,
        hash_bytes(FIXTURE_GENESIS),
        1,
        1,
    ));
}

fn v3_fixture_state(prefunded: &[(Address, u64)]) -> StateDB {
    let state = StateDB::with_genesis(prefunded);
    bind_fixture_recovery_domain(&state);
    state
}

fn signed_transfer(
    state: &StateDB,
    from: &KeyPair,
    to: Address,
    amount: u64,
    fee: u64,
    nonce: u64,
) -> Transaction {
    let mut transaction = Transaction::new_transfer(from.address(), to, amount, nonce);
    transaction.fee = fee;
    state
        .sign_transaction(&mut transaction, from)
        .expect("fixture transfer signs in the recovery domain");
    transaction
}

fn fixture_decision(height: u64) -> Hash256 {
    hash_bytes(format!("arc-v3-fee-settlement-fixture-decision-{height}").as_bytes())
}

/// The canonical consensus commit path with a deterministic timestamp.
fn commit_adaptive(
    state: &StateDB,
    transactions: &[Transaction],
    producer: Address,
) -> (Block, Vec<TxReceipt>) {
    let height = state.height() + 1;
    state
        .execute_block_adaptive_at_with_proof(
            transactions,
            producer,
            FIXTURE_TIMESTAMP_MS + height,
            fixture_decision(height),
        )
        .expect("fixture block executes")
}

fn commit_verified(
    state: &StateDB,
    transactions: &[Transaction],
    producer: Address,
) -> (Block, Vec<TxReceipt>) {
    let height = state.height() + 1;
    state
        .execute_block_verified_at(transactions, producer, FIXTURE_TIMESTAMP_MS + height)
        .expect("fixture block executes")
}

fn record_block(transcript: &mut Vec<String>, block: &Block, receipts: &[TxReceipt]) {
    let outcomes: String = receipts
        .iter()
        .map(|receipt| if receipt.success { '1' } else { '0' })
        .collect();
    transcript.push(format!(
        "h{} block={} root={} txs={} ok={outcomes}",
        block.header.height,
        block.hash.to_hex(),
        block.header.state_root.to_hex(),
        block.header.tx_count,
    ));
}

/// Digest of every state-WAL operation (with its block height) in order.
fn wal_digest(directory: &Path) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-v3-fee-settlement-fixture-wal-v1");
    for entry in read_wal(directory.join("state.wal")) {
        hasher.update(&entry.block_height.to_le_bytes());
        hasher.update(&bincode::serialize(&entry.op).expect("WAL operation serializes"));
    }
    hasher.finalize().to_hex().to_string()
}

/// Reference copy of the pre-builder greedy v3 selection in
/// `arc-node/src/consensus.rs` (proposal loop 4137-4155, commit loop
/// 4550-4568 on main 4070114c). A complete valid candidate is kept as is;
/// otherwise every transaction is retried against the admitted prefix and
/// either admitted or (proposal) deferred / (commit) omitted. Quadratic in
/// the candidate count, including signature verification: tests only.
fn reference_greedy_selection(
    state: &StateDB,
    candidates: &[Transaction],
) -> (Vec<Transaction>, Vec<Transaction>) {
    if state.validate_v3_block_admission(candidates).is_ok() {
        return (candidates.to_vec(), Vec::new());
    }
    let mut admitted: Vec<Transaction> = Vec::with_capacity(candidates.len());
    let mut deferred = Vec::new();
    for transaction in candidates {
        let mut candidate = admitted.clone();
        candidate.push(transaction.clone());
        if state.validate_v3_block_admission(&candidate).is_ok() {
            admitted.push(transaction.clone());
        } else {
            deferred.push(transaction.clone());
        }
    }
    (admitted, deferred)
}

fn render_transcript(transcript: &[String]) -> String {
    transcript
        .iter()
        .map(|line| format!("    \"{line}\","))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_matches_goldens(transcript: &[String], goldens: &[&str]) {
    let rendered = render_transcript(transcript);
    assert!(
        !goldens.is_empty(),
        "record these v3 launch-rule goldens from unmodified main logic:\n{rendered}"
    );
    assert_eq!(
        transcript, goldens,
        "the v3 launch-rule history changed; actual transcript:\n{rendered}"
    );
}

/// Execute the launch-rule fixture history on a fresh persistent recovered-v3
/// state. `configure` runs after the recovery domain is bound and before the
/// first block.
fn launch_rule_transcript(configure: impl FnOnce(&StateDB)) -> Vec<String> {
    let directory = fixture_dir("launch-rule");
    let senders: Vec<KeyPair> = (0..4)
        .map(|index| fixture_key("launch-sender", index))
        .collect();
    let sender_addresses: Vec<Address> = senders.iter().map(KeyPair::address).collect();
    let existing = fixture_address("launch-existing", 0);
    let mut prefunded: Vec<(Address, u64)> = sender_addresses
        .iter()
        .map(|address| (*address, 1_000_000))
        .collect();
    prefunded.push((existing, 5));
    let state =
        StateDB::with_genesis_persistent(&prefunded, &directory, hash_bytes(FIXTURE_GENESIS))
            .expect("fixture state opens");
    bind_fixture_recovery_domain(&state);
    configure(&state);
    let producer = fixture_key("launch-producer", 0).address();
    let recipient = |index| fixture_address("launch-recipient", index);
    let mut transcript = Vec::new();

    let first = signed_transfer(&state, &senders[0], recipient(0), 100, 1, 0);
    let (block, receipts) = commit_adaptive(&state, std::slice::from_ref(&first), producer);
    record_block(&mut transcript, &block, &receipts);

    let (block, receipts) = commit_adaptive(&state, &[], producer);
    record_block(&mut transcript, &block, &receipts);

    let to_existing = signed_transfer(&state, &senders[1], existing, 250, 7, 0);
    let (block, receipts) = commit_verified(&state, std::slice::from_ref(&to_existing), producer);
    record_block(&mut transcript, &block, &receipts);

    let second_from_first = signed_transfer(&state, &senders[0], recipient(1), 33, 100, 1);
    let (block, receipts) =
        commit_adaptive(&state, std::slice::from_ref(&second_from_first), producer);
    record_block(&mut transcript, &block, &receipts);

    // Disjoint transfers still share the fee treasury under the launch rule,
    // and a shared recipient conflicts under every rule. Neither candidate
    // block may mutate anything.
    let disjoint = signed_transfer(&state, &senders[2], recipient(2), 1, 1, 0);
    let other_disjoint = signed_transfer(&state, &senders[3], recipient(3), 1, 1, 0);
    let shared_recipient = signed_transfer(&state, &senders[3], recipient(2), 1, 1, 0);
    state.wal.sync().expect("fixture WAL syncs");
    let before = (
        state.height(),
        state.get_state_root(),
        std::fs::metadata(directory.join("state.wal"))
            .expect("fixture WAL exists")
            .len(),
    );
    for candidate in [
        vec![disjoint.clone(), other_disjoint],
        vec![disjoint.clone(), shared_recipient],
    ] {
        assert!(state.validate_v3_block_admission(&candidate).is_err());
        let height = state.height() + 1;
        assert!(
            state
                .execute_block_adaptive_at_with_proof(
                    &candidate,
                    producer,
                    FIXTURE_TIMESTAMP_MS + height,
                    fixture_decision(height),
                )
                .is_err()
        );
        state.wal.sync().expect("fixture WAL syncs");
        assert_eq!(
            (
                state.height(),
                state.get_state_root(),
                std::fs::metadata(directory.join("state.wal"))
                    .expect("fixture WAL exists")
                    .len(),
            ),
            before,
            "a refused v3 block left a trace"
        );
    }

    let (block, receipts) = commit_adaptive(&state, std::slice::from_ref(&disjoint), producer);
    record_block(&mut transcript, &block, &receipts);

    let (block, receipts) = commit_verified(&state, &[], producer);
    record_block(&mut transcript, &block, &receipts);

    let to_sender = signed_transfer(&state, &senders[3], sender_addresses[0], 500, 2, 0);
    let (block, receipts) = commit_adaptive(&state, std::slice::from_ref(&to_sender), producer);
    record_block(&mut transcript, &block, &receipts);

    let treasury = v3_fee_treasury_address();
    transcript.push(format!(
        "treasury balance={} history={}",
        state
            .get_account(&treasury)
            .map(|account| account.balance)
            .unwrap_or_default(),
        state.get_account_txs(&treasury.0).len(),
    ));
    for (index, address) in sender_addresses.iter().enumerate() {
        let account = state.get_account(address).expect("fixture sender exists");
        transcript.push(format!(
            "sender{index} balance={} nonce={}",
            account.balance, account.nonce
        ));
    }

    // The consensus selection over one hash-ordered candidate batch: a shared
    // recipient, a second spend by one sender, a below-minimum fee and an
    // exact duplicate next to ordinary transfers.
    let duplicated = signed_transfer(&state, &senders[1], recipient(11), 10, 3, 1);
    let mut candidates = vec![
        signed_transfer(&state, &senders[0], recipient(10), 10, 1, 2),
        duplicated.clone(),
        signed_transfer(&state, &senders[2], recipient(10), 10, 1, 1),
        signed_transfer(&state, &senders[0], recipient(12), 10, 1, 2),
        signed_transfer(&state, &senders[3], recipient(13), 10, 0, 1),
        duplicated,
        signed_transfer(&state, &senders[3], recipient(14), 10, 4, 1),
    ];
    candidates.sort_by_key(|transaction| transaction.hash.0);
    let (admitted, deferred) = reference_greedy_selection(&state, &candidates);
    assert_eq!(admitted.len(), 1, "the launch rule admits one transfer");
    for transaction in &admitted {
        transcript.push(format!("select {}", transaction.hash.to_hex()));
    }
    for transaction in &deferred {
        transcript.push(format!("defer {}", transaction.hash.to_hex()));
    }

    state.wal.sync().expect("fixture WAL syncs");
    drop(state);
    transcript.push(format!("wal={}", wal_digest(&directory)));
    std::fs::remove_dir_all(&directory).expect("fixture directory is removable");
    transcript
}

#[test]
fn v3_launch_rule_goldens() {
    let transcript = launch_rule_transcript(|_| {});
    assert_matches_goldens(&transcript, V3_LAUNCH_RULE_GOLDENS);
}

#[test]
fn reference_greedy_selection_keeps_one_launch_rule_transfer_per_block() {
    let senders: Vec<KeyPair> = (0..3)
        .map(|index| fixture_key("reference-sender", index))
        .collect();
    let state = v3_fixture_state(
        &senders
            .iter()
            .map(|key| (key.address(), 1_000))
            .collect::<Vec<_>>(),
    );
    let mut candidates: Vec<Transaction> = senders
        .iter()
        .enumerate()
        .map(|(index, key)| {
            signed_transfer(
                &state,
                key,
                fixture_address("reference-recipient", index as u64),
                5,
                1,
                0,
            )
        })
        .collect();
    candidates.sort_by_key(|transaction| transaction.hash.0);
    let (admitted, deferred) = reference_greedy_selection(&state, &candidates);
    assert_eq!(admitted.len(), 1, "the fee treasury admits one transfer");
    assert_eq!(admitted[0].hash, candidates[0].hash, "first in hash order");
    assert_eq!(deferred.len(), candidates.len() - 1);
    state
        .validate_v3_block_admission(&admitted)
        .expect("the reference selection is a valid block");
}

/// Deterministic pseudo-random stream for differential candidates.
struct FixtureRng {
    seed: u64,
    counter: u64,
}

impl FixtureRng {
    fn new(seed: u64) -> Self {
        Self { seed, counter: 0 }
    }

    fn draw(&mut self) -> u64 {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-v3-fee-settlement-fixture-rng-v1");
        hasher.update(&self.seed.to_le_bytes());
        hasher.update(&self.counter.to_le_bytes());
        self.counter += 1;
        let digest = hasher.finalize();
        let mut word = [0u8; 8];
        word.copy_from_slice(&digest.as_bytes()[..8]);
        u64::from_le_bytes(word)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.draw() % bound as u64) as usize
    }
}

/// Funded senders, the six-member recovery authority (faucet signers) and a
/// funded faucet pool on a recovered-v3 state.
struct DifferentialFixture {
    state: StateDB,
    senders: Vec<KeyPair>,
    sender_addresses: Vec<Address>,
    validators: Vec<KeyPair>,
}

fn differential_fixture() -> DifferentialFixture {
    let senders: Vec<KeyPair> = (0..8)
        .map(|index| fixture_key("differential-sender", index))
        .collect();
    let sender_addresses: Vec<Address> = senders.iter().map(KeyPair::address).collect();
    let validators: Vec<KeyPair> = (0..6)
        .map(|index| fixture_key("differential-validator", index))
        .collect();
    let validator_addresses: Vec<Address> = validators.iter().map(KeyPair::address).collect();
    let mut prefunded: Vec<(Address, u64)> = sender_addresses
        .iter()
        .map(|address| (*address, 1_000_000))
        .collect();
    prefunded.extend(validator_addresses.iter().map(|address| (*address, 1_000)));
    prefunded.push((
        arc_types::transaction::faucet_pool_address(),
        100 * arc_types::transaction::FAUCET_CLAIM_MAX,
    ));
    let state = v3_fixture_state(&prefunded);
    state.seed_genesis_validators(
        &validator_addresses
            .iter()
            .map(|address| (*address, 5_000_000))
            .collect::<Vec<_>>(),
    );
    DifferentialFixture {
        state,
        senders,
        sender_addresses,
        validators,
    }
}

/// A seeded candidate batch mixing admissible transfers and faucet claims
/// with every kind of conflict and refusal the selection has to resolve.
fn differential_candidates(
    fixture: &DifferentialFixture,
    seed: u64,
    count: usize,
) -> Vec<Transaction> {
    let state = &fixture.state;
    let mut rng = FixtureRng::new(seed);
    let shared: Vec<Address> = (0..3)
        .map(|index| fixture_address("differential-shared", seed * 16 + index))
        .collect();
    let mut candidates: Vec<Transaction> = Vec::with_capacity(count);
    for index in 0..count {
        let fresh = fixture_address("differential-fresh", seed * 10_000 + index as u64);
        let roll = rng.below(100);
        let transaction = if roll < 8 && !candidates.is_empty() {
            let earlier = rng.below(candidates.len());
            candidates[earlier].clone()
        } else if roll < 20 {
            let validator = &fixture.validators[rng.below(fixture.validators.len())];
            let recipient = if rng.below(2) == 0 {
                shared[rng.below(shared.len())]
            } else {
                fresh
            };
            let amount = 1 + rng.below(10) as u64;
            let mut claim =
                Transaction::new_faucet_claim(validator.address(), recipient, amount, 0);
            state
                .sign_transaction(&mut claim, validator)
                .expect("fixture faucet claim signs");
            claim
        } else {
            let sender = rng.below(fixture.senders.len());
            let recipient = match rng.below(10) {
                0..=4 => fresh,
                5..=7 => shared[rng.below(shared.len())],
                8 => fixture.sender_addresses[rng.below(fixture.sender_addresses.len())],
                _ => v3_fee_treasury_address(),
            };
            let fee = rng.below(8) as u64;
            let nonce = u64::from(rng.below(10) == 0);
            let amount = 1 + rng.below(50) as u64;
            signed_transfer(
                state,
                &fixture.senders[sender],
                recipient,
                amount,
                fee,
                nonce,
            )
        };
        candidates.push(transaction);
    }
    candidates
}

fn hashes_of(transactions: &[Transaction]) -> Vec<Hash256> {
    transactions
        .iter()
        .map(|transaction| transaction.hash)
        .collect()
}

/// Offer `candidates` to one builder and to the reference greedy loop; both
/// must admit and refuse exactly the same transactions in the same order.
fn assert_builder_matches_reference(state: &StateDB, candidates: &[Transaction]) -> usize {
    let (expected, expected_refused) = reference_greedy_selection(state, candidates);
    let mut admission = state
        .v3_block_admission()
        .expect("next height is representable");
    let mut admitted = Vec::new();
    let mut refused = Vec::new();
    for transaction in candidates {
        match admission.try_push(transaction) {
            Ok(()) => admitted.push(transaction.hash),
            Err(_) => refused.push(transaction.hash),
        }
    }
    assert_eq!(admitted, hashes_of(&expected), "admitted set differs");
    assert_eq!(refused, hashes_of(&expected_refused), "refused set differs");
    assert_eq!(admission.len(), admitted.len());
    assert_eq!(admission.is_empty(), admitted.is_empty());
    assert_eq!(admission.execution_height(), state.height() + 1);
    state
        .validate_v3_block_admission(&expected)
        .expect("the selection is a valid block");
    admitted.len()
}

#[test]
fn v3_block_admission_builder_matches_reference_greedy() {
    let fixture = differential_fixture();
    let mut admitted = 0;
    for seed in 1..=3 {
        let mut candidates = differential_candidates(&fixture, seed, 24);
        // The order a DagBlock commits, then an arbitrary other order.
        candidates.sort_by_key(|transaction| transaction.hash.0);
        admitted += assert_builder_matches_reference(&fixture.state, &candidates);
        candidates.reverse();
        assert_builder_matches_reference(&fixture.state, &candidates);
    }
    assert!(admitted >= 3, "every seeded batch admits something");
}

#[test]
fn v3_block_admission_builder_refuses_without_recording() {
    let fixture = differential_fixture();
    let state = &fixture.state;
    let first = signed_transfer(
        state,
        &fixture.senders[0],
        fixture_address("refusal-recipient", 0),
        5,
        1,
        0,
    );
    let mut admission = state.v3_block_admission().expect("next height");
    assert!(admission.is_empty());
    admission
        .try_push(&first)
        .expect("first transfer is admissible");
    let refusals = [
        first.clone(),
        signed_transfer(
            state,
            &fixture.senders[0],
            fixture_address("refusal-recipient", 1),
            5,
            1,
            0,
        ),
        signed_transfer(
            state,
            &fixture.senders[1],
            fixture_address("refusal-recipient", 0),
            5,
            1,
            0,
        ),
        signed_transfer(
            state,
            &fixture.senders[2],
            fixture_address("refusal-recipient", 2),
            5,
            0,
            0,
        ),
    ];
    for refused in &refusals {
        assert!(admission.try_push(refused).is_err());
        assert_eq!(admission.len(), 1, "a refusal must not be recorded");
    }
    let mut claim = Transaction::new_faucet_claim(
        fixture.validators[0].address(),
        fixture_address("refusal-faucet", 0),
        1,
        0,
    );
    state
        .sign_transaction(&mut claim, &fixture.validators[0])
        .expect("claim signs");
    admission
        .try_push(&claim)
        .expect("a disjoint family still fits after refusals");
    assert_eq!(admission.len(), 2);
    state
        .validate_v3_block_admission(&[first, claim])
        .expect("the builder's block is valid");
}

#[test]
fn v3_fee_settlement_is_dormant_without_a_schedule_entry() {
    use block_stm::FeeTreasuryAccess::{BlockEpilogue, PerTransaction};

    let state = v3_fixture_state(&[]);
    assert_eq!(state.v3_fee_settlement_activation_height(), None);
    assert!(!state.v3_fee_settlement_activation_overridden());
    for height in [0, 1, u64::MAX] {
        assert!(!state.v3_fee_settlement_active_at(height));
        assert_eq!(state.v3_fee_access_at(height), PerTransaction);
    }

    // The override activates from exactly its height on a v3 state.
    state.set_v3_fee_settlement_activation_override(Some(10));
    assert!(state.v3_fee_settlement_activation_overridden());
    assert_eq!(state.v3_fee_settlement_activation_height(), Some(10));
    assert!(!state.v3_fee_settlement_active_at(9));
    assert!(state.v3_fee_settlement_active_at(10));
    assert!(state.v3_fee_settlement_active_at(u64::MAX));
    assert_eq!(state.v3_fee_access_at(9), PerTransaction);
    assert_eq!(state.v3_fee_access_at(10), BlockEpilogue);

    // An explicit `None` override disables it again.
    state.set_v3_fee_settlement_activation_override(None);
    assert!(state.v3_fee_settlement_activation_overridden());
    assert_eq!(state.v3_fee_settlement_activation_height(), None);
    assert!(!state.v3_fee_settlement_active_at(u64::MAX));

    // A legacy (non-recovered) chain never leaves the launch rule.
    let legacy = StateDB::with_genesis(&[]);
    legacy.set_v3_fee_settlement_activation_override(Some(0));
    assert_eq!(legacy.v3_fee_settlement_activation_height(), Some(0));
    assert!(!legacy.v3_fee_settlement_active_at(1));
    assert_eq!(legacy.v3_fee_access_at(1), PerTransaction);
}

#[test]
fn v3_fee_settlement_schedule_matches_only_the_exact_transaction_domain() {
    let domain = hash_bytes(b"scheduled-transaction-domain");
    let other = hash_bytes(b"another-transaction-domain");
    let entry = domain.to_hex();
    let upper = entry.to_uppercase();
    let prefixed = format!("0x{entry}");
    let schedule = [(entry.as_str(), 77)];
    assert_eq!(
        scheduled_v3_fee_settlement_activation(&schedule, || Some(domain)),
        Some(77)
    );
    assert_eq!(
        scheduled_v3_fee_settlement_activation(&schedule, || Some(other)),
        None
    );
    assert_eq!(
        scheduled_v3_fee_settlement_activation(&schedule, || None),
        None,
        "a chain without a recovery domain never activates"
    );
    for malformed in [upper.as_str(), prefixed.as_str()] {
        assert_eq!(
            scheduled_v3_fee_settlement_activation(&[(malformed, 77)], || Some(domain)),
            None
        );
    }
    let mut derived = false;
    assert_eq!(
        scheduled_v3_fee_settlement_activation(&[], || {
            derived = true;
            Some(domain)
        }),
        None
    );
    assert!(!derived, "an empty schedule never derives the domain");
}

#[test]
fn v3_fee_settlement_schedule_is_well_formed() {
    let mut domains = HashSet::new();
    for (domain, height) in V3_BLOCK_FEE_SETTLEMENT_SCHEDULE {
        assert_eq!(domain.len(), 64, "{domain}");
        assert!(
            domain
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "{domain} is not lower-case hex"
        );
        assert!(domains.insert(*domain), "duplicate entry for {domain}");
        assert!(*height > 0, "{domain} activates at genesis");
    }
}

fn total_balance(state: &StateDB) -> u128 {
    state
        .accounts
        .iter()
        .map(|entry| u128::from(entry.value().balance) + u128::from(entry.value().staked_balance))
        .sum()
}

fn treasury_balance(state: &StateDB) -> Option<u64> {
    state
        .get_account(&v3_fee_treasury_address())
        .map(|treasury| treasury.balance)
}

#[test]
fn v3_launch_rule_goldens_hold_until_activation() {
    for activation in [None, Some(1_000)] {
        let transcript = launch_rule_transcript(|state| {
            state.set_v3_fee_settlement_activation_override(activation);
        });
        assert_matches_goldens(&transcript, V3_LAUNCH_RULE_GOLDENS);
    }
}

#[test]
fn v3_fee_settlement_pre_activation_replay_is_identical() {
    let senders: Vec<KeyPair> = (0..6)
        .map(|index| fixture_key("replay-sender", index))
        .collect();
    let prefunded: Vec<(Address, u64)> = senders
        .iter()
        .map(|sender| (sender.address(), 10_000))
        .collect();
    let producer = fixture_key("replay-producer", 0).address();
    let replay = |activation: Option<Option<u64>>| {
        let state = v3_fixture_state(&prefunded);
        if let Some(activation) = activation {
            state.set_v3_fee_settlement_activation_override(activation);
        }
        let mut history = Vec::new();
        for (index, sender) in senders.iter().enumerate() {
            let index = index as u64;
            let transfer = signed_transfer(
                &state,
                sender,
                fixture_address("replay-recipient", index),
                25,
                index + 1,
                0,
            );
            let (block, _) = commit_adaptive(&state, std::slice::from_ref(&transfer), producer);
            history.push((block.hash, block.header.state_root));
            let (block, _) = commit_adaptive(&state, &[], producer);
            history.push((block.hash, block.header.state_root));
        }
        (history, treasury_balance(&state))
    };
    let launch_rule = replay(None);
    assert_eq!(launch_rule.1, Some(21));
    assert_eq!(replay(Some(None)), launch_rule);
    assert_eq!(
        replay(Some(Some(1_000))),
        launch_rule,
        "an activation beyond the history changes nothing"
    );
    assert_eq!(
        replay(Some(Some(5))),
        launch_rule,
        "single-transfer blocks settle to the same headers and roots under either rule"
    );
}

#[test]
fn v3_fee_settlement_admits_and_executes_1024_disjoint_transfers() {
    let cap = V3_MAX_TRANSACTIONS_PER_BLOCK;
    let directory = fixture_dir("max-block");
    let senders: Vec<KeyPair> = (0..=cap as u64)
        .map(|index| fixture_key("max-block-sender", index))
        .collect();
    let sender_addresses: Vec<Address> = senders.iter().map(KeyPair::address).collect();
    let prefunded: Vec<(Address, u64)> = sender_addresses
        .iter()
        .map(|address| (*address, 5_000))
        .collect();
    let state =
        StateDB::with_genesis_persistent(&prefunded, &directory, hash_bytes(FIXTURE_GENESIS))
            .expect("fixture state opens");
    bind_fixture_recovery_domain(&state);
    state.set_v3_fee_settlement_activation_override(Some(1));
    let producer = fixture_key("max-block-producer", 0).address();
    let transactions: Vec<Transaction> = senders
        .iter()
        .enumerate()
        .map(|(index, sender)| {
            let index = index as u64;
            let fee = if index < cap as u64 { index + 1 } else { 1 };
            signed_transfer(
                &state,
                sender,
                fixture_address("max-block-recipient", index),
                7,
                fee,
                0,
            )
        })
        .collect();
    let (block_transactions, overflow) = transactions.split_at(cap);

    // The builder admits exactly the cap and refuses the next transfer.
    let mut admission = state.v3_block_admission().expect("next height");
    for transaction in block_transactions {
        admission
            .try_push(transaction)
            .expect("a disjoint transfer is admitted");
    }
    assert_eq!(admission.len(), cap);
    let refusal = admission
        .try_push(&overflow[0])
        .expect_err("the 1,025th transfer is refused");
    assert!(refusal.to_string().contains("maximum is 1024"), "{refusal}");
    assert!(state.validate_v3_block_admission(&transactions).is_err());

    let supply = total_balance(&state);
    let (block, receipts) = commit_adaptive(&state, block_transactions, producer);
    assert_eq!(block.header.tx_count as usize, cap);
    assert!(receipts.iter().all(|receipt| receipt.success));
    assert_eq!(treasury_balance(&state), Some(524_800));
    assert_eq!(
        total_balance(&state),
        supply,
        "transfers and fees conserve supply"
    );
    for (index, address) in sender_addresses.iter().enumerate().take(cap) {
        let account = state.get_account(address).expect("sender exists");
        assert_eq!(account.balance, 5_000 - 7 - (index as u64 + 1));
        assert_eq!(account.nonce, 1);
    }
    assert_eq!(
        state
            .get_account(&fixture_address("max-block-recipient", 0))
            .expect("recipient created")
            .balance,
        7
    );
    let untouched = state
        .get_account(&sender_addresses[cap])
        .expect("excluded sender exists");
    assert_eq!((untouched.balance, untouched.nonce), (5_000, 0));

    // The epilogue is the only treasury write: one WAL record at the block.
    state.wal.sync().expect("fixture WAL syncs");
    drop(state);
    let treasury = v3_fee_treasury_address();
    let treasury_writes: Vec<u64> = read_wal(directory.join("state.wal"))
        .iter()
        .filter(|entry| matches!(&entry.op, WalOp::SetAccount(address, _) if *address == treasury))
        .map(|entry| entry.block_height)
        .collect();
    assert_eq!(treasury_writes, vec![1]);
    std::fs::remove_dir_all(&directory).expect("fixture directory is removable");
}

/// What one execution engine produced, minus the wall-clock parts of a block.
#[derive(Debug, PartialEq, Eq)]
struct EngineOutcome {
    state_root: Hash256,
    receipts: Vec<(Hash256, u32, bool, u64)>,
    treasury: Option<u64>,
}

fn engine_outcome(state: &StateDB, executed: (Block, Vec<TxReceipt>)) -> EngineOutcome {
    let (block, receipts) = executed;
    assert_eq!(block.header.state_root, state.get_state_root());
    assert_eq!(block.header.height, 1);
    EngineOutcome {
        state_root: block.header.state_root,
        receipts: receipts
            .iter()
            .map(|receipt| {
                (
                    receipt.tx_hash,
                    receipt.index,
                    receipt.success,
                    receipt.gas_used,
                )
            })
            .collect(),
        treasury: treasury_balance(state),
    }
}

#[test]
fn v3_fee_settlement_engines_agree() {
    let count = 128;
    let senders: Vec<KeyPair> = (0..count)
        .map(|index| fixture_key("engine-sender", index))
        .collect();
    let prefunded: Vec<(Address, u64)> = senders
        .iter()
        .map(|sender| (sender.address(), 10_000))
        .collect();
    let fresh_state = || {
        let state = v3_fixture_state(&prefunded);
        state.set_v3_fee_settlement_activation_override(Some(1));
        state
    };
    let signer = fresh_state();
    let transactions: Vec<Transaction> = senders
        .iter()
        .enumerate()
        .map(|(index, sender)| {
            let index = index as u64;
            signed_transfer(
                &signer,
                sender,
                fixture_address("engine-recipient", index),
                10,
                index + 1,
                0,
            )
        })
        .collect();
    let producer = fixture_key("engine-producer", 0).address();

    let mut outcomes = Vec::new();
    let state = fresh_state();
    outcomes.push((
        "partitioned execute_block",
        engine_outcome(
            &state,
            state
                .execute_block(&transactions, producer)
                .expect("executes"),
        ),
    ));
    let state = fresh_state();
    assert_eq!(
        state.execution_mode(&transactions),
        block_stm::AdaptiveMode::BlockSTM,
        "128 transfers to distinct recipients select BlockSTM"
    );
    outcomes.push((
        "adaptive BlockSTM",
        engine_outcome(&state, commit_adaptive(&state, &transactions, producer)),
    ));
    let state = fresh_state();
    outcomes.push((
        "sequential verified",
        engine_outcome(&state, commit_verified(&state, &transactions, producer)),
    ));
    let state = fresh_state();
    outcomes.push((
        "batch-verified",
        engine_outcome(
            &state,
            state
                .execute_block_gpu_verified(&transactions, producer)
                .expect("executes"),
        ),
    ));
    let state = fresh_state();
    outcomes.push((
        "sender-sharded parallel",
        engine_outcome(
            &state,
            state
                .execute_block_parallel(&transactions, producer)
                .expect("executes"),
        ),
    ));
    let state = fresh_state();
    outcomes.push((
        "partitioned execute_block_stm",
        engine_outcome(
            &state,
            state
                .execute_block_stm(&transactions, producer)
                .expect("executes"),
        ),
    ));

    let (_, reference) = &outcomes[0];
    assert_eq!(reference.treasury, Some((1..=count).sum::<u64>()));
    assert!(reference.receipts.iter().all(|receipt| receipt.2));
    for (engine, outcome) in &outcomes {
        assert_eq!(outcome, reference, "{engine} diverged");
    }
}

#[test]
fn v3_fee_settlement_keeps_other_conflicts() {
    let keys: Vec<KeyPair> = (0..3)
        .map(|index| fixture_key("conflict-sender", index))
        .collect();
    let addresses: Vec<Address> = keys.iter().map(KeyPair::address).collect();
    let state = v3_fixture_state(
        &addresses
            .iter()
            .map(|address| (*address, 1_000))
            .collect::<Vec<_>>(),
    );
    state.set_v3_fee_settlement_activation_override(Some(1));
    let fresh = |index| fixture_address("conflict-recipient", index);
    let transfer = |from: usize, to: Address| signed_transfer(&state, &keys[from], to, 5, 1, 0);

    let disjoint = [transfer(0, fresh(0)), transfer(1, fresh(1))];
    state
        .validate_v3_block_admission(&disjoint)
        .expect("disjoint transfers share a block");
    state
        .validate_v3_dag_availability(&disjoint)
        .expect("disjoint transfers are available together");

    let conflicts = [
        (
            "shared recipient",
            [transfer(0, fresh(2)), transfer(1, fresh(2))],
        ),
        (
            "chained A to B to C",
            [transfer(0, addresses[1]), transfer(1, fresh(3))],
        ),
        (
            "recipient also sends",
            [transfer(1, fresh(4)), transfer(0, addresses[1])],
        ),
        (
            "one nonce spent twice",
            [transfer(0, fresh(5)), transfer(0, fresh(6))],
        ),
    ];
    for (label, pair) in &conflicts {
        assert!(state.validate_v3_block_admission(pair).is_err(), "{label}");
        assert!(state.validate_v3_dag_availability(pair).is_err(), "{label}");
        let mut admission = state.v3_block_admission().expect("next height");
        admission.try_push(&pair[0]).expect(label);
        assert!(admission.try_push(&pair[1]).is_err(), "{label}");
        assert_eq!(admission.len(), 1, "{label}");
    }

    // The fee treasury is never a party to a transfer, in either direction.
    let treasury = v3_fee_treasury_address();
    let to_treasury = transfer(2, treasury);
    assert!(
        state
            .validate_v3_transaction_admission(&to_treasury)
            .is_err()
    );
    let mut admission = state.v3_block_admission().expect("next height");
    assert!(admission.try_push(&to_treasury).is_err());
    let mut from_treasury = Transaction::new_transfer(treasury, fresh(7), 1, 0);
    from_treasury.fee = 1;
    state
        .sign_transaction(&mut from_treasury, &keys[2])
        .expect("signs");
    assert!(admission.try_push(&from_treasury).is_err());
    assert!(admission.is_empty());
}

#[test]
fn settle_v3_block_fees_counts_only_successful_transfers() {
    use block_stm::FeeTreasuryAccess::{BlockEpilogue, PerTransaction};

    let keys: Vec<KeyPair> = (0..3)
        .map(|index| fixture_key("settle-sender", index))
        .collect();
    let treasury = v3_fee_treasury_address();
    let mut prefunded: Vec<(Address, u64)> =
        keys.iter().map(|key| (key.address(), 1_000)).collect();
    prefunded.push((treasury, 40));
    let state = v3_fixture_state(&prefunded);
    let mut fee_bearing_claim = Transaction::new_faucet_claim(
        keys[0].address(),
        fixture_address("settle-recipient", 9),
        1,
        0,
    );
    fee_bearing_claim.fee = 13;
    let transactions = vec![
        signed_transfer(
            &state,
            &keys[0],
            fixture_address("settle-recipient", 0),
            5,
            5,
            0,
        ),
        signed_transfer(
            &state,
            &keys[1],
            fixture_address("settle-recipient", 1),
            5,
            7,
            0,
        ),
        signed_transfer(
            &state,
            &keys[2],
            fixture_address("settle-recipient", 2),
            5,
            11,
            0,
        ),
        fee_bearing_claim,
    ];
    let receipts_with = |outcomes: [bool; 4]| -> Vec<TxReceipt> {
        transactions
            .iter()
            .zip(outcomes)
            .enumerate()
            .map(|(index, (transaction, success))| TxReceipt {
                tx_hash: transaction.hash,
                block_height: 1,
                block_hash: Hash256::ZERO,
                index: index as u32,
                success,
                gas_used: 0,
                value_commitment: None,
                inclusion_proof: None,
                logs: vec![],
            })
            .collect()
    };
    let receipts = receipts_with([true, false, true, true]);

    // Under the launch rule each transfer already credited its own fee.
    state
        .settle_v3_block_fees(1, PerTransaction, &transactions, &receipts)
        .expect("no-op");
    assert_eq!(treasury_balance(&state), Some(40));
    // A receipt list that does not match the block is refused unwritten.
    assert!(
        state
            .settle_v3_block_fees(1, BlockEpilogue, &transactions, &receipts[..3])
            .is_err()
    );
    assert_eq!(treasury_balance(&state), Some(40));
    // Only successful transfers count: 5 + 11, not the failed 7 and not the
    // fee field of another family.
    state
        .settle_v3_block_fees(1, BlockEpilogue, &transactions, &receipts)
        .expect("settles");
    assert_eq!(treasury_balance(&state), Some(56));

    // Nothing to settle writes nothing, not even an absent treasury account.
    let empty = v3_fixture_state(&[]);
    empty
        .settle_v3_block_fees(
            1,
            BlockEpilogue,
            &transactions,
            &receipts_with([false, false, false, true]),
        )
        .expect("nothing to settle");
    assert_eq!(treasury_balance(&empty), None);

    // A credit that would overflow the treasury is refused atomically.
    let full = v3_fixture_state(&[(treasury, u64::MAX - 10)]);
    assert!(
        full.settle_v3_block_fees(1, BlockEpilogue, &transactions, &receipts)
            .is_err()
    );
    assert_eq!(treasury_balance(&full), Some(u64::MAX - 10));
}

#[test]
fn empty_faucet_reward_blocks_do_not_touch_treasury() {
    let fixture = differential_fixture();
    let state = &fixture.state;
    state.set_v3_fee_settlement_activation_override(Some(1));
    let producer = fixture.validators[0].address();

    commit_adaptive(state, &[], producer);
    assert_eq!(
        treasury_balance(state),
        None,
        "an empty block settles nothing"
    );

    let mut claim = Transaction::new_faucet_claim(
        fixture.validators[1].address(),
        fixture_address("treasury-faucet-recipient", 0),
        3,
        0,
    );
    state
        .sign_transaction(&mut claim, &fixture.validators[1])
        .expect("claim signs");
    let (_, receipts) = commit_adaptive(state, std::slice::from_ref(&claim), producer);
    assert!(receipts[0].success);
    assert_eq!(treasury_balance(state), None, "a faucet claim pays no fee");

    // A faucet claim and a transfer share a block; only the transfer's fee
    // reaches the treasury.
    let mut second_claim = Transaction::new_faucet_claim(
        fixture.validators[2].address(),
        fixture_address("treasury-faucet-recipient", 1),
        3,
        0,
    );
    state
        .sign_transaction(&mut second_claim, &fixture.validators[2])
        .expect("claim signs");
    let transfer = signed_transfer(
        state,
        &fixture.senders[0],
        fixture_address("treasury-transfer-recipient", 0),
        5,
        4,
        0,
    );
    let (block, receipts) = commit_adaptive(state, &[second_claim, transfer], producer);
    assert_eq!(block.header.tx_count, 2);
    assert!(receipts.iter().all(|receipt| receipt.success));
    assert_eq!(treasury_balance(state), Some(4));
}

#[test]
fn v3_block_admission_rejects_fee_sum_overflow() {
    let keys: Vec<KeyPair> = (0..3)
        .map(|index| fixture_key("overflow-sender", index))
        .collect();
    let treasury = v3_fee_treasury_address();
    let mut prefunded: Vec<(Address, u64)> =
        keys.iter().map(|key| (key.address(), 1_000)).collect();
    prefunded.push((treasury, u64::MAX - 10));
    let state = v3_fixture_state(&prefunded);
    state.set_v3_fee_settlement_activation_override(Some(1));
    let transfer = |index: usize, fee: u64| {
        signed_transfer(
            &state,
            &keys[index],
            fixture_address("overflow-recipient", index as u64),
            1,
            fee,
            0,
        )
    };
    let (first, second, third) = (transfer(0, 6), transfer(1, 6), transfer(2, 4));
    for single in [&first, &second, &third] {
        state
            .validate_v3_block_admission(std::slice::from_ref(single))
            .expect("each fee fits the treasury alone");
    }
    let error = state
        .validate_v3_block_admission(&[first.clone(), second.clone()])
        .expect_err("6 + 6 exceeds the remaining 10");
    assert!(
        error.to_string().contains("overflow the fee treasury"),
        "{error}"
    );
    let mut admission = state.v3_block_admission().expect("next height");
    admission.try_push(&first).expect("first fits");
    assert!(admission.try_push(&second).is_err());
    admission
        .try_push(&third)
        .expect("a refused fee leaves the running sum unchanged");
    assert_eq!(admission.len(), 2);

    let (_, receipts) = commit_adaptive(
        &state,
        &[first, third],
        fixture_key("overflow-producer", 0).address(),
    );
    assert!(receipts.iter().all(|receipt| receipt.success));
    assert_eq!(treasury_balance(&state), Some(u64::MAX));

    // The running fee sum itself is checked too.
    let rich: Vec<KeyPair> = (0..2)
        .map(|index| fixture_key("overflow-rich", index))
        .collect();
    let rich_state = v3_fixture_state(
        &rich
            .iter()
            .map(|key| (key.address(), u64::MAX))
            .collect::<Vec<_>>(),
    );
    rich_state.set_v3_fee_settlement_activation_override(Some(1));
    let huge: Vec<Transaction> = rich
        .iter()
        .enumerate()
        .map(|(index, key)| {
            signed_transfer(
                &rich_state,
                key,
                fixture_address("overflow-rich-recipient", index as u64),
                1,
                u64::MAX - 1,
                0,
            )
        })
        .collect();
    rich_state
        .validate_v3_block_admission(&huge[..1])
        .expect("one huge fee fits an empty treasury");
    let error = rich_state
        .validate_v3_block_admission(&huge)
        .expect_err("two huge fees overflow u64");
    assert!(error.to_string().contains("overflow"), "{error}");
}

#[test]
fn v3_block_admission_builder_matches_reference_greedy_under_block_settlement() {
    let fixture = differential_fixture();
    fixture
        .state
        .set_v3_fee_settlement_activation_override(Some(1));
    let mut most_admitted = 0;
    for seed in 11..=13 {
        let mut candidates = differential_candidates(&fixture, seed, 24);
        candidates.sort_by_key(|transaction| transaction.hash.0);
        most_admitted = most_admitted.max(assert_builder_matches_reference(
            &fixture.state,
            &candidates,
        ));
        candidates.reverse();
        assert_builder_matches_reference(&fixture.state, &candidates);
    }
    assert!(
        most_admitted >= 2,
        "block-level settlement must admit several transfers in one block"
    );
}

#[test]
fn dag_availability_relaxes_treasury_only_when_scheduled() {
    let keys: Vec<KeyPair> = (0..3)
        .map(|index| fixture_key("availability-sender", index))
        .collect();
    let state = v3_fixture_state(
        &keys
            .iter()
            .map(|key| (key.address(), 1_000))
            .collect::<Vec<_>>(),
    );
    let fresh = |index| fixture_address("availability-recipient", index);
    let pair = [
        signed_transfer(&state, &keys[0], fresh(0), 1, 1, 0),
        signed_transfer(&state, &keys[1], fresh(1), 1, 1, 0),
    ];
    let shared = [
        signed_transfer(&state, &keys[0], fresh(2), 1, 1, 0),
        signed_transfer(&state, &keys[2], fresh(2), 1, 1, 0),
    ];
    assert!(
        state.validate_v3_dag_availability(&pair).is_err(),
        "without a schedule the fee-treasury key still conflicts"
    );

    // A schedule relaxes availability at every height, long before it
    // activates, so a lagging node keeps post-activation DAG parents ...
    state.set_v3_fee_settlement_activation_override(Some(1_000_000));
    state
        .validate_v3_dag_availability(&pair)
        .expect("availability is never height-gated");
    assert!(state.validate_v3_dag_availability(&shared).is_err());
    // ... while execution keeps the launch rule until the exact height.
    assert!(state.validate_v3_block_admission(&pair).is_err());
    assert!(
        state
            .validate_v3_block_admission_at(&pair, 999_999)
            .is_err()
    );
    state
        .validate_v3_block_admission_at(&pair, 1_000_000)
        .expect("admitted from the activation height on");

    // An explicitly disabled schedule restores the launch rule everywhere.
    state.set_v3_fee_settlement_activation_override(None);
    assert!(state.validate_v3_dag_availability(&pair).is_err());
}

#[test]
fn settled_transfers_are_not_indexed_under_the_fee_treasury() {
    let senders: Vec<KeyPair> = (0..4)
        .map(|index| fixture_key("history-sender", index))
        .collect();
    let state = v3_fixture_state(
        &senders
            .iter()
            .map(|sender| (sender.address(), 1_000))
            .collect::<Vec<_>>(),
    );
    state.set_v3_fee_settlement_activation_override(Some(3));
    let treasury = v3_fee_treasury_address();
    let producer = fixture_key("history-producer", 0).address();
    let recipient = |index| fixture_address("history-recipient", index);

    // Under the launch rule every transfer is part of the treasury history.
    let first = signed_transfer(&state, &senders[0], recipient(0), 1, 1, 0);
    commit_adaptive(&state, std::slice::from_ref(&first), producer);
    let second = signed_transfer(&state, &senders[1], recipient(1), 1, 1, 0);
    commit_adaptive(&state, std::slice::from_ref(&second), producer);
    assert_eq!(
        state.get_account_txs(&treasury.0),
        vec![first.hash, second.hash]
    );

    // Settled transfers touch only their own accounts and are indexed there.
    let settled = [
        signed_transfer(&state, &senders[2], recipient(2), 1, 1, 0),
        signed_transfer(&state, &senders[3], recipient(3), 1, 1, 0),
    ];
    let (block, receipts) = commit_adaptive(&state, &settled, producer);
    assert_eq!(block.header.height, 3);
    assert!(receipts.iter().all(|receipt| receipt.success));
    assert_eq!(treasury_balance(&state), Some(4));
    assert_eq!(
        state.get_account_txs(&treasury.0),
        vec![first.hash, second.hash],
        "settled transfers must not grow the treasury history"
    );
    for transfer in &settled {
        let TxBody::Transfer(body) = &transfer.body else {
            unreachable!("fixture transfers are transfers");
        };
        assert_eq!(state.get_account_txs(&transfer.from.0), vec![transfer.hash]);
        assert_eq!(state.get_account_txs(&body.to.0), vec![transfer.hash]);
    }
}
