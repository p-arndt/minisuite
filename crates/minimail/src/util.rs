// Small utilities: date formatting/parsing (RFC 5322 + ISO-8601), ids. Pure std.

use std::time::{SystemTime, UNIX_EPOCH};

const DAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

// Days since epoch -> (year, month 1-12, day 1-31, weekday 0=Sun). Howard Hinnant's algorithm.
pub fn civil_from_days(days: i64) -> (i32, u32, u32, u32) {
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
    // 1970-01-01 was Thursday (4).
    let wd = (((days % 7) + 7 + 4) % 7) as u32;
    (y as i32, m, d, wd)
}

#[allow(dead_code)] // frozen SPEC §5 util API; no consumer in this binary build
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

// Current wall-clock time as seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn http_date_now() -> String {
    http_date(now_secs())
}

// RFC 7231: "Thu, 01 Jan 1970 00:00:00 GMT"
pub fn http_date(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, mo, d, wd) = civil_from_days(days);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAY_NAMES[wd as usize],
        d,
        MONTH_NAMES[(mo - 1) as usize],
        y,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// ISO 8601 / RFC 3339 in UTC: "1970-01-01T00:00:00.000Z"
pub fn iso8601(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, mo, d, _) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
        y,
        mo,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// RFC 5322 Date: "Thu, 01 Jan 1970 00:00:00 +0000"
#[allow(dead_code)] // frozen SPEC §5 util API; minimail stores raw Date, never re-formats
pub fn rfc5322_date(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, mo, d, wd) = civil_from_days(days);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} +0000",
        DAY_NAMES[wd as usize],
        d,
        MONTH_NAMES[(mo - 1) as usize],
        y,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// Zone -> offset in seconds east of UTC. Numeric "+HHMM"/"-HHMM" or a named zone.
#[allow(dead_code)] // helper of the frozen parse_rfc5322_date, itself unused in this binary
fn parse_zone(z: &str) -> Option<i64> {
    if z.is_empty() {
        return None;
    }
    let b = z.as_bytes();
    if (b[0] == b'+' || b[0] == b'-') && z.len() == 5 {
        let sign = if b[0] == b'+' { 1 } else { -1 };
        let hh: i64 = z[1..3].parse().ok()?;
        let mm: i64 = z[3..5].parse().ok()?;
        return Some(sign * (hh * 3600 + mm * 60));
    }
    let hours = match z.to_ascii_uppercase().as_str() {
        "GMT" | "UT" | "UTC" | "Z" => 0,
        "EST" => -5,
        "EDT" => -4,
        "CST" => -6,
        "CDT" => -5,
        "MST" => -7,
        "MDT" => -6,
        "PST" => -8,
        "PDT" => -7,
        _ => return None,
    };
    Some(hours * 3600)
}

/// Parse an RFC 5322 Date: header to unix seconds. Lenient: optional "Wed, "
/// day-of-week, named month via MONTH_NAMES.position, numeric (+0000) or named
/// (GMT/UT/UTC/EST/EDT/CST/CDT/MST/MDT/PST/PDT) zone. Returns None on garbage.
#[allow(dead_code)] // frozen SPEC §5 util API; minimail stores raw Date, never parses it
pub fn parse_rfc5322_date(s: &str) -> Option<u64> {
    let s = s.trim();
    // Drop an optional leading day-of-week token ending in a comma ("Wed, ").
    let s = match s.find(',') {
        Some(i) if i <= 3 => s[i + 1..].trim_start(),
        _ => s,
    };
    let mut it = s.split_whitespace();

    let day: i64 = it.next()?.parse().ok()?;
    let mon_str = it.next()?;
    let mon = MONTH_NAMES
        .iter()
        .position(|m| m.eq_ignore_ascii_case(mon_str))? as i64
        + 1;

    let year_str = it.next()?;
    let mut year: i64 = year_str.parse().ok()?;
    // RFC 2822 obsolete year forms.
    match year_str.len() {
        2 => year += if year < 50 { 2000 } else { 1900 },
        3 => year += 1900,
        _ => {}
    }

    let time = it.next()?;
    let mut tp = time.split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let se: i64 = match tp.next() {
        Some(x) => x.parse().ok()?,
        None => 0,
    };

    let offset = parse_zone(it.next().unwrap_or("+0000"))?;
    let days = days_from_civil(year, mon, day);
    let total = days * 86400 + h * 3600 + mi * 60 + se - offset;
    if total < 0 {
        None
    } else {
        Some(total as u64)
    }
}

