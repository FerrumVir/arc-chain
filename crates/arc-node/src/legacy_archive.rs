//! Read-only RPC view over one content-verified legacy ARCCHKPT capture.
//!
//! This module deliberately does not construct [`arc_state::StateDB`]. It has
//! no WAL, mempool, consensus engine, P2P transport, signing key, worker loop,
//! or POST handler. The only state is an immutable, bounded ARCCHKPT payload
//! loaded from an exact SHA-256-pinned regular file.

use arc_crypto::Hash256;
use arc_state::recovery::{ARCCHKPT_MAX_PAYLOAD_BYTES, ArcCheckpoint};
use arc_types::{Account, Block, Transaction, TxReceipt};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{MethodFilter, on},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Take},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

const PROVENANCE_SCHEMA: &str = "arc.legacy-archive.query.v1";
const BINDING_SCHEMA: &str = "arc.recovery.capture-binding.v3";
const MAX_BINDING_BYTES: u64 = 1024 * 1024;
const MAX_ARCHIVE_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_INVENTORY_BYTES: u64 = 64 * 1024;
const MAX_BINDING_INDEX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_BLOCKS_PAGE: usize = 100;
const MAX_BLOCK_TX_PAGE: u32 = 1_000;
const MAX_ACCOUNT_TX_PAGE: u32 = 1_000;

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<Value>)>;

fn api_error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": message.into() })))
}

fn normalize_hash(value: &str, field: &str) -> anyhow::Result<String> {
    let bare = value.strip_prefix("0x").unwrap_or(value);
    anyhow::ensure!(
        bare.len() == 64
            && bare
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "{field} must be exactly 32 lowercase hexadecimal bytes"
    );
    Ok(bare.to_string())
}

fn required_string<'a>(value: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|item| !item.is_empty())
        .ok_or_else(|| anyhow::anyhow!("binding.{key} is missing or not a string"))
}

fn required_u64(value: &Value, key: &str) -> anyhow::Result<u64> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("binding.{key} is missing or not a nonnegative integer"))
}

fn mode_is_read_only(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o222 == 0
    }
    #[cfg(windows)]
    {
        metadata.permissions().readonly()
    }
    #[cfg(not(any(unix, windows)))]
    {
        metadata.permissions().readonly()
    }
}

fn same_open_file(path: &fs::Metadata, opened: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        path.dev() == opened.dev() && path.ino() == opened.ino()
    }
    #[cfg(not(unix))]
    {
        // Content is decoded from the already-opened, SHA-pinned byte string,
        // so a later path replacement cannot alter the served view. Windows
        // does not expose a stable inode through std; length/type checks retain
        // the portable part of the pre-open safety contract.
        path.file_type().is_file() == opened.file_type().is_file()
    }
}

/// Read one exact, mode-read-only regular inode into memory.
///
/// The expected SHA-256 authenticates the bytes. Device/inode checks ensure a
/// path swap cannot make the service describe a different file than it read.
fn read_pinned_regular_file(
    path: &Path,
    expected_sha256: &str,
    max_bytes: u64,
    label: &str,
) -> anyhow::Result<Vec<u8>> {
    read_pinned_regular_file_after_inspect(path, expected_sha256, max_bytes, label, || {})
}

fn read_pinned_regular_file_after_inspect(
    path: &Path,
    expected_sha256: &str,
    max_bytes: u64,
    label: &str,
    after_inspect: impl FnOnce(),
) -> anyhow::Result<Vec<u8>> {
    let expected = normalize_hash(expected_sha256, &format!("expected {label} SHA-256"))?;
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| anyhow::anyhow!("cannot inspect {label} {}: {error}", path.display()))?;
    anyhow::ensure!(
        path_metadata.file_type().is_file() && !path_metadata.file_type().is_symlink(),
        "{label} must be a regular non-symlink file"
    );
    anyhow::ensure!(
        mode_is_read_only(&path_metadata),
        "{label} must be mode-read-only"
    );
    anyhow::ensure!(
        path_metadata.len() <= max_bytes,
        "{label} exceeds its {max_bytes}-byte safety limit"
    );

    after_inspect();
    let file = File::open(path)
        .map_err(|error| anyhow::anyhow!("cannot open {label} {}: {error}", path.display()))?;
    let opened_metadata = file.metadata()?;
    anyhow::ensure!(
        same_open_file(&path_metadata, &opened_metadata)
            && opened_metadata.len() == path_metadata.len(),
        "{label} path identity changed while it was opened"
    );
    let mut bytes = Vec::with_capacity(usize::try_from(opened_metadata.len()).unwrap_or(0));
    let mut bounded: Take<File> = file.take(max_bytes.saturating_add(1));
    bounded.read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 == opened_metadata.len(),
        "{label} changed while read"
    );

    let after = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        after.file_type().is_file()
            && !after.file_type().is_symlink()
            && same_open_file(&after, &opened_metadata)
            && after.len() == opened_metadata.len(),
        "{label} path identity changed after it was read"
    );
    let actual = hex::encode(Sha256::digest(&bytes));
    anyhow::ensure!(
        actual == expected,
        "{label} SHA-256 mismatch: expected {expected}, got {actual}"
    );
    Ok(bytes)
}

fn find_sorted<'a, T>(rows: &'a [(Hash256, T)], hash: &Hash256) -> Option<&'a T> {
    rows.binary_search_by_key(&hash.0, |entry| entry.0.0)
        .ok()
        .map(|index| &rows[index].1)
}

#[derive(Clone, Debug)]
pub struct LegacyArchiveSpec {
    pub archive_manifest: PathBuf,
    pub complete: PathBuf,
    pub inventory: PathBuf,
    pub binding_index: PathBuf,
    pub checkpoint: PathBuf,
    pub binding: PathBuf,
    /// Out-of-band root published by the finalized recovery rollout.
    pub expected_archive_manifest_sha256: String,
    /// Out-of-band completion-marker root published by the finalized rollout.
    pub expected_complete_sha256: String,
    /// Selects one exact validator row; all provenance is derived from files.
    pub node: String,
}

#[derive(Clone, Debug)]
pub enum LegacyArchiveListen {
    /// Retained for explicit local development and non-Unix operator hosts.
    Tcp(SocketAddr),
    /// Production origin transport. Filesystem ownership and modes identify
    /// the archive service to the reviewed filtering proxy.
    #[cfg(unix)]
    Unix(PathBuf),
}

