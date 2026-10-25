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
use morlaco_core::chain::{Chain, WalletId};
use morlaco_core::db::Database;
use morlaco_core::lots::{Lot, Summary};
use morlaco_core::paths;
use morlaco_core::portfolio::{self, Entry, History, Portfolio};
use morlaco_core::price::{CachedPrices, LivePrices, PriceSource};
use morlaco_core::util::date_string;

use crate::encryption;
use crate::format;
use crate::keys::{Keyring, Keys, MIN_PASSPHRASE_ONLY_CHARS, Mode, format_recovery_key};
use crate::migrate;
use crate::store::{Store, Wallet};
use crate::system_keyring::PlatformKeyring;
use crate::unlock;

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
    /// Downloaded and priced history by wallet. Kept so switching views is
    /// instant.
    histories: HashMap<WalletId, History>,
    /// USD price of each chain's coin from the latest successful load.
    prices_now: HashMap<Chain, f64>,
    /// Derived from `histories` by [`State::recompute`], by wallet.
    portfolios: HashMap<WalletId, Portfolio>,
    /// Why a wallet failed to load, by wallet.
    errors: HashMap<WalletId, String>,
    /// What opens the database; worker threads need it for their own
    /// connections.
    keys: Keys,
}

impl State {
    /// Wallets in the current view that are not loaded yet.
    fn missing_wallets(&self) -> Vec<WalletId> {
        let wanted: Vec<&Wallet> = match self.selection {
            Selection::Overview => self.store.wallets.iter().collect(),
            Selection::Wallet(i) => self.store.wallets.get(i).into_iter().collect(),
        };
        wanted
            .into_iter()
            .filter(|w| !self.histories.contains_key(&w.id))
            .map(|w| w.id.clone())
            .collect()
    }

    /// Whether `wallets` can all be valued from what is already loaded: each
    /// has its history and its chain has a price.
    fn can_show(&self, wallets: &[WalletId]) -> bool {
        wallets
            .iter()
            .all(|w| self.histories.contains_key(w) && self.prices_now.contains_key(&w.chain))
    }

    /// Rebuilds every portfolio from the loaded histories.
    ///
    /// Wallets are valued together so that coins moved between them keep
    /// their original cost basis. Call after any history is added, replaced
    /// or removed.
    fn recompute(&mut self) {
        let loaded: Vec<&History> = self
            .store
            .wallets
            .iter()
            .filter_map(|w| self.histories.get(&w.id))
            .collect();
        let portfolios = portfolio::assemble(&loaded, &self.prices_now);
        self.portfolios = portfolios
            .into_iter()
            .map(|p| (p.wallet.clone(), p))
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
        // Not selectable: it is a short summary, and a selectable label takes
        // the window's first keyboard focus and shows a text caret.
        let subtitle = gtk::Label::builder()
            .css_classes(["dim-label", "caption"])
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
        let price = Figure::new("Price");
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

    /// Fills the figures. `coin` describes the one coin all of the figures
    /// are about; when they mix chains it is `None` and the rows that only
    /// make sense for a single coin are hidden.
    fn update(
        &self,
        title: &str,
        subtitle: &str,
        s: &Summary,
        coin: Option<CoinFigures>,
        hidden: bool,
    ) {
        self.heading.set_label(title);
        self.subtitle.set_label(subtitle);
        self.gain
            .set_label(&format::signed_usd(s.unrealized_usd, hidden));
        self.gain_pct
            .set_label(&format::percent(s.unrealized_pct, hidden));
        tone(&self.gain, s.unrealized_usd, hidden);
        tone(&self.gain_pct, s.unrealized_usd, hidden);

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

        self.balance.row.set_visible(coin.is_some());
        self.price.row.set_visible(coin.is_some());
        if let Some(coin) = coin {
            self.balance
                .value
                .set_label(&format::amount(coin.balance_units, coin.chain, hidden));
            self.price
                .row
                .set_title(&format!("{} price", coin.chain.symbol()));
            self.price
                .value
                .set_label(&format::usd(coin.price_usd, false));
        }
    }
}

/// The single-coin figures of a summary.
struct CoinFigures {
    chain: Chain,
    balance_units: u128,
    price_usd: f64,
}

/// The main window and everything behind it. Built by [`build_main_window`].
pub struct Ui {
    /// The window. The caller presents it.
    pub window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    split: adw::OverlaySplitView,
    wallet_list: gtk::ListBox,
    /// Shows the portfolio on screen and opens the list of portfolios.
    portfolio_btn: gtk::MenuButton,
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
    lots: PagedList,
    history: PagedList,
    state: RefCell<State>,
    /// Incremented on every load or selection so late answers are dropped.
    generation: Cell<u64>,
    /// Number of worker threads reading data. While any runs, the database
    /// must not be re-encrypted.
    running: Cell<u32>,
    /// Whether the full-page loading screen is what the user sees.
    showing_loading: Cell<bool>,
    /// Shown in the header while a refresh runs behind saved data.
    refresh_spinner: gtk::Spinner,
}

/// Builds the window, or raises it if it already exists.
pub fn present(app: &adw::Application) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }
    let main_app = app.clone();
    // The database may need a passphrase first, so the main window is built
    // once it is open.
    unlock::start(
        app,
        Rc::new(move |unlocked| open_main_window(&main_app, unlocked)),
    );
}

/// Builds the main window around `store` without presenting it.
///
/// With `refresh_on_start` the app fetches fresh data in the background, as
/// it does normally. Without it the window only shows what is already in the
/// store, which is what the screenshot example needs: no network.
pub fn build_main_window(
    app: &adw::Application,
    store: Store,
    keys: Keys,
    refresh_on_start: bool,
) -> Rc<Ui> {
    let ui = Ui::new(app, store, keys);
    ui.connect_signals();
    ui.rebuild_sidebar();
    ui.start(refresh_on_start);
    // Start with the sidebar focused, so no label grabs the focus and shows a
    // text caret. A focus ring only appears once the keyboard is used.
    if let Some(row) = ui.wallet_list.selected_row() {
        GtkWindowExt::set_focus(&ui.window, Some(&row));
    }
    ui
}

fn open_main_window(app: &adw::Application, unlocked: unlock::Unlocked) {
    match open_store(unlocked.db) {
        Ok(store) => {
            let ui = build_main_window(app, store, unlocked.keys, true);
            ui.window.present();
            if let Some(aside) = unlocked.set_aside {
                ui.toast_until_dismissed(&format!(
                    "Data from an earlier version can’t be read. It was kept as {} \
                     and Morlaco starts empty. Add your wallets again.",
                    aside.file_name().map_or_else(
                        || aside.display().to_string(),
                        |n| n.to_string_lossy().into_owned()
                    )
                ));
            }
            if let Some(notice) = unlocked.notice {
                ui.toast(&notice);
            }
            if unlocked.created_new_secret {
                ui.suggest_recovery_key();
            }
        }
        Err(message) => unlock::show_error(app, "Couldn’t Open Your Data", &message),
    }
}

