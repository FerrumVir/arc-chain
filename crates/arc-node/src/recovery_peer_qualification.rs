const PEER_RECOVERY_CANDIDATE_SCHEMA: &str = "arc.peer-recovery.candidate.v1";
const PEER_RECOVERY_QUALIFICATION_REPORT_SCHEMA: &str = "arc.peer-recovery.qualification-report.v1";
const PEER_RECOVERY_CANDIDATE_DIR_PREFIX: &str = "peer-recovery-candidate-";
const MAX_QUALIFICATION_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_QUALIFICATION_CERTIFICATE_BYTES: u64 = 64 * 1024;
const MAX_QUALIFICATION_SIGNING_RECORD_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PeerRecoveryCandidateManifest {
    schema: String,
    candidate_data_dir: String,
    source_data_dir: String,
    expected_state_wal_sha256: String,
    genesis_path: String,
    expected_genesis_sha256: String,
    expected_recovery_manifest_hash: String,
    recovery_epoch: u64,
    validator_set_id: u64,
    target_validator: String,
    expected_target_signing_record_sha256: String,
    prior_target_highest_dag_round: u64,
    prior_target_height: u64,
    finality_certificate_path: String,
    expected_finality_certificate_sha256: String,
}

#[derive(Debug, Clone)]
struct ValidatedCandidateManifest {
    manifest: PeerRecoveryCandidateManifest,
    candidate_data_dir: PathBuf,
    source_data_dir: PathBuf,
    genesis_path: PathBuf,
    finality_certificate_path: PathBuf,
    expected_recovery_manifest_hash: Hash256,
    target_validator: Hash256,
}

#[derive(Debug, Serialize)]
struct PeerStateQualificationReport {
    schema: &'static str,
    status: &'static str,
    manifest_sha256: String,
    candidate_data_dir: String,
    source_data_dir: String,
    target_validator: String,
    target_stake: u64,
    committee_size: usize,
    committee_total_stake: u64,
    committee_quorum: u64,
    genesis_hash: String,
    recovery_manifest_hash: String,
    recovery_epoch: u64,
    validator_set_id: u64,
    consensus_domain_hash: String,
    prior_target_height: u64,
    qualified_state_height: u64,
    certified_block_hash: String,
    certified_state_root: String,
    certified_tx_root: String,
    finality_certificate_signing_stake: u64,
    prior_target_highest_dag_round: u64,
    restored_dag_current_round: u64,
    restored_dag_next_commit_round: u64,
    generation_pin_sequence: u64,
    generation_pin_hash: String,
    verified_retained_dag_blocks: usize,
    repaired_tail_commit_round: Option<u64>,
    state_wal_sha256_unchanged: String,
    target_signing_record_sha256_unchanged: String,
    target_signing_record_skipped_rounds: usize,
    target_signing_record_finality_votes: usize,
    target_signing_record_last_applied_round: Option<u64>,
    qualified_checks: Vec<&'static str>,
    unqualified_limitations: Vec<&'static str>,
}

fn parse_strict_hash256(encoded: &str, label: &str) -> Result<Hash256> {
    let hex_body = encoded.strip_prefix("0x").unwrap_or(encoded);
    ensure!(
        hex_body.len() == 64 && hex_body.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{label} must be 64 hex characters"
    );
    let raw = hex::decode(hex_body).with_context(|| format!("failed to decode {label} hex"))?;
    let bytes: [u8; 32] = raw
        .try_into()
        .map_err(|_| anyhow::anyhow!("{label} did not decode to 32 bytes"))?;
    let hash = Hash256(bytes);
    ensure!(hash != Hash256::ZERO, "{label} must be non-zero");
    Ok(hash)
}

fn validate_expected_sha256_hex(encoded: &str, label: &str) -> Result<()> {
    ensure!(
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
        "{label} must be 64 lowercase hex characters"
    );
    Ok(())
}

fn read_bounded_regular_file_with_hash(
    path: &Path,
    max_bytes: u64,
    expected_sha256: &str,
    label: &str,
) -> Result<(Vec<u8>, RecoveryReadOnlyInput)> {
    use sha2::{Digest, Sha256};

    let initial_metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        initial_metadata.is_file()
            && initial_metadata.len() > 0
            && initial_metadata.len() <= max_bytes,
        "{label} must be a regular file within 1..={max_bytes} bytes before hashing"
    );
    let observed = recovery_read_only_input(path, label)?;
    ensure!(
        observed.size > 0 && observed.size <= max_bytes,
        "{label} size {} is outside 1..={} bytes",
        observed.size,
        max_bytes
    );
    require_recovery_input_hash(&observed, expected_sha256, label)?;

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to no-follow open {label} {}", path.display()))?;
    let meta = file
        .metadata()
        .with_context(|| format!("failed to inspect open {label} {}", path.display()))?;
    ensure!(meta.is_file(), "{label} is not a regular file");
    let projection = recovery_metadata_projection(&meta)?;
    ensure!(projection.5 == 1, "{label} must have exactly one hard link");

    let mut bytes = Vec::with_capacity(observed.size.min(max_bytes) as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {label} {}", path.display()))?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= max_bytes,
        "{label} exceeded bounded read limit of {max_bytes} bytes"
    );
    let actual_sha256 = hex::encode(Sha256::digest(&bytes));
    ensure!(
        actual_sha256 == observed.sha256,
        "{label} content changed between hash audit and bounded read"
    );
    Ok((bytes, observed))
}

fn ensure_non_symlink_directory(path: &Path, label: &str) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty(),
        "{label} path must not be empty"
    );
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {label} {}", path.display()))?;
    ensure!(
        meta.file_type().is_dir() && !meta.file_type().is_symlink(),
        "{label} must be an existing real directory (not a symlink): {}",
        path.display()
    );
    path.canonicalize()
        .with_context(|| format!("failed to canonicalize {label} {}", path.display()))
}

fn ensure_candidate_namespace_isolation(
    candidate_data_dir: &Path,
    source_data_dir: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let raw_name = candidate_data_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("candidate_data_dir must have a valid UTF-8 basename"))?;
    ensure!(
        raw_name.starts_with(PEER_RECOVERY_CANDIDATE_DIR_PREFIX)
            && raw_name.len() > PEER_RECOVERY_CANDIDATE_DIR_PREFIX.len(),
        "candidate_data_dir basename {raw_name:?} must start with {PEER_RECOVERY_CANDIDATE_DIR_PREFIX:?}"
    );
    let canonical_candidate =
        ensure_non_symlink_directory(candidate_data_dir, "candidate_data_dir")?;
    let canonical_source = ensure_non_symlink_directory(source_data_dir, "source_data_dir")?;
    let canonical_name = canonical_candidate
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("canonical candidate_data_dir lacks a valid basename"))?;
    ensure!(
        canonical_name.starts_with(PEER_RECOVERY_CANDIDATE_DIR_PREFIX)
            && canonical_name.len() > PEER_RECOVERY_CANDIDATE_DIR_PREFIX.len(),
        "canonical candidate_data_dir basename {canonical_name:?} must start with {PEER_RECOVERY_CANDIDATE_DIR_PREFIX:?}"
    );
    ensure!(
        canonical_candidate != canonical_source,
        "candidate_data_dir must not equal source_data_dir"
    );
    ensure!(
        !canonical_candidate.starts_with(&canonical_source),
        "candidate_data_dir must not be nested inside source_data_dir"
    );
    ensure!(
        !canonical_source.starts_with(&canonical_candidate),
        "source_data_dir must not be nested inside candidate_data_dir"
    );
    Ok((canonical_candidate, canonical_source))
}

