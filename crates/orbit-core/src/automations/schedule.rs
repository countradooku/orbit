//! Schedules: five-field cron, a recurrence-rule subset and intervals, in
//! any IANA time zone.
//!
//! Hand-rolled on `jiff` (already linked, through `env_logger`) instead of
//! `croner`/`cron` plus `rrule`: those bring `chrono`, `chrono-tz` and
//! `regex` with a second time-zone database, for what is a bitset matcher
//! and a day-by-day search. Both syntaxes compile to one [`Matcher`], so cron
//! and rules share the search, the daylight-saving rules and the
//! descriptions.
//!
//! Daylight saving: a time that does not exist (the spring-forward gap)
//! runs right after the gap; a time that happens twice (the fall-back fold)
//! runs once, the first time. Every occurrence is strictly later than the
//! one before, so no instant ever fires twice.

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use orbit_proto::Trigger;

const MINUTE_MS: u64 = 60_000;
/// The search gives up after this many days (a rule such as "February
/// 30th" never matches).
const SEARCH_DAYS: i64 = 366 * 8;
/// 2024-01-01, a Monday: what `INTERVAL` counts from without `DTSTART`.
const ANCHOR_DAYS: i64 = 19_723;

/// A parsed schedule trigger.
#[derive(Clone, Debug)]
pub enum Schedule {
    Calendar { matcher: Box<Matcher>, tz: TimeZone },
    Interval { every_ms: u64 },
}

impl Schedule {
    /// Parses a schedule trigger; `Ok(None)` for event triggers.
    pub fn of(trigger: &Trigger) -> Result<Option<Schedule>, String> {
        Ok(Some(match trigger {
            Trigger::Cron { expr, tz } => Schedule::Calendar { matcher: Box::new(Matcher::cron(expr)?), tz: zone(tz)? },
            Trigger::Rule { rule, tz } => Schedule::Calendar { matcher: Box::new(Matcher::rule(rule)?), tz: zone(tz)? },
            Trigger::Interval { minutes } => {
                if *minutes == 0 {
                    return Err("An interval needs at least one minute.".into());
                }
                Schedule::Interval { every_ms: *minutes as u64 * MINUTE_MS }
            }
            Trigger::Event(_) => return Ok(None),
        }))
    }

    /// The first occurrence strictly after `after_ms` (Unix ms). For an
    /// interval that is one period later.
    pub fn next_after(&self, after_ms: u64) -> Option<u64> {
        match self {
            Schedule::Interval { every_ms } => Some(after_ms + every_ms),
            Schedule::Calendar { matcher, tz } => matcher.next_after(tz, after_ms),
        }
    }

    /// The next `count` occurrences after `after_ms`.
    pub fn upcoming(&self, after_ms: u64, count: usize) -> Vec<u64> {
        let mut out = Vec::with_capacity(count);
        let mut at = after_ms;
        while out.len() < count {
            match self.next_after(at) {
                Some(next) => {
                    out.push(next);
                    at = next;
                }
                None => break,
            }
        }
        out
    }

    /// A short description: `weekdays 9:00`, `every 15 min`.
    pub fn describe(&self) -> String {
        match self {
            Schedule::Interval { every_ms } => describe_minutes(every_ms / MINUTE_MS),
            Schedule::Calendar { matcher, .. } => matcher.describe(),
        }
    }

    /// How far apart runs are at least, roughly: jitter stays well inside
    /// it.
    pub fn min_gap_ms(&self) -> u64 {
        match self {
            Schedule::Interval { every_ms } => *every_ms,
            Schedule::Calendar { matcher, .. } => {
                if matcher.minutes.count_ones() > 1 || matcher.hours.count_ones() > 1 {
                    MINUTE_MS
                } else {
                    3_600_000
                }
            }
        }
    }

    /// The time zone of a calendar schedule.
    pub fn zone(&self) -> Option<&TimeZone> {
        match self {
            Schedule::Calendar { tz, .. } => Some(tz),
            Schedule::Interval { .. } => None,
        }
    }
}

/// An IANA zone by name; empty means the system's.
pub fn zone(name: &str) -> Result<TimeZone, String> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(TimeZone::system());
    }
    TimeZone::get(name).map_err(|_| format!("“{name}” is not a time zone Orbit knows (try “Europe/London”)."))
}

/// The system zone's name, for showing.
pub fn system_zone_name() -> String {
    TimeZone::system().iana_name().unwrap_or("UTC").to_owned()
}

fn describe_minutes(minutes: u64) -> String {
    match minutes {
        m if m % 1440 == 0 && m >= 1440 => plural(m / 1440, "day"),
        m if m % 60 == 0 && m >= 60 => plural(m / 60, "hour"),
        m => format!("every {m} min"),
    }
}

fn plural(n: u64, unit: &str) -> String {
    if n == 1 { format!("every {unit}") } else { format!("every {n} {unit}s") }
}

