use crate::protocol::*;
use arc_consensus::DagBlock;
use arc_crypto::{Hash256, KeyPair, Signature as CryptoSignature, hash_bytes};
use arc_types::Transaction;
use dashmap::DashMap;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

const MAX_PEERS: u32 = 256;

#[allow(dead_code)]
const TARGET_OUTBOUND: u32 = 16;

const PEER_MSG_RATE_LIMIT: u32 = 500;
const PEER_BYTE_RATE_LIMIT: u64 = 64 * 1024 * 1024;
const RATE_LIMIT_WINDOW_SECS: u64 = 1;

#[derive(Default)]
pub struct TransportWirePolicy {
    legacy_v3: bool,
    suppressed_messages: AtomicU64,
}

impl TransportWirePolicy {
    pub fn new(legacy_v3: bool) -> Self {
        Self {
            legacy_v3,
            suppressed_messages: AtomicU64::new(0),
        }
    }

    pub fn legacy_v3(&self) -> bool {
        self.legacy_v3
    }

    pub fn suppressed_messages(&self) -> u64 {
        self.suppressed_messages.load(Ordering::Relaxed)
    }

    fn permits(&self, message: MessageType) -> bool {
        if self.legacy_v3 && message as u8 > 0x12 {
            self.suppressed_messages.fetch_add(1, Ordering::Relaxed);
            debug!(
                ?message,
                "Suppressed outbound frame by explicit legacy-v3 wire policy"
            );
            false
        } else {
            true
        }
    }
}

#[derive(Debug)]
pub enum InboundMessage {
    PeerConnected {
        address: Hash256,
        stake: u64,
        connection_id: u64,
    },
    PeerDisconnected {
        address: Hash256,
        connection_id: u64,
    },
    DagBlockWithTxs {
        block: DagBlock,
        transactions: Vec<Transaction>,
    },
    Transactions(Vec<Vec<u8>>),
    StateDiff {
        source: Hash256,
        block_hash: Hash256,
        diff: arc_types::StateDiff,
        block_height: u64,
    },
    SnapshotManifestRequest {
        source: Hash256,
    },
    SnapshotChunkRequest {
        source: Hash256,
        manifest_hash: Hash256,
        chunk_index: u32,
    },
    SnapshotManifestResponse {
        source: Hash256,
        manifest: arc_state::SnapshotManifest,
    },
    SnapshotChunkResponse {
        source: Hash256,
        chunk: arc_state::StateSnapshot,
    },
    InferenceRequest {
        request_id: Hash256,
        input: String,
        max_tokens: u32,
        requester: Hash256,
    },
    InferenceResponse {
        request_id: Hash256,
        output: String,
        output_hash: Hash256,
        model_hash: Hash256,
        ms_per_token: u64,
        responder: Hash256,
    },
    HeartbeatWithRound {
        peer: Hash256,
        dag_round: u64,
        committed_round: u64,
    },
    ShardForward {
        request_id: Hash256,
        model_id: Hash256,
        next_layer: u32,
        total_layers: u32,
        token_position: u32,
        activations: Vec<u8>,
        activation_hash: Hash256,
    },
    ShardResult {
        request_id: Hash256,
        token_id: u32,
        logits_hash: Hash256,
        responder: Hash256,
    },
    ShardAnnounce {
        model_id: Hash256,
        start_layer: u32,
        end_layer: u32,
        expert_indices: Vec<u32>,
        node_address: Hash256,
        available_memory: u64,
        gpu_tier: u8,
    },
    RoundSyncRequest {
        peer: Hash256,
        their_round: u64,
        their_committed: u64,
    },
    RoundSyncResponse {
        current_round: u64,
        last_committed_round: u64,
    },
    ConsensusAbsenceVote {
        source: Hash256,
        vote: arc_consensus::view_change::SkipVote,
    },
    ConsensusAbsenceCertificate {
        source: Hash256,
        certificate: arc_consensus::view_change::SkipCertificate,
    },
    ConsensusFinalityVote {
        source: Hash256,
        vote: arc_consensus::view_change::FinalityVote,
    },
    ConsensusFinalityCertificate {
        source: Hash256,
        certificate: arc_consensus::view_change::FinalityCertificate,
    },
    DagHistoryRequest {
        source: Hash256,
        from_round: u64,
        max_rounds: u64,
    },
    DagHistoryResponse {
        source: Hash256,
        blocks: Vec<DagBlock>,
        transactions: Vec<Transaction>,
    },
    CheckpointRequest {
        source: Hash256,
        needed_below_height: u64,
    },
    CheckpointResponse {
        source: Hash256,
        envelope: Box<arc_consensus::view_change::CheckpointEnvelope>,
        payload: Vec<u8>,
    },
    NativeInferenceVote {
        source: Hash256,
        request_id: Hash256,
        tokens: Vec<u32>,
        vote: arc_types::inference_contract::InferenceVote,
    },
}

#[derive(Debug)]
pub enum OutboundMessage {
    BroadcastDagBlock {
        block: DagBlock,
        transactions: Vec<Transaction>,
    },
    BroadcastTransactions(Vec<Vec<u8>>),
    BroadcastAbsenceVote(arc_consensus::view_change::SkipVote),
    BroadcastAbsenceCertificate(arc_consensus::view_change::SkipCertificate),
    BroadcastFinalityVote(arc_consensus::view_change::FinalityVote),
    BroadcastFinalityCertificate(arc_consensus::view_change::FinalityCertificate),
    SendDagHistoryRequest {
        target: Hash256,
        from_round: u64,
        max_rounds: u64,
    },
    SendDagHistoryResponse {
        target: Hash256,
        blocks: Vec<DagBlock>,
        transactions: Vec<Transaction>,
        absence_certificates: Vec<arc_consensus::view_change::SkipCertificate>,
    },
    SendCheckpointRequest {
        target: Hash256,
        needed_below_height: u64,
    },
    SendCheckpointResponse {
        target: Hash256,
        envelope: Box<arc_consensus::view_change::CheckpointEnvelope>,
        payload: Vec<u8>,
    },
    BroadcastNativeInferenceVote {
        request_id: Hash256,
        tokens: Vec<u32>,
        vote: arc_types::inference_contract::InferenceVote,
    },
    BroadcastStateDiff {
        block_hash: Hash256,
        diff: arc_types::StateDiff,
        block_height: u64,
    },
    BroadcastInferenceRequest {
        request_id: Hash256,
        input: String,
        max_tokens: u32,
        requester: Hash256,
    },
    SendInferenceResponse {
        request_id: Hash256,
        output: String,
        output_hash: Hash256,
        model_hash: Hash256,
        ms_per_token: u64,
        responder: Hash256,
    },
    SendShardForward {
        target: Hash256,
        message: crate::protocol::ShardForwardMessage,
    },
    SendShardResult {
        target: Hash256,
        message: crate::protocol::ShardResultMessage,
    },
    BroadcastShardAnnounce {
        message: crate::protocol::ShardAnnounceMessage,
    },
    BroadcastHeartbeatWithRound {
        dag_round: u64,
        committed_round: u64,
    },
    SendRoundSyncRequest {
        target: Hash256,
        my_round: u64,
        my_committed: u64,
    },
    SendRoundSyncResponse {
        target: Hash256,
        current_round: u64,
        last_committed_round: u64,
        validator_count: u32,
        total_stake: u64,
    },
}

fn validator_tls_identity(
    keypair: &KeyPair,
) -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let KeyPair::Ed25519(signing_key) = keypair else {
        anyhow::bail!("validator QUIC identity requires an Ed25519 validator key")
    };

    let mut pkcs8 = Vec::with_capacity(48);
    pkcs8.extend_from_slice(&[
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ]);
    pkcs8.extend_from_slice(&signing_key.to_bytes());
    let private_der = PrivatePkcs8KeyDer::from(pkcs8);
    let certificate_key =
        rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&private_der, &rcgen::PKCS_ED25519).map_err(
            |error| anyhow::anyhow!("failed to import validator key for QUIC TLS: {error}"),
        )?;
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .map_err(|error| anyhow::anyhow!("failed to build validator certificate: {error}"))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        format!("arc-validator-{}", keypair.address().to_hex()),
    );
    let certificate = params
        .self_signed(&certificate_key)
        .map_err(|error| anyhow::anyhow!("failed to self-sign validator certificate: {error}"))?;
    let cert_der = CertificateDer::from(certificate.der().to_vec());
    let key_der = PrivatePkcs8KeyDer::from(certificate_key.serialize_der()).into();
    Ok((cert_der, key_der))
}

fn validator_address_from_certificate(
    certificate: &CertificateDer<'_>,
) -> Result<Hash256, rustls::Error> {
    const ED25519_SPKI_PREFIX: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let parsed = webpki::EndEntityCert::try_from(certificate)
        .map_err(|_| rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
    let spki = parsed.subject_public_key_info();
    let spki = spki.as_ref();
    if spki.len() != ED25519_SPKI_PREFIX.len() + 32 || !spki.starts_with(&ED25519_SPKI_PREFIX) {
        return Err(rustls::Error::InvalidCertificate(
            rustls::CertificateError::BadEncoding,
        ));
    }
    Ok(hash_bytes(&spki[ED25519_SPKI_PREFIX.len()..]))
}

fn peer_tls_validator_address(connection: &quinn::Connection) -> anyhow::Result<Hash256> {
    let identity = connection
        .peer_identity()
        .ok_or_else(|| anyhow::anyhow!("QUIC peer did not present a validator certificate"))?;
    let certificates = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| anyhow::anyhow!("QUIC peer identity is not a rustls certificate chain"))?;
    anyhow::ensure!(
        certificates.len() == 1,
        "validator QUIC peer must present exactly one leaf certificate"
    );
    validator_address_from_certificate(&certificates[0])
        .map_err(|error| anyhow::anyhow!("invalid validator QUIC certificate: {error}"))
}

#[derive(Debug)]
struct ValidatorCertificateVerifier {
    allowed_validators: Arc<HashSet<[u8; 32]>>,
    root_hints: Vec<rustls::DistinguishedName>,
}

impl ValidatorCertificateVerifier {
    fn new(allowed_validators: Arc<HashSet<[u8; 32]>>) -> Self {
        Self {
            allowed_validators,
            root_hints: Vec::new(),
        }
    }

    fn verify_membership(&self, certificate: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let address = validator_address_from_certificate(certificate)?;
        if !self.allowed_validators.contains(&address.0) {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ));
        }
        Ok(())
    }

    fn verify_tls12(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let provider = rustls::crypto::CryptoProvider::get_default()
            .expect("rustls provider is installed before validator TLS construction");
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let provider = rustls::crypto::CryptoProvider::get_default()
            .expect("rustls provider is installed before validator TLS construction");
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &provider.signature_verification_algorithms,
        )
    }
}

impl rustls::client::danger::ServerCertVerifier for ValidatorCertificateVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if !intermediates.is_empty() {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::BadEncoding,
            ));
        }
        self.verify_membership(end_entity)?;
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.verify_tls12(message, certificate, signature)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.verify_tls13(message, certificate, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ED25519]
    }
}

impl rustls::server::danger::ClientCertVerifier for ValidatorCertificateVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &self.root_hints
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        if !intermediates.is_empty() {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::BadEncoding,
            ));
        }
        self.verify_membership(end_entity)?;
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.verify_tls12(message, certificate, signature)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.verify_tls13(message, certificate, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ED25519]
    }
}

fn make_server_config(
    keypair: &KeyPair,
    allowed_validators: Arc<HashSet<[u8; 32]>>,
) -> anyhow::Result<quinn::ServerConfig> {
    let (cert_der, key_der) = validator_tls_identity(keypair)?;
    let client_verifier = Arc::new(ValidatorCertificateVerifier::new(allowed_validators));
    let server_crypto = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|error| {
            anyhow::anyhow!("failed to build validator rustls server config: {error}")
        })?;

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(server_crypto).expect("failed to create QUIC server config"),
    ));

    let transport = Arc::get_mut(&mut server_config.transport).unwrap();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(std::time::Duration::from_secs(300)).unwrap(),
    ));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    transport.stream_receive_window(quinn::VarInt::from_u32(64 * 1024 * 1024));
    transport.receive_window(quinn::VarInt::from_u32(256 * 1024 * 1024));

    Ok(server_config)
}

fn make_client_config(
    keypair: &KeyPair,
    allowed_validators: Arc<HashSet<[u8; 32]>>,
) -> anyhow::Result<quinn::ClientConfig> {
    let (cert_der, key_der) = validator_tls_identity(keypair)?;
    let server_verifier = Arc::new(ValidatorCertificateVerifier::new(allowed_validators));
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(server_verifier)
        .with_client_auth_cert(vec![cert_der], key_der)
        .map_err(|error| {
            anyhow::anyhow!("failed to build validator rustls client config: {error}")
        })?;

    let mut client_config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).expect("failed to create QUIC client config"),
    ));

    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(std::time::Duration::from_secs(300)).unwrap(),
    ));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    transport.stream_receive_window(quinn::VarInt::from_u32(64 * 1024 * 1024));
    transport.receive_window(quinn::VarInt::from_u32(256 * 1024 * 1024));
    client_config.transport_config(Arc::new(transport));

    Ok(client_config)
}

const PEER_AUTH_EXPORTER_LABEL: &[u8] = b"EXPORTER-ARC-peer-auth-v3";
const PEER_AUTH_EXPORTER_CONTEXT: &[u8] = b"arc-chain-validator-identity";

fn connection_binding(connection: &quinn::Connection) -> anyhow::Result<[u8; 32]> {
    let mut binding = [0u8; 32];
    connection
        .export_keying_material(
            &mut binding,
            PEER_AUTH_EXPORTER_LABEL,
            PEER_AUTH_EXPORTER_CONTEXT,
        )
        .map_err(|error| anyhow::anyhow!("could not derive QUIC session binding: {error:?}"))?;
    Ok(binding)
}

