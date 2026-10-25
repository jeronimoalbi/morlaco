//! SQLite cache for historical prices.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;

/// Local cache of BTC/USD prices, keyed by day.
///
/// Historical prices never change, so a cached day is never refreshed.
pub struct Cache {
    conn: Connection,
}

impl Cache {
    /// Opens (creating if needed) the cache database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// Creates a throwaway cache that lives only in memory.
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS prices (
                 day INTEGER PRIMARY KEY,
                 usd REAL NOT NULL
             )",
            [],
        )?;
        Ok(Self { conn })
    }

    /// Returns the cached price for `day` (days since the Unix epoch).
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_round_trip() {
        let cache = Cache::in_memory().expect("in-memory db opens");
        assert_eq!(cache.price(19_000).expect("query"), None);
        cache.put_price(19_000, 35_564.0).expect("insert");
        assert_eq!(cache.price(19_000).expect("query"), Some(35_564.0));
    }
}