/// How often a rule's period repeats, and from where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Period {
    None,
    /// Every `n` hours, counted in hours since the anchor.
    Hours { n: u32, anchor: i64 },
    Days { n: u32, anchor: i64 },
    /// Anchored at the Monday of the anchor's week.
    Weeks { n: u32, anchor: i64 },
    /// Anchored at the anchor's month (months since year 0).
    Months { n: u32, anchor: i64 },
    Years { n: u32, anchor: i64 },
}

/// Which civil minutes match, as bitsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Matcher {
    minutes: u64,
    hours: u32,
    /// Bits 1–31.
    mdays: u32,
    /// Days counted from the month's end: bit 1 is the last day.
    last_mdays: u32,
    /// Bits 1–12.
    months: u16,
    /// Bit 0 is Monday.
    wdays: u8,
    /// (n, weekday): the n-th (negative: from the end) weekday of the month.
    nth: Vec<(i8, u8)>,
    /// Cron: a restricted day-of-month and day-of-week match either.
    either_day: bool,
    period: Period,
    /// No occurrence before (days, minute of day).
    start: Option<(i64, u32)>,
    /// None after.
    until: Option<(i64, u32)>,
    /// Kept for describing.
    freq: Freq,
    interval: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Freq {
    Cron,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

const ALL_MINUTES: u64 = (1 << 60) - 1;
const ALL_HOURS: u32 = (1 << 24) - 1;
const ALL_MDAYS: u32 = !1;
const ALL_MONTHS: u16 = 0b1_1111_1111_1110;
const ALL_WDAYS: u8 = 0x7f;
const WEEKDAYS: u8 = 0b1_1111;
const WEEKEND: u8 = 0b110_0000;

const DAY_NAMES: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
const SHORT_DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const LONG_DAYS: [&str; 7] = ["Mondays", "Tuesdays", "Wednesdays", "Thursdays", "Fridays", "Saturdays", "Sundays"];
const MONTH_NAMES: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const SHORT_MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

impl Matcher {
    fn base(freq: Freq) -> Self {
        Matcher {
            minutes: ALL_MINUTES,
            hours: ALL_HOURS,
            mdays: ALL_MDAYS,
            last_mdays: 0,
            months: ALL_MONTHS,
            wdays: ALL_WDAYS,
            nth: Vec::new(),
            either_day: false,
            period: Period::None,
            start: None,
            until: None,
            freq,
            interval: 1,
        }
    }

    /// Five fields: minute, hour, day of month, month, day of week. Lists
    /// (`1,15`), ranges (`1-5`), steps (`*/15`, `9-17/2`), names (`mon`,
    /// `jan`), `7` for Sunday, and `@hourly`, `@daily`, `@weekly`,
    /// `@monthly`, `@yearly`.
    pub fn cron(expr: &str) -> Result<Matcher, String> {
        let expr = expr.trim();
        let expanded = match expr.to_ascii_lowercase().as_str() {
            "@hourly" => "0 * * * *".to_owned(),
            "@daily" | "@midnight" => "0 0 * * *".to_owned(),
            "@weekly" => "0 0 * * 0".to_owned(),
            "@monthly" => "0 0 1 * *".to_owned(),
            "@yearly" | "@annually" => "0 0 1 1 *".to_owned(),
            _ if expr.starts_with('@') => return Err(format!("“{expr}” is not a cron shortcut.")),
            _ => expr.to_owned(),
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "A cron expression has five fields (minute hour day month weekday); this has {}.",
                fields.len()
            ));
        }
        let mut m = Matcher::base(Freq::Cron);
        m.minutes = field(fields[0], 0, 59, &[], "minute")?;
        m.hours = field(fields[1], 0, 23, &[], "hour")? as u32;
        m.mdays = field(fields[2], 1, 31, &[], "day of month")? as u32;
        m.months = field(fields[3], 1, 12, &MONTH_NAMES, "month")? as u16;
        // Cron counts Sunday as 0 (and 7); the matcher counts Monday as 0.
        let sunday_first = field(fields[4], 0, 7, &["sun", "mon", "tue", "wed", "thu", "fri", "sat"], "weekday")?;
        let mut wdays = 0u8;
        for d in 0..=7u32 {
            if sunday_first & (1 << d) != 0 {
                wdays |= 1 << ((d + 6) % 7);
            }
        }
        m.wdays = wdays;
        m.either_day = fields[2] != "*" && fields[4] != "*";
        Ok(m)
    }

    /// An iCalendar-style rule: `FREQ` (`HOURLY`, `DAILY`, `WEEKLY`,
    /// `MONTHLY`, `YEARLY`), `INTERVAL`, `BYDAY` (`MO,TU`; `1MO`, `-1FR` in
    /// monthly rules), `BYHOUR`, `BYMINUTE`, `BYMONTHDAY` (negative from the
    /// end), `BYMONTH`, `DTSTART` and `UNTIL` (`20261231T090000`).
    pub fn rule(rule: &str) -> Result<Matcher, String> {
        let text = rule.trim();
        let text = text.strip_prefix("RRULE:").unwrap_or(text);
        let mut freq = None;
        let (mut interval, mut byday, mut byhour, mut byminute, mut bymonthday, mut bymonth) =
            (1u32, None, None, None, None, None);
        let (mut dtstart, mut until) = (None, None);
        for part in text.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let (key, value) = part.split_once('=').ok_or_else(|| format!("“{part}” needs a value (KEY=VALUE)."))?;
            let value = value.trim();
            match key.trim().to_ascii_uppercase().as_str() {
                "FREQ" => {
                    freq = Some(match value.to_ascii_uppercase().as_str() {
                        "HOURLY" => Freq::Hourly,
                        "DAILY" => Freq::Daily,
                        "WEEKLY" => Freq::Weekly,
                        "MONTHLY" => Freq::Monthly,
                        "YEARLY" => Freq::Yearly,
                        "MINUTELY" | "SECONDLY" => {
                            return Err("For runs more often than hourly, use an interval.".into());
                        }
                        other => return Err(format!("“{other}” is not a frequency.")),
                    })
                }
                "INTERVAL" => {
                    interval = value
                        .parse::<u32>()
                        .ok()
                        .filter(|n| (1..=1000).contains(n))
                        .ok_or_else(|| format!("INTERVAL must be a number from 1, not “{value}”."))?
                }
                "BYDAY" => byday = Some(value.to_owned()),
                "BYHOUR" => byhour = Some(numbers(value, 0, 23, "BYHOUR")?),
                "BYMINUTE" => byminute = Some(numbers(value, 0, 59, "BYMINUTE")?),
                "BYMONTHDAY" => bymonthday = Some(value.to_owned()),
                "BYMONTH" => bymonth = Some(numbers(value, 1, 12, "BYMONTH")?),
                "DTSTART" => dtstart = Some(stamp(value)?),
                "UNTIL" => until = Some(stamp(value)?),
                "WKST" => {}
                "COUNT" => return Err("COUNT isn't supported; end a rule with UNTIL.".into()),
                other => return Err(format!("{other} isn't supported in Orbit's rules.")),
            }
        }
        let freq = freq.ok_or("A rule needs a FREQ, such as FREQ=DAILY.")?;
        let mut m = Matcher::base(freq);
        m.interval = interval;
        m.start = dtstart;
        m.until = until;
        let (start_day, start_minute) = dtstart.unwrap_or((ANCHOR_DAYS, 0));
        let (sy, smo, sd) = civil_from_days(start_day);
        let start_weekday = weekday(start_day);
        let one = |v: u32| -> u64 { 1u64 << v };
        m.minutes = match &byminute {
            Some(list) => *list,
            None if freq == Freq::Hourly && dtstart.is_none() => 1,
            None => one(start_minute % 60),
        };
        m.hours = match &byhour {
            Some(list) => *list as u32,
            None if freq == Freq::Hourly => ALL_HOURS,
            None => one(start_minute / 60) as u32,
        };
        if let Some(list) = &bymonth {
            m.months = (*list as u16) & ALL_MONTHS;
        }
        if let Some(days) = &byday {
            let (plain, nth) = weekdays(days)?;
            if !nth.is_empty() && !matches!(freq, Freq::Monthly | Freq::Yearly) {
                return Err("Numbered days (1MO, -1FR) only work in monthly or yearly rules.".into());
            }
            // With only numbered days, no plain weekday matches.
            m.nth = nth;
            m.wdays = plain;
        }
        if let Some(days) = &bymonthday {
            let (plain, last) = month_days(days)?;
            m.mdays = plain;
            m.last_mdays = last;
        }
        match freq {
            Freq::Weekly if byday.is_none() => m.wdays = 1 << start_weekday,
            Freq::Monthly | Freq::Yearly if byday.is_none() && bymonthday.is_none() => m.mdays = 1 << sd,
            _ => {}
        }
        if freq == Freq::Yearly && bymonth.is_none() {
            m.months = 1 << smo;
        }
        m.period = match (freq, interval) {
            (_, 1) => Period::None,
            (Freq::Hourly, n) => Period::Hours { n, anchor: start_day * 24 + (start_minute / 60) as i64 },
            (Freq::Daily, n) => Period::Days { n, anchor: start_day },
            (Freq::Weekly, n) => Period::Weeks { n, anchor: start_day - weekday(start_day) as i64 },
            (Freq::Monthly, n) => Period::Months { n, anchor: sy as i64 * 12 + smo as i64 - 1 },
            (Freq::Yearly, n) => Period::Years { n, anchor: sy as i64 },
            (Freq::Cron, _) => Period::None,
        };
        if m.minutes == 0 || m.hours == 0 || m.months == 0 || (m.wdays == 0 && m.nth.is_empty()) {
            return Err("This rule never matches.".into());
        }
        Ok(m)
    }

    fn day_matches(&self, days: i64, y: i32, mo: u32, d: u32) -> bool {
        if self.months & (1 << mo) == 0 {
            return false;
        }
        let wd = weekday(days);
        let len = month_len(y, mo);
        let mday_restricted = self.mdays != ALL_MDAYS || self.last_mdays != 0;
        let mday_ok = self.mdays & (1 << d) != 0 || self.last_mdays & (1 << (len - d + 1)) != 0;
        let nth_ok = self.nth.iter().any(|&(n, w)| {
            w as u32 == wd && {
                let index = ((d - 1) / 7 + 1) as i8;
                let from_end = -(((len - d) / 7 + 1) as i8);
                n == index || n == from_end
            }
        });
        let wday_ok = self.wdays & (1 << wd) != 0 || nth_ok;
        let day_ok = if self.either_day && mday_restricted { mday_ok || wday_ok } else { mday_ok && wday_ok };
        day_ok
            && match self.period {
                Period::Days { n, anchor } => (days - anchor).rem_euclid(n as i64) == 0,
                Period::Weeks { n, anchor } => ((days - anchor).div_euclid(7)).rem_euclid(n as i64) == 0,
                Period::Months { n, anchor } => (y as i64 * 12 + mo as i64 - 1 - anchor).rem_euclid(n as i64) == 0,
                Period::Years { n, anchor } => (y as i64 - anchor).rem_euclid(n as i64) == 0,
                _ => true,
            }
    }

    fn hour_matches(&self, days: i64, hour: u32) -> bool {
        self.hours & (1 << hour) != 0
            && match self.period {
                Period::Hours { n, anchor } => (days * 24 + hour as i64 - anchor).rem_euclid(n as i64) == 0,
                _ => true,
            }
    }

    /// The first matching civil minute at or after `(days, minute)`.
    fn next_civil(&self, mut days: i64, mut minute: u32) -> Option<(i64, u32)> {
        if let Some(start) = self.start
            && (days, minute) < start
        {
            (days, minute) = start;
        }
        let end = days + SEARCH_DAYS;
        while days < end {
            let (y, mo, d) = civil_from_days(days);
            if self.months & (1 << mo) == 0 {
                // Jump to the first of the next month.
                days += (month_len(y, mo) - d + 1) as i64;
                minute = 0;
                continue;
            }
            if self.day_matches(days, y, mo, d) {
                let mut hour = minute / 60;
                let mut from = minute % 60;
                while hour < 24 {
                    if self.hour_matches(days, hour) {
                        let left = self.minutes >> from << from;
                        if left != 0 {
                            let found = (days, hour * 60 + left.trailing_zeros());
                            if self.until.is_some_and(|u| found > u) {
                                return None;
                            }
                            return Some(found);
                        }
                    }
                    hour += 1;
                    from = 0;
                }
            }
            days += 1;
            minute = 0;
            if self.until.is_some_and(|u| (days, 0) > u) {
                return None;
            }
        }
        None
    }

    /// The first occurrence strictly after `after_ms`, in `tz`.
    pub fn next_after(&self, tz: &TimeZone, after_ms: u64) -> Option<u64> {
        let after = Timestamp::from_millisecond(after_ms as i64).ok()?;
        let local = tz.to_datetime(after);
        let (mut days, mut minute) = (
            days_from_civil(local.year() as i32, local.month() as u32, local.day() as u32),
            local.hour() as u32 * 60 + local.minute() as u32 + 1,
        );
        for _ in 0..64 {
            if minute >= 1440 {
                days += 1;
                minute -= 1440;
            }
            let (d, m) = self.next_civil(days, minute)?;
            let (y, mo, dd) = civil_from_days(d);
            let dt = DateTime::new(y as i16, mo as i8, dd as i8, (m / 60) as i8, (m % 60) as i8, 0, 0).ok()?;
            let ambiguous = tz.to_ambiguous_timestamp(dt);
            // A gap runs right after it; a fold runs the first time. If
            // that instant is not later (we are inside a fold already),
            // take the second one.
            for ts in [ambiguous.clone().compatible(), ambiguous.later()].into_iter().flatten() {
                let ms = ts.as_millisecond();
                if ms > after_ms as i64 {
                    return Some(ms as u64);
                }
            }
            (days, minute) = (d, m + 1);
        }
        None
    }

    pub fn describe(&self) -> String {
        let time = self.describe_time();
        let days = self.describe_days();
        let every = match (self.freq, self.interval) {
            (_, 1) | (Freq::Cron, _) => String::new(),
            (Freq::Hourly, n) => format!("every {n} hours"),
            (Freq::Daily, n) => format!("every {n} days"),
            (Freq::Weekly, n) => format!("every {n} weeks"),
            (Freq::Monthly, n) => format!("every {n} months"),
            (Freq::Yearly, n) => format!("every {n} years"),
        };
        let mut parts: Vec<String> = Vec::new();
        if !every.is_empty() {
            parts.push(every);
        }
        match (days.as_str(), time.0) {
            ("", _) | ("daily", false) => parts.push(time.1),
            (d, true) => parts.push(format!("{d} {}", time.1)),
            (d, false) => {
                parts.push(d.to_owned());
                parts.push(time.1);
            }
        }
        let mut text = parts.join(" · ");
        if let Some((d, m)) = self.until {
            let (y, mo, dd) = civil_from_days(d);
            text.push_str(&format!(" until {dd} {} {y} {}:{:02}", SHORT_MONTHS[mo as usize - 1], m / 60, m % 60));
        }
        text
    }

    /// (whether it reads after a day phrase, the text).
    fn describe_time(&self) -> (bool, String) {
        let minutes: Vec<u32> = bits(self.minutes as u128, 60);
        let hours: Vec<u32> = bits(self.hours as u128, 24);
        if matches!(self.period, Period::Hours { .. }) {
            return (false, format!("at {}", list(&minutes.iter().map(|m| format!(":{m:02}")).collect::<Vec<_>>())));
        }
        if self.hours == ALL_HOURS {
            if minutes.len() == 60 {
                return (false, "every minute".into());
            }
            if let Some(step) = step_of(&minutes, 60) {
                return (false, format!("every {step} min"));
            }
            if minutes.len() == 1 {
                return (false, format!("hourly at :{:02}", minutes[0]));
            }
            return (false, format!("hourly at {}", list(&minutes.iter().map(|m| format!(":{m:02}")).collect::<Vec<_>>())));
        }
        if let (Some(step), [minute]) = (step_of(&hours, 24), minutes.as_slice()) {
            let from = if hours[0] == 0 { String::new() } else { format!(" from {}:{minute:02}", hours[0]) };
            return (false, format!("every {step} hours at :{minute:02}{from}"));
        }
        if hours.len() * minutes.len() <= 4 {
            let times: Vec<String> =
                hours.iter().flat_map(|h| minutes.iter().map(move |m| format!("{h}:{m:02}"))).collect();
            return (true, list(&times));
        }
        (false, format!("{} times a day", hours.len() * minutes.len()))
    }

    fn describe_days(&self) -> String {
        let mut text = String::new();
        let mday_restricted = self.mdays != ALL_MDAYS || self.last_mdays != 0;
        let wday_restricted = self.wdays != ALL_WDAYS;
        if !self.nth.is_empty() {
            let names: Vec<String> = self
                .nth
                .iter()
                .map(|&(n, w)| {
                    let which = match n {
                        1 => "first",
                        2 => "second",
                        3 => "third",
                        4 => "fourth",
                        5 => "fifth",
                        -1 => "last",
                        -2 => "second to last",
                        _ => "nth",
                    };
                    format!("{which} {}", SHORT_DAYS[w as usize])
                })
                .collect();
            text = format!("on the {}", list(&names));
        } else if mday_restricted {
            let mut names: Vec<String> = bits(self.mdays as u128, 32).iter().map(|d| ordinal(*d)).collect();
            names.extend(bits(self.last_mdays as u128, 32).iter().map(|d| match d {
                1 => "last day".to_owned(),
                n => format!("{} to last day", ordinal(*n)),
            }));
            text = format!("monthly on the {}", list(&names));
            if wday_restricted {
                let joiner = if self.either_day { "or" } else { "if" };
                text.push_str(&format!(" {joiner} {}", self.weekday_text()));
            }
        } else if wday_restricted {
            text = self.weekday_text();
        } else if self.freq == Freq::Cron || matches!(self.freq, Freq::Daily) {
            if self.hours != ALL_HOURS {
                text = "daily".into();
            }
        }
        if self.months != ALL_MONTHS {
            let months: Vec<String> = bits(self.months as u128, 13).iter().map(|m| SHORT_MONTHS[*m as usize - 1].to_owned()).collect();
            if text.is_empty() {
                text = format!("in {}", list(&months));
            } else {
                text.push_str(&format!(" in {}", list(&months)));
            }
        }
        text
    }

    fn weekday_text(&self) -> String {
        match self.wdays {
            WEEKDAYS => "weekdays".into(),
            WEEKEND => "weekends".into(),
            ALL_WDAYS => "daily".into(),
            w if w.count_ones() == 1 => LONG_DAYS[w.trailing_zeros() as usize].into(),
            w => bits(w as u128, 7).iter().map(|d| SHORT_DAYS[*d as usize]).collect::<Vec<_>>().join(", "),
        }
    }
}

