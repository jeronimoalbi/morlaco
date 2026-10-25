//! Ethereum history from the Blockscout API.
//!
//! A node cannot list the transactions of an address, so an explorer's index
//! is used. Ether reaches an address in three ways, and each has its own
//! endpoint:
//!
//! - **Transactions** sent from or to the address.
//! - **Internal transfers**: a contract paying out ether while it runs a
//!   transaction that the address may not be part of.
//! - **Withdrawals** from staking validators, which belong to no transaction.
//!
//! They are merged into one [`Activity`] per transaction hash.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::chain::{Activity, Chain, ChainBackend};
use crate::util::parse_iso_utc;
use crate::{Error, Result};

const BASE_URL: &str = "https://eth.blockscout.com/api/v2";

/// Wei in one gwei. Withdrawals are reported in gwei.
const WEI_PER_GWEI: u128 = 1_000_000_000;

/// How long to wait before asking for another page. The service has a limit
/// per second for clients without a key.
const PAUSE_BETWEEN_PAGES: Duration = Duration::from_millis(250);

/// Reads Ethereum addresses through Blockscout.
pub struct EthereumBackend;

impl ChainBackend for EthereumBackend {
    fn chain(&self) -> Chain {
        Chain::Ethereum
    }

    fn fetch(
        &self,
        address: &str,
        since_height: u64,
        on_progress: &mut dyn FnMut(usize),
    ) -> Result<Vec<Activity>> {
        Chain::Ethereum.check_address(address)?;
        let address = address.to_ascii_lowercase();
        let list = |path: &str| format!("{BASE_URL}/addresses/{address}/{path}");

        let mut found = 0;
        let transactions: Vec<RawTx> =
            walk(page_fetcher(list("transactions")), since_height, |n| {
                on_progress(found + n)
            })?;
        found += transactions.len();
        let internal: Vec<RawInternal> = walk(
            page_fetcher(list("internal-transactions")),
            since_height,
            |n| on_progress(found + n),
        )?;
        found += internal.len();
        // Asking for the withdrawals of an address that has none takes
        // minutes to fail, so the address summary says first whether there are
        // any.
        let withdrawals: Vec<RawWithdrawal> = if has_withdrawals(&address)? {
            walk(page_fetcher(list("withdrawals")), since_height, |n| {
                on_progress(found + n)
            })?
        } else {
            Vec::new()
        };

        merge(&address, &transactions, &internal, &withdrawals)
    }
}

#[derive(Deserialize)]
struct RawSummary {
    #[serde(default)]
    has_beacon_chain_withdrawals: bool,
}

