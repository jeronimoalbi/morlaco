//! The icons the app uses, embedded in the binary.
//!
//! The interface asks the icon theme for standard names such as
//! `sidebar-show-symbolic`. Systems without the Adwaita icon theme (macOS, for
//! one) can't answer, and the buttons showed up empty. So the icons are
//! compiled in (see `build.rs`) and offered to GTK as a fallback: a theme that
//! has an icon still wins, so on GNOME nothing changes.

use gtk4::{IconTheme, gdk, gio, glib};

/// Where the embedded icons are in the resource, laid out like an icon theme
/// (`scalable/<category>/<name>.svg`).
const RESOURCE_PATH: &str = "/io/github/jeronimoalbi/Morlaco/icons";

/// The compiled resource: every icon in `data/morlaco.gresource.xml`.
static RESOURCE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/morlaco.gresource"));

/// Makes the embedded icons available to GTK. Call once, after GTK started.
pub fn register() {
    match gio::Resource::from_data(&glib::Bytes::from_static(RESOURCE)) {
        Ok(resource) => gio::resources_register(&resource),
        Err(e) => {
            eprintln!("morlaco: could not load the embedded icons: {e}");
            return;
        }
    }
    if let Some(display) = gdk::Display::default() {
        IconTheme::for_display(&display).add_resource_path(RESOURCE_PATH);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_icons_are_in_the_resource() {
        let resource = gio::Resource::from_data(&glib::Bytes::from_static(RESOURCE))
            .expect("the resource is valid");
        for name in [
            "scalable/actions/sidebar-show-symbolic.svg",
            "scalable/actions/go-home-symbolic.svg",
            "scalable/mimetypes/x-office-calendar-symbolic.svg",
            "scalable/apps/io.github.jeronimoalbi.Morlaco-symbolic.svg",
        ] {
            let path = format!("{RESOURCE_PATH}/{name}");
            assert!(
                resource
                    .lookup_data(&path, gio::ResourceLookupFlags::NONE)
                    .is_ok(),
                "{name} is missing"
            );
        }
    }
}
