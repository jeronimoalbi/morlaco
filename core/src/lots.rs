//! FIFO lot accounting.
//!
//! Every incoming transfer opens a lot priced at the USD rate of that
//! moment. Outgoing transfers consume the oldest lots first. Network fees
//! consume lots too, but with zero proceeds, so they show up as a realized
//! loss and never as a sale at market price.
//!
//! Amounts are whole numbers of the chain's smallest unit (satoshis, wei,
//! lamports). A ledger covers one chain, so it knows that chain's decimals.

use std::collections::VecDeque;

/// One wallet's side of a single-wallet replay, with the price at the time.
#[derive(Debug, Clone, PartialEq)]
pub struct PricedDelta {
    /// Block time as a Unix timestamp.
    pub time: i64,
    /// Units received minus units sent, not counting the fee.
    pub amount: i128,
    /// Units paid in fees on top of `amount`.
    pub fee: u128,
    /// USD per whole coin at `time`.
    pub price_usd: f64,
}

/// Coins acquired at one moment and not yet spent.
#[derive(Debug, Clone, PartialEq)]
pub struct Lot {
    /// Acquisition time as a Unix timestamp.
    pub time: i64,
    /// Remaining size in smallest units.
    pub units: u128,
    /// USD per whole coin when the lot was acquired.
    pub unit_price_usd: f64,
    /// Decimal places of the coin, to convert `units` to whole coins.
    pub decimals: u8,
}

impl Lot {
    /// Remaining amount in whole coins.
    pub fn coins(&self) -> f64 {
        to_coins(self.units, self.decimals)
    }

    /// What the remaining coins cost, in USD.
    pub fn cost_usd(&self) -> f64 {
        self.coins() * self.unit_price_usd
    }

    /// What the remaining coins are worth at `price_usd` per coin.
    pub fn value_usd(&self, price_usd: f64) -> f64 {
        self.coins() * price_usd
    }

    /// Unrealized gain (positive) or loss (negative) at `price_usd`.
    pub fn unrealized_usd(&self, price_usd: f64) -> f64 {
        self.value_usd(price_usd) - self.cost_usd()
    }
}

/// Converts smallest units to whole coins.
fn to_coins(units: u128, decimals: u8) -> f64 {
    units as f64 / 10f64.powi(i32::from(decimals))
}

/// The result of replaying a wallet history.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Position {
    /// Lots still held, oldest first.
    pub open_lots: Vec<Lot>,
    /// Gains and losses from coins that were spent.
    pub realized_usd: f64,
}

/// Headline numbers for a [`Position`] at one price. All values are in USD,
/// so summaries of different chains can be added together.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
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
            cost_basis_usd: 0.0,
            value_usd: 0.0,
            unrealized_usd: 0.0,
            unrealized_pct: None,
            realized_usd: 0.0,
        };
        for s in items {
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
    /// Everything held, in smallest units.
    pub fn balance_units(&self) -> u128 {
        self.open_lots.iter().map(|l| l.units).sum()
    }

    /// Totals the open lots at the current price per coin.
    pub fn summarize(&self, price_now_usd: f64) -> Summary {
        let cost_basis_usd: f64 = self.open_lots.iter().map(Lot::cost_usd).sum();
        let value_usd: f64 = self
            .open_lots
            .iter()
            .map(|l| l.value_usd(price_now_usd))
            .sum();
        let unrealized_usd = value_usd - cost_basis_usd;

        Summary {
            cost_basis_usd,
            value_usd,
            unrealized_usd,
            unrealized_pct: (cost_basis_usd > 0.0).then(|| unrealized_usd / cost_basis_usd * 100.0),
            realized_usd: self.realized_usd,
        }
    }
}

/// Removes up to `units` from the front of `lots` and returns the removed
/// pieces, each keeping the date and price of the lot it came from.
///
/// Returns less than `units` only if the history spends more than it ever
/// received.
fn take(lots: &mut VecDeque<Lot>, mut units: u128) -> Vec<Lot> {
    let mut pieces = Vec::new();

    while units > 0 {
        let Some(front) = lots.front_mut() else { break };
        let used = units.min(front.units);
        pieces.push(Lot {
            units: used,
            ..front.clone()
        });
        front.units -= used;
        units -= used;
        if front.units == 0 {
            lots.pop_front();
        }
    }

    pieces
}

fn total_units(pieces: &[Lot]) -> u128 {
    pieces.iter().map(|l| l.units).sum()
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
    /// Units received minus units sent, not counting the fee. Negative when
    /// the wallet paid out.
    pub amount: i128,
    /// Units this wallet paid in fees, on top of `amount`. A move with an
    /// `amount` of zero is just a fee.
    pub fee: u128,
}

