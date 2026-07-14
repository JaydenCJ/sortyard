//! Offline ISO 8601 date/time parsing to UTC epoch milliseconds.
//!
//! Accepted shapes (`/` may replace `-` as the date separator):
//!
//! - `2026-07-01`
//! - `2026-07-01T14:30`, `2026-07-01 14:30:05`, `2026-07-01T14:30:05.250`
//! - any of the above with a `Z`, `+09:00`, `-0530` or `+07` offset
//!
//! Times without an offset are treated as UTC. Fractional seconds beyond
//! milliseconds are truncated. Calendar math is done with the classic
//! days-from-civil algorithm, so no timezone database or system clock is
//! involved — parsing is fully deterministic.

/// Byte cursor over the input, with small fixed-width integer readers.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Cursor {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Read exactly `n` ASCII digits as an integer.
    fn digits(&mut self, n: usize) -> Option<i64> {
        let end = self.pos.checked_add(n)?;
        if end > self.bytes.len() {
            return None;
        }
        let mut v: i64 = 0;
        for &b in &self.bytes[self.pos..end] {
            if !b.is_ascii_digit() {
                return None;
            }
            v = v * 10 + i64::from(b - b'0');
        }
        self.pos = end;
        Some(v)
    }

    fn done(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

/// Parse a date or date-time string to UTC epoch milliseconds.
pub fn parse_datetime(text: &str) -> Result<i64, String> {
    let err = || format!("cannot parse {text:?} as a date");
    let mut c = Cursor::new(text);

    let year = c.digits(4).ok_or_else(err)?;
    let sep = match c.peek() {
        Some(b'-') | Some(b'/') => c.bytes[c.pos],
        _ => return Err(err()),
    };
    c.pos += 1;
    let month = c.digits(2).ok_or_else(err)?;
    if !c.eat(sep) {
        return Err(err());
    }
    let day = c.digits(2).ok_or_else(err)?;

    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month as u32) {
        return Err(format!("{text:?} is not a valid calendar date"));
    }

    let mut millis = days_from_civil(year, month as u32, day as u32) * 86_400_000;

    // Optional time part after 'T' or a single space.
    if c.eat(b'T') || c.eat(b' ') {
        let hour = c.digits(2).ok_or_else(err)?;
        if !c.eat(b':') {
            return Err(err());
        }
        let minute = c.digits(2).ok_or_else(err)?;
        let mut second = 0;
        if c.eat(b':') {
            second = c.digits(2).ok_or_else(err)?;
        }
        if hour > 23 || minute > 59 || second > 59 {
            return Err(format!("{text:?} has an out-of-range time"));
        }
        millis += (hour * 3600 + minute * 60 + second) * 1000;
        if c.eat(b'.') {
            millis += frac_millis(&mut c).ok_or_else(err)?;
        }
        millis -= offset_minutes(&mut c).ok_or_else(err)? * 60_000;
    }

    if c.done() {
        Ok(millis)
    } else {
        Err(err())
    }
}

/// 1 to 9 fractional-second digits; the first three are milliseconds, the
/// rest are truncated.
fn frac_millis(c: &mut Cursor) -> Option<i64> {
    let mut count = 0;
    let mut ms = 0i64;
    while let Some(b) = c.peek() {
        if !b.is_ascii_digit() {
            break;
        }
        if count < 3 {
            ms = ms * 10 + i64::from(b - b'0');
        }
        count += 1;
        c.pos += 1;
    }
    if count == 0 || count > 9 {
        return None;
    }
    while count < 3 {
        ms *= 10;
        count += 1;
    }
    Some(ms)
}