fn compute_handshake_challenge(message: &HandshakeMessage) -> Hash256 {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-peer-auth-v3");
    hasher.update(&message.nonce);
    hasher.update(message.validator_address.as_ref());
    hasher.update(&message.stake.to_le_bytes());
    hasher.update(&message.listen_port.to_le_bytes());
    hasher.update(message.genesis_hash.as_ref());
    hasher.update(&(message.public_key.len() as u64).to_le_bytes());
    hasher.update(&message.public_key);
    hasher.update(&message.protocol_version.to_le_bytes());
    hasher.update(&message.min_compatible_version.to_le_bytes());
    hasher.update(&message.dag_round.to_le_bytes());
    Hash256(*hasher.finalize().as_bytes())
}

fn make_signed_handshake(
    local_address: Hash256,
    local_stake: u64,
    listen_port: u16,
    genesis_hash: Hash256,
    keypair: &KeyPair,
    session_binding: [u8; 32],
) -> HandshakeMessage {
    let mut message = HandshakeMessage {
        validator_address: local_address,
        stake: local_stake,
        listen_port,
        genesis_hash,
        public_key: keypair.public_key_bytes(),
        nonce: session_binding,
        challenge_sig: Vec::new(),
        protocol_version: crate::protocol::PROTOCOL_VERSION,
        min_compatible_version: crate::protocol::MIN_COMPATIBLE_VERSION,
        dag_round: 0,
    };
    let challenge = compute_handshake_challenge(&message);
    let signature = keypair
        .sign(&challenge)
        .expect("signing session-bound handshake failed");
    message.challenge_sig =
        bincode::serialize(&signature).expect("peer-auth signature is serializable");
    message
}

fn verify_handshake(msg: &HandshakeMessage, expected_binding: &[u8; 32]) -> anyhow::Result<()> {
    if &msg.nonce != expected_binding {
        anyhow::bail!("peer identity proof belongs to a different QUIC session");
    }
    if msg.public_key.len() != 32 {
        anyhow::bail!("peer Ed25519 public key must be exactly 32 bytes");
    }
    if msg.challenge_sig.len() > 1024 {
        anyhow::bail!("peer identity signature exceeds the protocol bound");
    }

    let derived_address = hash_bytes(&msg.public_key);
    if derived_address != msg.validator_address {
        anyhow::bail!(
            "public key does not derive to claimed address: derived={}, claimed={}",
            derived_address,
            msg.validator_address
        );
    }

    let challenge = compute_handshake_challenge(msg);
    let sig: CryptoSignature = deserialize_message(&msg.challenge_sig)
        .map_err(|e| anyhow::anyhow!("failed to deserialize challenge signature: {e}"))?;
    let CryptoSignature::Ed25519 { public_key, .. } = &sig else {
        anyhow::bail!("peer identity proof must use Ed25519");
    };
    if public_key.as_slice() != msg.public_key.as_slice() {
        anyhow::bail!("peer identity signature key does not match handshake public key");
    }
    sig.verify(&challenge, &msg.validator_address)
        .map_err(|e| anyhow::anyhow!("challenge signature verification failed: {e}"))?;

    let peer_version = if msg.protocol_version == 0 {
        1
    } else {
        msg.protocol_version
    };
    let peer_min = if msg.min_compatible_version == 0 {
        1
    } else {
        msg.min_compatible_version
    };

    if !crate::protocol::protocol_ranges_overlap(
        crate::protocol::PROTOCOL_VERSION,
        crate::protocol::MIN_COMPATIBLE_VERSION,
        peer_version,
        peer_min,
    ) {
        if peer_min > crate::protocol::PROTOCOL_VERSION {
            anyhow::bail!(
                "peer requires protocol version >= {} but we are at {}",
                peer_min,
                crate::protocol::PROTOCOL_VERSION
            );
        }
        anyhow::bail!(
            "peer protocol version {} is below our minimum {}",
            peer_version,
            crate::protocol::MIN_COMPATIBLE_VERSION
        );
    }

    tracing::debug!(
        "Peer {} handshake OK: protocol v{}, dag_round={}",
        msg.validator_address,
        peer_version,
        msg.dag_round
    );

    Ok(())
}

fn payload_identity_is_authenticated(
    authenticated_peer: &Hash256,
    declared_identity: &Hash256,
) -> bool {
    authenticated_peer == declared_identity
}

#[derive(Clone, Copy)]
struct PeerRateWindow {
    message_count: u32,
    payload_bytes: u64,
    window_start_epoch_secs: u64,
}

struct PeerRateLimiter {
    counters: DashMap<Hash256, PeerRateWindow>,
}

impl PeerRateLimiter {
    fn new() -> Self {
        Self {
            counters: DashMap::new(),
        }
    }

    fn allow(&self, peer: &Hash256, payload_len: u32) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.allow_at(peer, payload_len, now)
    }

    fn allow_at(&self, peer: &Hash256, payload_len: u32, now: u64) -> bool {
        let payload_len = u64::from(payload_len);
        let mut entry = self.counters.entry(*peer).or_insert(PeerRateWindow {
            message_count: 0,
            payload_bytes: 0,
            window_start_epoch_secs: now,
        });
        let window = entry.value_mut();

        if now.saturating_sub(window.window_start_epoch_secs) >= RATE_LIMIT_WINDOW_SECS {
            *window = PeerRateWindow {
                message_count: 0,
                payload_bytes: 0,
                window_start_epoch_secs: now,
            };
        }

        if window.message_count >= PEER_MSG_RATE_LIMIT
            || payload_len > PEER_BYTE_RATE_LIMIT.saturating_sub(window.payload_bytes)
        {
            return false;
        }

        window.message_count += 1;
        window.payload_bytes += payload_len;
        true
    }

    fn remove_peer(&self, peer: &Hash256) {
        self.counters.remove(peer);
    }
}

struct PeerConnection<S> {
    connection_id: u64,
    send: SharedPeerStream<S>,
    dial_addr: SocketAddr,
    stake: u64,
    initiated_locally: bool,
    installed_at: std::time::Instant,
}

const SIMULTANEOUS_DIAL_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

const PEER_WRITE_BASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const PEER_WRITE_MIN_BYTES_PER_SEC: u64 = 1024 * 1024;

fn peer_write_timeout(base: std::time::Duration, payload_len: usize) -> std::time::Duration {
    let allowance_ms = (payload_len as u64).saturating_mul(1000) / PEER_WRITE_MIN_BYTES_PER_SEC;
    base.saturating_add(std::time::Duration::from_millis(allowance_ms))
}

struct PeerStream<S> {
    stream: S,
    torn: bool,
}

type SharedPeerStream<S> = Arc<tokio::sync::Mutex<PeerStream<S>>>;

fn shared_peer_stream<S>(stream: S) -> SharedPeerStream<S> {
    Arc::new(tokio::sync::Mutex::new(PeerStream {
        stream,
        torn: false,
    }))
}

#[derive(Debug)]
enum PeerWriteFailure {
    Io(std::io::Error),
    TimedOut,
    Torn,
}

impl std::fmt::Display for PeerWriteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::TimedOut => f.write_str("write timed out"),
            Self::Torn => f.write_str("stream holds a partial frame from an earlier write"),
        }
    }
}

#[derive(Debug)]
enum PeerWrite {
    Sent,
    NotConnected,
    Busy,
    Failed {
        connection_id: u64,
        failure: PeerWriteFailure,
    },
}

struct PeerConnections<S = quinn::SendStream> {
    peers: DashMap<[u8; 32], PeerConnection<S>>,
    next_connection_id: AtomicU64,
    inbound_tx: mpsc::Sender<InboundMessage>,
    peer_count: Arc<AtomicU32>,
    wire_policy: Arc<TransportWirePolicy>,
    write_base_timeout: std::time::Duration,
}

impl<S> PeerConnections<S> {
    fn new(
        inbound_tx: mpsc::Sender<InboundMessage>,
        peer_count: Arc<AtomicU32>,
        wire_policy: Arc<TransportWirePolicy>,
    ) -> Self {
        Self {
            peers: DashMap::new(),
            next_connection_id: AtomicU64::new(1),
            inbound_tx,
            peer_count,
            wire_policy,
            write_base_timeout: PEER_WRITE_BASE_TIMEOUT,
        }
    }

    fn install_directed(
        &self,
        key: [u8; 32],
        local_key: [u8; 32],
        send: S,
        dial_addr: SocketAddr,
        stake: u64,
        initiated_locally: bool,
    ) -> Option<(u64, Option<u64>)> {
        use dashmap::mapref::entry::Entry;
        let now = std::time::Instant::now();
        let preferred_initiator_is_local = local_key < key;
        match self.peers.entry(key) {
            Entry::Occupied(mut entry) => {
                let existing = entry.get();
                let simultaneous =
                    now.saturating_duration_since(existing.installed_at) < SIMULTANEOUS_DIAL_WINDOW;
                if simultaneous
                    && existing.initiated_locally == preferred_initiator_is_local
                    && initiated_locally != preferred_initiator_is_local
                {
                    return None;
                }
                let previous_id = existing.connection_id;
                let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
                entry.insert(PeerConnection {
                    connection_id,
                    send: shared_peer_stream(send),
                    dial_addr,
                    stake,
                    initiated_locally,
                    installed_at: now,
                });
                Some((connection_id, Some(previous_id)))
            }
            Entry::Vacant(entry) => {
                let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
                entry.insert(PeerConnection {
                    connection_id,
                    send: shared_peer_stream(send),
                    dial_addr,
                    stake,
                    initiated_locally,
                    installed_at: now,
                });
                self.peer_count.fetch_add(1, Ordering::Relaxed);
                Some((connection_id, None))
            }
        }
    }

    fn is_connected(&self, key: &[u8; 32]) -> bool {
        self.peers.contains_key(key)
    }

