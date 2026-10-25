//! Saved wallets and settings, kept in a small tab-separated text file.
//!
//! ```text
//! hidden<TAB>1
//! wallet<TAB>bc1q...<TAB>Cold storage
//! ```

use std::io;
use std::path::{Path, PathBuf};

use morlaco_core::paths;

/// A tracked address with an optional user label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wallet {
    /// Bitcoin address.
    pub address: String,
    /// Name chosen by the user; may be empty.
    pub label: String,
}

impl Wallet {
    /// The label if there is one, otherwise a shortened address.
    pub fn display_name(&self) -> String {
        if self.label.is_empty() {
            crate::format::short_address(&self.address)
        } else {
            self.label.clone()
        }
    }
}

/// Everything the app remembers between runs.
#[derive(Debug, Default)]
pub struct Store {
    /// Tracked wallets in the order they were added.
    pub wallets: Vec<Wallet>,
    /// Whether amounts are masked.
    pub hidden: bool,
    path: Option<PathBuf>,
}

impl Store {
    /// Loads the store from the config directory. A missing or unreadable
    /// file yields an empty store; the file is replaced on the next save.
    pub fn load() -> Self {
        let path = paths::config_dir().map(|d| d.join("wallets.tsv"));
        let mut store = path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| Self::parse(&text))
            .unwrap_or_default();
        store.path = path;
        store
    }

    fn parse(text: &str) -> Self {
        let mut store = Self::default();
        for line in text.lines() {
            let mut fields = line.split('\t');
            match (fields.next(), fields.next(), fields.next()) {
                (Some("hidden"), Some(v), _) => store.hidden = v == "1",
                (Some("wallet"), Some(address), label) if !address.is_empty() => {
                    store.wallets.push(Wallet {
                        address: address.to_owned(),
                        label: label.unwrap_or_default().to_owned(),
                    });
                }
                _ => {}
            }
        }
        store
    }

    fn serialize(&self) -> String {
        let mut out = format!("hidden\t{}\n", u8::from(self.hidden));
        for w in &self.wallets {
            out.push_str(&format!("wallet\t{}\t{}\n", w.address, w.label));
        }
        out
    }

    /// Writes the store to disk.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Err(io::Error::other("no config directory"));
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        write_atomic(path, &self.serialize())
    }

    /// Index of the wallet with this address.
    pub fn position(&self, address: &str) -> Option<usize> {
        self.wallets.iter().position(|w| w.address == address)
    }

    /// Adds a wallet and returns its index. Control characters in the label
    /// are replaced so they cannot break the file format.
    pub fn add(&mut self, address: &str, label: &str) -> usize {
        self.wallets.push(Wallet {
            address: address.to_owned(),
            label: clean_label(label),
        });
        self.wallets.len() - 1
    }

    /// Changes the label of the wallet at `index`, if it exists. An empty
    /// label makes the wallet display its address again.
    pub fn rename(&mut self, index: usize, label: &str) {
        if let Some(wallet) = self.wallets.get_mut(index) {
            wallet.label = clean_label(label);
        }
    }

    /// Removes the wallet at `index`, if it exists.
    pub fn remove(&mut self, index: usize) {
        if index < self.wallets.len() {
            self.wallets.remove(index);
        }
    }
}

/// Trims a label and replaces control characters so they cannot break the
/// file format.
fn clean_label(label: &str) -> String {
    label
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Writes via a temporary file so a crash cannot leave a half-written store.
fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut store = Store {
            hidden: true,
            ..Store::default()
        };
        store.add("bc1qaaaaaaaaaaaaaaaa", "Cold storage");
        store.add("bc1qbbbbbbbbbbbbbbbb", "");

        let back = Store::parse(&store.serialize());
        assert!(back.hidden);
        assert_eq!(back.wallets, store.wallets);
    }

    #[test]
    fn labels_cannot_break_the_format() {
        let mut store = Store::default();
        store.add("bc1qaaaaaaaaaaaaaaaa", "  evil\tlabel\nwallet\tbc1qother ");
        let back = Store::parse(&store.serialize());
        assert_eq!(back.wallets.len(), 1);
        assert_eq!(back.wallets[0].label, "evil label wallet bc1qother");
    }

    #[test]
    fn rename_sets_and_clears_the_label() {
        let mut store = Store::default();
        store.add("bc1qaaaaaaaaaaaaaaaa", "Old");

        store.rename(0, "  New\tname ");
        assert_eq!(store.wallets[0].label, "New name");
        assert_eq!(store.wallets[0].display_name(), "New name");

        store.rename(0, "");
        assert_eq!(store.wallets[0].display_name(), "bc1qaaaa…aaaaaa");

        store.rename(9, "ignored");
        assert_eq!(store.wallets.len(), 1);
    }

    #[test]
    fn garbage_lines_are_ignored() {
        let store = Store::parse("nonsense\nwallet\t\nhidden\n\nwallet\tbc1qok\n");
        assert_eq!(store.wallets.len(), 1);
        assert_eq!(store.wallets[0].address, "bc1qok");
        assert!(!store.hidden);
    }

    #[test]
    fn remove_ignores_out_of_range() {
        let mut store = Store::default();
        store.add("bc1qaaaaaaaaaaaaaaaa", "");
        store.remove(5);
        assert_eq!(store.wallets.len(), 1);
        store.remove(0);
        assert!(store.wallets.is_empty());
    }
}
