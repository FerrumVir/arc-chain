//! Router Agent — On-chain AI agent that routes inference requests.
//!
//! Demonstrates:
//! - Receiving inference requests and routing to available agents
//! - Selecting the cheapest/fastest inference provider
//! - Collecting a routing fee via Settle TXs
//! - Agent-to-agent communication patterns
//!
//! # Architecture
//!
//! ```text
//!                          ┌───────────────┐
//!                          │  Router Agent  │
//!                 request  │  ┌──────────┐ │  route
//! ┌──────────┐ ─────────> │  │ Routing  │ │ ──────> ┌──────────────┐
//! │  Client   │            │  │  Table   │ │         │ Sentiment    │
//! │           │ <───────── │  └──────────┘ │ <────── │ Agent        │
//! └──────────┘   result   │               │  result └──────────────┘
//!                         │  Settle TXs   │
//!                         │  (routing fee) │ ──────> ┌──────────────┐
//!                         └───────────────┘         │ Oracle Agent │
//!                                                   └──────────────┘
//! ```

use arc_crypto::{hash_bytes, Hash256};
use arc_crypto::signature::Signature;
use arc_types::transaction::{
    RegisterBody, SettleBody, Transaction, TxBody, TxType,
};
use arc_vm::agent::{
    Agent, AgentAction, AgentConfig, AgentId, AgentRegistry, AgentState, ActionResult, ActionType,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const AGENT_NAME: &str = "inference-router-v1";

/// Routing fee in ARC (charged per routed request).
const ROUTING_FEE: u64 = 10;

// ---------------------------------------------------------------------------
// Routing table
// ---------------------------------------------------------------------------

/// An entry in the routing table representing an available inference provider.
#[derive(Debug, Clone)]
struct ProviderEntry {
    agent_id: AgentId,
    name: String,
    cost_per_request: u64,
    avg_latency_ms: u64,
    capabilities: Vec<String>,
    available: bool,
    #[allow(dead_code)]
    reputation: f64,
}

/// Strategy for selecting an inference provider.
#[derive(Debug, Clone, Copy)]
enum RoutingStrategy {
    Cheapest,
    Fastest,
    Balanced,
}

/// Routing table managing available inference providers.
struct RoutingTable {
    providers: Vec<ProviderEntry>,
}

impl RoutingTable {
    fn new() -> Self {
        Self { providers: Vec::new() }
    }

    fn register_provider(&mut self, entry: ProviderEntry) {
        self.providers.push(entry);
    }

    fn select_provider(
        &self,
        capability: &str,
        strategy: RoutingStrategy,
    ) -> Option<&ProviderEntry> {
        let candidates: Vec<&ProviderEntry> = self.providers.iter()
            .filter(|p| p.available && p.capabilities.iter().any(|c| c == capability))
            .collect();

        if candidates.is_empty() {
            return None;
        }

        match strategy {
            RoutingStrategy::Cheapest => {
                candidates.into_iter().min_by_key(|p| p.cost_per_request)
            }
            RoutingStrategy::Fastest => {
                candidates.into_iter().min_by_key(|p| p.avg_latency_ms)
            }
            RoutingStrategy::Balanced => {
                candidates.into_iter().min_by(|a, b| {
                    let score_a = a.cost_per_request as f64 + a.avg_latency_ms as f64 * 0.1;
                    let score_b = b.cost_per_request as f64 + b.avg_latency_ms as f64 * 0.1;
                    score_a.partial_cmp(&score_b).unwrap_or(std::cmp::Ordering::Equal)
                })
            }
        }
    }

    fn provider_count(&self) -> usize {
        self.providers.len()
    }

    fn available_count(&self) -> usize {
        self.providers.iter().filter(|p| p.available).count()
    }
}

/// An inference request to be routed.
#[derive(Debug)]
struct InferenceRoutingRequest {
    capability: String,
    input_data: Vec<u8>,
    max_cost: u64,
    strategy: RoutingStrategy,
}

// ---------------------------------------------------------------------------
// Transaction helpers
// ---------------------------------------------------------------------------

fn build_register_tx(owner: Hash256, nonce: u64) -> Transaction {
    let body = TxBody::RegisterAgent(RegisterBody {
        agent_name: AGENT_NAME.to_string(),
        capabilities: vec![0xFF],
        endpoint: "arc://agents/inference-router-v1".to_string(),
        protocol: hash_bytes(b"inference-router-v1"),
        metadata: serde_json::to_vec(&serde_json::json!({
            "type": "inference-router",
            "routing_fee": ROUTING_FEE,
            "strategies": ["cheapest", "fastest", "balanced"],
            "description": "Routes inference requests to optimal providers"
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

fn build_routing_fee_settle_tx(
    from: Hash256,
    router_addr: Hash256,
    request_hash: Hash256,
    nonce: u64,
) -> Transaction {
    let body = TxBody::Settle(SettleBody {
        agent_id: router_addr,
        service_hash: request_hash,
        amount: ROUTING_FEE,
        usage_units: 1,
        amount_commitment: None,
    });

    let hash = hash_bytes(&serde_json::to_vec(&body).unwrap_or_default());

    Transaction {
        tx_type: TxType::Settle,
        from,
        nonce,
        body,
        fee: 0,
        gas_limit: 25_000,
        hash,
        signature: Signature::null(),
        sig_verified: false,
    }
}

fn build_provider_settle_tx(
    from: Hash256,
    provider_addr: Hash256,
    service_hash: Hash256,
    amount: u64,
    nonce: u64,
) -> Transaction {
    let body = TxBody::Settle(SettleBody {
        agent_id: provider_addr,
        service_hash,
        amount,
        usage_units: 1,
        amount_commitment: None,
    });

    let hash = hash_bytes(&serde_json::to_vec(&body).unwrap_or_default());

    Transaction {
        tx_type: TxType::Settle,
        from,
        nonce,
        body,
        fee: 0,
        gas_limit: 25_000,
        hash,
        signature: Signature::null(),
        sig_verified: false,
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    println!("=== ARC Chain Router Agent ===\n");

    let owner = hash_bytes(b"router-agent-owner");

    // 1. Register the router agent.
    println!("[1/5] Registering router agent on-chain");
    let register_tx = build_register_tx(owner, 0);
    println!("      RegisterAgent TX: hash={}", register_tx.hash);

    let mut registry = AgentRegistry::new();
    let router_id_bytes: [u8; 32] = hash_bytes(AGENT_NAME.as_bytes()).0;
    let router_id = AgentId(router_id_bytes);

    let router_agent = Agent {
        id: router_id,
        owner: owner.0,
        name: AGENT_NAME.to_string(),
        model_id: [0u8; 32],
        config: AgentConfig {
            max_gas_per_action: 1_000_000,
            max_actions_per_block: 200,
            allowed_contracts: Vec::new(),
            auto_fund: false,
            memory_limit_bytes: 1_048_576,
        },
        state: AgentState::Created,
        created_at: 0,
        total_actions: 0,
        reputation: 1.0,
        balance: 5_000_000,
    };

    registry.register(router_agent).expect("router registration failed");
    registry.update_state(&router_id, AgentState::Active).expect("activation failed");
    println!("      Router agent registered: {:?}", router_id);

    // 2. Populate the routing table with inference providers.
    println!("[2/5] Populating routing table with inference providers\n");
    let mut routing_table = RoutingTable::new();

    let providers_data: Vec<(&str, &str, u64, u64, Vec<&str>, f64)> = vec![
        ("sentiment-fast",  "SentimentFast",  50,  10, vec!["sentiment"],                   0.95),
        ("sentiment-cheap", "SentimentCheap", 20,  50, vec!["sentiment"],                   0.88),
        ("oracle-primary",  "OraclePrimary",  30,   5, vec!["price-oracle"],                0.99),
        ("oracle-backup",   "OracleBackup",   25,  15, vec!["price-oracle"],                0.92),
        ("embedding-gpu",   "EmbeddingGPU",   100,  3, vec!["embedding", "sentiment"],      0.97),
    ];

    for (key, name, cost, latency, caps, rep) in &providers_data {
        let entry = ProviderEntry {
            agent_id: AgentId(hash_bytes(key.as_bytes()).0),
            name: name.to_string(),
            cost_per_request: *cost,
            avg_latency_ms: *latency,
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            available: true,
            reputation: *rep,
        };
        println!("  Registered: {} (cost: {}, latency: {}ms, caps: {:?})",
            name, cost, latency, caps);
        routing_table.register_provider(entry);
    }
    println!("\n      Total providers: {}, available: {}",
        routing_table.provider_count(), routing_table.available_count());

    // 3. Register provider agents on-chain.
    println!("\n[3/5] Registering provider agents on-chain");
    for (i, (key, _name, _cost, _latency, _caps, _rep)) in providers_data.iter().enumerate() {
        let id_bytes = hash_bytes(key.as_bytes()).0;
        let id = AgentId(id_bytes);
        let agent = Agent {
            id,
            owner: owner.0,
            name: key.to_string(),
            model_id: hash_bytes(format!("{}-model", key).as_bytes()).0,
            config: AgentConfig::default(),
            state: AgentState::Created,
            created_at: 0,
            total_actions: 0,
            reputation: 1.0,
            balance: 1_000_000,
        };
        registry.register(agent).expect("provider registration failed");
        registry.update_state(&id, AgentState::Active).expect("provider activation failed");
        println!("      Provider {} registered: {:?}", i + 1, id);
    }

    // 4. Process sample routing requests.
    println!("\n[4/5] Processing inference routing requests\n");

    let requests = vec![
        InferenceRoutingRequest {
            capability: "sentiment".to_string(),
            input_data: b"I love ARC Chain!".to_vec(),
            max_cost: 100,
            strategy: RoutingStrategy::Cheapest,
        },
        InferenceRoutingRequest {
            capability: "sentiment".to_string(),
            input_data: b"Terrible gas fees on other chains.".to_vec(),
            max_cost: 200,
            strategy: RoutingStrategy::Fastest,
        },
        InferenceRoutingRequest {
            capability: "price-oracle".to_string(),
            input_data: b"ARC/USD".to_vec(),
            max_cost: 50,
            strategy: RoutingStrategy::Cheapest,
        },
        InferenceRoutingRequest {
            capability: "price-oracle".to_string(),
            input_data: b"ETH/USD".to_vec(),
            max_cost: 100,
            strategy: RoutingStrategy::Balanced,
        },
        InferenceRoutingRequest {
            capability: "embedding".to_string(),
            input_data: b"Generate embedding for this text".to_vec(),
            max_cost: 200,
            strategy: RoutingStrategy::Fastest,
        },
        InferenceRoutingRequest {
            capability: "nonexistent".to_string(),
            input_data: b"This should fail to route".to_vec(),
            max_cost: 100,
            strategy: RoutingStrategy::Cheapest,
        },
    ];

    let mut total_routing_fees = 0u64;
    let mut total_provider_fees = 0u64;
    let mut successful_routes = 0u64;
    let mut nonce = 1u64;

    for (i, request) in requests.iter().enumerate() {
        println!("  Request {}: capability=\"{}\", strategy={:?}, max_cost={}",
            i + 1, request.capability, request.strategy, request.max_cost);

        match routing_table.select_provider(&request.capability, request.strategy) {
            Some(provider) => {
                let total_cost = provider.cost_per_request + ROUTING_FEE;
                if total_cost > request.max_cost {
                    println!("    REJECTED: total cost {} exceeds max_cost {}\n",
                        total_cost, request.max_cost);
                    continue;
                }

                println!("    Routed to: {} (cost: {} + {} routing = {})",
                    provider.name, provider.cost_per_request, ROUTING_FEE, total_cost);

                let result_data = hash_bytes(&request.input_data).0.to_vec();

                let action = AgentAction {
                    agent_id: router_id,
                    action_type: ActionType::Message,
                    target: provider.agent_id.0,
                    data: request.input_data.clone(),
                    gas_used: 150,
                    timestamp: i as u64,
                    result: ActionResult::Success(result_data),
                };
                registry.execute_action(&router_id, action).expect("routing action failed");

                let request_hash = hash_bytes(&request.input_data);
                let routing_settle = build_routing_fee_settle_tx(
                    owner, Hash256(router_id_bytes), request_hash, nonce,
                );
                nonce += 1;
                println!("    Routing fee Settle TX: hash={}",
                    &hex::encode(routing_settle.hash.0)[..16]);

                let provider_settle = build_provider_settle_tx(
                    Hash256(router_id_bytes),
                    Hash256(provider.agent_id.0),
                    request_hash,
                    provider.cost_per_request,
                    nonce,
                );
                nonce += 1;
                println!("    Provider fee Settle TX: hash={} (amount: {})",
                    &hex::encode(provider_settle.hash.0)[..16],
                    provider.cost_per_request);

                total_routing_fees += ROUTING_FEE;
                total_provider_fees += provider.cost_per_request;
                successful_routes += 1;
            }
            None => {
                println!("    NO PROVIDER FOUND for capability \"{}\"",
                    request.capability);
            }
        }
        println!();
    }

    // 5. Summary.
    println!("[5/5] Router agent summary\n");
    let final_router = registry.get(&router_id).expect("router not found");
    println!("  Name:               {}", final_router.name);
    println!("  State:              {}", final_router.state);
    println!("  Total actions:      {}", final_router.total_actions);
    println!("  Balance:            {}", final_router.balance);
    println!("  Successful routes:  {}", successful_routes);
    println!("  Total routing fees: {} ARC", total_routing_fees);
    println!("  Total provider fees:{} ARC", total_provider_fees);
    println!("  Providers:          {} ({} available)",
        routing_table.provider_count(), routing_table.available_count());

    println!("\nRouter Agent completed successfully.");
}
