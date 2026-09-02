use arc_crypto::{Hash256, KeyPair, hash_bytes};
use arc_state::{StateDB, block_stm};
use arc_types::transaction::{
    InferenceAttestationBody, InferenceChallengeBody, faucet_pool_address,
};
use arc_types::{Address, Transaction, TxBody, TxReceipt, TxType};

const PARALLEL_THRESHOLD: usize = 100;

fn address(domain: u8, index: u16) -> Address {
    hash_bytes(&[domain, index as u8, (index >> 8) as u8])
}

fn verified_transfer(from: Address, to: Address, amount: u64, nonce: u64) -> Transaction {
    let mut tx = Transaction::new_transfer(from, to, amount, nonce);
    tx.sig_verified = true;
    tx
}

fn verified_attestation(from: Address, bond: u64) -> Transaction {
    let mut tx = Transaction {
        tx_type: TxType::InferenceAttestation,
        from,
        nonce: 0,
        body: TxBody::InferenceAttestation(InferenceAttestationBody {
            model_id: hash_bytes(b"canonical-order-model"),
            input_hash: hash_bytes(b"canonical-order-input"),
            output_hash: hash_bytes(b"canonical-order-output"),
            challenge_period: 100,
            bond,
            beneficiary: None,
        }),
        fee: 0,
        gas_limit: 0,
        hash: Hash256::ZERO,
        signature: arc_crypto::Signature::null(),
        sig_verified: true,
    };
    tx.hash = tx.compute_hash();
    tx
}

fn verified_challenge(
    from: Address,
    attestation_hash: Hash256,
    challenger_bond: u64,
) -> Transaction {
    let mut tx = Transaction {
        tx_type: TxType::InferenceChallenge,
        from,
        nonce: 0,
        body: TxBody::InferenceChallenge(InferenceChallengeBody {
            attestation_hash,
            challenger_output_hash: hash_bytes(&[from.0[0], 0xCC]),
            challenger_bond,
        }),
        fee: 0,
        gas_limit: 0,
        hash: Hash256::ZERO,
        signature: arc_crypto::Signature::null(),
        sig_verified: true,
    };
    tx.hash = tx.compute_hash();
    tx
}

fn append_disjoint_fillers(
    transactions: &mut Vec<Transaction>,
    genesis: &mut Vec<(Address, u64)>,
    count: usize,
) {
    for index in 0..count as u16 {
        let sender = address(0xF1, index);
        let recipient = address(0xF2, index);
        genesis.push((sender, 1));
        genesis.push((recipient, 0));
        transactions.push(verified_transfer(sender, recipient, 1, 0));
    }
}

fn assert_receipt_semantics_match(sequential: &[TxReceipt], adaptive: &[TxReceipt]) {
    assert_eq!(sequential.len(), adaptive.len());
    for (expected, actual) in sequential.iter().zip(adaptive) {
        assert_eq!(actual.tx_hash, expected.tx_hash);
        assert_eq!(actual.block_height, expected.block_height);
        assert_eq!(actual.index, expected.index);
        assert_eq!(actual.success, expected.success);
        assert_eq!(actual.gas_used, expected.gas_used);
        assert_eq!(actual.value_commitment, expected.value_commitment);
        assert_eq!(actual.inclusion_proof, expected.inclusion_proof);
        assert_eq!(actual.logs.len(), expected.logs.len());
    }
}

fn assert_accounts_match(sequential: &StateDB, adaptive: &StateDB, addresses: &[Address]) {
    for address in addresses {
        let expected = sequential.get_account(address);
        let actual = adaptive.get_account(address);
        match (expected, actual) {
            (Some(expected), Some(actual)) => {
                assert_eq!(actual.address, expected.address);
                assert_eq!(actual.balance, expected.balance, "balance for {address}");
                assert_eq!(actual.nonce, expected.nonce, "nonce for {address}");
                assert_eq!(actual.code_hash, expected.code_hash);
                assert_eq!(actual.storage_root, expected.storage_root);
                assert_eq!(actual.staked_balance, expected.staked_balance);
            }
            (None, None) => {}
            states => panic!("account presence mismatch for {address}: {states:?}"),
        }
    }
}