    fn remove_if_current(&self, key: &[u8; 32], connection_id: u64) -> bool {
        use dashmap::mapref::entry::Entry;

        match self.peers.entry(*key) {
            Entry::Occupied(entry) if entry.get().connection_id == connection_id => {
                entry.remove();
                self.peer_count.fetch_sub(1, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> PeerConnections<S> {
    async fn remove_and_notify(&self, address: Hash256, connection_id: u64) -> bool {
        if !self.remove_if_current(&address.0, connection_id) {
            return false;
        }
        let _ = self
            .inbound_tx
            .send(InboundMessage::PeerDisconnected {
                address,
                connection_id,
            })
            .await;
        true
    }

    async fn broadcast(&self, msg_type: MessageType, payload: &[u8]) {
        if !self.wire_policy.permits(msg_type) {
            return;
        }
        if payload.len() > MAX_PAYLOAD_SIZE as usize {
            error!(
                ?msg_type,
                payload_len = payload.len(),
                "Refusing oversized outbound P2P broadcast"
            );
            return;
        }
        let peer_keys: Vec<([u8; 32], u64)> = self
            .peers
            .iter()
            .map(|e| (*e.key(), e.connection_id))
            .collect();
        let peer_count = peer_keys.len();
        let mut sent = 0usize;
        let mut dead_peers = Vec::new();
        for (key, connection_id) in &peer_keys {
            match self
                .write_to_peer(key, Some(*connection_id), msg_type, payload)
                .await
            {
                PeerWrite::Sent => sent += 1,
                PeerWrite::NotConnected => {}
                PeerWrite::Busy => {
                    warn!(
                        ?msg_type,
                        "Peer stream busy with an earlier write; skipped this frame for it"
                    );
                }
                PeerWrite::Failed {
                    connection_id,
                    failure,
                } => {
                    warn!(?msg_type, %failure, "Failed to send to peer, removing connection");
                    dead_peers.push((*key, connection_id));
                }
            }
        }
        for (key, connection_id) in dead_peers {
            self.remove_and_notify(Hash256(key), connection_id).await;
        }
        if msg_type == MessageType::DagBlockWithTxs && peer_count > 0 {
            debug!(
                "Broadcast {:?}: sent to {}/{} peers ({} bytes)",
                msg_type,
                sent,
                peer_count,
                payload.len()
            );
        }
    }

    async fn send_to(&self, target: &Hash256, msg_type: MessageType, payload: &[u8]) {
        if !self.wire_policy.permits(msg_type) {
            return;
        }
        if payload.len() > MAX_PAYLOAD_SIZE as usize {
            error!(
                ?msg_type,
                payload_len = payload.len(),
                target = %target,
                "Refusing oversized outbound P2P message"
            );
            return;
        }
        match self.write_to_peer(&target.0, None, msg_type, payload).await {
            PeerWrite::Sent => {}
            PeerWrite::NotConnected => {
                debug!(
                    "send_to: peer {} not connected, cannot send {:?}",
                    target, msg_type
                );
            }
            PeerWrite::Busy => {
                warn!(
                    "send_to: peer {} stream busy with an earlier write, skipped {:?}",
                    target, msg_type
                );
            }
            PeerWrite::Failed {
                connection_id,
                failure,
            } => {
                warn!("Failed to send {:?} to {}: {}", msg_type, target, failure);
                self.remove_and_notify(*target, connection_id).await;
            }
        }
    }

    async fn write_to_peer(
        &self,
        key: &[u8; 32],
        expected_generation: Option<u64>,
        msg_type: MessageType,
        payload: &[u8],
    ) -> PeerWrite {
        let handle = self
            .peers
            .get(key)
            .map(|entry| (entry.connection_id, entry.send.clone()));
        let Some((connection_id, stream)) = handle else {
            return PeerWrite::NotConnected;
        };
        if expected_generation.is_some_and(|expected| expected != connection_id) {
            return PeerWrite::NotConnected;
        }
        let limit = peer_write_timeout(self.write_base_timeout, payload.len());
        let Ok(mut guard) = tokio::time::timeout(limit, stream.lock()).await else {
            return PeerWrite::Busy;
        };
        if guard.torn {
            return PeerWrite::Failed {
                connection_id,
                failure: PeerWriteFailure::Torn,
            };
        }
        guard.torn = true;
        let written =
            tokio::time::timeout(limit, write_message(&mut guard.stream, msg_type, payload)).await;
        let failure = match written {
            Ok(Ok(())) => {
                guard.torn = false;
                return PeerWrite::Sent;
            }
            Ok(Err(error)) => PeerWriteFailure::Io(error),
            Err(_) => PeerWriteFailure::TimedOut,
        };
        PeerWrite::Failed {
            connection_id,
            failure,
        }
    }

    async fn probe_peers(&self) -> usize {
        let peer_keys: Vec<([u8; 32], u64)> = self
            .peers
            .iter()
            .map(|e| (*e.key(), e.connection_id))
            .collect();
        let mut dead = Vec::new();
        for (key, connection_id) in &peer_keys {
            if let PeerWrite::Failed {
                connection_id,
                failure,
            } = self
                .write_to_peer(key, Some(*connection_id), MessageType::Heartbeat, &[])
                .await
            {
                debug!(connection_id, %failure, "Heartbeat probe failed");
                dead.push((*key, connection_id));
            }
        }
        let mut removed = 0;
        for (key, connection_id) in dead {
            if self.remove_and_notify(Hash256(key), connection_id).await {
                debug!(connection_id, "Pruned dead peer connection");
                removed += 1;
            }
        }
        removed
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_transport(
    listen_addr: SocketAddr,
    bootstrap_peers: Vec<SocketAddr>,
    local_address: Hash256,
    local_stake: u64,
    genesis_hash: Hash256,
    allowed_validators: Arc<HashSet<[u8; 32]>>,
    outbound_rx: mpsc::Receiver<OutboundMessage>,
    inbound_tx: mpsc::Sender<InboundMessage>,
    peer_count: Arc<AtomicU32>,
    local_keypair: KeyPair,
    data_dir: String,
) {
    run_transport_inner(
        listen_addr,
        bootstrap_peers,
        local_address,
        local_stake,
        genesis_hash,
        allowed_validators,
        outbound_rx,
        inbound_tx,
        peer_count,
        local_keypair,
        data_dir,
        None,
        None,
        Arc::new(TransportWirePolicy::default()),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub async fn run_transport_with_readiness(
    listen_addr: SocketAddr,
    bootstrap_peers: Vec<SocketAddr>,
    local_address: Hash256,
    local_stake: u64,
    genesis_hash: Hash256,
    allowed_validators: Arc<HashSet<[u8; 32]>>,
    outbound_rx: mpsc::Receiver<OutboundMessage>,
    inbound_tx: mpsc::Sender<InboundMessage>,
    peer_count: Arc<AtomicU32>,
    local_keypair: KeyPair,
    data_dir: String,
    startup: tokio::sync::oneshot::Sender<std::result::Result<SocketAddr, String>>,
) {
    run_transport_inner(
        listen_addr,
        bootstrap_peers,
        local_address,
        local_stake,
        genesis_hash,
        allowed_validators,
        outbound_rx,
        inbound_tx,
        peer_count,
        local_keypair,
        data_dir,
        Some(startup),
        None,
        Arc::new(TransportWirePolicy::default()),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
pub async fn run_transport_with_readiness_and_shutdown(
    listen_addr: SocketAddr,
    bootstrap_peers: Vec<SocketAddr>,
    local_address: Hash256,
    local_stake: u64,
    genesis_hash: Hash256,
    allowed_validators: Arc<HashSet<[u8; 32]>>,
    outbound_rx: mpsc::Receiver<OutboundMessage>,
    inbound_tx: mpsc::Sender<InboundMessage>,
    peer_count: Arc<AtomicU32>,
    local_keypair: KeyPair,
    data_dir: String,
    startup: tokio::sync::oneshot::Sender<std::result::Result<SocketAddr, String>>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    wire_policy: Arc<TransportWirePolicy>,
) {
    run_transport_inner(
        listen_addr,
        bootstrap_peers,
        local_address,
        local_stake,
        genesis_hash,
        allowed_validators,
        outbound_rx,
        inbound_tx,
        peer_count,
        local_keypair,
        data_dir,
        Some(startup),
        Some(shutdown),
        wire_policy,
    )
    .await;
}

fn report_transport_startup_failure(
    startup: &mut Option<tokio::sync::oneshot::Sender<std::result::Result<SocketAddr, String>>>,
    message: String,
) {
    if let Some(sender) = startup.take() {
        let _ = sender.send(Err(message));
    }
}

async fn wait_for_transport_shutdown(receiver: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    let Some(receiver) = receiver.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if *receiver.borrow_and_update() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

const TRANSPORT_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

async fn join_transport_task(
    mut task: tokio::task::JoinHandle<()>,
    grace: std::time::Duration,
) -> Result<(), tokio::task::JoinError> {
    match tokio::time::timeout(grace, &mut task).await {
        Ok(result) => result,
        Err(_) => {
            warn!("P2P task exceeded shutdown grace; aborting and joining");
            task.abort();
            task.await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_transport_inner(
    listen_addr: SocketAddr,
    bootstrap_peers: Vec<SocketAddr>,
    local_address: Hash256,
    local_stake: u64,
    genesis_hash: Hash256,
    allowed_validators: Arc<HashSet<[u8; 32]>>,
    mut outbound_rx: mpsc::Receiver<OutboundMessage>,
    inbound_tx: mpsc::Sender<InboundMessage>,
    peer_count: Arc<AtomicU32>,
    local_keypair: KeyPair,
    data_dir: String,
    mut startup: Option<tokio::sync::oneshot::Sender<std::result::Result<SocketAddr, String>>>,
    mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    wire_policy: Arc<TransportWirePolicy>,
) {
    if allowed_validators.is_empty() || !allowed_validators.contains(&local_address.0) {
        let message = format!(
            "P2P transport requires a non-empty fixed validator allowlist containing local identity {local_address}"
        );
        error!(
            local = %local_address,
            allowed = allowed_validators.len(),
            %message
        );
        report_transport_startup_failure(&mut startup, message);
        return;
    }
    if local_keypair.address() != local_address {
        let message = format!(
            "P2P transport local address {local_address} does not match validator key {}",
            local_keypair.address()
        );
        error!(
            declared = %local_address,
            key_address = %local_keypair.address(),
            %message
        );
        report_transport_startup_failure(&mut startup, message);
        return;
    }
    let _ = rustls::crypto::ring::default_provider().install_default();

    let server_config = match make_server_config(&local_keypair, allowed_validators.clone()) {
        Ok(config) => config,
        Err(error) => {
            let message =
                format!("failed to construct validator-authenticated QUIC server: {error}");
            error!(%message);
            report_transport_startup_failure(&mut startup, message);
            return;
        }
    };
    let configured_addr = listen_addr;
    let mut endpoint = {
        let mut bound: Option<quinn::Endpoint> = None;
        for attempt in 0..5 {
            match quinn::Endpoint::server(server_config.clone(), configured_addr) {
                Ok(ep) => {
                    bound = Some(ep);
                    break;
                }
                Err(e) => {
                    warn!(
                        "QUIC bind on {} attempt {} failed: {} - retrying in 2s",
                        configured_addr,
                        attempt + 1,
                        e
                    );
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        }
        if bound.is_none() {
            if local_stake > 0 {
                let message = format!(
                    "staked validator cannot bind configured QUIC address {configured_addr}; refusing an unreachable ephemeral identity"
                );
                error!(configured = %configured_addr, %message);
                report_transport_startup_failure(&mut startup, message);
                return;
            }
            let fallback = SocketAddr::new(configured_addr.ip(), 0);
            match quinn::Endpoint::server(server_config.clone(), fallback) {
                Ok(ep) => {
                    warn!(
                        "Configured UDP port {} unavailable after 5 attempts - \
                         bound to an OS-assigned ephemeral port instead. This node \
                         will participate normally as a consumer/observer but cannot \
                         accept inbound dials as a public seed. Common cause on \
                         Windows: Hyper-V's dynamic UDP exclusion range covers {}.",
                        configured_addr.port(),
                        configured_addr.port()
                    );
                    bound = Some(ep);
                }
                Err(e) => {
                    let message = format!(
                        "QUIC bind failed on configured {configured_addr} and ephemeral fallback: {e}"
                    );
                    error!(%message);
                    report_transport_startup_failure(&mut startup, message);
                    return;
                }
            }
        }
        bound.unwrap()
    };
    let client_config = match make_client_config(&local_keypair, allowed_validators.clone()) {
        Ok(config) => config,
        Err(error) => {
            let message =
                format!("failed to construct validator-authenticated QUIC client: {error}");
            error!(%message);
            report_transport_startup_failure(&mut startup, message);
            return;
        }
    };
    endpoint.set_default_client_config(client_config);

    let listen_addr = endpoint.local_addr().unwrap_or(configured_addr);
    if listen_addr.port() != configured_addr.port() {
        info!(
            "P2P transport listening on {} (configured was {})",
            listen_addr, configured_addr
        );
    } else {
        info!("P2P transport listening on {}", listen_addr);
    }
    if let Some(sender) = startup.take() {
        let _ = sender.send(Ok(listen_addr));
    }

    if wire_policy.legacy_v3() {
        warn!(
            "Legacy-v3 wire mode: outbound history, checkpoint, absence, finality, and native-vote frames are suppressed; authenticated live DAG gossip remains enabled"
        );
    }
    let connections = Arc::new(PeerConnections::new(
        inbound_tx.clone(),
        peer_count.clone(),
        wire_policy,
    ));
    let rate_limiter = Arc::new(PeerRateLimiter::new());
    let keypair = Arc::new(local_keypair);
    let local_identity = LocalPeerIdentity {
        address: local_address,
        advertised_stake: local_stake,
        listen_port: listen_addr.port(),
        genesis_hash,
    };
    let handshake_slots = Arc::new(tokio::sync::Semaphore::new(
        allowed_validators.len().saturating_mul(2).max(8),
    ));

    let (pex_dial_tx, mut pex_dial_rx) = mpsc::channel::<SocketAddr>(64);

    {
        let mut dial_handles = Vec::new();
        let allow_loopback_peers = std::env::var("ARC_ALLOW_LOOPBACK_PEERS").is_ok();
        for peer_addr in &bootstrap_peers {
            if peer_addr == &listen_addr {
                continue;
            }
            if !allow_loopback_peers {
                if peer_addr.ip().is_loopback() {
                    if peer_addr.port() == listen_addr.port() {
                        continue;
                    }
                } else if std::net::UdpSocket::bind(SocketAddr::new(peer_addr.ip(), 0)).is_ok() {
                    info!("Skipping self-dial to {} (local interface)", peer_addr);
                    continue;
                }
            } else if peer_addr.port() == listen_addr.port() {
                continue;
            }
            info!("Dialing bootstrap peer {}", peer_addr);
            let ep = endpoint.clone();
            let addr = *peer_addr;
            let ctx = PeerContext::new(
                local_identity,
                &keypair,
                &connections,
                &inbound_tx,
                &pex_dial_tx,
                &rate_limiter,
                &allowed_validators,
            );
            dial_handles.push(tokio::spawn(async move {
                for attempt in 1..=3u32 {
                    let timeout_secs = 5 * attempt as u64;
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(timeout_secs),
                        dial_peer(&ep, addr, &ctx),
                    )
                    .await
                    {
                        Ok(Ok(())) => {
                            info!("Connected to bootstrap peer {} (attempt {})", addr, attempt);
                            break;
                        }
                        Ok(Err(e)) => {
                            if attempt < 3 {
                                warn!(
                                    "Failed to connect to {} (attempt {}): {} - retrying",
                                    addr, attempt, e
                                );
                                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            } else {
                                warn!("Failed to connect to {} after 3 attempts: {}", addr, e);
                            }
                        }
                        Err(_) => {
                            if attempt < 3 {
                                warn!(
                                    "Timeout connecting to {} ({}s, attempt {}) - retrying",
                                    addr, timeout_secs, attempt
                                );
                            } else {
                                warn!(
                                    "Timeout connecting to {} after 3 attempts ({}s each)",
                                    addr, timeout_secs
                                );
                            }
                        }
                    }
                }
            }));
        }
        let dial_phase_peer_count = peer_count.clone();
        tokio::spawn(async move {
            for h in dial_handles {
                let _ = h.await;
            }
            info!(
                "Bootstrap dial phase complete, {} peers connected",
                dial_phase_peer_count.load(Ordering::Relaxed)
            );
        });
    }

    let persisted_peers = load_peers_from_disk(&data_dir);
    if !persisted_peers.is_empty() {
        info!(
            "Loading {} persisted peers from disk",
            persisted_peers.len()
        );
    }
    for peer_addr in &persisted_peers {
        if bootstrap_peers.contains(peer_addr) {
            continue;
        }
        let ctx = PeerContext::new(
            local_identity,
            &keypair,
            &connections,
            &inbound_tx,
            &pex_dial_tx,
            &rate_limiter,
            &allowed_validators,
        );
        let ep = endpoint.clone();
        let addr = *peer_addr;
        tokio::spawn(async move {
            match dial_peer(&ep, addr, &ctx).await {
                Ok(()) => info!("Connected to persisted peer {}", addr),
                Err(e) => debug!("Failed to connect to persisted peer {}: {}", addr, e),
            }
        });
    }

    let conn_out = connections.clone();
    let mut outbound_shutdown = shutdown.clone();
    let outbound_task = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                biased;
                _ = wait_for_transport_shutdown(&mut outbound_shutdown) => break,
                message = outbound_rx.recv() => message,
            };
            let Some(msg) = msg else {
                break;
            };
            match msg {
                OutboundMessage::BroadcastDagBlock {
                    block,
                    transactions,
                } => {
                    let payload = DagBlockWithTxsMessage {
                        block,
                        transactions,
                    };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .broadcast(MessageType::DagBlockWithTxs, &bytes)
                            .await;
                    }
                }
                OutboundMessage::BroadcastAbsenceVote(vote) => {
                    let payload = crate::protocol::ConsensusAbsenceVoteMessage { vote };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .broadcast(MessageType::ConsensusAbsenceVote, &bytes)
                            .await;
                    }
                }
                OutboundMessage::BroadcastAbsenceCertificate(certificate) => {
                    let payload =
                        crate::protocol::ConsensusAbsenceCertificateMessage { certificate };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .broadcast(MessageType::ConsensusAbsenceCertificate, &bytes)
                            .await;
                    }
                }
                OutboundMessage::BroadcastFinalityVote(vote) => {
                    let payload = crate::protocol::ConsensusFinalityVoteMessage { vote };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .broadcast(MessageType::ConsensusFinalityVote, &bytes)
                            .await;
                    }
                }
                OutboundMessage::BroadcastFinalityCertificate(certificate) => {
                    let payload =
                        crate::protocol::ConsensusFinalityCertificateMessage { certificate };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .broadcast(MessageType::ConsensusFinalityCertificate, &bytes)
                            .await;
                    }
                }
                OutboundMessage::SendDagHistoryRequest {
                    target,
                    from_round,
                    max_rounds,
                } => {
                    let payload = crate::protocol::DagHistoryRequestMessage {
                        from_round,
                        max_rounds,
                    };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .send_to(&target, MessageType::DagHistoryRequest, &bytes)
                            .await;
                    }
                }
                OutboundMessage::SendDagHistoryResponse {
                    target,
                    blocks,
                    transactions,
                    absence_certificates,
                } => {
                    for certificate in absence_certificates {
                        let payload =
                            crate::protocol::ConsensusAbsenceCertificateMessage { certificate };
                        if let Ok(bytes) = bincode::serialize(&payload) {
                            conn_out
                                .send_to(&target, MessageType::ConsensusAbsenceCertificate, &bytes)
                                .await;
                        }
                    }
                    let payload = crate::protocol::DagHistoryResponseMessage {
                        blocks,
                        transactions,
                    };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .send_to(&target, MessageType::DagHistoryResponse, &bytes)
                            .await;
                    }
                }
                OutboundMessage::BroadcastNativeInferenceVote {
                    request_id,
                    tokens,
                    vote,
                } => {
                    let payload = crate::protocol::NativeInferenceVoteMessage {
                        request_id,
                        tokens,
                        vote,
                    };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .broadcast(MessageType::NativeInferenceVote, &bytes)
                            .await;
                    }
                }
                OutboundMessage::SendCheckpointRequest {
                    target,
                    needed_below_height,
                } => {
                    let payload = crate::protocol::CheckpointRequestMessage {
                        needed_below_height,
                    };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .send_to(&target, MessageType::CheckpointRequest, &bytes)
                            .await;
                    }
                }
                OutboundMessage::SendCheckpointResponse {
                    target,
                    envelope,
                    payload,
                } => {
                    let message = crate::protocol::CheckpointResponseMessage {
                        envelope: *envelope,
                        payload,
                    };
                    if let Ok(bytes) = bincode::serialize(&message) {
                        conn_out
                            .send_to(&target, MessageType::CheckpointResponse, &bytes)
                            .await;
                    }
                }
                OutboundMessage::BroadcastTransactions(txs) => {
                    for batch in txs.chunks(crate::MAX_TX_PER_GOSSIP) {
                        let payload = crate::protocol::TxGossipMessage {
                            transactions: batch.to_vec(),
                        };
                        if let Ok(bytes) = bincode::serialize(&payload) {
                            conn_out.broadcast(MessageType::TxGossip, &bytes).await;
                        }
                    }
                }
                OutboundMessage::BroadcastStateDiff {
                    block_hash,
                    diff,
                    block_height,
                } => {
                    let payload = crate::protocol::StateDiffMessage {
                        block_hash,
                        diff,
                        block_height,
                    };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out.broadcast(MessageType::StateDiff, &bytes).await;
                    }
                }
                OutboundMessage::BroadcastInferenceRequest {
                    request_id,
                    input,
                    max_tokens,
                    requester,
                } => {
                    let payload = crate::protocol::InferenceRequestMessage {
                        request_id,
                        input,
                        max_tokens,
                        requester,
                    };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .broadcast(MessageType::InferenceRequest, &bytes)
                            .await;
                    }
                }
                OutboundMessage::SendInferenceResponse {
                    request_id,
                    output,
                    output_hash,
                    model_hash,
                    ms_per_token,
                    responder,
                } => {
                    let payload = crate::protocol::InferenceResponseMessage {
                        request_id,
                        output,
                        output_hash,
                        model_hash,
                        ms_per_token,
                        responder,
                    };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .broadcast(MessageType::InferenceResponse, &bytes)
                            .await;
                    }
                }
                OutboundMessage::SendShardForward { target, message } => {
                    if let Ok(bytes) = bincode::serialize(&message) {
                        conn_out
                            .send_to(&target, MessageType::ShardForward, &bytes)
                            .await;
                    }
                }
                OutboundMessage::SendShardResult { target, message } => {
                    if let Ok(bytes) = bincode::serialize(&message) {
                        conn_out
                            .send_to(&target, MessageType::ShardResult, &bytes)
                            .await;
                    }
                }
                OutboundMessage::BroadcastShardAnnounce { message } => {
                    if let Ok(bytes) = bincode::serialize(&message) {
                        conn_out.broadcast(MessageType::ShardAnnounce, &bytes).await;
                    }
                }
                OutboundMessage::BroadcastHeartbeatWithRound {
                    dag_round,
                    committed_round,
                } => {
                    let payload = crate::protocol::HeartbeatMessage {
                        dag_round,
                        committed_round,
                        protocol_version: crate::protocol::PROTOCOL_VERSION,
                    };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out.broadcast(MessageType::Heartbeat, &bytes).await;
                    }
                }
                OutboundMessage::SendRoundSyncRequest {
                    target,
                    my_round,
                    my_committed,
                } => {
                    let payload = crate::protocol::RoundSyncRequestMessage {
                        my_round,
                        my_committed,
                    };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .send_to(&target, MessageType::RoundSyncRequest, &bytes)
                            .await;
                    }
                }
                OutboundMessage::SendRoundSyncResponse {
                    target,
                    current_round,
                    last_committed_round,
                    validator_count,
                    total_stake,
                } => {
                    let payload = crate::protocol::RoundSyncResponseMessage {
                        current_round,
                        last_committed_round,
                        validator_count,
                        total_stake,
                    };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out
                            .send_to(&target, MessageType::RoundSyncResponse, &bytes)
                            .await;
                    }
                }
            }
        }
    });

    let mut pex_interval = tokio::time::interval(std::time::Duration::from_secs(60));
    pex_interval.tick().await;

    let mut reconnect_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    reconnect_interval.tick().await;

    let conn_pex = connections.clone();

    let pex_task = {
        let conn_bg = connections.clone();
        let bp = bootstrap_peers.clone();
        let ep = endpoint.clone();
        let kp = keypair.clone();
        let itx = inbound_tx.clone();
        let pdt = pex_dial_tx.clone();
        let rl = rate_limiter.clone();
        let av = allowed_validators.clone();
        let dd = data_dir.clone();
        let mut pex_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut pex_tick = tokio::time::interval(std::time::Duration::from_secs(60));
            pex_tick.tick().await;
            let mut recon_tick = tokio::time::interval(std::time::Duration::from_secs(30));
            recon_tick.tick().await;
            loop {
                tokio::select! {
                    biased;
                    _ = wait_for_transport_shutdown(&mut pex_shutdown) => break,
                    _ = pex_tick.tick() => {
                        let mut all_peers: Vec<crate::protocol::PexPeerInfo> = conn_bg
                            .peers.iter()
                            .map(|entry| crate::protocol::PexPeerInfo {
                                address: Hash256(*entry.key()),
                                socket_addr: entry.value().dial_addr.to_string(),
                                stake: entry.value().stake,
                            })
                            .collect();
                        use rand::seq::SliceRandom;
                        use rand::thread_rng;
                        all_peers.shuffle(&mut thread_rng());
                        all_peers.truncate(16);
                        if !all_peers.is_empty() {
                            let pex_msg = crate::protocol::PeerExchangeMessage { peers: all_peers };
                            if let Ok(bytes) = bincode::serialize(&pex_msg) {
                                debug!("Broadcasting PEX with {} peers", pex_msg.peers.len());
                                conn_bg.broadcast(MessageType::PeerExchange, &bytes).await;
                            }
                        }
                        save_peers_to_disk(&dd, &conn_bg);
                    }
                    _ = recon_tick.tick() => {
                        let mut candidates: Vec<SocketAddr> = bp.clone();
                        candidates.extend(load_peers_from_disk(&dd));
                        candidates.sort();
                        candidates.dedup();

                        conn_bg.probe_peers().await;

                        let connected_addrs: std::collections::HashSet<SocketAddr> = conn_bg.peers.iter()
                            .map(|e| e.value().dial_addr)
                            .collect();
                        let allow_lo = std::env::var("ARC_ALLOW_LOOPBACK_PEERS").is_ok();
                        let disconnected: Vec<SocketAddr> = candidates.into_iter()
                            .filter(|a| !connected_addrs.contains(a))
                            .filter(|a| {
                                if allow_lo {
                                    return a.port() != listen_addr.port();
                                }
                                if a.ip().is_loopback() {
                                    return a.port() != listen_addr.port();
                                }
                                let test_addr = SocketAddr::new(a.ip(), 0);
                                if std::net::UdpSocket::bind(test_addr).is_ok() { return false; }
                                true
                            })
                            .collect();
                        let reconnect_batch: Vec<_> = disconnected.into_iter().take(8).collect();
                        if !reconnect_batch.is_empty() {
                            info!("Reconnect: {} peers to retry", reconnect_batch.len());
                        }
                        for addr in reconnect_batch {
                            let ep2 = ep.clone();
                            let ctx = PeerContext::new(
                                local_identity,
                                &kp,
                                &conn_bg,
                                &itx,
                                &pdt,
                                &rl,
                                &av,
                            );
                            tokio::spawn(async move {
                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(10),
                                    dial_peer(&ep2, addr, &ctx),
                                ).await {
                                    Ok(Ok(())) => info!("Reconnect: connected to {}", addr),
                                    Ok(Err(e)) => debug!("Reconnect to {} failed: {}", addr, e),
                                    Err(_) => debug!("Reconnect to {} timed out", addr),
                                }
                            });
                        }
                    }
                }
            }
        })
    };

    let mut lifecycle_stop = false;
    loop {
        tokio::select! {
            biased;
            _ = wait_for_transport_shutdown(&mut shutdown) => {
                info!("P2P transport lifecycle shutdown requested");
                lifecycle_stop = true;
                break;
            }
            incoming_opt = endpoint.accept() => {
                let incoming = match incoming_opt {
                    Some(inc) => inc,
                    None => {
                        info!("QUIC endpoint closed");
                        break;
                    }
                };

                let conn = match incoming.await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("Failed to accept connection: {}", e);
                        continue;
                    }
                };

                let remote_addr = conn.remote_address();
                info!("Incoming connection from {}", remote_addr);

                let Ok(handshake_permit) = handshake_slots.clone().try_acquire_owned() else {
                    warn!("Validator handshake concurrency limit reached, rejecting {}", remote_addr);
                    continue;
                };

                let ctx = PeerContext::new(
                    local_identity,
                    &keypair,
                    &connections,
                    &inbound_tx,
                    &pex_dial_tx,
                    &rate_limiter,
                    &allowed_validators,
                );

                tokio::spawn(async move {
                    let _handshake_permit = handshake_permit;
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        accept_peer(conn, &ctx)
                    ).await;
                    if result.is_err() {
                        warn!("Handshake timeout from {}", remote_addr);
                    } else if let Ok(Err(e)) = result
                    {
                        warn!("Failed to accept peer from {}: {}", remote_addr, e);
                    }
                });
            }

            addr = pex_dial_rx.recv() => {
                if let Some(peer_addr) = addr {
                    let conn_pex_clone = conn_pex.clone();
                    let ep = endpoint.clone();
                    let ctx = PeerContext::new(
                        local_identity,
                        &keypair,
                        &connections,
                        &inbound_tx,
                        &pex_dial_tx,
                        &rate_limiter,
                        &allowed_validators,
                    );
                    tokio::spawn(async move {
                        let already = conn_pex_clone
                            .peers
                            .iter()
                            .any(|e| e.value().dial_addr == peer_addr);
                        if already { return; }
                        info!("PEX: dialing discovered peer {}", peer_addr);
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            dial_peer(&ep, peer_addr, &ctx),
                        ).await {
                            Ok(Ok(())) => info!("PEX: connected to {}", peer_addr),
                            Ok(Err(e)) => debug!("PEX: failed to dial {}: {}", peer_addr, e),
                            Err(_) => debug!("PEX: dial to {} timed out", peer_addr),
                        }
                    });
                }
            }
        }
    }

    endpoint.close(0_u32.into(), b"node lifecycle shutdown");
    if !lifecycle_stop {
        outbound_task.abort();
        pex_task.abort();
    }
    let (outbound_result, pex_result) = tokio::join!(
        join_transport_task(outbound_task, TRANSPORT_SHUTDOWN_GRACE),
        join_transport_task(pex_task, TRANSPORT_SHUTDOWN_GRACE),
    );
    if let Err(error) = outbound_result {
        warn!(%error, "P2P outbound task failed during shutdown");
    }
    if let Err(error) = pex_result {
        warn!(%error, "P2P peer-maintenance task failed during shutdown");
    }
    if tokio::time::timeout(TRANSPORT_SHUTDOWN_GRACE, endpoint.wait_idle())
        .await
        .is_err()
    {
        warn!("QUIC endpoint did not become idle within shutdown grace; continuing shutdown");
    }
    connections.peers.clear();
    peer_count.store(0, Ordering::SeqCst);
    info!("P2P transport stopped at the lifecycle barrier");
}