fn bits(mask: u128, len: u32) -> Vec<u32> {
    (0..len).filter(|b| mask & (1 << b) != 0).collect()
}

/// The step of an evenly spaced list starting at 0 that covers `range`.
fn step_of(values: &[u32], range: u32) -> Option<u32> {
    if values.len() < 2 || values[0] > values[1] {
        return None;
    }
    let step = values[1] - values[0];
    let evenly = values.windows(2).all(|w| w[1] - w[0] == step);
    let covers = values[0] < step && values.last().is_some_and(|l| l + step >= range);
    (evenly && covers && range % step == 0 && values[0] == 0).then_some(step)
}

fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// One cron field as a bitset.
fn field(text: &str, min: u32, max: u32, names: &[&str], what: &str) -> Result<u64, String> {
    let mut mask = 0u64;
    let value = |s: &str| -> Result<u32, String> {
        let lower = s.to_ascii_lowercase();
        if let Some(i) = names.iter().position(|n| *n == lower) {
            // Month names count from 1; weekday names from 0.
            return Ok(i as u32 + if min == 1 { 1 } else { 0 });
        }
        s.parse::<u32>().ok().filter(|v| (min..=max).contains(v)).ok_or_else(|| {
            format!("“{s}” is not a valid {what} (use {min}–{max}{}).", if names.is_empty() { "" } else { " or a name" })
        })
    };
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 =
                    step.parse().ok().filter(|s| *s > 0).ok_or_else(|| format!("“{step}” is not a valid step in the {what} field."))?;
                (range, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let (a, b) = (value(a)?, value(b)?);
            if a > b {
                return Err(format!("The {what} range “{range}” runs backwards."));
            }
            (a, b)
        } else {
            let v = value(range)?;
            // `5/15` means from 5 to the end in steps of 15.
            (v, if part.contains('/') { max } else { v })
        };
        let mut v = lo;
        while v <= hi {
            mask |= 1 << v;
            v += step;
        }
    }
    if mask == 0 {
        return Err(format!("The {what} field is empty."));
    }
    Ok(mask)
}

