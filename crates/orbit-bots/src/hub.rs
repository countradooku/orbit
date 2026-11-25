//! The hub: everything platform-independent.
//!
//! It is an ordinary engine client. It follows the engine's events (thread
//! statuses, requests, timeline items) and turns them into chat messages;
//! it turns chat messages and button presses into engine commands, always
//! [`Command::Via`] so the timeline says where they came from and the guard
//! can keep its approvals for the app. It owns the transports, starting and
//! stopping them as settings and tokens change.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossfire::oneshot::TxOneshot;
use crossfire::{MTx, mpsc};
use orbit_core::EngineHandle;
use orbit_net::state::ClientState;
use orbit_net::Update;
use orbit_proto::*;

use crate::chat::*;
use crate::store::{ChatKey, Paired, Store};
use crate::transport::Ctx;

/// Pairing codes expire after this long.
const PAIRING_TTL_MS: u64 = 15 * 60 * 1000;
/// At most this many people wait to pair at once.
const PAIRING_MAX: usize = 20;
/// Inbound messages per person: a burst of this many…
const BURST: f64 = 8.0;
/// …refilled at one every this many seconds (20 a minute).
const REFILL_SECS: f64 = 3.0;
/// Status lines are edited at most this often.
const STATUS_EVERY: Duration = Duration::from_secs(4);
/// A finished turn waits at most this long for its file list.
const END_WAIT: Duration = Duration::from_secs(3);

struct Running {
    task: tokio::task::JoinHandle<()>,
    out: MTx<mpsc::List<Out>>,
    tokens: (String, String),
}

struct Bucket {
    tokens: f64,
    at: Instant,
    warned: Option<Instant>,
}

/// A turn a chat is watching.
#[derive(Default)]
struct Track {
    started_ms: u64,
    /// What the agent is doing now ("editing src/main.rs").
    doing: Option<(ItemId, String)>,
    /// The latest answer text of the turn.
    answer: Option<String>,
    last_status: String,
    last_edit: Option<Instant>,
    /// The turn ended; its summary waits for the file list.
    ended: Option<(TurnSummary, Instant)>,
}

/// A request's message, to edit once it is answered.
struct Asked {
    thread: ThreadId,
    msg: Msg,
    sent: Vec<(ChatKey, Option<String>)>,
}

pub struct Hub {
    engine: EngineHandle,
    pub state: ClientState,
    store: Store,
    secrets: BotSecrets,
    endpoints: Endpoints,
    inbound: MTx<mpsc::List<Incoming>>,
    reports: MTx<mpsc::List<Report>>,
    running: HashMap<Platform, Running>,
    platforms: HashMap<Platform, PlatformState>,
    pairing: Vec<(PairingRequest, ChatKey)>,
    buckets: HashMap<(Platform, String), Bucket>,
    app_active: bool,
    guard: GuardSettings,
    tracks: HashMap<ThreadId, Track>,
    status_ids: HashMap<(ThreadId, ChatKey), String>,
    /// Final status lines for status messages whose id hasn't come back
    /// yet (a turn that ended before its status line was delivered).
    closing: HashMap<(ThreadId, ChatKey), String>,
    known: HashSet<RequestId>,
    asked: HashMap<RequestId, Asked>,
    /// A first message waiting for its project to be picked.
    waiting_prompt: HashMap<ChatKey, String>,
}

impl Hub {
    pub fn new(
        engine: EngineHandle,
        store: Store,
        endpoints: Endpoints,
        inbound: MTx<mpsc::List<Incoming>>,
        reports: MTx<mpsc::List<Report>>,
    ) -> Self {
        Self {
            engine,
            state: ClientState::default(),
            store,
            secrets: BotSecrets::default(),
            endpoints,
            inbound,
            reports,
            running: HashMap::new(),
            platforms: Platform::ALL.into_iter().map(|p| (p, PlatformState::new(p))).collect(),
            pairing: Vec::new(),
            buckets: HashMap::new(),
            app_active: false,
            guard: GuardSettings::default(),
            tracks: HashMap::new(),
            status_ids: HashMap::new(),
            closing: HashMap::new(),
            known: HashSet::new(),
            asked: HashMap::new(),
            waiting_prompt: HashMap::new(),
        }
    }

    async fn call(&self, command: Command) -> Result<Reply, String> {
        self.engine.call(command).await.unwrap_or_else(|_| Err("Orbit has stopped.".into()))
    }

    async fn via(&self, who: &Incoming, action: ViaAction) -> Result<Reply, String> {
        let via = Via { channel: who.chat.platform.label().to_owned(), user: who.user_name.clone() };
        self.call(Command::Via { via, action }).await
    }

    // ── State for the settings page ──────────────────────────────────────

    pub fn bots_state(&self) -> BotsState {
        let platforms = Platform::ALL
            .into_iter()
            .map(|p| {
                let mut s = self.platforms.get(&p).cloned().unwrap_or_else(|| PlatformState::new(p));
                s.enabled = self.store.data.enabled(p);
                s.token_hint = token_hint(self.secrets.token(p));
                s.app_token_hint = token_hint(self.secrets.app_token(p));
                s
            })
            .collect();
        BotsState {
            platforms,
            paired: self.store.data.paired.iter().map(|p| p.user.clone()).collect(),
            pending: self.pairing.iter().map(|(r, _)| r.clone()).collect(),
            options: self.store.data.options.clone(),
        }
    }

    fn publish(&self) {
        self.engine.publish_bots(self.bots_state());
    }

    // ── Transports ───────────────────────────────────────────────────────

