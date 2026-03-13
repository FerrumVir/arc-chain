// Add to lib.rs: pub mod indexer;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Core event types
// ---------------------------------------------------------------------------

/// Emitted event (like Ethereum logs)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub block_height: u64,
    pub block_hash: [u8; 32],
    pub tx_hash: [u8; 32],
    pub tx_index: u32,
    pub log_index: u32,
    pub contract_address: [u8; 32],
    pub topics: Vec<[u8; 32]>,    // topic[0] = event signature hash
    pub data: Vec<u8>,             // ABI-encoded event data
    pub timestamp: u64,
    pub removed: bool,             // True if in a reorged block
}

/// Event filter (like eth_getLogs filter)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventFilter {
    pub from_block: Option<u64>,
    pub to_block: Option<u64>,
    pub addresses: Vec<[u8; 32]>,                  // Match any of these contracts
    pub topics: Vec<Option<Vec<[u8; 32]>>>,        // topic[n] matches any in list
}

// ---------------------------------------------------------------------------
// Indexed summaries
// ---------------------------------------------------------------------------

/// Indexed block summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedBlock {
    pub height: u64,
    pub hash: [u8; 32],
    pub parent_hash: [u8; 32],
    pub state_root: [u8; 32],
    pub tx_count: u32,
    pub event_count: u32,
    pub gas_used: u64,
    pub timestamp: u64,
    pub proposer: [u8; 32],
    pub size_bytes: u64,
}

/// Indexed transaction summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedTransaction {
    pub hash: [u8; 32],
    pub block_height: u64,
    pub block_hash: [u8; 32],
    pub index: u32,
    pub from: [u8; 32],
    pub to: Option<[u8; 32]>,
    pub value: u64,
    pub tx_type: String,
    pub status: TxStatus,
    pub gas_used: u64,
    pub events: Vec<Event>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TxStatus {
    Success,
    Reverted,
    Pending,
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

/// WebSocket subscription types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SubscriptionType {
    NewBlocks,
    NewTransactions,
    EventLogs(EventFilter),
    PendingTransactions,
    SyncStatus,
}

/// Subscription message sent to clients
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SubscriptionMessage {
    Block(IndexedBlock),
    Transaction(IndexedTransaction),
    Log(Event),
    SyncProgress {
        current: u64,
        highest: u64,
        syncing: bool,
    },
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

/// Pagination for queries
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pagination {
    pub offset: u64,
    pub limit: u64,
    pub total: u64,
}

/// Address activity summary (for explorers)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressActivity {
    pub address: [u8; 32],
    pub tx_count: u64,
    pub first_seen: u64,
    pub last_seen: u64,
    pub total_sent: u128,
    pub total_received: u128,
    pub contract_interactions: u64,
    pub events_emitted: u64,
}

// ---------------------------------------------------------------------------
// Indexer state
// ---------------------------------------------------------------------------

/// Indexer state
pub struct IndexerState {
    blocks: Vec<IndexedBlock>,
    events: Vec<Event>,
    latest_height: u64,
    subscriptions: Vec<Subscription>,
    next_sub_id: u64,
}

/// Active subscription
pub struct Subscription {
    pub id: u64,
    pub sub_type: SubscriptionType,
    pub created_at: u64,
    pub last_notified: u64,
}

// ---------------------------------------------------------------------------
// Implementations
// ---------------------------------------------------------------------------

impl EventFilter {
    pub fn new() -> Self {
        Self {
            from_block: None,
            to_block: None,
            addresses: Vec::new(),
            topics: Vec::new(),
        }
    }

    pub fn with_address(mut self, addr: [u8; 32]) -> Self {
        self.addresses.push(addr);
        self
    }

    pub fn with_topic(mut self, index: usize, topic: [u8; 32]) -> Self {
        // Extend the topics vec if needed so `index` is valid.
        while self.topics.len() <= index {
            self.topics.push(None);
        }
        match &mut self.topics[index] {
            Some(list) => list.push(topic),
            slot @ None => *slot = Some(vec![topic]),
        }
        self
    }

    pub fn with_block_range(mut self, from: u64, to: u64) -> Self {
        self.from_block = Some(from);
        self.to_block = Some(to);
        self
    }