#[derive(Clone, Copy)]
struct LocalPeerIdentity {
    address: Hash256,
    advertised_stake: u64,
    listen_port: u16,
    genesis_hash: Hash256,
}

struct PeerContext {
    identity: LocalPeerIdentity,
    keypair: Arc<KeyPair>,
    connections: Arc<PeerConnections>,
    inbound_tx: mpsc::Sender<InboundMessage>,
    pex_dial_tx: mpsc::Sender<SocketAddr>,
    rate_limiter: Arc<PeerRateLimiter>,
    allowed_validators: Arc<HashSet<[u8; 32]>>,
}

impl PeerContext {
    fn new(
        identity: LocalPeerIdentity,
        keypair: &Arc<KeyPair>,
        connections: &Arc<PeerConnections>,
        inbound_tx: &mpsc::Sender<InboundMessage>,
        pex_dial_tx: &mpsc::Sender<SocketAddr>,
        rate_limiter: &Arc<PeerRateLimiter>,
        allowed_validators: &Arc<HashSet<[u8; 32]>>,
    ) -> Self {
        Self {
            identity,
            keypair: keypair.clone(),
            connections: connections.clone(),
            inbound_tx: inbound_tx.clone(),
            pex_dial_tx: pex_dial_tx.clone(),
            rate_limiter: rate_limiter.clone(),
            allowed_validators: allowed_validators.clone(),
        }
    }
}

