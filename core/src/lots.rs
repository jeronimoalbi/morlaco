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

impl Summary {
    /// Adds several summaries together, e.g. one per wallet.
    ///
    /// The percentage is recomputed from the combined cost, not averaged.
    pub fn combine<'a>(items: impl IntoIterator<Item = &'a Summary>) -> Summary {
        let mut total = Summary {
            balance_sats: 0,
            cost_basis_usd: 0.0,
            value_usd: 0.0,
            unrealized_usd: 0.0,
            unrealized_pct: None,
            realized_usd: 0.0,
        };
        for s in items {
            total.balance_sats += s.balance_sats;
            total.cost_basis_usd += s.cost_basis_usd;
            total.value_usd += s.value_usd;
            total.unrealized_usd += s.unrealized_usd;
            total.realized_usd += s.realized_usd;
        }
        total.unrealized_pct = (total.cost_basis_usd > 0.0)
            .then(|| total.unrealized_usd / total.cost_basis_usd * 100.0);
        total
    }
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

/// Removes up to `sats` from the front of `lots` and returns the removed
/// pieces, each keeping the date and price of the lot it came from.
///
/// Returns less than `sats` only if the history spends more than it ever
/// received.
fn take(lots: &mut VecDeque<Lot>, mut sats: u64) -> Vec<Lot> {
    let mut pieces = Vec::new();

    while sats > 0 {
        let Some(front) = lots.front_mut() else { break };
        let used = sats.min(front.sats);
        pieces.push(Lot {
            time: front.time,
            sats: used,
            unit_price_usd: front.unit_price_usd,
        });
        front.sats -= used;
        sats -= used;
        if front.sats == 0 {
            lots.pop_front();
        }
    }

    pieces
}

fn total_sats(pieces: &[Lot]) -> u64 {
    pieces.iter().map(|l| l.sats).sum()
}

fn total_cost(pieces: &[Lot]) -> f64 {
    pieces.iter().map(Lot::cost_usd).sum()
}

/// Adds `lot` keeping the queue ordered by acquisition time, so FIFO keeps
/// meaning "oldest coins first" after coins arrive from another wallet.
fn insert_by_age(lots: &mut VecDeque<Lot>, lot: Lot) {
    let at = lots.partition_point(|l| l.time <= lot.time);
    lots.insert(at, lot);
}

/// One wallet's side of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Move {
    /// Index of the wallet in the [`Ledger`].
    pub wallet: usize,
    /// Sats received minus sats spent. Negative when the wallet paid out.
    pub net_sats: i64,
    /// Share of the network fee paid from this wallet.
    pub fee_sats: u64,
}

/// FIFO lots for several wallets at once.
///
/// When one transaction moves coins out of one tracked wallet and into
/// another, the coins keep their original date and price instead of being
/// booked as a sale and a purchase.
#[derive(Debug, Clone)]
pub struct Ledger {
    lots: Vec<VecDeque<Lot>>,
    realized_usd: Vec<f64>,
}

impl Ledger {
    /// Creates a ledger for `wallets` empty wallets.
    pub fn new(wallets: usize) -> Self {
        Self {
            lots: vec![VecDeque::new(); wallets],
            realized_usd: vec![0.0; wallets],
        }
    }

    /// Applies one transaction. `moves` lists every tracked wallet it touches
    /// and each wallet may appear once.
    ///
    /// Returns, for each move, how many sats went to or came from another
    /// tracked wallet.
    pub fn apply(&mut self, moves: &[Move], time: i64, price_usd: f64) -> Vec<u64> {
        let sendable = |m: &Move| {
            let out = m.net_sats.unsigned_abs();
            out - m.fee_sats.min(out)
        };
        let senders = || moves.iter().filter(|m| m.net_sats < 0);
        let receivers = || moves.iter().filter(|m| m.net_sats > 0);

        let total_out: u64 = senders().map(sendable).sum();
        let total_in: u64 = receivers().map(|m| m.net_sats.unsigned_abs()).sum();
        let mut internal_left = total_out.min(total_in);

        let mut internal = vec![0; moves.len()];
        let mut in_transit: VecDeque<Lot> = VecDeque::new();

        // Senders: coins for other wallets move; the rest is sold; fees are lost.
        for (i, m) in moves.iter().enumerate().filter(|(_, m)| m.net_sats < 0) {
            let moved = sendable(m).min(internal_left);
            internal_left -= moved;
            internal[i] = moved;

            let pieces = take(&mut self.lots[m.wallet], moved);
            in_transit.extend(pieces);

            let sold = take(&mut self.lots[m.wallet], sendable(m) - moved);
            self.realized_usd[m.wallet] +=
                total_sats(&sold) as f64 / SATS_PER_BTC * price_usd - total_cost(&sold);

            let out = m.net_sats.unsigned_abs();
            let fee = take(&mut self.lots[m.wallet], m.fee_sats.min(out));
            self.realized_usd[m.wallet] -= total_cost(&fee);
        }

        // Receivers: coins from other wallets keep their basis; the rest is
        // a purchase at the current price.
        let mut to_assign = total_out.min(total_in);
        for (i, m) in moves.iter().enumerate().filter(|(_, m)| m.net_sats > 0) {
            let inflow = m.net_sats.unsigned_abs();
            let share = inflow.min(to_assign);
            to_assign -= share;
            internal[i] = share;

            let pieces = take(&mut in_transit, share);
            let given = total_sats(&pieces);
            for piece in pieces {
                insert_by_age(&mut self.lots[m.wallet], piece);
            }
            // Coins from outside, or that no sender had lots for because its
            // history is incomplete, are priced at market.
            let bought = inflow - given;
            if bought > 0 {
                insert_by_age(
                    &mut self.lots[m.wallet],
                    Lot {
                        time,
                        sats: bought,
                        unit_price_usd: price_usd,
                    },
                );
            }
        }

        internal
    }

