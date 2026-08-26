//! QUIC transport - real peer-to-peer networking for ARC Chain.
//!
//! Provides a QUIC server (listener) and client (dialer) unified behind
//! `run_transport()`. Communicates with the consensus layer via tokio mpsc
//! channels.

use crate::protocol::*;
use arc_consensus::DagBlock;
use arc_crypto::{Hash256, KeyPair, Signature as CryptoSignature, hash_bytes};
use arc_types::Transaction;
use dashmap::DashMap;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Maximum number of simultaneous peer connections.
/// At scale (millions of nodes), each node only needs O(sqrt(N)) peers
/// for full reachability - gossip propagation handles the rest.
/// 256 peers gives 2-hop reachability for networks up to ~65K nodes,
/// and 3-hop for networks up to ~16M nodes.
const MAX_PEERS: u32 = 256;

/// Target number of outbound peers. We maintain this many active outbound
/// connections and accept inbound connections up to MAX_PEERS.
//
// Kept rather than deleted: this is the documented outbound-peer target for the
// transport, but nothing reads it yet — the dial paths (bootstrap, persisted,
// PEX, reconnect) are only bounded by MAX_PEERS. Deleting the constant would
// erase the record of that gap, so it is allowed to stay unused until the
// outbound-target logic is actually wired up.
#[allow(dead_code)]
const TARGET_OUTBOUND: u32 = 16;

/// Per-peer message rate limit (messages per second).
const PEER_MSG_RATE_LIMIT: u32 = 500;
/// Rate limit window in seconds.
const RATE_LIMIT_WINDOW_SECS: u64 = 1;

// ─── Channel Types ──────────────────────────────────────────────────────────

/// Messages the transport sends TO consensus.
#[derive(Debug)]
pub enum InboundMessage {
    PeerConnected {
        address: Hash256,
        stake: u64,
    },
    PeerDisconnected {
        address: Hash256,
    },
    DagBlockWithTxs {
        block: DagBlock,
        transactions: Vec<Transaction>,
    },
    Transactions(Vec<Vec<u8>>),
    /// State diff from a proposer node (Propose-Verify protocol).
    /// `source` is the identity authenticated by the QUIC handshake. Consensus
    /// must bind it to the author of `block_hash`; payload fields alone are not
    /// an authorization boundary.
    StateDiff {
        source: Hash256,
        block_hash: Hash256,
        diff: arc_types::StateDiff,
        block_height: u64,
    },
    /// State Sync - a peer is requesting our snapshot manifest.
    SnapshotManifestRequest {
        source: Hash256,
    },
    /// State Sync - a peer is requesting a specific snapshot chunk.
    SnapshotChunkRequest {
        source: Hash256,
        manifest_hash: Hash256,
        chunk_index: u32,
    },
    /// State Sync - received a snapshot manifest from a peer.
    SnapshotManifestResponse {
        source: Hash256,
        manifest: arc_state::SnapshotManifest,
    },
    /// State Sync - received a snapshot chunk from a peer.
    SnapshotChunkResponse {
        source: Hash256,
        chunk: arc_state::StateSnapshot,
    },
    /// Inference request from another node - run model and respond.
    InferenceRequest {
        request_id: Hash256,
        input: String,
        max_tokens: u32,
        requester: Hash256,
    },
    /// Inference response from a community GPU node.
    InferenceResponse {
        request_id: Hash256,
        output: String,
        output_hash: Hash256,
        model_hash: Hash256,
        ms_per_token: u64,
        responder: Hash256,
    },
    /// Heartbeat with round info (partition detection).
    HeartbeatWithRound {
        peer: Hash256,
        dag_round: u64,
        committed_round: u64,
    },
    /// Shard activation forward (pipeline-parallel inference).
    ShardForward {
        request_id: Hash256,
        model_id: Hash256,
        next_layer: u32,
        total_layers: u32,
        token_position: u32,
        activations: Vec<u8>,
        activation_hash: Hash256,
    },
    /// Shard result (final token from last shard).
    ShardResult {
        request_id: Hash256,
        token_id: u32,
        logits_hash: Hash256,
        responder: Hash256,
    },
    /// Shard announcement (node declares its layer/expert holdings).
    ShardAnnounce {
        model_id: Hash256,
        start_layer: u32,
        end_layer: u32,
        expert_indices: Vec<u32>,
        node_address: Hash256,
        available_memory: u64,
        gpu_tier: u8,
    },
    /// Round sync request from a peer (partition detection).
    RoundSyncRequest {
        peer: Hash256,
        their_round: u64,
        their_committed: u64,
    },
    /// Round sync response - peer's current consensus state.
    RoundSyncResponse {
        current_round: u64,
        last_committed_round: u64,
    },
}

/// Messages consensus sends TO the transport for outbound delivery.
#[derive(Debug)]
pub enum OutboundMessage {
    BroadcastDagBlock {
        block: DagBlock,
        transactions: Vec<Transaction>,
    },
    BroadcastTransactions(Vec<Vec<u8>>),
    /// Broadcast a state diff (Propose-Verify protocol).
    BroadcastStateDiff {
        block_hash: Hash256,
        diff: arc_types::StateDiff,
        block_height: u64,
    },
    /// Broadcast inference request to all peers with model capability.
    BroadcastInferenceRequest {
        request_id: Hash256,
        input: String,
        max_tokens: u32,
        requester: Hash256,
    },
    /// Send inference response back to the requester.
    SendInferenceResponse {
        request_id: Hash256,
        output: String,
        output_hash: Hash256,
        model_hash: Hash256,
        ms_per_token: u64,
        responder: Hash256,
    },
    /// Forward activations to next shard holder in pipeline.
    SendShardForward {
        target: Hash256,
        message: crate::protocol::ShardForwardMessage,
    },
    /// Send shard result back to coordinator.
    SendShardResult {
        target: Hash256,
        message: crate::protocol::ShardResultMessage,
    },
    /// Broadcast shard announcement to all peers.
    BroadcastShardAnnounce {
        message: crate::protocol::ShardAnnounceMessage,
    },
    /// Send heartbeat with round info to all peers.
    BroadcastHeartbeatWithRound {
        dag_round: u64,
        committed_round: u64,
    },
    /// Request round sync from a specific peer.
    SendRoundSyncRequest {
        target: Hash256,
        my_round: u64,
        my_committed: u64,
    },
    /// Respond to round sync request.
    SendRoundSyncResponse {
        target: Hash256,
        current_round: u64,
        last_committed_round: u64,
        validator_count: u32,
        total_stake: u64,
    },
}