    /// Returns `true` if the given event satisfies every non-empty clause in
    /// this filter (conjunction of address list, topic lists, and block range).
    pub fn matches(&self, event: &Event) -> bool {
        // Block range check
        if let Some(from) = self.from_block {
            if event.block_height < from {
                return false;
            }
        }
        if let Some(to) = self.to_block {
            if event.block_height > to {
                return false;
            }
        }

        // Address check — empty list means "any address"
        if !self.addresses.is_empty()
            && !self.addresses.contains(&event.contract_address)
        {
            return false;
        }

        // Topic check — each position is independently matched (OR within,
        // AND across positions). A `None` position matches anything.
        for (i, topic_filter) in self.topics.iter().enumerate() {
            if let Some(allowed) = topic_filter {
                match event.topics.get(i) {
                    Some(event_topic) if allowed.contains(event_topic) => {}
                    _ => return false,
                }
            }
        }

        true
    }
}

impl Default for EventFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexerState {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            events: Vec::new(),
            latest_height: 0,
            subscriptions: Vec::new(),
            next_sub_id: 1,
        }
    }

    pub fn index_block(&mut self, block: IndexedBlock) {
        if block.height > self.latest_height {
            self.latest_height = block.height;
        }
        self.blocks.push(block);
    }

    pub fn index_event(&mut self, event: Event) {
        self.events.push(event);
    }

    pub fn get_block(&self, height: u64) -> Option<&IndexedBlock> {
        self.blocks.iter().find(|b| b.height == height)
    }

    pub fn get_events(&self, filter: &EventFilter) -> Vec<&Event> {
        self.events.iter().filter(|e| filter.matches(e)).collect()
    }

    pub fn latest_height(&self) -> u64 {
        self.latest_height
    }

    pub fn subscribe(&mut self, sub_type: SubscriptionType) -> u64 {
        let id = self.next_sub_id;
        self.next_sub_id += 1;
        self.subscriptions.push(Subscription {
            id,
            sub_type,
            created_at: 0,
            last_notified: 0,
        });
        id
    }

    pub fn unsubscribe(&mut self, id: u64) -> bool {
        let before = self.subscriptions.len();
        self.subscriptions.retain(|s| s.id != id);
        self.subscriptions.len() < before
    }

    pub fn active_subscriptions(&self) -> usize {
        self.subscriptions.len()
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }
}

impl Default for IndexerState {
    fn default() -> Self {
        Self::new()
    }
}

impl AddressActivity {
    pub fn new(address: [u8; 32]) -> Self {
        Self {
            address,
            tx_count: 0,
            first_seen: u64::MAX,
            last_seen: 0,
            total_sent: 0,
            total_received: 0,
            contract_interactions: 0,
            events_emitted: 0,
        }
    }

    pub fn record_sent(&mut self, amount: u128, height: u64) {
        self.total_sent += amount;
        self.tx_count += 1;
        if height < self.first_seen {
            self.first_seen = height;
        }
        if height > self.last_seen {
            self.last_seen = height;
        }
    }

    pub fn record_received(&mut self, amount: u128, height: u64) {
        self.total_received += amount;
        self.tx_count += 1;
        if height < self.first_seen {
            self.first_seen = height;
        }
        if height > self.last_seen {
            self.last_seen = height;
        }
    }

    /// Net flow: total received minus total sent. Positive means net inflow.
    pub fn net_flow(&self) -> i128 {
        self.total_received as i128 - self.total_sent as i128
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a minimal Event for testing.
    fn make_event(
        block_height: u64,
        contract: [u8; 32],
        topics: Vec<[u8; 32]>,
    ) -> Event {
        Event {
            block_height,
            block_hash: [0u8; 32],
            tx_hash: [0u8; 32],
            tx_index: 0,
            log_index: 0,
            contract_address: contract,
            topics,
            data: Vec::new(),
            timestamp: block_height * 12,
            removed: false,
        }
    }

    fn addr(byte: u8) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = byte;
        a
    }

    // 1. Filter by contract address
    #[test]
    fn test_event_filter_matches_address() {
        let filter = EventFilter::new().with_address(addr(1));
        let event_match = make_event(10, addr(1), vec![]);
        let event_miss = make_event(10, addr(2), vec![]);
        assert!(filter.matches(&event_match));
        assert!(!filter.matches(&event_miss));
    }

    // 2. Filter by topic[0]
    #[test]
    fn test_event_filter_matches_topic() {
        let topic = addr(0xAA);
        let filter = EventFilter::new().with_topic(0, topic);
        let event_match = make_event(5, addr(1), vec![topic]);
        let event_miss = make_event(5, addr(1), vec![addr(0xBB)]);
        assert!(filter.matches(&event_match));
        assert!(!filter.matches(&event_miss));
    }

