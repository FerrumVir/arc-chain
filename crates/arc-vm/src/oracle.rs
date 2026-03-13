//! Enhanced Price Oracle
//! Multi-source price aggregation with TWAP, staleness detection, and confidence scoring.

use std::collections::HashMap;

/// A trading pair (e.g., BTC/USD).
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct TradingPair {
    pub base: String,
    pub quote: String,
}

impl TradingPair {
    pub fn new(base: &str, quote: &str) -> Self {
        Self {
            base: base.to_string(),
            quote: quote.to_string(),
        }
    }

    pub fn key(&self) -> String {
        format!("{}/{}", self.base, self.quote)
    }
}

/// Source of a price feed.
#[derive(Debug, Clone, PartialEq)]
pub enum PriceSource {
    Chainlink,
    Pyth,
    UniswapTWAP,
    InternalDex,
    Aggregated,
}

/// A single price data point from a source.
#[derive(Debug, Clone)]
pub struct PriceFeed {
    pub pair: TradingPair,
    pub price: u64,
    pub decimals: u8,
    pub timestamp: u64,
    pub source: PriceSource,
    pub confidence: f64,
}

/// Aggregated price from multiple sources.
#[derive(Debug, Clone)]
pub struct AggregatedPrice {
    pub price: u64,
    pub decimals: u8,
    pub sources: usize,
    pub confidence: f64,
    pub timestamp: u64,
}

/// Oracle errors.
#[derive(Debug, Clone, PartialEq)]
pub enum OracleError {
    PairNotFound,
    StalePrice,
    InsufficientSources,
    PriceDeviation,
    InvalidPrice,
}

impl std::fmt::Display for OracleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PairNotFound => write!(f, "Trading pair not found"),
            Self::StalePrice => write!(f, "Price data is stale"),
            Self::InsufficientSources => write!(f, "Insufficient price sources"),
            Self::PriceDeviation => write!(f, "Price deviation exceeds threshold"),
            Self::InvalidPrice => write!(f, "Invalid price value"),
        }
    }
}

/// Configuration for the price oracle.
#[derive(Debug, Clone)]
pub struct OracleConfig {
    /// Minimum number of sources required for aggregation.
    pub min_sources: usize,
    /// Maximum age in seconds before a price is considered stale.
    pub max_staleness_secs: u64,
    /// Maximum allowed percentage deviation between sources.
    pub max_deviation_percent: f64,
}

/// Entry in the TWAP circular buffer.
#[derive(Debug, Clone)]
struct TwapEntry {
    price: u64,
    timestamp: u64,
}

/// Per-pair state maintained by the oracle.
#[derive(Debug, Clone)]
struct PairState {
    /// Latest feeds per source
    feeds: HashMap<String, PriceFeed>,
    /// Registered sources for this pair
    registered_sources: Vec<PriceSource>,
    /// Circular buffer for TWAP computation
    twap_buffer: Vec<TwapEntry>,
    /// Max entries in the TWAP buffer
    twap_capacity: usize,
}

impl PairState {
    fn new(capacity: usize) -> Self {
        Self {
            feeds: HashMap::new(),
            registered_sources: Vec::new(),
            twap_buffer: Vec::new(),
            twap_capacity: capacity,
        }
    }

    fn push_twap(&mut self, price: u64, timestamp: u64) {
        if self.twap_buffer.len() >= self.twap_capacity {
            self.twap_buffer.remove(0);
        }
        self.twap_buffer.push(TwapEntry { price, timestamp });
    }
}

/// Multi-source price oracle with TWAP support.
pub struct PriceOracle {
    config: OracleConfig,
    pairs: HashMap<String, PairState>,
    twap_capacity: usize,
}

impl PriceOracle {
    /// Create a new price oracle with the given configuration.
    pub fn new(config: OracleConfig) -> Self {
        Self {
            config,
            pairs: HashMap::new(),
            twap_capacity: 1024,
        }
    }

