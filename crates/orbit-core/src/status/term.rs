//! What runs in one Orbit terminal, from the evidence the engine sees: the
//! PTY's output, the user's keystrokes, and hook calls.
//!
//! Evidence ranks hook > OSC 7501 > heuristic. A weaker source never
//! overrides a stronger one, with two deliberate exceptions that only move
//! toward "not working": the user pressing Esc or Ctrl+C while an agent
//! works (no hook fires on an interrupt), and the shell printing its prompt
//! again (OSC 133), which proves the agent left the foreground.
//!
//! Liveness comes from output: a TUI that is working redraws its spinner
//! many times a second. A "working" agent whose terminal printed nothing for
//! [`UNVERIFIED_AFTER`] becomes [`AgentState::Unverified`]; Orbit does not
//! pretend it still works, nor that it stopped.

use std::time::{Duration, Instant};

use orbit_proto::{AgentState, Evidence, NeedKind, TermId, TerminalAgent, ThreadId};

use super::osc::{Osc, OscScanner, ReportState, parse_report, state_word};

/// A working agent with no output or report for this long is unverified.
pub const UNVERIFIED_AFTER: Duration = Duration::from_secs(120);
/// Longest typed line remembered while looking for an agent command.
const MAX_LINE: usize = 512;

/// Agents recognised by the command that starts them.
pub const KNOWN_AGENTS: &[&str] = &[
    "claude", "codex", "gemini", "opencode", "pi", "qwen", "aider", "amp", "cursor-agent", "goose", "kiro-cli", "droid",
    "copilot", "grok", "kimi", "crush", "auggie", "cline",
];

/// A hook call, already parsed from its payload.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookCall {
    pub agent: String,
    /// The agent's own event name (`PreToolUse`, `agent-turn-complete`…).
    pub event: String,
    pub tool: String,
    /// Claude's `notification_type`, or a `kind` given by a script.
    pub kind: String,
    pub message: String,
}

/// What a hook call means for the agent's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookEffect {
    State { state: AgentState, detail: String },
    Started,
    Ended,
    /// Carries no state (subagent bookkeeping, unknown events): only
    /// proves the agent is alive.
    Alive,
}

/// Maps a hook call to its effect. Event names are Claude Code's and
/// Codex's (they share most of them); Codex's `notify` sends
/// `agent-turn-complete`. Plain state words work too, for custom scripts.
pub fn hook_effect(call: &HookCall) -> HookEffect {
    let working = |detail: &str| HookEffect::State { state: AgentState::Working, detail: detail.to_owned() };
    let needs = |kind, detail: &str| HookEffect::State { state: AgentState::NeedsYou(kind), detail: detail.to_owned() };
    match call.event.as_str() {
        "SessionStart" => HookEffect::Started,
        "SessionEnd" => HookEffect::Ended,
        "UserPromptSubmit" | "UserPromptExpansion" | "PostToolUse" | "PostToolUseFailure" | "PostToolBatch"
        | "PreCompact" | "PostCompact" | "PermissionDenied" | "ElicitationResult" => working(""),
        "PreToolUse" => working(&call.tool),
        "PermissionRequest" => needs(NeedKind::Permission, &call.tool),
        "Elicitation" => needs(NeedKind::Question, &call.message),
        "Notification" => match call.kind.as_str() {
            "permission_prompt" | "ToolPermission" => needs(NeedKind::Permission, &call.message),
            "elicitation_dialog" | "agent_needs_input" => needs(NeedKind::Question, &call.message),
            "idle_prompt" | "agent_completed" => HookEffect::State { state: AgentState::Idle, detail: String::new() },
            "auth_success" => HookEffect::Alive,
            _ => {
                let lower = call.message.to_lowercase();
                if lower.contains("permission") || lower.contains("approv") {
                    needs(NeedKind::Permission, &call.message)
                } else if lower.contains("waiting for your input") {
                    HookEffect::State { state: AgentState::Idle, detail: String::new() }
                } else {
                    HookEffect::Alive
                }
            }
        },
        "Stop" | "agent-turn-complete" | "Interrupt" => HookEffect::State { state: AgentState::Idle, detail: String::new() },
        "StopFailure" => {
            let lower = call.message.to_lowercase();
            let state = if lower.contains("rate limit") || lower.contains("usage limit") || lower.contains("quota") {
                AgentState::RateLimited
            } else {
                AgentState::Failed
            };
            HookEffect::State { state, detail: call.message.clone() }
        }
        word => match state_word(&word.to_ascii_lowercase(), Some(call.kind.as_str())) {
            Some(ReportState::Is(state)) => HookEffect::State { state, detail: call.message.clone() },
            Some(ReportState::Ended) => HookEffect::Ended,
            None => HookEffect::Alive,
        },
    }
}

