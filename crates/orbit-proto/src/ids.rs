//! Compact, time-ordered identifiers.
//!
//! An id is a `u64`: the top 44 bits are milliseconds since 2024-01-01 (good
//! until the year 2581) and the low 20 bits are a per-process counter. Ids
//! therefore sort by creation time, fit in a register, hash in one round and
//! cost 8 bytes on the wire instead of a 36-byte UUID string.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const EPOCH_MS: u64 = 1_704_067_200_000; // 2024-01-01T00:00:00Z
const COUNTER_BITS: u32 = 20;

static LAST: AtomicU64 = AtomicU64::new(0);

/// Returns a new id that is strictly greater than every id this process
/// returned before, even if the wall clock goes backwards.
pub fn next_raw_id() -> u64 {
    let now = now_ms().saturating_sub(EPOCH_MS) << COUNTER_BITS;
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let candidate = if now > last { now } else { last + 1 };
        match LAST.compare_exchange_weak(last, candidate, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return candidate,
            Err(actual) => last = actual,
        }
    }
}

/// Makes sure ids created after a restart sort after ids loaded from disk.
pub fn observe_raw_id(id: u64) {
    LAST.fetch_max(id, Ordering::Relaxed);
}

/// Wall clock in milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Milliseconds since the Unix epoch at which `raw` was created.
pub fn id_time_ms(raw: u64) -> u64 {
    (raw >> COUNTER_BITS) + EPOCH_MS
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, bitcode::Encode, bitcode::Decode)]
        #[derive(wincode::SchemaWrite, wincode::SchemaRead)]
        pub struct $name(pub u64);

        impl $name {
            pub fn new() -> Self {
                Self(next_raw_id())
            }
            pub fn created_ms(self) -> u64 {
                id_time_ms(self.0)
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!($prefix, "_{:x}"), self.0)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!($prefix, "_{:x}"), self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = std::num::ParseIntError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let hex = s.strip_prefix(concat!($prefix, "_")).unwrap_or(s);
                u64::from_str_radix(hex, 16).map(Self)
            }
        }
    };
}

id_type!(/// A project: one repository or folder on disk.
    ProjectId, "prj");
id_type!(/// A thread: one conversation with one agent.
    ThreadId, "thr");
id_type!(/// One entry in a thread's timeline.
    ItemId, "itm");
id_type!(/// A pending approval or question from an agent.
    RequestId, "req");
id_type!(/// An interactive terminal session.
    TermId, "trm");
id_type!(/// A scheduled or event-triggered agent run setup.
    AutomationId, "aut");
id_type!(/// One run of an automation.
    RunId, "run");
id_type!(/// An agent team: an architect thread and its workers.
    TeamId, "team");
id_type!(/// A race: one prompt run by several candidates side by side.
    RaceId, "race");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_strictly_increasing() {
        let mut prev = 0;
        for _ in 0..100_000 {
            let id = next_raw_id();
            assert!(id > prev);
            prev = id;
        }
    }

    #[test]
    fn id_round_trips_through_text() {
        let id = ThreadId::new();
        let parsed: ThreadId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
        assert!(id.created_ms() <= now_ms() + 1);
    }
}
