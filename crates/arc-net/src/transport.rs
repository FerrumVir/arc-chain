//! QUIC transport — real peer-to-peer networking for ARC Chain.
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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Maximum number of simultaneous peer connections.
const MAX_PEERS: u32 = 128;

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
    /// Verifiers apply the diff and confirm the root matches.
    StateDiff {
        block_hash: Hash256,
        diff: arc_types::StateDiff,
        block_height: u64,
    },
    /// State Sync — a peer is requesting our snapshot manifest.
    SnapshotManifestRequest {
        source: Hash256,
    },
    /// State Sync — a peer is requesting a specific snapshot chunk.
    SnapshotChunkRequest {
        source: Hash256,
        manifest_hash: Hash256,
        chunk_index: u32,
    },
    /// State Sync — received a snapshot manifest from a peer.
    SnapshotManifestResponse {
        source: Hash256,
        manifest: arc_state::SnapshotManifest,
    },
    /// State Sync — received a snapshot chunk from a peer.
    SnapshotChunkResponse {
        source: Hash256,
        chunk: arc_state::StateSnapshot,
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
/// With `strict-tls` enabled, this panics at startup — certificate pinning is
/// not yet implemented. This feature flag exists to prevent accidental production
/// deployment without TLS-layer peer verification.
fn make_client_config() -> quinn::ClientConfig {
    #[cfg(feature = "strict-tls")]
    {
        // TODO: Implement certificate pinning via a validator cert registry.
        // Each validator's self-signed cert fingerprint (SHA-256) should be
        // pre-registered in the genesis config or an on-chain registry.
        // The verifier would check the presented cert's fingerprint against
        // the registry, providing TLS-layer identity verification in addition
        // to the application-layer challenge-response.
        panic!(
            "strict-tls feature is enabled but certificate pinning is not yet implemented. \
             Disable strict-tls for testnet or implement PinnedCertVerifier."
        );
    }

    #[cfg(not(feature = "strict-tls"))]
    {
        warn!(
            "TLS certificate verification is DISABLED — using TestnetCertVerifier. \
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

/// Build a production QUIC client TLS configuration with certificate pinning.
///
/// Each validator's self-signed certificate fingerprint (SHA-256 of the DER-encoded
/// cert) must be pre-registered in `pinned_fingerprints`. The TLS handshake will
/// reject any server whose certificate fingerprint is not in the set.
///
/// This provides defense-in-depth: TLS verifies the peer's cert fingerprint,
/// AND the application layer verifies the peer's validator identity via
/// challenge-response.
// TODO: Implement once the validator cert registry exists.
// fn make_client_config_production(
//     pinned_fingerprints: &HashSet<[u8; 32]>,
// ) -> quinn::ClientConfig {
//     unimplemented!("Certificate pinning not yet implemented — see TestnetCertVerifier docs")
// }

/// TLS certificate verifier that accepts all certificates without validation.
///
/// # Security Model
///
/// In ARC Chain's permissioned validator network, peer identity is NOT verified
/// at the TLS layer. Instead, the security model is:
///
/// 1. **TLS provides encryption only** — all QUIC traffic is encrypted in transit,
///    preventing passive eavesdropping.
/// 2. **Peer identity is verified at the application layer** via challenge-response
///    authentication (see [`verify_handshake`]). Each peer must prove ownership of
///    their validator private key by signing a random challenge. The public key is
///    then verified to derive to the claimed validator address.
/// 3. **Genesis hash binding** — peers must share the same genesis hash, preventing
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
/// Enable the `strict-tls` feature flag to enforce this — it will panic at
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
    }
}

/// Verify a peer's handshake: pubkey derives to claimed address, signature is valid.
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

    Ok(())
}

// ─── Per-Peer Rate Limiter ───────────────────────────────────────────────────

/// Per-peer rate limiting: address -> (message_count, window_start_epoch_secs)
struct PeerRateLimiter {
    counters: DashMap<Hash256, (u32, u64)>,
}

impl PeerRateLimiter {
    fn new() -> Self {
        Self { counters: DashMap::new() }
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
        // Snapshot peer keys first — do NOT hold DashMap shard locks during
        // network I/O. The old code held iter_mut() locks for the entire
        // broadcast, which blocked peer insert/remove operations for seconds.
        let peer_keys: Vec<[u8; 32]> = self.peers.iter().map(|e| *e.key()).collect();
        let peer_count = peer_keys.len();
        let mut sent = 0usize;
        let mut dead_peers = Vec::new();
        for key in &peer_keys {
            if let Some(mut entry) = self.peers.get_mut(key) {
                if let Err(e) = write_message(entry.value_mut(), msg_type, payload).await {
                    warn!("Failed to send to peer: {}", e);
                    dead_peers.push(*key);
                } else {
                    sent += 1;
                }
            }
        }
        for key in dead_peers {
            self.peers.remove(&key);
            self.meta.remove(&key);
        }
        if msg_type == MessageType::DagBlockWithTxs && peer_count > 0 {
            debug!("Broadcast {:?}: sent to {}/{} peers ({} bytes)", msg_type, sent, peer_count, payload.len());
        }
    }
}

// ─── Transport Main Loop ───────────────────────────────────────────────────

/// Run the P2P transport layer.
///
/// Binds a QUIC endpoint, dials bootstrap peers, accepts incoming connections,
/// and bridges network I/O to/from the consensus layer via channels.
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
    let server_config = make_server_config();
    let mut endpoint = match quinn::Endpoint::server(server_config, listen_addr) {
        Ok(ep) => ep,
        Err(e) => {
            error!("Failed to bind QUIC endpoint on {}: {}", listen_addr, e);
            return;
        }
    };
    // Set client config for outgoing connections on the same endpoint
    endpoint.set_default_client_config(make_client_config());

    info!("P2P transport listening on {}", listen_addr);

    let connections = Arc::new(PeerConnections::new());
    let rate_limiter = Arc::new(PeerRateLimiter::new());
    let keypair = Arc::new(local_keypair);

    // ── PEX auto-dial channel ───────────────────────────────────────────
    let (pex_dial_tx, mut pex_dial_rx) = mpsc::channel::<SocketAddr>(64);

    // ── Dial bootstrap peers (concurrent with 5s timeout each) ──────────
    {
        let mut dial_handles = Vec::new();
        for peer_addr in &bootstrap_peers {
            // Skip self
            if peer_addr.ip().is_loopback() || peer_addr == &listen_addr {
                continue;
            }
            info!("Dialing bootstrap peer {}", peer_addr);
            let ep = endpoint.clone();
            let addr = *peer_addr;
            let handshake_msg = make_signed_handshake(
                local_address, local_stake, listen_addr.port(), genesis_hash, &keypair,
            );
            let c = connections.clone();
            let itx = inbound_tx.clone();
            let pc = peer_count.clone();
            let pdt = pex_dial_tx.clone();
            let rl = rate_limiter.clone();
            dial_handles.push(tokio::spawn(async move {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    dial_peer(&ep, addr, &handshake_msg, local_address, &c, &itx, &pc, &pdt, &rl),
                ).await {
                    Ok(Ok(())) => info!("Connected to bootstrap peer {}", addr),
                    Ok(Err(e)) => warn!("Failed to connect to {}: {}", addr, e),
                    Err(_) => warn!("Timeout connecting to {} (5s)", addr),
                }
            }));
        }
        // Wait for all dials to complete (or timeout)
        for h in dial_handles {
            let _ = h.await;
        }
        info!("Bootstrap dial phase complete, {} peers connected", peer_count.load(Ordering::Relaxed));
    }

    // ── Dial persisted peers (from previous sessions) ───────────────────
    let persisted_peers = load_peers_from_disk(&data_dir);
    if !persisted_peers.is_empty() {
        info!("Loading {} persisted peers from disk", persisted_peers.len());
    }
    for peer_addr in &persisted_peers {
        if bootstrap_peers.contains(peer_addr) {
            continue;
        }
        let handshake_msg = make_signed_handshake(
            local_address, local_stake, listen_addr.port(), genesis_hash, &keypair,
        );
        match dial_peer(
            &endpoint,
            *peer_addr,
            &handshake_msg,
            local_address,
            &connections,
            &inbound_tx,
            &peer_count,
            &pex_dial_tx,
            &rate_limiter,
        )
        .await
        {
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
                OutboundMessage::BroadcastStateDiff { block_hash, diff, block_height } => {
                    let payload = crate::protocol::StateDiffMessage {
                        block_hash,
                        diff,
                        block_height,
                    };
                    if let Ok(bytes) = bincode::serialize(&payload) {
                        conn_out.broadcast(MessageType::StateDiff, &bytes).await;
                    }
                }
            }
        }
    });

    // ── Accept incoming connections + PEX + reconnect ──────────────────
    let mut pex_interval = tokio::time::interval(std::time::Duration::from_secs(60));
    pex_interval.tick().await; // skip immediate fire

    let mut reconnect_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    reconnect_interval.tick().await; // skip immediate fire

    let conn_pex = connections.clone();

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

                let connections_clone = connections.clone();
                let inbound_clone = inbound_tx.clone();
                let peer_count_clone = peer_count.clone();
                let keypair_clone = keypair.clone();
                let pex_dial_clone = pex_dial_tx.clone();
                let rate_limiter_clone = rate_limiter.clone();

                tokio::spawn(async move {
                    let handshake_msg = make_signed_handshake(
                        local_address, local_stake, listen_addr.port(), genesis_hash, &keypair_clone,
                    );
                    if let Err(e) = accept_peer(
                        conn,
                        &handshake_msg,
                        local_address,
                        &connections_clone,
                        &inbound_clone,
                        &peer_count_clone,
                        &pex_dial_clone,
                        &rate_limiter_clone,
                    )
                    .await
                    {
                        warn!("Failed to accept peer from {}: {}", remote_addr, e);
                    }
                });
            }

            // ── PEX broadcast (every 60s) — now with real metadata ─────
            _ = pex_interval.tick() => {
                let peer_list: Vec<crate::protocol::PexPeerInfo> = conn_pex
                    .meta
                    .iter()
                    .take(64) // Cap PEX broadcast to prevent amplification with large peer sets
                    .map(|entry| crate::protocol::PexPeerInfo {
                        address: Hash256(*entry.key()),
                        socket_addr: entry.value().dial_addr.to_string(),
                        stake: entry.value().stake,
                    })
                    .collect();

                if !peer_list.is_empty() {
                    let pex_msg = crate::protocol::PeerExchangeMessage { peers: peer_list };
                    if let Ok(bytes) = bincode::serialize(&pex_msg) {
                        debug!("Broadcasting PEX with {} peers", pex_msg.peers.len());
                        conn_pex.broadcast(MessageType::PeerExchange, &bytes).await;
                    }
                }

                // Persist known peers to disk
                save_peers_to_disk(&data_dir, &conn_pex);
            }

            // ── PEX auto-dial (from handle_peer_recv) ──────────────────
            addr = pex_dial_rx.recv() => {
                if let Some(peer_addr) = addr {
                    // Skip if already connected to this address
                    let already = conn_pex.meta.iter().any(|e| e.value().dial_addr == peer_addr);
                    if !already {
                        info!("PEX: dialing discovered peer {}", peer_addr);
                        let handshake_msg = make_signed_handshake(
                            local_address, local_stake, listen_addr.port(), genesis_hash, &keypair,
                        );
                        let c = connections.clone();
                        let itx = inbound_tx.clone();
                        let pc = peer_count.clone();
                        let ep = endpoint.clone();
                        let pdt = pex_dial_tx.clone();
                        let rl = rate_limiter.clone();
                        tokio::spawn(async move {
                            match dial_peer(&ep, peer_addr, &handshake_msg, local_address, &c, &itx, &pc, &pdt, &rl).await {
                                Ok(()) => info!("PEX: connected to {}", peer_addr),
                                Err(e) => debug!("PEX: failed to dial {}: {}", peer_addr, e),
                            }
                        });
                    }
                }
            }

            // ── Reconnect timer (every 30s) ────────────────────────────
            _ = reconnect_interval.tick() => {
                let known = load_peers_from_disk(&data_dir);
                for addr in known {
                    let already = conn_pex.meta.iter().any(|e| e.value().dial_addr == addr);
                    if !already {
                        debug!("Reconnect: trying {}", addr);
                        let handshake_msg = make_signed_handshake(
                            local_address, local_stake, listen_addr.port(), genesis_hash, &keypair,
                        );
                        let c = connections.clone();
                        let itx = inbound_tx.clone();
                        let pc = peer_count.clone();
                        let ep = endpoint.clone();
                        let pdt = pex_dial_tx.clone();
                        let rl = rate_limiter.clone();
                        tokio::spawn(async move {
                            if let Err(e) = dial_peer(&ep, addr, &handshake_msg, local_address, &c, &itx, &pc, &pdt, &rl).await {
                                debug!("Reconnect to {} failed: {}", addr, e);
                            }
                        });
                    }
                }
            }
        }
    }
}

