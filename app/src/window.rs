//! The main window: wallet sidebar, overview, holdings and transactions.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use gtk::{gio, glib};
use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use morlaco_core::cache::Cache;
use morlaco_core::chain;
use morlaco_core::lots::Summary;
use morlaco_core::paths;
use morlaco_core::portfolio::{self, History, Portfolio};
use morlaco_core::price::{CachedPrices, MempoolPrices, PriceSource};
use morlaco_core::util::date_string;

use crate::format;
use crate::store::{Store, Wallet};

const EXPLORER_TX_URL: &str = "https://mempool.space/tx/";

/// What the content area is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selection {
    /// Totals across every wallet.
    Overview,
    /// One wallet, by index into the store.
    Wallet(usize),
}

/// Mutable data behind the widgets.
struct State {
    store: Store,
    selection: Selection,
    /// Downloaded and priced history by address. Kept so switching views is
    /// instant.
    histories: HashMap<String, History>,
    /// BTC/USD from the latest successful load.
    price_now: Option<f64>,
    /// Derived from `histories` by [`State::recompute`], by address.
    portfolios: HashMap<String, Portfolio>,
    /// Why a wallet failed to load, by address.
    errors: HashMap<String, String>,
}

impl State {
    /// Rebuilds every portfolio from the loaded histories.
    ///
    /// Wallets are valued together so that coins moved between them keep
    /// their original cost basis. Call after any history is added, replaced
    /// or removed.
    fn recompute(&mut self) {
        let portfolios = match self.price_now {
            Some(price) => {
                let loaded: Vec<&History> = self
                    .store
                    .wallets
                    .iter()
                    .filter_map(|w| self.histories.get(&w.address))
                    .collect();
                portfolio::assemble(&loaded, price)
            }
            None => Vec::new(),
        };
        self.portfolios = portfolios
            .into_iter()
            .map(|p| (p.address.clone(), p))
            .collect();
    }
}

/// One labelled figure in a summary table.
struct Figure {
    row: adw::ActionRow,
    value: gtk::Label,
}

impl Figure {
    fn new(title: &str) -> Self {
        let value = gtk::Label::builder()
            .css_classes(["numeric"])
            .selectable(true)
            .build();
        let row = adw::ActionRow::builder().title(title).build();
        row.add_suffix(&value);
        Self { row, value }
    }
}

/// Headline gain/loss plus the totals table. Used by both the overview and
/// the per-wallet holdings page.
struct SummaryView {
    root: gtk::Box,
    heading: gtk::Label,
    subtitle: gtk::Label,
    gain: gtk::Label,
    gain_pct: gtk::Label,
    balance: Figure,
    value: Figure,
    cost: Figure,
    realized: Figure,
    price: Figure,
}

impl SummaryView {
    fn new() -> Self {
        let heading = gtk::Label::builder()
            .css_classes(["title-2"])
            .wrap(true)
            .build();
        let subtitle = gtk::Label::builder()
            .css_classes(["dim-label", "caption"])
            .selectable(true)
            .build();
        let caption = gtk::Label::builder()
            .label("Unrealized gain / loss")
            .css_classes(["dim-label"])
            .margin_top(12)
            .build();
        let gain = gtk::Label::builder()
            .css_classes(["title-1", "numeric"])
            .selectable(true)
            .build();
        let gain_pct = gtk::Label::builder()
            .css_classes(["title-4", "numeric"])
            .build();

        let hero = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        for w in [&heading, &subtitle, &caption, &gain, &gain_pct] {
            hero.append(w);
        }

        let balance = Figure::new("Balance");
        let value = Figure::new("Current value");
        let cost = Figure::new("Cost basis");
        let realized = Figure::new("Realized gain / loss");
        let price = Figure::new("BTC price");
        let totals = adw::PreferencesGroup::new();
        for f in [&balance, &value, &cost, &realized, &price] {
            totals.add(&f.row);
        }

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .build();
        root.append(&hero);
        root.append(&totals);

        Self {
            root,
            heading,
            subtitle,
            gain,
            gain_pct,
            balance,
            value,
            cost,
            realized,
            price,
        }
    }

