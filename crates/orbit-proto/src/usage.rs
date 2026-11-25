//! Usage, quota and accounts.
//!
//! * The **quota board** ([`QuotaBoard`]): every account's limit windows (the
//!   five-hour and weekly windows of a Claude or ChatGPT plan), how fast
//!   they are being used and when they would run out at that pace, the
//!   threads that use them most, and the usage settings. The engine keeps it
//!   current from what sessions report and from the providers' own usage
//!   endpoints, and sends it whole in the snapshot and in [`crate::Event::Quota`]
//!   (ephemeral: never logged).
//! * **Usage stats** ([`UsageStats`]): tokens and cost per day, model,
//!   account, project and thread, from a ledger of finished turns.
//! * **Failover**: when an account hits its limit, a thread can continue on
//!   another account of the same agent (or, with a warning, another agent)
//!   through the usual handoff; [`FailoverPolicy`] says whether Orbit asks,
//!   switches by itself, or never offers it. A switch always leaves a notice
//!   in the thread's timeline.
//!
//! The settings live in the engine's own file (`usage.bin`), not in the
//! logged [`crate::Settings`], so they change without a schema version.

use bitcode::{Decode, Encode};

use crate::ids::*;
use crate::model::*;

// ── Quota board ──────────────────────────────────────────────────────────

/// Every account's quota, as the engine knows it now.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct QuotaBoard {
    /// One per provider instance that has (or can have) a plan limit, in the
    /// order instances are listed.
    pub accounts: Vec<AccountQuota>,
    pub settings: UsageSettings,
}

impl QuotaBoard {
    pub fn account(&self, instance: &str) -> Option<&AccountQuota> {
        self.accounts.iter().find(|a| a.instance == instance)
    }
}

/// One account: a provider instance with its own sign-in.
#[derive(Clone, Debug, Default, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct AccountQuota {
    /// The provider instance id ([`crate::settings::ProviderInstance`]).
    pub instance: String,
    pub kind: ProviderKind,
    /// The instance's display name.
    pub name: String,
    /// The plan as the provider names it (`Max 20x`, `ChatGPT Plus`).
    pub plan: Option<String>,
    /// Tightest first is up to the reader; the engine keeps the provider's
    /// order (shortest window first).
    pub windows: Vec<QuotaWindowState>,
    pub status: QuotaStatus,
    /// When the windows were last read, from any source (Unix ms; 0: never).
    pub updated_ms: u64,
    pub source: QuotaSource,
    /// The threads that used this account most in its shortest window.
    pub top: Vec<Consumer>,
}

impl AccountQuota {
    /// The most-used window: what a ring shows.
    pub fn tightest(&self) -> Option<&QuotaWindowState> {
        self.windows.iter().max_by(|a, b| a.used().total_cmp(&b.used()))
    }

    /// Requests are refused until a window resets.
    pub fn limited(&self, now_ms: u64) -> bool {
        self.windows.iter().any(|w| w.window.rejected && (w.window.resets_ms == 0 || w.window.resets_ms > now_ms))
    }

    /// The share left in the tightest window (1.0 when nothing is known).
    pub fn headroom(&self) -> f32 {
        1.0 - self.tightest().map(QuotaWindowState::used).unwrap_or(0.0)
    }
}

/// One limit window with what Orbit derives from its history.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct QuotaWindowState {
    pub window: QuotaWindow,
    /// Interface copy: `5-hour`, `Weekly`, `Weekly · Opus`.
    pub label: String,
    /// The window's length in milliseconds; 0 when unknown.
    pub span_ms: u64,
    /// Share of the window used per hour at the recent pace, when there is
    /// enough recent history to say.
    pub burn_per_hour: Option<f32>,
    /// When the window runs out at that pace (Unix ms). `None` with a burn
    /// rate means it lasts until it resets.
    pub runs_out_ms: Option<u64>,
}

impl QuotaWindowState {
    /// Share used, 0.0–1.0; a refused window counts as full.
    pub fn used(&self) -> f32 {
        if self.window.rejected { 1.0 } else { self.window.utilization.unwrap_or(0.0).clamp(0.0, 1.0) }
    }
}

