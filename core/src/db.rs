//! The local SQLite database: tracked wallets, settings and cached prices.

use std::fs::DirBuilder;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{Error, Result};

/// How long a connection waits for another one to release a lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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

/// A wallet as stored: an address and the user's label for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletRecord {
    /// Bitcoin address.
    pub address: String,
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
    /// Fails with [`Error::WrongKey`] when it does not match the file.
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

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS prices (
                 day INTEGER PRIMARY KEY,
                 usd REAL NOT NULL
             );
             CREATE TABLE IF NOT EXISTS wallets (
                 position INTEGER PRIMARY KEY AUTOINCREMENT,
                 address TEXT NOT NULL UNIQUE,
                 label TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS settings (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        Ok(Self { conn })
    }

    // --- Prices ----------------------------------------------------------

    /// Returns the cached BTC/USD price for `day` (days since the Unix
    /// epoch). Historical prices never change, so a cached day is never
    /// refreshed.
    pub fn price(&self, day: i64) -> Result<Option<f64>> {
        let usd = self
            .conn
            .query_row("SELECT usd FROM prices WHERE day = ?1", params![day], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(usd)
    }

    /// Stores the price for `day`, replacing any previous value.
    pub fn put_price(&self, day: i64, usd: f64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO prices (day, usd) VALUES (?1, ?2)",
            params![day, usd],
        )?;
        Ok(())
    }

    // --- Wallets ---------------------------------------------------------

    /// All wallets in the order they were added.
    pub fn wallets(&self) -> Result<Vec<WalletRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT address, label FROM wallets ORDER BY position")?;
        let rows = stmt.query_map([], |r| {
            Ok(WalletRecord {
                address: r.get(0)?,
                label: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Adds a wallet at the end of the list. Fails if the address is
    /// already tracked.
    pub fn add_wallet(&self, address: &str, label: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO wallets (address, label) VALUES (?1, ?2)",
            params![address, label],
        )?;
        Ok(())
    }

    /// Changes the label of the wallet with this address.
    pub fn rename_wallet(&self, address: &str, label: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE wallets SET label = ?2 WHERE address = ?1",
            params![address, label],
        )?;
        Ok(())
    }

    /// Stops tracking the wallet with this address.
    pub fn remove_wallet(&self, address: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM wallets WHERE address = ?1", params![address])?;
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

    #[test]
    fn price_round_trip() {
        let db = db();
        assert_eq!(db.price(19_000).expect("query"), None);
        db.put_price(19_000, 35_564.0).expect("insert");
        assert_eq!(db.price(19_000).expect("query"), Some(35_564.0));
    }

    #[test]
    fn wallets_keep_insertion_order() {
        let db = db();
        db.add_wallet("bc1qb", "Second alphabetically")
            .expect("add");
        db.add_wallet("bc1qa", "").expect("add");

        let addresses: Vec<String> = db
            .wallets()
            .expect("list")
            .into_iter()
            .map(|w| w.address)
            .collect();
        assert_eq!(addresses, ["bc1qb", "bc1qa"]);
    }

    #[test]
    fn duplicate_wallets_are_rejected() {
        let db = db();
        db.add_wallet("bc1qa", "").expect("add");
        assert!(db.add_wallet("bc1qa", "again").is_err());
        assert_eq!(db.wallets().expect("list").len(), 1);
    }

    #[test]
    fn rename_and_remove() {
        let db = db();
        db.add_wallet("bc1qa", "Old").expect("add");
        db.add_wallet("bc1qb", "Keep").expect("add");

        db.rename_wallet("bc1qa", "New").expect("rename");
        assert_eq!(db.wallets().expect("list")[0].label, "New");

        db.remove_wallet("bc1qa").expect("remove");
        let left = db.wallets().expect("list");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].address, "bc1qb");

        // Unknown addresses are not an error.
        db.remove_wallet("bc1qzzz").expect("remove");
    }

    #[test]
    fn settings_round_trip_and_overwrite() {
        let db = db();
        assert_eq!(db.setting("hidden").expect("read"), None);
        db.set_setting("hidden", "1").expect("write");
        db.set_setting("hidden", "0").expect("write");
        assert_eq!(db.setting("hidden").expect("read"), Some("0".to_owned()));
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
            .add_wallet("bc1qa", "Saved")
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
            .add_wallet("bc1qa", "")
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
        db.add_wallet("bc1qa", "").expect("add");
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

        let plain = Connection::open(&path).expect("plain db");
        plain
            .execute_batch(
                "CREATE TABLE wallets (position INTEGER PRIMARY KEY AUTOINCREMENT,
                     address TEXT NOT NULL UNIQUE, label TEXT NOT NULL);
                 INSERT INTO wallets (address, label) VALUES ('bc1qa', 'Old');",
            )
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
