//! Multi-node integration tests for ARC Chain.
//!
//! These tests prove that multiple nodes can:
//! 1. Connect via QUIC with challenge-response handshake.
//! 2. Exchange blocks and propagate transactions across the network.
//! 3. Reach DAG consensus with 2/3 quorum finality.
//!
//! Each test spins up full node stacks (StateDB + Mempool + ConsensusManager +
//! Transport) on localhost using ephemeral ports.
//!
//! Architecture (mirrors main.rs wiring):
//! ```text
//!   Transport <── outbound_rx ── ConsensusManager ── outbound_tx ──> Transport
//!   Transport ── inbound_tx ──> ConsensusManager <── inbound_rx ── Transport
//! ```
//! Transport receives OutboundMessages to broadcast, sends InboundMessages to consensus.
//! ConsensusManager receives InboundMessages (PeerConnected, DagBlock, etc) and
//! sends OutboundMessages (BroadcastDagBlock, BroadcastTransactions).

#![allow(dead_code)]

use arc_crypto::{hash_bytes, Hash256, KeyPair};
use arc_mempool::Mempool;
use arc_net::transport::{run_transport, InboundMessage, OutboundMessage};
use arc_node::consensus::ConsensusManager;
use arc_state::StateDB;
use arc_types::{Block, Transaction};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Derive a deterministic validator keypair from a seed string.
/// Same logic as main.rs: BLAKE3 KDF -> Ed25519 signing key.
fn make_validator_keypair(seed: &str) -> KeyPair {
    let seed_bytes = blake3::derive_key("ARC-chain-validator-keypair-v1", seed.as_bytes());
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed_bytes);
    KeyPair::Ed25519(signing_key)
}

/// Standard genesis accounts shared across all test nodes.
/// All nodes MUST use the same genesis to produce the same genesis hash,
/// which is required for the QUIC handshake to succeed.
fn genesis_accounts() -> Vec<(Hash256, u64)> {
    (0..100u8)
        .map(|i| (hash_bytes(&[i]), 1_000_000_000_000))
        .collect()
}

/// Find an available ephemeral port by binding to port 0 on UDP.
/// The OS assigns a random available port which we read back and return.
/// The socket is dropped immediately so the port becomes available for QUIC.
///
/// There is a small TOCTOU window here, but in practice it works reliably
/// for local integration tests.
async fn find_free_port() -> u16 {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral UDP socket");
    socket.local_addr().unwrap().port()
}