fn audit_candidate_file_isolation(candidate: &Path) -> Result<()> {
    let mut pending = vec![(candidate.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((directory, depth)) = pending.pop() {
        ensure!(depth <= 16, "candidate directory nesting exceeds 16 levels");
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            visited += 1;
            ensure!(
                visited <= 10_000,
                "candidate namespace exceeds 10000 entries"
            );
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            ensure!(
                !metadata.file_type().is_symlink(),
                "candidate contains a symlink: {}",
                path.display()
            );
            if metadata.is_dir() {
                pending.push((path, depth + 1));
            } else {
                ensure!(
                    metadata.is_file(),
                    "candidate contains a non-regular file: {}",
                    path.display()
                );
                ensure!(
                    recovery_metadata_projection(&metadata)?.5 == 1,
                    "candidate file must have exactly one hard link: {}",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn ensure_distinct_from_input(
    left: &RecoveryReadOnlyInput,
    left_label: &str,
    right: &RecoveryReadOnlyInput,
    right_label: &str,
) -> Result<()> {
    #[cfg(unix)]
    ensure!(
        !(left.device == right.device && left.inode == right.inode),
        "{left_label} shares device/inode with {right_label}"
    );
    #[cfg(not(unix))]
    let _ = (left, left_label, right, right_label);
    Ok(())
}

fn ensure_distinct_from_outside_path(
    candidate: &RecoveryReadOnlyInput,
    candidate_label: &str,
    outside_path: &Path,
    outside_label: &str,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        for meta_result in [
            std::fs::symlink_metadata(outside_path),
            std::fs::metadata(outside_path),
        ] {
            match meta_result {
                Ok(meta) => ensure!(
                    !(meta.dev() == candidate.device && meta.ino() == candidate.inode),
                    "{candidate_label} shares device/inode with {outside_label} ({})",
                    outside_path.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to inspect {outside_label} {}",
                            outside_path.display()
                        )
                    });
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (candidate, candidate_label, outside_path, outside_label);
    Ok(())
}

fn validate_candidate_manifest(
    manifest: PeerRecoveryCandidateManifest,
) -> Result<ValidatedCandidateManifest> {
    ensure!(
        manifest.schema == PEER_RECOVERY_CANDIDATE_SCHEMA,
        "unsupported candidate manifest schema {:?}; expected {:?}",
        manifest.schema,
        PEER_RECOVERY_CANDIDATE_SCHEMA
    );
    ensure!(
        manifest.recovery_epoch > 0,
        "recovery_epoch must be non-zero"
    );
    ensure!(
        manifest.validator_set_id > 0,
        "validator_set_id must be non-zero"
    );
    ensure!(
        manifest.prior_target_height > 0,
        "prior_target_height must be positive"
    );
    ensure!(
        manifest.prior_target_highest_dag_round > 0,
        "prior_target_highest_dag_round must be positive"
    );
    manifest
        .prior_target_height
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("prior_target_height overflows u64"))?;
    manifest
        .prior_target_highest_dag_round
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("prior_target_highest_dag_round overflows u64"))?;

    validate_expected_sha256_hex(
        &manifest.expected_state_wal_sha256,
        "expected_state_wal_sha256",
    )?;
    validate_expected_sha256_hex(&manifest.expected_genesis_sha256, "expected_genesis_sha256")?;
    validate_expected_sha256_hex(
        &manifest.expected_target_signing_record_sha256,
        "expected_target_signing_record_sha256",
    )?;
    validate_expected_sha256_hex(
        &manifest.expected_finality_certificate_sha256,
        "expected_finality_certificate_sha256",
    )?;

    let expected_recovery_manifest_hash = parse_strict_hash256(
        &manifest.expected_recovery_manifest_hash,
        "expected_recovery_manifest_hash",
    )?;
    let target_validator = parse_strict_hash256(&manifest.target_validator, "target_validator")?;

    ensure!(
        !manifest.genesis_path.is_empty(),
        "genesis_path must not be empty"
    );
    ensure!(
        !manifest.finality_certificate_path.is_empty(),
        "finality_certificate_path must not be empty"
    );

    let (candidate_data_dir, source_data_dir) = ensure_candidate_namespace_isolation(
        Path::new(&manifest.candidate_data_dir),
        Path::new(&manifest.source_data_dir),
    )?;
    let genesis_path = PathBuf::from(&manifest.genesis_path);
    let finality_certificate_path = PathBuf::from(&manifest.finality_certificate_path);

    Ok(ValidatedCandidateManifest {
        manifest,
        candidate_data_dir,
        source_data_dir,
        genesis_path,
        finality_certificate_path,
        expected_recovery_manifest_hash,
        target_validator,
    })
}

fn load_and_validate_candidate_manifest(
    manifest_path: &Path,
    expected_manifest_sha256: &str,
) -> Result<(ValidatedCandidateManifest, RecoveryReadOnlyInput)> {
    validate_expected_sha256_hex(expected_manifest_sha256, "expected_manifest_sha256")?;
    let (bytes, manifest_input) = read_bounded_regular_file_with_hash(
        manifest_path,
        MAX_QUALIFICATION_MANIFEST_BYTES,
        expected_manifest_sha256,
        "candidate_manifest",
    )?;
    let parsed: PeerRecoveryCandidateManifest = serde_json::from_slice(&bytes)
        .context("failed to parse candidate manifest JSON under strict schema")?;
    let validated = validate_candidate_manifest(parsed)?;
    Ok((validated, manifest_input))
}

fn decode_strict_finality_certificate(
    bytes: &[u8],
) -> Result<arc_consensus::view_change::FinalityCertificate> {
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_QUALIFICATION_CERTIFICATE_BYTES,
        "finality certificate size {} exceeds 1..={} bytes",
        bytes.len(),
        MAX_QUALIFICATION_CERTIFICATE_BYTES
    );
    let certificate: arc_consensus::view_change::FinalityCertificate =
        bincode::deserialize_limited_exact::<_, { MAX_QUALIFICATION_CERTIFICATE_BYTES as usize }>(
            bytes,
        )
        .context("finality certificate must be raw bincode FinalityCertificate")?;
    let reencoded = bincode::serialize(&certificate)
        .context("failed to re-encode decoded FinalityCertificate")?;
    ensure!(
        reencoded.len() == bytes.len(),
        "finality certificate contains unexpected trailing or non-canonical bytes"
    );
    Ok(certificate)
}

fn verify_retained_dag_block_signatures(
    records: &[RetainedDagRecord],
    domain: &arc_consensus::ConsensusDomain,
    committee: &arc_consensus::ValidatorSet,
) -> Result<usize> {
    let mut verified_blocks = 0usize;
    for record in records {
        if record.kind != RetainedRecordKind::DagBlock {
            continue;
        }
        let block: arc_consensus::DagBlock = bincode::deserialize(&record.payload)
            .with_context(|| format!("invalid retained DAG block {}", record.object_hash))?;
        ensure!(
            block.hash == record.object_hash && block.round == record.round,
            "retained DAG block {} envelope differs from decoded hash/round",
            record.object_hash
        );
        ensure!(
            committee.can_produce_blocks(&block.author),
            "retained DAG block {} author {} is not an approved block producer",
            block.hash,
            block.author
        );
        ensure!(
            block.verify_hash_in_domain(domain),
            "retained DAG block {} failed active recovery domain hash verification",
            block.hash
        );
        ensure!(
            !block.signature.is_empty(),
            "retained DAG block {} has an empty signature (rejected in keyless qualification)",
            block.hash
        );
        let signature: arc_crypto::Signature = bincode::deserialize(&block.signature)
            .with_context(|| {
                format!("retained DAG block {} has malformed signature", block.hash)
            })?;
        signature
            .verify(&block.hash, &block.author)
            .map_err(|error| {
                anyhow::anyhow!(
                    "retained DAG block {} signature failed verification for {}: {error}",
                    block.hash,
                    block.author
                )
            })?;
        verified_blocks = verified_blocks
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("verified DAG block count overflows usize"))?;
    }
    Ok(verified_blocks)
}

fn qualification_genesis_accounts(genesis: &config::GenesisConfig) -> Result<Vec<(Hash256, u64)>> {
    genesis.validated_accounts()
}

fn qualification_split_target_and_peers(
    validators: &[(Hash256, u64)],
    target_validator: Hash256,
) -> Result<(u64, Vec<(Hash256, u64)>)> {
    let mut seen = std::collections::HashSet::with_capacity(validators.len());
    let mut target_stake = None;
    let mut peers = Vec::with_capacity(validators.len().saturating_sub(1));
    for &(address, stake) in validators {
        ensure!(
            seen.insert(address),
            "duplicate validator {} in approved genesis validator set",
            address
        );
        ensure!(stake > 0, "approved validator {} has zero stake", address);
        if address == target_validator {
            target_stake = Some(stake);
        } else {
            peers.push((address, stake));
        }
    }
    let stake = target_stake.ok_or_else(|| {
        anyhow::anyhow!(
            "target validator {} is not a member of the approved genesis committee",
            target_validator
        )
    })?;
    Ok((stake, peers))
}

fn qualification_block_tx_root(block: &Block) -> Hash256 {
    block.header.tx_root
}

fn validate_target_signing_highwaters(
    record: &arc_consensus::view_change::ConsensusSigningRecord,
    prior_height: u64,
    prior_round: u64,
) -> Result<()> {
    ensure!(
        record
            .skipped_rounds
            .keys()
            .all(|round| *round <= prior_round)
            && record
                .last_applied_round
                .is_none_or(|round| round <= prior_round),
        "prior_target_highest_dag_round is below preserved target signing history"
    );
    ensure!(
        record
            .finality_votes
            .keys()
            .all(|height| *height <= prior_height),
        "prior_target_height is below preserved target finality signing history"
    );
    Ok(())
}

fn run_qualify_peer_state_command(
    candidate_manifest_path: &str,
    expected_manifest_sha256: &str,
) -> Result<()> {
    let (validated, manifest_input) = load_and_validate_candidate_manifest(
        Path::new(candidate_manifest_path),
        expected_manifest_sha256,
    )?;
    let manifest = &validated.manifest;
    let candidate_dir = &validated.candidate_data_dir;
    let source_dir = &validated.source_data_dir;
    let manifest_hex = validated.expected_recovery_manifest_hash.to_hex();

    audit_candidate_file_isolation(candidate_dir)?;

    let genesis_input = recovery_read_only_input(&validated.genesis_path, "genesis")?;
    require_recovery_input_hash(&genesis_input, &manifest.expected_genesis_sha256, "genesis")?;

    let (cert_bytes, cert_input) = read_bounded_regular_file_with_hash(
        &validated.finality_certificate_path,
        MAX_QUALIFICATION_CERTIFICATE_BYTES,
        &manifest.expected_finality_certificate_sha256,
        "finality_certificate",
    )?;
    let certificate = decode_strict_finality_certificate(&cert_bytes)?;

    let candidate_wal_path = candidate_dir.join("state.wal");
    let candidate_recovery_active_path = candidate_dir.join("recovery.active");
    let candidate_checkpoint_path = candidate_dir.join(format!("recovery-{manifest_hex}.arcchkpt"));
    let candidate_signing_record_path = candidate_dir.join("consensus-signing-record.bin");
    let candidate_dag_wal_dir = candidate_dir.join(format!("dag-wal-recovery-{manifest_hex}"));
    let candidate_dag_current_path = candidate_dag_wal_dir.join("CURRENT");

    let wal_before = recovery_read_only_input(&candidate_wal_path, "candidate state.wal")?;
    require_recovery_input_hash(
        &wal_before,
        &manifest.expected_state_wal_sha256,
        "candidate state.wal",
    )?;
    let recovery_active_before =
        recovery_read_only_input(&candidate_recovery_active_path, "candidate recovery.active")?;
    ensure!(
        recovery_active_before.size > 0,
        "candidate recovery.active is empty"
    );
    let checkpoint_before =
        recovery_read_only_input(&candidate_checkpoint_path, "candidate active ARCCHKPT")?;
    let (signing_record_bytes, signing_record_before) = read_bounded_regular_file_with_hash(
        &candidate_signing_record_path,
        MAX_QUALIFICATION_SIGNING_RECORD_BYTES,
        &manifest.expected_target_signing_record_sha256,
        "target consensus-signing-record.bin",
    )?;
    let target_signing_record =
        arc_consensus::view_change::ConsensusSigningRecord::decode(&signing_record_bytes)
            .context("failed to strictly decode target consensus-signing-record.bin")?;
    validate_target_signing_highwaters(
        &target_signing_record,
        manifest.prior_target_height,
        manifest.prior_target_highest_dag_round,
    )?;

    ensure_distinct_from_input(
        &wal_before,
        "candidate state.wal",
        &genesis_input,
        "genesis",
    )?;
    ensure_distinct_from_input(
        &signing_record_before,
        "target consensus-signing-record.bin",
        &genesis_input,
        "genesis",
    )?;
    ensure_distinct_from_input(
        &wal_before,
        "candidate state.wal",
        &signing_record_before,
        "target consensus-signing-record.bin",
    )?;
    ensure_distinct_from_input(
        &wal_before,
        "candidate state.wal",
        &cert_input,
        "finality_certificate",
    )?;
    ensure_distinct_from_outside_path(
        &wal_before,
        "candidate state.wal",
        &source_dir.join("state.wal"),
        "source state.wal",
    )?;
    ensure_distinct_from_outside_path(
        &signing_record_before,
        "target consensus-signing-record.bin",
        &source_dir.join("consensus-signing-record.bin"),
        "source consensus-signing-record.bin",
    )?;
    ensure_distinct_from_outside_path(
        &recovery_active_before,
        "candidate recovery.active",
        &source_dir.join("recovery.active"),
        "source recovery.active",
    )?;
    ensure_distinct_from_outside_path(
        &checkpoint_before,
        "candidate active ARCCHKPT",
        &source_dir.join(format!("recovery-{manifest_hex}.arcchkpt")),
        "source active ARCCHKPT",
    )?;
    ensure_distinct_from_outside_path(
        &genesis_input,
        "genesis",
        &candidate_wal_path,
        "candidate state.wal",
    )?;
    ensure_distinct_from_outside_path(
        &genesis_input,
        "genesis",
        &candidate_signing_record_path,
        "target consensus-signing-record.bin",
    )?;

    let _ = ensure_non_symlink_directory(&candidate_dag_wal_dir, "candidate recovery DAG WAL")?;
    let _ = recovery_read_only_input(
        &candidate_dag_current_path,
        "candidate recovery DAG CURRENT",
    )?;
    let precheck_pin = read_recovery_dag_pin(
        candidate_dir,
        validated.expected_recovery_manifest_hash,
    )?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "candidate recovery DAG external pin is missing; refusing to initialize a fresh generation"
        )
    })?;

    let (genesis, recovery_network) = recovery_network_from_genesis(
        &manifest.genesis_path,
        manifest.recovery_epoch,
        manifest.validator_set_id,
    )?;
    let genesis_hash = recovery_network.genesis_hash;
    let reward_activation_height = recovery_network.community_rewards_v1_activation_height;
    let approved_validators = recovery_network.validators.clone();
    let genesis_accounts = qualification_genesis_accounts(&genesis)?;
    let (target_stake, peer_validators) =
        qualification_split_target_and_peers(&approved_validators, validated.target_validator)?;

    let mut data_dir_lock = acquire_node_data_dir_lock(candidate_dir, None)?;
    audit_candidate_file_isolation(candidate_dir)?;
    let state = StateDB::with_genesis_persistent_recovery_in_prepared_directory(
        &genesis_accounts,
        data_dir_lock.prepared_directory(),
        recovery_network,
        None,
    )
    .context("failed to open candidate recovery-bound persistent state")?;
    data_dir_lock.finish_persistent_state_startup();
    state.set_community_rewards_v1_activation_height(reward_activation_height);

    let active_context = state
        .recovery_context()
        .ok_or_else(|| anyhow::anyhow!("candidate state has no active recovery context"))?;
    let active_manifest_hash = state
        .recovery_manifest_hash()
        .ok_or_else(|| anyhow::anyhow!("candidate state is missing its recovery manifest hash"))?;
    ensure!(
        active_manifest_hash == validated.expected_recovery_manifest_hash,
        "candidate active recovery manifest {} differs from expected {}",
        active_manifest_hash,
        validated.expected_recovery_manifest_hash
    );
    ensure!(
        active_context.recovery_epoch == manifest.recovery_epoch
            && active_context.validator_set_id == manifest.validator_set_id,
        "candidate active recovery context ({}, {}) differs from expected ({}, {})",
        active_context.recovery_epoch,
        active_context.validator_set_id,
        manifest.recovery_epoch,
        manifest.validator_set_id
    );
    let actual_domain = arc_consensus::ConsensusDomain::new(
        active_context.domain_hash(),
        active_context.recovery_epoch,
        active_context.validator_set_id,
    );

    let consensus = ConsensusManager::new(
        validated.target_validator,
        target_stake,
        0,
        false,
        &peer_validators,
    );
    install_startup_consensus_domains(&consensus.engine, genesis_hash, Some(actual_domain))
        .map_err(|error| anyhow::anyhow!("failed to install recovery consensus domain: {error}"))?;
    let committee = consensus.engine.frozen_validator_set().clone();
    ensure!(
        committee.epoch == 1 && committee.len() == approved_validators.len(),
        "constructed committee does not match approved epoch-1 genesis committee"
    );
    let committee_target = committee
        .get_validator(&validated.target_validator)
        .ok_or_else(|| anyhow::anyhow!("target validator missing from frozen committee"))?;
    ensure!(
        committee_target.stake == target_stake,
        "target validator committee stake {} differs from approved stake {}",
        committee_target.stake,
        target_stake
    );

    let finality_certificate_signing_stake = certificate
        .verify(&actual_domain, &committee)
        .map_err(|error| {
            anyhow::anyhow!(
                "finality certificate failed domain/committee/quorum verification: {error}"
            )
        })?;
    ensure!(
        certificate.height > manifest.prior_target_height,
        "finality certificate height {} does not exceed prior_target_height {}",
        certificate.height,
        manifest.prior_target_height
    );
    ensure!(
        state.height() == certificate.height,
        "candidate state height {} differs from certified height {}",
        state.height(),
        certificate.height
    );
    let canonical_tip = state.get_block(certificate.height).ok_or_else(|| {
        anyhow::anyhow!(
            "candidate canonical block at certified height {} is missing",
            certificate.height
        )
    })?;
    let computed_state_root = state.get_state_root();
    let tip_tx_root = qualification_block_tx_root(&canonical_tip);
    ensure!(
        canonical_tip.hash == certificate.block_hash,
        "candidate canonical block hash {} differs from certified {}",
        canonical_tip.hash,
        certificate.block_hash
    );
    ensure!(
        canonical_tip.header.state_root == certificate.state_root
            && computed_state_root == certificate.state_root,
        "candidate state root (header {}, computed {}) differs from certified {}",
        canonical_tip.header.state_root,
        computed_state_root,
        certificate.state_root
    );
    ensure!(
        tip_tx_root == certificate.tx_root,
        "candidate canonical tx_root {} differs from certified {}",
        tip_tx_root,
        certificate.tx_root
    );

    let startup = prepare_recovery_dag_startup(candidate_dir, &state)?
        .ok_or_else(|| anyhow::anyhow!("candidate state did not produce a recovery DAG startup"))?;
    ensure!(
        startup.binding.manifest_hash == validated.expected_recovery_manifest_hash
            && startup.binding.consensus_domain == actual_domain,
        "candidate recovery DAG startup binding differs from active recovery domain"
    );
    ensure!(
        startup.archived_legacy_wal.is_none(),
        "candidate data directory unexpectedly archived a legacy DAG WAL"
    );

    let external_pin = read_recovery_dag_pin(&startup.data_dir, startup.binding.manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("candidate recovery DAG external pin disappeared"))?;
    ensure!(
        external_pin.generation == precheck_pin.generation,
        "candidate recovery DAG external pin changed during qualification"
    );
    let gen_binding = generation_dag_binding(&startup)?;
    let store = GenerationStore::new(&startup.wal_dir);
    let selected_generation = store
        .load_current(&gen_binding, Some(external_pin.generation))
        .context("candidate recovery DAG generation failed binding/pin verification")?;
    let audit = store
        .audit(&gen_binding)
        .context("candidate recovery DAG generation store audit failed")?;
    ensure!(
        matches!(audit.status, StoreAuditStatus::Clean)
            && audit.current.pin == selected_generation.pin,
        "candidate recovery DAG generation store is not cleanly pinned"
    );
    validate_recovery_generation_anchor(&state, &startup, &selected_generation)?;

    let (records, summary) =
        stream_recovery_generation_records(&store, &gen_binding, selected_generation.pin)?;
    ensure!(
        summary.generation_pin == selected_generation.pin
            && summary.active_pin.generation_pin == selected_generation.pin,
        "streamed recovery DAG pin differs from selected generation"
    );
    ensure!(
        summary.active_suffix == TornSuffix::Clean,
        "candidate recovery DAG active log has torn suffix {:?}; refusing quarantine",
        summary.active_suffix
    );
    ensure!(
        summary.active_valid_prefix_bytes == summary.active_total_file_bytes,
        "candidate recovery DAG active log has {} trailing bytes beyond valid prefix",
        summary
            .active_total_file_bytes
            .saturating_sub(summary.active_valid_prefix_bytes)
    );

    let verified_retained_dag_blocks =
        verify_retained_dag_block_signatures(&records, &actual_domain, &committee)?;

    let replay = replay_recovery_dag_generation(
        consensus.engine.as_ref(),
        &state,
        &startup,
        &selected_generation,
        &records,
    )?;
    ensure!(
        replay.current_round > manifest.prior_target_highest_dag_round,
        "restored DAG current round {} does not exceed prior_target_highest_dag_round {}",
        replay.current_round,
        manifest.prior_target_highest_dag_round
    );
    ensure!(
        replay.next_commit_round > manifest.prior_target_highest_dag_round,
        "restored DAG next commit round {} does not exceed prior_target_highest_dag_round {}; capture newer committed history before resuming skip voting",
        replay.next_commit_round,
        manifest.prior_target_highest_dag_round
    );
    ensure!(
        consensus.engine.current_round() == replay.current_round
            && consensus.engine.last_committed_round() == replay.next_commit_round,
        "replayed consensus cursors drifted from returned replay summary"
    );
    let parent_round = replay
        .current_round
        .checked_sub(1)
        .context("restored DAG round has no previous round")?;
    ensure!(
        consensus.engine.has_quorum_parents_for_round(parent_round),
        "restored DAG tail lacks quorum blocks or certificates for every absent validator; capture a complete parent round"
    );

    let qualified_state_height = state.height();
    let wal_dir_to_sync = startup.wal_dir.clone();
    state
        .try_sync_wal()
        .context("candidate WAL durability barrier failed")?;
    drop(consensus);
    drop(state);
    sync_directory(&wal_dir_to_sync)?;
    sync_directory(candidate_dir)?;

    let wal_after = recovery_read_only_input(
        &candidate_wal_path,
        "candidate state.wal (post-qualification)",
    )?;
    ensure!(
        wal_after.sha256 == wal_before.sha256 && wal_after.size == wal_before.size,
        "candidate state.wal changed during qualification (before {}, after {})",
        wal_before.sha256,
        wal_after.sha256
    );
    let signing_record_after = recovery_read_only_input(
        &candidate_signing_record_path,
        "target consensus-signing-record.bin (post-qualification)",
    )?;
    ensure!(
        signing_record_after.sha256 == signing_record_before.sha256
            && signing_record_after.size == signing_record_before.size,
        "target consensus-signing-record.bin changed during qualification"
    );
    let recovery_active_after = recovery_read_only_input(
        &candidate_recovery_active_path,
        "candidate recovery.active (post-qualification)",
    )?;
    ensure!(
        recovery_active_after.sha256 == recovery_active_before.sha256,
        "candidate recovery.active changed during qualification"
    );
    require_recovery_input_hash(
        &recovery_read_only_input(&validated.genesis_path, "genesis after qualification")?,
        &manifest.expected_genesis_sha256,
        "genesis after qualification",
    )?;
    drop(data_dir_lock);

    let report = PeerStateQualificationReport {
        schema: PEER_RECOVERY_QUALIFICATION_REPORT_SCHEMA,
        status: "QUALIFIED_OFFLINE_CANDIDATE",
        manifest_sha256: manifest_input.sha256,
        candidate_data_dir: candidate_dir.display().to_string(),
        source_data_dir: source_dir.display().to_string(),
        target_validator: validated.target_validator.to_hex(),
        target_stake,
        committee_size: committee.len(),
        committee_total_stake: committee.total_stake,
        committee_quorum: committee.quorum,
        genesis_hash: genesis_hash.to_hex(),
        recovery_manifest_hash: active_manifest_hash.to_hex(),
        recovery_epoch: active_context.recovery_epoch,
        validator_set_id: active_context.validator_set_id,
        consensus_domain_hash: actual_domain.domain_hash.to_hex(),
        prior_target_height: manifest.prior_target_height,
        qualified_state_height,
        certified_block_hash: certificate.block_hash.to_hex(),
        certified_state_root: certificate.state_root.to_hex(),
        certified_tx_root: certificate.tx_root.to_hex(),
        finality_certificate_signing_stake,
        prior_target_highest_dag_round: manifest.prior_target_highest_dag_round,
        restored_dag_current_round: replay.current_round,
        restored_dag_next_commit_round: replay.next_commit_round,
        generation_pin_sequence: selected_generation.pin.sequence,
        generation_pin_hash: selected_generation.pin.hash.to_hex(),
        verified_retained_dag_blocks,
        repaired_tail_commit_round: replay.repaired_commit.map(|(_, round)| round),
        state_wal_sha256_unchanged: wal_after.sha256,
        target_signing_record_sha256_unchanged: signing_record_after.sha256,
        target_signing_record_skipped_rounds: target_signing_record.skipped_rounds.len(),
        target_signing_record_finality_votes: target_signing_record.finality_votes.len(),
        target_signing_record_last_applied_round: target_signing_record.last_applied_round,
        qualified_checks: vec![
            "candidate_manifest_schema_size_hash_and_namespace_isolation",
            "no_symlink_roots_no_source_nesting_no_shared_inode_or_hardlinks",
            "strict_target_signing_record_decode_and_unchanged_post_open_hash",
            "persistent_recovery_state_open_and_unchanged_state_wal_and_recovery_active",
            "raw_bincode_finality_certificate_domain_committee_quorum_and_tip_match",
            "existing_pinned_clean_recovery_dag_generation_without_quarantine_or_compaction",
            "keyless_explicit_ed25519_verification_of_every_retained_dag_block",
            "strict_recovery_dag_replay_and_positive_highwater_advancement",
        ],
        unqualified_limitations: vec![
            "does_not_certify_source_capture_atomicity_or_live_process_copy_safety",
            "does_not_prove_operator_supplied_prior_target_height_or_round_is_complete",
            "does_not_activate_production_bind_network_load_models_or_sign_with_target_key",
            "does_not_prove_runtime_network_catch_up_or_live_finality_voting",
        ],
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

const PEER_RECOVERY_CAPTURE_INSPECTION_SCHEMA: &str = "arc.peer-recovery.capture-inspection.v1";

struct InspectPeerCaptureArgs {
    candidate_dir: PathBuf,
    expected_recovery_manifest_hash: String,
    recovery_epoch: u64,
    validator_set_id: u64,
    target_signing_record_path: PathBuf,
    expected_target_signing_record_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SigningRecordHighwaters {
    skipped_rounds_count: usize,
    finality_votes_count: usize,
    max_skipped_round: Option<u64>,
    last_applied_round: Option<u64>,
    highest_observed_dag_round: Option<u64>,
    max_finality_vote_height: Option<u64>,
}

#[derive(Debug)]
struct PeerCaptureStoreInspection {
    generation_pin: GenerationPin,
    dag_baseline_height: u64,
    commit_record_count: u64,
    candidate_certificate_heights: [u64; 2],
    generation_current_round: u64,
    generation_next_dag_round: u64,
    max_retained_dag_round: Option<u64>,
    target_signing_record_sha256: String,
    target_signing_highwaters: SigningRecordHighwaters,
}

#[derive(Debug, Serialize)]
struct PeerCaptureInspectionReport {
    schema: &'static str,
    status: &'static str,
    candidate_dir: String,
    recovery_manifest_hash: String,
    recovery_epoch: u64,
    validator_set_id: u64,
    consensus_domain_hash: String,
    generation_pin_sequence: u64,
    generation_pin_hash: String,
    dag_baseline_height: u64,
    commit_record_count: u64,
    candidate_certificate_heights: [u64; 2],
    generation_current_round: u64,
    generation_next_dag_round: u64,
    max_retained_dag_round: Option<u64>,
    target_signing_record_sha256: String,
    target_signing_highwaters: SigningRecordHighwaters,
    not_verified_by_capture_inspection: Vec<&'static str>,
}

fn summarize_signing_record_highwaters(
    record: &arc_consensus::view_change::ConsensusSigningRecord,
) -> SigningRecordHighwaters {
    let max_skipped_round = record.skipped_rounds.keys().copied().max();
    let last_applied_round = record.last_applied_round;
    let highest_observed_dag_round = match (max_skipped_round, last_applied_round) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(round), None) | (None, Some(round)) => Some(round),
        (None, None) => None,
    };
    let max_finality_vote_height = record.finality_votes.keys().copied().max();
    SigningRecordHighwaters {
        skipped_rounds_count: record.skipped_rounds.len(),
        finality_votes_count: record.finality_votes.len(),
        max_skipped_round,
        last_applied_round,
        highest_observed_dag_round,
        max_finality_vote_height,
    }
}

