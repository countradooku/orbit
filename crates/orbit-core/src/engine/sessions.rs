//! Provider instances and agent sessions: which instance a thread runs on,
//! probing every instance, switching instance mid-thread with a portable
//! handoff, and starting a fresh session in the same thread (`/clear`).

use crossfire::BlockingTxTrait;
use orbit_proto::*;

use super::orchestrator::{CommandResult, Internal, Orchestrator};
use crate::provider::{self, Launch, SessionInput, presets};

/// The handoff summary's budget, in estimated tokens (4 bytes each).
pub const HANDOFF_TOKENS: usize = 6_000;
/// Turns at the start and end of a thread that are always kept in full
/// when the budget allows.
const HANDOFF_RECENT_TURNS: usize = 3;
/// Per-message cut-offs inside the summary.
const USER_CHARS: usize = 2_000;
const ASSISTANT_CHARS: usize = 3_000;
const CONDENSED_CHARS: usize = 160;

/// Typed commands that start a fresh session instead of reaching the agent
/// (T3 Code passed `/clear` through and kept the old session, #24152).
pub const CLEAR_COMMANDS: &[&str] = &["/clear", "/new", "/reset"];

impl Orchestrator {
    // ── Instances ────────────────────────────────────────────────────────

    /// The instance a thread config runs on. A removed or disabled instance
    /// falls back to the built-in one of the same runtime.
    pub(crate) fn instance_of(&self, config: &ThreadConfig) -> ProviderInstance {
        let id = config.instance_id();
        if id == config.provider.id() {
            return ProviderInstance::builtin(config.provider);
        }
        presets::find(&self.store.settings().agents.instances, id)
            .filter(|i| i.kind == config.provider)
            .unwrap_or_else(|| ProviderInstance::builtin(config.provider))
    }

    /// How to start `instance` under the current settings.
    pub(crate) fn launch_of(&self, instance: &ProviderInstance) -> Launch {
        Launch::of(instance, &self.store.settings().provider(instance.kind))
    }

    /// The provider list before any probe answered: the instances that are
    /// always listed, marked as being checked.
    pub(crate) fn initial_providers(settings: &Settings) -> Vec<ProviderInfo> {
        presets::effective(&settings.agents.instances)
            .into_iter()
            .filter(|(_, listed)| *listed)
            .map(|(i, _)| ProviderInfo {
                kind: i.kind,
                instance: i.id.clone(),
                name: i.display_name().to_owned(),
                message: Some("Checking…".into()),
                ..Default::default()
            })
            .collect()
    }

    /// Probes every instance in parallel; each probe has its own timeout and
    /// a missing binary answers at once, so nothing waits on a slow agent.
    pub fn refresh_providers(&self) {
        for (instance, _) in presets::effective(&self.store.settings().agents.instances) {
            self.probe_instance(&instance);
        }
    }

    /// Probes the instances of `kind` (its settings changed).
    pub(crate) fn probe_provider(&self, kind: ProviderKind) {
        for (instance, _) in presets::effective(&self.store.settings().agents.instances) {
            if instance.kind == kind {
                self.probe_instance(&instance);
            }
        }
    }