/// Whether an account's limits can be read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum QuotaStatus {
    /// Not read yet.
    #[default]
    Unknown,
    /// Being read now.
    Loading,
    Ok,
    /// The account is not signed in to a plan with limits (an API key, or
    /// no sign-in at all); the message says which.
    NoPlan(String),
    /// The provider could not be asked; the message says why and what to do.
    Unavailable(String),
}

/// Where the latest reading came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum QuotaSource {
    #[default]
    None,
    /// A running session reported it with a turn.
    Session,
    /// The provider's usage endpoint (Claude's OAuth usage, Codex
    /// `account/rateLimits/read`).
    Provider,
}

/// A thread and what it used.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Consumer {
    pub thread: ThreadId,
    /// The thread's title, or `Deleted thread`.
    pub title: String,
    pub tokens: u64,
    pub cost_micros: u64,
}

// ── Settings ─────────────────────────────────────────────────────────────

/// What Orbit does when an account reaches its limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum FailoverPolicy {
    /// Offer to continue on another account in the thread's limit banner.
    #[default]
    Ask,
    /// Continue on the account of the same agent with the most headroom,
    /// with a notice in the timeline; ask when none has any.
    Auto,
    /// Wait for the reset; never offer another account.
    Never,
}

impl FailoverPolicy {
    pub const ALL: [FailoverPolicy; 3] = [FailoverPolicy::Ask, FailoverPolicy::Auto, FailoverPolicy::Never];

    pub fn label(self) -> &'static str {
        match self {
            FailoverPolicy::Ask => "Ask",
            FailoverPolicy::Auto => "Automatically",
            FailoverPolicy::Never => "Never",
        }
    }
}

/// What an account pin applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum PinTarget {
    /// New threads of this project start on the account, and are never
    /// moved off it automatically.
    Project(ProjectId),
    /// The automation's runs use the account, and are never moved off it
    /// automatically.
    Automation(AutomationId),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct AccountPin {
    pub target: PinTarget,
    /// The provider instance id.
    pub instance: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct UsageSettings {
    pub failover: FailoverPolicy,
    /// Notify when a window passes 80 % and 95 % (once per window period),
    /// under the notification settings' "Turn failed" switch.
    pub alert_80: bool,
    pub alert_95: bool,
    /// Read limits from the providers' usage endpoints while Orbit is in
    /// front. Off: only what running sessions report.
    pub poll: bool,
    pub pins: Vec<AccountPin>,
}

impl Default for UsageSettings {
    fn default() -> Self {
        Self { failover: FailoverPolicy::Ask, alert_80: true, alert_95: true, poll: true, pins: Vec::new() }
    }
}

impl UsageSettings {
    /// One pin per target, none naming an empty instance. Idempotent.
    pub fn sanitized(mut self) -> Self {
        let mut seen = Vec::new();
        for ix in (0..self.pins.len()).rev() {
            let pin = &self.pins[ix];
            if pin.instance.trim().is_empty() || seen.contains(&pin.target) {
                self.pins.remove(ix);
            } else {
                seen.push(pin.target);
            }
        }
        for pin in &mut self.pins {
            pin.instance = pin.instance.trim().to_owned();
        }
        self
    }

    /// The instance `target` is pinned to.
    pub fn pin(&self, target: PinTarget) -> Option<&str> {
        self.pins.iter().find(|p| p.target == target).map(|p| p.instance.as_str())
    }

    /// Pins `target` to `instance`, or unpins it with `None`.
    pub fn set_pin(&mut self, target: PinTarget, instance: Option<String>) {
        self.pins.retain(|p| p.target != target);
        if let Some(instance) = instance.filter(|i| !i.trim().is_empty()) {
            self.pins.push(AccountPin { target, instance });
        }
    }
}

// ── Stats ────────────────────────────────────────────────────────────────

/// One finished turn in the usage ledger.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct TurnUsage {
    /// When the turn ended (Unix ms).
    pub ended_ms: u64,
    pub thread: ThreadId,
    pub project: Option<ProjectId>,
    pub kind: ProviderKind,
    /// The provider instance (account) id.
    pub instance: String,
    /// The model slug; empty when the provider's default was used and its
    /// name was not known.
    pub model: String,
    pub usage: Usage,
}