    /// Starts, restarts or stops each platform to match the settings and
    /// tokens.
    pub fn reconcile(&mut self) {
        for platform in Platform::ALL {
            let tokens = (self.secrets.token(platform).to_owned(), self.secrets.app_token(platform).to_owned());
            let wanted = self.store.data.enabled(platform)
                && !tokens.0.is_empty()
                && (platform != Platform::Slack || !tokens.1.is_empty());
            let same = self.running.get(&platform).is_some_and(|r| r.tokens == tokens && !r.task.is_finished());
            if wanted && same {
                continue;
            }
            if let Some(old) = self.running.remove(&platform) {
                old.task.abort();
            }
            let state = self.platforms.entry(platform).or_insert_with(|| PlatformState::new(platform));
            if !wanted {
                state.connection = if self.store.data.enabled(platform) { BotConnection::NeedsToken } else { BotConnection::Off };
                state.account.clear();
                continue;
            }
            state.connection = BotConnection::Connecting;
            let (out, out_rx) = mpsc::unbounded_async::<Out>();
            let ctx = Ctx {
                platform,
                endpoints: self.endpoints.clone(),
                inbound: self.inbound.clone(),
                reports: self.reports.clone(),
            };
            let (token, app) = tokens.clone();
            let task = match platform {
                Platform::Telegram => tokio::task::spawn_local(crate::telegram::run(ctx, out_rx, token)),
                Platform::Slack => tokio::task::spawn_local(crate::slack::run(ctx, out_rx, token, app)),
                Platform::Discord => tokio::task::spawn_local(crate::discord::run(ctx, out_rx, token)),
            };
            self.running.insert(platform, Running { task, out, tokens });
        }
    }

    pub fn stop_all(&mut self) {
        for (_, run) in self.running.drain() {
            run.task.abort();
        }
    }

    fn out(&self, job: Out) {
        let platform = match &job {
            Out::Send { chat, .. } | Out::Edit { chat, .. } | Out::File { chat, .. } => chat.platform,
        };
        if let Some(run) = self.running.get(&platform) {
            let _ = crossfire::BlockingTxTrait::send(&run.out, job);
        }
    }

    fn say(&self, chat: &ChatKey, msg: Msg) {
        self.out(Out::Send { chat: chat.clone(), msg, tag: None });
    }

    pub fn on_report(&mut self, report: Report) {
        match report {
            Report::Sent { tag: Tag::Status(thread), chat, id } => {
                let key = (thread, chat);
                match self.closing.remove(&key) {
                    Some(text) => self.out(Out::Edit { chat: key.1, id, msg: Msg::plain(text) }),
                    None => {
                        self.status_ids.insert(key, id);
                    }
                }
            }
            Report::Sent { tag: Tag::Request(request), chat, id } => {
                if let Some(asked) = self.asked.get_mut(&request)
                    && let Some(slot) = asked.sent.iter_mut().find(|(c, _)| *c == chat)
                {
                    slot.1 = Some(id);
                }
            }
            Report::Connection { platform, connection, account } => {
                let state = self.platforms.entry(platform).or_insert_with(|| PlatformState::new(platform));
                if let Some(account) = account {
                    state.account = account;
                }
                if state.connection != connection {
                    state.connection = connection;
                    self.publish();
                }
            }
            Report::Error { platform, error } => {
                let state = self.platforms.entry(platform).or_insert_with(|| PlatformState::new(platform));
                state.error = Some(error);
                state.error_ms = now_ms();
                self.publish();
            }
        }
    }

    // ── Commands from the engine (settings page, CLI) ────────────────────

    pub fn set_secrets(&mut self, secrets: BotSecrets) {
        self.secrets = secrets;
        for state in self.platforms.values_mut() {
            // A new token deserves a fresh start.
            if state.connection == BotConnection::Rejected {
                state.connection = BotConnection::Off;
            }
        }
        self.reconcile();
        self.publish();
    }

    pub fn on_bot_command(&mut self, command: BotCommand, reply: TxOneshot<Result<Reply, String>>) {
        let result = self.bot_command(command);
        if result.is_ok() {
            self.publish();
        }
        reply.send(result);
    }

    fn bot_command(&mut self, command: BotCommand) -> Result<Reply, String> {
        self.expire_pairings();
        match command {
            BotCommand::State => return Ok(Reply::Bots(BotReply::State(self.bots_state()))),
            BotCommand::Enable { platform, enabled } => {
                self.store.data.set_enabled(platform, enabled);
                self.store.save();
                if enabled && let Some(state) = self.platforms.get_mut(&platform) {
                    state.error = None;
                }
                self.reconcile();
            }
            BotCommand::SetTokens { .. } => return Err("Tokens are saved by the engine.".into()),
            BotCommand::SetOptions(mut options) => {
                if options.access == Access::Full {
                    options.access = Access::AutoEdit;
                }
                self.store.data.options = options;
                self.store.save();
            }
            BotCommand::Approve { code } => {
                let code = code.trim().to_owned();
                let ix = self
                    .pairing
                    .iter()
                    .position(|(r, _)| r.code == code)
                    .ok_or_else(|| format!("No one is waiting with code {code}. Codes expire after 15 minutes."))?;
                let (request, chat) = self.pairing.remove(ix);
                let user = PairedUser {
                    platform: request.platform,
                    user_id: request.user_id.clone(),
                    name: request.name.clone(),
                    paired_ms: now_ms(),
                };
                self.store.data.paired.retain(|p| !(p.user.platform == user.platform && p.user.user_id == user.user_id));
                self.store.data.paired.push(Paired { user: user.clone(), home: chat.clone() });
                self.store.save();
                self.say(&chat, Msg::plain(format!("You’re paired, {}. {}", user.name, HELP_SHORT)));
                self.publish();
                return Ok(Reply::Bots(BotReply::Paired(user)));
            }
            BotCommand::Reject { code } => {
                let before = self.pairing.len();
                self.pairing.retain(|(r, _)| r.code != code.trim());
                if self.pairing.len() == before {
                    return Err(format!("No one is waiting with code {}.", code.trim()));
                }
            }
            BotCommand::Allow { platform, user_id, name } => {
                let user_id = user_id.trim().to_owned();
                if user_id.is_empty() {
                    return Err("Give the person’s user id on that platform.".into());
                }
                let name = if name.trim().is_empty() { user_id.clone() } else { name.trim().to_owned() };
                let user = PairedUser { platform, user_id: user_id.clone(), name, paired_ms: now_ms() };
                // Their direct chat is known once they write; Telegram's and
                // Discord's direct chats share the user's id or are found then.
                let home = ChatKey::new(platform, if platform == Platform::Telegram { user_id.clone() } else { String::new() });
                self.store.data.paired.retain(|p| !(p.user.platform == platform && p.user.user_id == user_id));
                self.store.data.paired.push(Paired { user: user.clone(), home });
                self.pairing.retain(|(r, _)| !(r.platform == platform && r.user_id == user_id));
                self.store.save();
                return Ok(Reply::Bots(BotReply::Paired(user)));
            }
            BotCommand::Revoke { platform, user_id } => {
                let before = self.store.data.paired.len();
                self.store.data.paired.retain(|p| !(p.user.platform == platform && p.user.user_id == user_id.trim()));
                if self.store.data.paired.len() == before {
                    return Err("That person isn’t paired.".into());
                }
                self.store.save();
            }
            BotCommand::AppActive(active) => {
                self.app_active = active;
                return Ok(Reply::Done);
            }
        }
        Ok(Reply::Done)
    }