    fn update(&self, title: &str, subtitle: &str, s: &Summary, price_usd: f64, hidden: bool) {
        self.heading.set_label(title);
        self.subtitle.set_label(subtitle);
        self.gain
            .set_label(&format::signed_usd(s.unrealized_usd, hidden));
        self.gain_pct
            .set_label(&format::percent(s.unrealized_pct, hidden));
        tone(&self.gain, s.unrealized_usd, hidden);
        tone(&self.gain_pct, s.unrealized_usd, hidden);

        self.balance
            .value
            .set_label(&format::btc(s.balance_sats, hidden));
        self.value
            .value
            .set_label(&format::usd(s.value_usd, hidden));
        self.cost
            .value
            .set_label(&format::usd(s.cost_basis_usd, hidden));
        self.realized
            .value
            .set_label(&format::signed_usd(s.realized_usd, hidden));
        tone(&self.realized.value, s.realized_usd, hidden);
        self.price.value.set_label(&format::usd(price_usd, false));
    }
}

struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    split: adw::OverlaySplitView,
    wallet_list: gtk::ListBox,
    pages: gtk::Stack,
    title_stack: gtk::Stack,
    switcher_bar: adw::ViewSwitcherBar,
    loading_label: gtk::Label,
    error_page: adw::StatusPage,
    refresh_btn: gtk::Button,
    hide_btn: gtk::ToggleButton,
    overview: SummaryView,
    overview_wallets: gtk::ListBox,
    overview_warning: gtk::Label,
    holdings: SummaryView,
    lots_list: gtk::ListBox,
    history_list: gtk::ListBox,
    state: RefCell<State>,
    /// Incremented on every load or selection so late answers are dropped.
    generation: Cell<u64>,
    loading: Cell<bool>,
}

/// Builds the window, or raises it if it already exists.
pub fn present(app: &adw::Application) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }
    let ui = Ui::new(app);
    ui.connect_signals();
    ui.rebuild_sidebar();
    ui.select(Selection::Overview);
    ui.window.present();
}

