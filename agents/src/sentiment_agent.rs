//! Sentiment Agent — On-chain AI agent for binary text sentiment classification.
//!
//! Demonstrates:
//! - Building a small 3-layer neural net (Dense 128->64->2 with ReLU + Softmax)
//! - Serializing model weights using the NeuralNet binary format
//! - Registering as an agent on-chain via RegisterAgent TX
//! - Processing inference requests through the inference precompile (0x0A)
//! - Settling payments via zero-fee Settle TX
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────┐     RegisterAgent TX      ┌──────────────┐
//! │  Sentiment   │ ─────────────────────────>│  ARC Chain   │
//! │    Agent     │     Inference Request     │              │
//! │              │ <────────────────────────>│  Precompile  │
//! │  (3-layer    │     Settle TX (0-fee)     │    0x0A      │
//! │   neural     │ ─────────────────────────>│              │
//! │   net)       │                           │              │
//! └─────────────┘                           └──────────────┘
//! ```

use arc_crypto::hash::hash_bytes;
use arc_crypto::signature::Signature;
use arc_crypto::Hash256;
use arc_types::transaction::{
    RegisterBody, SettleBody, Transaction, TxBody, TxType,
};
use arc_vm::agent::{
    Agent, AgentConfig, AgentId, AgentRegistry, AgentState, ActionResult, ActionType, AgentAction,
};
use arc_vm::inference::{
    InferenceConfig, InferenceEngine, InferenceInput, InferenceParams, InferenceRequest,
    Layer, ModelInfo, NeuralNet,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Agent name registered on-chain.
const AGENT_NAME: &str = "sentiment-classifier-v1";

/// Inference precompile address (0x0A).
const INFERENCE_PRECOMPILE: u8 = 0x0A;

/// Input dimension: simple bag-of-words encoding uses 128 features.
const INPUT_DIM: usize = 128;

/// Hidden layer dimension.
const HIDDEN_DIM: usize = 64;

/// Output dimension: [positive, negative].
const OUTPUT_DIM: usize = 2;

// ---------------------------------------------------------------------------
// Model construction
// ---------------------------------------------------------------------------

/// Build a small 3-layer sentiment classification neural network.
///
/// Architecture: Dense(128->64) -> ReLU -> Dense(64->2) -> Softmax
///
/// Weights are initialized with a simple deterministic pattern so that the
/// model produces consistent (though untrained) outputs suitable for
/// demonstration and testing.
fn build_sentiment_model() -> NeuralNet {
    // Layer 1: Dense 128 -> 64
    let mut weights_1 = Vec::with_capacity(HIDDEN_DIM);
    for i in 0..HIDDEN_DIM {
        let mut row = Vec::with_capacity(INPUT_DIM);
        for j in 0..INPUT_DIM {
            // Xavier-like initialization scaled down for stability.
            let val = ((i as f32 * 7.0 + j as f32 * 13.0) % 100.0 - 50.0) / 500.0;
            row.push(val);
        }
        weights_1.push(row);
    }
    let bias_1 = vec![0.01_f32; HIDDEN_DIM];

    // Layer 2: Dense 64 -> 2
    let mut weights_2 = Vec::with_capacity(OUTPUT_DIM);
    for i in 0..OUTPUT_DIM {
        let mut row = Vec::with_capacity(HIDDEN_DIM);
        for j in 0..HIDDEN_DIM {
            let val = ((i as f32 * 11.0 + j as f32 * 3.0) % 100.0 - 50.0) / 500.0;
            row.push(val);
        }
        weights_2.push(row);
    }
    let bias_2 = vec![0.0_f32; OUTPUT_DIM];

    NeuralNet {
        layers: vec![
            Layer::Dense { weights: weights_1, bias: bias_1 },
            Layer::ReLU,
            Layer::Dense { weights: weights_2, bias: bias_2 },
            Layer::Softmax,
        ],
        input_size: INPUT_DIM,
        output_size: OUTPUT_DIM,
    }
}

/// Encode text into a fixed-size feature vector using a simple bag-of-bytes
/// approach. Each byte of the input text increments the corresponding bucket
/// (mod INPUT_DIM), then the vector is L2-normalized.
fn encode_text(text: &str) -> Vec<f32> {
    let mut features = vec![0.0_f32; INPUT_DIM];
    for &b in text.as_bytes() {
        features[b as usize % INPUT_DIM] += 1.0;
    }
    // L2 normalize
    let norm: f32 = features.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for f in &mut features {
            *f /= norm;
        }
    }
    features
}

