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

#[cfg(test)]
mod tests {
    use super::*;

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
