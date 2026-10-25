//! Opening the encrypted database at startup.
//!
//! Normally this finishes at once: the key comes from the keyring. A window
//! is shown only when the user must act: a passphrase is needed, a passphrase
//! has to be chosen because there is no keyring, or something failed.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use morlaco_core::Error;
use morlaco_core::db::Database;
use morlaco_core::paths;

use crate::encryption::{self, PassphraseProblem};
use crate::keys::{self, Keyring, Keys, MIN_PASSPHRASE_ONLY_CHARS, SecretService};

/// An open database and what happened while opening it.
pub struct Unlocked {
    /// The database.
    pub db: Database,
    /// What opened it.
    pub keys: Keys,
    /// Where an older database was moved to, if there was one that this
    /// version cannot read. The new database then starts empty.
    pub set_aside: Option<PathBuf>,
}

/// Called once the database is open.
pub type Ready = Rc<dyn Fn(Unlocked)>;

/// Opens the database. A file in the layout of version 1 cannot be read, so
/// it is renamed out of the way and a fresh database takes its place.
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

// --- Flow ------------------------------------------------------------------

/// Opens the database and calls `ready`, possibly after the user has
/// entered a passphrase.
pub fn start(app: &adw::Application, ready: Ready) {
    let Some(path) = paths::data_file() else {
        return show_error(
            app,
            "Couldn’t Open Your Data",
            "No data directory is available.",
        );
    };
    let file = match file_state(&path) {
        Ok(file) => file,
        Err(err) => return show_error(app, "Couldn’t Open Your Data", &err.to_string()),
    };

    match plan(&SecretService.lookup(), file) {
        Next::OpenWithSecret(secret) => open_with_secret(app, path, secret, ready),
        Next::CreateSecret => create_secret(app, path, file, ready),
        Next::Prompt { keyring_reachable } => {
            show_passphrase_prompt(app, path, None, keyring_reachable, ready);
        }
        Next::SetupPassphrase => show_setup(app, path, file, None, ready),
    }
}

/// Makes a secret for a new install. If the keyring will not keep it the
/// user can still choose a passphrase instead.
fn create_secret(app: &adw::Application, path: PathBuf, file: FileState, ready: Ready) {
    let secret = match keys::generate_secret() {
        Ok(secret) => secret,
        Err(message) => return show_error(app, "Couldn’t Create the Key", &message),
    };
    // Save the key before using it, so data is never encrypted with a key
    // that was not kept.
    if let Err(message) = SecretService.store(&secret) {
        return show_setup(app, path, file, Some(message), ready);
    }
    open_with_secret(app, path, secret, ready);
}

fn open_with_secret(app: &adw::Application, path: PathBuf, secret: String, ready: Ready) {
    // A file from an earlier version has no encryption yet.
    if let Err(err) = Database::encrypt_plain_file(&path, &secret) {
        return show_error(app, "Couldn’t Encrypt Your Data", &err.to_string());
    }
    match open_database(&path, &secret) {
        Ok((db, set_aside)) => ready(Unlocked {
            db,
            keys: Keys::Keyring { secret },
            set_aside,
        }),
        // The secret alone does not open it, so a passphrase was added.
        Err(Error::WrongKey) => show_passphrase_prompt(app, path, Some(secret), true, ready),
        Err(err) => show_error(app, "Couldn’t Open Your Data", &err.to_string()),
    }
}

// --- Windows ---------------------------------------------------------------

fn gate_window(app: &adw::Application, page: &adw::StatusPage) -> adw::ApplicationWindow {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(page));
    adw::ApplicationWindow::builder()
        .application(app)
        .title("Morlaco")
        .default_width(480)
        .default_height(480)
        .content(&toolbar)
        .build()
}

/// Shows a message instead of the app.
pub fn show_error(app: &adw::Application, title: &str, message: &str) {
    let page = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title(title)
        .description(message)
        .build();
    gate_window(app, &page).present();
}

fn boxed_fields() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

fn pill_button(label: &str, class: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .halign(gtk::Align::Center)
        .css_classes([class, "pill"])
        .build()
}

