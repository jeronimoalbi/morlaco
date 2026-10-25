//! Ties history, prices and lots together into one view per wallet.
//!
//! Work happens in two steps. [`fetch`] downloads and prices one wallet on
//! its own. [`assemble`] then replays several histories together, so a
//! transaction that moves coins between two tracked wallets of the same
//! chain is recognized as a transfer and not booked as a sale and a
//! purchase.

use std::collections::{HashMap, HashSet};

use crate::Result;
use crate::chain::{Activity, Chain, WalletId};
use crate::db::Database;
use crate::lots::{Ledger, Lot, Move, Summary};
use crate::price::PriceSource;

/// The confirmed transactions of one wallet, each priced.
#[derive(Debug, Clone)]
pub struct History {
    /// The wallet that was fetched.
    pub wallet: WalletId,
    /// Oldest first.
    pub txs: Vec<PricedTx>,
}

/// A transaction's effect on one wallet and the USD price of the chain's
/// coin at the time.
#[derive(Debug, Clone)]
pub struct PricedTx {
    /// What the transaction did to the wallet.
    pub activity: Activity,
    /// USD per whole coin when it was confirmed.
    pub price_usd: f64,
}

/// A transaction as shown in a wallet's history.
#[derive(Debug, Clone)]
pub struct Entry {
    /// What the transaction did to the wallet.
    pub activity: Activity,
    /// USD per whole coin when it was confirmed.
    pub price_usd: f64,
    /// Units that moved to or from another tracked wallet. Zero for
    /// ordinary payments.
    pub internal_units: u128,
}

/// Everything a front end needs to show one wallet.
#[derive(Debug, Clone)]
pub struct Portfolio {
    /// The wallet that was tracked.
    pub wallet: WalletId,
    /// USD per whole coin used to value the holdings.
    pub price_now_usd: f64,
    /// Everything held, in the chain's smallest unit.
    pub balance_units: u128,
    /// Headline numbers.
    pub summary: Summary,
    /// Lots still held, oldest first.
    pub lots: Vec<Lot>,
    /// Every confirmed transaction, oldest first.
    pub history: Vec<Entry>,
}

/// Blocks this deep can still be replaced by a chain reorganization, so
/// they are always fetched again.
const REORG_DEPTH: u64 = 6;

/// Brings the saved history of `wallet` up to date and returns it.
///
/// Only blocks above the last saved one, minus [`REORG_DEPTH`], are
/// downloaded and priced; everything older comes from `db`. The result is
/// saved in one step, so a failure leaves the earlier state intact.
///
/// `on_progress` receives the number of new transactions found so far.
pub fn fetch(
    wallet: &WalletId,
    prices: &impl PriceSource,
    db: &Database,
    mut on_progress: impl FnMut(usize),
) -> Result<History> {
    let saved = db.history(wallet)?;
    let since = refetch_above(&saved);

    let fresh = wallet
        .chain
        .backend()
        .fetch(&wallet.address, since, &mut on_progress)?
        .into_iter()
        .map(|activity| {
            prices
                .usd_at(wallet.chain, activity.time)
                .map(|price_usd| PricedTx {
                    activity,
                    price_usd,
                })
        })
        .collect::<Result<Vec<_>>>()?;

    db.replace_history_above(wallet, since, &fresh)?;
    Ok(History {
        wallet: wallet.clone(),
        txs: merge(saved, since, fresh),
    })
}

/// Height above which saved transactions must be fetched again. 0 when
/// nothing is saved.
fn refetch_above(saved: &[PricedTx]) -> u64 {
    saved
        .iter()
        .map(|t| t.activity.height)
        .max()
        .map_or(0, |tip| tip.saturating_sub(REORG_DEPTH))
}

/// Keeps the saved transactions at or below `since` and adds the fresh ones.
fn merge(saved: Vec<PricedTx>, since: u64, fresh: Vec<PricedTx>) -> Vec<PricedTx> {
    let mut txs: Vec<PricedTx> = saved
        .into_iter()
        .filter(|t| t.activity.height <= since)
        .collect();
    txs.extend(fresh);
    txs
}

/// One transaction as seen across the tracked wallets of a chain.
struct Group<'a> {
    txid: &'a str,
    height: u64,
    index: u32,
    time: i64,
    price_usd: f64,
    parents: &'a [String],
    /// Wallet index and what the transaction did to it.
    touches: Vec<(usize, &'a Activity)>,
}