    pub(crate) fn probe_instance(&self, instance: &ProviderInstance) {
        let launch = self.launch_of(instance);
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let info = provider::probe(launch).await;
            let _ = tx.send(Internal::Probed(info));
        });
    }

    /// Applies a probe or catalog result. Instances that are only listed
    /// when found are dropped while not installed, and so are results for
    /// instances that no longer exist.
    pub(crate) fn set_provider(&mut self, mut info: ProviderInfo) {
        let effective = presets::effective(&self.store.settings().agents.instances);
        let Some((instance, listed)) = effective.iter().find(|(i, _)| i.id == info.instance_id()) else {
            let before = self.providers.len();
            self.providers.retain(|p| p.instance_id() != info.instance_id());
            if self.providers.len() != before {
                self.emit(Event::Providers(self.providers.clone()));
            }
            return;
        };
        info.instance = instance.id.clone();
        info.name = instance.display_name().to_owned();
        let slot = self.providers.iter().position(|p| p.instance_id() == info.instance_id());
        match slot {
            _ if !info.installed && !listed => {
                if let Some(ix) = slot {
                    self.providers.remove(ix);
                } else {
                    return;
                }
            }
            Some(ix) => {
                if info.quota.is_empty() {
                    info.quota = std::mem::take(&mut self.providers[ix].quota);
                }
                if info.version.is_none() {
                    info.version = self.providers[ix].version.take();
                }
                self.providers[ix] = info;
            }
            None => {
                // Keep the presets' order.
                let rank = |id: &str| effective.iter().position(|(i, _)| i.id == id).unwrap_or(usize::MAX);
                let at = self.providers.iter().position(|p| rank(p.instance_id()) > rank(info.instance_id()));
                match at {
                    Some(at) => self.providers.insert(at, info),
                    None => self.providers.push(info),
                }
            }
        }
        self.emit(Event::Providers(self.providers.clone()));
    }

    /// The thread's provider entry (for catalog and quota updates from its
    /// session).
    pub(crate) fn tag_with_instance(&self, thread: ThreadId, mut info: ProviderInfo) -> ProviderInfo {
        if let Some(t) = self.store.thread(thread) {
            let instance = self.instance_of(&t.config);
            info.kind = instance.kind;
            info.instance = instance.id;
        }
        info
    }

    // ── Switching instance ───────────────────────────────────────────────

    /// A thread's config changed from `old` to `new`: apply it to the live
    /// session, or end it and hand the conversation off when the instance
    /// changed.
    pub(crate) fn apply_config_change(&mut self, thread: ThreadId, old: &ThreadConfig, new: &ThreadConfig) {
        if old.same_instance(new) {
            if let Some(session) = self.runtimes.get(&thread).and_then(|r| r.session.as_ref()) {
                session.send(SessionInput::Configure(new.clone()));
            }
            return;
        }
        let (from, to) = (self.instance_of(old), self.instance_of(new));
        if let Some(session) = self.runtimes.get_mut(&thread).and_then(|r| r.session.take()) {
            session.send(SessionInput::Shutdown);
        }
        if from.shares_sessions_with(&to) {
            // Same runtime and configuration (another name or variables):
            // the next process resumes the same session.
            return;
        }
        let had_history = self.store.thread(thread).is_some_and(|t| t.turns > 0);
        self.update_thread(thread, |t| t.session = None);
        if let Some(runtime) = self.runtimes.get_mut(&thread) {
            runtime.resume_at = None;
        }
        if !had_history {
            return;
        }
        let items = match self.store.load_timeline(thread) {
            Ok(timeline) => timeline.items().to_vec(),
            Err(_) => return,
        };
        let from_name = self.display_name(&from);
        let to_name = self.display_name(&to);
        let handoff = build_handoff(&items, &from_name, &to_name, HANDOFF_TOKENS);
        let turn = self.store.thread(thread).map(|t| t.turns).unwrap_or(0);
        self.runtimes.entry(thread).or_default().handoff = Some(handoff.context.clone());
        self.put_item(thread, Item { id: ItemId::new(), turn, status: ItemStatus::Done, body: ItemBody::Handoff(handoff) });
    }

    fn display_name(&self, instance: &ProviderInstance) -> String {
        self.providers
            .iter()
            .find(|p| p.instance_id() == instance.id)
            .map(|p| p.display_name().to_owned())
            .unwrap_or_else(|| instance.display_name().to_owned())
    }

    /// Prepends a pending handoff to the first prompt of the new session.
    pub(crate) fn attach_handoff(&mut self, thread: ThreadId, prompt: &mut SessionInput) {
        let Some(context) = self.runtimes.get_mut(&thread).and_then(|r| r.handoff.take()) else { return };
        if let SessionInput::Prompt { text, steer: false, .. } = prompt {
            *text = format!("{context}\n\n---\n\nThe user's new message:\n\n{text}");
        }
    }

    // ── Fresh sessions ───────────────────────────────────────────────────

    /// Ends the thread's agent session; the next message starts a fresh one
    /// in the same thread. Pending handoffs and rollback cursors are
    /// dropped too, so nothing from before reaches the new session.
    pub(crate) fn new_session(&mut self, thread: ThreadId) -> CommandResult {
        let t = self.store.thread(thread).ok_or("That thread no longer exists.")?;
        if t.archived {
            return Err("This thread is archived. Unarchive it to continue.".into());
        }
        if self.runtimes.get(&thread).is_some_and(|r| r.turn.is_some() || r.checkpointing || r.held_prompt.is_some()) {
            return Err("Stop the current turn first, then start a new session.".into());
        }
        let had_session = t.session.is_some() || self.runtimes.get(&thread).is_some_and(|r| r.session.is_some());
        if let Some(runtime) = self.runtimes.get_mut(&thread) {
            if let Some(session) = runtime.session.take() {
                session.send(SessionInput::Shutdown);
            }
            runtime.resume_at = None;
            runtime.handoff = None;
        }
        self.update_thread(thread, |t| t.session = None);
        if had_session || self.store.thread(thread).is_some_and(|t| t.turns > 0) {
            self.notice(
                thread,
                NoticeLevel::Info,
                "New session: the agent starts fresh and no longer sees the messages above.".into(),
            );
        }
        Ok(Reply::Done)
    }

    /// `/clear` and its aliases, typed alone, start a fresh session instead
    /// of reaching the agent. Returns the command's result when it was one.
    pub(crate) fn intercept_clear(&mut self, thread: ThreadId, text: &str, attachments: &[Attachment]) -> Option<CommandResult> {
        let word = text.trim();
        (attachments.is_empty() && CLEAR_COMMANDS.iter().any(|c| word.eq_ignore_ascii_case(c))).then(|| self.new_session(thread))
    }
}

