//! One-time import of data kept by earlier versions.
//!
//! Wallets and the privacy setting used to live in a tab-separated text file,
//! and prices in a separate cache database. Both now live in the main
//! database.

use std::path::Path;

use morlaco_core::Result;
use morlaco_core::paths;

use crate::store::Store;

/// What the old text file contained.
#[derive(Debug, Default, PartialEq, Eq)]
struct Legacy {
    wallets: Vec<(String, String)>,
    hidden: bool,
}

/// Parses the old file format:
///
/// ```text
/// hidden<TAB>1
/// wallet<TAB>bc1q...<TAB>Cold storage
/// ```
///
/// Unrecognized lines are ignored.
fn parse(text: &str) -> Legacy {
    let mut legacy = Legacy::default();
    for line in text.lines() {
        let mut fields = line.split('\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some("hidden"), Some(v), _) => legacy.hidden = v == "1",
            (Some("wallet"), Some(address), label) if !address.is_empty() => {
                legacy
                    .wallets
                    .push((address.to_owned(), label.unwrap_or_default().to_owned()));
            }
            _ => {}
        }
    }
    legacy
}

/// Moves the old wallet file into `store` and deletes it, then deletes the
/// old price cache. Does nothing when the files are not there.
///
/// The text file is deleted only after its contents were saved. A file that
/// cannot be removed is left alone and imported again (without duplicates)
/// on the next start.
pub fn import_legacy_data(store: &mut Store) -> Result<()> {
    import_from(
        store,
        paths::legacy_wallets_file().as_deref(),
        paths::legacy_cache_file().as_deref(),
    )
}

fn import_from(
    store: &mut Store,
    wallets_file: Option<&Path>,
    cache_file: Option<&Path>,
) -> Result<()> {
    if let Some(path) = wallets_file
        && let Ok(text) = std::fs::read_to_string(path)
    {
        let legacy = parse(&text);
        for (address, label) in &legacy.wallets {
            if store.position(address).is_none() {
                store.add(address, label)?;
            }
        }
        if legacy.hidden {
            store.set_hidden(true)?;
        }
        // Not fatal: the import is repeatable.
        let _ = std::fs::remove_file(path);
    }

    // Only holds public prices, which are fetched again when needed.
    if let Some(path) = cache_file {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wallets_and_the_privacy_flag() {
        let legacy =
            parse("hidden\t1\nwallet\tbc1qa\tCold storage\nwallet\tbc1qb\t\nwallet\tbc1qc\n");
        assert!(legacy.hidden);
        assert_eq!(
            legacy.wallets,
            [
                ("bc1qa".to_owned(), "Cold storage".to_owned()),
                ("bc1qb".to_owned(), String::new()),
                ("bc1qc".to_owned(), String::new()),
            ]
        );
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("morlaco-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn import_moves_wallets_into_the_store_and_removes_the_old_files() {
        use morlaco_core::db::Database;

        let dir = temp_dir("import");
        let wallets = dir.join("wallets.tsv");
        let cache = dir.join("cache.sqlite3");
        std::fs::write(
            &wallets,
            "hidden\t1\nwallet\tbc1qa\tSaved\nwallet\tbc1qb\t\n",
        )
        .expect("write");
        std::fs::write(&cache, "old prices").expect("write");

        let mut store = Store::open(Database::in_memory().expect("db")).expect("store");
        // Already present: must not be added twice.
        store.add("bc1qb", "").expect("add");

        import_from(&mut store, Some(&wallets), Some(&cache)).expect("import");

        let addresses: Vec<&str> = store.wallets.iter().map(|w| w.address.as_str()).collect();
        assert_eq!(addresses, ["bc1qb", "bc1qa"]);
        assert!(store.hidden);
        assert!(!wallets.exists() && !cache.exists());

        // A second run with nothing left to import is a no-op.
        import_from(&mut store, Some(&wallets), Some(&cache)).expect("import");
        assert_eq!(store.wallets.len(), 2);

        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn garbage_lines_are_ignored() {
        let legacy = parse("nonsense\nwallet\t\nhidden\n\nwallet\tbc1qok\n");
        assert_eq!(legacy.wallets, [("bc1qok".to_owned(), String::new())]);
        assert!(!legacy.hidden);
    }
}
