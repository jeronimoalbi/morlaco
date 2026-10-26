//! Changing how the database key is protected.
//!
//! Switching re-encrypts the whole database, which is the one moment a
//! crash could lose data. The order of the steps keeps the key recoverable
//! at every point: a new keyring secret is saved *before* the database is
//! re-encrypted with it, and an old one is removed only *after*.

use std::fmt;

use crate::keys::{self, Keyring, Keys, MIN_PASSPHRASE_ONLY_CHARS, Mode};

/// Where the user wants the key to end up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A secret in the keyring.
    Keyring,
    /// The keyring secret and this passphrase.
    KeyringAndPassphrase(String),
    /// This passphrase alone.
    PassphraseOnly(String),
}

/// Why a passphrase cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassphraseProblem {
    /// Nothing was entered.
    Empty,
    /// The two entries differ.
    Mismatch,
    /// Too short to be the only protection.
    TooShort,
}

impl fmt::Display for PassphraseProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PassphraseProblem::Empty => write!(f, "Enter a passphrase."),
            PassphraseProblem::Mismatch => write!(f, "The two passphrases don’t match."),
            PassphraseProblem::TooShort => {
                write!(f, "Use at least {MIN_PASSPHRASE_ONLY_CHARS} characters.")
            }
        }
    }
}

/// A passphrase that is not empty and was typed twice the same way.
pub fn check_passphrase(new: &str, confirm: &str) -> Result<(), PassphraseProblem> {
    if new.is_empty() {
        Err(PassphraseProblem::Empty)
    } else if new != confirm {
        Err(PassphraseProblem::Mismatch)
    } else {
        Ok(())
    }
}

/// [`check_passphrase`] plus the minimum length for a passphrase that is the
/// only protection. Counts characters, not bytes.
pub fn check_passphrase_only(new: &str, confirm: &str) -> Result<(), PassphraseProblem> {
    check_passphrase(new, confirm)?;
    if new.chars().count() < MIN_PASSPHRASE_ONLY_CHARS {
        return Err(PassphraseProblem::TooShort);
    }
    Ok(())
}

/// Turns what the user entered in the dialog into a [`Target`].
///
/// With the keyring on and both fields empty the target is the keyring alone.
pub fn target_from_inputs(
    keep_in_keyring: bool,
    new: &str,
    confirm: &str,
) -> Result<Target, PassphraseProblem> {
    if keep_in_keyring {
        if new.is_empty() && confirm.is_empty() {
            return Ok(Target::Keyring);
        }
        check_passphrase(new, confirm)?;
        Ok(Target::KeyringAndPassphrase(new.to_owned()))
    } else {
        check_passphrase_only(new, confirm)?;
        Ok(Target::PassphraseOnly(new.to_owned()))
    }
}

/// The result of a successful change.
#[derive(Debug)]
pub struct Changed {
    /// The keys that now open the database.
    pub keys: Keys,
    /// Something that went wrong after the change itself had worked, such as
    /// an old keyring entry that could not be removed.
    pub warning: Option<String>,
}