fn ensure_candidate_dir_for_capture_inspection(candidate_dir: &Path) -> Result<PathBuf> {
    let raw_name = candidate_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("candidate_dir must have a valid UTF-8 basename"))?;
    ensure!(
        raw_name.starts_with(PEER_RECOVERY_CANDIDATE_DIR_PREFIX)
            && raw_name.len() > PEER_RECOVERY_CANDIDATE_DIR_PREFIX.len(),
        "candidate_dir basename {raw_name:?} must start with {PEER_RECOVERY_CANDIDATE_DIR_PREFIX:?}"
    );
    let canonical = ensure_non_symlink_directory(candidate_dir, "candidate_dir")?;
    let canonical_name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("canonical candidate_dir lacks a valid basename"))?;
    ensure!(
        canonical_name.starts_with(PEER_RECOVERY_CANDIDATE_DIR_PREFIX)
            && canonical_name.len() > PEER_RECOVERY_CANDIDATE_DIR_PREFIX.len(),
        "canonical candidate_dir basename {canonical_name:?} must start with {PEER_RECOVERY_CANDIDATE_DIR_PREFIX:?}"
    );
    audit_candidate_file_isolation(&canonical)?;
    Ok(canonical)
}

fn inspect_persistent_peer_capture_store(
    candidate_dir: &Path,
    expected_manifest_hash: Hash256,
    gen_binding: &GenerationDagBinding,
    target_signing_record_path: &Path,
    expected_target_signing_record_sha256: &str,
) -> Result<PeerCaptureStoreInspection> {
    let canonical_candidate = ensure_candidate_dir_for_capture_inspection(candidate_dir)?;
    validate_expected_sha256_hex(
        expected_target_signing_record_sha256,
        "expected_target_signing_record_sha256",
    )?;
    let (signing_bytes, signing_input) = read_bounded_regular_file_with_hash(
        target_signing_record_path,
        MAX_QUALIFICATION_SIGNING_RECORD_BYTES,
        expected_target_signing_record_sha256,
        "target consensus-signing-record.bin",
    )?;
    let signing_record = arc_consensus::view_change::ConsensusSigningRecord::decode(&signing_bytes)
        .context("failed to strictly decode target consensus-signing-record.bin")?;
    let target_signing_highwaters = summarize_signing_record_highwaters(&signing_record);

    let dag_wal_dir = canonical_candidate.join(format!(
        "dag-wal-recovery-{}",
        expected_manifest_hash.to_hex()
    ));
    let _ = ensure_non_symlink_directory(&dag_wal_dir, "candidate recovery DAG WAL")?;
    let _ = recovery_read_only_input(
        &dag_wal_dir.join("CURRENT"),
        "candidate recovery DAG CURRENT",
    )?;
    let external_pin = read_recovery_dag_pin(&canonical_candidate, expected_manifest_hash)?
        .ok_or_else(|| {
            anyhow::anyhow!("candidate recovery DAG external pin is missing; refusing inspection")
        })?;

    let store = GenerationStore::new(&dag_wal_dir);
    let selected_generation = store
        .load_current(gen_binding, Some(external_pin.generation))
        .context("candidate recovery DAG generation failed binding/pin verification")?;
    let audit = store
        .audit(gen_binding)
        .context("candidate recovery DAG generation store audit failed")?;
    ensure!(
        matches!(audit.status, StoreAuditStatus::Clean)
            && audit.current.pin == selected_generation.pin,
        "candidate recovery DAG generation store is not cleanly pinned"
    );

    let (records, summary) =
        stream_recovery_generation_records(&store, gen_binding, selected_generation.pin)?;
    ensure!(
        summary.generation_pin == selected_generation.pin
            && summary.active_pin.generation_pin == selected_generation.pin,
        "streamed recovery DAG pin differs from selected generation"
    );
    ensure!(
        summary.active_suffix == TornSuffix::Clean,
        "candidate recovery DAG active log has torn suffix {:?}; refusing quarantine",
        summary.active_suffix
    );
    ensure!(
        summary.active_valid_prefix_bytes == summary.active_total_file_bytes,
        "candidate recovery DAG active log has {} trailing bytes beyond valid prefix",
        summary
            .active_total_file_bytes
            .saturating_sub(summary.active_valid_prefix_bytes)
    );

    let mut block_rounds = std::collections::HashMap::<Hash256, u64>::new();
    let mut max_retained_dag_round: Option<u64> = None;
    for record in &records {
        if record.kind == RetainedRecordKind::DagBlock {
            block_rounds.insert(record.object_hash, record.round);
            max_retained_dag_round = Some(
                max_retained_dag_round.map_or(record.round, |current| current.max(record.round)),
            );
        }
    }

    let mut commit_record_count = 0u64;
    for record in &records {
        if record.kind == RetainedRecordKind::Commit {
            ensure!(
                block_rounds.get(&record.object_hash) == Some(&record.round),
                "retained DAG commit {} at round {} has no matching retained DAG block",
                record.object_hash,
                record.round
            );
            commit_record_count = commit_record_count
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("retained DAG commit count overflows u64"))?;
        }
    }

    let dag_baseline_height = selected_generation.manifest.baseline_state.height;
    let committed_height = dag_baseline_height
        .checked_add(commit_record_count)
        .ok_or_else(|| anyhow::anyhow!("committed height overflows u64"))?;
    let tail_repaired_height = committed_height
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("tail-repaired height overflows u64"))?;

    Ok(PeerCaptureStoreInspection {
        generation_pin: selected_generation.pin,
        dag_baseline_height,
        commit_record_count,
        candidate_certificate_heights: [committed_height, tail_repaired_height],
        generation_current_round: selected_generation.manifest.dag_cursor.current_round,
        generation_next_dag_round: selected_generation.manifest.dag_cursor.next_dag_round,
        max_retained_dag_round,
        target_signing_record_sha256: signing_input.sha256,
        target_signing_highwaters,
    })
}

