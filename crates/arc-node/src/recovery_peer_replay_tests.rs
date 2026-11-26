const PEER_REPLAY_WEIGHTS: [u64; 6] = [
    6_666_667, 6_666_667, 6_666_667, 6_666_666, 6_666_667, 6_666_666,
];
const PEER_REPLAY_TOTAL_STAKE: u64 = 40_000_000;
const PEER_REPLAY_STRICT_QUORUM: u64 = 26_666_667;
const PEER_REPLAY_FAULT_TOLERANCE: u64 = 13_333_333;
const PEER_REPLAY_SOURCE_INDEX: usize = 0;
const PEER_REPLAY_TARGET_INDEX: usize = 4;
const PEER_REPLAY_SECOND_ABSENT_INDEX: usize = 5;

fn peer_replay_temp_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
}

struct PeerReplayFixture {
    seeds: [[u8; 32]; 6],
    validators: [Hash256; 6],
    domain: arc_consensus::ConsensusDomain,
    committee: ValidatorSet,
    committee_hash: Hash256,
}

struct PeerReplayWindow {
    bootstrap: u64,
    last_round: u64,
    candidate: DagBlock,
    blocks_by_round: Vec<Vec<DagBlock>>,
    bootstrap_absence: arc_consensus::view_change::SkipCertificate,
    target_absence: arc_consensus::view_change::SkipCertificate,
    second_absence: arc_consensus::view_change::SkipCertificate,
    records: Vec<RetainedDagRecord>,
}

impl PeerReplayFixture {
    fn new(domain_label: &[u8]) -> Self {
        let mut entries: Vec<(Hash256, [u8; 32])> = (0..6usize)
            .map(|index| {
                let seed = hash_bytes(format!("recovery-peer-replay-key-{index}").as_bytes()).0;
                let address = arc_crypto::KeyPair::from_ed25519_secret_bytes(&seed).address();
                (address, seed)
            })
            .collect();
        entries.sort_by_key(|(address, _)| address.0);

        let mut seeds = [[0u8; 32]; 6];
        let mut validators = [Hash256::ZERO; 6];
        for (index, (address, seed)) in entries.into_iter().enumerate() {
            seeds[index] = seed;
            validators[index] = address;
        }

        let committee = ValidatorSet::new(
            validators
                .iter()
                .zip(PEER_REPLAY_WEIGHTS.iter())
                .enumerate()
                .map(|(index, (address, weight))| {
                    Validator::new(*address, *weight, index as u16).unwrap()
                })
                .collect(),
            1,
        );
        assert_eq!(committee.total_stake, PEER_REPLAY_TOTAL_STAKE);
        assert_eq!(committee.quorum, PEER_REPLAY_STRICT_QUORUM);
        assert_eq!(
            committee.fault_tolerance_stake(),
            PEER_REPLAY_FAULT_TOLERANCE
        );
        assert_eq!(
            committee.stake_for_addresses(&validators[..4]),
            PEER_REPLAY_STRICT_QUORUM
        );
        assert!(committee.has_quorum(&validators[..4]));
        assert_eq!(committee.stake_for_addresses(&validators[..3]), 20_000_001);
        assert!(!committee.has_quorum(&validators[..3]));
        assert_eq!(
            committee.stake_for_addresses(&validators[4..]),
            PEER_REPLAY_FAULT_TOLERANCE
        );
        assert_eq!(
            committee
                .get_validator(&validators[PEER_REPLAY_TARGET_INDEX])
                .unwrap()
                .stake,
            6_666_667
        );

        let domain = arc_consensus::ConsensusDomain::new(hash_bytes(domain_label), 3, 9);
        let committee_hash = arc_consensus::view_change::validator_set_hash(&committee);
        Self {
            seeds,
            validators,
            domain,
            committee,
            committee_hash,
        }
    }

    fn keypair(&self, index: usize) -> arc_crypto::KeyPair {
        arc_crypto::KeyPair::from_ed25519_secret_bytes(&self.seeds[index])
    }

    fn strict_engine(&self, local_index: usize) -> ConsensusEngine {
        let keypair = self.keypair(local_index);
        let engine = ConsensusEngine::new_with_keypair(
            self.committee.clone(),
            self.validators[local_index],
            keypair,
        );
        engine.install_consensus_domain(self.domain).unwrap();
        engine
    }

    fn binding(&self) -> RecoveryDagBinding {
        let mut binding = recovery_test_binding(self.domain);
        binding.manifest_hash = hash_bytes(b"recovery-peer-replay-manifest");
        binding.validator_set_commitment = self.committee_hash;
        binding.source_height = 0;
        binding.transition_height = 1;
        binding.source_consensus_round = 100;
        binding.initial_consensus_round = 101;
        binding
    }

    fn startup(&self, data_dir: &std::path::Path) -> RecoveryDagStartup {
        RecoveryDagStartup {
            data_dir: data_dir.to_path_buf(),
            wal_dir: data_dir.join("generation-store"),
            binding: self.binding(),
            archived_legacy_wal: None,
        }
    }

    fn signed_block(
        &self,
        author_index: usize,
        round: u64,
        parents: Vec<Hash256>,
        domain: &arc_consensus::ConsensusDomain,
    ) -> DagBlock {
        let keypair = self.keypair(author_index);
        let transactions = Vec::new();
        let ordering_commitment = DagBlock::compute_ordering_commitment(&transactions);
        let mut block = DagBlock {
            author: keypair.address(),
            round,
            parents,
            transactions,
            timestamp: round,
            hash: Hash256::ZERO,
            signature: Vec::new(),
            ordering_commitment,
        };
        block.hash = block.compute_hash_in_domain(domain);
        let signature = keypair.sign(&block.hash).unwrap();
        block.signature = bincode::serialize(&signature).unwrap();
        block
    }

