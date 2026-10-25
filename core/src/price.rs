//! BTC/USD prices from the mempool.space API, with a local cache.

use serde::Deserialize;

use crate::db::Database;
use crate::{Error, Result};

const BASE_URL: &str = "https://mempool.space/api/v1";
const SECONDS_PER_DAY: i64 = 86_400;

/// Anything that can quote BTC in USD.
pub trait PriceSource {
    /// Price at the given Unix timestamp.
    fn usd_at(&self, time: i64) -> Result<f64>;
    /// Current price.
    fn usd_now(&self) -> Result<f64>;
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

/// Live prices straight from the API, with no caching.
pub struct MempoolPrices;

impl PriceSource for MempoolPrices {
    fn usd_at(&self, time: i64) -> Result<f64> {
        let url = format!("{BASE_URL}/historical-price?currency=USD&timestamp={time}");
        let resp: HistoricalResponse = ureq::get(&url).call()?.body_mut().read_json()?;
        resp.prices
            .first()
            .map(|p| p.usd)
            .ok_or(Error::NoPrice(time))
    }

    fn usd_now(&self) -> Result<f64> {
        let resp: CurrentResponse = ureq::get(&format!("{BASE_URL}/prices"))
            .call()?
            .body_mut()
            .read_json()?;
        Ok(resp.usd)
    }
}

/// Wraps a [`PriceSource`] so each calendar day is fetched only once.
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
    fn usd_at(&self, time: i64) -> Result<f64> {
        let day = time.div_euclid(SECONDS_PER_DAY);
        if let Some(usd) = self.cache.price(day)? {
            return Ok(usd);
        }
        let usd = self.source.usd_at(time)?;
        self.cache.put_price(day, usd)?;
        Ok(usd)
    }

    fn usd_now(&self) -> Result<f64> {
        self.source.usd_now()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// Counts calls so the test can prove the cache is used.
    struct Counting(Cell<u32>);

    impl PriceSource for &Counting {
        fn usd_at(&self, _time: i64) -> Result<f64> {
            self.0.set(self.0.get() + 1);
            Ok(100.0)
        }

        fn usd_now(&self) -> Result<f64> {
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
        assert_eq!(prices.usd_at(morning).expect("price"), 100.0);
        assert_eq!(prices.usd_at(evening).expect("price"), 100.0);
        assert_eq!(source.0.get(), 1);

        prices.usd_at(morning + SECONDS_PER_DAY).expect("price");
        assert_eq!(source.0.get(), 2);
    }
}
