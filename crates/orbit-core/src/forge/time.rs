//! ISO 8601 timestamps from host and tracker APIs, as Unix milliseconds.

/// Parses `2024-05-01T12:34:56Z`, with optional fractional seconds and a
/// `Z`, `+02:00` or `+0200` offset. Returns 0 for anything else (the
/// models use 0 for "unknown"), including GitHub's `0001-01-01T00:00:00Z`.
pub fn parse_ms(text: &str) -> u64 {
    parse(text.trim()).unwrap_or(0)
}

fn parse(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        let mut ms = 0i64;
        for (i, d) in frac[..digits].bytes().take(3).enumerate() {
            ms += i64::from(d - b'0') * 10i64.pow(2 - i as u32);
        }
        millis = ms;
        rest = &frac[digits..];
    }
    let offset_minutes = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let digits: String = rest[1..].chars().filter(char::is_ascii_digit).collect();
            if digits.len() != 4 {
                return None;
            }
            sign * (digits[..2].parse::<i64>().ok()? * 60 + digits[2..].parse::<i64>().ok()?)
        }
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;
    let ms = secs * 1000 + millis;
    (ms > 0).then_some(ms as u64)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_in_every_host_format() {
        assert_eq!(parse_ms("1970-01-01T00:00:01Z"), 1000);
        assert_eq!(parse_ms("2024-05-01T12:34:56Z"), 1_714_566_896_000);
        assert_eq!(parse_ms("2024-05-01T12:34:56.789Z"), 1_714_566_896_789);
        assert_eq!(parse_ms("2024-05-01T14:34:56+02:00"), 1_714_566_896_000);
        assert_eq!(parse_ms("2024-05-01T14:34:56.000+0200"), 1_714_566_896_000);
        assert_eq!(parse_ms("2024-05-01T10:34:56.5-02:00"), 1_714_566_896_500);
        assert_eq!(parse_ms("0001-01-01T00:00:00Z"), 0);
        assert_eq!(parse_ms(""), 0);
        assert_eq!(parse_ms("yesterday"), 0);
    }
}
