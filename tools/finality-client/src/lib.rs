//! Offline verifier for the exact `/finality/{height}` and `/block/{height}`
//! JSON payloads. Trust comes from a separately pinned manifest, never from
//! fields in either RPC response.

use arc_consensus::{
    ConsensusDomain, Validator, ValidatorSet,
    view_change::{FinalityCertificate, validator_set_hash},
};
use arc_crypto::Hash256;
use arc_types::Block;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use thiserror::Error;

pub const MAX_TRUST_BYTES: usize = 1024 * 1024;
pub const MAX_FINALITY_JSON_BYTES: usize = 1024 * 1024;
pub const MAX_BLOCK_JSON_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CERTIFICATE_BYTES: usize = 256 * 1024;
pub const MAX_VALIDATORS: usize = 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustManifest {
    pub schema_version: u16,
    /// Host identity from the independently pinned capture/trust receipt.
    pub host: String,
    /// Human/audit anchor from the operator-verified recovery inputs.
    pub chain_genesis: Hash256,
    pub checkpoint_height: u64,
    pub checkpoint_hash: Hash256,
    pub domain_hash: Hash256,
    pub recovery_epoch: u64,
    pub validator_set_id: u64,
    pub validator_set_epoch: u64,
    pub validator_set_hash: Hash256,
    pub total_stake: u64,
    pub quorum: u64,
    pub validators: Vec<TrustedValidator>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedValidator {
    pub address: Hash256,
    pub stake: u64,
}

#[derive(Debug, Deserialize)]
struct FinalityResponse {
    height: u64,
    block_hash: Hash256,
    state_root: Hash256,
    tx_root: Hash256,
    validator_set_hash: Hash256,
    voters: Vec<Hash256>,
    signing_stake: Option<u64>,
    quorum: u64,
    total_stake: u64,
    certificate_bincode_hex: String,
}

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("input exceeds size limit: {0}")]
    TooLarge(&'static str),
    #[error("trust manifest SHA-256 does not match the pinned value")]
    TrustDigest,
    #[error("invalid trust manifest: {0}")]
    Trust(String),
    #[error("invalid RPC JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid certificate hex")]
    CertificateHex,
    #[error("certificate decode failed: {0}")]
    CertificateDecode(String),
    #[error("certificate verification failed: {0}")]
    CertificateVerify(String),
    #[error("RPC projection mismatch: {0}")]
    Projection(&'static str),
    #[error("block hash does not match its serialized header")]
    BlockHash,
    #[error("block transaction count or Merkle root does not match its body")]
    BlockTransactions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFinality {
    pub host: String,
    pub trust_sha256: String,
    pub checkpoint_height: u64,
    pub height: u64,
    pub block_hash: Hash256,
    pub state_root: Hash256,
    pub tx_root: Hash256,
    pub signing_stake: u64,
    pub quorum: u64,
}

pub fn verify_payloads(
    trust_bytes: &[u8],
    expected_trust_sha256: &str,
    observed_host: &str,
    expected_height: u64,
    finality_json: &[u8],
    block_json: &[u8],
) -> Result<VerifiedFinality, VerifyError> {
    if trust_bytes.len() > MAX_TRUST_BYTES {
        return Err(VerifyError::TooLarge("trust manifest"));
    }
    if finality_json.len() > MAX_FINALITY_JSON_BYTES {
        return Err(VerifyError::TooLarge("finality response"));
    }
    if block_json.len() > MAX_BLOCK_JSON_BYTES {
        return Err(VerifyError::TooLarge("block response"));
    }

    let digest = format!("{:x}", Sha256::digest(trust_bytes));
    if !constant_time_hex_eq(&digest, expected_trust_sha256) {
        return Err(VerifyError::TrustDigest);
    }

    let trust: TrustManifest = serde_json::from_slice(trust_bytes)?;
    let (domain, set) = build_trusted_domain_and_set(&trust, observed_host, expected_height)?;
    let response: FinalityResponse = serde_json::from_slice(finality_json)?;
    let block: Block = serde_json::from_slice(block_json)?;

    if expected_height <= trust.checkpoint_height {
        return Err(VerifyError::Trust(
            "expected certificate height must be above the trusted checkpoint".into(),
        ));
    }
    if response.height != expected_height {
        return Err(VerifyError::Projection(
            "response height differs from requested height",
        ));
    }
    if block.header.height != expected_height {
        return Err(VerifyError::Projection(
            "block height differs from requested height",
        ));
    }
    if block.hash != Block::compute_hash(&block.header) {
        return Err(VerifyError::BlockHash);
    }
    if block.header.tx_count as usize != block.tx_hashes.len()
        || arc_crypto::MerkleTree::from_leaves(block.tx_hashes.clone()).root()
            != block.header.tx_root
    {
        return Err(VerifyError::BlockTransactions);
    }

    if response.certificate_bincode_hex.len() > MAX_CERTIFICATE_BYTES * 2
        || response.certificate_bincode_hex.len() % 2 != 0
    {
        return Err(VerifyError::TooLarge("certificate hex"));
    }
    let encoded =
        hex::decode(&response.certificate_bincode_hex).map_err(|_| VerifyError::CertificateHex)?;
    let certificate: FinalityCertificate = arc_bincode::deserialize_limited_exact::<
        FinalityCertificate,
        MAX_CERTIFICATE_BYTES,
    >(&encoded)
    .map_err(|error| VerifyError::CertificateDecode(error.to_string()))?;
    if certificate.height != expected_height {
        return Err(VerifyError::Projection(
            "certificate height differs from requested height",
        ));
    }
    let signing_stake = certificate
        .verify(&domain, &set)
        .map_err(|error| VerifyError::CertificateVerify(error.to_string()))?;

    if response.block_hash != certificate.block_hash {
        return Err(VerifyError::Projection(
            "JSON block_hash differs from certificate",
        ));
    }
    if response.state_root != certificate.state_root {
        return Err(VerifyError::Projection(
            "JSON state_root differs from certificate",
        ));
    }
    if response.tx_root != certificate.tx_root {
        return Err(VerifyError::Projection(
            "JSON tx_root differs from certificate",
        ));
    }
    if response.validator_set_hash != certificate.validator_set_hash {
        return Err(VerifyError::Projection(
            "JSON validator_set_hash differs from certificate",
        ));
    }
    if response.voters
        != certificate
            .votes
            .iter()
            .map(|vote| vote.voter)
            .collect::<Vec<_>>()
    {
        return Err(VerifyError::Projection(
            "JSON voters differ from certificate vote order",
        ));
    }
    if response.signing_stake != Some(signing_stake) {
        return Err(VerifyError::Projection(
            "JSON signing_stake differs from verified stake",
        ));
    }
    if response.quorum != set.quorum || response.total_stake != set.total_stake {
        return Err(VerifyError::Projection(
            "JSON committee totals differ from trusted committee",
        ));
    }
    if certificate.block_hash != block.hash {
        return Err(VerifyError::Projection(
            "certificate block_hash differs from block",
        ));
    }
    if certificate.state_root != block.header.state_root {
        return Err(VerifyError::Projection(
            "certificate state_root differs from block header",
        ));
    }
    if certificate.tx_root != block.header.tx_root {
        return Err(VerifyError::Projection(
            "certificate tx_root differs from block header",
        ));
    }

    Ok(VerifiedFinality {
        host: trust.host,
        trust_sha256: digest,
        checkpoint_height: trust.checkpoint_height,
        height: expected_height,
        block_hash: certificate.block_hash,
        state_root: certificate.state_root,
        tx_root: certificate.tx_root,
        signing_stake,
        quorum: set.quorum,
    })
}

fn build_trusted_domain_and_set(
    trust: &TrustManifest,
    observed_host: &str,
    expected_height: u64,
) -> Result<(ConsensusDomain, ValidatorSet), VerifyError> {
    if trust.schema_version != 1 {
        return Err(VerifyError::Trust("unsupported schema_version".into()));
    }
    if trust.host.is_empty() || trust.host != observed_host {
        return Err(VerifyError::Trust(
            "observed host differs from pinned trust host".into(),
        ));
    }
    if trust.chain_genesis == Hash256::ZERO || trust.checkpoint_hash == Hash256::ZERO {
        return Err(VerifyError::Trust(
            "genesis/checkpoint anchor must be nonzero".into(),
        ));
    }
    if trust.domain_hash == Hash256::ZERO {
        return Err(VerifyError::Trust("domain_hash must be nonzero".into()));
    }
    if expected_height <= trust.checkpoint_height {
        return Err(VerifyError::Trust(
            "expected certificate height must be above the trusted checkpoint".into(),
        ));
    }
    if trust.validators.is_empty() || trust.validators.len() > MAX_VALIDATORS {
        return Err(VerifyError::Trust(
            "validator count is outside supported bounds".into(),
        ));
    }
    let mut seen = HashSet::with_capacity(trust.validators.len());
    let mut total = 0u64;
    let mut validators = Vec::with_capacity(trust.validators.len());
    for member in &trust.validators {
        if member.address == Hash256::ZERO || !seen.insert(member.address) {
            return Err(VerifyError::Trust(
                "validator address is zero or duplicated".into(),
            ));
        }
        total = total
            .checked_add(member.stake)
            .ok_or_else(|| VerifyError::Trust("validator stake total overflows u64".into()))?;
        // Finality verification authenticates only address/stake membership.
        // The library constructor requires a shard slot, which is irrelevant
        // to its finality set hash and vote checks, so use a neutral slot.
        let validator = Validator::new(member.address, member.stake, 0)
            .ok_or_else(|| VerifyError::Trust("validator stake is below minimum tier".into()))?;
        validators.push(validator);
    }
    if total != trust.total_stake {
        return Err(VerifyError::Trust(
            "trusted total_stake does not match members".into(),
        ));
    }
    let set = ValidatorSet::new(validators, trust.validator_set_epoch);
    if set.quorum != trust.quorum {
        return Err(VerifyError::Trust(
            "trusted quorum does not match derived quorum".into(),
        ));
    }
    let actual_set_hash = validator_set_hash(&set);
    if actual_set_hash != trust.validator_set_hash {
        return Err(VerifyError::Trust(
            "trusted validator_set_hash does not match members".into(),
        ));
    }
    let domain = ConsensusDomain::new(
        trust.domain_hash,
        trust.recovery_epoch,
        trust.validator_set_id,
    );
    Ok((domain, set))
}

fn constant_time_hex_eq(expected: &str, supplied: &str) -> bool {
    if supplied.len() != expected.len() {
        return false;
    }
    expected
        .bytes()
        .zip(supplied.bytes())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_consensus::view_change::{FinalityVote, validator_set_hash};
    use arc_crypto::{KeyPair, hash_bytes};
    use arc_types::{BlockHeader, ProtocolVersion};
    use serde_json::json;

    struct Fixture {
        trust: Vec<u8>,
        trust_sha: String,
        set: ValidatorSet,
        block: Block,
        finality: Vec<u8>,
    }

    impl Fixture {
        fn new() -> Self {
            let keys: Vec<_> = (0..4)
                .map(|index| {
                    KeyPair::from_ed25519_secret_bytes(
                        &hash_bytes(format!("offline-finality-test-{index}").as_bytes()).0,
                    )
                })
                .collect();
            let validators = keys
                .iter()
                .map(|key| Validator::new(key.address(), 500_000, 0).unwrap())
                .collect();
            let set = ValidatorSet::new(validators, 1);
            let domain = ConsensusDomain::new(hash_bytes(b"test-domain"), 1, 1);
            let block = Block::new(
                BlockHeader {
                    height: 7,
                    timestamp: 123,
                    parent_hash: hash_bytes(b"parent"),
                    tx_root: hash_bytes(b"tx-root"),
                    state_root: hash_bytes(b"state-root"),
                    proof_hash: Hash256::ZERO,
                    tx_count: 0,
                    producer: keys[0].address(),
                    protocol_version: ProtocolVersion::new(3, 0, 0),
                    state_diff: None,
                },
                vec![],
            );
            let set_hash = validator_set_hash(&set);
            let votes = keys
                .iter()
                .take(3)
                .map(|key| {
                    FinalityVote::sign(
                        domain,
                        set_hash,
                        7,
                        block.hash,
                        block.header.state_root,
                        block.header.tx_root,
                        key,
                    )
                    .unwrap()
                })
                .collect();
            let certificate = FinalityCertificate {
                domain,
                validator_set_hash: set_hash,
                height: 7,
                block_hash: block.hash,
                state_root: block.header.state_root,
                tx_root: block.header.tx_root,
                votes,
            };
            let certificate_hex = hex::encode(arc_bincode::serialize(&certificate).unwrap());
            let finality = serde_json::to_vec(&json!({
                "height": 7,
                "block_hash": block.hash,
                "state_root": block.header.state_root,
                "tx_root": block.header.tx_root,
                "validator_set_hash": set_hash,
                "voters": certificate.votes.iter().map(|vote| vote.voter).collect::<Vec<_>>(),
                "signing_stake": 1_500_000,
                "quorum": set.quorum,
                "total_stake": set.total_stake,
                "certificate_bincode_hex": certificate_hex,
                "verified_by_server": true,
                "note": "informational"
            }))
            .unwrap();
            let trust_value = json!({
                "schema_version": 1,
                "host": "lhr.example.invalid",
                "chain_genesis": hash_bytes(b"genesis"),
                "checkpoint_height": 6,
                "checkpoint_hash": hash_bytes(b"checkpoint"),
                "domain_hash": domain.domain_hash,
                "recovery_epoch": 1,
                "validator_set_id": 1,
                "validator_set_epoch": 1,
                "validator_set_hash": set_hash,
                "total_stake": set.total_stake,
                "quorum": set.quorum,
                "validators": keys.iter().map(|key| json!({
                    "address": key.address(), "stake": 500_000
                })).collect::<Vec<_>>()
            });
            let trust = serde_json::to_vec(&trust_value).unwrap();
            let trust_sha = format!("{:x}", Sha256::digest(&trust));
            Self {
                trust,
                trust_sha,
                set,
                block,
                finality,
            }
        }

        fn verify(&self, finality: &[u8], block: &[u8]) -> Result<VerifiedFinality, VerifyError> {
            verify_payloads(
                &self.trust,
                &self.trust_sha,
                "lhr.example.invalid",
                7,
                finality,
                block,
            )
        }

        fn block_json(&self) -> Vec<u8> {
            serde_json::to_vec(&self.block).unwrap()
        }
    }

    #[test]
    fn verifies_exact_rpc_payloads_against_pinned_trust() {
        let fixture = Fixture::new();
        let result = fixture
            .verify(&fixture.finality, &fixture.block_json())
            .unwrap();
        assert_eq!(result.height, 7);
        assert_eq!(result.host, "lhr.example.invalid");
        assert_eq!(result.signing_stake, 1_500_000);
        assert_eq!(result.quorum, fixture.set.quorum);

        // A server-side status bit is not proof either way; the decoded
        // certificate's signatures and quorum are the evidence.
        let mut response: serde_json::Value = serde_json::from_slice(&fixture.finality).unwrap();
        response["verified_by_server"] = json!(false);
        let server_claim_false = serde_json::to_vec(&response).unwrap();
        assert!(
            fixture
                .verify(&server_claim_false, &fixture.block_json())
                .is_ok()
        );
    }

    #[test]
    fn rejects_changed_trust_bytes_and_wrong_capture_host() {
        let fixture = Fixture::new();
        assert!(matches!(
            verify_payloads(
                &fixture.trust,
                &"00".repeat(32),
                "lhr.example.invalid",
                7,
                &fixture.finality,
                &fixture.block_json(),
            ),
            Err(VerifyError::TrustDigest)
        ));
        assert!(matches!(
            verify_payloads(
                &fixture.trust,
                &fixture.trust_sha,
                "ams.example.invalid",
                7,
                &fixture.finality,
                &fixture.block_json(),
            ),
            Err(VerifyError::Trust(_))
        ));
    }

    #[test]
    fn rejects_wrong_domain_and_committee_from_trust_manifest() {
        let fixture = Fixture::new();
        let mut wrong_domain: serde_json::Value = serde_json::from_slice(&fixture.trust).unwrap();
        wrong_domain["domain_hash"] = json!(hash_bytes(b"wrong-domain"));
        let wrong_domain = serde_json::to_vec(&wrong_domain).unwrap();
        let sha = format!("{:x}", Sha256::digest(&wrong_domain));
        assert!(matches!(
            verify_payloads(
                &wrong_domain,
                &sha,
                "lhr.example.invalid",
                7,
                &fixture.finality,
                &fixture.block_json()
            ),
            Err(VerifyError::CertificateVerify(_))
        ));

        let mut wrong_set: serde_json::Value = serde_json::from_slice(&fixture.trust).unwrap();
        wrong_set["validators"][0]["stake"] = json!(600_000);
        let wrong_set = serde_json::to_vec(&wrong_set).unwrap();
        let sha = format!("{:x}", Sha256::digest(&wrong_set));
        assert!(matches!(
            verify_payloads(
                &wrong_set,
                &sha,
                "lhr.example.invalid",
                7,
                &fixture.finality,
                &fixture.block_json()
            ),
            Err(VerifyError::Trust(_))
        ));
    }

    #[test]
    fn rejects_tampered_vote_bytes_and_non_exact_bincode() {
        let fixture = Fixture::new();
        let mut response: serde_json::Value = serde_json::from_slice(&fixture.finality).unwrap();
        let mut bytes = hex::decode(response["certificate_bincode_hex"].as_str().unwrap()).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x80;
        response["certificate_bincode_hex"] = json!(hex::encode(bytes));
        let tampered = serde_json::to_vec(&response).unwrap();
        assert!(fixture.verify(&tampered, &fixture.block_json()).is_err());

        let mut response: serde_json::Value = serde_json::from_slice(&fixture.finality).unwrap();
        let mut bytes = hex::decode(response["certificate_bincode_hex"].as_str().unwrap()).unwrap();
        bytes.push(0);
        response["certificate_bincode_hex"] = json!(hex::encode(bytes));
        let suffixed = serde_json::to_vec(&response).unwrap();
        assert!(matches!(
            fixture.verify(&suffixed, &fixture.block_json()),
            Err(VerifyError::CertificateDecode(_))
        ));
    }

    #[test]
    fn rejects_self_selected_height_and_projection_mismatches() {
        let fixture = Fixture::new();
        assert!(matches!(
            verify_payloads(
                &fixture.trust,
                &fixture.trust_sha,
                "lhr.example.invalid",
                8,
                &fixture.finality,
                &fixture.block_json()
            ),
            Err(VerifyError::Projection(_))
        ));
        let mut response: serde_json::Value = serde_json::from_slice(&fixture.finality).unwrap();
        response["state_root"] = json!(hash_bytes(b"wrong-root"));
        let mismatched = serde_json::to_vec(&response).unwrap();
        assert!(matches!(
            fixture.verify(&mismatched, &fixture.block_json()),
            Err(VerifyError::Projection(_))
        ));
        let mut bad_block = fixture.block.clone();
        bad_block.header.state_root = hash_bytes(b"different-state");
        let block_json = serde_json::to_vec(&bad_block).unwrap();
        assert!(matches!(
            fixture.verify(&fixture.finality, &block_json),
            Err(VerifyError::BlockHash)
        ));

        let mut bad_transactions = fixture.block.clone();
        bad_transactions
            .tx_hashes
            .push(hash_bytes(b"not-in-header-root"));
        let block_json = serde_json::to_vec(&bad_transactions).unwrap();
        assert!(matches!(
            fixture.verify(&fixture.finality, &block_json),
            Err(VerifyError::BlockTransactions)
        ));
    }

    #[test]
    fn rejects_duplicate_or_inconsistent_trusted_committee() {
        let fixture = Fixture::new();
        let mut duplicate: serde_json::Value = serde_json::from_slice(&fixture.trust).unwrap();
        duplicate["validators"][1]["address"] = duplicate["validators"][0]["address"].clone();
        let duplicate = serde_json::to_vec(&duplicate).unwrap();
        let sha = format!("{:x}", Sha256::digest(&duplicate));
        assert!(matches!(
            verify_payloads(
                &duplicate,
                &sha,
                "lhr.example.invalid",
                7,
                &fixture.finality,
                &fixture.block_json()
            ),
            Err(VerifyError::Trust(_))
        ));
    }

    #[test]
    fn rejects_a_valid_certificate_against_an_alternate_trusted_committee() {
        let fixture = Fixture::new();
        let mut alternate: serde_json::Value = serde_json::from_slice(&fixture.trust).unwrap();
        let members = alternate["validators"]
            .as_array()
            .unwrap()
            .iter()
            .take(3)
            .map(|member| serde_json::from_value::<TrustedValidator>(member.clone()).unwrap())
            .collect::<Vec<_>>();
        let validators = members
            .iter()
            .map(|member| Validator::new(member.address, member.stake, 0).unwrap())
            .collect();
        let alternate_set = ValidatorSet::new(validators, 1);
        alternate["validators"] = json!(
            members
                .iter()
                .map(|member| json!({"address": member.address, "stake": member.stake}))
                .collect::<Vec<_>>()
        );
        alternate["validator_set_hash"] = json!(validator_set_hash(&alternate_set));
        alternate["total_stake"] = json!(alternate_set.total_stake);
        alternate["quorum"] = json!(alternate_set.quorum);
        let alternate = serde_json::to_vec(&alternate).unwrap();
        let sha = format!("{:x}", Sha256::digest(&alternate));
        assert!(matches!(
            verify_payloads(
                &alternate,
                &sha,
                "lhr.example.invalid",
                7,
                &fixture.finality,
                &fixture.block_json(),
            ),
            Err(VerifyError::CertificateVerify(_))
        ));
    }
}
