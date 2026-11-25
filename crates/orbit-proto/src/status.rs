//! Live status evidence: how Orbit knows what an agent is doing, and how
//! sure it is.
//!
//! A thread's durable [`ThreadStatus`] says what the engine last decided
//! (running, waiting for you, failed…). [`ThreadLive`] adds what is only
//! true right now and is recomputed from live evidence, so it is never
//! logged: whether the agent is still talking, has gone quiet, or can no
//! longer be confirmed, and which agents run as plain TUIs in the thread's
//! terminals. [`LiveStatus::of`] folds both into the one state every surface
//! draws, so the sidebar, tabs and tray never disagree.
//!
//! The rule behind every type here: Orbit never guesses "idle" or
//! "working". A state inferred from weak evidence says so
//! ([`Evidence::Heuristic`]), and a state Orbit cannot confirm is shown as
//! [`Liveness::Unverified`].

use bitcode::{Decode, Encode};

use crate::ids::*;
use crate::model::*;

/// Where a status comes from, strongest first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
pub enum Evidence {
    /// The agent's structured protocol: a thread's own session.
    #[default]
    Protocol,
    /// The agent's own hooks, calling Orbit's local endpoint.
    Hook,
    /// The OSC 7501 program-status sequence in the terminal's output.
    Osc,
    /// Inferred from the terminal title, a typed command or prompt text.
    /// Interface copy says "probably".
    Heuristic,
}

impl Evidence {
    /// Whether a state with this evidence is a guess.
    pub fn is_guess(self) -> bool {
        self == Evidence::Heuristic
    }
}

/// How fresh the evidence behind a thread's status is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum Liveness {
    /// Signals arrive as expected, or nothing is running.
    #[default]
    Live,
    /// A turn runs and the agent's process is alive, but it has said
    /// nothing since `since_ms` (Unix ms). `tool` is the tool call still
    /// open, if any: a long build is quiet by nature.
    Quiet { since_ms: u64, tool: Option<String> },
    /// Orbit lost contact and cannot confirm the status (the machine slept,
    /// the agent stopped answering). `reason` is interface copy.
    Unverified { since_ms: u64, reason: String },
}

/// What a thread's agents are doing right now, beyond its durable status.
/// Ephemeral: sent as [`crate::Event::ThreadLive`] and in the snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct ThreadLive {
    /// Freshness of the thread's own session during a turn.
    pub liveness: Liveness,
    /// Unix ms of the session's last signal in the current turn; 0 when
    /// no turn runs.
    pub last_signal_ms: u64,
    /// Agents running as TUIs in the thread's terminals.
    pub terminals: Vec<TerminalAgent>,
}

impl ThreadLive {
    pub fn is_default(&self) -> bool {
        *self == ThreadLive::default()
    }
}

/// An agent running in an Orbit terminal (`claude`, `codex`, `gemini`…
/// typed at a shell prompt), as Orbit sees it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TerminalAgent {
    pub term: TermId,
    /// `claude`, `codex`, `gemini`…; empty when an agent reported status
    /// without saying which it is.
    pub agent: String,
    pub state: AgentState,
    /// What the current state rests on.
    pub evidence: Evidence,
    /// When the agent entered `state` (Unix ms).
    pub since_ms: u64,
    /// The last sign of life: a hook call, a status report or output (Unix ms).
    pub last_signal_ms: u64,
    /// One line of detail: the tool it runs, what it asks. May be empty.
    pub detail: String,
}

/// The state of a terminal agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum AgentState {
    Working,
    /// Blocked on the user.
    NeedsYou(NeedKind),
    /// Finished its turn; waiting for the next prompt.
    Idle,
    Failed,
    RateLimited,
    /// It was working, but Orbit has heard nothing for too long to say so.
    Unverified,
}

/// What a blocked agent needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum NeedKind {
    /// Approve a command, an edit or a tool.
    Permission,
    /// Answer a question.
    Question,
    /// Sign in.
    Auth,
}