    fn skip_certificate(
        &self,
        domain: arc_consensus::ConsensusDomain,
        committee_hash: Hash256,
        round: u64,
        absentee: Hash256,
        signer_indices: &[usize],
    ) -> arc_consensus::view_change::SkipCertificate {
        use arc_consensus::view_change::{AbsenceReason, SkipCertificate, SkipVote};

        let votes = signer_indices
            .iter()
            .map(|index| {
                let keypair = self.keypair(*index);
                SkipVote::sign(
                    domain,
                    committee_hash,
                    round,
                    absentee,
                    AbsenceReason::NoBlock,
                    self.committee.quorum,
                    &keypair,
                )
                .unwrap()
            })
            .collect();
        SkipCertificate::new(
            domain,
            committee_hash,
            round,
            absentee,
            AbsenceReason::NoBlock,
            votes,
        )
    }

    fn build_live_quorum_window(&self) -> PeerReplayWindow {
        let bootstrap = self.binding().initial_consensus_round;
        let last_round = bootstrap + 7;
        let live_signers = [0usize, 1, 2, 3];

        assert_eq!(bootstrap as usize % 6, PEER_REPLAY_SECOND_ABSENT_INDEX);
        assert_eq!((bootstrap + 1) as usize % 6, PEER_REPLAY_SOURCE_INDEX);
        assert_eq!((bootstrap + 5) as usize % 6, PEER_REPLAY_TARGET_INDEX);
        assert_eq!(
            (bootstrap + 6) as usize % 6,
            PEER_REPLAY_SECOND_ABSENT_INDEX
        );

        let bootstrap_absence = self.skip_certificate(
            self.domain,
            self.committee_hash,
            bootstrap,
            self.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
            &live_signers,
        );
        let target_absence = self.skip_certificate(
            self.domain,
            self.committee_hash,
            bootstrap + 5,
            self.validators[PEER_REPLAY_TARGET_INDEX],
            &live_signers,
        );
        let second_absence = self.skip_certificate(
            self.domain,
            self.committee_hash,
            bootstrap + 6,
            self.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
            &live_signers,
        );
        assert!(
            bootstrap_absence
                .verify(&self.domain, &self.committee)
                .is_ok()
        );
        assert!(target_absence.verify(&self.domain, &self.committee).is_ok());
        assert!(second_absence.verify(&self.domain, &self.committee).is_ok());

        let mut blocks_by_round: Vec<Vec<DagBlock>> = Vec::new();
        for round in bootstrap..=last_round {
            let parents = if round == bootstrap {
                Vec::new()
            } else {
                blocks_by_round
                    .last()
                    .unwrap()
                    .iter()
                    .map(|block| block.hash)
                    .collect()
            };
            let round_blocks: Vec<DagBlock> = live_signers
                .iter()
                .map(|author| self.signed_block(*author, round, parents.clone(), &self.domain))
                .collect();
            assert_eq!(round_blocks.len(), 4);
            assert!(round_blocks.iter().all(|block| {
                !block.signature.is_empty()
                    && block.author != self.validators[PEER_REPLAY_TARGET_INDEX]
                    && block.author != self.validators[PEER_REPLAY_SECOND_ABSENT_INDEX]
            }));
            blocks_by_round.push(round_blocks);
        }

        let candidate = blocks_by_round[1]
            .iter()
            .find(|block| block.author == self.validators[PEER_REPLAY_SOURCE_INDEX])
            .unwrap()
            .clone();
        assert_eq!(candidate.round, bootstrap + 1);

        let mut records = Vec::new();
        for certificate in [&bootstrap_absence, &target_absence, &second_absence] {
            let payload = bincode::serialize(certificate).unwrap();
            records.push(RetainedDagRecord::absence_certificate(
                certificate.round,
                hash_bytes(&payload),
                payload,
            ));
        }
        for round_blocks in &blocks_by_round {
            for block in round_blocks {
                records.push(RetainedDagRecord::dag_block(
                    block.round,
                    block.hash,
                    bincode::serialize(block).unwrap(),
                ));
            }
        }

        PeerReplayWindow {
            bootstrap,
            last_round,
            candidate,
            blocks_by_round,
            bootstrap_absence,
            target_absence,
            second_absence,
            records,
        }
    }

    fn build_canonical_state(&self, bootstrap: u64, candidate: &DagBlock) -> StateDB {
        let state = StateDB::with_genesis(&[(hash_bytes(b"peer-replay-funded"), 100)]);
        state
            .execute_block_adaptive_at_with_proof(
                &[],
                self.validators[PEER_REPLAY_SOURCE_INDEX],
                bootstrap,
                hash_bytes(b"peer-replay-baseline"),
            )
            .unwrap();
        let baseline = state.get_block(1).unwrap();
        assert_ne!(baseline.header.state_root, Hash256::ZERO);
        state
            .execute_block_adaptive_at_with_proof(
                &[],
                candidate.author,
                candidate.round,
                candidate.state_decision_commitment(&self.domain),
            )
            .unwrap();
        assert_eq!(state.height(), 2);
        assert_eq!(
            state.get_block(2).unwrap().header.proof_hash,
            candidate.state_decision_commitment(&self.domain)
        );
        state
    }

