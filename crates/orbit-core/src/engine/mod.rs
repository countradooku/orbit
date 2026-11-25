//! The engine thread and its handle.
//!
//! The engine runs on one dedicated OS thread with a single-threaded Tokio
//! runtime: all provider I/O is asynchronous, so one thread is enough and
//! the engine costs one stack and one runtime, not a pool. Clients talk to
//! it through an [`EngineHandle`] that can be cloned freely and used from
//! any thread, including the UI thread, without blocking.

mod bus;
pub use bus::Bus;
mod orchestrator;
mod timeline;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::time::Duration;

use crossfire::oneshot::{RxOneshot, TxOneshot, oneshot};
use crossfire::{BlockingTxTrait, MTx, mpsc};
use orbit_proto::*;

pub use bus::EventRx;
pub use orchestrator::{Deferred, Internal, Launcher, Orchestrator, Outcome, prompt_uuid};

use crate::provider;
use crate::store::Store;

/// What a subscriber receives when it attaches.
pub struct Subscribed {
    /// A full snapshot when the subscriber's `since` could not be replayed.
    pub snapshot: Option<Snapshot>,
    /// Events after `since`, when it could be replayed.
    pub replay: Vec<Envelope>,
    pub events: EventRx,
}

enum Call {
    Command { command: Command, reply: TxOneshot<Result<Reply, String>> },
    Subscribe { epoch: u64, since: u64, reply: TxOneshot<Subscribed> },
    Shutdown { done: TxOneshot<()> },
}

pub struct EngineOptions {
    pub data_dir: PathBuf,
    /// Probe installed providers at startup.
    pub probe_providers: bool,
}

impl EngineOptions {
    /// The platform's per-user data directory: `%APPDATA%\Orbit` on Windows,
    /// `~/Library/Application Support/Orbit` on macOS and
    /// `$XDG_DATA_HOME/Orbit` (default `~/.local/share/Orbit`) on Linux.
    pub fn default_data_dir() -> PathBuf {
        dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Orbit")
    }
}

/// A cheap, cloneable handle on the engine thread.
#[derive(Clone)]
pub struct EngineHandle {
    calls: MTx<mpsc::List<Call>>,
}

impl EngineHandle {
    /// Opens the store and starts the engine thread.
    pub fn start(options: EngineOptions) -> std::io::Result<Self> {
        Self::start_with(options, Box::new(provider::start))
    }

    /// Starts the engine with a custom session launcher (tests, benchmarks).
    pub fn start_with(options: EngineOptions, launcher: Launcher) -> std::io::Result<Self> {
        let store = Store::open(&options.data_dir)?;
        let (calls, call_rx) = mpsc::unbounded_async::<Call>();
        std::thread::Builder::new()
            .name("orbit-engine".into())
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("engine runtime");
                runtime.block_on(run(store, call_rx, launcher, options.probe_providers));
            })?;
        Ok(Self { calls })
    }

    /// Sends a command; await the returned receiver for the reply.
    pub fn call(&self, command: Command) -> RxOneshot<Result<Reply, String>> {
        let (reply, rx) = oneshot();
        let _ = self.calls.send(Call::Command { command, reply });
        rx
    }

    /// Fire-and-forget command.
    pub fn send(&self, command: Command) {
        let (reply, _rx) = oneshot();
        let _ = self.calls.send(Call::Command { command, reply });
    }

    /// Attaches a subscriber. Pass the `epoch` and `seq` of the last event
    /// applied, or zeros for a fresh client.
    pub fn subscribe(&self, epoch: u64, since: u64) -> RxOneshot<Subscribed> {
        let (reply, rx) = oneshot();
        let _ = self.calls.send(Call::Subscribe { epoch, since, reply });
        rx
    }

    /// Persists everything, stops every agent session and waits (up to two
    /// seconds) for the engine to finish.
    pub fn shutdown(&self) {
        let (done, rx) = oneshot();
        if self.calls.send(Call::Shutdown { done }).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

async fn run(store: Store, calls: crossfire::AsyncRx<mpsc::List<Call>>, launcher: Launcher, probe: bool) {
    let (agent_tx, agent_rx) = mpsc::unbounded_async::<(ThreadId, provider::AgentEvent)>();
    let (internal_tx, internal_rx) = mpsc::unbounded_async::<Internal>();
    let (term_tx, term_rx) =
        mpsc::bounded_blocking_async::<(TermId, Vec<u8>)>(crate::terminal::OUTPUT_CAPACITY);
    let (exit_tx, exit_rx) = mpsc::unbounded_async::<(TermId, Option<i32>)>();
    let mut orchestrator = Orchestrator::new(store, agent_tx, internal_tx, launcher, term_tx, exit_tx);
    orchestrator.recover();
    if probe {
        orchestrator.refresh_providers();
    }
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            call = calls.recv() => match call {
                Ok(Call::Command { command, reply }) => match orchestrator.handle(command) {
                    Outcome::Ready(result) => reply.send(result),
                    Outcome::Deferred(future) => {
                        tokio::spawn(async move { reply.send(future.await) });
                    }
                },
                Ok(Call::Subscribe { epoch, since, reply }) => {
                    let (snapshot, replay, events) = orchestrator.subscribe(epoch, since);
                    reply.send(Subscribed { snapshot, replay, events });
                }
                Ok(Call::Shutdown { done }) => {
                    orchestrator.shutdown();
                    // Release the data folder before saying we are done.
                    drop(orchestrator);
                    done.send(());
                    return;
                }
                Err(_) => {
                    orchestrator.shutdown();
                    break;
                }
            },
            event = agent_rx.recv() => {
                if let Ok((thread, event)) = event {
                    orchestrator.handle_agent(thread, event);
                    // Drain whatever else is ready so a burst of deltas is
                    // applied in one pass.
                    while let Ok((thread, event)) = agent_rx.try_recv() {
                        orchestrator.handle_agent(thread, event);
                    }
                }
            }
            output = term_rx.recv() => {
                if let Ok((term, data)) = output {
                    orchestrator.terminal_output(term, data);
                }
            }
            exit = exit_rx.recv() => {
                if let Ok((term, code)) = exit {
                    orchestrator.terminal_exited(term, code);
                }
            }
            msg = internal_rx.recv() => {
                if let Ok(msg) = msg {
                    orchestrator.handle_internal(msg);
                }
            }
            _ = tick.tick() => orchestrator.tick(),
        }
    }
}

// Feature areas add their engine logic as `impl Orchestrator` blocks in their
// own modules here (review, sessions, workspaces, forge…), touching
// orchestrator.rs only to dispatch commands to them.