// Simple non-crypto pseudo-random hex for request/correlation ids.
#[allow(dead_code)] // frozen SPEC §5 util API; no consumer in this binary build
pub fn request_id() -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let n = t.as_nanos() as u64;
    let mix = n
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    format!("{:016X}", mix)
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
    fn iso() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1704164645), "2024-01-02T03:04:05.000Z");
    }

    #[test]
    fn rfc5322_format_zero() {
        assert_eq!(rfc5322_date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
    }

    #[test]
    fn rfc5322_round_trip_epoch() {
        let s = rfc5322_date(0);
        assert_eq!(parse_rfc5322_date(&s), Some(0));
    }

    #[test]
    fn rfc5322_round_trip_many() {
        for &t in &[0u64, 1, 1704164645, 1582934400, 1720000000, 253402300799] {
            let s = rfc5322_date(t);
            assert_eq!(parse_rfc5322_date(&s), Some(t), "round trip {t} via {s}");
        }
    }

    #[test]
    fn parse_numeric_offset() {
        // +0000 and an eastern +0100 should differ by 3600.
        let z = parse_rfc5322_date("Tue, 02 Jan 2024 03:04:05 +0000").unwrap();
        assert_eq!(z, 1704164645);
        let east = parse_rfc5322_date("Tue, 02 Jan 2024 04:04:05 +0100").unwrap();
        assert_eq!(east, 1704164645);
        let west = parse_rfc5322_date("Tue, 02 Jan 2024 02:04:05 -0100").unwrap();
        assert_eq!(west, 1704164645);
    }

    #[test]
    fn parse_named_zones() {
        let gmt = parse_rfc5322_date("2 Jan 2024 03:04:05 GMT").unwrap();
        assert_eq!(gmt, 1704164645);
        // EST is -0500, so 22:04:05 EST on Jan 1 == 03:04:05 UTC on Jan 2.
        let est = parse_rfc5322_date("Mon, 01 Jan 2024 22:04:05 EST").unwrap();
        assert_eq!(est, 1704164645);
    }

    #[test]
    fn parse_without_dow_or_seconds() {
        let a = parse_rfc5322_date("2 Jan 2024 03:04 +0000").unwrap();
        assert_eq!(a, 1704164640);
    }

    #[test]
    fn parse_two_digit_year() {
        // "24" -> 2024
        let a = parse_rfc5322_date("Tue, 02 Jan 24 03:04:05 +0000").unwrap();
        assert_eq!(a, 1704164645);
    }

    #[test]
    fn parse_garbage_is_none() {
        assert!(parse_rfc5322_date("").is_none());
        assert!(parse_rfc5322_date("not a date").is_none());
        assert!(parse_rfc5322_date("02 Foo 2024 03:04:05 +0000").is_none());
        assert!(parse_rfc5322_date("02 Jan 2024 03:04:05 XYZ").is_none());
        assert!(parse_rfc5322_date("02 Jan 2024").is_none());
    }

    #[test]
    fn civil_days_round_trip() {
        for &t in &[0i64, 719468, -100, 100000, 253402300799 / 86400] {
            let (y, m, d, _) = civil_from_days(t);
            assert_eq!(days_from_civil(y as i64, m as i64, d as i64), t);
        }
    }

    #[test]
    fn request_id_is_hex_16() {
        let r = request_id();
        assert_eq!(r.len(), 16);
        assert!(r
            .chars()
            .all(|c| c.is_ascii_hexdigit() && (c.is_ascii_digit() || c.is_ascii_uppercase())));
    }
}
