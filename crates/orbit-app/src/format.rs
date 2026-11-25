//! Interface copy for numbers and times. Short, sentence case, tabular.

/// `now`, `4m`, `3h`, `2d`, then a short date. Used in dense lists where the
/// column is narrow and scanned, not read.
pub fn relative_time(then_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    match secs {
        0..=59 => "now".into(),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        86_400..=604_799 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

/// `950`, `12.4K`, `1.2M`.
pub fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=99_999 => trim(format!("{:.1}K", n as f64 / 1e3)),
        100_000..=999_999 => format!("{}K", n / 1_000),
        _ => trim(format!("{:.1}M", n as f64 / 1e6)),
    }
}

/// `$0.04`, `$1.20`, `<$0.01`; empty for zero.
pub fn cost(micros: u64) -> String {
    match micros {
        0 => String::new(),
        1..=9_999 => "<$0.01".into(),
        _ => format!("${:.2}", micros as f64 / 1e6),
    }
}

/// `850 ms`, `12 s`, `3m 05s`, `1h 02m`.
pub fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    match ms {
        0..=999 => format!("{ms} ms"),
        1_000..=59_999 => format!("{secs} s"),
        60_000..=3_599_999 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// A reset time as a clock time when it is today, else a relative day.
pub fn resets_at(resets_ms: u64, now_ms: u64) -> String {
    if resets_ms == 0 {
        return "Resets soon".into();
    }
    let mins = resets_ms.saturating_sub(now_ms) / 60_000;
    match mins {
        0 => "Resets in under a minute".into(),
        1..=59 => format!("Resets in {mins} min"),
        60..=1_439 => format!("Resets in {}h {:02}m", mins / 60, mins % 60),
        _ => format!("Resets in {} days", mins / 1_440),
    }
}

fn trim(s: String) -> String {
    s.replace(".0K", "K").replace(".0M", "M")
}

// ── Times of day ─────────────────────────────────────────────────────────

/// Whether clock times read `14:05` or `2:05 PM` (Settings › Appearance ›
/// Clock). A process-wide flag, so formatting needs no context.
static CLOCK_12H: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn clock_24h() -> bool {
    !CLOCK_12H.load(std::sync::atomic::Ordering::Relaxed)
}

/// Follows the clock setting; called whenever settings change.
pub fn set_clock(clock: orbit_proto::ClockFormat) {
    CLOCK_12H.store(clock == orbit_proto::ClockFormat::H12, std::sync::atomic::Ordering::Relaxed);
}

fn local(ms: u64) -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone as _;
    chrono::Local.timestamp_millis_opt(ms as i64).single().unwrap_or_default()
}

/// The local calendar day of a Unix time, as a day number: equal numbers
/// are the same day where the reader is.
pub fn local_day(ms: u64) -> i32 {
    use chrono::Datelike as _;
    local(ms).date_naive().num_days_from_ce()
}

/// A message's time, as short as the day allows: `14:05` today,
/// `Yesterday 14:05`, `Mon 14:05` this week, `12 Nov 14:05` this year,
/// `12 Nov 2025` before.
pub fn timestamp(ms: u64, now_ms: u64) -> String {
    timestamp_in(local(ms).naive_local(), local(now_ms).naive_local(), clock_24h())
}

/// A day divider's label: `Today`, `Yesterday`, `Monday`, `12 November`,
/// `12 November 2025`.
pub fn day_label(ms: u64, now_ms: u64) -> String {
    day_label_in(local(ms).naive_local(), local(now_ms).naive_local())
}

/// The full time for a tooltip: `Tuesday, 24 November 2026, 14:05:09`.
pub fn full_timestamp(ms: u64) -> String {
    let at = local(ms).naive_local();
    format!("{}, {}", at.format("%A, %-d %B %Y"), clock(at, clock_24h(), true))
}

fn clock(at: chrono::NaiveDateTime, h24: bool, seconds: bool) -> String {
    match (h24, seconds) {
        (true, false) => at.format("%H:%M").to_string(),
        (true, true) => at.format("%H:%M:%S").to_string(),
        (false, false) => at.format("%-I:%M %p").to_string(),
        (false, true) => at.format("%-I:%M:%S %p").to_string(),
    }
}

