//! Where Morlaco keeps its files, following the XDG base directory spec.

use std::path::PathBuf;

/// Resolves `$<var>/morlaco`, falling back to `$HOME/<fallback>/morlaco`.
fn app_dir(var: &str, fallback: &str) -> Option<PathBuf> {
    let base = std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))?;
    Some(base.join("morlaco"))
}

/// Path of the database (`~/.local/share/morlaco/morlaco.db`).
pub fn data_file() -> Option<PathBuf> {
    app_dir("XDG_DATA_HOME", ".local/share").map(|d| d.join("morlaco.db"))
}

/// Where earlier versions kept the wallet list (`~/.config/morlaco/wallets.tsv`).
pub fn legacy_wallets_file() -> Option<PathBuf> {
    app_dir("XDG_CONFIG_HOME", ".config").map(|d| d.join("wallets.tsv"))
}

/// Where earlier versions kept cached prices (`~/.cache/morlaco/cache.sqlite3`).
pub fn legacy_cache_file() -> Option<PathBuf> {
    app_dir("XDG_CACHE_HOME", ".cache").map(|d| d.join("cache.sqlite3"))
}
