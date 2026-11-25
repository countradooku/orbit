//! When runs are due: a min-heap of due times fed by the engine's tick, the
//! missed-run rules, and jitter.
//!
//! The heap holds one entry per armed schedule trigger. The tick only peeks
//! at its top, so it costs one comparison when nothing is due and nothing
//! at all when there are no automations. Entries are never removed in
//! place: an entry whose automation was paused, edited or deleted is
//! dropped when it comes up, by checking it against the persisted state.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use orbit_proto::AutomationId;

use super::schedule::Schedule;

/// The most a run is moved after its nominal time, so automations set for
/// the same minute do not all start at once.
pub const MAX_JITTER_MS: u64 = 45_000;
/// A run this late or less is on time: the tick and jitter account for it.
pub const ON_TIME_MS: u64 = MAX_JITTER_MS + 30_000;
/// Missed occurrences counted before giving up counting.
const MAX_COUNT: u32 = 100_000;

/// An armed trigger: (fire at, automation, trigger index, nominal due).
type Entry = Reverse<(u64, u64, u32, u64)>;

#[derive(Default)]
pub struct Scheduler {
    heap: BinaryHeap<Entry>,
}

impl Scheduler {
    /// Arms trigger `index` of `id` for its nominal time `due_ms`; it fires
    /// at `due_ms` plus a jitter that stays the same across restarts.
    pub fn arm(&mut self, id: AutomationId, index: usize, due_ms: u64, min_gap_ms: u64) {
        let fire = due_ms + jitter_ms(id, due_ms, min_gap_ms);
        self.heap.push(Reverse((fire, id.0, index as u32, due_ms)));
    }

    /// When the next entry fires, if any.
    pub fn next_fire(&self) -> Option<u64> {
        self.heap.peek().map(|Reverse((fire, ..))| *fire)
    }

    /// The next entry that fires at or before `now_ms`: (automation,
    /// trigger index, nominal due time). The caller checks it is still
    /// current.
    pub fn pop_due(&mut self, now_ms: u64) -> Option<(AutomationId, usize, u64)> {
        if self.next_fire()? > now_ms {
            return None;
        }
        let Reverse((_, id, index, due)) = self.heap.pop()?;
        Some((AutomationId(id), index as usize, due))
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
}

/// A deterministic delay in `[0, MAX_JITTER_MS]`, and under a tenth of the
/// schedule's gap, from the automation and the occurrence.
pub fn jitter_ms(id: AutomationId, due_ms: u64, min_gap_ms: u64) -> u64 {
    let cap = MAX_JITTER_MS.min(min_gap_ms / 10);
    if cap == 0 {
        return 0;
    }
    // SplitMix64 of the pair: cheap, well spread, stable.
    let mut x = id.0 ^ due_ms.rotate_left(29) ^ 0x9e37_79b9_7f4a_7c15;
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    x % (cap + 1)
}

/// What to do about a trigger that came due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Start a run for this occurrence (`late`: past its time, but within
    /// the grace window).
    pub fire: Option<(u64, bool)>,
    /// Occurrences that were missed by more than the grace window.
    pub missed: u32,
    /// The trigger's next nominal time.
    pub next_ms: u64,
}