/// The status a thread shows once its live evidence is counted: the input
/// to the one glyph vocabulary every surface draws (`orbit-app`'s
/// `ui::Glyph`). `Quiet` draws as working with its "No output for…" line.
/// Ordered by how much it needs the user: `Permission` first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LiveStatus {
    /// Amber shield: approve something.
    Permission,
    /// Amber `?`: answer a question or sign in.
    NeedsYou,
    /// Red `!`.
    Failed,
    /// Waiting for the provider's quota to reset.
    RateLimited,
    /// Rotating arc.
    Working,
    /// Working, but nothing heard for a while ("No output for 2m").
    Quiet,
    /// Dashed ring: Orbit cannot confirm what the agent is doing.
    Unverified,
    /// Blue dot: a result the user has not seen.
    DoneUnread,
    /// No glyph.
    Idle,
}

impl LiveStatus {
    /// The glyph of a thread: its own status and freshness, and the most
    /// urgent of the agents in its terminals.
    pub fn of(thread: &Thread, live: Option<&ThreadLive>) -> LiveStatus {
        let own = Self::of_thread(thread, live.map(|l| &l.liveness));
        let terminals = live.into_iter().flat_map(|l| l.terminals.iter()).map(|a| Self::of_agent(a.state));
        terminals.fold(own, Ord::min)
    }

    fn of_thread(thread: &Thread, liveness: Option<&Liveness>) -> LiveStatus {
        if thread.status == ThreadStatus::Waiting || !thread.pending.is_empty() {
            let permission = thread.pending.iter().any(|p| {
                matches!(p.request.kind, RequestKind::Command { .. } | RequestKind::FileChange { .. } | RequestKind::Tool { .. })
            });
            return if permission { LiveStatus::Permission } else { LiveStatus::NeedsYou };
        }
        match &thread.status {
            ThreadStatus::Failed { .. } => LiveStatus::Failed,
            ThreadStatus::RateLimited { .. } => LiveStatus::RateLimited,
            ThreadStatus::Running | ThreadStatus::Starting => match liveness {
                Some(Liveness::Unverified { .. }) => LiveStatus::Unverified,
                Some(Liveness::Quiet { .. }) => LiveStatus::Quiet,
                _ => LiveStatus::Working,
            },
            _ if thread.unread => LiveStatus::DoneUnread,
            _ => LiveStatus::Idle,
        }
    }

    /// The glyph of a terminal agent's state.
    pub fn of_agent(state: AgentState) -> LiveStatus {
        match state {
            AgentState::Working => LiveStatus::Working,
            AgentState::NeedsYou(NeedKind::Permission) => LiveStatus::Permission,
            AgentState::NeedsYou(_) => LiveStatus::NeedsYou,
            AgentState::Idle => LiveStatus::Idle,
            AgentState::Failed => LiveStatus::Failed,
            AgentState::RateLimited => LiveStatus::RateLimited,
            AgentState::Unverified => LiveStatus::Unverified,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LiveStatus::Permission => "Needs your approval",
            LiveStatus::NeedsYou => "Needs you",
            LiveStatus::Failed => "Failed",
            LiveStatus::RateLimited => "Waiting for quota",
            LiveStatus::Working => "Working",
            LiveStatus::Quiet => "No recent output",
            LiveStatus::Unverified => "Unverified",
            LiveStatus::DoneUnread => "New result",
            LiveStatus::Idle => "Idle",
        }
    }
}

/// The one-line explanation of a thread's freshness, for hovers and
/// status lines: "No output for 4m · cargo test", "Unverified · Orbit was
/// asleep". `None` when there is nothing to qualify.
pub fn liveness_line(liveness: &Liveness, now_ms: u64) -> Option<String> {
    match liveness {
        Liveness::Live => None,
        Liveness::Quiet { since_ms, tool } => {
            let quiet = format!("No output for {}", short_duration(now_ms.saturating_sub(*since_ms)));
            Some(match tool {
                Some(tool) if !tool.is_empty() => format!("{quiet} · {tool}"),
                _ => quiet,
            })
        }
        Liveness::Unverified { reason, .. } => Some(format!("Unverified · {reason}")),
    }
}