/// Loads the saved wallets and settings from the open database.
fn open_store(db: Database) -> Result<Store, String> {
    let mut store = Store::open(db).map_err(|e| e.to_string())?;

    // A failed import is not fatal: the old files are kept and tried again
    // on the next start.
    if let Err(err) = migrate::import_legacy_data(&mut store) {
        glib::g_warning!("morlaco", "could not import earlier data: {err}");
    }
    Ok(store)
}

impl Ui {
    fn new(app: &adw::Application, store: Store, keys: Keys) -> Rc<Self> {
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
        let portfolio_btn = gtk::MenuButton::builder()
            .always_show_arrow(true)
            .css_classes(["flat"])
            .tooltip_text("Portfolios")
            .build();
        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.pack_start(&add_btn);
        sidebar_header.set_title_widget(Some(&portfolio_btn));
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
            "Coins moved between your wallets keep their original cost basis, even between \
             portfolios. Coins moved to or from any other wallet count as a sale or a purchase.",
        );
        let overview_box = page_box();
        overview_box.append(&overview.root);
        overview_box.append(&overview_warning);
        overview_box.append(&overview_group);
        overview_box.append(&overview_note);

        // Wallet pages
        let holdings = SummaryView::new();
        let lots = PagedList::new("lots");
        let lots_group = adw::PreferencesGroup::builder()
            .title("Lots")
            .description("Coins still held, oldest first")
            .build();
        lots_group.add(&lots.list);
        lots_group.add(&lots.footer);
        let holdings_box = page_box();
        holdings_box.append(&holdings.root);
        holdings_box.append(&lots_group);
        holdings_box.append(&note(
            "Cost basis is the market price of the coin on the day it arrived at \
             this address. It is an estimate, not what you paid.",
        ));

        let history = PagedList::new("transactions");
        let history_box = page_box();
        history_box.append(&history.list);
        history_box.append(&history.footer);

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
            .icon_name(crate::SYMBOLIC_ICON)
            .title("No Wallets Yet")
            .description("Add a wallet address to see its holdings and estimated gains.")
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
        let data = gio::Menu::new();
        data.append(Some("_Encryption…"), Some("win.encryption"));
        data.append(Some("_Recovery Key…"), Some("win.recovery-key"));
        data.append(Some("_Data Providers…"), Some("win.data-providers"));
        data.append(Some("_Clear Local Data…"), Some("win.clear-data"));
        menu.append_section(None, &data);
        let app_menu = gio::Menu::new();
        app_menu.append(Some("_About Morlaco"), Some("app.about"));
        app_menu.append(Some("_Quit"), Some("app.quit"));
        menu.append_section(None, &app_menu);
        let menu_btn = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("Main Menu")
            .build();

        let content_header = adw::HeaderBar::builder().title_widget(&title_stack).build();
        content_header.pack_start(&sidebar_btn);
        let refresh_spinner = gtk::Spinner::builder().visible(false).build();
        content_header.pack_start(&refresh_btn);
        content_header.pack_start(&refresh_spinner);
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
            portfolio_btn,
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
            lots,
            history,
            state: RefCell::new(State {
                store,
                keys,
                selection: Selection::Overview,
                histories: HashMap::new(),
                prices_now: HashMap::new(),
                portfolios: HashMap::new(),
                errors: HashMap::new(),
            }),
            generation: Cell::new(0),
            running: Cell::new(0),
            showing_loading: Cell::new(false),
            refresh_spinner,
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
        self.add_action("encryption", |ui| ui.show_encryption_dialog());
        self.add_action("recovery-key", |ui| ui.show_recovery_key_dialog());
        self.add_action("data-providers", |ui| ui.show_data_providers_dialog());
        self.add_action("clear-data", |ui| ui.confirm_clear_data());

