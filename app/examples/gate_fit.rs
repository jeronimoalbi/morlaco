//! Checks, in real windows, that the unlock, setup and error pages show all
//! of their content: nothing is squeezed and the lowest button is visible.
//!
//! This guards against the bug where the "Use Recovery Key" and "Start Over"
//! buttons were cut off because the window was too short. Pages are tried at
//! three text sizes, since larger text needs more room.
//!
//! It opens and closes a window per page, so it needs a display and is not
//! part of CI:
//!
//! ```sh
//! cargo run --example gate_fit               # the sizing the app uses
//! cargo run --example gate_fit -- --compare  # also the two earlier sizings
//! ```
//!
//! It exits with a failure status if the app's sizing hides anything.

// The pages are built by the same file the app uses. Not every item in it is
// used here.
#[allow(dead_code)]
#[path = "../src/gate.rs"]
mod gate;

use std::process::ExitCode;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

const TEXT_SCALES: [f64; 3] = [1.0, 1.5, 2.0];

/// How a window is sized.
#[derive(Clone, Copy, PartialEq)]
enum Sizing {
    /// What the app does: not resizable, a default width and no fixed height,
    /// so the window gets its natural size.
    Natural,
    /// The first version: resizable, 480 by 480, regular page.
    Original,
    /// The first fix: resizable, 520 by 620, compact page.
    Patch,
}

impl Sizing {
    fn name(self) -> &'static str {
        match self {
            Sizing::Natural => "natural (app)",
            Sizing::Original => "480x480 (original)",
            Sizing::Patch => "520x620 (first fix)",
        }
    }
}

/// A page to check and the widgets on it that a person has to reach, which
/// must all be visible in the window.
struct Case {
    name: &'static str,
    page: adw::StatusPage,
    reachable: Vec<gtk::Widget>,
}

fn cases() -> Vec<Case> {
    let unreachable = "The keyring can’t be reached. If you use a passphrase, enter it. \
                       Otherwise unlock your keyring and press Try Again.";
    let missing = "Enter your passphrase to open your data. If you never set one, the key \
                   for your data is missing from the keyring.";
    let setup_reason = "The keyring couldn’t be used (The name org.freedesktop.secrets was not \
                        provided by any .service files). Your passphrase will be the only thing \
                        protecting your data, so use at least 12 characters. If you forget it, \
                        your data can’t be recovered.";
    let long_error = "database error: file error: Permission denied (os error 13) while opening \
                      /home/someone/.local/share/morlaco/a-very-long-directory-name/morlaco.db \
                      which could not be read because it belongs to another user";

    let prompt_a = gate::prompt_layout(missing, false);
    let prompt_b = gate::prompt_layout(unreachable, true);
    let setup_a = gate::setup_layout(
        "No keyring was found. Your passphrase will be the only thing protecting your data, so \
         use at least 12 characters. If you forget it, your data can’t be recovered.",
    );
    let setup_b = gate::setup_layout(setup_reason);
    setup_b
        .problem
        .set_label("The two passphrases don’t match.");
    let error = gate::error_layout("Couldn’t Open Your Data", long_error);

    vec![
        Case {
            name: "prompt, keyring reachable",
            reachable: vec![
                prompt_a.unlock.clone().upcast(),
                prompt_a.more.clone().upcast(),
            ],
            page: prompt_a.page.root,
        },
        Case {
            name: "prompt, keyring unreachable",
            reachable: vec![
                prompt_b.unlock.clone().upcast(),
                prompt_b.more.clone().upcast(),
            ],
            page: prompt_b.page.root,
        },
        Case {
            name: "setup, no keyring found",
            reachable: vec![setup_a.create.clone().upcast()],
            page: setup_a.page.root,
        },
        Case {
            name: "setup, keyring error and a problem",
            reachable: vec![setup_b.create.clone().upcast()],
            page: setup_b.page.root,
        },
        Case {
            name: "error, long message",
            reachable: Vec::new(),
            page: error.root,
        },
    ]
}

