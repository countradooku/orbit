//! The editor's schedule builder: a few plain choices ("every weekday at
//! 9:00") that become a recurrence rule, plus cron and rule text for experts.
//! Pure, so it is tested without a window.

use orbit_core::automations::schedule::Schedule;
use orbit_proto::Trigger;

/// How often, as the builder offers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    /// No schedule: only events start runs.
    Never,
    Hourly,
    Daily,
    Weekdays,
    Weekly,
    Monthly,
    Interval,
    Cron,
    Rule,
}

impl Repeat {
    pub const MENU: [Repeat; 9] = [
        Repeat::Hourly,
        Repeat::Daily,
        Repeat::Weekdays,
        Repeat::Weekly,
        Repeat::Monthly,
        Repeat::Interval,
        Repeat::Cron,
        Repeat::Rule,
        Repeat::Never,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Repeat::Never => "No schedule",
            Repeat::Hourly => "Every hour",
            Repeat::Daily => "Every day",
            Repeat::Weekdays => "Every weekday",
            Repeat::Weekly => "Every week",
            Repeat::Monthly => "Every month",
            Repeat::Interval => "At an interval",
            Repeat::Cron => "Cron expression",
            Repeat::Rule => "Recurrence rule",
        }
    }
}

pub const DAY_LETTERS: [&str; 7] = ["M", "T", "W", "T", "F", "S", "S"];
pub const DAY_NAMES: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const RULE_DAYS: [&str; 7] = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];

/// The builder's choices; text fields stay text until a trigger is built,
/// so a half-typed time is an error message, not a lost keystroke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Builder {
    pub repeat: Repeat,
    /// `09:00` (daily, weekdays, weekly, monthly).
    pub time: String,
    /// `15` (hourly: minutes past the hour).
    pub minute: String,
    /// Weekly: Monday is bit 0.
    pub days: u8,
    /// Monthly: `1`–`31`, or `last`.
    pub month_day: String,
    /// Interval: a number of `unit_hours` hours or minutes.
    pub every: String,
    pub unit_hours: bool,
    pub cron: String,
    pub rule: String,
    /// IANA zone; empty is the computer's.
    pub tz: String,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            repeat: Repeat::Weekdays,
            time: "09:00".into(),
            minute: "0".into(),
            days: 1,
            month_day: "1".into(),
            every: "30".into(),
            unit_hours: false,
            cron: "0 9 * * 1-5".into(),
            rule: "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=9;BYMINUTE=0".into(),
            tz: String::new(),
        }
    }
}

/// `9`, `9:30`, `09:30`, `21:05` → (hour, minute).
pub fn parse_time(text: &str) -> Option<(u32, u32)> {
    let text = text.trim();
    let (h, m) = match text.split_once(':') {
        Some((h, m)) => (h.trim().parse().ok()?, m.trim().parse().ok()?),
        None => (text.parse().ok()?, 0),
    };
    (h < 24 && m < 60).then_some((h, m))
}

impl Builder {
    /// The builder for an existing schedule trigger: plain choices when the
    /// trigger is one of them, else its text.
    pub fn of(trigger: Option<&Trigger>) -> Builder {
        let mut b = Builder::default();
        match trigger {
            None => b.repeat = Repeat::Never,
            Some(Trigger::Interval { minutes }) => {
                b.repeat = Repeat::Interval;
                b.unit_hours = *minutes >= 60 && minutes % 60 == 0;
                b.every = if b.unit_hours { (minutes / 60).to_string() } else { minutes.to_string() };
            }
            Some(Trigger::Cron { expr, tz }) => {
                b.repeat = Repeat::Cron;
                b.cron = expr.clone();
                b.tz = tz.clone();
            }
            Some(Trigger::Rule { rule, tz }) => {
                b.tz = tz.clone();
                b.rule = rule.clone();
                if !b.read_rule(rule) {
                    b.repeat = Repeat::Rule;
                }
            }
            Some(Trigger::Event(_)) => b.repeat = Repeat::Never,
        }
        b
    }

