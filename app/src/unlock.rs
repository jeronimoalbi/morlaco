//! Opening the encrypted database at startup.
//!
//! Normally this finishes at once: the key comes from the keyring. A page is
//! shown, in the app's window, only when the user must act: a passphrase is
//! needed, a passphrase has to be chosen because there is no keyring, or
//! something failed.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use morlaco_core::Error;
use morlaco_core::db::Database;
use morlaco_core::paths;

use crate::encryption::{self, PassphraseProblem};
use crate::gate::{self, GatePage, PromptLayout, SetupLayout, boxed_fields};
use crate::keys::{self, Keyring, Keys, MIN_PASSPHRASE_ONLY_CHARS};
use crate::shell::Shell;
use crate::system_keyring::PlatformKeyring;

/// An open database and what happened while opening it.
pub struct Unlocked {
    /// The database.
    pub db: Database,
    /// What opened it.
    pub keys: Keys,
    /// Where an older database was moved to, if there was one that this
    /// version cannot read. The new database then starts empty.
    pub set_aside: Option<PathBuf>,
    /// Whether a new keyring secret was just made, so the user should be
    /// encouraged to save their recovery key.
    pub created_new_secret: bool,
    /// Something to tell the user, such as what happened when a recovered key
    /// was saved in the keyring again.
    pub notice: Option<String>,
}

/// Called once the database is open.
pub type Ready = Rc<dyn Fn(Unlocked)>;

/// Opens the database. A file in the layout of version 1 cannot be read, so
/// it is renamed out of the way and a fresh database takes its place.
/// Opens the file with a passphrase just chosen, encrypting it first when it
/// was a plain one.
fn open_new(
    file: FileState,
    path: &Path,
    passphrase: &str,
) -> Result<(Database, Option<PathBuf>), Error> {
    if file == FileState::Plain {
        Database::encrypt_plain_file(path, passphrase)?;
    }
    open_database(path, passphrase)
}

fn open_database(path: &Path, key: &str) -> Result<(Database, Option<PathBuf>), Error> {
    match Database::open(path, key) {
        Err(Error::OldFormat) => {
            let aside = Database::set_aside_old_format(path)?;
            Ok((Database::open(path, key)?, Some(aside)))
        }
        other => other.map(|db| (db, None)),
    }
}

// --- Deciding what to do ---------------------------------------------------

/// What is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileState {
    /// No database yet.
    Missing,
    /// A database an earlier version wrote without encryption.
    Plain,
    /// An encrypted database.
    Encrypted,
}

fn file_state(path: &Path) -> Result<FileState, Error> {
    if !path.exists() {
        Ok(FileState::Missing)
    } else if Database::is_plain_file(path)? {
        Ok(FileState::Plain)
    } else {
        Ok(FileState::Encrypted)
    }
}

/// What startup should do next.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// The keyring has the secret: open with it.
    OpenWithSecret(String),
    /// New install (or an unencrypted file to protect) and a working keyring:
    /// make a secret, save it, then open.
    CreateSecret,
    /// An encrypted file but no secret to open it with: ask for a passphrase.
    /// It is either passphrase-only data or the keyring entry is gone.
    Prompt {
        /// Whether the keyring answered, so "try again" is pointless.
        keyring_reachable: bool,
    },
    /// New install and no keyring: the user chooses a passphrase.
    SetupPassphrase,
}

/// Decides what to do from what the keyring said and what is on disk.
fn plan(keyring: &Result<Option<String>, String>, file: FileState) -> Next {
    match (keyring, file) {
        (Ok(Some(secret)), _) => Next::OpenWithSecret(secret.clone()),
        (Ok(None), FileState::Missing | FileState::Plain) => Next::CreateSecret,
        (Ok(None), FileState::Encrypted) => Next::Prompt {
            keyring_reachable: true,
        },
        (Err(_), FileState::Missing | FileState::Plain) => Next::SetupPassphrase,
        (Err(_), FileState::Encrypted) => Next::Prompt {
            keyring_reachable: false,
        },
    }
}

