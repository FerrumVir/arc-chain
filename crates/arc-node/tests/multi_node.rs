#![allow(dead_code)]

use arc_crypto::{Hash256, KeyPair, hash_bytes};
use arc_mempool::Mempool;
use arc_net::transport::{InboundMessage, OutboundMessage, run_transport};
use arc_node::consensus::ConsensusManager;
use arc_state::StateDB;
use arc_types::{Block, Transaction};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

fn init_tracing() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "error".to_string());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn make_validator_keypair(seed: &str) -> KeyPair {
    let seed_bytes = blake3::derive_key("ARC-chain-validator-keypair-v1", seed.as_bytes());
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed_bytes);
    KeyPair::Ed25519(signing_key)
}

fn validator_allowlist(addresses: &[Hash256]) -> Arc<std::collections::HashSet<[u8; 32]>> {
    Arc::new(addresses.iter().map(|address| address.0).collect())
}

fn genesis_keypair(index: u8) -> KeyPair {
    make_validator_keypair(&format!("test-genesis-account-{index}"))
}

fn genesis_accounts() -> Vec<(Hash256, u64)> {
    (0..100u8)
        .map(|i| (genesis_keypair(i).address(), 1_000_000_000_000))
        .collect()
}

async fn find_free_port() -> u16 {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral UDP socket");
    socket.local_addr().unwrap().port()
}

