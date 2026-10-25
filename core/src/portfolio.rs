//! Ties history, prices and lots together into one view of a wallet.

use crate::Result;
use crate::chain::{self, Delta};
use crate::lots::{self, Lot, PricedDelta, Summary};
use crate::price::PriceSource;

/// Everything a front end needs to show one wallet.
#[derive(Debug, Clone)]
pub struct Portfolio {
    /// The address that was tracked.
    pub address: String,
    /// BTC/USD at the time the portfolio was built.
    pub price_now_usd: f64,
    /// Headline numbers.
    pub summary: Summary,
    /// Lots still held, oldest first.
    pub lots: Vec<Lot>,
    /// Every confirmed transaction with its price, oldest first.
    pub history: Vec<(Delta, f64)>,
}

/// Fetches the history of `address` and values it.
///
/// `on_progress` receives the number of transactions fetched so far.
pub fn build(
    address: &str,
    prices: &impl PriceSource,
    on_progress: impl FnMut(usize),
) -> Result<Portfolio> {
    let deltas = chain::fetch_history(address, on_progress)?;

    let history = deltas
        .into_iter()
        .map(|d| prices.usd_at(d.time).map(|p| (d, p)))
        .collect::<Result<Vec<_>>>()?;

    let priced: Vec<PricedDelta> = history
        .iter()
        .map(|(d, price_usd)| PricedDelta {
            time: d.time,
            net_sats: d.net_sats,
            fee_sats: d.fee_sats,
            price_usd: *price_usd,
        })
        .collect();

    let position = lots::fifo(&priced);
    let price_now_usd = prices.usd_now()?;

    Ok(Portfolio {
        address: address.to_owned(),
        price_now_usd,
        summary: position.summarize(price_now_usd),
        lots: position.open_lots,
        history,
    })
}