/// Moves the database to `target`.
///
/// `rekey` re-encrypts the database with the key it is given. If this
/// returns an error nothing has changed and the old keys still work.
pub fn change_protection(
    keyring: &dyn Keyring,
    current: &Keys,
    target: Target,
    rekey: impl FnOnce(&str) -> Result<(), String>,
) -> Result<Changed, String> {
    // A target that uses the keyring needs a secret: keep the current one, or
    // make one and save it *before* anything depends on it.
    let mut stored_new_secret = false;
    let mut secret = || -> Result<String, String> {
        if let Some(secret) = current.secret() {
            return Ok(secret.to_owned());
        }
        let secret = keys::generate_secret()?;
        keyring
            .store(&secret)
            .map_err(|e| format!("The keyring couldn’t keep the key: {e}"))?;
        stored_new_secret = true;
        Ok(secret)
    };

    let new_keys = match target {
        Target::Keyring => Keys::Keyring { secret: secret()? },
        Target::KeyringAndPassphrase(passphrase) => Keys::KeyringAndPassphrase {
            secret: secret()?,
            passphrase,
        },
        Target::PassphraseOnly(passphrase) => Keys::PassphraseOnly { passphrase },
    };

    if new_keys == *current {
        return Ok(Changed {
            keys: new_keys,
            warning: None,
        });
    }

    // The point of no return. If it fails the database is untouched, so a
    // secret saved just for it is not needed.
    if let Err(err) = rekey(&new_keys.database_key()) {
        if stored_new_secret {
            let _ = keyring.clear();
        }
        return Err(err);
    }

    // Without the keyring, an old entry is only clutter. If it cannot be
    // removed, startup still copes: it tries the passphrase on its own too.
    let warning = if new_keys.mode() == Mode::PassphraseOnly && current.secret().is_some() {
        keyring
            .clear()
            .err()
            .map(|e| format!("The old key could not be removed from the keyring: {e}"))
    } else {
        None
    };

    Ok(Changed {
        keys: new_keys,
        warning,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::Path;

    use morlaco::Error;
    use morlaco::chain::{Chain, WalletId};
    use morlaco::db::Database;

    use super::*;
    use crate::keys::MemoryKeyring;
    use crate::testutil::TempDir;

    const PASSPHRASE: &str = "a long enough passphrase";

    fn rekeyed(db: &Database) -> impl FnOnce(&str) -> Result<(), String> + '_ {
        |key| db.rekey(key).map_err(|e| e.to_string())
    }

    /// Opens with the given keys and checks the data survived.
    fn assert_opens(path: &Path, keys: &Keys) {
        let db = Database::open(path, &keys.database_key()).expect("opens with the new keys");
        assert_eq!(db.wallets().expect("wallets")[0].label, "Kept");
    }

    fn assert_rejects(path: &Path, keys: &Keys) {
        assert!(matches!(
            Database::open(path, &keys.database_key()),
            Err(Error::WrongKey)
        ));
    }

    fn setup(name: &str, secret: &str) -> (TempDir, std::path::PathBuf, Database, Keys) {
        let dir = TempDir::new(name);
        let path = dir.path().join("morlaco.db");
        let keys = Keys::Keyring {
            secret: secret.into(),
        };
        let db = Database::open(&path, &keys.database_key()).expect("creates");
        db.add_wallet(&WalletId::new(Chain::Bitcoin, "bc1qa"), "Kept")
            .expect("add");
        (dir, path, db, keys)
    }

    #[test]
    fn every_mode_can_reach_every_other_and_the_data_follows() {
        let (_dir, path, db, mut keys) = setup("modes", "s1");
        let keyring = MemoryKeyring::with("s1");

        let steps = [
            Target::KeyringAndPassphrase(PASSPHRASE.into()),
            Target::PassphraseOnly(PASSPHRASE.into()),
            Target::Keyring,
            Target::PassphraseOnly("another long passphrase".into()),
            Target::KeyringAndPassphrase("pw".into()),
            Target::Keyring,
        ];
        for target in steps {
            let before = keys.clone();
            let changed =
                change_protection(&keyring, &keys, target.clone(), rekeyed(&db)).expect("changes");
            keys = changed.keys;
            assert!(
                changed.warning.is_none(),
                "{target:?}: {:?}",
                changed.warning
            );

            assert_opens(&path, &keys);
            if before != keys {
                assert_rejects(&path, &before);
            }
            // The keyring holds a secret exactly when the mode uses one.
            assert_eq!(keyring.current().as_deref(), keys.secret(), "{target:?}");
        }
    }

    #[test]
    fn a_passphrase_alone_opens_the_file_without_any_keyring() {
        let (_dir, path, db, keys) = setup("alone", "s1");
        let keyring = MemoryKeyring::with("s1");
        change_protection(
            &keyring,
            &keys,
            Target::PassphraseOnly(PASSPHRASE.into()),
            rekeyed(&db),
        )
        .expect("changes");

        // Just the plain passphrase, as a standard SQLCipher tool would use it.
        let reopened = Database::open(&path, PASSPHRASE).expect("opens with the passphrase");
        assert_eq!(reopened.wallets().expect("wallets").len(), 1);
        assert_eq!(keyring.current(), None);
    }

    #[test]
    fn if_the_keyring_cannot_keep_the_new_key_nothing_changes() {
        let (_dir, path, db, _) = setup("storefail", "unused");
        // Start from passphrase-only data.
        let alone = Keys::PassphraseOnly {
            passphrase: PASSPHRASE.into(),
        };
        db.rekey(&alone.database_key()).expect("rekey");

        let keyring = MemoryKeyring::default();
        keyring.fail_store.set(true);
        let rekey_called = Cell::new(false);
        let result = change_protection(&keyring, &alone, Target::Keyring, |key| {
            rekey_called.set(true);
            db.rekey(key).map_err(|e| e.to_string())
        });

        assert!(result.is_err());
        assert!(
            !rekey_called.get(),
            "must not re-encrypt without a saved key"
        );
        assert_opens(&path, &alone);
    }

    #[test]
    fn if_re_encryption_fails_the_new_secret_is_removed_again() {
        let alone = Keys::PassphraseOnly {
            passphrase: PASSPHRASE.into(),
        };
        let keyring = MemoryKeyring::default();

        let result = change_protection(&keyring, &alone, Target::Keyring, |_| {
            Err("disk full".to_owned())
        });

        assert_eq!(result.expect_err("fails"), "disk full");
        assert_eq!(keyring.current(), None, "no stray key left in the keyring");
    }

    #[test]
    fn if_the_old_entry_cannot_be_removed_it_is_only_a_warning() {
        let (_dir, path, db, keys) = setup("clearfail", "s1");
        let keyring = MemoryKeyring::with("s1");
        keyring.fail_clear.set(true);

        let changed = change_protection(
            &keyring,
            &keys,
            Target::PassphraseOnly(PASSPHRASE.into()),
            rekeyed(&db),
        )
        .expect("the change itself worked");

        assert!(changed.warning.is_some());
        assert_opens(&path, &changed.keys);
        assert_eq!(
            keyring.current().as_deref(),
            Some("s1"),
            "stale entry remains"
        );
    }

    #[test]
    fn asking_for_what_is_already_in_place_does_nothing() {
        let keys = Keys::Keyring {
            secret: "s1".into(),
        };
        let keyring = MemoryKeyring::with("s1");
        let changed = change_protection(&keyring, &keys, Target::Keyring, |_| {
            panic!("nothing should be re-encrypted")
        })
        .expect("no-op");
        assert_eq!(changed.keys, keys);
    }

    #[test]
    fn passphrase_rules() {
        assert_eq!(check_passphrase("", ""), Err(PassphraseProblem::Empty));
        assert_eq!(
            check_passphrase("abc", "abd"),
            Err(PassphraseProblem::Mismatch)
        );
        assert_eq!(check_passphrase("abc", "abc"), Ok(()));

        // Exactly the minimum passes, one short fails.
        let ok = "x".repeat(MIN_PASSPHRASE_ONLY_CHARS);
        let short = "x".repeat(MIN_PASSPHRASE_ONLY_CHARS - 1);
        assert_eq!(check_passphrase_only(&ok, &ok), Ok(()));
        assert_eq!(
            check_passphrase_only(&short, &short),
            Err(PassphraseProblem::TooShort)
        );
        // Characters count, not bytes: eleven two-byte characters are still short.
        let accents = "é".repeat(MIN_PASSPHRASE_ONLY_CHARS - 1);
        assert!(accents.len() >= MIN_PASSPHRASE_ONLY_CHARS);
        assert_eq!(
            check_passphrase_only(&accents, &accents),
            Err(PassphraseProblem::TooShort)
        );
    }

    #[test]
    fn dialog_inputs_become_the_right_target() {
        let long = "a long enough passphrase";
        assert_eq!(target_from_inputs(true, "", ""), Ok(Target::Keyring));
        assert_eq!(
            target_from_inputs(true, "pw", "pw"),
            Ok(Target::KeyringAndPassphrase("pw".into()))
        );
        assert_eq!(
            target_from_inputs(true, "pw", "other"),
            Err(PassphraseProblem::Mismatch)
        );
        assert_eq!(
            target_from_inputs(true, "", "x"),
            Err(PassphraseProblem::Empty)
        );
        assert_eq!(
            target_from_inputs(false, "short", "short"),
            Err(PassphraseProblem::TooShort)
        );
        assert_eq!(
            target_from_inputs(false, "", ""),
            Err(PassphraseProblem::Empty)
        );
        assert_eq!(
            target_from_inputs(false, long, long),
            Ok(Target::PassphraseOnly(long.into()))
        );
    }
}