/// Decides about a trigger due at `due_ms`, seen at `now_ms`. Of all the
/// occurrences up to now only the newest may run, and only within
/// `grace_ms`: a computer that slept through eight hourly runs runs once
/// (or not at all), never eight times.
pub fn plan(schedule: &Schedule, due_ms: u64, now_ms: u64, grace_ms: u64) -> Plan {
    if due_ms > now_ms {
        return Plan { fire: None, missed: 0, next_ms: due_ms };
    }
    let (newest, count) = match schedule {
        Schedule::Interval { every_ms } => {
            let n = (now_ms - due_ms) / every_ms;
            (due_ms + n * every_ms, (n + 1).min(MAX_COUNT as u64) as u32)
        }
        Schedule::Calendar { .. } => {
            let (mut newest, mut count) = (due_ms, 1u32);
            while count < MAX_COUNT {
                match schedule.next_after(newest) {
                    Some(next) if next <= now_ms => {
                        newest = next;
                        count += 1;
                    }
                    _ => break,
                }
            }
            (newest, count)
        }
    };
    let lateness = now_ms - newest;
    let on_time = lateness <= grace_ms.max(ON_TIME_MS);
    let fire = on_time.then_some((newest, lateness > ON_TIME_MS));
    let missed = if on_time { count - 1 } else { count };
    let next_ms = match (schedule, fire) {
        // Intervals count from the run.
        (Schedule::Interval { every_ms }, Some(_)) => now_ms + every_ms,
        _ => schedule.next_after(now_ms).unwrap_or(0),
    };
    Plan { fire, missed, next_ms }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbit_proto::Trigger;

    const HOUR: u64 = 3_600_000;
    const MIN: u64 = 60_000;

    fn hourly() -> Schedule {
        Schedule::of(&Trigger::Cron { expr: "0 * * * *".into(), tz: "UTC".into() }).unwrap().unwrap()
    }

    /// 2026-11-25 00:00 UTC.
    const T0: u64 = 1_795_564_800_000;

    #[test]
    fn on_time_runs_fire_and_schedule_the_next() {
        let p = plan(&hourly(), T0, T0 + 20_000, 30 * MIN);
        assert_eq!(p, Plan { fire: Some((T0, false)), missed: 0, next_ms: T0 + HOUR });
    }

    #[test]
    fn a_sleep_within_grace_runs_once_late() {
        // Asleep from 23:50 to 02:10: 00:00, 01:00 and 02:00 were missed;
        // 02:00 is 10 minutes late, inside a 30-minute grace.
        let p = plan(&hourly(), T0, T0 + 2 * HOUR + 10 * MIN, 30 * MIN);
        assert_eq!(p.fire, Some((T0 + 2 * HOUR, true)));
        assert_eq!(p.missed, 2);
        assert_eq!(p.next_ms, T0 + 3 * HOUR);
    }

    #[test]
    fn a_sleep_past_grace_records_missed_runs_only() {
        let p = plan(&hourly(), T0, T0 + 2 * HOUR + 45 * MIN, 30 * MIN);
        assert_eq!(p.fire, None);
        assert_eq!(p.missed, 3);
        assert_eq!(p.next_ms, T0 + 3 * HOUR);
        // No grace at all still tolerates the tick and jitter.
        let p = plan(&hourly(), T0, T0 + MAX_JITTER_MS + 500, 0);
        assert_eq!(p.fire, Some((T0, false)));
    }

    #[test]
    fn intervals_count_from_the_run() {
        let every = Schedule::of(&Trigger::Interval { minutes: 10 }).unwrap().unwrap();
        let p = plan(&every, T0, T0 + 35 * MIN, 0);
        assert_eq!((p.fire, p.missed), (None, 4));
        let p = plan(&every, T0, T0 + 31 * MIN, 5 * MIN);
        assert_eq!(p.fire, Some((T0 + 30 * MIN, false)));
        assert_eq!(p.next_ms, T0 + 41 * MIN);
    }

    #[test]
    fn jitter_is_bounded_stable_and_spread() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..2000u64 {
            let id = AutomationId(i * 7919);
            let j = jitter_ms(id, T0, HOUR);
            assert!(j <= MAX_JITTER_MS);
            assert_eq!(j, jitter_ms(id, T0, HOUR), "the same after a restart");
            assert!(jitter_ms(id, T0, 2 * MIN) <= 12_000, "under a tenth of a short gap");
            seen.insert(j / 1000);
        }
        assert!(seen.len() > 40, "spread over the window: {}", seen.len());
        assert_eq!(jitter_ms(AutomationId(5), T0, 0), 0);
    }

    #[test]
    fn the_heap_pops_in_order_and_only_when_due() {
        let mut s = Scheduler::default();
        assert!(s.pop_due(u64::MAX).is_none());
        s.arm(AutomationId(2), 0, T0 + HOUR, HOUR);
        s.arm(AutomationId(1), 1, T0, HOUR);
        assert!(s.pop_due(T0 - 1).is_none());
        let (id, index, due) = s.pop_due(T0 + MAX_JITTER_MS).unwrap();
        assert_eq!((id, index, due), (AutomationId(1), 1, T0));
        assert!(s.pop_due(T0 + MAX_JITTER_MS).is_none());
        assert_eq!(s.len(), 1);
    }
}