    fn initial_generation_input(&self, state: &StateDB, bootstrap: u64) -> GenerationInput {
        let baseline = state.get_block(1).unwrap();
        GenerationInput {
            binding: recovery_test_generation_binding(&self.binding()),
            baseline_state: DagBaselineState {
                height: 1,
                block_hash: baseline.hash,
                state_root: baseline.header.state_root,
                dag_anchor: None,
            },
            dag_cursor: DagCursor {
                committed_block_count: 0,
                next_dag_round: bootstrap,
                current_round: bootstrap,
                retention_floor_round: bootstrap,
                retention_ceiling_round: recovery_retention_ceiling(bootstrap).unwrap(),
            },
            retention_limits: RetentionLimits::default(),
        }
    }
}

fn peer_replay_copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let file_type = entry.file_type().unwrap();
        let target_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            peer_replay_copy_dir(&entry.path(), &target_path);
        } else {
            assert!(
                file_type.is_file(),
                "unexpected non-file entry in generation store: {:?}",
                entry.path()
            );
            std::fs::copy(entry.path(), target_path).unwrap();
        }
    }
}

fn peer_replay_write_signing_record(
    path: &std::path::Path,
    record: &arc_consensus::view_change::ConsensusSigningRecord,
) -> Vec<u8> {
    use std::io::Write;

    let bytes = record.encode();
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temporary).unwrap();
    file.write_all(&bytes).unwrap();
    file.sync_all().unwrap();
    std::fs::rename(&temporary, path).unwrap();
    bytes
}

fn peer_replay_expect_cert_err<T>(
    result: Result<T, arc_consensus::view_change::CertificateError>,
    expected: arc_consensus::view_change::CertificateError,
) {
    match result {
        Ok(_) => panic!("expected certificate error {expected:?}, got Ok"),
        Err(actual) => assert_eq!(actual, expected),
    }
}

fn peer_replay_replace_block_record(
    records: &[RetainedDagRecord],
    target_round: u64,
    target_author: Hash256,
    replacement: &DagBlock,
) -> Vec<RetainedDagRecord> {
    let mut replaced = false;
    let updated = records
        .iter()
        .map(|record| {
            if record.kind == RetainedRecordKind::DagBlock && record.round == target_round {
                let decoded: DagBlock = bincode::deserialize(&record.payload).unwrap();
                if decoded.author == target_author {
                    replaced = true;
                    return RetainedDagRecord::dag_block(
                        replacement.round,
                        replacement.hash,
                        bincode::serialize(replacement).unwrap(),
                    );
                }
            }
            record.clone()
        })
        .collect();
    assert!(replaced);
    updated
}

fn peer_replay_replace_certificate_record(
    records: &[RetainedDagRecord],
    target_round: u64,
    target_absentee: Hash256,
    replacement: &arc_consensus::view_change::SkipCertificate,
) -> Vec<RetainedDagRecord> {
    let mut replaced = false;
    let updated = records
        .iter()
        .map(|record| {
            if record.kind == RetainedRecordKind::AbsenceCertificate && record.round == target_round
            {
                let decoded: arc_consensus::view_change::SkipCertificate =
                    bincode::deserialize(&record.payload).unwrap();
                if decoded.absentee == target_absentee {
                    replaced = true;
                    let payload = bincode::serialize(replacement).unwrap();
                    return RetainedDagRecord::absence_certificate(
                        replacement.round,
                        hash_bytes(&payload),
                        payload,
                    );
                }
            }
            record.clone()
        })
        .collect();
    assert!(replaced);
    updated
}

