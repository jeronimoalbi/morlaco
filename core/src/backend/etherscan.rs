//! Ethereum history from the Etherscan API, for people with an API key.
//!
//! Blockscout needs no key but is slow for busy addresses. Etherscan is
//! faster, and a free key is enough. The three lists it returns are the same
//! as Blockscout's, so both end up in the same
//! [`combine`](crate::backend::ethereum::combine).

use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::backend::ethereum::{Transfer, Tx, Withdrawal, combine};
use crate::chain::Activity;
use crate::{Error, Result};

const BASE_URL: &str = "https://api.etherscan.io/v2/api";
/// Ethereum's id in Etherscan's API, which serves many chains.
const CHAIN_ID: &str = "1";
/// Rows per request. Etherscan's most.
const PAGE_SIZE: usize = 1000;
/// Free keys may make a few requests per second.
const PAUSE_BETWEEN_PAGES: Duration = Duration::from_millis(250);
/// The block that ends every request: later than any block there will be.
const LAST_BLOCK: &str = "99999999";

/// Reads the history of `address` (in lowercase) from Etherscan.
pub fn fetch(
    address: &str,
    since_height: u64,
    key: &str,
    on_progress: &mut dyn FnMut(usize),
) -> Result<Vec<Activity>> {
    let ask = |action: &str, start: u64| request(address, action, start, key);

    let mut found = 0;
    let transactions: Vec<RawTx> = list(
        |start| ask("txlist", start),
        since_height,
        |n| on_progress(found + n),
    )?;
    found += transactions.len();
    let internal: Vec<RawTransfer> = list(
        |start| ask("txlistinternal", start),
        since_height,
        |n| on_progress(found + n),
    )?;
    found += internal.len();
    let withdrawals: Vec<RawWithdrawal> = list(
        |start| ask("txsBeaconWithdrawal", start),
        since_height,
        |n| on_progress(found + n),
    )?;

    Ok(combine(
        address,
        &convert_all(&transactions, RawTx::to_tx)?,
        &convert_all(&internal, RawTransfer::to_transfer)?,
        &convert_all(&withdrawals, RawWithdrawal::to_withdrawal)?,
    ))
}

fn convert_all<R, T>(rows: &[R], convert: impl Fn(&R) -> Result<Option<T>>) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.extend(convert(row)?);
    }
    Ok(out)
}

/// What every answer is wrapped in. A list that is empty comes back with
/// status "0", like an error does.
#[derive(Deserialize)]
struct Envelope {
    status: String,
    message: String,
    result: serde_json::Value,
}

/// Asks for the rows of one list from block `start` on, oldest first.
fn request(address: &str, action: &str, start: u64, key: &str) -> Result<Vec<serde_json::Value>> {
    let mut response = ureq::get(BASE_URL)
        .query("chainid", CHAIN_ID)
        .query("module", "account")
        .query("action", action)
        .query("address", address)
        .query("startblock", start.to_string())
        .query("endblock", LAST_BLOCK)
        .query("page", "1")
        .query("offset", PAGE_SIZE.to_string())
        .query("sort", "asc")
        .query("apikey", key)
        .call()?;
    let envelope: Envelope = response.body_mut().read_json()?;
    read_envelope(envelope)
}

/// The rows of an answer, or what went wrong.
fn read_envelope(envelope: Envelope) -> Result<Vec<serde_json::Value>> {
    if envelope.status == "1" {
        return match envelope.result {
            serde_json::Value::Array(rows) => Ok(rows),
            _ => Err(Error::BadResponse("the list of transactions is missing")),
        };
    }
    // No rows is not a failure.
    let note = envelope.result.as_str().unwrap_or_default();
    if envelope.result.as_array().is_some_and(Vec::is_empty)
        || envelope
            .message
            .to_ascii_lowercase()
            .contains("no transactions found")
        || envelope
            .message
            .to_ascii_lowercase()
            .contains("no records found")
    {
        return Ok(Vec::new());
    }
    if note.to_ascii_lowercase().contains("rate limit") {
        return Err(Error::RateLimited);
    }
    let reason = if note.is_empty() {
        &envelope.message
    } else {
        note
    };
    Err(Error::Provider(reason.to_owned()))
}

/// A row of one of the lists.
trait Row: DeserializeOwned {
    fn block(&self) -> u64;
    /// Tells rows apart, so one that is asked for twice is kept once.
    fn key(&self) -> String;
}

