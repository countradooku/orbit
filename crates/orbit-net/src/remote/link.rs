//! A connection to an SSH or WSL host that keeps itself up: before every
//! attempt it probes the host for `orbitd` ([`Remote::probe`]) and then
//! speaks to `orbitd --local --stdio` there. What it found is kept as a
//! [`LinkPhase`] the interface shows: probing, ready, `orbitd` to install,
//! or a failure with its fix.
//!
//! A failure only the user can fix (a host key to accept, a passphrase, an
//! `orbitd` to install) is not probed again every few seconds: the link
//! waits up to five minutes, or until [`RemoteLink::retry`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Failure, FailureKind, HostEntry, Probe, ProbeInfo, Remote, Tools};
use crate::RemoteClient;

/// How long a failure the user must fix is kept before probing again.
const USER_FIX_WAIT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkPhase {
    /// Not tried yet, or probing now.
    Probing,
    /// `orbitd` answered; connecting or connected through it.
    Ready(ProbeInfo),
    /// No usable `orbitd` there: install one ([`Probe::Missing`] or
    /// [`Probe::Outdated`]).
    NeedsInstall(Probe),
    Failed(Failure),
}

impl LinkPhase {
    /// What the user must do before trying again helps.
    pub fn waits_on_user(&self) -> bool {
        match self {
            LinkPhase::NeedsInstall(_) => true,
            LinkPhase::Failed(f) => matches!(f.kind, FailureKind::Auth | FailureKind::HostKey | FailureKind::NoTool | FailureKind::NoDistro),
            _ => false,
        }
    }
}

pub struct RemoteLink {
    entry: HostEntry,
    tools: Tools,
    phase: Mutex<(LinkPhase, Instant)>,
    retry: AtomicBool,
}

impl RemoteLink {
    pub fn new(entry: HostEntry, tools: Tools) -> Arc<Self> {
        Arc::new(Self { entry, tools, phase: Mutex::new((LinkPhase::Probing, Instant::now())), retry: AtomicBool::new(false) })
    }

    pub fn entry(&self) -> &HostEntry {
        &self.entry
    }

    pub fn tools(&self) -> &Tools {
        &self.tools
    }

    pub fn phase(&self) -> LinkPhase {
        self.phase.lock().unwrap_or_else(|e| e.into_inner()).0.clone()
    }

    fn set(&self, phase: LinkPhase) {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner()) = (phase, Instant::now());
    }

    /// Probe again on the next attempt even if the last failure waits on
    /// the user (they say they fixed it). Pair with
    /// [`RemoteClient::reconnect_now`].
    pub fn retry(&self) {
        self.retry.store(true, Ordering::Release);
    }

    /// `orbitd` was just installed: connect through it.
    pub fn installed(&self, info: ProbeInfo) {
        self.set(LinkPhase::Ready(info));
        self.retry();
    }

    /// The words for a phase that keeps the link down.
    pub fn reason(&self, phase: &LinkPhase) -> String {
        let label = self.entry.label();
        match phase {
            LinkPhase::Probing => format!("Looking for orbitd on {label}…"),
            LinkPhase::Ready(_) => format!("Connecting to {label}…"),
            LinkPhase::NeedsInstall(Probe::Outdated(info)) => format!(
                "{label} has orbitd from Orbit {} (protocol {}); this Orbit needs protocol {}.",
                info.version,
                info.protocol,
                orbit_proto::PROTOCOL_VERSION
            ),
            LinkPhase::NeedsInstall(_) => format!("orbitd isn’t installed on {label}."),
            LinkPhase::Failed(failure) => failure.message.clone(),
        }
    }

    /// One attempt's target: probes, then names `orbitd --local --stdio`.
    fn resolve(&self) -> Result<(String, String), String> {
        let (phase, since) = self.phase.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let retry = self.retry.swap(false, Ordering::AcqRel);
        if phase.waits_on_user() && !retry && since.elapsed() < USER_FIX_WAIT {
            return Err(self.reason(&phase));
        }
        let remote = Remote::new(&self.entry, &self.tools);
        // Just installed: its probe is fresh, connect at once.
        if let (LinkPhase::Ready(info), true) = (&phase, retry)
            && let Some(url) = remote.connect_url(info)
        {
            return Ok((url, String::new()));
        }
        self.set(LinkPhase::Probing);
        let next = match remote.probe() {
            Ok(Probe::Ready(info)) => match remote.connect_url(&info) {
                Some(url) => {
                    self.set(LinkPhase::Ready(info));
                    return Ok((url, String::new()));
                }
                None => LinkPhase::Failed(Failure::other("This host is not reached over SSH or WSL.")),
            },
            Ok(probe) => LinkPhase::NeedsInstall(probe),
            Err(failure) => LinkPhase::Failed(failure),
        };
        let reason = self.reason(&next);
        self.set(next);
        Err(reason)
    }

    /// A client that keeps this host connected.
    pub fn connect(self: &Arc<Self>) -> std::io::Result<RemoteClient> {
        let link = self.clone();
        RemoteClient::connect_with(Box::new(move || link.resolve()))
    }
}