#[test]
fn partition_preserves_transitive_conflict_order() {
    // tx 1 depends on tx 0 through A; tx 2 depends on tx 1 through C but is
    // disjoint from tx 0. Ordinary first-fit coloring put tx 2 back in batch
    // 0, executing it before its conflicting predecessor in batch 1.
    let transactions = vec![
        verified_transfer(address(1, 1), address(1, 2), 1, 0),
        verified_transfer(address(1, 3), address(1, 1), 1, 0),
        verified_transfer(address(1, 4), address(1, 3), 1, 0),
    ];

    assert_eq!(
        block_stm::partition_batches(&transactions),
        vec![vec![0], vec![1], vec![2]],
    );
}

#[test]
fn adaptive_matches_sequential_for_parallel_threshold_dependency_chain() {
    let account_a = address(2, 1);
    let account_b = address(2, 2);
    let account_c = address(2, 3);
    let account_d = address(2, 4);
    let mut genesis = vec![
        (account_a, 1),
        (account_b, 0),
        (account_c, 0),
        (account_d, 1),
    ];
    let mut transactions = vec![
        verified_transfer(account_a, account_b, 1, 0),
        // Canonically fails: C is funded only by the later third transaction.
        verified_transfer(account_c, account_a, 1, 0),
        verified_transfer(account_d, account_c, 1, 0),
    ];
    let filler_count = PARALLEL_THRESHOLD - transactions.len();
    append_disjoint_fillers(&mut transactions, &mut genesis, filler_count);
    assert_eq!(transactions.len(), PARALLEL_THRESHOLD);
    assert_eq!(
        block_stm::choose_execution_mode(&transactions),
        block_stm::AdaptiveMode::BlockSTM,
    );

    let sequential = StateDB::with_genesis(&genesis);
    let adaptive = StateDB::with_genesis(&genesis);
    let producer = address(2, 99);
    let (sequential_block, sequential_receipts) = sequential
        .execute_block_verified(&transactions, producer)
        .unwrap();
    let (adaptive_block, adaptive_receipts) = adaptive
        .execute_block_adaptive(&transactions, producer)
        .unwrap();

    assert_eq!(
        sequential_receipts
            .iter()
            .take(3)
            .map(|receipt| receipt.success)
            .collect::<Vec<_>>(),
        vec![true, false, true],
    );
    assert_receipt_semantics_match(&sequential_receipts, &adaptive_receipts);
    assert_eq!(
        adaptive_block.header.tx_root,
        sequential_block.header.tx_root
    );
    assert_eq!(
        adaptive_block.header.state_root,
        sequential_block.header.state_root,
    );
    assert_accounts_match(
        &sequential,
        &adaptive,
        &[account_a, account_b, account_c, account_d],
    );
}

