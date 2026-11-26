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
    absences_by_round: Vec<[arc_consensus::view_change::SkipCertificate; 2]>,
    records: Vec<RetainedDagRecord>,
}

impl PeerReplayWindow {
    fn absence(
        &self,
        round: u64,
        absentee_index: usize,
    ) -> &arc_consensus::view_change::SkipCertificate {
        let round_offset = usize::try_from(round - self.bootstrap).unwrap();
        let slot = match absentee_index {
            PEER_REPLAY_TARGET_INDEX => 0,
            PEER_REPLAY_SECOND_ABSENT_INDEX => 1,
            other => panic!("unexpected absentee index {other}"),
        };
        &self.absences_by_round[round_offset][slot]
    }
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
        assert_eq!((bootstrap + 2) as usize % 6, 1);
        assert_eq!((bootstrap + 5) as usize % 6, PEER_REPLAY_TARGET_INDEX);
        assert_eq!(
            (bootstrap + 6) as usize % 6,
            PEER_REPLAY_SECOND_ABSENT_INDEX
        );
        assert_eq!(last_round as usize % 6, PEER_REPLAY_SOURCE_INDEX);

        let mut absences_by_round = Vec::new();
        for round in bootstrap..=last_round {
            let target_cert = self.skip_certificate(
                self.domain,
                self.committee_hash,
                round,
                self.validators[PEER_REPLAY_TARGET_INDEX],
                &live_signers,
            );
            let second_cert = self.skip_certificate(
                self.domain,
                self.committee_hash,
                round,
                self.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
                &live_signers,
            );
            assert_eq!(
                target_cert.verify(&self.domain, &self.committee).unwrap(),
                PEER_REPLAY_STRICT_QUORUM
            );
            assert_eq!(
                second_cert.verify(&self.domain, &self.committee).unwrap(),
                PEER_REPLAY_STRICT_QUORUM
            );
            absences_by_round.push([target_cert, second_cert]);
        }
        assert_eq!(absences_by_round.len(), 8);

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
        for round_absences in &absences_by_round {
            for certificate in round_absences {
                let payload = bincode::serialize(certificate).unwrap();
                records.push(RetainedDagRecord::absence_certificate(
                    certificate.round,
                    hash_bytes(&payload),
                    payload,
                ));
            }
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
        assert_eq!(records.len(), 48);

        PeerReplayWindow {
            bootstrap,
            last_round,
            candidate,
            blocks_by_round,
            absences_by_round,
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
    let mut replaced = 0usize;
    let updated = records
        .iter()
        .map(|record| {
            if record.kind == RetainedRecordKind::DagBlock && record.round == target_round {
                let decoded: DagBlock = bincode::deserialize(&record.payload).unwrap();
                if decoded.author == target_author {
                    replaced += 1;
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
    assert_eq!(replaced, 1);
    updated
}

fn peer_replay_replace_certificate_record(
    records: &[RetainedDagRecord],
    target_round: u64,
    target_absentee: Hash256,
    replacement: &arc_consensus::view_change::SkipCertificate,
) -> Vec<RetainedDagRecord> {
    let mut replaced = 0usize;
    let updated = records
        .iter()
        .map(|record| {
            if record.kind == RetainedRecordKind::AbsenceCertificate && record.round == target_round
            {
                let decoded: arc_consensus::view_change::SkipCertificate =
                    bincode::deserialize(&record.payload).unwrap();
                if decoded.absentee == target_absentee {
                    replaced += 1;
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
    assert_eq!(replaced, 1);
    updated
}

fn peer_replay_remove_certificate_record(
    records: &[RetainedDagRecord],
    target_round: u64,
    target_absentee: Hash256,
) -> Vec<RetainedDagRecord> {
    let mut removed = 0usize;
    let updated: Vec<RetainedDagRecord> = records
        .iter()
        .filter(|record| {
            if record.kind == RetainedRecordKind::AbsenceCertificate && record.round == target_round
            {
                let decoded: arc_consensus::view_change::SkipCertificate =
                    bincode::deserialize(&record.payload).unwrap();
                if decoded.absentee == target_absentee {
                    removed += 1;
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect();
    assert_eq!(removed, 1);
    assert_eq!(updated.len() + 1, records.len());
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
    let in_window_skip_round = window
        .absence(window.bootstrap + 5, PEER_REPLAY_TARGET_INDEX)
        .round;
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

    let (source_replay, source_compacted, source_engine, source_dir_path) = {
        let source_dir = peer_replay_temp_dir("arc-recovery-peer-replay-source");
        let source_dir_path = source_dir.path().to_path_buf();
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
        assert_eq!(staged.len(), 48);

        let source_engine = fixture.strict_engine(PEER_REPLAY_SOURCE_INDEX);
        assert!(source_engine.requires_full_round_participation());
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
        assert_eq!(
            source_engine.find_committed_anchor(&canonical_tip.header.proof_hash, &fixture.domain),
            Some((window.candidate.hash, window.candidate.round))
        );
        for round in window.bootstrap..=window.last_round {
            assert!(
                source_engine
                    .has_skip_certificate(round, &fixture.validators[PEER_REPLAY_TARGET_INDEX])
            );
            assert!(
                source_engine.has_skip_certificate(
                    round,
                    &fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX]
                )
            );
            assert!(source_engine.has_quorum_parents_for_round(round));
        }

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
        assert_eq!(source_compacted.manifest.baseline_state.height, 2);
        assert_eq!(
            source_compacted.manifest.baseline_state.block_hash,
            canonical_tip.hash
        );
        assert_eq!(
            source_compacted.manifest.baseline_state.state_root,
            canonical_tip.header.state_root
        );
        assert_eq!(
            source_compacted.manifest.baseline_state.dag_anchor,
            Some(BaselineDagAnchor {
                hash: window.candidate.hash,
                round: window.candidate.round,
            })
        );
        assert_eq!(
            source_compacted.manifest.dag_cursor.committed_block_count,
            1
        );
        assert_eq!(
            source_compacted.manifest.dag_cursor.next_dag_round,
            window.candidate.round + 1
        );
        assert_eq!(
            source_compacted.manifest.dag_cursor.retention_floor_round,
            window.candidate.round + 1
        );
        assert_eq!(
            source_compacted.manifest.dag_cursor.current_round,
            window.last_round + 1
        );
        assert_eq!(
            source_compacted.manifest.dag_cursor.retention_ceiling_round,
            recovery_retention_ceiling(window.candidate.round + 1).unwrap()
        );
        assert_eq!(source_compacted.manifest.retained_records.record_count, 36);

        let target_startup = fixture.startup(target_dir.path());
        peer_replay_copy_dir(&source_startup.wal_dir, &target_startup.wal_dir);
        (
            source_replay,
            source_compacted,
            source_engine,
            source_dir_path,
        )
    };
    assert!(!source_dir_path.exists());

    let target_startup = fixture.startup(target_dir.path());
    let target_store = GenerationStore::new(&target_startup.wal_dir);
    validate_recovery_generation_anchor(&target_state, &target_startup, &source_compacted).unwrap();
    let (copied_records, copied_summary) =
        stage_recovery_generation_records(&target_store, &source_compacted).unwrap();
    assert_eq!(copied_records.len(), 36);
    assert!(copied_records.iter().all(|record| {
        record.kind != RetainedRecordKind::Commit
            && record.round > window.candidate.round
            && record.round <= window.last_round
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
    assert_eq!(copied_absence_count, 12);

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
    assert!(target_engine.committed_blocks().is_empty());
    assert_eq!(
        target_engine.find_committed_anchor(&canonical_tip.header.proof_hash, &fixture.domain),
        None
    );
    for round in window.bootstrap..=window.candidate.round {
        assert!(
            !target_engine
                .has_skip_certificate(round, &fixture.validators[PEER_REPLAY_TARGET_INDEX])
        );
        assert!(
            !target_engine
                .has_skip_certificate(round, &fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX])
        );
        assert!(!target_engine.has_quorum_parents_for_round(round));
    }
    for round in (window.candidate.round + 1)..=window.last_round {
        assert!(
            target_engine
                .has_skip_certificate(round, &fixture.validators[PEER_REPLAY_TARGET_INDEX])
        );
        assert!(
            target_engine
                .has_skip_certificate(round, &fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX])
        );
        assert!(target_engine.has_quorum_parents_for_round(round));
    }
    for round_blocks in &window.blocks_by_round[..2] {
        for block in round_blocks {
            assert!(target_engine.get_block(&block.hash).is_none());
        }
    }
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
    assert_eq!(restarted_records.len(), 36);
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
    assert!(target_restarted_engine.has_quorum_parents_for_round(window.last_round));
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

    let target_key = fixture.keypair(PEER_REPLAY_TARGET_INDEX);
    let next_round = window.last_round + 1;
    let mut expected_next_parents: Vec<Hash256> = window
        .blocks_by_round
        .last()
        .unwrap()
        .iter()
        .map(|block| block.hash)
        .collect();
    expected_next_parents.sort_by_key(|hash| hash.0);

    let target_live_block = target_restarted_engine
        .propose_block(Vec::new(), next_round)
        .unwrap();
    assert_eq!(
        target_live_block.author,
        fixture.validators[PEER_REPLAY_TARGET_INDEX]
    );
    assert_eq!(target_live_block.round, next_round);
    assert_eq!(target_live_block.parents, expected_next_parents);
    assert_eq!(
        target_live_block.hash,
        target_live_block.compute_hash_in_domain(&fixture.domain)
    );
    assert_eq!(
        target_live_block.ordering_commitment,
        DagBlock::compute_ordering_commitment(&target_live_block.transactions)
    );
    assert!(!target_live_block.signature.is_empty());
    assert!(
        target_restarted_engine
            .get_block(&target_live_block.hash)
            .is_some()
    );

    let target_signatures = [
        target_key.sign(&target_live_block.hash).unwrap(),
        bincode::deserialize(&target_live_block.signature).unwrap(),
    ];
    for signature in &target_signatures {
        signature
            .verify(&target_live_block.hash, &target_live_block.author)
            .unwrap();
    }
    assert!(
        target_signatures[1]
            .verify(
                &target_live_block.hash,
                &fixture.validators[PEER_REPLAY_SOURCE_INDEX],
            )
            .is_err()
    );

    assert!(matches!(
        target_restarted_engine.propose_block(Vec::new(), next_round),
        Err(arc_consensus::ConsensusError::DuplicateBlock)
    ));
    assert!(matches!(
        target_restarted_engine.receive_block(&target_live_block),
        Err(arc_consensus::ConsensusError::DuplicateBlock)
    ));

    let mut tampered_target_live = target_live_block.clone();
    *tampered_target_live.signature.last_mut().unwrap() ^= 0xff;
    assert!(matches!(
        source_engine.receive_block(&tampered_target_live),
        Err(arc_consensus::ConsensusError::InvalidSignature)
    ));
    assert!(source_engine.get_block(&target_live_block.hash).is_none());

    source_engine.receive_block(&target_live_block).unwrap();
    assert!(source_engine.get_block(&target_live_block.hash).is_some());
    assert!(matches!(
        source_engine.receive_block(&target_live_block),
        Err(arc_consensus::ConsensusError::DuplicateBlock)
    ));

    let source_live_block = source_engine.propose_block(Vec::new(), next_round).unwrap();
    assert_eq!(
        source_live_block.author,
        fixture.validators[PEER_REPLAY_SOURCE_INDEX]
    );
    assert_eq!(source_live_block.round, next_round);
    assert_eq!(source_live_block.parents, expected_next_parents);
    assert_eq!(
        source_live_block.hash,
        source_live_block.compute_hash_in_domain(&fixture.domain)
    );
    assert!(!source_live_block.signature.is_empty());
    assert!(matches!(
        source_engine.propose_block(Vec::new(), next_round),
        Err(arc_consensus::ConsensusError::DuplicateBlock)
    ));
    target_restarted_engine
        .receive_block(&source_live_block)
        .unwrap();

    for author_index in [1usize, 2, 3] {
        let live_block = fixture.signed_block(
            author_index,
            next_round,
            expected_next_parents.clone(),
            &fixture.domain,
        );
        target_restarted_engine.receive_block(&live_block).unwrap();
        source_engine.receive_block(&live_block).unwrap();
    }

    assert!(!target_restarted_engine.has_quorum_parents_for_round(next_round));
    assert!(!source_engine.has_quorum_parents_for_round(next_round));
    let _ = target_restarted_engine.advance_round();
    let _ = source_engine.advance_round();
    assert_eq!(target_restarted_engine.current_round(), next_round);
    assert_eq!(source_engine.current_round(), next_round);

    let next_round_live_signers = [0usize, 1, 2, 3, PEER_REPLAY_TARGET_INDEX];
    let next_round_live_stake = fixture.committee.stake_for_addresses(&[
        fixture.validators[0],
        fixture.validators[1],
        fixture.validators[2],
        fixture.validators[3],
        fixture.validators[PEER_REPLAY_TARGET_INDEX],
    ]);
    assert_eq!(next_round_live_stake, PEER_REPLAY_TOTAL_STAKE - 6_666_666);
    let next_round_second_absence = fixture.skip_certificate(
        fixture.domain,
        fixture.committee_hash,
        next_round,
        fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
        &next_round_live_signers,
    );
    assert_eq!(
        next_round_second_absence
            .verify(&fixture.domain, &fixture.committee)
            .unwrap(),
        next_round_live_stake
    );
    assert_eq!(
        target_restarted_engine
            .register_skip_certificate(next_round_second_absence.clone())
            .unwrap(),
        next_round_live_stake
    );
    assert_eq!(
        source_engine
            .register_skip_certificate(next_round_second_absence)
            .unwrap(),
        next_round_live_stake
    );
    assert!(target_restarted_engine.has_quorum_parents_for_round(next_round));
    assert!(source_engine.has_quorum_parents_for_round(next_round));
    let _ = target_restarted_engine.advance_round();
    let _ = source_engine.advance_round();
    assert_eq!(target_restarted_engine.current_round(), next_round + 1);
    assert_eq!(source_engine.current_round(), next_round + 1);

    let followup_block = target_restarted_engine
        .propose_block(Vec::new(), next_round + 1)
        .unwrap();
    assert_eq!(
        followup_block.author,
        fixture.validators[PEER_REPLAY_TARGET_INDEX]
    );
    assert_eq!(followup_block.round, next_round + 1);
    assert_eq!(followup_block.parents.len(), 5);
    assert!(followup_block.parents.contains(&target_live_block.hash));
    assert!(followup_block.parents.contains(&source_live_block.hash));
    assert_eq!(
        followup_block.hash,
        followup_block.compute_hash_in_domain(&fixture.domain)
    );
    assert!(!followup_block.signature.is_empty());
    source_engine.receive_block(&followup_block).unwrap();
    assert!(source_engine.get_block(&followup_block.hash).is_some());

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
    let (staged, staged_summary) = stage_recovery_generation_records(&store, &generation).unwrap();

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
    let (staged, staged_summary) = stage_recovery_generation_records(&store, &generation).unwrap();
    assert_eq!(staged.len(), 48);

    let bootstrap_leader_absence =
        window.absence(window.bootstrap, PEER_REPLAY_SECOND_ABSENT_INDEX);
    let target_leader_round = window.bootstrap + 5;
    let target_leader_absence = window.absence(target_leader_round, PEER_REPLAY_TARGET_INDEX);

    let mut subquorum_bootstrap = bootstrap_leader_absence.clone();
    subquorum_bootstrap.votes.truncate(3);
    peer_replay_expect_cert_err(
        subquorum_bootstrap.verify(&fixture.domain, &fixture.committee),
        CertificateError::BelowQuorum {
            signing: 20_000_001,
            quorum: PEER_REPLAY_STRICT_QUORUM,
        },
    );

    let mut subquorum_target = target_leader_absence.clone();
    subquorum_target.votes.truncate(3);
    peer_replay_expect_cert_err(
        subquorum_target.verify(&fixture.domain, &fixture.committee),
        CertificateError::BelowQuorum {
            signing: 20_000_001,
            quorum: PEER_REPLAY_STRICT_QUORUM,
        },
    );

    let mut empty_cert = bootstrap_leader_absence.clone();
    empty_cert.votes.clear();
    peer_replay_expect_cert_err(
        empty_cert.verify(&fixture.domain, &fixture.committee),
        CertificateError::Empty,
    );

    let mut duplicate_voter_cert = bootstrap_leader_absence.clone();
    duplicate_voter_cert.votes[3] = duplicate_voter_cert.votes[0].clone();
    peer_replay_expect_cert_err(
        duplicate_voter_cert.verify(&fixture.domain, &fixture.committee),
        CertificateError::DuplicateVoter(fixture.validators[0]),
    );

    let outsider = arc_crypto::KeyPair::from_ed25519_secret_bytes(
        &hash_bytes(b"recovery-peer-replay-outsider-key").0,
    );
    let mut outsider_cert = bootstrap_leader_absence.clone();
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

    let mut bad_sig_cert = bootstrap_leader_absence.clone();
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
        target_leader_round,
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

    assert_eq!(target_leader_round as usize % 6, PEER_REPLAY_TARGET_INDEX);
    let missing_leader_absence = peer_replay_remove_certificate_record(
        &staged,
        target_leader_round,
        fixture.validators[PEER_REPLAY_TARGET_INDEX],
    );
    let missing_leader_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
    let missing_leader_err = replay_recovery_dag_generation(
        &missing_leader_engine,
        &state,
        &startup,
        &generation,
        &missing_leader_absence,
    )
    .unwrap_err();
    assert!(
        missing_leader_err
            .to_string()
            .to_ascii_lowercase()
            .contains("insufficient parents"),
        "unexpected missing-leader error: {missing_leader_err}"
    );

    for (label, round, absentee_index) in [
        (
            "bootstrap-nonleader-target",
            window.bootstrap,
            PEER_REPLAY_TARGET_INDEX,
        ),
        (
            "live-leader-round-nonleader-target",
            window.bootstrap + 2,
            PEER_REPLAY_TARGET_INDEX,
        ),
        (
            "live-leader-round-nonleader-second",
            window.bootstrap + 2,
            PEER_REPLAY_SECOND_ABSENT_INDEX,
        ),
        (
            "absent-leader-round-nonleader-second",
            target_leader_round,
            PEER_REPLAY_SECOND_ABSENT_INDEX,
        ),
    ] {
        assert_ne!(
            round as usize % 6,
            absentee_index,
            "regression case {label} must remove a non-leader absence certificate"
        );
        let missing_nonleader = peer_replay_remove_certificate_record(
            &staged,
            round,
            fixture.validators[absentee_index],
        );
        let missing_nonleader_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
        let err = replay_recovery_dag_generation(
            &missing_nonleader_engine,
            &state,
            &startup,
            &generation,
            &missing_nonleader,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains("insufficient parents"),
            "missing non-leader absence {label} must fail with insufficient parents, got: {err}"
        );
    }

    let direct_nonleader_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
    assert!(direct_nonleader_engine.requires_full_round_participation());
    direct_nonleader_engine
        .install_recovery_cursor(startup.binding.source_consensus_round)
        .unwrap();
    direct_nonleader_engine
        .install_recovery_generation_cursor(window.bootstrap, window.bootstrap, window.bootstrap)
        .unwrap();
    direct_nonleader_engine
        .register_skip_certificate(bootstrap_leader_absence.clone())
        .unwrap();
    for block in &window.blocks_by_round[0] {
        direct_nonleader_engine.receive_block(block).unwrap();
        let _ = direct_nonleader_engine.advance_round();
    }
    assert!(!direct_nonleader_engine.has_quorum_parents_for_round(window.bootstrap));
    assert_eq!(direct_nonleader_engine.current_round(), window.bootstrap);
    assert!(matches!(
        direct_nonleader_engine.receive_block(&window.blocks_by_round[1][0]),
        Err(arc_consensus::ConsensusError::InsufficientParents)
    ));
    direct_nonleader_engine
        .register_skip_certificate(
            window
                .absence(window.bootstrap, PEER_REPLAY_TARGET_INDEX)
                .clone(),
        )
        .unwrap();
    assert!(direct_nonleader_engine.has_quorum_parents_for_round(window.bootstrap));
    let _ = direct_nonleader_engine.advance_round();
    assert_eq!(
        direct_nonleader_engine.current_round(),
        window.bootstrap + 1
    );
    direct_nonleader_engine
        .receive_block(&window.blocks_by_round[1][0])
        .unwrap();

    let source_engine = fixture.strict_engine(PEER_REPLAY_SOURCE_INDEX);
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
    let (compacted_records, _) = stage_recovery_generation_records(&store, &compacted).unwrap();
    assert_eq!(compacted_records.len(), 36);

    for (label, round, absentee_index) in [
        (
            "compacted-boundary-nonleader-target",
            window.bootstrap + 2,
            PEER_REPLAY_TARGET_INDEX,
        ),
        (
            "compacted-nonleader-second",
            target_leader_round,
            PEER_REPLAY_SECOND_ABSENT_INDEX,
        ),
    ] {
        assert_ne!(
            round as usize % 6,
            absentee_index,
            "compacted regression case {label} must remove a non-leader absence certificate"
        );
        let missing_compacted_nonleader = peer_replay_remove_certificate_record(
            &compacted_records,
            round,
            fixture.validators[absentee_index],
        );
        let compacted_target_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
        let err = replay_recovery_dag_generation(
            &compacted_target_engine,
            &state,
            &startup,
            &compacted,
            &missing_compacted_nonleader,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains("insufficient parents"),
            "compacted missing non-leader absence {label} must fail with insufficient parents, got: {err}"
        );
    }

    assert_ne!(
        window.last_round as usize % 6,
        PEER_REPLAY_SECOND_ABSENT_INDEX
    );
    let tail_parents: Vec<Hash256> = window
        .blocks_by_round
        .last()
        .unwrap()
        .iter()
        .map(|block| block.hash)
        .collect();
    let next_round_block = fixture.signed_block(
        PEER_REPLAY_SOURCE_INDEX,
        window.last_round + 1,
        tail_parents,
        &fixture.domain,
    );

    let missing_last_round_nonleader = peer_replay_remove_certificate_record(
        &staged,
        window.last_round,
        fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
    );
    let tail_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
    let tail_replay = replay_recovery_dag_generation(
        &tail_engine,
        &state,
        &startup,
        &generation,
        &missing_last_round_nonleader,
    )
    .unwrap();
    assert_eq!(tail_replay.current_round, window.last_round);
    assert!(!tail_engine.has_quorum_parents_for_round(window.last_round));
    assert!(matches!(
        tail_engine.receive_block(&next_round_block),
        Err(arc_consensus::ConsensusError::InsufficientParents)
    ));

    let missing_compacted_tail_nonleader = peer_replay_remove_certificate_record(
        &compacted_records,
        window.last_round,
        fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
    );
    let compacted_tail_engine = fixture.strict_engine(PEER_REPLAY_TARGET_INDEX);
    replay_recovery_dag_generation(
        &compacted_tail_engine,
        &state,
        &startup,
        &compacted,
        &missing_compacted_tail_nonleader,
    )
    .unwrap();
    assert!(!compacted_tail_engine.has_quorum_parents_for_round(window.last_round));
    assert!(matches!(
        compacted_tail_engine.propose_block(Vec::new(), window.last_round + 1),
        Err(arc_consensus::ConsensusError::InsufficientParents)
    ));
    assert!(matches!(
        compacted_tail_engine.receive_block(&next_round_block),
        Err(arc_consensus::ConsensusError::InsufficientParents)
    ));
}

fn peer_qualify_sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn peer_qualify_flip_hex(hex_str: &str) -> String {
    let mut chars: Vec<char> = hex_str.chars().collect();
    chars[0] = if chars[0] == '0' { '1' } else { '0' };
    chars.into_iter().collect()
}

fn peer_qualify_write_manifest(path: &Path, manifest: &PeerRecoveryCandidateManifest) -> String {
    let bytes = serde_json::to_vec(manifest).unwrap();
    std::fs::write(path, &bytes).unwrap();
    peer_qualify_sha256_bytes(&bytes)
}

fn peer_qualify_file_snapshot(path: &Path) -> (String, u64) {
    let input = recovery_read_only_input(path, "qualification snapshot").unwrap();
    (input.sha256, input.size)
}

fn peer_qualify_expect_err(result: Result<()>, expected_substring: &str) {
    let error = result.expect_err("expected qualification failure");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains(expected_substring),
        "expected error containing {expected_substring:?}, got {rendered:?}"
    );
}

fn peer_qualify_write_genesis(path: &Path, validators: &[Hash256; 6]) {
    let mut toml = String::from(
        "[chain]\nname = \"arc-qualify-peer-state-e2e\"\nchain_id = \"0x415243\"\n\
         validator_set_complete = true\ninstance_id = \"arc-qualify-peer-state-e2e\"\n\n",
    );
    for address in validators {
        toml.push_str(&format!(
            "[[accounts]]\naddress = \"{}\"\nbalance = 1_000_000_000_000\n\n",
            address.to_hex()
        ));
    }
    for (index, address) in validators.iter().enumerate() {
        toml.push_str(&format!(
            "[[validators]]\naddress = \"{}\"\nstake = {}\n\n",
            address.to_hex(),
            PEER_REPLAY_WEIGHTS[index]
        ));
    }
    std::fs::write(path, toml).unwrap();
}

fn peer_qualify_assert_no_key_files(dir: &Path) {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in std::fs::read_dir(&current).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            assert!(!meta.file_type().is_symlink());
            if meta.is_dir() {
                pending.push(path);
            } else {
                assert!(meta.is_file());
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                assert!(
                    !name.ends_with(".key")
                        && !name.ends_with(".key.json")
                        && !name.contains("secret")
                );
            }
        }
    }
}

#[test]
fn recovery_peer_qualify_command_accepts_persistent_candidate_and_rejects_mutations() {
    use std::collections::HashMap;

    use arc_consensus::ConsensusDomain;
    use arc_consensus::view_change::{
        AbsenceReason, ConsensusSigningRecord, FinalityCertificate, FinalityVote,
        validator_set_hash,
    };
    use arc_state::recovery::{
        ArcCheckpoint, RecoveryExportSpec, RecoveryImport, RecoveryValidator,
        recovery_stake_reserve_address,
    };
    use arc_types::transaction::{JoinValidatorBody, Transaction, TxBody};

    let recovery_epoch = 1u64;
    let validator_set_id = 1u64;
    let mut fixture = PeerReplayFixture::new(b"qualify-peer-state-e2e-placeholder");
    let target_validator = fixture.validators[PEER_REPLAY_TARGET_INDEX];

    let root = peer_replay_temp_dir("arc-qualify-peer-e2e-");
    let genesis_path = root.path().join("genesis.toml");
    peer_qualify_write_genesis(&genesis_path, &fixture.validators);
    let (genesis, recovery_network) = recovery_network_from_genesis(
        genesis_path.to_str().unwrap(),
        recovery_epoch,
        validator_set_id,
    )
    .unwrap();
    let genesis_accounts = qualification_genesis_accounts(&genesis).unwrap();

    let mut funds = genesis_accounts.clone();
    funds.push((recovery_stake_reserve_address(), PEER_REPLAY_TOTAL_STAKE));
    let source_genesis_state = StateDB::with_genesis(&funds);
    let joins: Vec<Transaction> = (0..6usize)
        .map(|index| {
            let key = fixture.keypair(index);
            let mut tx = Transaction::new_transfer(key.address(), key.address(), 0, 0);
            tx.body = TxBody::JoinValidator(JoinValidatorBody {
                pubkey: key.public_key_bytes().try_into().unwrap(),
                initial_stake: PEER_REPLAY_WEIGHTS[index],
            });
            tx.tx_type = tx.body.tx_type();
            tx.sign(&key).unwrap();
            tx
        })
        .collect();
    let (_, receipts) = source_genesis_state
        .execute_block_adaptive_at_with_proof(
            &joins,
            fixture.validators[PEER_REPLAY_SOURCE_INDEX],
            1,
            Hash256::ZERO,
        )
        .unwrap();
    assert!(receipts.iter().all(|receipt| receipt.success));
    for address in &fixture.validators {
        let mut account = source_genesis_state.get_account(address).unwrap();
        account.staked_balance = 0;
        source_genesis_state.update_account(address, account);
    }
    source_genesis_state
        .execute_block_adaptive_at_with_proof(
            &[],
            fixture.validators[PEER_REPLAY_SOURCE_INDEX],
            2,
            Hash256::ZERO,
        )
        .unwrap();

    let recovery_validators: Vec<RecoveryValidator> = (0..6usize)
        .map(|index| {
            let key = fixture.keypair(index);
            RecoveryValidator {
                address: key.address(),
                public_key: key.public_key_bytes().try_into().unwrap(),
                stake: PEER_REPLAY_WEIGHTS[index],
            }
        })
        .collect();
    let mut checkpoint = ArcCheckpoint::export_unsigned(
        &source_genesis_state,
        RecoveryExportSpec {
            chain_id: recovery_network.chain_id.clone(),
            genesis_hash: recovery_network.genesis_hash,
            source_consensus_round: 100,
            recovery_epoch,
            validator_set_id,
            validators: recovery_validators,
            community_rewards_v1_activation_height: recovery_network
                .community_rewards_v1_activation_height,
            created_at_unix_ms: 3,
        },
    )
    .unwrap();
    for index in 0..6usize {
        checkpoint.add_signature(&fixture.keypair(index)).unwrap();
    }
    let manifest_hash = checkpoint.manifest_hash();
    let manifest_hex = manifest_hash.to_hex();
    let approved_checkpoint_path = root.path().join("approved.arcchkpt");
    checkpoint.write_to(&approved_checkpoint_path).unwrap();

    let source_dir = root.path().join("source-lhr");
    let candidate_dir = root
        .path()
        .join(format!("{PEER_RECOVERY_CANDIDATE_DIR_PREFIX}ams"));
    std::fs::create_dir(&source_dir).unwrap();
    std::fs::create_dir(&candidate_dir).unwrap();

    let mut source_lock = acquire_node_data_dir_lock(&source_dir, None).unwrap();
    let source_state = StateDB::with_genesis_persistent_recovery_in_prepared_directory(
        &genesis_accounts,
        source_lock.prepared_directory(),
        recovery_network.clone(),
        Some(RecoveryImport {
            checkpoint_path: approved_checkpoint_path.clone(),
            approved_manifest_hash: manifest_hash,
        }),
    )
    .unwrap();
    source_lock.finish_persistent_state_startup();
    source_state.set_community_rewards_v1_activation_height(
        recovery_network.community_rewards_v1_activation_height,
    );

    let active_context = source_state.recovery_context().unwrap();
    assert_eq!(source_state.recovery_manifest_hash(), Some(manifest_hash));
    assert_eq!(active_context.recovery_epoch, recovery_epoch);
    assert_eq!(active_context.validator_set_id, validator_set_id);
    let actual_domain = ConsensusDomain::new(
        active_context.domain_hash(),
        active_context.recovery_epoch,
        active_context.validator_set_id,
    );
    assert_eq!(
        actual_domain.domain_hash,
        checkpoint.manifest.recovery_context().domain_hash()
    );

    let (target_stake, peer_validators) =
        qualification_split_target_and_peers(&recovery_network.validators, target_validator)
            .unwrap();
    let probe_consensus =
        ConsensusManager::new(target_validator, target_stake, 0, false, &peer_validators);
    install_startup_consensus_domains(
        &probe_consensus.engine,
        recovery_network.genesis_hash,
        Some(actual_domain),
    )
    .unwrap();
    let committee = probe_consensus.engine.frozen_validator_set().clone();
    let committee_hash = validator_set_hash(&committee);
    assert_eq!(committee_hash, fixture.committee_hash);
    drop(probe_consensus);

    fixture.domain = actual_domain;
    let window = fixture.build_live_quorum_window();
    let transition_height = checkpoint.manifest.source_height + 1;
    assert_eq!(source_state.height(), transition_height);
    let baseline_block = source_state.get_block(transition_height).unwrap();

    let (certified_block, _) = source_state
        .execute_block_adaptive_at_with_proof(
            &[],
            window.candidate.author,
            window.candidate.round,
            window.candidate.state_decision_commitment(&actual_domain),
        )
        .unwrap();
    source_state.try_sync_wal().unwrap();
    let certified_height = transition_height + 1;
    assert_eq!(source_state.height(), certified_height);
    assert_eq!(
        certified_block.hash,
        source_state.get_block(certified_height).unwrap().hash
    );
    assert_eq!(
        certified_block.header.state_root,
        source_state.get_state_root()
    );
    assert!(
        std::fs::metadata(source_dir.join("state.wal"))
            .unwrap()
            .len()
            > 0
    );
    let certified_tx_root = qualification_block_tx_root(&certified_block);

    let source_startup = prepare_recovery_dag_startup(&source_dir, &source_state)
        .unwrap()
        .unwrap();
    assert_eq!(source_startup.binding.manifest_hash, manifest_hash);
    assert_eq!(source_startup.binding.consensus_domain, actual_domain);
    assert_eq!(source_startup.binding.transition_height, transition_height);
    assert_eq!(
        source_startup.binding.initial_consensus_round,
        window.bootstrap
    );
    assert!(source_startup.archived_legacy_wal.is_none());

    let source_gen_binding = generation_dag_binding(&source_startup).unwrap();
    let source_store = GenerationStore::new(&source_startup.wal_dir);
    let source_initial_generation = source_store
        .create_initial(
            GenerationInput {
                binding: source_gen_binding.clone(),
                baseline_state: DagBaselineState {
                    height: transition_height,
                    block_hash: baseline_block.hash,
                    state_root: baseline_block.header.state_root,
                    dag_anchor: None,
                },
                dag_cursor: DagCursor {
                    committed_block_count: 0,
                    next_dag_round: window.bootstrap,
                    current_round: window.bootstrap,
                    retention_floor_round: window.bootstrap,
                    retention_ceiling_round: recovery_retention_ceiling(window.bootstrap).unwrap(),
                },
                retention_limits: RetentionLimits::default(),
            },
            window.records.clone(),
        )
        .unwrap();
    validate_recovery_generation_anchor(&source_state, &source_startup, &source_initial_generation)
        .unwrap();
    let (staged, staged_summary) =
        stage_recovery_generation_records(&source_store, &source_initial_generation).unwrap();
    let source_engine = fixture.strict_engine(PEER_REPLAY_SOURCE_INDEX);
    let source_replay = replay_recovery_dag_generation(
        &source_engine,
        &source_state,
        &source_startup,
        &source_initial_generation,
        &staged,
    )
    .unwrap();
    assert_eq!(
        source_replay.repaired_commit,
        Some((window.candidate.hash, window.candidate.round))
    );
    assert_eq!(source_replay.current_round, window.last_round + 1);
    let source_compacted = compact_replayed_recovery_generation(
        &source_store,
        &source_state,
        &source_startup,
        &source_initial_generation,
        &staged_summary,
        &source_engine,
        &staged,
    )
    .unwrap();
    assert_ne!(source_compacted.pin, source_initial_generation.pin);
    let source_audit = source_store.audit(&source_gen_binding).unwrap();
    assert!(matches!(source_audit.status, StoreAuditStatus::Clean));
    assert_eq!(source_audit.current.pin, source_compacted.pin);

    let source_wal_dir = source_startup.wal_dir.clone();
    source_state.try_sync_wal().unwrap();
    drop(source_engine);
    drop(source_state);
    sync_directory(&source_wal_dir).unwrap();
    sync_directory(&source_dir).unwrap();
    drop(source_lock);

    peer_replay_copy_dir(&source_dir, &candidate_dir);
    write_recovery_dag_pin_atomically(&candidate_dir, manifest_hash, source_compacted.pin).unwrap();

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
    peer_replay_write_signing_record(
        &source_dir.join("consensus-signing-record.bin"),
        &source_signing_record,
    );

    let prior_target_height = transition_height;
    let prior_target_highest_dag_round = window.bootstrap - 1;
    let target_skip_round = window.bootstrap - 6;
    let target_signing_record = ConsensusSigningRecord {
        skipped_rounds: HashMap::from([(
            target_skip_round,
            HashMap::from([(fixture.validators[1], AbsenceReason::NoBlock)]),
        )]),
        finality_votes: HashMap::from([(
            prior_target_height,
            (
                baseline_block.hash,
                baseline_block.header.state_root,
                qualification_block_tx_root(&baseline_block),
            ),
        )]),
        last_applied_round: Some(target_skip_round),
    };
    peer_replay_write_signing_record(
        &candidate_dir.join("consensus-signing-record.bin"),
        &target_signing_record,
    );

    let make_finality_cert = |domain: ConsensusDomain,
                              signer_indices: &[usize],
                              block_hash: Hash256,
                              state_root: Hash256,
                              tx_root: Hash256|
     -> FinalityCertificate {
        let votes = signer_indices
            .iter()
            .map(|&index| {
                FinalityVote::sign(
                    domain,
                    committee_hash,
                    certified_height,
                    block_hash,
                    state_root,
                    tx_root,
                    &fixture.keypair(index),
                )
                .unwrap()
            })
            .collect();
        FinalityCertificate {
            domain,
            validator_set_hash: committee_hash,
            height: certified_height,
            block_hash,
            state_root,
            tx_root,
            votes,
        }
    };
    let valid_cert = make_finality_cert(
        actual_domain,
        &[0, 1, 2, 3],
        certified_block.hash,
        certified_block.header.state_root,
        certified_tx_root,
    );
    assert_eq!(
        valid_cert.verify(&actual_domain, &committee),
        Ok(PEER_REPLAY_STRICT_QUORUM)
    );
    let cert_path = root.path().join("finality.bin");
    std::fs::write(&cert_path, bincode::serialize(&valid_cert).unwrap()).unwrap();

    let candidate_wal_path = candidate_dir.join("state.wal");
    let candidate_marker_path = candidate_dir.join("recovery.active");
    let candidate_sign_path = candidate_dir.join("consensus-signing-record.bin");
    let candidate_ckpt_path = candidate_dir.join(format!("recovery-{manifest_hex}.arcchkpt"));
    let candidate_current_path = candidate_dir
        .join(format!("dag-wal-recovery-{manifest_hex}"))
        .join("CURRENT");
    let candidate_pin_path = recovery_dag_pin_path(&candidate_dir, manifest_hash);

    let wal_before = peer_qualify_file_snapshot(&candidate_wal_path);
    let marker_before = peer_qualify_file_snapshot(&candidate_marker_path);
    let sign_before = peer_qualify_file_snapshot(&candidate_sign_path);
    let ckpt_before = peer_qualify_file_snapshot(&candidate_ckpt_path);
    let current_before = peer_qualify_file_snapshot(&candidate_current_path);
    let pin_before = peer_qualify_file_snapshot(&candidate_pin_path);
    let genesis_before = peer_qualify_file_snapshot(&genesis_path);
    let cert_before = peer_qualify_file_snapshot(&cert_path);
    assert!(wal_before.1 > 0 && marker_before.1 > 0 && sign_before.1 > 0);
    peer_qualify_assert_no_key_files(&candidate_dir);

    let valid_manifest = PeerRecoveryCandidateManifest {
        schema: PEER_RECOVERY_CANDIDATE_SCHEMA.to_string(),
        candidate_data_dir: candidate_dir.display().to_string(),
        source_data_dir: source_dir.display().to_string(),
        expected_state_wal_sha256: wal_before.0.clone(),
        genesis_path: genesis_path.display().to_string(),
        expected_genesis_sha256: genesis_before.0.clone(),
        expected_recovery_manifest_hash: manifest_hex.clone(),
        recovery_epoch,
        validator_set_id,
        target_validator: target_validator.to_hex(),
        expected_target_signing_record_sha256: sign_before.0.clone(),
        prior_target_highest_dag_round,
        prior_target_height,
        finality_certificate_path: cert_path.display().to_string(),
        expected_finality_certificate_sha256: cert_before.0.clone(),
    };
    let manifest_path = root.path().join("candidate-manifest.json");
    let manifest_sha = peer_qualify_write_manifest(&manifest_path, &valid_manifest);

    run_qualify_peer_state_command(manifest_path.to_str().unwrap(), &manifest_sha).unwrap();

    assert_eq!(peer_qualify_file_snapshot(&candidate_wal_path), wal_before);
    assert_eq!(
        peer_qualify_file_snapshot(&candidate_marker_path),
        marker_before
    );
    assert_eq!(
        peer_qualify_file_snapshot(&candidate_sign_path),
        sign_before
    );
    assert_eq!(
        peer_qualify_file_snapshot(&candidate_ckpt_path),
        ckpt_before
    );
    assert_eq!(
        peer_qualify_file_snapshot(&candidate_current_path),
        current_before
    );
    assert_eq!(peer_qualify_file_snapshot(&candidate_pin_path), pin_before);
    assert_eq!(peer_qualify_file_snapshot(&genesis_path), genesis_before);
    assert_eq!(peer_qualify_file_snapshot(&cert_path), cert_before);
    assert_eq!(
        read_recovery_dag_pin(&candidate_dir, manifest_hash)
            .unwrap()
            .unwrap()
            .generation,
        source_compacted.pin
    );
    peer_qualify_assert_no_key_files(&candidate_dir);

    assert!(
        run_qualify_peer_state_command(
            manifest_path.to_str().unwrap(),
            &peer_qualify_flip_hex(&manifest_sha),
        )
        .is_err()
    );

    let run_with_manifest = |label: &str, manifest: &PeerRecoveryCandidateManifest| -> Result<()> {
        let path = root.path().join(format!("manifest-{label}.json"));
        let sha = peer_qualify_write_manifest(&path, manifest);
        run_qualify_peer_state_command(path.to_str().unwrap(), &sha)
    };

    let mut bad_prior_height = valid_manifest.clone();
    bad_prior_height.prior_target_height = certified_height;
    peer_qualify_expect_err(
        run_with_manifest("bad-prior-height", &bad_prior_height),
        "does not exceed prior_target_height",
    );

    let mut bad_prior_round_low = valid_manifest.clone();
    bad_prior_round_low.prior_target_highest_dag_round = target_skip_round - 1;
    peer_qualify_expect_err(
        run_with_manifest("bad-prior-round-low", &bad_prior_round_low),
        "below preserved target signing history",
    );

    let mut bad_prior_round_high = valid_manifest.clone();
    bad_prior_round_high.prior_target_highest_dag_round = window.last_round + 1;
    peer_qualify_expect_err(
        run_with_manifest("bad-prior-round-high", &bad_prior_round_high),
        "does not exceed prior_target_highest_dag_round",
    );

    let mut bad_wal_sha = valid_manifest.clone();
    bad_wal_sha.expected_state_wal_sha256 = peer_qualify_flip_hex(&wal_before.0);
    assert!(run_with_manifest("bad-wal-sha", &bad_wal_sha).is_err());

    let run_with_cert = |label: &str, cert: &FinalityCertificate| -> Result<()> {
        let path = root.path().join(format!("cert-{label}.bin"));
        let bytes = bincode::serialize(cert).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let mut manifest = valid_manifest.clone();
        manifest.finality_certificate_path = path.display().to_string();
        manifest.expected_finality_certificate_sha256 = peer_qualify_sha256_bytes(&bytes);
        run_with_manifest(label, &manifest)
    };

    let mut subquorum_cert = valid_cert.clone();
    subquorum_cert.votes.truncate(3);
    peer_qualify_expect_err(
        run_with_cert("subquorum", &subquorum_cert),
        "finality certificate failed domain/committee/quorum verification",
    );

    let mut forged_cert = valid_cert.clone();
    forged_cert.votes[0].signature = forged_cert.votes[1].signature.clone();
    peer_qualify_expect_err(
        run_with_cert("forged-cert", &forged_cert),
        "finality certificate failed domain/committee/quorum verification",
    );

    let wrong_domain = ConsensusDomain::new(
        hash_bytes(b"qualify-peer-state-e2e-wrong-domain"),
        recovery_epoch,
        validator_set_id,
    );
    let wrong_domain_cert = make_finality_cert(
        wrong_domain,
        &[0, 1, 2, 3],
        certified_block.hash,
        certified_block.header.state_root,
        certified_tx_root,
    );
    peer_qualify_expect_err(
        run_with_cert("wrong-domain-cert", &wrong_domain_cert),
        "finality certificate failed domain/committee/quorum verification",
    );

    let wrong_tip_cert = make_finality_cert(
        actual_domain,
        &[0, 1, 2, 3],
        hash_bytes(b"qualify-peer-state-e2e-wrong-tip"),
        certified_block.header.state_root,
        certified_tx_root,
    );
    peer_qualify_expect_err(
        run_with_cert("wrong-tip-cert", &wrong_tip_cert),
        "candidate canonical block hash",
    );

    let run_mutated_dag = |label: &str, records: Vec<RetainedDagRecord>| -> Result<()> {
        let mut_dir = root
            .path()
            .join(format!("{PEER_RECOVERY_CANDIDATE_DIR_PREFIX}{label}"));
        peer_replay_copy_dir(&candidate_dir, &mut_dir);
        let mut_dag_dir = mut_dir.join(format!("dag-wal-recovery-{manifest_hex}"));
        std::fs::remove_dir_all(&mut_dag_dir).unwrap();
        let mut_store = GenerationStore::new(&mut_dag_dir);
        let mut_gen = mut_store
            .create_initial(
                GenerationInput {
                    binding: source_gen_binding.clone(),
                    baseline_state: DagBaselineState {
                        height: transition_height,
                        block_hash: baseline_block.hash,
                        state_root: baseline_block.header.state_root,
                        dag_anchor: None,
                    },
                    dag_cursor: DagCursor {
                        committed_block_count: 0,
                        next_dag_round: window.bootstrap,
                        current_round: window.bootstrap,
                        retention_floor_round: window.bootstrap,
                        retention_ceiling_round: recovery_retention_ceiling(window.bootstrap)
                            .unwrap(),
                    },
                    retention_limits: RetentionLimits::default(),
                },
                records,
            )
            .unwrap();
        let original_binding = candidate_dir
            .join(format!("dag-wal-recovery-{manifest_hex}"))
            .join("recovery-dag.binding.json");
        let mutation_binding = mut_dag_dir.join("recovery-dag.binding.json");
        std::fs::copy(&original_binding, &mutation_binding).unwrap();
        assert_eq!(
            peer_qualify_file_snapshot(&mutation_binding),
            peer_qualify_file_snapshot(&original_binding),
        );
        write_recovery_dag_pin_atomically(&mut_dir, manifest_hash, mut_gen.pin).unwrap();
        let mut manifest = valid_manifest.clone();
        manifest.candidate_data_dir = mut_dir.display().to_string();
        run_with_manifest(label, &manifest)
    };

    run_mutated_dag("uncompacted-control", window.records.clone()).unwrap();

    let target_block = window.blocks_by_round.last().unwrap()[0].clone();
    let mut forged_dag_block = target_block.clone();
    forged_dag_block.signature =
        bincode::serialize(&fixture.keypair(1).sign(&forged_dag_block.hash).unwrap()).unwrap();
    peer_qualify_expect_err(
        run_mutated_dag(
            "forged-dag-sig",
            peer_replay_replace_block_record(
                &window.records,
                target_block.round,
                target_block.author,
                &forged_dag_block,
            ),
        ),
        "signature failed verification",
    );

    let mut unsigned_dag_block = target_block.clone();
    unsigned_dag_block.signature.clear();
    peer_qualify_expect_err(
        run_mutated_dag(
            "unsigned-dag-block",
            peer_replay_replace_block_record(
                &window.records,
                target_block.round,
                target_block.author,
                &unsigned_dag_block,
            ),
        ),
        "has an empty signature",
    );

    let wrong_domain_dag_block = fixture.signed_block(
        PEER_REPLAY_SOURCE_INDEX,
        target_block.round,
        target_block.parents.clone(),
        &wrong_domain,
    );
    peer_qualify_expect_err(
        run_mutated_dag(
            "wrong-domain-dag-block",
            peer_replay_replace_block_record(
                &window.records,
                target_block.round,
                target_block.author,
                &wrong_domain_dag_block,
            ),
        ),
        "failed active recovery domain hash verification",
    );

    for absentee in [
        target_validator,
        fixture.validators[PEER_REPLAY_SECOND_ABSENT_INDEX],
    ] {
        assert!(
            run_mutated_dag(
                &format!("missing-parent-skip-cert-{}", absentee.to_hex()),
                peer_replay_remove_certificate_record(
                    &window.records,
                    window.last_round - 1,
                    absentee,
                ),
            )
            .is_err()
        );
    }
    run_mutated_dag(
        "partial-current-round-with-complete-parent",
        peer_replay_remove_certificate_record(&window.records, window.last_round, target_validator),
    )
    .unwrap();

    let run_pin_mutation = |label: &str, pin: Option<GenerationPin>| -> Result<()> {
        let mut_dir = root
            .path()
            .join(format!("{PEER_RECOVERY_CANDIDATE_DIR_PREFIX}{label}"));
        peer_replay_copy_dir(&candidate_dir, &mut_dir);
        match pin {
            Some(stale_pin) => {
                write_recovery_dag_pin_atomically(&mut_dir, manifest_hash, stale_pin).unwrap();
            }
            None => {
                std::fs::remove_file(recovery_dag_pin_path(&mut_dir, manifest_hash)).unwrap();
            }
        }
        let mut manifest = valid_manifest.clone();
        manifest.candidate_data_dir = mut_dir.display().to_string();
        run_with_manifest(label, &manifest)
    };

    peer_qualify_expect_err(
        run_pin_mutation("missing-dag-pin", None),
        "external pin is missing",
    );
    peer_qualify_expect_err(
        run_pin_mutation("stale-dag-pin", Some(source_initial_generation.pin)),
        "failed binding/pin verification",
    );

    assert_eq!(peer_qualify_file_snapshot(&candidate_wal_path), wal_before);
    assert_eq!(
        peer_qualify_file_snapshot(&candidate_marker_path),
        marker_before
    );
    assert_eq!(
        peer_qualify_file_snapshot(&candidate_sign_path),
        sign_before
    );
}
