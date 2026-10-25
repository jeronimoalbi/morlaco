//! Renders the main window, filled with two example wallets, to a PNG for the
//! README. The numbers are made up. Nothing is read from or written to your
//! data, the keyring or the network; the only file written is the image.
//!
//! It opens the real window, so it needs a display and is not part of CI:
//!
//! ```sh
//! cargo run --example screenshot                     # docs/screenshots/overview.png
//! cargo run --example screenshot -- path/to/out.png
//! ```

// The app's modules are included as they are, to build the real window. The
// example does not use every item in them.
#![allow(dead_code)]

#[path = "../src/encryption.rs"]
mod encryption;
#[path = "../src/format.rs"]
mod format;
#[path = "../src/gate.rs"]
mod gate;
#[path = "../src/keys.rs"]
mod keys;
#[path = "../src/migrate.rs"]
mod migrate;
#[path = "../src/store.rs"]
mod store;
#[path = "../src/system_keyring.rs"]
mod system_keyring;
#[path = "../src/unlock.rs"]
mod unlock;
#[path = "../src/window.rs"]
mod window;

use std::path::PathBuf;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, glib, graphene, gsk};
use gtk4 as gtk;
use libadwaita as adw;
use morlaco_core::chain::{Activity, Chain, WalletId};
use morlaco_core::db::Database;
use morlaco_core::portfolio::PricedTx;

use crate::keys::Keys;
use crate::store::Store;

// Constants the included modules expect from the crate root.
const APP_ID: &str = "io.github.jeronimoalbi.Morlaco";
const SYMBOLIC_ICON: &str = "io.github.jeronimoalbi.Morlaco-symbolic";

/// A different application ID, so this never hands over to a running Morlaco.
const EXAMPLE_APP_ID: &str = "io.github.jeronimoalbi.Morlaco.Screenshot";

/// Logical size of the window, and how many pixels per logical pixel the
/// image has (2 keeps it sharp on high-resolution screens).
const WINDOW_SIZE: (i32, i32) = (1000, 740);

/// Room around the window for its shadow, in logical pixels.
const SHADOW_MARGIN: f32 = 56.0;
const IMAGE_SCALE: f32 = 2.0;

/// The made-up price of one bitcoin "today", in USD.
const PRICE_NOW: f64 = 104_250.0;

const SATS_PER_BTC: i128 = 100_000_000;

/// One transaction of an example wallet. `btc` is signed; `price` is the
/// made-up USD price that day; `fee_sats` is what was paid on top.
fn tx(txid: &str, height: u64, time: i64, btc: f64, price: f64, fee_sats: u128) -> PricedTx {
    PricedTx {
        activity: Activity {
            txid: txid.to_owned(),
            height,
            index: None,
            time,
            amount: (btc * SATS_PER_BTC as f64).round() as i128,
            fee: fee_sats,
            parents: Vec::new(),
        },
        price_usd: price,
    }
}

/// An in-memory database holding two example wallets and a price. The
/// addresses are the public test vectors from BIP 173.
fn example_store() -> Store {
    let db = Database::in_memory().expect("in-memory database");
    let wallets = [
        (
            "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4",
            "Cold storage",
            vec![
                tx("a1", 779_000, 1_678_752_000, 0.50, 24_500.0, 0),
                tx("a2", 830_000, 1_708_387_200, 0.25, 52_300.0, 0),
                tx("a3", 868_000, 1_731_369_600, 0.10, 88_100.0, 0),
            ],
        ),
        (
            "bc1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3qccfmv3",
            "Everyday",
            vec![
                tx("b1", 845_000, 1_717_372_800, 0.15, 67_800.0, 0),
                tx("b2", 880_000, 1_737_331_200, -0.05, 102_000.0, 2_000),
            ],
        ),
    ];
    for (address, label, txs) in wallets {
        let id = WalletId::new(Chain::Bitcoin, address);
        db.add_wallet(&id, label).expect("add wallet");
        // Saving the history also marks the wallet as fetched.
        db.replace_history_above(&id, 0, &txs)
            .expect("save history");
    }
    db.set_setting(
        &format!("last_price:{}", Chain::Bitcoin.key()),
        &PRICE_NOW.to_string(),
    )
    .expect("save price");
    Store::open(db).expect("open the store")
}

/// Draws the window, on a transparent background with a soft shadow, into a
/// PNG.
fn capture(window: &adw::ApplicationWindow, out: &PathBuf) {
    let (width, height) = (window.width() as f32, window.height() as f32);
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    snapshot.scale(IMAGE_SCALE, IMAGE_SCALE);

    // The shadow first, so the window is drawn over it.
    let corner = graphene::Size::new(14.0, 14.0);
    let outline = gsk::RoundedRect::new(
        graphene::Rect::new(SHADOW_MARGIN, SHADOW_MARGIN, width, height),
        corner,
        corner,
        corner,
        corner,
    );
    snapshot.append_outset_shadow(
        &outline,
        &gdk::RGBA::new(0.0, 0.0, 0.0, 0.32),
        0.0,
        8.0,
        0.0,
        32.0,
    );

    snapshot.translate(&graphene::Point::new(SHADOW_MARGIN, SHADOW_MARGIN));
    paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
    let node = snapshot.to_node().expect("the window drew something");
    let renderer = window
        .native()
        .and_then(|native| native.renderer())
        .expect("a renderer for the window");

    let image_size = graphene::Rect::new(
        0.0,
        0.0,
        (width + 2.0 * SHADOW_MARGIN) * IMAGE_SCALE,
        (height + 2.0 * SHADOW_MARGIN) * IMAGE_SCALE,
    );
    let texture = renderer.render_texture(&node, Some(&image_size));
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).expect("create the output directory");
    }
    texture.save_to_png(out).expect("write the PNG");
    println!(
        "wrote {} ({}x{})",
        out.display(),
        texture.width(),
        texture.height()
    );
}

fn main() -> glib::ExitCode {
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "docs/screenshots/overview.png".to_owned()),
    );

    let app = adw::Application::builder()
        .application_id(EXAMPLE_APP_ID)
        .build();
    app.connect_startup(|_| {
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::IconTheme::for_display(&display)
                .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
        }
        // The same look every time, whatever the desktop is set to.
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceLight);
    });
    app.connect_activate(move |app| {
        let keys = Keys::Keyring {
            secret: "0".repeat(64),
        };
        let ui = window::build_main_window(app, example_store(), keys, false);
        ui.window.set_default_size(WINDOW_SIZE.0, WINDOW_SIZE.1);
        ui.window.present();

        // Give the window time to be shown and laid out, then draw it.
        let (window, out, app) = (ui.window.clone(), out.clone(), app.clone());
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            capture(&window, &out);
            app.quit();
        });
    });
    app.run_with_args::<&str>(&[])
}
