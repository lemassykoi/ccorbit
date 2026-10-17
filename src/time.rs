//! Time parsing and the compact number and duration formats used on screen.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parses the UTC timestamps Claude Code writes, such as `2026-10-02T23:56:06.584Z`.
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    let num = |a: usize, b: usize| -> Option<i64> { s.get(a..b)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut ms = 0;
    if s.as_bytes().get(19) == Some(&b'.') {
        let digits: String = s[20..].chars().take_while(char::is_ascii_digit).take(3).collect();
        ms = format!("{digits:0<3}").parse().unwrap_or(0);
    }
    Some(days_from_civil(y, mo, d) * 86_400_000 + h * 3_600_000 + mi * 60_000 + se * 1000 + ms)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn fmt_dur(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else if ms < 3_600_000 {
        format!("{}m{:02}s", ms / 60_000, (ms / 1000) % 60)
    } else {
        format!("{}h{:02}m", ms / 3_600_000, (ms / 60_000) % 60)
    }
}

pub fn fmt_n(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => format!("{:.1}k", n as f64 / 1e3),
        10_000..=999_999 => format!("{}k", n / 1000),
        1_000_000..=9_999_999 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{}M", n / 1_000_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_timestamps() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_iso_ms("2026-10-02T23:56:06.579Z"), Some(1_790_985_366_579));
        assert_eq!(parse_iso_ms("2024-02-29T12:00:00Z"), Some(1_709_208_000_000));
        assert_eq!(parse_iso_ms("garbage"), None);
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_dur(400), "0.4s");
        assert_eq!(fmt_dur(63_000), "1m03s");
        assert_eq!(fmt_n(1_234), "1.2k");
        assert_eq!(fmt_n(2_500_000), "2.5M");
    }
}
