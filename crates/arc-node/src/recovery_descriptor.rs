//! Verification for the compact public projection of a protected ARCCHKPT.
//!
//! The full checkpoint remains in the protected recovery handoff.  A release
//! descriptor is safe to use as the community-node retirement trust surface
//! only if it contains enough of `RecoveryManifest` to recompute the exact
//! validator-signed hash.  Treating a manifest hash and human-readable fields
//! as two unrelated assertions would let a valid certificate decorate a false
//! height, root, or transition projection.

use crate::config;
use anyhow::{Context, Result, ensure};
use arc_crypto::{Hash256, Signature, hash_bytes};
use arc_state::recovery::{
    ARCCHKPT_FORMAT_VERSION, ARCCHKPT_MAX_PAYLOAD_BYTES, RECOVERY_PROTOCOL_VERSION,
    RECOVERY_SIGNATURES_REQUIRED, RECOVERY_VALIDATOR_SET_SIZE, RecoveryManifest, RecoveryValidator,
};
use arc_types::{ProtocolVersion, strict_supermajority_threshold};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;

const DESCRIPTOR_SCHEMA: &str = "arc-recovery-checkpoint-descriptor/v1";
const DESCRIPTOR_MAX_BYTES: u64 = 1024 * 1024;
const EXPECTED_REPOSITORY: &str = "FerrumVir/arc-chain";
const EXPECTED_CHAIN_ID: &str = "0x415243";
const EXPECTED_SOURCE_HEIGHT: u64 = 137_145;
const EXPECTED_TRANSITION_HEIGHT: u64 = 137_146;
const EXPECTED_RECOVERY_EPOCH: u64 = 1;
const EXPECTED_VALIDATOR_SET_ID: u64 = 1;
const EXPECTED_REWARD_ACTIVATION_HEIGHT: u64 = 137_146;
const EXPECTED_FLEET: [(&str, &str, &str, u64); RECOVERY_VALIDATOR_SET_SIZE] = [
    (
        "nyc",
        "149.28.32.76",
        "adf4ff16f997c871c16f3897e67881311d08f975f28ebdcf79e86ea9e3b99d0f",
        6_666_667,
    ),
    (
        "lax",
        "140.82.16.112",
        "44d20543df6e76696da2ebbbd79e4243cd41729fa5b890e2618991e489314780",
        6_666_667,
    ),
    (
        "ams",
        "136.244.109.1",
        "5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21",
        6_666_667,
    ),
    (
        "lhr",
        "104.238.171.11",
        "228787281308d6c1a560848c2c168814bde1b6153e9e65a286d7211f04628fdd",
        6_666_667,
    ),
    (
        "nrt",
        "202.182.107.41",
        "f03cbab49cf553a05541ddebc09b32a4c5507efb157d354b6d7f8c6682c32f5f",
        6_666_666,
    ),
    (
        "sgp",
        "149.28.153.31",
        "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b",
        6_666_666,
    ),
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecoveryCheckpointDescriptor {
    schema_version: String,
    repository: String,
    release_tag: String,
    release_commit: String,
    recovery_manifest_sha256: String,
    freeze_plan_sha256: String,
    capture_id: String,
    inspector_binary_sha256: String,
    checkpoint_file: DescriptorCheckpointFile,
    canonical_inspection: DescriptorInspection,
    checkpoint_certificate: DescriptorCertificate,
    approved_validators: Vec<DescriptorApprovedValidator>,
    verified_quorum: DescriptorQuorum,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DescriptorCheckpointFile {
    filename: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DescriptorInspection {
    format_version: u16,
    chain_id: String,
    manifest_hash: String,
    payload_hash: String,
    network_genesis_hash: String,
    full_state_root: String,
    source_height: u64,
    source_consensus_round: u64,
    created_at_unix_ms: u64,
    source_block_hash: String,
    source_state_root: String,
    transition_height: u64,
    transition_block_hash: String,
    recovery_domain: String,
    recovery_epoch: u64,
    validator_set_id: u64,
    protocol_version: String,
    validator_count: usize,
    #[serde(deserialize_with = "deserialize_explicit_optional_u64")]
    community_rewards_v1_activation_height: Option<u64>,
}

/// JSON must contain the activation field even when its value is `null`.
/// The explicit deserializer prevents serde's ordinary missing-`Option`
/// fallback from weakening the descriptor's exact-schema contract.
fn deserialize_explicit_optional_u64<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<u64>::deserialize(deserializer)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DescriptorCertificate {
    signing_hash: String,
    validators: Vec<DescriptorCertificateValidator>,
    signatures: Vec<DescriptorCertificateSignature>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DescriptorCertificateValidator {
    address: String,
    public_key: String,
    stake: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DescriptorCertificateSignature {
    validator: String,
    public_key: String,
    signature: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DescriptorApprovedValidator {
    address: String,
    host: String,
    name: String,
    origin: String,
    stake: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DescriptorQuorum {
    status: String,
    required_signatures: usize,
    verified_signature_count: usize,
    validator_count: usize,
    signed_validator_addresses: Vec<String>,
    signed_stake: u64,
    total_stake: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct VerifiedDescriptorSummary {
    pub(crate) status: &'static str,
    pub(crate) manifest_hash: String,
    pub(crate) signing_hash: String,
    pub(crate) network_genesis_hash: String,
    pub(crate) recovery_domain: String,
    pub(crate) recovery_epoch: u64,
    pub(crate) validator_set_id: u64,
    pub(crate) source_height: u64,
    pub(crate) transition_height: u64,
    pub(crate) validator_count: usize,
    pub(crate) verified_signature_count: usize,
    pub(crate) signed_stake: u64,
    pub(crate) total_stake: u64,
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn decode_lower_hex<const N: usize>(value: &str, label: &str) -> Result<[u8; N]> {
    ensure!(
        is_lower_hex(value, N * 2),
        "{label} must be exactly {} lowercase hexadecimal characters",
        N * 2
    );
    let decoded = hex::decode(value).with_context(|| format!("{label} is not hexadecimal"))?;
    decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("{label} must decode to exactly {N} bytes"))
}

fn parse_hash(value: &str, label: &str) -> Result<Hash256> {
    Ok(Hash256(decode_lower_hex::<32>(value, label)?))
}

fn validate_release_tag(tag: &str) -> Result<()> {
    let Some(version) = tag.strip_prefix('v') else {
        anyhow::bail!("descriptor release_tag must start with v");
    };
    let components = version.split('.').collect::<Vec<_>>();
    ensure!(
        components.len() == 3,
        "descriptor release_tag must be canonical vX.Y.Z"
    );
    let mut parsed = Vec::with_capacity(3);
    for component in components {
        ensure!(
            !component.is_empty()
                && component.as_bytes().iter().all(u8::is_ascii_digit)
                && (component == "0" || !component.starts_with('0')),
            "descriptor release_tag must be canonical vX.Y.Z"
        );
        parsed.push(
            component
                .parse::<u64>()
                .context("descriptor release_tag component exceeds u64")?,
        );
    }
    ensure!(
        (parsed[0], parsed[1], parsed[2]) >= (0, 8, 0),
        "descriptor release_tag predates the v0.8 recovery release floor"
    );
    Ok(())
}

fn read_descriptor(path: &Path) -> Result<RecoveryCheckpointDescriptor> {
    let mut file = arc_crypto::secret_file::open_owned_nofollow_read(path)
        .with_context(|| format!("failed to no-follow open descriptor {}", path.display()))?;
    let length = file
        .metadata()
        .context("failed to inspect open recovery descriptor")?
        .len();
    ensure!(
        length > 0 && length <= DESCRIPTOR_MAX_BYTES,
        "recovery descriptor must be between 1 and {DESCRIPTOR_MAX_BYTES} bytes"
    );
    let mut bytes = Vec::with_capacity(length as usize);
    file.read_to_end(&mut bytes)
        .context("failed to read bounded recovery descriptor")?;
    ensure!(
        bytes.len() as u64 == length,
        "recovery descriptor length changed while reading"
    );
    serde_json::from_slice(&bytes).context("recovery descriptor JSON is invalid or non-canonical")
}

fn parse_protocol_version(value: &str) -> Result<ProtocolVersion> {
    ensure!(
        value == RECOVERY_PROTOCOL_VERSION.to_string(),
        "descriptor protocol_version is not the recovery protocol"
    );
    Ok(RECOVERY_PROTOCOL_VERSION)
}

fn fixed_expected_validator_set() -> Result<Vec<(Hash256, u64)>> {
    let mut expected = EXPECTED_FLEET
        .iter()
        .enumerate()
        .map(|(index, (_, _, address, stake))| {
            Ok((
                parse_hash(address, &format!("built-in ARC validator #{index}"))?,
                *stake,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    expected.sort_by_key(|entry| entry.0.0);
    Ok(expected)
}

fn verify_descriptor_with_expected(
    descriptor: &RecoveryCheckpointDescriptor,
    genesis: Option<&config::GenesisConfig>,
    expected_validator_set: &[(Hash256, u64)],
) -> Result<VerifiedDescriptorSummary> {
    ensure!(
        descriptor.schema_version == DESCRIPTOR_SCHEMA,
        "recovery descriptor schema is unsupported"
    );
    ensure!(
        descriptor.repository == EXPECTED_REPOSITORY,
        "recovery descriptor targets a different repository"
    );
    validate_release_tag(&descriptor.release_tag)?;
    ensure!(
        is_lower_hex(&descriptor.release_commit, 40),
        "descriptor release_commit must be one full lowercase Git SHA"
    );
    for (value, label) in [
        (
            descriptor.recovery_manifest_sha256.as_str(),
            "recovery manifest SHA-256",
        ),
        (
            descriptor.freeze_plan_sha256.as_str(),
            "freeze-plan SHA-256",
        ),
        (descriptor.capture_id.as_str(), "capture ID"),
        (
            descriptor.inspector_binary_sha256.as_str(),
            "inspector binary SHA-256",
        ),
        (
            descriptor.checkpoint_file.sha256.as_str(),
            "checkpoint SHA-256",
        ),
    ] {
        ensure!(
            is_lower_hex(value, 64),
            "descriptor {label} must be one lowercase 32-byte hexadecimal value"
        );
    }
    ensure!(
        descriptor.checkpoint_file.filename == "recovery.arcchkpt",
        "descriptor checkpoint filename is unsupported"
    );
    ensure!(
        descriptor.checkpoint_file.size_bytes >= 16
            && descriptor.checkpoint_file.size_bytes <= ARCCHKPT_MAX_PAYLOAD_BYTES as u64 + 16,
        "descriptor checkpoint size is outside the ARCCHKPT v1 bound"
    );

    let inspection = &descriptor.canonical_inspection;
    ensure!(
        inspection.format_version == ARCCHKPT_FORMAT_VERSION,
        "descriptor checkpoint format version is unsupported"
    );
    ensure!(
        inspection.validator_count == RECOVERY_VALIDATOR_SET_SIZE,
        "descriptor inspection has the wrong validator count"
    );
    ensure!(
        inspection.chain_id == EXPECTED_CHAIN_ID
            && inspection.source_height == EXPECTED_SOURCE_HEIGHT
            && inspection.transition_height == EXPECTED_TRANSITION_HEIGHT
            && inspection.recovery_epoch == EXPECTED_RECOVERY_EPOCH
            && inspection.validator_set_id == EXPECTED_VALIDATOR_SET_ID
            && inspection.community_rewards_v1_activation_height
                == Some(EXPECTED_REWARD_ACTIVATION_HEIGHT),
        "descriptor does not bind the fixed ARC H=137145 to H+1=137146 recovery policy"
    );
    let protocol_version = parse_protocol_version(&inspection.protocol_version)?;

    let descriptor_genesis_hash = parse_hash(
        &inspection.network_genesis_hash,
        "descriptor network_genesis_hash",
    )?;
    ensure!(
        descriptor_genesis_hash != Hash256::ZERO,
        "descriptor network genesis must not be zero"
    );

    ensure!(
        expected_validator_set.len() == RECOVERY_VALIDATOR_SET_SIZE
            && expected_validator_set
                .windows(2)
                .all(|window| window[0].0.0 < window[1].0.0),
        "expected ARC recovery validator set is not the canonical six"
    );
    if let Some(genesis) = genesis {
        let expected_genesis_hash = genesis.network_hash(false)?;
        ensure!(
            descriptor_genesis_hash == expected_genesis_hash,
            "descriptor network genesis differs from the signed genesis"
        );
        ensure!(
            inspection.chain_id == genesis.chain.chain_id
                && inspection.community_rewards_v1_activation_height
                    == genesis.chain.community_rewards_v1_activation_height,
            "descriptor chain policy differs from the signed genesis"
        );
        let mut genesis_validators = genesis.validated_validator_set(false)?;
        genesis_validators.sort_by_key(|entry| entry.0.0);
        ensure!(
            genesis_validators == expected_validator_set,
            "signed genesis differs from the fixed six-validator ARC recovery set"
        );
    }
    ensure!(
        descriptor.checkpoint_certificate.validators.len() == RECOVERY_VALIDATOR_SET_SIZE,
        "descriptor certificate must contain exactly six validators"
    );

    let mut manifest_validators: Vec<RecoveryValidator> =
        Vec::with_capacity(RECOVERY_VALIDATOR_SET_SIZE);
    for (index, (raw, (expected_address, expected_stake))) in descriptor
        .checkpoint_certificate
        .validators
        .iter()
        .zip(expected_validator_set.iter())
        .enumerate()
    {
        let address = parse_hash(
            &raw.address,
            &format!("descriptor certificate validator #{index} address"),
        )?;
        let public_key = decode_lower_hex::<32>(
            &raw.public_key,
            &format!("descriptor certificate validator #{index} public key"),
        )?;
        ensure!(
            address == *expected_address && raw.stake == *expected_stake && raw.stake > 0,
            "descriptor certificate validator #{index} differs from signed genesis"
        );
        ensure!(
            hash_bytes(&public_key) == address,
            "descriptor certificate validator #{index} public key derives to another address"
        );
        if let Some(previous) = manifest_validators.last() {
            ensure!(
                previous.address.0 < address.0,
                "descriptor certificate validators are not strictly address-ordered"
            );
        }
        manifest_validators.push(RecoveryValidator {
            address,
            public_key,
            stake: raw.stake,
        });
    }

    let manifest = RecoveryManifest {
        format_version: inspection.format_version,
        chain_id: inspection.chain_id.clone(),
        genesis_hash: descriptor_genesis_hash,
        source_height: inspection.source_height,
        source_block_hash: parse_hash(
            &inspection.source_block_hash,
            "descriptor source_block_hash",
        )?,
        source_state_root: parse_hash(
            &inspection.source_state_root,
            "descriptor source_state_root",
        )?,
        source_consensus_round: inspection.source_consensus_round,
        recovery_epoch: inspection.recovery_epoch,
        validator_set_id: inspection.validator_set_id,
        protocol_version,
        validators: manifest_validators,
        community_rewards_v1_activation_height: inspection.community_rewards_v1_activation_height,
        full_state_root: parse_hash(&inspection.full_state_root, "descriptor full_state_root")?,
        payload_hash: parse_hash(&inspection.payload_hash, "descriptor payload_hash")?,
        created_at_unix_ms: inspection.created_at_unix_ms,
    };
    let manifest_hash = manifest.content_hash();
    ensure!(
        manifest_hash == parse_hash(&inspection.manifest_hash, "descriptor manifest_hash")?,
        "descriptor fields do not reconstruct the validator-signed manifest hash"
    );
    let signing_hash = manifest.signing_hash();
    ensure!(
        signing_hash
            == parse_hash(
                &descriptor.checkpoint_certificate.signing_hash,
                "descriptor signing_hash",
            )?,
        "descriptor signing hash does not derive from its manifest"
    );
    ensure!(
        manifest.recovery_context().domain_hash()
            == parse_hash(&inspection.recovery_domain, "descriptor recovery_domain")?,
        "descriptor recovery domain does not derive from its manifest"
    );
    let transition = manifest.transition_block()?;
    ensure!(
        transition.header.height == inspection.transition_height
            && transition.hash
                == parse_hash(
                    &inspection.transition_block_hash,
                    "descriptor transition_block_hash",
                )?,
        "descriptor transition block does not derive from its manifest"
    );

    ensure!(
        descriptor.approved_validators.len() == RECOVERY_VALIDATOR_SET_SIZE,
        "descriptor approved validator inventory is not the exact six"
    );
    let expected_by_address = expected_validator_set
        .iter()
        .copied()
        .collect::<HashMap<_, _>>();
    let mut approved_addresses = HashSet::new();
    for (index, (approved, (expected_name, expected_host, _, _))) in descriptor
        .approved_validators
        .iter()
        .zip(EXPECTED_FLEET.iter())
        .enumerate()
    {
        ensure!(
            approved.name == *expected_name
                && approved.host == *expected_host
                && approved.origin == format!("http://{expected_host}:9090"),
            "descriptor approved validator #{index} has an unexpected fleet identity"
        );
        let address = parse_hash(
            &approved.address,
            &format!("descriptor approved validator #{index} address"),
        )?;
        ensure!(
            approved_addresses.insert(address),
            "descriptor approved validator inventory repeats an address"
        );
        ensure!(
            expected_by_address.get(&address) == Some(&approved.stake),
            "descriptor approved validator #{index} differs from signed genesis"
        );
    }

    let certificate = &descriptor.checkpoint_certificate;
    ensure!(
        certificate.signatures.len() >= RECOVERY_SIGNATURES_REQUIRED
            && certificate.signatures.len() <= RECOVERY_VALIDATOR_SET_SIZE,
        "descriptor certificate lacks the required 5-of-6 identity quorum"
    );
    let validators_by_address = manifest
        .validators
        .iter()
        .map(|validator| (validator.address, validator))
        .collect::<HashMap<_, _>>();
    let mut signed_addresses: Vec<Hash256> = Vec::with_capacity(certificate.signatures.len());
    let mut signed_stake = 0u64;
    for (index, approval) in certificate.signatures.iter().enumerate() {
        let address = parse_hash(
            &approval.validator,
            &format!("descriptor signature #{index} validator"),
        )?;
        if let Some(previous) = signed_addresses.last() {
            ensure!(
                previous.0 < address.0,
                "descriptor signatures are not strictly validator-ordered"
            );
        }
        let validator = validators_by_address.get(&address).ok_or_else(|| {
            anyhow::anyhow!("descriptor signature #{index} has an unknown signer")
        })?;
        let public_key = decode_lower_hex::<32>(
            &approval.public_key,
            &format!("descriptor signature #{index} public key"),
        )?;
        ensure!(
            public_key == validator.public_key,
            "descriptor signature #{index} public key differs from its validator"
        );
        let signature = decode_lower_hex::<64>(
            &approval.signature,
            &format!("descriptor signature #{index}"),
        )?;
        Signature::Ed25519 {
            public_key,
            signature: signature.to_vec(),
        }
        .verify(&signing_hash, &address)
        .with_context(|| format!("descriptor signature #{index} is invalid"))?;
        signed_stake = signed_stake
            .checked_add(validator.stake)
            .context("descriptor signed stake exceeds u64::MAX")?;
        signed_addresses.push(address);
    }
    let total_stake = manifest
        .validators
        .iter()
        .try_fold(0u64, |total, validator| {
            total
                .checked_add(validator.stake)
                .context("descriptor validator stake exceeds u64::MAX")
        })?;
    ensure!(
        signed_stake >= strict_supermajority_threshold(total_stake),
        "descriptor certificate lacks a strict signed-stake supermajority"
    );

    let quorum = &descriptor.verified_quorum;
    ensure!(
        quorum.status == "VERIFIED_QUORUM"
            && quorum.required_signatures == RECOVERY_SIGNATURES_REQUIRED
            && quorum.verified_signature_count == signed_addresses.len()
            && quorum.validator_count == RECOVERY_VALIDATOR_SET_SIZE
            && quorum.signed_stake == signed_stake
            && quorum.total_stake == total_stake,
        "descriptor quorum summary differs from its verified certificate"
    );
    let summary_addresses = quorum
        .signed_validator_addresses
        .iter()
        .enumerate()
        .map(|(index, value)| {
            parse_hash(value, &format!("descriptor quorum signer address #{index}"))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        summary_addresses == signed_addresses,
        "descriptor quorum signer inventory differs from its certificate"
    );

    Ok(VerifiedDescriptorSummary {
        status: "VERIFIED_DESCRIPTOR_QUORUM",
        manifest_hash: manifest_hash.to_hex(),
        signing_hash: signing_hash.to_hex(),
        network_genesis_hash: manifest.genesis_hash.to_hex(),
        recovery_domain: manifest.recovery_context().domain_hash().to_hex(),
        recovery_epoch: manifest.recovery_epoch,
        validator_set_id: manifest.validator_set_id,
        source_height: manifest.source_height,
        transition_height: transition.header.height,
        validator_count: manifest.validators.len(),
        verified_signature_count: signed_addresses.len(),
        signed_stake,
        total_stake,
    })
}

fn verify_descriptor(
    descriptor: &RecoveryCheckpointDescriptor,
    genesis: Option<&config::GenesisConfig>,
) -> Result<VerifiedDescriptorSummary> {
    let expected = fixed_expected_validator_set()?;
    verify_descriptor_with_expected(descriptor, genesis, &expected)
}

pub(crate) fn verify_and_print(descriptor_path: &Path, genesis_path: &Path) -> Result<()> {
    let descriptor = read_descriptor(descriptor_path)?;
    let genesis_path = genesis_path
        .to_str()
        .context("genesis path is not valid UTF-8")?;
    let genesis = config::load_genesis(genesis_path)
        .context("descriptor verification requires the signed production genesis")?;
    let summary = verify_descriptor(&descriptor, Some(&genesis))?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

/// Verify the self-contained, validator-certified cutover descriptor for the
/// local retirement protocol. The release binding independently pins these
/// exact bytes; this check supplies the cryptographic and fixed-network layer.
pub(crate) fn verify_for_retirement(descriptor_path: &Path) -> Result<VerifiedDescriptorSummary> {
    let descriptor = read_descriptor(descriptor_path)?;
    verify_descriptor(&descriptor, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::KeyPair;

    fn verify_fixture_descriptor(
        descriptor: &RecoveryCheckpointDescriptor,
        genesis: &config::GenesisConfig,
    ) -> Result<VerifiedDescriptorSummary> {
        let mut expected = genesis.validated_validator_set(false)?;
        expected.sort_by_key(|entry| entry.0.0);
        verify_descriptor_with_expected(descriptor, Some(genesis), &expected)
    }

    fn fixture() -> (RecoveryCheckpointDescriptor, config::GenesisConfig) {
        let keys = (0..RECOVERY_VALIDATOR_SET_SIZE)
            .map(|_| KeyPair::generate_ed25519())
            .collect::<Vec<_>>();
        let stakes = [10u64; RECOVERY_VALIDATOR_SET_SIZE];
        let mut validators = keys
            .iter()
            .zip(stakes)
            .map(|(key, stake)| RecoveryValidator {
                address: key.address(),
                public_key: key.public_key_bytes().try_into().unwrap(),
                stake,
            })
            .collect::<Vec<_>>();
        validators.sort_by_key(|validator| validator.address.0);
        let chain = config::ChainInfo {
            name: "descriptor-test".into(),
            chain_id: "0x415243".into(),
            validator_set_complete: true,
            community_rewards_v1_activation_height: Some(137_146),
        };
        let genesis = config::GenesisConfig {
            chain,
            accounts: keys
                .iter()
                .map(|key| config::GenesisAccount {
                    address: key.address().to_hex(),
                    balance: 0,
                })
                .collect(),
            validators: keys
                .iter()
                .zip(stakes)
                .map(|(key, stake)| config::GenesisValidator {
                    address: Some(key.address().to_hex()),
                    insecure_dev_seed: None,
                    stake,
                })
                .collect(),
        };
        let manifest = RecoveryManifest {
            format_version: ARCCHKPT_FORMAT_VERSION,
            chain_id: genesis.chain.chain_id.clone(),
            genesis_hash: genesis.network_hash(false).unwrap(),
            source_height: 137_145,
            source_block_hash: hash_bytes(b"descriptor source block"),
            source_state_root: hash_bytes(b"descriptor source state"),
            source_consensus_round: 91,
            recovery_epoch: 1,
            validator_set_id: 1,
            protocol_version: RECOVERY_PROTOCOL_VERSION,
            validators: validators.clone(),
            community_rewards_v1_activation_height: Some(137_146),
            full_state_root: hash_bytes(b"descriptor full state"),
            payload_hash: hash_bytes(b"descriptor payload"),
            created_at_unix_ms: 1_700_000_000_000,
        };
        let signing_hash = manifest.signing_hash();
        let transition = manifest.transition_block().unwrap();
        let key_by_address = keys
            .iter()
            .map(|key| (key.address(), key))
            .collect::<HashMap<_, _>>();
        let signatures = validators
            .iter()
            .take(RECOVERY_SIGNATURES_REQUIRED)
            .map(|validator| {
                let Signature::Ed25519 {
                    public_key,
                    signature,
                } = key_by_address[&validator.address]
                    .sign(&signing_hash)
                    .unwrap()
                else {
                    panic!("fixture key is not Ed25519")
                };
                DescriptorCertificateSignature {
                    validator: validator.address.to_hex(),
                    public_key: hex::encode(public_key),
                    signature: hex::encode(signature),
                }
            })
            .collect::<Vec<_>>();
        let signed_stake = validators
            .iter()
            .take(RECOVERY_SIGNATURES_REQUIRED)
            .map(|validator| validator.stake)
            .sum();
        let total_stake = validators.iter().map(|validator| validator.stake).sum();
        let approved_validators = EXPECTED_FLEET
            .iter()
            .zip(keys.iter().zip(stakes))
            .map(
                |((name, host, _, _), (key, stake))| DescriptorApprovedValidator {
                    address: key.address().to_hex(),
                    host: (*host).into(),
                    name: (*name).into(),
                    origin: format!("http://{host}:9090"),
                    stake,
                },
            )
            .collect();
        let descriptor = RecoveryCheckpointDescriptor {
            schema_version: DESCRIPTOR_SCHEMA.into(),
            repository: EXPECTED_REPOSITORY.into(),
            release_tag: "v0.8.0".into(),
            release_commit: "a".repeat(40),
            recovery_manifest_sha256: "b".repeat(64),
            freeze_plan_sha256: "c".repeat(64),
            capture_id: "d".repeat(64),
            inspector_binary_sha256: "e".repeat(64),
            checkpoint_file: DescriptorCheckpointFile {
                filename: "recovery.arcchkpt".into(),
                size_bytes: 1024,
                sha256: "f".repeat(64),
            },
            canonical_inspection: DescriptorInspection {
                format_version: manifest.format_version,
                chain_id: manifest.chain_id.clone(),
                manifest_hash: manifest.content_hash().to_hex(),
                payload_hash: manifest.payload_hash.to_hex(),
                network_genesis_hash: manifest.genesis_hash.to_hex(),
                full_state_root: manifest.full_state_root.to_hex(),
                source_height: manifest.source_height,
                source_consensus_round: manifest.source_consensus_round,
                created_at_unix_ms: manifest.created_at_unix_ms,
                source_block_hash: manifest.source_block_hash.to_hex(),
                source_state_root: manifest.source_state_root.to_hex(),
                transition_height: transition.header.height,
                transition_block_hash: transition.hash.to_hex(),
                recovery_domain: manifest.recovery_context().domain_hash().to_hex(),
                recovery_epoch: manifest.recovery_epoch,
                validator_set_id: manifest.validator_set_id,
                protocol_version: manifest.protocol_version.to_string(),
                validator_count: manifest.validators.len(),
                community_rewards_v1_activation_height: manifest
                    .community_rewards_v1_activation_height,
            },
            checkpoint_certificate: DescriptorCertificate {
                signing_hash: signing_hash.to_hex(),
                validators: validators
                    .iter()
                    .map(|validator| DescriptorCertificateValidator {
                        address: validator.address.to_hex(),
                        public_key: hex::encode(validator.public_key),
                        stake: validator.stake,
                    })
                    .collect(),
                signatures: signatures.clone(),
            },
            approved_validators,
            verified_quorum: DescriptorQuorum {
                status: "VERIFIED_QUORUM".into(),
                required_signatures: RECOVERY_SIGNATURES_REQUIRED,
                verified_signature_count: signatures.len(),
                validator_count: RECOVERY_VALIDATOR_SET_SIZE,
                signed_validator_addresses: signatures
                    .iter()
                    .map(|signature| signature.validator.clone())
                    .collect(),
                signed_stake,
                total_stake,
            },
        };
        (descriptor, genesis)
    }

    #[test]
    fn certificate_reconstructs_and_verifies_the_exact_manifest() {
        let (descriptor, genesis) = fixture();
        let summary = verify_fixture_descriptor(&descriptor, &genesis).unwrap();
        assert_eq!(summary.status, "VERIFIED_DESCRIPTOR_QUORUM");
        assert_eq!(summary.transition_height, 137_146);
        assert_eq!(summary.verified_signature_count, 5);
    }

    #[test]
    fn projected_height_or_root_cannot_float_beside_a_valid_certificate() {
        let (mut descriptor, genesis) = fixture();
        descriptor.canonical_inspection.source_height += 1;
        let error = verify_fixture_descriptor(&descriptor, &genesis)
            .unwrap_err()
            .to_string();
        assert!(error.contains("fixed ARC H=137145 to H+1=137146 recovery policy"));

        let (mut descriptor, genesis) = fixture();
        descriptor.canonical_inspection.full_state_root = hash_bytes(b"forged").to_hex();
        assert!(verify_fixture_descriptor(&descriptor, &genesis).is_err());
    }

    #[test]
    fn signature_and_quorum_tampering_fail_closed() {
        let (mut descriptor, genesis) = fixture();
        descriptor.checkpoint_certificate.signatures[0].signature = "0".repeat(128);
        assert!(verify_fixture_descriptor(&descriptor, &genesis).is_err());

        let (mut descriptor, genesis) = fixture();
        descriptor.checkpoint_certificate.signatures.pop();
        assert!(verify_fixture_descriptor(&descriptor, &genesis).is_err());

        let (mut descriptor, genesis) = fixture();
        descriptor.verified_quorum.signed_stake -= 1;
        assert!(verify_fixture_descriptor(&descriptor, &genesis).is_err());
    }

    #[test]
    fn nullable_activation_is_mandatory_and_legacy_tags_are_rejected() {
        let (descriptor, _genesis) = fixture();
        let mut value = serde_json::to_value(&descriptor).unwrap();
        value["canonical_inspection"]
            .as_object_mut()
            .unwrap()
            .remove("community_rewards_v1_activation_height");
        assert!(serde_json::from_value::<RecoveryCheckpointDescriptor>(value).is_err());

        let (mut descriptor, genesis) = fixture();
        descriptor.release_tag = "v0.7.99".into();
        assert!(verify_fixture_descriptor(&descriptor, &genesis).is_err());
    }
}
