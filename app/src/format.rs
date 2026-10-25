//! Display formatting. Every amount goes through here so privacy mode can
//! mask it in one place.

use morlaco_core::chain::Chain;

/// Shown in place of any masked number.
pub const MASK: &str = "•••••";

/// Inserts thousands separators into a string of digits.
fn group(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1234.5` becomes `$1,234.50`.
fn dollars(value: f64) -> String {
    let fixed = format!("{:.2}", value.abs());
    let (int, frac) = fixed.split_once('.').unwrap_or((&fixed, "00"));
    format!("${}.{frac}", group(int))
}

fn sign(value: f64) -> &'static str {
    if value < 0.0 { "−" } else { "+" }
}

/// Plain dollar amount, e.g. `$52,300.00`.
pub fn usd(value: f64, hidden: bool) -> String {
    if hidden {
        MASK.to_owned()
    } else if value < 0.0 {
        format!("−{}", dollars(value))
    } else {
        dollars(value)
    }
}

/// Dollar amount with an explicit sign, e.g. `+$11,300.00`.
pub fn signed_usd(value: f64, hidden: bool) -> String {
    if hidden {
        MASK.to_owned()
    } else {
        format!("{}{}", sign(value), dollars(value))
    }
}

/// Percentage with an explicit sign, or an em dash when there is no cost.
pub fn percent(pct: Option<f64>, hidden: bool) -> String {
    match pct {
        _ if hidden => MASK.to_owned(),
        Some(p) => format!("{}{:.2}%", sign(p), p.abs()),
        None => "—".to_owned(),
    }
}

/// Most decimals shown for a coin amount. Bitcoin has 8; chains with more
/// (18 for Ethereum) are rounded to keep amounts readable.
const MAX_SHOWN_DECIMALS: u8 = 8;

fn coins_text(units: u128, chain: Chain) -> String {
    let decimals = usize::from(chain.decimals().min(MAX_SHOWN_DECIMALS));
    format!("{:.*}", decimals, chain.to_coins(units))
}

/// Amount of a chain's coin from its smallest units, e.g. `0.52000000 BTC`.
pub fn amount(units: u128, chain: Chain, hidden: bool) -> String {
    if hidden {
        MASK.to_owned()
    } else {
        format!("{} {}", coins_text(units, chain), chain.symbol())
    }
}

/// Amount with an explicit sign.
pub fn signed_amount(units: i128, chain: Chain, hidden: bool) -> String {
    if hidden {
        MASK.to_owned()
    } else {
        let sign = if units < 0 { "−" } else { "+" };
        format!(
            "{sign}{} {}",
            coins_text(units.unsigned_abs(), chain),
            chain.symbol()
        )
    }
}

/// `bc1qar0srrr…gtzzwf5mdq` style abbreviation. Short strings are unchanged.
pub fn short_address(address: &str) -> String {
    let chars: Vec<char> = address.chars().collect();
    if chars.len() <= 16 {
        return address.to_owned();
    }
    let head: String = chars[..8].iter().collect();
    let tail: String = chars[chars.len() - 6..].iter().collect();
    format!("{head}…{tail}")
}

/// "Showing 50 of 1234 transactions", for a list that shows part of its rows.
/// `noun` is what the rows are, in the plural.
pub fn showing_of(shown: usize, total: usize, noun: &str) -> String {
    format!("Showing {shown} of {total} {noun}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_grouping() {
        assert_eq!(group("1"), "1");
        assert_eq!(group("123"), "123");
        assert_eq!(group("1234"), "1,234");
        assert_eq!(group("1234567"), "1,234,567");
    }

    #[test]
    fn dollar_formats() {
        assert_eq!(usd(52_300.0, false), "$52,300.00");
        assert_eq!(usd(-2_500.5, false), "−$2,500.50");
        assert_eq!(signed_usd(11_300.0, false), "+$11,300.00");
        assert_eq!(signed_usd(-0.5, false), "−$0.50");
    }

    #[test]
    fn percent_formats() {
        assert_eq!(percent(Some(27.6), false), "+27.60%");
        assert_eq!(percent(Some(-3.0), false), "−3.00%");
        assert_eq!(percent(None, false), "—");
    }

    #[test]
    fn amount_formats() {
        let btc = Chain::Bitcoin;
        assert_eq!(amount(52_000_000, btc, false), "0.52000000 BTC");
        assert_eq!(signed_amount(-30_200, btc, false), "−0.00030200 BTC");
        assert_eq!(signed_amount(10_000, btc, false), "+0.00010000 BTC");
    }

    #[test]
    fn hidden_masks_everything() {
        assert_eq!(usd(1.0, true), MASK);
        assert_eq!(signed_usd(-1.0, true), MASK);
        assert_eq!(percent(Some(5.0), true), MASK);
        assert_eq!(percent(None, true), MASK);
        assert_eq!(amount(1, Chain::Bitcoin, true), MASK);
        assert_eq!(signed_amount(-1, Chain::Bitcoin, true), MASK);
    }

    #[test]
    fn address_abbreviation() {
        assert_eq!(
            short_address("bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"),
            "bc1qar0s…wf5mdq"
        );
        assert_eq!(short_address("short"), "short");
    }

    #[test]
    fn says_how_many_transactions_are_shown() {
        assert_eq!(
            showing_of(50, 1234, "transactions"),
            "Showing 50 of 1234 transactions"
        );
        assert_eq!(showing_of(50, 120, "lots"), "Showing 50 of 120 lots");
    }
}
