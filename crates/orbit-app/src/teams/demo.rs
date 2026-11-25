//! Debug builds: shows teams mid-flight for visual checks
//! (`ORBIT_DEBUG_DEMO=teams`, with the data of `examples/seed_teams.rs`).
//!
//! The engine never runs an agent here: after a restart nothing works, so
//! the seeded workers' and candidates' threads are only drawn as working,
//! in the client's copy of their records.

use std::collections::HashMap;

use orbit_proto::*;

use crate::app_state::AppState;

/// Marks the threads of working workers and unfinished candidates as
/// running, and the architect of a team with mail on the way as waiting.
pub fn apply(app: &gpui_kit::Entity<AppState>, cx: &mut gpui_kit::App) {
    let state = app.read(cx).state();
    let mut working: Vec<ThreadId> = state
        .teams
        .values()
        .flat_map(|t| t.workers.iter())
        .filter(|w| w.state == WorkerState::Working)
        .filter_map(|w| w.thread)
        .collect();
    working.extend(
        state
            .races
            .values()
            .filter(|r| r.state == RaceState::Running)
            .flat_map(|r| r.candidates.iter())
            .filter(|c| c.finished.is_none())
            .map(|c| c.thread),
    );
    app.update(cx, |app, cx| {
        app.debug_edit_threads(
            |threads: &mut HashMap<ThreadId, Thread>| {
                for id in &working {
                    if let Some(t) = threads.get_mut(id) {
                        t.status = ThreadStatus::Running;
                    }
                }
            },
            cx,
        )
    });
}
