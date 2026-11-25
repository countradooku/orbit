//! The Orbit protocol over WebSockets, and one client API for every backend.
//!
//! A UI talks to a [`Backend`]: either the engine in its own process or an
//! `orbitd` host over the network. Both expose the same calls and the same
//! [`Feed`] of [`Update`]s, so the UI never knows which one it has, and a
//! remote host that drops its connection is replayed from the last event the
//! UI applied instead of being reloaded.

pub mod client;
pub mod server;
pub mod state;

use crossfire::oneshot::{RxOneshot, oneshot};
use crossfire::{AsyncRx, mpsc};
use orbit_core::{EngineHandle, engine::EventRx};
use orbit_proto::*;

pub use client::RemoteClient;
pub use server::{ServerOptions, serve};

/// The port `orbitd` listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 7840;

/// Events are sent in frames of at most this many envelopes.
pub(crate) const MAX_BATCH: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting,
    Connected,
    /// Lost; reconnecting. `reason` is shown to the user.
    Disconnected { reason: String },
}

/// What a UI applies to its state.
#[derive(Debug)]
pub enum Update {
    /// Replace all state with this snapshot.
    Reset(Snapshot),
    Events(Vec<Envelope>),
    Status(ConnectionStatus),
}

/// The engine, local or remote.
#[derive(Clone)]
pub enum Backend {
    Local(EngineHandle),
    Remote(RemoteClient),
}

impl Backend {
    pub fn call(&self, command: Command) -> RxOneshot<Result<Reply, String>> {
        match self {
            Backend::Local(engine) => engine.call(command),
            Backend::Remote(remote) => remote.call(command),
        }
    }

    /// Fire-and-forget.
    pub fn send(&self, command: Command) {
        match self {
            Backend::Local(engine) => engine.send(command),
            Backend::Remote(remote) => drop(remote.call(command)),
        }
    }

    /// The stream of updates. Call once per UI.
    pub fn feed(&self) -> Feed {
        match self {
            Backend::Local(engine) => Feed::Local(LocalFeed { engine: engine.clone(), events: None, epoch: 0, seq: 0 }),
            Backend::Remote(remote) => match remote.updates() {
                Some(rx) => Feed::Remote(rx),
                None => Feed::Closed,
            },
        }
    }

    pub fn shutdown(&self) {
        match self {
            Backend::Local(engine) => engine.shutdown(),
            Backend::Remote(remote) => remote.close(),
        }
    }
}

pub enum Feed {
    Local(LocalFeed),
    Remote(AsyncRx<mpsc::List<Update>>),
    /// A second feed of a remote backend: always ends immediately.
    Closed,
}

pub struct LocalFeed {
    engine: EngineHandle,
    events: Option<EventRx>,
    epoch: u64,
    seq: u64,
}

impl Feed {
    /// Waits for the next updates and returns everything available at once,
    /// so a burst of deltas is applied in one pass and costs one render.
    /// `None` means the backend is gone for good.
    pub async fn next_batch(&mut self) -> Option<Vec<Update>> {
        match self {
            Feed::Remote(rx) => {
                let first = rx.recv().await.ok()?;
                let mut batch = vec![first];
                while let Ok(update) = rx.try_recv() {
                    batch.push(update);
                }
                Some(batch)
            }
            Feed::Local(local) => local.next_batch().await,
            Feed::Closed => None,
        }
    }
}

impl LocalFeed {
    async fn next_batch(&mut self) -> Option<Vec<Update>> {
        loop {
            let Some(events) = &self.events else {
                // (Re)subscribe from the last applied event. A subscriber that
                // fell behind lands here after the engine dropped it.
                let sub = self.engine.subscribe(self.epoch, self.seq).await.ok()?;
                let mut batch = Vec::new();
                if let Some(snapshot) = sub.snapshot {
                    self.epoch = snapshot.epoch;
                    self.seq = snapshot.seq;
                    batch.push(Update::Reset(snapshot));
                }
                if !sub.replay.is_empty() {
                    self.track(&sub.replay);
                    batch.push(Update::Events(sub.replay));
                }
                self.events = Some(sub.events);
                if !batch.is_empty() {
                    return Some(batch);
                }
                continue;
            };
            match events.recv().await {
                Ok(first) => {
                    let mut envs = Vec::with_capacity(16);
                    envs.push(first);
                    while envs.len() < MAX_BATCH * 8 {
                        match events.try_recv() {
                            Ok(env) => envs.push(env),
                            Err(_) => break,
                        }
                    }
                    self.track(&envs);
                    return Some(vec![Update::Events(envs)]);
                }
                Err(_) => self.events = None,
            }
        }
    }

    fn track(&mut self, envs: &[Envelope]) {
        if let Some(seq) = envs.iter().rev().find(|e| e.seq != 0).map(|e| e.seq) {
            self.seq = seq;
        }
    }
}

/// A oneshot that already holds `result`.
pub(crate) fn ready<T>(result: T) -> RxOneshot<T> {
    let (tx, rx) = oneshot();
    tx.send(result);
    rx
}

#[cfg(test)]
mod tests;