#[derive(Clone, Debug)]
struct Tracked {
    agent: String,
    state: AgentState,
    evidence: Evidence,
    since_ms: u64,
    detail: String,
    last_signal: Instant,
    last_signal_ms: u64,
    /// The strongest source that ever reported for this agent.
    strongest: Evidence,
    /// The state before it went unverified, restored by fresh output.
    before_unverified: Option<(AgentState, Evidence)>,
}

/// Everything known about one terminal.
#[derive(Debug)]
pub struct TermWatch {
    pub term: TermId,
    /// The thread whose status the agents count toward.
    pub thread: Option<ThreadId>,
    /// Set by an explicit `BindTerminal`, which inference never overrides.
    pub bound_explicitly: bool,
    /// Proves a hook call came from this terminal.
    pub token: String,
    scanner: OscScanner,
    /// The command line being typed at the shell prompt.
    line: Vec<u8>,
    /// The line was edited in ways Orbit cannot follow (history, completion).
    line_dirty: bool,
    agent: Option<Tracked>,
}

impl TermWatch {
    pub fn new(term: TermId, token: String, thread: Option<ThreadId>) -> Self {
        Self {
            term,
            thread,
            bound_explicitly: false,
            token,
            scanner: OscScanner::default(),
            line: Vec::new(),
            line_dirty: false,
            agent: None,
        }
    }

    /// The agent as the protocol reports it.
    pub fn agent(&self) -> Option<TerminalAgent> {
        self.agent.as_ref().map(|a| TerminalAgent {
            term: self.term,
            agent: a.agent.clone(),
            state: a.state,
            evidence: a.evidence,
            since_ms: a.since_ms,
            last_signal_ms: a.last_signal_ms,
            detail: a.detail.clone(),
        })
    }

    /// Output from the PTY. Returns whether the reported agent changed.
    pub fn on_output(&mut self, data: &[u8], now: Instant, now_ms: u64) -> bool {
        let before = self.agent();
        let mut found: Vec<Found> = Vec::new();
        self.scanner.scan(data, |osc| match osc {
            Osc::Status(payload) => found.push(Found::Report(parse_report(payload))),
            Osc::Title(title) => found.push(Found::Title(String::from_utf8_lossy(title).into_owned())),
            Osc::Prompt => found.push(Found::Prompt),
        });
        for f in found {
            match f {
                Found::Report(report) => {
                    if report.probe && report.state.is_none() {
                        // A program that speaks the protocol announced
                        // itself: something agent-like started.
                        self.ensure_agent(report.agent.as_deref().unwrap_or(""), Evidence::Osc, now, now_ms);
                        continue;
                    }
                    match report.state {
                        Some(ReportState::Ended) => self.end(),
                        Some(ReportState::Is(state)) => {
                            let name = report.agent.as_deref().unwrap_or("");
                            self.set(name, state, Evidence::Osc, report.detail.unwrap_or_default(), now, now_ms);
                        }
                        None => {
                            if let (Some(a), Some(detail)) = (self.agent.as_mut(), report.detail) {
                                a.detail = detail;
                            }
                        }
                    }
                }
                Found::Title(title) => self.on_title(&title, now, now_ms),
                Found::Prompt => self.end(),
            }
        }
        if let Some(a) = self.agent.as_mut() {
            a.last_signal = now;
            a.last_signal_ms = now_ms;
            if a.state == AgentState::Unverified
                && let Some((state, evidence)) = a.before_unverified.take()
            {
                a.state = state;
                a.evidence = evidence;
                a.since_ms = now_ms;
            }
        }
        if self.agent.as_ref().is_some_and(|a| a.strongest == Evidence::Heuristic) {
            self.prompt_patterns(data, now, now_ms);
        }
        self.changed(&before)
    }