// ---------------------------------------------------------------------------
// Transaction helpers
// ---------------------------------------------------------------------------

/// Construct a RegisterAgent transaction for the sentiment agent.
fn build_register_tx(owner: Hash256, nonce: u64) -> Transaction {
    let body = TxBody::RegisterAgent(RegisterBody {
        agent_name: AGENT_NAME.to_string(),
        capabilities: vec![0x01], // sentiment classification capability
        endpoint: "arc://agents/sentiment-classifier-v1".to_string(),
        protocol: hash_bytes(b"sentiment-classification-v1"),
        metadata: serde_json::to_vec(&serde_json::json!({
            "model": "3-layer-dense",
            "input_dim": INPUT_DIM,
            "hidden_dim": HIDDEN_DIM,
            "output_dim": OUTPUT_DIM,
            "activation": "relu+softmax",
            "task": "binary-sentiment"
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
    }
}

/// Construct a Settle TX for a completed inference request (zero fee).
fn build_settle_tx(
    from: Hash256,
    agent_addr: Hash256,
    service_hash: Hash256,
    nonce: u64,
) -> Transaction {
    let body = TxBody::Settle(SettleBody {
        agent_id: agent_addr,
        service_hash,
        amount: 0, // zero-fee settlement
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
    }
}

// ---------------------------------------------------------------------------
// Sentiment result
// ---------------------------------------------------------------------------

/// Result of a sentiment classification.
#[derive(Debug, Clone)]
pub struct SentimentResult {
    pub label: String,
    pub confidence: f64,
    pub scores: [f64; 2], // [positive, negative]
}

impl std::fmt::Display for SentimentResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (confidence: {:.2}%) — positive: {:.4}, negative: {:.4}",
            self.label,
            self.confidence * 100.0,
            self.scores[0],
            self.scores[1],
        )
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    println!("=== ARC Chain Sentiment Agent ===\n");

    // 1. Build the neural network model.
    println!("[1/5] Building 3-layer sentiment model (Dense {}->{}->{})",
        INPUT_DIM, HIDDEN_DIM, OUTPUT_DIM);
    let model = build_sentiment_model();
    println!("      Model built: {} layers, input_size={}, output_size={}",
        model.layers.len(), model.input_size, model.output_size);

    // 2. Serialize model weights.
    println!("[2/5] Serializing model weights to NeuralNet binary format");
    let model_bytes = model.to_bytes();
    println!("      Serialized model size: {} bytes", model_bytes.len());

    // Verify round-trip deserialization.
    let model_restored = NeuralNet::from_bytes(&model_bytes)
        .expect("model round-trip deserialization failed");
    assert_eq!(model.layers.len(), model_restored.layers.len());
    println!("      Round-trip deserialization verified");

    // 3. Register agent on-chain.
    println!("[3/5] Constructing RegisterAgent transaction");
    let owner = hash_bytes(b"sentiment-agent-owner");
    let register_tx = build_register_tx(owner, 0);
    println!("      TX type: {:?}, hash: {}", register_tx.tx_type, register_tx.hash);

    // Register in the agent registry.
    let mut registry = AgentRegistry::new();
    let agent_id_bytes: [u8; 32] = hash_bytes(AGENT_NAME.as_bytes()).0;
    let agent_id = AgentId(agent_id_bytes);
    let model_id: [u8; 32] = hash_bytes(b"sentiment-model-v1").0;

    let agent = Agent {
        id: agent_id,
        owner: owner.0,
        name: AGENT_NAME.to_string(),
        model_id,
        config: AgentConfig {
            max_gas_per_action: 1_000_000,
            max_actions_per_block: 100,
            allowed_contracts: Vec::new(),
            auto_fund: false,
            memory_limit_bytes: 1_048_576,
        },
        state: AgentState::Created,
        created_at: 0,
        total_actions: 0,
        reputation: 1.0,
        balance: 1_000_000,
    };

    registry.register(agent).expect("agent registration failed");
    registry.update_state(&agent_id, AgentState::Active).expect("activation failed");
    println!("      Agent registered and activated: {:?}", agent_id);

    // 4. Deploy model into inference engine.
    println!("[4/5] Loading model into inference engine");
    let config = InferenceConfig {
        max_loaded_models: 10,
        default_timeout_ms: 5000,
        max_tokens: 1024,
        temperature: 0.0,
    };
    let mut engine = InferenceEngine::new(config);
    let model_info = ModelInfo {
        name: "sentiment-3layer".to_string(),
        model_type: "classifier".to_string(),
        parameter_count: (INPUT_DIM * HIDDEN_DIM + HIDDEN_DIM + HIDDEN_DIM * OUTPUT_DIM + OUTPUT_DIM) as u64,
        quantization: "f32".to_string(),
        max_context: INPUT_DIM as u32,
    };
    engine.load_model_with_weights(model_id, model_info, &model_bytes)
        .expect("model loading failed");
    println!("      Model loaded into engine (id: {})", hex::encode(&model_id[..8]));

    // 5. Process sample inference requests.
    println!("[5/5] Processing inference requests\n");

    let test_texts = [
        "This product is amazing and I love it!",
        "Terrible experience, would not recommend.",
        "The weather is nice today.",
        "I am so happy with the results!",
        "This is the worst thing I have ever seen.",
        "Neutral statement about blockchain technology.",
    ];

    for (i, text) in test_texts.iter().enumerate() {
        // Encode text to feature vector.
        let features = encode_text(text);

        // Run forward pass through the model.
        let output = model.forward(&features);
        assert_eq!(output.len(), OUTPUT_DIM, "unexpected output dimension");

        let positive_score = output[0] as f64;
        let negative_score = output[1] as f64;
        let (label, confidence) = if positive_score >= negative_score {
            ("Positive", positive_score)
        } else {
            ("Negative", negative_score)
        };

        let result = SentimentResult {
            label: label.to_string(),
            confidence,
            scores: [positive_score, negative_score],
        };

        println!("  Request {}: \"{}\"", i + 1, text);
        println!("    Result: {}", result);

        // Record the action in the agent registry.
        let action = AgentAction {
            agent_id,
            action_type: ActionType::Inference,
            target: model_id,
            data: text.as_bytes().to_vec(),
            gas_used: 100,
            timestamp: i as u64,
            result: ActionResult::Success(output.iter().flat_map(|f| f.to_le_bytes()).collect()),
        };
        registry.execute_action(&agent_id, action).expect("action execution failed");

        // Build a Settle TX for the completed inference.
        let service_hash = hash_bytes(text.as_bytes());
        let settle_tx = build_settle_tx(owner, Hash256(agent_id_bytes), service_hash, (i + 1) as u64);
        println!("    Settle TX: hash={}", settle_tx.hash);
        println!();
    }

    // Print final agent state.
    let final_agent = registry.get(&agent_id).expect("agent not found");
    println!("--- Agent Summary ---");
    println!("  Name:          {}", final_agent.name);
    println!("  State:         {}", final_agent.state);
    println!("  Total actions: {}", final_agent.total_actions);
    println!("  Balance:       {}", final_agent.balance);
    println!("  Reputation:    {:.2}", final_agent.reputation);

    println!("\nSentiment Agent completed successfully.");
}