#[test]
fn recovery_peer_generation_replays_for_different_absent_validator_compacts_and_restarts() {
    use std::collections::HashMap;

    use arc_consensus::view_change::{
        AbsenceReason, ConsensusSigningRecord, DEFAULT_SKIP_GRACE_MS, SkipRefusal, SkipTracker,
    };
    use arc_node::recovery_dag_wal::BaselineDagAnchor;

    let fixture = PeerReplayFixture::new(b"recovery-peer-replay-positive-domain");
    let window = fixture.build_live_quorum_window();
    let source_state = fixture.build_canonical_state(window.bootstrap, &window.candidate);
    let target_state = fixture.build_canonical_state(window.bootstrap, &window.candidate);

    let canonical_tip = source_state.get_block(2).unwrap();
    let target_tip = target_state.get_block(2).unwrap();
    assert_eq!(source_state.height(), 2);
    assert_eq!(target_state.height(), 2);
    assert_eq!(canonical_tip.hash, target_tip.hash);
    assert_eq!(
        canonical_tip.header.state_root,
        target_tip.header.state_root
    );
    assert_eq!(
        canonical_tip.header.proof_hash,
        target_tip.header.proof_hash
    );

    let baseline_block = target_state.get_block(1).unwrap();
    let older_finality_identity = (
        baseline_block.hash,
        baseline_block.header.state_root,
        baseline_block.header.proof_hash,
    );
    let older_skip_round = window.bootstrap - 6;
    let older_absentee = fixture.validators[1];
    let in_window_skip_round = window.target_absence.round;
    let in_window_absentee = fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX];

    let mut target_skipped_rounds = HashMap::new();
    target_skipped_rounds.insert(
        older_skip_round,
        HashMap::from([(older_absentee, AbsenceReason::NoBlock)]),
    );
    target_skipped_rounds.insert(
        in_window_skip_round,
        HashMap::from([(in_window_absentee, AbsenceReason::NoBlock)]),
    );
    let target_signing_record = ConsensusSigningRecord {
        skipped_rounds: target_skipped_rounds,
        finality_votes: HashMap::from([(1u64, older_finality_identity)]),
        last_applied_round: Some(older_skip_round),
    };

    let target_dir = peer_replay_temp_dir("arc-recovery-peer-replay-target");
    let target_signing_path = target_dir.path().join("consensus-signing-record.bin");
    let target_signing_bytes =
        peer_replay_write_signing_record(&target_signing_path, &target_signing_record);

    let (source_replay, source_compacted, source_engine) = {
        let source_dir = peer_replay_temp_dir("arc-recovery-peer-replay-source");
        let source_signing_path = source_dir.path().join("consensus-signing-record.bin");
        let source_signing_record = ConsensusSigningRecord {
            skipped_rounds: HashMap::from([(
                window.bootstrap,
                HashMap::from([(
                    fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
                    AbsenceReason::NoBlock,
                )]),
            )]),
            finality_votes: HashMap::new(),
            last_applied_round: Some(window.candidate.round),
        };
        let source_signing_bytes =
            peer_replay_write_signing_record(&source_signing_path, &source_signing_record);
        assert_ne!(source_signing_bytes, target_signing_bytes);

        let source_startup = fixture.startup(source_dir.path());
        let source_store = GenerationStore::new(&source_startup.wal_dir);
        let source_generation = source_store
            .create_initial(
                fixture.initial_generation_input(&source_state, window.bootstrap),
                window.records.clone(),
            )
            .unwrap();
        validate_recovery_generation_anchor(&source_state, &source_startup, &source_generation)
            .unwrap();
        let (staged, staged_summary) =
            stage_recovery_generation_records(&source_store, &source_generation).unwrap();
        assert_eq!(staged, window.records);

        let source_engine = fixture.strict_engine(PEER_REPLAY_SOURCE_INDEX);
        let source_replay = replay_recovery_dag_generation(
            &source_engine,
            &source_state,
            &source_startup,
            &source_generation,
            &staged,
        )
        .unwrap();
        assert_eq!(
            source_replay.repaired_commit,
            Some((window.candidate.hash, window.candidate.round))
        );
        assert_eq!(source_replay.current_round, window.last_round + 1);
        assert_eq!(source_replay.next_commit_round, window.candidate.round + 1);
        assert_eq!(
            source_engine.committed_blocks(),
            vec![window.candidate.hash]
        );

        let source_compacted = compact_replayed_recovery_generation(
            &source_store,
            &source_state,
            &source_startup,
            &source_generation,
            &staged_summary,
            &source_engine,
            &staged,
        )
        .unwrap();
        assert_eq!(
            source_compacted.manifest.baseline_state.dag_anchor,
            Some(BaselineDagAnchor {
                hash: window.candidate.hash,
                round: window.candidate.round,
            })
        );
        assert_eq!(
            source_compacted.manifest.dag_cursor.next_dag_round,
            window.candidate.round + 1
        );
        assert_eq!(
            source_compacted.manifest.dag_cursor.current_round,
            window.last_round + 1
        );

        let target_startup = fixture.startup(target_dir.path());
        peer_replay_copy_dir(&source_startup.wal_dir, &target_startup.wal_dir);
        (source_replay, source_compacted, source_engine)
    };

    let target_startup = fixture.startup(target_dir.path());
    let target_store = GenerationStore::new(&target_startup.wal_dir);
    validate_recovery_generation_anchor(&target_state, &target_startup, &source_compacted).unwrap();
    let (copied_records, copied_summary) =
        stage_recovery_generation_records(&target_store, &source_compacted).unwrap();
    assert_eq!(copied_records.len(), 26);
    assert!(copied_records.iter().all(|record| {
        record.kind != RetainedRecordKind::Commit && record.round > window.candidate.round
    }));
    let copied_block_count = copied_records
        .iter()
        .filter(|record| record.kind == RetainedRecordKind::DagBlock)
        .count();
    let copied_absence_count = copied_records
        .iter()
        .filter(|record| record.kind == RetainedRecordKind::AbsenceCertificate)
        .count();
    assert_eq!(copied_block_count, 24);
    assert_eq!(copied_absence_count, 2);

    let target_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
    let target_replay = replay_recovery_dag_generation(
        &target_engine,
        &target_state,
        &target_startup,
        &source_compacted,
        &copied_records,
    )
    .unwrap();
    assert_eq!(target_replay.repaired_commit, None);
    assert_eq!(target_replay.current_round, source_replay.current_round);
    assert_eq!(
        target_replay.next_commit_round,
        source_replay.next_commit_round
    );
    assert_eq!(target_engine.current_round(), source_engine.current_round());
    assert_eq!(
        target_engine.last_committed_round(),
        source_engine.last_committed_round()
    );
    assert!(target_engine.has_skip_certificate(
        window.target_absence.round,
        &fixture.validators[PEER_REPLAY_TARGET_INDEX]
    ));
    assert!(target_engine.has_skip_certificate(
        window.second_absence.round,
        &fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX]
    ));
    assert!(!target_engine.has_skip_certificate(
        window.bootstrap_absence.round,
        &fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX]
    ));
    for round_blocks in &window.blocks_by_round[2..] {
        for block in round_blocks {
            assert!(target_engine.get_block(&block.hash).is_some());
        }
    }

    let unchanged_on_target = compact_replayed_recovery_generation(
        &target_store,
        &target_state,
        &target_startup,
        &source_compacted,
        &copied_summary,
        &target_engine,
        &copied_records,
    )
    .unwrap();
    assert_eq!(unchanged_on_target.pin, source_compacted.pin);

    let target_restarted_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
    let (restarted_records, restarted_summary) =
        stage_recovery_generation_records(&target_store, &unchanged_on_target).unwrap();
    let target_restarted_replay = replay_recovery_dag_generation(
        &target_restarted_engine,
        &target_state,
        &target_startup,
        &unchanged_on_target,
        &restarted_records,
    )
    .unwrap();
    assert_eq!(target_restarted_replay.repaired_commit, None);
    assert_eq!(
        target_restarted_replay.current_round,
        source_replay.current_round
    );
    assert_eq!(
        target_restarted_replay.next_commit_round,
        source_replay.next_commit_round
    );
    assert_eq!(target_state.height(), source_state.height());
    assert_eq!(
        target_state.get_block(2).unwrap().header.state_root,
        canonical_tip.header.state_root
    );
    assert_eq!(target_state.get_block(2).unwrap().hash, canonical_tip.hash);
    let unchanged_after_restart = compact_replayed_recovery_generation(
        &target_store,
        &target_state,
        &target_startup,
        &unchanged_on_target,
        &restarted_summary,
        &target_restarted_engine,
        &restarted_records,
    )
    .unwrap();
    assert_eq!(unchanged_after_restart.pin, source_compacted.pin);

    let native_target_dir = peer_replay_temp_dir("arc-recovery-peer-replay-target-native");
    let native_target_startup = fixture.startup(native_target_dir.path());
    let native_target_store = GenerationStore::new(&native_target_startup.wal_dir);
    let native_target_generation = native_target_store
        .create_initial(
            fixture.initial_generation_input(&target_state, window.bootstrap),
            window.records.clone(),
        )
        .unwrap();
    let (native_staged, native_summary) =
        stage_recovery_generation_records(&native_target_store, &native_target_generation).unwrap();
    let native_target_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
    let native_replay = replay_recovery_dag_generation(
        &native_target_engine,
        &target_state,
        &native_target_startup,
        &native_target_generation,
        &native_staged,
    )
    .unwrap();
    assert_eq!(
        native_replay.repaired_commit,
        Some((window.candidate.hash, window.candidate.round))
    );
    assert_eq!(native_replay.current_round, source_replay.current_round);
    assert_eq!(
        native_replay.next_commit_round,
        source_replay.next_commit_round
    );
    assert_eq!(
        native_target_engine.committed_blocks(),
        vec![window.candidate.hash]
    );
    let native_compacted = compact_replayed_recovery_generation(
        &native_target_store,
        &target_state,
        &native_target_startup,
        &native_target_generation,
        &native_summary,
        &native_target_engine,
        &native_staged,
    )
    .unwrap();
    assert_eq!(native_compacted.pin, source_compacted.pin);
    assert_eq!(
        native_compacted.manifest.baseline_state,
        source_compacted.manifest.baseline_state
    );
    assert_eq!(
        native_compacted.manifest.dag_cursor,
        source_compacted.manifest.dag_cursor
    );
    assert_eq!(
        native_compacted.manifest.retained_records.record_count,
        source_compacted.manifest.retained_records.record_count
    );

    let mut with_explicit_commit = native_staged.clone();
    with_explicit_commit.push(RetainedDagRecord::commit(
        window.candidate.round,
        window.candidate.hash,
    ));
    let explicit_target_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
    let explicit_replay = replay_recovery_dag_generation(
        &explicit_target_engine,
        &target_state,
        &native_target_startup,
        &native_target_generation,
        &with_explicit_commit,
    )
    .unwrap();
    assert_eq!(explicit_replay.repaired_commit, None);
    assert_eq!(explicit_replay.current_round, source_replay.current_round);
    assert_eq!(
        explicit_replay.next_commit_round,
        source_replay.next_commit_round
    );
    assert_eq!(
        explicit_target_engine.committed_blocks(),
        vec![window.candidate.hash]
    );

    let next_round = window.last_round + 1;
    let next_parents: Vec<Hash256> = window
        .blocks_by_round
        .last()
        .unwrap()
        .iter()
        .map(|block| block.hash)
        .collect();
    for author_index in [0usize, 1, 2, 3, PEER_REPLAY_TARGET_INDEX] {
        let live_block = fixture.signed_block(
            author_index,
            next_round,
            next_parents.clone(),
            &fixture.domain,
        );
        target_restarted_engine.receive_block(&live_block).unwrap();
        let _ = target_restarted_engine.advance_round();
        source_engine.receive_block(&live_block).unwrap();
        let _ = source_engine.advance_round();
    }
    assert_eq!(target_restarted_engine.current_round(), next_round + 1);
    assert_eq!(source_engine.current_round(), next_round + 1);

    let retained_signing_bytes = std::fs::read(&target_signing_path).unwrap();
    assert_eq!(retained_signing_bytes, target_signing_bytes);
    let reloaded_record = ConsensusSigningRecord::decode(&retained_signing_bytes).unwrap();
    assert_eq!(reloaded_record, target_signing_record);
    assert_eq!(reloaded_record.next_round_to_scan(), older_skip_round + 1);
    assert_eq!(
        reloaded_record.finality_votes.get(&1),
        Some(&older_finality_identity)
    );
    assert_eq!(
        reloaded_record
            .skipped_rounds
            .get(&older_skip_round)
            .and_then(|entries| entries.get(&older_absentee)),
        Some(&AbsenceReason::NoBlock)
    );

    let target_key = fixture.keypair(PEER_REPLAY_TARGET_INDEX);
    let mut tracker = SkipTracker::new(
        fixture.domain,
        fixture.committee_hash,
        DEFAULT_SKIP_GRACE_MS,
        reloaded_record,
    );
    assert_eq!(tracker.record(), &target_signing_record);
    assert!(tracker.refuses(older_skip_round, &older_absentee));
    assert!(tracker.refuses(in_window_skip_round, &in_window_absentee));

    assert_eq!(
        tracker.sign_if_permitted(
            older_skip_round,
            older_absentee,
            AbsenceReason::NoBlock,
            target_restarted_replay.next_commit_round,
            fixture.committee.quorum,
            10_000,
            &target_key,
        ),
        Err(SkipRefusal::RoundAlreadyPassed)
    );

    let present_round = window.bootstrap + 4;
    let present_author = fixture.validators[3];
    tracker.observe(
        present_round,
        &present_author,
        AbsenceReason::NoBlock,
        fixture.committee.quorum,
        false,
        fixture.committee.quorum,
        1_000,
    );
    assert_eq!(
        tracker.sign_if_permitted(
            present_round,
            present_author,
            AbsenceReason::NoBlock,
            target_restarted_replay.next_commit_round,
            fixture.committee.quorum,
            10_000,
            &target_key,
        ),
        Err(SkipRefusal::AbsenteeBlockPresent)
    );

    assert_eq!(
        tracker.sign_if_permitted(
            next_round,
            fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
            AbsenceReason::NoBlock,
            target_restarted_replay.next_commit_round,
            fixture.committee.quorum,
            10_000,
            &target_key,
        ),
        Err(SkipRefusal::RoundBelowQuorum)
    );

    tracker.note_durable_commit_round(window.candidate.round);
    assert_eq!(
        tracker.record().last_applied_round,
        Some(window.candidate.round)
    );
    assert_eq!(
        tracker.record().next_round_to_scan(),
        window.candidate.round + 1
    );
    tracker.note_durable_commit_round(older_skip_round);
    assert_eq!(
        tracker.record().last_applied_round,
        Some(window.candidate.round)
    );
    assert_eq!(
        tracker.record().finality_votes.get(&1),
        Some(&older_finality_identity)
    );
}

