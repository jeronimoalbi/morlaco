//! The local SQLite database: tracked wallets, saved transactions, settings
//! and cached prices.
//!
//! The file is encrypted with SQLCipher. Its layout is versioned with
//! `PRAGMA user_version`; a file from an incompatible layout is reported as
//! such instead of being touched.

use std::fs::DirBuilder;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::chain::{Activity, Chain, WalletId};
use crate::portfolio::PricedTx;
use crate::{Error, Result};

/// How long a connection waits for another one to release a lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The layout this version reads and writes.
const SCHEMA_VERSION: i64 = 2;

/// Tables of the current layout.
///
/// Amounts are stored as text because SQLite integers are 64-bit, which is
/// too small for the smallest unit of some chains (wei).
const SCHEMA: &str = "
    CREATE TABLE prices (
        chain TEXT NOT NULL,
        day INTEGER NOT NULL,
        usd REAL NOT NULL,
        PRIMARY KEY (chain, day)
    );
    CREATE TABLE wallets (
        position INTEGER PRIMARY KEY AUTOINCREMENT,
        chain TEXT NOT NULL,
        address TEXT NOT NULL,
        label TEXT NOT NULL,
        UNIQUE (chain, address)
    );
    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE txs (
        chain TEXT NOT NULL,
        address TEXT NOT NULL,
        txid TEXT NOT NULL,
        height INTEGER NOT NULL,
        idx INTEGER,
        time INTEGER NOT NULL,
        amount TEXT NOT NULL,
        fee TEXT NOT NULL,
        parents TEXT NOT NULL,
        price_usd REAL NOT NULL,
        PRIMARY KEY (chain, address, txid)
    );
    CREATE INDEX txs_by_height ON txs (chain, address, height);
    CREATE TABLE synced (
        chain TEXT NOT NULL,
        address TEXT NOT NULL,
        PRIMARY KEY (chain, address)
    );";

/// SQLite stores signed 64-bit integers. Heights never get near that limit,
/// so saturating is only a formality.
fn to_sql_int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn conversion_error(
    index: usize,
    err: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(err))
}

/// Reads an unsigned column, failing on a negative value.
fn get_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(err),
        )
    })
}

/// Reads an amount stored as decimal text.
fn get_i128(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<i128> {
    let text: String = row.get(index)?;
    text.parse().map_err(|err| conversion_error(index, err))
}

fn get_u128(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u128> {
    let text: String = row.get(index)?;
    text.parse().map_err(|err| conversion_error(index, err))
}

/// Runs `PRAGMA <name> = '<value>'` with the value quoted as an SQL string.
///
/// rusqlite's `pragma_update` is not used because it fails on `key` for an
/// existing encrypted file even when the key is right.
fn set_pragma_text(conn: &Connection, name: &str, value: &str) -> Result<()> {
    let quoted = value.replace('\'', "''");
    conn.execute_batch(&format!("PRAGMA {name} = '{quoted}';"))?;
    Ok(())
}

/// Reads from the file, which fails when the key is wrong or the file is
/// not a database.
fn check_readable(conn: &Connection) -> Result<()> {
    match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::NotADatabase =>
        {
            Err(Error::WrongKey)
        }
        Err(err) => Err(err.into()),
    }
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let found = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |_| Ok(()),
        )
        .optional()?;
    Ok(found.is_some())
}

/// A wallet as stored: where it is and the user's label for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletRecord {
    /// The chain and address.
    pub id: WalletId,
    /// Name chosen by the user; may be empty.
    pub label: String,
}

/// Handle to the database file.
///
/// A connection is not shared between threads. A thread that needs the
/// database opens its own; SQLite serializes their writes.
pub struct Database {
    conn: Connection,
}

impl Database {
    /// Opens (creating if needed) the encrypted database at `path`.
    ///
    /// `key` is a passphrase; SQLCipher stretches it into the real key.
    /// Fails with [`Error::WrongKey`] when it does not match the file,
    /// [`Error::OldFormat`] for a file from version 1 and
    /// [`Error::NewerFormat`] for one from a newer version. Such files are
    /// left untouched.
    ///
    /// New directories are private to the user (0700) and the file is made
    /// readable by the user only (0600).
    pub fn open(path: &Path, key: &str) -> Result<Self> {
        if let Some(dir) = path.parent() {
            DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        }
        let conn = Connection::open(path)?;
        set_pragma_text(&conn, "key", key)?;
        check_readable(&conn)?;
        let db = Self::init(conn)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(db)
    }