impl Ui {
    fn new(app: &adw::Application) -> Rc<Self> {
        let store = Store::load();
        let hidden = store.hidden;

        // Sidebar
        let wallet_list = gtk::ListBox::builder()
            .css_classes(["navigation-sidebar"])
            .build();
        let add_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add Wallet (Ctrl+N)")
            .action_name("win.add-wallet")
            .build();
        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.pack_start(&add_btn);
        let sidebar_toolbar = adw::ToolbarView::new();
        sidebar_toolbar.add_top_bar(&sidebar_header);
        sidebar_toolbar.set_content(Some(
            &gtk::ScrolledWindow::builder().child(&wallet_list).build(),
        ));

        // Overview page
        let overview = SummaryView::new();
        let overview_wallets = boxed_list();
        let overview_group = adw::PreferencesGroup::builder().title("Wallets").build();
        overview_group.add(&overview_wallets);
        let overview_warning = gtk::Label::builder()
            .css_classes(["warning"])
            .wrap(true)
            .xalign(0.0)
            .visible(false)
            .build();
        let overview_note = note(
            "Coins moved between the wallets listed here keep their original cost basis. \
             Coins moved to or from any other wallet count as a sale or a purchase.",
        );
        let overview_box = page_box();
        overview_box.append(&overview.root);
        overview_box.append(&overview_warning);
        overview_box.append(&overview_group);
        overview_box.append(&overview_note);

        // Wallet pages
        let holdings = SummaryView::new();
        let lots_list = boxed_list();
        let lots_group = adw::PreferencesGroup::builder()
            .title("Lots")
            .description("Coins still held, oldest first")
            .build();
        lots_group.add(&lots_list);
        let holdings_box = page_box();
        holdings_box.append(&holdings.root);
        holdings_box.append(&lots_group);
        holdings_box.append(&note(
            "Cost basis is the BTC price when coins arrived at this address. \
             It is an estimate, not what you paid.",
        ));

        let history_list = boxed_list();
        let history_box = page_box();
        history_box.append(&history_list);

        let view_stack = adw::ViewStack::new();
        view_stack.add_titled_with_icon(
            &scrolled_clamp(&holdings_box),
            Some("holdings"),
            "Holdings",
            "view-list-symbolic",
        );
        view_stack.add_titled_with_icon(
            &scrolled_clamp(&history_box),
            Some("transactions"),
            "Transactions",
            "view-sort-descending-symbolic",
        );

        // Page states: empty, loading, error, overview, data
        let empty_page = adw::StatusPage::builder()
            .icon_name("list-add-symbolic")
            .title("No Wallets Yet")
            .description("Add a Bitcoin address to see its holdings and estimated gains.")
            .child(
                &gtk::Button::builder()
                    .label("Add Wallet")
                    .halign(gtk::Align::Center)
                    .css_classes(["suggested-action", "pill"])
                    .action_name("win.add-wallet")
                    .build(),
            )
            .build();
        let loading_label = gtk::Label::builder()
            .label("Fetching transactions…")
            .css_classes(["dim-label"])
            .build();
        let loading_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .build();
        loading_box.append(&gtk::Spinner::builder().spinning(true).build());
        loading_box.append(&loading_label);
        let error_page = adw::StatusPage::builder()
            .icon_name("dialog-error-symbolic")
            .title("Couldn’t Load Wallet")
            .build();

        let pages = gtk::Stack::new();
        pages.add_named(&empty_page, Some("empty"));
        pages.add_named(&loading_box, Some("loading"));
        pages.add_named(&error_page, Some("error"));
        pages.add_named(&scrolled_clamp(&overview_box), Some("overview"));
        pages.add_named(&view_stack, Some("data"));

        // Content header. The title is the tab switcher for a wallet and a
        // plain title for the overview, which has no tabs.
        let switcher = adw::ViewSwitcher::builder()
            .policy(adw::ViewSwitcherPolicy::Wide)
            .stack(&view_stack)
            .build();
        let switcher_bar = adw::ViewSwitcherBar::builder().stack(&view_stack).build();
        let title_stack = gtk::Stack::new();
        title_stack.add_named(&switcher, Some("switcher"));
        title_stack.add_named(&adw::WindowTitle::new("Overview", ""), Some("title"));

        let sidebar_btn = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Toggle Sidebar (F9)")
            .build();
        let refresh_btn = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh (Ctrl+R)")
            .action_name("win.refresh")
            .sensitive(false)
            .build();
        let hide_btn = gtk::ToggleButton::builder().active(hidden).build();
        sync_hide_button(&hide_btn);

        let menu = gio::Menu::new();
        menu.append(Some("_About Morlaco"), Some("app.about"));
        menu.append(Some("_Quit"), Some("app.quit"));
        let menu_btn = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("Main Menu")
            .build();

        let content_header = adw::HeaderBar::builder().title_widget(&title_stack).build();
        content_header.pack_start(&sidebar_btn);
        content_header.pack_start(&refresh_btn);
        content_header.pack_end(&menu_btn);
        content_header.pack_end(&hide_btn);

        let content_toolbar = adw::ToolbarView::new();
        content_toolbar.add_top_bar(&content_header);
        content_toolbar.add_bottom_bar(&switcher_bar);
        content_toolbar.set_content(Some(&pages));

        let split = adw::OverlaySplitView::builder()
            .sidebar(&sidebar_toolbar)
            .content(&content_toolbar)
            .min_sidebar_width(240.0)
            .max_sidebar_width(360.0)
            .build();
        split
            .bind_property("show-sidebar", &sidebar_btn, "active")
            .bidirectional()
            .sync_create()
            .build();
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&split));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Morlaco")
            .default_width(900)
            .default_height(640)
            .width_request(360)
            .content(&toasts)
            .build();

        // Narrow layout: the sidebar becomes an overlay and the tabs move to
        // the bottom bar.
        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            600.0,
            adw::LengthUnit::Sp,
        ));
        narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
        narrow.add_setter(&switcher_bar, "reveal", Some(&true.to_value()));
        narrow.add_setter(&switcher, "visible", Some(&false.to_value()));
        window.add_breakpoint(narrow);

        Rc::new(Self {
            window,
            toasts,
            split,
            wallet_list,
            pages,
            title_stack,
            switcher_bar,
            loading_label,
            error_page,
            refresh_btn,
            hide_btn,
            overview,
            overview_wallets,
            overview_warning,
            holdings,
            lots_list,
            history_list,
            state: RefCell::new(State {
                store,
                selection: Selection::Overview,
                histories: HashMap::new(),
                price_now: None,
                portfolios: HashMap::new(),
                errors: HashMap::new(),
            }),
            generation: Cell::new(0),
            loading: Cell::new(false),
        })
    }

    fn connect_signals(self: &Rc<Self>) {
        let app = self
            .window
            .application()
            .expect("the window is created with an application");

        self.add_action("add-wallet", |ui| ui.show_add_dialog());
        self.add_action("refresh", |ui| ui.refresh());
        self.add_action("toggle-privacy", |ui| {
            ui.hide_btn.set_active(!ui.hide_btn.is_active());
        });
        self.add_action("toggle-sidebar", |ui| {
            ui.split.set_show_sidebar(!ui.split.shows_sidebar());
        });
        // Opens a wallet by index; used by the rows on the overview page.
        let show_wallet = gio::SimpleAction::new("show-wallet", Some(glib::VariantTy::UINT32));
        let ui = self.clone();
        show_wallet.connect_activate(move |_, param| {
            if let Some(index) = param.and_then(|p| p.get::<u32>()) {
                ui.select(Selection::Wallet(index as usize));
            }
        });
        self.window.add_action(&show_wallet);

        app.set_accels_for_action("win.add-wallet", &["<Primary>n"]);
        app.set_accels_for_action("win.refresh", &["<Primary>r", "F5"]);
        app.set_accels_for_action("win.toggle-privacy", &["<Primary>h"]);
        app.set_accels_for_action("win.toggle-sidebar", &["F9"]);

        let about = gio::SimpleAction::new("about", None);
        let window = self.window.clone();
        about.connect_activate(move |_, _| show_about(&window));
        app.add_action(&about);

        let quit = gio::SimpleAction::new("quit", None);
        let quit_app = app.clone();
        quit.connect_activate(move |_, _| quit_app.quit());
        app.add_action(&quit);
        app.set_accels_for_action("app.quit", &["<Primary>q"]);

        let ui = self.clone();
        self.hide_btn.connect_toggled(move |btn| {
            sync_hide_button(btn);
            {
                let mut state = ui.state.borrow_mut();
                state.store.hidden = btn.is_active();
                if let Err(err) = state.store.save() {
                    drop(state);
                    ui.toast(&format!("Couldn’t save settings: {err}"));
                }
            }
            ui.render();
        });

        let ui = self.clone();
        self.wallet_list.connect_row_activated(move |_, row| {
            // Row 0 is the overview; wallets follow in store order.
            let selection = match usize::try_from(row.index()) {
                Ok(0) => Selection::Overview,
                Ok(n) => Selection::Wallet(n - 1),
                Err(_) => return,
            };
            ui.select(selection);
        });
    }

    fn add_action(self: &Rc<Self>, name: &str, f: impl Fn(&Rc<Self>) + 'static) {
        let ui = self.clone();
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| f(&ui));
        self.window.add_action(&action);
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    // --- Sidebar ---------------------------------------------------------

    fn rebuild_sidebar(self: &Rc<Self>) {
        clear(&self.wallet_list);

        let overview = adw::ActionRow::builder()
            .title("Overview")
            .activatable(true)
            .build();
        overview.add_prefix(&gtk::Image::from_icon_name("go-home-symbolic"));
        self.wallet_list.append(&overview);

        for (index, wallet) in self.state.borrow().store.wallets.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(wallet.display_name())
                .use_markup(false)
                .activatable(true)
                .build();
            if !wallet.label.is_empty() {
                row.set_subtitle(&format::short_address(&wallet.address));
            }

            let rename = gtk::Button::builder()
                .icon_name("document-edit-symbolic")
                .tooltip_text("Rename Wallet")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let ui = self.clone();
            rename.connect_clicked(move |_| ui.show_rename_dialog(index));
            row.add_suffix(&rename);

            let remove = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Remove Wallet")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let ui = self.clone();
            remove.connect_clicked(move |_| ui.remove_wallet(index));
            row.add_suffix(&remove);
            self.wallet_list.append(&row);
        }
        self.sync_sidebar_selection();
    }

    fn sync_sidebar_selection(&self) {
        let row_index = match self.state.borrow().selection {
            Selection::Overview => 0,
            Selection::Wallet(i) => i + 1,
        };
        let row = i32::try_from(row_index)
            .ok()
            .and_then(|i| self.wallet_list.row_at_index(i));
        self.wallet_list.select_row(row.as_ref());
    }

    fn show_add_dialog(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Add Wallet"),
            Some("Enter a Bitcoin address to track. Only public blockchain data is read."),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("add", "_Add")]);
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("add", false);
        dialog.set_default_response(Some("add"));
        dialog.set_close_response("cancel");

        let address = adw::EntryRow::builder().title("Address").build();
        let label = adw::EntryRow::builder().title("Label (optional)").build();
        let fields = boxed_list();
        fields.append(&address);
        fields.append(&label);
        dialog.set_extra_child(Some(&fields));

        let dlg = dialog.clone();
        address.connect_changed(move |entry| {
            let valid = chain::check_address(entry.text().trim()).is_ok();
            dlg.set_response_enabled("add", valid);
        });

        let ui = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "add" {
                ui.add_wallet(address.text().trim(), label.text().as_str());
            }
        });
        dialog.present(Some(&self.window));
    }

    fn show_rename_dialog(self: &Rc<Self>, index: usize) {
        let Some(wallet) = self.state.borrow().store.wallets.get(index).cloned() else {
            return;
        };

        let dialog = adw::AlertDialog::new(
            Some("Rename Wallet"),
            Some(&format::short_address(&wallet.address)),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("save", "_Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");

        let name = adw::EntryRow::builder()
            .title("Label")
            .text(&wallet.label)
            .activates_default(true)
            .build();
        let fields = boxed_list();
        fields.append(&name);
        dialog.set_extra_child(Some(&fields));

        let ui = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "save" {
                ui.rename_wallet(index, name.text().as_str());
            }
        });
        dialog.present(Some(&self.window));
    }

    fn rename_wallet(self: &Rc<Self>, index: usize, label: &str) {
        {
            let mut state = self.state.borrow_mut();
            state.store.rename(index, label);
            if let Err(err) = state.store.save() {
                drop(state);
                self.toast(&format!("Couldn’t save wallets: {err}"));
            }
        }
        self.rebuild_sidebar();
        self.render();
    }

    fn add_wallet(self: &Rc<Self>, address: &str, label: &str) {
        let (index, existed) = {
            let mut state = self.state.borrow_mut();
            match state.store.position(address) {
                Some(i) => (i, true),
                None => {
                    let i = state.store.add(address, label);
                    if let Err(err) = state.store.save() {
                        drop(state);
                        self.toast(&format!("Couldn’t save wallets: {err}"));
                    }
                    (i, false)
                }
            }
        };
        if existed {
            self.toast("This wallet is already tracked");
        }
        self.rebuild_sidebar();
        self.select(Selection::Wallet(index));
    }

    fn remove_wallet(self: &Rc<Self>, index: usize) {
        let selection = {
            let mut state = self.state.borrow_mut();
            if let Some(wallet) = state.store.wallets.get(index) {
                let address = wallet.address.clone();
                state.histories.remove(&address);
                state.errors.remove(&address);
            }
            state.store.remove(index);
            // Wallets that were linked to the removed one now see its
            // transfers as sales and purchases.
            state.recompute();
            if let Err(err) = state.store.save() {
                drop(state);
                self.toast(&format!("Couldn’t save wallets: {err}"));
                state = self.state.borrow_mut();
            }
            match state.selection {
                Selection::Wallet(s) if s == index => Selection::Overview,
                Selection::Wallet(s) if s > index => Selection::Wallet(s - 1),
                other => other,
            }
        };
        self.rebuild_sidebar();
        self.select(selection);
    }

    // --- Selection and loading ------------------------------------------

    fn select(self: &Rc<Self>, selection: Selection) {
        self.state.borrow_mut().selection = selection;
        // Drop any load still running for the previous view.
        self.generation.set(self.generation.get() + 1);
        self.loading.set(false);

        self.sync_sidebar_selection();
        self.sync_chrome();
        if self.split.is_collapsed() {
            self.split.set_show_sidebar(false);
        }

        let missing = self.missing_addresses();
        if missing.is_empty() {
            self.show_current();
        } else {
            self.load(missing);
        }
    }

    /// Header controls that depend on what is selected.
    fn sync_chrome(&self) {
        let is_wallet = matches!(self.state.borrow().selection, Selection::Wallet(_));
        self.title_stack
            .set_visible_child_name(if is_wallet { "switcher" } else { "title" });
        self.switcher_bar.set_visible(is_wallet);
    }

    /// Addresses in the current view that are not loaded yet.
    fn missing_addresses(&self) -> Vec<String> {
        let state = self.state.borrow();
        let wanted: Vec<&Wallet> = match state.selection {
            Selection::Overview => state.store.wallets.iter().collect(),
            Selection::Wallet(i) => state.store.wallets.get(i).into_iter().collect(),
        };
        wanted
            .into_iter()
            .filter(|w| !state.histories.contains_key(&w.address))
            .map(|w| w.address.clone())
            .collect()
    }

    /// Reloads everything in the current view, ignoring what is cached.
    fn refresh(self: &Rc<Self>) {
        let addresses: Vec<String> = {
            let state = self.state.borrow();
            match state.selection {
                Selection::Overview => state
                    .store
                    .wallets
                    .iter()
                    .map(|w| w.address.clone())
                    .collect(),
                Selection::Wallet(i) => state
                    .store
                    .wallets
                    .get(i)
                    .map(|w| w.address.clone())
                    .into_iter()
                    .collect(),
            }
        };
        if !addresses.is_empty() {
            self.load(addresses);
        }
    }

    fn load(self: &Rc<Self>, addresses: Vec<String>) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.loading.set(true);

        self.pages.set_visible_child_name("loading");
        self.loading_label.set_label("Fetching transactions…");
        self.refresh_btn.set_sensitive(false);

        // The worker thread bumps these counters; the main loop reads them.
        let total = addresses.len();
        let done = Arc::new(AtomicUsize::new(0));
        let txs = Arc::new(AtomicUsize::new(0));
        let label = self.loading_label.clone();
        let (seen_done, seen_txs) = (done.clone(), txs.clone());
        let ticker = glib::timeout_add_local(Duration::from_millis(200), move || {
            let text = if total > 1 {
                let n = (seen_done.load(Ordering::Relaxed) + 1).min(total);
                format!("Loading wallet {n} of {total}…")
            } else {
                match seen_txs.load(Ordering::Relaxed) {
                    0 => "Fetching transactions…".to_owned(),
                    n => format!("Fetched {n} transactions…"),
                }
            };
            label.set_label(&text);
            glib::ControlFlow::Continue
        });

        let ui = self.clone();
        glib::spawn_future_local(async move {
            let names = addresses.clone();
            let outcome = gio::spawn_blocking(move || fetch_all(addresses, &done, &txs)).await;
            ticker.remove();
            let fetched =
                outcome.unwrap_or_else(|_| Fetched::failed(names, "the worker thread crashed"));
            ui.finish_load(generation, fetched);
        });
    }

    fn finish_load(self: &Rc<Self>, generation: u64, fetched: Fetched) {
        if generation != self.generation.get() {
            return;
        }
        self.loading.set(false);
        {
            let mut state = self.state.borrow_mut();
            if fetched.price_now.is_some() {
                state.price_now = fetched.price_now;
            }
            for (address, result) in fetched.wallets {
                match result {
                    Ok(history) => {
                        state.errors.remove(&address);
                        state.histories.insert(address, history);
                    }
                    Err(message) => {
                        state.histories.remove(&address);
                        state.errors.insert(address, message);
                    }
                }
            }
            state.recompute();
        }
        self.show_current();
    }

    // --- Rendering -------------------------------------------------------

    /// Redraws after a setting changes, unless a load is on screen.
    fn render(&self) {
        if !self.loading.get() {
            self.show_current();
        }
    }

    /// Picks the right page for the current selection and fills it.
    fn show_current(&self) {
        let hidden = self.state.borrow().store.hidden;
        let state = self.state.borrow();
        self.refresh_btn
            .set_sensitive(!state.store.wallets.is_empty());

        match state.selection {
            Selection::Overview => self.show_overview(&state, hidden),
            Selection::Wallet(i) => match state.store.wallets.get(i) {
                Some(wallet) => match state.portfolios.get(&wallet.address) {
                    Some(p) => {
                        self.show_wallet(wallet, p, hidden);
                        self.pages.set_visible_child_name("data");
                    }
                    None => self.show_error(state.errors.get(&wallet.address)),
                },
                None => self.pages.set_visible_child_name("empty"),
            },
        }
    }

    fn show_error(&self, message: Option<&String>) {
        self.error_page
            .set_description(Some(message.map_or("Unknown error", String::as_str)));
        self.pages.set_visible_child_name("error");
    }

    fn show_overview(&self, state: &State, hidden: bool) {
        let wallets = &state.store.wallets;
        if wallets.is_empty() {
            self.pages.set_visible_child_name("empty");
            return;
        }

        let loaded: Vec<(&Wallet, &Portfolio)> = wallets
            .iter()
            .filter_map(|w| state.portfolios.get(&w.address).map(|p| (w, p)))
            .collect();
        let Some((_, first)) = loaded.first() else {
            self.show_error(state.errors.values().next());
            return;
        };

        let total = Summary::combine(loaded.iter().map(|(_, p)| &p.summary));
        let count = match wallets.len() {
            1 => "1 wallet".to_owned(),
            n => format!("{n} wallets"),
        };
        self.overview
            .update("All Wallets", &count, &total, first.price_now_usd, hidden);

        let failed = wallets.len() - loaded.len();
        self.overview_warning.set_visible(failed > 0);
        self.overview_warning.set_label(&format!(
            "{failed} wallet(s) could not be loaded and are not included in the totals. \
             Use Refresh to try again."
        ));

        clear(&self.overview_wallets);
        for (index, wallet) in wallets.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(wallet.display_name())
                .use_markup(false)
                .activatable(true)
                .build();
            match state.portfolios.get(&wallet.address) {
                Some(p) => {
                    row.set_subtitle(&format::btc(p.summary.balance_sats, hidden));
                    let gain = gtk::Label::builder().css_classes(["numeric"]).build();
                    gain.set_label(&format::signed_usd(p.summary.unrealized_usd, hidden));
                    tone(&gain, p.summary.unrealized_usd, hidden);
                    row.add_suffix(&gain);
                }
                None => {
                    row.set_subtitle(
                        state
                            .errors
                            .get(&wallet.address)
                            .map_or("Not loaded", String::as_str),
                    );
                    row.add_css_class("error");
                }
            }
            let go = gtk::Image::from_icon_name("go-next-symbolic");
            row.add_suffix(&go);

            let window = self.window.clone();
            row.connect_activated(move |_| {
                // Reuse the sidebar's selection path so every view stays in sync.
                if let Err(err) = gtk::prelude::WidgetExt::activate_action(
                    &window,
                    "win.show-wallet",
                    Some(&(index as u32).to_variant()),
                ) {
                    glib::g_warning!("morlaco", "could not open wallet: {err}");
                }
            });
            self.overview_wallets.append(&row);
        }
        self.pages.set_visible_child_name("overview");
    }

    fn show_wallet(&self, wallet: &Wallet, p: &Portfolio, hidden: bool) {
        self.holdings.update(
            &wallet.display_name(),
            &format::short_address(&wallet.address),
            &p.summary,
            p.price_now_usd,
            hidden,
        );

        clear(&self.lots_list);
        if p.lots.is_empty() {
            self.lots_list.append(
                &adw::ActionRow::builder()
                    .title("No coins held")
                    .sensitive(false)
                    .build(),
            );
        }
        for lot in &p.lots {
            let pl = lot.unrealized_usd(p.price_now_usd);
            let row = adw::ActionRow::builder()
                .title(format::btc(lot.sats, hidden))
                .subtitle(format!(
                    "Acquired {} at {}",
                    date_string(lot.time),
                    format::usd(lot.unit_price_usd, false)
                ))
                .build();
            let label = gtk::Label::builder().css_classes(["numeric"]).build();
            label.set_label(&format::signed_usd(pl, hidden));
            tone(&label, pl, hidden);
            row.add_suffix(&label);
            self.lots_list.append(&row);
        }

        clear(&self.history_list);
        for entry in p.history.iter().rev() {
            let delta = &entry.delta;
            let kind = if entry.internal_sats > 0 {
                "Transfer between your wallets · "
            } else {
                ""
            };
            let row = adw::ActionRow::builder()
                .title(date_string(delta.time))
                .subtitle(format!(
                    "{kind}{} · BTC at {}",
                    format::short_address(&delta.txid),
                    format::usd(entry.price_usd, false)
                ))
                .use_markup(false)
                .activatable(true)
                .build();
            let label = gtk::Label::builder().css_classes(["numeric"]).build();
            label.set_label(&format::signed_btc(delta.net_sats, hidden));
            tone(&label, delta.net_sats as f64, hidden);
            row.add_suffix(&label);

            let uri = format!("{EXPLORER_TX_URL}{}", delta.txid);
            row.connect_activated(move |_| {
                // Opening a browser can fail on a headless system; nothing to recover.
                let _ = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
            });
            self.history_list.append(&row);
        }
    }
}