    /// Keystrokes the user sent. Returns whether the reported agent changed.
    pub fn on_input(&mut self, data: &[u8], now: Instant, now_ms: u64) -> bool {
        let before = self.agent();
        if let Some(a) = &self.agent {
            let interrupt = data == [0x1b] || data == [0x03];
            let enter = data.contains(&b'\r') || data.contains(&b'\n');
            match a.state {
                AgentState::Working | AgentState::Unverified if interrupt => {
                    // No hook fires on an interrupt; the user just stopped it.
                    self.set_forced(AgentState::Idle, Evidence::Heuristic, "stopped", now, now_ms);
                }
                AgentState::NeedsYou(_) if a.strongest == Evidence::Heuristic && (enter || interrupt) => {
                    self.set_forced(AgentState::Working, Evidence::Heuristic, "", now, now_ms);
                }
                AgentState::Idle | AgentState::Failed | AgentState::RateLimited
                    if a.strongest == Evidence::Heuristic && enter =>
                {
                    // A prompt was submitted to an agent that has no hooks.
                    self.set_forced(AgentState::Working, Evidence::Heuristic, "", now, now_ms);
                }
                _ => {}
            }
            return self.changed(&before);
        }
        for &b in data {
            match b {
                b'\r' | b'\n' => {
                    let line = std::mem::take(&mut self.line);
                    let dirty = std::mem::replace(&mut self.line_dirty, false);
                    if !dirty
                        && let Some(agent) = agent_command(&String::from_utf8_lossy(&line))
                    {
                        self.ensure_agent(agent, Evidence::Heuristic, now, now_ms);
                        // Whatever else is in this chunk was typed to the agent.
                        break;
                    }
                }
                0x7f | 0x08 => {
                    // Backspace: drop one whole character.
                    while let Some(last) = self.line.pop() {
                        if last & 0xc0 != 0x80 {
                            break;
                        }
                    }
                }
                0x03 | 0x15 => {
                    self.line.clear();
                    self.line_dirty = false;
                }
                // Escape sequences (history, cursor keys) and completion
                // change the line in ways Orbit cannot see.
                0x1b | b'\t' => self.line_dirty = true,
                b if b < 0x20 => {}
                b => {
                    if self.line.len() < MAX_LINE {
                        self.line.push(b);
                    } else {
                        self.line_dirty = true;
                    }
                }
            }
        }
        self.changed(&before)
    }

    /// A hook call from this terminal. Returns whether the agent changed.
    pub fn on_hook(&mut self, call: &HookCall, now: Instant, now_ms: u64) -> bool {
        let before = self.agent();
        match hook_effect(call) {
            HookEffect::Started => self.set(&call.agent, AgentState::Idle, Evidence::Hook, String::new(), now, now_ms),
            HookEffect::Ended => self.end(),
            HookEffect::State { state, detail } => self.set(&call.agent, state, Evidence::Hook, detail, now, now_ms),
            HookEffect::Alive => {
                self.ensure_agent(&call.agent, Evidence::Hook, now, now_ms);
                if let Some(a) = self.agent.as_mut() {
                    a.last_signal = now;
                    a.last_signal_ms = now_ms;
                }
            }
        }
        self.changed(&before)
    }

