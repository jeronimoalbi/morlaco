//! `morlaco <address>`: print holdings and estimated gain/loss for a Bitcoin address.

use anyhow::{Context, Result, bail};
use morlaco_core::cache::Cache;
use morlaco_core::lots::SATS_PER_BTC;
use morlaco_core::paths;
use morlaco_core::portfolio::{self, Portfolio};
use morlaco_core::price::{CachedPrices, MempoolPrices};
use morlaco_core::util::date_string;

fn main() -> Result<()> {
    let Some(address) = std::env::args().nth(1) else {
        bail!("usage: morlaco <bitcoin-address>");
    };

    let path = paths::cache_file().context("cannot locate a cache directory (set HOME)")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let cache = Cache::open(&path).context("opening the price cache")?;
    let prices = CachedPrices::new(MempoolPrices, &cache);

    let portfolio = portfolio::build(&address, &prices, |n| {
        eprint!("\rfetched {n} transactions...");
    })
    .context("building the portfolio")?;
    eprintln!();

    print_portfolio(&portfolio);
    Ok(())
}

fn print_portfolio(p: &Portfolio) {
    let s = &p.summary;
    println!("Address        {}", p.address);
    println!(
        "Balance        {:.8} BTC",
        s.balance_sats as f64 / SATS_PER_BTC
    );
    println!("BTC price      ${:.2}", p.price_now_usd);
    println!("Value          ${:.2}", s.value_usd);
    println!("Cost basis     ${:.2}", s.cost_basis_usd);
    match s.unrealized_pct {
        Some(pct) => println!("Unrealized     {:+.2} USD ({pct:+.2}%)", s.unrealized_usd),
        None => println!("Unrealized     {:+.2} USD", s.unrealized_usd),
    }
    println!("Realized       {:+.2} USD", s.realized_usd);

    if p.lots.is_empty() {
        return;
    }
    println!(
        "\n{:<12} {:>14} {:>12} {:>12}",
        "Acquired", "Amount (BTC)", "Buy price", "P/L (USD)"
    );
    for lot in &p.lots {
        println!(
            "{:<12} {:>14.8} {:>12.2} {:>+12.2}",
            date_string(lot.time),
            lot.btc(),
            lot.unit_price_usd,
            lot.unrealized_usd(p.price_now_usd),
        );
    }
}
