//! AI-native blockchain types for the ARC Chain.
//!
//! This module provides all AI/ML-related types including model registry,
//! model sharding, inference marketplace, on-chain chat, model marketplace,
//! inference caching, and compute credits.

use std::collections::HashMap;

// ============================================================================
// Model Registry (#43)
// ============================================================================

/// Unique identifier for a registered model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModelId(pub [u8; 32]);

impl ModelId {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Quantization format for model weights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Quantization {
    F32,
    F16,
    BF16,
    INT8,
    INT4,
    GPTQ,
    AWQ,
}

/// Parameters describing a model's architecture and size.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelParameters {
    pub parameter_count: u64,
    pub quantization: Quantization,
    pub architecture: String,
}

/// The category of an AI model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelType {
    LLM,
    ImageGen,
    AudioGen,
    Classifier,
    Embedding,
    Custom(String),
}

/// Full metadata for a registered model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelMetadata {
    pub id: ModelId,
    pub name: String,
    pub version: String,
    pub owner: [u8; 32],
    pub model_type: ModelType,
    pub size_bytes: u64,
    pub hash: [u8; 32],
    pub created_at: u64,
    pub parameters: ModelParameters,
}

/// On-chain registry of AI models.
#[derive(Debug, Default)]
pub struct ModelRegistry {
    models: HashMap<ModelId, ModelMetadata>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self {
            models: HashMap::new(),
        }
    }

    /// Register a new model. Returns an error if the model ID already exists.
    pub fn register(&mut self, metadata: ModelMetadata) -> Result<(), String> {
        if self.models.contains_key(&metadata.id) {
            return Err(format!("Model {:?} already registered", metadata.id));
        }
        self.models.insert(metadata.id, metadata);
        Ok(())
    }

    /// Look up a model by its ID.
    pub fn lookup(&self, id: &ModelId) -> Option<&ModelMetadata> {
        self.models.get(id)
    }

    /// List all models owned by a given address.
    pub fn list_by_owner(&self, owner: &[u8; 32]) -> Vec<&ModelMetadata> {
        self.models
            .values()
            .filter(|m| &m.owner == owner)
            .collect()
    }

    /// List all models of a given type.
    pub fn list_by_type(&self, model_type: &ModelType) -> Vec<&ModelMetadata> {
        self.models
            .values()
            .filter(|m| &m.model_type == model_type)
            .collect()
    }

    /// Return the total number of registered models.
    pub fn count(&self) -> usize {
        self.models.len()
    }
}

// ============================================================================
// Model Sharding (#45)
// ============================================================================

/// Status of a model shard on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardStatus {
    Pending,
    Downloading,
    Ready,
    Failed,
    Migrating,
}

/// Configuration for sharding a model across nodes.
#[derive(Debug, Clone, PartialEq)]
pub struct ShardConfig {
    pub model_id: ModelId,
    pub total_shards: u32,
    pub shard_size_bytes: u64,
    pub redundancy_factor: u8,
}

/// A single shard of a model distributed across the network.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelShard {
    pub shard_id: u32,
    pub model_id: ModelId,
    pub data_hash: [u8; 32],
    pub node_assignments: Vec<[u8; 32]>,
    pub status: ShardStatus,
}

/// Assignment of a shard to a specific node with load information.
#[derive(Debug, Clone, PartialEq)]
pub struct ShardAssignment {
    pub shard: ModelShard,
    pub node_id: [u8; 32],
    pub load_percentage: f64,
}

// ============================================================================
// Inference Marketplace (#48)
// ============================================================================

/// Priority level for inference requests.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Low,
    Medium,
    High,
    Critical,
}

/// Input payload for an inference request.
#[derive(Debug, Clone, PartialEq)]
pub enum InferenceInput {
    Text(String),
    Image(Vec<u8>),
    Audio(Vec<u8>),
    Embedding(Vec<f32>),
    Structured(Vec<u8>),
}

/// Output payload from an inference result.
#[derive(Debug, Clone, PartialEq)]
pub enum InferenceOutput {
    Text(String),
    Image(Vec<u8>),
    Audio(Vec<u8>),
    Embedding(Vec<f32>),
    Classification(Vec<(String, f64)>),
}

/// Cryptographic proof that an inference was computed correctly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultProof {
    pub merkle_root: [u8; 32],
    pub commitment: [u8; 32],
}