    /// Register a price source for a trading pair.
    pub fn register_source(&mut self, pair: TradingPair, source: PriceSource) {
        let key = pair.key();
        let state = self
            .pairs
            .entry(key)
            .or_insert_with(|| PairState::new(self.twap_capacity));
        if !state.registered_sources.contains(&source) {
            state.registered_sources.push(source);
        }
    }

    /// Update a price feed for a trading pair from a specific source.
    pub fn update_price(&mut self, feed: PriceFeed) -> Result<(), OracleError> {
        if feed.price == 0 {
            return Err(OracleError::InvalidPrice);
        }
        if feed.confidence < 0.0 || feed.confidence > 1.0 {
            return Err(OracleError::InvalidPrice);
        }

        let key = feed.pair.key();
        let state = self
            .pairs
            .entry(key)
            .or_insert_with(|| PairState::new(self.twap_capacity));

        // Add to TWAP buffer
        state.push_twap(feed.price, feed.timestamp);

        // Store latest feed keyed by source
        let source_key = format!("{:?}", feed.source);
        state.feeds.insert(source_key, feed);

        Ok(())
    }

    /// Get the aggregated price for a trading pair.
    ///
    /// Aggregates across all available sources, checking for minimum source
    /// count and maximum deviation.
    pub fn get_price(&self, pair: &TradingPair) -> Result<AggregatedPrice, OracleError> {
        let key = pair.key();
        let state = self.pairs.get(&key).ok_or(OracleError::PairNotFound)?;

        let feeds: Vec<&PriceFeed> = state.feeds.values().collect();
        if feeds.is_empty() {
            return Err(OracleError::PairNotFound);
        }
        if feeds.len() < self.config.min_sources {
            return Err(OracleError::InsufficientSources);
        }

        // Check deviation between sources
        let prices: Vec<u64> = feeds.iter().map(|f| f.price).collect();
        let min_price = *prices.iter().min().unwrap();
        let max_price = *prices.iter().max().unwrap();
        if min_price > 0 {
            let deviation = ((max_price - min_price) as f64 / min_price as f64) * 100.0;
            if deviation > self.config.max_deviation_percent {
                return Err(OracleError::PriceDeviation);
            }
        }

        // Weighted average by confidence
        let total_confidence: f64 = feeds.iter().map(|f| f.confidence).sum();
        let weighted_price = if total_confidence > 0.0 {
            let sum: f64 = feeds
                .iter()
                .map(|f| f.price as f64 * f.confidence)
                .sum();
            (sum / total_confidence) as u64
        } else {
            // Simple average as fallback
            let sum: u64 = prices.iter().sum();
            sum / prices.len() as u64
        };

        let avg_confidence = total_confidence / feeds.len() as f64;
        let latest_timestamp = feeds.iter().map(|f| f.timestamp).max().unwrap_or(0);
        let decimals = feeds[0].decimals;

        Ok(AggregatedPrice {
            price: weighted_price,
            decimals,
            sources: feeds.len(),
            confidence: avg_confidence,
            timestamp: latest_timestamp,
        })
    }

    /// Compute the time-weighted average price over a given window.
    ///
    /// Uses the circular buffer of historical price entries.
    /// The window_secs parameter specifies how far back from the latest
    /// entry to include.
    pub fn get_twap(
        &self,
        pair: &TradingPair,
        window_secs: u64,
    ) -> Result<u64, OracleError> {
        let key = pair.key();
        let state = self.pairs.get(&key).ok_or(OracleError::PairNotFound)?;

        if state.twap_buffer.is_empty() {
            return Err(OracleError::PairNotFound);
        }

        let latest_ts = state.twap_buffer.last().unwrap().timestamp;
        let cutoff = if latest_ts >= window_secs {
            latest_ts - window_secs
        } else {
            0
        };

        // Filter entries within the window
        let entries: Vec<&TwapEntry> = state
            .twap_buffer
            .iter()
            .filter(|e| e.timestamp >= cutoff)
            .collect();

        if entries.is_empty() {
            return Err(OracleError::InsufficientSources);
        }

        if entries.len() == 1 {
            return Ok(entries[0].price);
        }

        // Time-weighted average: weight each price by the duration until the next entry
        let mut weighted_sum: u128 = 0;
        let mut total_weight: u128 = 0;

        for i in 0..entries.len() - 1 {
            let duration = entries[i + 1].timestamp - entries[i].timestamp;
            if duration > 0 {
                weighted_sum += entries[i].price as u128 * duration as u128;
                total_weight += duration as u128;
            }
        }

        // Include the last entry with weight 1 if no time span was available
        if total_weight == 0 {
            // All entries have the same timestamp; simple average
            let sum: u64 = entries.iter().map(|e| e.price).sum();
            return Ok(sum / entries.len() as u64);
        }

        // Give the last entry a weight equal to the average interval
        let avg_interval = total_weight / (entries.len() as u128 - 1);
        weighted_sum += entries.last().unwrap().price as u128 * avg_interval;
        total_weight += avg_interval;

        Ok((weighted_sum / total_weight) as u64)
    }