#[derive(Clone, Debug, Serialize)]
pub struct LegacyArchiveProvenance {
    pub schema: &'static str,
    pub read_only: bool,
    pub classification: &'static str,
    pub capture_id: String,
    pub node: String,
    pub rollout_manifest_sha256: String,
    pub archive_manifest_sha256: String,
    pub complete_sha256: String,
    pub bundle_sha256: String,
    pub inventory_sha256: String,
    pub binding_index_sha256: String,
    pub binding_sha256: String,
    pub checkpoint_sha256: String,
    pub checkpoint_manifest_hash: String,
    pub checkpoint_payload_hash: String,
    pub canonical_checkpoint_height: u64,
    pub source_height: u64,
    pub source_block_hash: String,
    pub source_state_root: String,
    pub source_consensus_round: u64,
    pub recovery_epoch: u64,
    pub validator_set_id: u64,
}

fn parse_inventory(bytes: &[u8]) -> anyhow::Result<HashMap<String, String>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("legacy archive inventory is not UTF-8"))?;
    let mut values = HashMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("legacy archive inventory has a malformed line"))?;
        anyhow::ensure!(
            !key.is_empty()
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
            "legacy archive inventory has an unsafe key"
        );
        anyhow::ensure!(
            !value.is_empty(),
            "legacy archive inventory has an empty value"
        );
        anyhow::ensure!(
            values.insert(key.to_string(), value.to_string()).is_none(),
            "legacy archive inventory repeats {key}"
        );
    }
    let scope = values
        .get("archive_scope")
        .ok_or_else(|| anyhow::anyhow!("legacy archive inventory has no archive_scope"))?;
    let expected: &[&str] = match scope.as_str() {
        "complete-stopped-legacy-data-v3" => &[
            "manifest_sha256",
            "capture_id",
            "node",
            "classification",
            "canonical_match",
            "archive_scope",
            "complete_data_dir",
            "excluded_outside_data_dir_private_material",
            "excluded_service_environments",
            "excluded_build_models_and_git",
            "capture_index_sha256",
            "binding_index_sha256",
        ],
        "complete-content-indexed-stopped-legacy-source-v4" => &[
            "manifest_sha256",
            "capture_id",
            "node",
            "classification",
            "canonical_match",
            "archive_scope",
            "source_tree_retained_locally",
            "model_excluded_and_bound_by_rollout",
            "capture_index_sha256",
            "source_index_sha256",
            "binding_index_sha256",
        ],
        _ => anyhow::bail!("legacy archive inventory scope is unsupported"),
    };
    anyhow::ensure!(
        values.len() == expected.len() && expected.iter().all(|key| values.contains_key(*key)),
        "legacy archive inventory fields differ from its sealed schema"
    );
    Ok(values)
}

fn indexed_sha256(index: &[u8], required_path: &str) -> anyhow::Result<String> {
    let text = std::str::from_utf8(index)
        .map_err(|_| anyhow::anyhow!("legacy archive binding index is not UTF-8"))?;
    let mut found = None;
    for line in text.lines() {
        let (digest, path) = line
            .split_once("  ")
            .ok_or_else(|| anyhow::anyhow!("legacy archive binding index has a malformed line"))?;
        let digest = normalize_hash(digest, "binding index member SHA-256")?;
        anyhow::ensure!(
            !path.is_empty()
                && !path.starts_with('/')
                && !path
                    .split('/')
                    .any(|part| part.is_empty() || part == "." || part == "..")
                && path
                    .bytes()
                    .all(|byte| { byte.is_ascii_alphanumeric() || b"_.@/+:-".contains(&byte) }),
            "legacy archive binding index has an unsafe path"
        );
        if path == required_path {
            anyhow::ensure!(
                found.is_none(),
                "legacy archive binding index repeats {required_path}"
            );
            found = Some(digest);
        }
    }
    found.ok_or_else(|| {
        anyhow::anyhow!("legacy archive binding index does not contain {required_path}")
    })
}

#[derive(Clone, Debug, Serialize)]
struct TransactionOccurrence {
    block_height: u64,
    block_hash: String,
    index: u32,
}

/// Immutable archive payload plus indexes derived solely from its verified
/// block vector.
pub struct LegacyArchiveView {
    checkpoint: ArcCheckpoint,
    provenance: LegacyArchiveProvenance,
    transaction_occurrences: HashMap<[u8; 32], Vec<TransactionOccurrence>>,
}

