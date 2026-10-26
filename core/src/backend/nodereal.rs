//! BNB Smart Chain history from NodeReal's MegaNode API.
//!
//! A node cannot list the transactions of an address, and no explorer's
//! free keyless service covers BNB Smart Chain, so the history comes from
//! NodeReal's `nr_getAssetTransfers`, which needs a (free) API key. It lists
//! the normal and the internal transfers of an address, which are turned into
//! the same neutral rows as Ethereum's and merged by
//! [`combine`](crate::backend::ethereum::combine).
//!
//! The method looks at no more than 100,000 blocks per request, and for one
//! direction at a time (sent or received), so a history is read window by
//! window, from the first block that is not saved yet to the newest.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::backend::ethereum::{Transfer, Tx, combine};
use crate::chain::{Activity, Chain, ChainBackend, FetchOptions};
use crate::{Error, Result};

/// Most blocks one request may cover.
const WINDOW: u64 = 100_000;
/// Rows per page, the most the API gives (1000 in hexadecimal).
const PAGE_SIZE: &str = "0x3e8";
/// Wait after each request, to stay within a free key's rate limit.
const PAUSE: Duration = Duration::from_millis(200);

/// Reads BNB Smart Chain addresses through NodeReal.
pub struct BnbBackend;

impl ChainBackend for BnbBackend {
    fn chain(&self) -> Chain {
        Chain::Bnb
    }

    fn fetch(
        &self,
        address: &str,
        since_height: u64,
        options: &FetchOptions,
        on_progress: &mut dyn FnMut(usize),
    ) -> Result<Vec<Activity>> {
        Chain::Bnb.check_address(address)?;
        let key = options
            .nodereal_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .ok_or(Error::KeyNeeded {
                network: Chain::Bnb.name(),
                provider: "NodeReal",
            })?;
        let rpc = HttpRpc {
            url: format!("https://bsc-mainnet.nodereal.io/v1/{key}"),
        };
        scan(
            &rpc,
            &address.to_ascii_lowercase(),
            since_height,
            on_progress,
        )
    }
}

/// Makes JSON-RPC calls. A trait so the scan can be tested without a network.
trait Rpc {
    /// Calls `method` and returns its `result`.
    fn call(&self, method: &str, params: Value) -> Result<Value>;
}

struct HttpRpc {
    url: String,
}

impl Rpc for HttpRpc {
    fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let answer = match ureq::post(&self.url).send_json(&body) {
            Ok(mut response) => response.body_mut().read_json::<Value>()?,
            // The key is part of the address, so a refused key is a 401 or 403.
            Err(ureq::Error::StatusCode(401 | 403)) => {
                return Err(Error::Provider("NodeReal refused the API key".to_owned()));
            }
            Err(e) => return Err(e.into()),
        };
        std::thread::sleep(PAUSE);
        read_answer(answer)
    }
}

/// The `result` of a JSON-RPC answer, or the error it carries.
fn read_answer(answer: Value) -> Result<Value> {
    if let Some(error) = answer.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(Error::Provider(message.to_owned()));
    }
    answer
        .get("result")
        .cloned()
        .ok_or(Error::BadResponse("the answer has no result"))
}

/// Which side of a transfer the address is on.
#[derive(Clone, Copy)]
enum Side {
    Sender,
    Recipient,
}

/// Reads the history of `address` (in lowercase) in blocks above
/// `since_height`, up to the newest block.
fn scan(
    rpc: &dyn Rpc,
    address: &str,
    since_height: u64,
    on_progress: &mut dyn FnMut(usize),
) -> Result<Vec<Activity>> {
    let head = parse_hex_u64(
        rpc.call("eth_blockNumber", json!([]))?
            .as_str()
            .ok_or(Error::BadResponse("the newest block is not text"))?,
    )?;

    let mut txs = Vec::new();
    let mut transfers = Vec::new();
    let mut start = since_height + 1;
    while start <= head {
        let end = (start + WINDOW - 1).min(head);
        for side in [Side::Sender, Side::Recipient] {
            read_window(rpc, address, (start, end), side, &mut txs, &mut transfers)?;
        }
        on_progress(txs.len() + transfers.len());
        start = end + 1;
    }
    Ok(combine(address, &txs, &transfers, &[]))
}