    /// Finishes and returns each wallet's position, in wallet order.
    pub fn into_positions(self) -> Vec<Position> {
        self.lots
            .into_iter()
            .zip(self.realized_usd)
            .map(|(lots, realized_usd)| Position {
                open_lots: lots.into(),
                realized_usd,
            })
            .collect()
    }
}

/// Replays `deltas` (oldest first) for a single wallet into a [`Position`].
pub fn fifo(deltas: &[PricedDelta]) -> Position {
    let mut ledger = Ledger::new(1);
    for d in deltas {
        let moves = [Move {
            wallet: 0,
            net_sats: d.net_sats,
            fee_sats: d.fee_sats,
        }];
        ledger.apply(&moves, d.time, d.price_usd);
    }
    ledger.into_positions().pop().unwrap_or_default()
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
    fn combined_summary_adds_up_and_reweights_the_percentage() {
        // 1 BTC bought at 10k (+100%) and 1 BTC bought at 30k (-33%), price 20k.
        let a = fifo(&[delta(BTC, 0, 10_000.0)]).summarize(20_000.0);
        let b = fifo(&[delta(BTC, 0, 30_000.0)]).summarize(20_000.0);

        let total = Summary::combine([&a, &b]);
        assert_eq!(total.balance_sats, 2 * BTC as u64);
        assert_eq!(total.cost_basis_usd, 40_000.0);
        assert_eq!(total.value_usd, 40_000.0);
        assert_eq!(total.unrealized_usd, 0.0);
        assert_eq!(total.unrealized_pct, Some(0.0));
    }

    fn mv(wallet: usize, net_sats: i64, fee_sats: u64) -> Move {
        Move {
            wallet,
            net_sats,
            fee_sats,
        }
    }

    /// Wallet 0 buys 1 BTC at 10k at time 1.
    fn funded_ledger() -> Ledger {
        let mut ledger = Ledger::new(2);
        ledger.apply(&[mv(0, BTC, 0)], 1, 10_000.0);
        ledger
    }

    #[test]
    fn transfer_between_wallets_keeps_the_original_basis() {
        let mut ledger = funded_ledger();
        let internal = ledger.apply(&[mv(0, -BTC, 0), mv(1, BTC, 0)], 2, 60_000.0);
        assert_eq!(internal, vec![BTC as u64, BTC as u64]);

        let [a, b] = ledger.into_positions().try_into().expect("two wallets");
        assert!(a.open_lots.is_empty());
        assert_eq!(a.realized_usd, 0.0);
        assert_eq!(b.open_lots.len(), 1);
        assert_eq!(b.open_lots[0].unit_price_usd, 10_000.0);
        assert_eq!(b.open_lots[0].time, 1);
        assert_eq!(b.summarize(60_000.0).unrealized_usd, 50_000.0);
    }

    #[test]
    fn transfer_fee_is_a_loss_and_the_rest_arrives() {
        let mut ledger = funded_ledger();
        let fee = BTC / 1_000;
        ledger.apply(&[mv(0, -BTC, fee as u64), mv(1, BTC - fee, 0)], 2, 60_000.0);

        let [a, b] = ledger.into_positions().try_into().expect("two wallets");
        // 0.001 BTC bought at 10k is lost: 10 USD.
        assert!((a.realized_usd + 10.0).abs() < 1e-9);
        assert_eq!(b.open_lots[0].sats, (BTC - fee) as u64);
        assert_eq!(b.open_lots[0].unit_price_usd, 10_000.0);
    }

    #[test]
    fn payment_to_a_third_party_is_a_sale_alongside_the_transfer() {
        // Send 1 BTC: half to wallet 1, half to someone else.
        let mut ledger = funded_ledger();
        let internal = ledger.apply(&[mv(0, -BTC, 0), mv(1, BTC / 2, 0)], 2, 60_000.0);
        assert_eq!(internal, vec![(BTC / 2) as u64, (BTC / 2) as u64]);

        let [a, b] = ledger.into_positions().try_into().expect("two wallets");
        // Sold 0.5 BTC bought at 10k for 60k: 25,000 gained.
        assert_eq!(a.realized_usd, 25_000.0);
        assert_eq!(b.open_lots[0].sats, (BTC / 2) as u64);
        assert_eq!(b.open_lots[0].unit_price_usd, 10_000.0);
    }

    #[test]
    fn moved_lots_slot_in_by_age() {
        let mut ledger = funded_ledger();
        ledger.apply(&[mv(1, BTC, 0)], 100, 30_000.0);
        ledger.apply(&[mv(0, -BTC, 0), mv(1, BTC, 0)], 200, 60_000.0);

        let [_, b] = ledger.into_positions().try_into().expect("two wallets");
        let times: Vec<i64> = b.open_lots.iter().map(|l| l.time).collect();
        assert_eq!(times, vec![1, 100]);
    }

    #[test]
    fn missing_sender_history_falls_back_to_a_purchase() {
        let mut ledger = Ledger::new(2);
        ledger.apply(&[mv(0, -BTC, 0), mv(1, BTC, 0)], 5, 40_000.0);

        let [a, b] = ledger.into_positions().try_into().expect("two wallets");
        assert_eq!(a.realized_usd, 0.0);
        assert_eq!(b.open_lots.len(), 1);
        assert_eq!(b.open_lots[0].unit_price_usd, 40_000.0);
    }

    #[test]
    fn combining_nothing_is_empty() {
        let total = Summary::combine([]);
        assert_eq!(total.balance_sats, 0);
        assert_eq!(total.unrealized_pct, None);
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
