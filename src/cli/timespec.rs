//! Parsing the time arguments the CLI and API accept.
//!
//! Three forms, uniformly: an absolute RFC3339 timestamp, a relative offset
//! like `-24h`, or `scan:<id>` to name a sample directly. `now` is accepted as
//! a synonym for the present.

use anyhow::{Result, bail};
use crate::model::ScanId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A wall-clock instant, to be resolved to the nearest preceding scan.
    At(i64),
    /// An exact scan, no resolution needed.
    Scan(ScanId),
}

pub fn parse(s: &str, now: i64) -> Result<Target> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("scan:") {
        return Ok(Target::Scan(rest.parse()?));
    }
    if s == "now" {
        return Ok(Target::At(now));
    }
    if let Some(rest) = s.strip_prefix('-') {
        return Ok(Target::At(now - parse_duration(rest)?));
    }
    if let Some(rest) = s.strip_prefix('+') {
        return Ok(Target::At(now + parse_duration(rest)?));
    }
    if let Ok(epoch) = s.parse::<i64>() {
        return Ok(Target::At(epoch));
    }
    // A bare duration means "ago". Unambiguous, because a duration always
    // carries a unit suffix and a bare integer was already taken as an epoch
    // above -- and it is what people type when the shell has eaten their
    // leading minus.
    if let Ok(d) = parse_duration(s) {
        return Ok(Target::At(now - d));
    }
    Ok(Target::At(parse_rfc3339(s)?))
}

/// Accepts `90s`, `15m`, `24h`, `7d`, `3w`, and bare seconds.
///
/// Deliberately stops at weeks: "1 month" is ambiguous (28? 30? 31?) and a
/// disk-growth report that silently picks one is worse than one that refuses.
pub fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    if let Ok(n) = s.parse::<i64>() {
        return Ok(n);
    }
    let (num, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(s.len()),
    );
    let n: f64 = num.parse().map_err(|_| anyhow::anyhow!("bad duration {s:?}"))?;
    let mult = match unit.trim() {
        "s" | "sec" | "secs" => 1.0,
        "m" | "min" | "mins" => 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600.0,
        "d" | "day" | "days" => 86_400.0,
        "w" | "wk" | "week" | "weeks" => 604_800.0,
        other => bail!(
            "unknown duration unit {other:?} in {s:?} — use s, m, h, d or w \
             (months and years are ambiguous, so give days or weeks)"
        ),
    };
    Ok((n * mult) as i64)
}

/// Minimal RFC3339 / ISO-8601 parser for the subset a human types.
///
/// Accepts `2026-09-10`, `2026-09-10T14:35`, `2026-09-10T14:35:02`, with an
/// optional `Z` or `+HH:MM` offset, and a space in place of the `T`.
pub fn parse_rfc3339(s: &str) -> Result<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', ' ']).unwrap_or((s, ""));
    let d: Vec<&str> = date.split('-').collect();
    if d.len() != 3 {
        bail!("cannot parse {s:?} as a time — try 2026-09-10, 2026-09-10T14:35, -24h or scan:<id>");
    }
    let (y, mo, da): (i64, i64, i64) = (d[0].parse()?, d[1].parse()?, d[2].parse()?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        bail!("{s:?} is not a valid date");
    }

    let (time, offset) = split_offset(rest);
    let mut hms = [0i64; 3];
    if !time.is_empty() {
        for (i, part) in time.split(':').take(3).enumerate() {
            // Tolerate fractional seconds by discarding them: sub-second
            // precision is meaningless against samples minutes apart.
            let part = part.split('.').next().unwrap_or(part);
            hms[i] = part.parse()?;
        }
    }
    if hms[0] > 23 || hms[1] > 59 || hms[2] > 60 {
        bail!("{s:?} has an out-of-range time");
    }

    let days = days_from_civil(y, mo as u32, da as u32);
    Ok(days * 86_400 + hms[0] * 3_600 + hms[1] * 60 + hms[2] - offset)
}