/// One wallet's outcome from the worker thread.
type WalletResult = (String, Result<History, String>);

/// What a load brings back.
struct Fetched {
    /// Current BTC/USD, or `None` when it could not be read, in which case
    /// every wallet is marked as failed because nothing can be valued.
    price_now: Option<f64>,
    wallets: Vec<WalletResult>,
}

impl Fetched {
    fn failed(addresses: Vec<String>, message: &str) -> Self {
        Self {
            price_now: None,
            wallets: addresses
                .into_iter()
                .map(|a| (a, Err(message.to_owned())))
                .collect(),
        }
    }
}

/// Reads blockchain data and prices for each address. Runs on a worker thread.
///
/// `done` counts finished wallets and `txs` the transactions fetched so far
/// for the current one.
fn fetch_all(addresses: Vec<String>, done: &AtomicUsize, txs: &AtomicUsize) -> Fetched {
    let cache = match open_cache() {
        Ok(cache) => cache,
        Err(message) => return Fetched::failed(addresses, &message),
    };
    let prices = CachedPrices::new(MempoolPrices, &cache);
    let price_now = match prices.usd_now() {
        Ok(price) => price,
        Err(err) => return Fetched::failed(addresses, &err.to_string()),
    };

    let wallets = addresses
        .into_iter()
        .map(|address| {
            txs.store(0, Ordering::Relaxed);
            let result = portfolio::fetch(&address, &prices, |n| txs.store(n, Ordering::Relaxed))
                .map_err(|e| e.to_string());
            done.fetch_add(1, Ordering::Relaxed);
            (address, result)
        })
        .collect();

    Fetched {
        price_now: Some(price_now),
        wallets,
    }
}

