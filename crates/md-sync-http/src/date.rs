//! Tiny date helpers. The workspace deliberately avoids `chrono`/`time`:
//! a sync tool needs three formats (RFC 3339, RFC 1123, SigV4 basic) and
//! a civil-date conversion — about 60 lines, no dependency.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Days from civil (Howard Hinnant's algorithm, public domain math).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Split a `SystemTime` into (year, month, day, hour, min, sec) in UTC.
pub fn utc_parts(t: SystemTime) -> (i64, u32, u32, u32, u32, u32) {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    (
        y,
        m,
        d,
        (tod / 3600) as u32,
        ((tod % 3600) / 60) as u32,
        (tod % 60) as u32,
    )
}

pub fn unix_from_parts(y: i64, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> SystemTime {
    let days = days_from_civil(y, m, d);
    let secs = days * 86_400 + hh as i64 * 3600 + mm as i64 * 60 + ss as i64;
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH
    }
}

/// `2026-11-18T16:28:52Z` / with fractional seconds / with `+08:00`.
/// Returns `None` on malformed input.
pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let (date, rest) = s.split_once('T').or_else(|| s.split_once(' '))?;
    let mut di = date.split('-');
    let y: i64 = di.next()?.parse().ok()?;
    let m: u32 = di.next()?.parse().ok()?;
    let d: u32 = di.next()?.parse().ok()?;
    // Time part: HH:MM:SS[.frac][Z|+hh:mm|-hh:mm]
    let (time, offset_secs) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0i64)
    } else if let Some(idx) = rest.rfind(['+', '-']) {
        let (t, off) = rest.split_at(idx);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let mut oi = off[1..].split(':');
        let oh: i64 = oi.next()?.parse().ok()?;
        let om: i64 = oi.next().unwrap_or("0").parse().ok()?;
        (t, sign * (oh * 3600 + om * 60))
    } else {
        (rest, 0)
    };
    let time = time.split('.').next()?;
    let mut ti = time.split(':');
    let hh: u32 = ti.next()?.parse().ok()?;
    let mm: u32 = ti.next()?.parse().ok()?;
    let ss: u32 = ti.next()?.parse().ok()?;
    let base = unix_from_parts(y, m, d, hh, mm, ss);
    if offset_secs >= 0 {
        base.checked_sub(Duration::from_secs(offset_secs as u64))
    } else {
        base.checked_add(Duration::from_secs((-offset_secs) as u64))
    }
}

/// `Mon, 18 Nov 2026 16:28:52 GMT` (WebDAV `getlastmodified`).
pub fn parse_rfc1123(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let s = s.split_once(", ")?.1;
    let mut parts = s.split_whitespace();
    let d: u32 = parts.next()?.parse().ok()?;
    let mon = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let y: i64 = parts.next()?.parse().ok()?;
    let mut ti = parts.next()?.split(':');
    let hh: u32 = ti.next()?.parse().ok()?;
    let mm: u32 = ti.next()?.parse().ok()?;
    let ss: u32 = ti.next()?.parse().ok()?;
    Some(unix_from_parts(y, mon, d, hh, mm, ss))
}

/// SigV4 date stamp `20261118` and full timestamp `20261118T162852Z`.
pub fn amz_dates(t: SystemTime) -> (String, String) {
    let (y, m, d, hh, mm, ss) = utc_parts(t);
    (
        format!("{y:04}{m:02}{d:02}"),
        format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z"),
    )
}

/// Format as RFC 3339 UTC, `2026-11-18T16:28:52Z`.
pub fn format_rfc3339(t: SystemTime) -> String {
    let (y, m, d, hh, mm, ss) = utc_parts(t);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_roundtrip() {
        let t = unix_from_parts(2026, 11, 18, 16, 28, 52);
        assert_eq!(parse_rfc3339("2026-11-18T16:28:52Z"), Some(t));
        assert_eq!(
            parse_rfc3339("2026-11-18T16:28:52.123Z"),
            Some(t),
            "fractional seconds ignored"
        );
        assert_eq!(
            parse_rfc3339("2026-11-19T00:28:52+08:00"),
            Some(t),
            "offset applied"
        );
    }

    #[test]
    fn rfc1123_works() {
        let t = unix_from_parts(2026, 11, 18, 16, 28, 52);
        assert_eq!(parse_rfc1123("Wed, 18 Nov 2026 16:28:52 GMT"), Some(t));
    }

    #[test]
    fn amz_format() {
        let t = unix_from_parts(2026, 11, 18, 16, 28, 52);
        assert_eq!(amz_dates(t), ("20261118".into(), "20261118T162852Z".into()));
    }
}
