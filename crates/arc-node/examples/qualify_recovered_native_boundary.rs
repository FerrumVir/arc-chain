//! Offline-only checker for native migration on a disposable copy of a
//! recovered state directory. It never starts a node, listener, model, or
//! network connection and never opens the supplied source directory writable.

use anyhow::{Context, Result, bail, ensure};
use arc_crypto::{Hash256, hash_bytes};
use arc_node::{config, native_inference};
use arc_state::recovery::RecoveryNetworkPolicy;
use arc_state::{NativeMigrationRecord, StateDB};
use arc_types::Account;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct Args {
    source: PathBuf,
    scratch: PathBuf,
    genesis: PathBuf,
    activation: PathBuf,
    manifest: Hash256,
    height: u64,
    root: Hash256,
    epoch: u64,
    validator_set_id: u64,
}

fn usage() -> &'static str {
    "usage: cargo run -p arc-node --example qualify_recovered_native_boundary -- \\\n     --state-dir SOURCE --scratch-dir NEW_DIR --genesis GENESIS.toml \\\n     --activation-request REQUEST.json --expected-manifest-hash HEX \\\n     --expected-height N --expected-root HEX --recovery-epoch N \\\n     --validator-set-id N"
}

fn args() -> Result<Args> {
    let mut values = BTreeMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(key) = it.next() {
        ensure!(
            key.starts_with("--"),
            "unexpected argument {key:?}\n{}",
            usage()
        );
        let value = it
            .next()
            .with_context(|| format!("missing value for {key}"))?;
        ensure!(
            values.insert(key.clone(), value).is_none(),
            "duplicate argument {key}"
        );
    }
    let mut take = |key: &str| -> Result<String> {
        values
            .remove(key)
            .with_context(|| format!("required argument {key} missing\n{}", usage()))
    };
    let source = PathBuf::from(take("--state-dir")?);
    let scratch = PathBuf::from(take("--scratch-dir")?);
    let genesis = PathBuf::from(take("--genesis")?);
    let activation = PathBuf::from(take("--activation-request")?);
    let manifest = Hash256::from_hex(&take("--expected-manifest-hash")?)?;
    let height = take("--expected-height")?.parse()?;
    let root = Hash256::from_hex(&take("--expected-root")?)?;
    let epoch = take("--recovery-epoch")?.parse()?;
    let validator_set_id = take("--validator-set-id")?.parse()?;
    ensure!(values.is_empty(), "unknown arguments: {:?}", values.keys());
    ensure!(
        epoch > 0 && validator_set_id > 0,
        "recovery epoch and validator-set id must be nonzero"
    );
    Ok(Args {
        source,
        scratch,
        genesis,
        activation,
        manifest,
        height,
        root,
        epoch,
        validator_set_id,
    })
}

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn mode(_: &fs::Metadata) -> u32 {
    0
}

fn reject_symlink(path: &Path) -> Result<fs::Metadata> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "symlink refused: {}",
        path.display()
    );
    Ok(metadata)
}

fn fingerprint(dir: &Path) -> Result<String> {
    let mut entries = Vec::new();
    fn walk(root: &Path, here: &Path, out: &mut Vec<(String, u32, u64, [u8; 32])>) -> Result<()> {
        let mut children = fs::read_dir(here)?.collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let path = child.path();
            let metadata = reject_symlink(&path)?;
            let relative = path.strip_prefix(root)?.to_string_lossy().into_owned();
            if metadata.is_dir() {
                out.push((relative.clone(), mode(&metadata), 0, [0; 32]));
                walk(root, &path, out)?;
            } else {
                ensure!(
                    metadata.is_file(),
                    "non-regular file refused: {}",
                    path.display()
                );
                let digest = file_sha256(&path)?;
                out.push((relative, mode(&metadata), metadata.len(), digest));
            }
        }
        Ok(())
    }
    walk(dir, dir, &mut entries)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let bytes = bincode::serialize(&entries)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn file_sha256(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hasher.finalize().into())
}