/// FIFO lots for several wallets of one chain.
///
/// When one transaction moves coins out of one tracked wallet and into
/// another, the coins keep their original date and price instead of being
/// booked as a sale and a purchase.
#[derive(Debug, Clone)]
pub struct Ledger {
    decimals: u8,
    lots: Vec<VecDeque<Lot>>,
    realized_usd: Vec<f64>,
}

impl Ledger {
    /// Creates a ledger for `wallets` empty wallets of a chain whose coin
    /// has `decimals` decimal places.
    pub fn new(wallets: usize, decimals: u8) -> Self {
        Self {
            decimals,
            lots: vec![VecDeque::new(); wallets],
            realized_usd: vec![0.0; wallets],
        }
    }

    /// Applies one transaction. `moves` lists every tracked wallet it touches
    /// and each wallet may appear once. `price_usd` is per whole coin.
    ///
    /// Returns, for each move, how many units went to or came from another
    /// tracked wallet.
    pub fn apply(&mut self, moves: &[Move], time: i64, price_usd: f64) -> Vec<u128> {
        let sent = |m: &Move| {
            if m.amount < 0 {
                m.amount.unsigned_abs()
            } else {
                0
            }
        };
        let received = |m: &Move| {
            if m.amount > 0 {
                m.amount.unsigned_abs()
            } else {
                0
            }
        };

        let total_out: u128 = moves.iter().map(sent).sum();
        let total_in: u128 = moves.iter().map(received).sum();
        let moved_between_wallets = total_out.min(total_in);
        let mut internal_left = moved_between_wallets;

        let mut internal = vec![0; moves.len()];
        let mut in_transit: VecDeque<Lot> = VecDeque::new();

        // Senders: coins for other wallets move; the rest is sold.
        for (i, m) in moves.iter().enumerate().filter(|(_, m)| m.amount < 0) {
            let sending = sent(m);
            let moved = sending.min(internal_left);
            internal_left -= moved;
            internal[i] = moved;

            in_transit.extend(take(&mut self.lots[m.wallet], moved));

            let sold = take(&mut self.lots[m.wallet], sending - moved);
            self.realized_usd[m.wallet] +=
                to_coins(total_units(&sold), self.decimals) * price_usd - total_cost(&sold);
        }

        // Receivers: coins from other wallets keep their basis; the rest is
        // a purchase at the current price.
        let mut to_assign = moved_between_wallets;
        for (i, m) in moves.iter().enumerate().filter(|(_, m)| m.amount > 0) {
            let inflow = received(m);
            let share = inflow.min(to_assign);
            to_assign -= share;
            internal[i] = share;

            let pieces = take(&mut in_transit, share);
            let given = total_units(&pieces);
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
                        units: bought,
                        unit_price_usd: price_usd,
                        decimals: self.decimals,
                    },
                );
            }
        }

        // Fees are paid last and destroy coins: a loss, not a sale.
        for m in moves.iter().filter(|m| m.fee > 0) {
            let lost = take(&mut self.lots[m.wallet], m.fee);
            self.realized_usd[m.wallet] -= total_cost(&lost);
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

/// Replays `deltas` (oldest first) for a single wallet of a chain with
/// `decimals` decimal places.
pub fn fifo(decimals: u8, deltas: &[PricedDelta]) -> Position {
    let mut ledger = Ledger::new(1, decimals);
    for d in deltas {
        let moves = [Move {
            wallet: 0,
            amount: d.amount,
            fee: d.fee,
        }];
        ledger.apply(&moves, d.time, d.price_usd);
    }
    ledger.into_positions().pop().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One bitcoin in satoshis.
    const BTC: i128 = 100_000_000;
    /// One coin of an 18-decimals chain, in its smallest unit.
    const ETH: i128 = 1_000_000_000_000_000_000;

    fn delta(amount: i128, fee: u128, price_usd: f64) -> PricedDelta {
        PricedDelta {
            time: 0,
            amount,
            fee,
            price_usd,
        }
    }

    /// Single-wallet replay for an 8-decimals chain.
    fn fifo8(deltas: &[PricedDelta]) -> Position {
        fifo(8, deltas)
    }

    #[test]
    fn single_buy_has_unrealized_gain() {
        let pos = fifo8(&[delta(BTC, 0, 10_000.0)]);
        let s = pos.summarize(15_000.0);
        assert_eq!(pos.balance_units(), BTC as u128);
        assert_eq!(s.cost_basis_usd, 10_000.0);
        assert_eq!(s.unrealized_usd, 5_000.0);
        assert_eq!(s.unrealized_pct, Some(50.0));
        assert_eq!(s.realized_usd, 0.0);
    }

    #[test]
    fn unrealized_loss_is_negative() {
        let s = fifo8(&[delta(BTC, 0, 10_000.0)]).summarize(7_500.0);
        assert_eq!(s.unrealized_usd, -2_500.0);
        assert_eq!(s.unrealized_pct, Some(-25.0));
    }

    #[test]
    fn sale_consumes_the_oldest_lot_first() {
        // Buy 1 BTC at 10k, buy 1 BTC at 20k, sell 1.5 BTC at 30k.
        let pos = fifo8(&[
            delta(BTC, 0, 10_000.0),
            delta(BTC, 0, 20_000.0),
            delta(-(BTC + BTC / 2), 0, 30_000.0),
        ]);

        // 1.0 @ 10k and 0.5 @ 20k are consumed: cost 20k, proceeds 45k.
        assert_eq!(pos.realized_usd, 25_000.0);
        assert_eq!(pos.open_lots.len(), 1);
        assert_eq!(pos.open_lots[0].units, (BTC / 2) as u128);
        assert_eq!(pos.open_lots[0].unit_price_usd, 20_000.0);
    }

    #[test]
    fn fee_is_a_loss_not_a_sale() {
        // Buy 1 BTC at 10k, send 0.5 BTC away paying a 0.01 BTC fee, price 10k.
        let fee = BTC / 100;
        let pos = fifo8(&[
            delta(BTC, 0, 10_000.0),
            delta(-(BTC / 2), fee as u128, 10_000.0),
        ]);

        // The 0.5 BTC sent breaks even; the fee loses 0.01 * 10k = 100.
        assert!((pos.realized_usd + 100.0).abs() < 1e-9);
        assert_eq!(pos.open_lots[0].units, (BTC / 2 - fee) as u128);
    }

    #[test]
    fn a_fee_without_any_payment_is_still_a_loss() {
        let pos = fifo8(&[
            delta(BTC, 0, 10_000.0),
            delta(0, (BTC / 100) as u128, 50_000.0),
        ]);
        // The lost 0.01 BTC was bought at 10k, whatever the price now.
        assert!((pos.realized_usd + 100.0).abs() < 1e-9);
        assert_eq!(pos.balance_units(), (BTC - BTC / 100) as u128);
    }

    #[test]
    fn a_chain_with_18_decimals_handles_amounts_beyond_u64() {
        // 50 coins is 5e19 units, more than u64::MAX (about 1.8e19).
        assert!(50 * ETH > i128::from(u64::MAX));
        let pos = fifo(
            18,
            &[delta(50 * ETH, 0, 2_000.0), delta(-(20 * ETH), 0, 3_000.0)],
        );

        // Sold 20 coins bought at 2k for 3k each: 20,000 gained.
        assert_eq!(pos.realized_usd, 20_000.0);
        assert_eq!(pos.balance_units(), (30 * ETH) as u128);
        let s = pos.summarize(2_500.0);
        assert_eq!(s.cost_basis_usd, 60_000.0);
        assert_eq!(s.value_usd, 75_000.0);
    }

    #[test]
    fn an_account_style_fee_is_charged_on_top_of_the_amount() {
        // Buy 1 coin at 2k, send 0.4 and pay 0.01 of gas.
        let pos = fifo(
            18,
            &[
                delta(ETH, 0, 2_000.0),
                delta(-(ETH * 4 / 10), (ETH / 100) as u128, 2_000.0),
            ],
        );

        // The coins sent break even; the gas loses 0.01 * 2k = 20.
        assert!((pos.realized_usd + 20.0).abs() < 1e-9);
        assert_eq!(
            pos.balance_units(),
            (ETH - ETH * 4 / 10 - ETH / 100) as u128
        );
    }

    #[test]
    fn combined_summary_adds_up_and_reweights_the_percentage() {
        // 1 BTC bought at 10k (+100%) and 1 BTC bought at 30k (-33%), price 20k.
        let a = fifo8(&[delta(BTC, 0, 10_000.0)]).summarize(20_000.0);
        let b = fifo8(&[delta(BTC, 0, 30_000.0)]).summarize(20_000.0);

        let total = Summary::combine([&a, &b]);
        assert_eq!(total.cost_basis_usd, 40_000.0);
        assert_eq!(total.value_usd, 40_000.0);
        assert_eq!(total.unrealized_usd, 0.0);
        assert_eq!(total.unrealized_pct, Some(0.0));
    }

    fn mv(wallet: usize, amount: i128, fee: u128) -> Move {
        Move {
            wallet,
            amount,
            fee,
        }
    }

    /// Wallet 0 buys 1 BTC at 10k at time 1.
    fn funded_ledger() -> Ledger {
        let mut ledger = Ledger::new(2, 8);
        ledger.apply(&[mv(0, BTC, 0)], 1, 10_000.0);
        ledger
    }

    #[test]
    fn transfer_between_wallets_keeps_the_original_basis() {
        let mut ledger = funded_ledger();
        let internal = ledger.apply(&[mv(0, -BTC, 0), mv(1, BTC, 0)], 2, 60_000.0);
        assert_eq!(internal, vec![BTC as u128, BTC as u128]);

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
        // The sender spends 1 BTC in total: 0.999 reaches wallet 1, 0.001 is the fee.
        ledger.apply(
            &[mv(0, -(BTC - fee), fee as u128), mv(1, BTC - fee, 0)],
            2,
            60_000.0,
        );

        let [a, b] = ledger.into_positions().try_into().expect("two wallets");
        // 0.001 BTC bought at 10k is lost: 10 USD.
        assert!((a.realized_usd + 10.0).abs() < 1e-9);
        assert_eq!(b.open_lots[0].units, (BTC - fee) as u128);
        assert_eq!(b.open_lots[0].unit_price_usd, 10_000.0);
    }

    #[test]
    fn payment_to_a_third_party_is_a_sale_alongside_the_transfer() {
        // Send 1 BTC: half to wallet 1, half to someone else.
        let mut ledger = funded_ledger();
        let internal = ledger.apply(&[mv(0, -BTC, 0), mv(1, BTC / 2, 0)], 2, 60_000.0);
        assert_eq!(internal, vec![(BTC / 2) as u128, (BTC / 2) as u128]);

        let [a, b] = ledger.into_positions().try_into().expect("two wallets");
        // Sold 0.5 BTC bought at 10k for 60k: 25,000 gained.
        assert_eq!(a.realized_usd, 25_000.0);
        assert_eq!(b.open_lots[0].units, (BTC / 2) as u128);
        assert_eq!(b.open_lots[0].unit_price_usd, 10_000.0);
    }

    #[test]
    fn a_token_style_transfer_between_wallets_on_an_18_decimals_chain() {
        // Wallet 0 buys 10 coins at 2k, then sends 4 to wallet 1 paying gas
        // from its own balance.
        let mut ledger = Ledger::new(2, 18);
        ledger.apply(&[mv(0, 10 * ETH, 0)], 1, 2_000.0);
        let gas = ETH / 50;
        ledger.apply(
            &[mv(0, -(4 * ETH), gas as u128), mv(1, 4 * ETH, 0)],
            2,
            3_000.0,
        );

        let [a, b] = ledger.into_positions().try_into().expect("two wallets");
        assert_eq!(b.open_lots[0].unit_price_usd, 2_000.0);
        assert_eq!(b.balance_units(), (4 * ETH) as u128);
        // Only the gas is lost: 0.02 * 2k = 40.
        assert!((a.realized_usd + 40.0).abs() < 1e-9);
        assert_eq!(a.balance_units(), (6 * ETH - gas) as u128);
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
        let mut ledger = Ledger::new(2, 8);
        ledger.apply(&[mv(0, -BTC, 0), mv(1, BTC, 0)], 5, 40_000.0);

        let [a, b] = ledger.into_positions().try_into().expect("two wallets");
        assert_eq!(a.realized_usd, 0.0);
        assert_eq!(b.open_lots.len(), 1);
        assert_eq!(b.open_lots[0].unit_price_usd, 40_000.0);
    }

    #[test]
    fn combining_nothing_is_empty() {
        let total = Summary::combine([]);
        assert_eq!(total.cost_basis_usd, 0.0);
        assert_eq!(total.unrealized_pct, None);
    }

    #[test]
    fn empty_history_has_no_percentage() {
        let pos = fifo8(&[]);
        let s = pos.summarize(50_000.0);
        assert_eq!(pos.balance_units(), 0);
        assert_eq!(s.unrealized_pct, None);
    }

    #[test]
    fn overspending_does_not_panic() {
        let pos = fifo8(&[delta(-BTC, 0, 10_000.0)]);
        assert!(pos.open_lots.is_empty());
        assert_eq!(pos.realized_usd, 0.0);
    }
}
