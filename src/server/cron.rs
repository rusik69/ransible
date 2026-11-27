//! Minimal 5-field cron matcher (UTC): `min hour dom mon dow`, with `*`, lists, ranges and steps.

fn field_matches(spec: &str, val: u32, lo: u32, hi: u32) -> bool {
    spec.split(',').any(|part| {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0)),
            None => (part, Some(1)),
        };
        let Some(step) = step else { return false };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            match (a.parse::<u32>(), b.parse::<u32>()) {
                (Ok(a), Ok(b)) => (a, b),
                _ => return false,
            }
        } else {
            match range.parse::<u32>() {
                // "5/10" means 5-hi stepping 10; plain "5" is exactly 5
                Ok(n) if part.contains('/') => (n, hi),
                Ok(n) => (n, n),
                _ => return false,
            }
        };
        val >= a && val <= b && (val - a).is_multiple_of(step)
    })
}

fn expand(s: &str) -> &str {
    match s {
        "@hourly" => "0 * * * *",
        "@daily" | "@midnight" => "0 0 * * *",
        "@weekly" => "0 0 * * 0",
        "@monthly" => "0 0 1 * *",
        "@yearly" | "@annually" => "0 0 1 1 *",
        o => o,
    }
}

pub fn validate(spec: &str) -> bool {
    let f: Vec<&str> = expand(spec.trim()).split_whitespace().collect();
    if f.len() != 5 {
        return false;
    }
    let ok = |s: &str, lo: u32, hi: u32| (lo..=hi).any(|v| field_matches(s, v, lo, hi));
    ok(f[0], 0, 59) && ok(f[1], 0, 23) && ok(f[2], 1, 31) && ok(f[3], 1, 12) && ok(f[4], 0, 7)
}

/// (minute, hour, day-of-month, month, day-of-week 0=Sunday) for a Unix timestamp (UTC).
pub fn utc_fields(secs: i64) -> (u32, u32, u32, u32, u32) {
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719468;
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let dow = ((days.rem_euclid(7) + 4) % 7) as u32; // 1970-01-01 was a Thursday
    (((rem % 3600) / 60) as u32, (rem / 3600) as u32, d, m, dow)
}

pub fn matches(spec: &str, secs: i64) -> bool {
    let f: Vec<&str> = expand(spec.trim()).split_whitespace().collect();
    if f.len() != 5 {
        return false;
    }
    let (mi, h, d, mo, dow) = utc_fields(secs);
    let dow_ok = field_matches(f[4], dow, 0, 7) || (dow == 0 && field_matches(f[4], 7, 0, 7));
    let (dom_star, dow_star) = (f[2] == "*", f[4] == "*");
    let day_ok = match (dom_star, dow_star) {
        (true, true) => true,
        (false, true) => field_matches(f[2], d, 1, 31),
        (true, false) => dow_ok,
        // standard cron: when both are restricted, either may match
        (false, false) => field_matches(f[2], d, 1, 31) || dow_ok,
    };
    field_matches(f[0], mi, 0, 59)
        && field_matches(f[1], h, 0, 23)
        && field_matches(f[3], mo, 1, 12)
        && day_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-11-27 20:30:00 UTC (a Friday)
    const T: i64 = 1_795_811_400;

    #[test]
    fn fields() {
        assert_eq!(utc_fields(T), (30, 20, 27, 11, 5));
        assert_eq!(utc_fields(0), (0, 0, 1, 1, 4));
    }

    #[test]
    fn matching() {
        assert!(matches("* * * * *", T));
        assert!(matches("30 20 * * *", T));
        assert!(matches("*/15 20 * * 5", T));
        assert!(matches("0,30 18-22 27 11 *", T));
        assert!(!matches("31 20 * * *", T));
        assert!(!matches("30 20 * * 1-4", T));
        assert!(matches("30 20 1 * 5", T)); // dom OR dow
        assert!(!matches("30 20 1 * 1", T));
    }

    #[test]
    fn aliases_and_validation() {
        assert!(matches("@hourly", T - 30 * 60));
        assert!(validate("*/5 * * * *") && validate("@daily"));
        assert!(!validate("61 * * * *") && !validate("* * * *") && !validate("a b c d e"));
    }
}