/// A request for model inference on the marketplace.
#[derive(Debug, Clone, PartialEq)]
pub struct InferenceRequest {
    pub id: [u8; 32],
    pub requester: [u8; 32],
    pub model_id: ModelId,
    pub input: InferenceInput,
    pub max_cost: u64,
    pub deadline: u64,
    pub priority: Priority,
}

/// The result of a completed inference.
#[derive(Debug, Clone, PartialEq)]
pub struct InferenceResult {
    pub request_id: [u8; 32],
    pub provider: [u8; 32],
    pub output: InferenceOutput,
    pub compute_time_ms: u64,
    pub cost: u64,
    pub proof: ResultProof,
}

/// Status of an inference request in the marketplace.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InferenceRequestStatus {
    Open,
    Claimed([u8; 32]),   // provider who claimed it
    Completed,
    Disputed,
}

/// Decentralized marketplace for inference requests and results.
#[derive(Debug, Default)]
pub struct InferenceMarketplace {
    requests: HashMap<[u8; 32], InferenceRequest>,
    statuses: HashMap<[u8; 32], InferenceRequestStatus>,
    results: HashMap<[u8; 32], InferenceResult>,
}

impl InferenceMarketplace {
    pub fn new() -> Self {
        Self {
            requests: HashMap::new(),
            statuses: HashMap::new(),
            results: HashMap::new(),
        }
    }

    /// Submit a new inference request to the marketplace.
    pub fn submit_request(&mut self, request: InferenceRequest) -> Result<(), String> {
        if self.requests.contains_key(&request.id) {
            return Err("Request ID already exists".to_string());
        }
        let id = request.id;
        self.requests.insert(id, request);
        self.statuses.insert(id, InferenceRequestStatus::Open);
        Ok(())
    }

    /// Claim an open inference request as a provider.
    pub fn claim_request(
        &mut self,
        request_id: &[u8; 32],
        provider: [u8; 32],
    ) -> Result<(), String> {
        match self.statuses.get(request_id) {
            Some(InferenceRequestStatus::Open) => {
                self.statuses
                    .insert(*request_id, InferenceRequestStatus::Claimed(provider));
                Ok(())
            }
            Some(_) => Err("Request is not open for claiming".to_string()),
            None => Err("Request not found".to_string()),
        }
    }

    /// Submit the result of a completed inference.
    pub fn submit_result(&mut self, result: InferenceResult) -> Result<(), String> {
        let request_id = &result.request_id;
        match self.statuses.get(request_id) {
            Some(InferenceRequestStatus::Claimed(provider)) if *provider == result.provider => {
                let req = self
                    .requests
                    .get(request_id)
                    .ok_or("Request not found")?;
                if result.cost > req.max_cost {
                    return Err("Result cost exceeds max_cost".to_string());
                }
                self.statuses
                    .insert(*request_id, InferenceRequestStatus::Completed);
                self.results.insert(*request_id, result);
                Ok(())
            }
            Some(InferenceRequestStatus::Claimed(_)) => {
                Err("Only the claiming provider can submit results".to_string())
            }
            Some(_) => Err("Request is not in claimed state".to_string()),
            None => Err("Request not found".to_string()),
        }
    }

    /// Dispute a completed inference result.
    pub fn dispute_result(&mut self, request_id: &[u8; 32]) -> Result<(), String> {
        match self.statuses.get(request_id) {
            Some(InferenceRequestStatus::Completed) => {
                self.statuses
                    .insert(*request_id, InferenceRequestStatus::Disputed);
                Ok(())
            }
            Some(_) => Err("Request is not in completed state".to_string()),
            None => Err("Request not found".to_string()),
        }
    }

    /// Get the result for a completed inference.
    pub fn get_result(&self, request_id: &[u8; 32]) -> Option<&InferenceResult> {
        self.results.get(request_id)
    }

    /// Get an inference request by its ID.
    pub fn get_request(&self, request_id: &[u8; 32]) -> Option<&InferenceRequest> {
        self.requests.get(request_id)
    }

    /// Return the total number of requests in the marketplace.
    pub fn request_count(&self) -> usize {
        self.requests.len()
    }
}

// ============================================================================
// On-chain Chat / Inference (#71)
// ============================================================================

/// Configuration for a chat inference call.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatConfig {
    pub max_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub stop_sequences: Vec<String>,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            max_tokens: 4096,
            temperature: 0.7,
            top_p: 1.0,
            stop_sequences: Vec::new(),
        }
    }
}

/// A single message in an on-chain chat session.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub id: [u8; 32],
    pub sender: [u8; 32],
    pub model_id: ModelId,
    pub prompt: String,
    pub response: Option<String>,
    pub timestamp: u64,
    pub tokens_used: u64,
    pub cost: u64,
}