/// The keys to try for a typed passphrase, most likely first.
///
/// With a secret the data is most likely protected by both. A passphrase on
/// its own is tried too, because after switching to passphrase-only mode an
/// old keyring entry may be left behind.
fn candidates(secret: Option<&str>, passphrase: &str) -> Vec<Keys> {
    let mut keys = Vec::new();
    if let Some(secret) = secret {
        keys.push(Keys::KeyringAndPassphrase {
            secret: secret.to_owned(),
            passphrase: passphrase.to_owned(),
        });
    }
    keys.push(Keys::PassphraseOnly {
        passphrase: passphrase.to_owned(),
    });
    keys
}

/// Opens the database with the first of `candidates` that fits. A wrong key
/// moves on to the next; any other failure stops.
fn open_with_candidates(
    path: &Path,
    candidates: Vec<Keys>,
) -> Result<(Database, Keys, Option<PathBuf>), Error> {
    for keys in candidates {
        match open_database(path, &keys.database_key()) {
            Ok((db, set_aside)) => return Ok((db, keys, set_aside)),
            Err(Error::WrongKey) => continue,
            Err(other) => return Err(other),
        }
    }
    Err(Error::WrongKey)
}

/// Why a recovery key could not be used.
#[derive(Debug, PartialEq, Eq)]
enum RecoveryProblem {
    /// It is not 64 hexadecimal digits.
    Malformed,
    /// It does not open the data, alone or with the passphrase that was given.
    NoMatch {
        /// Whether a passphrase was part of the attempt.
        tried_passphrase: bool,
    },
    /// Something else went wrong while opening the file.
    Other(String),
}

impl std::fmt::Display for RecoveryProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecoveryProblem::Malformed => write!(
                f,
                "That doesn’t look like a recovery key. It has 64 letters and digits \
                 (0–9 and a–f), usually in groups separated by dashes."
            ),
            RecoveryProblem::NoMatch {
                tried_passphrase: false,
            } => write!(
                f,
                "That key doesn’t open your data. If you also set a passphrase, enter \
                 it too."
            ),
            RecoveryProblem::NoMatch {
                tried_passphrase: true,
            } => write!(f, "The recovery key or the passphrase is wrong."),
            RecoveryProblem::Other(message) => write!(f, "{message}"),
        }
    }
}

/// Opens the data with a recovery key, and with the passphrase too if one was
/// given. The recovery key is the keyring secret, so the result is a keyring
/// mode, with or without a passphrase.
fn recover(
    path: &Path,
    recovery_key: &str,
    passphrase: &str,
) -> Result<(Database, Keys, Option<PathBuf>), RecoveryProblem> {
    let secret = keys::parse_recovery_key(recovery_key).ok_or(RecoveryProblem::Malformed)?;
    let tried_passphrase = !passphrase.is_empty();

    let mut candidates = vec![Keys::Keyring {
        secret: secret.clone(),
    }];
    if tried_passphrase {
        candidates.push(Keys::KeyringAndPassphrase {
            secret,
            passphrase: passphrase.to_owned(),
        });
    }
    match open_with_candidates(path, candidates) {
        Ok(opened) => Ok(opened),
        Err(Error::WrongKey) => Err(RecoveryProblem::NoMatch { tried_passphrase }),
        Err(other) => Err(RecoveryProblem::Other(other.to_string())),
    }
}

/// Saves a recovered secret in the keyring again, and says what happened.
/// `None` when the keys have no secret to save.
fn restore_secret(keyring: &dyn Keyring, keys: &Keys) -> Option<String> {
    let secret = keys.secret()?;
    Some(match keyring.store(secret) {
        Ok(()) => "Your key was saved in the keyring again.".to_owned(),
        Err(err) => format!(
            "Your data is open, but the key couldn’t be saved in the keyring ({err}). \
             You’ll need your recovery key again next time."
        ),
    })
}

// --- Flow ------------------------------------------------------------------

