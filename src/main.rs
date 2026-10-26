//! Morlaco: a GNOME app that tracks crypto wallets and estimates gains.

mod encryption;
mod format;
mod gate;
mod icons;
mod keys;
mod migrate;
mod shell;
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
        icons::register();
        gtk4::Window::set_default_icon_name(APP_ID);
    });
    app.connect_activate(window::present);
    app.run()
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