#[test]
fn adaptive_matches_sequential_for_cross_sender_treasury_rewards() {
    let validator_a = KeyPair::generate_ed25519();
    let validator_b = KeyPair::generate_ed25519();
    // Put the lexicographically larger sender first so the old sender sort
    // deterministically reversed the two shared-treasury transactions.
    let (first_validator, second_validator) = if validator_a.address().0 > validator_b.address().0 {
        (&validator_a, &validator_b)
    } else {
        (&validator_b, &validator_a)
    };
    let first_recipient = address(3, 1);
    let second_recipient = address(3, 2);
    let pool = faucet_pool_address();
    let mut genesis = vec![(pool, 10_000), (first_recipient, 0), (second_recipient, 0)];

    let mut first =
        Transaction::new_faucet_claim(first_validator.address(), first_recipient, 7_000, 0);
    first.sign(first_validator).unwrap();
    let mut second =
        Transaction::new_faucet_claim(second_validator.address(), second_recipient, 7_000, 0);
    second.sign(second_validator).unwrap();
    let mut transactions = vec![first, second];
    let filler_count = PARALLEL_THRESHOLD - transactions.len();
    append_disjoint_fillers(&mut transactions, &mut genesis, filler_count);
    assert_eq!(
        block_stm::choose_execution_mode(&transactions),
        block_stm::AdaptiveMode::BlockSTM,
    );

    let sequential = StateDB::with_genesis(&genesis);
    let adaptive = StateDB::with_genesis(&genesis);
    let validators = [
        (validator_a.address(), StateDB::MIN_VALIDATOR_STAKE),
        (validator_b.address(), StateDB::MIN_VALIDATOR_STAKE),
    ];
    sequential.seed_genesis_validators(&validators);
    adaptive.seed_genesis_validators(&validators);
    let producer = first_validator.address();

    let (sequential_block, sequential_receipts) = sequential
        .execute_block_verified(&transactions, producer)
        .unwrap();
    let (adaptive_block, adaptive_receipts) = adaptive
        .execute_block_adaptive(&transactions, producer)
        .unwrap();

    assert_eq!(
        sequential_receipts
            .iter()
            .take(2)
            .map(|receipt| receipt.success)
            .collect::<Vec<_>>(),
        vec![true, false],
    );
    assert_receipt_semantics_match(&sequential_receipts, &adaptive_receipts);
    assert_eq!(
        adaptive_block.header.tx_root,
        sequential_block.header.tx_root
    );
    assert_eq!(
        adaptive_block.header.state_root,
        sequential_block.header.state_root,
    );
    assert_accounts_match(
        &sequential,
        &adaptive,
        &[pool, first_recipient, second_recipient],
    );
    assert_eq!(adaptive.get_account(&pool).unwrap().balance, 3_000);
    assert_eq!(
        adaptive.get_account(&first_recipient).unwrap().balance,
        7_000
    );
    assert_eq!(adaptive.get_account(&second_recipient).unwrap().balance, 0);
}

#[test]
fn adaptive_matches_sequential_for_cross_sender_challenges() {
    let pool = faucet_pool_address();
    let worker = address(4, 1);
    let challenger_a = address(4, 2);
    let challenger_b = address(4, 3);
    let bond = 100;
    let mut genesis = vec![
        (
            pool,
            10 * arc_types::economics::INFERENCE_ATTESTATION_REWARD,
        ),
        (worker, bond),
        (challenger_a, 500),
        (challenger_b, 500),
    ];
    let sequential = StateDB::with_genesis(&genesis);
    let adaptive = StateDB::with_genesis(&genesis);
    let attestation = verified_attestation(worker, bond);
    let attestation_hash = attestation.hash;
    let producer = address(4, 99);
    sequential
        .execute_block_verified(std::slice::from_ref(&attestation), producer)
        .unwrap();
    adaptive
        .execute_block_verified(std::slice::from_ref(&attestation), producer)
        .unwrap();

    let mut transactions = vec![
        verified_challenge(challenger_a, attestation_hash, 200),
        verified_challenge(challenger_b, attestation_hash, 300),
    ];
    let filler_count = PARALLEL_THRESHOLD - transactions.len();
    append_disjoint_fillers(&mut transactions, &mut genesis, filler_count);
    // Fillers were added to genesis after the databases were created; install
    // those accounts explicitly so both executions start from the same state.
    for (account, balance) in genesis.iter().skip(4) {
        sequential.update_account(account, arc_types::Account::new(*account, *balance));
        adaptive.update_account(account, arc_types::Account::new(*account, *balance));
    }
    assert_eq!(
        block_stm::choose_execution_mode(&transactions),
        block_stm::AdaptiveMode::BlockSTM,
    );

    let (sequential_block, sequential_receipts) = sequential
        .execute_block_verified(&transactions, producer)
        .unwrap();
    let (adaptive_block, adaptive_receipts) = adaptive
        .execute_block_adaptive(&transactions, producer)
        .unwrap();

    assert_receipt_semantics_match(&sequential_receipts, &adaptive_receipts);
    assert_eq!(
        adaptive_block.header.tx_root,
        sequential_block.header.tx_root
    );
    assert_eq!(
        adaptive_block.header.state_root,
        sequential_block.header.state_root,
    );
    let escrow = hash_bytes(&[b"arc-inference", attestation_hash.as_ref()].concat());
    assert_accounts_match(
        &sequential,
        &adaptive,
        &[challenger_a, challenger_b, escrow],
    );
    assert_eq!(adaptive.get_account(&escrow).unwrap().balance, bond + 500);
}