/// Split a trailing UTC offset off a time string, returning seconds east.
fn split_offset(t: &str) -> (&str, i64) {
    if let Some(stripped) = t.strip_suffix('Z').or_else(|| t.strip_suffix('z')) {
        return (stripped, 0);
    }
    // Look for +HH:MM / -HH:MM after the start, so a leading sign isn't eaten.
    if let Some(pos) = t.rfind(['+', '-']).filter(|&p| p > 0) {
        let (time, off) = t.split_at(pos);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let off = &off[1..];
        let (h, m) = off.split_once(':').unwrap_or((off, "0"));
        if let (Ok(h), Ok(m)) = (h.parse::<i64>(), m.parse::<i64>()) {
            return (time, sign * (h * 3_600 + m * 60));
        }
    }
    (t, 0)
}

/// Howard Hinnant's days-from-civil algorithm.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("15m").unwrap(), 900);
        assert_eq!(parse_duration("24h").unwrap(), 86_400);
        assert_eq!(parse_duration("7d").unwrap(), 604_800);
        assert_eq!(parse_duration("2w").unwrap(), 1_209_600);
        assert_eq!(parse_duration("300").unwrap(), 300);
        assert_eq!(parse_duration("1.5h").unwrap(), 5_400);
    }

    #[test]
    fn rejects_ambiguous_units_with_a_useful_message() {
        let e = parse_duration("3mo").unwrap_err().to_string();
        assert!(e.contains("ambiguous"), "unhelpful error: {e}");
    }

    #[test]
    fn parses_epoch_anchor() {
        // 2026-09-10T00:00:00Z
        assert_eq!(parse_rfc3339("2026-09-10").unwrap(), 1_788_998_400);
        assert_eq!(parse_rfc3339("2026-09-10T00:00:00Z").unwrap(), 1_788_998_400);
        assert_eq!(parse_rfc3339("2026-09-10 00:00:00").unwrap(), 1_788_998_400);
        assert_eq!(parse_rfc3339("1970-01-01").unwrap(), 0);
    }

    #[test]
    fn honours_utc_offsets() {
        let z = parse_rfc3339("2026-09-10T12:00:00Z").unwrap();
        let plus = parse_rfc3339("2026-09-10T14:00:00+02:00").unwrap();
        let minus = parse_rfc3339("2026-09-10T07:00:00-05:00").unwrap();
        assert_eq!(z, plus, "+02:00 should resolve to the same instant");
        assert_eq!(z, minus, "-05:00 should resolve to the same instant");
    }

    #[test]
    fn discards_fractional_seconds() {
        assert_eq!(
            parse_rfc3339("2026-09-10T00:00:01.523Z").unwrap(),
            parse_rfc3339("2026-09-10T00:00:01Z").unwrap()
        );
    }

    #[test]
    fn parses_targets() {
        let now = 1_000_000;
        assert_eq!(parse("now", now).unwrap(), Target::At(now));
        assert_eq!(parse("-24h", now).unwrap(), Target::At(now - 86_400));
        assert_eq!(parse("scan:42", now).unwrap(), Target::Scan(42));
        assert_eq!(parse("1788998400", now).unwrap(), Target::At(1_788_998_400));
        // A bare duration reads as "ago", so a shell-mangled "-7d" still works.
        assert_eq!(parse("7d", now).unwrap(), Target::At(now - 604_800));
        assert_eq!(parse("90m", now).unwrap(), Target::At(now - 5_400));
    }

    #[test]
    fn round_trips_through_civil_date_conversion() {
        // The two algorithms are inverses; a mismatch would silently shift
        // every displayed timestamp.
        for epoch in [0i64, 1_000_000_000, 1_788_998_400, 2_000_000_000] {
            let days = epoch.div_euclid(86_400);
            let (y, m, d) = crate::cli::civil_from_days_pub(days);
            assert_eq!(days_from_civil(y, m, d), days, "round trip failed at {epoch}");
        }
    }
}
