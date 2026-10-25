//! USD prices of each chain's native coin, with a local cache.

use serde::Deserialize;

use crate::chain::Chain;
use crate::db::Database;
use crate::util::date_string;
use crate::{Error, Result};

const MEMPOOL_URL: &str = "https://mempool.space/api/v1";
const COINBASE_CANDLES_URL: &str = "https://api.exchange.coinbase.com/products/ETH-USD/candles";
const COINBASE_SPOT_URL: &str = "https://api.coinbase.com/v2/prices/ETH-USD/spot";
const SECONDS_PER_DAY: i64 = 86_400;
/// Daily candles Coinbase returns per request, at most.
const CANDLES_PER_REQUEST: i64 = 300;

/// Anything that can quote a chain's native coin in USD.
pub trait PriceSource {
    /// Price of one whole coin at the given Unix timestamp.
    fn usd_at(&self, chain: Chain, time: i64) -> Result<f64>;
    /// Current price of one whole coin.
    fn usd_now(&self, chain: Chain) -> Result<f64>;

    /// Price for the UTC day with this number (days since the Unix epoch),
    /// plus the prices of other days the source came across on the way, as
    /// `(day, price)`. A source that serves many days in one request returns
    /// them here so they are not asked for again. By default only the day
    /// itself.
    fn usd_days(&self, chain: Chain, day: i64) -> Result<Vec<(i64, f64)>> {
        Ok(vec![(day, self.usd_at(chain, day * SECONDS_PER_DAY)?)])
    }
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

#[derive(Debug, Deserialize)]
struct SpotResponse {
    data: SpotData,
}

#[derive(Debug, Deserialize)]
struct SpotData {
    /// A decimal number written as text.
    amount: String,
}

/// One daily candle from Coinbase: time (start of the UTC day), low, high,
/// open, close and volume.
type Candle = (i64, f64, f64, f64, f64, f64);

/// Live Ether prices from Coinbase, with no caching. Other chains are not
/// covered.
///
/// A day's price is the middle of its opening and closing price. That is
/// coarser than Bitcoin's price at the minute, which is fine for an estimate.
pub struct CoinbasePrices;

impl PriceSource for CoinbasePrices {
    fn usd_at(&self, chain: Chain, time: i64) -> Result<f64> {
        let day = time.div_euclid(SECONDS_PER_DAY);
        self.usd_days(chain, day)?
            .into_iter()
            .find(|(candle_day, _)| *candle_day == day)
            .map(|(_, usd)| usd)
            .ok_or(Error::NoPrice(time))
    }

    fn usd_now(&self, chain: Chain) -> Result<f64> {
        require_ethereum(chain)?;
        let resp: SpotResponse = ureq::get(COINBASE_SPOT_URL)
            .call()?
            .body_mut()
            .read_json()?;
        resp.data
            .amount
            .parse()
            .map_err(|_| Error::BadResponse("the price is not a number"))
    }

    /// Asks for `day` and the following days, up to the most one request
    /// returns.
    fn usd_days(&self, chain: Chain, day: i64) -> Result<Vec<(i64, f64)>> {
        require_ethereum(chain)?;
        let start = day * SECONDS_PER_DAY;
        let last = day + CANDLES_PER_REQUEST - 1;
        let url = format!(
            "{COINBASE_CANDLES_URL}?granularity={SECONDS_PER_DAY}&start={}T00:00:00Z&end={}T00:00:00Z",
            date_string(start),
            date_string(last * SECONDS_PER_DAY),
        );
        let candles: Vec<Candle> = ureq::get(&url).call()?.body_mut().read_json()?;
        Ok(days_from_candles(&candles))
    }
}

/// The price of each day in `candles`: the middle of open and close.
fn days_from_candles(candles: &[Candle]) -> Vec<(i64, f64)> {
    candles
        .iter()
        .map(|&(time, _low, _high, open, close, _volume)| {
            (time.div_euclid(SECONDS_PER_DAY), (open + close) / 2.0)
        })
        .collect()
}

fn require_ethereum(chain: Chain) -> Result<()> {
    if chain == Chain::Ethereum {
        Ok(())
    } else {
        Err(Error::UnsupportedChain(chain.name()))
    }
}

/// Live prices for every chain the app can track: each chain goes to the
/// service that knows it.
pub struct LivePrices;

impl PriceSource for LivePrices {
    fn usd_at(&self, chain: Chain, time: i64) -> Result<f64> {
        match chain {
            Chain::Ethereum => CoinbasePrices.usd_at(chain, time),
            _ => MempoolPrices.usd_at(chain, time),
        }
    }

