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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(date_string(0), "1970-01-01");
        assert_eq!(date_string(1_700_000_000), "2023-11-14");
        assert_eq!(date_string(1_230_940_800), "2009-01-03");
    }
}