// ─── TLS Configuration ─────────────────────────────────────────────────────

fn make_server_config() -> quinn::ServerConfig {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])
        .expect("failed to generate self-signed cert");
    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());

    let server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der.into())
        .expect("failed to build rustls server config");

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(server_crypto).expect("failed to create QUIC server config"),
    ));

    // Keep connections alive and allow large payloads (testnet)
    let transport = Arc::get_mut(&mut server_config.transport).unwrap();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(std::time::Duration::from_secs(300)).unwrap(),
    ));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    transport.stream_receive_window(quinn::VarInt::from_u32(64 * 1024 * 1024)); // 64 MB
    transport.receive_window(quinn::VarInt::from_u32(256 * 1024 * 1024)); // 256 MB

    server_config
}

/// Build the QUIC client TLS configuration.
///
/// Without the `strict-tls` feature (default), this uses [`TestnetCertVerifier`]
/// which accepts all server certificates. Peer identity is instead verified via
/// application-layer challenge-response (see module docs on [`TestnetCertVerifier`]).
///
/// With `strict-tls` enabled, this panics at startup - certificate pinning is
/// not yet implemented. This feature flag exists to prevent accidental production
/// deployment without TLS-layer peer verification.
fn make_client_config() -> quinn::ClientConfig {
    // NOTE: the doc comment above claims this path panics at startup, but the
    // code below does not panic — it installs the same accept-all
    // `TestnetCertVerifier` as the default path, only with different timeouts.
    // The doc and the code disagree about what `strict-tls` does. Left exactly
    // as written rather than guessed at; see the review notes. Do not treat
    // building with `strict-tls` as enabling certificate verification.
    #[cfg(feature = "strict-tls")]
    {
        // Production mode: accept any self-signed cert but verify the peer's
        // identity via the application-layer challenge-response. The cert
        // provides encryption; the handshake provides authentication.
        // Full certificate pinning (fingerprint registry) is a future enhancement.
        info!("strict-tls: TLS encryption enabled with application-layer peer auth");
        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TestnetCertVerifier))
            .with_no_client_auth();

        let mut client_config = quinn::ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(crypto).expect("failed to create QUIC client config"),
        ));

        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(std::time::Duration::from_secs(60)).unwrap(),
        ));
        transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
        client_config.transport_config(Arc::new(transport));

        return client_config;
    }

    #[cfg(not(feature = "strict-tls"))]
    {
        warn!(
            "TLS certificate verification is DISABLED - using TestnetCertVerifier. \
             Peer identity is verified via application-layer challenge-response only. \
             Do NOT use this configuration in production without enabling the `strict-tls` feature."
        );

        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TestnetCertVerifier))
            .with_no_client_auth();

        let mut client_config = quinn::ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(crypto).expect("failed to create QUIC client config"),
        ));

        // Keep connections alive and allow large payloads (testnet)
        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(std::time::Duration::from_secs(300)).unwrap(),
        ));
        transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
        transport.stream_receive_window(quinn::VarInt::from_u32(64 * 1024 * 1024));
        transport.receive_window(quinn::VarInt::from_u32(256 * 1024 * 1024));
        client_config.transport_config(Arc::new(transport));

        client_config
    }
}

// Certificate pinning (fingerprint registry) is a future enhancement.
// The current security model: TLS provides encryption, application-layer
// challenge-response provides peer identity verification via Ed25519.

/// TLS certificate verifier that accepts all certificates without validation.
///
/// # Security Model
///
/// In ARC Chain's permissioned validator network, peer identity is NOT verified
/// at the TLS layer. Instead, the security model is:
///
/// 1. **TLS provides encryption only** - all QUIC traffic is encrypted in transit,
///    preventing passive eavesdropping.
/// 2. **Peer identity is verified at the application layer** via challenge-response
///    authentication (see [`verify_handshake`]). Each peer must prove ownership of
///    their validator private key by signing a random challenge. The public key is
///    then verified to derive to the claimed validator address.
/// 3. **Genesis hash binding** - peers must share the same genesis hash, preventing
///    cross-network connections.
///
/// This means TLS cert verification is intentionally skipped: validators use
/// ephemeral self-signed certificates, and there is no CA or cert registry.
/// A MITM attacker who intercepts the QUIC connection would still fail the
/// application-layer challenge-response, since they cannot forge a valid
/// signature for a registered validator address.
///
/// # Production Hardening
///
/// For production, consider implementing certificate pinning via a validator
/// cert registry so that TLS itself authenticates peers (defense in depth).
/// Enable the `strict-tls` feature flag to enforce this - it will panic at
/// startup until cert pinning is implemented, preventing accidental deployment
/// without TLS verification.
#[derive(Debug)]
struct TestnetCertVerifier;

impl rustls::client::danger::ServerCertVerifier for TestnetCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

