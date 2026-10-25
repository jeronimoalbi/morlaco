//! Bitcoin history from the mempool.space (Esplora) API.

use serde::Deserialize;

use crate::Result;
use crate::chain::{Activity, Chain, ChainBackend, FetchOptions};

const BASE_URL: &str = "https://mempool.space/api";

/// Reads Bitcoin addresses through the Esplora API.
pub struct BitcoinBackend;

impl ChainBackend for BitcoinBackend {
    fn chain(&self) -> Chain {
        Chain::Bitcoin
    }

    fn fetch(
        &self,
        address: &str,
        since_height: u64,
        _options: &FetchOptions,
        on_progress: &mut dyn FnMut(usize),
    ) -> Result<Vec<Activity>> {
        Chain::Bitcoin.check_address(address)?;
        collect_history(
            address,
            since_height,
            |cursor| {
                let url = match cursor {
                    None => format!("{BASE_URL}/address/{address}/txs"),
                    Some(txid) => format!("{BASE_URL}/address/{address}/txs/chain/{txid}"),
                };
                let page: Vec<RawTx> = ureq::get(&url).call()?.body_mut().read_json()?;
                Ok(page)
            },
            on_progress,
        )
    }
}

#[derive(Debug, Deserialize)]
struct RawTx {
    txid: String,
    vin: Vec<RawVin>,
    vout: Vec<RawOutput>,
    fee: u64,
    status: RawStatus,
}

#[derive(Debug, Deserialize)]
struct RawVin {
    /// Id of the transaction being spent.
    #[serde(default)]
    txid: String,
    /// Absent for coinbase inputs.
    prevout: Option<RawOutput>,
}

#[derive(Debug, Deserialize)]
struct RawOutput {
    /// Absent for outputs without an address, such as OP_RETURN.
    scriptpubkey_address: Option<String>,
    value: u64,
}

#[derive(Debug, Deserialize)]
struct RawStatus {
    confirmed: bool,
    block_height: Option<u64>,
    block_time: Option<i64>,
}

impl RawTx {
    fn is_confirmed(&self) -> bool {
        self.status.confirmed
    }

    /// Returns the effect on `address`, or `None` for unconfirmed
    /// transactions and transactions that do not touch the address.
    fn activity_for(&self, address: &str) -> Option<Activity> {
        let time = self.status.block_time.filter(|_| self.is_confirmed())?;
        let height = self.status.block_height?;
        let pays_to = |o: &RawOutput| o.scriptpubkey_address.as_deref() == Some(address);

        let received: u64 = self
            .vout
            .iter()
            .filter(|o| pays_to(o))
            .map(|o| o.value)
            .sum();
        let spent: u64 = self
            .vin
            .iter()
            .filter_map(|i| i.prevout.as_ref())
            .filter(|o| pays_to(o))
            .map(|o| o.value)
            .sum();

        if received == 0 && spent == 0 {
            return None;
        }

        // When other parties fund part of the inputs, the fee is split in
        // proportion to what each side put in.
        let total_in: u64 = self
            .vin
            .iter()
            .filter_map(|i| i.prevout.as_ref())
            .map(|o| o.value)
            .sum();
        let fee = if spent > 0 && total_in > 0 {
            u128::from(self.fee) * u128::from(spent) / u128::from(total_in)
        } else {
            0
        };

        // The fee is part of what the address spent, but it is reported on
        // its own, so the amount excludes it.
        let amount = i128::from(received) - i128::from(spent) + fee as i128;

        Some(Activity {
            txid: self.txid.clone(),
            height,
            index: None,
            time,
            amount,
            fee,
            parents: self.vin.iter().map(|i| i.txid.clone()).collect(),
        })
    }
}