    fn expire_pairings(&mut self) -> bool {
        let now = now_ms();
        let before = self.pairing.len();
        self.pairing.retain(|(r, _)| r.expires_ms > now);
        self.pairing.len() != before
    }

    // ── Engine events ────────────────────────────────────────────────────

    /// Applies a batch from the engine, reacting to each event in order
    /// (a fast turn's start, answer and end can share one batch).
    pub fn on_updates(&mut self, updates: Vec<Update>) {
        for update in updates {
            match update {
                Update::Events(envs) => {
                    for env in envs {
                        let reaction = match &env.event {
                            Event::ThreadUpserted(t) => Some((t.id, None)),
                            Event::ItemUpserted { thread, item } => Some((*thread, Some(item.clone()))),
                            Event::GuardChanged(g) => {
                                self.guard = g.clone();
                                None
                            }
                            _ => None,
                        };
                        self.state.apply(vec![Update::Events(vec![env])]);
                        match reaction {
                            Some((thread, Some(item))) => self.on_item(thread, item),
                            Some((thread, None)) => self.on_thread(thread),
                            None => {}
                        }
                    }
                }
                other => {
                    if let Update::Reset(snapshot) = &other {
                        // What was pending before we looked is not news.
                        for t in &snapshot.threads {
                            self.known.extend(t.pending.iter().map(|p| p.request.id));
                        }
                    }
                    self.state.apply(vec![other]);
                }
            }
        }
    }

    pub fn set_guard(&mut self, guard: GuardSettings) {
        self.guard = guard;
    }

    fn watched(&self, thread: ThreadId) -> Vec<ChatKey> {
        self.store.data.chats_of(thread).cloned().collect()
    }

    fn push_now(&self) -> bool {
        match self.store.data.options.notify {
            BotNotify::Off => false,
            BotNotify::Always => true,
            BotNotify::WhenAway => !self.app_active,
        }
    }

    /// Paired people's own chats, for needs-you pushes.
    fn homes(&self) -> Vec<ChatKey> {
        let mut homes: Vec<ChatKey> = self
            .store
            .data
            .paired
            .iter()
            .map(|p| p.home.clone())
            .filter(|h| !h.chat.is_empty() && self.running.contains_key(&h.platform))
            .collect();
        homes.sort();
        homes.dedup();
        homes
    }

    fn on_thread(&mut self, id: ThreadId) {
        let Some(thread) = self.state.threads.get(&id).cloned() else {
            self.tracks.remove(&id);
            return;
        };
        let chats = self.watched(id);
        // New requests.
        let fresh: Vec<PendingRequest> = thread.pending.iter().filter(|p| !self.known.contains(&p.request.id)).cloned().collect();
        for pending in fresh {
            self.known.insert(pending.request.id);
            let targets = if !chats.is_empty() {
                chats.clone()
            } else if self.push_now() {
                self.homes()
            } else {
                Vec::new()
            };
            if !targets.is_empty() {
                self.announce(&thread, &pending.request, targets);
            }
        }
        // Turns.
        if chats.is_empty() {
            if let ThreadStatus::Failed { message } = &thread.status
                && self.tracks.remove(&id).is_some()
                && self.push_now()
            {
                for home in self.homes() {
                    self.say(&home, Msg::plain(format!("Failed · {}\n{message}", title_of(&thread))));
                }
            }
            return;
        }
        match &thread.status {
            ThreadStatus::Running | ThreadStatus::Waiting if !self.tracks.contains_key(&id) => self.begin(id, &chats),
            ThreadStatus::Failed { message } if self.tracks.contains_key(&id) => {
                let message = message.clone();
                self.finish(id, None, Some(message));
            }
            _ => {}
        }
    }

    fn begin(&mut self, id: ThreadId, chats: &[ChatKey]) {
        self.tracks.insert(id, Track { started_ms: now_ms(), ..Track::default() });
        let text = self.status_text(id).unwrap_or_else(|| "Working".into());
        if let Some(track) = self.tracks.get_mut(&id) {
            track.last_status = text.clone();
            track.last_edit = Some(Instant::now());
        }
        for chat in chats {
            self.status_ids.remove(&(id, chat.clone()));
            self.closing.remove(&(id, chat.clone()));
            self.out(Out::Send { chat: chat.clone(), msg: Msg::plain(text.clone()), tag: Some(Tag::Status(id)) });
        }
    }

    fn on_item(&mut self, thread: ThreadId, item: Item) {
        if let ItemBody::Request(request) = &item.body
            && let Some(decision) = &request.resolution
        {
            self.answered(request.id, decision);
            return;
        }
        let Some(track) = self.tracks.get_mut(&thread) else { return };
        match &item.body {
            ItemBody::Tool(tool) if item.status == ItemStatus::Streaming => track.doing = Some((item.id, doing(tool))),
            ItemBody::Tool(_) => {
                if track.doing.as_ref().is_some_and(|(i, _)| *i == item.id) {
                    track.doing = None;
                }
            }
            ItemBody::Assistant { text } if !text.trim().is_empty() => track.answer = Some(text.clone()),
            ItemBody::TurnEnd(summary) => {
                let ready = summary.checkpoint.is_some() || summary.base_checkpoint.is_none() || summary.interrupted;
                let since = track.ended.as_ref().map_or_else(Instant::now, |(_, at)| *at);
                track.ended = Some((summary.clone(), since));
                if ready {
                    self.finish(thread, Some(summary.clone()), None);
                }
            }
            _ => {}
        }
    }

