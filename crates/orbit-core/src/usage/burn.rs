//! Window history, burn rate and projection.
//!
//! Every reading of a window (from a session's report or the provider's
//! usage endpoint) is a [`Sample`]. The burn rate is the slope of the
//! window's use over the recent past: the latest run of samples within
//! [`HORIZON_MS`] that belong to the same window period (same reset time,
//! use never falling), spanning at least [`MIN_SPAN_MS`]. Projection divides
//! what is left by that rate; when the result lies past the reset, the
//! window lasts.
//!
//! Readings are coarse (the usage endpoint reports whole percents), so the
//! rate is only claimed over a span long enough to see movement, and a quiet
//! hour honestly reads as zero.

use std::collections::VecDeque;

use orbit_proto::*;

/// How far back the burn rate looks.
pub const HORIZON_MS: u64 = 60 * 60_000;
/// The shortest span a rate is claimed over.
pub const MIN_SPAN_MS: u64 = 10 * 60_000;
/// Samples kept per window.
pub const KEEP: usize = 240;
/// Samples older than this are dropped.
pub const KEEP_MS: u64 = 8 * 24 * 3_600_000;
/// Two readings within this of each other are one moment: the newer one
/// replaces the older (sessions report with every turn).
const MERGE_MS: u64 = 60_000;
/// Reset times that differ by less than this are the same period (sources
/// round them differently).
const SAME_PERIOD_MS: u64 = 5 * 60_000;

/// One reading of a window.
#[derive(Clone, Copy, Debug, PartialEq, wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Sample {
    pub at_ms: u64,
    /// Share used, 0.0–1.0.
    pub used: f32,
    /// When the window resets (Unix ms); 0 when unknown.
    pub resets_ms: u64,
}

fn same_period(a: &Sample, b: &Sample) -> bool {
    a.resets_ms == 0 || b.resets_ms == 0 || a.resets_ms.abs_diff(b.resets_ms) < SAME_PERIOD_MS
}

/// Adds a reading, keeping the history short: near-simultaneous readings
/// merge, a plateau keeps only its ends, and old samples go.
pub fn record(samples: &mut VecDeque<Sample>, sample: Sample) {
    if let Some(last) = samples.back()
        && sample.at_ms < last.at_ms
    {
        return; // out of order: an older reading arriving late
    }
    if let Some(last) = samples.back_mut()
        && sample.at_ms - last.at_ms < MERGE_MS
        && same_period(last, &sample)
    {
        // Keep the earlier time so a rate measured from it stays honest,
        // unless the reading moved.
        if (sample.used - last.used).abs() > f32::EPSILON {
            *last = sample;
        }
        return;
    }
    let len = samples.len();
    if len >= 2 {
        let (a, b) = (samples[len - 2], samples[len - 1]);
        let flat = |x: &Sample, y: &Sample| (x.used - y.used).abs() <= f32::EPSILON && same_period(x, y);
        if flat(&a, &b) && flat(&b, &sample) {
            // The middle of a plateau says nothing the ends don't.
            samples[len - 1] = sample;
            return;
        }
    }
    samples.push_back(sample);
    while samples.len() > KEEP || samples.front().is_some_and(|s| sample.at_ms - s.at_ms > KEEP_MS) {
        samples.pop_front();
    }
}

/// Share of the window used per hour at the recent pace; `None` without
/// enough recent history.
pub fn burn_per_hour(samples: &VecDeque<Sample>, now_ms: u64) -> Option<f32> {
    let last = *samples.back()?;
    if now_ms.saturating_sub(last.at_ms) > HORIZON_MS {
        return None;
    }
    let mut first = last;
    for s in samples.iter().rev().skip(1) {
        // Going back in time, use only falls; a higher earlier value means
        // the window reset in between.
        if now_ms.saturating_sub(s.at_ms) > HORIZON_MS || !same_period(s, &last) || s.used > first.used + f32::EPSILON {
            break;
        }
        first = *s;
    }
    let span = last.at_ms - first.at_ms;
    if span < MIN_SPAN_MS {
        return None;
    }
    Some((last.used - first.used).max(0.0) / (span as f32 / 3_600_000.0))
}

/// When a window at `used` runs out burning `per_hour`: `None` when it
/// lasts until `resets_ms` (or nothing is being used).
pub fn runs_out(used: f32, per_hour: f32, now_ms: u64, resets_ms: u64) -> Option<u64> {
    if per_hour <= 1e-4 {
        return None;
    }
    let hours = (1.0 - used).max(0.0) / per_hour;
    let at = now_ms + (hours as f64 * 3_600_000.0) as u64;
    (resets_ms == 0 || at < resets_ms).then_some(at)
}