/// The paging loop behind [`BitcoinBackend::fetch`]. `fetch_page` receives
/// the id of the last transaction of the previous page, or `None` for the
/// first page.
///
/// Pages arrive newest first, so paging stops as soon as a page reaches a
/// block at or below `since_height`. `on_progress` is called after each page
/// with the number of transactions found so far. Unconfirmed transactions are
/// skipped because they have no block time to price.
fn collect_history(
    address: &str,
    since_height: u64,
    mut fetch_page: impl FnMut(Option<&str>) -> Result<Vec<RawTx>>,
    on_progress: &mut dyn FnMut(usize),
) -> Result<Vec<Activity>> {
    let mut found = Vec::new();
    let mut cursor: Option<String> = None;

    loop {
        let page = fetch_page(cursor.as_deref())?;

        found.extend(
            page.iter()
                .filter_map(|tx| tx.activity_for(address))
                .filter(|a| a.height > since_height),
        );
        on_progress(found.len());

        // The first page lists mempool transactions before confirmed ones,
        // so the cursor is the last confirmed transaction. A page with none
        // ends the history.
        let Some(last) = page.iter().rev().find(|tx| tx.is_confirmed()) else {
            break;
        };
        if last.status.block_height.is_some_and(|h| h <= since_height) {
            break;
        }
        cursor = Some(last.txid.clone());
    }

    found.sort_by_key(|a| a.height);
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "bc1qme00000000000000000000000000000000";
    const OTHER: &str = "bc1qother000000000000000000000000000";

    fn tx(json: &str) -> RawTx {
        serde_json::from_str(json).expect("fixture is valid JSON")
    }

    #[test]
    fn incoming_payment_is_positive_with_no_fee() {
        let t = tx(&format!(
            r#"{{"txid":"a","fee":300,
                "vin":[{{"prevout":{{"scriptpubkey_address":"{OTHER}","value":10300}}}}],
                "vout":[{{"scriptpubkey_address":"{ME}","value":10000}}],
                "status":{{"confirmed":true,"block_height":100,"block_time":1700000000}}}}"#
        ));
        let a = t.activity_for(ME).expect("touches address");
        assert_eq!((a.amount, a.fee, a.time), (10_000, 0, 1_700_000_000));
    }

    #[test]
    fn outgoing_payment_nets_out_change_and_reports_the_fee_separately() {
        let t = tx(&format!(
            r#"{{"txid":"b","fee":200,
                "vin":[{{"prevout":{{"scriptpubkey_address":"{ME}","value":50000}}}}],
                "vout":[{{"scriptpubkey_address":"{OTHER}","value":30000}},
                        {{"scriptpubkey_address":"{ME}","value":19800}}],
                "status":{{"confirmed":true,"block_height":100,"block_time":1700000100}}}}"#
        ));
        let a = t.activity_for(ME).expect("touches address");
        // 30,000 paid to someone else, plus a 200 fee.
        assert_eq!((a.amount, a.fee), (-30_000, 200));
    }

    #[test]
    fn fee_is_split_when_others_fund_part_of_the_inputs() {
        // ME puts in 30,000 of 40,000; the 400 sat fee is split 300 / 100.
        let t = tx(&format!(
            r#"{{"txid":"f","fee":400,
                "vin":[{{"txid":"p1","prevout":{{"scriptpubkey_address":"{ME}","value":30000}}}},
                       {{"txid":"p2","prevout":{{"scriptpubkey_address":"{OTHER}","value":10000}}}}],
                "vout":[{{"scriptpubkey_address":"{OTHER}","value":39600}}],
                "status":{{"confirmed":true,"block_height":7,"block_time":3}}}}"#
        ));
        let a = t.activity_for(ME).expect("touches address");
        assert_eq!((a.amount, a.fee, a.height), (-29_700, 300, 7));
        assert_eq!(a.parents, vec!["p1", "p2"]);
        assert_eq!(a.index, None);
    }

    #[test]
    fn a_wallet_that_receives_more_than_it_spent_still_pays_its_fee() {
        // ME puts in 1,000 of 11,000 and takes 4,000 out, so its balance
        // rises by 3,000. The 1,100 fee is split by what each side put in.
        let t = tx(&format!(
            r#"{{"txid":"g","fee":1100,
                "vin":[{{"prevout":{{"scriptpubkey_address":"{ME}","value":1000}}}},
                       {{"prevout":{{"scriptpubkey_address":"{OTHER}","value":10000}}}}],
                "vout":[{{"scriptpubkey_address":"{ME}","value":4000}},
                        {{"scriptpubkey_address":"{OTHER}","value":5900}}],
                "status":{{"confirmed":true,"block_height":9,"block_time":4}}}}"#
        ));
        let a = t.activity_for(ME).expect("touches address");
        // ME's share of the fee is 1,100 * 1,000 / 11,000 = 100, so the
        // amount before the fee is +3,100.
        assert_eq!((a.amount, a.fee), (3_100, 100));
        assert_eq!(a.amount - a.fee as i128, 3_000);
    }

    #[test]
    fn unconfirmed_and_unrelated_transactions_are_skipped() {
        let pending = tx(&format!(
            r#"{{"txid":"c","fee":1,"vin":[],
                "vout":[{{"scriptpubkey_address":"{ME}","value":5}}],
                "status":{{"confirmed":false}}}}"#
        ));
        assert!(pending.activity_for(ME).is_none());

        let unrelated = tx(&format!(
            r#"{{"txid":"d","fee":1,"vin":[],
                "vout":[{{"scriptpubkey_address":"{OTHER}","value":5}}],
                "status":{{"confirmed":true,"block_height":100,"block_time":1}}}}"#
        ));
        assert!(unrelated.activity_for(ME).is_none());
    }

    #[test]
    fn coinbase_inputs_and_op_return_outputs_parse() {
        let t = tx(&format!(
            r#"{{"txid":"e","fee":0,
                "vin":[{{"prevout":null}}],
                "vout":[{{"scriptpubkey_address":"{ME}","value":625}},
                        {{"value":0}}],
                "status":{{"confirmed":true,"block_height":100,"block_time":2}}}}"#
        ));
        assert_eq!(t.activity_for(ME).map(|a| a.amount), Some(625));
    }

    /// A confirmed transaction paying `ME` in block `height`.
    fn raw(txid: &str, height: u64) -> RawTx {
        tx(&format!(
            r#"{{"txid":"{txid}","fee":0,"vin":[],
                "vout":[{{"scriptpubkey_address":"{ME}","value":1000}}],
                "status":{{"confirmed":true,"block_height":{height},"block_time":{height}}}}}"#
        ))
    }

    /// Runs `collect_history` over fixed pages and reports how many pages it
    /// asked for and the heights it returned.
    fn run(pages: &[&[(&str, u64)]], since: u64) -> (usize, Vec<u64>) {
        let mut requested = 0;
        let found = collect_history(
            ME,
            since,
            |_| {
                let page = pages.get(requested).copied().unwrap_or(&[]);
                requested += 1;
                Ok(page.iter().map(|(id, h)| raw(id, *h)).collect())
            },
            &mut |_| {},
        )
        .expect("pages are valid");
        (requested, found.iter().map(|a| a.height).collect())
    }

    const PAGES: [&[(&str, u64)]; 3] = [
        &[("f", 6), ("e", 5)],
        &[("d", 4), ("c", 3)],
        &[("b", 2), ("a", 1)],
    ];

    #[test]
    fn a_full_fetch_reads_every_page_and_sorts_oldest_first() {
        // After the last full page, one empty page ends the history.
        assert_eq!(run(&PAGES, 0), (4, vec![1, 2, 3, 4, 5, 6]));
    }

    #[test]
    fn paging_stops_at_the_first_page_that_reaches_known_blocks() {
        // Heights 4 and below are known: the second page reaches them.
        assert_eq!(run(&PAGES, 4), (2, vec![5, 6]));
    }

    #[test]
    fn nothing_new_costs_a_single_request() {
        assert_eq!(run(&PAGES, 5), (1, vec![6]));
        assert_eq!(run(&PAGES, 6), (1, vec![]));
    }

    #[test]
    fn mempool_transactions_do_not_count_and_do_not_end_paging() {
        let pending = tx(&format!(
            r#"{{"txid":"p","fee":0,"vin":[],
                "vout":[{{"scriptpubkey_address":"{ME}","value":5}}],
                "status":{{"confirmed":false}}}}"#
        ));
        let mut first = vec![pending];
        first.push(raw("f", 6));
        let mut calls = 0;
        let found = collect_history(
            ME,
            0,
            |_| {
                calls += 1;
                Ok(if calls == 1 {
                    std::mem::take(&mut first)
                } else {
                    vec![]
                })
            },
            &mut |_| {},
        )
        .expect("valid");
        assert_eq!(found.len(), 1);
        assert_eq!(calls, 2);
    }
}
