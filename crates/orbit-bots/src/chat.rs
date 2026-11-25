//! What passes between the hub and the platform transports.

use orbit_proto::*;

use crate::store::ChatKey;

/// A message or button press from a platform.
#[derive(Clone, Debug)]
pub struct Incoming {
    pub chat: ChatKey,
    pub user_id: String,
    pub user_name: String,
    /// A direct chat with the bot (not a channel or group).
    pub direct: bool,
    pub kind: InKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InKind {
    Text(String),
    /// A button's `data`. The transport has already acknowledged it.
    Button(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Normal,
    Primary,
    Danger,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Button {
    pub label: String,
    /// Sent back as [`InKind::Button`]; at most 64 bytes (Telegram's limit).
    pub data: String,
    pub style: Style,
}

impl Button {
    pub fn new(label: impl Into<String>, data: impl Into<String>, style: Style) -> Self {
        Self { label: label.into(), data: data.into(), style }
    }
}

/// A message to show: markdown as the agents write it, or plain text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Msg {
    pub text: String,
    /// Interpret `text` as markdown (agent answers); otherwise it is shown
    /// as typed.
    pub markdown: bool,
    /// Rows of buttons under the (last chunk of the) message.
    pub buttons: Vec<Vec<Button>>,
}

impl Msg {
    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), markdown: false, buttons: Vec::new() }
    }

    pub fn markdown(text: impl Into<String>) -> Self {
        Self { text: text.into(), markdown: true, buttons: Vec::new() }
    }

    pub fn with_buttons(mut self, rows: Vec<Vec<Button>>) -> Self {
        self.buttons = rows;
        self
    }
}

/// Which message a send creates, so the hub can edit it later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tag {
    /// The live status line of a thread's turn.
    Status(ThreadId),
    /// The approval or question message of a request.
    Request(RequestId),
}

/// Work for a platform's outbox.
#[derive(Clone, Debug)]
pub enum Out {
    Send { chat: ChatKey, msg: Msg, tag: Option<Tag> },
    Edit { chat: ChatKey, id: String, msg: Msg },
    File { chat: ChatKey, name: String, bytes: Vec<u8>, caption: String },
}

/// What transports tell the hub besides messages.
#[derive(Clone, Debug)]
pub enum Report {
    /// A tagged message was sent; `id` edits it.
    Sent { tag: Tag, chat: ChatKey, id: String },
    Connection { platform: Platform, connection: BotConnection, account: Option<String> },
    Error { platform: Platform, error: String },
}

/// Where each platform's API lives. Tests point them at local fakes.
#[derive(Clone, Debug)]
pub struct Endpoints {
    /// `https://api.telegram.org`; requests go to `<base>/bot<token>/<method>`.
    pub telegram: String,
    /// `https://slack.com/api`.
    pub slack: String,
    /// `https://discord.com/api/v10`.
    pub discord: String,
    /// Retry delays start here and double up to `backoff_max`.
    pub backoff_min_ms: u64,
    pub backoff_max_ms: u64,
    /// Overrides each platform's least time between messages to one chat
    /// (tests run faster than Telegram's one a second).
    pub pace_ms: Option<u64>,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            telegram: "https://api.telegram.org".into(),
            slack: "https://slack.com/api".into(),
            discord: "https://discord.com/api/v10".into(),
            backoff_min_ms: 1_000,
            backoff_max_ms: 60_000,
            pace_ms: None,
        }
    }
}

impl Endpoints {
    /// The least time between two messages to one chat.
    pub fn pace(&self, platform_default: std::time::Duration) -> std::time::Duration {
        self.pace_ms.map_or(platform_default, std::time::Duration::from_millis)
    }
}

/// The delay before retry `attempt` (1-based): doubling from the minimum,
/// capped, with up to a quarter of jitter so many hosts don't retry in step.
pub fn backoff(endpoints: &Endpoints, attempt: u32) -> std::time::Duration {
    let base = endpoints.backoff_min_ms.saturating_mul(1u64 << attempt.saturating_sub(1).min(16));
    let capped = base.min(endpoints.backoff_max_ms);
    let jitter = capped / 4 * (now_ms() % 1000) / 1000;
    std::time::Duration::from_millis(capped - jitter)
}
