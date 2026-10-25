//! FIFO lot accounting.
//!
//! Every incoming transfer opens a lot priced at the BTC/USD rate of that
//! moment. Outgoing transfers consume the oldest lots first. Network fees
//! consume lots too, but with zero proceeds, so they show up as a realized
//! loss and never as a sale at market price.

use std::collections::VecDeque;

/// Satoshis per bitcoin.
pub const SATS_PER_BTC: f64 = 100_000_000.0;

/// A transaction effect together with the BTC/USD price when it happened.
#[derive(Debug, Clone, PartialEq)]
pub struct PricedDelta {
    /// Block time as a Unix timestamp.
    pub time: i64,
    /// Sats received minus sats spent. Negative for outgoing transfers.
    pub net_sats: i64,
    /// Network fee in sats paid by this address.
    pub fee_sats: u64,
    /// USD per BTC at `time`.
    pub price_usd: f64,
}

/// Coins acquired at one moment and not yet spent.
#[derive(Debug, Clone, PartialEq)]
pub struct Lot {
    /// Acquisition time as a Unix timestamp.
    pub time: i64,
    /// Remaining size in sats.
    pub sats: u64,
    /// USD per BTC when the lot was acquired.
    pub unit_price_usd: f64,
}

impl Lot {
    /// Remaining amount in BTC.
    pub fn btc(&self) -> f64 {
        self.sats as f64 / SATS_PER_BTC
    }

    /// What the remaining coins cost, in USD.
    pub fn cost_usd(&self) -> f64 {
        self.btc() * self.unit_price_usd
    }

    /// What the remaining coins are worth at `price_usd`.
    pub fn value_usd(&self, price_usd: f64) -> f64 {
        self.btc() * price_usd
    }

    /// Unrealized gain (positive) or loss (negative) at `price_usd`.
    pub fn unrealized_usd(&self, price_usd: f64) -> f64 {
        self.value_usd(price_usd) - self.cost_usd()
    }
}

/// The result of replaying a wallet history.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Position {
    /// Lots still held, oldest first.
    pub open_lots: Vec<Lot>,
    /// Gains and losses from coins that were spent.
    pub realized_usd: f64,
}

/// Headline numbers for a [`Position`] at one price.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// Total held, in sats.
    pub balance_sats: u64,
    /// What the held coins cost, in USD.
    pub cost_basis_usd: f64,
    /// What the held coins are worth now, in USD.
    pub value_usd: f64,
    /// `value_usd - cost_basis_usd`.
    pub unrealized_usd: f64,
    /// Unrealized change relative to cost, or `None` when the cost is zero.
    pub unrealized_pct: Option<f64>,
    /// Realized gain or loss, in USD.
    pub realized_usd: f64,
}

impl Position {
    /// Totals the open lots at the current price.
    pub fn summarize(&self, price_now_usd: f64) -> Summary {
        let cost_basis_usd: f64 = self.open_lots.iter().map(Lot::cost_usd).sum();
        let value_usd: f64 = self
            .open_lots
            .iter()
            .map(|l| l.value_usd(price_now_usd))
            .sum();
        let unrealized_usd = value_usd - cost_basis_usd;

        Summary {
            balance_sats: self.open_lots.iter().map(|l| l.sats).sum(),
            cost_basis_usd,
            value_usd,
            unrealized_usd,
            unrealized_pct: (cost_basis_usd > 0.0).then(|| unrealized_usd / cost_basis_usd * 100.0),
            realized_usd: self.realized_usd,
        }
    }
}

/// Removes up to `sats` from the front of `lots`.
///
/// Returns how many sats were actually taken and what they cost in USD. The
/// two differ only if the history spends more than it ever received.
fn take(lots: &mut VecDeque<Lot>, mut sats: u64) -> (u64, f64) {
    let mut taken = 0;
    let mut cost = 0.0;

    while sats > 0 {
        let Some(front) = lots.front_mut() else { break };
        let used = sats.min(front.sats);
        cost += used as f64 / SATS_PER_BTC * front.unit_price_usd;
        front.sats -= used;
        taken += used;
        sats -= used;
        if front.sats == 0 {
            lots.pop_front();
        }
    }

    (taken, cost)
}