/// `Z`, `+HH:MM`, `-HHMM` or `+HH`; empty means UTC. Returns signed minutes
/// east of UTC.
fn offset_minutes(c: &mut Cursor) -> Option<i64> {
    match c.peek() {
        None => Some(0),
        Some(b'Z') | Some(b'z') => {
            c.pos += 1;
            Some(0)
        }
        Some(sign @ (b'+' | b'-')) => {
            c.pos += 1;
            let hours = c.digits(2)?;
            let has_minutes = c.eat(b':') || c.peek().is_some_and(|b| b.is_ascii_digit());
            let minutes = if has_minutes { c.digits(2)? } else { 0 };
            if hours > 23 || minutes > 59 {
                return None;
            }
            let total = hours * 60 + minutes;
            Some(if sign == b'-' { -total } else { total })
        }
        Some(_) => None,
    }
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(y) => 29,
        _ => 28,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// days-from-civil formulation).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = u64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_dates_are_midnight_utc_around_the_epoch() {
        assert_eq!(parse_datetime("1970-01-01").unwrap(), 0);
        assert_eq!(parse_datetime("1970-01-02").unwrap(), 86_400_000);
        assert_eq!(parse_datetime("1969-12-31").unwrap(), -86_400_000);
    }

    #[test]
    fn slash_separator_is_accepted_but_not_mixed() {
        assert_eq!(
            parse_datetime("2026/07/01").unwrap(),
            parse_datetime("2026-07-01").unwrap()
        );
        assert!(parse_datetime("2026-07/01").is_err());
    }

    #[test]
    fn time_components_and_fractions_add_up() {
        let base = parse_datetime("2026-07-01").unwrap();
        assert_eq!(
            parse_datetime("2026-07-01T01:02:03").unwrap(),
            base + 3_723_000
        );
        assert_eq!(
            parse_datetime("2026-07-01 01:02").unwrap(),
            base + 3_720_000
        );
        assert_eq!(
            parse_datetime("2026-07-01T00:00:00.25").unwrap(),
            base + 250
        );
        assert_eq!(
            parse_datetime("2026-07-01T00:00:00.123456789").unwrap(),
            base + 123,
            "sub-millisecond digits truncate"
        );
    }

    #[test]
    fn offsets_normalize_to_the_same_instant() {
        let utc = parse_datetime("2026-01-02T03:04:05Z").unwrap();
        assert_eq!(parse_datetime("2026-01-02T05:04:05+02:00").unwrap(), utc);
        assert_eq!(parse_datetime("2026-01-01T21:34:05-0530").unwrap(), utc);
        assert_eq!(parse_datetime("2026-01-02T10:04:05+07").unwrap(), utc);
    }

    #[test]
    fn calendar_and_clock_bounds_are_enforced() {
        assert!(parse_datetime("2024-02-29").is_ok());
        assert!(parse_datetime("2000-02-29").is_ok(), "400-year rule");
        assert!(parse_datetime("2100-02-29").is_err(), "100-year rule");
        assert!(parse_datetime("2026-02-29").is_err());
        assert!(parse_datetime("2026-13-01").is_err());
        assert!(parse_datetime("2026-00-10").is_err());
        assert!(parse_datetime("2026-04-31").is_err());
        assert!(parse_datetime("2026-07-01T24:00").is_err());
        assert!(parse_datetime("2026-07-01T10:60").is_err());
    }

    #[test]
    fn garbage_and_trailing_content_are_rejected() {
        assert!(parse_datetime("2026-07-01junk").is_err());
        assert!(parse_datetime("2026-07-01T10:00:00Zx").is_err());
        assert!(parse_datetime("").is_err());
        assert!(parse_datetime("not a date").is_err());
    }

    #[test]
    fn instants_order_chronologically() {
        // The epoch-milliseconds values must order chronologically across
        // month, year and timezone boundaries.
        let seq = [
            "1999-12-31T23:59:59",
            "2000-01-01",
            "2000-02-29T12:00",
            "2026-07-01T09:00+09:00", // == 2026-07-01T00:00:00Z
            "2026-07-01T00:00:00.001",
        ];
        let parsed: Vec<i64> = seq.iter().map(|s| parse_datetime(s).unwrap()).collect();
        for pair in parsed.windows(2) {
            assert!(pair[0] < pair[1], "{parsed:?} must be strictly increasing");
        }
        // Cross-checked against `date -u -d 2026-07-01 +%s` == 1782864000.
        assert_eq!(
            parse_datetime("2026-07-01T00:00:00Z").unwrap(),
            1_782_864_000_000
        );
    }
}
