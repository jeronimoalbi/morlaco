//! Saved wallets and settings, kept in the database.
//!
//! The store holds a copy in memory for the UI and writes every change
//! through to the database first, so a failed write never leaves the screen
//! showing something that was not saved.

use morlaco_core::chain::{Chain, WalletId};
use morlaco_core::db::Database;
use morlaco_core::portfolio::{History, SETTING_ETHERSCAN_KEY, SETTING_NODEREAL_KEY};
use morlaco_core::{Error, Result};

const HIDDEN_KEY: &str = "hidden";
const ACTIVE_PORTFOLIO_KEY: &str = "active_portfolio";

/// A tracked wallet with an optional user label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wallet {
    /// The chain and address.
    pub id: WalletId,
    /// Name chosen by the user; may be empty.
    pub label: String,
    /// The [`Portfolio::id`] of the portfolio the wallet is in.
    pub portfolio: i64,
}

/// A named group of wallets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Portfolio {
    /// Stable identifier.
    pub id: i64,
    /// Name chosen by the user.
    pub name: String,
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
    /// Tracked wallets in the order they were added, in every portfolio.
    pub wallets: Vec<Wallet>,
    /// The portfolios in the order they were created. Never empty.
    pub portfolios: Vec<Portfolio>,
    /// The [`Portfolio::id`] of the portfolio on screen.
    active: i64,
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
                portfolio: w.portfolio,
            })
            .collect();
        let portfolios: Vec<Portfolio> = db
            .portfolios()?
            .into_iter()
            .map(|p| Portfolio {
                id: p.id,
                name: p.name,
            })
            .collect();
        let first = portfolios
            .first()
            .map(|p| p.id)
            .ok_or(Error::NoSuchPortfolio)?;
        // The saved choice may be gone, for example after a restore.
        let active = db
            .setting(ACTIVE_PORTFOLIO_KEY)?
            .and_then(|saved| saved.parse().ok())
            .filter(|id| portfolios.iter().any(|p| p.id == *id))
            .unwrap_or(first);
        let hidden = db.setting(HIDDEN_KEY)?.as_deref() == Some("1");
        Ok(Self {
            wallets,
            portfolios,
            active,
            hidden,
            db,
        })
    }

    /// The portfolio on screen.
    pub fn active_portfolio(&self) -> &Portfolio {
        self.portfolios
            .iter()
            .find(|p| p.id == self.active)
            .unwrap_or(&self.portfolios[0])
    }

    /// The indices, in [`wallets`](Self::wallets), of the wallets of the
    /// portfolio on screen.
    pub fn visible_wallets(&self) -> Vec<usize> {
        let active = self.active_portfolio().id;
        (0..self.wallets.len())
            .filter(|&i| self.wallets[i].portfolio == active)
            .collect()
    }

    /// Puts another portfolio on screen and remembers the choice.
    pub fn set_active(&mut self, id: i64) -> Result<()> {
        if !self.portfolios.iter().any(|p| p.id == id) {
            return Err(Error::NoSuchPortfolio);
        }
        self.db.set_setting(ACTIVE_PORTFOLIO_KEY, &id.to_string())?;
        self.active = id;
        Ok(())
    }

    /// Creates a portfolio and returns its id. It is not put on screen.
    pub fn add_portfolio(&mut self, name: &str) -> Result<i64> {
        let name = clean_label(name);
        let id = self.db.add_portfolio(&name)?;
        self.portfolios.push(Portfolio {
            id,
            name: name.trim().to_owned(),
        });
        Ok(id)
    }

    /// Gives a portfolio a new name.
    pub fn rename_portfolio(&mut self, id: i64, name: &str) -> Result<()> {
        let name = clean_label(name);
        self.db.rename_portfolio(id, &name)?;
        if let Some(portfolio) = self.portfolios.iter_mut().find(|p| p.id == id) {
            portfolio.name = name;
        }
        Ok(())
    }

    /// Removes a portfolio and its wallets, and returns the wallets that went.
    /// If it was on screen, the first remaining portfolio is shown instead.
    pub fn remove_portfolio(&mut self, id: i64) -> Result<Vec<WalletId>> {
        self.db.remove_portfolio(id)?;
        self.portfolios.retain(|p| p.id != id);
        let mut removed = Vec::new();
        self.wallets.retain(|w| {
            let gone = w.portfolio == id;
            if gone {
                removed.push(w.id.clone());
            }
            !gone
        });
        if self.active == id {
            let first = self.portfolios[0].id;
            self.set_active(first)?;
        }
        Ok(removed)
    }

    /// Moves the wallet at `index`, if it exists, to another portfolio.
    pub fn move_wallet(&mut self, index: usize, portfolio: i64) -> Result<()> {
        if let Some(wallet) = self.wallets.get_mut(index) {
            self.db.move_wallet(&wallet.id, portfolio)?;
            wallet.portfolio = portfolio;
        }
        Ok(())
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

    /// Adds a wallet to the portfolio on screen and returns its index.
    /// Control characters in the label are replaced with spaces.
    pub fn add(&mut self, id: &WalletId, label: &str) -> Result<usize> {
        let label = clean_label(label);
        let portfolio = self.active;
        self.db.add_wallet_to(id, &label, portfolio)?;
        self.wallets.push(Wallet {
            id: id.clone(),
            label,
            portfolio,
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

    /// The Etherscan API key the user entered, if any.
    pub fn etherscan_key(&self) -> Option<String> {
        self.db
            .setting(SETTING_ETHERSCAN_KEY)
            .ok()?
            .filter(|key| !key.is_empty())
    }

    /// Saves the Etherscan API key. An empty key goes back to the keyless
    /// service.
    pub fn set_etherscan_key(&self, key: &str) -> Result<()> {
        self.db.set_setting(SETTING_ETHERSCAN_KEY, key)
    }

    /// The NodeReal API key the user entered, if any. BNB Smart Chain needs
    /// one.
    pub fn nodereal_key(&self) -> Option<String> {
        self.db
            .setting(SETTING_NODEREAL_KEY)
            .ok()?
            .filter(|key| !key.is_empty())
    }

    /// Saves the NodeReal API key. An empty key removes it.
    pub fn set_nodereal_key(&self, key: &str) -> Result<()> {
        self.db.set_setting(SETTING_NODEREAL_KEY, key)
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

    #[test]
    fn the_etherscan_key_is_kept_and_can_be_removed() {
        let store = Store::open(Database::in_memory().expect("db")).expect("open");
        assert_eq!(store.etherscan_key(), None);
        store.set_etherscan_key("ABC123").expect("save");
        assert_eq!(store.etherscan_key().as_deref(), Some("ABC123"));
        store.set_etherscan_key("").expect("clear");
        assert_eq!(store.etherscan_key(), None);
    }

    #[test]
    fn a_new_store_has_one_portfolio_on_screen() {
        let store = store();
        assert_eq!(store.portfolios.len(), 1);
        assert_eq!(store.active_portfolio().name, "Main");
    }

    #[test]
    fn wallets_go_to_the_portfolio_on_screen_and_only_those_are_visible() {
        let mut store = store();
        store
            .add(&btc("bc1qaaaaaaaaaaaaaaaa"), "In main")
            .expect("add");
        let business = store.add_portfolio("Business").expect("create");
        store.set_active(business).expect("switch");
        store
            .add(&btc("bc1qbbbbbbbbbbbbbbbb"), "In business")
            .expect("add");

        assert_eq!(store.visible_wallets(), [1]);
        assert_eq!(store.wallets[1].portfolio, business);
        store
            .set_active(store.portfolios[0].id)
            .expect("switch back");
        assert_eq!(store.visible_wallets(), [0]);
    }

    #[test]
    fn the_portfolio_on_screen_is_remembered() {
        let mut store = store();
        let business = store.add_portfolio("Business").expect("create");
        store.set_active(business).expect("switch");

        // Open the same database again: the choice comes back.
        let reopened = Store::open(store.db).expect("reopens");
        assert_eq!(reopened.active_portfolio().name, "Business");
    }

    #[test]
    fn a_remembered_portfolio_that_no_longer_exists_falls_back_to_the_first() {
        let store = store();
        store
            .db
            .set_setting(ACTIVE_PORTFOLIO_KEY, "999")
            .expect("save");
        let reopened = Store::open(store.db).expect("reopens");
        assert_eq!(reopened.active_portfolio().name, "Main");
    }

    #[test]
    fn portfolios_can_be_renamed_and_names_are_checked() {
        let mut store = store();
        let id = store.add_portfolio("  Business\t").expect("create");
        assert_eq!(store.portfolios[1].name, "Business");
        store.rename_portfolio(id, " Work ").expect("rename");
        assert_eq!(store.portfolios[1].name, "Work");
        assert!(matches!(
            store.add_portfolio("work"),
            Err(Error::PortfolioNameTaken)
        ));
        assert!(matches!(
            store.rename_portfolio(id, "  "),
            Err(Error::EmptyPortfolioName)
        ));
        assert_eq!(store.portfolios.len(), 2);
    }

    #[test]
    fn removing_a_portfolio_removes_its_wallets_and_moves_the_screen_off_it() {
        let mut store = store();
        store
            .add(&btc("bc1qaaaaaaaaaaaaaaaa"), "keep")
            .expect("add");
        let business = store.add_portfolio("Business").expect("create");
        store.set_active(business).expect("switch");
        store
            .add(&btc("bc1qbbbbbbbbbbbbbbbb"), "drop")
            .expect("add");

        let removed = store.remove_portfolio(business).expect("remove");
        assert_eq!(removed, [btc("bc1qbbbbbbbbbbbbbbbb")]);
        assert_eq!(store.wallets.len(), 1);
        assert_eq!(store.portfolios.len(), 1);
        assert_eq!(store.active_portfolio().name, "Main");

        let only = store.portfolios[0].id;
        assert!(matches!(
            store.remove_portfolio(only),
            Err(Error::LastPortfolio)
        ));
    }

    #[test]
    fn a_wallet_can_move_to_another_portfolio() {
        let mut store = store();
        store.add(&btc("bc1qaaaaaaaaaaaaaaaa"), "").expect("add");
        let business = store.add_portfolio("Business").expect("create");
        store.move_wallet(0, business).expect("move");
        assert_eq!(store.wallets[0].portfolio, business);
        assert!(store.visible_wallets().is_empty());
        let reopened = Store::open(store.db).expect("reopens");
        assert_eq!(reopened.wallets[0].portfolio, business);
    }

    #[test]
    fn the_nodereal_key_is_kept_apart_from_the_etherscan_key() {
        let store = store();
        assert_eq!(store.nodereal_key(), None);
        store.set_nodereal_key("NODE").expect("save");
        store.set_etherscan_key("SCAN").expect("save");
        assert_eq!(store.nodereal_key().as_deref(), Some("NODE"));
        assert_eq!(store.etherscan_key().as_deref(), Some("SCAN"));
        store.set_nodereal_key("").expect("clear");
        assert_eq!(store.nodereal_key(), None);
        assert_eq!(store.etherscan_key().as_deref(), Some("SCAN"));
    }
}