/// One line for a terminal agent: "Claude · working", "Codex · probably
/// waiting for you".
pub fn agent_line(agent: &TerminalAgent) -> String {
    let name = match agent.agent.as_str() {
        "" => "Agent".to_owned(),
        "claude" => "Claude Code".to_owned(),
        "codex" => "Codex".to_owned(),
        "gemini" => "Gemini".to_owned(),
        "opencode" => "OpenCode".to_owned(),
        other => humanize_id(other),
    };
    let state = match agent.state {
        AgentState::Working => "working",
        AgentState::NeedsYou(NeedKind::Permission) => "waiting for your approval",
        AgentState::NeedsYou(NeedKind::Auth) => "waiting for you to sign in",
        AgentState::NeedsYou(NeedKind::Question) => "waiting for you",
        AgentState::Idle => "done",
        AgentState::Failed => "failed",
        AgentState::RateLimited => "out of quota",
        AgentState::Unverified => "not heard from",
    };
    let probably = if agent.evidence.is_guess() && agent.state != AgentState::Unverified { "probably " } else { "" };
    if agent.detail.is_empty() {
        format!("{name} · {probably}{state}")
    } else {
        format!("{name} · {probably}{state} · {}", agent.detail)
    }
}

/// `90_000` → `1m`, `7_200_000` → `2h`, `45_000` → `45s`.
pub fn short_duration(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(state: AgentState, evidence: Evidence) -> TerminalAgent {
        TerminalAgent {
            term: TermId(1),
            agent: "codex".into(),
            state,
            evidence,
            since_ms: 0,
            last_signal_ms: 0,
            detail: String::new(),
        }
    }

    #[test]
    fn quiet_and_unverified_turns_never_draw_as_working() {
        let running = Thread { status: ThreadStatus::Running, ..Default::default() };
        assert_eq!(LiveStatus::of(&running, None), LiveStatus::Working);
        let quiet = ThreadLive { liveness: Liveness::Quiet { since_ms: 0, tool: None }, ..Default::default() };
        assert_eq!(LiveStatus::of(&running, Some(&quiet)), LiveStatus::Quiet);
        let lost = ThreadLive { liveness: Liveness::Unverified { since_ms: 0, reason: "x".into() }, ..Default::default() };
        assert_eq!(LiveStatus::of(&running, Some(&lost)), LiveStatus::Unverified);
        // Freshness qualifies a running turn only.
        let idle = Thread { unread: true, ..Default::default() };
        assert_eq!(LiveStatus::of(&idle, Some(&lost)), LiveStatus::DoneUnread);
    }

    #[test]
    fn approvals_differ_from_questions_and_terminal_agents_count() {
        let request = |kind| PendingRequest {
            item: ItemId(1),
            request: AgentRequest { id: RequestId(1), provider_ref: String::new(), kind, resolution: None },
            created_ms: 0,
        };
        let mut thread = Thread { status: ThreadStatus::Waiting, ..Default::default() };
        thread.pending = vec![request(RequestKind::Question { question: "?".into(), options: vec![], multi_select: false })];
        assert_eq!(LiveStatus::of(&thread, None), LiveStatus::NeedsYou);
        thread.pending = vec![request(RequestKind::Command { command: "ls".into(), cwd: ".".into(), reason: String::new() })];
        assert_eq!(LiveStatus::of(&thread, None), LiveStatus::Permission);

        let idle = Thread::default();
        let live = ThreadLive {
            terminals: vec![agent(AgentState::Idle, Evidence::Hook), agent(AgentState::NeedsYou(NeedKind::Permission), Evidence::Heuristic)],
            ..Default::default()
        };
        assert_eq!(LiveStatus::of(&idle, Some(&live)), LiveStatus::Permission);
    }

    #[test]
    fn guesses_say_so() {
        assert_eq!(agent_line(&agent(AgentState::Working, Evidence::Hook)), "Codex · working");
        assert_eq!(
            agent_line(&agent(AgentState::NeedsYou(NeedKind::Permission), Evidence::Heuristic)),
            "Codex · probably waiting for your approval"
        );
        assert_eq!(agent_line(&agent(AgentState::Unverified, Evidence::Heuristic)), "Codex · not heard from");
        let quiet = Liveness::Quiet { since_ms: 1_000, tool: Some("cargo test".into()) };
        assert_eq!(liveness_line(&quiet, 241_000).as_deref(), Some("No output for 4m · cargo test"));
        assert_eq!(liveness_line(&Liveness::Live, 0), None);
        assert_eq!(short_duration(7_200_000), "2h");
    }
}