/// The paging loop. `fetch_rows` gets the block to start from and returns up
/// to `PAGE_SIZE` rows, oldest first. A full page means there may be more, so
/// the next request starts at the block of the last row, and the rows of that
/// block that came twice are dropped. Blocks up to `since_height` are known
/// already.
fn list<T: Row>(
    mut fetch_rows: impl FnMut(u64) -> Result<Vec<serde_json::Value>>,
    since_height: u64,
    mut on_progress: impl FnMut(usize),
) -> Result<Vec<T>> {
    let mut found: Vec<T> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut start = since_height + 1;
    let mut first = true;
    loop {
        if !first {
            std::thread::sleep(PAUSE_BETWEEN_PAGES);
        }
        first = false;

        let rows = fetch_rows(start)?;
        let full = rows.len() >= PAGE_SIZE;
        let page: Vec<T> = rows
            .into_iter()
            .map(|row| {
                serde_json::from_value(row).map_err(|_| Error::BadResponse("an entry is malformed"))
            })
            .collect::<Result<_>>()?;
        let last_block = page.last().map(Row::block);
        for row in page {
            if seen.insert(row.key()) {
                found.push(row);
            }
        }
        on_progress(found.len());

        match last_block {
            Some(block) if full => {
                if block == start {
                    // A whole page in one block cannot be paged by block.
                    return Err(Error::BadResponse("too many entries in a single block"));
                }
                start = block;
            }
            _ => return Ok(found),
        }
    }
}