fn run_inspect_peer_capture_command(args: InspectPeerCaptureArgs) -> Result<()> {
    ensure!(args.recovery_epoch > 0, "recovery_epoch must be non-zero");
    ensure!(
        args.validator_set_id > 0,
        "validator_set_id must be non-zero"
    );
    let expected_manifest_hash = parse_strict_hash256(
        &args.expected_recovery_manifest_hash,
        "expected_recovery_manifest_hash",
    )?;
    let candidate_dir = ensure_candidate_dir_for_capture_inspection(&args.candidate_dir)?;
    let checkpoint_path = candidate_dir.join(format!(
        "recovery-{}.arcchkpt",
        expected_manifest_hash.to_hex()
    ));
    let _ = recovery_read_only_input(&checkpoint_path, "candidate active ARCCHKPT")?;
    let checkpoint =
        arc_state::recovery::ArcCheckpoint::read_from(&checkpoint_path).with_context(|| {
            format!(
                "failed to read candidate active recovery checkpoint {}",
                checkpoint_path.display()
            )
        })?;
    ensure!(
        checkpoint.manifest_hash() == expected_manifest_hash,
        "candidate checkpoint hash {} differs from expected {}",
        checkpoint.manifest_hash(),
        expected_manifest_hash
    );
    checkpoint
        .verify_content()
        .context("candidate active recovery checkpoint content failed verification")?;
    let context = checkpoint.manifest.recovery_context();
    ensure!(
        context.recovery_epoch == args.recovery_epoch
            && context.validator_set_id == args.validator_set_id,
        "candidate checkpoint context ({}, {}) differs from expected ({}, {})",
        context.recovery_epoch,
        context.validator_set_id,
        args.recovery_epoch,
        args.validator_set_id
    );
    let validator_set_commitment = checkpoint_validator_set_commitment(&checkpoint.manifest)?;
    let gen_binding = GenerationDagBinding {
        recovery_manifest_hash: expected_manifest_hash,
        recovery_domain: context.domain_hash(),
        validator_set_commitment,
    };

    let inspection = inspect_persistent_peer_capture_store(
        &candidate_dir,
        expected_manifest_hash,
        &gen_binding,
        &args.target_signing_record_path,
        &args.expected_target_signing_record_sha256,
    )?;

    let report = PeerCaptureInspectionReport {
        schema: PEER_RECOVERY_CAPTURE_INSPECTION_SCHEMA,
        status: "INSPECTED_CAPTURE_DAG_AND_SIGNING_RECORD_ONLY",
        candidate_dir: candidate_dir.display().to_string(),
        recovery_manifest_hash: expected_manifest_hash.to_hex(),
        recovery_epoch: context.recovery_epoch,
        validator_set_id: context.validator_set_id,
        consensus_domain_hash: context.domain_hash().to_hex(),
        generation_pin_sequence: inspection.generation_pin.sequence,
        generation_pin_hash: inspection.generation_pin.hash.to_hex(),
        dag_baseline_height: inspection.dag_baseline_height,
        commit_record_count: inspection.commit_record_count,
        candidate_certificate_heights: inspection.candidate_certificate_heights,
        generation_current_round: inspection.generation_current_round,
        generation_next_dag_round: inspection.generation_next_dag_round,
        max_retained_dag_round: inspection.max_retained_dag_round,
        target_signing_record_sha256: inspection.target_signing_record_sha256,
        target_signing_highwaters: inspection.target_signing_highwaters,
        not_verified_by_capture_inspection: vec![
            "does_not_open_state_db_or_read_or_verify_state_wal",
            "does_not_verify_dag_block_signatures_or_finality_certificate",
            "does_not_replace_qualify_peer_state_as_final_qualification_authority",
            "does_not_certify_source_capture_atomicity_or_signing_history_completeness",
        ],
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

#[cfg(test)]
mod recovery_peer_qualification_tests {
    use super::*;

    fn qualification_test_sha256(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(bytes))
    }

    fn qualification_test_manifest(
        candidate_dir: &Path,
        source_dir: &Path,
        genesis_path: &Path,
        cert_path: &Path,
    ) -> PeerRecoveryCandidateManifest {
        PeerRecoveryCandidateManifest {
            schema: PEER_RECOVERY_CANDIDATE_SCHEMA.to_string(),
            candidate_data_dir: candidate_dir.display().to_string(),
            source_data_dir: source_dir.display().to_string(),
            expected_state_wal_sha256: "a".repeat(64),
            genesis_path: genesis_path.display().to_string(),
            expected_genesis_sha256: "b".repeat(64),
            expected_recovery_manifest_hash: "c".repeat(64),
            recovery_epoch: 3,
            validator_set_id: 9,
            target_validator: "d".repeat(64),
            expected_target_signing_record_sha256: "e".repeat(64),
            prior_target_highest_dag_round: 25_656_607,
            prior_target_height: 15_297_331,
            finality_certificate_path: cert_path.display().to_string(),
            expected_finality_certificate_sha256: "f".repeat(64),
        }
    }

    fn qualification_test_committee() -> (
        [arc_crypto::KeyPair; 6],
        arc_consensus::ValidatorSet,
        arc_consensus::ConsensusDomain,
        Hash256,
    ) {
        let weights: [u64; 6] = [
            6_666_667, 6_666_667, 6_666_667, 6_666_666, 6_666_667, 6_666_666,
        ];
        let keys: [arc_crypto::KeyPair; 6] = std::array::from_fn(|index| {
            arc_crypto::KeyPair::from_ed25519_secret_bytes(
                &hash_bytes(format!("qualify-peer-state-key-{index}").as_bytes()).0,
            )
        });
        let committee = arc_consensus::ValidatorSet::new(
            keys.iter()
                .zip(weights.iter())
                .enumerate()
                .map(|(index, (key, stake))| {
                    arc_consensus::Validator::new(key.address(), *stake, index as u16).unwrap()
                })
                .collect(),
            1,
        );
        assert_eq!(committee.total_stake, 40_000_000);
        assert_eq!(committee.quorum, 26_666_667);
        let domain =
            arc_consensus::ConsensusDomain::new(hash_bytes(b"qualify-peer-state-domain"), 3, 9);
        let committee_hash = arc_consensus::view_change::validator_set_hash(&committee);
        (keys, committee, domain, committee_hash)
    }

    fn qualification_test_finality_vote(
        domain: arc_consensus::ConsensusDomain,
        committee_hash: Hash256,
        height: u64,
        block_hash: Hash256,
        state_root: Hash256,
        tx_root: Hash256,
        keypair: &arc_crypto::KeyPair,
    ) -> arc_consensus::view_change::FinalityVote {
        arc_consensus::view_change::FinalityVote::sign(
            domain,
            committee_hash,
            height,
            block_hash,
            state_root,
            tx_root,
            keypair,
        )
        .unwrap()
    }

    #[test]
    fn qualify_peer_state_rejects_bad_manifest_schema_unknown_fields_limits_and_hash() {
        let target = hash_bytes(b"qualified-target");
        let peer = hash_bytes(b"qualified-peer");
        assert_eq!(
            qualification_split_target_and_peers(&[(target, 7), (peer, 11)], target).unwrap(),
            (7, vec![(peer, 11)])
        );
        assert!(qualification_split_target_and_peers(&[(peer, 11)], target).is_err());
        assert!(qualification_split_target_and_peers(&[(target, 7), (target, 7)], target).is_err());
        assert!(qualification_split_target_and_peers(&[(target, 7), (peer, 0)], target).is_err());
        let cli = Cli::try_parse_from([
            "arc-node",
            "recovery",
            "qualify-peer-state",
            "--candidate-manifest",
            "/tmp/candidate.json",
            "--expected-manifest-sha256",
            &"a".repeat(64),
        ])
        .unwrap();
        assert!(matches!(
            cli.operator_command,
            Some(OperatorCommand::Recovery {
                command: RecoveryCommand::QualifyPeerState { .. }
            })
        ));
        let root = tempfile::Builder::new()
            .prefix("arc-qualify-manifest-")
            .tempdir()
            .unwrap();
        let candidate = root.path().join("peer-recovery-candidate-ams");
        let source = root.path().join("source-lhr");
        std::fs::create_dir(&candidate).unwrap();
        std::fs::create_dir(&source).unwrap();
        let genesis = root.path().join("genesis.toml");
        let cert = root.path().join("finality.bin");
        std::fs::write(&genesis, b"genesis").unwrap();
        std::fs::write(&cert, b"cert").unwrap();

        let valid = qualification_test_manifest(&candidate, &source, &genesis, &cert);
        assert!(validate_candidate_manifest(valid.clone()).is_ok());

        let mut wrong_schema = valid.clone();
        wrong_schema.schema = "arc.peer-recovery.candidate.v0".to_string();
        assert!(validate_candidate_manifest(wrong_schema).is_err());

        for mutation in 0..6 {
            let mut bad = valid.clone();
            match mutation {
                0 => bad.recovery_epoch = 0,
                1 => bad.validator_set_id = 0,
                2 => bad.prior_target_height = 0,
                3 => bad.prior_target_highest_dag_round = 0,
                4 => bad.prior_target_height = u64::MAX,
                _ => bad.prior_target_highest_dag_round = u64::MAX,
            }
            assert!(validate_candidate_manifest(bad).is_err());
        }

        let mut with_unknown: serde_json::Value = serde_json::to_value(&valid).unwrap();
        with_unknown
            .as_object_mut()
            .unwrap()
            .insert("unexpected_field".to_string(), serde_json::json!(true));
        let unknown_bytes = serde_json::to_vec(&with_unknown).unwrap();
        let unknown_path = root.path().join("unknown-field.json");
        std::fs::write(&unknown_path, &unknown_bytes).unwrap();
        let unknown_hash = qualification_test_sha256(&unknown_bytes);
        assert!(load_and_validate_candidate_manifest(&unknown_path, &unknown_hash).is_err());

        let valid_bytes = serde_json::to_vec(&valid).unwrap();
        let valid_path = root.path().join("valid-manifest.json");
        std::fs::write(&valid_path, &valid_bytes).unwrap();
        let wrong_hash = "0".repeat(64);
        assert!(load_and_validate_candidate_manifest(&valid_path, &wrong_hash).is_err());

        let oversized_path = root.path().join("oversized-manifest.json");
        let oversized_bytes = vec![b' '; (MAX_QUALIFICATION_MANIFEST_BYTES + 1) as usize];
        std::fs::write(&oversized_path, &oversized_bytes).unwrap();
        let oversized_hash = qualification_test_sha256(&oversized_bytes);
        assert!(load_and_validate_candidate_manifest(&oversized_path, &oversized_hash).is_err());

        let mut record = arc_consensus::view_change::ConsensusSigningRecord {
            last_applied_round: Some(101),
            ..Default::default()
        };
        assert!(validate_target_signing_highwaters(&record, 10, 100).is_err());
        assert!(validate_target_signing_highwaters(&record, 10, 101).is_ok());
        record
            .finality_votes
            .insert(11, (Hash256::ZERO, Hash256::ZERO, Hash256::ZERO));
        assert!(validate_target_signing_highwaters(&record, 10, 101).is_err());
        assert!(validate_target_signing_highwaters(&record, 11, 101).is_ok());
        record.skipped_rounds.entry(102).or_default();
        assert!(validate_target_signing_highwaters(&record, 11, 101).is_err());
    }

    #[test]
    fn qualify_peer_state_rejects_bad_paths_nesting_symlinks_and_hardlinks() {
        let root = tempfile::Builder::new()
            .prefix("arc-qualify-paths-")
            .tempdir()
            .unwrap();
        let source = root.path().join("source-lhr");
        let bad_prefix = root.path().join("not-a-candidate-dir");
        let nested_in_source = source.join("peer-recovery-candidate-nested");
        let candidate = root.path().join("peer-recovery-candidate-ams");
        let source_in_candidate = candidate.join("source-inside");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&bad_prefix).unwrap();
        std::fs::create_dir(&nested_in_source).unwrap();
        std::fs::create_dir(&candidate).unwrap();
        std::fs::create_dir(&source_in_candidate).unwrap();

        assert!(ensure_candidate_namespace_isolation(&candidate, &candidate).is_err());
        assert!(ensure_candidate_namespace_isolation(&bad_prefix, &source).is_err());
        assert!(ensure_candidate_namespace_isolation(&nested_in_source, &source).is_err());
        assert!(ensure_candidate_namespace_isolation(&candidate, &source_in_candidate).is_err());
        assert!(ensure_candidate_namespace_isolation(&candidate, &source).is_ok());
        assert!(audit_candidate_file_isolation(&candidate).is_ok());

        #[cfg(unix)]
        {
            let symlink_candidate = root.path().join("peer-recovery-candidate-link");
            std::os::unix::fs::symlink(&candidate, &symlink_candidate).unwrap();
            assert!(ensure_candidate_namespace_isolation(&symlink_candidate, &source).is_err());

            let nested_link = source_in_candidate.join("generation-link");
            std::os::unix::fs::symlink(&source, &nested_link).unwrap();
            assert!(audit_candidate_file_isolation(&candidate).is_err());
            std::fs::remove_file(&nested_link).unwrap();

            let source_wal = source.join("state.wal");
            let linked_wal = candidate.join("state.wal");
            std::fs::write(&source_wal, b"wal-bytes").unwrap();
            std::fs::hard_link(&source_wal, &linked_wal).unwrap();
            assert!(recovery_read_only_input(&linked_wal, "candidate state.wal").is_err());
            assert!(audit_candidate_file_isolation(&candidate).is_err());
        }
    }

    #[test]
    fn qualify_peer_state_rejects_keyless_unsigned_forged_and_wrong_domain_dag_blocks() {
        let (keys, committee, domain, _) = qualification_test_committee();
        let mut block = arc_consensus::DagBlock {
            author: keys[0].address(),
            round: 101,
            parents: Vec::new(),
            transactions: Vec::new(),
            timestamp: 101,
            hash: Hash256::ZERO,
            signature: Vec::new(),
            ordering_commitment: arc_consensus::DagBlock::compute_ordering_commitment(&[]),
        };
        block.hash = block.compute_hash_in_domain(&domain);

        let unsigned_record = RetainedDagRecord::dag_block(
            block.round,
            block.hash,
            bincode::serialize(&block).unwrap(),
        );
        assert!(
            verify_retained_dag_block_signatures(&[unsigned_record], &domain, &committee).is_err()
        );

        let mut wrong_signer = block.clone();
        wrong_signer.signature = bincode::serialize(&keys[1].sign(&block.hash).unwrap()).unwrap();
        let wrong_signer_record = RetainedDagRecord::dag_block(
            wrong_signer.round,
            wrong_signer.hash,
            bincode::serialize(&wrong_signer).unwrap(),
        );
        assert!(
            verify_retained_dag_block_signatures(&[wrong_signer_record], &domain, &committee)
                .is_err()
        );

        let wrong_domain =
            arc_consensus::ConsensusDomain::new(hash_bytes(b"other-qualify-domain"), 3, 9);
        let mut wrong_domain_block = block.clone();
        wrong_domain_block.hash = wrong_domain_block.compute_hash_in_domain(&wrong_domain);
        wrong_domain_block.signature =
            bincode::serialize(&keys[0].sign(&wrong_domain_block.hash).unwrap()).unwrap();
        let wrong_domain_record = RetainedDagRecord::dag_block(
            wrong_domain_block.round,
            wrong_domain_block.hash,
            bincode::serialize(&wrong_domain_block).unwrap(),
        );
        assert!(
            verify_retained_dag_block_signatures(&[wrong_domain_record], &domain, &committee)
                .is_err()
        );

        let mut signed = block;
        signed.signature = bincode::serialize(&keys[0].sign(&signed.hash).unwrap()).unwrap();
        let signed_record = RetainedDagRecord::dag_block(
            signed.round,
            signed.hash,
            bincode::serialize(&signed).unwrap(),
        );
        assert_eq!(
            verify_retained_dag_block_signatures(&[signed_record], &domain, &committee).unwrap(),
            1
        );
    }

    #[test]
    fn qualify_peer_state_rejects_oversized_wrong_domain_forged_and_subquorum_certificates() {
        use arc_consensus::view_change::{CertificateError, FinalityCertificate};

        let (keys, committee, domain, committee_hash) = qualification_test_committee();
        let height = 17_773_592;
        let block_hash = hash_bytes(b"lhr-certified-block");
        let state_root = hash_bytes(b"lhr-certified-state-root");
        let tx_root = hash_bytes(b"lhr-certified-tx-root");

        let votes: Vec<_> = (0..4usize)
            .map(|index| {
                qualification_test_finality_vote(
                    domain,
                    committee_hash,
                    height,
                    block_hash,
                    state_root,
                    tx_root,
                    &keys[index],
                )
            })
            .collect();
        let valid_cert = FinalityCertificate {
            domain,
            validator_set_hash: committee_hash,
            height,
            block_hash,
            state_root,
            tx_root,
            votes,
        };
        let encoded = bincode::serialize(&valid_cert).unwrap();
        let decoded = decode_strict_finality_certificate(&encoded).unwrap();
        assert_eq!(decoded.verify(&domain, &committee), Ok(26_666_667));

        let mut with_trailing = encoded;
        with_trailing.push(0u8);
        assert!(decode_strict_finality_certificate(&with_trailing).is_err());

        let oversized = vec![0u8; (MAX_QUALIFICATION_CERTIFICATE_BYTES + 1) as usize];
        assert!(decode_strict_finality_certificate(&oversized).is_err());

        let mut subquorum = valid_cert.clone();
        subquorum.votes.truncate(3);
        assert_eq!(
            subquorum.verify(&domain, &committee),
            Err(CertificateError::BelowQuorum {
                signing: 20_000_001,
                quorum: 26_666_667,
            })
        );

        let mut forged = valid_cert.clone();
        forged.votes[0].signature = forged.votes[1].signature.clone();
        assert_eq!(
            forged.verify(&domain, &committee),
            Err(CertificateError::BadSignature(keys[0].address()))
        );

        let wrong_domain =
            arc_consensus::ConsensusDomain::new(hash_bytes(b"wrong-cert-domain"), 3, 9);
        let wrong_domain_cert = FinalityCertificate {
            domain: wrong_domain,
            validator_set_hash: committee_hash,
            height,
            block_hash,
            state_root,
            tx_root,
            votes: (0..4usize)
                .map(|index| {
                    qualification_test_finality_vote(
                        wrong_domain,
                        committee_hash,
                        height,
                        block_hash,
                        state_root,
                        tx_root,
                        &keys[index],
                    )
                })
                .collect(),
        };
        assert_eq!(
            wrong_domain_cert.verify(&domain, &committee),
            Err(CertificateError::WrongDomain {
                expected: Box::new(domain),
                found: Box::new(wrong_domain),
            })
        );
    }
    fn qualification_test_signed_dag_block(
        keypair: &arc_crypto::KeyPair,
        round: u64,
        parents: Vec<Hash256>,
        domain: &arc_consensus::ConsensusDomain,
    ) -> arc_consensus::DagBlock {
        let mut block = arc_consensus::DagBlock {
            author: keypair.address(),
            round,
            parents,
            transactions: Vec::new(),
            timestamp: round,
            hash: Hash256::ZERO,
            signature: Vec::new(),
            ordering_commitment: arc_consensus::DagBlock::compute_ordering_commitment(&[]),
        };
        block.hash = block.compute_hash_in_domain(domain);
        block.signature = bincode::serialize(&keypair.sign(&block.hash).unwrap()).unwrap();
        block
    }

    #[test]
    fn inspect_peer_capture_reads_real_persistent_generation_and_target_signing_record() {
        let cli = Cli::try_parse_from([
            "arc-node",
            "recovery",
            "inspect-peer-capture",
            "--candidate-dir",
            "/tmp/peer-recovery-candidate-sgp",
            "--expected-recovery-manifest-hash",
            &"a".repeat(64),
            "--recovery-epoch",
            "1",
            "--validator-set-id",
            "1",
            "--target-signing-record",
            "/tmp/consensus-signing-record.bin",
            "--expected-target-signing-record-sha256",
            &"b".repeat(64),
        ])
        .unwrap();
        assert!(matches!(
            cli.operator_command,
            Some(OperatorCommand::Recovery {
                command: RecoveryCommand::InspectPeerCapture { .. }
            })
        ));

        let (keys, _, domain, committee_hash) = qualification_test_committee();
        let manifest_hash = hash_bytes(b"inspect-peer-capture-manifest");
        let gen_binding = GenerationDagBinding {
            recovery_manifest_hash: manifest_hash,
            recovery_domain: domain.domain_hash,
            validator_set_commitment: committee_hash,
        };

        let root = tempfile::Builder::new()
            .prefix("arc-inspect-peer-capture-")
            .tempdir()
            .unwrap();
        let candidate_dir = root.path().join("peer-recovery-candidate-sgp");
        std::fs::create_dir(&candidate_dir).unwrap();
        let dag_wal_dir =
            candidate_dir.join(format!("dag-wal-recovery-{}", manifest_hash.to_hex()));

        let base_round = 29_119_000u64;
        let base_height = 17_773_590u64;
        let block_0 =
            qualification_test_signed_dag_block(&keys[0], base_round, Vec::new(), &domain);
        let block_1 = qualification_test_signed_dag_block(
            &keys[1],
            base_round + 1,
            vec![block_0.hash],
            &domain,
        );
        let records = vec![
            RetainedDagRecord::dag_block(
                block_0.round,
                block_0.hash,
                bincode::serialize(&block_0).unwrap(),
            ),
            RetainedDagRecord::dag_block(
                block_1.round,
                block_1.hash,
                bincode::serialize(&block_1).unwrap(),
            ),
            RetainedDagRecord::commit(block_0.round, block_0.hash),
            RetainedDagRecord::commit(block_1.round, block_1.hash),
        ];

        let store = GenerationStore::new(&dag_wal_dir);
        let generation = store
            .create_initial(
                GenerationInput {
                    binding: gen_binding.clone(),
                    baseline_state: DagBaselineState {
                        height: base_height,
                        block_hash: hash_bytes(b"sgp-baseline-block"),
                        state_root: hash_bytes(b"sgp-baseline-state-root"),
                        dag_anchor: None,
                    },
                    dag_cursor: DagCursor {
                        committed_block_count: 0,
                        next_dag_round: base_round,
                        current_round: base_round,
                        retention_floor_round: base_round,
                        retention_ceiling_round: recovery_retention_ceiling(base_round).unwrap(),
                    },
                    retention_limits: RetentionLimits::default(),
                },
                records,
            )
            .unwrap();
        write_recovery_dag_pin_atomically(&candidate_dir, manifest_hash, generation.pin).unwrap();

        let mut signing_record = arc_consensus::view_change::ConsensusSigningRecord {
            last_applied_round: Some(25_656_605),
            ..Default::default()
        };
        signing_record
            .skipped_rounds
            .entry(25_656_607)
            .or_default()
            .insert(
                keys[5].address(),
                arc_consensus::view_change::AbsenceReason::NoBlock,
            );
        signing_record.finality_votes.insert(
            15_297_331,
            (
                hash_bytes(b"sgp-prior-block"),
                hash_bytes(b"sgp-prior-state"),
                hash_bytes(b"sgp-prior-tx"),
            ),
        );
        let signing_bytes = signing_record.encode();
        let signing_sha256 = qualification_test_sha256(&signing_bytes);
        let signing_path = candidate_dir.join("consensus-signing-record.bin");
        std::fs::write(&signing_path, &signing_bytes).unwrap();

        let inspection = inspect_persistent_peer_capture_store(
            &candidate_dir,
            manifest_hash,
            &gen_binding,
            &signing_path,
            &signing_sha256,
        )
        .unwrap();
        assert_eq!(inspection.generation_pin, generation.pin);
        assert_eq!(inspection.dag_baseline_height, base_height);
        assert_eq!(inspection.commit_record_count, 2);
        assert_eq!(
            inspection.candidate_certificate_heights,
            [17_773_592, 17_773_593]
        );
        assert_eq!(inspection.max_retained_dag_round, Some(base_round + 1));
        assert_eq!(inspection.target_signing_record_sha256, signing_sha256);
        assert_eq!(
            inspection.target_signing_highwaters,
            SigningRecordHighwaters {
                skipped_rounds_count: 1,
                finality_votes_count: 1,
                max_skipped_round: Some(25_656_607),
                last_applied_round: Some(25_656_605),
                highest_observed_dag_round: Some(25_656_607),
                max_finality_vote_height: Some(15_297_331),
            }
        );
        assert_eq!(std::fs::read(&signing_path).unwrap(), signing_bytes);

        let mut wrong_domain_binding = gen_binding.clone();
        wrong_domain_binding.recovery_domain = hash_bytes(b"wrong-domain");
        assert!(
            inspect_persistent_peer_capture_store(
                &candidate_dir,
                manifest_hash,
                &wrong_domain_binding,
                &signing_path,
                &signing_sha256,
            )
            .is_err()
        );

        let wrong_sha = "0".repeat(64);
        assert!(
            inspect_persistent_peer_capture_store(
                &candidate_dir,
                manifest_hash,
                &gen_binding,
                &signing_path,
                &wrong_sha,
            )
            .is_err()
        );

        let corrupt_signing_path = root.path().join("corrupt-signing-record.bin");
        let corrupt_bytes = b"not-a-valid-signing-record";
        std::fs::write(&corrupt_signing_path, corrupt_bytes).unwrap();
        let corrupt_sha = qualification_test_sha256(corrupt_bytes);
        assert!(
            inspect_persistent_peer_capture_store(
                &candidate_dir,
                manifest_hash,
                &gen_binding,
                &corrupt_signing_path,
                &corrupt_sha,
            )
            .is_err()
        );

        let unpinned_dir = root.path().join("peer-recovery-candidate-unpinned");
        std::fs::create_dir(&unpinned_dir).unwrap();
        let unpinned_wal =
            unpinned_dir.join(format!("dag-wal-recovery-{}", manifest_hash.to_hex()));
        GenerationStore::new(&unpinned_wal)
            .create_initial(
                GenerationInput {
                    binding: gen_binding.clone(),
                    baseline_state: DagBaselineState {
                        height: base_height,
                        block_hash: hash_bytes(b"unpinned-block"),
                        state_root: hash_bytes(b"unpinned-root"),
                        dag_anchor: None,
                    },
                    dag_cursor: DagCursor {
                        committed_block_count: 0,
                        next_dag_round: base_round,
                        current_round: base_round,
                        retention_floor_round: base_round,
                        retention_ceiling_round: recovery_retention_ceiling(base_round).unwrap(),
                    },
                    retention_limits: RetentionLimits::default(),
                },
                Vec::new(),
            )
            .unwrap();
        assert!(
            inspect_persistent_peer_capture_store(
                &unpinned_dir,
                manifest_hash,
                &gen_binding,
                &signing_path,
                &signing_sha256,
            )
            .is_err()
        );

        let orphan_dir = root.path().join("peer-recovery-candidate-orphan-commit");
        std::fs::create_dir(&orphan_dir).unwrap();
        let orphan_wal = orphan_dir.join(format!("dag-wal-recovery-{}", manifest_hash.to_hex()));
        let orphan_gen = GenerationStore::new(&orphan_wal)
            .create_initial(
                GenerationInput {
                    binding: gen_binding.clone(),
                    baseline_state: DagBaselineState {
                        height: base_height,
                        block_hash: hash_bytes(b"orphan-block"),
                        state_root: hash_bytes(b"orphan-root"),
                        dag_anchor: None,
                    },
                    dag_cursor: DagCursor {
                        committed_block_count: 0,
                        next_dag_round: base_round,
                        current_round: base_round,
                        retention_floor_round: base_round,
                        retention_ceiling_round: recovery_retention_ceiling(base_round).unwrap(),
                    },
                    retention_limits: RetentionLimits::default(),
                },
                vec![RetainedDagRecord::commit(
                    base_round,
                    hash_bytes(b"missing-dag-block"),
                )],
            )
            .unwrap();
        write_recovery_dag_pin_atomically(&orphan_dir, manifest_hash, orphan_gen.pin).unwrap();
        assert!(
            inspect_persistent_peer_capture_store(
                &orphan_dir,
                manifest_hash,
                &gen_binding,
                &signing_path,
                &signing_sha256,
            )
            .is_err()
        );

        let non_candidate_dir = root.path().join("live-source-dir");
        std::fs::create_dir(&non_candidate_dir).unwrap();
        assert!(
            inspect_persistent_peer_capture_store(
                &non_candidate_dir,
                manifest_hash,
                &gen_binding,
                &signing_path,
                &signing_sha256,
            )
            .is_err()
        );

        std::fs::remove_file(dag_wal_dir.join("CURRENT")).unwrap();
        assert!(
            inspect_persistent_peer_capture_store(
                &candidate_dir,
                manifest_hash,
                &gen_binding,
                &signing_path,
                &signing_sha256,
            )
            .is_err()
        );
    }
}
