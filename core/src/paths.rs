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

/// Directory for user settings and saved wallets (`~/.config/morlaco`).
pub fn config_dir() -> Option<PathBuf> {
    app_dir("XDG_CONFIG_HOME", ".config")
}

/// Path of the price cache database (`~/.cache/morlaco/cache.sqlite3`).
pub fn cache_file() -> Option<PathBuf> {
    app_dir("XDG_CACHE_HOME", ".cache").map(|d| d.join("cache.sqlite3"))
}