    /// Check if the price data for a pair is stale at the given timestamp.
    pub fn is_stale(&self, pair: &TradingPair, now: u64) -> bool {
        let key = pair.key();
        match self.pairs.get(&key) {
            None => true,
            Some(state) => {
                if state.feeds.is_empty() {
                    return true;
                }
                let latest_ts = state.feeds.values().map(|f| f.timestamp).max().unwrap_or(0);
                now.saturating_sub(latest_ts) > self.config.max_staleness_secs
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> OracleConfig {
        OracleConfig {
            min_sources: 1,
            max_staleness_secs: 300,
            max_deviation_percent: 5.0,
        }
    }

    fn btc_usd() -> TradingPair {
        TradingPair::new("BTC", "USD")
    }

    fn eth_usd() -> TradingPair {
        TradingPair::new("ETH", "USD")
    }

    fn feed(pair: TradingPair, price: u64, source: PriceSource, ts: u64) -> PriceFeed {
        PriceFeed {
            pair,
            price,
            decimals: 8,
            timestamp: ts,
            source,
            confidence: 0.95,
        }
    }

    #[test]
    fn test_update_and_get_price_single_source() {
        let mut oracle = PriceOracle::new(default_config());
        oracle
            .update_price(feed(btc_usd(), 50_000_0000_0000, PriceSource::Chainlink, 1000))
            .unwrap();

        let agg = oracle.get_price(&btc_usd()).unwrap();
        assert_eq!(agg.price, 50_000_0000_0000);
        assert_eq!(agg.sources, 1);
        assert_eq!(agg.decimals, 8);
    }

    #[test]
    fn test_aggregation_multiple_sources() {
        let mut oracle = PriceOracle::new(default_config());
        oracle
            .update_price(PriceFeed {
                pair: btc_usd(),
                price: 50_000,
                decimals: 2,
                timestamp: 1000,
                source: PriceSource::Chainlink,
                confidence: 0.9,
            })
            .unwrap();
        oracle
            .update_price(PriceFeed {
                pair: btc_usd(),
                price: 50_100,
                decimals: 2,
                timestamp: 1001,
                source: PriceSource::Pyth,
                confidence: 0.8,
            })
            .unwrap();

        let agg = oracle.get_price(&btc_usd()).unwrap();
        assert_eq!(agg.sources, 2);
        // Weighted: (50000*0.9 + 50100*0.8) / (0.9+0.8) = (45000+40080)/1.7 = 50047
        assert!(agg.price >= 50_000 && agg.price <= 50_100);
    }

    #[test]
    fn test_insufficient_sources() {
        let mut config = default_config();
        config.min_sources = 3;
        let mut oracle = PriceOracle::new(config);
        oracle
            .update_price(feed(btc_usd(), 50_000, PriceSource::Chainlink, 1000))
            .unwrap();

        let result = oracle.get_price(&btc_usd());
        assert_eq!(result.unwrap_err(), OracleError::InsufficientSources);
    }

    #[test]
    fn test_price_deviation_error() {
        let mut config = default_config();
        config.max_deviation_percent = 1.0;
        let mut oracle = PriceOracle::new(config);

        oracle
            .update_price(feed(btc_usd(), 50_000, PriceSource::Chainlink, 1000))
            .unwrap();
        oracle
            .update_price(feed(btc_usd(), 55_000, PriceSource::Pyth, 1001))
            .unwrap();

        let result = oracle.get_price(&btc_usd());
        assert_eq!(result.unwrap_err(), OracleError::PriceDeviation);
    }

    #[test]
    fn test_invalid_price_zero() {
        let mut oracle = PriceOracle::new(default_config());
        let result = oracle.update_price(feed(btc_usd(), 0, PriceSource::Chainlink, 1000));
        assert_eq!(result.unwrap_err(), OracleError::InvalidPrice);
    }

    #[test]
    fn test_invalid_confidence() {
        let mut oracle = PriceOracle::new(default_config());
        let result = oracle.update_price(PriceFeed {
            pair: btc_usd(),
            price: 50_000,
            decimals: 8,
            timestamp: 1000,
            source: PriceSource::Chainlink,
            confidence: 1.5,
        });
        assert_eq!(result.unwrap_err(), OracleError::InvalidPrice);
    }

    #[test]
    fn test_pair_not_found() {
        let oracle = PriceOracle::new(default_config());
        let result = oracle.get_price(&eth_usd());
        assert_eq!(result.unwrap_err(), OracleError::PairNotFound);
    }

    #[test]
    fn test_is_stale() {
        let mut oracle = PriceOracle::new(default_config());
        oracle
            .update_price(feed(btc_usd(), 50_000, PriceSource::Chainlink, 1000))
            .unwrap();

        assert!(!oracle.is_stale(&btc_usd(), 1100));  // 100s < 300s
        assert!(!oracle.is_stale(&btc_usd(), 1300));  // 300s == 300s
        assert!(oracle.is_stale(&btc_usd(), 1301));   // 301s > 300s
        assert!(oracle.is_stale(&eth_usd(), 1000));   // not registered
    }

    #[test]
    fn test_register_source() {
        let mut oracle = PriceOracle::new(default_config());
        oracle.register_source(btc_usd(), PriceSource::Chainlink);
        oracle.register_source(btc_usd(), PriceSource::Pyth);
        // Duplicate registration should not add again
        oracle.register_source(btc_usd(), PriceSource::Chainlink);

        let state = oracle.pairs.get(&btc_usd().key()).unwrap();
        assert_eq!(state.registered_sources.len(), 2);
    }

    #[test]
    fn test_twap_basic() {
        let mut oracle = PriceOracle::new(default_config());

        // Feed prices at different timestamps
        oracle.update_price(feed(btc_usd(), 100, PriceSource::Chainlink, 1000)).unwrap();
        oracle.update_price(feed(btc_usd(), 200, PriceSource::Pyth, 1010)).unwrap();
        oracle.update_price(feed(btc_usd(), 150, PriceSource::InternalDex, 1020)).unwrap();

        let twap = oracle.get_twap(&btc_usd(), 100).unwrap();
        // Entries: (100, 1000), (200, 1010), (150, 1020)
        // Weights: 100*10=1000, 200*10=2000, 150*10=1500
        // Total weight: 30, sum: 4500 => 150
        assert!(twap > 0);
        assert!(twap >= 100 && twap <= 200);
    }

    #[test]
    fn test_twap_single_entry() {
        let mut oracle = PriceOracle::new(default_config());
        oracle.update_price(feed(btc_usd(), 42_000, PriceSource::Chainlink, 1000)).unwrap();

        let twap = oracle.get_twap(&btc_usd(), 100).unwrap();
        assert_eq!(twap, 42_000);
    }

    #[test]
    fn test_twap_pair_not_found() {
        let oracle = PriceOracle::new(default_config());
        let result = oracle.get_twap(&eth_usd(), 100);
        assert_eq!(result.unwrap_err(), OracleError::PairNotFound);
    }
}