/// Merges the histories of one chain into transactions, oldest first.
///
/// Transactions are ordered by block height, then by position in the block
/// where the chain reports one. Inside one block, a transaction that spends
/// another's output comes after it.
fn ordered_groups<'a>(histories: &[&'a History]) -> Vec<Group<'a>> {
    let mut by_txid: HashMap<&str, Group> = HashMap::new();
    for (wallet, history) in histories.iter().enumerate() {
        for tx in &history.txs {
            let a = &tx.activity;
            by_txid
                .entry(a.txid.as_str())
                .or_insert_with(|| Group {
                    txid: a.txid.as_str(),
                    height: a.height,
                    index: a.index.unwrap_or(0),
                    time: a.time,
                    price_usd: tx.price_usd,
                    parents: &a.parents,
                    touches: Vec::new(),
                })
                .touches
                .push((wallet, a));
        }
    }

    let mut groups: Vec<Group> = by_txid.into_values().collect();
    groups.sort_by_key(|g| (g.height, g.index, g.txid));

    let mut ordered = Vec::with_capacity(groups.len());
    let mut rest = groups.into_iter().peekable();
    while let Some(first) = rest.next() {
        let height = first.height;
        let mut block = vec![first];
        while let Some(next) = rest.next_if(|g| g.height == height) {
            block.push(next);
        }
        ordered.extend(spending_order(block));
    }
    ordered
}

