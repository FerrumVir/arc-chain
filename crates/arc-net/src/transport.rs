//! QUIC transport — real peer-to-peer networking for ARC Chain.
//!
//! Provides a QUIC server (listener) and client (dialer) unified behind
//! `run_transport()`. Communicates with the consensus layer via tokio mpsc
//! channels.

use crate::protocol::*;
use arc_consensus::DagBlock;
use arc_crypto::Hash256;
use arc_types::Transaction;
use dashmap::DashMap;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

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
}

/// Messages consensus sends TO the transport for outbound delivery.
#[derive(Debug)]
pub enum OutboundMessage {
    BroadcastDagBlock {
        block: DagBlock,
        transactions: Vec<Transaction>,
    },
    BroadcastTransactions(Vec<Vec<u8>>),
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

    // Keep connections alive for a long time (testnet)
    let transport = Arc::get_mut(&mut server_config.transport).unwrap();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(std::time::Duration::from_secs(300)).unwrap(),
    ));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));

    server_config
}

fn make_client_config() -> quinn::ClientConfig {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    let mut client_config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).expect("failed to create QUIC client config"),
    ));

    // Keep connections alive for a long time (testnet)
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(std::time::Duration::from_secs(300)).unwrap(),
    ));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    client_config.transport_config(Arc::new(transport));

    client_config
}

/// Skip TLS certificate verification (testnet only).
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
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

// ─── Peer Connection Map ────────────────────────────────────────────────────

/// Tracks active peer send streams for outbound broadcast.
struct PeerConnections {
    peers: DashMap<[u8; 32], quinn::SendStream>,
}

impl PeerConnections {
    fn new() -> Self {
        Self {
            peers: DashMap::new(),
        }
    }

    async fn broadcast(&self, msg_type: MessageType, payload: &[u8]) {
        let mut dead_peers = Vec::new();
        for mut entry in self.peers.iter_mut() {
            if let Err(e) = write_message(entry.value_mut(), msg_type, payload).await {
                warn!("Failed to send to peer: {}", e);
                dead_peers.push(*entry.key());
            }
        }
        for key in dead_peers {
            self.peers.remove(&key);
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
    let handshake_msg = HandshakeMessage {
        validator_address: local_address,
        stake: local_stake,
        listen_port: listen_addr.port(),
        genesis_hash,
    };

    // ── Dial bootstrap peers ────────────────────────────────────────────
    for peer_addr in &bootstrap_peers {
        info!("Dialing bootstrap peer {}", peer_addr);
        match dial_peer(
            &endpoint,
            *peer_addr,
            &handshake_msg,
            &connections,
            &inbound_tx,
        )
        .await
        {
            Ok(()) => info!("Connected to bootstrap peer {}", peer_addr),
            Err(e) => warn!("Failed to connect to {}: {}", peer_addr, e),
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
            }
        }
    });

    // ── Accept incoming connections ─────────────────────────────────────
    loop {
        let incoming = match endpoint.accept().await {
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

        let handshake_clone = handshake_msg.clone();
        let connections_clone = connections.clone();
        let inbound_clone = inbound_tx.clone();

        tokio::spawn(async move {
            if let Err(e) = accept_peer(
                conn,
                &handshake_clone,
                &connections_clone,
                &inbound_clone,
            )
            .await
            {
                warn!("Failed to accept peer from {}: {}", remote_addr, e);
            }
        });
    }
}

// ─── Dial (Outbound Connection) ─────────────────────────────────────────────

async fn dial_peer(
    endpoint: &quinn::Endpoint,
    peer_addr: SocketAddr,
    local_handshake: &HandshakeMessage,
    connections: &Arc<PeerConnections>,
    inbound_tx: &mpsc::Sender<InboundMessage>,
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

    info!(
        "Handshake complete with {} (stake: {}, port: {})",
        remote.validator_address, remote.stake, remote.listen_port
    );

    // Register peer
    connections
        .peers
        .insert(remote.validator_address.0, send);
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
    tokio::spawn(async move {
        handle_peer_recv(recv, peer_addr_hash, &inbound_clone).await;
        connections_ref.peers.remove(&peer_addr_hash.0);
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
    connections: &Arc<PeerConnections>,
    inbound_tx: &mpsc::Sender<InboundMessage>,
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

    // Send our handshake ack
    let payload = bincode::serialize(local_handshake)?;
    write_message(&mut send, MessageType::HandshakeAck, &payload).await?;

    info!(
        "Accepted peer {} (stake: {})",
        remote.validator_address, remote.stake
    );

    // Register peer
    connections
        .peers
        .insert(remote.validator_address.0, send);
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
    tokio::spawn(async move {
        handle_peer_recv(recv, peer_addr_hash, &inbound_clone).await;
        connections_ref.peers.remove(&peer_addr_hash.0);
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
    inbound_tx: &mpsc::Sender<InboundMessage>,
) {
    loop {
        let (msg_type, data) = match read_message(&mut recv).await {
            Ok(m) => m,
            Err(e) => {
                debug!("Peer {} stream closed: {}", peer_address, e);
                break;
            }
        };

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
            other => {
                warn!("Unexpected message type {:?} from {}", other, peer_address);
            }
        }
    }
}