/// Replays `deltas` (oldest first) into a [`Position`].
pub fn fifo(deltas: &[PricedDelta]) -> Position {
    let mut lots: VecDeque<Lot> = VecDeque::new();
    let mut realized_usd = 0.0;

    for d in deltas {
        if d.net_sats > 0 {
            lots.push_back(Lot {
                time: d.time,
                sats: d.net_sats.unsigned_abs(),
                unit_price_usd: d.price_usd,
            });
            continue;
        }

        let outflow = d.net_sats.unsigned_abs();
        let fee = d.fee_sats.min(outflow);

        let (sold, sold_cost) = take(&mut lots, outflow - fee);
        realized_usd += sold as f64 / SATS_PER_BTC * d.price_usd - sold_cost;

        let (_, fee_cost) = take(&mut lots, fee);
        realized_usd -= fee_cost;
    }

    Position {
        open_lots: lots.into(),
        realized_usd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BTC: i64 = 100_000_000;

    fn delta(net_sats: i64, fee_sats: u64, price_usd: f64) -> PricedDelta {
        PricedDelta {
            time: 0,
            net_sats,
            fee_sats,
            price_usd,
        }
    }

    #[test]
    fn single_buy_has_unrealized_gain() {
        let pos = fifo(&[delta(BTC, 0, 10_000.0)]);
        let s = pos.summarize(15_000.0);
        assert_eq!(s.balance_sats, BTC as u64);
        assert_eq!(s.cost_basis_usd, 10_000.0);
        assert_eq!(s.unrealized_usd, 5_000.0);
        assert_eq!(s.unrealized_pct, Some(50.0));
        assert_eq!(s.realized_usd, 0.0);
    }

    #[test]
    fn unrealized_loss_is_negative() {
        let s = fifo(&[delta(BTC, 0, 10_000.0)]).summarize(7_500.0);
        assert_eq!(s.unrealized_usd, -2_500.0);
        assert_eq!(s.unrealized_pct, Some(-25.0));
    }

    #[test]
    fn sale_consumes_the_oldest_lot_first() {
        // Buy 1 BTC at 10k, buy 1 BTC at 20k, sell 1.5 BTC at 30k.
        let pos = fifo(&[
            delta(BTC, 0, 10_000.0),
            delta(BTC, 0, 20_000.0),
            delta(-(BTC + BTC / 2), 0, 30_000.0),
        ]);

        // 1.0 @ 10k and 0.5 @ 20k are consumed: cost 20k, proceeds 45k.
        assert_eq!(pos.realized_usd, 25_000.0);
        assert_eq!(pos.open_lots.len(), 1);
        assert_eq!(pos.open_lots[0].sats, (BTC / 2) as u64);
        assert_eq!(pos.open_lots[0].unit_price_usd, 20_000.0);
    }

    #[test]
    fn fee_is_a_loss_not_a_sale() {
        // Buy 1 BTC at 10k, send 0.5 BTC away paying a 0.01 BTC fee, price 10k.
        let fee = BTC / 100;
        let pos = fifo(&[
            delta(BTC, 0, 10_000.0),
            delta(-(BTC / 2 + fee), fee as u64, 10_000.0),
        ]);

        // The 0.5 BTC sent breaks even; the fee loses 0.01 * 10k = 100.
        assert!((pos.realized_usd + 100.0).abs() < 1e-9);
        assert_eq!(pos.open_lots[0].sats, (BTC / 2 - fee) as u64);
    }

    #[test]
    fn empty_history_has_no_percentage() {
        let s = fifo(&[]).summarize(50_000.0);
        assert_eq!(s.balance_sats, 0);
        assert_eq!(s.unrealized_pct, None);
    }

    #[test]
    fn overspending_does_not_panic() {
        let pos = fifo(&[delta(-BTC, 0, 10_000.0)]);
        assert!(pos.open_lots.is_empty());
        assert_eq!(pos.realized_usd, 0.0);
    }
}