struct TestNode {
    address: Hash256,
    stake: u64,
    port: u16,
    state: Arc<StateDB>,
    mempool: Arc<Mempool>,
    peer_count: Arc<AtomicU32>,
    task_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl TestNode {
    async fn start(
        seed: &str,
        stake: u64,
        port: u16,
        bootstrap_peers: Vec<SocketAddr>,
        peer_validators: Vec<(Hash256, u64)>,
    ) -> Self {
        let keypair = make_validator_keypair(seed);
        let address = keypair.address();

        let genesis = genesis_accounts();
        let state = Arc::new(StateDB::with_genesis(&genesis));
        let mempool = Arc::new(Mempool::new(100_000));

        let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>(1000);
        let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundMessage>(1000);
        let peer_count = Arc::new(AtomicU32::new(0));

        let genesis_hash = Block::genesis().hash;
        let listen_addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        let mut transport_validators = vec![address];
        transport_validators.extend(peer_validators.iter().map(|(peer, _)| *peer));
        let transport_allowlist = validator_allowlist(&transport_validators);

        let transport_keypair = keypair.clone();
        let transport_inbound_tx = inbound_tx.clone();
        let transport_peer_count = peer_count.clone();
        let transport_handle = tokio::spawn(run_transport(
            listen_addr,
            bootstrap_peers,
            address,
            stake,
            genesis_hash,
            transport_allowlist,
            outbound_rx,
            transport_inbound_tx,
            transport_peer_count,
            transport_keypair,
            String::new(),
        ));

        let consensus =
            ConsensusManager::new_with_keypair(address, stake, 4, false, &peer_validators, keypair);
        let state_clone = state.clone();
        let mempool_clone = mempool.clone();
        let consensus_handle = tokio::spawn(async move {
            consensus
                .run_consensus_loop(
                    state_clone,
                    mempool_clone,
                    Some(inbound_rx),
                    Some(outbound_tx),
                    None,
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

#[tokio::test]
async fn test_two_nodes_connect() {
    init_tracing();

    let port_a = find_free_port().await;
    let port_b = find_free_port().await;

    let seed_a = "test-validator-0";
    let seed_b = "test-validator-1";
    let stake = 5_000_000u64;

    let keypair_a = make_validator_keypair(seed_a);
    let address_a = keypair_a.address();
    let keypair_b = make_validator_keypair(seed_b);
    let address_b = keypair_b.address();

    let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();

    let (inbound_tx_a, mut inbound_rx_a) = mpsc::channel::<InboundMessage>(100);
    let (_outbound_tx_a, outbound_rx_a) = mpsc::channel::<OutboundMessage>(100);
    let peer_count_a = Arc::new(AtomicU32::new(0));
    let genesis_hash = Block::genesis().hash;

    let listen_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();
    let pc_a = peer_count_a.clone();
    let kp_a = keypair_a.clone();
    let handle_a = tokio::spawn(run_transport(
        listen_a,
        vec![],
        address_a,
        stake,
        genesis_hash,
        validator_allowlist(&[address_a, address_b]),
        outbound_rx_a,
        inbound_tx_a,
        pc_a,
        kp_a,
        String::new(),
    ));

    tokio::time::sleep(Duration::from_millis(200)).await;

    let (inbound_tx_b, mut inbound_rx_b) = mpsc::channel::<InboundMessage>(100);
    let (_outbound_tx_b, outbound_rx_b) = mpsc::channel::<OutboundMessage>(100);
    let peer_count_b = Arc::new(AtomicU32::new(0));

    let listen_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse().unwrap();
    let pc_b = peer_count_b.clone();
    let kp_b = keypair_b.clone();
    let handle_b = tokio::spawn(run_transport(
        listen_b,
        vec![addr_a],
        address_b,
        stake,
        genesis_hash,
        validator_allowlist(&[address_a, address_b]),
        outbound_rx_b,
        inbound_tx_b,
        pc_b,
        kp_b,
        String::new(),
    ));

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

    let msg_b = timeout(Duration::from_secs(5), inbound_rx_b.recv()).await;
    assert!(msg_b.is_ok(), "Node B did not receive PeerConnected");
    match msg_b.unwrap() {
        Some(InboundMessage::PeerConnected {
            address, stake: s, ..
        }) => {
            assert_eq!(address, address_a, "Node B should see Node A's address");
            assert_eq!(s, stake, "Stake should match");
        }
        other => panic!("Expected PeerConnected from Node A, got: {:?}", other),
    }

    let msg_a = timeout(Duration::from_secs(5), inbound_rx_a.recv()).await;
    assert!(msg_a.is_ok(), "Node A did not receive PeerConnected");
    match msg_a.unwrap() {
        Some(InboundMessage::PeerConnected {
            address, stake: s, ..
        }) => {
            assert_eq!(address, address_b, "Node A should see Node B's address");
            assert_eq!(s, stake, "Stake should match");
        }
        other => panic!("Expected PeerConnected from Node B, got: {:?}", other),
    }

    handle_a.abort();
    handle_b.abort();
}

#[tokio::test]
async fn test_unknown_validator_is_rejected_after_authentication() {
    init_tracing();

    let port_a = find_free_port().await;
    let port_c = find_free_port().await;
    let stake = 5_000_000u64;
    let keypair_a = make_validator_keypair("allowlisted-validator-a");
    let keypair_b = make_validator_keypair("allowlisted-validator-b");
    let keypair_c = make_validator_keypair("unknown-validator-c");
    let address_a = keypair_a.address();
    let address_b = keypair_b.address();
    let address_c = keypair_c.address();
    let genesis_hash = Block::genesis().hash;

    let (inbound_tx_a, mut inbound_rx_a) = mpsc::channel::<InboundMessage>(16);
    let (_outbound_tx_a, outbound_rx_a) = mpsc::channel::<OutboundMessage>(16);
    let peer_count_a = Arc::new(AtomicU32::new(0));
    let handle_a = tokio::spawn(run_transport(
        format!("127.0.0.1:{port_a}").parse().unwrap(),
        vec![],
        address_a,
        stake,
        genesis_hash,
        validator_allowlist(&[address_a, address_b]),
        outbound_rx_a,
        inbound_tx_a,
        peer_count_a.clone(),
        keypair_a,
        String::new(),
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let (inbound_tx_c, mut inbound_rx_c) = mpsc::channel::<InboundMessage>(16);
    let (_outbound_tx_c, outbound_rx_c) = mpsc::channel::<OutboundMessage>(16);
    let peer_count_c = Arc::new(AtomicU32::new(0));
    let handle_c = tokio::spawn(run_transport(
        format!("127.0.0.1:{port_c}").parse().unwrap(),
        vec![format!("127.0.0.1:{port_a}").parse().unwrap()],
        address_c,
        stake,
        genesis_hash,
        validator_allowlist(&[address_a, address_c]),
        outbound_rx_c,
        inbound_tx_c,
        peer_count_c.clone(),
        keypair_c,
        String::new(),
    ));

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(peer_count_a.load(Ordering::Relaxed), 0);
    assert_eq!(peer_count_c.load(Ordering::Relaxed), 0);
    assert!(
        timeout(Duration::from_millis(250), inbound_rx_a.recv())
            .await
            .is_err(),
        "allowlisted validator must not emit PeerConnected for an unknown identity"
    );
    assert!(
        timeout(Duration::from_millis(250), inbound_rx_c.recv())
            .await
            .is_err(),
        "unknown node must not observe a successful validator connection"
    );

    handle_a.abort();
    handle_c.abort();
}

#[tokio::test]
async fn test_block_propagation() {
    init_tracing();

    let port_a = find_free_port().await;
    let port_b = find_free_port().await;

    let seed_a = "test-validator-0";
    let seed_b = "test-validator-1";
    let stake = 5_000_000u64;

    let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();

    let address_a = make_validator_keypair(seed_a).address();
    let address_b = make_validator_keypair(seed_b).address();
    let node_a = TestNode::start(seed_a, stake, port_a, vec![], vec![(address_b, stake)]).await;

    tokio::time::sleep(Duration::from_millis(300)).await;

    let node_b = TestNode::start(
        seed_b,
        stake,
        port_b,
        vec![addr_a],
        vec![(address_a, stake)],
    )
    .await;

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

    tokio::time::sleep(Duration::from_millis(500)).await;

    let genesis = genesis_accounts();
    let sender = genesis_keypair(0);
    let mut tx = Transaction::new_transfer(sender.address(), genesis[1].0, 1000, 0);
    tx.sign(&sender).expect("sign block-propagation transfer");
    node_a
        .mempool
        .insert(tx)
        .expect("failed to insert tx into Node A mempool");

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
    let block_a = node_a.state.get_block(1).expect("node A block 1");
    let block_b = node_b.state.get_block(1).expect("node B block 1");
    assert_eq!(
        block_a.header.state_root, block_b.header.state_root,
        "validators must derive one state root for the first committed DAG block"
    );
    assert_eq!(
        block_a.hash, block_b.hash,
        "consensus-provided timestamps must make the linear block hash deterministic"
    );
}

#[tokio::test]
async fn test_three_node_consensus() {
    init_tracing();

    let port_0 = find_free_port().await;
    let port_1 = find_free_port().await;
    let port_2 = find_free_port().await;

    let seeds = ["test-validator-0", "test-validator-1", "test-validator-2"];
    let stake = 5_000_000u64;

    let addr_0: SocketAddr = format!("127.0.0.1:{}", port_0).parse().unwrap();
    let addr_1: SocketAddr = format!("127.0.0.1:{}", port_1).parse().unwrap();

    let validator_addresses: Vec<Hash256> = seeds
        .iter()
        .map(|seed| make_validator_keypair(seed).address())
        .collect();
    let peers_for = |local: usize| {
        validator_addresses
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != local)
            .map(|(_, address)| (*address, stake))
            .collect::<Vec<_>>()
    };
    let node_0 = TestNode::start(seeds[0], stake, port_0, vec![], peers_for(0)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let node_1 = TestNode::start(seeds[1], stake, port_1, vec![addr_0], peers_for(1)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let node_2 = TestNode::start(seeds[2], stake, port_2, vec![addr_0, addr_1], peers_for(2)).await;

    let mesh_ok = timeout(Duration::from_secs(20), async {
        loop {
            let p0 = node_0.peer_count.load(Ordering::Relaxed);
            let p1 = node_1.peer_count.load(Ordering::Relaxed);
            let p2 = node_2.peer_count.load(Ordering::Relaxed);
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

    tokio::time::sleep(Duration::from_millis(1000)).await;

    let genesis = genesis_accounts();

    let sender_0 = genesis_keypair(10);
    let mut tx_0 = Transaction::new_transfer(sender_0.address(), genesis[11].0, 500, 0);
    tx_0.sign(&sender_0).expect("sign node-0 transfer");
    node_0.mempool.insert(tx_0).expect("insert tx into Node 0");

    let sender_1 = genesis_keypair(20);
    let mut tx_1 = Transaction::new_transfer(sender_1.address(), genesis[21].0, 750, 0);
    tx_1.sign(&sender_1).expect("sign node-1 transfer");
    node_1.mempool.insert(tx_1).expect("insert tx into Node 1");

    let sender_2 = genesis_keypair(30);
    let mut tx_2 = Transaction::new_transfer(sender_2.address(), genesis[31].0, 250, 0);
    tx_2.sign(&sender_2).expect("sign node-2 transfer");
    node_2.mempool.insert(tx_2).expect("insert tx into Node 2");

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

    assert!(h0 >= 1, "Node 0 should have finalized blocks");
    assert!(h1 >= 1, "Node 1 should have finalized blocks");
    assert!(h2 >= 1, "Node 2 should have finalized blocks");

    let block_0 = node_0.state.get_block(1).expect("node 0 block 1");
    let block_1 = node_1.state.get_block(1).expect("node 1 block 1");
    let block_2 = node_2.state.get_block(1).expect("node 2 block 1");
    assert_eq!(block_0.header.state_root, block_1.header.state_root);
    assert_eq!(block_0.header.state_root, block_2.header.state_root);
    assert_eq!(block_0.hash, block_1.hash);
    assert_eq!(block_0.hash, block_2.hash);

    let max_h = h0.max(h1).max(h2);
    let min_h = h0.min(h1).min(h2);
    assert!(
        max_h - min_h <= 2,
        "Node heights diverged too much: max={}, min={} (tolerance=2)",
        max_h,
        min_h,
    );
}

#[tokio::test]
async fn test_genesis_mismatch_rejected() {
    init_tracing();

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
    let genesis_hash_b = hash_bytes(b"different-genesis-for-another-chain");

    let addr_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();

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
        validator_allowlist(&[address_a, address_b]),
        outbound_rx_a,
        inbound_tx_a,
        pc_a,
        keypair_a,
        String::new(),
    ));

    tokio::time::sleep(Duration::from_millis(200)).await;

    let (inbound_tx_b, _inbound_rx_b) = mpsc::channel::<InboundMessage>(100);
    let (_outbound_tx_b, outbound_rx_b) = mpsc::channel::<OutboundMessage>(100);
    let peer_count_b = Arc::new(AtomicU32::new(0));

    let listen_b: SocketAddr = format!("127.0.0.1:{}", port_b).parse().unwrap();
    let pc_b = peer_count_b.clone();
    let handle_b = tokio::spawn(run_transport(
        listen_b,
        vec![addr_a],
        address_b,
        stake,
        genesis_hash_b,
        validator_allowlist(&[address_a, address_b]),
        outbound_rx_b,
        inbound_tx_b,
        pc_b,
        keypair_b,
        String::new(),
    ));

    tokio::time::sleep(Duration::from_secs(3)).await;

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

    let _ = address_a;
    let _ = address_b;

    handle_a.abort();
    handle_b.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_transaction_gossip() {
    init_tracing();

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

    let (inbound_tx_a, mut inbound_rx_a) = mpsc::channel::<InboundMessage>(100);
    let (outbound_tx_a, outbound_rx_a) = mpsc::channel::<OutboundMessage>(100);
    let peer_count_a = Arc::new(AtomicU32::new(0));

    let listen_a: SocketAddr = format!("127.0.0.1:{}", port_a).parse().unwrap();
    let pc_a = peer_count_a.clone();
    let handle_a = tokio::spawn(run_transport(
        listen_a,
        vec![],
        address_a,
        stake,
        genesis_hash,
        validator_allowlist(&[address_a, address_b]),
        outbound_rx_a,
        inbound_tx_a,
        pc_a,
        keypair_a,
        String::new(),
    ));

    tokio::time::sleep(Duration::from_millis(200)).await;

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
        validator_allowlist(&[address_a, address_b]),
        outbound_rx_b,
        inbound_tx_b,
        pc_b,
        keypair_b,
        String::new(),
    ));

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

    let _ = timeout(Duration::from_secs(2), inbound_rx_a.recv()).await;
    let _ = timeout(Duration::from_secs(2), inbound_rx_b.recv()).await;

    let tx_data: Vec<Vec<u8>> = vec![
        b"raw-tx-alpha-001".to_vec(),
        b"raw-tx-beta-002".to_vec(),
        b"raw-tx-gamma-003".to_vec(),
    ];

    outbound_tx_a
        .send(OutboundMessage::BroadcastTransactions(tx_data.clone()))
        .await
        .expect("failed to send BroadcastTransactions on Node A outbound channel");

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
        other => panic!("Expected InboundMessage::Transactions, got: {:?}", other),
    }

    let _ = address_a;
    let _ = address_b;

    handle_a.abort();
    handle_b.abort();
}