async fn dial_peer(
    endpoint: &quinn::Endpoint,
    peer_addr: SocketAddr,
    ctx: &PeerContext,
) -> anyhow::Result<()> {
    let local_address = ctx.identity.address;
    let connections = &ctx.connections;
    let inbound_tx = &ctx.inbound_tx;

    let conn = endpoint.connect(peer_addr, "localhost")?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let session_binding = connection_binding(&conn)?;
    let local_handshake = make_signed_handshake(
        ctx.identity.address,
        ctx.identity.advertised_stake,
        ctx.identity.listen_port,
        ctx.identity.genesis_hash,
        &ctx.keypair,
        session_binding,
    );

    let payload = bincode::serialize(&local_handshake)?;
    write_message(&mut send, MessageType::Handshake, &payload).await?;

    let (msg_type, data) = read_message(&mut recv).await?;
    if msg_type != MessageType::HandshakeAck {
        anyhow::bail!("expected HandshakeAck, got {:?}", msg_type);
    }
    let remote: HandshakeMessage = deserialize_message(&data)?;

    if remote.genesis_hash != local_handshake.genesis_hash {
        anyhow::bail!(
            "genesis mismatch: local={} remote={}",
            local_handshake.genesis_hash,
            remote.genesis_hash
        );
    }

    verify_handshake(&remote, &session_binding)?;
    let tls_address = peer_tls_validator_address(&conn)?;
    if tls_address != remote.validator_address {
        anyhow::bail!(
            "peer TLS validator {} differs from exporter-bound handshake {}",
            tls_address,
            remote.validator_address
        );
    }
    if !ctx.allowed_validators.contains(&remote.validator_address.0) {
        anyhow::bail!(
            "authenticated peer {} is not in the fixed validator transport allowlist",
            remote.validator_address
        );
    }

    let dial_addr = SocketAddr::new(conn.remote_address().ip(), remote.listen_port);

    if remote.validator_address == local_address {
        debug!(
            "Rejected self-connection (dial) to {}",
            remote.validator_address
        );
        return Ok(());
    }

    info!(
        "Handshake verified with {} (stake: {}, dial: {})",
        remote.validator_address, remote.stake, dial_addr
    );

    let Some((connection_id, replaced_generation)) = connections.install_directed(
        remote.validator_address.0,
        local_address.0,
        send,
        dial_addr,
        remote.stake,
        true,
    ) else {
        info!(
            peer = %remote.validator_address,
            "Simultaneous dial: keeping the connection the lower address initiated; closing this one"
        );
        conn.close(0u32.into(), b"simultaneous dial duplicate");
        return Ok(());
    };
    if let Some(replaced_generation) = replaced_generation {
        info!(
            peer = %remote.validator_address,
            replaced_generation,
            connection_id,
            "Replaced authenticated validator connection generation (dial)"
        );
    }
    let _ = inbound_tx
        .send(InboundMessage::PeerConnected {
            address: remote.validator_address,
            stake: remote.stake,
            connection_id,
        })
        .await;

    let peer_addr_hash = remote.validator_address;
    let recv_context = PeerRecvContext::from_peer_context(ctx);
    let connections_ref = recv_context.connections.clone();
    let rate_limiter_clone = recv_context.rate_limiter.clone();
    tokio::spawn(async move {
        let _conn = conn;
        handle_peer_recv(recv, peer_addr_hash, recv_context).await;
        if connections_ref
            .remove_and_notify(peer_addr_hash, connection_id)
            .await
        {
            rate_limiter_clone.remove_peer(&peer_addr_hash);
        } else {
            debug!(
                peer = %peer_addr_hash,
                connection_id,
                "Ignored stale dial-connection cleanup"
            );
        }
    });

    Ok(())
}

async fn accept_peer(conn: quinn::Connection, ctx: &PeerContext) -> anyhow::Result<()> {
    let local_address = ctx.identity.address;
    let connections = &ctx.connections;
    let inbound_tx = &ctx.inbound_tx;

    let (mut send, mut recv) = conn.accept_bi().await?;
    let session_binding = connection_binding(&conn)?;
    let local_handshake = make_signed_handshake(
        ctx.identity.address,
        ctx.identity.advertised_stake,
        ctx.identity.listen_port,
        ctx.identity.genesis_hash,
        &ctx.keypair,
        session_binding,
    );

    let (msg_type, data) = read_message(&mut recv).await?;
    if msg_type != MessageType::Handshake {
        anyhow::bail!("expected Handshake, got {:?}", msg_type);
    }
    let remote: HandshakeMessage = deserialize_message(&data)?;

    if remote.genesis_hash != local_handshake.genesis_hash {
        anyhow::bail!("genesis mismatch");
    }

    verify_handshake(&remote, &session_binding)?;
    let tls_address = peer_tls_validator_address(&conn)?;
    if tls_address != remote.validator_address {
        anyhow::bail!(
            "peer TLS validator {} differs from exporter-bound handshake {}",
            tls_address,
            remote.validator_address
        );
    }
    if !ctx.allowed_validators.contains(&remote.validator_address.0) {
        anyhow::bail!(
            "authenticated peer {} is not in the fixed validator transport allowlist",
            remote.validator_address
        );
    }

    let payload = bincode::serialize(&local_handshake)?;
    write_message(&mut send, MessageType::HandshakeAck, &payload).await?;

    let dial_addr = SocketAddr::new(conn.remote_address().ip(), remote.listen_port);

    if remote.validator_address == local_address {
        debug!(
            "Rejected self-connection (accept) from {}",
            remote.validator_address
        );
        return Ok(());
    }

    info!(
        "Accepted verified peer {} (stake: {}, dial: {})",
        remote.validator_address, remote.stake, dial_addr
    );

    let Some((connection_id, replaced_generation)) = connections.install_directed(
        remote.validator_address.0,
        local_address.0,
        send,
        dial_addr,
        remote.stake,
        false,
    ) else {
        info!(
            peer = %remote.validator_address,
            "Simultaneous dial: keeping the connection the lower address initiated; closing this one"
        );
        conn.close(0u32.into(), b"simultaneous dial duplicate");
        return Ok(());
    };
    if let Some(replaced_generation) = replaced_generation {
        info!(
            peer = %remote.validator_address,
            replaced_generation,
            connection_id,
            "Replaced authenticated validator connection generation (accept)"
        );
    }
    let _ = inbound_tx
        .send(InboundMessage::PeerConnected {
            address: remote.validator_address,
            stake: remote.stake,
            connection_id,
        })
        .await;

    let peer_addr_hash = remote.validator_address;
    let recv_context = PeerRecvContext::from_peer_context(ctx);
    let connections_ref = recv_context.connections.clone();
    let rate_limiter_clone = recv_context.rate_limiter.clone();
    tokio::spawn(async move {
        let _conn = conn;
        handle_peer_recv(recv, peer_addr_hash, recv_context).await;
        if connections_ref
            .remove_and_notify(peer_addr_hash, connection_id)
            .await
        {
            rate_limiter_clone.remove_peer(&peer_addr_hash);
        } else {
            debug!(
                peer = %peer_addr_hash,
                connection_id,
                "Ignored stale accepted-connection cleanup"
            );
        }
    });

    Ok(())
}

struct PeerRecvContext {
    local_address: Hash256,
    inbound_tx: mpsc::Sender<InboundMessage>,
    pex_dial_tx: mpsc::Sender<SocketAddr>,
    connections: Arc<PeerConnections>,
    rate_limiter: Arc<PeerRateLimiter>,
    allowed_validators: Arc<HashSet<[u8; 32]>>,
}

impl PeerRecvContext {
    fn from_peer_context(context: &PeerContext) -> Self {
        Self {
            local_address: context.identity.address,
            inbound_tx: context.inbound_tx.clone(),
            pex_dial_tx: context.pex_dial_tx.clone(),
            connections: context.connections.clone(),
            rate_limiter: context.rate_limiter.clone(),
            allowed_validators: context.allowed_validators.clone(),
        }
    }
}

