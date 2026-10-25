//! Layout of the pages shown before the app opens: the unlock prompt, the
//! passphrase setup and error messages.
//!
//! This file only builds widgets. It has no behaviour and depends on nothing
//! but GTK.
//!
//! The pages are shown inside the app's one window (see `shell`), as its first
//! screen. They are plain `adw::StatusPage`s, which scroll if the window is
//! too small for them, so the buttons stay reachable at any text size.

use gtk::gio;
use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;

/// Names of the window actions behind the "⋮" menu on the unlock page, without
/// the `win.` prefix. The menu and the code that registers the actions both
/// use these, so a typo cannot silently disable a menu item.
pub const ACTION_TRY_AGAIN: &str = "try-again";
/// Opens the recovery key dialog.
pub const ACTION_USE_RECOVERY_KEY: &str = "use-recovery-key";
/// Offers to delete the saved data.
pub const ACTION_START_OVER: &str = "start-over";

/// One page: a status page with a body for the page's own widgets.
pub struct GatePage {
    /// Put this in the window.
    pub root: adw::StatusPage,
    /// Where the page's own widgets go.
    pub body: gtk::Box,
}

impl GatePage {
    /// Replaces the description under the title.
    pub fn set_description(&self, text: &str) {
        self.root.set_description(Some(text));
    }
}

/// Builds a page: an icon, a title, a description and the page's own widgets.
/// The compact style keeps the tallest page, the passphrase setup, inside the
/// window's default height.
pub fn gate_page(icon_name: &str, title: &str, description: &str) -> GatePage {
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();
    // The entry and buttons keep a sensible width in a wide window.
    let clamp = adw::Clamp::builder()
        .maximum_size(380)
        .tightening_threshold(300)
        .child(&body)
        .build();
    let root = adw::StatusPage::builder()
        .icon_name(icon_name)
        .title(title)
        .description(description)
        .child(&clamp)
        .css_classes(["compact"])
        .build();
    GatePage { root, body }
}

/// A list with the rounded "boxed" look, for entry rows.
pub fn boxed_fields() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

/// A wide rounded button. `class` is a style such as `suggested-action`.
pub fn pill_button(label: &str, class: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .halign(gtk::Align::Center)
        .css_classes([class, "pill"])
        .build()
}

/// The unlock prompt and the widgets its behaviour needs.
///
/// The secondary actions are not on the page. They sit in the menu of the
/// "⋮" button next to Unlock, as window actions (see the `ACTION_*` names),
/// and appear only when that button is used.
pub struct PromptLayout {
    /// The page.
    pub page: GatePage,
    /// Where the passphrase is typed.
    pub entry: adw::PasswordEntryRow,
    /// Opens the data.
    pub unlock: gtk::Button,
}

/// The menu behind the "⋮" button. "Try Again" is only offered when the
/// keyring could not be reached, and "Start Over" has a section to itself
/// because it deletes data.
fn more_menu(show_try_again: bool) -> gio::Menu {
    let menu = gio::Menu::new();

    let common = gio::Menu::new();
    if show_try_again {
        common.append(Some("Try Again"), Some(&format!("win.{ACTION_TRY_AGAIN}")));
    }
    common.append(
        Some("Use Recovery Key"),
        Some(&format!("win.{ACTION_USE_RECOVERY_KEY}")),
    );
    menu.append_section(None, &common);

    let destructive = gio::Menu::new();
    destructive.append(
        Some("Start Over"),
        Some(&format!("win.{ACTION_START_OVER}")),
    );
    menu.append_section(None, &destructive);
    menu
}

/// Builds the unlock prompt.
pub fn prompt_layout(description: &str, show_try_again: bool) -> PromptLayout {
    let page = gate_page(crate::APP_ID, "Unlock Morlaco", description);

    let entry = adw::PasswordEntryRow::builder().title("Passphrase").build();
    let fields = boxed_fields();
    fields.append(&entry);
    let unlock = pill_button("Unlock", "suggested-action");

    let more = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .menu_model(&more_menu(show_try_again))
        .tooltip_text("More Options")
        .valign(gtk::Align::Center)
        .css_classes(["circular"])
        .build();
    // An icon alone gives a screen reader nothing to say.
    more.update_property(&[gtk::accessible::Property::Label("More options")]);

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .halign(gtk::Align::Center)
        .spacing(6)
        .build();
    row.append(&unlock);
    row.append(&more);

    page.body.append(&fields);
    page.body.append(&row);

    PromptLayout {
        page,
        entry,
        unlock,
    }
}

/// The passphrase setup page and the widgets its behaviour needs.
pub struct SetupLayout {
    /// The page.
    pub page: GatePage,
    /// The new passphrase.
    pub new: adw::PasswordEntryRow,
    /// The same passphrase again.
    pub confirm: adw::PasswordEntryRow,
    /// What is wrong with the passphrase, if anything.
    pub problem: gtk::Label,
    /// Creates the data. Starts disabled.
    pub create: gtk::Button,
}

/// Builds the passphrase setup page.
pub fn setup_layout(description: &str) -> SetupLayout {
    let page = gate_page(crate::APP_ID, "Choose a Passphrase", description);

    let new = adw::PasswordEntryRow::builder().title("Passphrase").build();
    let confirm = adw::PasswordEntryRow::builder()
        .title("Confirm passphrase")
        .build();
    let fields = boxed_fields();
    fields.append(&new);
    fields.append(&confirm);
    let problem = gtk::Label::builder()
        .css_classes(["dim-label", "caption"])
        .wrap(true)
        .xalign(0.0)
        .build();
    let create = pill_button("Create", "suggested-action");
    create.set_sensitive(false);

    page.body.append(&fields);
    page.body.append(&problem);
    page.body.append(&create);

    SetupLayout {
        page,
        new,
        confirm,
        problem,
        create,
    }
}

/// Builds an error page.
pub fn error_layout(title: &str, message: &str) -> GatePage {
    gate_page("dialog-error-symbolic", title, message)
}
