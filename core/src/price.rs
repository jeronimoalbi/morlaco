//! USD prices of each chain's native coin, with a local cache.

use serde::Deserialize;

use crate::chain::Chain;
use crate::db::Database;
use crate::{Error, Result};

const MEMPOOL_URL: &str = "https://mempool.space/api/v1";
const SECONDS_PER_DAY: i64 = 86_400;

/// Anything that can quote a chain's native coin in USD.
pub trait PriceSource {
    /// Price of one whole coin at the given Unix timestamp.
    fn usd_at(&self, chain: Chain, time: i64) -> Result<f64>;
    /// Current price of one whole coin.
    fn usd_now(&self, chain: Chain) -> Result<f64>;
}

#[derive(Debug, Deserialize)]
struct HistoricalResponse {
    prices: Vec<HistoricalPoint>,
}

#[derive(Debug, Deserialize)]
struct HistoricalPoint {
    #[serde(rename = "USD")]
    usd: f64,
}

#[derive(Debug, Deserialize)]
struct CurrentResponse {
    #[serde(rename = "USD")]
    usd: f64,
}

/// Live Bitcoin prices straight from mempool.space, with no caching. Other
/// chains are not covered.
pub struct MempoolPrices;

impl PriceSource for MempoolPrices {
    fn usd_at(&self, chain: Chain, time: i64) -> Result<f64> {
        require_bitcoin(chain)?;
        let url = format!("{MEMPOOL_URL}/historical-price?currency=USD&timestamp={time}");
        let resp: HistoricalResponse = ureq::get(&url).call()?.body_mut().read_json()?;
        resp.prices
            .first()
            .map(|p| p.usd)
            .ok_or(Error::NoPrice(time))
    }

    fn usd_now(&self, chain: Chain) -> Result<f64> {
        require_bitcoin(chain)?;
        let resp: CurrentResponse = ureq::get(&format!("{MEMPOOL_URL}/prices"))
            .call()?
            .body_mut()
            .read_json()?;
        Ok(resp.usd)
    }
}

fn require_bitcoin(chain: Chain) -> Result<()> {
    if chain == Chain::Bitcoin {
        Ok(())
    } else {
        Err(Error::UnsupportedChain(chain.name()))
    }
}

/// Wraps a [`PriceSource`] so each chain's calendar day is fetched only once.
///
/// Prices are cached at day granularity, which is plenty for a cost-basis
/// estimate. The current price is never cached.
pub struct CachedPrices<'a, S> {
    source: S,
    cache: &'a Database,
}

impl<'a, S: PriceSource> CachedPrices<'a, S> {
    /// Combines a price `source` with a `cache`.
    pub fn new(source: S, cache: &'a Database) -> Self {
        Self { source, cache }
    }
}

impl<S: PriceSource> PriceSource for CachedPrices<'_, S> {
    fn usd_at(&self, chain: Chain, time: i64) -> Result<f64> {
        let day = time.div_euclid(SECONDS_PER_DAY);
        if let Some(usd) = self.cache.price(chain, day)? {
            return Ok(usd);
        }
        let usd = self.source.usd_at(chain, time)?;
        self.cache.put_price(chain, day, usd)?;
        Ok(usd)
    }

    fn usd_now(&self, chain: Chain) -> Result<f64> {
        self.source.usd_now(chain)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// Counts calls so the test can prove the cache is used.
    struct Counting(Cell<u32>);

    impl PriceSource for &Counting {
        fn usd_at(&self, _chain: Chain, _time: i64) -> Result<f64> {
            self.0.set(self.0.get() + 1);
            Ok(100.0)
        }

        fn usd_now(&self, _chain: Chain) -> Result<f64> {
            Ok(200.0)
        }
    }

    #[test]
    fn same_day_hits_the_source_once() {
        let source = Counting(Cell::new(0));
        let cache = Database::in_memory().expect("in-memory db opens");
        let prices = CachedPrices::new(&source, &cache);

        let morning = 1_700_000_000;
        let evening = morning + 3_600;
        assert_eq!(
            prices.usd_at(Chain::Bitcoin, morning).expect("price"),
            100.0
        );
        assert_eq!(
            prices.usd_at(Chain::Bitcoin, evening).expect("price"),
            100.0
        );
        assert_eq!(source.0.get(), 1);

        prices
            .usd_at(Chain::Bitcoin, morning + SECONDS_PER_DAY)
            .expect("price");
        assert_eq!(source.0.get(), 2);
    }

    #[test]
    fn each_chain_has_its_own_cached_price() {
        let source = Counting(Cell::new(0));
        let cache = Database::in_memory().expect("in-memory db opens");
        let prices = CachedPrices::new(&source, &cache);

        prices.usd_at(Chain::Bitcoin, 1_700_000_000).expect("price");
        prices.usd_at(Chain::Fake, 1_700_000_000).expect("price");
        // The same day on another chain is a different price.
        assert_eq!(source.0.get(), 2);
    }

    #[test]
    fn mempool_only_prices_bitcoin() {
        assert!(matches!(
            MempoolPrices.usd_now(Chain::Fake),
            Err(Error::UnsupportedChain("Fake Chain"))
        ));
        assert!(matches!(
            MempoolPrices.usd_at(Chain::Fake, 0),
            Err(Error::UnsupportedChain(_))
        ));
    }
}