impl LegacyArchiveView {
    pub fn load(spec: &LegacyArchiveSpec) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !spec.node.is_empty()
                && spec.node.len() <= 63
                && spec
                    .node
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
            "archive node name must be lowercase DNS-safe text"
        );
        let archive_manifest = normalize_hash(
            &spec.expected_archive_manifest_sha256,
            "expected archive manifest SHA-256",
        )?;
        let complete_sha256 = normalize_hash(
            &spec.expected_complete_sha256,
            "expected archive completion SHA-256",
        )?;
        let archive_manifest_bytes = read_pinned_regular_file(
            &spec.archive_manifest,
            &archive_manifest,
            MAX_ARCHIVE_METADATA_BYTES,
            "legacy archive manifest",
        )?;
        let archive: Value = serde_json::from_slice(&archive_manifest_bytes)
            .map_err(|error| anyhow::anyhow!("legacy archive manifest is not JSON: {error}"))?;
        anyhow::ensure!(
            required_string(&archive, "schema")? == "arc.recovery.archive-manifest.v2",
            "legacy archive manifest schema is unsupported"
        );
        let capture_id = normalize_hash(
            required_string(&archive, "capture_id")?,
            "archive capture id",
        )?;
        let rollout_manifest = normalize_hash(
            required_string(&archive, "rollout_manifest_sha256")?,
            "archive rollout manifest SHA-256",
        )?;
        let freeze_plan_sha256 = normalize_hash(
            required_string(&archive, "freeze_plan_sha256")?,
            "archive freeze plan SHA-256",
        )?;
        let source_commit = required_string(&archive, "source_commit")?;
        let canonical_checkpoint_height = archive
            .get("canonical_reference")
            .ok_or_else(|| anyhow::anyhow!("legacy archive manifest has no canonical reference"))
            .and_then(|reference| required_u64(reference, "source_height"))?;

        let complete_bytes = read_pinned_regular_file(
            &spec.complete,
            &complete_sha256,
            MAX_ARCHIVE_METADATA_BYTES,
            "legacy archive completion marker",
        )?;
        let complete: Value = serde_json::from_slice(&complete_bytes).map_err(|error| {
            anyhow::anyhow!("legacy archive completion marker is not JSON: {error}")
        })?;
        let complete_fields = [
            "schema",
            "freeze_plan_sha256",
            "capture_id",
            "rollout_manifest_sha256",
            "source_commit",
            "archive_manifest_sha256",
            "object_count_before_complete",
            "validator_bundle_count",
        ];
        anyhow::ensure!(
            complete.as_object().is_some_and(|object| {
                object.len() == complete_fields.len()
                    && complete_fields
                        .iter()
                        .all(|field| object.contains_key(*field))
            }) && required_string(&complete, "schema")? == "arc.recovery.archive-complete.v1"
                && normalize_hash(
                    required_string(&complete, "archive_manifest_sha256")?,
                    "completion archive manifest SHA-256"
                )? == archive_manifest
                && normalize_hash(
                    required_string(&complete, "capture_id")?,
                    "completion capture id"
                )? == capture_id
                && normalize_hash(
                    required_string(&complete, "rollout_manifest_sha256")?,
                    "completion rollout manifest SHA-256"
                )? == rollout_manifest
                && normalize_hash(
                    required_string(&complete, "freeze_plan_sha256")?,
                    "completion freeze plan SHA-256"
                )? == freeze_plan_sha256
                && required_string(&complete, "source_commit")? == source_commit
                && required_u64(&complete, "validator_bundle_count")? == 6,
            "legacy archive completion marker does not seal this exact six-node archive manifest"
        );

        let bundle_rows = archive
            .get("validator_bundles")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!("legacy archive manifest has no validator bundle rows")
            })?;
        anyhow::ensure!(
            bundle_rows.len() == 6,
            "legacy archive manifest is not a six-node capture"
        );
        let matches = bundle_rows
            .iter()
            .filter(|row| row.get("node").and_then(Value::as_str) == Some(spec.node.as_str()))
            .collect::<Vec<_>>();
        anyhow::ensure!(
            matches.len() == 1,
            "legacy archive manifest has no unique row for selected node"
        );
        let bundle_row = matches[0];
        anyhow::ensure!(
            required_string(bundle_row, "classification")? == "valid_noncanonical_fork",
            "archive manifest does not classify the selected node as a valid noncanonical fork"
        );
        let bundle_sha256 = normalize_hash(
            required_string(
                bundle_row
                    .get("bundle")
                    .ok_or_else(|| anyhow::anyhow!("archive validator row has no bundle object"))?,
                "sha256",
            )?,
            "archive bundle SHA-256",
        )?;
        let inventory_sha256 = normalize_hash(
            required_string(
                bundle_row.get("inventory").ok_or_else(|| {
                    anyhow::anyhow!("archive validator row has no inventory object")
                })?,
                "sha256",
            )?,
            "archive inventory SHA-256",
        )?;
        let inventory_bytes = read_pinned_regular_file(
            &spec.inventory,
            &inventory_sha256,
            MAX_INVENTORY_BYTES,
            "legacy archive inventory",
        )?;
        let inventory = parse_inventory(&inventory_bytes)?;
        anyhow::ensure!(
            inventory.get("manifest_sha256") == Some(&rollout_manifest)
                && inventory.get("capture_id") == Some(&capture_id)
                && inventory.get("node") == Some(&spec.node)
                && inventory.get("classification").map(String::as_str)
                    == Some("valid_noncanonical_fork")
                && inventory.get("canonical_match").map(String::as_str) == Some("false"),
            "legacy archive inventory identity/classification differs from the sealed manifest"
        );
        let binding_index_sha256 = normalize_hash(
            inventory.get("binding_index_sha256").ok_or_else(|| {
                anyhow::anyhow!("legacy archive inventory has no binding index root")
            })?,
            "archive binding index SHA-256",
        )?;
        let binding_index_bytes = read_pinned_regular_file(
            &spec.binding_index,
            &binding_index_sha256,
            MAX_BINDING_INDEX_BYTES,
            "legacy archive binding index",
        )?;
        let checkpoint_sha256 = indexed_sha256(&binding_index_bytes, "candidate.arcchkpt")?;
        let binding_sha256 = indexed_sha256(&binding_index_bytes, "binding.json")?;

        let checkpoint_bytes = read_pinned_regular_file(
            &spec.checkpoint,
            &checkpoint_sha256,
            (ARCCHKPT_MAX_PAYLOAD_BYTES as u64).saturating_add(16),
            "legacy archive checkpoint",
        )?;
        let checkpoint = ArcCheckpoint::read_from_bytes(&checkpoint_bytes)?;
        checkpoint.verify_content()?;
        anyhow::ensure!(
            checkpoint.signatures.is_empty(),
            "a legacy fork query checkpoint must remain unsigned and can never be an activation package"
        );
        let binding_bytes = read_pinned_regular_file(
            &spec.binding,
            &binding_sha256,
            MAX_BINDING_BYTES,
            "legacy archive binding",
        )?;
        let binding: Value = serde_json::from_slice(&binding_bytes)
            .map_err(|error| anyhow::anyhow!("legacy archive binding is not JSON: {error}"))?;
        anyhow::ensure!(
            required_string(&binding, "schema")? == BINDING_SCHEMA,
            "legacy archive binding schema is unsupported"
        );
        anyhow::ensure!(
            required_string(&binding, "capture_id")? == capture_id,
            "legacy archive binding capture id differs"
        );
        anyhow::ensure!(
            required_string(&binding, "node")? == spec.node,
            "legacy archive binding node differs"
        );
        anyhow::ensure!(
            normalize_hash(
                required_string(&binding, "rollout_manifest_sha256")?,
                "binding rollout manifest SHA-256"
            )? == rollout_manifest,
            "legacy archive binding rollout manifest differs"
        );
        anyhow::ensure!(
            required_string(&binding, "classification")? == "valid_noncanonical_fork"
                && binding.get("canonical_match").and_then(Value::as_bool) == Some(false)
                && required_u64(&binding, "export_exit_code")? == 0,
            "only an internally valid, explicitly noncanonical fork may be served"
        );
        let exported = binding
            .get("exported")
            .filter(|value| value.is_object())
            .ok_or_else(|| {
                anyhow::anyhow!("legacy archive binding has no valid exported summary")
            })?;

        let expected_checkpoint_manifest = checkpoint.manifest_hash().to_hex();
        let expected_payload = checkpoint.manifest.payload_hash.to_hex();
        let comparisons = [
            (
                normalize_hash(
                    required_string(exported, "source_block_hash")?,
                    "exported source block hash",
                )?,
                checkpoint.manifest.source_block_hash.to_hex(),
                "source block hash",
            ),
            (
                normalize_hash(
                    required_string(exported, "source_state_root")?,
                    "exported source state root",
                )?,
                checkpoint.manifest.source_state_root.to_hex(),
                "source state root",
            ),
            (
                normalize_hash(
                    required_string(exported, "full_state_root")?,
                    "exported full state root",
                )?,
                checkpoint.manifest.full_state_root.to_hex(),
                "full state root",
            ),
            (
                normalize_hash(
                    required_string(exported, "manifest_hash")?,
                    "exported checkpoint manifest hash",
                )?,
                expected_checkpoint_manifest.clone(),
                "checkpoint manifest hash",
            ),
            (
                normalize_hash(
                    required_string(exported, "payload_hash")?,
                    "exported checkpoint payload hash",
                )?,
                expected_payload.clone(),
                "checkpoint payload hash",
            ),
        ];
        for (actual, expected, label) in comparisons {
            anyhow::ensure!(
                actual == expected,
                "binding {label} differs from candidate ARCCHKPT"
            );
        }
        for (field, actual) in [
            ("source_height", checkpoint.manifest.source_height),
            (
                "source_consensus_round",
                checkpoint.manifest.source_consensus_round,
            ),
            ("created_at_unix_ms", checkpoint.manifest.created_at_unix_ms),
            ("recovery_epoch", checkpoint.manifest.recovery_epoch),
            ("validator_set_id", checkpoint.manifest.validator_set_id),
        ] {
            anyhow::ensure!(
                required_u64(exported, field)? == actual,
                "binding {field} differs from candidate ARCCHKPT"
            );
        }

        let mut transaction_occurrences = HashMap::<[u8; 32], Vec<TransactionOccurrence>>::new();
        for (height, block) in &checkpoint.payload.blocks {
            for (index, hash) in block.tx_hashes.iter().enumerate() {
                transaction_occurrences
                    .entry(hash.0)
                    .or_default()
                    .push(TransactionOccurrence {
                        block_height: *height,
                        block_hash: format!("0x{}", block.hash.to_hex()),
                        index: u32::try_from(index).unwrap_or(u32::MAX),
                    });
            }
        }

        Ok(Self {
            provenance: LegacyArchiveProvenance {
                schema: PROVENANCE_SCHEMA,
                read_only: true,
                classification: "valid_noncanonical_fork",
                capture_id,
                node: spec.node.clone(),
                rollout_manifest_sha256: rollout_manifest,
                archive_manifest_sha256: archive_manifest,
                complete_sha256,
                bundle_sha256,
                inventory_sha256,
                binding_index_sha256,
                binding_sha256,
                checkpoint_sha256,
                checkpoint_manifest_hash: expected_checkpoint_manifest,
                checkpoint_payload_hash: expected_payload,
                canonical_checkpoint_height,
                source_height: checkpoint.manifest.source_height,
                source_block_hash: format!("0x{}", checkpoint.manifest.source_block_hash.to_hex()),
                source_state_root: format!("0x{}", checkpoint.manifest.source_state_root.to_hex()),
                source_consensus_round: checkpoint.manifest.source_consensus_round,
                recovery_epoch: checkpoint.manifest.recovery_epoch,
                validator_set_id: checkpoint.manifest.validator_set_id,
            },
            checkpoint,
            transaction_occurrences,
        })
    }

    pub fn provenance(&self) -> &LegacyArchiveProvenance {
        &self.provenance
    }

    fn block(&self, height: u64) -> Option<&Block> {
        self.checkpoint
            .payload
            .blocks
            .binary_search_by_key(&height, |entry| entry.0)
            .ok()
            .map(|index| &self.checkpoint.payload.blocks[index].1)
    }

    fn receipt(&self, hash: &Hash256) -> Option<&TxReceipt> {
        find_sorted(&self.checkpoint.payload.receipts, hash)
    }

    fn transaction(&self, hash: &Hash256) -> Option<&Transaction> {
        find_sorted(&self.checkpoint.payload.full_transactions, hash)
    }

    fn account(&self, address: &Hash256) -> Option<&Account> {
        find_sorted(&self.checkpoint.payload.accounts, address)
    }
}