    /// Where [`set_aside_old_format`](Self::set_aside_old_format) moves the
    /// file of `path`.
    pub fn set_aside_path(path: &Path) -> PathBuf {
        path.with_extension("db.v1-old")
    }

    /// Renames a database of an old layout out of the way, so a fresh one
    /// can be created at `path`. Nothing is read or deleted; the old data
    /// stays recoverable in the returned file.
    pub fn set_aside_old_format(path: &Path) -> Result<PathBuf> {
        let aside = Self::set_aside_path(path);
        std::fs::rename(path, &aside)?;
        Ok(aside)
    }

    /// Whether `path` is a readable database that is not encrypted, as
    /// written by an earlier version. `false` if the file is missing or
    /// encrypted. Never modifies the file.
    pub fn is_plain_file(path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        match check_readable(&Connection::open(path)?) {
            Ok(()) => Ok(true),
            Err(Error::WrongKey) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Encrypts, with `key`, a database that an earlier version wrote
    /// without encryption.
    ///
    /// Returns `false` when there is nothing to do: the file does not exist
    /// or is already encrypted.
    pub fn encrypt_plain_file(path: &Path, key: &str) -> Result<bool> {
        if !Self::is_plain_file(path)? {
            return Ok(false);
        }
        let conn = Connection::open(path)?;

        let tmp = path.with_extension("db-encrypting");
        let _ = std::fs::remove_file(&tmp);
        conn.execute(
            "ATTACH DATABASE ?1 AS encrypted KEY ?2",
            params![tmp.to_string_lossy(), key],
        )?;
        conn.query_row("SELECT sqlcipher_export('encrypted')", [], |_| Ok(()))?;
        // The export copies the data but not the layout version.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        conn.execute_batch(&format!("PRAGMA encrypted.user_version = {version};"))?;
        conn.execute("DETACH DATABASE encrypted", [])?;
        drop(conn);

        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, path)?;
        Ok(true)
    }

    /// Re-encrypts the whole database with a new key.
    ///
    /// No other connection may be using the file meanwhile.
    pub fn rekey(&self, new_key: &str) -> Result<()> {
        set_pragma_text(&self.conn, "rekey", new_key)?;
        Ok(())
    }

    /// Creates a throwaway database that lives only in memory.
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    /// Creates the tables of a new database or checks that an existing one
    /// has the current layout.
    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

        if version == SCHEMA_VERSION {
            // Current layout; nothing to do.
        } else if version > SCHEMA_VERSION {
            return Err(Error::NewerFormat(version));
        } else if version == 0 && !table_exists(&conn, "wallets")? {
            // Brand new file.
            conn.execute_batch(SCHEMA)?;
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        } else {
            // Version 1 never set `user_version`, so it reads as 0 with tables.
            return Err(Error::OldFormat);
        }
        Ok(Self { conn })
    }

    // --- Prices ----------------------------------------------------------

    /// Returns the cached USD price of one coin of `chain` for `day` (days
    /// since the Unix epoch). Historical prices never change, so a cached
    /// day is never refreshed.
    pub fn price(&self, chain: Chain, day: i64) -> Result<Option<f64>> {
        let usd = self
            .conn
            .query_row(
                "SELECT usd FROM prices WHERE chain = ?1 AND day = ?2",
                params![chain.key(), day],
                |r| r.get(0),
            )
            .optional()?;
        Ok(usd)
    }

