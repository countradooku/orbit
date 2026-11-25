//! Chat bots: driving Orbit's agents from Telegram, Slack and Discord.
//!
//! The bots run in the engine's process (the desktop app or `orbitd`) and
//! connect out to each platform: Telegram by long polling, Slack by Socket
//! Mode and Discord by its gateway, so the host never opens a port. Their
//! settings travel as [`BotCommand`]s; tokens are kept by the engine in its
//! integrations file (owner-only, never logged), and clients only ever see a
//! hint of them.

use bitcode::{Decode, Encode};

use crate::ids::{ProjectId, RequestId, ThreadId};
use crate::model::{Access, Decision};
use crate::wire::ThreadStart;

/// A chat platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum Platform {
    Telegram,
    Slack,
    Discord,
}

impl Platform {
    pub const ALL: [Platform; 3] = [Platform::Telegram, Platform::Slack, Platform::Discord];

    pub fn label(self) -> &'static str {
        match self {
            Platform::Telegram => "Telegram",
            Platform::Slack => "Slack",
            Platform::Discord => "Discord",
        }
    }

    /// The lowercase id the CLI takes.
    pub fn id(self) -> &'static str {
        match self {
            Platform::Telegram => "telegram",
            Platform::Slack => "slack",
            Platform::Discord => "discord",
        }
    }

    pub fn from_id(id: &str) -> Option<Platform> {
        Platform::ALL.into_iter().find(|p| p.id().eq_ignore_ascii_case(id.trim()))
    }
}

/// Where a command came from, when not from the app itself. The engine
/// records it in the thread's timeline ("Sent via Telegram by Ana") and
/// refuses guarded approvals from it (see [`crate::guard`]).
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Via {
    /// "Telegram", "Slack", "Discord".
    pub channel: String,
    /// The person's display name on that channel.
    pub user: String,
}

/// What a remote origin may do ([`crate::Command::Via`]): the few actions
/// a chat needs, nothing that edits settings, files or history.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum ViaAction {
    /// As `Command::Send`, without attachments.
    Send { thread: ThreadId, text: String, steer: bool },
    /// As `Command::Resolve`.
    Resolve { thread: ThreadId, request: RequestId, decision: Decision },
    /// As `Command::Interrupt`, or `Command::ForceStop` with `force`.
    Stop { thread: ThreadId, force: bool },
    /// As `Command::NewSession`.
    NewSession { thread: ThreadId },
    /// As `Command::StartThread`.
    Start(ThreadStart),
}

/// The connection of one platform.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum BotConnection {
    /// Turned off.
    #[default]
    Off,
    /// Turned on, but a token is missing.
    NeedsToken,
    Connecting,
    Connected,
    /// Lost or refused; trying again at `retry_ms` (Unix ms).
    Retrying { attempt: u32, retry_ms: u64 },
    /// The platform refused the token; nothing is retried until it changes.
    Rejected,
}

/// One platform as the settings page shows it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PlatformState {
    pub platform: Platform,
    pub enabled: bool,
    /// The last four characters of the saved token ("…x9Q2"), or empty.
    pub token_hint: String,
    /// Slack's app-level token (`xapp-…`) hint, for Socket Mode.
    pub app_token_hint: String,
    pub connection: BotConnection,
    /// The bot's own name on the platform once connected (`@orbit_bot`).
    pub account: String,
    /// The last error, kept after a recovery so it can still be read.
    pub error: Option<String>,
    pub error_ms: u64,
}

impl PlatformState {
    pub fn new(platform: Platform) -> Self {
        Self {
            platform,
            enabled: false,
            token_hint: String::new(),
            app_token_hint: String::new(),
            connection: BotConnection::Off,
            account: String::new(),
            error: None,
            error_ms: 0,
        }
    }

    /// Whether every token the platform needs is saved.
    pub fn has_tokens(&self) -> bool {
        !self.token_hint.is_empty() && (self.platform != Platform::Slack || !self.app_token_hint.is_empty())
    }
}

/// A person allowed to drive Orbit from a chat.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PairedUser {
    pub platform: Platform,
    /// The platform's stable user id.
    pub user_id: String,
    pub name: String,
    pub paired_ms: u64,
}

/// Someone who wrote to a bot and is waiting to be let in.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PairingRequest {
    pub platform: Platform,
    pub user_id: String,
    pub name: String,
    /// Six digits, shown to the sender and approved in the app or with
    /// `orbit bot pair <code>`.
    pub code: String,
    pub created_ms: u64,
    pub expires_ms: u64,
}

/// When needs-you events are pushed to paired chats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum BotNotify {
    Off,
    /// When no Orbit window is in front.
    #[default]
    WhenAway,
    Always,
}