/// An on-chain chat session consisting of multiple messages.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatSession {
    pub id: [u8; 32],
    pub user: [u8; 32],
    pub model_id: ModelId,
    pub messages: Vec<ChatMessage>,
    pub created_at: u64,
    pub total_cost: u64,
}

impl ChatSession {
    pub fn new(id: [u8; 32], user: [u8; 32], model_id: ModelId, created_at: u64) -> Self {
        Self {
            id,
            user,
            model_id,
            messages: Vec::new(),
            created_at,
            total_cost: 0,
        }
    }

    /// Append a message to the session and update the total cost.
    pub fn add_message(&mut self, message: ChatMessage) {
        self.total_cost += message.cost;
        self.messages.push(message);
    }

    /// Return the number of messages in this session.
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }
}

// ============================================================================
// Model Marketplace (#72)
// ============================================================================

/// A review left on a model listing.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelReview {
    pub reviewer: [u8; 32],
    pub rating: u8,
    pub comment: String,
    pub timestamp: u64,
}

/// A model listed for sale on the marketplace.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelListing {
    pub model_id: ModelId,
    pub seller: [u8; 32],
    pub price_per_inference: u64,
    pub price_per_token: u64,
    pub total_inferences: u64,
    pub rating: f64,
    pub reviews: Vec<ModelReview>,
}

impl ModelListing {
    /// Add a review and recalculate the average rating.
    pub fn add_review(&mut self, review: ModelReview) {
        self.reviews.push(review);
        let total: f64 = self.reviews.iter().map(|r| r.rating as f64).sum();
        self.rating = total / self.reviews.len() as f64;
    }

    /// Increment the total inference counter.
    pub fn record_inference(&mut self) {
        self.total_inferences += 1;
    }
}

/// Aggregate statistics for the model marketplace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceStats {
    pub total_models: u64,
    pub total_inferences: u64,
    pub total_revenue: u64,
}

// ============================================================================
// Inference Caching (#74)
// ============================================================================

/// Policy for cache eviction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachePolicy {
    LRU,
    LFU,
    TTL,
    Custom,
}

/// A cached inference result.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheEntry {
    pub key: [u8; 32],
    pub model_id: ModelId,
    pub input_hash: [u8; 32],
    pub output: Vec<u8>,
    pub created_at: u64,
    pub hit_count: u64,
    pub ttl: u64,
}

/// Statistics for a cache instance.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub total_entries: u64,
    pub total_hits: u64,
    pub total_misses: u64,
    pub total_evictions: u64,
}

/// Inference result cache with configurable eviction policies.
#[derive(Debug)]
pub struct InferenceCache {
    entries: HashMap<[u8; 32], CacheEntry>,
    policy: CachePolicy,
    max_entries: usize,
    stats: CacheStats,
}

impl InferenceCache {
    pub fn new(policy: CachePolicy, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            policy,
            max_entries,
            stats: CacheStats::default(),
        }
    }

    /// Retrieve a cached entry by key, incrementing the hit count.
    pub fn get(&mut self, key: &[u8; 32]) -> Option<&CacheEntry> {
        if self.entries.contains_key(key) {
            // Increment hit count — we need to use get_mut then return immutable ref
            if let Some(entry) = self.entries.get_mut(key) {
                entry.hit_count += 1;
            }
            self.stats.total_hits += 1;
            self.entries.get(key)
        } else {
            self.stats.total_misses += 1;
            None
        }
    }

    /// Insert or update a cache entry. Evicts entries if the cache is full.
    pub fn put(&mut self, entry: CacheEntry) {
        if self.entries.len() >= self.max_entries && !self.entries.contains_key(&entry.key) {
            self.evict_one();
        }
        self.entries.insert(entry.key, entry);
        self.stats.total_entries = self.entries.len() as u64;
    }

    /// Evict a single entry based on the cache policy.
    fn evict_one(&mut self) {
        let key_to_remove = match self.policy {
            CachePolicy::LRU | CachePolicy::TTL | CachePolicy::Custom => {
                // LRU: evict the entry with the lowest hit_count (approximation)
                self.entries
                    .iter()
                    .min_by_key(|(_, e)| e.hit_count)
                    .map(|(k, _)| *k)
            }
            CachePolicy::LFU => {
                // LFU: evict the least frequently used entry
                self.entries
                    .iter()
                    .min_by_key(|(_, e)| e.hit_count)
                    .map(|(k, _)| *k)
            }
        };

        if let Some(key) = key_to_remove {
            self.entries.remove(&key);
            self.stats.total_evictions += 1;
            self.stats.total_entries = self.entries.len() as u64;
        }
    }

    /// Remove a specific entry from the cache.
    pub fn evict(&mut self, key: &[u8; 32]) -> bool {
        if self.entries.remove(key).is_some() {
            self.stats.total_evictions += 1;
            self.stats.total_entries = self.entries.len() as u64;
            true
        } else {
            false
        }
    }

    /// Return current cache statistics.
    pub fn stats(&self) -> &CacheStats {
        &self.stats
    }

    /// Clear all entries from the cache.
    pub fn clear(&mut self) {
        let count = self.entries.len() as u64;
        self.entries.clear();
        self.stats.total_evictions += count;
        self.stats.total_entries = 0;
    }

    /// Return the number of entries currently cached.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ============================================================================
