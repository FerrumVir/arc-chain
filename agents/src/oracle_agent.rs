//! Price Oracle Agent — On-chain AI agent that attests to price data.
//!
//! Demonstrates:
//! - Fetching external price data (simulated)
//! - Attesting to prices on-chain via InferenceAttestation TX (Tier 2)
//! - Serving price feeds through the oracle precompile (0x04)
//! - Agent lifecycle management (registration, activation, execution)
//!
//! # Architecture
//!
//! ```text
//! ┌───────────────┐    RegisterAgent TX     ┌──────────────┐
//! │  Price Oracle  │ ──────────────────────> │  ARC Chain   │
//! │     Agent      │    Price Update         │              │
//! │                │ ──────────────────────> │  Oracle      │
//! │  (simulated    │    InferenceAttestation │  Precompile  │
//! │   feeds)       │ ──────────────────────> │  0x04        │
//! └───────────────┘                         └──────────────┘
//! ```

use arc_crypto::{hash_bytes, Hash256};
use arc_crypto::signature::Signature;
use arc_types::transaction::{
    InferenceAttestationBody, RegisterBody, Transaction, TxBody, TxType,
};
use arc_vm::agent::{
    Agent, AgentAction, AgentConfig, AgentId, AgentRegistry, AgentState, ActionResult, ActionType,
};
use arc_vm::precompiles::{OracleRegistry, PriceFeed};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const AGENT_NAME: &str = "price-oracle-v1";

/// Simulated token addresses for price feeds.
fn token_addresses() -> Vec<([u8; 32], &'static str, u128)> {
    vec![
        (hash_bytes(b"ARC").0,  "ARC",  2_500_000_000_000_000_000),    // $2.50
        (hash_bytes(b"ETH").0,  "ETH",  3_200_000_000_000_000_000_000), // $3,200
        (hash_bytes(b"BTC").0,  "BTC",  67_500_000_000_000_000_000_000), // $67,500
        (hash_bytes(b"USDC").0, "USDC", 1_000_000_000_000_000_000),    // $1.00
        (hash_bytes(b"SOL").0,  "SOL",  145_000_000_000_000_000_000),  // $145
    ]
}

// ---------------------------------------------------------------------------
// Transaction helpers
// ---------------------------------------------------------------------------

/// Construct a RegisterAgent transaction for the oracle agent.
fn build_register_tx(owner: Hash256, nonce: u64) -> Transaction {
    let body = TxBody::RegisterAgent(RegisterBody {
        agent_name: AGENT_NAME.to_string(),
        capabilities: vec![0x04], // oracle/price-feed capability
        endpoint: "arc://agents/price-oracle-v1".to_string(),
        protocol: hash_bytes(b"price-oracle-v1"),
        metadata: serde_json::to_vec(&serde_json::json!({
            "type": "price-oracle",
            "feeds": ["ARC/USD", "ETH/USD", "BTC/USD", "USDC/USD", "SOL/USD"],
            "update_interval_blocks": 10,
            "tier": 2,
            "attestation_bond": 1000
        })).unwrap_or_default(),
    });

    let hash = hash_bytes(&serde_json::to_vec(&body).unwrap_or_default());

    Transaction {
        tx_type: TxType::RegisterAgent,
        from: owner,
        nonce,
        body,
        fee: 0,
        gas_limit: 30_000,
        hash,
        signature: Signature::null(),
        sig_verified: false,
    }
}

/// Construct an InferenceAttestation TX for a price update.
fn build_attestation_tx(
    from: Hash256,
    model_id: Hash256,
    input_data: &[u8],
    output_data: &[u8],
    nonce: u64,
) -> Transaction {
    let body = TxBody::InferenceAttestation(InferenceAttestationBody {
        model_id,
        input_hash: hash_bytes(input_data),
        output_hash: hash_bytes(output_data),
        challenge_period: 100, // 100 blocks
        bond: 1000,            // 1000 ARC bond
    });

    let hash = hash_bytes(&serde_json::to_vec(&body).unwrap_or_default());

    Transaction {
        tx_type: TxType::InferenceAttestation,
        from,
        nonce,
        body,
        fee: 0,
        gas_limit: 50_000,
        hash,
        signature: Signature::null(),
        sig_verified: false,
    }
}