fn numbers(text: &str, min: u32, max: u32, key: &str) -> Result<u64, String> {
    let mut mask = 0u64;
    for part in text.split(',').map(str::trim) {
        let v: u32 = part
            .parse()
            .ok()
            .filter(|v| (min..=max).contains(v))
            .ok_or_else(|| format!("{key} takes numbers from {min} to {max}, not “{part}”."))?;
        mask |= 1 << v;
    }
    Ok(mask)
}

/// `MO,TU` and numbered `1MO,-1FR`: (plain weekdays, numbered ones).
fn weekdays(text: &str) -> Result<(u8, Vec<(i8, u8)>), String> {
    let mut plain = 0u8;
    let mut nth = Vec::new();
    for part in text.split(',').map(|p| p.trim().to_ascii_uppercase()) {
        if part.len() < 2 {
            return Err(format!("“{part}” is not a day (MO, TU, WE, TH, FR, SA, SU)."));
        }
        let (num, name) = part.split_at(part.len() - 2);
        let day = DAY_NAMES
            .iter()
            .position(|d| *d == name)
            .ok_or_else(|| format!("“{part}” is not a day (MO, TU, WE, TH, FR, SA, SU)."))? as u8;
        if num.is_empty() {
            plain |= 1 << day;
        } else {
            let n: i8 = num
                .parse()
                .ok()
                .filter(|n: &i8| *n != 0 && (-5..=5).contains(n))
                .ok_or_else(|| format!("“{part}”: the number before a day is 1–5 or −1 to −5."))?;
            nth.push((n, day));
        }
    }
    Ok((plain, nth))
}