/// Opens the database and calls `ready`, possibly after the user has
/// entered a passphrase.
pub fn start(shell: &Rc<Shell>, ready: Ready) {
    let Some(path) = paths::data_file() else {
        return show_error(
            shell,
            "Couldn’t Open Your Data",
            "No data directory is available.",
        );
    };
    let file = match file_state(&path) {
        Ok(file) => file,
        Err(err) => return show_error(shell, "Couldn’t Open Your Data", &err.to_string()),
    };

    match plan(&PlatformKeyring.lookup(), file) {
        Next::OpenWithSecret(secret) => open_with_secret(shell, path, secret, false, ready),
        Next::CreateSecret => create_secret(shell, path, file, ready),
        Next::Prompt { keyring_reachable } => {
            show_passphrase_prompt(shell, path, None, keyring_reachable, ready);
        }
        Next::SetupPassphrase => show_setup(shell, path, file, None, ready),
    }
}

/// Makes a secret for a new install. If the keyring will not keep it the
/// user can still choose a passphrase instead.
fn create_secret(shell: &Rc<Shell>, path: PathBuf, file: FileState, ready: Ready) {
    let secret = match keys::generate_secret() {
        Ok(secret) => secret,
        Err(message) => return show_error(shell, "Couldn’t Create the Key", &message),
    };
    // Save the key before using it, so data is never encrypted with a key
    // that was not kept.
    if let Err(message) = PlatformKeyring.store(&secret) {
        return show_setup(shell, path, file, Some(message), ready);
    }
    open_with_secret(shell, path, secret, true, ready);
}

fn open_with_secret(
    shell: &Rc<Shell>,
    path: PathBuf,
    secret: String,
    created_new_secret: bool,
    ready: Ready,
) {
    // A file from an earlier version has no encryption yet.
    if let Err(err) = Database::encrypt_plain_file(&path, &secret) {
        return show_error(shell, "Couldn’t Encrypt Your Data", &err.to_string());
    }
    match open_database(&path, &secret) {
        Ok((db, set_aside)) => ready(Unlocked {
            db,
            keys: Keys::Keyring { secret },
            set_aside,
            created_new_secret,
            notice: None,
        }),
        // The secret alone does not open it, so a passphrase was added.
        Err(Error::WrongKey) => show_passphrase_prompt(shell, path, Some(secret), true, ready),
        Err(err) => show_error(shell, "Couldn’t Open Your Data", &err.to_string()),
    }
}

// --- Windows ---------------------------------------------------------------

/// Shows a message instead of the app.
pub fn show_error(shell: &Rc<Shell>, title: &str, message: &str) {
    shell.show_gate(&gate::error_layout(title, message));
}

/// The setup page for a machine without a usable keyring: the user chooses
/// the passphrase that will be the only protection.
fn show_setup(
    shell: &Rc<Shell>,
    path: PathBuf,
    file: FileState,
    reason: Option<String>,
    ready: Ready,
) {
    let intro = match &reason {
        Some(reason) => format!("The keyring couldn’t be used ({reason}). "),
        None => "No keyring was found. ".to_owned(),
    };
    let SetupLayout {
        page,
        new,
        confirm,
        problem,
        create,
    } = gate::setup_layout(&format!(
        "{intro}Your passphrase will be the only thing protecting your data, so use \
         at least {MIN_PASSPHRASE_ONLY_CHARS} characters. If you forget it, your data \
         can’t be recovered."
    ));
    shell.show_gate(&page);
    let window = shell.window.clone();

    // Saying what is wrong only once the user has typed something.
    let validate = {
        let (new, confirm, problem, create) = (
            new.clone(),
            confirm.clone(),
            problem.clone(),
            create.clone(),
        );
        Rc::new(move || {
            let checked = encryption::check_passphrase_only(&new.text(), &confirm.text());
            create.set_sensitive(checked.is_ok());
            let hint = match checked {
                Err(PassphraseProblem::Empty) => String::new(),
                Err(other) => other.to_string(),
                Ok(()) => String::new(),
            };
            problem.set_label(&hint);
        })
    };
    let on_new = validate.clone();
    new.connect_changed(move |_| on_new());
    let on_confirm = validate.clone();
    confirm.connect_changed(move |_| on_confirm());

    let attempt = {
        let (new, problem) = (new.clone(), problem.clone());
        Rc::new(move || {
            let passphrase = new.text().to_string();
            let result = open_new(file, &path, &passphrase);
            let (db, set_aside) = match result {
                Ok(opened) => opened,
                Err(err) => {
                    problem.set_label(&err.to_string());
                    return;
                }
            };
            ready(Unlocked {
                db,
                keys: Keys::PassphraseOnly { passphrase },
                set_aside,
                created_new_secret: false,
                notice: None,
            });
        })
    };
    // Enter in the last field does what the button does, once it is enabled.
    let (on_create, create_button) = (attempt.clone(), create.clone());
    confirm.connect_entry_activated(move |_| {
        if create_button.is_sensitive() {
            on_create();
        }
    });
    create.connect_clicked(move |_| attempt());

    window.present();
    new.grab_focus();
}

