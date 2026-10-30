//! Small formatting helpers shared by front ends.

/// Formats a Unix timestamp as `YYYY-MM-DD` (UTC).
///
/// Uses the days-to-civil-date algorithm by Howard Hinnant, which avoids a
/// date library for this one conversion.
pub fn date_string(timestamp: i64) -> String {
    let z = timestamp.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Reads an ISO 8601 UTC timestamp such as `2026-10-24T09:13:35.000000Z` as
/// a Unix timestamp. Fractions of a second are dropped. `None` if the text
/// does not have that shape.
///
/// The inverse of the algorithm in [`date_string`], also by Howard Hinnant.
pub fn parse_iso_utc(text: &str) -> Option<i64> {
    let (date, time) = text.split_once('T')?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    let time = time.trim_end_matches('Z');
    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next()?.split('.').next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Reads a date typed as `YYYY-MM-DD` as noon UTC of that day, so the whole
/// day shares one price. `None` if the text is not a real date of that shape.
pub fn parse_date_utc(text: &str) -> Option<i64> {
    let text = text.trim();
    let bytes = text.as_bytes();
    let shaped = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
    if !shaped {
        return None;
    }
    let time = parse_iso_utc(&format!("{text}T12:00:00Z"))?;
    // Days such as February 31st are rolled over by the conversion, so the
    // result must read back as the same date.
    (date_string(time) == text).then_some(time)
}

/// Reads a decimal amount of coins such as `0.015` as units of the chain's
/// smallest denomination. A comma works as the decimal separator. `None` for
/// empty text, signs, more decimals than the coin has, or a value too large.
pub fn parse_units(text: &str, decimals: u8) -> Option<u128> {
    let text = text.trim().replace(',', ".");
    let (whole, fraction) = text.split_once('.').unwrap_or((text.as_str(), ""));
    let digits_only = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && fraction.is_empty())
        || !digits_only(whole)
        || !digits_only(fraction)
        || fraction.len() > usize::from(decimals)
    {
        return None;
    }
    let padded = format!("{fraction:0<width$}", width = usize::from(decimals));
    let whole: u128 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let fraction: u128 = if padded.is_empty() {
        0
    } else {
        padded.parse().ok()?
    };
    whole
        .checked_mul(10u128.checked_pow(u32::from(decimals))?)?
        .checked_add(fraction)
}

/// Writes `units` as a decimal amount of coins without trailing zeros, the
/// inverse of [`parse_units`].
pub fn units_to_text(units: u128, decimals: u8) -> String {
    let scale = 10u128.pow(u32::from(decimals));
    let (whole, fraction) = (units / scale, units % scale);
    if fraction == 0 {
        return whole.to_string();
    }
    let fraction = format!("{fraction:0width$}", width = usize::from(decimals));
    format!("{whole}.{}", fraction.trim_end_matches('0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(date_string(0), "1970-01-01");
        assert_eq!(date_string(1_700_000_000), "2023-11-14");
        assert_eq!(date_string(1_230_940_800), "2009-01-03");
    }

    #[test]
    fn reads_iso_timestamps() {
        assert_eq!(parse_iso_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso_utc("2023-11-14T22:13:20.000000Z"),
            Some(1_700_000_000)
        );
        assert_eq!(parse_iso_utc("2009-01-03T18:15:05Z"), Some(1_231_006_505));
        assert_eq!(parse_iso_utc("not a time"), None);
        assert_eq!(parse_iso_utc("2023-13-01T00:00:00Z"), None);
    }

    #[test]
    fn iso_and_date_strings_agree() {
        for t in [0, 951_782_400, 1_700_000_000, 2_000_000_000] {
            let day = date_string(t);
            assert_eq!(
                parse_iso_utc(&format!("{day}T00:00:00Z")),
                Some(t.div_euclid(86_400) * 86_400)
            );
        }
    }

    #[test]
    fn typed_dates_become_noon_utc() {
        assert_eq!(parse_date_utc("2023-11-14"), Some(1_699_963_200));
        assert_eq!(parse_date_utc(" 1970-01-01 "), Some(43_200));
    }

    #[test]
    fn dates_that_do_not_exist_are_rejected() {
        for bad in [
            "2023-02-30",
            "2023-13-01",
            "2023-1-01",
            "14/11/2023",
            "",
            "2023-11-14x",
        ] {
            assert_eq!(parse_date_utc(bad), None, "{bad}");
        }
    }

    #[test]
    fn amounts_become_smallest_units() {
        assert_eq!(parse_units("1", 8), Some(100_000_000));
        assert_eq!(parse_units("0.015", 8), Some(1_500_000));
        assert_eq!(parse_units(".5", 8), Some(50_000_000));
        assert_eq!(parse_units("2,5", 8), Some(250_000_000));
        assert_eq!(parse_units("1.5", 18), Some(1_500_000_000_000_000_000));
        assert_eq!(parse_units("0.00000001", 8), Some(1));
    }

    #[test]
    fn amounts_that_do_not_fit_are_rejected() {
        for bad in ["", ".", "-1", "+1", "1.000000001", "1e3", "abc", "1.2.3"] {
            assert_eq!(parse_units(bad, 8), None, "{bad}");
        }
        assert_eq!(
            parse_units("340282366920938463463374607431768211456", 0),
            None
        );
    }

    #[test]
    fn units_read_back_as_the_text_that_made_them() {
        for (text, decimals) in [("1", 8), ("0.015", 8), ("2.5", 18), ("0.00000001", 8)] {
            let units = parse_units(text, decimals).expect("parses");
            assert_eq!(units_to_text(units, decimals), text);
        }
        assert_eq!(units_to_text(0, 8), "0");
    }
}
