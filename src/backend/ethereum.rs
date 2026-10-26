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

use crate::backend::etherscan;
use crate::chain::{Activity, Chain, ChainBackend, FetchOptions};
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
        options: &FetchOptions,
        on_progress: &mut dyn FnMut(usize),
    ) -> Result<Vec<Activity>> {
        Chain::Ethereum.check_address(address)?;
        let address = address.to_ascii_lowercase();
        if let Some(key) = &options.etherscan_key {
            return etherscan::fetch(&address, since_height, key, on_progress);
        }
        fetch_blockscout(&Http { address: &address }, since_height, on_progress)
    }
}

/// Where Blockscout's lists of one address come from.
trait Lists {
    fn address(&self) -> &str;
    fn transactions(&self, cursor: Option<&Params>) -> Result<Page<RawTx>>;
    fn internal(&self, cursor: Option<&Params>) -> Result<Page<RawInternal>>;
    fn withdrawals(&self, cursor: Option<&Params>) -> Result<Page<RawWithdrawal>>;
    /// Whether the address has received staking withdrawals.
    fn has_withdrawals(&self) -> Result<bool>;
}

/// The lists of the real service.
struct Http<'a> {
    address: &'a str,
}

impl Http<'_> {
    fn url(&self, path: &str) -> String {
        format!("{BASE_URL}/addresses/{}/{path}", self.address)
    }
}

impl Lists for Http<'_> {
    fn address(&self) -> &str {
        self.address
    }

    fn transactions(&self, cursor: Option<&Params>) -> Result<Page<RawTx>> {
        get_page(&self.url("transactions"), cursor)
    }

    fn internal(&self, cursor: Option<&Params>) -> Result<Page<RawInternal>> {
        get_page(&self.url("internal-transactions"), cursor)
    }

    fn withdrawals(&self, cursor: Option<&Params>) -> Result<Page<RawWithdrawal>> {
        get_page(&self.url("withdrawals"), cursor)
    }

    fn has_withdrawals(&self) -> Result<bool> {
        has_withdrawals(self.address)
    }
}

/// Reads the three lists of an address and merges them.
fn fetch_blockscout(
    lists: &impl Lists,
    since_height: u64,
    mut on_progress: impl FnMut(usize),
) -> Result<Vec<Activity>> {
    let mut found = 0;
    let transactions = walk(
        |c| lists.transactions(c),
        since_height,
        |n| on_progress(found + n),
    )?;
    found += transactions.len();
    let internal = walk(
        |c| lists.internal(c),
        since_height,
        |n| on_progress(found + n),
    )?;
    found += internal.len();
    // Asking for the withdrawals of an address that has none takes minutes to
    // fail, so the address summary says first whether there are any.
    let withdrawals = if lists.has_withdrawals()? {
        walk(
            |c| lists.withdrawals(c),
            since_height,
            |n| on_progress(found + n),
        )?
    } else {
        Vec::new()
    };
    merge(lists.address(), &transactions, &internal, &withdrawals)
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
    page_from_answer(request.call())
}