    /// The turn is over: close the status line and post the answer.
    fn finish(&mut self, id: ThreadId, summary: Option<TurnSummary>, failure: Option<String>) {
        let Some(track) = self.tracks.remove(&id) else { return };
        let elapsed = short_duration(now_ms().saturating_sub(track.started_ms));
        let (status, answer) = match (&failure, &summary) {
            (Some(message), _) => (format!("Failed · {elapsed}"), format!("Failed: {message}")),
            (None, Some(s)) if s.interrupted => (format!("Stopped · {elapsed}"), track.answer.clone().unwrap_or_default()),
            _ => (format!("Done · {elapsed}"), track.answer.clone().unwrap_or_default()),
        };
        let files = summary.as_ref().map(|s| files_line(&s.files)).unwrap_or_default();
        let mut text = answer;
        if !files.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&files);
        }
        for chat in self.watched(id) {
            match self.status_ids.remove(&(id, chat.clone())) {
                Some(msg_id) => self.out(Out::Edit { chat: chat.clone(), id: msg_id, msg: Msg::plain(status.clone()) }),
                None => {
                    self.closing.insert((id, chat.clone()), status.clone());
                }
            }
            if !text.trim().is_empty() {
                self.out(Out::Send { chat: chat.clone(), msg: Msg::markdown(text.clone()), tag: None });
            }
        }
    }

    /// Posts an approval or question with its buttons.
    fn announce(&mut self, thread: &Thread, request: &AgentRequest, chats: Vec<ChatKey>) {
        let title = title_of(thread);
        let guarded = self.guard.check(&request.kind);
        let base = format!("{}:{:x}:{:x}", "a", thread.id.0, request.id.0);
        let (text, buttons) = match &request.kind {
            RequestKind::Question { question, options, multi_select } => {
                let mut text = format!("**Question** · {title}\n{question}");
                for (i, o) in options.iter().enumerate() {
                    text.push_str(&format!("\n{}. {}", i + 1, o.label));
                    if !o.description.is_empty() {
                        text.push_str(&format!(" — {}", o.description));
                    }
                }
                text.push_str(if *multi_select {
                    "\n\nTap one, or reply with several numbers (1, 3) or your own answer."
                } else {
                    "\n\nTap one, or reply with your own answer."
                });
                let rows = options
                    .iter()
                    .enumerate()
                    .map(|(i, o)| {
                        let label: String = format!("{}. {}", i + 1, o.label).chars().take(40).collect();
                        vec![Button::new(label, format!("q:{:x}:{:x}:{i}", thread.id.0, request.id.0), Style::Normal)]
                    })
                    .collect();
                (text, rows)
            }
            kind => {
                let mut text = describe_request(kind, &title);
                let buttons = if let Some(hit) = &guarded {
                    text.push_str(&format!(
                        "\n\n**Guarded: {}.** Approve it in the Orbit app; you can deny it here.",
                        hit.label
                    ));
                    vec![vec![Button::new("No", format!("{base}:n"), Style::Danger)]]
                } else if matches!(kind, RequestKind::Plan { .. }) {
                    vec![vec![
                        Button::new("Carry it out", format!("{base}:y"), Style::Primary),
                        Button::new("Keep planning", format!("{base}:n"), Style::Normal),
                    ]]
                } else {
                    vec![vec![
                        Button::new("Yes", format!("{base}:y"), Style::Primary),
                        Button::new("No", format!("{base}:n"), Style::Danger),
                        Button::new("Always", format!("{base}:s"), Style::Normal),
                    ]]
                };
                (text, buttons)
            }
        };
        let msg = Msg::markdown(text).with_buttons(buttons);
        for chat in &chats {
            self.out(Out::Send { chat: chat.clone(), msg: msg.clone(), tag: Some(Tag::Request(request.id)) });
        }
        self.asked.insert(
            request.id,
            Asked { thread: thread.id, msg, sent: chats.into_iter().map(|c| (c, None)).collect() },
        );
    }

    /// A request was answered (here, in the app or anywhere): its messages
    /// lose their buttons and say how.
    fn answered(&mut self, request: RequestId, decision: &Decision) {
        let Some(asked) = self.asked.remove(&request) else { return };
        let outcome = match decision {
            Decision::Allow => "Approved".to_owned(),
            Decision::AllowForSession => "Approved for this session".to_owned(),
            Decision::Deny { .. } => "Denied".to_owned(),
            Decision::Answer { text } => format!("Answered: {}", text.lines().collect::<Vec<_>>().join(", ")),
        };
        let mut msg = asked.msg.clone();
        msg.buttons.clear();
        // Drop the instructions line and say what happened.
        if let Some(cut) = msg.text.rfind("\n\nTap one") {
            msg.text.truncate(cut);
        }
        if let Some(cut) = msg.text.rfind("\n\n**Guarded") {
            msg.text.truncate(cut);
        }
        msg.text.push_str(&format!("\n\n→ {outcome}"));
        let _ = asked.thread;
        for (chat, id) in asked.sent {
            if let Some(id) = id {
                self.out(Out::Edit { chat, id, msg: msg.clone() });
            }
        }
    }

    /// The status line of a running turn: "Working · editing src/main.rs · 2m".
    fn status_text(&self, id: ThreadId) -> Option<String> {
        let thread = self.state.threads.get(&id)?;
        let track = self.tracks.get(&id);
        let now = now_ms();
        let elapsed = track.map(|t| short_duration(now.saturating_sub(t.started_ms))).unwrap_or_default();
        let live = self.state.live.get(&id);
        let with_time = |text: String| if elapsed.is_empty() { text } else { format!("{text} · {elapsed}") };
        Some(match LiveStatus::of(thread, live) {
            LiveStatus::Permission => with_time("Waiting for your approval".into()),
            LiveStatus::NeedsYou => with_time("Waiting for your answer".into()),
            LiveStatus::Quiet => {
                let line = live.and_then(|l| liveness_line(&l.liveness, now)).unwrap_or_else(|| "No output for a while".into());
                with_time(format!("Working · {line}"))
            }
            LiveStatus::Unverified => {
                let line = live.and_then(|l| liveness_line(&l.liveness, now)).unwrap_or_else(|| "Unverified".into());
                with_time(line)
            }
            LiveStatus::RateLimited => "Out of quota · resumes when the provider allows".into(),
            LiveStatus::Failed => "Failed".into(),
            _ => match track.and_then(|t| t.doing.as_ref()) {
                Some((_, doing)) => with_time(format!("Working · {doing}")),
                None => with_time("Working".into()),
            },
        })
    }

    // ── Time ─────────────────────────────────────────────────────────────

    /// Whether [`Hub::tick`] has work: turns to show or codes to expire.
    pub fn busy(&self) -> bool {
        !self.tracks.is_empty() || !self.pairing.is_empty()
    }

    pub fn tick(&mut self) {
        if self.expire_pairings() {
            self.publish();
        }
        let ids: Vec<ThreadId> = self.tracks.keys().copied().collect();
        for id in ids {
            let overdue = self.tracks.get(&id).and_then(|t| t.ended.as_ref()).filter(|(_, at)| at.elapsed() >= END_WAIT);
            if let Some((summary, _)) = overdue {
                let summary = summary.clone();
                self.finish(id, Some(summary), None);
                continue;
            }
            let Some(text) = self.status_text(id) else {
                self.tracks.remove(&id);
                continue;
            };
            let Some(track) = self.tracks.get_mut(&id) else { continue };
            if track.ended.is_some() || text == track.last_status || track.last_edit.is_some_and(|t| t.elapsed() < STATUS_EVERY) {
                continue;
            }
            track.last_status = text.clone();
            track.last_edit = Some(Instant::now());
            for chat in self.watched(id) {
                if let Some(msg_id) = self.status_ids.get(&(id, chat.clone())) {
                    self.out(Out::Edit { chat, id: msg_id.clone(), msg: Msg::plain(text.clone()) });
                }
            }
        }
    }

    // ── Chats ────────────────────────────────────────────────────────────

    fn allow_inbound(&mut self, platform: Platform, user: &str) -> Result<(), bool> {
        let bucket = self.buckets.entry((platform, user.to_owned())).or_insert(Bucket { tokens: BURST, at: Instant::now(), warned: None });
        let refill = bucket.at.elapsed().as_secs_f64() / REFILL_SECS;
        bucket.tokens = (bucket.tokens + refill).min(BURST);
        bucket.at = Instant::now();
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        // Over the limit: warn once in a while, else drop quietly.
        let warn = bucket.warned.is_none_or(|w| w.elapsed() >= Duration::from_secs(30));
        if warn {
            bucket.warned = Some(Instant::now());
        }
        Err(warn)
    }

    pub async fn on_incoming(&mut self, m: Incoming) {
        let platform = m.chat.platform;
        if !self.store.data.enabled(platform) {
            return;
        }
        if let Err(warn) = self.allow_inbound(platform, &m.user_id) {
            if warn {
                self.say(&m.chat, Msg::plain("Slow down: up to 20 messages a minute. The last ones were skipped."));
            }
            return;
        }
        let Some(paired) = self.store.data.paired(platform, &m.user_id).cloned() else {
            self.pair(&m);
            return;
        };
        // Learn the home chat of someone added by id.
        if m.direct && paired.home.chat.is_empty() {
            if let Some(p) = self.store.data.paired.iter_mut().find(|p| p.user == paired.user) {
                p.home = ChatKey::new(platform, m.chat.chat.clone());
            }
            self.store.save();
        }
        let result = match &m.kind {
            InKind::Button(data) => self.button(&m, data).await,
            InKind::Text(text) => match command(text) {
                Some((name, args)) if is_command(&name) => self.command(&m, &name, &args).await,
                _ => self.message(&m, text.trim()).await,
            },
        };
        if let Err(error) = result {
            self.say(&m.chat, Msg::plain(error));
        }
    }

    fn pair(&mut self, m: &Incoming) {
        self.expire_pairings();
        if !m.direct {
            self.say(&m.chat, Msg::plain("Send me a direct message to pair with this Orbit."));
            return;
        }
        let platform = m.chat.platform;
        let code = match self.pairing.iter().find(|(r, _)| r.platform == platform && r.user_id == m.user_id) {
            Some((r, _)) => r.code.clone(),
            None if self.pairing.len() >= PAIRING_MAX => {
                self.say(&m.chat, Msg::plain("Too many people are waiting to pair. Try again later."));
                return;
            }
            None => {
                let code = pairing_code(&self.pairing);
                let now = now_ms();
                self.pairing.push((
                    PairingRequest {
                        platform,
                        user_id: m.user_id.clone(),
                        name: m.user_name.clone(),
                        code: code.clone(),
                        created_ms: now,
                        expires_ms: now + PAIRING_TTL_MS,
                    },
                    ChatKey::new(platform, m.chat.chat.clone()),
                ));
                self.publish();
                code
            }
        };
        self.say(
            &m.chat,
            Msg::plain(format!(
                "Pairing code: {code}\nAsk the owner of this Orbit to approve it in Settings › Bots, or to run `orbit bot pair {code}`. The code expires in 15 minutes."
            )),
        );
    }

    async fn button(&mut self, m: &Incoming, data: &str) -> Result<(), String> {
        let parts: Vec<&str> = data.split(':').collect();
        let hex = |s: &str| u64::from_str_radix(s, 16).map_err(|_| "That button is out of date.".to_owned());
        match parts.as_slice() {
            ["a", thread, request, choice] => {
                let (thread, request) = (ThreadId(hex(thread)?), RequestId(hex(request)?));
                let decision = match *choice {
                    "y" => Decision::Allow,
                    "s" => Decision::AllowForSession,
                    _ => Decision::Deny { feedback: String::new() },
                };
                self.resolve(m, thread, request, decision).await
            }
            ["q", thread, request, index] => {
                let (thread, request) = (ThreadId(hex(thread)?), RequestId(hex(request)?));
                let index: usize = index.parse().map_err(|_| "That button is out of date.".to_owned())?;
                let label = self
                    .pending(thread)
                    .into_iter()
                    .find(|p| p.request.id == request)
                    .and_then(|p| match p.request.kind {
                        RequestKind::Question { options, .. } => options.get(index).map(|o| o.label.clone()),
                        _ => None,
                    })
                    .ok_or("That question was already answered.")?;
                self.resolve(m, thread, request, Decision::Answer { text: label }).await
            }
            ["n", project] => {
                let project = if *project == "-" { None } else { Some(ProjectId(hex(project)?)) };
                let prompt = self.waiting_prompt.remove(&m.chat);
                self.start(m, project, prompt).await
            }
            ["t", thread] => {
                let thread = ThreadId(hex(thread)?);
                self.switch(m, thread)
            }
            _ => Err("That button is out of date.".into()),
        }
    }

    async fn resolve(&mut self, m: &Incoming, thread: ThreadId, request: RequestId, decision: Decision) -> Result<(), String> {
        if !self.pending(thread).iter().any(|p| p.request.id == request) {
            return Err("That was already answered.".into());
        }
        self.via(m, ViaAction::Resolve { thread, request, decision }).await.map(drop)
    }

    fn pending(&self, thread: ThreadId) -> Vec<PendingRequest> {
        self.state.threads.get(&thread).map(|t| t.pending.clone()).unwrap_or_default()
    }

    /// The thread this chat drives, if it still exists.
    fn bound(&self, chat: &ChatKey) -> Option<ThreadId> {
        self.store.data.binding(chat).filter(|t| self.state.threads.get(t).is_some_and(|t| !t.archived))
    }

    async fn message(&mut self, m: &Incoming, text: &str) -> Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        let Some(thread) = self.bound(&m.chat) else {
            return self.new_with(m, String::new(), text.to_owned()).await;
        };
        let pending = self.pending(thread);
        // A question waiting: the message answers it.
        if let Some(q) = pending.iter().find(|p| matches!(p.request.kind, RequestKind::Question { .. })) {
            let RequestKind::Question { options, .. } = &q.request.kind else { unreachable!() };
            let answer = answer_text(text, options);
            return self.resolve(m, thread, q.request.id, Decision::Answer { text: answer }).await;
        }
        // One approval waiting: yes / no / always answer it.
        let approvals: Vec<&PendingRequest> =
            pending.iter().filter(|p| !matches!(p.request.kind, RequestKind::Question { .. })).collect();
        if let [only] = approvals.as_slice()
            && let Some(decision) = yes_no(text)
        {
            let request = only.request.id;
            return self.resolve(m, thread, request, decision).await;
        }
        let steer = self.store.data.options.steer;
        self.via(m, ViaAction::Send { thread, text: text.to_owned(), steer }).await?;
        if !self.tracks.contains_key(&thread) {
            let chats = self.watched(thread);
            self.begin(thread, &chats);
        }
        Ok(())
    }

    /// `/new [project] [prompt]`, or a first message: picks the project
    /// (named, the default, the only one, or asks) and starts a thread.
    async fn new_with(&mut self, m: &Incoming, args: String, prompt: String) -> Result<(), String> {
        let mut words = args.splitn(2, char::is_whitespace);
        let first = words.next().unwrap_or_default();
        let named = (!first.is_empty())
            .then(|| self.state.projects.iter().find(|p| p.name.eq_ignore_ascii_case(first)).map(|p| p.id))
            .flatten();
        let prompt = match (named, prompt.is_empty()) {
            (Some(_), true) => words.next().unwrap_or_default().trim().to_owned(),
            (None, true) => args.trim().to_owned(),
            (_, false) => prompt,
        };
        let project = match named {
            Some(p) => Some(p),
            None => match (self.store.data.options.project, self.state.projects.as_slice()) {
                (Some(p), _) if self.state.projects.iter().any(|x| x.id == p) => Some(p),
                (_, []) => None,
                (_, [one]) => Some(one.id),
                (_, many) => {
                    let mut rows: Vec<Vec<Button>> = many
                        .iter()
                        .take(12)
                        .map(|p| vec![Button::new(p.name.clone(), format!("n:{:x}", p.id.0), Style::Normal)])
                        .collect();
                    rows.push(vec![Button::new("No project", "n:-", Style::Normal)]);
                    if !prompt.is_empty() {
                        self.waiting_prompt.insert(m.chat.clone(), prompt);
                    }
                    self.say(&m.chat, Msg::plain("Which project?").with_buttons(rows));
                    return Ok(());
                }
            },
        };
        self.start(m, project, (!prompt.is_empty()).then_some(prompt)).await
    }

    async fn start(&mut self, m: &Incoming, project: Option<ProjectId>, prompt: Option<String>) -> Result<(), String> {
        let options = self.store.data.options.clone();
        let mut config = self.state.defaults.clone();
        config.access = if options.access == Access::Full { Access::AutoEdit } else { options.access };
        let start = ThreadStart { project, config, worktree: options.worktree && project.is_some(), ..Default::default() };
        let Reply::Thread(thread) = self.via(m, ViaAction::Start(start)).await? else {
            return Err("Orbit didn’t start a thread.".into());
        };
        self.store.data.bind(m.chat.clone(), thread);
        self.store.save();
        let place = project
            .and_then(|p| self.state.projects.iter().find(|x| x.id == p))
            .map_or_else(|| "a scratch folder".to_owned(), |p| p.name.clone());
        match prompt {
            Some(prompt) => {
                // Wait for the thread to arrive in our state, so its turn is
                // tracked from the start.
                self.say(&m.chat, Msg::plain(format!("New thread in {place}.")));
                self.via(m, ViaAction::Send { thread, text: prompt, steer: false }).await?;
                let chats = self.watched(thread);
                if !self.tracks.contains_key(&thread) {
                    self.begin(thread, &chats);
                }
            }
            None => self.say(&m.chat, Msg::plain(format!("New thread in {place}. Send a message to start."))),
        }
        Ok(())
    }

    fn switch(&mut self, m: &Incoming, thread: ThreadId) -> Result<(), String> {
        let t = self.state.threads.get(&thread).ok_or("That thread no longer exists.")?;
        let line = format!("Now in “{}” · {}", title_of(t), self.state.live_status(t).label());
        self.store.data.bind(m.chat.clone(), thread);
        self.store.save();
        self.say(&m.chat, Msg::plain(line));
        Ok(())
    }

    async fn command(&mut self, m: &Incoming, name: &str, args: &str) -> Result<(), String> {
        match name {
            "help" | "start" => {
                self.say(&m.chat, Msg::plain(HELP));
                Ok(())
            }
            "new" => self.new_with(m, args.to_owned(), String::new()).await,
            "projects" => {
                if self.state.projects.is_empty() {
                    self.say(&m.chat, Msg::plain("No projects yet. Add one in Orbit; until then /new starts in a scratch folder."));
                    return Ok(());
                }
                let rows = self
                    .state
                    .projects
                    .iter()
                    .take(20)
                    .map(|p| vec![Button::new(format!("New thread in {}", p.name), format!("n:{:x}", p.id.0), Style::Normal)])
                    .collect();
                let list: Vec<String> = self.state.projects.iter().map(|p| format!("• {} — {}", p.name, p.path)).collect();
                self.say(&m.chat, Msg::plain(format!("Projects:\n{}", list.join("\n"))).with_buttons(rows));
                Ok(())
            }
            "threads" => {
                let current = self.bound(&m.chat);
                let mut threads: Vec<&Thread> = self.state.threads.values().filter(|t| !t.archived && t.parent.is_none()).collect();
                threads.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
                if threads.is_empty() {
                    self.say(&m.chat, Msg::plain("No threads yet. Send a message to start one."));
                    return Ok(());
                }
                let mut lines = Vec::new();
                let mut rows = Vec::new();
                for t in threads.into_iter().take(10) {
                    let mark = if Some(t.id) == current { " (this chat)" } else { "" };
                    lines.push(format!("• {} · {}{mark}", title_of(t), self.state.live_status(t).label()));
                    let label: String = title_of(t).chars().take(40).collect();
                    rows.push(vec![Button::new(label, format!("t:{:x}", t.id.0), Style::Normal)]);
                }
                self.say(&m.chat, Msg::plain(format!("Recent threads:\n{}", lines.join("\n"))).with_buttons(rows));
                Ok(())
            }
            "switch" => {
                let query = args.trim().to_lowercase();
                if query.is_empty() {
                    return Err("Say which: /switch <part of its title>, or pick one with /threads.".into());
                }
                let found: Vec<ThreadId> = self
                    .state
                    .threads
                    .values()
                    .filter(|t| !t.archived)
                    .filter(|t| t.title.to_lowercase().contains(&query) || t.id.to_string().contains(&query))
                    .map(|t| t.id)
                    .collect();
                match found.as_slice() {
                    [one] => self.switch(m, *one),
                    [] => Err(format!("No thread matches “{}”.", args.trim())),
                    _ => Err(format!("{} threads match; be more specific, or use /threads.", found.len())),
                }
            }
            "status" => {
                let mut counts: HashMap<&str, usize> = HashMap::new();
                for t in self.state.threads.values().filter(|t| !t.archived) {
                    let key = match self.state.live_status(t) {
                        LiveStatus::Working | LiveStatus::Quiet | LiveStatus::Unverified => "working",
                        LiveStatus::Permission | LiveStatus::NeedsYou => "need you",
                        LiveStatus::Failed => "failed",
                        _ => continue,
                    };
                    *counts.entry(key).or_default() += 1;
                }
                let mut fleet: Vec<String> = ["working", "need you", "failed"]
                    .iter()
                    .filter_map(|k| counts.get(k).map(|n| format!("{n} {k}")))
                    .collect();
                if fleet.is_empty() {
                    fleet.push("nothing running".into());
                }
                let mut text = format!("Orbit: {}", fleet.join(" · "));
                if let Some(id) = self.bound(&m.chat)
                    && let Some(t) = self.state.threads.get(&id)
                {
                    let status = self.status_text(id).unwrap_or_else(|| self.state.live_status(t).label().to_owned());
                    text.push_str(&format!("\nThis chat: “{}” · {status}", title_of(t)));
                    let usage = self.state.usage(id);
                    if usage.cost_micros > 0 {
                        text.push_str(&format!(" · ${:.2}", usage.cost_micros as f64 / 1e6));
                    }
                }
                self.say(&m.chat, Msg::plain(text));
                Ok(())
            }
            "stop" => {
                let all = args.trim().eq_ignore_ascii_case("all");
                let force = args.trim().eq_ignore_ascii_case("force");
                let targets: Vec<ThreadId> = if all {
                    self.state.threads.values().filter(|t| t.status.is_busy()).map(|t| t.id).collect()
                } else {
                    self.bound(&m.chat).into_iter().collect()
                };
                if targets.is_empty() {
                    return Err(if all { "Nothing is running.".into() } else { "This chat has no thread yet.".into() });
                }
                for thread in &targets {
                    self.via(m, ViaAction::Stop { thread: *thread, force }).await?;
                }
                self.say(&m.chat, Msg::plain(if all { format!("Stopping {} threads.", targets.len()) } else { "Stopping.".into() }));
                Ok(())
            }
            "diff" => {
                let thread = self.bound(&m.chat).ok_or("This chat has no thread yet.")?;
                let Reply::Diff(patch) = self.call(Command::Diff { thread, scope: DiffScope::Thread }).await? else {
                    return Err("Orbit returned no diff.".into());
                };
                if patch.trim().is_empty() {
                    self.say(&m.chat, Msg::plain("No changes yet."));
                    return Ok(());
                }
                let stat = diff_stat(&patch);
                self.say(&m.chat, Msg::markdown(stat));
                self.out(Out::File {
                    chat: m.chat.clone(),
                    name: format!("{thread}.patch"),
                    bytes: patch.into_bytes(),
                    caption: String::new(),
                });
                Ok(())
            }
            "clear" => {
                let thread = self.bound(&m.chat).ok_or("This chat has no thread yet.")?;
                self.via(m, ViaAction::NewSession { thread }).await?;
                self.say(&m.chat, Msg::plain("Started a fresh session in this thread."));
                Ok(())
            }
            _ => unreachable!("is_command"),
        }
    }
}