    /// Fills the plain choices from a rule they can express.
    fn read_rule(&mut self, rule: &str) -> bool {
        let mut fields = std::collections::HashMap::new();
        for part in rule.trim().trim_start_matches("RRULE:").split(';').filter(|p| !p.trim().is_empty()) {
            let Some((k, v)) = part.split_once('=') else { return false };
            fields.insert(k.trim().to_ascii_uppercase(), v.trim().to_ascii_uppercase());
        }
        if fields.keys().any(|k| !matches!(k.as_str(), "FREQ" | "BYDAY" | "BYHOUR" | "BYMINUTE" | "BYMONTHDAY")) {
            return false;
        }
        let single = |key: &str, max: u32| -> Option<u32> {
            match fields.get(key) {
                None => Some(0),
                Some(v) => v.parse::<u32>().ok().filter(|n| *n <= max),
            }
        };
        let (Some(hour), Some(minute)) = (single("BYHOUR", 23), single("BYMINUTE", 59)) else { return false };
        let mut days = 0u8;
        if let Some(list) = fields.get("BYDAY") {
            for d in list.split(',') {
                match RULE_DAYS.iter().position(|x| *x == d) {
                    Some(i) => days |= 1 << i,
                    None => return false,
                }
            }
        }
        self.time = format!("{hour:02}:{minute:02}");
        self.minute = minute.to_string();
        let freq = fields.get("FREQ").map(String::as_str).unwrap_or_default();
        self.repeat = match freq {
            "HOURLY" if !fields.contains_key("BYHOUR") && days == 0 && !fields.contains_key("BYMONTHDAY") => Repeat::Hourly,
            "DAILY" if days == 0 && !fields.contains_key("BYMONTHDAY") => Repeat::Daily,
            "WEEKLY" if days == 0b1_1111 => Repeat::Weekdays,
            "WEEKLY" if days != 0 => {
                self.days = days;
                Repeat::Weekly
            }
            "MONTHLY" if days == 0 => match fields.get("BYMONTHDAY").map(String::as_str) {
                Some("-1") => {
                    self.month_day = "last".into();
                    Repeat::Monthly
                }
                Some(d) if d.parse::<u32>().is_ok_and(|d| (1..=31).contains(&d)) => {
                    self.month_day = d.to_owned();
                    Repeat::Monthly
                }
                _ => return false,
            },
            _ => return false,
        };
        true
    }

    /// The trigger the choices make; `Ok(None)` for no schedule.
    pub fn trigger(&self) -> Result<Option<Trigger>, String> {
        let tz = self.tz.trim().to_owned();
        let time = || parse_time(&self.time).ok_or_else(|| format!("“{}” is not a time; use 24-hour HH:MM.", self.time.trim()));
        let rule = |body: String| Ok(Some(Trigger::Rule { rule: body, tz: tz.clone() }));
        match self.repeat {
            Repeat::Never => Ok(None),
            Repeat::Hourly => {
                let minute: u32 = self
                    .minute
                    .trim()
                    .trim_start_matches(':')
                    .parse()
                    .ok()
                    .filter(|m| *m < 60)
                    .ok_or("Minutes past the hour are 0–59.")?;
                rule(format!("FREQ=HOURLY;BYMINUTE={minute}"))
            }
            Repeat::Daily => {
                let (h, m) = time()?;
                rule(format!("FREQ=DAILY;BYHOUR={h};BYMINUTE={m}"))
            }
            Repeat::Weekdays => {
                let (h, m) = time()?;
                rule(format!("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR={h};BYMINUTE={m}"))
            }
            Repeat::Weekly => {
                let (h, m) = time()?;
                if self.days == 0 {
                    return Err("Pick at least one day.".into());
                }
                let days: Vec<&str> = (0..7).filter(|d| self.days & (1 << d) != 0).map(|d| RULE_DAYS[d]).collect();
                rule(format!("FREQ=WEEKLY;BYDAY={};BYHOUR={h};BYMINUTE={m}", days.join(",")))
            }
            Repeat::Monthly => {
                let (h, m) = time()?;
                let day = match self.month_day.trim().to_ascii_lowercase().as_str() {
                    "last" | "-1" => -1,
                    d => d.parse::<i32>().ok().filter(|d| (1..=31).contains(d)).ok_or("The day of the month is 1–31, or “last”.")?,
                };
                rule(format!("FREQ=MONTHLY;BYMONTHDAY={day};BYHOUR={h};BYMINUTE={m}"))
            }
            Repeat::Interval => {
                let n: u32 = self.every.trim().parse().ok().filter(|n| *n > 0).ok_or("The interval is a whole number above 0.")?;
                let minutes = if self.unit_hours { n * 60 } else { n };
                if minutes < 5 {
                    return Err("Runs need at least 5 minutes between them.".into());
                }
                Ok(Some(Trigger::Interval { minutes }))
            }
            Repeat::Cron => Ok(Some(Trigger::Cron { expr: self.cron.trim().to_owned(), tz })),
            Repeat::Rule => Ok(Some(Trigger::Rule { rule: self.rule.trim().to_owned(), tz })),
        }
    }