// ── The handoff summary ──────────────────────────────────────────────────

/// One turn, rendered for the summary.
struct TurnText {
    number: u32,
    user: String,
    assistant: String,
    actions: Vec<String>,
    files: Vec<String>,
}

impl TurnText {
    fn full(&self) -> String {
        let mut out = format!("### Turn {}\n", self.number);
        if !self.user.is_empty() {
            out.push_str(&format!("User: {}\n", clip(&self.user, USER_CHARS)));
        }
        if !self.actions.is_empty() {
            out.push_str(&format!("Agent actions: {}\n", self.actions.join("; ")));
        }
        if !self.files.is_empty() {
            out.push_str(&format!("Files changed: {}\n", self.files.join(", ")));
        }
        if !self.assistant.is_empty() {
            out.push_str(&format!("Agent: {}\n", clip(&self.assistant, ASSISTANT_CHARS)));
        }
        out
    }

    fn condensed(&self) -> String {
        let mut line = format!("- Turn {}: {}", self.number, clip(&one_line(&self.user), CONDENSED_CHARS));
        if !self.files.is_empty() {
            let files: Vec<&str> = self.files.iter().take(6).map(String::as_str).collect();
            line.push_str(&format!(" (changed {}{})", files.join(", "), if self.files.len() > 6 { ", …" } else { "" }));
        }
        line.push('\n');
        line
    }
}

/// Builds the context a new agent receives when a thread moves to it: the
/// original request and the most recent turns in full, the turns between
/// them condensed to one line each, all within `budget_tokens`. Pure and
/// deterministic: no model call, no quota, instant.
pub fn build_handoff(items: &[Item], from: &str, to: &str, budget_tokens: usize) -> Handoff {
    let turns = collect_turns(items);
    let budget = budget_tokens * 4;
    let header = format!(
        "[Context handed off from {from}]\n\
         You are continuing a conversation that began with another coding agent ({from}) in this same \
         workspace. Its changes are already on disk; read files rather than trusting this summary for \
         their contents. The conversation so far, oldest first:\n\n"
    );
    let footer = "\n[End of handed-off context]";
    // Headings and the "left out" note.
    let mut used = header.len() + footer.len() + 96;

    // Choose turns from the newest backwards, plus the first one.
    let n = turns.len();
    let mut full = vec![false; n];
    let first = turns.first().map(|t| t.full().len() + 1).unwrap_or_default();
    if n > 0 && used + first <= budget {
        full[0] = true;
        used += first;
    }
    for ix in (1..n).rev() {
        let len = turns[ix].full().len() + 1;
        let recent = n - ix <= HANDOFF_RECENT_TURNS;
        if used + len <= budget && (recent || used + len <= budget * 3 / 4) {
            full[ix] = true;
            used += len;
        } else if !recent {
            break;
        }
    }
    let mut condensed = vec![false; n];
    for ix in 0..n {
        if !full[ix] {
            let line = turns[ix].condensed();
            if used + line.len() <= budget {
                condensed[ix] = true;
                used += line.len();
            }
        }
    }
    let mut body = String::with_capacity(used);
    let mut omitted = 0;
    let mut in_condensed = false;
    for ix in 0..n {
        if full[ix] {
            if in_condensed {
                body.push('\n');
                in_condensed = false;
            }
            body.push_str(&turns[ix].full());
            body.push('\n');
        } else if condensed[ix] {
            if !in_condensed {
                body.push_str("Earlier turns, condensed:\n");
                in_condensed = true;
            }
            body.push_str(&turns[ix].condensed());
        } else {
            omitted += 1;
        }
    }
    if omitted > 0 {
        body.push_str(&format!("\n({omitted} more turns left out to keep this short.)\n"));
    }
    let context = format!("{header}{}{footer}", body.trim_end());
    Handoff {
        from: from.to_owned(),
        to: to.to_owned(),
        turns: full.iter().filter(|f| **f).count() as u32,
        condensed: (n - full.iter().filter(|f| **f).count()) as u32,
        tokens: context.len().div_ceil(4) as u32,
        context,
    }
}