fn timestamp_in(at: chrono::NaiveDateTime, now: chrono::NaiveDateTime, h24: bool) -> String {
    use chrono::Datelike as _;
    let days = (now.date() - at.date()).num_days();
    let time = clock(at, h24, false);
    match days {
        0 => time,
        1 => format!("Yesterday {time}"),
        2..=6 => format!("{} {time}", at.format("%a")),
        _ if at.year() == now.year() => format!("{} {time}", at.format("%-d %b")),
        _ => at.format("%-d %b %Y").to_string(),
    }
}

fn day_label_in(at: chrono::NaiveDateTime, now: chrono::NaiveDateTime) -> String {
    use chrono::Datelike as _;
    match (now.date() - at.date()).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => at.format("%A").to_string(),
        _ if at.year() == now.year() => at.format("%-d %B").to_string(),
        _ => at.format("%-d %B %Y").to_string(),
    }
}

/// Bytes as `812 B`, `12.4 KB`, `3.1 MB`.
pub fn bytes(n: u64) -> String {
    match n {
        0..=999 => format!("{n} B"),
        1_000..=999_999 => trim_unit(format!("{:.1} KB", n as f64 / 1e3)),
        _ => trim_unit(format!("{:.1} MB", n as f64 / 1e6)),
    }
}

fn trim_unit(s: String) -> String {
    s.replace(".0 ", " ")
}

/// Compact JSON (a tool's input) indented two spaces per level, for
/// reading. Text that is not JSON comes back as it was.
pub fn pretty_json(json: &str) -> String {
    let trimmed = json.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return json.to_owned();
    }
    let mut out = String::with_capacity(trimmed.len() + trimmed.len() / 4);
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str("  ");
        }
    };
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                // Keep empty objects and arrays on one line.
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
                if matches!(chars.peek(), Some('}' | ']')) {
                    out.push(chars.next().unwrap());
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> chrono::NaiveDateTime {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn timestamps_are_as_short_as_the_day_allows() {
        let now = at("2026-11-25 10:00");
        assert_eq!(timestamp_in(at("2026-11-25 09:05"), now, true), "09:05");
        assert_eq!(timestamp_in(at("2026-11-25 21:05"), now, false), "9:05 PM");
        assert_eq!(timestamp_in(at("2026-11-24 23:59"), now, true), "Yesterday 23:59");
        assert_eq!(timestamp_in(at("2026-11-21 08:00"), now, true), "Sat 08:00");
        assert_eq!(timestamp_in(at("2026-03-02 08:00"), now, true), "2 Mar 08:00");
        assert_eq!(timestamp_in(at("2025-03-02 08:00"), now, true), "2 Mar 2025");
        assert_eq!(day_label_in(at("2026-11-25 00:01"), now), "Today");
        assert_eq!(day_label_in(at("2026-11-24 12:00"), now), "Yesterday");
        assert_eq!(day_label_in(at("2026-11-20 12:00"), now), "Friday");
        assert_eq!(day_label_in(at("2026-01-20 12:00"), now), "20 January");
        assert_eq!(day_label_in(at("2024-01-20 12:00"), now), "20 January 2024");
    }

    #[test]
    fn json_reads_indented_and_strings_stay_intact() {
        assert_eq!(
            pretty_json(r#"{"command":"ls, -la","opts":{"a":[1,2],"b":{}},"s":"x\"}{y"}"#),
            "{\n  \"command\": \"ls, -la\",\n  \"opts\": {\n    \"a\": [\n      1,\n      2\n    ],\n    \"b\": {}\n  },\n  \"s\": \"x\\\"}{y\"\n}"
        );
        assert_eq!(pretty_json("plain text"), "plain text");
        assert_eq!(bytes(812), "812 B");
        assert_eq!(bytes(12_400), "12.4 KB");
        assert_eq!(bytes(3_000_000), "3 MB");
    }

    #[test]
    fn numbers_read_at_a_glance() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(12_400), "12.4K");
        assert_eq!(tokens(41_000), "41K");
        assert_eq!(tokens(200_000), "200K");
        assert_eq!(tokens(1_200_000), "1.2M");
        assert_eq!(cost(0), "");
        assert_eq!(cost(8_315), "<$0.01");
        assert_eq!(cost(1_204_000), "$1.20");
        assert_eq!(duration(850), "850 ms");
        assert_eq!(duration(185_000), "3m 05s");
        assert_eq!(relative_time(0, 30_000), "now");
        assert_eq!(relative_time(0, 3 * 3_600_000), "3h");
    }
}