/// Puts the transactions of one block in spending order, keeping the
/// existing order otherwise.
fn spending_order(mut block: Vec<Group<'_>>) -> Vec<Group<'_>> {
    let in_block: HashSet<&str> = block.iter().map(|g| g.txid).collect();
    let mut placed: HashSet<&str> = HashSet::new();
    let mut out = Vec::with_capacity(block.len());

    while !block.is_empty() {
        let ready = |g: &Group| {
            g.parents
                .iter()
                .all(|p| !in_block.contains(p.as_str()) || placed.contains(p.as_str()))
        };
        // A block cannot contain a spending cycle; the fallback only guards
        // against malformed data.
        let at = block.iter().position(ready).unwrap_or(0);
        let group = block.remove(at);
        placed.insert(group.txid);
        out.push(group);
    }
    out
}

/// Values several wallets together.
///
/// `prices_now` holds the current USD price of one coin per chain; wallets of
/// a chain without a price are left out. Returns one [`Portfolio`] per
/// remaining history, in the order of `histories`.
///
/// Coins moved between wallets of the same chain keep their original date
/// and price. Wallets of different chains never match, even if a transaction
/// id looked the same.
pub fn assemble(histories: &[&History], prices_now: &HashMap<Chain, f64>) -> Vec<Portfolio> {
    let mut slots: Vec<Option<Portfolio>> = histories.iter().map(|_| None).collect();

    let mut chains: Vec<Chain> = Vec::new();
    for history in histories {
        if !chains.contains(&history.wallet.chain) {
            chains.push(history.wallet.chain);
        }
    }

    for chain in chains {
        let Some(&price_now_usd) = prices_now.get(&chain) else {
            continue;
        };
        let members: Vec<usize> = (0..histories.len())
            .filter(|&i| histories[i].wallet.chain == chain)
            .collect();
        let subset: Vec<&History> = members.iter().map(|&i| histories[i]).collect();

        for (portfolio, &original) in assemble_chain(chain, &subset, price_now_usd)
            .into_iter()
            .zip(&members)
        {
            slots[original] = Some(portfolio);
        }
    }

    slots.into_iter().flatten().collect()
}

/// [`assemble`] for histories that all belong to `chain`.
fn assemble_chain(chain: Chain, histories: &[&History], price_now_usd: f64) -> Vec<Portfolio> {
    let mut ledger = Ledger::new(histories.len(), chain.decimals());
    let mut entries: Vec<Vec<Entry>> = vec![Vec::new(); histories.len()];

    for group in ordered_groups(histories) {
        let moves: Vec<Move> = group
            .touches
            .iter()
            .map(|(wallet, a)| Move {
                wallet: *wallet,
                amount: a.amount,
                fee: a.fee,
            })
            .collect();
        let internal = ledger.apply(&moves, group.time, group.price_usd);

        for ((wallet, activity), internal_units) in group.touches.iter().zip(internal) {
            entries[*wallet].push(Entry {
                activity: (*activity).clone(),
                price_usd: group.price_usd,
                internal_units,
            });
        }
    }

    ledger
        .into_positions()
        .into_iter()
        .zip(entries)
        .zip(histories)
        .map(|((position, history), source)| Portfolio {
            wallet: source.wallet.clone(),
            price_now_usd,
            balance_units: position.balance_units(),
            summary: position.summarize(price_now_usd),
            lots: position.open_lots,
            history,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BTC: i128 = 100_000_000;

    fn tx(txid: &str, height: u64, amount: i128, fee: u128, parents: &[&str]) -> PricedTx {
        PricedTx {
            activity: Activity {
                txid: txid.to_owned(),
                height,
                index: None,
                time: height as i64 * 600,
                amount,
                fee,
                parents: parents.iter().map(|p| (*p).to_owned()).collect(),
            },
            price_usd: 10_000.0 * height as f64,
        }
    }

    fn history_on(chain: Chain, address: &str, txs: Vec<PricedTx>) -> History {
        History {
            wallet: WalletId::new(chain, address),
            txs,
        }
    }

    fn history(address: &str, txs: Vec<PricedTx>) -> History {
        history_on(Chain::Bitcoin, address, txs)
    }

    /// The same price for every chain.
    fn prices(usd: f64) -> HashMap<Chain, f64> {
        HashMap::from([(Chain::Bitcoin, usd), (Chain::Fake, usd)])
    }

    fn ids(txs: &[PricedTx]) -> Vec<&str> {
        txs.iter().map(|t| t.activity.txid.as_str()).collect()
    }

    #[test]
    fn nothing_saved_means_a_full_fetch() {
        assert_eq!(refetch_above(&[]), 0);
    }

    #[test]
    fn the_last_blocks_are_refetched_in_case_of_a_reorg() {
        let saved = [tx("a", 100, BTC, 0, &[]), tx("b", 120, BTC, 0, &[])];
        assert_eq!(refetch_above(&saved), 120 - REORG_DEPTH);
        // Near the start of the chain it cannot go below zero.
        assert_eq!(refetch_above(&[tx("c", 2, BTC, 0, &[])]), 0);
    }

    #[test]
    fn merge_drops_saved_blocks_that_are_refetched() {
        let saved = vec![
            tx("old", 100, BTC, 0, &[]),
            tx("replaced", 118, BTC, 0, &[]),
            tx("tip", 120, BTC, 0, &[]),
        ];
        let fresh = vec![tx("tip", 120, BTC, 0, &[]), tx("new", 125, BTC, 0, &[])];
        let merged = merge(saved, 114, fresh);
        assert_eq!(ids(&merged), ["old", "tip", "new"]);
    }

    #[test]
    fn a_transfer_between_tracked_wallets_is_not_a_sale() {
        // Wallet A buys at height 1 (10k) and sends everything to B at
        // height 6 (60k). Both histories contain the same transaction.
        let a = history(
            "A",
            vec![tx("buy", 1, BTC, 0, &[]), tx("move", 6, -BTC, 0, &["buy"])],
        );
        let b = history("B", vec![tx("move", 6, BTC, 0, &["buy"])]);

        let out = assemble(&[&a, &b], &prices(60_000.0));
        assert_eq!(out[0].summary.realized_usd, 0.0);
        assert!(out[0].lots.is_empty());
        assert_eq!(out[1].summary.cost_basis_usd, 10_000.0);
        assert_eq!(out[1].summary.unrealized_usd, 50_000.0);
        assert_eq!(out[1].balance_units, BTC as u128);

        let sent = &out[0].history[1];
        let received = &out[1].history[0];
        assert_eq!(sent.internal_units, BTC as u128);
        assert_eq!(received.internal_units, BTC as u128);
        assert_eq!(out[0].history[0].internal_units, 0);
    }

    #[test]
    fn the_same_move_without_the_other_wallet_is_a_sale() {
        let a = history(
            "A",
            vec![tx("buy", 1, BTC, 0, &[]), tx("move", 6, -BTC, 0, &["buy"])],
        );
        let out = assemble(&[&a], &prices(60_000.0));
        assert_eq!(out[0].summary.realized_usd, 50_000.0);
    }

    #[test]
    fn a_fee_paid_on_a_transfer_is_a_loss_for_the_sender() {
        // A sends 0.4 BTC to B and pays a 0.01 BTC fee on top.
        let fee = BTC / 100;
        let a = history(
            "A",
            vec![
                tx("buy", 1, BTC, 0, &[]),
                tx("move", 6, -(BTC * 4 / 10), fee as u128, &["buy"]),
            ],
        );
        let b = history("B", vec![tx("move", 6, BTC * 4 / 10, 0, &["buy"])]);

        let out = assemble(&[&a, &b], &prices(60_000.0));
        // 0.01 BTC bought at 10k is lost: 100 USD.
        assert!((out[0].summary.realized_usd + 100.0).abs() < 1e-9);
        assert_eq!(out[0].balance_units, (BTC - BTC * 4 / 10 - fee) as u128);
        assert_eq!(out[1].balance_units, (BTC * 4 / 10) as u128);
    }

    #[test]
    fn spending_order_beats_txid_order_inside_a_block() {
        // Both transactions are in block 5. "z-first" creates the coins B
        // spends in "a-second"; sorting by txid alone would reverse them.
        let a = history(
            "A",
            vec![
                tx("buy", 1, BTC, 0, &[]),
                tx("z-first", 5, -BTC, 0, &["buy"]),
            ],
        );
        let b = history(
            "B",
            vec![
                tx("z-first", 5, BTC, 0, &["buy"]),
                tx("a-second", 5, -BTC, 0, &["z-first"]),
            ],
        );
        let c = history("C", vec![tx("a-second", 5, BTC, 0, &["z-first"])]);

        let out = assemble(&[&a, &b, &c], &prices(50_000.0));
        // The coins travelled A -> B -> C untouched, still costing 10k.
        assert_eq!(out[2].summary.cost_basis_usd, 10_000.0);
        assert!(out.iter().all(|p| p.summary.realized_usd == 0.0));
    }

    #[test]
    fn block_position_orders_transactions_on_chains_that_report_it() {
        // No parents to go by: only the index says "first" comes before
        // "second", although "a-second" sorts earlier by txid.
        let mut first = tx("z-first", 5, 10 * BTC, 0, &[]);
        first.activity.index = Some(1);
        let mut second = tx("a-second", 5, -(10 * BTC), 0, &[]);
        second.activity.index = Some(2);
        let w = history_on(Chain::Fake, "0xw", vec![second, first]);

        let out = assemble(&[&w], &prices(100.0));
        // Received first, then sold: a sale of the coins just bought.
        assert_eq!(out[0].balance_units, 0);
        let order: Vec<&str> = out[0]
            .history
            .iter()
            .map(|e| e.activity.txid.as_str())
            .collect();
        assert_eq!(order, ["z-first", "a-second"]);
    }

    #[test]
    fn wallets_on_different_chains_never_match_even_with_the_same_txid() {
        // On Bitcoin: buy at 10k, then "move" away. On the other chain the
        // same txid text receives coins. They are unrelated.
        let btc = history(
            "A",
            vec![tx("buy", 1, BTC, 0, &[]), tx("move", 6, -BTC, 0, &["buy"])],
        );
        let other = history_on(Chain::Fake, "0xB", vec![tx("move", 6, BTC, 0, &[])]);

        let out = assemble(&[&btc, &other], &prices(60_000.0));
        assert_eq!(out[0].summary.realized_usd, 50_000.0);
        assert_eq!(out[1].history[0].internal_units, 0);
        // Bought at the market price of its own block, not carried over.
        assert_eq!(out[1].lots[0].unit_price_usd, 60_000.0);
        assert_eq!(out[1].lots[0].decimals, 18);
    }

    #[test]
    fn portfolios_come_back_in_the_order_of_the_histories() {
        let a = history("A", vec![tx("a", 1, BTC, 0, &[])]);
        let f = history_on(Chain::Fake, "0xF", vec![tx("f", 1, BTC, 0, &[])]);
        let b = history("B", vec![tx("b", 1, BTC, 0, &[])]);

        let out = assemble(&[&a, &f, &b], &prices(1.0));
        let wallets: Vec<&str> = out.iter().map(|p| p.wallet.address.as_str()).collect();
        assert_eq!(wallets, ["A", "0xF", "B"]);
    }

    #[test]
    fn a_chain_without_a_price_is_left_out() {
        let a = history("A", vec![tx("a", 1, BTC, 0, &[])]);
        let f = history_on(Chain::Fake, "0xF", vec![tx("f", 1, BTC, 0, &[])]);

        let only_bitcoin = HashMap::from([(Chain::Bitcoin, 1.0)]);
        let out = assemble(&[&a, &f], &only_bitcoin);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].wallet.address, "A");
    }
}
