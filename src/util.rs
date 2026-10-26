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
}