    fn usd_now(&self, chain: Chain) -> Result<f64> {
        match chain {
            Chain::Ethereum => CoinbasePrices.usd_now(chain),
            _ => MempoolPrices.usd_now(chain),
        }
    }

    fn usd_days(&self, chain: Chain, day: i64) -> Result<Vec<(i64, f64)>> {
        match chain {
            Chain::Ethereum => CoinbasePrices.usd_days(chain, day),
            _ => MempoolPrices.usd_days(chain, day),
        }
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
        // The source may return more days than asked for, so keep them all.
        let mut wanted = None;
        for (found_day, usd) in self.source.usd_days(chain, day)? {
            self.cache.put_price(chain, found_day, usd)?;
            if found_day == day {
                wanted = Some(usd);
            }
        }
        wanted.ok_or(Error::NoPrice(time))
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

    #[test]
    fn a_day_is_the_middle_of_open_and_close() {
        let candles = [
            (1_620_777_600, 3700.0, 4384.43, 4179.77, 3826.37, 617_506.0),
            (1_620_691_200, 3760.0, 4183.25, 3949.45, 4179.76, 371_785.0),
        ];
        let days = days_from_candles(&candles);
        assert_eq!(days[0], (18_759, (4179.77 + 3826.37) / 2.0));
        assert_eq!(days[1].0, 18_758);
    }

    #[test]
    fn candles_are_read_from_coinbase_json() {
        let json = "[[1620777600,3700,4384.43,4179.77,3826.37,617506.79],[1620691200,3760,4183.25,3949.45,4179.76,371785.2]]";
        let candles: Vec<Candle> = serde_json::from_str(json).expect("candles parse");
        assert_eq!(candles.len(), 2);
        assert_eq!(candles[0].0, 1_620_777_600);
    }

    #[test]
    fn spot_price_is_read_from_text() {
        let resp: SpotResponse =
            serde_json::from_str(r#"{"data":{"amount":"2359.675","base":"ETH","currency":"USD"}}"#)
                .expect("spot parses");
        assert_eq!(resp.data.amount.parse::<f64>().ok(), Some(2359.675));
    }

    #[test]
    fn coinbase_only_prices_ether() {
        assert!(matches!(
            CoinbasePrices.usd_now(Chain::Bitcoin),
            Err(Error::UnsupportedChain("Bitcoin"))
        ));
        assert!(matches!(
            CoinbasePrices.usd_days(Chain::Bitcoin, 0),
            Err(Error::UnsupportedChain(_))
        ));
    }

    /// Returns a whole week for any day asked for, like Coinbase does.
    struct Weekly(Cell<u32>);

    impl PriceSource for &Weekly {
        fn usd_at(&self, _chain: Chain, _time: i64) -> Result<f64> {
            Err(Error::NoPrice(0))
        }

        fn usd_now(&self, _chain: Chain) -> Result<f64> {
            Ok(0.0)
        }

        fn usd_days(&self, _chain: Chain, day: i64) -> Result<Vec<(i64, f64)>> {
            self.0.set(self.0.get() + 1);
            Ok((day..day + 7).map(|d| (d, d as f64)).collect())
        }
    }

    #[test]
    fn days_that_come_along_are_cached_too() {
        let source = Weekly(Cell::new(0));
        let cache = Database::in_memory().expect("in-memory db opens");
        let prices = CachedPrices::new(&source, &cache);

        let first = 19_000 * SECONDS_PER_DAY;
        assert_eq!(
            prices.usd_at(Chain::Ethereum, first).expect("price"),
            19_000.0
        );
        // Three days later was in the same answer.
        let later = first + 3 * SECONDS_PER_DAY;
        assert_eq!(
            prices.usd_at(Chain::Ethereum, later).expect("price"),
            19_003.0
        );
        assert_eq!(source.0.get(), 1);
        // Eight days later was not.
        prices
            .usd_at(Chain::Ethereum, first + 8 * SECONDS_PER_DAY)
            .expect("price");
        assert_eq!(source.0.get(), 2);
    }

    #[test]
    fn live_prices_send_each_chain_to_its_service() {
        // Neither call is made here: an unknown chain is refused before any
        // request, which shows the routing falls through to Bitcoin's service.
        assert!(matches!(
            LivePrices.usd_now(Chain::Fake),
            Err(Error::UnsupportedChain("Fake Chain"))
        ));
    }
}
