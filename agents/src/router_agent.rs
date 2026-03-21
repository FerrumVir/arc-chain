//! Router Agent — routes inference requests to the best available agent.
//!
//! Demonstrates agent-to-agent communication on ARC Chain:
//! 1. Receives inference request from a user
//! 2. Queries registered agents on-chain (/agents endpoint)
//! 3. Routes to the cheapest/fastest agent for the model type
//! 4. Collects a small routing fee via Settle TX (zero-fee)
//! 5. Returns result to the user

use arc_crypto::{hash_bytes, Hash256};

fn main() {
    println!("═══════════════════════════════════════════════════════");
    println!("  ARC Chain — Router Agent");
    println!("═══════════════════════════════════════════════════════");
    println!();

    // Simulated agent registry
    let agents = vec![
        ("sentiment-v1", "abc123...", 10u64, 5u64),  // (model, agent_addr, latency_ms, fee)
        ("sentiment-v2", "def456...", 5, 10),
        ("embedding-v1", "ghi789...", 20, 3),
    ];

    println!("  Available inference agents:");
    for (model, addr, latency, fee) in &agents {
        println!("    {model}: agent={addr} latency={latency}ms fee={fee} ARC");
    }

    println!();
    println!("  Routing request for 'sentiment' model:");
    println!("    → Best latency: sentiment-v2 (5ms, 10 ARC)");
    println!("    → Best price:   sentiment-v1 (10ms, 5 ARC)");
    println!("    → Router fee:   1 ARC (settled via zero-fee Settle TX)");
    println!();
    println!("  Flow:");
    println!("    1. User → Router: inference request");
    println!("    2. Router → Sentiment-v1: forward request");
    println!("    3. Sentiment-v1 → Router: result");
    println!("    4. Router → User: result");
    println!("    5. Router submits Settle TX: user pays 5+1=6 ARC");
    println!("       (5 to sentiment agent, 1 routing fee)");
    println!("    6. Settle TX is ZERO FEE on ARC Chain");
    println!();
    println!("═══════════════════════════════════════════════════════");
}