// Compute Credits (#75)
// ============================================================================

/// The reason for a credit transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreditReason {
    InferencePurchase,
    ModelProvision,
    StakingReward,
    Refund,
    Bonus,
}

/// A compute credit balance for a network participant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeCredit {
    pub owner: [u8; 32],
    pub balance: u64,
    pub locked: u64,
    pub earned: u64,
    pub spent: u64,
}

impl ComputeCredit {
    pub fn new(owner: [u8; 32]) -> Self {
        Self {
            owner,
            balance: 0,
            locked: 0,
            earned: 0,
            spent: 0,
        }
    }

    /// Available balance (total balance minus locked).
    pub fn available(&self) -> u64 {
        self.balance.saturating_sub(self.locked)
    }
}

/// A record of a credit transfer between accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditTransaction {
    pub from: [u8; 32],
    pub to: [u8; 32],
    pub amount: u64,
    pub reason: CreditReason,
    pub timestamp: u64,
}

/// Ledger managing compute credit balances and transactions.
#[derive(Debug, Default)]
pub struct CreditLedger {
    accounts: HashMap<[u8; 32], ComputeCredit>,
    transactions: Vec<CreditTransaction>,
}

impl CreditLedger {
    pub fn new() -> Self {
        Self {
            accounts: HashMap::new(),
            transactions: Vec::new(),
        }
    }

    /// Get or create an account for the given address.
    fn ensure_account(&mut self, owner: [u8; 32]) -> &mut ComputeCredit {
        self.accounts
            .entry(owner)
            .or_insert_with(|| ComputeCredit::new(owner))
    }

    /// Deposit credits into an account.
    pub fn deposit(&mut self, owner: [u8; 32], amount: u64, reason: CreditReason, timestamp: u64) {
        let account = self.ensure_account(owner);
        account.balance += amount;
        account.earned += amount;
        self.transactions.push(CreditTransaction {
            from: [0u8; 32], // system mint
            to: owner,
            amount,
            reason,
            timestamp,
        });
    }

    /// Withdraw credits from an account. Fails if insufficient available balance.
    pub fn withdraw(
        &mut self,
        owner: [u8; 32],
        amount: u64,
        reason: CreditReason,
        timestamp: u64,
    ) -> Result<(), String> {
        let account = self.ensure_account(owner);
        if account.available() < amount {
            return Err(format!(
                "Insufficient available balance: {} < {}",
                account.available(),
                amount
            ));
        }
        account.balance -= amount;
        account.spent += amount;
        self.transactions.push(CreditTransaction {
            from: owner,
            to: [0u8; 32], // system burn
            amount,
            reason,
            timestamp,
        });
        Ok(())
    }

    /// Lock credits in an account (e.g., for pending inference).
    pub fn lock(&mut self, owner: [u8; 32], amount: u64) -> Result<(), String> {
        let account = self.ensure_account(owner);
        if account.available() < amount {
            return Err(format!(
                "Insufficient available balance to lock: {} < {}",
                account.available(),
                amount
            ));
        }
        account.locked += amount;
        Ok(())
    }

    /// Unlock previously locked credits.
    pub fn unlock(&mut self, owner: [u8; 32], amount: u64) -> Result<(), String> {
        let account = self.ensure_account(owner);
        if account.locked < amount {
            return Err(format!(
                "Cannot unlock more than locked: {} < {}",
                account.locked, amount
            ));
        }
        account.locked -= amount;
        Ok(())
    }