    /// Ages the evidence. Returns whether the reported agent changed.
    pub fn tick(&mut self, now: Instant, now_ms: u64) -> bool {
        let Some(a) = self.agent.as_mut() else { return false };
        if a.state == AgentState::Working && now.saturating_duration_since(a.last_signal) >= UNVERIFIED_AFTER {
            a.before_unverified = Some((a.state, a.evidence));
            a.state = AgentState::Unverified;
            a.since_ms = now_ms;
            return true;
        }
        false
    }

    /// The machine slept or the clock jumped: nothing heard before can be
    /// trusted to still hold for a working agent.
    pub fn distrust(&mut self, reason_ms: u64) -> bool {
        let Some(a) = self.agent.as_mut() else { return false };
        if a.state == AgentState::Working {
            a.before_unverified = Some((a.state, a.evidence));
            a.state = AgentState::Unverified;
            a.since_ms = reason_ms;
            return true;
        }
        false
    }

    fn changed(&self, before: &Option<TerminalAgent>) -> bool {
        // Signal times alone do not make a change worth broadcasting.
        let now = self.agent();
        match (before, &now) {
            (None, None) => false,
            (Some(a), Some(b)) => {
                (a.state, a.evidence, &a.agent, &a.detail, a.since_ms) != (b.state, b.evidence, &b.agent, &b.detail, b.since_ms)
            }
            _ => true,
        }
    }

    fn ensure_agent(&mut self, name: &str, evidence: Evidence, now: Instant, now_ms: u64) {
        match self.agent.as_mut() {
            Some(a) => {
                // A source at least as strong as any before may name it.
                if !name.is_empty() && (a.agent.is_empty() || rank(evidence) <= rank(a.strongest)) {
                    a.agent = name.to_owned();
                }
                if rank(evidence) < rank(a.strongest) {
                    a.strongest = evidence;
                }
            }
            None => {
                self.agent = Some(Tracked {
                    agent: name.to_owned(),
                    state: AgentState::Idle,
                    evidence,
                    since_ms: now_ms,
                    detail: String::new(),
                    last_signal: now,
                    last_signal_ms: now_ms,
                    strongest: evidence,
                    before_unverified: None,
                });
                self.line.clear();
            }
        }
    }

    /// Applies a reported state unless a stronger source owns the agent.
    fn set(&mut self, name: &str, state: AgentState, evidence: Evidence, detail: String, now: Instant, now_ms: u64) {
        self.ensure_agent(name, evidence, now, now_ms);
        let a = self.agent.as_mut().expect("ensured");
        a.last_signal = now;
        a.last_signal_ms = now_ms;
        if rank(evidence) > rank(a.strongest) {
            return;
        }
        a.before_unverified = None;
        if a.state != state || a.evidence != evidence {
            a.since_ms = now_ms;
        }
        a.state = state;
        a.evidence = evidence;
        a.detail = detail;
    }

    /// Applies a state whatever source owns the agent (interrupts).
    fn set_forced(&mut self, state: AgentState, evidence: Evidence, detail: &str, now: Instant, now_ms: u64) {
        let Some(a) = self.agent.as_mut() else { return };
        a.before_unverified = None;
        a.state = state;
        a.evidence = evidence;
        a.since_ms = now_ms;
        a.detail = detail.to_owned();
        a.last_signal = now;
        a.last_signal_ms = now_ms;
    }

    fn end(&mut self) {
        self.agent = None;
        self.line.clear();
        self.line_dirty = false;
    }