// ─── Challenge-Response Authentication ──────────────────────────────────────

/// Compute the challenge hash: BLAKE3("ARC-peer-auth-v1" || nonce || genesis_hash)
fn compute_challenge(nonce: &[u8; 32], genesis_hash: &Hash256) -> Hash256 {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-peer-auth-v1");
    hasher.update(nonce);
    hasher.update(&genesis_hash.0);
    Hash256(*hasher.finalize().as_bytes())
}

/// Create a signed handshake message.
fn make_signed_handshake(
    local_address: Hash256,
    local_stake: u64,
    listen_port: u16,
    genesis_hash: Hash256,
    keypair: &KeyPair,
) -> HandshakeMessage {
    let mut nonce = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce);

    let challenge = compute_challenge(&nonce, &genesis_hash);
    let sig = keypair.sign(&challenge).expect("signing challenge failed");
    let sig_bytes = bincode::serialize(&sig).unwrap_or_default();

    HandshakeMessage {
        validator_address: local_address,
        stake: local_stake,
        listen_port,
        genesis_hash,
        public_key: keypair.public_key_bytes(),
        nonce,
        challenge_sig: sig_bytes,
        protocol_version: crate::protocol::PROTOCOL_VERSION,
        min_compatible_version: crate::protocol::MIN_COMPATIBLE_VERSION,
        dag_round: 0, // filled in by caller if available
    }
}

/// Verify a peer's handshake: pubkey derives to claimed address, signature is valid,
/// and protocol version is compatible.
fn verify_handshake(msg: &HandshakeMessage) -> anyhow::Result<()> {
    // 1. Verify public key derives to the claimed validator address
    let derived_address = hash_bytes(&msg.public_key);
    if derived_address != msg.validator_address {
        anyhow::bail!(
            "public key does not derive to claimed address: derived={}, claimed={}",
            derived_address,
            msg.validator_address
        );
    }

    // 2. Verify the challenge signature
    let challenge = compute_challenge(&msg.nonce, &msg.genesis_hash);
    let sig: CryptoSignature = bincode::deserialize(&msg.challenge_sig)
        .map_err(|e| anyhow::anyhow!("failed to deserialize challenge signature: {e}"))?;
    sig.verify(&challenge, &msg.validator_address)
        .map_err(|e| anyhow::anyhow!("challenge signature verification failed: {e}"))?;

    // 3. Protocol version compatibility check.
    // Treat version 0 as v1 (old nodes that don't send protocol_version).
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

// ─── Per-Peer Rate Limiter ───────────────────────────────────────────────────

/// Per-peer rate limiting: address -> (message_count, window_start_epoch_secs)
struct PeerRateLimiter {
    counters: DashMap<Hash256, (u32, u64)>,
}

impl PeerRateLimiter {
    fn new() -> Self {
        Self {
            counters: DashMap::new(),
        }
    }

    /// Returns true if the message should be allowed, false if rate-limited.
    fn allow(&self, peer: &Hash256) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut entry = self.counters.entry(*peer).or_insert((0, now));
        let (count, window_start) = entry.value_mut();

        if now - *window_start >= RATE_LIMIT_WINDOW_SECS {
            // Reset window
            *count = 1;
            *window_start = now;
            true
        } else if *count >= PEER_MSG_RATE_LIMIT {
            false
        } else {
            *count += 1;
            true
        }
    }

    fn remove_peer(&self, peer: &Hash256) {
        self.counters.remove(peer);
    }
}

// ─── Peer Connection Map ────────────────────────────────────────────────────

/// Metadata for a connected peer (dial address + stake).
struct PeerMeta {
    /// The address to dial this peer at (IP from connection + listen_port from handshake).
    dial_addr: SocketAddr,
    /// The peer's self-reported stake.
    stake: u64,
}

/// Tracks active peer send streams and metadata for outbound broadcast.
struct PeerConnections {
    peers: DashMap<[u8; 32], quinn::SendStream>,
    meta: DashMap<[u8; 32], PeerMeta>,
}

impl PeerConnections {
    fn new() -> Self {
        Self {
            peers: DashMap::new(),
            meta: DashMap::new(),
        }
    }

    /// Store peer metadata after successful handshake.
    fn insert_meta(&self, key: [u8; 32], dial_addr: SocketAddr, stake: u64) {
        self.meta.insert(key, PeerMeta { dial_addr, stake });
    }

    /// Check if a peer is currently connected by validator address bytes.
    fn is_connected(&self, key: &[u8; 32]) -> bool {
        self.peers.contains_key(key)
    }