/// Asks for the passphrase. With a `secret` the keyring had a key that did
/// not open the file alone; without one the file is either passphrase-only or
/// its keyring entry is missing.
///
/// Try Again, Use Recovery Key and Start Over are window actions, reached
/// from the "⋮" menu next to Unlock.
fn show_passphrase_prompt(
    shell: &Rc<Shell>,
    path: PathBuf,
    secret: Option<String>,
    keyring_reachable: bool,
    ready: Ready,
) {
    let description = match (&secret, keyring_reachable) {
        (Some(_), _) => "Enter your passphrase to open your data.",
        (None, true) => {
            "Enter your passphrase to open your data. If you never set one, the key \
             for your data is missing from the keyring. You can use your recovery key \
             from the ⋮ menu."
        }
        (None, false) => {
            "The keyring can’t be reached. If you use a passphrase, enter it. \
             Otherwise unlock your keyring and choose Try Again from the ⋮ menu."
        }
    };
    let PromptLayout {
        page,
        entry,
        unlock,
    } = gate::prompt_layout(description, !keyring_reachable);
    let page = Rc::new(page);
    shell.show_gate(&page);
    let window = shell.window.clone();

    let attempt = {
        let (entry, page) = (entry.clone(), page.clone());
        let path = path.clone();
        let ready = ready.clone();
        Rc::new(move || {
            let passphrase = entry.text().to_string();
            match open_with_candidates(&path, candidates(secret.as_deref(), &passphrase)) {
                Ok((db, keys, set_aside)) => {
                    ready(Unlocked {
                        db,
                        keys,
                        set_aside,
                        created_new_secret: false,
                        notice: None,
                    });
                }
                Err(Error::WrongKey) => {
                    entry.add_css_class("error");
                    entry.set_text("");
                    page.set_description("That passphrase is wrong. Try again.");
                }
                Err(err) => page.set_description(&err.to_string()),
            }
        })
    };
    let on_activate = attempt.clone();
    entry.connect_entry_activated(move |_| on_activate());
    unlock.connect_clicked(move |_| attempt());
    entry.connect_changed(|entry| entry.remove_css_class("error"));

    // The actions behind the "⋮" menu. Try Again is in the menu only when the
    // keyring could not be reached, so it is registered only then.
    if !keyring_reachable {
        let (retry_shell, retry_ready) = (shell.clone(), ready.clone());
        let try_again = gio::SimpleAction::new(gate::ACTION_TRY_AGAIN, None);
        try_again.connect_activate(move |_, _| start(&retry_shell, retry_ready.clone()));
        window.add_action(&try_again);
    }

    let (recover_path, recover_ready) = (path.clone(), ready.clone());
    let (recover_window, recover_page) = (window.clone(), page.clone());
    let use_recovery_key = gio::SimpleAction::new(gate::ACTION_USE_RECOVERY_KEY, None);
    use_recovery_key.connect_activate(move |_, _| {
        show_recovery_dialog(
            &recover_window,
            &recover_page,
            recover_path.clone(),
            recover_ready.clone(),
        );
    });
    window.add_action(&use_recovery_key);

    let (over_shell, over_window) = (shell.clone(), window.clone());
    let start_over = gio::SimpleAction::new(gate::ACTION_START_OVER, None);
    start_over.connect_activate(move |_, _| {
        confirm_start_over(&over_shell, path.clone(), ready.clone(), &over_window);
    });
    window.add_action(&start_over);

    window.present();
    entry.grab_focus();
}