    /// Transfer credits between two accounts.
    pub fn transfer(
        &mut self,
        from: [u8; 32],
        to: [u8; 32],
        amount: u64,
        reason: CreditReason,
        timestamp: u64,
    ) -> Result<(), String> {
        // Check sender balance first
        {
            let sender = self.ensure_account(from);
            if sender.available() < amount {
                return Err(format!(
                    "Insufficient available balance for transfer: {} < {}",
                    sender.available(),
                    amount
                ));
            }
        }
        // Debit sender
        {
            let sender = self.accounts.get_mut(&from).unwrap();
            sender.balance -= amount;
            sender.spent += amount;
        }
        // Credit receiver
        {
            let receiver = self.ensure_account(to);
            receiver.balance += amount;
            receiver.earned += amount;
        }
        self.transactions.push(CreditTransaction {
            from,
            to,
            amount,
            reason,
            timestamp,
        });
        Ok(())
    }

    /// Get the current balance for an account.
    pub fn balance(&self, owner: &[u8; 32]) -> Option<&ComputeCredit> {
        self.accounts.get(owner)
    }

    /// Return all recorded transactions.
    pub fn transaction_history(&self) -> &[CreditTransaction] {
        &self.transactions
    }

    /// Return the total number of accounts.
    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Helpers ---

    fn test_model_id(seed: u8) -> ModelId {
        ModelId::new([seed; 32])
    }

