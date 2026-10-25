//! The one window the app lives in.
//!
//! It opens on the unlock page (or the passphrase setup, or an error) and,
//! once the data is open, shows the app in the same window. So unlocking is
//! the first screen of the app, not a separate window in front of it.

use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;

use crate::gate::{self, GatePage};

/// Name of the stack page that shows the unlock page and its relatives.
const GATE: &str = "gate";
/// Name of the stack page that shows the app.
const APP: &str = "app";

/// The window, and what it is showing.
pub struct Shell {
    /// The window. It is created here and never replaced.
    pub window: adw::ApplicationWindow,
    stack: gtk::Stack,
}

impl Shell {
    /// Creates the window, showing only the app icon until there is
    /// something to ask. It is not presented yet: a window with nothing to
    /// show but a blank page would flash by when the keyring opens the data
    /// at once.
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Morlaco")
            .default_width(900)
            .default_height(640)
            .width_request(360)
            .content(&stack)
            .build();
        let shell = Rc::new(Self { window, stack });
        shell.put_gate_page(&adw::StatusPage::builder().icon_name(crate::APP_ID).build());
        shell
    }

    /// Shows an unlock, setup or error page and presents the window.
    pub fn show_gate(&self, page: &GatePage) {
        self.put_gate_page(&page.root);
        self.window.present();
    }

    /// Shows the app, which replaces the unlock page and everything that
    /// belonged to it. The caller presents the window.
    pub fn show_app(&self, content: &impl IsA<gtk::Widget>) {
        self.stack.add_named(content, Some(APP));
        self.stack.set_visible_child_name(APP);

        // The actions behind the unlock page's "⋮" menu mean nothing now.
        for name in [
            gate::ACTION_TRY_AGAIN,
            gate::ACTION_USE_RECOVERY_KEY,
            gate::ACTION_START_OVER,
        ] {
            self.window.remove_action(name);
        }
        // Take the unlock page away once the crossfade has finished.
        self.stack.connect_transition_running_notify(|stack| {
            if !stack.is_transition_running()
                && let Some(old) = stack.child_by_name(GATE)
            {
                stack.remove(&old);
            }
        });
    }

    /// Puts `page` on the stack as the gate page, in place of any earlier
    /// one. The header is flat and has no title, so the page reads as the
    /// app's first screen.
    fn put_gate_page(&self, page: &adw::StatusPage) {
        let header = adw::HeaderBar::builder().show_title(false).build();
        header.add_css_class("flat");
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(page));

        if let Some(old) = self.stack.child_by_name(GATE) {
            self.stack.remove(&old);
        }
        self.stack.add_named(&toolbar, Some(GATE));
        self.stack.set_visible_child_name(GATE);
    }
}