/// Whether the address has received staking withdrawals. An address the
/// service has never seen has none.
fn has_withdrawals(address: &str) -> Result<bool> {
    match ureq::get(format!("{BASE_URL}/addresses/{address}")).call() {
        Ok(mut response) => {
            let summary: RawSummary = response.body_mut().read_json()?;
            Ok(summary.has_beacon_chain_withdrawals)
        }
        Err(ureq::Error::StatusCode(404)) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Fetches the pages of one list of an address, given the list's URL.
fn page_fetcher<T: DeserializeOwned>(
    url: String,
) -> impl FnMut(Option<&Params>) -> Result<Page<T>> {
    move |cursor| get_page(&url, cursor)
}

/// The query parameters that ask for the page after the one just received.
type Params = Vec<(String, String)>;

/// One page of a list, newest first.
struct Page<T> {
    items: Vec<T>,
    /// Where the next page starts. `None` on the last page.
    next: Option<Params>,
}

#[derive(Deserialize)]
struct RawPage<T> {
    items: Vec<T>,
    next_page_params: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Asks for one page. An address the service has never seen is answered with
/// "not found", which for a wallet means an empty history.
fn get_page<T: DeserializeOwned>(url: &str, cursor: Option<&Params>) -> Result<Page<T>> {
    let mut request = ureq::get(url);
    if let Some(params) = cursor {
        std::thread::sleep(PAUSE_BETWEEN_PAGES);
        for (key, value) in params {
            request = request.query(key, value);
        }
    }
    let raw: RawPage<T> = match request.call() {
        Ok(mut response) => response.body_mut().read_json()?,
        Err(ureq::Error::StatusCode(404)) => {
            return Ok(Page {
                items: Vec::new(),
                next: None,
            });
        }
        Err(e) => return Err(e.into()),
    };
    let next = raw.next_page_params.map(|map| {
        map.into_iter()
            .filter_map(|(key, value)| match value {
                serde_json::Value::String(text) => Some((key, text)),
                serde_json::Value::Number(number) => Some((key, number.to_string())),
                _ => None,
            })
            .collect()
    });
    Ok(Page {
        items: raw.items,
        next,
    })
}

/// An entry of one of the lists, which says in which block it happened.
trait InBlock {
    fn block(&self) -> Option<u64>;
}

/// The paging loop. `fetch_page` receives where the next page starts, or
/// `None` for the first page. Pages arrive newest first, so paging stops as
/// soon as a page reaches a block at or below `since_height`. Entries that
/// are not in a block yet (pending) are skipped, because they have no time to
/// price. `on_progress` gets the number of entries kept so far.
fn walk<T: InBlock>(
    mut fetch_page: impl FnMut(Option<&Params>) -> Result<Page<T>>,
    since_height: u64,
    mut on_progress: impl FnMut(usize),
) -> Result<Vec<T>> {
    let mut found = Vec::new();
    let mut cursor: Option<Params> = None;
    loop {
        let page = fetch_page(cursor.as_ref())?;
        let reached_known = page
            .items
            .iter()
            .filter_map(InBlock::block)
            .any(|height| height <= since_height);
        found.extend(
            page.items
                .into_iter()
                .filter(|item| item.block().is_some_and(|height| height > since_height)),
        );
        on_progress(found.len());

        match page.next {
            Some(next) if !reached_known => cursor = Some(next),
            _ => return Ok(found),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawAddress {
    hash: String,
}

#[derive(Debug, Deserialize)]
struct RawFee {
    value: String,
}

#[derive(Debug, Deserialize)]
struct RawTx {
    hash: String,
    block_number: Option<u64>,
    /// Position inside the block.
    position: Option<u32>,
    timestamp: Option<String>,
    /// Wei, as a decimal string.
    value: String,
    fee: Option<RawFee>,
    /// `ok` or `error`.
    status: Option<String>,
    from: Option<RawAddress>,
    to: Option<RawAddress>,
}

#[derive(Debug, Deserialize)]
struct RawInternal {
    transaction_hash: String,
    block_number: Option<u64>,
    index: Option<u64>,
    timestamp: Option<String>,
    value: String,
    success: Option<bool>,
    from: Option<RawAddress>,
    to: Option<RawAddress>,
}

#[derive(Debug, Deserialize)]
struct RawWithdrawal {
    index: u64,
    block_number: Option<u64>,
    timestamp: Option<String>,
    /// Gwei, as a decimal string.
    amount: String,
}

impl InBlock for RawTx {
    fn block(&self) -> Option<u64> {
        self.block_number
    }
}

impl InBlock for RawInternal {
    fn block(&self) -> Option<u64> {
        self.block_number
    }
}

impl InBlock for RawWithdrawal {
    fn block(&self) -> Option<u64> {
        self.block_number
    }
}

fn is(address: &str, side: &Option<RawAddress>) -> bool {
    side.as_ref()
        .is_some_and(|side| side.hash.eq_ignore_ascii_case(address))
}

fn wei(text: &str) -> Result<u128> {
    text.parse()
        .map_err(|_| Error::BadResponse("an amount is not a number"))
}

fn signed(units: u128) -> i128 {
    i128::try_from(units).unwrap_or(i128::MAX)
}

fn time_of(timestamp: &Option<String>) -> Result<i64> {
    timestamp
        .as_deref()
        .and_then(parse_iso_utc)
        .ok_or(Error::BadResponse("a block time is missing"))
}

/// Combines the three lists into one activity per transaction hash, oldest
/// first.
fn merge(
    address: &str,
    transactions: &[RawTx],
    internal: &[RawInternal],
    withdrawals: &[RawWithdrawal],
) -> Result<Vec<Activity>> {
    let mut by_hash: BTreeMap<String, Activity> = BTreeMap::new();

    for tx in transactions {
        let (Some(height), sent, received) =
            (tx.block_number, is(address, &tx.from), is(address, &tx.to))
        else {
            continue;
        };
        if !sent && !received {
            continue;
        }
        // A failed transaction moves no ether, but its fee is still paid.
        let value = if tx.status.as_deref() == Some("error") {
            0
        } else {
            wei(&tx.value)?
        };
        let mut amount = 0;
        if received {
            amount += signed(value);
        }
        if sent {
            amount -= signed(value);
        }
        // The fee is paid by whoever sent the transaction.
        let fee = match (&tx.fee, sent) {
            (Some(fee), true) => wei(&fee.value)?,
            _ => 0,
        };
        by_hash.insert(
            tx.hash.clone(),
            Activity {
                txid: tx.hash.clone(),
                height,
                index: tx.position,
                time: time_of(&tx.timestamp)?,
                amount,
                fee,
                parents: Vec::new(),
            },
        );
    }

    for transfer in internal {
        let Some(height) = transfer.block_number else {
            continue;
        };
        // The first internal entry of a transaction is the call itself, whose
        // value is already counted from the transaction. Failed calls move
        // nothing.
        if transfer.index == Some(0) || transfer.success == Some(false) {
            continue;
        }
        let value = signed(wei(&transfer.value)?);
        let change = match (is(address, &transfer.to), is(address, &transfer.from)) {
            (true, false) => value,
            (false, true) => -value,
            _ => continue,
        };
        let activity = match by_hash.get_mut(&transfer.transaction_hash) {
            Some(activity) => activity,
            None => by_hash
                .entry(transfer.transaction_hash.clone())
                .or_insert(Activity {
                    txid: transfer.transaction_hash.clone(),
                    height,
                    index: None,
                    time: time_of(&transfer.timestamp)?,
                    amount: 0,
                    fee: 0,
                    parents: Vec::new(),
                }),
        };
        activity.amount += change;
    }

    for withdrawal in withdrawals {
        let Some(height) = withdrawal.block_number else {
            continue;
        };
        let id = format!("withdrawal-{}", withdrawal.index);
        by_hash.insert(
            id.clone(),
            Activity {
                txid: id,
                height,
                index: None,
                time: time_of(&withdrawal.timestamp)?,
                amount: signed(wei(&withdrawal.amount)? * WEI_PER_GWEI),
                fee: 0,
                parents: Vec::new(),
            },
        );
    }

    let mut found: Vec<Activity> = by_hash.into_values().collect();
    // Transfers that net to nothing (a contract that pays back what it
    // received) change no balance and are not worth a row.
    found.retain(|a| a.amount != 0 || a.fee != 0);
    found.sort_by_key(|a| (a.height, a.index));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const ETH: u128 = 1_000_000_000_000_000_000;

    fn txs(json: &str) -> Vec<RawTx> {
        serde_json::from_str(json).expect("fixture is valid JSON")
    }

    fn tx_json(
        hash: &str,
        block: u64,
        from: &str,
        to: &str,
        value: u128,
        fee: u128,
        status: &str,
    ) -> String {
        format!(
            r#"{{"hash":"{hash}","block_number":{block},"position":3,
                "timestamp":"2023-11-14T22:13:20.000000Z","value":"{value}",
                "fee":{{"type":"actual","value":"{fee}"}},"status":"{status}",
                "from":{{"hash":"{from}"}},"to":{{"hash":"{to}"}}}}"#
        )
    }

    fn one(json: String) -> Vec<RawTx> {
        txs(&format!("[{json}]"))
    }

    #[test]
    fn incoming_payment_has_no_fee() {
        let found = merge(
            ME,
            &one(tx_json("0x1", 100, OTHER, ME, 2 * ETH, 21_000, "ok")),
            &[],
            &[],
        )
        .expect("merges");
        assert_eq!(found.len(), 1);
        let a = &found[0];
        assert_eq!(
            (a.amount, a.fee, a.height, a.index),
            (2 * ETH as i128, 0, 100, Some(3))
        );
        assert_eq!(a.time, 1_700_000_000);
    }

    #[test]
    fn outgoing_payment_reports_the_fee_separately() {
        let found = merge(
            ME,
            &one(tx_json("0x2", 100, ME, OTHER, ETH, 21_000, "ok")),
            &[],
            &[],
        )
        .expect("merges");
        assert_eq!((found[0].amount, found[0].fee), (-(ETH as i128), 21_000));
    }

    #[test]
    fn a_failed_transaction_still_costs_its_fee() {
        let found = merge(
            ME,
            &one(tx_json("0x3", 100, ME, OTHER, ETH, 50_000, "error")),
            &[],
            &[],
        )
        .expect("merges");
        assert_eq!((found[0].amount, found[0].fee), (0, 50_000));
    }

    #[test]
    fn a_payment_to_yourself_only_costs_the_fee() {
        let found = merge(
            ME,
            &one(tx_json("0x4", 100, ME, ME, ETH, 21_000, "ok")),
            &[],
            &[],
        )
        .expect("merges");
        assert_eq!((found[0].amount, found[0].fee), (0, 21_000));
    }

    #[test]
    fn transactions_that_do_not_touch_the_address_are_ignored() {
        let found = merge(
            ME,
            &one(tx_json("0x5", 100, OTHER, OTHER, ETH, 21_000, "ok")),
            &[],
            &[],
        )
        .expect("merges");
        assert!(found.is_empty());
    }

    fn internal(json: &str) -> Vec<RawInternal> {
        serde_json::from_str(json).expect("fixture is valid JSON")
    }

    #[test]
    fn a_contract_paying_out_is_a_receipt_in_its_own_transaction() {
        let transfers = internal(&format!(
            r#"[{{"transaction_hash":"0x6","block_number":101,"index":2,
                  "timestamp":"2023-11-14T22:13:20.000000Z","value":"{ETH}","success":true,
                  "from":{{"hash":"{OTHER}"}},"to":{{"hash":"{ME}"}}}}]"#
        ));
        let found = merge(ME, &[], &transfers, &[]).expect("merges");
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].amount, found[0].fee, found[0].index),
            (ETH as i128, 0, None)
        );
    }

    #[test]
    fn internal_transfers_join_the_transaction_they_belong_to() {
        // ME calls a contract with 1 ETH and the contract sends 0.4 ETH back.
        let transactions = one(tx_json("0x7", 102, ME, OTHER, ETH, 30_000, "ok"));
        let transfers = internal(&format!(
            r#"[{{"transaction_hash":"0x7","block_number":102,"index":1,
                  "timestamp":"2023-11-14T22:13:20.000000Z","value":"{}","success":true,
                  "from":{{"hash":"{OTHER}"}},"to":{{"hash":"{ME}"}}}}]"#,
            ETH * 4 / 10
        ));
        let found = merge(ME, &transactions, &transfers, &[]).expect("merges");
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].amount, found[0].fee),
            (-(ETH as i128) * 6 / 10, 30_000)
        );
    }

    #[test]
    fn the_calls_own_entry_and_failed_calls_are_not_counted_twice() {
        let transactions = one(tx_json("0x8", 103, OTHER, ME, ETH, 21_000, "ok"));
        let transfers = internal(&format!(
            r#"[{{"transaction_hash":"0x8","block_number":103,"index":0,
                  "timestamp":"2023-11-14T22:13:20.000000Z","value":"{ETH}","success":true,
                  "from":{{"hash":"{OTHER}"}},"to":{{"hash":"{ME}"}}}},
                {{"transaction_hash":"0x8","block_number":103,"index":4,
                  "timestamp":"2023-11-14T22:13:20.000000Z","value":"{ETH}","success":false,
                  "from":{{"hash":"{OTHER}"}},"to":{{"hash":"{ME}"}}}}]"#
        ));
        let found = merge(ME, &transactions, &transfers, &[]).expect("merges");
        assert_eq!(found[0].amount, ETH as i128);
    }

    #[test]
    fn a_withdrawal_is_converted_from_gwei() {
        let withdrawals: Vec<RawWithdrawal> = serde_json::from_str(
            r#"[{"index":77,"block_number":104,"timestamp":"2023-11-14T22:13:20.000000Z",
                 "amount":"32000000000","validator_index":5}]"#,
        )
        .expect("fixture is valid JSON");
        let found = merge(ME, &[], &[], &withdrawals).expect("merges");
        assert_eq!(found[0].txid, "withdrawal-77");
        assert_eq!(found[0].amount, 32 * ETH as i128);
    }

    #[test]
    fn pending_transactions_are_skipped() {
        let pending: Vec<RawTx> = serde_json::from_str(&format!(
            r#"[{{"hash":"0x9","block_number":null,"position":null,"timestamp":null,
                  "value":"{ETH}","fee":null,"status":null,
                  "from":{{"hash":"{OTHER}"}},"to":{{"hash":"{ME}"}}}}]"#
        ))
        .expect("fixture is valid JSON");
        assert!(merge(ME, &pending, &[], &[]).expect("merges").is_empty());
    }

    #[test]
    fn results_come_out_oldest_first() {
        let transactions = txs(&format!(
            "[{},{}]",
            tx_json("0xb", 200, OTHER, ME, ETH, 0, "ok"),
            tx_json("0xa", 100, OTHER, ME, ETH, 0, "ok")
        ));
        let found = merge(ME, &transactions, &[], &[]).expect("merges");
        assert_eq!(
            found.iter().map(|a| a.height).collect::<Vec<_>>(),
            [100, 200]
        );
    }

    #[derive(Debug)]
    struct Item(Option<u64>);

    impl InBlock for Item {
        fn block(&self) -> Option<u64> {
            self.0
        }
    }

    /// Runs `walk` over fixed pages and reports how many pages it asked for
    /// and the blocks it kept.
    fn run(pages: &[&[u64]], since: u64) -> (usize, Vec<u64>) {
        let mut requested = 0;
        let kept = walk(
            |_cursor| {
                let page = pages.get(requested).copied().unwrap_or(&[]);
                requested += 1;
                Ok(Page {
                    items: page.iter().map(|h| Item(Some(*h))).collect(),
                    next: Some(vec![("block_number".to_owned(), requested.to_string())]),
                })
            },
            since,
            |_| {},
        )
        .expect("pages are valid");
        (requested, kept.iter().filter_map(InBlock::block).collect())
    }

    #[test]
    fn paging_stops_at_the_first_page_that_reaches_known_blocks() {
        let (requested, kept) = run(&[&[50, 40], &[30, 20], &[10]], 25);
        assert_eq!(requested, 2);
        assert_eq!(kept, [50, 40, 30]);
    }

    #[test]
    fn paging_stops_when_there_is_no_next_page() {
        let mut calls = 0;
        let kept = walk(
            |_| {
                calls += 1;
                Ok(Page {
                    items: vec![Item(Some(9))],
                    next: None,
                })
            },
            0,
            |_| {},
        )
        .expect("pages are valid");
        assert_eq!((calls, kept.len()), (1, 1));
    }

    #[test]
    fn the_summary_says_whether_there_are_withdrawals() {
        let with: RawSummary =
            serde_json::from_str(r#"{"has_beacon_chain_withdrawals":true,"x":1}"#)
                .expect("fixture is valid JSON");
        let without: RawSummary =
            serde_json::from_str(r#"{"is_contract":false}"#).expect("fixture is valid JSON");
        assert!(with.has_beacon_chain_withdrawals);
        assert!(!without.has_beacon_chain_withdrawals);
    }

    #[test]
    fn page_parameters_are_read_from_strings_and_numbers() {
        let page: RawPage<RawWithdrawal> = serde_json::from_str(
            r#"{"items":[],"next_page_params":{"index":5,"inserted_at":"2026-10-03T08:28:10Z","gone":null}}"#,
        )
        .expect("fixture is valid JSON");
        let params = page.next_page_params.expect("has params");
        assert_eq!(params.len(), 3);
    }
}
