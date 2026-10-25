//! Morlaco: a GNOME app that tracks a Bitcoin wallet and estimates gains.

mod format;
mod store;
mod window;

use gtk4::glib;
use libadwaita as adw;
use libadwaita::prelude::*;

const APP_ID: &str = "io.github.jeronimoalbi.Morlaco";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(window::present);
    app.run()
}
