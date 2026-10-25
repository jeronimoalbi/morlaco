//! Morlaco: a GNOME app that tracks crypto wallets and estimates gains.

mod encryption;
mod format;
mod gate;
mod keys;
mod migrate;
mod store;
mod system_keyring;
#[cfg(test)]
mod testutil;
mod unlock;
mod window;

use gtk4::glib;
use libadwaita as adw;
use libadwaita::prelude::*;

/// The application ID. It also names the app's full-colour icon, its desktop
/// file and, with `-symbolic` added, its symbolic icon.
const APP_ID: &str = "io.github.jeronimoalbi.Morlaco";

/// The symbolic (single-colour) version of the app icon, for use inside the
/// interface.
const SYMBOLIC_ICON: &str = "io.github.jeronimoalbi.Morlaco-symbolic";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        register_icons();
        gtk4::Window::set_default_icon_name(APP_ID);
    });
    app.connect_activate(window::present);
    app.run()
}

/// Lets GTK find the icons shipped in `app/data/icons` when running from a
/// source checkout. Only debug builds do this: a release build is meant to
/// be installed, where the icons are in the system icon theme, and must not
/// contain the build machine's source path.
fn register_icons() {
    #[cfg(debug_assertions)]
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::IconTheme::for_display(&display)
            .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn the_symbolic_icon_is_named_after_the_app_id() {
        assert_eq!(SYMBOLIC_ICON, format!("{APP_ID}-symbolic"));
    }

    #[test]
    fn both_icons_are_shipped_under_their_names() {
        let icons = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/icons/hicolor");
        assert!(icons.join(format!("scalable/apps/{APP_ID}.svg")).is_file());
        assert!(
            icons
                .join(format!("symbolic/apps/{SYMBOLIC_ICON}.svg"))
                .is_file()
        );
    }
}