/// Which turns [`UsageCommand::Stats`] and [`UsageCommand::Export`] cover.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct UsageQuery {
    /// Unix ms, inclusive; 0 from the beginning.
    pub from_ms: u64,
    /// Unix ms, exclusive; 0 until now.
    pub to_ms: u64,
    pub project: Option<ProjectId>,
    /// Only this provider instance (account).
    pub instance: Option<String>,
    /// The IANA time zone days are counted in; empty for the engine's.
    pub tz: String,
}

/// Token and cost totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct UsageTotals {
    pub turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Millionths of a US dollar, as the providers report it (0 for
    /// providers that report no cost).
    pub cost_micros: u64,
}

impl UsageTotals {
    pub fn add(&mut self, usage: &Usage) {
        self.turns += 1;
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_read_tokens += usage.cache_read_tokens;
        self.cache_write_tokens += usage.cache_write_tokens;
        self.cost_micros += usage.cost_micros;
    }

    pub fn merge(&mut self, other: &UsageTotals) {
        self.turns += other.turns;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.cost_micros += other.cost_micros;
    }

    /// Every token the turns moved: input, output and cache traffic.
    pub fn tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// One day's totals; `date` is `YYYY-MM-DD` in the query's time zone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct DayUsage {
    pub date: String,
    pub totals: UsageTotals,
}

/// Totals of one model, account, project or thread.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct UsageGroup {
    /// A stable key: the model slug, instance id, project id or thread id.
    pub key: String,
    /// Interface copy for the key.
    pub label: String,
    pub totals: UsageTotals,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct UsageStats {
    pub totals: UsageTotals,
    /// Every day of the range in order, empty days included (at most 400).
    pub days: Vec<DayUsage>,
    /// Groups sorted by tokens, most first.
    pub models: Vec<UsageGroup>,
    pub accounts: Vec<UsageGroup>,
    pub projects: Vec<UsageGroup>,
    /// The 50 threads that used the most.
    pub threads: Vec<UsageGroup>,
}

// ── Commands ─────────────────────────────────────────────────────────────

/// Quota, stats and failover; see the module documentation.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum UsageCommand {
    /// The quota board. Replies [`UsageReply::Board`].
    Board,
    /// Read limits now: one account's, or every account's that is due.
    /// Replies `Done`; the board follows as [`crate::Event::Quota`].
    Refresh { instance: Option<String> },
    /// A client's window came to the front. Limits are read while a client
    /// was in front recently, and stale ones are read now; nothing is
    /// polled while every window sits in the background.
    Seen,
    /// Replace the usage settings. Replies [`UsageReply::Board`].
    SetSettings(UsageSettings),
    /// Replies [`UsageReply::Stats`].
    Stats(UsageQuery),
    /// The query's turns as CSV, one row per turn. Replies `Text`.
    Export(UsageQuery),
    /// Continue a thread that hit its limit on another provider instance:
    /// the conversation is handed off and the limited turn sent again
    /// there. Replies `Done`.
    Failover { thread: ThreadId, instance: String },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum UsageReply {
    Board(QuotaBoard),
    Stats(UsageStats),
}

// ── Windows ──────────────────────────────────────────────────────────────