/// The setup page for a machine without a usable keyring: the user chooses
/// the passphrase that will be the only protection.
fn show_setup(
    app: &adw::Application,
    path: PathBuf,
    file: FileState,
    reason: Option<String>,
    ready: Ready,
) {
    let new = adw::PasswordEntryRow::builder().title("Passphrase").build();
    let confirm = adw::PasswordEntryRow::builder()
        .title("Confirm passphrase")
        .build();
    let fields = boxed_fields();
    fields.append(&new);
    fields.append(&confirm);
    let problem = gtk::Label::builder()
        .css_classes(["dim-label", "caption"])
        .xalign(0.0)
        .build();
    let create = pill_button("Create", "suggested-action");
    create.set_sensitive(false);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();
    content.append(&fields);
    content.append(&problem);
    content.append(&create);

    let intro = match &reason {
        Some(reason) => format!("The keyring couldn’t be used ({reason}). "),
        None => "No keyring was found. ".to_owned(),
    };
    let page = adw::StatusPage::builder()
        .icon_name("dialog-password-symbolic")
        .title("Choose a Passphrase")
        .description(format!(
            "{intro}Your passphrase will be the only thing protecting your data, so use \
             at least {MIN_PASSPHRASE_ONLY_CHARS} characters. If you forget it, your data \
             can’t be recovered."
        ))
        .child(&content)
        .build();
    let window = gate_window(app, &page);

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
        let (new, problem, window) = (new.clone(), problem.clone(), window.clone());
        Rc::new(move || {
            let passphrase = new.text().to_string();
            let result = (|| -> Result<(Database, Option<PathBuf>), Error> {
                if file == FileState::Plain {
                    Database::encrypt_plain_file(&path, &passphrase)?;
                }
                open_database(&path, &passphrase)
            })();
            match result {
                Ok((db, set_aside)) => {
                    ready(Unlocked {
                        db,
                        keys: Keys::PassphraseOnly { passphrase },
                        set_aside,
                    });
                    window.close();
                }
                Err(err) => problem.set_label(&err.to_string()),
            }
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
fn show_passphrase_prompt(
    app: &adw::Application,
    path: PathBuf,
    secret: Option<String>,
    keyring_reachable: bool,
    ready: Ready,
) {
    let entry = adw::PasswordEntryRow::builder().title("Passphrase").build();
    let fields = boxed_fields();
    fields.append(&entry);
    let unlock = pill_button("Unlock", "suggested-action");

    let try_again = gtk::Button::builder()
        .label("Try Again")
        .css_classes(["flat"])
        .visible(!keyring_reachable)
        .build();
    let start_over = gtk::Button::builder()
        .label("Start Over…")
        .css_classes(["flat"])
        .build();
    let more = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .halign(gtk::Align::Center)
        .spacing(12)
        .build();
    more.append(&try_again);
    more.append(&start_over);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();
    content.append(&fields);
    content.append(&unlock);
    content.append(&more);

    let description = match (&secret, keyring_reachable) {
        (Some(_), _) => "Enter your passphrase to open your data.",
        (None, true) => {
            "Enter your passphrase to open your data. If you never set one, the key \
             for your data is missing from the keyring."
        }
        (None, false) => {
            "The keyring can’t be reached. If you use a passphrase, enter it. \
             Otherwise unlock your keyring and press Try Again."
        }
    };
    let page = adw::StatusPage::builder()
        .icon_name("dialog-password-symbolic")
        .title("Unlock Morlaco")
        .description(description)
        .child(&content)
        .build();
    let window = gate_window(app, &page);

    let attempt = {
        let (entry, page, window) = (entry.clone(), page.clone(), window.clone());
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
                    });
                    window.close();
                }
                Err(Error::WrongKey) => {
                    entry.add_css_class("error");
                    entry.set_text("");
                    page.set_description(Some("That passphrase is wrong. Try again."));
                }
                Err(err) => page.set_description(Some(&err.to_string())),
            }
        })
    };
    let on_activate = attempt.clone();
    entry.connect_entry_activated(move |_| on_activate());
    unlock.connect_clicked(move |_| attempt());
    entry.connect_changed(|entry| entry.remove_css_class("error"));

    let (retry_app, retry_ready, retry_window) = (app.clone(), ready.clone(), window.clone());
    try_again.connect_clicked(move |_| {
        start(&retry_app, retry_ready.clone());
        retry_window.close();
    });
    let (over_app, over_window) = (app.clone(), window.clone());
    start_over.connect_clicked(move |_| {
        confirm_start_over(&over_app, path.clone(), ready.clone(), &over_window);
    });

    window.present();
    entry.grab_focus();
}

/// Asks whether to delete the saved data, and if so does it and starts again.
fn confirm_start_over(
    app: &adw::Application,
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

    let (app, window) = (app.clone(), parent.clone());
    dialog.connect_response(None, move |_, response| {
        if response != "delete" {
            return;
        }
        match delete_database_files(&path) {
            Ok(()) => {
                start(&app, ready.clone());
                window.close();
            }
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
    use crate::keys::Mode;
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