async fn handle_peer_recv(
    mut recv: quinn::RecvStream,
    peer_address: Hash256,
    context: PeerRecvContext,
) {
    let PeerRecvContext {
        local_address,
        inbound_tx,
        pex_dial_tx,
        connections,
        rate_limiter,
        allowed_validators,
    } = context;
    if !allowed_validators.contains(&peer_address.0) {
        warn!(
            peer = %peer_address,
            "Closing receive loop for identity outside fixed validator allowlist"
        );
        return;
    }
    loop {
        let (msg_type, payload_len) = match read_message_header(&mut recv).await {
            Ok(header) => header,
            Err(e) => {
                debug!("Peer {} stream closed: {}", peer_address, e);
                break;
            }
        };

        if !rate_limiter.allow(&peer_address, payload_len) {
            warn!(
                peer = %peer_address,
                payload_len,
                "Closing peer connection after message/byte rate limit"
            );
            break;
        }

        let data = match read_message_payload(&mut recv, payload_len).await {
            Ok(data) => data,
            Err(e) => {
                debug!(
                    "Peer {} stream closed while reading payload: {}",
                    peer_address, e
                );
                break;
            }
        };

        match msg_type {
            MessageType::DagBlockWithTxs => {
                match deserialize_message::<DagBlockWithTxsMessage>(&data) {
                    Ok(msg) => {
                        debug!(
                            "Received DAG block from {} round={}",
                            peer_address, msg.block.round
                        );
                        let _ = inbound_tx
                            .send(InboundMessage::DagBlockWithTxs {
                                block: msg.block,
                                transactions: msg.transactions,
                            })
                            .await;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to deserialize DagBlockWithTxs from {}: {}",
                            peer_address, e
                        );
                    }
                }
            }
            MessageType::TxGossip => {
                match deserialize_message::<crate::protocol::TxGossipMessage>(&data) {
                    Ok(msg) => {
                        if msg.transactions.len() > crate::MAX_TX_PER_GOSSIP {
                            warn!(
                                peer = %peer_address,
                                count = msg.transactions.len(),
                                max = crate::MAX_TX_PER_GOSSIP,
                                "Rejected oversized transaction gossip batch"
                            );
                            continue;
                        }
                        debug!(
                            "Received {} gossiped txs from {}",
                            msg.transactions.len(),
                            peer_address
                        );
                        let _ = inbound_tx
                            .send(InboundMessage::Transactions(msg.transactions))
                            .await;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to deserialize TxGossip from {}: {}",
                            peer_address, e
                        );
                    }
                }
            }
            MessageType::StateDiff => {
                match deserialize_message::<crate::protocol::StateDiffMessage>(&data) {
                    Ok(msg) => {
                        debug!(
                            "Received state diff for block {} from {}",
                            msg.block_hash, peer_address
                        );
                        let _ = inbound_tx
                            .send(InboundMessage::StateDiff {
                                source: peer_address,
                                block_hash: msg.block_hash,
                                diff: msg.diff,
                                block_height: msg.block_height,
                            })
                            .await;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to deserialize StateDiff from {}: {}",
                            peer_address, e
                        );
                    }
                }
            }
            MessageType::PeerExchange => {
                match deserialize_message::<crate::protocol::PeerExchangeMessage>(&data) {
                    Ok(msg) => {
                        if msg.peers.len() > 128 {
                            warn!(
                                "PEX from {} has {} peers (>128), truncating",
                                peer_address,
                                msg.peers.len()
                            );
                        }
                        debug!(
                            "Received PEX with {} peers from {}",
                            msg.peers.len(),
                            peer_address
                        );
                        for pex_peer in msg.peers.iter().take(16) {
                            if !allowed_validators.contains(&pex_peer.address.0) {
                                continue;
                            }
                            if pex_peer.address == local_address {
                                continue;
                            }
                            if connections.is_connected(&pex_peer.address.0) {
                                continue;
                            }
                            if pex_peer.socket_addr.is_empty() {
                                continue;
                            }
                            if connections.peers.len() as u32 >= MAX_PEERS {
                                debug!("PEX: at connection limit, skipping {}", pex_peer.address);
                                break;
                            }
                            if let Ok(addr) = pex_peer.socket_addr.parse::<SocketAddr>() {
                                debug!(
                                    "PEX: queueing discovered peer {} at {}",
                                    pex_peer.address, addr
                                );
                                let _ = pex_dial_tx.try_send(addr);
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Failed to deserialize PeerExchange from {}: {}",
                            peer_address, e
                        );
                    }
                }
            }
            MessageType::SnapshotManifestRequest => {
                match deserialize_message::<crate::protocol::SnapshotManifestRequestMessage>(&data)
                {
                    Ok(_msg) => {
                        debug!("Received snapshot manifest request from {}", peer_address);
                        let _ = inbound_tx
                            .send(InboundMessage::SnapshotManifestRequest {
                                source: peer_address,
                            })
                            .await;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to deserialize SnapshotManifestRequest from {}: {}",
                            peer_address, e
                        );
                    }
                }
            }
            MessageType::SnapshotManifestResponse => {
                match deserialize_message::<crate::protocol::SnapshotManifestResponseMessage>(&data)
                {
                    Ok(msg) => {
                        debug!(
                            "Received snapshot manifest from {} (height={}, chunks={})",
                            peer_address, msg.manifest.version, msg.manifest.total_chunks
                        );
                        let _ = inbound_tx
                            .send(InboundMessage::SnapshotManifestResponse {
                                source: peer_address,
                                manifest: msg.manifest,
                            })
                            .await;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to deserialize SnapshotManifestResponse from {}: {}",
                            peer_address, e
                        );
                    }
                }
            }
            MessageType::SnapshotChunkRequest => {
                match deserialize_message::<crate::protocol::SnapshotChunkRequestMessage>(&data) {
                    Ok(msg) => {
                        debug!(
                            "Received snapshot chunk request from {} (chunk={})",
                            peer_address, msg.chunk_index
                        );
                        let _ = inbound_tx
                            .send(InboundMessage::SnapshotChunkRequest {
                                source: peer_address,
                                manifest_hash: msg.manifest_hash,
                                chunk_index: msg.chunk_index,
                            })
                            .await;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to deserialize SnapshotChunkRequest from {}: {}",
                            peer_address, e
                        );
                    }
                }
            }
            MessageType::SnapshotChunkResponse => {
                match deserialize_message::<crate::protocol::SnapshotChunkResponseMessage>(&data) {
                    Ok(msg) => {
                        debug!(
                            "Received snapshot chunk from {} (index={}/{})",
                            peer_address, msg.chunk.chunk_index, msg.chunk.total_chunks
                        );
                        let _ = inbound_tx
                            .send(InboundMessage::SnapshotChunkResponse {
                                source: peer_address,
                                chunk: msg.chunk,
                            })
                            .await;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to deserialize SnapshotChunkResponse from {}: {}",
                            peer_address, e
                        );
                    }
                }
            }
            MessageType::InferenceRequest => {
                match deserialize_message::<crate::protocol::InferenceRequestMessage>(&data) {
                    Ok(msg) => {
                        if !payload_identity_is_authenticated(&peer_address, &msg.requester) {
                            warn!(
                                peer = %peer_address,
                                declared_requester = %msg.requester,
                                "Rejected inference request with spoofed requester identity"
                            );
                            continue;
                        }
                        info!(
                            "Inference request from {} ({})",
                            peer_address, msg.request_id
                        );
                        let _ = inbound_tx
                            .send(InboundMessage::InferenceRequest {
                                request_id: msg.request_id,
                                input: msg.input,
                                max_tokens: msg.max_tokens,
                                requester: msg.requester,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad InferenceRequest from {}: {}", peer_address, e),
                }
            }
            MessageType::InferenceResponse => {
                match deserialize_message::<crate::protocol::InferenceResponseMessage>(&data) {
                    Ok(msg) => {
                        if !payload_identity_is_authenticated(&peer_address, &msg.responder) {
                            warn!(
                                peer = %peer_address,
                                declared_responder = %msg.responder,
                                "Rejected inference response with spoofed responder identity"
                            );
                            continue;
                        }
                        info!(
                            "Inference response from {} for {}",
                            peer_address, msg.request_id
                        );
                        let _ = inbound_tx
                            .send(InboundMessage::InferenceResponse {
                                request_id: msg.request_id,
                                output: msg.output,
                                output_hash: msg.output_hash,
                                model_hash: msg.model_hash,
                                ms_per_token: msg.ms_per_token,
                                responder: msg.responder,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad InferenceResponse from {}: {}", peer_address, e),
                }
            }
            MessageType::Heartbeat => {
                if !data.is_empty()
                    && let Ok(hb) = deserialize_message::<crate::protocol::HeartbeatMessage>(&data)
                {
                    let _ = inbound_tx
                        .send(InboundMessage::HeartbeatWithRound {
                            peer: peer_address,
                            dag_round: hb.dag_round,
                            committed_round: hb.committed_round,
                        })
                        .await;
                }
            }
            MessageType::ShardForward => {
                match deserialize_message::<crate::protocol::ShardForwardMessage>(&data) {
                    Ok(msg) => {
                        let _ = inbound_tx
                            .send(InboundMessage::ShardForward {
                                request_id: msg.request_id,
                                model_id: msg.model_id,
                                next_layer: msg.next_layer,
                                total_layers: msg.total_layers,
                                token_position: msg.token_position,
                                activations: msg.activations,
                                activation_hash: msg.activation_hash,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad ShardForward from {}: {}", peer_address, e),
                }
            }
            MessageType::ShardResult => {
                match deserialize_message::<crate::protocol::ShardResultMessage>(&data) {
                    Ok(msg) => {
                        if !payload_identity_is_authenticated(&peer_address, &msg.responder) {
                            warn!(
                                peer = %peer_address,
                                declared_responder = %msg.responder,
                                "Rejected shard result with spoofed responder identity"
                            );
                            continue;
                        }
                        let _ = inbound_tx
                            .send(InboundMessage::ShardResult {
                                request_id: msg.request_id,
                                token_id: msg.token_id,
                                logits_hash: msg.logits_hash,
                                responder: msg.responder,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad ShardResult from {}: {}", peer_address, e),
                }
            }
            MessageType::ShardAnnounce => {
                match deserialize_message::<crate::protocol::ShardAnnounceMessage>(&data) {
                    Ok(msg) => {
                        if !payload_identity_is_authenticated(&peer_address, &msg.node_address) {
                            warn!(
                                peer = %peer_address,
                                declared_node = %msg.node_address,
                                "Rejected shard announcement with spoofed node identity"
                            );
                            continue;
                        }
                        let _ = inbound_tx
                            .send(InboundMessage::ShardAnnounce {
                                model_id: msg.model_id,
                                start_layer: msg.start_layer,
                                end_layer: msg.end_layer,
                                expert_indices: msg.expert_indices,
                                node_address: msg.node_address,
                                available_memory: msg.available_memory,
                                gpu_tier: msg.gpu_tier,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad ShardAnnounce from {}: {}", peer_address, e),
                }
            }
            MessageType::ConsensusAbsenceVote => {
                match deserialize_message::<crate::protocol::ConsensusAbsenceVoteMessage>(&data) {
                    Ok(msg) => {
                        let _ = inbound_tx
                            .send(InboundMessage::ConsensusAbsenceVote {
                                source: peer_address,
                                vote: msg.vote,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad ConsensusAbsenceVote from {}: {}", peer_address, e),
                }
            }
            MessageType::ConsensusAbsenceCertificate => {
                match deserialize_message::<crate::protocol::ConsensusAbsenceCertificateMessage>(
                    &data,
                ) {
                    Ok(msg) => {
                        let _ = inbound_tx
                            .send(InboundMessage::ConsensusAbsenceCertificate {
                                source: peer_address,
                                certificate: msg.certificate,
                            })
                            .await;
                    }
                    Err(e) => {
                        warn!(
                            "Bad ConsensusAbsenceCertificate from {}: {}",
                            peer_address, e
                        )
                    }
                }
            }
            MessageType::ConsensusFinalityVote => {
                match deserialize_message::<crate::protocol::ConsensusFinalityVoteMessage>(&data) {
                    Ok(msg) => {
                        let _ = inbound_tx
                            .send(InboundMessage::ConsensusFinalityVote {
                                source: peer_address,
                                vote: msg.vote,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad ConsensusFinalityVote from {}: {}", peer_address, e),
                }
            }
            MessageType::ConsensusFinalityCertificate => {
                match deserialize_message::<crate::protocol::ConsensusFinalityCertificateMessage>(
                    &data,
                ) {
                    Ok(msg) => {
                        let _ = inbound_tx
                            .send(InboundMessage::ConsensusFinalityCertificate {
                                source: peer_address,
                                certificate: msg.certificate,
                            })
                            .await;
                    }
                    Err(e) => {
                        warn!(
                            "Bad ConsensusFinalityCertificate from {}: {}",
                            peer_address, e
                        )
                    }
                }
            }
            MessageType::DagHistoryRequest => {
                match deserialize_message::<crate::protocol::DagHistoryRequestMessage>(&data) {
                    Ok(msg) => {
                        let _ = inbound_tx
                            .send(InboundMessage::DagHistoryRequest {
                                source: peer_address,
                                from_round: msg.from_round,
                                max_rounds: msg.max_rounds,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad DagHistoryRequest from {}: {}", peer_address, e),
                }
            }
            MessageType::DagHistoryResponse => {
                match deserialize_message::<crate::protocol::DagHistoryResponseMessage>(&data) {
                    Ok(msg) => {
                        let _ = inbound_tx
                            .send(InboundMessage::DagHistoryResponse {
                                source: peer_address,
                                blocks: msg.blocks,
                                transactions: msg.transactions,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad DagHistoryResponse from {}: {}", peer_address, e),
                }
            }
            MessageType::NativeInferenceVote => {
                match deserialize_message::<crate::protocol::NativeInferenceVoteMessage>(&data) {
                    Ok(msg) if msg.tokens.len() <= crate::protocol::MAX_NATIVE_VOTE_TOKENS => {
                        let _ = inbound_tx
                            .send(InboundMessage::NativeInferenceVote {
                                source: peer_address,
                                request_id: msg.request_id,
                                tokens: msg.tokens,
                                vote: msg.vote,
                            })
                            .await;
                    }
                    Ok(_) => warn!("Oversized NativeInferenceVote from {}", peer_address),
                    Err(e) => warn!("Bad NativeInferenceVote from {}: {}", peer_address, e),
                }
            }
            MessageType::CheckpointRequest => {
                match deserialize_message::<crate::protocol::CheckpointRequestMessage>(&data) {
                    Ok(msg) => {
                        let _ = inbound_tx
                            .send(InboundMessage::CheckpointRequest {
                                source: peer_address,
                                needed_below_height: msg.needed_below_height,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad CheckpointRequest from {}: {}", peer_address, e),
                }
            }
            MessageType::CheckpointResponse => {
                match deserialize_message::<crate::protocol::CheckpointResponseMessage>(&data) {
                    Ok(msg) => {
                        let _ = inbound_tx
                            .send(InboundMessage::CheckpointResponse {
                                source: peer_address,
                                envelope: Box::new(msg.envelope),
                                payload: msg.payload,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad CheckpointResponse from {}: {}", peer_address, e),
                }
            }
            MessageType::RoundSyncRequest => {
                match deserialize_message::<crate::protocol::RoundSyncRequestMessage>(&data) {
                    Ok(msg) => {
                        let _ = inbound_tx
                            .send(InboundMessage::RoundSyncRequest {
                                peer: peer_address,
                                their_round: msg.my_round,
                                their_committed: msg.my_committed,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad RoundSyncRequest from {}: {}", peer_address, e),
                }
            }
            MessageType::RoundSyncResponse => {
                match deserialize_message::<crate::protocol::RoundSyncResponseMessage>(&data) {
                    Ok(msg) => {
                        let _ = inbound_tx
                            .send(InboundMessage::RoundSyncResponse {
                                current_round: msg.current_round,
                                last_committed_round: msg.last_committed_round,
                            })
                            .await;
                    }
                    Err(e) => warn!("Bad RoundSyncResponse from {}: {}", peer_address, e),
                }
            }
            MessageType::Handshake | MessageType::HandshakeAck => {
                debug!(
                    "Unexpected handshake message from {} in data loop",
                    peer_address
                );
            }
        }
    }
}

fn save_peers_to_disk(data_dir: &str, connections: &PeerConnections) {
    let peers: Vec<String> = connections
        .peers
        .iter()
        .map(|entry| entry.value().dial_addr.to_string())
        .collect();
    if peers.is_empty() {
        return;
    }
    let path = std::path::Path::new(data_dir).join("known_peers.json");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(&peers) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                warn!("Failed to save peers to {}: {}", path.display(), e);
            } else {
                debug!("Saved {} peers to {}", peers.len(), path.display());
            }
        }
        Err(e) => warn!("Failed to serialize peer list: {}", e),
    }
}

fn load_peers_from_disk(data_dir: &str) -> Vec<SocketAddr> {
    let path = std::path::Path::new(data_dir).join("known_peers.json");
    match std::fs::read_to_string(&path) {
        Ok(json) => serde_json::from_str::<Vec<String>>(&json)
            .unwrap_or_default()
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_handshake(binding: [u8; 32]) -> HandshakeMessage {
        let keypair = KeyPair::generate_ed25519();
        make_signed_handshake(
            keypair.address(),
            500_000,
            7_331,
            Hash256([7_u8; 32]),
            &keypair,
            binding,
        )
    }

    #[test]
    fn handshake_identity_proof_is_bound_to_one_quic_session() {
        let binding = [11_u8; 32];
        let message = test_handshake(binding);

        verify_handshake(&message, &binding).expect("fresh session proof should verify");
        assert!(
            verify_handshake(&message, &[12_u8; 32]).is_err(),
            "a captured proof must not verify on another QUIC session"
        );
    }

    #[test]
    fn handshake_signature_authenticates_every_semantic_field() {
        let binding = [19_u8; 32];
        let original = test_handshake(binding);
        verify_handshake(&original, &binding).unwrap();

        let mut mutations: Vec<(&str, HandshakeMessage)> = Vec::new();

        let mut message = original.clone();
        message.validator_address.0[0] ^= 1;
        mutations.push(("validator address", message));

        let mut message = original.clone();
        message.stake += 1;
        mutations.push(("stake", message));

        let mut message = original.clone();
        message.listen_port += 1;
        mutations.push(("listen port", message));

        let mut message = original.clone();
        message.genesis_hash.0[0] ^= 1;
        mutations.push(("genesis hash", message));

        let mut message = original.clone();
        message.public_key[0] ^= 1;
        mutations.push(("public key", message));

        let mut message = original.clone();
        message.nonce[0] ^= 1;
        mutations.push(("session binding", message));

        let mut message = original.clone();
        message.challenge_sig[0] ^= 1;
        mutations.push(("identity signature", message));

        let mut message = original.clone();
        message.protocol_version += 1;
        mutations.push(("protocol version", message));

        let mut message = original.clone();
        message.min_compatible_version += 1;
        mutations.push(("minimum compatible version", message));

        let mut message = original.clone();
        message.dag_round += 1;
        mutations.push(("DAG round", message));

        for (field, message) in mutations {
            assert!(
                verify_handshake(&message, &binding).is_err(),
                "mutating {field} must invalidate the handshake"
            );
        }
    }

    #[test]
    fn session_bound_handshake_survives_wire_round_trip() {
        let binding = [23_u8; 32];
        let message = test_handshake(binding);
        let bytes = bincode::serialize(&message).unwrap();
        let decoded: HandshakeMessage = deserialize_message(&bytes).unwrap();

        verify_handshake(&decoded, &binding).expect("wire round trip should retain the proof");
    }

    #[tokio::test]
    async fn legacy_v3_wire_filters_extensions_without_closing_streams() {
        for legacy in [true, false] {
            let policy = Arc::new(TransportWirePolicy::new(legacy));
            let (inbound_tx, _inbound_rx) = mpsc::channel(1);
            let peer_count = Arc::new(AtomicU32::new(0));
            let connections = PeerConnections::new(inbound_tx, peer_count.clone(), policy.clone());
            let peer = Hash256([8; 32]);
            let (send, mut receive) = tokio::io::duplex(4096);
            let (generation, _) = connections
                .install_directed(
                    peer.0,
                    [7; 32],
                    send,
                    "127.0.0.1:9945".parse().unwrap(),
                    1,
                    true,
                )
                .unwrap();
            let mut expected = Vec::new();
            let mut suppressed = 0;
            for byte in 1..=u8::MAX {
                let Some(kind) = MessageType::from_u8(byte) else {
                    continue;
                };
                connections.broadcast(kind, &[byte, 1]).await;
                connections.send_to(&peer, kind, &[byte, 2]).await;
                if legacy && byte > 0x12 {
                    suppressed += 2;
                } else {
                    expected.push((kind, vec![byte, 1]));
                    expected.push((kind, vec![byte, 2]));
                }
            }
            connections
                .send_to(&peer, MessageType::Heartbeat, b"still-connected")
                .await;
            expected.push((MessageType::Heartbeat, b"still-connected".to_vec()));
            assert_eq!(peer_count.load(Ordering::Relaxed), 1);
            assert_eq!(policy.suppressed_messages(), suppressed);
            assert!(connections.remove_if_current(&peer.0, generation));
            for (kind, payload) in expected {
                let (received_kind, received_payload) = read_message(&mut receive).await.unwrap();
                assert_eq!(received_kind, kind);
                assert_eq!(received_payload, payload);
                if legacy {
                    assert!(
                        (received_kind as u8) <= 0x12,
                        "legacy parser would disconnect"
                    );
                }
            }
            assert_eq!(
                read_message(&mut receive).await.unwrap_err().kind(),
                std::io::ErrorKind::UnexpectedEof
            );
        }
    }

    #[tokio::test]
    async fn both_quic_endpoints_derive_the_same_exporter_binding() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server_key = KeyPair::generate_ed25519();
        let client_key = KeyPair::generate_ed25519();
        let allowed: Arc<HashSet<[u8; 32]>> = Arc::new(
            [server_key.address().0, client_key.address().0]
                .into_iter()
                .collect(),
        );
        let server = quinn::Endpoint::server(
            make_server_config(&server_key, allowed.clone()).unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let server_addr = server.local_addr().unwrap();

        let accept = tokio::spawn(async move {
            let incoming = server.accept().await.expect("server endpoint is open");
            let connection = incoming.await.expect("server QUIC handshake succeeds");
            let peer = peer_tls_validator_address(&connection)
                .expect("server sees the pinned client validator certificate");
            (
                connection_binding(&connection).expect("server exporter is available"),
                peer,
            )
        });

        let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(make_client_config(&client_key, allowed.clone()).unwrap());
        let connection = client
            .connect(server_addr, "localhost")
            .unwrap()
            .await
            .expect("client QUIC handshake succeeds");
        let client_binding = connection_binding(&connection).expect("client exporter is available");
        assert_eq!(
            peer_tls_validator_address(&connection).unwrap(),
            server_key.address()
        );
        let (server_binding, server_observed_client) = accept.await.unwrap();

        assert_eq!(client_binding, server_binding);
        assert_ne!(client_binding, [0_u8; 32]);
        assert_eq!(server_observed_client, client_key.address());
    }

    #[tokio::test]
    async fn readiness_reports_fail_closed_validator_configuration() {
        let keypair = KeyPair::generate_ed25519();
        let (outbound_tx, outbound_rx) = mpsc::channel(1);
        let (inbound_tx, _inbound_rx) = mpsc::channel(1);
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        drop(outbound_tx);

        run_transport_with_readiness(
            "127.0.0.1:0".parse().unwrap(),
            Vec::new(),
            keypair.address(),
            5_000_000,
            Hash256([41_u8; 32]),
            Arc::new(HashSet::new()),
            outbound_rx,
            inbound_tx,
            Arc::new(AtomicU32::new(0)),
            keypair,
            std::env::temp_dir().to_string_lossy().into_owned(),
            startup_tx,
        )
        .await;

        let error = startup_rx
            .await
            .expect("transport must report its startup decision")
            .unwrap_err();
        assert!(error.contains("non-empty fixed validator allowlist"));
    }

    #[tokio::test]
    async fn readiness_is_sent_only_after_authenticated_endpoint_binds() {
        let keypair = KeyPair::generate_ed25519();
        let address = keypair.address();
        let (outbound_tx, outbound_rx) = mpsc::channel(1);
        let (inbound_tx, _inbound_rx) = mpsc::channel(1);
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_transport_with_readiness(
            "127.0.0.1:0".parse().unwrap(),
            Vec::new(),
            address,
            5_000_000,
            Hash256([42_u8; 32]),
            Arc::new([address.0].into_iter().collect()),
            outbound_rx,
            inbound_tx,
            Arc::new(AtomicU32::new(0)),
            keypair,
            std::env::temp_dir().to_string_lossy().into_owned(),
            startup_tx,
        ));

        let bound = tokio::time::timeout(std::time::Duration::from_secs(2), startup_rx)
            .await
            .expect("authenticated endpoint startup timed out")
            .expect("transport dropped its startup result")
            .expect("authenticated endpoint must bind");
        assert!(bound.ip().is_loopback());
        assert_ne!(bound.port(), 0);

        drop(outbound_tx);
        task.abort();
    }

    #[tokio::test]
    async fn transport_shutdown_aborts_and_joins_stalled_task() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, mut dropped_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _drop_marker = dropped_tx;
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            join_transport_task(task, std::time::Duration::from_millis(10)),
        )
        .await
        .expect("stalled transport task exceeded the shutdown bound");
        assert!(result.unwrap_err().is_cancelled());
        assert_eq!(
            dropped_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed),
            "task state must be dropped before the join returns"
        );
    }

    #[tokio::test]
    async fn transport_shutdown_allows_task_to_finish_during_grace() {
        let task = tokio::spawn(async {
            tokio::task::yield_now().await;
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            join_transport_task(task, std::time::Duration::from_secs(1)),
        )
        .await
        .expect("cooperative transport task exceeded the shutdown bound")
        .expect("cooperative transport task was aborted");
    }

    #[tokio::test]
    async fn lifecycle_shutdown_joins_bound_transport() {
        let keypair = KeyPair::generate_ed25519();
        let address = keypair.address();
        let (_outbound_tx, outbound_rx) = mpsc::channel(1);
        let (inbound_tx, _inbound_rx) = mpsc::channel(1);
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let peer_count = Arc::new(AtomicU32::new(0));
        let task = tokio::spawn(run_transport_with_readiness_and_shutdown(
            "127.0.0.1:0".parse().unwrap(),
            Vec::new(),
            address,
            5_000_000,
            Hash256([43_u8; 32]),
            Arc::new([address.0].into_iter().collect()),
            outbound_rx,
            inbound_tx,
            peer_count.clone(),
            keypair,
            std::env::temp_dir().to_string_lossy().into_owned(),
            startup_tx,
            shutdown_rx,
            Arc::new(TransportWirePolicy::default()),
        ));

        tokio::time::timeout(std::time::Duration::from_secs(2), startup_rx)
            .await
            .expect("authenticated endpoint startup timed out")
            .expect("transport dropped its startup result")
            .expect("authenticated endpoint must bind");
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("transport did not join after lifecycle shutdown")
            .expect("transport task panicked");
        assert_eq!(peer_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn validator_certificate_is_the_arc_validator_key() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let keypair = KeyPair::generate_ed25519();
        let (certificate, _) = validator_tls_identity(&keypair).unwrap();
        let (certificate_again, _) = validator_tls_identity(&keypair).unwrap();
        assert_eq!(
            certificate.as_ref(),
            certificate_again.as_ref(),
            "a validator key must produce one stable pinned certificate"
        );
        assert_eq!(
            validator_address_from_certificate(&certificate).unwrap(),
            keypair.address()
        );
        assert!(validator_address_from_certificate(&CertificateDer::from(vec![1, 2, 3])).is_err());
    }

    #[test]
    fn certificate_membership_rejects_a_valid_unknown_validator() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let allowed = KeyPair::generate_ed25519();
        let unknown = KeyPair::generate_ed25519();
        let (unknown_certificate, _) = validator_tls_identity(&unknown).unwrap();
        let verifier = ValidatorCertificateVerifier::new(Arc::new(
            [allowed.address().0].into_iter().collect(),
        ));
        assert!(verifier.verify_membership(&unknown_certificate).is_err());
    }

    #[test]
    fn declared_payload_identity_must_match_authenticated_peer() {
        let authenticated = Hash256([31_u8; 32]);
        let spoofed = Hash256([32_u8; 32]);

        assert!(payload_identity_is_authenticated(
            &authenticated,
            &authenticated
        ));
        assert!(!payload_identity_is_authenticated(&authenticated, &spoofed));
    }

    #[test]
    fn rate_limiter_enforces_message_budget_at_frame_header() {
        let limiter = PeerRateLimiter::new();
        let peer = Hash256([41_u8; 32]);
        let now = 1_000;

        for _ in 0..PEER_MSG_RATE_LIMIT {
            assert!(limiter.allow_at(&peer, 1, now));
        }
        assert!(!limiter.allow_at(&peer, 1, now));
        assert!(limiter.allow_at(&peer, 1, now + RATE_LIMIT_WINDOW_SECS));
    }

    #[test]
    fn rate_limiter_enforces_byte_budget_at_frame_header() {
        let limiter = PeerRateLimiter::new();
        let peer = Hash256([42_u8; 32]);
        let now = 2_000;

        for _ in 0..4 {
            assert!(limiter.allow_at(&peer, MAX_PAYLOAD_SIZE, now));
        }
        assert!(!limiter.allow_at(&peer, 1, now));
        assert!(limiter.allow_at(&peer, MAX_PAYLOAD_SIZE, now + RATE_LIMIT_WINDOW_SECS));
    }

    fn test_connections<S>() -> (PeerConnections<S>, Arc<AtomicU32>) {
        let (inbound_tx, _inbound_rx) = mpsc::channel(8);
        let peer_count = Arc::new(AtomicU32::new(0));
        (
            PeerConnections::new(
                inbound_tx,
                peer_count.clone(),
                Arc::new(TransportWirePolicy::default()),
            ),
            peer_count,
        )
    }

    #[test]
    fn authenticated_reconnect_replaces_only_the_same_validator_slot() {
        let (connections, peer_count) = test_connections();
        let peer = [3_u8; 32];
        let dial_addr: SocketAddr = "127.0.0.1:7331".parse().unwrap();

        let (first_id, replaced) = connections
            .install_directed(peer, LOCAL, (), dial_addr, 500_000, true)
            .unwrap();
        assert_eq!(replaced, None);
        let (replacement_id, replaced) = connections
            .install_directed(peer, LOCAL, (), dial_addr, 500_000, true)
            .unwrap();
        assert_eq!(replaced, Some(first_id));
        assert!(replacement_id > first_id);
        assert_eq!(connections.peers.len(), 1);
        assert_eq!(peer_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            connections.peers.get(&peer).unwrap().connection_id,
            replacement_id
        );
        assert!(!connections.remove_if_current(&peer, first_id));
        assert!(connections.remove_if_current(&peer, replacement_id));
        assert_eq!(peer_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn stale_disconnect_cannot_remove_replacement_generation() {
        let (connections, peer_count) = test_connections();
        let peer = [9_u8; 32];
        let dial_addr: SocketAddr = "127.0.0.1:7332".parse().unwrap();

        let (old_id, replaced) = connections
            .install_directed(peer, LOCAL, (), dial_addr, 500_000, true)
            .unwrap();
        assert_eq!(replaced, None);
        assert!(connections.remove_if_current(&peer, old_id));
        let (replacement_id, replaced) = connections
            .install_directed(peer, LOCAL, (), dial_addr, 500_000, true)
            .unwrap();
        assert_eq!(replaced, None);
        assert!(replacement_id > old_id);

        assert!(!connections.remove_if_current(&peer, old_id));
        assert_eq!(
            connections.peers.get(&peer).unwrap().connection_id,
            replacement_id
        );
        assert_eq!(peer_count.load(Ordering::Relaxed), 1);

        assert!(connections.remove_if_current(&peer, replacement_id));
        assert_eq!(peer_count.load(Ordering::Relaxed), 0);
    }

    fn installed_stream<S: Copy>(connections: &PeerConnections<S>, key: &[u8; 32]) -> S {
        let stream = connections.peers.get(key).unwrap().send.clone();
        stream.try_lock().expect("no write in flight").stream
    }

    const LOCAL: [u8; 32] = [1_u8; 32];

    fn simultaneous_dial(
        a_accepts_first: bool,
        b_accepts_first: bool,
    ) -> (&'static str, &'static str) {
        let a = [1_u8; 32];
        let b = [2_u8; 32];
        let addr: SocketAddr = "127.0.0.1:7333".parse().unwrap();
        let (a_view, _) = test_connections::<&'static str>();
        let (b_view, _) = test_connections::<&'static str>();
        let a_steps = [("A->B", true), ("B->A", false)];
        let b_steps = [("B->A", true), ("A->B", false)];
        for (view, local, remote, steps, accepts_first) in [
            (&a_view, a, b, a_steps, a_accepts_first),
            (&b_view, b, a, b_steps, b_accepts_first),
        ] {
            let mut order = steps.to_vec();
            if accepts_first {
                order.reverse();
            }
            for (label, dialed) in order {
                let _ = view.install_directed(remote, local, label, addr, 500_000, dialed);
            }
        }
        let kept = |view: &PeerConnections<&'static str>, remote: [u8; 32]| {
            assert_eq!(view.peers.len(), 1, "exactly one connection per peer");
            installed_stream(view, &remote)
        };
        (kept(&a_view, b), kept(&b_view, a))
    }

    #[test]
    fn a_simultaneous_dial_converges_on_one_connection_in_every_completion_order() {
        for a_accepts_first in [false, true] {
            for b_accepts_first in [false, true] {
                let (a_kept, b_kept) = simultaneous_dial(a_accepts_first, b_accepts_first);
                assert_eq!(
                    (a_kept, b_kept),
                    ("A->B", "A->B"),
                    "order a_accepts_first={a_accepts_first} b_accepts_first={b_accepts_first}"
                );
            }
        }
    }

    #[test]
    fn a_losing_duplicate_neither_bumps_the_peer_count_nor_replaces_the_winner() {
        let (connections, peer_count) = test_connections::<&'static str>();
        let local = [1_u8; 32];
        let remote = [2_u8; 32];
        let addr: SocketAddr = "127.0.0.1:7334".parse().unwrap();
        let (kept_id, _) = connections
            .install_directed(remote, local, "A->B", addr, 500_000, true)
            .unwrap();
        assert!(
            connections
                .install_directed(remote, local, "B->A", addr, 500_000, false)
                .is_none()
        );
        assert_eq!(peer_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            connections.peers.get(&remote).unwrap().connection_id,
            kept_id
        );
        assert_eq!(installed_stream(&connections, &remote), "A->B");
    }

    #[test]
    fn outside_the_window_a_reconnect_replaces_the_old_connection_in_either_direction() {
        let (connections, peer_count) = test_connections::<&'static str>();
        let local = [1_u8; 32];
        let remote = [2_u8; 32];
        let addr: SocketAddr = "127.0.0.1:7335".parse().unwrap();
        let (old_id, _) = connections
            .install_directed(remote, local, "A->B (stale)", addr, 500_000, true)
            .unwrap();
        {
            let mut entry = connections.peers.get_mut(&remote).unwrap();
            entry.installed_at = entry
                .installed_at
                .checked_sub(SIMULTANEOUS_DIAL_WINDOW + std::time::Duration::from_secs(1))
                .expect("monotonic clock far enough from boot");
        }
        let (new_id, replaced) = connections
            .install_directed(remote, local, "B->A (restart)", addr, 500_000, false)
            .expect("a reconnect outside the window replaces");
        assert_eq!(replaced, Some(old_id));
        assert_eq!(
            connections.peers.get(&remote).unwrap().connection_id,
            new_id
        );
        assert_eq!(peer_count.load(Ordering::Relaxed), 1);
    }

    enum TestStream {
        Stalled(Arc<tokio::sync::Notify>),
        Open(tokio::io::DuplexStream),
    }

    impl tokio::io::AsyncWrite for TestStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            match self.get_mut() {
                Self::Stalled(attempted) => {
                    attempted.notify_one();
                    std::task::Poll::Pending
                }
                Self::Open(stream) => {
                    tokio::io::AsyncWrite::poll_write(std::pin::Pin::new(stream), cx, buf)
                }
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.get_mut() {
                Self::Stalled(_) => std::task::Poll::Pending,
                Self::Open(stream) => {
                    tokio::io::AsyncWrite::poll_flush(std::pin::Pin::new(stream), cx)
                }
            }
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.get_mut() {
                Self::Stalled(_) => std::task::Poll::Pending,
                Self::Open(stream) => {
                    tokio::io::AsyncWrite::poll_shutdown(std::pin::Pin::new(stream), cx)
                }
            }
        }
    }

    const SHORT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

    fn stalled_stream() -> (TestStream, Arc<tokio::sync::Notify>) {
        let attempted = Arc::new(tokio::sync::Notify::new());
        (TestStream::Stalled(attempted.clone()), attempted)
    }

    fn fast_timeout_connections(
        write_base_timeout: std::time::Duration,
    ) -> (
        PeerConnections<TestStream>,
        Arc<AtomicU32>,
        mpsc::Receiver<InboundMessage>,
    ) {
        let (inbound_tx, inbound_rx) = mpsc::channel(8);
        let peer_count = Arc::new(AtomicU32::new(0));
        let mut connections = PeerConnections::new(
            inbound_tx,
            peer_count.clone(),
            Arc::new(TransportWirePolicy::default()),
        );
        connections.write_base_timeout = write_base_timeout;
        (connections, peer_count, inbound_rx)
    }

    #[test]
    fn the_write_timeout_grows_with_the_payload() {
        let base = PEER_WRITE_BASE_TIMEOUT;
        assert_eq!(peer_write_timeout(base, 0), base);
        assert_eq!(
            peer_write_timeout(base, MAX_PAYLOAD_SIZE as usize),
            base + std::time::Duration::from_secs(16)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_peer_write_never_blocks_the_peer_map() {
        let (connections, peer_count, mut inbound_rx) =
            fast_timeout_connections(std::time::Duration::from_secs(2));
        let connections = Arc::new(connections);
        let stalled = [5_u8; 32];
        let other = [6_u8; 32];
        let addr: SocketAddr = "127.0.0.1:7336".parse().unwrap();
        let (stream, attempted) = stalled_stream();
        let (stalled_generation, _) = connections
            .install_directed(stalled, LOCAL, stream, addr, 1, true)
            .unwrap();

        let writer = {
            let connections = connections.clone();
            tokio::spawn(async move {
                connections
                    .send_to(&Hash256(stalled), MessageType::DagHistoryResponse, &[7; 64])
                    .await;
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), attempted.notified())
            .await
            .expect("the write reached the stalled stream");

        let (done_tx, done_rx) = std::sync::mpsc::channel::<&'static str>();
        let (replacement_send, mut replacement_receive) = tokio::io::duplex(4096);
        {
            let connections = connections.clone();
            let done = done_tx.clone();
            tokio::spawn(async move {
                assert!(connections.is_connected(&stalled));
                assert!(matches!(connections.peers.len(), 1 | 2));
                done.send("is_connected and len").unwrap();
            });
        }
        {
            let connections = connections.clone();
            let done = done_tx;
            tokio::spawn(async move {
                let (_, replaced) = connections
                    .install_directed(
                        stalled,
                        LOCAL,
                        TestStream::Open(replacement_send),
                        addr,
                        1,
                        true,
                    )
                    .expect("a same-direction reconnect replaces the stalled stream");
                assert_eq!(replaced, Some(stalled_generation));
                let (stream, _) = stalled_stream();
                connections
                    .install_directed(other, LOCAL, stream, addr, 1, true)
                    .unwrap();
                done.send("install_directed").unwrap();
            });
        }
        for _ in 0..2 {
            done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("peer-map access completed while a peer write was stuck");
        }

        tokio::time::timeout(std::time::Duration::from_secs(5), writer)
            .await
            .expect("the stuck write gave up")
            .unwrap();
        assert!(connections.is_connected(&stalled));
        assert_eq!(connections.peers.len(), 2);
        assert_eq!(peer_count.load(Ordering::Relaxed), 2);
        assert!(
            inbound_rx.try_recv().is_err(),
            "no disconnect for a replaced generation"
        );

        connections
            .send_to(&Hash256(stalled), MessageType::Heartbeat, b"replacement")
            .await;
        let (kind, payload) = read_message(&mut replacement_receive).await.unwrap();
        assert_eq!(kind, MessageType::Heartbeat);
        assert_eq!(payload, b"replacement");
    }

    #[tokio::test]
    async fn a_timed_out_write_removes_and_reports_only_its_own_generation() {
        let (connections, peer_count, mut inbound_rx) =
            fast_timeout_connections(SHORT_WRITE_TIMEOUT);
        let peer = [5_u8; 32];
        let addr: SocketAddr = "127.0.0.1:7337".parse().unwrap();
        let (stream, _) = stalled_stream();
        let (generation, _) = connections
            .install_directed(peer, LOCAL, stream, addr, 1, true)
            .unwrap();

        let started = std::time::Instant::now();
        connections
            .send_to(&Hash256(peer), MessageType::DagHistoryResponse, &[1; 32])
            .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(!connections.is_connected(&peer));
        assert_eq!(peer_count.load(Ordering::Relaxed), 0);
        match inbound_rx.try_recv() {
            Ok(InboundMessage::PeerDisconnected {
                address,
                connection_id,
            }) => {
                assert_eq!(address, Hash256(peer));
                assert_eq!(connection_id, generation);
            }
            _ => panic!("expected one disconnect for the timed-out generation"),
        }
        assert!(inbound_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn no_frame_follows_a_write_cut_off_mid_frame() {
        let (connections, _, _inbound_rx) = fast_timeout_connections(SHORT_WRITE_TIMEOUT);
        let peer = [5_u8; 32];
        let addr: SocketAddr = "127.0.0.1:7338".parse().unwrap();
        let (stream, _) = stalled_stream();
        let (generation, _) = connections
            .install_directed(peer, LOCAL, stream, addr, 1, true)
            .unwrap();

        match connections
            .write_to_peer(&peer, None, MessageType::Heartbeat, &[])
            .await
        {
            PeerWrite::Failed {
                connection_id,
                failure: PeerWriteFailure::TimedOut,
            } => assert_eq!(connection_id, generation),
            other => panic!("expected a timeout, got {other:?}"),
        }
        let started = std::time::Instant::now();
        match connections
            .write_to_peer(&peer, Some(generation), MessageType::Heartbeat, &[])
            .await
        {
            PeerWrite::Failed {
                failure: PeerWriteFailure::Torn,
                ..
            } => {}
            other => panic!("expected a torn stream, got {other:?}"),
        }
        assert!(started.elapsed() < connections.write_base_timeout);
        assert!(matches!(
            connections
                .write_to_peer(&peer, Some(generation + 1), MessageType::Heartbeat, &[])
                .await,
            PeerWrite::NotConnected
        ));
    }

    #[tokio::test]
    async fn a_write_queued_behind_a_longer_one_skips_the_peer_without_removing_it() {
        let (connections, peer_count, mut inbound_rx) =
            fast_timeout_connections(SHORT_WRITE_TIMEOUT);
        let connections = Arc::new(connections);
        let peer = [5_u8; 32];
        let addr: SocketAddr = "127.0.0.1:7340".parse().unwrap();
        let (stream, attempted) = stalled_stream();
        let (generation, _) = connections
            .install_directed(peer, LOCAL, stream, addr, 1, true)
            .unwrap();

        let large = vec![0_u8; 2 * 1024 * 1024];
        let holder_limit = peer_write_timeout(SHORT_WRITE_TIMEOUT, large.len());
        let holder = {
            let connections = connections.clone();
            tokio::spawn(async move {
                connections
                    .write_to_peer(
                        &peer,
                        Some(generation),
                        MessageType::DagHistoryResponse,
                        &large,
                    )
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), attempted.notified())
            .await
            .expect("the long write reached the stream");

        let started = std::time::Instant::now();
        assert!(matches!(
            connections
                .write_to_peer(&peer, Some(generation), MessageType::Heartbeat, &[])
                .await,
            PeerWrite::Busy
        ));
        assert_eq!(
            connections.probe_peers().await,
            0,
            "a busy peer is not pruned"
        );
        assert!(started.elapsed() < holder_limit);
        assert!(connections.is_connected(&peer));
        assert_eq!(peer_count.load(Ordering::Relaxed), 1);
        assert!(
            inbound_rx.try_recv().is_err(),
            "no disconnect for a busy peer"
        );

        match holder.await.unwrap() {
            PeerWrite::Failed {
                connection_id,
                failure: PeerWriteFailure::TimedOut,
            } => assert_eq!(connection_id, generation),
            other => panic!("expected the long write to time out, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_heartbeat_probe_prunes_a_stalled_peer_and_keeps_a_live_one() {
        let (connections, peer_count, _inbound_rx) = fast_timeout_connections(SHORT_WRITE_TIMEOUT);
        let stalled = [5_u8; 32];
        let live = [6_u8; 32];
        let addr: SocketAddr = "127.0.0.1:7339".parse().unwrap();
        let (stream, _) = stalled_stream();
        connections
            .install_directed(stalled, LOCAL, stream, addr, 1, true)
            .unwrap();
        let (live_send, mut live_receive) = tokio::io::duplex(4096);
        connections
            .install_directed(live, LOCAL, TestStream::Open(live_send), addr, 1, true)
            .unwrap();

        let removed =
            tokio::time::timeout(std::time::Duration::from_secs(5), connections.probe_peers())
                .await
                .expect("the probe is bounded by the write timeout");
        assert_eq!(removed, 1);
        assert!(!connections.is_connected(&stalled));
        assert!(connections.is_connected(&live));
        assert_eq!(peer_count.load(Ordering::Relaxed), 1);
        let (kind, payload) = read_message(&mut live_receive).await.unwrap();
        assert_eq!(kind, MessageType::Heartbeat);
        assert!(payload.is_empty());
    }
}