/// Reads every page of one side of one window of blocks.
fn read_window(
    rpc: &dyn Rpc,
    address: &str,
    (from_block, to_block): (u64, u64),
    side: Side,
    txs: &mut Vec<Tx>,
    transfers: &mut Vec<Transfer>,
) -> Result<()> {
    let mut page_key: Option<String> = None;
    loop {
        let mut query = json!({
            "category": ["external", "internal"],
            "fromBlock": format!("{from_block:#x}"),
            "toBlock": format!("{to_block:#x}"),
            "order": "asc",
            "maxCount": PAGE_SIZE,
        });
        query[match side {
            Side::Sender => "fromAddress",
            Side::Recipient => "toAddress",
        }] = json!(address);
        if let Some(key) = &page_key {
            query["pageKey"] = json!(key);
        }

        let result = rpc.call("nr_getAssetTransfers", json!([query]))?;
        let page: Page = serde_json::from_value(result)
            .map_err(|_| Error::BadResponse("the list of transfers is malformed"))?;
        for row in page.transfers {
            // A transfer from the address to itself is in both sides.
            if matches!(side, Side::Recipient) && row.from.eq_ignore_ascii_case(address) {
                continue;
            }
            row.sort_into(txs, transfers)?;
        }
        match page.page_key.filter(|key| !key.is_empty()) {
            Some(key) => page_key = Some(key),
            None => return Ok(()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Page {
    #[serde(default, rename = "pageKey")]
    page_key: Option<String>,
    #[serde(default)]
    transfers: Vec<Row>,
}

/// A number that arrives as a JSON number or as text, in decimal or `0x` hex.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Number {
    Int(u64),
    Text(String),
}

impl Number {
    fn value(&self) -> Result<u128> {
        match self {
            Number::Int(n) => Ok(u128::from(*n)),
            Number::Text(text) => parse_hex_or_decimal(text),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Row {
    category: String,
    #[serde(rename = "blockNum")]
    block_num: String,
    from: String,
    #[serde(default)]
    to: Option<String>,
    value: String,
    hash: String,
    #[serde(default, rename = "blockTimeStamp", alias = "blockTimestamp")]
    time: Option<Number>,
    #[serde(default, rename = "gasPrice")]
    gas_price: Option<Number>,
    #[serde(default, rename = "gasUsed")]
    gas_used: Option<Number>,
    /// 1 when the transaction succeeded, 0 when it failed.
    #[serde(default, rename = "receiptsStatus")]
    receipts_status: Option<Number>,
}

impl Row {
    fn failed(&self) -> Result<bool> {
        Ok(match &self.receipts_status {
            Some(status) => status.value()? == 0,
            None => false,
        })
    }

    fn time(&self) -> Result<i64> {
        let seconds = self
            .time
            .as_ref()
            .ok_or(Error::BadResponse("a block time is missing"))?
            .value()?;
        i64::try_from(seconds).map_err(|_| Error::BadResponse("a block time is too large"))
    }

    /// Files the row as a transaction or an internal transfer. Other kinds
    /// (tokens) were not asked for and are ignored.
    fn sort_into(&self, txs: &mut Vec<Tx>, transfers: &mut Vec<Transfer>) -> Result<()> {
        let height = u64::try_from(parse_hex_or_decimal(&self.block_num)?)
            .map_err(|_| Error::BadResponse("a block number is too large"))?;
        let from = self.from.to_ascii_lowercase();
        let to = self.to.clone().unwrap_or_default().to_ascii_lowercase();
        match self.category.as_str() {
            "external" => {
                let failed = self.failed()?;
                let gas = match (&self.gas_price, &self.gas_used) {
                    (Some(price), Some(used)) => price.value()? * used.value()?,
                    _ => 0,
                };
                txs.push(Tx {
                    hash: self.hash.clone(),
                    height,
                    index: None,
                    time: self.time()?,
                    // A failed transaction moves no value, but its gas is paid.
                    value: if failed {
                        0
                    } else {
                        parse_hex_or_decimal(&self.value)?
                    },
                    fee: gas,
                    from,
                    to,
                });
            }
            "internal" => {
                if self.failed()? {
                    return Ok(());
                }
                transfers.push(Transfer {
                    hash: self.hash.clone(),
                    height,
                    time: self.time()?,
                    value: parse_hex_or_decimal(&self.value)?,
                    from,
                    to,
                });
            }
            _ => {}
        }
        Ok(())
    }
}

fn parse_hex_u64(text: &str) -> Result<u64> {
    u64::try_from(parse_hex_or_decimal(text)?)
        .map_err(|_| Error::BadResponse("a number is too large"))
}

/// Reads `0x…` as hexadecimal and anything else as decimal. `0x` alone is 0.
fn parse_hex_or_decimal(text: &str) -> Result<u128> {
    let bad = Error::BadResponse("a number is malformed");
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some("") => Ok(0),
        Some(hex) => u128::from_str_radix(hex, 16).map_err(|_| bad),
        None => text.parse().map_err(|_| bad),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const ME: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const BNB: u128 = 1_000_000_000_000_000_000;

    fn hex(n: u128) -> String {
        format!("{n:#x}")
    }

    fn external(hash: &str, block: u64, from: &str, to: &str, value: u128, status: u64) -> Value {
        json!({
            "category": "external", "blockNum": hex(u128::from(block)), "from": from, "to": to,
            "value": hex(value), "asset": "BNB", "hash": hash, "blockTimeStamp": 1_700_000_000,
            "gasPrice": 5_000_000_000_u64, "gasUsed": 21_000, "receiptsStatus": status
        })
    }

    fn internal(hash: &str, block: u64, from: &str, to: &str, value: u128) -> Value {
        json!({
            "category": "internal", "blockNum": hex(u128::from(block)), "from": from, "to": to,
            "value": hex(value), "asset": "BNB", "hash": hash, "blockTimeStamp": 1_700_000_000
        })
    }

    /// A fake NodeReal: the newest block, a list of rows, and the page size.
    struct Fake {
        head: u64,
        rows: Vec<Value>,
        page_size: usize,
        windows: RefCell<Vec<(u64, u64)>>,
    }

    impl Fake {
        fn new(head: u64, rows: Vec<Value>) -> Self {
            Self {
                head,
                rows,
                page_size: 1000,
                windows: RefCell::new(Vec::new()),
            }
        }
    }

    impl Rpc for Fake {
        fn call(&self, method: &str, params: Value) -> Result<Value> {
            if method == "eth_blockNumber" {
                return Ok(json!(hex(u128::from(self.head))));
            }
            assert_eq!(method, "nr_getAssetTransfers");
            let q = &params[0];
            let block = |name: &str| parse_hex_u64(q[name].as_str().expect("block")).expect("hex");
            let (from_block, to_block) = (block("fromBlock"), block("toBlock"));
            assert!(to_block - from_block < WINDOW, "window too large");
            let offset: usize = q["pageKey"].as_str().map_or(0, |k| k.parse().expect("key"));
            if offset == 0 {
                self.windows.borrow_mut().push((from_block, to_block));
            }
            let side_matches = |row: &Value| {
                q["fromAddress"].as_str().is_none_or(|a| row["from"] == a)
                    && q["toAddress"].as_str().is_none_or(|a| row["to"] == a)
            };
            let matching: Vec<&Value> = self
                .rows
                .iter()
                .filter(|r| {
                    let b = parse_hex_u64(r["blockNum"].as_str().expect("block")).expect("hex");
                    (from_block..=to_block).contains(&b) && side_matches(r)
                })
                .collect();
            let page: Vec<&Value> = matching
                .iter()
                .skip(offset)
                .take(self.page_size)
                .copied()
                .collect();
            let more = offset + self.page_size < matching.len();
            Ok(if more {
                json!({ "pageKey": (offset + self.page_size).to_string(), "transfers": page })
            } else {
                json!({ "transfers": page })
            })
        }
    }

    fn run(fake: &Fake, since: u64) -> Vec<Activity> {
        scan(fake, ME, since, &mut |_| {}).expect("scan works")
    }

    #[test]
    fn incoming_and_outgoing_payments_and_their_fee() {
        let fake = Fake::new(
            1_000,
            vec![
                external("0x1", 10, OTHER, ME, 2 * BNB, 1),
                external("0x2", 20, ME, OTHER, BNB, 1),
            ],
        );
        let found = run(&fake, 0);
        assert_eq!(found.len(), 2);
        assert_eq!(
            (found[0].txid.as_str(), found[0].amount, found[0].fee),
            ("0x1", 2 * BNB as i128, 0)
        );
        assert_eq!(
            (found[1].txid.as_str(), found[1].amount, found[1].fee),
            ("0x2", -(BNB as i128), 21_000 * 5_000_000_000)
        );
        assert_eq!(found[0].time, 1_700_000_000);
        assert_eq!(found[0].index, None);
    }

    #[test]
    fn a_failed_transaction_costs_gas_but_moves_nothing() {
        let fake = Fake::new(100, vec![external("0x3", 10, ME, OTHER, BNB, 0)]);
        let found = run(&fake, 0);
        assert_eq!((found[0].amount, found[0].fee), (0, 21_000 * 5_000_000_000));
    }

    #[test]
    fn a_payment_to_yourself_is_counted_once() {
        let fake = Fake::new(100, vec![external("0x4", 10, ME, ME, BNB, 1)]);
        let found = run(&fake, 0);
        // Value out and in cancel; only the gas is left.
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].amount, found[0].fee), (0, 21_000 * 5_000_000_000));
    }

    #[test]
    fn internal_transfers_join_their_transaction() {
        // ME sends 1 BNB to a contract, which pays 0.4 back inside the call.
        let fake = Fake::new(
            100,
            vec![
                external("0x5", 10, ME, OTHER, BNB, 1),
                internal("0x5", 10, OTHER, ME, BNB * 4 / 10),
            ],
        );
        let found = run(&fake, 0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].amount, -(BNB as i128) * 6 / 10);
    }

    #[test]
    fn a_payout_from_a_contract_is_a_receipt_of_its_own() {
        let fake = Fake::new(100, vec![internal("0x6", 10, OTHER, ME, BNB)]);
        let found = run(&fake, 0);
        assert_eq!((found[0].amount, found[0].fee), (BNB as i128, 0));
    }

    #[test]
    fn a_failed_internal_call_moves_nothing() {
        let mut failed = internal("0x7", 10, OTHER, ME, BNB);
        failed["receiptsStatus"] = json!(0);
        let fake = Fake::new(100, vec![failed]);
        assert!(run(&fake, 0).is_empty());
    }

    #[test]
    fn pages_are_followed_until_there_are_no_more() {
        let rows: Vec<Value> = (1..=7)
            .map(|n| external(&format!("0x{n}"), n * 10, OTHER, ME, BNB, 1))
            .collect();
        let mut fake = Fake::new(1_000, rows);
        fake.page_size = 2;
        assert_eq!(run(&fake, 0).len(), 7);
    }

    #[test]
    fn blocks_are_read_in_windows_with_no_gap_and_no_overlap() {
        let fake = Fake::new(250_050, vec![]);
        run(&fake, 0);
        let windows = fake.windows.borrow();
        // Each window is asked for on both sides, in order.
        let distinct: Vec<(u64, u64)> = windows.iter().step_by(2).copied().collect();
        assert_eq!(
            distinct,
            [(1, 100_000), (100_001, 200_000), (200_001, 250_050)]
        );
        assert_eq!(windows.len(), 6);
    }

    #[test]
    fn known_blocks_are_not_asked_for_again() {
        let fake = Fake::new(300, vec![]);
        run(&fake, 250);
        assert_eq!(fake.windows.borrow()[0], (251, 300));
        // Nothing newer than what is saved means no window at all.
        let none = Fake::new(250, vec![]);
        run(&none, 250);
        assert!(none.windows.borrow().is_empty());
    }

    #[test]
    fn nothing_is_read_without_a_key() {
        let err = BnbBackend
            .fetch(ME, 0, &FetchOptions::default(), &mut |_| {})
            .expect_err("no key");
        assert!(matches!(
            err,
            Error::KeyNeeded {
                provider: "NodeReal",
                ..
            }
        ));
        let blank = FetchOptions {
            nodereal_key: Some("  ".to_owned()),
            ..FetchOptions::default()
        };
        assert!(matches!(
            BnbBackend.fetch(ME, 0, &blank, &mut |_| {}),
            Err(Error::KeyNeeded { .. })
        ));
        // An address that is not one is refused before the key matters.
        assert!(matches!(
            BnbBackend.fetch("nope", 0, &FetchOptions::default(), &mut |_| {}),
            Err(Error::InvalidAddress)
        ));
    }

    #[test]
    fn a_json_rpc_error_is_reported_in_the_providers_words() {
        let err = read_answer(
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"invalid api key"}}),
        )
        .expect_err("refused");
        assert!(matches!(err, Error::Provider(text) if text == "invalid api key"));
        assert_eq!(
            read_answer(json!({"result": "0x1"})).expect("ok"),
            json!("0x1")
        );
        assert!(matches!(read_answer(json!({})), Err(Error::BadResponse(_))));
    }

    #[test]
    fn numbers_are_read_as_hex_or_decimal_text_or_json_numbers() {
        assert_eq!(parse_hex_or_decimal("0x0").expect("n"), 0);
        assert_eq!(parse_hex_or_decimal("0x").expect("n"), 0);
        assert_eq!(
            parse_hex_or_decimal("0xde0b6b3a7640000").expect("n"),
            10u128.pow(18)
        );
        // A value padded to 32 bytes.
        assert_eq!(
            parse_hex_or_decimal(&format!("0x{:064x}", 1234)).expect("n"),
            1234
        );
        assert_eq!(parse_hex_or_decimal("42").expect("n"), 42);
        assert!(parse_hex_or_decimal("0xzz").is_err());
        assert_eq!(Number::Int(7).value().expect("n"), 7);
        assert_eq!(Number::Text("0x10".to_owned()).value().expect("n"), 16);
    }

    #[test]
    fn rows_in_the_documented_shape_are_read() {
        let doc = r#"{"category":"external","blockNum":"0x6c92dd","from":"0x646eafba97ec6ee7631887cdee468b323dd65d4f",
            "to":"0x8f07f7312f3ede8b0094f09ab1554c8d89f47ddf","value":"0x0","asset":"BNB",
            "hash":"0xf7605ce61c1855348cde8512d752265a8cbbf57e0ac4c8bc259155f8c1592838",
            "blockTimeStamp":1620089457,"gasPrice":5000000000,"gasUsed":35813,"receiptsStatus":1}"#;
        let row: Row = serde_json::from_str(doc).expect("documented row parses");
        let (mut txs, mut transfers) = (Vec::new(), Vec::new());
        row.sort_into(&mut txs, &mut transfers).expect("sorted");
        assert_eq!((txs.len(), transfers.len()), (1, 0));
        assert_eq!(
            (txs[0].height, txs[0].time, txs[0].fee),
            (7_115_485, 1_620_089_457, 35_813 * 5_000_000_000)
        );
    }
}