fn collect_turns(items: &[Item]) -> Vec<TurnText> {
    let mut turns: Vec<TurnText> = Vec::new();
    for item in items {
        if item.status == ItemStatus::Queued || item.turn == 0 {
            continue;
        }
        if turns.last().is_none_or(|t| t.number != item.turn) {
            turns.push(TurnText { number: item.turn, user: String::new(), assistant: String::new(), actions: Vec::new(), files: Vec::new() });
        }
        let turn = turns.last_mut().unwrap();
        match &item.body {
            ItemBody::User(m) => {
                if !turn.user.is_empty() {
                    turn.user.push_str("\n\n");
                }
                turn.user.push_str(m.text.trim());
                for a in &m.attachments {
                    turn.user.push_str(&format!("\n[attached {}]", a.name));
                }
            }
            ItemBody::Assistant { text } if !text.trim().is_empty() => {
                // The last reply of the turn is its conclusion.
                turn.assistant = text.trim().to_owned();
            }
            ItemBody::Tool(call) => {
                let summary = match call.kind {
                    ToolKind::Command => Some(format!("ran `{}`", clip(&one_line(&call.title), 120))),
                    ToolKind::Task => Some(format!("delegated \"{}\"", clip(&one_line(&call.title), 80))),
                    ToolKind::Web => Some(format!("looked up {}", clip(&one_line(&call.title), 80))),
                    _ => None,
                };
                if let Some(summary) = summary
                    && turn.actions.len() < 8
                {
                    turn.actions.push(summary);
                }
                for f in &call.files {
                    if !turn.files.contains(&f.path) {
                        turn.files.push(f.path.clone());
                    }
                }
            }
            ItemBody::Plan { steps } => {
                let open = steps.iter().filter(|s| s.state != StepState::Done).count();
                turn.actions.push(format!("plan of {} steps ({open} open)", steps.len()));
            }
            ItemBody::TurnEnd(summary) => {
                for f in &summary.files {
                    if !turn.files.contains(&f.path) {
                        turn.files.push(f.path.clone());
                    }
                }
            }
            ItemBody::Request(r) => {
                if let (RequestKind::Question { question, .. }, Some(Decision::Answer { text })) = (&r.kind, &r.resolution) {
                    turn.actions.push(format!("asked \"{}\" (answer: {})", clip(&one_line(question), 80), clip(&one_line(text), 80)));
                }
            }
            _ => {}
        }
    }
    turns
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(turn: u32, body: ItemBody) -> Item {
        Item { id: ItemId::new(), turn, status: ItemStatus::Done, body }
    }

    fn user(turn: u32, text: &str) -> Item {
        item(turn, ItemBody::User(UserMessage { text: text.into(), attachments: vec![], steer: false }))
    }

    fn reply(turn: u32, text: &str) -> Item {
        item(turn, ItemBody::Assistant { text: text.into() })
    }

    #[test]
    fn a_short_thread_hands_off_in_full() {
        let items = vec![
            user(1, "Add a login form"),
            item(1, ItemBody::Tool(ToolCall {
                call_id: "c".into(),
                kind: ToolKind::Command,
                name: "bash".into(),
                title: "npm test".into(),
                input: String::new(),
                output: String::new(),
                output_blob: None,
                exit_code: Some(0),
                files: vec![FileChange { path: "src/login.tsx".into(), kind: ChangeKind::Added, additions: 40, deletions: 0 }],
            })),
            reply(1, "Added the form and its tests."),
            user(2, "Now validate the email"),
            reply(2, "Done: emails are validated."),
        ];
        let handoff = build_handoff(&items, "Claude Code", "Codex", HANDOFF_TOKENS);
        assert_eq!((handoff.turns, handoff.condensed), (2, 0));
        assert!(handoff.context.contains("Add a login form"));
        assert!(handoff.context.contains("ran `npm test`"));
        assert!(handoff.context.contains("src/login.tsx"));
        assert!(handoff.context.contains("emails are validated"));
        assert!(handoff.context.starts_with("[Context handed off from Claude Code]"));
        assert_eq!(handoff.tokens as usize, handoff.context.len().div_ceil(4));
    }

    #[test]
    fn a_long_thread_stays_within_budget_and_keeps_both_ends() {
        let mut items = Vec::new();
        for turn in 1..=200 {
            items.push(user(turn, &format!("Request number {turn}: {}", "details ".repeat(60))));
            items.push(reply(turn, &format!("Answer {turn}: {}", "explanation ".repeat(120))));
        }
        let handoff = build_handoff(&items, "Codex", "Gemini CLI", 4_000);
        assert!(handoff.context.len() <= 4_000 * 4, "{} bytes", handoff.context.len());
        assert!(handoff.context.contains("Request number 1:"), "the original request is kept");
        assert!(handoff.context.contains("Answer 200:"), "the latest turn is kept");
        assert!(handoff.condensed > 0);
        assert!(handoff.turns >= 2);
    }

    #[test]
    fn clipping_respects_char_boundaries() {
        assert_eq!(clip("héllo", 2), "h…");
        assert_eq!(clip("short", 10), "short");
    }
}
