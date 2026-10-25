//! Morlaco: a GNOME app that tracks a Bitcoin wallet and estimates gains.

mod format;
mod keys;
mod migrate;
mod store;
mod unlock;
mod window;

use gtk4::glib;
use libadwaita as adw;
use libadwaita::prelude::*;

const APP_ID: &str = "io.github.jeronimoalbi.Morlaco";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| register_icons());
    app.connect_activate(window::present);
    app.run()
}

/// Lets GTK find the icons shipped in `app/data/icons` when running from a
/// source checkout. An installed app finds them in the system icon theme, in
/// which case this path simply does not exist.
fn register_icons() {
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::IconTheme::for_display(&display)
            .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
    }
}