fn copy_tree_new(source: &Path, target: &Path) -> Result<()> {
    reject_symlink(source)?;
    let source = fs::canonicalize(source).context("canonicalize source state directory")?;
    ensure!(
        reject_symlink(&source)?.is_dir(),
        "source is not a real directory"
    );
    let parent = target
        .parent()
        .context("scratch directory needs a parent")?;
    fs::create_dir_all(parent)?;
    let parent = fs::canonicalize(parent)?;
    let target = parent.join(
        target
            .file_name()
            .context("scratch directory needs a name")?,
    );
    ensure!(
        target != source && !target.starts_with(&source),
        "scratch directory must be outside the source tree"
    );
    match fs::symlink_metadata(&target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => bail!("scratch directory already exists: {}", target.display()),
        Err(error) => return Err(error.into()),
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&target)
        .context("create new private scratch directory")?;
    fn copy_into(source: &Path, target: &Path) -> Result<()> {
        for entry in fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()? {
            let src = entry.path();
            let metadata = reject_symlink(&src)?;
            let dst = target.join(entry.file_name());
            if metadata.is_dir() {
                fs::create_dir(&dst)?;
                copy_into(&src, &dst)?;
                fs::set_permissions(&dst, metadata.permissions())?;
            } else {
                ensure!(
                    metadata.is_file(),
                    "non-regular file refused: {}",
                    src.display()
                );
                let mut input = File::open(&src)?;
                let mut output = OpenOptions::new().write(true).create_new(true).open(&dst)?;
                std::io::copy(&mut input, &mut output)?;
                output.sync_all()?;
                fs::set_permissions(&dst, metadata.permissions())?;
            }
        }
        Ok(())
    }
    if let Err(error) = copy_into(&source, &target) {
        let _ = fs::remove_dir_all(&target);
        return Err(error);
    }
    Ok(())
}

fn expected_hash_file(path: &Path, expected: Hash256, what: &str) -> Result<()> {
    let actual = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let actual =
        Hash256::from_hex(actual.trim()).with_context(|| format!("parse {}", path.display()))?;
    ensure!(
        actual == expected,
        "{what} mismatch: expected {}, got {}",
        expected,
        actual
    );
    Ok(())
}

