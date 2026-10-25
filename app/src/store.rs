//! Saved wallets and settings, kept in the database.
//!
//! The store holds a copy in memory for the UI and writes every change
//! through to the database first, so a failed write never leaves the screen
//! showing something that was not saved.

use morlaco_core::Result;
use morlaco_core::chain::{Chain, WalletId};
use morlaco_core::db::Database;
use morlaco_core::portfolio::History;

const HIDDEN_KEY: &str = "hidden";

/// A tracked wallet with an optional user label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wallet {
    /// The chain and address.
    pub id: WalletId,
    /// Name chosen by the user; may be empty.
    pub label: String,
}

impl Wallet {
    /// The label if there is one, otherwise a shortened address.
    pub fn display_name(&self) -> String {
        if self.label.is_empty() {
            crate::format::short_address(&self.id.address)
        } else {
            self.label.clone()
        }
    }
}

/// Everything the app remembers between runs.
pub struct Store {
    /// Tracked wallets in the order they were added.
    pub wallets: Vec<Wallet>,
    /// Whether amounts are masked.
    pub hidden: bool,
    db: Database,
}

impl Store {
    /// Reads wallets and settings from `db`.
    pub fn open(db: Database) -> Result<Self> {
        let wallets = db
            .wallets()?
            .into_iter()
            .map(|w| Wallet {
                id: w.id,
                label: w.label,
            })
            .collect();
        let hidden = db.setting(HIDDEN_KEY)?.as_deref() == Some("1");
        Ok(Self {
            wallets,
            hidden,
            db,
        })
    }

    /// Index of the wallet with this id.
    pub fn position(&self, id: &WalletId) -> Option<usize> {
        self.wallets.iter().position(|w| &w.id == id)
    }

    /// Every chain that has at least one tracked wallet, in the order the
    /// chains are listed.
    pub fn chains_in_use(&self) -> Vec<Chain> {
        Chain::ALL
            .iter()
            .copied()
            .filter(|c| self.wallets.iter().any(|w| w.id.chain == *c))
            .collect()
    }

    /// Adds a wallet and returns its index. Control characters in the label
    /// are replaced with spaces.
    pub fn add(&mut self, id: &WalletId, label: &str) -> Result<usize> {
        let label = clean_label(label);
        self.db.add_wallet(id, &label)?;
        self.wallets.push(Wallet {
            id: id.clone(),
            label,
        });
        Ok(self.wallets.len() - 1)
    }

    /// Changes the label of the wallet at `index`, if it exists. An empty
    /// label makes the wallet display its address again.
    pub fn rename(&mut self, index: usize, label: &str) -> Result<()> {
        let label = clean_label(label);
        if let Some(wallet) = self.wallets.get_mut(index) {
            self.db.rename_wallet(&wallet.id, &label)?;
            wallet.label = label;
        }
        Ok(())
    }

    /// Removes the wallet at `index`, if it exists.
    pub fn remove(&mut self, index: usize) -> Result<()> {
        if let Some(wallet) = self.wallets.get(index) {
            self.db.remove_wallet(&wallet.id)?;
            self.wallets.remove(index);
        }
        Ok(())
    }

    /// Turns amount masking on or off.
    pub fn set_hidden(&mut self, hidden: bool) -> Result<()> {
        self.db
            .set_setting(HIDDEN_KEY, if hidden { "1" } else { "0" })?;
        self.hidden = hidden;
        Ok(())
    }

    /// The history saved for `wallet`, or `None` if it was never fetched.
    pub fn saved_history(&self, wallet: &WalletId) -> Result<Option<History>> {
        if !self.db.is_synced(wallet)? {
            return Ok(None);
        }
        Ok(Some(History {
            wallet: wallet.clone(),
            txs: self.db.history(wallet)?,
        }))
    }

    /// The USD price of `chain`'s coin from the last successful refresh.
    pub fn last_price(&self, chain: Chain) -> Option<f64> {
        let saved = self.db.setting(&last_price_key(chain)).ok()??;
        saved.parse().ok()
    }

    /// Remembers the price so the next start can show values at once.
    pub fn set_last_price(&self, chain: Chain, usd: f64) -> Result<()> {
        self.db
            .set_setting(&last_price_key(chain), &usd.to_string())
    }

    /// Re-encrypts the database with a new key. See [`Database::rekey`].
    pub fn rekey(&self, key: &str) -> Result<()> {
        self.db.rekey(key)
    }