    fn test_address(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    fn test_model_metadata(seed: u8) -> ModelMetadata {
        ModelMetadata {
            id: test_model_id(seed),
            name: format!("model-{}", seed),
            version: "1.0.0".to_string(),
            owner: test_address(seed),
            model_type: ModelType::LLM,
            size_bytes: 1_000_000,
            hash: [seed; 32],
            created_at: 1000,
            parameters: ModelParameters {
                parameter_count: 7_000_000_000,
                quantization: Quantization::F16,
                architecture: "transformer".to_string(),
            },
        }
    }

    // --- Model Registry Tests ---

    #[test]
    fn test_registry_register_and_lookup() {
        let mut registry = ModelRegistry::new();
        let meta = test_model_metadata(1);
        registry.register(meta.clone()).unwrap();
        let found = registry.lookup(&test_model_id(1)).unwrap();
        assert_eq!(found.name, "model-1");
        assert_eq!(found.version, "1.0.0");
    }

    #[test]
    fn test_registry_duplicate_register_fails() {
        let mut registry = ModelRegistry::new();
        let meta = test_model_metadata(1);
        registry.register(meta.clone()).unwrap();
        let result = registry.register(meta);
        assert!(result.is_err());
    }

    #[test]
    fn test_registry_lookup_missing_returns_none() {
        let registry = ModelRegistry::new();
        assert!(registry.lookup(&test_model_id(99)).is_none());
    }

    #[test]
    fn test_registry_list_by_owner() {
        let mut registry = ModelRegistry::new();
        let mut m1 = test_model_metadata(1);
        m1.owner = test_address(10);
        let mut m2 = test_model_metadata(2);
        m2.owner = test_address(10);
        let mut m3 = test_model_metadata(3);
        m3.owner = test_address(20);

        registry.register(m1).unwrap();
        registry.register(m2).unwrap();
        registry.register(m3).unwrap();

        let owned = registry.list_by_owner(&test_address(10));
        assert_eq!(owned.len(), 2);
        let other = registry.list_by_owner(&test_address(20));
        assert_eq!(other.len(), 1);
    }

    #[test]
    fn test_registry_list_by_type() {
        let mut registry = ModelRegistry::new();
        let mut m1 = test_model_metadata(1);
        m1.model_type = ModelType::LLM;
        let mut m2 = test_model_metadata(2);
        m2.model_type = ModelType::ImageGen;
        let mut m3 = test_model_metadata(3);
        m3.model_type = ModelType::LLM;

        registry.register(m1).unwrap();
        registry.register(m2).unwrap();
        registry.register(m3).unwrap();

        let llms = registry.list_by_type(&ModelType::LLM);
        assert_eq!(llms.len(), 2);
        let image = registry.list_by_type(&ModelType::ImageGen);
        assert_eq!(image.len(), 1);
    }

    #[test]
    fn test_registry_count() {
        let mut registry = ModelRegistry::new();
        assert_eq!(registry.count(), 0);
        registry.register(test_model_metadata(1)).unwrap();
        registry.register(test_model_metadata(2)).unwrap();
        assert_eq!(registry.count(), 2);
    }

    // --- Inference Marketplace Tests ---

    fn test_inference_request(seed: u8) -> InferenceRequest {
        InferenceRequest {
            id: test_address(seed),
            requester: test_address(seed + 100),
            model_id: test_model_id(1),
            input: InferenceInput::Text(format!("prompt-{}", seed)),
            max_cost: 1000,
            deadline: 99999,
            priority: Priority::Medium,
        }
    }

    #[test]
    fn test_marketplace_submit_and_get_request() {
        let mut market = InferenceMarketplace::new();
        let req = test_inference_request(1);
        market.submit_request(req).unwrap();
        let fetched = market.get_request(&test_address(1)).unwrap();
        assert_eq!(fetched.max_cost, 1000);
    }

    #[test]
    fn test_marketplace_duplicate_request_fails() {
        let mut market = InferenceMarketplace::new();
        let req = test_inference_request(1);
        market.submit_request(req.clone()).unwrap();
        assert!(market.submit_request(req).is_err());
    }

    #[test]
    fn test_marketplace_claim_and_submit_result() {
        let mut market = InferenceMarketplace::new();
        let req = test_inference_request(1);
        let req_id = req.id;
        market.submit_request(req).unwrap();

        let provider = test_address(50);
        market.claim_request(&req_id, provider).unwrap();

        let result = InferenceResult {
            request_id: req_id,
            provider,
            output: InferenceOutput::Text("answer".to_string()),
            compute_time_ms: 150,
            cost: 500,
            proof: ResultProof {
                merkle_root: [0xAA; 32],
                commitment: [0xBB; 32],
            },
        };
        market.submit_result(result).unwrap();

        let fetched = market.get_result(&req_id).unwrap();
        assert_eq!(fetched.cost, 500);
    }

    #[test]
    fn test_marketplace_claim_already_claimed_fails() {
        let mut market = InferenceMarketplace::new();
        let req = test_inference_request(1);
        let req_id = req.id;
        market.submit_request(req).unwrap();
        market.claim_request(&req_id, test_address(50)).unwrap();
        assert!(market.claim_request(&req_id, test_address(51)).is_err());
    }

    #[test]
    fn test_marketplace_wrong_provider_submit_fails() {
        let mut market = InferenceMarketplace::new();
        let req = test_inference_request(1);
        let req_id = req.id;
        market.submit_request(req).unwrap();
        market.claim_request(&req_id, test_address(50)).unwrap();

        let result = InferenceResult {
            request_id: req_id,
            provider: test_address(99), // wrong provider
            output: InferenceOutput::Text("bad".to_string()),
            compute_time_ms: 100,
            cost: 100,
            proof: ResultProof {
                merkle_root: [0; 32],
                commitment: [0; 32],
            },
        };
        assert!(market.submit_result(result).is_err());
    }

    #[test]
    fn test_marketplace_dispute_result() {
        let mut market = InferenceMarketplace::new();
        let req = test_inference_request(1);
        let req_id = req.id;
        let provider = test_address(50);
        market.submit_request(req).unwrap();
        market.claim_request(&req_id, provider).unwrap();
        market
            .submit_result(InferenceResult {
                request_id: req_id,
                provider,
                output: InferenceOutput::Text("answer".to_string()),
                compute_time_ms: 100,
                cost: 500,
                proof: ResultProof {
                    merkle_root: [0; 32],
                    commitment: [0; 32],
                },
            })
            .unwrap();

        market.dispute_result(&req_id).unwrap();
        // Disputing again should fail (already disputed)
        assert!(market.dispute_result(&req_id).is_err());
    }

    #[test]
    fn test_marketplace_cost_exceeds_max_fails() {
        let mut market = InferenceMarketplace::new();
        let req = test_inference_request(1);
        let req_id = req.id;
        let provider = test_address(50);
        market.submit_request(req).unwrap();
        market.claim_request(&req_id, provider).unwrap();

        let result = InferenceResult {
            request_id: req_id,
            provider,
            output: InferenceOutput::Text("expensive".to_string()),
            compute_time_ms: 100,
            cost: 9999, // exceeds max_cost of 1000
            proof: ResultProof {
                merkle_root: [0; 32],
                commitment: [0; 32],
            },
        };
        assert!(market.submit_result(result).is_err());
    }

    // --- Chat Session Tests ---

    #[test]
    fn test_chat_session_add_messages() {
        let mut session =
            ChatSession::new(test_address(1), test_address(2), test_model_id(1), 1000);
        assert_eq!(session.message_count(), 0);
        assert_eq!(session.total_cost, 0);

        session.add_message(ChatMessage {
            id: test_address(10),
            sender: test_address(2),
            model_id: test_model_id(1),
            prompt: "Hello".to_string(),
            response: Some("Hi there!".to_string()),
            timestamp: 1001,
            tokens_used: 50,
            cost: 100,
        });

        session.add_message(ChatMessage {
            id: test_address(11),
            sender: test_address(2),
            model_id: test_model_id(1),
            prompt: "How are you?".to_string(),
            response: Some("I'm well!".to_string()),
            timestamp: 1002,
            tokens_used: 40,
            cost: 80,
        });

        assert_eq!(session.message_count(), 2);
        assert_eq!(session.total_cost, 180);
    }

    // --- Model Marketplace Tests ---

    #[test]
    fn test_model_listing_add_review() {
        let mut listing = ModelListing {
            model_id: test_model_id(1),
            seller: test_address(1),
            price_per_inference: 100,
            price_per_token: 1,
            total_inferences: 0,
            rating: 0.0,
            reviews: Vec::new(),
        };

        listing.add_review(ModelReview {
            reviewer: test_address(10),
            rating: 5,
            comment: "Great model!".to_string(),
            timestamp: 1000,
        });
        assert!((listing.rating - 5.0).abs() < f64::EPSILON);

        listing.add_review(ModelReview {
            reviewer: test_address(11),
            rating: 3,
            comment: "Decent".to_string(),
            timestamp: 1001,
        });
        assert!((listing.rating - 4.0).abs() < f64::EPSILON);

        listing.record_inference();
        listing.record_inference();
        assert_eq!(listing.total_inferences, 2);
    }

    // --- Inference Cache Tests ---

    #[test]
    fn test_cache_put_and_get() {
        let mut cache = InferenceCache::new(CachePolicy::LRU, 10);
        let entry = CacheEntry {
            key: test_address(1),
            model_id: test_model_id(1),
            input_hash: test_address(2),
            output: vec![1, 2, 3],
            created_at: 1000,
            hit_count: 0,
            ttl: 3600,
        };
        cache.put(entry);
        assert_eq!(cache.len(), 1);

        let fetched = cache.get(&test_address(1)).unwrap();
        assert_eq!(fetched.output, vec![1, 2, 3]);
        assert_eq!(fetched.hit_count, 1);
        assert_eq!(cache.stats().total_hits, 1);
    }

    #[test]
    fn test_cache_miss() {
        let mut cache = InferenceCache::new(CachePolicy::LRU, 10);
        assert!(cache.get(&test_address(99)).is_none());
        assert_eq!(cache.stats().total_misses, 1);
    }

    #[test]
    fn test_cache_eviction_on_full() {
        let mut cache = InferenceCache::new(CachePolicy::LRU, 2);
        for i in 0..3u8 {
            cache.put(CacheEntry {
                key: test_address(i),
                model_id: test_model_id(1),
                input_hash: test_address(i + 100),
                output: vec![i],
                created_at: 1000 + i as u64,
                hit_count: 0,
                ttl: 3600,
            });
        }
        // Cache should have evicted one entry to make room for the 3rd
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.stats().total_evictions, 1);
    }