/// Simulate fetching a price with deterministic jitter.
fn simulate_price_fetch(base_price: u128, round: u64) -> u128 {
    let jitter_basis = ((round * 7 + 13) % 100) as u128;
    let jitter_pct = jitter_basis as i128 - 50; // range: -50 to +49
    let delta = (base_price as i128 * jitter_pct) / 10_000; // max 0.5% change
    (base_price as i128 + delta) as u128
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    println!("=== ARC Chain Price Oracle Agent ===\n");

    let owner = hash_bytes(b"oracle-agent-owner");

    // 1. Register agent on-chain.
    println!("[1/4] Registering oracle agent on-chain");
    let register_tx = build_register_tx(owner, 0);
    println!("      RegisterAgent TX: hash={}", register_tx.hash);

    let mut registry = AgentRegistry::new();
    let agent_id_bytes: [u8; 32] = hash_bytes(AGENT_NAME.as_bytes()).0;
    let agent_id = AgentId(agent_id_bytes);

    let agent = Agent {
        id: agent_id,
        owner: owner.0,
        name: AGENT_NAME.to_string(),
        model_id: hash_bytes(b"oracle-model").0,
        config: AgentConfig {
            max_gas_per_action: 1_000_000,
            max_actions_per_block: 50,
            allowed_contracts: Vec::new(),
            auto_fund: false,
            memory_limit_bytes: 1_048_576,
        },
        state: AgentState::Created,
        created_at: 0,
        total_actions: 0,
        reputation: 1.0,
        balance: 10_000_000,
    };

    registry.register(agent).expect("agent registration failed");
    registry.update_state(&agent_id, AgentState::Active).expect("activation failed");
    println!("      Agent registered and activated: {:?}", agent_id);

    // 2. Initialize oracle registry with price feeds.
    println!("[2/4] Setting up oracle registry with initial price feeds");
    let mut oracle = OracleRegistry::new();
    let tokens = token_addresses();

    for (token_addr, symbol, base_price) in &tokens {
        let feed = PriceFeed {
            token: *token_addr,
            price_usd: *base_price,
            timestamp: 1_000_000,
            round_id: 0,
            source: format!("{}-oracle-agent", symbol),
        };
        oracle.update_price(feed);
        println!("      Initialized {}/USD feed: ${:.2}",
            symbol, *base_price as f64 / 1e18);
    }
    println!("      Oracle registry: {} feeds active", oracle.price_count());

    // 3. Simulate price update rounds.
    println!("[3/4] Simulating price update rounds\n");
    let num_rounds = 5u64;
    let model_id = hash_bytes(b"oracle-model");

    for round in 1..=num_rounds {
        println!("  --- Round {} ---", round);

        for (token_addr, symbol, base_price) in &tokens {
            // Simulate fetching new price.
            let new_price = simulate_price_fetch(*base_price, round);

            // Update oracle registry.
            let feed = PriceFeed {
                token: *token_addr,
                price_usd: new_price,
                timestamp: 1_000_000 + round * 12,
                round_id: round,
                source: format!("{}-oracle-agent", symbol),
            };
            oracle.update_price(feed);

            // Build attestation TX.
            let input_data = format!("{}-round-{}", symbol, round);
            let output_data = format!("{}", new_price);
            let attestation_tx = build_attestation_tx(
                owner,
                model_id,
                input_data.as_bytes(),
                output_data.as_bytes(),
                round * tokens.len() as u64,
            );

            println!("    {}/USD: ${:.2} (attestation: {})",
                symbol,
                new_price as f64 / 1e18,
                &hex::encode(attestation_tx.hash.0)[..16],
            );

            // Record action in agent registry.
            let action = AgentAction {
                agent_id,
                action_type: ActionType::Inference,
                target: *token_addr,
                data: new_price.to_le_bytes().to_vec(),
                gas_used: 200,
                timestamp: round,
                result: ActionResult::Success(new_price.to_le_bytes().to_vec()),
            };
            registry.execute_action(&agent_id, action).expect("action execution failed");
        }

        // Verify prices are readable from oracle.
        for (token_addr, _symbol, _) in &tokens {
            let feed = oracle.get_price(token_addr).expect("feed not found");
            assert_eq!(feed.round_id, round);
        }

        println!();
    }

    // 4. Print agent summary.
    println!("[4/4] Agent summary\n");
    let final_agent = registry.get(&agent_id).expect("agent not found");
    println!("  Name:          {}", final_agent.name);
    println!("  State:         {}", final_agent.state);
    println!("  Total actions: {}", final_agent.total_actions);
    println!("  Balance:       {}", final_agent.balance);
    println!("  Reputation:    {:.2}", final_agent.reputation);
    println!("  Oracle feeds:  {}", oracle.price_count());

    // Final prices.
    println!("\n  Latest prices:");
    for (token_addr, symbol, _) in &tokens {
        let feed = oracle.get_price(token_addr).unwrap();
        println!("    {}/USD: ${:.2} (round {}, source: {})",
            symbol,
            feed.price_usd as f64 / 1e18,
            feed.round_id,
            feed.source,
        );
    }

    println!("\nPrice Oracle Agent completed successfully.");
}