#[derive(Deserialize)]
struct BlocksQuery {
    from: Option<u64>,
    to: Option<u64>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct BlockTransactionsQuery {
    offset: Option<u32>,
    limit: Option<u32>,
}

#[derive(Deserialize)]
struct AccountTransactionsQuery {
    offset: Option<u32>,
    limit: Option<u32>,
}

fn parse_hash(value: &str) -> Result<Hash256, (StatusCode, Json<Value>)> {
    let normalized = value.strip_prefix("0x").unwrap_or(value);
    Hash256::from_hex(normalized).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "hash must be exactly 32 hexadecimal bytes",
        )
    })
}

async fn provenance(State(view): State<Arc<LegacyArchiveView>>) -> Json<LegacyArchiveProvenance> {
    Json(view.provenance.clone())
}

async fn health(State(view): State<Arc<LegacyArchiveView>>) -> Json<Value> {
    let latest_timestamp = view
        .block(view.provenance.source_height)
        .map(|block| block.header.timestamp)
        .unwrap_or(0);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let last_block_age_secs = if latest_timestamp == 0 {
        None
    } else {
        Some(now_ms.saturating_sub(latest_timestamp) / 1_000)
    };
    Json(json!({
        "status": "archived",
        "read_only": true,
        "chain_advancing": false,
        "last_block_age_secs": last_block_age_secs,
        "height": view.provenance.source_height,
        "peers": 0,
        "archive_manifest_sha256": view.provenance.archive_manifest_sha256,
    }))
}

async fn info(State(view): State<Arc<LegacyArchiveView>>) -> Json<Value> {
    Json(json!({
        "chain": "ARC Chain preserved legacy fork",
        "version": env!("CARGO_PKG_VERSION"),
        "block_height": view.provenance.source_height,
        "account_count": view.checkpoint.payload.accounts.len(),
        "read_only": true,
        "classification": view.provenance.classification,
    }))
}