fn number<T: std::str::FromStr>(text: &str, what: &'static str) -> Result<T> {
    text.parse().map_err(|_| Error::BadResponse(what))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTx {
    block_number: String,
    time_stamp: String,
    hash: String,
    #[serde(default)]
    transaction_index: String,
    from: String,
    #[serde(default)]
    to: String,
    value: String,
    gas_used: String,
    gas_price: String,
    /// "1" for a transaction that failed.
    #[serde(default)]
    is_error: String,
}

impl Row for RawTx {
    fn block(&self) -> u64 {
        self.block_number.parse().unwrap_or(0)
    }

    fn key(&self) -> String {
        self.hash.clone()
    }
}

impl RawTx {
    fn to_tx(&self) -> Result<Option<Tx>> {
        let gas_used: u128 = number(&self.gas_used, "gas used is not a number")?;
        let gas_price: u128 = number(&self.gas_price, "a gas price is not a number")?;
        let failed = self.is_error == "1";
        Ok(Some(Tx {
            hash: self.hash.clone(),
            height: number(&self.block_number, "a block number is not a number")?,
            index: self.transaction_index.parse().ok(),
            time: number(&self.time_stamp, "a block time is not a number")?,
            // A failed transaction moves no ether, but its gas is paid.
            value: if failed {
                0
            } else {
                number(&self.value, "an amount is not a number")?
            },
            fee: gas_used * gas_price,
            from: self.from.to_ascii_lowercase(),
            to: self.to.to_ascii_lowercase(),
        }))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTransfer {
    block_number: String,
    time_stamp: String,
    hash: String,
    from: String,
    #[serde(default)]
    to: String,
    value: String,
    /// "1" for a call that failed.
    #[serde(default)]
    is_error: String,
    /// Where in the transaction's calls this is.
    #[serde(default)]
    trace_id: String,
}

impl Row for RawTransfer {
    fn block(&self) -> u64 {
        self.block_number.parse().unwrap_or(0)
    }

    fn key(&self) -> String {
        format!("{}/{}", self.hash, self.trace_id)
    }
}

impl RawTransfer {
    fn to_transfer(&self) -> Result<Option<Transfer>> {
        // Etherscan lists only the calls made inside a transaction, not the
        // transaction itself. Failed calls moved nothing.
        if self.is_error == "1" {
            return Ok(None);
        }
        Ok(Some(Transfer {
            hash: self.hash.clone(),
            height: number(&self.block_number, "a block number is not a number")?,
            time: number(&self.time_stamp, "a block time is not a number")?,
            value: number(&self.value, "an amount is not a number")?,
            from: self.from.to_ascii_lowercase(),
            to: self.to.to_ascii_lowercase(),
        }))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWithdrawal {
    withdrawal_index: String,
    block_number: String,
    timestamp: String,
    /// Gwei.
    amount: String,
}

impl Row for RawWithdrawal {
    fn block(&self) -> u64 {
        self.block_number.parse().unwrap_or(0)
    }

    fn key(&self) -> String {
        self.withdrawal_index.clone()
    }
}

impl RawWithdrawal {
    fn to_withdrawal(&self) -> Result<Option<Withdrawal>> {
        let gwei: u128 = number(&self.amount, "an amount is not a number")?;
        Ok(Some(Withdrawal {
            index: number(&self.withdrawal_index, "a withdrawal index is not a number")?,
            height: number(&self.block_number, "a block number is not a number")?,
            time: number(&self.timestamp, "a block time is not a number")?,
            wei: gwei * 1_000_000_000,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn envelope(json: &str) -> Envelope {
        serde_json::from_str(json).expect("fixture is valid JSON")
    }

    #[test]
    fn rows_are_read_from_a_successful_answer() {
        let rows = read_envelope(envelope(
            r#"{"status":"1","message":"OK","result":[{"a":1}]}"#,
        ))
        .expect("rows");
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn an_empty_list_is_not_an_error() {
        let rows = read_envelope(envelope(
            r#"{"status":"0","message":"No transactions found","result":[]}"#,
        ))
        .expect("no rows");
        assert!(rows.is_empty());
    }

    #[test]
    fn a_wrong_key_is_reported_with_the_providers_words() {
        let err = read_envelope(envelope(
            r#"{"status":"0","message":"NOTOK","result":"Invalid API Key"}"#,
        ))
        .expect_err("refused");
        assert!(matches!(err, Error::Provider(text) if text == "Invalid API Key"));
    }

    #[test]
    fn hitting_the_rate_limit_is_its_own_error() {
        let err = read_envelope(envelope(
            r#"{"status":"0","message":"NOTOK","result":"Max rate limit reached"}"#,
        ))
        .expect_err("refused");
        assert!(matches!(err, Error::RateLimited));
    }

    fn tx_row(
        hash: &str,
        block: u64,
        from: &str,
        to: &str,
        value: u128,
        is_error: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "blockNumber": block.to_string(), "timeStamp": "1700000000", "hash": hash,
            "transactionIndex": "7", "from": from, "to": to, "value": value.to_string(),
            "gasUsed": "21000", "gasPrice": "50000000000", "isError": is_error,
            "txreceipt_status": "1"
        })
    }

    #[test]
    fn a_transaction_costs_gas_used_times_gas_price() {
        let row: RawTx =
            serde_json::from_value(tx_row("0x1", 100, ME, OTHER, 5, "0")).expect("row");
        let tx = row.to_tx().expect("converts").expect("kept");
        assert_eq!(tx.fee, 21_000 * 50_000_000_000);
        assert_eq!(
            (tx.height, tx.index, tx.time, tx.value),
            (100, Some(7), 1_700_000_000, 5)
        );
    }

    #[test]
    fn a_failed_transaction_keeps_its_fee_but_not_its_value() {
        let row: RawTx =
            serde_json::from_value(tx_row("0x2", 100, ME, OTHER, 5, "1")).expect("row");
        let tx = row.to_tx().expect("converts").expect("kept");
        assert_eq!((tx.value, tx.fee > 0), (0, true));
    }

    #[test]
    fn internal_transfers_and_withdrawals_are_read() {
        let transfer: RawTransfer = serde_json::from_value(serde_json::json!({
            "blockNumber": "101", "timeStamp": "1700000000", "hash": "0x3",
            "from": OTHER, "to": ME, "value": "1000", "isError": "0", "traceId": "0_1"
        }))
        .expect("row");
        assert_eq!(transfer.key(), "0x3/0_1");
        let t = transfer.to_transfer().expect("converts").expect("kept");
        assert_eq!((t.value, t.height), (1000, 101));

        let failed: RawTransfer = serde_json::from_value(serde_json::json!({
            "blockNumber": "101", "timeStamp": "1700000000", "hash": "0x3",
            "from": OTHER, "to": ME, "value": "1000", "isError": "1", "traceId": "0_2"
        }))
        .expect("row");
        assert!(failed.to_transfer().expect("converts").is_none());

        let withdrawal: RawWithdrawal = serde_json::from_value(serde_json::json!({
            "withdrawalIndex": "77", "validatorIndex": "5", "address": ME,
            "amount": "32000000000", "blockNumber": "104", "timestamp": "1700000000"
        }))
        .expect("row");
        let w = withdrawal.to_withdrawal().expect("converts").expect("kept");
        assert_eq!((w.index, w.wei), (77, 32_000_000_000_000_000_000));
    }

    /// Fake rows 1..=total, one per block, `PAGE_SIZE` at a time from `start`.
    fn blocks(total: u64) -> impl FnMut(u64) -> Result<Vec<serde_json::Value>> {
        move |start| {
            Ok((start..=total)
                .take(PAGE_SIZE)
                .map(|b| tx_row(&format!("0x{b}"), b, ME, OTHER, 1, "0"))
                .collect())
        }
    }

    #[test]
    fn a_long_history_is_read_in_pages_without_repeats() {
        let rows: Vec<RawTx> = list(blocks(2_500), 0, |_| {}).expect("pages are valid");
        assert_eq!(rows.len(), 2_500);
        assert_eq!(rows.first().map(Row::block), Some(1));
        assert_eq!(rows.last().map(Row::block), Some(2_500));
    }

    #[test]
    fn known_blocks_are_not_asked_for_again() {
        let rows: Vec<RawTx> = list(blocks(30), 20, |_| {}).expect("pages are valid");
        assert_eq!(
            rows.iter().map(Row::block).collect::<Vec<_>>(),
            (21..=30).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_page_inside_one_block_is_an_error_not_a_loop() {
        let result: Result<Vec<RawTx>> = list(
            |_| {
                Ok((0..PAGE_SIZE)
                    .map(|n| tx_row(&format!("0x{n}"), 50, ME, OTHER, 1, "0"))
                    .collect())
            },
            49,
            |_| {},
        );
        assert!(matches!(result, Err(Error::BadResponse(_))));
    }
}