fn history_digest(state: &StateDB, height: u64, account_addresses: &[Hash256]) -> Result<String> {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-offline-native-history-check-v1");
    let mut seen = 0u64;
    let mut start = 0;
    while start < height {
        let end = height.min(start.saturating_add(255));
        let blocks = state.get_block_range(start, end, 256);
        let block_hashes: Vec<_> = blocks
            .iter()
            .map(|block| (block.header.height, block.hash))
            .collect();
        append_existing_block_hashes(&mut hasher, start, end, &block_hashes);
        for block in blocks {
            let block_height = block.header.height;
            ensure!(
                block_height >= start && block_height <= end,
                "block returned outside requested range"
            );
            hasher.update(&bincode::serialize(&block)?);
            seen += 1;
        }
        start = end + 1;
    }
    hasher.update(&seen.to_be_bytes());
    hasher.update(b"account-tx-history-v1\0");
    hasher.update(&(account_addresses.len() as u64).to_be_bytes());
    for address in account_addresses {
        hasher.update(address.as_ref());
        let transactions = state.get_account_txs(&address.0);
        hasher.update(&(transactions.len() as u64).to_be_bytes());
        for tx in transactions {
            hasher.update(tx.as_ref());
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn append_existing_block_hashes(
    hasher: &mut blake3::Hasher,
    start: u64,
    end: u64,
    blocks: &[(u64, Hash256)],
) {
    hasher.update(&start.to_be_bytes());
    hasher.update(&end.to_be_bytes());
    hasher.update(&(blocks.len() as u64).to_be_bytes());
    for (height, hash) in blocks {
        hasher.update(&height.to_be_bytes());
        hasher.update(hash.as_ref());
    }
}

fn account_snapshot(state: &StateDB) -> Result<Vec<(Hash256, Account)>> {
    let snapshot = state.export_durable_snapshot();
    let mut accounts = snapshot.accounts;
    accounts.sort_by_key(|(address, _)| address.0);
    Ok(accounts)
}

fn account_digest(accounts: &[(Hash256, Account)]) -> Result<String> {
    Ok(hex::encode(Sha256::digest(bincode::serialize(accounts)?)))
}

fn assert_preexisting_accounts_preserved(
    before: &[(Hash256, Account)],
    after: &[(Hash256, Account)],
    allowed_new: Hash256,
) -> Result<()> {
    let mut old: Vec<_> = before.iter().collect();
    old.sort_by_key(|(address, _)| address.0);
    let mut new: Vec<_> = after.iter().collect();
    new.sort_by_key(|(address, _)| address.0);
    ensure!(
        old.windows(2).all(|pair| pair[0].0 != pair[1].0),
        "duplicate address in pre-activation account snapshot"
    );
    ensure!(
        new.windows(2).all(|pair| pair[0].0 != pair[1].0),
        "duplicate address in post-activation account snapshot"
    );

    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        match old[i].0.0.cmp(&new[j].0.0) {
            std::cmp::Ordering::Less => {
                bail!("pre-existing account {} was removed", old[i].0)
            }
            std::cmp::Ordering::Greater => {
                ensure!(
                    new[j].0 == allowed_new,
                    "unexpected account appeared during activation: {}",
                    new[j].0
                );
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                ensure!(
                    bincode::serialize(&old[i].1)? == bincode::serialize(&new[j].1)?,
                    "pre-existing account {} changed",
                    old[i].0
                );
                i += 1;
                j += 1;
            }
        }
    }
    ensure!(
        i == old.len(),
        "pre-existing account {} was removed",
        old[i].0
    );
    for (address, _) in new.iter().skip(j) {
        ensure!(
            *address == allowed_new,
            "unexpected account appeared during activation: {address}"
        );
    }
    Ok(())
}

fn policy(
    genesis: &config::GenesisConfig,
    epoch: u64,
    validator_set_id: u64,
) -> Result<RecoveryNetworkPolicy> {
    let validators = genesis.validated_validator_set(false)?;
    Ok(RecoveryNetworkPolicy {
        chain_id: genesis.chain.chain_id.clone(),
        genesis_hash: genesis.network_hash(false)?,
        recovery_epoch: epoch,
        validator_set_id,
        validators,
        community_rewards_v1_activation_height: genesis
            .chain
            .community_rewards_v1_activation_height,
    })
}

fn run() -> Result<()> {
    let started = std::time::Instant::now();
    let args = args()?;
    reject_symlink(&args.source)?;
    let source = fs::canonicalize(&args.source).context("canonicalize input state directory")?;
    let source_before = fingerprint(&source).context("fingerprint immutable input")?;
    eprintln!(
        "qualification progress: input fingerprint complete at {:.1}s",
        started.elapsed().as_secs_f64()
    );
    let marker = source.join("recovery.active");
    expected_hash_file(&marker, args.manifest, "approved recovery marker")?;

    copy_tree_new(&source, &args.scratch)?;
    let scratch_parent = fs::canonicalize(args.scratch.parent().unwrap_or(Path::new(".")))?;
    let scratch = scratch_parent.join(
        args.scratch
            .file_name()
            .context("scratch path must have a name")?,
    );
    ensure!(
        fingerprint(&scratch)? == source_before,
        "copied state differs from source fingerprint"
    );
    eprintln!(
        "qualification progress: private copy verified at {:.1}s",
        started.elapsed().as_secs_f64()
    );

    let genesis_path = args.genesis.to_string_lossy();
    let genesis = config::load_genesis(&genesis_path)?;
    let network = policy(&genesis, args.epoch, args.validator_set_id)?;
    let genesis_hash = network.genesis_hash;
    expected_hash_file(
        &scratch.join("genesis.network-hash"),
        genesis_hash,
        "genesis binding",
    )?;
    let state = StateDB::with_genesis_persistent_recovery(
        &genesis.validated_accounts()?,
        &scratch,
        network.clone(),
        None,
    )
    .context("open disposable state with required existing recovery marker")?;
    ensure!(
        state.recovery_manifest_hash() == Some(args.manifest),
        "state opened without the approved recovery manifest"
    );
    let recovery = state
        .recovery_context()
        .context("state has no recovery context; fresh-genesis fallback refused")?;
    ensure!(
        recovery.genesis_hash == genesis_hash
            && recovery.recovery_epoch == args.epoch
            && recovery.validator_set_id == args.validator_set_id,
        "recovered chain identity differs from requested identity"
    );
    ensure!(
        state.height() == args.height,
        "replay height mismatch: expected {}, got {}",
        args.height,
        state.height()
    );
    let anchor = state
        .get_block(args.height)
        .context("replayed anchor block missing")?;
    ensure!(
        anchor.header.state_root == args.root,
        "replayed root mismatch: expected {}, got {}",
        args.root,
        anchor.header.state_root
    );
    ensure!(
        state.get_state_root() == args.root,
        "computed replay state root differs from expected root"
    );
    ensure!(
        state.native_inference_context().is_none(),
        "state already has native inference active; this checker expects the pre-activation state"
    );
    eprintln!(
        "qualification progress: replay verified height={} accounts={} at {:.1}s",
        state.height(),
        state.account_count(),
        started.elapsed().as_secs_f64()
    );
    ensure!(args.height.checked_add(1).is_some(), "height overflow");
    let activation_height = args.height + 1;

    let request = native_inference::load_activation_request(&args.activation)?;
    ensure!(
        request.migration.is_none(),
        "activation request must not contain a pre-existing migration record; this checker derives H = current height + 1"
    );
    let context = native_inference::assemble_activation_context(&state, &request)?;
    ensure!(
        context.domain.recovery_epoch == args.epoch && context.domain.chain_genesis == genesis_hash,
        "activation request context does not match recovered identity"
    );
    let commitment = context.commitment()?;
    let record = NativeMigrationRecord {
        chain_genesis: genesis_hash,
        recovery_epoch: args.epoch,
        validator_set_id: args.validator_set_id,
        activation_height,
        context_commitment: commitment,
    };
    ensure!(
        record.refusal_against(&state).is_none(),
        "derived migration record refused by recovered state"
    );

    let before_accounts = account_snapshot(&state)?;
    let before_account_digest = account_digest(&before_accounts)?;
    let addresses: Vec<_> = before_accounts
        .iter()
        .map(|(address, _)| *address)
        .collect();
    let before_history = history_digest(&state, args.height, &addresses)?;
    eprintln!(
        "qualification progress: pre-activation accounts/history captured at {:.1}s",
        started.elapsed().as_secs_f64()
    );
    let prev = state.get_block(args.height).unwrap();
    let producer = state
        .active_validators()
        .first()
        .context("no active validator in recovered state")?
        .0;
    state.authorize_native_migration(record.clone(), context.clone())?;
    let mut decision = blake3::Hasher::new_derive_key("ARC-offline-native-migration-decision-v1");
    decision.update(recovery.domain_hash().as_ref());
    decision.update(genesis_hash.as_ref());
    decision.update(args.manifest.as_ref());
    decision.update(&args.epoch.to_be_bytes());
    decision.update(&args.validator_set_id.to_be_bytes());
    decision.update(&activation_height.to_be_bytes());
    decision.update(prev.hash.as_ref());
    decision.update(prev.header.proof_hash.as_ref());
    decision.update(commitment.as_ref());
    let synthetic_decision = Hash256(*decision.finalize().as_bytes());
    let (block, _) = state.execute_block_adaptive_at_with_proof(
        &[],
        producer,
        prev.header.timestamp.saturating_add(1),
        synthetic_decision,
    )?;
    ensure!(
        block.header.height == activation_height && state.height() == activation_height,
        "activation did not execute at the exact target height"
    );
    ensure!(
        block.header.proof_hash == synthetic_decision,
        "activation block does not carry the derived synthetic decision proof"
    );
    ensure!(
        state.native_inference_context().as_ref() == Some(&context),
        "activation context did not become active in target block"
    );
    ensure!(
        state.native_migration().as_ref() == Some(&record),
        "migration record changed during activation"
    );
    eprintln!(
        "qualification progress: activation block H={} applied at {:.1}s",
        activation_height,
        started.elapsed().as_secs_f64()
    );
    ensure!(
        block.header.state_root != args.root,
        "activation block did not change the committed state root"
    );
    ensure!(
        state.get_state_root() == block.header.state_root,
        "computed post-activation root differs from activation block root"
    );
    let context_account = hash_bytes(b"ARC-isolated-inference-context-account-v1");
    eprintln!(
        "qualification progress: checking {} pre-existing accounts at {:.1}s",
        before_accounts.len(),
        started.elapsed().as_secs_f64()
    );
    assert_preexisting_accounts_preserved(
        &before_accounts,
        &account_snapshot(&state)?,
        context_account,
    )?;
    ensure!(
        history_digest(&state, args.height, &addresses)? == before_history,
        "activation changed pre-existing block or account-transaction history"
    );
    state
        .try_sync_wal()
        .context("sync disposable WAL before reopen")?;
    let activated_root = block.header.state_root;
    drop(state);

    let reopened = StateDB::with_genesis_persistent_recovery(
        &genesis.validated_accounts()?,
        &scratch,
        network,
        None,
    )
    .context("reopen disposable activated state")?;
    eprintln!(
        "qualification progress: reopened activated scratch state at {:.1}s",
        started.elapsed().as_secs_f64()
    );
    ensure!(
        reopened.height() == activation_height,
        "activated height did not persist across reopen"
    );
    ensure!(
        reopened
            .get_block(activation_height)
            .context("activation block missing after reopen")?
            .header
            .state_root
            == activated_root,
        "activation root changed after reopen"
    );
    ensure!(
        reopened
            .get_block(activation_height)
            .context("activation block missing after reopen")?
            .header
            .proof_hash
            == synthetic_decision,
        "synthetic decision proof changed after reopen"
    );
    ensure!(
        reopened.get_state_root() == activated_root,
        "computed root after reopen differs from activation root"
    );
    ensure!(
        reopened.native_inference_context().as_ref() == Some(&context),
        "activated binding did not persist across reopen"
    );
    assert_preexisting_accounts_preserved(
        &before_accounts,
        &account_snapshot(&reopened)?,
        context_account,
    )?;
    ensure!(
        history_digest(&reopened, args.height, &addresses)? == before_history,
        "reopen lost pre-activation block or account-transaction history"
    );
    drop(reopened);

    let source_after = fingerprint(&source).context("re-fingerprint immutable input")?;
    ensure!(
        source_after == source_before,
        "input state changed during qualification"
    );
    println!("OFFLINE STATE-CODE QUALIFICATION ONLY");
    eprintln!(
        "qualification progress: final checks complete at {:.1}s",
        started.elapsed().as_secs_f64()
    );
    println!("source tree sha256: {source_before}");
    println!("approved recovery manifest: {}", args.manifest);
    println!("replayed height/root: {}/{}", args.height, args.root);
    println!("activation height/context commitment: {activation_height}/{commitment}");
    println!("activated block root after reopen: {activated_root}");
    println!(
        "synthetic offline decision proof: {synthetic_decision} (not DAG-certified; no quorum)"
    );
    println!("pre-existing account digest: {before_account_digest}");
    println!(
        "pre-existing account and history digests preserved; input paths, contents, sizes, and descendant modes unchanged"
    );
    println!(
        "This does not qualify a model, consensus/fleet execution, production configuration, or release."
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("offline recovered-state boundary qualification failed: {error:#}");
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_copy_preserves_content_and_mode_and_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let file = source.join("state.wal");
        fs::write(&file, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        }
        let before = fingerprint(&source).unwrap();
        let target = temp.path().join("private-copy");
        copy_tree_new(&source, &target).unwrap();
        assert_eq!(fingerprint(&source).unwrap(), before);
        assert_eq!(fingerprint(&target).unwrap(), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(target.join("state.wal"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o640
            );
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert!(copy_tree_new(&source, &target).is_err());
    }

    #[test]
    fn history_digest_records_gaps_and_detects_missing_existing_blocks() {
        fn digest(items: &[(u64, Hash256)]) -> String {
            let mut hasher = blake3::Hasher::new_derive_key("test-history");
            append_existing_block_hashes(&mut hasher, 0, 4, items);
            hasher.finalize().to_hex().to_string()
        }
        let sparse = [(1, Hash256([1; 32])), (4, Hash256([4; 32]))];
        assert_eq!(digest(&sparse), digest(&sparse));
        assert_ne!(digest(&sparse), digest(&[(1, Hash256([1; 32]))]));
        assert_ne!(
            digest(&sparse),
            digest(&[
                (1, Hash256([1; 32])),
                (2, Hash256([2; 32])),
                (4, Hash256([4; 32]))
            ])
        );
    }

    #[test]
    fn native_context_account_is_the_only_allowed_added_account() {
        let prior = Hash256([1; 32]);
        let native = hash_bytes(b"ARC-isolated-inference-context-account-v1");
        let before = vec![(prior, Account::new(prior, 9))];
        let after = vec![
            (prior, Account::new(prior, 9)),
            (native, Account::new(native, 0)),
        ];
        assert!(assert_preexisting_accounts_preserved(&before, &after, native).is_ok());
        assert!(assert_preexisting_accounts_preserved(&before, &[], native).is_err());
        assert!(
            assert_preexisting_accounts_preserved(
                &before,
                &[(prior, Account::new(prior, 10))],
                native
            )
            .is_err()
        );
        let unrelated = Hash256([2; 32]);
        assert!(
            assert_preexisting_accounts_preserved(
                &before,
                &[
                    (prior, Account::new(prior, 9)),
                    (unrelated, Account::new(unrelated, 0))
                ],
                native
            )
            .is_err()
        );
        assert!(
            assert_preexisting_accounts_preserved(
                &[
                    (prior, Account::new(prior, 9)),
                    (prior, Account::new(prior, 9))
                ],
                &before,
                native
            )
            .is_err()
        );
        assert!(
            assert_preexisting_accounts_preserved(
                &before,
                &[
                    (prior, Account::new(prior, 9)),
                    (prior, Account::new(prior, 9))
                ],
                native
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_symlinks_are_refused() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(temp.path().join("outside"), b"do not follow").unwrap();
        symlink(temp.path().join("outside"), source.join("link")).unwrap();
        assert!(fingerprint(&source).is_err());
        assert!(copy_tree_new(&source, &temp.path().join("copy")).is_err());
    }
}