    /// Title heuristics: a braille spinner in front means working; Claude
    /// Code's `✳` means idle. Only for agents without stronger evidence.
    fn on_title(&mut self, title: &str, now: Instant, now_ms: u64) {
        let first = title.trim_start().chars().next();
        let working = first.is_some_and(|c| ('\u{2800}'..='\u{28ff}').contains(&c) && c != '\u{2800}');
        let idle = first == Some('✳');
        let lower = title.to_lowercase();
        let named = KNOWN_AGENTS.iter().find(|a| lower.contains(*a)).copied().or_else(|| lower.contains("claude code").then_some("claude"));
        if !(working || idle) || (self.agent.is_none() && named.is_none()) {
            // A spinner alone does not prove an agent: builds draw them too.
            return;
        }
        let name = named.unwrap_or("");
        if working {
            self.set(name, AgentState::Working, Evidence::Heuristic, String::new(), now, now_ms);
        } else if idle {
            self.set(name, AgentState::Idle, Evidence::Heuristic, String::new(), now, now_ms);
        }
    }

    /// Approval prompts as the agents print them. A guess: only used while
    /// nothing stronger reports.
    fn prompt_patterns(&mut self, data: &[u8], now: Instant, now_ms: u64) {
        const PATTERNS: &[&[u8]] = &[
            b"Do you want to proceed?",
            b"Do you want to make this edit",
            b"Do you want to create ",
            b"Would you like to run the following command",
            b"Would you like to make the following edits",
            b"Allow execution of",
            b"Apply this change?",
            b"Waiting for user confirmation",
        ];
        if memchr::memchr(b'?', data).is_none() && memchr::memmem::find(data, b"Allow").is_none() && memchr::memmem::find(data, b"Waiting").is_none() {
            return;
        }
        if PATTERNS.iter().any(|p| memchr::memmem::find(data, p).is_some()) {
            self.set("", AgentState::NeedsYou(NeedKind::Permission), Evidence::Heuristic, String::new(), now, now_ms);
        }
    }
}

enum Found {
    Report(super::osc::Report),
    Title(String),
    Prompt,
}

fn rank(evidence: Evidence) -> u8 {
    match evidence {
        Evidence::Protocol => 0,
        Evidence::Hook => 1,
        Evidence::Osc => 2,
        Evidence::Heuristic => 3,
    }
}