/// Puts the page in a window sized as `sizing` says and shows it.
fn present(page: &adw::StatusPage, sizing: Sizing) -> adw::Window {
    page.remove_css_class("compact");
    if sizing != Sizing::Original {
        page.add_css_class("compact");
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(page));
    let builder = adw::Window::builder().content(&toolbar);
    let window = match sizing {
        Sizing::Natural => builder
            .resizable(false)
            .default_width(gate::WINDOW_WIDTH)
            .build(),
        Sizing::Original => builder.default_width(480).default_height(480).build(),
        Sizing::Patch => builder.default_width(520).default_height(620).build(),
    };
    window.present();
    window
}

/// Runs the main loop until the window has been shown and has settled.
fn settle(window: &adw::Window) {
    let context = glib::MainContext::default();
    let mut calm = 0;
    for _ in 0..500 {
        while context.iteration(false) {}
        if window.is_mapped() && window.width() > 1 && window.height() > 1 {
            calm += 1;
            if calm > 8 {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What was found in one window.
struct Outcome {
    window: (i32, i32),
    page_height: i32,
    needed_height: i32,
    /// Bottom edge of the lowest reachable widget, within the window, if any.
    lowest_bottom: Option<i32>,
    visible: bool,
}

fn inspect(window: &adw::Window, case: &Case) -> Outcome {
    let page = &case.page;
    // The height the page needs at the width it really has.
    let needed_height = page.measure(gtk::Orientation::Vertical, page.width()).1;
    let squeezed = page.height() < needed_height;

    // Where each widget a person must reach ends, within the window. A widget
    // whose position cannot be worked out counts as not visible.
    let bottoms: Vec<Option<i32>> = case
        .reachable
        .iter()
        .map(|widget| {
            widget
                .compute_bounds(window)
                .map(|bounds| (bounds.y() + bounds.height()).ceil() as i32)
        })
        .collect();
    let lowest_bottom = bottoms.iter().flatten().max().copied();
    let all_visible = bottoms
        .iter()
        .all(|bottom| bottom.is_some_and(|b| b <= window.height()));

    Outcome {
        window: (window.width(), window.height()),
        page_height: page.height(),
        needed_height,
        lowest_bottom,
        visible: !squeezed && all_visible,
    }
}

fn main() -> ExitCode {
    let compare = std::env::args().any(|arg| arg == "--compare");
    gtk::init().expect("GTK needs a display");
    adw::init().expect("libadwaita");
    let settings = gtk::Settings::default().expect("default settings");
    let default_dpi = settings.gtk_xft_dpi();

    let sizings: &[Sizing] = if compare {
        &[Sizing::Original, Sizing::Patch, Sizing::Natural]
    } else {
        &[Sizing::Natural]
    };

    println!(
        "{:<5} {:<20} {:<36} {:>9} {:>11} {:>12}  verdict",
        "text", "sizing", "page", "window", "page/needed", "button/win h"
    );
    let mut app_failures = 0;
    for scale in TEXT_SCALES {
        settings.set_gtk_xft_dpi((f64::from(default_dpi) * scale) as i32);
        for &sizing in sizings {
            for case in cases() {
                let window = present(&case.page, sizing);
                settle(&window);
                let outcome = inspect(&window, &case);
                window.close();
                while glib::MainContext::default().iteration(false) {}

                if !outcome.visible && sizing == Sizing::Natural {
                    app_failures += 1;
                }
                println!(
                    "{:<5} {:<20} {:<36} {:>9} {:>11} {:>12}  {}",
                    format!("{scale}x"),
                    sizing.name(),
                    case.name,
                    format!("{}x{}", outcome.window.0, outcome.window.1),
                    format!("{}/{}", outcome.page_height, outcome.needed_height),
                    outcome
                        .lowest_bottom
                        .map_or("-".to_owned(), |b| format!("{}/{}", b, outcome.window.1)),
                    if outcome.visible { "ok" } else { "CUT OFF" }
                );
            }
        }
    }
    settings.set_gtk_xft_dpi(default_dpi);

    if app_failures > 0 {
        eprintln!("\n{app_failures} page(s) hide content with the app's sizing");
        ExitCode::FAILURE
    } else {
        println!("\nwith the app's sizing every page shows all of its content");
        ExitCode::SUCCESS
    }
}
