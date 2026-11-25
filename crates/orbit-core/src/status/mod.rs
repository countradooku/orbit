//! The honest status engine: the evidence behind every status Orbit shows.
//!
//! `docs/STATUS.md` describes the state machine. This module holds the
//! parts that do not need the orchestrator, all with time passed in so
//! they can be tested without waiting:
//!
//! * [`TurnWatch`]: when a thread's session last said anything during a
//!   turn, and whether it has gone quiet.
//! * [`ClockWatch`]: notices that the machine slept or the clock jumped.
//! * [`term`]: what runs in each terminal (hooks, OSC 7501, heuristics).
//! * [`osc`]: the zero-copy OSC scanner for PTY output.
//! * [`hook`]: the hook endpoint, the `orbit-hook` invoker and injection.
//!
//! The engine side is `engine/status.rs`.

pub mod hook;
pub mod osc;
pub mod term;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use orbit_proto::{Liveness, ProviderKind, TermId, ThreadId, ThreadLive};

/// How long a provider may stay silent during a turn before Orbit says so.
/// Codex streams command output, so its silences mean more; ACP agents
/// often think before their first token.
pub fn silence_threshold(kind: ProviderKind) -> Duration {
    match kind {
        ProviderKind::Codex => Duration::from_secs(90),
        ProviderKind::Acp => Duration::from_secs(180),
        ProviderKind::Claude | ProviderKind::OpenCode | ProviderKind::Pi => Duration::from_secs(120),
    }
}

/// A gap between ticks this long means the process was not running: the
/// machine slept, or the engine was frozen.
pub const STALL: Duration = Duration::from_secs(30);
/// Wall time and monotonic time disagreeing by this much means the clock
/// jumped or the monotonic clock paused during sleep (Linux, macOS).
pub const CLOCK_SKEW: Duration = Duration::from_secs(10);

/// Freshness of one thread's session during a turn.
#[derive(Clone, Debug)]
pub struct TurnWatch {
    pub last_signal: Instant,
    pub last_signal_ms: u64,
    pub liveness: Liveness,
    /// Tool calls started and not finished: id → title.
    pub open_tools: Vec<(String, String)>,
}

impl TurnWatch {
    pub fn new(now: Instant, now_ms: u64) -> Self {
        Self { last_signal: now, last_signal_ms: now_ms, liveness: Liveness::Live, open_tools: Vec::new() }
    }

    /// The agent said something. Returns whether the liveness changed.
    pub fn signal(&mut self, now: Instant, now_ms: u64) -> bool {
        self.last_signal = now;
        self.last_signal_ms = now_ms;
        let changed = self.liveness != Liveness::Live;
        self.liveness = Liveness::Live;
        changed
    }

    /// Ages the evidence. `waiting` turns (blocked on the user) are quiet
    /// by design and never age. Returns whether the liveness changed.
    pub fn tick(&mut self, now: Instant, threshold: Duration, waiting: bool) -> bool {
        if waiting || self.liveness != Liveness::Live {
            return false;
        }
        if now.saturating_duration_since(self.last_signal) < threshold {
            return false;
        }
        let tool = self.open_tools.last().map(|(_, title)| title.clone()).filter(|t| !t.is_empty());
        self.liveness = Liveness::Quiet { since_ms: self.last_signal_ms, tool };
        true
    }

    /// Orbit lost touch: whatever it last heard no longer counts.
    pub fn distrust(&mut self, now_ms: u64, reason: String) -> bool {
        if matches!(self.liveness, Liveness::Unverified { .. }) {
            return false;
        }
        self.liveness = Liveness::Unverified { since_ms: now_ms, reason };
        true
    }
}

/// Detects sleep and clock jumps from consecutive ticks.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClockWatch {
    last: Option<(Instant, u64)>,
}

/// What a tick saw happen to time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockJump {
    /// Orbit did not run for `missed` (sleep, or a frozen process).
    Slept { missed: Duration },
    /// The wall clock moved by `by_ms` more than elapsed time (either way).
    Jumped { by_ms: i64 },
}

impl ClockWatch {
    pub fn observe(&mut self, now: Instant, now_ms: u64) -> Option<ClockJump> {
        let previous = self.last.replace((now, now_ms));
        let (then, then_ms) = previous?;
        let mono = now.saturating_duration_since(then);
        let wall = now_ms as i64 - then_ms as i64;
        let skew = wall - mono.as_millis() as i64;
        if mono >= STALL {
            return Some(ClockJump::Slept { missed: mono });
        }
        if skew.unsigned_abs() >= CLOCK_SKEW.as_millis() as u64 {
            // Monotonic clocks pause during sleep on Linux and macOS, so a
            // forward skew is how sleep looks there.
            return Some(if skew > 0 { ClockJump::Slept { missed: Duration::from_millis(skew as u64) } } else { ClockJump::Jumped { by_ms: skew } });
        }
        None
    }
}

/// The status engine's state, owned by the orchestrator.
#[derive(Default)]
pub struct StatusState {
    /// Threads with a turn being watched.
    pub turns: HashMap<ThreadId, TurnWatch>,
    pub terminals: HashMap<TermId, term::TermWatch>,
    /// What was last broadcast per thread, so only changes go out.
    pub published: HashMap<ThreadId, ThreadLive>,
    pub clock: ClockWatch,
    /// The hook endpoint, once started (on the first terminal).
    pub endpoint: Option<String>,
    pub endpoint_failed: bool,
    /// Sessions seen with a closed input channel on the last tick.
    pub lost: HashSet<ThreadId>,
}

/// A random token for a terminal.
pub fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_turn_turns_quiet_with_its_open_tool_and_recovers() {
        let t0 = Instant::now();
        let mut w = TurnWatch::new(t0, 5_000);
        w.open_tools.push(("c1".into(), "cargo test".into()));
        let threshold = silence_threshold(ProviderKind::Claude);
        assert!(!w.tick(t0 + threshold - Duration::from_secs(1), threshold, false));
        assert!(!w.tick(t0 + threshold * 3, threshold, true), "waiting on the user is not silence");
        assert!(w.tick(t0 + threshold, threshold, false));
        assert_eq!(w.liveness, Liveness::Quiet { since_ms: 5_000, tool: Some("cargo test".into()) });
        assert!(!w.tick(t0 + threshold * 2, threshold, false), "reported once");
        assert!(w.signal(t0 + threshold * 2, 9_000));
        assert_eq!(w.liveness, Liveness::Live);
    }

    #[test]
    fn sleep_and_clock_jumps_are_noticed() {
        let t0 = Instant::now();
        let mut c = ClockWatch::default();
        assert_eq!(c.observe(t0, 1_000_000), None);
        assert_eq!(c.observe(t0 + Duration::from_millis(500), 1_000_500), None);
        // Monotonic time kept running (Windows): a long gap between ticks.
        let jump = c.observe(t0 + Duration::from_secs(3600), 4_600_000);
        assert!(matches!(jump, Some(ClockJump::Slept { missed }) if missed >= Duration::from_secs(3599)));
        // Monotonic time paused (Linux, macOS): wall time ran ahead.
        let t = t0 + Duration::from_secs(3601);
        assert!(matches!(c.observe(t, 4_601_000 + 7_200_000), Some(ClockJump::Slept { .. })));
        // The wall clock was set back.
        let t = t + Duration::from_millis(500);
        assert!(matches!(c.observe(t, 4_601_000), Some(ClockJump::Jumped { by_ms }) if by_ms < 0));
        assert_eq!(c.observe(t + Duration::from_millis(500), 4_601_500), None);
    }
}
