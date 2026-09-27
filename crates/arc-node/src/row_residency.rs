//! Operator-pinned manifests for private fixed-residency row workers.
//!
//! This validates bounded manifest metadata and file sizes only. It does not
//! read model weights or authenticate row-file contents. Partial connections
//! require the shared daemon's exact-stream manifest pin handshake over trusted
//! private SSH; canonical numerical and package qualification remain required. Offline
//! exporter/conformance manifests are inputs to configuration validation,
//! not evidence of production qualification.

use crate::row_cohort::{RowWorkerEntry, machine_address, projection_stages};
use arc_assign::resident::{ResidentRange, WorkerResidency, validate_layout};
use arc_crypto::Hash256;
use arc_inference::low_residency::CanonicalRowSource;
use arc_inference::tensor_parallel::RowAssignment;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const CONFIG_FORMAT: &str = "arc.private-row-residency.v1";
const FULL_FORMAT: &str = "arc.tensor-row-low-residency-bundle.v1";
const PARTITION_FORMAT: &str = "arc.tensor-row-offline-partition-bundle.v1";
const MAX_MANIFEST_BYTES: u64 = 1 << 20;
const MAX_MANIFEST_FILES: usize = 4096;
const MAX_BUNDLE_BYTES: u128 = 1 << 30;
const ARCROW_HEADER_BYTES: u128 = 85;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartialRowConfig {
    pub format: String,
    pub manifests: Vec<ManifestPin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestPin {
    pub worker_id: String,
    pub path: PathBuf,
    pub blake3: Hash256,
}

impl PartialRowConfig {
    /// Check operator intent against the exact worker set before opening files.
    pub fn validate(&self, workers: &[RowWorkerEntry]) -> Result<(), String> {
        if self.format != CONFIG_FORMAT {
            return Err("unsupported private row-residency config format".into());
        }
        if workers.is_empty() || workers.len() > 32 || self.manifests.len() != workers.len() {
            return Err(
                "private row-residency requires one manifest for each of 1..=32 workers".into(),
            );
        }
        let mut configured = BTreeSet::new();
        for worker in workers {
            if worker.id.is_empty()
                || worker.id.len() > 256
                || !configured.insert(worker.id.as_str())
                || !worker.resident_layers.is_empty()
                || worker.resident_output
            {
                return Err(
                    "worker IDs must be unique and legacy residency declarations empty".into(),
                );
            }
        }
        let mut pinned = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for pin in &self.manifests {
            if pin.worker_id.is_empty()
                || pin.worker_id.len() > 256
                || !configured.contains(pin.worker_id.as_str())
                || !pinned.insert(pin.worker_id.as_str())
                || pin.path.as_os_str().is_empty()
                || !pin.path.is_absolute()
                || pin
                    .path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
                || pin.blake3 == Hash256::ZERO
                || !paths.insert(pin.path.as_os_str().to_string_lossy().into_owned())
            {
                return Err("invalid, duplicate, or unpinned manifest entry".into());
            }
        }
        if pinned.len() != configured.len() {
            return Err("manifest pins do not cover the configured worker set".into());
        }
        Ok(())
    }

    /// Load only bounded metadata; row payloads are neither read nor trusted here.
    pub fn load(
        &self,
        workers: &[RowWorkerEntry],
        validator: Hash256,
        model: &dyn CanonicalRowSource,
        artifact: Hash256,
    ) -> Result<Vec<WorkerResidency>, String> {
        self.validate(workers)?;
        if validator == Hash256::ZERO || artifact == Hash256::ZERO {
            return Err("validator and artifact identities must be nonzero".into());
        }
        let profile = model
            .canonical_execution_profile()
            .ok_or("canonical model profile missing")?;
        if profile.is_empty() || profile.len() > 256 {
            return Err("canonical model profile has invalid length".into());
        }
        let (keys, stages) = projection_stages(model);
        if keys.is_empty() || keys.len() != stages.len() || stages.len() > 4096 {
            return Err("canonical model has invalid projection dimensions".into());
        }
        let mut stage_by_key = BTreeMap::new();
        for (index, ((layer, tensor), stage)) in keys.iter().zip(&stages).enumerate() {
            let Some((rows, cols)) = model.projection_shape(*layer, *tensor) else {
                return Err("canonical projection disappeared during validation".into());
            };
            if rows == 0
                || cols == 0
                || stage.rows != rows as u64
                || stage.cols != cols as u64
                || stage.layer != layer.map(|value| value as u32)
                || stage_by_key.insert((*layer, *tensor), index).is_some()
            {
                return Err(
                    "canonical model has invalid or duplicate projection dimensions".into(),
                );
            }
        }

        let workers_by_id: BTreeMap<_, _> = workers
            .iter()
            .map(|worker| (worker.id.as_str(), worker))
            .collect();
        let mut output = Vec::with_capacity(workers.len());
        for pin in &self.manifests {
            let worker = workers_by_id
                .get(pin.worker_id.as_str())
                .ok_or("manifest worker is not configured")?;
            let bytes = read_pinned_manifest(&pin.path, pin.blake3)?;
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|e| format!("manifest JSON: {e}"))?;
            let manifest_format = value
                .get("format")
                .and_then(serde_json::Value::as_str)
                .ok_or("manifest format missing")?;
            if manifest_format != FULL_FORMAT && manifest_format != PARTITION_FORMAT {
                return Err("unsupported row bundle manifest format".into());
            }
            require_string(&value, "artifact_blake3", &artifact.to_hex())?;
            require_string(&value, "execution_profile", profile)?;
            require_string(&value, "worker_id", &pin.worker_id)?;

            let partition = if manifest_format == PARTITION_FORMAT {
                let spec = value
                    .get("row_partition")
                    .ok_or("partition metadata missing")?;
                let rank = spec
                    .get("rank")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("partition rank missing")?;
                let count = spec
                    .get("count")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("partition count missing")?;
                if count == 0 || count > 32 || rank >= count {
                    return Err("invalid row partition bounds".into());
                }
                Some((rank, count))
            } else {
                None
            };
            let files = value
                .get("files")
                .and_then(serde_json::Value::as_array)
                .ok_or("manifest files missing")?;
            if files.is_empty() || files.len() > MAX_MANIFEST_FILES {
                return Err("manifest file count is outside bounds".into());
            }
            let mut seen_names = BTreeSet::new();
            let mut stage_ranges: BTreeMap<usize, (u64, u64)> = BTreeMap::new();
            let mut ranges = Vec::with_capacity(stages.len());
            let mut total_bytes = 0u128;
            for file in files {
                let name = file
                    .get("file")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("row file name missing")?;
                let relative = Path::new(name);
                if name.is_empty()
                    || relative.components().count() != 1
                    || !matches!(relative.components().next(), Some(Component::Normal(_)))
                    || !seen_names.insert(name.to_owned())
                {
                    return Err("row file name must be a unique basename".into());
                }
                let assignment: RowAssignment = serde_json::from_value(
                    file.get("assignment")
                        .cloned()
                        .ok_or("row assignment missing")?,
                )
                .map_err(|e| format!("row assignment: {e}"))?;
                if assignment.artifact_id != artifact
                    || assignment.execution_profile != profile
                    || assignment.worker_id != pin.worker_id
                {
                    return Err("row assignment identity does not match pinned model/worker".into());
                }
                let stage = *stage_by_key
                    .get(&(assignment.layer, assignment.tensor))
                    .ok_or("row assignment names an absent model projection")?;
                let dimensions = &stages[stage];
                let start =
                    u64::try_from(assignment.row_start).map_err(|_| "row start overflow")?;
                let end = u64::try_from(assignment.row_end).map_err(|_| "row end overflow")?;
                if start >= end || end > dimensions.rows {
                    return Err("row assignment has invalid projection dimensions".into());
                }
                if let Some((rank, count)) = partition {
                    let expected_start =
                        u128::from(dimensions.rows) * u128::from(rank) / u128::from(count);
                    let expected_end =
                        u128::from(dimensions.rows) * (u128::from(rank) + 1) / u128::from(count);
                    if u128::from(start) != expected_start || u128::from(end) != expected_end {
                        return Err("row assignment disagrees with declared partition".into());
                    }
                }
                if stage_ranges.insert(stage, (start, end)).is_some() {
                    return Err("duplicate projection interval for worker".into());
                }
                let row_payload_bytes = u128::from(end - start)
                    .checked_mul(u128::from(dimensions.cols) + 8)
                    .ok_or("canonical row payload size overflow")?;
                let expected_size = ARCROW_HEADER_BYTES
                    .checked_add(profile.len() as u128)
                    .and_then(|n| n.checked_add(pin.worker_id.len() as u128))
                    .and_then(|n| n.checked_add(row_payload_bytes))
                    .ok_or("canonical row-file size overflow")?;
                let declared_size = file
                    .get("bytes")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("row file size missing")?;
                if u128::from(declared_size) != expected_size {
                    return Err(
                        "declared row-file size disagrees with canonical ARCROW01 size".into(),
                    );
                }
                let digest = file
                    .get("blake3")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("row file digest missing")?;
                if Hash256::from_hex(digest).map_err(|e| format!("row file digest: {e}"))?
                    == Hash256::ZERO
                {
                    return Err("row file digest must be nonzero".into());
                }
                total_bytes = total_bytes
                    .checked_add(expected_size)
                    .ok_or("worker bundle size overflow")?;
                if total_bytes > MAX_BUNDLE_BYTES {
                    return Err("worker canonical row bundle exceeds 1 GiB".into());
                }
                ranges.push(ResidentRange {
                    stage,
                    row_start: start,
                    row_end: end,
                });
            }
            let declared_total = value
                .get("serialized_row_bytes")
                .and_then(serde_json::Value::as_u64)
                .ok_or("manifest row-byte total missing")?;
            if u128::from(declared_total) != total_bytes {
                return Err("manifest aggregate row bytes disagree with files".into());
            }
            output.push(WorkerResidency {
                worker: machine_address(&validator, worker),
                manifest_hash: pin.blake3,
                ranges,
            });
        }
        validate_layout(&stages, &output, workers.len())?;
        Ok(output)
    }
}