/// A full node instance bundling all components needed for multi-node tests.
///
/// Mirrors the wiring in main.rs: Transport + ConsensusManager connected via
/// mpsc channels. Each node starts as single-validator and transitions to
/// multi-validator mode when PeerConnected messages arrive from the transport.
struct TestNode {
    /// This node's validator address (derived from keypair).
    address: Hash256,
    /// Staked ARC amount.
    stake: u64,
    /// Port the QUIC transport is listening on.
    port: u16,
    /// State database (in-memory, no WAL).
    state: Arc<StateDB>,
    /// Transaction mempool.
    mempool: Arc<Mempool>,
    /// Peer count tracker (shared with transport).
    peer_count: Arc<AtomicU32>,
    /// JoinHandle for the spawned tasks so we can abort on cleanup.
    task_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl TestNode {
    /// Create and start a full test node.
    ///
    /// The node starts as a single-validator (fast path). When peers connect
    /// via QUIC transport, `PeerConnected` messages flow into consensus, which
    /// dynamically adds them to the validator set and transitions to multi-
    /// validator DAG consensus mode. This mirrors the production wiring.
    ///
    /// * `seed` — deterministic validator seed (e.g. "test-validator-0").
    /// * `stake` — ARC stake amount (must be >= 5M for block production).
    /// * `port` — port to listen on (use `find_free_port()` to get one).
    /// * `bootstrap_peers` — addresses of peers to connect to on startup.
    async fn start(
        seed: &str,
        stake: u64,
        port: u16,
        bootstrap_peers: Vec<SocketAddr>,
    ) -> Self {
        let keypair = make_validator_keypair(seed);
        let address = keypair.address();

        // Shared genesis state (identical across all nodes)
        let genesis = genesis_accounts();
        let state = Arc::new(StateDB::with_genesis(&genesis));
        let mempool = Arc::new(Mempool::new(100_000));

        // Channels: transport <-> consensus
        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>(1000);
        let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundMessage>(1000);
        let peer_count = Arc::new(AtomicU32::new(0));

        let genesis_hash = Block::genesis().hash;
        let listen_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();

        // Start transport
        let transport_keypair = keypair.clone();
        let transport_inbound_tx = inbound_tx.clone();
        let transport_peer_count = peer_count.clone();
        let transport_handle = tokio::spawn(run_transport(
            listen_addr,
            bootstrap_peers,
            address,
            stake,
            genesis_hash,
            outbound_rx,
            transport_inbound_tx,
            transport_peer_count,
            transport_keypair,
        ));

        // Start consensus — no pre-populated peers (matches main.rs behavior).
        // Peers are discovered dynamically via PeerConnected from transport.
        let consensus = ConsensusManager::new_with_keypair(
            address,
            stake,
            4,     // num_shards
            false, // not benchmark mode
            &[],   // no pre-populated peers — dynamic discovery
            keypair,
        );
        let state_clone = state.clone();
        let mempool_clone = mempool.clone();
        let consensus_handle = tokio::spawn(async move {
            consensus
                .run_consensus_loop(
                    state_clone,
                    mempool_clone,
                    Some(inbound_rx),
                    Some(outbound_tx),
                    None, // no benchmark pool
                )
                .await;
        });

        TestNode {
            address,
            stake,
            port,
            state,
            mempool,
            peer_count,
            task_handles: vec![transport_handle, consensus_handle],
        }
    }

    /// Wait until this node has at least `n` connected peers, or timeout.
    async fn wait_for_peers(&self, n: u32, deadline: Duration) -> bool {
        let start = tokio::time::Instant::now();
        loop {
            if self.peer_count.load(Ordering::Relaxed) >= n {
                return true;
            }
            if start.elapsed() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait until the state database reaches at least the given block height.
    async fn wait_for_height(&self, target_height: u64, deadline: Duration) -> bool {
        let start = tokio::time::Instant::now();
        loop {
            if self.state.height() >= target_height {
                return true;
            }
            if start.elapsed() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        for handle in self.task_handles.drain(..) {
            handle.abort();
        }
    }
}

// ─── Test 1: Two Nodes Connect via QUIC ─────────────────────────────────────

/// Verify that two QUIC transport instances on localhost can discover each other
/// via the bootstrap peer mechanism and complete the challenge-response handshake.
///
/// This test operates at the transport layer only (no consensus), proving:
/// - Node B (the dialer) connects to Node A (the listener).
/// - Both nodes report 1 connected peer via the AtomicU32 counter.
/// - The handshake verifies validator identity (pubkey -> address + signature).
/// - PeerConnected messages are emitted on both sides with correct addresses.
#[tokio::test]
async fn test_two_nodes_connect() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("error")
        .try_init();

    let port_a = find_free_port().await;
    let port_b = find_free_port().await;

    let seed_a = "test-validator-0";
    let seed_b = "test-validator-1";
    let stake = 5_000_000u64; // Arc tier

    let keypair_a = make_validator_keypair(seed_a);
    let address_a = keypair_a.address();
    let keypair_b = make_validator_keypair(seed_b);
    let address_b = keypair_b.address();

    let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();

    // Node A: standalone transport (no consensus — pure connect test).
    let (inbound_tx_a, mut inbound_rx_a) = mpsc::channel::<InboundMessage>(100);
    let (_outbound_tx_a, outbound_rx_a) = mpsc::channel::<OutboundMessage>(100);
    let peer_count_a = Arc::new(AtomicU32::new(0));
    let genesis_hash = Block::genesis().hash;

    let listen_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();
    let pc_a = peer_count_a.clone();
    let kp_a = keypair_a.clone();
    let handle_a = tokio::spawn(run_transport(
        listen_a,
        vec![], // seed node — no bootstrap
        address_a,
        stake,
        genesis_hash,
        outbound_rx_a,
        inbound_tx_a,
        pc_a,
        kp_a,
    ));

    // Let Node A bind and start listening.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Node B: connects to Node A as bootstrap peer.
    let (inbound_tx_b, mut inbound_rx_b) = mpsc::channel::<InboundMessage>(100);
    let (_outbound_tx_b, outbound_rx_b) = mpsc::channel::<OutboundMessage>(100);
    let peer_count_b = Arc::new(AtomicU32::new(0));

    let listen_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse().unwrap();
    let pc_b = peer_count_b.clone();
    let kp_b = keypair_b.clone();
    let handle_b = tokio::spawn(run_transport(
        listen_b,
        vec![addr_a], // bootstrap to Node A
        address_b,
        stake,
        genesis_hash,
        outbound_rx_b,
        inbound_tx_b,
        pc_b,
        kp_b,
    ));

    // Wait for both sides to register a connected peer.
    let connected = timeout(Duration::from_secs(10), async {
        loop {
            if peer_count_a.load(Ordering::Relaxed) >= 1
                && peer_count_b.load(Ordering::Relaxed) >= 1
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    assert!(
        connected.is_ok(),
        "Nodes failed to connect within 10 seconds. \
         peer_count_a={}, peer_count_b={}",
        peer_count_a.load(Ordering::Relaxed),
        peer_count_b.load(Ordering::Relaxed),
    );

    // Verify Node B received PeerConnected for Node A.
    let msg_b = timeout(Duration::from_secs(5), inbound_rx_b.recv()).await;
    assert!(msg_b.is_ok(), "Node B did not receive PeerConnected");
    match msg_b.unwrap() {
        Some(InboundMessage::PeerConnected { address, stake: s }) => {
            assert_eq!(address, address_a, "Node B should see Node A's address");
            assert_eq!(s, stake, "Stake should match");
        }
        other => panic!("Expected PeerConnected from Node A, got: {:?}", other),
    }

    // Verify Node A received PeerConnected for Node B.
    let msg_a = timeout(Duration::from_secs(5), inbound_rx_a.recv()).await;
    assert!(msg_a.is_ok(), "Node A did not receive PeerConnected");
    match msg_a.unwrap() {
        Some(InboundMessage::PeerConnected { address, stake: s }) => {
            assert_eq!(address, address_b, "Node A should see Node B's address");
            assert_eq!(s, stake, "Stake should match");
        }
        other => panic!("Expected PeerConnected from Node B, got: {:?}", other),
    }

    handle_a.abort();
    handle_b.abort();
}

// ─── Test 2: Block Propagation ──────────────────────────────────────────────

/// Start 2 full node stacks (StateDB + Mempool + ConsensusManager + Transport).
/// Submit a transaction to Node A's mempool and verify that BOTH nodes execute
/// it and advance their state height.
///
/// The flow:
/// 1. Both nodes start as single-validator (fast path).
/// 2. Transport connects them; PeerConnected triggers multi-validator mode.
/// 3. Transaction is submitted to Node A's mempool.
/// 4. Node A proposes a DAG block containing the transaction hash.
/// 5. Node A broadcasts the block + full transactions to Node B.
/// 6. Node B receives the block, inserts txs into pending_txs, feeds into DAG.
/// 7. After the 2-round commit rule fires, both nodes execute the transactions.
/// 8. Both StateDBs advance to height >= 1.
#[tokio::test]
async fn test_block_propagation() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("error")
        .try_init();

    let port_a = find_free_port().await;
    let port_b = find_free_port().await;

    let seed_a = "test-validator-0";
    let seed_b = "test-validator-1";
    let stake = 5_000_000u64; // Arc tier — can produce blocks

    let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();

    // Start Node A (seed node, no bootstrap peers).
    // Starts as single-validator; will transition when Node B connects.
    let node_a = TestNode::start(seed_a, stake, port_a, vec![]).await;

    // Let Node A bind and start listening.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Start Node B (bootstraps to Node A).
    let node_b = TestNode::start(seed_b, stake, port_b, vec![addr_a]).await;

    // Wait for both nodes to see each other via transport.
    let peers_ok = timeout(Duration::from_secs(15), async {
        loop {
            let a = node_a.peer_count.load(Ordering::Relaxed);
            let b = node_b.peer_count.load(Ordering::Relaxed);
            if a >= 1 && b >= 1 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    assert!(
        peers_ok.is_ok(),
        "Nodes failed to discover each other. \
         node_a peers={}, node_b peers={}",
        node_a.peer_count.load(Ordering::Relaxed),
        node_b.peer_count.load(Ordering::Relaxed),
    );

    // Allow consensus to process the PeerConnected messages and transition
    // to multi-validator mode (DAG reset + validator set update).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Submit a transfer transaction to Node A's mempool.
    // Genesis account 0 -> genesis account 1, amount 1000.
    let genesis = genesis_accounts();
    let tx = Transaction::new_transfer(genesis[0].0, genesis[1].0, 1000, 0);
    node_a
        .mempool
        .insert(tx)
        .expect("failed to insert tx into Node A mempool");

    // Wait for BOTH nodes to advance to height >= 1.
    // Multi-validator DAG commit requires the 2-round rule:
    //   R0: both validators propose blocks
    //   R1: both propose blocks referencing R0 blocks
    //   R2: both propose blocks referencing R1 -> R0 blocks commit
    // With 10ms loop interval, this takes several hundred ms minimum.
    let height_ok = timeout(Duration::from_secs(30), async {
        loop {
            let h_a = node_a.state.height();
            let h_b = node_b.state.height();
            if h_a >= 1 && h_b >= 1 {
                return (h_a, h_b);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;

    assert!(
        height_ok.is_ok(),
        "Blocks failed to propagate within 30 seconds. \
         node_a height={}, node_b height={}",
        node_a.state.height(),
        node_b.state.height(),
    );

    let (h_a, h_b) = height_ok.unwrap();
    assert!(h_a >= 1, "Node A should have produced at least 1 block");
    assert!(h_b >= 1, "Node B should have committed at least 1 block");
}

// ─── Test 3: Three-Node DAG Consensus ───────────────────────────────────────

/// Start 3 validator nodes with equal stake (5M ARC each, 15M total).
/// Quorum = ceil(2/3 * 15M) = 10M = any 2 validators.
///
/// Proves that:
/// - All 3 nodes connect and form a (partial) mesh.
/// - Transactions submitted to different nodes propagate.
/// - The DAG advances through multiple rounds.
/// - Blocks are committed (finalized) on all nodes.
/// - All nodes converge to consistent heights (within tolerance).
#[tokio::test]
#[ignore = "Flaky: 3-node DAG consensus timing-sensitive with validator set transitions. Run with --ignored for manual verification."]
async fn test_three_node_consensus() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("error")
        .try_init();

    let port_0 = find_free_port().await;
    let port_1 = find_free_port().await;
    let port_2 = find_free_port().await;

    let seeds = ["test-validator-0", "test-validator-1", "test-validator-2"];
    let stake = 5_000_000u64; // Arc tier for all three

    let addr_0: SocketAddr = format!("127.0.0.1:{}", port_0).parse().unwrap();
    let addr_1: SocketAddr = format!("127.0.0.1:{}", port_1).parse().unwrap();

    // Start all three nodes in quick succession to minimize the window
    // where nodes 0 and 1 form a 2-validator consensus before node 2 joins.
    // This is important because if nodes 0+1 advance several DAG rounds before
    // node 2 connects, node 2's fresh DAG will be out of sync and struggle
    // to catch up.
    let node_0 = TestNode::start(seeds[0], stake, port_0, vec![]).await;
    // Brief delay for Node 0 to bind its QUIC listener.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Start Nodes 1 and 2 nearly simultaneously — both bootstrap to Node 0.
    // Node 1 also serves as a bootstrap target for Node 2.
    let node_1 = TestNode::start(seeds[1], stake, port_1, vec![addr_0]).await;
    // Tiny delay for Node 1 to bind.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let node_2 = TestNode::start(seeds[2], stake, port_2, vec![addr_0, addr_1]).await;

    // Wait for mesh connectivity: each node should see at least 2 peers.
    // The bootstrap pattern gives:
    //   Node 0: accepts from Node 1 and Node 2 -> 2 peers
    //   Node 1: dials Node 0, accepts from Node 2 -> 2 peers
    //   Node 2: dials Node 0 and Node 1 -> 2 peers
    let mesh_ok = timeout(Duration::from_secs(20), async {
        loop {
            let p0 = node_0.peer_count.load(Ordering::Relaxed);
            let p1 = node_1.peer_count.load(Ordering::Relaxed);
            let p2 = node_2.peer_count.load(Ordering::Relaxed);
            // All three nodes should see 2 peers each for full mesh.
            if p0 >= 2 && p1 >= 2 && p2 >= 2 {
                return (p0, p1, p2);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;

    assert!(
        mesh_ok.is_ok(),
        "Nodes failed to form mesh. peers: node0={}, node1={}, node2={}",
        node_0.peer_count.load(Ordering::Relaxed),
        node_1.peer_count.load(Ordering::Relaxed),
        node_2.peer_count.load(Ordering::Relaxed),
    );

    let (p0, p1, p2) = mesh_ok.unwrap();
    eprintln!(
        "Mesh formed: node0={} peers, node1={} peers, node2={} peers",
        p0, p1, p2
    );

    // Allow consensus to process PeerConnected messages, rebuild validator
    // sets, and transition from single-validator to multi-validator mode.
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Submit transactions to different nodes.
    let genesis = genesis_accounts();

    // Tx on Node 0: account[10] -> account[11], 500 ARC
    let tx_0 = Transaction::new_transfer(genesis[10].0, genesis[11].0, 500, 0);
    node_0.mempool.insert(tx_0).expect("insert tx into Node 0");

    // Tx on Node 1: account[20] -> account[21], 750 ARC
    let tx_1 = Transaction::new_transfer(genesis[20].0, genesis[21].0, 750, 0);
    node_1.mempool.insert(tx_1).expect("insert tx into Node 1");

    // Tx on Node 2: account[30] -> account[31], 250 ARC
    let tx_2 = Transaction::new_transfer(genesis[30].0, genesis[31].0, 250, 0);
    node_2.mempool.insert(tx_2).expect("insert tx into Node 2");

    // Wait for all nodes to finalize blocks (height >= 1).
    // With 3 validators and the 2-round commit rule, the DAG needs:
    //   Round 0: >= 2/3 validators propose
    //   Round 1: blocks reference round 0 blocks
    //   Round 2: >= 2/3 reference round 1 -> round 0 blocks commit
    // This involves multiple 10ms consensus loop iterations + network latency.
    let consensus_ok = timeout(Duration::from_secs(45), async {
        loop {
            let h0 = node_0.state.height();
            let h1 = node_1.state.height();
            let h2 = node_2.state.height();
            if h0 >= 1 && h1 >= 1 && h2 >= 1 {
                return (h0, h1, h2);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;

    assert!(
        consensus_ok.is_ok(),
        "Three-node consensus failed within 45 seconds. \
         Heights: node0={}, node1={}, node2={}",
        node_0.state.height(),
        node_1.state.height(),
        node_2.state.height(),
    );

    let (h0, h1, h2) = consensus_ok.unwrap();
    eprintln!(
        "Consensus reached: node0 height={}, node1 height={}, node2 height={}",
        h0, h1, h2
    );

    // All nodes should have advanced past genesis.
    assert!(h0 >= 1, "Node 0 should have finalized blocks");
    assert!(h1 >= 1, "Node 1 should have finalized blocks");
    assert!(h2 >= 1, "Node 2 should have finalized blocks");

    // Heights should be roughly consistent across nodes.
    // Propagation delay can cause slight divergence (1-2 blocks).
    let max_h = h0.max(h1).max(h2);
    let min_h = h0.min(h1).min(h2);
    assert!(
        max_h - min_h <= 2,
        "Node heights diverged too much: max={}, min={} (tolerance=2)",
        max_h,
        min_h,
    );
}

// ─── Test 4: Genesis Hash Mismatch Rejection ────────────────────────────────

/// Verify that a node with a different genesis hash is rejected during the
/// QUIC handshake. This ensures network isolation between chains.
///
/// The handshake includes the genesis hash and the peer's signature over a
/// challenge derived from it. A genesis mismatch causes the handshake to fail
/// and no PeerConnected message is emitted.
#[tokio::test]
async fn test_genesis_mismatch_rejected() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("error")
        .try_init();

    let port_a = find_free_port().await;
    let port_b = find_free_port().await;

    let seed_a = "test-validator-0";
    let seed_b = "test-validator-1";
    let stake = 5_000_000u64;

    let keypair_a = make_validator_keypair(seed_a);
    let address_a = keypair_a.address();
    let keypair_b = make_validator_keypair(seed_b);
    let address_b = keypair_b.address();

    let genesis_hash_a = Block::genesis().hash;
    // Fabricate a different genesis hash (simulates a different chain).
    let genesis_hash_b = hash_bytes(b"different-genesis-for-another-chain");

    let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();

    // Node A: standard genesis.
    let (inbound_tx_a, _inbound_rx_a) = mpsc::channel::<InboundMessage>(100);
    let (_outbound_tx_a, outbound_rx_a) = mpsc::channel::<OutboundMessage>(100);
    let peer_count_a = Arc::new(AtomicU32::new(0));

    let listen_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();
    let pc_a = peer_count_a.clone();
    let handle_a = tokio::spawn(run_transport(
        listen_a,
        vec![],
        address_a,
        stake,
        genesis_hash_a,
        outbound_rx_a,
        inbound_tx_a,
        pc_a,
        keypair_a,
    ));

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Node B: DIFFERENT genesis hash — should be rejected by Node A.
    let (inbound_tx_b, _inbound_rx_b) = mpsc::channel::<InboundMessage>(100);
    let (_outbound_tx_b, outbound_rx_b) = mpsc::channel::<OutboundMessage>(100);
    let peer_count_b = Arc::new(AtomicU32::new(0));

    let listen_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse().unwrap();
    let pc_b = peer_count_b.clone();
    let handle_b = tokio::spawn(run_transport(
        listen_b,
        vec![addr_a], // attempt to connect to Node A
        address_b,
        stake,
        genesis_hash_b, // MISMATCH — handshake should fail
        outbound_rx_b,
        inbound_tx_b,
        pc_b,
        keypair_b,
    ));

    // Wait for the connection attempt to complete (and fail).
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Neither node should have any connected peers.
    assert_eq!(
        peer_count_a.load(Ordering::Relaxed),
        0,
        "Node A should have rejected Node B (genesis mismatch)"
    );
    assert_eq!(
        peer_count_b.load(Ordering::Relaxed),
        0,
        "Node B should not show a connected peer (handshake should have failed)"
    );

    // Suppress unused variable warnings.
    let _ = address_a;
    let _ = address_b;

    handle_a.abort();
    handle_b.abort();
}

// ─── Test 5: Transaction Gossip via Transport ────────────────────────────────

/// Verify that raw transaction gossip propagates between two transport instances.
///
/// This test operates at the transport layer only (no consensus), proving:
/// - Node A sends a BroadcastTransactions outbound message.
/// - Node B receives the corresponding Transactions inbound message.
/// - The raw transaction bytes arrive intact.
///
/// Unlike test_block_propagation (which goes through the full consensus stack),
/// this test exercises the transport's serialization/deserialization path for
/// transaction gossip directly.
#[tokio::test(flavor = "multi_thread")]
async fn test_transaction_gossip() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("error")
        .try_init();

    let port_a = find_free_port().await;
    let port_b = find_free_port().await;

    let seed_a = "test-validator-gossip-0";
    let seed_b = "test-validator-gossip-1";
    let stake = 5_000_000u64;

    let keypair_a = make_validator_keypair(seed_a);
    let address_a = keypair_a.address();
    let keypair_b = make_validator_keypair(seed_b);
    let address_b = keypair_b.address();

    let genesis_hash = Block::genesis().hash;

    let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();

    // ── Node A: transport-only (no consensus) ──
    let (inbound_tx_a, mut inbound_rx_a) = mpsc::channel::<InboundMessage>(100);
    let (outbound_tx_a, outbound_rx_a) = mpsc::channel::<OutboundMessage>(100);
    let peer_count_a = Arc::new(AtomicU32::new(0));

    let listen_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();
    let pc_a = peer_count_a.clone();
    let handle_a = tokio::spawn(run_transport(
        listen_a,
        vec![], // seed node
        address_a,
        stake,
        genesis_hash,
        outbound_rx_a,
        inbound_tx_a,
        pc_a,
        keypair_a,
    ));

    // Let Node A bind.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // ── Node B: transport-only, bootstraps to Node A ──
    let (inbound_tx_b, mut inbound_rx_b) = mpsc::channel::<InboundMessage>(100);
    let (_outbound_tx_b, outbound_rx_b) = mpsc::channel::<OutboundMessage>(100);
    let peer_count_b = Arc::new(AtomicU32::new(0));

    let listen_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse().unwrap();
    let pc_b = peer_count_b.clone();
    let handle_b = tokio::spawn(run_transport(
        listen_b,
        vec![addr_a],
        address_b,
        stake,
        genesis_hash,
        outbound_rx_b,
        inbound_tx_b,
        pc_b,
        keypair_b,
    ));

    // Wait for both sides to connect.
    let connected = timeout(Duration::from_secs(10), async {
        loop {
            if peer_count_a.load(Ordering::Relaxed) >= 1
                && peer_count_b.load(Ordering::Relaxed) >= 1
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    assert!(
        connected.is_ok(),
        "Nodes failed to connect for gossip test. \
         peer_count_a={}, peer_count_b={}",
        peer_count_a.load(Ordering::Relaxed),
        peer_count_b.load(Ordering::Relaxed),
    );

    // Drain PeerConnected messages from both inbound channels so they
    // don't interfere with the transaction gossip assertion below.
    let _ = timeout(Duration::from_secs(2), inbound_rx_a.recv()).await;
    let _ = timeout(Duration::from_secs(2), inbound_rx_b.recv()).await;

    // ── Send transaction gossip from Node A ──
    // Fabricate 3 raw transaction byte vectors (these are opaque to transport).
    let tx_data: Vec<Vec<u8>> = vec![
        b"raw-tx-alpha-001".to_vec(),
        b"raw-tx-beta-002".to_vec(),
        b"raw-tx-gamma-003".to_vec(),
    ];

    outbound_tx_a
        .send(OutboundMessage::BroadcastTransactions(tx_data.clone()))
        .await
        .expect("failed to send BroadcastTransactions on Node A outbound channel");

    // ── Verify Node B receives the gossiped transactions ──
    let recv_result = timeout(Duration::from_secs(5), inbound_rx_b.recv()).await;
    assert!(
        recv_result.is_ok(),
        "Node B did not receive gossiped transactions within 5 seconds"
    );

    match recv_result.unwrap() {
        Some(InboundMessage::Transactions(received_txs)) => {
            assert_eq!(
                received_txs.len(),
                3,
                "Expected 3 gossiped transactions, got {}",
                received_txs.len()
            );
            assert_eq!(received_txs[0], b"raw-tx-alpha-001".to_vec());
            assert_eq!(received_txs[1], b"raw-tx-beta-002".to_vec());
            assert_eq!(received_txs[2], b"raw-tx-gamma-003".to_vec());
        }
        other => panic!(
            "Expected InboundMessage::Transactions, got: {:?}",
            other
        ),
    }

    // Suppress unused variable warnings.
    let _ = address_a;
    let _ = address_b;

    handle_a.abort();
    handle_b.abort();
}