    /// Stores the price for `day`, replacing any previous value.
    pub fn put_price(&self, chain: Chain, day: i64, usd: f64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO prices (chain, day, usd) VALUES (?1, ?2, ?3)",
            params![chain.key(), day, usd],
        )?;
        Ok(())
    }

    // --- Wallets ---------------------------------------------------------

    /// All wallets in the order they were added.
    pub fn wallets(&self) -> Result<Vec<WalletRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT chain, address, label FROM wallets ORDER BY position")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|(key, address, label)| {
                let chain = Chain::from_key(&key).ok_or(Error::UnknownChain(key))?;
                Ok(WalletRecord {
                    id: WalletId::new(chain, address),
                    label,
                })
            })
            .collect()
    }

    /// Adds a wallet at the end of the list. Fails if it is already tracked.
    pub fn add_wallet(&self, wallet: &WalletId, label: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO wallets (chain, address, label) VALUES (?1, ?2, ?3)",
            params![wallet.chain.key(), wallet.address, label],
        )?;
        Ok(())
    }

    /// Changes the label of a wallet.
    pub fn rename_wallet(&self, wallet: &WalletId, label: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE wallets SET label = ?3 WHERE chain = ?1 AND address = ?2",
            params![wallet.chain.key(), wallet.address, label],
        )?;
        Ok(())
    }

    /// Stops tracking a wallet and forgets its saved transactions.
    pub fn remove_wallet(&self, wallet: &WalletId) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let args = params![wallet.chain.key(), wallet.address];
        tx.execute(
            "DELETE FROM wallets WHERE chain = ?1 AND address = ?2",
            args,
        )?;
        tx.execute("DELETE FROM txs WHERE chain = ?1 AND address = ?2", args)?;
        tx.execute("DELETE FROM synced WHERE chain = ?1 AND address = ?2", args)?;
        tx.commit()?;
        Ok(())
    }

    // --- Transaction history --------------------------------------------

    /// Whether the history of `wallet` was fetched and saved at least once.
    /// A wallet without transactions is synced too.
    pub fn is_synced(&self, wallet: &WalletId) -> Result<bool> {
        let found = self
            .conn
            .query_row(
                "SELECT 1 FROM synced WHERE chain = ?1 AND address = ?2",
                params![wallet.chain.key(), wallet.address],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// The saved transactions of `wallet`, oldest first.
    pub fn history(&self, wallet: &WalletId) -> Result<Vec<PricedTx>> {
        let mut stmt = self.conn.prepare(
            "SELECT txid, height, idx, time, amount, fee, parents, price_usd
             FROM txs WHERE chain = ?1 AND address = ?2
             ORDER BY height, idx, txid",
        )?;
        let rows = stmt.query_map(params![wallet.chain.key(), wallet.address], |r| {
            let parents: String = r.get(6)?;
            let index: Option<i64> = r.get(2)?;
            Ok(PricedTx {
                activity: Activity {
                    txid: r.get(0)?,
                    height: get_u64(r, 1)?,
                    index: index
                        .map(|i| u32::try_from(i).map_err(|e| conversion_error(2, e)))
                        .transpose()?,
                    time: r.get(3)?,
                    amount: get_i128(r, 4)?,
                    fee: get_u128(r, 5)?,
                    parents: parents
                        .split(',')
                        .filter(|p| !p.is_empty())
                        .map(str::to_owned)
                        .collect(),
                },
                price_usd: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Replaces everything of `wallet` above `height` with `fresh` and marks
    /// the wallet as synced, all or nothing. Transactions at or below
    /// `height` are kept.
    pub fn replace_history_above(
        &self,
        wallet: &WalletId,
        height: u64,
        fresh: &[PricedTx],
    ) -> Result<()> {
        let chain = wallet.chain.key();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM txs WHERE chain = ?1 AND address = ?2 AND height > ?3",
            params![chain, wallet.address, to_sql_int(height)],
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO txs
                 (chain, address, txid, height, idx, time, amount, fee, parents, price_usd)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for t in fresh {
                let a = &t.activity;
                insert.execute(params![
                    chain,
                    wallet.address,
                    a.txid,
                    to_sql_int(a.height),
                    a.index.map(i64::from),
                    a.time,
                    a.amount.to_string(),
                    a.fee.to_string(),
                    a.parents.join(","),
                    t.price_usd,
                ])?;
            }
        }
        tx.execute(
            "INSERT OR IGNORE INTO synced (chain, address) VALUES (?1, ?2)",
            params![chain, wallet.address],
        )?;
        tx.commit()?;
        Ok(())
    }

    // --- Settings --------------------------------------------------------

    /// Reads a setting.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        let value = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(value)
    }

    /// Writes a setting, replacing any previous value.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::in_memory().expect("in-memory db opens")
    }

    fn btc(address: &str) -> WalletId {
        WalletId::new(Chain::Bitcoin, address)
    }

    #[test]
    fn price_round_trip_per_chain() {
        let db = db();
        assert_eq!(db.price(Chain::Bitcoin, 19_000).expect("query"), None);
        db.put_price(Chain::Bitcoin, 19_000, 35_564.0)
            .expect("insert");
        assert_eq!(
            db.price(Chain::Bitcoin, 19_000).expect("query"),
            Some(35_564.0)
        );
        // The same day on another chain is a different price.
        assert_eq!(db.price(Chain::Fake, 19_000).expect("query"), None);
    }

    #[test]
    fn wallets_keep_insertion_order() {
        let db = db();
        db.add_wallet(&btc("bc1qb"), "Second alphabetically")
            .expect("add");
        db.add_wallet(&btc("bc1qa"), "").expect("add");

        let addresses: Vec<String> = db
            .wallets()
            .expect("list")
            .into_iter()
            .map(|w| w.id.address)
            .collect();
        assert_eq!(addresses, ["bc1qb", "bc1qa"]);
    }

    #[test]
    fn duplicate_wallets_are_rejected_but_the_same_address_on_another_chain_is_not() {
        let db = db();
        db.add_wallet(&btc("x1"), "").expect("add");
        assert!(db.add_wallet(&btc("x1"), "again").is_err());

        db.add_wallet(&WalletId::new(Chain::Fake, "x1"), "")
            .expect("other chain");
        let wallets = db.wallets().expect("list");
        assert_eq!(wallets.len(), 2);
        assert_eq!(wallets[1].id.chain, Chain::Fake);
    }

    #[test]
    fn rename_and_remove() {
        let db = db();
        db.add_wallet(&btc("bc1qa"), "Old").expect("add");
        db.add_wallet(&btc("bc1qb"), "Keep").expect("add");

        db.rename_wallet(&btc("bc1qa"), "New").expect("rename");
        assert_eq!(db.wallets().expect("list")[0].label, "New");

        db.remove_wallet(&btc("bc1qa")).expect("remove");
        let left = db.wallets().expect("list");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id.address, "bc1qb");

        // Unknown wallets are not an error.
        db.remove_wallet(&btc("bc1qzzz")).expect("remove");
    }

    #[test]
    fn a_wallet_of_an_unknown_chain_is_reported() {
        let db = db();
        db.conn
            .execute(
                "INSERT INTO wallets (chain, address, label) VALUES ('dogecoin', 'D1', '')",
                [],
            )
            .expect("insert");
        assert!(matches!(db.wallets(), Err(Error::UnknownChain(k)) if k == "dogecoin"));
    }

    #[test]
    fn settings_round_trip_and_overwrite() {
        let db = db();
        assert_eq!(db.setting("hidden").expect("read"), None);
        db.set_setting("hidden", "1").expect("write");
        db.set_setting("hidden", "0").expect("write");
        assert_eq!(db.setting("hidden").expect("read"), Some("0".to_owned()));
    }

    fn priced(txid: &str, height: u64) -> PricedTx {
        PricedTx {
            activity: Activity {
                txid: txid.to_owned(),
                height,
                index: None,
                time: height as i64 * 600,
                amount: 1_000,
                fee: 0,
                parents: vec!["p1".to_owned(), "p2".to_owned()],
            },
            price_usd: 100.0 * height as f64,
        }
    }

    #[test]
    fn history_round_trips_in_height_order() {
        let db = db();
        let w = btc("bc1qa");
        assert!(!db.is_synced(&w).expect("query"));

        db.replace_history_above(&w, 0, &[priced("b", 9), priced("a", 3)])
            .expect("save");
        assert!(db.is_synced(&w).expect("query"));

        let saved = db.history(&w).expect("load");
        let heights: Vec<u64> = saved.iter().map(|t| t.activity.height).collect();
        assert_eq!(heights, [3, 9]);
        assert_eq!(saved[0].activity.parents, ["p1", "p2"]);
        assert_eq!(saved[1].price_usd, 900.0);
    }

    #[test]
    fn amounts_beyond_64_bits_and_negative_amounts_survive() {
        let db = db();
        let w = WalletId::new(Chain::Fake, "0xabc");
        let mut tx = priced("big", 5);
        // 50 coins of an 18-decimals chain is 5e19 units, above u64::MAX.
        tx.activity.amount = -50_000_000_000_000_000_000;
        tx.activity.fee = 21_000_000_000_000_000;
        tx.activity.index = Some(7);
        db.replace_history_above(&w, 0, &[tx.clone()])
            .expect("save");

        let saved = db.history(&w).expect("load");
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].activity, tx.activity);
    }

    #[test]
    fn transactions_in_one_block_come_back_in_block_order() {
        let db = db();
        let w = WalletId::new(Chain::Fake, "0xabc");
        let mut second = priced("a-second-in-block", 5);
        second.activity.index = Some(9);
        let mut first = priced("z-first-in-block", 5);
        first.activity.index = Some(2);
        db.replace_history_above(&w, 0, &[second, first])
            .expect("save");

        let ids: Vec<String> = db
            .history(&w)
            .expect("load")
            .into_iter()
            .map(|t| t.activity.txid)
            .collect();
        assert_eq!(ids, ["z-first-in-block", "a-second-in-block"]);
    }

    #[test]
    fn replacing_above_a_height_keeps_older_transactions() {
        let db = db();
        let w = btc("bc1qa");
        db.replace_history_above(&w, 0, &[priced("a", 1), priced("b", 5), priced("c", 8)])
            .expect("save");

        // Blocks above 5 are redone: "c" is replaced by "d" and "e".
        db.replace_history_above(&w, 5, &[priced("d", 7), priced("e", 9)])
            .expect("save");

        let ids: Vec<String> = db
            .history(&w)
            .expect("load")
            .into_iter()
            .map(|t| t.activity.txid)
            .collect();
        assert_eq!(ids, ["a", "b", "d", "e"]);
    }

    #[test]
    fn an_empty_history_still_counts_as_synced() {
        let db = db();
        let w = btc("bc1qa");
        db.replace_history_above(&w, 0, &[]).expect("save");
        assert!(db.is_synced(&w).expect("query"));
        assert!(db.history(&w).expect("load").is_empty());
    }

    #[test]
    fn histories_are_kept_apart_and_removed_with_their_wallet() {
        let db = db();
        let (a, b) = (btc("bc1qa"), btc("bc1qb"));
        let same_text_elsewhere = WalletId::new(Chain::Fake, "bc1qa");
        for w in [&a, &b, &same_text_elsewhere] {
            db.add_wallet(w, "").expect("add");
            db.replace_history_above(w, 0, &[priced("a", 1)])
                .expect("save");
        }

        db.remove_wallet(&a).expect("remove");
        assert!(db.history(&a).expect("load").is_empty());
        assert!(!db.is_synced(&a).expect("query"));
        assert_eq!(db.history(&b).expect("load").len(), 1);
        // Same address text, different chain: untouched.
        assert_eq!(db.history(&same_text_elsewhere).expect("load").len(), 1);
    }

    #[test]
    fn files_are_private_to_the_user() {
        let dir = TempDir::new("perms");
        let path = dir.path().join("nested").join("morlaco.db");
        let _db = Database::open(&path, "key").expect("opens");

        let mode = |p: &Path| std::fs::metadata(p).expect("exists").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().expect("has parent")), 0o700);
    }

    #[test]
    fn the_file_is_encrypted_and_needs_the_right_key() {
        let dir = TempDir::new("encrypted");
        let path = dir.path().join("morlaco.db");
        Database::open(&path, "right")
            .expect("creates")
            .add_wallet(&btc("bc1qa"), "Saved")
            .expect("add");

        let header = std::fs::read(&path).expect("read file");
        assert!(
            !header.starts_with(b"SQLite format 3"),
            "file is plain SQLite"
        );
        assert!(
            !header.windows(5).any(|w| w == b"bc1qa"),
            "address is visible in the file"
        );

        assert!(matches!(
            Database::open(&path, "wrong"),
            Err(Error::WrongKey)
        ));
        let db = Database::open(&path, "right").expect("reopens");
        assert_eq!(db.wallets().expect("list")[0].label, "Saved");
    }

    #[test]
    fn keys_with_quotes_and_newlines_work() {
        let dir = TempDir::new("quotes");
        let path = dir.path().join("morlaco.db");
        let key = "it's a \"key\"\nwith; odd -- characters";
        Database::open(&path, key)
            .expect("creates")
            .add_wallet(&btc("bc1qa"), "")
            .expect("add");

        assert!(Database::open(&path, key).is_ok());
        assert!(matches!(
            Database::open(&path, "it''s"),
            Err(Error::WrongKey)
        ));
    }

    #[test]
    fn rekey_switches_to_the_new_key() {
        let dir = TempDir::new("rekey");
        let path = dir.path().join("morlaco.db");
        let db = Database::open(&path, "old").expect("creates");
        db.add_wallet(&btc("bc1qa"), "").expect("add");
        db.rekey("new").expect("rekey");
        drop(db);

        assert!(matches!(Database::open(&path, "old"), Err(Error::WrongKey)));
        let db = Database::open(&path, "new").expect("opens with the new key");
        assert_eq!(db.wallets().expect("list").len(), 1);
    }

    #[test]
    fn a_plain_file_from_an_earlier_version_gets_encrypted() {
        let dir = TempDir::new("plain");
        let path = dir.path().join("morlaco.db");
        assert!(!Database::encrypt_plain_file(&path, "key").expect("no file"));

        // A plain file in the current layout.
        let plain = Connection::open(&path).expect("plain db");
        plain.execute_batch(SCHEMA).expect("schema");
        plain
            .execute_batch(&format!(
                "INSERT INTO wallets (chain, address, label) VALUES ('bitcoin', 'bc1qa', 'Old');
                 PRAGMA user_version = {SCHEMA_VERSION};"
            ))
            .expect("fill");
        drop(plain);

        assert!(Database::encrypt_plain_file(&path, "key").expect("converts"));
        assert!(
            !std::fs::read(&path)
                .expect("read")
                .starts_with(b"SQLite format 3")
        );
        let db = Database::open(&path, "key").expect("opens");
        assert_eq!(db.wallets().expect("list")[0].label, "Old");

        // Already encrypted: left alone.
        assert!(!Database::encrypt_plain_file(&path, "key").expect("no-op"));
    }

    #[test]
    fn a_new_database_records_the_current_layout_version() {
        let db = db();
        let version: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("read");
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// The layout version 1 used: no chain column, amounts in satoshis.
    const V1_SCHEMA: &str = "
        CREATE TABLE prices (day INTEGER PRIMARY KEY, usd REAL NOT NULL);
        CREATE TABLE wallets (position INTEGER PRIMARY KEY AUTOINCREMENT,
            address TEXT NOT NULL UNIQUE, label TEXT NOT NULL);
        CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE txs (address TEXT NOT NULL, txid TEXT NOT NULL, height INTEGER NOT NULL,
            time INTEGER NOT NULL, net_sats INTEGER NOT NULL, fee_sats INTEGER NOT NULL,
            parents TEXT NOT NULL, price_usd REAL NOT NULL, PRIMARY KEY (address, txid));
        CREATE TABLE synced (address TEXT PRIMARY KEY);
        INSERT INTO wallets (address, label) VALUES ('bc1qold', 'Kept for later');";

    /// Writes an encrypted file in the version 1 layout.
    fn write_v1_file(path: &Path, key: &str) {
        let conn = Connection::open(path).expect("create");
        set_pragma_text(&conn, "key", key).expect("key");
        conn.execute_batch(V1_SCHEMA).expect("v1 schema");
    }

    #[test]
    fn a_version_1_file_is_reported_and_left_untouched() {
        let dir = TempDir::new("v1");
        let path = dir.path().join("morlaco.db");
        write_v1_file(&path, "key");
        let before = std::fs::read(&path).expect("read");

        assert!(matches!(
            Database::open(&path, "key"),
            Err(Error::OldFormat)
        ));
        assert_eq!(std::fs::read(&path).expect("read"), before);
    }

    #[test]
    fn setting_a_version_1_file_aside_lets_a_fresh_database_replace_it() {
        let dir = TempDir::new("aside");
        let path = dir.path().join("morlaco.db");
        write_v1_file(&path, "key");

        let aside = Database::set_aside_old_format(&path).expect("renames");
        assert_eq!(aside, dir.path().join("morlaco.db.v1-old"));
        assert_eq!(aside, Database::set_aside_path(&path));
        assert!(!path.exists());

        let fresh = Database::open(&path, "key").expect("fresh database");
        assert!(fresh.wallets().expect("list").is_empty());

        // The old data is still there for anyone who wants it.
        let conn = Connection::open(&aside).expect("old file");
        set_pragma_text(&conn, "key", "key").expect("key");
        let label: String = conn
            .query_row("SELECT label FROM wallets", [], |r| r.get(0))
            .expect("old wallet");
        assert_eq!(label, "Kept for later");
    }

    #[test]
    fn a_file_from_a_newer_version_is_rejected() {
        let dir = TempDir::new("newer");
        let path = dir.path().join("morlaco.db");
        let db = Database::open(&path, "key").expect("creates");
        db.conn
            .execute_batch("PRAGMA user_version = 99;")
            .expect("bump");
        drop(db);

        assert!(matches!(
            Database::open(&path, "key"),
            Err(Error::NewerFormat(99))
        ));
    }

    /// A directory under the system temp dir that is removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("morlaco-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