    /// The underlying database, to check what was written in tests.
    #[cfg(test)]
    pub fn database(&self) -> &Database {
        &self.db
    }
}

fn last_price_key(chain: Chain) -> String {
    format!("last_price:{}", chain.key())
}

/// Trims a label and replaces control characters so they cannot garble the
/// display.
fn clean_label(label: &str) -> String {
    label
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(Database::in_memory().expect("in-memory db opens")).expect("store opens")
    }

    fn btc(address: &str) -> WalletId {
        WalletId::new(Chain::Bitcoin, address)
    }

    #[test]
    fn changes_are_written_through_to_the_database() {
        let mut store = store();
        store
            .add(&btc("bc1qaaaaaaaaaaaaaaaa"), "Cold storage")
            .expect("add");
        store.add(&btc("bc1qbbbbbbbbbbbbbbbb"), "").expect("add");
        store.set_hidden(true).expect("set");

        let stored = store.database().wallets().expect("list");
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].label, "Cold storage");
        assert_eq!(
            store.database().setting("hidden").expect("read"),
            Some("1".into())
        );
    }

    #[test]
    fn labels_are_cleaned() {
        let mut store = store();
        store
            .add(&btc("bc1qaaaaaaaaaaaaaaaa"), "  evil\tlabel\nname ")
            .expect("add");
        assert_eq!(store.wallets[0].label, "evil label name");
    }

    #[test]
    fn rename_sets_and_clears_the_label() {
        let mut store = store();
        store.add(&btc("bc1qaaaaaaaaaaaaaaaa"), "Old").expect("add");

        store.rename(0, "  New\tname ").expect("rename");
        assert_eq!(store.wallets[0].label, "New name");
        assert_eq!(
            store.database().wallets().expect("list")[0].label,
            "New name"
        );

        store.rename(0, "").expect("rename");
        assert_eq!(store.wallets[0].display_name(), "bc1qaaaa…aaaaaa");

        store
            .rename(9, "ignored")
            .expect("out of range is not an error");
        assert_eq!(store.wallets.len(), 1);
    }

    #[test]
    fn a_failed_write_leaves_memory_unchanged() {
        let mut store = store();
        store.add(&btc("bc1qaaaaaaaaaaaaaaaa"), "").expect("add");
        assert!(store.add(&btc("bc1qaaaaaaaaaaaaaaaa"), "dup").is_err());
        assert_eq!(store.wallets.len(), 1);
    }

    #[test]
    fn remove_ignores_out_of_range() {
        let mut store = store();
        store.add(&btc("bc1qaaaaaaaaaaaaaaaa"), "").expect("add");
        store.remove(5).expect("out of range is not an error");
        assert_eq!(store.wallets.len(), 1);

        store.remove(0).expect("remove");
        assert!(store.wallets.is_empty());
        assert!(store.database().wallets().expect("list").is_empty());
    }

    #[test]
    fn the_last_price_is_remembered_per_chain() {
        let store = store();
        assert_eq!(store.last_price(Chain::Bitcoin), None);
        store
            .set_last_price(Chain::Bitcoin, 81_234.5)
            .expect("save");
        assert_eq!(store.last_price(Chain::Bitcoin), Some(81_234.5));
    }

    #[test]
    fn saved_history_is_only_returned_for_wallets_that_were_fetched() {
        let store = store();
        let wallet = btc("bc1qa");
        assert!(store.saved_history(&wallet).expect("query").is_none());

        // A fetched wallet with no transactions is saved, but empty.
        store
            .database()
            .replace_history_above(&wallet, 0, &[])
            .expect("save");
        let saved = store
            .saved_history(&wallet)
            .expect("query")
            .expect("synced");
        assert_eq!(saved.wallet, wallet);
        assert!(saved.txs.is_empty());
    }

    #[test]
    fn chains_in_use_lists_only_chains_with_wallets() {
        let mut store = store();
        assert!(store.chains_in_use().is_empty());
        store.add(&btc("bc1qa"), "").expect("add");
        assert_eq!(store.chains_in_use(), [Chain::Bitcoin]);
    }

    #[test]
    fn a_reopened_store_sees_earlier_changes() {
        let db = Database::in_memory().expect("in-memory db opens");
        db.add_wallet(&btc("bc1qaaaaaaaaaaaaaaaa"), "Saved")
            .expect("add");
        db.set_setting("hidden", "1").expect("set");

        let store = Store::open(db).expect("store opens");
        assert_eq!(store.wallets[0].label, "Saved");
        assert!(store.hidden);
    }
}
