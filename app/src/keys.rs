//! The database key and the three ways to protect it.
//!
//! - **Keyring:** a random secret kept in the login keyring. Nothing to type.
//!   It protects the file from anyone who cannot read your keyring.
//! - **Keyring and passphrase:** the secret combined with a passphrase you
//!   type at every start. Both are needed, so even a weak passphrase cannot
//!   be guessed offline without the keyring secret, and someone using your
//!   unlocked session still needs the passphrase.
//! - **Passphrase only:** no keyring at all. The file moves between computers
//!   and works where no keyring exists, but it is only as strong as the
//!   passphrase, which can be guessed offline if the file leaks.

use std::collections::HashMap;
use std::io::Read;

use gtk4::gio;
use libsecret::{
    COLLECTION_DEFAULT, Schema, SchemaAttributeType, SchemaFlags, password_clear_sync,
    password_lookup_sync, password_store_sync,
};

const SCHEMA_NAME: &str = "io.github.jeronimoalbi.Morlaco.DatabaseKey";
const LABEL: &str = "Morlaco database key";

/// Shortest passphrase accepted when it is the only protection.
pub const MIN_PASSPHRASE_ONLY_CHARS: usize = 12;

/// How the database key is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A secret in the keyring.
    Keyring,
    /// The keyring secret together with a passphrase.
    KeyringAndPassphrase,
    /// A passphrase alone.
    PassphraseOnly,
}

/// What opens the database.
#[derive(Clone, PartialEq, Eq)]
pub enum Keys {
    /// The secret from the keyring.
    Keyring {
        /// The 256-bit secret, hex encoded.
        secret: String,
    },
    /// The keyring secret and a passphrase.
    KeyringAndPassphrase {
        /// The 256-bit secret, hex encoded.
        secret: String,
        /// What the user types.
        passphrase: String,
    },
    /// A passphrase and nothing else.
    PassphraseOnly {
        /// What the user types.
        passphrase: String,
    },
}

// Not derived: the secrets must never end up in a log or a panic message.
impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keys({:?}, secrets hidden)", self.mode())
    }
}

impl Keys {
    /// How this key is protected.
    pub fn mode(&self) -> Mode {
        match self {
            Keys::Keyring { .. } => Mode::Keyring,
            Keys::KeyringAndPassphrase { .. } => Mode::KeyringAndPassphrase,
            Keys::PassphraseOnly { .. } => Mode::PassphraseOnly,
        }
    }

    /// The keyring secret, if this protection uses one.
    pub fn secret(&self) -> Option<&str> {
        match self {
            Keys::Keyring { secret } | Keys::KeyringAndPassphrase { secret, .. } => Some(secret),
            Keys::PassphraseOnly { .. } => None,
        }
    }