async fn stats(State(view): State<Arc<LegacyArchiveView>>) -> Json<Value> {
    Json(json!({
        "chain": "ARC Chain preserved legacy fork",
        "version": env!("CARGO_PKG_VERSION"),
        "block_height": view.provenance.source_height,
        "total_accounts": view.checkpoint.payload.accounts.len(),
        "total_transactions": view.transaction_occurrences.len(),
        "indexed_hashes": view.checkpoint.payload.tx_index.len(),
        "indexed_receipts": view.checkpoint.payload.receipts.len(),
        "validators": view.checkpoint.payload.validators.len(),
        "connected_peers": 0,
        "read_only": true,
    }))
}

async fn validators(State(view): State<Arc<LegacyArchiveView>>) -> Json<Value> {
    Json(json!({
        "validators": view.checkpoint.payload.validators.iter().map(|(address, stake)| json!({
            "address": format!("0x{}", address.to_hex()),
            "stake": stake,
            "active": *stake > 0,
        })).collect::<Vec<_>>(),
        "source": "preserved legacy fork checkpoint",
    }))
}

async fn latest_block(State(view): State<Arc<LegacyArchiveView>>) -> ApiResult<Block> {
    view.block(view.provenance.source_height)
        .cloned()
        .map(Json)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "archive source has no tip block"))
}

async fn block(
    State(view): State<Arc<LegacyArchiveView>>,
    AxumPath(height): AxumPath<u64>,
) -> ApiResult<Block> {
    view.block(height).cloned().map(Json).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("block {height} is not retained"),
        )
    })
}

async fn blocks(
    State(view): State<Arc<LegacyArchiveView>>,
    Query(query): Query<BlocksQuery>,
) -> Json<Value> {
    let from = query.from.unwrap_or(0);
    let to = query.to.unwrap_or(view.provenance.source_height);
    let limit = query.limit.unwrap_or(20).min(MAX_BLOCKS_PAGE);
    let rows = view
        .checkpoint
        .payload
        .blocks
        .iter()
        .rev()
        .filter(|(height, _)| *height >= from && *height <= to)
        .take(limit)
        .map(|(_, block)| {
            json!({
                "height": block.header.height,
                "hash": block.hash.to_hex(),
                "parent_hash": block.header.parent_hash.to_hex(),
                "state_root": block.header.state_root.to_hex(),
                "tx_root": block.header.tx_root.to_hex(),
                "tx_count": block.header.tx_count,
                "timestamp": block.header.timestamp,
                "producer": block.header.producer.to_hex(),
            })
        })
        .collect::<Vec<_>>();
    Json(json!({ "from": from, "to": to, "limit": limit, "count": rows.len(), "blocks": rows }))
}

async fn block_transactions(
    State(view): State<Arc<LegacyArchiveView>>,
    AxumPath(height): AxumPath<u64>,
    Query(query): Query<BlockTransactionsQuery>,
) -> ApiResult<Value> {
    let block = view.block(height).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("block {height} is not retained"),
        )
    })?;
    let offset = query.offset.unwrap_or(0);
    let limit = query.limit.unwrap_or(100).min(MAX_BLOCK_TX_PAGE);
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(block.tx_hashes.len());
    let end = start
        .saturating_add(limit as usize)
        .min(block.tx_hashes.len());
    let rows = block.tx_hashes[start..end]
        .iter()
        .enumerate()
        .map(|(relative, hash)| {
            json!({
                "index": start + relative,
                "hash": hash.to_hex(),
                "receipt_retained": view.receipt(hash).is_some(),
                "full_transaction_retained": view.transaction(hash).is_some(),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "block_height": height,
        "block_hash": format!("0x{}", block.hash.to_hex()),
        "tx_count": block.header.tx_count,
        "offset": offset,
        "limit": limit,
        "returned": rows.len(),
        "transactions": rows,
    })))
}

async fn transaction_receipt(
    State(view): State<Arc<LegacyArchiveView>>,
    AxumPath(raw): AxumPath<String>,
) -> ApiResult<TxReceipt> {
    let hash = parse_hash(&raw)?;
    view.receipt(&hash).cloned().map(Json).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "receipt was not retained by this source before capture",
        )
    })
}

async fn full_transaction(
    State(view): State<Arc<LegacyArchiveView>>,
    AxumPath(raw): AxumPath<String>,
) -> ApiResult<Value> {
    let hash = parse_hash(&raw)?;
    let transaction = view.transaction(&hash).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "full transaction body was not retained by this source before capture",
        )
    })?;
    Ok(Json(json!({
        "transaction": transaction,
        "receipt": view.receipt(&hash),
        "archive_provenance": view.provenance,
    })))
}

async fn transaction_occurrences(
    State(view): State<Arc<LegacyArchiveView>>,
    AxumPath(raw): AxumPath<String>,
) -> ApiResult<Value> {
    let hash = parse_hash(&raw)?;
    let occurrences = view.transaction_occurrences.get(&hash.0).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "transaction hash does not occur in a retained block",
        )
    })?;
    Ok(Json(json!({
        "schema": "arc.legacy-archive.transaction-occurrences.v1",
        "tx_hash": format!("0x{}", hash.to_hex()),
        "occurrences": occurrences,
        "unique_occurrence": occurrences.len() == 1,
        "receipt_retained": view.receipt(&hash).is_some(),
        "full_transaction_retained": view.transaction(&hash).is_some(),
        "retention_note": if view.receipt(&hash).is_some() && view.transaction(&hash).is_some() {
            Value::Null
        } else {
            Value::String("the stopped legacy node pruned some transaction detail before capture; block inclusion is preserved without inventing the missing body or receipt".to_string())
        },
    })))
}

async fn account(
    State(view): State<Arc<LegacyArchiveView>>,
    AxumPath(raw): AxumPath<String>,
) -> ApiResult<Account> {
    let address = parse_hash(&raw)?;
    view.account(&address).cloned().map(Json).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "account is not retained in this archive checkpoint",
        )
    })
}

async fn account_transactions(
    State(view): State<Arc<LegacyArchiveView>>,
    AxumPath(raw): AxumPath<String>,
    Query(query): Query<AccountTransactionsQuery>,
) -> ApiResult<Value> {
    let address = parse_hash(&raw)?;
    let hashes = find_sorted(&view.checkpoint.payload.account_txs, &address).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            "account transaction index is not retained",
        )
    })?;
    let offset = query.offset.unwrap_or(0);
    let limit = query.limit.unwrap_or(100).min(MAX_ACCOUNT_TX_PAGE);
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(hashes.len());
    let end = start.saturating_add(limit as usize).min(hashes.len());
    Ok(Json(json!({
        "address": format!("0x{}", address.to_hex()),
        "tx_count": hashes.len(),
        "offset": offset,
        "limit": limit,
        "returned": end.saturating_sub(start),
        "tx_hashes": hashes[start..end].iter().map(|hash| hash.to_hex()).collect::<Vec<_>>(),
    })))
}