/// How chats start and drive threads.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct BotOptions {
    /// Where `/new` without a project starts; `None` asks.
    pub project: Option<ProjectId>,
    /// Access of threads started from a chat. Never Full: Full is chosen
    /// per thread, in the app.
    pub access: Access,
    /// Start chat threads on their own worktree.
    pub worktree: bool,
    /// A message sent while the agent works steers the turn instead of
    /// waiting for it to end.
    pub steer: bool,
    pub notify: BotNotify,
}

impl Default for BotOptions {
    fn default() -> Self {
        Self { project: None, access: Access::Ask, worktree: false, steer: false, notify: BotNotify::WhenAway }
    }
}

/// Everything the Bots settings page shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct BotsState {
    pub platforms: Vec<PlatformState>,
    pub paired: Vec<PairedUser>,
    pub pending: Vec<PairingRequest>,
    pub options: BotOptions,
}

impl BotsState {
    pub fn platform(&self, platform: Platform) -> PlatformState {
        self.platforms.iter().find(|p| p.platform == platform).cloned().unwrap_or_else(|| PlatformState::new(platform))
    }
}

/// [`crate::Command::Bots`].
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum BotCommand {
    /// Replies [`BotReply::State`].
    State,
    Enable { platform: Platform, enabled: bool },
    /// Saves a platform's tokens; an empty string keeps the saved one and
    /// `clear` removes both. `app_token` is Slack's `xapp-…` token.
    SetTokens { platform: Platform, token: String, app_token: String, clear: bool },
    SetOptions(BotOptions),
    /// Lets the sender of a pairing code in. Replies [`BotReply::Paired`].
    Approve { code: String },
    Reject { code: String },
    /// Adds a person by their platform user id, without a code.
    Allow { platform: Platform, user_id: String, name: String },
    /// Removes a paired person; their chats stop working at once.
    Revoke { platform: Platform, user_id: String },
    /// Whether an Orbit window is in front, for [`BotNotify::WhenAway`].
    AppActive(bool),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum BotReply {
    State(BotsState),
    Paired(PairedUser),
}

/// The platforms' tokens. Never sent to a client and never logged:
/// `Debug` redacts them.
#[derive(Clone, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct BotSecrets {
    pub telegram: String,
    /// Slack's bot token (`xoxb-…`) and app-level token (`xapp-…`).
    pub slack_bot: String,
    pub slack_app: String,
    pub discord: String,
}

impl BotSecrets {
    pub fn token(&self, platform: Platform) -> &str {
        match platform {
            Platform::Telegram => &self.telegram,
            Platform::Slack => &self.slack_bot,
            Platform::Discord => &self.discord,
        }
    }

    pub fn app_token(&self, platform: Platform) -> &str {
        if platform == Platform::Slack { &self.slack_app } else { "" }
    }

    /// Applies [`BotCommand::SetTokens`].
    pub fn set(&mut self, platform: Platform, token: &str, app_token: &str, clear: bool) {
        let (main, app) = match platform {
            Platform::Telegram => (&mut self.telegram, None),
            Platform::Slack => (&mut self.slack_bot, Some(&mut self.slack_app)),
            Platform::Discord => (&mut self.discord, None),
        };
        if clear {
            main.clear();
        } else if !token.trim().is_empty() {
            *main = token.trim().to_owned();
        }
        if let Some(app) = app {
            if clear {
                app.clear();
            } else if !app_token.trim().is_empty() {
                *app = app_token.trim().to_owned();
            }
        }
    }
}

/// "…x9Q2": the last four characters, enough to tell tokens apart.
pub fn token_hint(token: &str) -> String {
    if token.is_empty() {
        return String::new();
    }
    let tail: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail}")
}

impl std::fmt::Debug for BotSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secret = |s: &str| if s.is_empty() { "" } else { "<redacted>" };
        f.debug_struct("BotSecrets")
            .field("telegram", &secret(&self.telegram))
            .field("slack_bot", &secret(&self.slack_bot))
            .field("slack_app", &secret(&self.slack_app))
            .field("discord", &secret(&self.discord))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_print() {
        let secrets = BotSecrets { telegram: "123:abc".into(), ..Default::default() };
        let shown = format!("{secrets:?}");
        assert!(!shown.contains("abc"), "{shown}");
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn setting_tokens_keeps_or_clears() {
        let mut s = BotSecrets::default();
        s.set(Platform::Slack, " xoxb-1 ", "xapp-2", false);
        assert_eq!((s.slack_bot.as_str(), s.slack_app.as_str()), ("xoxb-1", "xapp-2"));
        s.set(Platform::Slack, "", "xapp-3", false);
        assert_eq!((s.slack_bot.as_str(), s.slack_app.as_str()), ("xoxb-1", "xapp-3"));
        s.set(Platform::Slack, "", "", true);
        assert!(s.slack_bot.is_empty() && s.slack_app.is_empty());
        assert_eq!(token_hint("123456:ABCDEFx9Q2"), "…x9Q2");
        assert_eq!(token_hint(""), "");
        assert_eq!(Platform::from_id("Slack"), Some(Platform::Slack));
    }
}