    /// The trigger checked by the scheduler, with its next `count` times
    /// (Unix ms) after `now`, or what is wrong.
    pub fn preview(&self, now: u64, count: usize) -> Result<Option<(Trigger, Vec<u64>, String)>, String> {
        let Some(trigger) = self.trigger()? else { return Ok(None) };
        let schedule = Schedule::of(&trigger)?.ok_or("Not a schedule.")?;
        let times = schedule.upcoming(now, count);
        if times.is_empty() {
            return Err("This schedule never runs.".into());
        }
        Ok(Some((trigger, times, schedule.describe())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_choices_make_rules_and_read_back() {
        let mut b = Builder { repeat: Repeat::Weekly, days: 0b101, time: "7:30".into(), ..Builder::default() };
        let trigger = b.trigger().unwrap().unwrap();
        assert_eq!(trigger, Trigger::Rule { rule: "FREQ=WEEKLY;BYDAY=MO,WE;BYHOUR=7;BYMINUTE=30".into(), tz: String::new() });
        let back = Builder::of(Some(&trigger));
        assert_eq!((back.repeat, back.days, back.time.as_str()), (Repeat::Weekly, 0b101, "07:30"));
        b.repeat = Repeat::Weekdays;
        let back = Builder::of(b.trigger().unwrap().as_ref());
        assert_eq!(back.repeat, Repeat::Weekdays);
        b.repeat = Repeat::Monthly;
        b.month_day = "last".into();
        let back = Builder::of(b.trigger().unwrap().as_ref());
        assert_eq!((back.repeat, back.month_day.as_str()), (Repeat::Monthly, "last"));
        b.repeat = Repeat::Hourly;
        b.minute = "15".into();
        let back = Builder::of(b.trigger().unwrap().as_ref());
        assert_eq!((back.repeat, back.minute.as_str()), (Repeat::Hourly, "15"));
        let every = Builder::of(Some(&Trigger::Interval { minutes: 120 }));
        assert_eq!((every.repeat, every.every.as_str(), every.unit_hours), (Repeat::Interval, "2", true));
    }

    #[test]
    fn rules_it_cannot_show_stay_as_text() {
        let rule = Trigger::Rule { rule: "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO;BYHOUR=9".into(), tz: "Asia/Tokyo".into() };
        let b = Builder::of(Some(&rule));
        assert_eq!(b.repeat, Repeat::Rule);
        assert_eq!(b.trigger().unwrap(), Some(rule));
    }

    #[test]
    fn mistakes_are_explained() {
        let b = Builder { repeat: Repeat::Daily, time: "25:00".into(), ..Builder::default() };
        assert!(b.trigger().unwrap_err().contains("HH:MM"));
        let b = Builder { repeat: Repeat::Weekly, days: 0, ..Builder::default() };
        assert!(b.trigger().unwrap_err().contains("day"));
        let b = Builder { repeat: Repeat::Interval, every: "2".into(), ..Builder::default() };
        assert!(b.trigger().unwrap_err().contains("5 minutes"));
        let b = Builder { repeat: Repeat::Cron, cron: "* * *".into(), ..Builder::default() };
        assert!(b.preview(0, 5).unwrap_err().contains("five fields"));
        let b = Builder { repeat: Repeat::Daily, tz: "Nowhere/Land".into(), ..Builder::default() };
        assert!(b.preview(0, 5).unwrap_err().contains("time zone"));
    }

    #[test]
    fn the_preview_lists_the_next_runs() {
        let b = Builder { tz: "UTC".into(), ..Builder::default() };
        // Friday 2026-11-27 12:00 UTC.
        let (_, times, text) = b.preview(1_795_780_800_000, 5).unwrap().unwrap();
        assert_eq!(times.len(), 5);
        assert_eq!(text, "weekdays 9:00");
        assert!(times.windows(2).all(|w| w[1] > w[0]));
        assert_eq!((times[1] - times[0]) / 3_600_000, 24, "Monday to Tuesday");
    }
}