async fn reject_non_get(request: axum::extract::Request, next: Next) -> Response {
    if request.method() != Method::GET {
        return api_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "legacy archive accepts GET only",
        )
        .into_response();
    }
    next.run(request).await
}

/// Build the complete GET-only archive API. There is intentionally no merge
/// with the validator router and no route that mutates state.
pub fn router(view: Arc<LegacyArchiveView>) -> Router {
    Router::new()
        .route("/provenance", on(MethodFilter::GET, provenance))
        .route("/health", on(MethodFilter::GET, health))
        .route("/info", on(MethodFilter::GET, info))
        .route("/stats", on(MethodFilter::GET, stats))
        .route("/validators", on(MethodFilter::GET, validators))
        .route("/block/latest", on(MethodFilter::GET, latest_block))
        .route("/block/{height}", on(MethodFilter::GET, block))
        .route("/blocks", on(MethodFilter::GET, blocks))
        .route(
            "/block/{height}/txs",
            on(MethodFilter::GET, block_transactions),
        )
        .route("/tx/{hash}", on(MethodFilter::GET, transaction_receipt))
        .route("/tx/{hash}/full", on(MethodFilter::GET, full_transaction))
        .route(
            "/tx/{hash}/occurrences",
            on(MethodFilter::GET, transaction_occurrences),
        )
        .route("/account/{address}", on(MethodFilter::GET, account))
        .route(
            "/account/{address}/txs",
            on(MethodFilter::GET, account_transactions),
        )
        .with_state(view)
        .layer(axum::middleware::from_fn(reject_non_get))
}

