//! Opening the encrypted database at startup.
//!
//! Normally this finishes at once: the key comes from the keyring. A window
//! is shown only when the user must act: a passphrase is required, the
//! keyring lost the key, or something failed.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use morlaco_core::Error;
use morlaco_core::db::Database;
use morlaco_core::paths;

use crate::keys::{self, Keys};

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
    let secret = match keys::lookup() {
        Ok(secret) => secret,
        Err(message) => return show_error(app, "Couldn’t Reach the Keyring", &message),
    };
    match secret {
        Some(secret) => open_with_secret(app, path, secret, ready),
        None => first_run(app, path, ready),
    }
}

/// No secret in the keyring: either this is a new install, or the keyring
/// was reset.
fn first_run(app: &adw::Application, path: PathBuf, ready: Ready) {
    let plain = match Database::is_plain_file(&path) {
        Ok(plain) => plain,
        Err(err) => return show_error(app, "Couldn’t Open Your Data", &err.to_string()),
    };
    // An encrypted file without its key cannot be recovered.
    if path.exists() && !plain {
        return show_keyring_lost(app, path, ready);
    }

    // Save the key before using it, so data is never encrypted with a key
    // that was not kept.
    let secret = match keys::generate_secret().and_then(|s| keys::store(&s).map(|()| s)) {
        Ok(secret) => secret,
        Err(message) => return show_error(app, "Couldn’t Save the Key", &message),
    };
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
            keys: Keys::new(secret, None),
            set_aside,
        }),
        // The secret alone does not open it, so a passphrase was added.
        Err(Error::WrongKey) => show_passphrase_prompt(app, path, secret, ready),
        Err(err) => show_error(app, "Couldn’t Open Your Data", &err.to_string()),
    }
}

// --- Windows -------------------------------------------------------------

fn gate_window(app: &adw::Application, page: &adw::StatusPage) -> adw::ApplicationWindow {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(page));
    adw::ApplicationWindow::builder()
        .application(app)
        .title("Morlaco")
        .default_width(480)
        .default_height(420)
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

fn show_passphrase_prompt(app: &adw::Application, path: PathBuf, secret: String, ready: Ready) {
    let entry = adw::PasswordEntryRow::builder().title("Passphrase").build();
    let fields = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    fields.append(&entry);
    let unlock = gtk::Button::builder()
        .label("Unlock")
        .halign(gtk::Align::Center)
        .css_classes(["suggested-action", "pill"])
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();
    content.append(&fields);
    content.append(&unlock);

    let page = adw::StatusPage::builder()
        .icon_name("dialog-password-symbolic")
        .title("Unlock Morlaco")
        .description("Enter your passphrase to open your data.")
        .child(&content)
        .build();
    let window = gate_window(app, &page);

    let attempt = {
        let (entry, page, window) = (entry.clone(), page.clone(), window.clone());
        Rc::new(move || {
            let passphrase = entry.text().to_string();
            let key = keys::compose(&secret, Some(&passphrase));
            match open_database(&path, &key) {
                Ok((db, set_aside)) => {
                    ready(Unlocked {
                        db,
                        keys: Keys::new(secret.clone(), Some(passphrase)),
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

    window.present();
    entry.grab_focus();
}

fn show_keyring_lost(app: &adw::Application, path: PathBuf, ready: Ready) {
    let start_over = gtk::Button::builder()
        .label("Start Over…")
        .halign(gtk::Align::Center)
        .css_classes(["destructive-action", "pill"])
        .build();
    let page = adw::StatusPage::builder()
        .icon_name("dialog-warning-symbolic")
        .title("Can’t Unlock Your Data")
        .description(
            "The key for your saved data is missing from the keyring, so the data \
             can’t be read. This happens when the keyring was reset or the data was \
             copied from another computer.",
        )
        .child(&start_over)
        .build();
    let window = gate_window(app, &page);

    let app = app.clone();
    let parent = window.clone();
    start_over.connect_clicked(move |_| {
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

        let (app, path, ready, window) = (app.clone(), path.clone(), ready.clone(), parent.clone());
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
        dialog.present(Some(&parent));
    });
    window.present();
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