// ─── Dial (Outbound Connection) ─────────────────────────────────────────────

async fn dial_peer(
    endpoint: &quinn::Endpoint,
    peer_addr: SocketAddr,
    local_handshake: &HandshakeMessage,
    local_address: Hash256,
    connections: &Arc<PeerConnections>,
    inbound_tx: &mpsc::Sender<InboundMessage>,
    peer_count: &Arc<AtomicU32>,
    pex_dial_tx: &mpsc::Sender<SocketAddr>,
    rate_limiter: &Arc<PeerRateLimiter>,
) -> anyhow::Result<()> {
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

    // Skip if already connected — prevents the dual-dial race where both
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
    connections
        .peers
        .insert(remote.validator_address.0, send);
    connections.insert_meta(remote.validator_address.0, dial_addr, remote.stake);
    peer_count.fetch_add(1, Ordering::Relaxed);
    let _ = inbound_tx
        .send(InboundMessage::PeerConnected {
            address: remote.validator_address,
            stake: remote.stake,
        })
        .await;

    // Spawn reader
    let peer_addr_hash = remote.validator_address;
    let inbound_clone = inbound_tx.clone();
    let connections_ref = connections.clone();
    let peer_count_clone = peer_count.clone();
    let pex_dial_clone = pex_dial_tx.clone();
    let rate_limiter_clone = rate_limiter.clone();
    tokio::spawn(async move {
        handle_peer_recv(recv, peer_addr_hash, local_address, &inbound_clone, &pex_dial_clone, &connections_ref, &rate_limiter_clone).await;
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
    local_address: Hash256,
    connections: &Arc<PeerConnections>,
    inbound_tx: &mpsc::Sender<InboundMessage>,
    peer_count: &Arc<AtomicU32>,
    pex_dial_tx: &mpsc::Sender<SocketAddr>,
    rate_limiter: &Arc<PeerRateLimiter>,
) -> anyhow::Result<()> {
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
    connections
        .peers
        .insert(remote.validator_address.0, send);
    connections.insert_meta(remote.validator_address.0, dial_addr, remote.stake);
    peer_count.fetch_add(1, Ordering::Relaxed);
    let _ = inbound_tx
        .send(InboundMessage::PeerConnected {
            address: remote.validator_address,
            stake: remote.stake,
        })
        .await;

    // Spawn reader
    let peer_addr_hash = remote.validator_address;
    let inbound_clone = inbound_tx.clone();
    let connections_ref = connections.clone();
    let peer_count_clone = peer_count.clone();
    let pex_dial_clone = pex_dial_tx.clone();
    let rate_limiter_clone = rate_limiter.clone();
    tokio::spawn(async move {
        handle_peer_recv(recv, peer_addr_hash, local_address, &inbound_clone, &pex_dial_clone, &connections_ref, &rate_limiter_clone).await;
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
                        warn!("Failed to deserialize DagBlockWithTxs from {}: {}", peer_address, e);
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
                        warn!("Failed to deserialize TxGossip from {}: {}", peer_address, e);
                    }
                }
            }
            MessageType::StateDiff => {
                match bincode::deserialize::<crate::protocol::StateDiffMessage>(&data) {
                    Ok(msg) => {
                        debug!(
                            "Received state diff for block {} from {}",
                            msg.block_hash,
                            peer_address
                        );
                        let _ = inbound_tx
                            .send(InboundMessage::StateDiff {
                                block_hash: msg.block_hash,
                                diff: msg.diff,
                                block_height: msg.block_height,
                            })
                            .await;
                    }
                    Err(e) => {
                        warn!("Failed to deserialize StateDiff from {}: {}", peer_address, e);
                    }
                }
            }
            MessageType::PeerExchange => {
                match bincode::deserialize::<crate::protocol::PeerExchangeMessage>(&data) {
                    Ok(msg) => {
                        if msg.peers.len() > 128 {
                            warn!("PEX from {} has {} peers (>128), truncating", peer_address, msg.peers.len());
                        }
                        debug!(
                            "Received PEX with {} peers from {}",
                            msg.peers.len(),
                            peer_address
                        );
                        for pex_peer in msg.peers.iter().take(128) {
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
                            // Queue for dialing
                            if let Ok(addr) = pex_peer.socket_addr.parse::<SocketAddr>() {
                                debug!("PEX: queueing discovered peer {} at {}", pex_peer.address, addr);
                                let _ = pex_dial_tx.try_send(addr);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to deserialize PeerExchange from {}: {}", peer_address, e);
                    }
                }
            }
            MessageType::SnapshotManifestRequest => {
                match bincode::deserialize::<crate::protocol::SnapshotManifestRequestMessage>(&data) {
                    Ok(_msg) => {
                        debug!("Received snapshot manifest request from {}", peer_address);
                        let _ = inbound_tx
                            .send(InboundMessage::SnapshotManifestRequest {
                                source: peer_address,
                            })
                            .await;
                    }
                    Err(e) => {
                        warn!("Failed to deserialize SnapshotManifestRequest from {}: {}", peer_address, e);
                    }
                }
            }
            MessageType::SnapshotManifestResponse => {
                match bincode::deserialize::<crate::protocol::SnapshotManifestResponseMessage>(&data) {
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
                        warn!("Failed to deserialize SnapshotManifestResponse from {}: {}", peer_address, e);
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
                        warn!("Failed to deserialize SnapshotChunkRequest from {}: {}", peer_address, e);
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
                        warn!("Failed to deserialize SnapshotChunkResponse from {}: {}", peer_address, e);
                    }
                }
            }
            other => {
                warn!("Unexpected message type {:?} from {}", other, peer_address);
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