/// Asks for a recovery key (and the passphrase, if there is one) and opens the
/// data with it. A problem is shown on the unlock page.
fn show_recovery_dialog(
    parent: &adw::ApplicationWindow,
    page: &Rc<GatePage>,
    path: PathBuf,
    ready: Ready,
) {
    let dialog = adw::AlertDialog::new(
        Some("Use Recovery Key"),
        Some("Enter the recovery key you saved. If you also set a passphrase, enter that too."),
    );
    dialog.add_responses(&[("cancel", "_Cancel"), ("open", "_Open")]);
    dialog.set_response_appearance("open", adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled("open", false);
    dialog.set_default_response(Some("open"));
    dialog.set_close_response("cancel");

    let key = adw::EntryRow::builder().title("Recovery key").build();
    let passphrase = adw::PasswordEntryRow::builder()
        .title("Passphrase (if you set one)")
        .build();
    let fields = boxed_fields();
    fields.append(&key);
    fields.append(&passphrase);
    dialog.set_extra_child(Some(&fields));

    let enabler = dialog.clone();
    key.connect_changed(move |entry| {
        enabler.set_response_enabled("open", !entry.text().trim().is_empty());
    });

    let page = page.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "open" {
            return;
        }
        match recover(&path, &key.text(), &passphrase.text()) {
            Ok((db, keys, set_aside)) => {
                let notice = restore_secret(&PlatformKeyring, &keys);
                ready(Unlocked {
                    db,
                    keys,
                    set_aside,
                    created_new_secret: false,
                    notice,
                });
            }
            Err(problem) => page.set_description(&problem.to_string()),
        }
    });
    dialog.present(Some(parent));
}

/// Asks whether to delete the saved data, and if so does it and starts again.
fn confirm_start_over(
    shell: &Rc<Shell>,
    path: PathBuf,
    ready: Ready,
    parent: &adw::ApplicationWindow,
) {
    let dialog = adw::AlertDialog::new(
        Some("Delete the Saved Data?"),
        Some(
            "Your tracked wallets and settings will be deleted and Morlaco will \
             start empty. This can’t be undone.",
        ),
    );
    dialog.add_responses(&[("cancel", "_Cancel"), ("delete", "_Delete")]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_close_response("cancel");

    let shell = shell.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "delete" {
            return;
        }
        match delete_database_files(&path) {
            Ok(()) => start(&shell, ready.clone()),
            Err(message) => glib::g_warning!("morlaco", "could not delete the data: {message}"),
        }
    });
    dialog.present(Some(parent));
}

