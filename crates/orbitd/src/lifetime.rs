//! When the background host exits.
//!
//! * A **restart** request exits now, or once nothing is working (the app
//!   that asked starts the new host when it reconnects).
//! * With **no window attached** and no hold, the preference in `host.json`
//!   decides: `always` keeps running; `while_working` exits once nothing is
//!   working (no turn, no terminal running a command, no automation due in
//!   the next ten minutes) for [`Grace::idle`]; `never` exits after
//!   [`Grace::detached`], long enough for the app to restart.
//!
//! While a window is attached, or this computer is shared with paired
//! devices (they may connect at any time), the loop sleeps on the control's
//! notifier: no timer runs. Without one it looks once a second.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orbit_core::EngineHandle;
use orbit_net::host::{HostControl, read_policy};
use orbit_proto::{Command, HostActivity, HostCommand, HostReply, KeepRunning, Reply, RestartWhen, now_ms};

/// Why the host stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    Restart,
    /// Nothing to do and no window, past the grace period.
    Idle,
    Signal,
    EngineGone,
}

#[derive(Clone, Copy, Debug)]
pub struct Grace {
    /// `while_working`: how long the host stays with nothing to do.
    pub idle: Duration,
    /// `never`: how long it waits for a window to come back.
    pub detached: Duration,
}

impl Grace {
    /// The defaults, or `fixed` for both (tests, `--grace-ms`).
    pub fn with_override(fixed: Option<Duration>) -> Self {
        match fixed {
            Some(d) => Self { idle: d, detached: d },
            None => Self { idle: Duration::from_secs(60), detached: Duration::from_secs(5) },
        }
    }
}

const POLL: Duration = Duration::from_secs(1);

pub async fn run(engine: EngineHandle, control: Arc<HostControl>, data_dir: PathBuf, grace: Grace) -> Exit {
    let mut idle_since: Option<Instant> = None;
    let poll = POLL.min(grace.idle.max(Duration::from_millis(50)));
    loop {
        let restart = control.restart();
        if restart == Some(RestartWhen::Now) {
            return Exit::Restart;
        }
        let attached = control.windows() > 0 || control.sharing();
        if restart.is_none() && attached {
            idle_since = None;
            control.changed().await;
            continue;
        }
        // A hold keeps the host as a window would, and runs out by itself.
        let windowless = !attached && !control.held(now_ms());
        let Some(activity) = activity(&engine).await else { return Exit::EngineGone };
        let live = activity.is_live(now_ms());
        if restart == Some(RestartWhen::WhenIdle) && !live {
            return Exit::Restart;
        }
        if windowless {
            let (leave, after) = match read_policy(&data_dir) {
                KeepRunning::Always => (false, grace.idle),
                KeepRunning::WhileWorking => (!live, grace.idle),
                KeepRunning::Never => (true, grace.detached),
            };
            if leave {
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= after {
                    return Exit::Idle;
                }
            } else {
                idle_since = None;
            }
        } else {
            idle_since = None;
        }
        tokio::select! {
            _ = control.changed() => {}
            _ = tokio::time::sleep(poll) => {}
        }
    }
}

async fn activity(engine: &EngineHandle) -> Option<HostActivity> {
    match engine.call(Command::Host(HostCommand::Activity)).await {
        Ok(Ok(Reply::Host(HostReply::Activity(activity)))) => Some(activity),
        Ok(_) => Some(HostActivity::default()),
        Err(_) => None,
    }
}