    // 3. Filter by block range
    #[test]
    fn test_event_filter_block_range() {
        let filter = EventFilter::new().with_block_range(10, 20);
        assert!(filter.matches(&make_event(15, addr(1), vec![])));
        assert!(filter.matches(&make_event(10, addr(1), vec![])));
        assert!(filter.matches(&make_event(20, addr(1), vec![])));
        assert!(!filter.matches(&make_event(9, addr(1), vec![])));
        assert!(!filter.matches(&make_event(21, addr(1), vec![])));
    }

    // 4. Non-matching event rejected
    #[test]
    fn test_event_filter_no_match() {
        let filter = EventFilter::new()
            .with_address(addr(1))
            .with_topic(0, addr(0xAA))
            .with_block_range(100, 200);
        // Wrong address, wrong topic, wrong block
        let event = make_event(50, addr(99), vec![addr(0xFF)]);
        assert!(!filter.matches(&event));
    }

    // 5. Add block, retrieve by height
    #[test]
    fn test_indexer_index_block() {
        let mut state = IndexerState::new();
        let block = IndexedBlock {
            height: 42,
            hash: addr(0x42),
            parent_hash: addr(0x41),
            state_root: [0u8; 32],
            tx_count: 5,
            event_count: 3,
            gas_used: 21000,
            timestamp: 1000,
            proposer: addr(0x01),
            size_bytes: 2048,
        };
        state.index_block(block);

        assert_eq!(state.block_count(), 1);
        assert_eq!(state.latest_height(), 42);
        let retrieved = state.get_block(42).unwrap();
        assert_eq!(retrieved.height, 42);
        assert_eq!(retrieved.tx_count, 5);
        assert!(state.get_block(99).is_none());
    }

    // 6. Add events, filter retrieval
    #[test]
    fn test_indexer_index_events() {
        let mut state = IndexerState::new();
        state.index_event(make_event(10, addr(1), vec![addr(0xAA)]));
        state.index_event(make_event(20, addr(2), vec![addr(0xBB)]));
        state.index_event(make_event(30, addr(1), vec![addr(0xCC)]));

        assert_eq!(state.event_count(), 3);

        let filter = EventFilter::new().with_address(addr(1));
        let results = state.get_events(&filter);
        assert_eq!(results.len(), 2);
    }

    // 7. Subscribe, count, unsubscribe
    #[test]
    fn test_indexer_subscriptions() {
        let mut state = IndexerState::new();

        let id1 = state.subscribe(SubscriptionType::NewBlocks);
        let id2 = state.subscribe(SubscriptionType::NewTransactions);
        assert_eq!(state.active_subscriptions(), 2);

        assert!(state.unsubscribe(id1));
        assert_eq!(state.active_subscriptions(), 1);

        // Double-unsubscribe returns false
        assert!(!state.unsubscribe(id1));

        assert!(state.unsubscribe(id2));
        assert_eq!(state.active_subscriptions(), 0);
    }

    // 8. Record sent/received, check net flow
    #[test]
    fn test_address_activity() {
        let mut activity = AddressActivity::new(addr(0x01));

        activity.record_sent(100, 5);
        activity.record_received(250, 10);
        activity.record_sent(50, 15);

        assert_eq!(activity.total_sent, 150);
        assert_eq!(activity.total_received, 250);
        assert_eq!(activity.net_flow(), 100); // 250 - 150
        assert_eq!(activity.tx_count, 3);
        assert_eq!(activity.first_seen, 5);
        assert_eq!(activity.last_seen, 15);
    }

    // 9. Correct total/offset/limit
    #[test]
    fn test_pagination() {
        let page = Pagination {
            offset: 20,
            limit: 10,
            total: 95,
        };
        assert_eq!(page.offset, 20);
        assert_eq!(page.limit, 10);
        assert_eq!(page.total, 95);
    }

    // 10. All subscription types serialize round-trip
    #[test]
    fn test_subscription_types() {
        let types = vec![
            SubscriptionType::NewBlocks,
            SubscriptionType::NewTransactions,
            SubscriptionType::PendingTransactions,
            SubscriptionType::SyncStatus,
            SubscriptionType::EventLogs(EventFilter::new().with_address(addr(1))),
        ];

        for sub in &types {
            let json = serde_json::to_string(sub).expect("serialize");
            let _: SubscriptionType =
                serde_json::from_str(&json).expect("deserialize");
        }
    }
}