/// `1,15,-1`: (days, days from the end).
fn month_days(text: &str) -> Result<(u32, u32), String> {
    let (mut plain, mut last) = (0u32, 0u32);
    for part in text.split(',').map(str::trim) {
        let n: i32 = part
            .parse()
            .ok()
            .filter(|n: &i32| *n != 0 && (-31..=31).contains(n))
            .ok_or_else(|| format!("BYMONTHDAY takes 1–31 or −1 to −31, not “{part}”."))?;
        if n > 0 {
            plain |= 1 << n;
        } else {
            last |= 1 << (-n);
        }
    }
    Ok((if plain == 0 && last != 0 { 0 } else { plain }, last))
}

/// `20261231`, `20261231T0900`, `20261231T090000Z` → (days, minute of day).
fn stamp(text: &str) -> Result<(i64, u32), String> {
    let t = text.trim().trim_end_matches('Z');
    let bad = || format!("“{text}” is not a date such as 20261231T090000.");
    let (date, time) = t.split_once('T').unwrap_or((t, ""));
    if date.len() != 8 || !date.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let y: i32 = date[..4].parse().map_err(|_| bad())?;
    let mo: u32 = date[4..6].parse().map_err(|_| bad())?;
    let d: u32 = date[6..8].parse().map_err(|_| bad())?;
    if !(1..=12).contains(&mo) || d == 0 || d > month_len(y, mo) {
        return Err(bad());
    }
    let minute = if time.len() >= 4 {
        let h: u32 = time[..2].parse().map_err(|_| bad())?;
        let m: u32 = time[2..4].parse().map_err(|_| bad())?;
        if h > 23 || m > 59 {
            return Err(bad());
        }
        h * 60 + m
    } else if time.is_empty() {
        0
    } else {
        return Err(bad());
    };
    Ok((days_from_civil(y, mo, d), minute))
}

