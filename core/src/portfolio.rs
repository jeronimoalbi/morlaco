//! Ties history, prices and lots together into one view per wallet.
//!
//! Work happens in two steps. [`fetch`] downloads and prices one address on
//! its own. [`assemble`] then replays several histories together, so a
//! transaction that moves coins between two tracked wallets is recognized as
//! a transfer and not booked as a sale and a purchase.

use std::collections::{HashMap, HashSet};

use crate::Result;
use crate::chain::{self, Delta};
use crate::db::Database;
use crate::lots::{Ledger, Lot, Move, Summary};
use crate::price::PriceSource;

/// The confirmed transactions of one address, each priced.
#[derive(Debug, Clone)]
pub struct History {
    /// The address that was fetched.
    pub address: String,
    /// Oldest first.
    pub txs: Vec<PricedTx>,
}

/// A transaction's effect on one address and the BTC/USD price at the time.
#[derive(Debug, Clone)]
pub struct PricedTx {
    /// What the transaction did to the address.
    pub delta: Delta,
    /// USD per BTC when it was confirmed.
    pub price_usd: f64,
}

/// A transaction as shown in a wallet's history.
#[derive(Debug, Clone)]
pub struct Entry {
    /// What the transaction did to the wallet.
    pub delta: Delta,
    /// USD per BTC when it was confirmed.
    pub price_usd: f64,
    /// Sats that moved to or from another tracked wallet. Zero for ordinary
    /// payments.
    pub internal_sats: u64,
}

/// Everything a front end needs to show one wallet.
#[derive(Debug, Clone)]
pub struct Portfolio {
    /// The address that was tracked.
    pub address: String,
    /// BTC/USD used to value the holdings.
    pub price_now_usd: f64,
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

/// Brings the saved history of `address` up to date and returns it.
///
/// Only blocks above the last saved one, minus [`REORG_DEPTH`], are
/// downloaded and priced; everything older comes from `db`. The result is
/// saved in one step, so a failure leaves the earlier state intact.
///
/// `on_progress` receives the number of new transactions found so far.
pub fn fetch(
    address: &str,
    prices: &impl PriceSource,
    db: &Database,
    on_progress: impl FnMut(usize),
) -> Result<History> {
    let saved = db.history(address)?;
    let since = refetch_above(&saved);

    let fresh = chain::fetch_history(address, since, on_progress)?
        .into_iter()
        .map(|delta| {
            prices
                .usd_at(delta.time)
                .map(|price_usd| PricedTx { delta, price_usd })
        })
        .collect::<Result<Vec<_>>>()?;

    db.replace_history_above(address, since, &fresh)?;
    Ok(History {
        address: address.to_owned(),
        txs: merge(saved, since, fresh),
    })
}

/// Height above which saved transactions must be fetched again. 0 when
/// nothing is saved.
fn refetch_above(saved: &[PricedTx]) -> u64 {
    saved
        .iter()
        .map(|t| t.delta.height)
        .max()
        .map_or(0, |tip| tip.saturating_sub(REORG_DEPTH))
}

/// Keeps the saved transactions at or below `since` and adds the fresh ones.
fn merge(saved: Vec<PricedTx>, since: u64, fresh: Vec<PricedTx>) -> Vec<PricedTx> {
    let mut txs: Vec<PricedTx> = saved
        .into_iter()
        .filter(|t| t.delta.height <= since)
        .collect();
    txs.extend(fresh);
    txs
}

/// One transaction as seen across all tracked wallets.
struct Group<'a> {
    txid: &'a str,
    height: u64,
    time: i64,
    price_usd: f64,
    parents: &'a [String],
    /// Wallet index and what the transaction did to it.
    touches: Vec<(usize, &'a Delta)>,
}

/// Merges the histories into transactions, oldest first.
///
/// Transactions are ordered by block height. Inside one block, a
/// transaction that spends another's output comes after it.
fn ordered_groups<'a>(histories: &[&'a History]) -> Vec<Group<'a>> {
    let mut by_txid: HashMap<&str, Group> = HashMap::new();
    for (wallet, history) in histories.iter().enumerate() {
        for tx in &history.txs {
            by_txid
                .entry(tx.delta.txid.as_str())
                .or_insert_with(|| Group {
                    txid: tx.delta.txid.as_str(),
                    height: tx.delta.height,
                    time: tx.delta.time,
                    price_usd: tx.price_usd,
                    parents: &tx.delta.parents,
                    touches: Vec::new(),
                })
                .touches
                .push((wallet, &tx.delta));
        }
    }

    let mut groups: Vec<Group> = by_txid.into_values().collect();
    groups.sort_by_key(|g| (g.height, g.txid));

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