#[test]
fn recovery_peer_replay_rejects_unsigned_forged_and_wrong_domain_blocks() {
    let fixture = PeerReplayFixture::new(b"recovery-peer-replay-block-negative-domain");
    let window = fixture.build_live_quorum_window();
    let state = fixture.build_canonical_state(window.bootstrap, &window.candidate);

    let data_dir = peer_replay_temp_dir("arc-recovery-peer-replay-block-negative");
    let startup = fixture.startup(data_dir.path());
    let store = GenerationStore::new(&startup.wal_dir);
    let generation = store
        .create_initial(
            fixture.initial_generation_input(&state, window.bootstrap),
            window.records.clone(),
        )
        .unwrap();
    let (staged, _) = stage_recovery_generation_records(&store, &generation).unwrap();

    let bootstrap_block = window.blocks_by_round[0][0].clone();
    assert_eq!(bootstrap_block.round, window.bootstrap);
    assert_eq!(bootstrap_block.author, fixture.validators[0]);

    let mut unsigned_block = bootstrap_block.clone();
    unsigned_block.signature.clear();

    let mut malformed_sig_block = bootstrap_block.clone();
    malformed_sig_block.signature = vec![0xff; 8];

    let mut tampered_sig_block = bootstrap_block.clone();
    let wrong_hash_sig = fixture
        .keypair(0)
        .sign(&hash_bytes(b"different-block-hash"))
        .unwrap();
    tampered_sig_block.signature = bincode::serialize(&wrong_hash_sig).unwrap();

    let mut wrong_signer_block = bootstrap_block.clone();
    let other_signer_sig = fixture.keypair(1).sign(&bootstrap_block.hash).unwrap();
    wrong_signer_block.signature = bincode::serialize(&other_signer_sig).unwrap();

    let wrong_domain_hash =
        arc_consensus::ConsensusDomain::new(hash_bytes(b"wrong-peer-replay-domain"), 3, 9);
    let wrong_recovery_epoch =
        arc_consensus::ConsensusDomain::new(fixture.domain.domain_hash, 4, 9);
    let wrong_validator_set_id =
        arc_consensus::ConsensusDomain::new(fixture.domain.domain_hash, 3, 10);

    let wrong_domain_hash_block =
        fixture.signed_block(0, window.bootstrap, Vec::new(), &wrong_domain_hash);
    let wrong_epoch_block =
        fixture.signed_block(0, window.bootstrap, Vec::new(), &wrong_recovery_epoch);
    let wrong_set_id_block =
        fixture.signed_block(0, window.bootstrap, Vec::new(), &wrong_validator_set_id);

    let direct_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
    direct_engine
        .install_recovery_cursor(startup.binding.source_consensus_round)
        .unwrap();
    direct_engine
        .install_recovery_generation_cursor(window.bootstrap, window.bootstrap, window.bootstrap)
        .unwrap();

    for (label, bad_block) in [
        ("unsigned", &unsigned_block),
        ("malformed-signature", &malformed_sig_block),
        ("tampered-signature", &tampered_sig_block),
        ("wrong-signer", &wrong_signer_block),
    ] {
        let result = direct_engine.receive_block(bad_block);
        assert!(
            matches!(result, Err(arc_consensus::ConsensusError::InvalidSignature)),
            "block mutation {label} must fail with InvalidSignature"
        );
        assert!(direct_engine.get_block(&bad_block.hash).is_none());
    }

    for (label, bad_block) in [
        ("wrong-domain-hash", &wrong_domain_hash_block),
        ("wrong-recovery-epoch", &wrong_epoch_block),
        ("wrong-validator-set-id", &wrong_set_id_block),
    ] {
        let result = direct_engine.receive_block(bad_block);
        assert!(
            matches!(result, Err(arc_consensus::ConsensusError::InvalidBlock(_))),
            "domain mutation {label} must fail with InvalidBlock"
        );
        assert!(direct_engine.get_block(&bad_block.hash).is_none());
    }

    for (label, bad_block) in [
        ("unsigned", &unsigned_block),
        ("malformed-signature", &malformed_sig_block),
        ("tampered-signature", &tampered_sig_block),
        ("wrong-signer", &wrong_signer_block),
        ("wrong-domain-hash", &wrong_domain_hash_block),
        ("wrong-recovery-epoch", &wrong_epoch_block),
        ("wrong-validator-set-id", &wrong_set_id_block),
    ] {
        let mutated_records = peer_replay_replace_block_record(
            &staged,
            window.bootstrap,
            fixture.validators[0],
            bad_block,
        );
        let target_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
        assert!(
            replay_recovery_dag_generation(
                &target_engine,
                &state,
                &startup,
                &generation,
                &mutated_records,
            )
            .is_err(),
            "replay must reject {label} block"
        );
    }

    let source_engine = fixture.strict_engine(PEER_REPLAY_SOURCE_INDEX);
    let (_, staged_summary) = stage_recovery_generation_records(&store, &generation).unwrap();
    replay_recovery_dag_generation(&source_engine, &state, &startup, &generation, &staged).unwrap();
    let compacted = compact_replayed_recovery_generation(
        &store,
        &state,
        &startup,
        &generation,
        &staged_summary,
        &source_engine,
        &staged,
    )
    .unwrap();

    let behind_state = StateDB::with_genesis(&[(hash_bytes(b"peer-replay-funded"), 100)]);
    behind_state
        .execute_block_adaptive_at_with_proof(
            &[],
            fixture.validators[PEER_REPLAY_SOURCE_INDEX],
            window.bootstrap,
            hash_bytes(b"peer-replay-baseline"),
        )
        .unwrap();
    assert!(validate_recovery_generation_anchor(&behind_state, &startup, &compacted).is_err());

    let divergent_state = StateDB::with_genesis(&[(hash_bytes(b"peer-replay-funded"), 100)]);
    divergent_state
        .execute_block_adaptive_at_with_proof(
            &[],
            fixture.validators[PEER_REPLAY_SOURCE_INDEX],
            window.bootstrap,
            hash_bytes(b"peer-replay-baseline"),
        )
        .unwrap();
    divergent_state
        .execute_block_adaptive_at_with_proof(
            &[],
            window.candidate.author,
            window.candidate.round,
            hash_bytes(b"divergent-state-decision"),
        )
        .unwrap();
    assert!(validate_recovery_generation_anchor(&divergent_state, &startup, &compacted).is_err());
}

