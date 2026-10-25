//! The database key: a random secret kept in the login keyring, optionally
//! combined with a passphrase the user types.
//!
//! The secret alone protects the file from anyone who cannot read your
//! keyring. Adding a passphrase also protects it from someone using your
//! unlocked session. Both are needed together, so even a weak passphrase
//! cannot be guessed offline without the keyring secret.

use std::collections::HashMap;
use std::io::Read;

use gtk4::gio;
use libsecret::{
    COLLECTION_DEFAULT, Schema, SchemaAttributeType, SchemaFlags, password_clear_sync,
    password_lookup_sync, password_store_sync,
};

const SCHEMA_NAME: &str = "io.github.jeronimoalbi.Morlaco.DatabaseKey";
const LABEL: &str = "Morlaco database key";

/// What opens the database.
#[derive(Clone)]
pub struct Keys {
    secret: String,
    passphrase: Option<String>,
}

impl Keys {
    /// Combines the keyring secret with an optional passphrase.
    pub fn new(secret: String, passphrase: Option<String>) -> Self {
        Self { secret, passphrase }
    }

    /// Whether the user set a passphrase.
    pub fn has_passphrase(&self) -> bool {
        self.passphrase.is_some()
    }

    /// The same secret with a different passphrase (or none).
    pub fn with_passphrase(&self, passphrase: Option<String>) -> Self {
        Self::new(self.secret.clone(), passphrase)
    }

    /// The string handed to SQLCipher.
    pub fn database_key(&self) -> String {
        compose(&self.secret, self.passphrase.as_deref())
    }
}

/// Joins the secret and the passphrase. Without a passphrase the key is
/// the secret alone.
pub fn compose(secret: &str, passphrase: Option<&str>) -> String {
    match passphrase {
        Some(passphrase) => format!("{secret}\n{passphrase}"),
        None => secret.to_owned(),
    }
}

/// Creates a new random 256-bit secret, hex encoded.
pub fn generate_secret() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("could not read random bytes: {e}"))?;
    Ok(to_hex(&bytes))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn schema() -> Schema {
    Schema::new(
        SCHEMA_NAME,
        SchemaFlags::NONE,
        HashMap::from([("purpose", SchemaAttributeType::String)]),
    )
}

fn attributes() -> HashMap<&'static str, &'static str> {
    HashMap::from([("purpose", "database-key")])
}

/// Reads the secret from the keyring. `None` if there is none yet.
///
/// May block while the keyring asks the user to unlock it.
pub fn lookup() -> Result<Option<String>, String> {
    password_lookup_sync(Some(&schema()), attributes(), gio::Cancellable::NONE)
        .map(|found| found.map(|s| s.to_string()))
        .map_err(|e| e.message().to_owned())
}

/// Saves the secret in the keyring, replacing any earlier one.
pub fn store(secret: &str) -> Result<(), String> {
    password_store_sync(
        Some(&schema()),
        attributes(),
        Some(COLLECTION_DEFAULT),
        LABEL,
        secret,
        gio::Cancellable::NONE,
    )
    .map_err(|e| e.message().to_owned())
}

/// Removes the secret from the keyring.
pub fn clear() -> Result<(), String> {
    password_clear_sync(Some(&schema()), attributes(), gio::Cancellable::NONE)
        .map_err(|e| e.message().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_the_secret_alone_without_a_passphrase() {
        let keys = Keys::new("abc".into(), None);
        assert_eq!(keys.database_key(), "abc");
        assert!(!keys.has_passphrase());
    }

    #[test]
    fn a_passphrase_changes_the_key_and_both_parts_matter() {
        let plain = Keys::new("abc".into(), None);
        let locked = plain.with_passphrase(Some("pw".into()));
        assert!(locked.has_passphrase());
        assert_eq!(locked.database_key(), "abc\npw");
        assert_ne!(
            locked.database_key(),
            Keys::new("abd".into(), Some("pw".into())).database_key()
        );
        assert_eq!(locked.with_passphrase(None).database_key(), "abc");
    }

    #[test]
    fn secrets_are_64_hex_digits_and_differ() {
        let a = generate_secret().expect("random bytes");
        let b = generate_secret().expect("random bytes");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn hex_encoding() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
    }
}
