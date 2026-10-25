//! Bitcoin address history from the mempool.space (Esplora) API.

use serde::Deserialize;

use crate::{Error, Result};

const BASE_URL: &str = "https://mempool.space/api";

/// The effect of one confirmed transaction on a single address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    /// Transaction id.
    pub txid: String,
    /// Block height, used to order transactions.
    pub height: u64,
    /// Block time as a Unix timestamp.
    pub time: i64,
    /// Sats received minus sats spent by the address. Negative when the
    /// address paid out.
    pub net_sats: i64,
    /// Share of the network fee paid by this address: all of it when the
    /// address funded the whole transaction, none when it funded nothing.
    pub fee_sats: u64,
    /// Ids of the transactions whose outputs this one spends. Lets
    /// transactions in the same block be put in spending order.
    pub parents: Vec<String>,
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
    fn delta_for(&self, address: &str) -> Option<Delta> {
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
        let fee_sats = if spent > 0 && total_in > 0 {
            (u128::from(self.fee) * u128::from(spent) / u128::from(total_in)) as u64
        } else {
            0
        };

        Some(Delta {
            txid: self.txid.clone(),
            height,
            time,
            net_sats: received as i64 - spent as i64,
            fee_sats,
            parents: self.vin.iter().map(|i| i.txid.clone()).collect(),
        })
    }
}

/// Checks that `address` looks plausible before it is put in a URL.
///
/// This is not a checksum validation; the API rejects bad addresses with
/// HTTP 400, which maps to [`Error::InvalidAddress`].
pub fn check_address(address: &str) -> Result<()> {
    let plausible =
        (14..=90).contains(&address.len()) && address.chars().all(|c| c.is_ascii_alphanumeric());
    if plausible {
        Ok(())
    } else {
        Err(Error::InvalidAddress)
    }
}

/// Downloads the full confirmed history of `address`, oldest first.
///
/// `on_progress` is called after each page with the number of transactions
/// found so far. Unconfirmed transactions are skipped because they have no
/// block time to price.
pub fn fetch_history(address: &str, mut on_progress: impl FnMut(usize)) -> Result<Vec<Delta>> {
    check_address(address)?;

    let mut deltas = Vec::new();
    let mut last_seen: Option<String> = None;

    loop {
        let url = match &last_seen {
            None => format!("{BASE_URL}/address/{address}/txs"),
            Some(txid) => format!("{BASE_URL}/address/{address}/txs/chain/{txid}"),
        };
        let page: Vec<RawTx> = ureq::get(&url).call()?.body_mut().read_json()?;

        deltas.extend(page.iter().filter_map(|tx| tx.delta_for(address)));
        on_progress(deltas.len());

        // The first page lists mempool transactions before confirmed ones, so
        // the cursor is the last confirmed transaction. A page with none ends
        // the history.
        match page.iter().rev().find(|tx| tx.is_confirmed()) {
            Some(tx) => last_seen = Some(tx.txid.clone()),
            None => break,
        }
    }

    deltas.sort_by_key(|d| d.height);
    Ok(deltas)
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
        let d = t.delta_for(ME).expect("touches address");
        assert_eq!((d.net_sats, d.fee_sats, d.time), (10_000, 0, 1_700_000_000));
    }

    #[test]
    fn outgoing_payment_nets_out_change_and_carries_the_fee() {
        let t = tx(&format!(
            r#"{{"txid":"b","fee":200,
                "vin":[{{"prevout":{{"scriptpubkey_address":"{ME}","value":50000}}}}],
                "vout":[{{"scriptpubkey_address":"{OTHER}","value":30000}},
                        {{"scriptpubkey_address":"{ME}","value":19800}}],
                "status":{{"confirmed":true,"block_height":100,"block_time":1700000100}}}}"#
        ));
        let d = t.delta_for(ME).expect("touches address");
        assert_eq!((d.net_sats, d.fee_sats), (-30_200, 200));
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
        let d = t.delta_for(ME).expect("touches address");
        assert_eq!((d.net_sats, d.fee_sats, d.height), (-30_000, 300, 7));
        assert_eq!(d.parents, vec!["p1", "p2"]);
    }

    #[test]
    fn unconfirmed_and_unrelated_transactions_are_skipped() {
        let pending = tx(&format!(
            r#"{{"txid":"c","fee":1,"vin":[],
                "vout":[{{"scriptpubkey_address":"{ME}","value":5}}],
                "status":{{"confirmed":false}}}}"#
        ));
        assert!(pending.delta_for(ME).is_none());

        let unrelated = tx(&format!(
            r#"{{"txid":"d","fee":1,"vin":[],
                "vout":[{{"scriptpubkey_address":"{OTHER}","value":5}}],
                "status":{{"confirmed":true,"block_height":100,"block_time":1}}}}"#
        ));
        assert!(unrelated.delta_for(ME).is_none());
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
        assert_eq!(t.delta_for(ME).map(|d| d.net_sats), Some(625));
    }

    #[test]
    fn implausible_addresses_are_rejected() {
        assert!(check_address("").is_err());
        assert!(check_address("not an address!!!!!").is_err());
        assert!(check_address("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq").is_ok());
    }
}