        // Portfolios. The state of the radio action is the id on screen, so
        // the menu draws a check mark next to it.
        let active = self.state.borrow().store.active_portfolio().id;
        let select_portfolio = gio::SimpleAction::new_stateful(
            "select-portfolio",
            Some(glib::VariantTy::INT64),
            &active.to_variant(),
        );
        let ui = self.clone();
        select_portfolio.connect_activate(move |_, parameter| {
            if let Some(id) = parameter.and_then(|p| p.get::<i64>()) {
                ui.switch_portfolio(id);
            }
        });
        self.window.add_action(&select_portfolio);
        self.add_action("new-portfolio", |ui| ui.show_new_portfolio_dialog());
        self.add_action("rename-portfolio", |ui| ui.show_rename_portfolio_dialog());
        self.add_action("delete-portfolio", |ui| ui.show_delete_portfolio_dialog());
        self.sync_portfolio_ui();
        let ui = self.clone();
        self.history
            .more
            .connect_clicked(move |_| ui.show_more_history());
        let ui = self.clone();
        self.lots.more.connect_clicked(move |_| ui.show_more_lots());
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
            let saved = ui.state.borrow_mut().store.set_hidden(btn.is_active());
            if let Err(err) = saved {
                // Still apply it for this session.
                ui.state.borrow_mut().store.hidden = btn.is_active();
                ui.toast(&format!("Couldn’t save the setting: {err}"));
            }
            ui.render();
        });

        let ui = self.clone();
        self.wallet_list.connect_row_activated(move |_, row| {
            // Row 0 is the overview; the wallets of the portfolio on screen
            // follow in store order.
            let visible = ui.state.borrow().store.visible_wallets();
            let selection = match usize::try_from(row.index()) {
                Ok(0) => Selection::Overview,
                Ok(n) => match visible.get(n - 1) {
                    Some(&index) => Selection::Wallet(index),
                    None => return,
                },
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

    /// Reminds the user to save their recovery key, with a button that opens it.
    fn suggest_recovery_key(&self) {
        self.toasts.add_toast(
            adw::Toast::builder()
                .title("Your data is encrypted. Save your recovery key in case the keyring is ever reset.")
                .button_label("Save…")
                .action_name("win.recovery-key")
                .timeout(0)
                .build(),
        );
    }

    /// A message that stays until the user closes it, for things they need
    /// to notice.
    fn toast_until_dismissed(&self, message: &str) {
        self.toasts
            .add_toast(adw::Toast::builder().title(message).timeout(0).build());
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

        let state = self.state.borrow();
        // Naming the network only helps once there is more than one.
        let show_network = state.store.chains_in_use().len() > 1;
        for index in state.store.visible_wallets() {
            let wallet = &state.store.wallets[index];
            let row = adw::ActionRow::builder()
                .title(wallet.display_name())
                .use_markup(false)
                .activatable(true)
                .build();
            let mut subtitle = Vec::new();
            if show_network {
                subtitle.push(wallet.id.chain.name().to_owned());
            }
            if !wallet.label.is_empty() {
                subtitle.push(format::short_address(&wallet.id.address));
            }
            if !subtitle.is_empty() {
                row.set_subtitle(&subtitle.join(" · "));
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
        drop(state);
        self.sync_sidebar_selection();
    }

    fn sync_sidebar_selection(&self) {
        let state = self.state.borrow();
        let row_index = match state.selection {
            Selection::Overview => Some(0),
            // Wallets of the portfolio on screen follow the overview row.
            Selection::Wallet(i) => state
                .store
                .visible_wallets()
                .iter()
                .position(|&visible| visible == i)
                .map(|position| position + 1),
        };
        drop(state);
        let row = row_index
            .and_then(|i| i32::try_from(i).ok())
            .and_then(|i| self.wallet_list.row_at_index(i));
        self.wallet_list.select_row(row.as_ref());
    }

    fn show_add_dialog(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Add Wallet"),
            Some("Enter an address to track. Only public blockchain data is read."),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("add", "_Add")]);
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("add", false);
        dialog.set_default_response(Some("add"));
        dialog.set_close_response("cancel");

        // The network matters because one address format can exist on
        // several chains. With a single chain there is nothing to choose.
        let names: Vec<&str> = Chain::ALL.iter().map(|c| c.name()).collect();
        let network = adw::ComboRow::builder()
            .title("Network")
            .model(&gtk::StringList::new(&names))
            .visible(Chain::ALL.len() > 1)
            .build();
        let address = adw::EntryRow::builder().title("Address").build();
        let label = adw::EntryRow::builder().title("Label (optional)").build();
        let fields = boxed_list();
        fields.append(&network);
        fields.append(&address);
        fields.append(&label);
        dialog.set_extra_child(Some(&fields));

        let chosen_chain = {
            let network = network.clone();
            move || {
                Chain::ALL
                    .get(network.selected() as usize)
                    .copied()
                    .unwrap_or(Chain::Bitcoin)
            }
        };

        // Adding is possible once the address looks right for the network.
        let validate: Rc<dyn Fn()> = {
            let (address, dialog, chosen_chain) =
                (address.clone(), dialog.clone(), chosen_chain.clone());
            Rc::new(move || {
                let valid = chosen_chain().check_address(address.text().trim()).is_ok();
                dialog.set_response_enabled("add", valid);
            })
        };
        // Pasting an address picks its network, unless the one already chosen
        // fits it too (several chains may share an address format).
        let on_address = validate.clone();
        let detect_network = {
            let network = network.clone();
            move |entry: &adw::EntryRow| {
                let candidates = Chain::detect(entry.text().as_str());
                let current = Chain::ALL.get(network.selected() as usize).copied();
                let detected = match (candidates.first(), current) {
                    (Some(first), Some(now)) if !candidates.contains(&now) => Some(*first),
                    _ => None,
                };
                if let Some(chain) = detected
                    && let Some(position) = Chain::ALL.iter().position(|c| *c == chain)
                {
                    network.set_selected(u32::try_from(position).unwrap_or(0));
                }
            }
        };
        // Say the network was detected only while the one chosen fits.
        let sync_subtitle = {
            let (network, address, chosen_chain) =
                (network.clone(), address.clone(), chosen_chain.clone());
            Rc::new(move || {
                let fits = Chain::detect(address.text().as_str()).contains(&chosen_chain());
                network.set_subtitle(if fits {
                    "Detected from the address"
                } else {
                    ""
                });
            })
        };
        let on_address_subtitle = sync_subtitle.clone();
        address.connect_changed(move |entry| {
            detect_network(entry);
            on_address_subtitle();
            on_address();
        });
        network.connect_selected_notify(move |_| {
            sync_subtitle();
            validate();
        });

        let ui = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "add" {
                let wallet = WalletId::new(chosen_chain(), address.text().trim());
                ui.add_wallet(&wallet, label.text().as_str());
            }
        });
        dialog.present(Some(&self.window));
    }

    // --- Encryption ------------------------------------------------------

    fn show_encryption_dialog(self: &Rc<Self>) {
        if self.running.get() > 0 {
            self.toast("Wait for the refresh to finish first");
            return;
        }
        let mode = self.state.borrow().keys.mode();

        let current = match mode {
            Mode::Keyring => {
                "Your data is encrypted with a key kept in your system keyring. Nothing \
                 is asked when Morlaco starts."
            }
            Mode::KeyringAndPassphrase => {
                "Your data is encrypted with a key kept in your system keyring and a \
                 passphrase that you type every time Morlaco starts."
            }
            Mode::PassphraseOnly => {
                "Your data is encrypted with a passphrase alone, which you type every \
                 time Morlaco starts."
            }
        };
        let dialog = adw::AlertDialog::new(Some("Encryption"), Some(current));
        dialog.add_responses(&[("cancel", "_Cancel"), ("save", "_Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("save", false);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");

        let keep_in_keyring = adw::SwitchRow::builder()
            .title("Keep the key in the keyring")
            .subtitle("Recommended. Nothing to type unless you add a passphrase.")
            .active(mode != Mode::PassphraseOnly)
            .build();
        let new = adw::PasswordEntryRow::builder()
            .title("New passphrase")
            .build();
        let confirm = adw::PasswordEntryRow::builder()
            .title("Confirm passphrase")
            .build();
        let hint = gtk::Label::builder()
            .css_classes(["dim-label", "caption"])
            .wrap(true)
            .xalign(0.0)
            .build();
        let fields = boxed_list();
        fields.append(&keep_in_keyring);
        fields.append(&new);
        fields.append(&confirm);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        content.append(&fields);
        content.append(&hint);
        dialog.set_extra_child(Some(&content));

        // Saving is possible once the entries make sense, and the hint says
        // what the current choice means or what is wrong with it.
        let refresh: Rc<dyn Fn()> = {
            let (keep, new, confirm, hint, dialog) = (
                keep_in_keyring.clone(),
                new.clone(),
                confirm.clone(),
                hint.clone(),
                dialog.clone(),
            );
            Rc::new(move || {
                let keeping = keep.is_active();
                let checked = encryption::target_from_inputs(keeping, &new.text(), &confirm.text());
                dialog.set_response_enabled("save", checked.is_ok());

                let untouched = new.text().is_empty() && confirm.text().is_empty();
                let general = match (keeping, mode) {
                    (false, _) => format!(
                        "Without the keyring, your passphrase is the only protection. Use at \
                         least {MIN_PASSPHRASE_ONLY_CHARS} characters. If you forget it, your \
                         data can’t be recovered."
                    ),
                    (true, Mode::KeyringAndPassphrase) => {
                        "Leave both fields empty to remove the passphrase.".to_owned()
                    }
                    (true, _) => {
                        "Add a passphrase to also require one every time Morlaco starts.".to_owned()
                    }
                };
                let message = match checked {
                    Err(problem) if !untouched => problem.to_string(),
                    _ => general,
                };
                hint.set_label(&message);
            })
        };
        let on_new = refresh.clone();
        new.connect_changed(move |_| on_new());
        let on_confirm = refresh.clone();
        confirm.connect_changed(move |_| on_confirm());
        let on_switch = refresh.clone();
        keep_in_keyring.connect_active_notify(move |_| on_switch());
        refresh();

        let ui = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "save" {
                return;
            }
            let target = encryption::target_from_inputs(
                keep_in_keyring.is_active(),
                &new.text(),
                &confirm.text(),
            );
            match target {
                Ok(target) => ui.apply_protection(target),
                Err(problem) => ui.toast(&problem.to_string()),
            }
        });
        dialog.present(Some(&self.window));
    }

    fn apply_protection(self: &Rc<Self>, target: encryption::Target) {
        // No other connection may use the file while it is re-encrypted.
        if self.running.get() > 0 {
            self.toast("Wait for the refresh to finish first");
            return;
        }
        let before = self.state.borrow().keys.clone();
        let result = {
            let state = self.state.borrow();
            encryption::change_protection(&PlatformKeyring, &state.keys, target, |key| {
                state.store.rekey(key).map_err(|e| e.to_string())
            })
        };

        match result {
            Ok(changed) => {
                let message = if changed.keys == before {
                    "Nothing changed"
                } else {
                    match changed.keys.mode() {
                        Mode::Keyring => "Your key is now kept in the keyring",
                        Mode::KeyringAndPassphrase => {
                            "Your data now needs the keyring and a passphrase"
                        }
                        Mode::PassphraseOnly => "Your data now needs a passphrase only",
                    }
                };
                let new_secret = before.secret().is_none() && changed.keys.secret().is_some();
                self.state.borrow_mut().keys = changed.keys;
                self.toast(message);
                if let Some(warning) = changed.warning {
                    self.toast(&warning);
                }
                // A secret was just made, and it is the only thing that
                // opens the data if the keyring is ever lost.
                if new_secret {
                    self.suggest_recovery_key();
                }
            }
            Err(message) => self.toast(&format!("Couldn’t change the encryption: {message}")),
        }
    }

    /// Lets the user enter an Etherscan API key, which makes Ethereum wallets
    /// load from Etherscan instead of the keyless service.
    fn show_data_providers_dialog(self: &Rc<Self>) {
        let current = self
            .state
            .borrow()
            .store
            .etherscan_key()
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(
            Some("Data Providers"),
            Some(
                "Ethereum history comes from Blockscout, which needs no key but can be slow \
                 for busy addresses. With an Etherscan API key (free at etherscan.io/apis), \
                 Morlaco uses Etherscan instead, and Etherscan then sees your key and the \
                 addresses you look up. The key is saved in your encrypted data. Leave it \
                 empty to use Blockscout.",
            ),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("save", "_Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");

        let entry = adw::PasswordEntryRow::builder()
            .title("Etherscan API key")
            .text(&current)
            .build();
        let fields = boxed_list();
        fields.append(&entry);
        dialog.set_extra_child(Some(&fields));

        let ui = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "save" {
                return;
            }
            let key = entry.text().trim().to_owned();
            let saved = ui.state.borrow().store.set_etherscan_key(&key);
            match saved {
                Ok(()) if key.is_empty() => {
                    ui.toast("Using Blockscout. Refresh to reload Ethereum wallets.")
                }
                Ok(()) => ui.toast("Key saved. Refresh to reload Ethereum wallets with it."),
                Err(err) => ui.toast(&format!("Could not save the key: {err}")),
            }
        });
        dialog.present(Some(&self.window));
    }

    fn show_recovery_key_dialog(self: &Rc<Self>) {
        let keys = self.state.borrow().keys.clone();

        let Some(secret) = keys.secret() else {
            let dialog = adw::AlertDialog::new(
                Some("No Recovery Key"),
                Some(
                    "Your passphrase is the only key to your data, so there is no separate \
                     recovery key. If you forget the passphrase, your data can’t be recovered.",
                ),
            );
            dialog.add_response("close", "_Close");
            dialog.set_close_response("close");
            dialog.present(Some(&self.window));
            return;
        };

        let shown = format_recovery_key(secret);
        let passphrase_note = if keys.mode() == Mode::KeyringAndPassphrase {
            " You will still need your passphrase too."
        } else {
            ""
        };
        let dialog = adw::AlertDialog::new(
            Some("Recovery Key"),
            Some(&format!(
                "If your keyring is ever reset, or you move to another computer, this key \
                 lets you open your data again.{passphrase_note} Keep it somewhere safe and \
                 private, such as a password manager. Anyone who has this key and your data \
                 file can open your data."
            )),
        );
        dialog.add_responses(&[("close", "_Close"), ("copy", "_Copy")]);
        dialog.set_response_appearance("copy", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("copy"));
        dialog.set_close_response("close");
        dialog.set_extra_child(Some(
            &gtk::Label::builder()
                .label(&shown)
                .selectable(true)
                .wrap(true)
                .justify(gtk::Justification::Center)
                .css_classes(["monospace", "title-4"])
                .build(),
        ));

        let ui = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "copy" {
                ui.window.clipboard().set_text(&shown);
                ui.toast("Recovery key copied. A clipboard manager may keep a copy of it.");
            }
        });
        dialog.present(Some(&self.window));
    }

    fn confirm_clear_data(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some("Clear All Local Data?"),
            Some(
                "This deletes your tracked wallets, saved settings and the encryption \
                 key, then closes Morlaco. Nothing on the blockchain is affected. \
                 This can’t be undone.",
            ),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("clear", "_Clear Data")]);
        dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");

        let ui = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "clear" {
                ui.clear_data();
            }
        });
        dialog.present(Some(&self.window));
    }

    fn clear_data(self: &Rc<Self>) {
        let Some(path) = paths::data_file() else {
            self.toast("No data directory is available");
            return;
        };
        if let Err(message) = unlock::delete_database_files(&path) {
            self.toast(&format!("Couldn’t delete the data: {message}"));
            return;
        }
        // The data goes first. If the key then cannot be removed, the next
        // start finds a key and no data and simply begins again, whereas the
        // other order could leave data without its key.
        if let Err(message) = PlatformKeyring.clear() {
            glib::g_warning!("morlaco", "could not remove the key: {message}");
        }
        if let Some(app) = self.window.application() {
            app.quit();
        }
    }

    fn show_rename_dialog(self: &Rc<Self>, index: usize) {
        let Some(wallet) = self.state.borrow().store.wallets.get(index).cloned() else {
            return;
        };

        let dialog = adw::AlertDialog::new(
            Some("Rename Wallet"),
            Some(&format::short_address(&wallet.id.address)),
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

        // Moving a wallet only makes sense with somewhere to move it to.
        let portfolios = self.state.borrow().store.portfolios.clone();
        let portfolio_row = (portfolios.len() > 1).then(|| {
            let names: Vec<&str> = portfolios.iter().map(|p| p.name.as_str()).collect();
            let current = portfolios
                .iter()
                .position(|p| p.id == wallet.portfolio)
                .unwrap_or(0);
            adw::ComboRow::builder()
                .title("Portfolio")
                .model(&gtk::StringList::new(&names))
                .selected(u32::try_from(current).unwrap_or(0))
                .build()
        });
        if let Some(row) = &portfolio_row {
            fields.append(row);
        }
        dialog.set_extra_child(Some(&fields));

        let ui = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "save" {
                let portfolio = portfolio_row
                    .as_ref()
                    .and_then(|row| portfolios.get(row.selected() as usize))
                    .map(|p| p.id);
                ui.rename_wallet(index, name.text().as_str(), portfolio);
            }
        });
        dialog.present(Some(&self.window));
    }

    /// Saves a wallet's new label and, if `portfolio` is another one than it
    /// is in, moves it there. Its figures do not change: wallets are valued
    /// together whatever their portfolio.
    fn rename_wallet(self: &Rc<Self>, index: usize, label: &str, portfolio: Option<i64>) {
        let moved = {
            let mut state = self.state.borrow_mut();
            let current = state.store.wallets.get(index).map(|w| w.portfolio);
            let saved =
                state
                    .store
                    .rename(index, label)
                    .and_then(|()| match (portfolio, current) {
                        (Some(target), Some(now)) if target != now => {
                            state.store.move_wallet(index, target).map(|()| true)
                        }
                        _ => Ok(false),
                    });
            match saved {
                Ok(moved) => moved,
                Err(err) => {
                    drop(state);
                    self.toast(&format!("Couldn’t save the wallet: {err}"));
                    return;
                }
            }
        };
        self.rebuild_sidebar();
        if moved {
            // The wallet left the portfolio on screen.
            self.select(Selection::Overview);
        } else {
            self.render();
        }
    }

    // --- Portfolios ------------------------------------------------------

    /// Shows the portfolio on screen in the sidebar header, rebuilds the list
    /// of portfolios in its menu and turns Delete off when there is only one.
    fn sync_portfolio_ui(&self) {
        let state = self.state.borrow();
        let active = state.store.active_portfolio();
        self.portfolio_btn.set_label(&active.name);

        let list = gio::Menu::new();
        for portfolio in &state.store.portfolios {
            // An underscore in a menu label marks a shortcut letter.
            let item = gio::MenuItem::new(Some(&portfolio.name.replace('_', "__")), None);
            item.set_action_and_target_value(
                Some("win.select-portfolio"),
                Some(&portfolio.id.to_variant()),
            );
            list.append_item(&item);
        }
        let manage = gio::Menu::new();
        manage.append(Some("_New Portfolio…"), Some("win.new-portfolio"));
        manage.append(Some("_Rename Portfolio…"), Some("win.rename-portfolio"));
        manage.append(Some("_Delete Portfolio…"), Some("win.delete-portfolio"));
        let menu = gio::Menu::new();
        menu.append_section(None, &list);
        menu.append_section(None, &manage);
        self.portfolio_btn.set_menu_model(Some(&menu));

        let (active_id, several) = (active.id, state.store.portfolios.len() > 1);
        drop(state);
        gio::prelude::ActionGroupExt::change_action_state(
            &self.window,
            "select-portfolio",
            &active_id.to_variant(),
        );
        if let Some(delete) = self
            .window
            .lookup_action("delete-portfolio")
            .and_then(|action| action.downcast::<gio::SimpleAction>().ok())
        {
            delete.set_enabled(several);
        }
    }

    /// Puts another portfolio on screen, starting at its overview.
    fn switch_portfolio(self: &Rc<Self>, id: i64) {
        if self.state.borrow().store.active_portfolio().id == id {
            return;
        }
        let saved = self.state.borrow_mut().store.set_active(id);
        if let Err(err) = saved {
            self.toast(&format!("Couldn’t switch portfolio: {err}"));
            return;
        }
        self.sync_portfolio_ui();
        self.rebuild_sidebar();
        self.select(Selection::Overview);
    }

    /// Whether `name` can be given to a portfolio: not empty, and not the
    /// name of another one (`except`), ignoring case.
    fn portfolio_name_is_free(&self, name: &str, except: Option<i64>) -> bool {
        let name = name.trim();
        !name.is_empty()
            && !self
                .state
                .borrow()
                .store
                .portfolios
                .iter()
                .any(|p| Some(p.id) != except && p.name.eq_ignore_ascii_case(name))
    }

    fn show_new_portfolio_dialog(self: &Rc<Self>) {
        let dialog = self.portfolio_name_dialog(
            "New Portfolio",
            Some("A portfolio is a group of wallets, such as personal and business."),
            "_Create",
            "",
            None,
        );
        let ui = self.clone();
        dialog.connect_name_chosen(move |name| ui.create_portfolio(name));
        dialog.present(&self.window);
    }

    fn show_rename_portfolio_dialog(self: &Rc<Self>) {
        let portfolio = self.state.borrow().store.active_portfolio().clone();
        let dialog = self.portfolio_name_dialog(
            "Rename Portfolio",
            None,
            "_Save",
            &portfolio.name,
            Some(portfolio.id),
        );
        let ui = self.clone();
        dialog.connect_name_chosen(move |name| ui.rename_portfolio(portfolio.id, name));
        dialog.present(&self.window);
    }

    /// A dialog with one name field. Its button works once the name is not
    /// empty and not used by another portfolio.
    fn portfolio_name_dialog(
        self: &Rc<Self>,
        title: &str,
        body: Option<&str>,
        button: &str,
        initial: &str,
        except: Option<i64>,
    ) -> NameDialog {
        let dialog = adw::AlertDialog::new(Some(title), body);
        dialog.add_responses(&[("cancel", "_Cancel"), ("save", button)]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        dialog.set_response_enabled("save", false);

        let entry = adw::EntryRow::builder()
            .title("Name")
            .text(initial)
            .activates_default(true)
            .build();
        let fields = boxed_list();
        fields.append(&entry);
        dialog.set_extra_child(Some(&fields));

        let check = {
            let (ui, dialog) = (self.clone(), dialog.clone());
            move |entry: &adw::EntryRow| {
                let free = ui.portfolio_name_is_free(entry.text().as_str(), except);
                dialog.set_response_enabled("save", free);
            }
        };
        check(&entry);
        entry.connect_changed(check);
        NameDialog { dialog, entry }
    }

    fn create_portfolio(self: &Rc<Self>, name: &str) {
        let created = {
            let mut state = self.state.borrow_mut();
            state
                .store
                .add_portfolio(name)
                .and_then(|id| state.store.set_active(id))
        };
        if let Err(err) = created {
            self.toast(&format!("Couldn’t create the portfolio: {err}"));
        }
        // Even after a failed switch the list may have changed.
        self.sync_portfolio_ui();
        self.rebuild_sidebar();
        self.select(Selection::Overview);
    }

    fn rename_portfolio(self: &Rc<Self>, id: i64, name: &str) {
        let saved = self.state.borrow_mut().store.rename_portfolio(id, name);
        if let Err(err) = saved {
            self.toast(&format!("Couldn’t rename the portfolio: {err}"));
            return;
        }
        self.sync_portfolio_ui();
        self.render();
    }

    fn show_delete_portfolio_dialog(self: &Rc<Self>) {
        let (portfolio, wallets) = {
            let state = self.state.borrow();
            let portfolio = state.store.active_portfolio().clone();
            let wallets = state
                .store
                .wallets
                .iter()
                .filter(|w| w.portfolio == portfolio.id)
                .count();
            (portfolio, wallets)
        };
        let body = match wallets {
            0 => "This portfolio has no wallets.".to_owned(),
            n => format!(
                "This removes {} from Morlaco, with their saved transactions. Your coins are not \
                 affected. Coins moved between {} and your other portfolios will count as a sale \
                 or a purchase there.",
                if n == 1 {
                    "its wallet".to_owned()
                } else {
                    format!("its {n} wallets")
                },
                if n == 1 { "it" } else { "them" },
            ),
        };
        let dialog =
            adw::AlertDialog::new(Some(&format!("Delete “{}”?", portfolio.name)), Some(&body));
        dialog.add_responses(&[("cancel", "_Cancel"), ("delete", "_Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let ui = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "delete" {
                ui.delete_portfolio(portfolio.id);
            }
        });
        dialog.present(Some(&self.window));
    }

    fn delete_portfolio(self: &Rc<Self>, id: i64) {
        {
            let mut state = self.state.borrow_mut();
            match state.store.remove_portfolio(id) {
                Ok(removed) => {
                    for wallet in &removed {
                        state.histories.remove(wallet);
                        state.errors.remove(wallet);
                    }
                    // Wallets elsewhere that were linked to these now see the
                    // transfers as sales and purchases.
                    state.recompute();
                }
                Err(err) => {
                    drop(state);
                    self.toast(&format!("Couldn’t delete the portfolio: {err}"));
                    return;
                }
            }
        }
        self.sync_portfolio_ui();
        self.rebuild_sidebar();
        self.select(Selection::Overview);
    }

    fn add_wallet(self: &Rc<Self>, wallet: &WalletId, label: &str) {
        let existing = self.state.borrow().store.position(wallet);
        let index = match existing {
            Some(i) => {
                self.toast("This wallet is already tracked");
                i
            }
            None => {
                let added = self.state.borrow_mut().store.add(wallet, label);
                match added {
                    Ok(i) => i,
                    Err(err) => {
                        self.toast(&format!("Couldn’t add the wallet: {err}"));
                        return;
                    }
                }
            }
        };
        self.rebuild_sidebar();
        self.select(Selection::Wallet(index));
    }

    fn remove_wallet(self: &Rc<Self>, index: usize) {
        let selection = {
            let mut state = self.state.borrow_mut();
            let removed = state.store.wallets.get(index).map(|w| w.id.clone());
            if let Err(err) = state.store.remove(index) {
                drop(state);
                self.toast(&format!("Couldn’t remove the wallet: {err}"));
                return;
            }
            if let Some(id) = removed {
                state.histories.remove(&id);
                state.errors.remove(&id);
            }
            // Wallets that were linked to the removed one now see its
            // transfers as sales and purchases.
            state.recompute();
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
        // A wallet in another portfolio brings that portfolio on screen.
        if let Selection::Wallet(i) = selection {
            let target = {
                let state = self.state.borrow();
                state
                    .store
                    .wallets
                    .get(i)
                    .map(|w| w.portfolio)
                    .filter(|portfolio| *portfolio != state.store.active_portfolio().id)
            };
            if let Some(portfolio) = target {
                if let Err(err) = self.state.borrow_mut().store.set_active(portfolio) {
                    self.toast(&format!("Couldn’t save the setting: {err}"));
                }
                self.sync_portfolio_ui();
                self.rebuild_sidebar();
            }
        }
        self.state.borrow_mut().selection = selection;
        self.lots.reset();
        self.history.reset();

        self.sync_sidebar_selection();
        self.sync_chrome();
        if self.split.is_collapsed() {
            self.split.set_show_sidebar(false);
        }

        let missing = self.missing_wallets();
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

    /// Wallets in the current view that are not loaded yet.
    fn missing_wallets(&self) -> Vec<WalletId> {
        self.state.borrow().missing_wallets()
    }

    /// Reloads everything in the current view, ignoring what is cached.
    fn refresh(self: &Rc<Self>) {
        let wallets: Vec<WalletId> = {
            let state = self.state.borrow();
            match state.selection {
                Selection::Overview => state.store.wallets.iter().map(|w| w.id.clone()).collect(),
                Selection::Wallet(i) => state
                    .store
                    .wallets
                    .get(i)
                    .map(|w| w.id.clone())
                    .into_iter()
                    .collect(),
            }
        };
        if !wallets.is_empty() {
            self.load(wallets);
        }
    }

    fn load(self: &Rc<Self>, wallets: Vec<WalletId>) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.running.set(self.running.get() + 1);

        // With saved data to show, refresh quietly behind it. Otherwise
        // there is nothing to look at yet, so show a loading screen.
        let background = self.state.borrow().can_show(&wallets);
        if !background {
            self.showing_loading.set(true);
            self.pages.set_visible_child_name("loading");
            self.loading_label.set_label("Fetching transactions…");
        }
        self.refresh_btn.set_sensitive(false);
        self.refresh_spinner.set_visible(true);
        self.refresh_spinner.set_spinning(true);

        // The worker thread bumps these counters; the main loop reads them.
        let total = wallets.len();
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

        let key = self.state.borrow().keys.database_key();
        let ui = self.clone();
        glib::spawn_future_local(async move {
            let names = wallets.clone();
            let outcome = gio::spawn_blocking(move || fetch_all(wallets, &key, &done, &txs)).await;
            ticker.remove();
            let fetched =
                outcome.unwrap_or_else(|_| Fetched::failed(names, "the worker thread crashed"));
            ui.finish_load(generation, fetched);
        });
    }

    fn finish_load(self: &Rc<Self>, generation: u64, fetched: Fetched) {
        self.running.set(self.running.get().saturating_sub(1));
        let latest = generation == self.generation.get();

        // Results are always kept: they are keyed by wallet, so a load that
        // was overtaken by a newer one is still correct.
        let mut kept_saved_data = None;
        {
            let mut state = self.state.borrow_mut();
            for (chain, price) in fetched.prices_now {
                state.prices_now.insert(chain, price);
                if let Err(err) = state.store.set_last_price(chain, price) {
                    glib::g_warning!("morlaco", "could not save the price: {err}");
                }
            }
            for (wallet, result) in fetched.wallets {
                match result {
                    Ok(history) => {
                        state.errors.remove(&wallet);
                        state.histories.insert(wallet, history);
                    }
                    // A failed refresh must not throw away data we have.
                    Err(message) if state.histories.contains_key(&wallet) => {
                        kept_saved_data.get_or_insert(message);
                    }
                    Err(message) => {
                        state.errors.insert(wallet, message);
                    }
                }
            }
            state.recompute();
        }

        if self.running.get() == 0 {
            self.refresh_spinner.set_spinning(false);
            self.refresh_spinner.set_visible(false);
            let has_wallets = !self.state.borrow().store.wallets.is_empty();
            self.refresh_btn.set_sensitive(has_wallets);
        }
        // Do not replace the loading screen of a newer load.
        if latest || !self.showing_loading.get() {
            self.show_current();
        }
        if let Some(message) = kept_saved_data {
            self.toast(&format!("Couldn’t refresh: {message}. Showing saved data."));
        }
    }

    /// Fills the app from the database so it can be shown before any
    /// network request.
    fn restore_saved_data(&self) {
        let mut state = self.state.borrow_mut();
        let wallets: Vec<WalletId> = state.store.wallets.iter().map(|w| w.id.clone()).collect();
        for wallet in wallets {
            match state.store.saved_history(&wallet) {
                Ok(Some(history)) => {
                    state.histories.insert(wallet, history);
                }
                Ok(None) => {}
                Err(err) => glib::g_warning!("morlaco", "could not read saved data: {err}"),
            }
        }
        for chain in Chain::ALL {
            if let Some(price) = state.store.last_price(*chain) {
                state.prices_now.insert(*chain, price);
            }
        }
        state.recompute();
    }

    /// Shows what is saved at once, then refreshes it from the network when
    /// `refresh` is set.
    fn start(self: &Rc<Self>, refresh: bool) {
        self.restore_saved_data();
        self.sync_sidebar_selection();
        self.sync_chrome();

        let ready = {
            let state = self.state.borrow();
            let wallets: Vec<WalletId> = state.store.wallets.iter().map(|w| w.id.clone()).collect();
            wallets.is_empty() || state.can_show(&wallets)
        };
        if ready {
            self.show_current();
        }
        if refresh {
            self.refresh();
        }
    }

    // --- Rendering -------------------------------------------------------

    /// Redraws after a setting changes, unless a load is on screen.
    fn render(&self) {
        if !self.showing_loading.get() {
            self.show_current();
        }
    }

    /// Picks the right page for the current selection and fills it.
    fn show_current(&self) {
        self.showing_loading.set(false);
        let hidden = self.state.borrow().store.hidden;
        let state = self.state.borrow();
        self.refresh_btn
            .set_sensitive(!state.store.wallets.is_empty() && self.running.get() == 0);

        match state.selection {
            Selection::Overview => self.show_overview(&state, hidden),
            Selection::Wallet(i) => match state.store.wallets.get(i) {
                Some(wallet) => match state.portfolios.get(&wallet.id) {
                    Some(p) => {
                        self.show_wallet(wallet, p, hidden);
                        self.pages.set_visible_child_name("data");
                    }
                    None => self.show_error(state.errors.get(&wallet.id)),
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
        // The wallets of the portfolio on screen, with their place in the
        // store.
        let wallets: Vec<(usize, &Wallet)> = state
            .store
            .visible_wallets()
            .into_iter()
            .map(|index| (index, &state.store.wallets[index]))
            .collect();
        if wallets.is_empty() {
            self.pages.set_visible_child_name("empty");
            return;
        }

        let loaded: Vec<(&Wallet, &Portfolio)> = wallets
            .iter()
            .filter_map(|(_, w)| state.portfolios.get(&w.id).map(|p| (*w, p)))
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
        // Balance and price only make sense when every wallet holds the same
        // coin.
        let chain = first.wallet.chain;
        let coin = loaded
            .iter()
            .all(|(w, _)| w.id.chain == chain)
            .then(|| CoinFigures {
                chain,
                balance_units: loaded.iter().map(|(_, p)| p.balance_units).sum(),
                price_usd: first.price_now_usd,
            });
        self.overview.update(
            &state.store.active_portfolio().name,
            &count,
            &total,
            coin,
            hidden,
        );

        let failed = wallets.len() - loaded.len();
        self.overview_warning.set_visible(failed > 0);
        self.overview_warning.set_label(&format!(
            "{failed} wallet(s) could not be loaded and are not included in the totals. \
             Use Refresh to try again."
        ));

        clear(&self.overview_wallets);
        for (index, wallet) in wallets.iter().copied() {
            let row = adw::ActionRow::builder()
                .title(wallet.display_name())
                .use_markup(false)
                .activatable(true)
                .build();
            match state.portfolios.get(&wallet.id) {
                Some(p) => {
                    row.set_subtitle(&format::amount(p.balance_units, wallet.id.chain, hidden));
                    let gain = gtk::Label::builder().css_classes(["numeric"]).build();
                    gain.set_label(&format::signed_usd(p.summary.unrealized_usd, hidden));
                    tone(&gain, p.summary.unrealized_usd, hidden);
                    row.add_suffix(&gain);
                }
                None => {
                    row.set_subtitle(
                        state
                            .errors
                            .get(&wallet.id)
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
        let chain = wallet.id.chain;
        self.holdings.update(
            &wallet.display_name(),
            &format::short_address(&wallet.id.address),
            &p.summary,
            Some(CoinFigures {
                chain,
                balance_units: p.balance_units,
                price_usd: p.price_now_usd,
            }),
            hidden,
        );

        self.lots.fill(p.lots.len(), p.lots.iter(), |lot| {
            lot_row(lot, p.price_now_usd, chain, hidden)
        });
        if p.lots.is_empty() {
            self.lots.list.append(
                &adw::ActionRow::builder()
                    .title("No coins held")
                    .sensitive(false)
                    .build(),
            );
        }
        self.history
            .fill(p.history.len(), p.history.iter().rev(), |entry| {
                history_row(entry, chain, hidden)
            });
    }

    /// Adds the next page of lots to the list.
    fn show_more_lots(&self) {
        self.with_selected_portfolio(|wallet, p, hidden| {
            let chain = wallet.id.chain;
            self.lots.extend(p.lots.len(), p.lots.iter(), |lot| {
                lot_row(lot, p.price_now_usd, chain, hidden)
            });
        });
    }

    /// Adds the next page of transactions to the list.
    fn show_more_history(&self) {
        self.with_selected_portfolio(|wallet, p, hidden| {
            let chain = wallet.id.chain;
            self.history
                .extend(p.history.len(), p.history.iter().rev(), |entry| {
                    history_row(entry, chain, hidden)
                });
        });
    }

    /// Runs `f` with the wallet that is open, its figures and whether numbers
    /// are hidden. Does nothing if no wallet with figures is open.
    fn with_selected_portfolio(&self, f: impl FnOnce(&Wallet, &Portfolio, bool)) {
        let state = self.state.borrow();
        let Selection::Wallet(i) = state.selection else {
            return;
        };
        let Some(wallet) = state.store.wallets.get(i) else {
            return;
        };
        if let Some(p) = state.portfolios.get(&wallet.id) {
            f(wallet, p, state.store.hidden);
        }
    }
}

/// A dialog with a single name field.
struct NameDialog {
    dialog: adw::AlertDialog,
    entry: adw::EntryRow,
}

impl NameDialog {
    /// Calls `f` with the name when the user confirms.
    fn connect_name_chosen(&self, f: impl Fn(&str) + 'static) {
        let entry = self.entry.clone();
        self.dialog.connect_response(None, move |_, response| {
            if response == "save" {
                f(entry.text().as_str());
            }
        });
    }

    fn present(&self, parent: &adw::ApplicationWindow) {
        self.dialog.present(Some(parent));
    }
}

/// A list that shows its rows a page at a time, with a caption and a "Show
/// More" button under it. A busy wallet has thousands of lots and
/// transactions, and a row is a whole widget.
struct PagedList {
    list: gtk::ListBox,
    /// The caption and the button.
    footer: gtk::Box,
    more: gtk::Button,
    caption: gtk::Label,
    /// How many rows the list may show. Grows by `LIST_PAGE` with each "Show
    /// More" and starts over when another wallet is opened.
    limit: Cell<usize>,
    /// What the rows are, for the caption.
    noun: &'static str,
}

/// How many rows a list shows at first, and adds with each "Show More".
const LIST_PAGE: usize = 50;

impl PagedList {
    fn new(noun: &'static str) -> Self {
        let caption = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .build();
        let more = gtk::Button::builder()
            .label("Show More")
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .build();
        let footer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .build();
        footer.append(&caption);
        footer.append(&more);
        Self {
            list: boxed_list(),
            footer,
            more,
            caption,
            limit: Cell::new(LIST_PAGE),
            noun,
        }
    }

    /// Goes back to the first page.
    fn reset(&self) {
        self.limit.set(LIST_PAGE);
    }

    /// Replaces the rows with the first `limit` of `items`, which are in the
    /// order they are shown and `total` long.
    fn fill<T>(
        &self,
        total: usize,
        items: impl Iterator<Item = T>,
        row: impl Fn(T) -> adw::ActionRow,
    ) {
        clear(&self.list);
        self.append(total, 0, items, row);
    }

    /// Adds one more page after the rows already shown. They stay, so the
    /// scroll position does not move.
    fn extend<T>(
        &self,
        total: usize,
        items: impl Iterator<Item = T>,
        row: impl Fn(T) -> adw::ActionRow,
    ) {
        let already = self.limit.get().min(total);
        self.limit.set(already + LIST_PAGE);
        self.append(total, already, items, row);
    }

    /// Adds the rows after the first `skip`, up to the limit, and updates the
    /// footer.
    fn append<T>(
        &self,
        total: usize,
        skip: usize,
        items: impl Iterator<Item = T>,
        row: impl Fn(T) -> adw::ActionRow,
    ) {
        let shown = self.limit.get().min(total);
        for item in items.skip(skip).take(shown - skip) {
            self.list.append(&row(item));
        }
        self.footer.set_visible(shown < total);
        self.caption
            .set_label(&format::showing_of(shown, total, self.noun));
    }
}

/// One lot as a row: what is left of it and what that is worth against its cost.
fn lot_row(lot: &Lot, price_now_usd: f64, chain: Chain, hidden: bool) -> adw::ActionRow {
    let pl = lot.unrealized_usd(price_now_usd);
    let row = adw::ActionRow::builder()
        .title(format::amount(lot.units, chain, hidden))
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
    row
}

/// One transaction as a row that opens it in the block explorer.
fn history_row(entry: &Entry, chain: Chain, hidden: bool) -> adw::ActionRow {
    let activity = &entry.activity;
    let kind = if entry.internal_units > 0 {
        "Transfer between your wallets · "
    } else {
        ""
    };
    let row = adw::ActionRow::builder()
        .title(date_string(activity.time))
        .subtitle(format!(
            "{kind}{} · {} at {}",
            format::short_address(&activity.txid),
            chain.symbol(),
            format::usd(entry.price_usd, false)
        ))
        .use_markup(false)
        .activatable(true)
        .build();
    let label = gtk::Label::builder().css_classes(["numeric"]).build();
    // What the balance changed by, which includes the fee.
    let change = activity.amount - activity.fee as i128;
    label.set_label(&format::signed_amount(change, chain, hidden));
    tone(&label, change as f64, hidden);
    row.add_suffix(&label);

    let uri = chain.explorer_tx_url(&activity.txid);
    row.connect_activated(move |_| {
        // Opening a browser can fail on a headless system; nothing to recover.
        let _ = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
    });
    row
}

/// One wallet's outcome from the worker thread.
type WalletResult = (WalletId, Result<History, String>);

/// What a load brings back.
struct Fetched {
    /// Current USD price of each chain's coin that could be read.
    prices_now: HashMap<Chain, f64>,
    wallets: Vec<WalletResult>,
}

impl Fetched {
    fn failed(wallets: Vec<WalletId>, message: &str) -> Self {
        Self {
            prices_now: HashMap::new(),
            wallets: wallets
                .into_iter()
                .map(|w| (w, Err(message.to_owned())))
                .collect(),
        }
    }
}

/// Reads blockchain data and prices for each wallet. Runs on a worker thread.
///
/// `done` counts finished wallets and `txs` the transactions fetched so far
/// for the current one. A wallet whose chain price cannot be read fails,
/// because nothing can be valued without it.
fn fetch_all(wallets: Vec<WalletId>, key: &str, done: &AtomicUsize, txs: &AtomicUsize) -> Fetched {
    let cache = match open_database(key) {
        Ok(cache) => cache,
        Err(message) => return Fetched::failed(wallets, &message),
    };
    let prices = CachedPrices::new(LivePrices, &cache);

    // One price lookup per chain, however many wallets it has.
    let mut now: HashMap<Chain, Result<f64, String>> = HashMap::new();
    for wallet in &wallets {
        now.entry(wallet.chain)
            .or_insert_with(|| prices.usd_now(wallet.chain).map_err(|e| e.to_string()));
    }

    let results = wallets
        .into_iter()
        .map(|wallet| {
            txs.store(0, Ordering::Relaxed);
            let result = match &now[&wallet.chain] {
                Ok(_) => portfolio::fetch(&wallet, &prices, &cache, |n| {
                    txs.store(n, Ordering::Relaxed)
                })
                .map_err(|e| e.to_string()),
                Err(message) => Err(message.clone()),
            };
            done.fetch_add(1, Ordering::Relaxed);
            (wallet, result)
        })
        .collect();

    Fetched {
        prices_now: now
            .into_iter()
            .filter_map(|(chain, price)| price.ok().map(|p| (chain, p)))
            .collect(),
        wallets: results,
    }
}

/// Opens a connection for the worker thread; the main thread keeps its own.
fn open_database(key: &str) -> Result<Database, String> {
    let path = paths::data_file().ok_or("no data directory available")?;
    Database::open(&path, key).map_err(|e| e.to_string())
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
        .application_icon(crate::APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .comments("Track your crypto wallets and see their estimated gains and losses.")
        .developer_name("Jerónimo Albi")
        .build();
    about.present(Some(parent));
}