/// Interface copy and length of a limit window, from the id providers use
/// (`five_hour`, `seven_day_opus`, Codex's `5h`, `1w`, `codex 5h`).
pub fn describe_window(id: &str) -> (String, u64) {
    const HOUR: u64 = 3_600_000;
    let (prefix, span) = match id.rsplit_once(' ') {
        // Codex prefixes a named limit: `gpt-5-codex 5h`.
        Some((name, span)) if parse_span(span).is_some() => (Some(name), span),
        _ => (None, id),
    };
    let (label, ms) = match span {
        "five_hour" => ("5-hour".to_owned(), 5 * HOUR),
        "seven_day" => ("Weekly".to_owned(), 7 * 24 * HOUR),
        "extra_usage" => ("Extra usage".to_owned(), 0),
        s if s.starts_with("seven_day_") => {
            (format!("Weekly · {}", humanize_id(&s["seven_day_".len()..])), 7 * 24 * HOUR)
        }
        s => match parse_span(s) {
            Some(ms) if ms == 7 * 24 * HOUR => ("Weekly".to_owned(), ms),
            Some(ms) if ms == 24 * HOUR => ("Daily".to_owned(), ms),
            Some(ms) if ms % HOUR == 0 => (format!("{}-hour", ms / HOUR), ms),
            Some(ms) if ms % (24 * HOUR) == 0 => (format!("{}-day", ms / (24 * HOUR)), ms),
            Some(ms) => (format!("{}-minute", ms / 60_000), ms),
            None => (humanize_id(s), 0),
        },
    };
    match prefix {
        Some(name) => (format!("{label} · {name}"), ms),
        None => (label, ms),
    }
}

/// `5h` → 5 hours, `1w`, `7d`, `90m`, in milliseconds.
fn parse_span(text: &str) -> Option<u64> {
    let unit = text.chars().last()?;
    let n: u64 = text[..text.len() - unit.len_utf8()].parse().ok().filter(|n| *n > 0)?;
    let ms = match unit {
        'm' => 60_000,
        'h' => 3_600_000,
        'd' => 86_400_000,
        'w' => 7 * 86_400_000,
        _ => return None,
    };
    Some(n * ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};

    #[test]
    fn windows_read_as_interface_copy() {
        assert_eq!(describe_window("five_hour"), ("5-hour".into(), 5 * 3_600_000));
        assert_eq!(describe_window("seven_day"), ("Weekly".into(), 7 * 86_400_000));
        assert_eq!(describe_window("seven_day_opus").0, "Weekly · Opus");
        assert_eq!(describe_window("5h"), ("5-hour".into(), 5 * 3_600_000));
        assert_eq!(describe_window("1w").0, "Weekly");
        assert_eq!(describe_window("gpt-5-codex 5h").0, "5-hour · gpt-5-codex");
        assert_eq!(describe_window("primary"), ("Primary".into(), 0));
    }

    #[test]
    fn pins_keep_one_per_target() {
        let p = PinTarget::Project(ProjectId(1));
        let mut s = UsageSettings::default();
        s.set_pin(p, Some("claude-work".into()));
        s.set_pin(p, Some("claude".into()));
        s.pins.push(AccountPin { target: PinTarget::Automation(AutomationId(2)), instance: "  ".into() });
        s.pins.push(AccountPin { target: p, instance: "codex".into() });
        let s = s.sanitized();
        assert_eq!(s.pin(p), Some("codex"), "the last pin of a target wins");
        assert_eq!(s.pins.len(), 1);
        assert_eq!(s.clone().sanitized(), s);
        let mut s = s;
        s.set_pin(p, None);
        assert!(s.pins.is_empty());
    }

    #[test]
    fn the_board_round_trips_and_ranks_windows() {
        let window = |id: &str, u: f32, rejected| QuotaWindowState {
            window: QuotaWindow { window: id.into(), utilization: Some(u), resets_ms: 10, rejected },
            label: describe_window(id).0,
            span_ms: describe_window(id).1,
            burn_per_hour: Some(0.1),
            runs_out_ms: None,
        };
        let account = AccountQuota {
            instance: "claude".into(),
            windows: vec![window("five_hour", 0.4, false), window("seven_day", 0.7, false)],
            status: QuotaStatus::Ok,
            ..Default::default()
        };
        assert_eq!(account.tightest().unwrap().window.window, "seven_day");
        assert!((account.headroom() - 0.3).abs() < 1e-6);
        assert!(!account.limited(5));
        let mut limited = account.clone();
        limited.windows[0] = window("five_hour", 0.99, true);
        assert!(limited.limited(5));
        assert!(!limited.limited(11), "a reset window no longer limits");
        let board = QuotaBoard { accounts: vec![account, limited], settings: UsageSettings::default() };
        let back: QuotaBoard = decode(&encode(&board)).unwrap();
        assert_eq!(back, board);
    }
}