// ── Civil calendar arithmetic (proleptic Gregorian) ───────────────────────

/// Days since 1970-01-01.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y as i64 - 1 } else { y as i64 };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month, day) of a day number.
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
}

/// Monday = 0.
pub fn weekday(days: i64) -> u32 {
    (days + 3).rem_euclid(7) as u32
}

fn month_len(y: i32, m: u32) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The local day number of an instant in `tz` (for daily run limits).
pub fn local_day(tz: &TimeZone, ms: u64) -> u32 {
    let Ok(ts) = Timestamp::from_millisecond(ms as i64) else { return 0 };
    let dt = tz.to_datetime(ts);
    days_from_civil(dt.year() as i32, dt.month() as u32, dt.day() as u32).max(0) as u32
}

/// `YYYY-MM-DD` and `HH:MM` of an instant in `tz`.
pub fn local_date_time(tz: &TimeZone, ms: u64) -> (String, String) {
    let Ok(ts) = Timestamp::from_millisecond(ms as i64) else { return (String::new(), String::new()) };
    let dt = tz.to_datetime(ts);
    (
        format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day()),
        format!("{:02}:{:02}", dt.hour(), dt.minute()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(tz: &str, y: i16, mo: i8, d: i8, h: i8, mi: i8) -> u64 {
        let dt = DateTime::new(y, mo, d, h, mi, 0, 0).unwrap();
        TimeZone::get(tz).unwrap().to_ambiguous_timestamp(dt).compatible().unwrap().as_millisecond() as u64
    }

    fn local(tz: &str, ms: u64) -> String {
        let dt = TimeZone::get(tz).unwrap().to_datetime(Timestamp::from_millisecond(ms as i64).unwrap());
        format!("{:04}-{:02}-{:02} {:02}:{:02}", dt.year(), dt.month(), dt.day(), dt.hour(), dt.minute())
    }

    fn cron(expr: &str, tz: &str) -> Schedule {
        Schedule::of(&Trigger::Cron { expr: expr.into(), tz: tz.into() }).unwrap().unwrap()
    }

    fn rule(rule: &str, tz: &str) -> Schedule {
        Schedule::of(&Trigger::Rule { rule: rule.into(), tz: tz.into() }).unwrap().unwrap()
    }

    fn upcoming(s: &Schedule, tz: &str, from: u64, n: usize) -> Vec<String> {
        s.upcoming(from, n).into_iter().map(|t| local(tz, t)).collect()
    }

    #[test]
    fn civil_days_round_trip() {
        for days in -800_000..800_000i64 {
            if days % 997 != 0 {
                continue;
            }
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(days_from_civil(2024, 1, 1), ANCHOR_DAYS);
        assert_eq!(weekday(ANCHOR_DAYS), 0, "2024-01-01 is a Monday");
    }

    #[test]
    fn weekdays_at_nine_in_a_time_zone() {
        let tz = "America/New_York";
        let s = cron("0 9 * * 1-5", tz);
        // Friday 2026-11-27 10:00 → Monday, Tuesday…
        let from = at(tz, 2026, 11, 27, 10, 0);
        assert_eq!(upcoming(&s, tz, from, 3), ["2026-11-30 09:00", "2026-12-01 09:00", "2026-12-02 09:00"]);
        assert_eq!(s.describe(), "weekdays 9:00");
        let r = rule("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=9;BYMINUTE=0", tz);
        assert_eq!(r.upcoming(from, 5), s.upcoming(from, 5));
        assert_eq!(r.describe(), "weekdays 9:00");
    }

    #[test]
    fn the_same_rule_in_two_zones_is_hours_apart() {
        let s1 = cron("30 8 * * *", "Asia/Tokyo");
        let s2 = cron("30 8 * * *", "Europe/London");
        let from = at("UTC", 2026, 5, 31, 12, 0);
        let (a, b) = (s1.next_after(from).unwrap(), s2.next_after(from).unwrap());
        assert_eq!(local("Asia/Tokyo", a), "2026-06-01 08:30");
        assert_eq!(local("Europe/London", b), "2026-06-01 08:30");
        assert_eq!(b - a, 8 * 3_600_000, "BST is 8 hours behind JST in June");
    }

    #[test]
    fn spring_forward_runs_after_the_gap_once() {
        let tz = "America/New_York";
        // 2026-03-08 02:00 → 03:00 EDT.
        let daily = cron("30 2 * * *", tz);
        let from = at(tz, 2026, 3, 7, 12, 0);
        // 02:30 does not exist on the 8th: it runs right after the gap.
        assert_eq!(upcoming(&daily, tz, from, 3), ["2026-03-08 03:30", "2026-03-09 02:30", "2026-03-10 02:30"]);
        // Every 15 minutes through the gap: no instant twice, none lost
        // that exists.
        let quarter = cron("*/15 * * * *", tz);
        let times = quarter.upcoming(at(tz, 2026, 3, 8, 1, 30), 6);
        assert!(times.windows(2).all(|w| w[1] > w[0]));
        let shown: Vec<String> = times.iter().map(|t| local(tz, *t)).collect();
        assert_eq!(shown, ["2026-03-08 01:45", "2026-03-08 03:00", "2026-03-08 03:15", "2026-03-08 03:30", "2026-03-08 03:45", "2026-03-08 04:00"]);
    }

    #[test]
    fn fall_back_runs_once_in_the_fold() {
        let tz = "America/New_York";
        // 2026-11-01 02:00 EDT → 01:00 EST: 01:30 happens twice.
        let daily = cron("30 1 * * *", tz);
        let from = at(tz, 2026, 10, 31, 12, 0);
        let runs = daily.upcoming(from, 2);
        assert_eq!(local(tz, runs[0]), "2026-11-01 01:30");
        assert_eq!(local(tz, runs[1]), "2026-11-02 01:30", "not again an hour later");
        // Starting inside the second 01:xx (after the first 01:30 ran):
        let second = runs[0] + 3_600_000 - 20 * 60_000; // 01:10 EST
        let next = daily.next_after(second).unwrap();
        assert!(next > second);
        assert_eq!(local(tz, next), "2026-11-01 01:30", "the second 01:30 when the first is past");
    }

    #[test]
    fn cron_syntax() {
        assert_eq!(cron("@hourly", "UTC").describe(), "hourly at :00");
        assert_eq!(cron("*/15 * * * *", "UTC").describe(), "every 15 min");
        assert_eq!(cron("0 */2 * * *", "UTC").describe(), "every 2 hours at :00");
        assert_eq!(cron("0 9,14 * * MON-FRI", "UTC").describe(), "weekdays 9:00 and 14:00");
        assert_eq!(cron("0 3 * * 0", "UTC").describe(), "Sundays 3:00");
        assert_eq!(cron("0 3 * * 7", "UTC").describe(), "Sundays 3:00");
        assert_eq!(cron("0 2 * * *", "UTC").describe(), "daily 2:00");
        assert_eq!(cron("0 0 1,15 * *", "UTC").describe(), "monthly on the 1st and 15th 0:00");
        assert_eq!(cron("0 0 1 jan *", "UTC").describe(), "monthly on the 1st in Jan 0:00");
        for bad in ["* * * *", "61 * * * *", "* 24 * * *", "5-1 * * * *", "*/0 * * * *", "@sometimes", "0 0 0 * *"] {
            assert!(Matcher::cron(bad).is_err(), "{bad} should not parse");
        }
        // Day of month and weekday both set: either matches (cron rules).
        let s = cron("0 12 13 * 5", "UTC");
        let got = upcoming(&s, "UTC", at("UTC", 2026, 11, 1, 0, 0), 3);
        assert_eq!(got, ["2026-11-06 12:00", "2026-11-13 12:00", "2026-11-20 12:00"]);
    }

    #[test]
    fn rules_with_intervals_and_numbered_days() {
        let tz = "UTC";
        let biweekly = rule("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO;BYHOUR=10;BYMINUTE=0;DTSTART=20261102T100000", tz);
        let got = upcoming(&biweekly, tz, at(tz, 2026, 11, 1, 0, 0), 3);
        assert_eq!(got, ["2026-11-02 10:00", "2026-11-16 10:00", "2026-11-30 10:00"]);
        assert_eq!(biweekly.describe(), "every 2 weeks · Mondays 10:00");
        let first_monday = rule("FREQ=MONTHLY;BYDAY=1MO;BYHOUR=9", tz);
        let got = upcoming(&first_monday, tz, at(tz, 2026, 11, 3, 0, 0), 2);
        assert_eq!(got, ["2026-12-07 09:00", "2027-01-04 09:00"]);
        let last_day = rule("FREQ=MONTHLY;BYMONTHDAY=-1;BYHOUR=17;BYMINUTE=30", tz);
        let got = upcoming(&last_day, tz, at(tz, 2027, 1, 31, 18, 0), 2);
        assert_eq!(got, ["2027-02-28 17:30", "2027-03-31 17:30"]);
        let every_3_hours = rule("FREQ=HOURLY;INTERVAL=3;BYMINUTE=5", tz);
        let got = upcoming(&every_3_hours, tz, at(tz, 2026, 11, 3, 1, 0), 2);
        assert_eq!(got, ["2026-11-03 03:05", "2026-11-03 06:05"]);
        let until = rule("FREQ=DAILY;BYHOUR=9;UNTIL=20261105", tz);
        assert_eq!(until.upcoming(at(tz, 2026, 11, 3, 0, 0), 10).len(), 2);
        for bad in ["BYDAY=MO", "FREQ=DAILY;COUNT=3", "FREQ=WEEKLY;BYDAY=XX", "FREQ=MINUTELY", "FREQ=DAILY;BYHOUR=25", "FREQ=WEEKLY;BYDAY=1MO"] {
            assert!(Matcher::rule(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn intervals_and_bad_zones() {
        let s = Schedule::of(&Trigger::Interval { minutes: 90 }).unwrap().unwrap();
        assert_eq!(s.next_after(1_000), Some(1_000 + 90 * 60_000));
        assert_eq!(s.describe(), "every 90 min");
        assert_eq!(Schedule::of(&Trigger::Interval { minutes: 120 }).unwrap().unwrap().describe(), "every 2 hours");
        assert!(Schedule::of(&Trigger::Interval { minutes: 0 }).is_err());
        assert!(Schedule::of(&Trigger::Cron { expr: "0 9 * * *".into(), tz: "Mars/Olympus".into() }).is_err());
        assert!(Schedule::of(&Trigger::Cron { expr: "0 9 * * *".into(), tz: String::new() }).unwrap().is_some());
    }

    #[test]
    fn impossible_dates_end_the_search() {
        let s = cron("0 0 30 2 *", "UTC");
        assert_eq!(s.next_after(at("UTC", 2026, 1, 1, 0, 0)), None);
    }
}