    #[test]
    fn test_cache_explicit_evict() {
        let mut cache = InferenceCache::new(CachePolicy::LRU, 10);
        cache.put(CacheEntry {
            key: test_address(1),
            model_id: test_model_id(1),
            input_hash: test_address(2),
            output: vec![1],
            created_at: 1000,
            hit_count: 0,
            ttl: 3600,
        });
        assert!(cache.evict(&test_address(1)));
        assert!(cache.is_empty());
        assert!(!cache.evict(&test_address(1))); // already gone
    }

    #[test]
    fn test_cache_clear() {
        let mut cache = InferenceCache::new(CachePolicy::LFU, 10);
        for i in 0..5u8 {
            cache.put(CacheEntry {
                key: test_address(i),
                model_id: test_model_id(1),
                input_hash: test_address(i + 100),
                output: vec![i],
                created_at: 1000,
                hit_count: 0,
                ttl: 3600,
            });
        }
        assert_eq!(cache.len(), 5);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.stats().total_evictions, 5);
    }

    // --- Compute Credits Tests ---

    #[test]
    fn test_credit_deposit_and_balance() {
        let mut ledger = CreditLedger::new();
        let owner = test_address(1);
        ledger.deposit(owner, 1000, CreditReason::Bonus, 100);

        let account = ledger.balance(&owner).unwrap();
        assert_eq!(account.balance, 1000);
        assert_eq!(account.earned, 1000);
        assert_eq!(account.available(), 1000);
    }

    #[test]
    fn test_credit_withdraw() {
        let mut ledger = CreditLedger::new();
        let owner = test_address(1);
        ledger.deposit(owner, 1000, CreditReason::Bonus, 100);
        ledger
            .withdraw(owner, 300, CreditReason::InferencePurchase, 200)
            .unwrap();

        let account = ledger.balance(&owner).unwrap();
        assert_eq!(account.balance, 700);
        assert_eq!(account.spent, 300);
    }

    #[test]
    fn test_credit_withdraw_insufficient_fails() {
        let mut ledger = CreditLedger::new();
        let owner = test_address(1);
        ledger.deposit(owner, 100, CreditReason::Bonus, 100);
        let result = ledger.withdraw(owner, 500, CreditReason::InferencePurchase, 200);
        assert!(result.is_err());
    }

    #[test]
    fn test_credit_lock_and_unlock() {
        let mut ledger = CreditLedger::new();
        let owner = test_address(1);
        ledger.deposit(owner, 1000, CreditReason::Bonus, 100);

        ledger.lock(owner, 400).unwrap();
        let account = ledger.balance(&owner).unwrap();
        assert_eq!(account.locked, 400);
        assert_eq!(account.available(), 600);

        // Withdraw should respect locked balance
        assert!(ledger
            .withdraw(owner, 700, CreditReason::InferencePurchase, 200)
            .is_err());

        ledger.unlock(owner, 400).unwrap();
        let account = ledger.balance(&owner).unwrap();
        assert_eq!(account.locked, 0);
        assert_eq!(account.available(), 1000);
    }

    #[test]
    fn test_credit_transfer() {
        let mut ledger = CreditLedger::new();
        let alice = test_address(1);
        let bob = test_address(2);

        ledger.deposit(alice, 1000, CreditReason::Bonus, 100);
        ledger
            .transfer(alice, bob, 300, CreditReason::ModelProvision, 200)
            .unwrap();

        let alice_acc = ledger.balance(&alice).unwrap();
        assert_eq!(alice_acc.balance, 700);
        assert_eq!(alice_acc.spent, 300);

        let bob_acc = ledger.balance(&bob).unwrap();
        assert_eq!(bob_acc.balance, 300);
        assert_eq!(bob_acc.earned, 300);
    }

    #[test]
    fn test_credit_transfer_insufficient_fails() {
        let mut ledger = CreditLedger::new();
        let alice = test_address(1);
        let bob = test_address(2);

        ledger.deposit(alice, 100, CreditReason::Bonus, 100);
        let result = ledger.transfer(alice, bob, 500, CreditReason::ModelProvision, 200);
        assert!(result.is_err());
    }

    #[test]
    fn test_credit_transaction_history() {
        let mut ledger = CreditLedger::new();
        let owner = test_address(1);
        ledger.deposit(owner, 1000, CreditReason::Bonus, 100);
        ledger.deposit(owner, 500, CreditReason::StakingReward, 200);
        ledger
            .withdraw(owner, 300, CreditReason::InferencePurchase, 300)
            .unwrap();

        let history = ledger.transaction_history();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].amount, 1000);
        assert_eq!(history[0].reason, CreditReason::Bonus);
        assert_eq!(history[2].reason, CreditReason::InferencePurchase);
    }

    // --- Shard Tests ---

    #[test]
    fn test_shard_config_and_status() {
        let config = ShardConfig {
            model_id: test_model_id(1),
            total_shards: 8,
            shard_size_bytes: 500_000_000,
            redundancy_factor: 3,
        };
        assert_eq!(config.total_shards, 8);

        let shard = ModelShard {
            shard_id: 0,
            model_id: test_model_id(1),
            data_hash: [0xAB; 32],
            node_assignments: vec![test_address(10), test_address(11), test_address(12)],
            status: ShardStatus::Ready,
        };
        assert_eq!(shard.status, ShardStatus::Ready);
        assert_eq!(shard.node_assignments.len(), 3);

        let assignment = ShardAssignment {
            shard,
            node_id: test_address(10),
            load_percentage: 42.5,
        };
        assert!((assignment.load_percentage - 42.5).abs() < f64::EPSILON);
    }

    // --- ChatConfig Default Test ---

    #[test]
    fn test_chat_config_default() {
        let config = ChatConfig::default();
        assert_eq!(config.max_tokens, 4096);
        assert!((config.temperature - 0.7).abs() < f32::EPSILON);
        assert!((config.top_p - 1.0).abs() < f32::EPSILON);
        assert!(config.stop_sequences.is_empty());
    }
}