pub async fn serve(spec: LegacyArchiveSpec, listen: LegacyArchiveListen) -> anyhow::Result<()> {
    let view = Arc::new(LegacyArchiveView::load(&spec)?);
    match listen {
        LegacyArchiveListen::Tcp(address) => {
            anyhow::ensure!(
                address.ip().is_loopback(),
                "legacy archive TCP RPC must bind loopback; production must use --listen-unix behind the reviewed gateway"
            );
            let listener = tokio::net::TcpListener::bind(address).await?;
            tracing::info!(
                listen = %address,
                node = %view.provenance.node,
                source_height = view.provenance.source_height,
                archive_manifest_sha256 = %view.provenance.archive_manifest_sha256,
                "Serving immutable noncanonical legacy archive over explicit development TCP (GET only)"
            );
            axum::serve(listener, router(view)).await?;
        }
        #[cfg(unix)]
        LegacyArchiveListen::Unix(path) => {
            let (listener, _socket_guard) = crate::unix_listener::bind(&path)?;
            tracing::info!(
                listen_unix = %path.display(),
                node = %view.provenance.node,
                source_height = view.provenance.source_height,
                archive_manifest_sha256 = %view.provenance.archive_manifest_sha256,
                "Serving immutable noncanonical legacy archive over sealed Unix transport (GET only)"
            );
            axum::serve(listener, router(view)).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::{KeyPair, MerkleTree, hash_bytes};
    use arc_state::recovery::{
        ARCCHKPT_MAGIC, RECOVERY_PROTOCOL_VERSION, RecoveryContext, RecoveryManifest,
        RecoveryPayload, RecoverySignature, RecoveryValidator, recovery_stake_reserve_address,
    };
    use arc_types::{Account, BlockHeader, ProtocolVersion};
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request},
    };
    use tower::ServiceExt;

    #[cfg(unix)]
    fn sealed_runtime_directory() -> tempfile::TempDir {
        use std::os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _};

        let directory = tempfile::Builder::new()
            .prefix(".arc-archive-uds-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o750)).unwrap();
        let path = std::ffi::CString::new(directory.path().as_os_str().as_bytes()).unwrap();
        // SAFETY: chown receives a live NUL-terminated path, preserves uid,
        // and selects the process's effective primary group.
        assert_eq!(
            unsafe { libc::chown(path.as_ptr(), u32::MAX, libc::getegid()) },
            0
        );
        directory
    }

    fn sha256(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn make_read_only(path: &Path) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn make_owner_writable(path: &Path) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            permissions.set_mode(permissions.mode() | 0o200);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn write_read_only(path: &Path, bytes: &[u8]) -> String {
        fs::write(path, bytes).unwrap();
        make_read_only(path);
        sha256(bytes)
    }

    fn checkpoint() -> (ArcCheckpoint, Hash256) {
        let mut source_validators = (0..8)
            .map(|index| {
                (
                    hash_bytes(format!("legacy-validator-{index}").as_bytes()),
                    5_000_000,
                )
            })
            .collect::<Vec<_>>();
        source_validators.sort_by_key(|entry| entry.0.0);
        let reserve = recovery_stake_reserve_address();
        let mut accounts = vec![(reserve, Account::new(reserve, 80_000_000))];
        accounts.sort_by_key(|entry| entry.0.0);
        let transaction_hash = hash_bytes(b"retained-block-inclusion-with-pruned-detail");
        let mut payload = RecoveryPayload {
            blocks: Vec::new(),
            accounts,
            storage: Vec::new(),
            contracts: Vec::new(),
            receipts: Vec::new(),
            full_transactions: Vec::new(),
            tx_index: Vec::new(),
            account_txs: Vec::new(),
            identities: Vec::new(),
            event_logs: Vec::new(),
            validators: source_validators,
            staking_pool: 40_000_000,
        };
        let source_state_root = payload.legacy_state_root();
        let genesis = Block::genesis();
        let block = Block::new(
            BlockHeader {
                height: 1,
                timestamp: 1_787_777_000_000,
                parent_hash: genesis.hash,
                tx_root: MerkleTree::from_leaves(vec![transaction_hash]).root(),
                state_root: source_state_root,
                proof_hash: Hash256::ZERO,
                tx_count: 1,
                producer: Hash256::ZERO,
                protocol_version: ProtocolVersion::GENESIS,
                state_diff: None,
            },
            vec![transaction_hash],
        );
        payload.blocks = vec![(0, genesis), (1, block.clone())];

        let keys = (0..6)
            .map(|_| KeyPair::generate_ed25519())
            .collect::<Vec<_>>();
        let mut validators = keys
            .iter()
            .enumerate()
            .map(|(index, key)| RecoveryValidator {
                address: key.address(),
                public_key: key.public_key_bytes().try_into().unwrap(),
                stake: 6_666_666 + u64::from(index < 4),
            })
            .collect::<Vec<_>>();
        validators.sort_by_key(|validator| validator.address.0);
        let context = RecoveryContext::new("0x415243", hash_bytes(b"archive-test-genesis"), 1, 1);
        let full_state_root = payload
            .transition_consensus_state_root(&context, Some(2), &validators)
            .unwrap();
        let manifest = RecoveryManifest {
            format_version: 1,
            chain_id: "0x415243".into(),
            genesis_hash: hash_bytes(b"archive-test-genesis"),
            source_height: 1,
            source_block_hash: block.hash,
            source_state_root,
            source_consensus_round: 99,
            recovery_epoch: 1,
            validator_set_id: 1,
            protocol_version: RECOVERY_PROTOCOL_VERSION,
            validators,
            community_rewards_v1_activation_height: Some(2),
            full_state_root,
            payload_hash: payload.content_hash(),
            created_at_unix_ms: 1_787_777_000_000,
        };
        let checkpoint = ArcCheckpoint {
            magic: ARCCHKPT_MAGIC,
            manifest,
            payload,
            signatures: Vec::new(),
        };
        checkpoint.verify_content().unwrap();
        (checkpoint, transaction_hash)
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        spec: LegacyArchiveSpec,
        transaction_hash: Hash256,
    }

    fn fixture_with_canonical_height(
        manifest_classification: &str,
        signed: bool,
        canonical_checkpoint_height: u64,
    ) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let capture_id = "11".repeat(32);
        let rollout_manifest_sha256 = "22".repeat(32);
        let node = "nyc";
        let (mut checkpoint, transaction_hash) = checkpoint();
        if signed {
            checkpoint.signatures.push(RecoverySignature {
                validator: Hash256::ZERO,
                public_key: [0; 32],
                signature_halves: [[0; 32]; 2],
            });
        }
        let checkpoint_path = root.join("candidate.arcchkpt");
        checkpoint.write_to(&checkpoint_path).unwrap();
        make_read_only(&checkpoint_path);
        let checkpoint_sha256 = sha256(&fs::read(&checkpoint_path).unwrap());

        let exported = json!({
            "source_height": checkpoint.manifest.source_height,
            "source_block_hash": checkpoint.manifest.source_block_hash.to_hex(),
            "source_state_root": checkpoint.manifest.source_state_root.to_hex(),
            "full_state_root": checkpoint.manifest.full_state_root.to_hex(),
            "source_consensus_round": checkpoint.manifest.source_consensus_round,
            "created_at_unix_ms": checkpoint.manifest.created_at_unix_ms,
            "recovery_epoch": checkpoint.manifest.recovery_epoch,
            "validator_set_id": checkpoint.manifest.validator_set_id,
            "manifest_hash": checkpoint.manifest_hash().to_hex(),
            "payload_hash": checkpoint.manifest.payload_hash.to_hex(),
        });
        let binding = serde_json::to_vec(&json!({
            "schema": BINDING_SCHEMA,
            "capture_id": capture_id,
            "node": node,
            "rollout_manifest_sha256": rollout_manifest_sha256,
            "classification": "valid_noncanonical_fork",
            "canonical_match": false,
            "export_exit_code": 0,
            "exported": exported,
        }))
        .unwrap();
        let binding_path = root.join("binding.json");
        let binding_sha256 = write_read_only(&binding_path, &binding);

        let binding_index =
            format!("{checkpoint_sha256}  candidate.arcchkpt\n{binding_sha256}  binding.json\n");
        let binding_index_path = root.join("binding.files.sha256");
        let binding_index_sha256 = write_read_only(&binding_index_path, binding_index.as_bytes());
        let inventory = format!(
            "manifest_sha256={rollout_manifest_sha256}\n\
             capture_id={capture_id}\n\
             node={node}\n\
             classification=valid_noncanonical_fork\n\
             canonical_match=false\n\
             archive_scope=complete-content-indexed-stopped-legacy-source-v4\n\
             source_tree_retained_locally=true\n\
             model_excluded_and_bound_by_rollout=true\n\
             capture_index_sha256={}\n\
             source_index_sha256={}\n\
             binding_index_sha256={binding_index_sha256}\n",
            "33".repeat(32),
            "44".repeat(32),
        );
        let inventory_path = root.join("legacy-nyc.inventory");
        let inventory_sha256 = write_read_only(&inventory_path, inventory.as_bytes());
        let rows = ["nyc", "lax", "ams", "lhr", "nrt", "sgp"]
            .into_iter()
            .map(|row_node| {
                json!({
                    "node": row_node,
                    "classification": if row_node == node { manifest_classification } else { "valid_canonical" },
                    "bundle": { "sha256": sha256(format!("bundle-{row_node}").as_bytes()) },
                    "inventory": { "sha256": if row_node == node { inventory_sha256.clone() } else { sha256(format!("inventory-{row_node}").as_bytes()) } },
                })
            })
            .collect::<Vec<_>>();
        let archive_manifest = serde_json::to_vec(&json!({
            "schema": "arc.recovery.archive-manifest.v2",
            "freeze_plan_sha256": "55".repeat(32),
            "capture_id": capture_id,
            "rollout_manifest_sha256": rollout_manifest_sha256,
            "source_commit": "66".repeat(20),
            "canonical_reference": { "source_height": canonical_checkpoint_height },
            "validator_bundles": rows,
        }))
        .unwrap();
        let archive_manifest_path = root.join("ARCHIVE-MANIFEST.json");
        let archive_manifest_sha256 = write_read_only(&archive_manifest_path, &archive_manifest);
        let complete = serde_json::to_vec(&json!({
            "schema": "arc.recovery.archive-complete.v1",
            "freeze_plan_sha256": "55".repeat(32),
            "capture_id": capture_id,
            "rollout_manifest_sha256": rollout_manifest_sha256,
            "source_commit": "66".repeat(20),
            "archive_manifest_sha256": archive_manifest_sha256,
            "object_count_before_complete": 1,
            "validator_bundle_count": 6,
        }))
        .unwrap();
        let complete_path = root.join("COMPLETE.json");
        let complete_sha256 = write_read_only(&complete_path, &complete);
        Fixture {
            spec: LegacyArchiveSpec {
                archive_manifest: archive_manifest_path,
                complete: complete_path,
                inventory: inventory_path,
                binding_index: binding_index_path,
                checkpoint: checkpoint_path,
                binding: binding_path,
                expected_archive_manifest_sha256: archive_manifest_sha256,
                expected_complete_sha256: complete_sha256,
                node: node.into(),
            },
            transaction_hash,
            _directory: directory,
        }
    }

    fn fixture(manifest_classification: &str, signed: bool) -> Fixture {
        fixture_with_canonical_height(manifest_classification, signed, 0)
    }

    #[tokio::test]
    async fn archive_is_get_only_and_preserves_pruned_block_inclusion() {
        let fixture = fixture("valid_noncanonical_fork", false);
        let view = Arc::new(LegacyArchiveView::load(&fixture.spec).unwrap());
        assert_eq!(view.provenance.canonical_checkpoint_height, 0);
        assert_eq!(view.provenance.source_height, 1);
        let app = router(view);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/tx/{}/occurrences",
                        fixture.transaction_hash.to_hex()
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["unique_occurrence"], true);
        assert_eq!(body["receipt_retained"], false);
        assert_eq!(body["occurrences"][0]["block_height"], 1);

        for method in [
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/provenance")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        }
    }

    #[test]
    fn archive_accepts_explicit_noncanonical_forks_at_or_below_checkpoint_height() {
        for canonical_checkpoint_height in [1, 2] {
            let fixture = fixture_with_canonical_height(
                "valid_noncanonical_fork",
                false,
                canonical_checkpoint_height,
            );
            let view = LegacyArchiveView::load(&fixture.spec).unwrap();
            assert_eq!(view.provenance.source_height, 1);
            assert_eq!(
                view.provenance.canonical_checkpoint_height,
                canonical_checkpoint_height
            );
        }
    }

    #[tokio::test]
    async fn account_transaction_index_is_offset_paginated_and_hard_capped() {
        let fixture = fixture("valid_noncanonical_fork", false);
        let mut view = LegacyArchiveView::load(&fixture.spec).unwrap();
        let address = hash_bytes(b"archive-account-pagination");
        view.checkpoint.payload.account_txs = vec![(
            address,
            (0..1_500u64)
                .map(|index| hash_bytes(&index.to_le_bytes()))
                .collect(),
        )];
        let response = router(Arc::new(view))
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/account/{}/txs?offset=7&limit=999999",
                        address.to_hex()
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 256 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["tx_count"], 1_500);
        assert_eq!(body["offset"], 7);
        assert_eq!(body["limit"], MAX_ACCOUNT_TX_PAGE);
        assert_eq!(body["returned"], MAX_ACCOUNT_TX_PAGE);
        assert_eq!(body["tx_hashes"].as_array().unwrap().len(), 1_000);
    }

    #[test]
    fn archive_rejects_wrong_manifest_classification_and_signed_activation_material() {
        let canonical = fixture("valid_canonical", false);
        assert!(
            LegacyArchiveView::load(&canonical.spec)
                .err()
                .unwrap()
                .to_string()
                .contains("noncanonical fork")
        );
        let signed = fixture("valid_noncanonical_fork", true);
        assert!(
            LegacyArchiveView::load(&signed.spec)
                .err()
                .unwrap()
                .to_string()
                .contains("must remain unsigned")
        );

        let mut mismatched_complete = fixture("valid_noncanonical_fork", false);
        let path = &mismatched_complete.spec.complete;
        make_owner_writable(path);
        let mut value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        value["source_commit"] = json!("77".repeat(20));
        let bytes = serde_json::to_vec(&value).unwrap();
        fs::write(path, &bytes).unwrap();
        make_read_only(path);
        mismatched_complete.spec.expected_complete_sha256 = sha256(&bytes);
        assert!(
            LegacyArchiveView::load(&mismatched_complete.spec)
                .err()
                .unwrap()
                .to_string()
                .contains("exact six-node archive manifest")
        );
    }

    #[tokio::test]
    async fn archive_server_refuses_non_loopback_bind() {
        let fixture = fixture("valid_noncanonical_fork", false);
        let error = serve(
            fixture.spec,
            LegacyArchiveListen::Tcp("0.0.0.0:0".parse().unwrap()),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("must bind loopback"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn archive_server_serves_over_sealed_unix_transport_and_cleans_up() {
        let fixture = fixture("valid_noncanonical_fork", false);
        let runtime = sealed_runtime_directory();
        let socket_path = runtime.path().join("archive.sock");
        let server_path = socket_path.clone();
        let server = tokio::spawn(async move {
            serve(fixture.spec, LegacyArchiveListen::Unix(server_path)).await
        });
        let client = reqwest::Client::builder()
            .unix_socket(socket_path.clone())
            .build()
            .unwrap();
        let mut response = None;
        for _ in 0..200 {
            if let Ok(candidate) = client.get("http://localhost/health").send().await {
                response = Some(candidate);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            response.expect("archive UDS did not start").status(),
            StatusCode::OK
        );
        drop(client);
        server.abort();
        let _ = server.await;
        assert!(
            !socket_path.exists(),
            "exact archive socket inode was not removed after server cancellation"
        );
    }

    #[test]
    fn archive_rejects_wrong_root_and_corrupt_checkpoint_bytes() {
        let mut wrong_root = fixture("valid_noncanonical_fork", false);
        wrong_root.spec.expected_archive_manifest_sha256 = "ff".repeat(32);
        assert!(
            LegacyArchiveView::load(&wrong_root.spec)
                .err()
                .unwrap()
                .to_string()
                .contains("SHA-256 mismatch")
        );

        let corrupt = fixture("valid_noncanonical_fork", false);
        make_owner_writable(&corrupt.spec.checkpoint);
        let mut bytes = fs::read(&corrupt.spec.checkpoint).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&corrupt.spec.checkpoint, bytes).unwrap();
        make_read_only(&corrupt.spec.checkpoint);
        assert!(
            LegacyArchiveView::load(&corrupt.spec)
                .err()
                .unwrap()
                .to_string()
                .contains("checkpoint SHA-256 mismatch")
        );
    }

    #[cfg(unix)]
    #[test]
    fn pinned_file_rejects_inode_swap_even_when_replacement_bytes_match() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pinned");
        let replacement = directory.path().join("replacement");
        let expected = write_read_only(&path, b"same authenticated bytes");
        write_read_only(&replacement, b"same authenticated bytes");
        let error =
            read_pinned_regular_file_after_inspect(&path, &expected, 1024, "test pin", || {
                fs::rename(&replacement, &path).unwrap()
            })
            .unwrap_err();
        assert!(error.to_string().contains("path identity changed"));
    }
}