    /// The string handed to SQLCipher, which stretches it into the real key.
    ///
    /// For a passphrase alone that is the passphrase itself, so the file can
    /// also be opened with any standard SQLCipher tool.
    pub fn database_key(&self) -> String {
        match self {
            Keys::Keyring { secret } => secret.clone(),
            Keys::KeyringAndPassphrase { secret, passphrase } => compose(secret, Some(passphrase)),
            Keys::PassphraseOnly { passphrase } => passphrase.clone(),
        }
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

/// Where the secret is kept. A trait so everything that changes the keyring
/// can be tested without touching a real one.
pub trait Keyring {
    /// Reads the secret. `Ok(None)` if there is none yet, `Err` if the
    /// keyring cannot be reached. May block while it asks to be unlocked.
    fn lookup(&self) -> Result<Option<String>, String>;
    /// Saves the secret, replacing any earlier one.
    fn store(&self, secret: &str) -> Result<(), String>;
    /// Removes the secret.
    fn clear(&self) -> Result<(), String>;
}

/// The login keyring, through the Secret Service (libsecret).
pub struct SecretService;

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

impl Keyring for SecretService {
    fn lookup(&self) -> Result<Option<String>, String> {
        password_lookup_sync(Some(&schema()), attributes(), gio::Cancellable::NONE)
            .map(|found| found.map(|s| s.to_string()))
            .map_err(|e| e.message().to_owned())
    }

    fn store(&self, secret: &str) -> Result<(), String> {
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

    fn clear(&self) -> Result<(), String> {
        password_clear_sync(Some(&schema()), attributes(), gio::Cancellable::NONE)
            .map_err(|e| e.message().to_owned())
    }
}

/// An in-memory keyring for tests, which can be told to fail.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryKeyring {
    secret: std::cell::RefCell<Option<String>>,
    /// Make `lookup` report an unreachable keyring.
    pub unreachable: std::cell::Cell<bool>,
    /// Make `store` fail.
    pub fail_store: std::cell::Cell<bool>,
    /// Make `clear` fail.
    pub fail_clear: std::cell::Cell<bool>,
}

#[cfg(test)]
impl MemoryKeyring {
    /// A keyring that already holds `secret`.
    pub fn with(secret: &str) -> Self {
        let keyring = Self::default();
        *keyring.secret.borrow_mut() = Some(secret.to_owned());
        keyring
    }

    /// What is stored right now.
    pub fn current(&self) -> Option<String> {
        self.secret.borrow().clone()
    }
}

#[cfg(test)]
impl Keyring for MemoryKeyring {
    fn lookup(&self) -> Result<Option<String>, String> {
        if self.unreachable.get() {
            return Err("the keyring is not available".to_owned());
        }
        Ok(self.current())
    }

    fn store(&self, secret: &str) -> Result<(), String> {
        if self.fail_store.get() {
            return Err("the keyring refused to store the key".to_owned());
        }
        *self.secret.borrow_mut() = Some(secret.to_owned());
        Ok(())
    }

    fn clear(&self) -> Result<(), String> {
        if self.fail_clear.get() {
            return Err("the keyring refused to remove the key".to_owned());
        }
        *self.secret.borrow_mut() = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyring(secret: &str) -> Keys {
        Keys::Keyring {
            secret: secret.into(),
        }
    }

    #[test]
    fn the_key_is_the_secret_alone_in_keyring_mode() {
        let keys = keyring("abc");
        assert_eq!(keys.database_key(), "abc");
        assert_eq!(keys.mode(), Mode::Keyring);
        assert_eq!(keys.secret(), Some("abc"));
    }

    #[test]
    fn with_a_passphrase_both_parts_matter() {
        let both = Keys::KeyringAndPassphrase {
            secret: "abc".into(),
            passphrase: "pw".into(),
        };
        assert_eq!(both.mode(), Mode::KeyringAndPassphrase);
        assert_eq!(both.database_key(), "abc\npw");

        let other_secret = Keys::KeyringAndPassphrase {
            secret: "abd".into(),
            passphrase: "pw".into(),
        };
        assert_ne!(both.database_key(), other_secret.database_key());
        assert_ne!(both.database_key(), keyring("abc").database_key());
    }

    #[test]
    fn a_passphrase_alone_is_the_key_and_has_no_secret() {
        let only = Keys::PassphraseOnly {
            passphrase: "correct horse battery".into(),
        };
        assert_eq!(only.mode(), Mode::PassphraseOnly);
        assert_eq!(only.database_key(), "correct horse battery");
        assert_eq!(only.secret(), None);
    }

    #[test]
    fn debug_output_never_shows_a_secret_or_passphrase() {
        let keys = Keys::KeyringAndPassphrase {
            secret: "topsecret".into(),
            passphrase: "hunter2hunter2".into(),
        };
        let shown = format!("{keys:?}");
        assert!(!shown.contains("topsecret") && !shown.contains("hunter2"));
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

    #[test]
    fn the_memory_keyring_behaves_like_a_keyring() {
        let k = MemoryKeyring::default();
        assert_eq!(k.lookup(), Ok(None));
        k.store("s1").expect("store");
        k.store("s2").expect("replace");
        assert_eq!(k.lookup(), Ok(Some("s2".to_owned())));
        k.clear().expect("clear");
        assert_eq!(k.lookup(), Ok(None));

        k.unreachable.set(true);
        assert!(k.lookup().is_err());
    }
}
