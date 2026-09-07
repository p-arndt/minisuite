// Small utilities: date formatting and HTML escaping.

use std::time::{SystemTime, UNIX_EPOCH};

const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

// Days since epoch -> (year, month 1-12, day 1-31, weekday 0=Sun)
pub fn civil_from_days(days: i64) -> (i32, u32, u32, u32) {
    // Howard Hinnant's date algorithms.
    let z = days + 719468;
    let era = if z >= 0 {
        z / 146097
    } else {
        (z - 146096) / 146097
    };
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    // Weekday: 1970-01-01 was Thursday (4)
    let wd = (((days % 7) + 7 + 4) % 7) as u32;
    (y as i32, m, d, wd)
}

// Inverse of civil_from_days. Only the round-trip test needs it.
#[cfg(test)]
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let d = d as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe as i64) - 719468
}

pub fn http_date_now() -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    http_date(t.as_secs())
}

// Current wall-clock time as seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn http_date(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (y, mo, d, wd) = civil_from_days(days);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAY_NAMES[wd as usize],
        d,
        MONTH_NAMES[(mo - 1) as usize],
        y,
        h,
        m,
        s
    )
}

// Escape text for safe inclusion in HTML. Same set as XML escaping,
// but apostrophe becomes the numeric entity &#39; for broad compatibility.
pub fn html_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn http_date_unix0() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }
    #[test]
    fn http_date_non_zero() {
        // 2024-01-02 03:04:05 UTC = 1704164645
        assert_eq!(http_date(1704164645), "Tue, 02 Jan 2024 03:04:05 GMT");
    }

    #[test]
    fn http_date_leap_year() {
        // 2020-02-29 00:00:00 UTC = 1582934400
        assert_eq!(http_date(1582934400), "Sat, 29 Feb 2020 00:00:00 GMT");
    }

    #[test]
    fn days_from_civil_round_trips() {
        // Epoch and a known date round-trip through civil_from_days.
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        let days = days_from_civil(2024, 1, 2);
        let (y, mo, d, _) = civil_from_days(days);
        assert_eq!((y, mo, d), (2024, 1, 2));
    }

    #[test]
    fn html_escape_all_specials() {
        assert_eq!(
            html_escape("a<b>c&d\"e'f"),
            "a&lt;b&gt;c&amp;d&quot;e&#39;f"
        );
    }

    #[test]
    fn html_escape_passthrough() {
        assert_eq!(html_escape("hello world"), "hello world");
    }
}