const HELP_SHORT: &str = "Send a message to start a thread, or /help for commands.";

const HELP: &str = "Messages go to this chat’s thread (a new one the first time).
/new [project] [message] — start a new thread
/projects — list projects
/threads — recent threads; tap one to switch
/switch <title> — drive another thread from here
/status — what’s running, and this chat’s thread
/stop — stop this chat’s thread (/stop all, /stop force)
/diff — what changed, with the patch attached
/clear — fresh session in this thread
Approve with the buttons, or reply yes, no or always. Guarded actions can only be approved in the Orbit app.
On Slack, use !status instead of /status (or /orbit status).";

fn is_command(name: &str) -> bool {
    matches!(name, "help" | "start" | "new" | "projects" | "threads" | "switch" | "status" | "stop" | "diff" | "clear")
}

/// `/name args` or `!name args`; Telegram's `/name@bot` too.
fn command(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    let rest = text.strip_prefix('/').or_else(|| text.strip_prefix('!'))?;
    let (head, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let name = head.split('@').next().unwrap_or(head).to_lowercase();
    (!name.is_empty()).then(|| (name, args.trim().to_owned()))
}

fn yes_no(text: &str) -> Option<Decision> {
    match text.trim().trim_end_matches(['.', '!']).to_lowercase().as_str() {
        "yes" | "y" | "approve" | "ok" | "allow" => Some(Decision::Allow),
        "no" | "n" | "deny" | "reject" => Some(Decision::Deny { feedback: String::new() }),
        "always" | "allow always" => Some(Decision::AllowForSession),
        _ => None,
    }
}

/// A reply to a question: option numbers ("2", "1, 3") become their
/// labels (one per line), anything else is the answer as typed.
fn answer_text(text: &str, options: &[QuestionOption]) -> String {
    let numbers: Option<Vec<usize>> = text
        .split([',', ' '])
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<usize>().ok().filter(|n| (1..=options.len()).contains(n)))
        .collect();
    match numbers {
        Some(n) if !n.is_empty() => n.iter().map(|i| options[i - 1].label.clone()).collect::<Vec<_>>().join("\n"),
        _ => options.iter().find(|o| o.label.eq_ignore_ascii_case(text.trim())).map_or_else(|| text.to_owned(), |o| o.label.clone()),
    }
}