/// The page an answer holds. "Not found" is an empty page.
fn page_from_answer<T: DeserializeOwned>(
    answer: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<Page<T>> {
    match answer {
        Ok(mut response) => Ok(page_of(response.body_mut().read_json()?)),
        Err(ureq::Error::StatusCode(404)) => Ok(Page {
            items: Vec::new(),
            next: None,
        }),
        Err(e) => Err(e.into()),
    }
}

/// The page, with the cursor of the next one as query parameters.
fn page_of<T>(raw: RawPage<T>) -> Page<T> {
    let next = raw.next_page_params.map(|map| {
        map.into_iter()
            .filter_map(|(key, value)| match value {
                serde_json::Value::String(text) => Some((key, text)),
                serde_json::Value::Number(number) => Some((key, number.to_string())),
                _ => None,
            })
            .collect()
    });
    Page {
        items: raw.items,
        next,
    }
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

#[derive(Debug, Clone, Deserialize)]
struct RawAddress {
    hash: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RawFee {
    value: String,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

/// A transaction in the form both providers are turned into. Addresses are
/// in lowercase.
pub(crate) struct Tx {
    pub hash: String,
    pub height: u64,
    /// Position inside the block.
    pub index: Option<u32>,
    pub time: i64,
    /// Wei sent along. Zero for a failed transaction, which moves no ether.
    pub value: u128,
    /// Wei paid for gas.
    pub fee: u128,
    pub from: String,
    pub to: String,
}

/// Ether moved by a contract while it ran a transaction. Failed calls are
/// left out before this point.
pub(crate) struct Transfer {
    pub hash: String,
    pub height: u64,
    pub time: i64,
    pub value: u128,
    pub from: String,
    pub to: String,
}

/// Ether paid out by the staking system, which belongs to no transaction.
pub(crate) struct Withdrawal {
    pub index: u64,
    pub height: u64,
    pub time: i64,
    pub wei: u128,
}

/// Turns Blockscout's entries into the common form, skipping what is not in
/// a block yet, failed calls and the call entry that repeats a transaction's
/// own value.
fn merge(
    address: &str,
    transactions: &[RawTx],
    internal: &[RawInternal],
    withdrawals: &[RawWithdrawal],
) -> Result<Vec<Activity>> {
    let mut txs = Vec::new();
    for tx in transactions {
        let Some(height) = tx.block_number else {
            continue;
        };
        let failed = tx.status.as_deref() == Some("error");
        txs.push(Tx {
            hash: tx.hash.clone(),
            height,
            index: tx.position,
            time: time_of(&tx.timestamp)?,
            value: if failed { 0 } else { wei(&tx.value)? },
            fee: match &tx.fee {
                Some(fee) => wei(&fee.value)?,
                None => 0,
            },
            from: lowercase(&tx.from),
            to: lowercase(&tx.to),
        });
    }

    let mut transfers = Vec::new();
    for transfer in internal {
        let Some(height) = transfer.block_number else {
            continue;
        };
        // The first internal entry of a transaction is the call itself, whose
        // value is already counted from the transaction.
        if transfer.index == Some(0) || transfer.success == Some(false) {
            continue;
        }
        transfers.push(Transfer {
            hash: transfer.transaction_hash.clone(),
            height,
            time: time_of(&transfer.timestamp)?,
            value: wei(&transfer.value)?,
            from: lowercase(&transfer.from),
            to: lowercase(&transfer.to),
        });
    }

    let mut payouts = Vec::new();
    for withdrawal in withdrawals {
        let Some(height) = withdrawal.block_number else {
            continue;
        };
        payouts.push(Withdrawal {
            index: withdrawal.index,
            height,
            time: time_of(&withdrawal.timestamp)?,
            wei: wei(&withdrawal.amount)? * WEI_PER_GWEI,
        });
    }

    Ok(combine(address, &txs, &transfers, &payouts))
}

fn lowercase(side: &Option<RawAddress>) -> String {
    side.as_ref()
        .map(|side| side.hash.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Combines transactions, internal transfers and withdrawals into one
/// activity per transaction hash, oldest first. `address` is in lowercase.
pub(crate) fn combine(
    address: &str,
    transactions: &[Tx],
    internal: &[Transfer],
    withdrawals: &[Withdrawal],
) -> Vec<Activity> {
    let mut by_hash: BTreeMap<String, Activity> = BTreeMap::new();

    for tx in transactions {
        let (sent, received) = (tx.from == address, tx.to == address);
        if !sent && !received {
            continue;
        }
        let mut amount = 0;
        if received {
            amount += signed(tx.value);
        }
        if sent {
            amount -= signed(tx.value);
        }
        by_hash.insert(
            tx.hash.clone(),
            Activity {
                txid: tx.hash.clone(),
                height: tx.height,
                index: tx.index,
                time: tx.time,
                amount,
                // The fee is paid by whoever sent the transaction.
                fee: if sent { tx.fee } else { 0 },
                parents: Vec::new(),
            },
        );
    }

    for transfer in internal {
        let value = signed(transfer.value);
        let change = match (transfer.to == address, transfer.from == address) {
            (true, false) => value,
            (false, true) => -value,
            _ => continue,
        };
        by_hash
            .entry(transfer.hash.clone())
            .or_insert(Activity {
                txid: transfer.hash.clone(),
                height: transfer.height,
                index: None,
                time: transfer.time,
                amount: 0,
                fee: 0,
                parents: Vec::new(),
            })
            .amount += change;
    }

    for withdrawal in withdrawals {
        let id = format!("withdrawal-{}", withdrawal.index);
        by_hash.insert(
            id.clone(),
            Activity {
                txid: id,
                height: withdrawal.height,
                index: None,
                time: withdrawal.time,
                amount: signed(withdrawal.wei),
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
    found
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

    /// Lists served from memory, newest first, one page per call.
    struct Memory {
        transactions: Vec<RawTx>,
        withdrawals: Vec<RawWithdrawal>,
        has_withdrawals: bool,
        asked_withdrawals: std::cell::Cell<bool>,
    }

    fn only_page<T: Clone>(items: &[T]) -> Result<Page<T>> {
        Ok(Page {
            items: items.to_vec(),
            next: None,
        })
    }

    impl Lists for Memory {
        fn address(&self) -> &str {
            ME
        }

        fn transactions(&self, _: Option<&Params>) -> Result<Page<RawTx>> {
            only_page(&self.transactions)
        }

        fn internal(&self, _: Option<&Params>) -> Result<Page<RawInternal>> {
            only_page(&[])
        }

        fn withdrawals(&self, _: Option<&Params>) -> Result<Page<RawWithdrawal>> {
            self.asked_withdrawals.set(true);
            only_page(&self.withdrawals)
        }

        fn has_withdrawals(&self) -> Result<bool> {
            Ok(self.has_withdrawals)
        }
    }

    fn memory(has_withdrawals: bool) -> Memory {
        Memory {
            transactions: one(tx_json("0x1", 100, OTHER, ME, ETH, 0, "ok")),
            withdrawals: serde_json::from_str(
                r#"[{"index":7,"block_number":200,"timestamp":"2023-11-14T22:13:20.000000Z","amount":"1000000000"}]"#,
            )
            .expect("fixture is valid JSON"),
            has_withdrawals,
            asked_withdrawals: std::cell::Cell::new(false),
        }
    }

    #[test]
    fn the_three_lists_are_merged_and_progress_adds_up() {
        let lists = memory(true);
        let mut progress = Vec::new();
        let found = fetch_blockscout(&lists, 0, |n| progress.push(n)).expect("fetch");
        assert_eq!(found.len(), 2);
        assert_eq!(progress, [1, 1, 2]);
    }

    #[test]
    fn withdrawals_are_not_asked_for_when_the_address_has_none() {
        let lists = memory(false);
        let found = fetch_blockscout(&lists, 0, |_| {}).expect("fetch");
        assert_eq!(found.len(), 1);
        assert!(!lists.asked_withdrawals.get());
    }

    #[test]
    fn the_cursor_of_the_next_page_becomes_query_parameters() {
        let raw: RawPage<RawTx> = serde_json::from_str(
            r#"{"items":[],"next_page_params":{"block_number":5,"items_count":50,"label":"x","skip":null}}"#,
        )
        .expect("fixture is valid JSON");
        let mut next = page_of(raw).next.expect("cursor");
        next.sort();
        assert_eq!(
            next,
            [
                ("block_number".to_owned(), "5".to_owned()),
                ("items_count".to_owned(), "50".to_owned()),
                ("label".to_owned(), "x".to_owned()),
            ]
        );
    }

    #[test]
    fn the_last_page_has_no_cursor() {
        let raw: RawPage<RawTx> = serde_json::from_str(r#"{"items":[],"next_page_params":null}"#)
            .expect("fixture is valid JSON");
        assert!(page_of(raw).next.is_none());
    }

    #[test]
    fn an_address_the_service_never_saw_has_an_empty_page() {
        let page: Page<RawTx> =
            page_from_answer(Err(ureq::Error::StatusCode(404))).expect("empty page");
        assert!(page.items.is_empty() && page.next.is_none());
    }

    #[test]
    fn other_failures_are_errors() {
        let result: Result<Page<RawTx>> = page_from_answer(Err(ureq::Error::StatusCode(500)));
        assert!(result.is_err());
    }

    #[test]
    fn a_page_is_read_from_the_answer() {
        let body = r#"{"items":[],"next_page_params":{"block_number":5}}"#;
        let answer = ureq::http::Response::new(ureq::Body::builder().data(body));
        let page: Page<RawTx> = page_from_answer(Ok(answer)).expect("page");
        assert!(page.next.is_some());
    }
}
