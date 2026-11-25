//! What restarting Orbit means for the agents, and getting them ready.
//!
//! Installing an update quits the app. Whether that stops running agents
//! depends on where the engine lives: in this process it stops with it; in
//! a host (`orbitd`, local or remote) the agents keep going and the new app
//! reattaches. [`RestartHost`] is that seam. The default asks nothing of the
//! engine and reports how many agents would stop; a persistent host
//! replaces it with [`set_restart_host`] to drain or hand over first.

use std::rc::Rc;

use gpui_kit::*;

use crate::app_state::AppState;
use crate::ui::Attention;

pub trait RestartHost {
    /// Agents that would stop if the app quit now (0 when a host keeps
    /// them running across the restart).
    fn agents_at_risk(&self, cx: &App) -> usize;

    /// Gets the host ready for the app to quit and come back: drain, save,
    /// detach. The update is installed only when this succeeds.
    fn prepare(&self, cx: &mut App) -> Task<Result<(), String>>;
}

struct HostGlobal(Rc<dyn RestartHost>);

impl Global for HostGlobal {}

/// Replaces the restart host (the persistent host daemon does).
pub fn set_restart_host(host: Rc<dyn RestartHost>, cx: &mut App) {
    cx.set_global(HostGlobal(host));
}

/// Installs the default host for this connection.
pub fn init(app: Entity<AppState>, cx: &mut App) {
    if !cx.has_global::<HostGlobal>() {
        set_restart_host(Rc::new(EngineHost { app }), cx);
    }
}

pub fn agents_at_risk(cx: &App) -> usize {
    cx.try_global::<HostGlobal>().map_or(0, |h| h.0.agents_at_risk(cx))
}

pub fn prepare(cx: &mut App) -> Task<Result<(), String>> {
    match cx.try_global::<HostGlobal>().map(|h| h.0.clone()) {
        Some(host) => host.prepare(cx),
        None => Task::ready(Ok(())),
    }
}

/// The engine this app talks to today: in this process, or a remote host.
struct EngineHost {
    app: Entity<AppState>,
}

impl RestartHost for EngineHost {
    fn agents_at_risk(&self, cx: &App) -> usize {
        // A remote host outlives this app; only an in-process engine stops.
        if crate::settings::connected_host(cx).0.is_some() {
            return 0;
        }
        let state = self.app.read(cx).state();
        state.threads.values().filter(|t| !t.archived && Attention::of(t) == Attention::Working).count()
    }

    fn prepare(&self, cx: &mut App) -> Task<Result<(), String>> {
        // Unsent settings edits go now; the engine itself shuts down in an
        // orderly way as the app quits (`on_app_quit`).
        crate::settings::flush(cx);
        Task::ready(Ok(()))
    }
}