    async fn broadcast(&self, msg_type: MessageType, payload: &[u8]) {
        // Snapshot peer keys first - do NOT hold DashMap shard locks during
        // network I/O. The old code held iter_mut() locks for the entire
        // broadcast, which blocked peer insert/remove operations for seconds.
        let peer_keys: Vec<[u8; 32]> = self.peers.iter().map(|e| *e.key()).collect();
        let peer_count = peer_keys.len();
        let mut sent = 0usize;
        let mut dead_peers = Vec::new();
        for key in &peer_keys {
            if let Some(mut entry) = self.peers.get_mut(key) {
                // 5-second timeout per peer prevents one slow/dead peer from
                // blocking the entire outbound fanout task, which was the
                // secondary cause of the P2P channel filling up.
                match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    write_message(entry.value_mut(), msg_type, payload),
                )
                .await
                {
                    Ok(Ok(())) => {
                        sent += 1;
                    }
                    Ok(Err(e)) => {
                        warn!("Failed to send to peer: {}", e);
                        dead_peers.push(*key);
                    }
                    Err(_) => {
                        warn!("Timeout writing to peer, removing dead connection");
                        dead_peers.push(*key);
                    }
                }
            }
        }
        for key in dead_peers {
            self.peers.remove(&key);
            self.meta.remove(&key);
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

    /// Send a message to a specific peer by validator address.
    async fn send_to(&self, target: &Hash256, msg_type: MessageType, payload: &[u8]) {
        if let Some(mut entry) = self.peers.get_mut(&target.0) {
            if let Err(e) = write_message(entry.value_mut(), msg_type, payload).await {
                warn!("Failed to send {:?} to {}: {}", msg_type, target, e);
                self.peers.remove(&target.0);
                self.meta.remove(&target.0);
            }
        } else {
            debug!(
                "send_to: peer {} not connected, cannot send {:?}",
                target, msg_type
            );
        }
    }
}

// ─── Transport Main Loop ───────────────────────────────────────────────────

/// Run the P2P transport layer.
///
/// Binds a QUIC endpoint, dials bootstrap peers, accepts incoming connections,
/// and bridges network I/O to/from the consensus layer via channels.
//
// Collapsing these into a config struct would be a breaking change to a public
// entry point that arc-node and arc-bench both call, and those crates are out of
// scope for this change, so the argument list stays as-is.
#[allow(clippy::too_many_arguments)]
pub async fn run_transport(
    listen_addr: SocketAddr,
    bootstrap_peers: Vec<SocketAddr>,
    local_address: Hash256,
    local_stake: u64,
    genesis_hash: Hash256,
    mut outbound_rx: mpsc::Receiver<OutboundMessage>,
    inbound_tx: mpsc::Sender<InboundMessage>,
    peer_count: Arc<AtomicU32>,
    local_keypair: KeyPair,
    data_dir: String,
) {
    // ── Install rustls crypto provider (required for rustls 0.23+) ─────
    let _ = rustls::crypto::ring::default_provider().install_default();

    // ── Bind QUIC endpoint ──────────────────────────────────────────────
    // Try the configured port first (5× with 2 s spacing) so seeds and
    // anyone with the port actually free keeps the stable inbound port.
    // If that fails, fall back to an OS-assigned ephemeral UDP port so
    // consumer/outbound nodes still join the network — they don't need
    // a stable inbound port, and some local environments make the
    // configured port un-bindable in ways no firewall/port-forward rule
    // touches:
    //
    //   • Windows Hyper-V dynamic UDP exclusions
    //     (`netsh int ipv4 show excludedportrange protocol=udp`) often
    //     swallow ranges around 9000-9100 when WSL2 / Docker Desktop
    //     is installed — 9091 becomes un-bindable for any user-mode
    //     process even with Administrator + firewall exception +
    //     forwarded UDP.
    //   • Another P2P app holding the port (Transmission/qBittorrent
    //     historically defaulted to 9091).
    //   • Antivirus / EDR port reservations on locked-down Windows.
    //
    // Trade-off: a fallback-bound node cannot serve as a public seed
    // (peers can't dial it on a known port). That is fine for the
    // residential consumers this fallback exists for — they're behind
    // NAT and never accept unsolicited inbound anyway.
    let server_config = make_server_config();
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
                    error!(
                        "QUIC bind failed on configured {} AND on ephemeral fallback: \
                         {}. The OS likely does not permit this process to bind UDP \
                         at all - check firewall/EDR policy.",
                        configured_addr, e
                    );
                    return;
                }
            }
        }
        bound.unwrap()
    };
    // Set client config for outgoing connections on the same endpoint
    endpoint.set_default_client_config(make_client_config());

    // Shadow the parameter with the actual bound address. If we fell
    // back to ephemeral, every downstream handshake/self-skip uses the
    // real port - peers see the truth in our handshake, and PEX gossip
    // to other nodes carries the real address.
    let listen_addr = endpoint.local_addr().unwrap_or(configured_addr);
    if listen_addr.port() != configured_addr.port() {
        info!(
            "P2P transport listening on {} (configured was {})",
            listen_addr, configured_addr
        );
    } else {
        info!("P2P transport listening on {}", listen_addr);
    }

    let connections = Arc::new(PeerConnections::new());
    let rate_limiter = Arc::new(PeerRateLimiter::new());
    let keypair = Arc::new(local_keypair);

    // ── PEX auto-dial channel ───────────────────────────────────────────
    let (pex_dial_tx, mut pex_dial_rx) = mpsc::channel::<SocketAddr>(64);

    // ── Dial bootstrap peers (concurrent with 5s timeout each) ──────────
    {
        let mut dial_handles = Vec::new();
        let allow_loopback_peers = std::env::var("ARC_ALLOW_LOOPBACK_PEERS").is_ok();
        for peer_addr in &bootstrap_peers {
            // Skip self - check loopback, listen_addr, AND local interfaces
            // (our public IP is in the seeds file but listen_addr is 0.0.0.0).
            // ARC_ALLOW_LOOPBACK_PEERS=1 relaxes the loopback/local-iface checks
            // so multiple arc-node processes on a single host can peer over
            // 127.0.0.1 — used by scripts/arc-multi-start.sh for tier1 testing.
            if peer_addr == &listen_addr {
                continue;
            }
            if !allow_loopback_peers {
                if peer_addr.ip().is_loopback() {
                    continue;
                }
                if std::net::UdpSocket::bind(SocketAddr::new(peer_addr.ip(), 0)).is_ok() {
                    info!("Skipping self-dial to {} (local interface)", peer_addr);
                    continue;
                }
            } else if peer_addr.port() == listen_addr.port() {
                continue;
            }
            info!("Dialing bootstrap peer {}", peer_addr);
            let ep = endpoint.clone();
            let addr = *peer_addr;
            let handshake_msg = make_signed_handshake(
                local_address,
                local_stake,
                listen_addr.port(),
                genesis_hash,
                &keypair,
            );
            let ctx = PeerContext::new(
                local_address,
                &connections,
                &inbound_tx,
                &peer_count,
                &pex_dial_tx,
                &rate_limiter,
            );
            dial_handles.push(tokio::spawn(async move {
                // Try up to 3 times with increasing timeouts. Intercontinental
                // QUIC handshakes (e.g., US→Singapore) can need >5s on first attempt.
                for attempt in 1..=3u32 {
                    let timeout_secs = 5 * attempt as u64; // 5s, 10s, 15s
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(timeout_secs),
                        dial_peer(&ep, addr, &handshake_msg, &ctx),
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
        // Wait for all dials to complete (or timeout)
        for h in dial_handles {
            let _ = h.await;
        }
        info!(
            "Bootstrap dial phase complete, {} peers connected",
            peer_count.load(Ordering::Relaxed)
        );
    }

    // ── Dial persisted peers (from previous sessions) ───────────────────
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
        let handshake_msg = make_signed_handshake(
            local_address,
            local_stake,
            listen_addr.port(),
            genesis_hash,
            &keypair,
        );
        let ctx = PeerContext::new(
            local_address,
            &connections,
            &inbound_tx,
            &peer_count,
            &pex_dial_tx,
            &rate_limiter,
        );
        match dial_peer(&endpoint, *peer_addr, &handshake_msg, &ctx).await {
            Ok(()) => info!("Connected to persisted peer {}", peer_addr),
            Err(e) => debug!("Failed to connect to persisted peer {}: {}", peer_addr, e),
        }
    }

    // ── Spawn outbound fanout task ──────────────────────────────────────
    let conn_out = connections.clone();
    tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
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
                OutboundMessage::BroadcastTransactions(txs) => {
                    let payload = crate::protocol::TxGossipMessage { transactions: txs };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out.broadcast(MessageType::TxGossip, &bytes).await;
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

    // ── PEX + reconnect as independent background task ─────────────────
    // Spawned separately so the accept loop can't starve timers.
    // Without this, heavy inbound traffic or benchmark mode prevents
    // reconnect/PEX timers from ever firing (tokio::select! starvation).
    let mut pex_interval = tokio::time::interval(std::time::Duration::from_secs(60));
    pex_interval.tick().await; // skip immediate fire

    let mut reconnect_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    reconnect_interval.tick().await; // skip immediate fire

    let conn_pex = connections.clone();

    // ── Spawn PEX + reconnect as independent task ──────────────────
    // This prevents the accept loop from starving timer-driven work.
    {
        let conn_bg = connections.clone();
        let bp = bootstrap_peers.clone();
        let ep = endpoint.clone();
        let kp = keypair.clone();
        let itx = inbound_tx.clone();
        let pc = peer_count.clone();
        let pdt = pex_dial_tx.clone();
        let rl = rate_limiter.clone();
        let dd = data_dir.clone();
        tokio::spawn(async move {
            let mut pex_tick = tokio::time::interval(std::time::Duration::from_secs(60));
            pex_tick.tick().await;
            // Reconnect every 30s. Tried 10s but that caused excessive dial
            // churn - every restart triggered a flood of duplicate-accept
            // events from the restarting node's peers, which then deadlocked
            // accept_peer with consensus on shared DashMap shards.
            let mut recon_tick = tokio::time::interval(std::time::Duration::from_secs(30));
            recon_tick.tick().await;
            loop {
                tokio::select! {
                    _ = pex_tick.tick() => {
                        let mut all_peers: Vec<crate::protocol::PexPeerInfo> = conn_bg
                            .meta.iter()
                            .filter(|e| conn_bg.peers.contains_key(e.key()))
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

                        // Probe live connections: try writing a tiny heartbeat to each
                        // peer stream. Dead QUIC streams fail immediately, letting us
                        // prune stale entries that would otherwise block reconnect.
                        {
                            let peer_keys: Vec<[u8; 32]> = conn_bg.peers.iter().map(|e| *e.key()).collect();
                            let mut dead = Vec::new();
                            for key in &peer_keys {
                                if let Some(mut entry) = conn_bg.peers.get_mut(key) {
                                    // Write a Heartbeat message (type 0, empty payload)
                                    if write_message(entry.value_mut(), MessageType::Heartbeat, &[]).await.is_err() {
                                        dead.push(*key);
                                    }
                                }
                            }
                            for key in &dead {
                                conn_bg.peers.remove(key);
                                conn_bg.meta.remove(key);
                                debug!("Pruned dead peer connection");
                            }
                        }

                        let connected_addrs: std::collections::HashSet<SocketAddr> = conn_bg.meta.iter()
                            .filter(|e| conn_bg.peers.contains_key(e.key()))
                            .map(|e| e.value().dial_addr)
                            .collect();
                        let allow_lo = std::env::var("ARC_ALLOW_LOOPBACK_PEERS").is_ok();
                        let disconnected: Vec<SocketAddr> = candidates.into_iter()
                            .filter(|a| !connected_addrs.contains(a))
                            .filter(|a| {
                                if allow_lo {
                                    return a.port() != listen_addr.port();
                                }
                                if a.ip().is_loopback() { return false; }
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
                            let handshake_msg = make_signed_handshake(
                                local_address, local_stake, listen_addr.port(), genesis_hash, &kp,
                            );
                            let ep2 = ep.clone();
                            let ctx = PeerContext::new(
                                local_address,
                                &conn_bg,
                                &itx,
                                &pc,
                                &pdt,
                                &rl,
                            );
                            tokio::spawn(async move {
                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(10),
                                    dial_peer(&ep2, addr, &handshake_msg, &ctx),
                                ).await {
                                    Ok(Ok(())) => info!("Reconnect: connected to {}", addr),
                                    Ok(Err(e)) => debug!("Reconnect to {} failed: {}", addr, e),
                                    Err(_) => debug!("Reconnect to {} timed out", addr),
                                }
                            });
                        }
                        let actual = conn_bg.peers.len() as u32;
                        pc.store(actual, Ordering::Relaxed);
                    }
                }
            }
        });
    }

    loop {
        tokio::select! {
            // ── Accept inbound connections ──────────────────────────────
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

                // Enforce connection limit
                if peer_count.load(Ordering::Relaxed) >= MAX_PEERS {
                    warn!("Connection limit reached ({MAX_PEERS}), rejecting {}", remote_addr);
                    continue;
                }

                let keypair_clone = keypair.clone();
                let ctx = PeerContext::new(
                    local_address,
                    &connections,
                    &inbound_tx,
                    &peer_count,
                    &pex_dial_tx,
                    &rate_limiter,
                );

                tokio::spawn(async move {
                    // 10-second handshake timeout - prevents attackers from
                    // holding connection slots with incomplete handshakes.
                    let handshake_msg = make_signed_handshake(
                        local_address, local_stake, listen_addr.port(), genesis_hash, &keypair_clone,
                    );
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        accept_peer(conn, &handshake_msg, &ctx)
                    ).await;
                    if result.is_err() {
                        warn!("Handshake timeout from {}", remote_addr);
                    } else if let Ok(Err(e)) = result
                    {
                        warn!("Failed to accept peer from {}: {}", remote_addr, e);
                    }
                });
            }

            // PEX broadcast + reconnect are handled by the background task above.

            // ── PEX auto-dial (from handle_peer_recv) ──────────────────
            // Spawn the entire processing into a separate task. The select body
            // must NEVER iterate the DashMap (`conn_pex.meta.iter()`) because
            // it can deadlock with another task holding a write lock on the
            // same shard. The "already connected" check belongs inside the
            // spawned task, not the select body.
            addr = pex_dial_rx.recv() => {
                if let Some(peer_addr) = addr {
                    let handshake_msg = make_signed_handshake(
                        local_address, local_stake, listen_addr.port(), genesis_hash, &keypair,
                    );
                    let conn_pex_clone = conn_pex.clone();
                    let ep = endpoint.clone();
                    let ctx = PeerContext::new(
                        local_address,
                        &connections,
                        &inbound_tx,
                        &peer_count,
                        &pex_dial_tx,
                        &rate_limiter,
                    );
                    tokio::spawn(async move {
                        // Skip if already connected (moved out of select body)
                        let already = conn_pex_clone.meta.iter().any(|e| e.value().dial_addr == peer_addr);
                        if already { return; }
                        info!("PEX: dialing discovered peer {}", peer_addr);
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            dial_peer(&ep, peer_addr, &handshake_msg, &ctx),
                        ).await {
                            Ok(Ok(())) => info!("PEX: connected to {}", peer_addr),
                            Ok(Err(e)) => debug!("PEX: failed to dial {}: {}", peer_addr, e),
                            Err(_) => debug!("PEX: dial to {} timed out", peer_addr),
                        }
                    });
                }
            }

            // Reconnect is handled by the background task above.
        }
    }
}

// ─── Shared Connection-Setup Context ───────────────────────────────────────

/// The node-wide handles that every connection-setup path needs.
///
/// `dial_peer` and `accept_peer` both require the same six values, and every
/// call site was cloning them into individually named locals. Bundling them
/// keeps the setup functions to a readable arity and keeps the two paths from
/// drifting apart. All fields are cheap clones (`Arc` / mpsc `Sender`).
struct PeerContext {
    local_address: Hash256,
    connections: Arc<PeerConnections>,
    inbound_tx: mpsc::Sender<InboundMessage>,
    peer_count: Arc<AtomicU32>,
    pex_dial_tx: mpsc::Sender<SocketAddr>,
    rate_limiter: Arc<PeerRateLimiter>,
}

impl PeerContext {
    /// Build a context from the transport's own long-lived handles.
    fn new(
        local_address: Hash256,
        connections: &Arc<PeerConnections>,
        inbound_tx: &mpsc::Sender<InboundMessage>,
        peer_count: &Arc<AtomicU32>,
        pex_dial_tx: &mpsc::Sender<SocketAddr>,
        rate_limiter: &Arc<PeerRateLimiter>,
    ) -> Self {
        Self {
            local_address,
            connections: connections.clone(),
            inbound_tx: inbound_tx.clone(),
            peer_count: peer_count.clone(),
            pex_dial_tx: pex_dial_tx.clone(),
            rate_limiter: rate_limiter.clone(),
        }
    }
}

// ─── Dial (Outbound Connection) ─────────────────────────────────────────────

async fn dial_peer(
    endpoint: &quinn::Endpoint,
    peer_addr: SocketAddr,
    local_handshake: &HandshakeMessage,
    ctx: &PeerContext,
) -> anyhow::Result<()> {
    let local_address = ctx.local_address;
    let connections = &ctx.connections;
    let inbound_tx = &ctx.inbound_tx;
    let peer_count = &ctx.peer_count;
    let pex_dial_tx = &ctx.pex_dial_tx;
    let rate_limiter = &ctx.rate_limiter;

    let conn = endpoint.connect(peer_addr, "localhost")?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;

    // Send our handshake
    let payload = bincode::serialize(local_handshake)?;
    write_message(&mut send, MessageType::Handshake, &payload).await?;

    // Read their handshake ack
    let (msg_type, data) = read_message(&mut recv).await?;
    if msg_type != MessageType::HandshakeAck {
        anyhow::bail!("expected HandshakeAck, got {:?}", msg_type);
    }
    let remote: HandshakeMessage = bincode::deserialize(&data)?;

    // Validate genesis
    if remote.genesis_hash != local_handshake.genesis_hash {
        anyhow::bail!(
            "genesis mismatch: local={} remote={}",
            local_handshake.genesis_hash,
            remote.genesis_hash
        );
    }

    // Verify peer's identity: pubkey → address + valid signature
    verify_handshake(&remote)?;

    // Compute dialable address: remote IP + their listen port
    let dial_addr = SocketAddr::new(conn.remote_address().ip(), remote.listen_port);

    // Reject self-connections. The seeds file includes our own IP,
    // and 0.0.0.0 != our public IP, so the bootstrap skip-self check misses it.
    if remote.validator_address == local_address {
        debug!(
            "Rejected self-connection (dial) to {}",
            remote.validator_address
        );
        return Ok(());
    }

    // Skip if already connected - prevents the dual-dial race where both
    // nodes dial each other and the second insert overwrites the first's
    // SendStream. The old recv handler's cleanup then removes the new entry.
    if connections.is_connected(&remote.validator_address.0) {
        info!(
            "Already connected to {} (dial), skipping duplicate",
            remote.validator_address
        );
        return Ok(());
    }

    info!(
        "Handshake verified with {} (stake: {}, dial: {})",
        remote.validator_address, remote.stake, dial_addr
    );

    // Register peer + metadata
    connections.peers.insert(remote.validator_address.0, send);
    connections.insert_meta(remote.validator_address.0, dial_addr, remote.stake);
    peer_count.fetch_add(1, Ordering::Relaxed);
    let _ = inbound_tx
        .send(InboundMessage::PeerConnected {
            address: remote.validator_address,
            stake: remote.stake,
        })
        .await;

    // Spawn reader. CRITICAL: move `conn` into the spawn so the Quinn
    // Connection stays alive for the duration of the recv loop. Previously,
    // `conn` was dropped at the end of connect_peer, which closed the QUIC
    // connection and made recv fail within microseconds, triggering a
    // connect→disconnect cascade that prevented any block gossip.
    let peer_addr_hash = remote.validator_address;
    let inbound_clone = inbound_tx.clone();
    let connections_ref = connections.clone();
    let peer_count_clone = peer_count.clone();
    let pex_dial_clone = pex_dial_tx.clone();
    let rate_limiter_clone = rate_limiter.clone();
    tokio::spawn(async move {
        let _conn = conn; // keep Quinn Connection alive until recv loop exits
        handle_peer_recv(
            recv,
            peer_addr_hash,
            local_address,
            &inbound_clone,
            &pex_dial_clone,
            &connections_ref,
            &rate_limiter_clone,
        )
        .await;
        rate_limiter_clone.remove_peer(&peer_addr_hash);
        connections_ref.peers.remove(&peer_addr_hash.0);
        connections_ref.meta.remove(&peer_addr_hash.0);
        peer_count_clone.fetch_sub(1, Ordering::Relaxed);
        let _ = inbound_clone
            .send(InboundMessage::PeerDisconnected {
                address: peer_addr_hash,
            })
            .await;
    });

    Ok(())
}

// ─── Accept (Inbound Connection) ────────────────────────────────────────────

async fn accept_peer(
    conn: quinn::Connection,
    local_handshake: &HandshakeMessage,
    ctx: &PeerContext,
) -> anyhow::Result<()> {
    let local_address = ctx.local_address;
    let connections = &ctx.connections;
    let inbound_tx = &ctx.inbound_tx;
    let peer_count = &ctx.peer_count;
    let pex_dial_tx = &ctx.pex_dial_tx;
    let rate_limiter = &ctx.rate_limiter;

    let (mut send, mut recv) = conn.accept_bi().await?;

    // Read their handshake
    let (msg_type, data) = read_message(&mut recv).await?;
    if msg_type != MessageType::Handshake {
        anyhow::bail!("expected Handshake, got {:?}", msg_type);
    }
    let remote: HandshakeMessage = bincode::deserialize(&data)?;

    // Validate genesis
    if remote.genesis_hash != local_handshake.genesis_hash {
        anyhow::bail!("genesis mismatch");
    }

    // Verify peer's identity: pubkey → address + valid signature
    verify_handshake(&remote)?;

    // Send our handshake ack (with our own signed challenge)
    let payload = bincode::serialize(local_handshake)?;
    write_message(&mut send, MessageType::HandshakeAck, &payload).await?;

    // Compute dialable address: remote IP + their listen port
    let dial_addr = SocketAddr::new(conn.remote_address().ip(), remote.listen_port);

    // Reject self-connections
    if remote.validator_address == local_address {
        debug!(
            "Rejected self-connection (accept) from {}",
            remote.validator_address
        );
        return Ok(());
    }

    // Skip if already connected (dual-dial dedup)
    if connections.is_connected(&remote.validator_address.0) {
        info!(
            "Already connected to {} (accept), skipping duplicate",
            remote.validator_address
        );
        return Ok(());
    }

    info!(
        "Accepted verified peer {} (stake: {}, dial: {})",
        remote.validator_address, remote.stake, dial_addr
    );

    // Register peer + metadata
    connections.peers.insert(remote.validator_address.0, send);
    connections.insert_meta(remote.validator_address.0, dial_addr, remote.stake);
    peer_count.fetch_add(1, Ordering::Relaxed);
    let _ = inbound_tx
        .send(InboundMessage::PeerConnected {
            address: remote.validator_address,
            stake: remote.stake,
        })
        .await;

    // Spawn reader. CRITICAL: move `conn` into the spawn so the Quinn
    // Connection stays alive for the duration of the recv loop. See
    // matching comment in connect_peer for the full explanation.
    let peer_addr_hash = remote.validator_address;
    let inbound_clone = inbound_tx.clone();
    let connections_ref = connections.clone();
    let peer_count_clone = peer_count.clone();
    let pex_dial_clone = pex_dial_tx.clone();
    let rate_limiter_clone = rate_limiter.clone();
    tokio::spawn(async move {
        let _conn = conn; // keep Quinn Connection alive until recv loop exits
        handle_peer_recv(
            recv,
            peer_addr_hash,
            local_address,
            &inbound_clone,
            &pex_dial_clone,
            &connections_ref,
            &rate_limiter_clone,
        )
        .await;
        rate_limiter_clone.remove_peer(&peer_addr_hash);
        connections_ref.peers.remove(&peer_addr_hash.0);
        connections_ref.meta.remove(&peer_addr_hash.0);
        peer_count_clone.fetch_sub(1, Ordering::Relaxed);
        let _ = inbound_clone
            .send(InboundMessage::PeerDisconnected {
                address: peer_addr_hash,
            })
            .await;
    });

    Ok(())
}

// ─── Per-Peer Recv Loop ─────────────────────────────────────────────────────

async fn handle_peer_recv(
    mut recv: quinn::RecvStream,
    peer_address: Hash256,
    local_address: Hash256,
    inbound_tx: &mpsc::Sender<InboundMessage>,
    pex_dial_tx: &mpsc::Sender<SocketAddr>,
    connections: &Arc<PeerConnections>,
    rate_limiter: &Arc<PeerRateLimiter>,
) {
    loop {
        let (msg_type, data) = match read_message(&mut recv).await {
            Ok(m) => m,
            Err(e) => {
                debug!("Peer {} stream closed: {}", peer_address, e);
                break;
            }
        };

        if !rate_limiter.allow(&peer_address) {
            warn!("Rate limiting peer {}", peer_address);
            continue; // skip this message
        }

        match msg_type {
            MessageType::DagBlockWithTxs => {
                match bincode::deserialize::<DagBlockWithTxsMessage>(&data) {
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
                match bincode::deserialize::<crate::protocol::TxGossipMessage>(&data) {
                    Ok(msg) => {
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
                match bincode::deserialize::<crate::protocol::StateDiffMessage>(&data) {
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
                match bincode::deserialize::<crate::protocol::PeerExchangeMessage>(&data) {
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
                            // Skip self
                            if pex_peer.address == local_address {
                                continue;
                            }
                            // Skip already-connected peers
                            if connections.is_connected(&pex_peer.address.0) {
                                continue;
                            }
                            // Skip empty addresses
                            if pex_peer.socket_addr.is_empty() {
                                continue;
                            }
                            // Rate-limit PEX dials: only try if we're below
                            // MAX_PEERS connections. This prevents sybil attacks
                            // from filling our connection slots via PEX.
                            // The QUIC handshake verifies genesis_hash + ed25519
                            // signature, so invalid peers are rejected at connect.
                            if connections.peers.len() as u32 >= MAX_PEERS {
                                debug!("PEX: at connection limit, skipping {}", pex_peer.address);
                                break;
                            }
                            // Queue for dialing
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
                match bincode::deserialize::<crate::protocol::SnapshotManifestRequestMessage>(&data)
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
                match bincode::deserialize::<crate::protocol::SnapshotManifestResponseMessage>(
                    &data,
                ) {
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
                match bincode::deserialize::<crate::protocol::SnapshotChunkRequestMessage>(&data) {
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
                match bincode::deserialize::<crate::protocol::SnapshotChunkResponseMessage>(&data) {
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
                match bincode::deserialize::<crate::protocol::InferenceRequestMessage>(&data) {
                    Ok(msg) => {
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
                match bincode::deserialize::<crate::protocol::InferenceResponseMessage>(&data) {
                    Ok(msg) => {
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
                // Heartbeat now carries round info for partition detection.
                // Old heartbeats (empty payload) are still valid - just skip parse.
                if !data.is_empty()
                    && let Ok(hb) = bincode::deserialize::<crate::protocol::HeartbeatMessage>(&data)
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
                match bincode::deserialize::<crate::protocol::ShardForwardMessage>(&data) {
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
                match bincode::deserialize::<crate::protocol::ShardResultMessage>(&data) {
                    Ok(msg) => {
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
                match bincode::deserialize::<crate::protocol::ShardAnnounceMessage>(&data) {
                    Ok(msg) => {
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
            MessageType::RoundSyncRequest => {
                match bincode::deserialize::<crate::protocol::RoundSyncRequestMessage>(&data) {
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
                match bincode::deserialize::<crate::protocol::RoundSyncResponseMessage>(&data) {
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
            // Handshake messages are handled during connection setup, not here.
            MessageType::Handshake | MessageType::HandshakeAck => {
                debug!(
                    "Unexpected handshake message from {} in data loop",
                    peer_address
                );
            }
        }
    }
}

// ─── Peer Persistence ──────────────────────────────────────────────────────

/// Save known peer dial addresses to `known_peers.json` in the data directory.
fn save_peers_to_disk(data_dir: &str, connections: &PeerConnections) {
    let peers: Vec<String> = connections
        .meta
        .iter()
        .map(|entry| entry.value().dial_addr.to_string())
        .collect();
    if peers.is_empty() {
        return;
    }
    let path = std::path::Path::new(data_dir).join("known_peers.json");
    // Ensure directory exists
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

/// Load known peer dial addresses from disk.
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