/// A window as clients show it, with its rate and projection.
pub fn window_state(window: &QuotaWindow, samples: Option<&VecDeque<Sample>>, now_ms: u64) -> QuotaWindowState {
    let (label, span_ms) = describe_window(&window.window);
    let reset = window.resets_ms > 0 && window.resets_ms <= now_ms;
    // Past its reset the reading is stale: the window starts empty.
    let window = if reset {
        QuotaWindow { utilization: Some(0.0), rejected: false, resets_ms: 0, ..window.clone() }
    } else {
        window.clone()
    };
    let burn = if reset { None } else { samples.and_then(|s| burn_per_hour(s, now_ms)) };
    let used = if window.rejected { 1.0 } else { window.utilization.unwrap_or(0.0) };
    let runs_out_ms = burn.and_then(|rate| runs_out(used, rate, now_ms, window.resets_ms));
    QuotaWindowState { window, label, span_ms, burn_per_hour: burn, runs_out_ms }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    fn s(at_min: u64, used: f32) -> Sample {
        Sample { at_ms: at_min * MIN, used, resets_ms: 1_000 * MIN }
    }

    fn history(points: &[(u64, f32)]) -> VecDeque<Sample> {
        let mut h = VecDeque::new();
        for (at, used) in points {
            record(&mut h, s(*at, *used));
        }
        h
    }

    #[test]
    fn the_rate_is_the_slope_of_the_last_hour() {
        // 10 % over 30 minutes is 20 % an hour.
        let h = history(&[(100, 0.30), (110, 0.33), (120, 0.36), (130, 0.40)]);
        let rate = burn_per_hour(&h, 131 * MIN).unwrap();
        assert!((rate - 0.20).abs() < 1e-4, "{rate}");
        // At 40 % used, 60 % left at 20 %/h is three hours: minute 311.
        let at = runs_out(0.40, rate, 131 * MIN, 1_000 * MIN).unwrap();
        assert!((at as i64 - 311 * MIN as i64).abs() < MIN as i64, "{}", at / MIN);
        // Older history beyond the horizon is ignored.
        let h = history(&[(0, 0.0), (100, 0.30), (130, 0.40)]);
        let rate = burn_per_hour(&h, 131 * MIN).unwrap();
        assert!((rate - 0.20).abs() < 1e-4, "{rate}");
    }

    #[test]
    fn too_little_history_claims_nothing() {
        assert_eq!(burn_per_hour(&VecDeque::new(), 0), None);
        assert_eq!(burn_per_hour(&history(&[(100, 0.3)]), 100 * MIN), None);
        assert_eq!(burn_per_hour(&history(&[(100, 0.3), (105, 0.35)]), 106 * MIN), None, "five minutes is too short");
        // A last reading from two hours ago is too old to describe now.
        assert_eq!(burn_per_hour(&history(&[(0, 0.1), (30, 0.2)]), 150 * MIN), None);
    }

    #[test]
    fn a_reset_starts_a_new_run() {
        // Used went 0.9 → 0.95, then the window reset to 0.02 and climbed.
        let mut h = history(&[(100, 0.90), (115, 0.95)]);
        for (at, used) in [(120, 0.02), (135, 0.07)] {
            record(&mut h, Sample { at_ms: at * MIN, used, resets_ms: 1_300 * MIN });
        }
        let rate = burn_per_hour(&h, 136 * MIN).unwrap();
        assert!((rate - 0.20).abs() < 1e-4, "only the new period counts: {rate}");
    }

    #[test]
    fn a_quiet_hour_lasts_until_the_reset() {
        let h = history(&[(100, 0.5), (115, 0.5), (130, 0.5)]);
        assert_eq!(burn_per_hour(&h, 131 * MIN), Some(0.0));
        assert_eq!(runs_out(0.5, 0.0, 131 * MIN, 1_000 * MIN), None);
        // A pace that would only run out after the reset lasts too.
        assert_eq!(runs_out(0.5, 0.01, 0, 10 * MIN), None);
        // Already full: now.
        assert_eq!(runs_out(1.0, 0.2, 7, 0), Some(7));
    }

    #[test]
    fn history_stays_small() {
        let mut h = VecDeque::new();
        // Readings every 10 s merge into one sample a minute, and a plateau
        // keeps only its two ends…
        for i in 0..60 {
            record(&mut h, Sample { at_ms: i * 10_000, used: 0.1, resets_ms: 0 });
        }
        assert_eq!(h.len(), 2);
        assert_eq!(h.front().unwrap().at_ms, 0);
        for m in 1..30 {
            record(&mut h, s(m * 2, 0.1));
        }
        assert_eq!(h.len(), 2);
        assert_eq!(h.back().unwrap().at_ms, 58 * MIN);
        // …and the count is capped.
        for m in 0..1_000u64 {
            record(&mut h, s(100 + m * 2, m as f32 / 1_000.0));
        }
        assert!(h.len() <= KEEP);
        // A late, older reading is ignored.
        let last = *h.back().unwrap();
        record(&mut h, s(1, 0.9));
        assert_eq!(*h.back().unwrap(), last);
    }

    #[test]
    fn a_window_past_its_reset_reads_empty() {
        let w = QuotaWindow { window: "five_hour".into(), utilization: Some(0.97), resets_ms: 50 * MIN, rejected: true };
        let state = window_state(&w, None, 60 * MIN);
        assert_eq!(state.label, "5-hour");
        assert_eq!(state.used(), 0.0);
        assert!(!state.window.rejected);
        let live = window_state(&w, Some(&history(&[(20, 0.8), (40, 0.97)])), 41 * MIN);
        assert_eq!(live.used(), 1.0);
        assert!(live.burn_per_hour.is_some());
        assert_eq!(live.runs_out_ms, Some(41 * MIN), "a refused window has run out");
    }
}
