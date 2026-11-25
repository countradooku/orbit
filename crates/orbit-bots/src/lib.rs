//! Chat bots: drive Orbit's agents from Telegram, Slack and Discord.
//!
//! [`Bots::start`] runs them in the engine's process (the desktop app or
//! `orbitd`) on one thread of their own with a single-threaded runtime.
//! Every connection is outbound: Telegram is long-polled, Slack is reached
//! over Socket Mode and Discord over its gateway, so no port is opened and
//! nothing runs in anyone's cloud. With every platform off the thread just
//! sleeps.
//!
//! The bots are an ordinary engine client: they read events and send
//! commands like the app does, and attach to the engine
//! ([`orbit_core::EngineHandle::attach_bots`]) only to answer
//! [`BotCommand`]s from the settings page and the CLI and to receive the
//! platforms' tokens, which never leave the engine's process.
//!
//! See `docs/BOTS.md` for setup and behaviour.

pub mod chat;
mod discord;
mod hub;
mod net;
pub mod render;
mod slack;
mod store;
mod telegram;
mod transport;

use std::path::{Path, PathBuf};

use crossfire::{BlockingTxTrait, MTx, mpsc};
use orbit_core::EngineHandle;
use orbit_core::engine::BotCall;
use orbit_net::Backend;
use orbit_proto::*;

pub use chat::Endpoints;

/// The running bots. Dropping the handle leaves them running; call
/// [`Bots::stop`] to end them.
pub struct Bots {
    stop: MTx<mpsc::List<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Bots {
    /// Starts the bots for the engine whose data folder is `data_dir`.
    pub fn start(engine: EngineHandle, data_dir: &Path) -> std::io::Result<Bots> {
        Self::start_with(engine, data_dir, Endpoints::default())
    }

    /// Starts them against other API addresses (tests use local fakes).
    pub fn start_with(engine: EngineHandle, data_dir: &Path, endpoints: Endpoints) -> std::io::Result<Bots> {
        let (stop, stop_rx) = mpsc::unbounded_async::<()>();
        let (calls, calls_rx) = mpsc::unbounded_async::<BotCall>();
        engine.attach_bots(calls);
        let data_dir: PathBuf = data_dir.to_owned();
        let thread = std::thread::Builder::new().name("orbit-bots".into()).stack_size(1024 * 1024).spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("bots runtime");
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, run(engine, data_dir, endpoints, calls_rx, stop_rx));
        })?;
        Ok(Bots { stop, thread: Some(thread) })
    }

    /// Disconnects every platform and waits (briefly) for the thread.
    pub fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while !thread.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

async fn run(
    engine: EngineHandle,
    data_dir: PathBuf,
    endpoints: Endpoints,
    calls: crossfire::AsyncRx<mpsc::List<BotCall>>,
    stop: crossfire::AsyncRx<mpsc::List<()>>,
) {
    let (inbound_tx, inbound) = mpsc::unbounded_async::<chat::Incoming>();
    let (reports_tx, reports) = mpsc::unbounded_async::<chat::Report>();
    let mut hub = hub::Hub::new(engine.clone(), store::Store::open(&data_dir), endpoints, inbound_tx, reports_tx);
    if let Ok(Ok(Reply::Guard(guard))) = engine.call(Command::Guard(None)).await {
        hub.set_guard(guard);
    }
    let mut feed = Backend::Local(engine).feed();
    hub.reconcile();
    // The tick runs once a second while turns or pairings are open, on a
    // schedule events can't push back.
    let mut due: Option<std::time::Instant> = None;
    loop {
        due = match (hub.busy(), due) {
            (false, _) => None,
            (true, Some(at)) => Some(at),
            (true, None) => Some(std::time::Instant::now() + std::time::Duration::from_secs(1)),
        };
        tokio::select! {
            batch = feed.next_batch() => match batch {
                Some(updates) => hub.on_updates(updates),
                None => break,
            },
            call = calls.recv() => match call {
                Ok(BotCall::Command { command, reply }) => hub.on_bot_command(command, reply),
                Ok(BotCall::Secrets(secrets)) => hub.set_secrets(secrets),
                Err(_) => break,
            },
            incoming = inbound.recv() => {
                if let Ok(incoming) = incoming {
                    hub.on_incoming(incoming).await;
                }
            }
            report = reports.recv() => {
                if let Ok(report) = report {
                    hub.on_report(report);
                }
            }
            _ = sleep_until(due) => {
                due = None;
                hub.tick();
            }
            _ = stop.recv() => break,
        }
    }
    hub.stop_all();
}

async fn sleep_until(at: Option<std::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}