fn open_cache() -> Result<Cache, String> {
    let path = paths::cache_file().ok_or("no cache directory available")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    Cache::open(&path).map_err(|e| e.to_string())
}

fn boxed_list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn page_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build()
}

fn note(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["caption", "dim-label"])
        .wrap(true)
        .xalign(0.0)
        .build()
}

fn scrolled_clamp(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let clamp = adw::Clamp::builder().maximum_size(640).child(child).build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build()
}

/// Colors a figure green or red by sign. Masked figures stay neutral so the
/// color cannot leak whether the wallet is up or down.
fn tone(label: &gtk::Label, value: f64, hidden: bool) {
    label.remove_css_class("success");
    label.remove_css_class("error");
    if hidden || value == 0.0 {
        return;
    }
    label.add_css_class(if value > 0.0 { "success" } else { "error" });
}

/// Keeps the eye icon and tooltip in step with the toggle.
fn sync_hide_button(btn: &gtk::ToggleButton) {
    if btn.is_active() {
        btn.set_icon_name("view-reveal-symbolic");
        btn.set_tooltip_text(Some("Show Amounts (Ctrl+H)"));
    } else {
        btn.set_icon_name("view-conceal-symbolic");
        btn.set_tooltip_text(Some("Hide Amounts (Ctrl+H)"));
    }
}

fn show_about(parent: &adw::ApplicationWindow) {
    let about = adw::AboutDialog::builder()
        .application_name("Morlaco")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("Track a Bitcoin wallet and see its estimated gains and losses.")
        .developer_name("Jerónimo Albi")
        .build();
    about.present(Some(parent));
}