fn require_string(value: &serde_json::Value, key: &str, expected: &str) -> Result<(), String> {
    if value.get(key).and_then(serde_json::Value::as_str) == Some(expected) {
        Ok(())
    } else {
        Err(format!("manifest {key} does not match pinned identity"))
    }
}

fn read_pinned_manifest(path: &Path, expected: Hash256) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("manifest metadata {}: {e}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > MAX_MANIFEST_BYTES
    {
        return Err("manifest must be a regular file of 1 byte to 1 MiB".into());
    }
    let file = fs::File::open(path).map_err(|e| format!("manifest open: {e}"))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("manifest read: {e}"))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES
        || blake3::hash(&bytes).as_bytes() != expected.as_bytes()
    {
        return Err("manifest exceeds bound or pinned BLAKE3 digest mismatches".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_inference::cached_integer_model::{ArithmeticProfile, ModelConfig};
    use arc_inference::tensor_parallel::{TensorKey, TensorParallelError};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TinyModel(ModelConfig);
    impl CanonicalRowSource for TinyModel {
        fn config(&self) -> &ModelConfig {
            &self.0
        }
        fn canonical_execution_profile(&self) -> Option<&'static str> {
            Some("tiny-test-profile-v1")
        }
        fn projection_shape(
            &self,
            layer: Option<usize>,
            tensor: TensorKey,
        ) -> Option<(usize, usize)> {
            match (layer, tensor) {
                (
                    Some(0),
                    TensorKey::Wq
                    | TensorKey::Wk
                    | TensorKey::Wv
                    | TensorKey::Wo
                    | TensorKey::WGate
                    | TensorKey::WUp
                    | TensorKey::WDown,
                )
                | (None, TensorKey::LmHead) => Some((4, 3)),
                _ => None,
            }
        }
        fn projection_rows(
            &self,
            _: Option<usize>,
            _: TensorKey,
            _: usize,
            _: usize,
            _: &[i64],
        ) -> Result<Vec<i64>, TensorParallelError> {
            Err(TensorParallelError::WrongShape)
        }
    }

    fn model() -> TinyModel {
        TinyModel(ModelConfig {
            n_layers: 1,
            d_model: 3,
            n_heads: 1,
            n_kv_heads: 1,
            d_ff: 3,
            d_head: 3,
            d_kv: 3,
            vocab_size: 4,
            attn_scale: 1,
            rope_cos: vec![],
            rope_sin: vec![],
            max_seq: 8,
            eos_tokens: vec![],
            bos_token: 0,
            chat_template: String::new(),
            arithmetic_profile: ArithmeticProfile::LegacySplitHalfV0,
        })
    }

    fn worker(id: &str) -> RowWorkerEntry {
        RowWorkerEntry {
            id: id.into(),
            ssh_program: "ssh".into(),
            target: "worker@example".into(),
            known_hosts: "/tmp/known_hosts".into(),
            remote_command: vec!["worker".into()],
            timeout_ms: 1000,
            startup_timeout_ms: 1000,
            ram_headroom_bytes: 1 << 30,
            max_concurrency: 1,
            resident_layers: vec![],
            resident_output: false,
        }
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "arc-row-residency-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(path.join("rows")).unwrap();
            Self(path)
        }
        fn pin(&self, worker_id: &str, change: Option<&str>) -> ManifestPin {
            let artifact = Hash256([7; 32]);
            let model = model();
            let (keys, stages) = projection_stages(&model);
            let mut files = Vec::new();
            let mut total = 0u64;
            for ((layer, tensor), stage) in keys.into_iter().zip(stages) {
                let (row_start, row_end) = if change == Some("invalid-dimensions") {
                    (0, 5)
                } else {
                    (0, stage.rows as usize)
                };
                let assignment = RowAssignment {
                    artifact_id: if change == Some("wrong-artifact") {
                        Hash256([8; 32])
                    } else {
                        artifact
                    },
                    execution_profile: if change == Some("wrong-profile") {
                        "other-profile".into()
                    } else {
                        "tiny-test-profile-v1".into()
                    },
                    layer,
                    tensor,
                    row_start,
                    row_end,
                    worker_id: worker_id.into(),
                };
                let filename = format!("{}-{:?}.arcrow", layer.unwrap_or(99), tensor);
                let nbytes = 85
                    + assignment.execution_profile.len()
                    + worker_id.len()
                    + (row_end - row_start) * (stage.cols as usize + 8);
                files.push(serde_json::json!({"file":filename,"assignment":assignment,"bytes":nbytes,"blake3":blake3::hash(&[1]).to_hex().to_string()}));
                total += nbytes as u64;
            }
            if change == Some("duplicate") {
                files.push(files[0].clone());
            }
            if change == Some("missing-file-metadata") {
                files[0].as_object_mut().unwrap().remove("bytes");
            }
            let mut value = serde_json::json!({
                "format": PARTITION_FORMAT, "artifact_blake3": artifact.to_hex(),
                "execution_profile": "tiny-test-profile-v1", "worker_id": worker_id,
                "serialized_row_bytes": total, "row_partition":{"rank":0,"count":1}, "files":files
            });
            if change == Some("wrong-worker") {
                value["worker_id"] = serde_json::json!("elsewhere");
            }
            let bytes = serde_json::to_vec(&value).unwrap();
            let path = self.0.join("manifest.json");
            fs::write(&path, &bytes).unwrap();
            ManifestPin {
                worker_id: worker_id.into(),
                path,
                blake3: Hash256(*blake3::hash(&bytes).as_bytes()),
            }
        }
        fn config(pin: ManifestPin) -> PartialRowConfig {
            PartialRowConfig {
                format: CONFIG_FORMAT.into(),
                manifests: vec![pin],
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn private_manifest_load_binds_full_projection_set_including_lm_head() {
        let fixture = Fixture::new();
        let pin = fixture.pin("w1", None);
        let config = Fixture::config(pin);
        config.validate(&[worker("w1")]).unwrap();
        let residency = config
            .load(
                &[worker("w1")],
                Hash256([9; 32]),
                &model(),
                Hash256([7; 32]),
            )
            .unwrap();
        assert_eq!(residency.len(), 1);
        assert_eq!(residency[0].ranges.len(), 8);
        assert!(
            residency[0]
                .ranges
                .iter()
                .any(|range| range.stage == 7 && range.row_start == 0 && range.row_end == 4)
        );
    }

    #[test]
    fn private_manifest_rejects_identity_dimension_duplicate_and_missing_metadata() {
        for failure in [
            "wrong-profile",
            "wrong-artifact",
            "wrong-worker",
            "invalid-dimensions",
            "duplicate",
            "missing-file-metadata",
        ] {
            let fixture = Fixture::new();
            let pin = fixture.pin("w1", Some(failure));
            let result = Fixture::config(pin).load(
                &[worker("w1")],
                Hash256([9; 32]),
                &model(),
                Hash256([7; 32]),
            );
            assert!(result.is_err(), "accepted {failure}");
        }
    }

    #[test]
    fn private_manifest_rejects_oversize_and_wrong_config_pin() {
        let fixture = Fixture::new();
        let pin = fixture.pin("w1", None);
        let config = Fixture::config(pin.clone());
        assert!(config.validate(&[worker("different")]).is_err());
        fs::write(&pin.path, vec![b' '; (MAX_MANIFEST_BYTES + 1) as usize]).unwrap();
        assert!(read_pinned_manifest(&pin.path, pin.blake3).is_err());
    }
}