#[test]
fn recovery_peer_replay_rejects_subquorum_forged_and_wrong_domain_skip_certificates() {
    use arc_consensus::view_change::{
        AbsenceReason, CertificateError, SkipVote, validator_set_hash,
    };

    let fixture = PeerReplayFixture::new(b"recovery-peer-replay-cert-negative-domain");
    let window = fixture.build_live_quorum_window();
    let state = fixture.build_canonical_state(window.bootstrap, &window.candidate);

    let data_dir = peer_replay_temp_dir("arc-recovery-peer-replay-cert-negative");
    let startup = fixture.startup(data_dir.path());
    let store = GenerationStore::new(&startup.wal_dir);
    let generation = store
        .create_initial(
            fixture.initial_generation_input(&state, window.bootstrap),
            window.records.clone(),
        )
        .unwrap();
    let (staged, _) = stage_recovery_generation_records(&store, &generation).unwrap();

    let mut subquorum_bootstrap = window.bootstrap_absence.clone();
    subquorum_bootstrap.votes.truncate(3);
    peer_replay_expect_cert_err(
        subquorum_bootstrap.verify(&fixture.domain, &fixture.committee),
        CertificateError::BelowQuorum {
            signing: 20_000_001,
            quorum: PEER_REPLAY_STRICT_QUORUM,
        },
    );

    let mut subquorum_target = window.target_absence.clone();
    subquorum_target.votes.truncate(3);
    peer_replay_expect_cert_err(
        subquorum_target.verify(&fixture.domain, &fixture.committee),
        CertificateError::BelowQuorum {
            signing: 20_000_001,
            quorum: PEER_REPLAY_STRICT_QUORUM,
        },
    );

    let mut empty_cert = window.bootstrap_absence.clone();
    empty_cert.votes.clear();
    peer_replay_expect_cert_err(
        empty_cert.verify(&fixture.domain, &fixture.committee),
        CertificateError::Empty,
    );

    let mut duplicate_voter_cert = window.bootstrap_absence.clone();
    duplicate_voter_cert.votes[3] = duplicate_voter_cert.votes[0].clone();
    peer_replay_expect_cert_err(
        duplicate_voter_cert.verify(&fixture.domain, &fixture.committee),
        CertificateError::DuplicateVoter(fixture.validators[0]),
    );

    let outsider = arc_crypto::KeyPair::from_ed25519_secret_bytes(
        &hash_bytes(b"recovery-peer-replay-outsider-key").0,
    );
    let mut outsider_cert = window.bootstrap_absence.clone();
    outsider_cert.votes[3] = SkipVote::sign(
        fixture.domain,
        fixture.committee_hash,
        window.bootstrap,
        fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
        AbsenceReason::NoBlock,
        fixture.committee.quorum,
        &outsider,
    )
    .unwrap();
    peer_replay_expect_cert_err(
        outsider_cert.verify(&fixture.domain, &fixture.committee),
        CertificateError::UnknownVoter(outsider.address()),
    );

    let mut bad_sig_cert = window.bootstrap_absence.clone();
    bad_sig_cert.votes[0].signature = bad_sig_cert.votes[1].signature.clone();
    peer_replay_expect_cert_err(
        bad_sig_cert.verify(&fixture.domain, &fixture.committee),
        CertificateError::BadSignature(fixture.validators[0]),
    );

    let wrong_domain_hash =
        arc_consensus::ConsensusDomain::new(hash_bytes(b"wrong-cert-domain"), 3, 9);
    let wrong_recovery_epoch =
        arc_consensus::ConsensusDomain::new(fixture.domain.domain_hash, 4, 9);
    let wrong_validator_set_id =
        arc_consensus::ConsensusDomain::new(fixture.domain.domain_hash, 3, 10);

    let wrong_domain_certs: Vec<(&str, arc_consensus::view_change::SkipCertificate)> = [
        ("wrong-domain-hash", wrong_domain_hash),
        ("wrong-recovery-epoch", wrong_recovery_epoch),
        ("wrong-validator-set-id", wrong_validator_set_id),
    ]
    .into_iter()
    .map(|(label, wrong_domain)| {
        let cert = fixture.skip_certificate(
            wrong_domain,
            fixture.committee_hash,
            window.bootstrap,
            fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
            &[0, 1, 2, 3],
        );
        peer_replay_expect_cert_err(
            cert.verify(&fixture.domain, &fixture.committee),
            CertificateError::WrongDomain {
                expected: Box::new(fixture.domain),
                found: Box::new(wrong_domain),
            },
        );
        (label, cert)
    })
    .collect();

    let skewed_weights: [u64; 6] = [
        6_666_667, 6_666_667, 6_666_667, 6_666_667, 6_666_667, 6_666_665,
    ];
    let skewed_committee = ValidatorSet::new(
        fixture
            .validators
            .iter()
            .zip(skewed_weights.iter())
            .enumerate()
            .map(|(index, (address, weight))| {
                Validator::new(*address, *weight, index as u16).unwrap()
            })
            .collect(),
        1,
    );
    let wrong_weight_set_cert = fixture.skip_certificate(
        fixture.domain,
        validator_set_hash(&skewed_committee),
        window.bootstrap,
        fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
        &[0, 1, 2, 3],
    );
    peer_replay_expect_cert_err(
        wrong_weight_set_cert.verify(&fixture.domain, &fixture.committee),
        CertificateError::WrongValidatorSet,
    );

    let wrong_epoch_committee = ValidatorSet::new(fixture.committee.validators.clone(), 2);
    let wrong_epoch_set_cert = fixture.skip_certificate(
        fixture.domain,
        validator_set_hash(&wrong_epoch_committee),
        window.bootstrap,
        fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
        &[0, 1, 2, 3],
    );
    peer_replay_expect_cert_err(
        wrong_epoch_set_cert.verify(&fixture.domain, &fixture.committee),
        CertificateError::WrongValidatorSet,
    );

    let mut bootstrap_cases: Vec<(&str, &arc_consensus::view_change::SkipCertificate)> = vec![
        ("subquorum-bootstrap", &subquorum_bootstrap),
        ("empty", &empty_cert),
        ("duplicate-voter", &duplicate_voter_cert),
        ("outsider-voter", &outsider_cert),
        ("bad-signature", &bad_sig_cert),
        ("wrong-weight-validator-set", &wrong_weight_set_cert),
        ("wrong-epoch-validator-set", &wrong_epoch_set_cert),
    ];
    for (label, cert) in &wrong_domain_certs {
        bootstrap_cases.push((*label, cert));
    }

    for (label, bad_cert) in bootstrap_cases {
        let engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
        assert!(
            engine.register_skip_certificate(bad_cert.clone()).is_err(),
            "register_skip_certificate must reject {label}"
        );
        assert!(!engine.has_skip_certificate(
            window.bootstrap,
            &fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX]
        ));

        let mutated = peer_replay_replace_certificate_record(
            &staged,
            window.bootstrap,
            fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
            bad_cert,
        );
        let replay_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
        assert!(
            replay_recovery_dag_generation(
                &replay_engine,
                &state,
                &startup,
                &generation,
                &mutated,
            )
            .is_err(),
            "replay must reject {label} absence certificate"
        );
    }

    let subquorum_target_records = peer_replay_replace_certificate_record(
        &staged,
        window.target_absence.round,
        fixture.validators[PEER_REPLAY_TARGET_INDEX],
        &subquorum_target,
    );
    let subquorum_target_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
    assert!(
        replay_recovery_dag_generation(
            &subquorum_target_engine,
            &state,
            &startup,
            &generation,
            &subquorum_target_records,
        )
        .is_err()
    );

    let missing_target_absence: Vec<RetainedDagRecord> = staged
        .iter()
        .filter(|record| {
            !(record.kind == RetainedRecordKind::AbsenceCertificate
                && record.round == window.target_absence.round)
        })
        .cloned()
        .collect();
    assert_eq!(missing_target_absence.len() + 1, staged.len());
    let missing_target_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
    assert!(
        replay_recovery_dag_generation(
            &missing_target_engine,
            &state,
            &startup,
            &generation,
            &missing_target_absence,
        )
        .is_err()
    );
}