/// Values several wallets together at `price_now_usd`.
///
/// Returns one [`Portfolio`] per history, in the same order. Coins moved
/// between the given wallets keep their original date and price.
pub fn assemble(histories: &[&History], price_now_usd: f64) -> Vec<Portfolio> {
    let mut ledger = Ledger::new(histories.len());
    let mut entries: Vec<Vec<Entry>> = vec![Vec::new(); histories.len()];

    for group in ordered_groups(histories) {
        let moves: Vec<Move> = group
            .touches
            .iter()
            .map(|(wallet, d)| Move {
                wallet: *wallet,
                net_sats: d.net_sats,
                fee_sats: d.fee_sats,
            })
            .collect();
        let internal = ledger.apply(&moves, group.time, group.price_usd);

        for ((wallet, delta), internal_sats) in group.touches.iter().zip(internal) {
            entries[*wallet].push(Entry {
                delta: (*delta).clone(),
                price_usd: group.price_usd,
                internal_sats,
            });
        }
    }

    ledger
        .into_positions()
        .into_iter()
        .zip(entries)
        .zip(histories)
        .map(|((position, history), source)| Portfolio {
            address: source.address.clone(),
            price_now_usd,
            summary: position.summarize(price_now_usd),
            lots: position.open_lots,
            history,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BTC: i64 = 100_000_000;

    fn tx(txid: &str, height: u64, net_sats: i64, fee_sats: u64, parents: &[&str]) -> PricedTx {
        PricedTx {
            delta: Delta {
                txid: txid.to_owned(),
                height,
                time: height as i64 * 600,
                net_sats,
                fee_sats,
                parents: parents.iter().map(|p| (*p).to_owned()).collect(),
            },
            price_usd: 10_000.0 * height as f64,
        }
    }

    fn history(address: &str, txs: Vec<PricedTx>) -> History {
        History {
            address: address.to_owned(),
            txs,
        }
    }

    fn ids(txs: &[PricedTx]) -> Vec<&str> {
        txs.iter().map(|t| t.delta.txid.as_str()).collect()
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

        let out = assemble(&[&a, &b], 60_000.0);
        assert_eq!(out[0].summary.realized_usd, 0.0);
        assert!(out[0].lots.is_empty());
        assert_eq!(out[1].summary.cost_basis_usd, 10_000.0);
        assert_eq!(out[1].summary.unrealized_usd, 50_000.0);

        let sent = &out[0].history[1];
        let received = &out[1].history[0];
        assert_eq!(sent.internal_sats, BTC as u64);
        assert_eq!(received.internal_sats, BTC as u64);
        assert_eq!(out[0].history[0].internal_sats, 0);
    }

    #[test]
    fn the_same_move_without_the_other_wallet_is_a_sale() {
        let a = history(
            "A",
            vec![tx("buy", 1, BTC, 0, &[]), tx("move", 6, -BTC, 0, &["buy"])],
        );
        let out = assemble(&[&a], 60_000.0);
        assert_eq!(out[0].summary.realized_usd, 50_000.0);
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

        let out = assemble(&[&a, &b, &c], 50_000.0);
        // The coins travelled A -> B -> C untouched, still costing 10k.
        assert_eq!(out[2].summary.cost_basis_usd, 10_000.0);
        assert!(out.iter().all(|p| p.summary.realized_usd == 0.0));
    }
}
