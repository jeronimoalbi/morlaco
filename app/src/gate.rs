//! Layout of the pages shown before the app opens: the unlock prompt, the
//! passphrase setup and error messages.
//!
//! This file only builds widgets. It has no behaviour and depends on nothing
//! but GTK, so `examples/gate_fit.rs` can include it and check, in real
//! windows, that every button is visible.
//!
//! # Why the windows are not resizable
//!
//! A window's minimum size is computed without knowing how wrapped text will
//! reflow at the width the window really has, so a resizable window can be
//! made too short for its content, hiding the buttons at the bottom. That
//! happens even when nothing scrolls: measured with plain boxes, most pages
//! reported a minimum height smaller than they needed. A window that is not
//! resizable has no such freedom: GTK gives it its natural size, working out
//! the width first and then the height needed at that width, so everything is
//! shown at any text size, wording or translation. The window is built with
//! no fixed height for the same reason.
//!
//! The pages stay `adw::StatusPage`s, which scroll as a last resort. If a
//! screen is shorter than a page, the content is still reachable instead of
//! being cut off.

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;

/// Width the windows open at. Not a limit: the window grows if the content
/// needs more room.
///
/// `unlock::gate_window` builds the windows with this width, not resizable
/// and with no height; `examples/gate_fit.rs` repeats exactly that sizing to
/// check it, so change them together.
pub const WINDOW_WIDTH: i32 = 480;

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

/// Builds a page. The compact style keeps the window short.
pub fn gate_page(icon_name: &str, title: &str, description: &str) -> GatePage {
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();
    let root = adw::StatusPage::builder()
        .icon_name(icon_name)
        .title(title)
        .description(description)
        .child(&body)
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
pub struct PromptLayout {
    /// The page.
    pub page: GatePage,
    /// Where the passphrase is typed.
    pub entry: adw::PasswordEntryRow,
    /// Opens the data.
    pub unlock: gtk::Button,
    /// Starts over from the keyring check. Hidden unless the keyring could
    /// not be reached.
    pub try_again: gtk::Button,
    /// Opens the recovery key dialog.
    pub recovery: gtk::Button,
    /// Offers to delete the saved data.
    pub start_over: gtk::Button,
}

/// Builds the unlock prompt.
pub fn prompt_layout(description: &str, show_try_again: bool) -> PromptLayout {
    let page = gate_page("dialog-password-symbolic", "Unlock Morlaco", description);

    let entry = adw::PasswordEntryRow::builder().title("Passphrase").build();
    let fields = boxed_fields();
    fields.append(&entry);
    let unlock = pill_button("Unlock", "suggested-action");

    let flat = |label: &str| {
        gtk::Button::builder()
            .label(label)
            .css_classes(["flat"])
            .build()
    };
    let try_again = flat("Try Again");
    try_again.set_visible(show_try_again);
    let recovery = flat("Use Recovery Key");
    let start_over = flat("Start Over");
    let more = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .halign(gtk::Align::Center)
        .spacing(12)
        .build();
    more.append(&try_again);
    more.append(&recovery);
    more.append(&start_over);

    page.body.append(&fields);
    page.body.append(&unlock);
    page.body.append(&more);

    PromptLayout {
        page,
        entry,
        unlock,
        try_again,
        recovery,
        start_over,
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
    let page = gate_page(
        "dialog-password-symbolic",
        "Choose a Passphrase",
        description,
    );

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
