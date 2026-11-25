//! The background host: what keeps it running, and terminals that outlive
//! the app.
//!
//! On a desktop the engine runs in `orbitd --local`, a per-user host process
//! the app starts and connects to (see `docs/HOST.md`). These commands let a
//! client ask what the host is busy with, keep it alive across an app
//! restart, restart it (an update), and re-attach terminals with their
//! scrollback. None of these types are logged.

use bitcode::{Decode, Encode};

use crate::ids::*;

/// What [`Command::Host`](crate::Command::Host) asks.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum HostCommand {
    /// What keeps the host busy. Replies [`HostReply::Activity`].
    Activity,
    /// This connection is an app window: the host's lifetime follows the
    /// windows connected to it (see [`KeepRunning`]). Handled by the host
    /// process; an in-process engine replies `Done`.
    AttachUi,
    /// Keep running for at least `ms` even with no window connected, so
    /// the app can restart (an update) without the host exiting under it.
    Hold { ms: u64 },
    /// Restart the host process: now, or once nothing is working. The
    /// app starts the new one when it reconnects. Fails in-process.
    Restart { when: RestartWhen },
    /// Re-attach the thread's terminal, or open one in `cwd` when it has
    /// none. A sleeping terminal is woken. Replies [`HostReply::Terminal`].
    AttachTerminal { thread: ThreadId, cwd: String, cols: u16, rows: u16 },
    /// The screen that sent `AttachTerminal` has drawn the scrollback:
    /// send the output held since the reply. (Held so that nothing is lost
    /// or drawn twice between the reply and the screen taking over; it is
    /// released after a few seconds anyway.)
    TerminalReady { term: TermId },
    /// Wake a sleeping terminal (a fresh shell in the same folder, the
    /// scrollback kept). `Done` when it is awake already.
    WakeTerminal { term: TermId },
    /// Put a terminal to sleep now: its shell stops, its scrollback stays.
    SleepTerminal { term: TermId },
    /// The interactive terminals the host keeps. Replies
    /// [`HostReply::Terminals`].
    Terminals,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum HostReply {
    Activity(HostActivity),
    /// The attached terminal and the output to replay into a fresh screen.
    Terminal { session: TermSession, scrollback: Vec<u8> },
    Terminals(Vec<TermSession>),
}

/// When a requested restart happens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum RestartWhen {
    /// Once no agent works and no terminal runs a command.
    #[default]
    WhenIdle,
    /// Now: running agents and terminals are stopped.
    Now,
}

/// Whether the host keeps agents and terminals running once the last app
/// window closes. A preference of this computer (`<data>/host.json`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum KeepRunning {
    /// Until the computer restarts or the user quits it.
    Always,
    /// While an agent works, a terminal runs a command or an automation is
    /// due soon; then it exits after a grace period.
    #[default]
    WhileWorking,
    /// Exits shortly after the app closes, stopping agents and terminals.
    Never,
}

impl KeepRunning {
    pub const ALL: [KeepRunning; 3] = [KeepRunning::Always, KeepRunning::WhileWorking, KeepRunning::Never];

    /// The stable name used in `host.json`.
    pub fn key(self) -> &'static str {
        match self {
            KeepRunning::Always => "always",
            KeepRunning::WhileWorking => "while_working",
            KeepRunning::Never => "never",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.key() == key)
    }

    pub fn label(self) -> &'static str {
        match self {
            KeepRunning::Always => "Always",
            KeepRunning::WhileWorking => "While working",
            KeepRunning::Never => "Never",
        }
    }
}

/// What the host is busy with, and who it is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct HostActivity {
    /// Threads with a running turn.
    pub working: Vec<ThreadId>,
    /// Interactive terminals whose shell runs a command (a dev server, a
    /// build, an agent CLI).
    pub busy_terminals: u32,
    /// Open terminals, sleeping ones excluded.
    pub terminals: u32,
    pub sleeping_terminals: u32,
    /// The next scheduled automation run (Unix ms).
    pub next_automation_ms: Option<u64>,
    /// Filled in by the host process (zero in-process).
    pub pid: u32,
    /// The host's Orbit version.
    pub version: String,
    pub protocol: u16,
    pub started_ms: u64,
    /// App windows connected now.
    pub windows: u32,
    /// A restart waits for the work to finish.
    pub restart_pending: bool,
}

/// How soon a scheduled automation must be for the host to stay up for it.
pub const AUTOMATION_SOON_MS: u64 = 10 * 60 * 1000;

impl HostActivity {
    /// Something would stop if the host exited now.
    pub fn is_live(&self, now_ms: u64) -> bool {
        !self.working.is_empty()
            || self.busy_terminals > 0
            || self.next_automation_ms.is_some_and(|t| t <= now_ms.saturating_add(AUTOMATION_SOON_MS))
    }

    /// "2 agents are working", "an agent is working and 1 terminal runs a
    /// command", or `None` when nothing runs.
    pub fn summary(&self) -> Option<String> {
        let agents = match self.working.len() {
            0 => None,
            1 => Some("an agent is working".to_owned()),
            n => Some(format!("{n} agents are working")),
        };
        let terms = match self.busy_terminals {
            0 => None,
            1 => Some("a terminal runs a command".to_owned()),
            n => Some(format!("{n} terminals run commands")),
        };
        match (agents, terms) {
            (Some(a), Some(t)) => Some(format!("{a} and {t}")),
            (a, t) => a.or(t),
        }
    }
}

/// An interactive terminal the host keeps.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct TermSession {
    pub term: TermId,
    pub thread: Option<ThreadId>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    /// Its shell was stopped to save resources; it restarts on focus.
    pub sleeping: bool,
    /// Its shell runs a command.
    pub busy: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};
    use crate::wire::{Command, Reply};

    #[test]
    fn host_messages_round_trip() {
        let cmd = Command::Host(HostCommand::AttachTerminal { thread: ThreadId(3), cwd: "/r".into(), cols: 80, rows: 24 });
        assert_eq!(decode::<Command>(&encode(&cmd)).unwrap(), cmd);
        let reply = Reply::Host(HostReply::Terminal {
            session: TermSession { term: TermId(4), thread: Some(ThreadId(3)), cwd: "/r".into(), cols: 80, rows: 24, sleeping: false, busy: true },
            scrollback: b"hello\r\n".to_vec(),
        });
        assert_eq!(decode::<Reply>(&encode(&reply)).unwrap(), reply);
    }

    #[test]
    fn liveness_and_summary() {
        let now = 1_000_000;
        let mut a = HostActivity::default();
        assert!(!a.is_live(now));
        assert_eq!(a.summary(), None);
        a.next_automation_ms = Some(now + AUTOMATION_SOON_MS + 1);
        assert!(!a.is_live(now), "an automation hours away does not keep the host up");
        a.next_automation_ms = Some(now + 60_000);
        assert!(a.is_live(now));
        a.next_automation_ms = None;
        a.working = vec![ThreadId(1), ThreadId(2)];
        a.busy_terminals = 1;
        assert!(a.is_live(now));
        assert_eq!(a.summary().unwrap(), "2 agents are working and a terminal runs a command");
        for k in KeepRunning::ALL {
            assert_eq!(KeepRunning::from_key(k.key()), Some(k));
        }
    }
}