/// The agent a typed command line starts, if any: `claude`, `codex -m x`,
/// `FOO=1 gemini`, `npx @openai/codex`, `C:\bin\claude.exe`.
pub fn agent_command(line: &str) -> Option<&'static str> {
    let mut words = line.split_whitespace().skip_while(|w| w.contains('=') && !w.starts_with('-'));
    let mut word = words.next()?;
    if matches!(word, "npx" | "bunx" | "pnpx" | "uvx" | "&" | "exec" | "command") {
        word = words.find(|w| !w.starts_with('-'))?;
    } else if matches!(word, "pnpm" | "yarn" | "npm") {
        let sub = words.next()?;
        if !matches!(sub, "dlx" | "exec" | "x") {
            return None;
        }
        word = words.find(|w| !w.starts_with('-'))?;
    }
    let word = word.trim_matches(|c| c == '"' || c == '\'');
    let package = match word {
        w if w.starts_with("@anthropic-ai/claude-code") => Some("claude"),
        w if w.starts_with("@openai/codex") => Some("codex"),
        w if w.starts_with("@google/gemini-cli") => Some("gemini"),
        w if w.starts_with("opencode-ai") => Some("opencode"),
        _ => None,
    };
    if package.is_some() {
        return package;
    }
    let base = word.rsplit(['/', '\\']).next()?;
    let lower = base.to_ascii_lowercase();
    let stem = [".exe", ".cmd", ".ps1", ".bat"].iter().find_map(|ext| lower.strip_suffix(ext)).unwrap_or(&lower);
    KNOWN_AGENTS.iter().find(|a| **a == stem).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64, base: Instant) -> (Instant, u64) {
        (base + Duration::from_secs(secs), 1_000_000 + secs * 1000)
    }

    fn state(w: &TermWatch) -> Option<(AgentState, Evidence)> {
        w.agent().map(|a| (a.state, a.evidence))
    }

    #[test]
    fn typed_commands_name_agents() {
        assert_eq!(agent_command("claude"), Some("claude"));
        assert_eq!(agent_command("  codex -m gpt-5 "), Some("codex"));
        assert_eq!(agent_command("FOO=1 BAR=2 gemini --yolo"), Some("gemini"));
        assert_eq!(agent_command("npx -y @openai/codex@latest"), Some("codex"));
        assert_eq!(agent_command("pnpm dlx @anthropic-ai/claude-code"), Some("claude"));
        assert_eq!(agent_command(r"C:\Users\me\.local\bin\claude.exe --resume"), Some("claude"));
        assert_eq!(agent_command("& 'C:/tools/opencode.cmd'"), Some("opencode"));
        assert_eq!(agent_command("cargo test"), None);
        assert_eq!(agent_command("echo claude"), None);
        assert_eq!(agent_command(""), None);
    }

    #[test]
    fn a_typed_agent_is_a_guess_until_it_reports() {
        let base = Instant::now();
        let mut w = TermWatch::new(TermId(1), "t".into(), None);
        let (t, ms) = at(0, base);
        // Typed with a correction; history recall would make it unknown.
        assert!(!w.on_input(b"clx\x7faude", t, ms));
        assert!(w.on_input(b"\r", t, ms));
        assert_eq!(state(&w), Some((AgentState::Idle, Evidence::Heuristic)));
        assert_eq!(w.agent().unwrap().agent, "claude");
        // A prompt submitted to it: probably working.
        assert!(w.on_input(b"fix the bug\r", t, ms));
        assert_eq!(state(&w), Some((AgentState::Working, Evidence::Heuristic)));
        // Its approval prompt, as printed.
        let (t, ms) = at(5, base);
        assert!(w.on_output(b"\x1b[1m Do you want to proceed?\x1b[0m\r\n \xe2\x9d\xaf 1. Yes", t, ms));
        assert_eq!(state(&w), Some((AgentState::NeedsYou(NeedKind::Permission), Evidence::Heuristic)));
        // Hooks take over and the guesses stop.
        let call = HookCall { agent: "claude".into(), event: "PreToolUse".into(), tool: "Bash".into(), ..Default::default() };
        assert!(w.on_hook(&call, t, ms));
        assert_eq!(state(&w), Some((AgentState::Working, Evidence::Hook)));
        w.on_output(b"Do you want to proceed?", t, ms);
        assert_eq!(state(&w), Some((AgentState::Working, Evidence::Hook)));
    }

    #[test]
    fn history_recall_is_not_trusted() {
        let base = Instant::now();
        let mut w = TermWatch::new(TermId(1), "t".into(), None);
        let (t, ms) = at(0, base);
        assert!(!w.on_input(b"\x1b[A\r", t, ms));
        assert!(w.agent().is_none());
        assert!(!w.on_input(b"cla\tude\r", t, ms));
        assert!(w.agent().is_none());
    }

    #[test]
    fn silence_while_working_becomes_unverified_and_output_restores_it() {
        let base = Instant::now();
        let mut w = TermWatch::new(TermId(1), "t".into(), None);
        let (t, ms) = at(0, base);
        let call = HookCall { agent: "claude".into(), event: "UserPromptSubmit".into(), ..Default::default() };
        w.on_hook(&call, t, ms);
        let (t, ms) = at(119, base);
        assert!(!w.tick(t, ms));
        let (t, ms) = at(121, base);
        assert!(w.tick(t, ms));
        assert_eq!(state(&w), Some((AgentState::Unverified, Evidence::Hook)));
        // Done states never age: a finished agent stays finished.
        let (t, ms) = at(122, base);
        assert!(w.on_output(b"\x1b[2K spinner", t, ms));
        assert_eq!(state(&w), Some((AgentState::Working, Evidence::Hook)));
        let stop = HookCall { agent: "claude".into(), event: "Stop".into(), ..Default::default() };
        w.on_hook(&stop, t, ms);
        let (t, ms) = at(10_000, base);
        assert!(!w.tick(t, ms));
        assert_eq!(state(&w), Some((AgentState::Idle, Evidence::Hook)));
    }

    #[test]
    fn interrupts_and_the_shell_prompt_end_working() {
        let base = Instant::now();
        let mut w = TermWatch::new(TermId(1), "t".into(), None);
        let (t, ms) = at(0, base);
        w.on_hook(&HookCall { agent: "claude".into(), event: "UserPromptSubmit".into(), ..Default::default() }, t, ms);
        assert!(w.on_input(b"\x1b", t, ms));
        assert_eq!(state(&w), Some((AgentState::Idle, Evidence::Heuristic)));
        assert_eq!(w.agent().unwrap().detail, "stopped");
        // OSC 133;A: the shell shows its prompt, the agent is gone.
        assert!(w.on_output(b"\x1b]133;A\x07$ ", t, ms));
        assert!(w.agent().is_none());
    }

    #[test]
    fn osc_reports_rank_between_hooks_and_guesses() {
        let base = Instant::now();
        let mut w = TermWatch::new(TermId(1), "t".into(), None);
        let (t, ms) = at(0, base);
        assert!(!w.on_output(b"\x1b]0;\xe2\xa0\x8b Building\x07", t, ms), "a spinner alone is no agent");
        assert!(w.on_output(b"\x1b]0;\xe2\xa0\x8b Gemini - working\x07", t, ms));
        assert_eq!(state(&w), Some((AgentState::Working, Evidence::Heuristic)));
        assert!(w.on_output(b"\x1b]7501;state=blocked;kind=question;agent=pi;msg=Which file?\x07", t, ms));
        assert_eq!(state(&w), Some((AgentState::NeedsYou(NeedKind::Question), Evidence::Osc)));
        assert_eq!(w.agent().unwrap().agent, "pi");
        // A title guess cannot override the report.
        w.on_output(b"\x1b]0;\xe2\x9c\xb3 idle\x07", t, ms);
        assert_eq!(state(&w), Some((AgentState::NeedsYou(NeedKind::Question), Evidence::Osc)));
        assert!(w.on_output(b"\x1b]7501;state=exited\x07", t, ms));
        assert!(w.agent().is_none());
    }

    #[test]
    fn hook_events_map_to_states() {
        let call = |event: &str, kind: &str, message: &str| HookCall {
            agent: "claude".into(),
            event: event.into(),
            kind: kind.into(),
            message: message.into(),
            tool: "Bash".into(),
        };
        let state = |c: HookCall| match hook_effect(&c) {
            HookEffect::State { state, .. } => Some(state),
            _ => None,
        };
        assert_eq!(state(call("PermissionRequest", "", "")), Some(AgentState::NeedsYou(NeedKind::Permission)));
        assert_eq!(state(call("Notification", "permission_prompt", "")), Some(AgentState::NeedsYou(NeedKind::Permission)));
        assert_eq!(state(call("Notification", "idle_prompt", "")), Some(AgentState::Idle));
        assert_eq!(
            state(call("Notification", "", "Claude needs your permission to use Bash")),
            Some(AgentState::NeedsYou(NeedKind::Permission))
        );
        assert_eq!(state(call("Stop", "", "")), Some(AgentState::Idle));
        assert_eq!(state(call("agent-turn-complete", "", "")), Some(AgentState::Idle));
        assert_eq!(state(call("StopFailure", "", "Rate limit reached")), Some(AgentState::RateLimited));
        assert_eq!(state(call("StopFailure", "", "API error")), Some(AgentState::Failed));
        assert_eq!(hook_effect(&call("SessionEnd", "", "")), HookEffect::Ended);
        assert_eq!(hook_effect(&call("SubagentStart", "", "")), HookEffect::Alive);
        assert_eq!(state(call("working", "", "")), Some(AgentState::Working));
    }
}