/// Deletes the database file, its journal and any older database that was
/// set aside.
pub fn delete_database_files(path: &Path) -> Result<(), String> {
    for file in [
        path.to_owned(),
        path.with_extension("db-journal"),
        Database::set_aside_path(path),
    ] {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("{}: {err}", file.display())),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use morlaco_core::chain::{Chain, WalletId};

    use super::*;
    use crate::keys::{MemoryKeyring, Mode};
    use crate::testutil::TempDir;

    const SECRET: &str = "0123abcd";
    const PASSPHRASE: &str = "a long enough passphrase";

    fn found() -> Result<Option<String>, String> {
        Ok(Some(SECRET.to_owned()))
    }

    fn empty() -> Result<Option<String>, String> {
        Ok(None)
    }

    fn unreachable_keyring() -> Result<Option<String>, String> {
        Err("no keyring".to_owned())
    }

    #[test]
    fn a_secret_in_the_keyring_is_always_tried_first() {
        for file in [FileState::Missing, FileState::Plain, FileState::Encrypted] {
            assert_eq!(
                plan(&found(), file),
                Next::OpenWithSecret(SECRET.to_owned()),
                "{file:?}"
            );
        }
    }

    #[test]
    fn a_new_install_with_a_working_keyring_creates_a_secret() {
        assert_eq!(plan(&empty(), FileState::Missing), Next::CreateSecret);
        // An unencrypted file from an earlier version gets protected too.
        assert_eq!(plan(&empty(), FileState::Plain), Next::CreateSecret);
    }

    #[test]
    fn encrypted_data_without_a_secret_asks_for_the_passphrase() {
        assert_eq!(
            plan(&empty(), FileState::Encrypted),
            Next::Prompt {
                keyring_reachable: true
            }
        );
        assert_eq!(
            plan(&unreachable_keyring(), FileState::Encrypted),
            Next::Prompt {
                keyring_reachable: false
            }
        );
    }

    #[test]
    fn a_new_install_without_a_keyring_sets_up_a_passphrase() {
        assert_eq!(
            plan(&unreachable_keyring(), FileState::Missing),
            Next::SetupPassphrase
        );
        assert_eq!(
            plan(&unreachable_keyring(), FileState::Plain),
            Next::SetupPassphrase
        );
    }

    #[test]
    fn the_file_state_is_read_from_disk() {
        let dir = TempDir::new("filestate");
        let path = dir.path().join("morlaco.db");
        assert_eq!(file_state(&path).expect("state"), FileState::Missing);

        Database::open(&path, "key").expect("creates");
        assert_eq!(file_state(&path).expect("state"), FileState::Encrypted);
    }

    #[test]
    fn candidates_try_both_keys_when_a_secret_exists() {
        let with = candidates(Some(SECRET), "pw");
        assert_eq!(with.len(), 2);
        assert_eq!(with[0].mode(), Mode::KeyringAndPassphrase);
        assert_eq!(with[1].mode(), Mode::PassphraseOnly);

        let without = candidates(None, "pw");
        assert_eq!(without.len(), 1);
        assert_eq!(without[0].mode(), Mode::PassphraseOnly);
    }

    /// A database protected with `keys` that holds one wallet.
    fn protected_file(name: &str, keys: &Keys) -> (TempDir, PathBuf) {
        let dir = TempDir::new(name);
        let path = dir.path().join("morlaco.db");
        Database::open(&path, &keys.database_key())
            .expect("creates")
            .add_wallet(&WalletId::new(Chain::Bitcoin, "bc1qa"), "Kept")
            .expect("add");
        (dir, path)
    }

    #[test]
    fn a_keyring_plus_passphrase_file_opens_with_both() {
        let keys = Keys::KeyringAndPassphrase {
            secret: SECRET.into(),
            passphrase: PASSPHRASE.into(),
        };
        let (_dir, path) = protected_file("kp", &keys);

        let (db, matched, _) =
            open_with_candidates(&path, candidates(Some(SECRET), PASSPHRASE)).expect("opens");
        assert_eq!(matched, keys);
        assert_eq!(db.wallets().expect("wallets")[0].label, "Kept");
    }

    #[test]
    fn a_passphrase_only_file_opens_with_no_secret_at_all() {
        let keys = Keys::PassphraseOnly {
            passphrase: PASSPHRASE.into(),
        };
        let (_dir, path) = protected_file("alone", &keys);

        let (_, matched, _) =
            open_with_candidates(&path, candidates(None, PASSPHRASE)).expect("opens");
        assert_eq!(matched.mode(), Mode::PassphraseOnly);
    }

    #[test]
    fn a_leftover_keyring_entry_does_not_stop_a_passphrase_only_file() {
        // After switching to passphrase-only the old entry could not be
        // removed: the keyring still offers a secret that no longer fits.
        let keys = Keys::PassphraseOnly {
            passphrase: PASSPHRASE.into(),
        };
        let (_dir, path) = protected_file("stale", &keys);

        let (_, matched, _) =
            open_with_candidates(&path, candidates(Some("stale-secret"), PASSPHRASE))
                .expect("falls back to the passphrase alone");
        assert_eq!(matched.mode(), Mode::PassphraseOnly);
    }

    fn recovery_key() -> String {
        keys::format_recovery_key(&SECRET.repeat(8))
    }

    /// The secret the recovery tests use: `SECRET` repeated to 64 digits.
    fn full_secret() -> String {
        SECRET.repeat(8)
    }

    #[test]
    fn the_recovery_key_opens_keyring_protected_data() {
        let keys = Keys::Keyring {
            secret: full_secret(),
        };
        let (_dir, path) = protected_file("rk", &keys);

        let (db, matched, _) = recover(&path, &recovery_key(), "").expect("opens");
        assert_eq!(matched, keys);
        assert_eq!(db.wallets().expect("wallets")[0].label, "Kept");
    }

    #[test]
    fn data_with_a_passphrase_needs_it_as_well_as_the_recovery_key() {
        let keys = Keys::KeyringAndPassphrase {
            secret: full_secret(),
            passphrase: PASSPHRASE.into(),
        };
        let (_dir, path) = protected_file("rkpw", &keys);

        // The key alone is not enough, and the message says a passphrase may be needed.
        assert_eq!(
            recover(&path, &recovery_key(), "").err(),
            Some(RecoveryProblem::NoMatch {
                tried_passphrase: false
            })
        );
        assert_eq!(
            recover(&path, &recovery_key(), "not it").err(),
            Some(RecoveryProblem::NoMatch {
                tried_passphrase: true
            })
        );
        let (_, matched, _) = recover(&path, &recovery_key(), PASSPHRASE).expect("opens");
        assert_eq!(matched, keys);
    }

    #[test]
    fn a_wrong_recovery_key_does_not_open_anything() {
        let keys = Keys::Keyring {
            secret: full_secret(),
        };
        let (_dir, path) = protected_file("rkwrong", &keys);
        let other = keys::format_recovery_key(&"f".repeat(64));

        assert_eq!(
            recover(&path, &other, "").err(),
            Some(RecoveryProblem::NoMatch {
                tried_passphrase: false
            })
        );
    }

    #[test]
    fn a_malformed_recovery_key_is_called_out_before_trying_the_file() {
        let keys = Keys::Keyring {
            secret: full_secret(),
        };
        let (_dir, path) = protected_file("rkbad", &keys);
        assert_eq!(
            recover(&path, "definitely not a key", "").err(),
            Some(RecoveryProblem::Malformed)
        );
    }

    #[test]
    fn passphrase_only_data_has_no_recovery_key() {
        let keys = Keys::PassphraseOnly {
            passphrase: PASSPHRASE.into(),
        };
        let (_dir, path) = protected_file("rkalone", &keys);
        assert!(recover(&path, &recovery_key(), "").is_err());
    }

    #[test]
    fn a_recovered_secret_goes_back_into_the_keyring() {
        let keyring = MemoryKeyring::default();
        let keys = Keys::Keyring {
            secret: full_secret(),
        };
        let notice = restore_secret(&keyring, &keys).expect("a notice");
        assert!(notice.contains("saved in the keyring again"));
        assert_eq!(keyring.current(), Some(full_secret()));
    }

    #[test]
    fn if_the_keyring_will_not_take_it_back_the_user_is_told() {
        let keyring = MemoryKeyring::default();
        keyring.fail_store.set(true);
        let keys = Keys::Keyring {
            secret: full_secret(),
        };
        let notice = restore_secret(&keyring, &keys).expect("a notice");
        assert!(notice.contains("recovery key again next time"));
        assert_eq!(keyring.current(), None);
    }

    #[test]
    fn data_protected_by_a_passphrase_alone_has_nothing_to_restore() {
        let keyring = MemoryKeyring::default();
        let keys = Keys::PassphraseOnly {
            passphrase: PASSPHRASE.into(),
        };
        assert_eq!(restore_secret(&keyring, &keys), None);
    }

    #[test]
    fn a_wrong_passphrase_is_reported_as_a_wrong_key() {
        let keys = Keys::PassphraseOnly {
            passphrase: PASSPHRASE.into(),
        };
        let (_dir, path) = protected_file("wrong", &keys);

        assert!(matches!(
            open_with_candidates(&path, candidates(Some(SECRET), "not the passphrase")),
            Err(Error::WrongKey)
        ));
    }
}
