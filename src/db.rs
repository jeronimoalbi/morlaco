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
const SCHEMA_VERSION: i64 = 4;

/// What the made-up address of a manual wallet starts with.
pub const MANUAL_PREFIX: &str = "manual:";

/// Whether `id` was made up for a manual wallet or transaction and is not a
/// real address or transaction id.
pub fn is_made_up(id: &str) -> bool {
    id.starts_with(MANUAL_PREFIX)
}

/// The name of the portfolio every database starts with.
pub const DEFAULT_PORTFOLIO_NAME: &str = "Main";

/// Layout version 2 had no portfolios. Opening such a file adds them.
const VERSION_WITHOUT_PORTFOLIOS: i64 = 2;

/// Layout version 3 had no wallet modes. Opening such a file adds them.
const VERSION_WITHOUT_MODES: i64 = 3;

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
    CREATE TABLE portfolios (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL UNIQUE COLLATE NOCASE
    );
    CREATE TABLE wallets (
        position INTEGER PRIMARY KEY AUTOINCREMENT,
        chain TEXT NOT NULL,
        address TEXT NOT NULL,
        label TEXT NOT NULL,
        portfolio_id INTEGER NOT NULL DEFAULT 1,
        mode TEXT NOT NULL DEFAULT 'synced',
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

/// What turns a version 2 file into version 3: the portfolios table, with
/// one portfolio that takes every existing wallet (the column's default points
/// at it).
const ADD_PORTFOLIOS: &str = "
    CREATE TABLE portfolios (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT NOT NULL UNIQUE COLLATE NOCASE
    );
    ALTER TABLE wallets ADD COLUMN portfolio_id INTEGER NOT NULL DEFAULT 1;";

/// What turns a version 3 file into version 4: every existing wallet stays
/// synced.
const ADD_MODES: &str = "
    ALTER TABLE wallets ADD COLUMN mode TEXT NOT NULL DEFAULT 'synced';";

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

/// Where a copy of a file of layout `version` is kept before it is upgraded.
fn upgrade_copy_path(path: &Path, version: i64) -> PathBuf {
    path.with_extension(format!("db.v{version}-backup"))
}

/// Copies the file next to itself before its layout is upgraded, so the
/// upgrade can be undone by hand. An earlier copy is never overwritten.
fn keep_copy_before_upgrade(path: &Path, version: i64) -> Result<()> {
    let copy = upgrade_copy_path(path, version);
    if !copy.exists() {
        std::fs::copy(path, &copy)?;
        std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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

/// Where the transactions of a wallet come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletMode {
    /// Downloaded from the blockchain; the user cannot change them.
    Synced,
    /// Entered by the user.
    Manual,
}

impl WalletMode {
    fn from_key(key: &str) -> Option<Self> {
        match key {
            "synced" => Some(Self::Synced),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// A wallet as stored: where it is and the user's label for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletRecord {
    /// The chain and address.
    pub id: WalletId,
    /// Name chosen by the user; may be empty.
    pub label: String,
    /// The [`PortfolioRecord::id`] of the portfolio the wallet is in.
    pub portfolio: i64,
    /// Whether the transactions are downloaded or entered by the user.
    pub mode: WalletMode,
}

/// A named group of wallets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortfolioRecord {
    /// Stable identifier. Ids grow, so they also give the creation order.
    pub id: i64,
    /// Name chosen by the user.
    pub name: String,
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
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == VERSION_WITHOUT_PORTFOLIOS || version == VERSION_WITHOUT_MODES {
            keep_copy_before_upgrade(path, version)?;
        }
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
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(SCHEMA)?;
            tx.execute(
                "INSERT INTO portfolios (name) VALUES (?1)",
                params![DEFAULT_PORTFOLIO_NAME],
            )?;
            tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
            tx.commit()?;
        } else if version == VERSION_WITHOUT_PORTFOLIOS || version == VERSION_WITHOUT_MODES {
            // All or nothing: a failure leaves the file as it was.
            let tx = conn.unchecked_transaction()?;
            if version == VERSION_WITHOUT_PORTFOLIOS {
                tx.execute_batch(ADD_PORTFOLIOS)?;
                tx.execute(
                    "INSERT INTO portfolios (name) VALUES (?1)",
                    params![DEFAULT_PORTFOLIO_NAME],
                )?;
            }
            tx.execute_batch(ADD_MODES)?;
            tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
            tx.commit()?;
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
        let mut stmt = self.conn.prepare(
            "SELECT chain, address, label, portfolio_id, mode FROM wallets ORDER BY position",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|(key, address, label, portfolio, mode)| {
                let chain = Chain::from_key(&key).ok_or(Error::UnknownChain(key))?;
                let mode = WalletMode::from_key(&mode).ok_or(Error::UnknownWalletMode(mode))?;
                Ok(WalletRecord {
                    id: WalletId::new(chain, address),
                    label,
                    portfolio,
                    mode,
                })
            })
            .collect()
    }

    /// Adds a wallet at the end of the list, in the first portfolio. Fails if
    /// it is already tracked.
    pub fn add_wallet(&self, wallet: &WalletId, label: &str) -> Result<()> {
        let first = self.first_portfolio()?;
        self.add_wallet_to(wallet, label, first)
    }

    /// Adds a wallet at the end of the list, in `portfolio`. Fails if it is
    /// already tracked.
    pub fn add_wallet_to(&self, wallet: &WalletId, label: &str, portfolio: i64) -> Result<()> {
        self.require_portfolio(portfolio)?;
        self.conn.execute(
            "INSERT INTO wallets (chain, address, label, portfolio_id) VALUES (?1, ?2, ?3, ?4)",
            params![wallet.chain.key(), wallet.address, label, portfolio],
        )?;
        Ok(())
    }

    /// Adds a wallet whose transactions the user enters, in `portfolio`.
    /// It has no address on the chain, so it gets a made-up one, which is
    /// returned.
    pub fn add_manual_wallet(&self, chain: Chain, label: &str, portfolio: i64) -> Result<WalletId> {
        self.require_portfolio(portfolio)?;
        let tx = self.conn.unchecked_transaction()?;
        let next: i64 = tx.query_row(
            "SELECT coalesce(max(position), 0) + 1 FROM wallets",
            [],
            |r| r.get(0),
        )?;
        let wallet = WalletId::new(chain, format!("{MANUAL_PREFIX}{next}"));
        tx.execute(
            "INSERT INTO wallets (chain, address, label, portfolio_id, mode)
             VALUES (?1, ?2, ?3, ?4, 'manual')",
            params![chain.key(), wallet.address, label, portfolio],
        )?;
        // Nothing to download, so its saved history is complete from the start.
        tx.execute(
            "INSERT INTO synced (chain, address) VALUES (?1, ?2)",
            params![chain.key(), wallet.address],
        )?;
        tx.commit()?;
        Ok(wallet)
    }

    /// Makes a synced wallet manual: it stops downloading and its saved
    /// transactions become the user's to edit. There is no way back.
    pub fn convert_to_manual(&self, wallet: &WalletId) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let args = params![wallet.chain.key(), wallet.address];
        tx.execute(
            "UPDATE wallets SET mode = 'manual' WHERE chain = ?1 AND address = ?2",
            args,
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO synced (chain, address) VALUES (?1, ?2)",
            args,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// How the transactions of `wallet` come about, or `None` if it is not
    /// tracked.
    pub fn wallet_mode(&self, wallet: &WalletId) -> Result<Option<WalletMode>> {
        let mode: Option<String> = self
            .conn
            .query_row(
                "SELECT mode FROM wallets WHERE chain = ?1 AND address = ?2",
                params![wallet.chain.key(), wallet.address],
                |r| r.get(0),
            )
            .optional()?;
        mode.map(|key| WalletMode::from_key(&key).ok_or(Error::UnknownWalletMode(key)))
            .transpose()
    }

    fn require_manual(&self, wallet: &WalletId) -> Result<()> {
        match self.wallet_mode(wallet)? {
            Some(WalletMode::Manual) => Ok(()),
            _ => Err(Error::NotManual),
        }
    }

    /// Saves a transaction of a manual wallet, replacing the one with the same
    /// id. Fails if the wallet is not manual.
    pub fn put_manual_tx(&self, wallet: &WalletId, tx: &PricedTx) -> Result<()> {
        self.require_manual(wallet)?;
        self.insert_tx(&self.conn, wallet, tx)
    }

    /// Replaces the transaction `old_txid` of a manual wallet with `tx`, which
    /// may have another id, all or nothing.
    pub fn replace_manual_tx(
        &self,
        wallet: &WalletId,
        old_txid: &str,
        tx: &PricedTx,
    ) -> Result<()> {
        self.require_manual(wallet)?;
        let guard = self.conn.unchecked_transaction()?;
        guard.execute(
            "DELETE FROM txs WHERE chain = ?1 AND address = ?2 AND txid = ?3",
            params![wallet.chain.key(), wallet.address, old_txid],
        )?;
        self.insert_tx(&guard, wallet, tx)?;
        guard.commit()?;
        Ok(())
    }

    /// Removes a transaction of a manual wallet. An unknown id is not an
    /// error.
    pub fn delete_manual_tx(&self, wallet: &WalletId, txid: &str) -> Result<()> {
        self.require_manual(wallet)?;
        self.conn.execute(
            "DELETE FROM txs WHERE chain = ?1 AND address = ?2 AND txid = ?3",
            params![wallet.chain.key(), wallet.address, txid],
        )?;
        Ok(())
    }

    /// Puts a wallet in another portfolio. Its saved transactions stay.
    pub fn move_wallet(&self, wallet: &WalletId, portfolio: i64) -> Result<()> {
        self.require_portfolio(portfolio)?;
        self.conn.execute(
            "UPDATE wallets SET portfolio_id = ?3 WHERE chain = ?1 AND address = ?2",
            params![wallet.chain.key(), wallet.address, portfolio],
        )?;
        Ok(())
    }

    // --- Portfolios ------------------------------------------------------

    /// All portfolios in the order they were created.
    pub fn portfolios(&self) -> Result<Vec<PortfolioRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name FROM portfolios ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(PortfolioRecord {
                id: r.get(0)?,
                name: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    fn first_portfolio(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT min(id) FROM portfolios", [], |r| {
                r.get::<_, Option<i64>>(0)
            })?
            .ok_or(Error::NoSuchPortfolio)
    }

    fn require_portfolio(&self, id: i64) -> Result<()> {
        self.conn
            .query_row(
                "SELECT 1 FROM portfolios WHERE id = ?1",
                params![id],
                |_| Ok(()),
            )
            .optional()?
            .ok_or(Error::NoSuchPortfolio)
    }

    /// Checks a name the user typed: not empty, and not used by another
    /// portfolio (`except`), ignoring case. Returns it without surrounding
    /// spaces.
    fn clean_portfolio_name(&self, name: &str, except: Option<i64>) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::EmptyPortfolioName);
        }
        let taken = self
            .conn
            .query_row(
                "SELECT 1 FROM portfolios WHERE name = ?1 COLLATE NOCASE AND id != ?2",
                params![name, except.unwrap_or(-1)],
                |_| Ok(()),
            )
            .optional()?;
        if taken.is_some() {
            return Err(Error::PortfolioNameTaken);
        }
        Ok(name.to_owned())
    }

    /// Creates a portfolio and returns its id.
    pub fn add_portfolio(&self, name: &str) -> Result<i64> {
        let name = self.clean_portfolio_name(name, None)?;
        self.conn
            .execute("INSERT INTO portfolios (name) VALUES (?1)", params![name])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Gives a portfolio a new name.
    pub fn rename_portfolio(&self, id: i64, name: &str) -> Result<()> {
        self.require_portfolio(id)?;
        let name = self.clean_portfolio_name(name, Some(id))?;
        self.conn.execute(
            "UPDATE portfolios SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// Removes a portfolio together with its wallets and their saved
    /// transactions, all or nothing. The last portfolio cannot be removed.
    pub fn remove_portfolio(&self, id: i64) -> Result<()> {
        self.require_portfolio(id)?;
        let count: i64 = self
            .conn
            .query_row("SELECT count(*) FROM portfolios", [], |r| r.get(0))?;
        if count <= 1 {
            return Err(Error::LastPortfolio);
        }
        let tx = self.conn.unchecked_transaction()?;
        for table in ["txs", "synced"] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE (chain, address) IN
                     (SELECT chain, address FROM wallets WHERE portfolio_id = ?1)"
                ),
                params![id],
            )?;
        }
        tx.execute("DELETE FROM wallets WHERE portfolio_id = ?1", params![id])?;
        tx.execute("DELETE FROM portfolios WHERE id = ?1", params![id])?;
        tx.commit()?;
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
            for t in fresh {
                self.insert_tx(&tx, wallet, t)?;
            }
        }
        tx.execute(
            "INSERT OR IGNORE INTO synced (chain, address) VALUES (?1, ?2)",
            params![chain, wallet.address],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn insert_tx(&self, conn: &Connection, wallet: &WalletId, t: &PricedTx) -> Result<()> {
        let a = &t.activity;
        conn.prepare_cached(
            "INSERT OR REPLACE INTO txs
             (chain, address, txid, height, idx, time, amount, fee, parents, price_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?
        .execute(params![
            wallet.chain.key(),
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

    #[test]
    fn a_new_database_starts_with_one_portfolio() {
        let db = db();
        let portfolios = db.portfolios().expect("list");
        assert_eq!(portfolios.len(), 1);
        assert_eq!(portfolios[0].name, DEFAULT_PORTFOLIO_NAME);
    }

    #[test]
    fn wallets_are_added_to_a_portfolio_and_can_move() {
        let db = db();
        let main = db.portfolios().expect("list")[0].id;
        let other = db.add_portfolio("Business").expect("create");

        db.add_wallet(&btc("bc1qa"), "").expect("add to the first");
        db.add_wallet_to(&btc("bc1qb"), "", other).expect("add");
        let wallets = db.wallets().expect("list");
        assert_eq!((wallets[0].portfolio, wallets[1].portfolio), (main, other));

        db.move_wallet(&btc("bc1qa"), other).expect("move");
        assert_eq!(db.wallets().expect("list")[0].portfolio, other);

        assert!(matches!(
            db.add_wallet_to(&btc("bc1qc"), "", 999),
            Err(Error::NoSuchPortfolio)
        ));
        assert!(matches!(
            db.move_wallet(&btc("bc1qa"), 999),
            Err(Error::NoSuchPortfolio)
        ));
    }

    #[test]
    fn portfolio_names_are_trimmed_and_unique_ignoring_case() {
        let db = db();
        let id = db.add_portfolio("  Business  ").expect("create");
        assert_eq!(db.portfolios().expect("list")[1].name, "Business");

        assert!(matches!(
            db.add_portfolio(""),
            Err(Error::EmptyPortfolioName)
        ));
        assert!(matches!(
            db.add_portfolio("   "),
            Err(Error::EmptyPortfolioName)
        ));
        assert!(matches!(
            db.add_portfolio("business"),
            Err(Error::PortfolioNameTaken)
        ));
        assert!(matches!(
            db.add_portfolio("MAIN"),
            Err(Error::PortfolioNameTaken)
        ));

        // Renaming to its own name, only in another case, is fine.
        db.rename_portfolio(id, "BUSINESS").expect("rename");
        assert_eq!(db.portfolios().expect("list")[1].name, "BUSINESS");
        assert!(matches!(
            db.rename_portfolio(id, "main"),
            Err(Error::PortfolioNameTaken)
        ));
        assert!(matches!(
            db.rename_portfolio(999, "Anything"),
            Err(Error::NoSuchPortfolio)
        ));
    }

    #[test]
    fn removing_a_portfolio_removes_its_wallets_and_their_transactions_only() {
        let db = db();
        let other = db.add_portfolio("Business").expect("create");
        db.add_wallet(&btc("bc1qa"), "keep").expect("add");
        db.add_wallet_to(&btc("bc1qb"), "drop", other).expect("add");
        db.replace_history_above(&btc("bc1qa"), 0, &[priced("a1", 10)])
            .expect("save");
        db.replace_history_above(&btc("bc1qb"), 0, &[priced("b1", 10)])
            .expect("save");

        db.remove_portfolio(other).expect("remove");

        let left = db.wallets().expect("list");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id.address, "bc1qa");
        assert_eq!(db.history(&btc("bc1qa")).expect("history").len(), 1);
        assert!(db.history(&btc("bc1qb")).expect("history").is_empty());
        assert!(!db.is_synced(&btc("bc1qb")).expect("synced"));
        assert_eq!(db.portfolios().expect("list").len(), 1);
    }

    #[test]
    fn the_last_portfolio_cannot_be_removed() {
        let db = db();
        let only = db.portfolios().expect("list")[0].id;
        assert!(matches!(
            db.remove_portfolio(only),
            Err(Error::LastPortfolio)
        ));
        assert!(matches!(
            db.remove_portfolio(999),
            Err(Error::NoSuchPortfolio)
        ));
    }

    /// The layout version 2 used: wallets, but no portfolios.
    const V2_SCHEMA: &str = "
        CREATE TABLE prices (chain TEXT NOT NULL, day INTEGER NOT NULL, usd REAL NOT NULL,
            PRIMARY KEY (chain, day));
        CREATE TABLE wallets (position INTEGER PRIMARY KEY AUTOINCREMENT, chain TEXT NOT NULL,
            address TEXT NOT NULL, label TEXT NOT NULL, UNIQUE (chain, address));
        CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE txs (chain TEXT NOT NULL, address TEXT NOT NULL, txid TEXT NOT NULL,
            height INTEGER NOT NULL, idx INTEGER, time INTEGER NOT NULL, amount TEXT NOT NULL,
            fee TEXT NOT NULL, parents TEXT NOT NULL, price_usd REAL NOT NULL,
            PRIMARY KEY (chain, address, txid));
        CREATE TABLE synced (chain TEXT NOT NULL, address TEXT NOT NULL,
            PRIMARY KEY (chain, address));
        INSERT INTO wallets (chain, address, label) VALUES ('bitcoin', 'bc1qa', 'First');
        INSERT INTO wallets (chain, address, label) VALUES ('ethereum', '0xb', 'Second');
        INSERT INTO txs VALUES ('bitcoin', 'bc1qa', 'tx1', 10, NULL, 1700000000, '1000', '0', '', 30000.0);
        INSERT INTO synced VALUES ('bitcoin', 'bc1qa');
        INSERT INTO settings VALUES ('hidden', '1');
        PRAGMA user_version = 2;";

    fn write_v2_file(path: &Path, key: &str) {
        let conn = Connection::open(path).expect("create");
        set_pragma_text(&conn, "key", key).expect("key");
        conn.execute_batch(V2_SCHEMA).expect("v2 schema");
    }

    #[test]
    fn a_version_2_file_gets_one_portfolio_holding_everything() {
        let dir = TempDir::new("v2");
        let path = dir.path().join("morlaco.db");
        write_v2_file(&path, "key");

        let db = Database::open(&path, "key").expect("upgrades");
        let portfolios = db.portfolios().expect("list");
        assert_eq!(portfolios.len(), 1);
        assert_eq!(portfolios[0].name, DEFAULT_PORTFOLIO_NAME);

        let wallets = db.wallets().expect("list");
        assert_eq!(wallets.len(), 2);
        assert!(wallets.iter().all(|w| w.portfolio == portfolios[0].id));
        assert_eq!(wallets[0].label, "First");
        // Transactions, sync marks and settings are all still there.
        assert_eq!(db.history(&btc("bc1qa")).expect("history").len(), 1);
        assert!(db.is_synced(&btc("bc1qa")).expect("synced"));
        assert_eq!(db.setting("hidden").expect("read").as_deref(), Some("1"));
        let version: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("read");
        assert_eq!(version, SCHEMA_VERSION);

        // New wallets can be added to the upgraded file.
        db.add_wallet(&btc("bc1qnew"), "").expect("add");
    }

    #[test]
    fn upgrading_keeps_a_copy_of_the_old_file_once() {
        let dir = TempDir::new("v2copy");
        let path = dir.path().join("morlaco.db");
        write_v2_file(&path, "key");
        let before = std::fs::read(&path).expect("read");
        let copy = dir.path().join("morlaco.db.v2-backup");
        assert_eq!(copy, upgrade_copy_path(&path, VERSION_WITHOUT_PORTFOLIOS));

        drop(Database::open(&path, "key").expect("upgrades"));
        assert_eq!(std::fs::read(&copy).expect("copy exists"), before);
        let mode = std::fs::metadata(&copy)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        // Later opens leave the copy alone, even though the file has changed.
        let db = Database::open(&path, "key").expect("opens");
        db.add_wallet(&btc("bc1qnew"), "").expect("change");
        drop(db);
        drop(Database::open(&path, "key").expect("opens again"));
        assert_eq!(std::fs::read(&copy).expect("copy exists"), before);
    }

    #[test]
    fn a_version_2_file_is_not_copied_when_the_key_is_wrong() {
        let dir = TempDir::new("v2wrong");
        let path = dir.path().join("morlaco.db");
        write_v2_file(&path, "key");

        assert!(matches!(
            Database::open(&path, "other"),
            Err(Error::WrongKey)
        ));
        assert!(!dir.path().join("morlaco.db.v2-backup").exists());
    }

    /// The layout version 3 used: portfolios, but no wallet modes.
    const V3_SCHEMA: &str = "
        CREATE TABLE prices (chain TEXT NOT NULL, day INTEGER NOT NULL, usd REAL NOT NULL,
            PRIMARY KEY (chain, day));
        CREATE TABLE portfolios (id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE COLLATE NOCASE);
        CREATE TABLE wallets (position INTEGER PRIMARY KEY AUTOINCREMENT, chain TEXT NOT NULL,
            address TEXT NOT NULL, label TEXT NOT NULL, portfolio_id INTEGER NOT NULL DEFAULT 1,
            UNIQUE (chain, address));
        CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE txs (chain TEXT NOT NULL, address TEXT NOT NULL, txid TEXT NOT NULL,
            height INTEGER NOT NULL, idx INTEGER, time INTEGER NOT NULL, amount TEXT NOT NULL,
            fee TEXT NOT NULL, parents TEXT NOT NULL, price_usd REAL NOT NULL,
            PRIMARY KEY (chain, address, txid));
        CREATE TABLE synced (chain TEXT NOT NULL, address TEXT NOT NULL,
            PRIMARY KEY (chain, address));
        INSERT INTO portfolios (name) VALUES ('Main');
        INSERT INTO wallets (chain, address, label) VALUES ('bitcoin', 'bc1qa', 'First');
        INSERT INTO txs VALUES ('bitcoin', 'bc1qa', 'tx1', 10, NULL, 1700000000, '1000', '0', '', 30000.0);
        INSERT INTO synced VALUES ('bitcoin', 'bc1qa');
        PRAGMA user_version = 3;";

    #[test]
    fn a_version_3_file_gets_synced_wallets_and_keeps_a_copy() {
        let dir = TempDir::new("v3");
        let path = dir.path().join("morlaco.db");
        {
            let conn = Connection::open(&path).expect("create");
            set_pragma_text(&conn, "key", "key").expect("key");
            conn.execute_batch(V3_SCHEMA).expect("v3 schema");
        }

        let db = Database::open(&path, "key").expect("upgrades");
        let wallets = db.wallets().expect("list");
        assert_eq!(wallets.len(), 1);
        assert_eq!(wallets[0].mode, WalletMode::Synced);
        assert_eq!(db.history(&btc("bc1qa")).expect("history").len(), 1);
        assert!(dir.path().join("morlaco.db.v3-backup").exists());
    }

    fn manual_tx(txid: &str, time: i64, amount: i128) -> PricedTx {
        PricedTx {
            activity: Activity {
                txid: txid.to_owned(),
                height: 0,
                index: None,
                time,
                amount,
                fee: 0,
                parents: Vec::new(),
            },
            price_usd: 20_000.0,
        }
    }

    #[test]
    fn a_manual_wallet_gets_its_own_address_and_counts_as_synced() {
        let db = db();
        let portfolio = db.portfolios().expect("list")[0].id;
        let a = db
            .add_manual_wallet(Chain::Bitcoin, "Coinbase", portfolio)
            .expect("add");
        let b = db
            .add_manual_wallet(Chain::Bitcoin, "Kraken", portfolio)
            .expect("add");
        assert_ne!(a, b);
        assert!(a.address.starts_with(MANUAL_PREFIX));
        assert!(db.is_synced(&a).expect("synced"));

        let wallets = db.wallets().expect("list");
        assert!(wallets.iter().all(|w| w.mode == WalletMode::Manual));
    }

    #[test]
    fn manual_transactions_can_be_added_edited_and_removed() {
        let db = db();
        let portfolio = db.portfolios().expect("list")[0].id;
        let w = db
            .add_manual_wallet(Chain::Bitcoin, "Coinbase", portfolio)
            .expect("add");

        db.put_manual_tx(&w, &manual_tx("a", 100, 5)).expect("put");
        db.put_manual_tx(&w, &manual_tx("b", 200, 6)).expect("put");
        db.replace_manual_tx(&w, "a", &manual_tx("c", 150, 7))
            .expect("replace");
        db.delete_manual_tx(&w, "b").expect("delete");
        db.delete_manual_tx(&w, "missing").expect("unknown is fine");

        let ids: Vec<String> = db
            .history(&w)
            .expect("history")
            .into_iter()
            .map(|t| t.activity.txid)
            .collect();
        assert_eq!(ids, ["c"]);
    }

    #[test]
    fn a_synced_wallet_rejects_manual_edits() {
        let db = db();
        db.add_wallet(&btc("bc1qa"), "").expect("add");
        let tx = manual_tx("a", 100, 5);
        assert!(matches!(
            db.put_manual_tx(&btc("bc1qa"), &tx),
            Err(Error::NotManual)
        ));
        assert!(matches!(
            db.replace_manual_tx(&btc("bc1qa"), "a", &tx),
            Err(Error::NotManual)
        ));
        assert!(matches!(
            db.delete_manual_tx(&btc("bc1qa"), "a"),
            Err(Error::NotManual)
        ));
    }

    #[test]
    fn converting_keeps_the_transactions_and_unlocks_editing() {
        let db = db();
        let w = btc("bc1qa");
        db.add_wallet(&w, "Own").expect("add");
        db.replace_history_above(&w, 0, &[manual_tx("chain-tx", 100, 5)])
            .expect("save");

        db.convert_to_manual(&w).expect("convert");
        assert_eq!(db.wallets().expect("list")[0].mode, WalletMode::Manual);
        assert_eq!(db.history(&w).expect("history").len(), 1);

        db.replace_manual_tx(&w, "chain-tx", &manual_tx("chain-tx", 100, 9))
            .expect("edit");
        assert_eq!(db.history(&w).expect("history")[0].activity.amount, 9);
    }

    #[test]
    fn converting_a_wallet_that_never_synced_still_shows_its_history() {
        let db = db();
        let w = btc("bc1qa");
        db.add_wallet(&w, "").expect("add");
        assert!(!db.is_synced(&w).expect("synced"));
        db.convert_to_manual(&w).expect("convert");
        assert!(db.is_synced(&w).expect("synced"));
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