fn pairing_code(taken: &[(PairingRequest, ChatKey)]) -> String {
    use std::hash::{BuildHasher, Hasher};
    loop {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(now_ms());
        h.write_usize(taken.len());
        let code = format!("{:06}", h.finish() % 1_000_000);
        if !taken.iter().any(|(r, _)| r.code == code) {
            return code;
        }
    }
}

fn title_of(thread: &Thread) -> String {
    if thread.title.trim().is_empty() { "New thread".to_owned() } else { thread.title.clone() }
}

/// "editing src/main.rs", from a running tool call.
fn doing(tool: &ToolCall) -> String {
    let title: String = tool.title.chars().take(60).collect();
    match tool.kind {
        ToolKind::Command => format!("running {title}"),
        ToolKind::Edit => format!("editing {title}"),
        ToolKind::Read => format!("reading {title}"),
        ToolKind::Search => format!("searching {title}"),
        ToolKind::Web => format!("browsing {title}"),
        ToolKind::Task => format!("delegating {title}"),
        ToolKind::Todo => "planning".to_owned(),
        ToolKind::Mcp | ToolKind::Other => title,
    }
    .trim()
    .to_owned()
}

fn describe_request(kind: &RequestKind, title: &str) -> String {
    let cut = |s: &str, n: usize| -> String {
        if s.chars().count() <= n { s.to_owned() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
    };
    match kind {
        RequestKind::Command { command, cwd, reason } => {
            let mut text = format!("**Run this command?** · {title}\n```\n{}\n```\nin `{cwd}`", cut(command, 1000));
            if !reason.is_empty() {
                text.push_str(&format!("\n{reason}"));
            }
            text
        }
        RequestKind::FileChange { path, diff } => {
            let mut text = format!("**Change `{path}`?** · {title}");
            if !diff.trim().is_empty() {
                text.push_str(&format!("\n```diff\n{}\n```", cut(diff, 1000)));
            }
            text
        }
        RequestKind::Tool { name, input } => format!("**Use {name}?** · {title}\n```\n{}\n```", cut(input, 600)),
        RequestKind::Plan { plan } => format!("**Plan ready** · {title}\n{}", cut(plan, 3000)),
        RequestKind::Question { question, .. } => format!("**Question** · {title}\n{question}"),
    }
}

/// "Changed 3 files: `a.rs` +12 −3, …".
fn files_line(files: &[FileChange]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let shown: Vec<String> =
        files.iter().take(8).map(|f| format!("`{}` +{} −{}", f.path, f.additions, f.deletions)).collect();
    let more = if files.len() > 8 { format!(" and {} more", files.len() - 8) } else { String::new() };
    let noun = if files.len() == 1 { "file" } else { "files" };
    format!("Changed {} {noun}: {}{more}", files.len(), shown.join(", "))
}

/// A `git diff --stat`-like summary of a patch.
fn diff_stat(patch: &str) -> String {
    let mut files: Vec<(String, u32, u32)> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest.split(" b/").nth(1).unwrap_or(rest).to_owned();
            files.push((path, 0, 0));
        } else if let Some(last) = files.last_mut() {
            if line.starts_with('+') && !line.starts_with("+++") {
                last.1 += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                last.2 += 1;
            }
        }
    }
    let (adds, dels) = files.iter().fold((0, 0), |(a, d), f| (a + f.1, d + f.2));
    let mut text = format!("**{} files changed**, +{adds} −{dels}", files.len());
    for (path, a, d) in files.iter().take(20) {
        text.push_str(&format!("\n`{path}` +{a} −{d}"));
    }
    if files.len() > 20 {
        text.push_str(&format!("\n…and {} more", files.len() - 20));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_on_every_platform() {
        assert_eq!(command("/new api fix it"), Some(("new".into(), "api fix it".into())));
        assert_eq!(command("/status@orbit_bot"), Some(("status".into(), String::new())));
        assert_eq!(command("!stop all"), Some(("stop".into(), "all".into())));
        assert_eq!(command("fix the bug"), None);
        assert!(!is_command("compact"), "agent slash commands pass through");
    }

    #[test]
    fn replies_answer_requests() {
        assert_eq!(yes_no("Yes!"), Some(Decision::Allow));
        assert_eq!(yes_no("always"), Some(Decision::AllowForSession));
        assert!(matches!(yes_no("no"), Some(Decision::Deny { .. })));
        assert_eq!(yes_no("yes please do X"), None);
        let options = vec![QuestionOption::from("Red"), QuestionOption::from("Blue"), QuestionOption::from("Green")];
        assert_eq!(answer_text("2", &options), "Blue");
        assert_eq!(answer_text("1, 3", &options), "Red\nGreen");
        assert_eq!(answer_text("blue", &options), "Blue");
        assert_eq!(answer_text("something else", &options), "something else");
    }

    #[test]
    fn summaries_read_well() {
        let files = vec![FileChange { path: "src/a.rs".into(), kind: ChangeKind::Modified, additions: 12, deletions: 3 }];
        assert_eq!(files_line(&files), "Changed 1 file: `src/a.rs` +12 −3");
        let patch = "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1 +1,2 @@\n-a\n+b\n+c\n";
        assert_eq!(diff_stat(patch), "**1 files changed**, +2 −1\n`x.rs` +2 −1");
        let codes: Vec<(PairingRequest, ChatKey)> = Vec::new();
        let code = pairing_code(&codes);
        assert!(code.len() == 6 && code.chars().all(|c| c.is_ascii_digit()));
    }
}
