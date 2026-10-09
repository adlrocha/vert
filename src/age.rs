//! Compact relative-time formatting ("4m", "2h", "3d").

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const WEEK: i64 = 7 * DAY;
const MONTH: i64 = 30 * DAY;
const YEAR: i64 = 365 * DAY;

/// Formats the time elapsed from `then` to `now` (both Unix seconds) compactly.
///
/// Times in the future (clock skew) are shown as `"now"`.
///
/// ```
/// assert_eq!(vert::age::relative(1_000, 1_000 - 180), "3m");
/// ```
#[must_use]
pub fn relative(now: i64, then: i64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        s if s < MINUTE => "now".to_owned(),
        s if s < HOUR => format!("{}m", s / MINUTE),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < WEEK => format!("{}d", s / DAY),
        s if s < MONTH => format!("{}w", s / WEEK),
        s if s < YEAR => format!("{}mo", s / MONTH),
        s => format!("{}y", s / YEAR),
    }
}

/// Like [`relative`], phrased for prose: `"just now"` or `"4m ago"`.
#[must_use]
pub fn ago(now: i64, then: i64) -> String {
    match relative(now, then).as_str() {
        "now" => "just now".to_owned(),
        r => format!("{r} ago"),
    }
}

/// Current Unix time in seconds, or 0 if the system clock is before the epoch.
#[must_use]
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_each_unit() {
        let now = 100 * YEAR;
        let cases = [
            (0, "now"),
            (59, "now"),
            (MINUTE, "1m"),
            (59 * MINUTE, "59m"),
            (HOUR, "1h"),
            (DAY, "1d"),
            (WEEK, "1w"),
            (MONTH, "1mo"),
            (YEAR, "1y"),
            (3 * YEAR, "3y"),
        ];
        for (secs_ago, expected) in cases {
            assert_eq!(relative(now, now - secs_ago), expected, "{secs_ago}s ago");
        }
    }

    #[test]
    fn ago_phrasing() {
        assert_eq!(ago(100, 90), "just now");
        assert_eq!(ago(1000, 1000 - 120), "2m ago");
    }

    #[test]
    fn future_is_now() {
        assert_eq!(relative(0, 500), "now");
    }

    #[test]
    fn now_is_positive() {
        assert!(now() > 0);
    }
}
