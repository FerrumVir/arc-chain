use std::collections::HashMap;

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

#[derive(Debug, Clone, PartialEq)]
pub enum PriceSource {
    Chainlink,
    Pyth,
    UniswapTWAP,
    InternalDex,
    Aggregated,
}

#[derive(Debug, Clone)]
pub struct PriceFeed {
    pub pair: TradingPair,
    pub price: u64,
    pub decimals: u8,
    pub timestamp: u64,
    pub source: PriceSource,
    pub confidence: f64,
}

#[derive(Debug, Clone)]
pub struct AggregatedPrice {
    pub price: u64,
    pub decimals: u8,
    pub sources: usize,
    pub confidence: f64,
    pub timestamp: u64,
}

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

#[derive(Debug, Clone)]
pub struct OracleConfig {
    pub min_sources: usize,
    pub max_staleness_secs: u64,
    pub max_deviation_percent: f64,
}

#[derive(Debug, Clone)]
struct TwapEntry {
    price: u64,
    timestamp: u64,
}

#[derive(Debug, Clone)]
struct PairState {
    feeds: HashMap<String, PriceFeed>,
    registered_sources: Vec<PriceSource>,
    twap_buffer: Vec<TwapEntry>,
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

pub struct PriceOracle {
    config: OracleConfig,
    pairs: HashMap<String, PairState>,
    twap_capacity: usize,
}

impl PriceOracle {
    pub fn new(config: OracleConfig) -> Self {
        Self {
            config,
            pairs: HashMap::new(),
            twap_capacity: 1024,
        }
    }

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

        state.push_twap(feed.price, feed.timestamp);

        let source_key = format!("{:?}", feed.source);
        state.feeds.insert(source_key, feed);

        Ok(())
    }

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

        let prices: Vec<u64> = feeds.iter().map(|f| f.price).collect();
        let min_price = *prices.iter().min().unwrap();
        let max_price = *prices.iter().max().unwrap();
        if min_price > 0 {
            let deviation = ((max_price - min_price) as f64 / min_price as f64) * 100.0;
            if deviation > self.config.max_deviation_percent {
                return Err(OracleError::PriceDeviation);
            }
        }

        let total_confidence: f64 = feeds.iter().map(|f| f.confidence).sum();
        let weighted_price = if total_confidence > 0.0 {
            let sum: f64 = feeds.iter().map(|f| f.price as f64 * f.confidence).sum();
            (sum / total_confidence) as u64
        } else {
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

    pub fn get_twap(&self, pair: &TradingPair, window_secs: u64) -> Result<u64, OracleError> {
        let key = pair.key();
        let state = self.pairs.get(&key).ok_or(OracleError::PairNotFound)?;

        if state.twap_buffer.is_empty() {
            return Err(OracleError::PairNotFound);
        }

        let latest_ts = state.twap_buffer.last().unwrap().timestamp;
        let cutoff = latest_ts.saturating_sub(window_secs);

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

        let mut weighted_sum: u128 = 0;
        let mut total_weight: u128 = 0;

        for i in 0..entries.len() - 1 {
            let duration = entries[i + 1].timestamp - entries[i].timestamp;
            if duration > 0 {
                weighted_sum += entries[i].price as u128 * duration as u128;
                total_weight += duration as u128;
            }
        }

        if total_weight == 0 {
            let sum: u64 = entries.iter().map(|e| e.price).sum();
            return Ok(sum / entries.len() as u64);
        }

        let avg_interval = total_weight / (entries.len() as u128 - 1);
        weighted_sum += entries.last().unwrap().price as u128 * avg_interval;
        total_weight += avg_interval;

        Ok((weighted_sum / total_weight) as u64)
    }

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
            .update_price(feed(
                btc_usd(),
                5_000_000_000_000,
                PriceSource::Chainlink,
                1000,
            ))
            .unwrap();

        let agg = oracle.get_price(&btc_usd()).unwrap();
        assert_eq!(agg.price, 5_000_000_000_000);
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

        assert!(!oracle.is_stale(&btc_usd(), 1100));
        assert!(!oracle.is_stale(&btc_usd(), 1300));
        assert!(oracle.is_stale(&btc_usd(), 1301));
        assert!(oracle.is_stale(&eth_usd(), 1000));
    }

    #[test]
    fn test_register_source() {
        let mut oracle = PriceOracle::new(default_config());
        oracle.register_source(btc_usd(), PriceSource::Chainlink);
        oracle.register_source(btc_usd(), PriceSource::Pyth);
        oracle.register_source(btc_usd(), PriceSource::Chainlink);

        let state = oracle.pairs.get(&btc_usd().key()).unwrap();
        assert_eq!(state.registered_sources.len(), 2);
    }

    #[test]
    fn test_twap_basic() {
        let mut oracle = PriceOracle::new(default_config());

        oracle
            .update_price(feed(btc_usd(), 100, PriceSource::Chainlink, 1000))
            .unwrap();
        oracle
            .update_price(feed(btc_usd(), 200, PriceSource::Pyth, 1010))
            .unwrap();
        oracle
            .update_price(feed(btc_usd(), 150, PriceSource::InternalDex, 1020))
            .unwrap();

        let twap = oracle.get_twap(&btc_usd(), 100).unwrap();
        assert!(twap > 0);
        assert!((100..=200).contains(&twap));
    }

    #[test]
    fn test_twap_single_entry() {
        let mut oracle = PriceOracle::new(default_config());
        oracle
            .update_price(feed(btc_usd(), 42_000, PriceSource::Chainlink, 1000))
            .unwrap();

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
