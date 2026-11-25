//! What every platform transport shares: its context, the error kinds of
//! its API, and the outbox that sends at the platform's pace.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossfire::{AsyncRx, BlockingTxTrait, MTx, mpsc};
use orbit_proto::*;

use crate::chat::*;
use crate::store::ChatKey;

pub type OutRx = AsyncRx<mpsc::List<Out>>;

/// A transport's handles back to the hub.
#[derive(Clone)]
pub struct Ctx {
    pub platform: Platform,
    pub endpoints: Endpoints,
    pub inbound: MTx<mpsc::List<Incoming>>,
    pub reports: MTx<mpsc::List<Report>>,
}

impl Ctx {
    pub fn incoming(&self, incoming: Incoming) {
        let _ = self.inbound.send(incoming);
    }

    pub fn connection(&self, connection: BotConnection, account: Option<String>) {
        let _ = self.reports.send(Report::Connection { platform: self.platform, connection, account });
    }

    pub fn error(&self, error: impl Into<String>) {
        let _ = self.reports.send(Report::Error { platform: self.platform, error: error.into() });
    }

    /// The token was refused: say so and stop until it changes.
    pub fn rejected(&self, error: String) {
        self.error(error);
        self.connection(BotConnection::Rejected, None);
    }

    /// Reports the failure and waits out the backoff for `attempt`.
    pub async fn retry(&self, attempt: u32, error: String) {
        let wait = backoff(&self.endpoints, attempt);
        log::info!("{}: {error}; retrying in {:?}", self.platform.label(), wait);
        self.error(error);
        self.connection(BotConnection::Retrying { attempt, retry_ms: now_ms() + wait.as_millis() as u64 }, None);
        tokio::time::sleep(wait).await;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// The token is wrong or revoked.
    Rejected(String),
    /// Rate limited: try again after this long.
    Retry(Duration),
    /// The platform refused the request itself.
    Failed(String),
    /// Unreachable or failing; worth retrying later.
    Offline(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Rejected(e) | ApiError::Failed(e) | ApiError::Offline(e) => f.write_str(e),
            ApiError::Retry(wait) => write!(f, "Rate limited for {}s.", wait.as_secs().max(1)),
        }
    }
}

/// A platform's sending half.
pub trait Sender {
    /// The least time between two messages to one chat.
    fn interval(&self) -> Duration;
    /// Sends `msg` (in as many chunks as it needs); returns the id of the
    /// message that carries its buttons, to edit later.
    async fn send(&self, chat: &ChatKey, msg: &Msg) -> Result<Option<String>, ApiError>;
    async fn edit(&self, chat: &ChatKey, id: &str, msg: &Msg) -> Result<(), ApiError>;
    async fn file(&self, chat: &ChatKey, name: &str, bytes: &[u8], caption: &str) -> Result<(), ApiError>;
}

/// Sends a platform's messages in order, no faster than it allows, and
/// waits out its rate limits (up to three times per message).
pub struct Outbox;

impl Outbox {
    pub fn spawn<S: Sender + 'static>(sender: S, ctx: &Ctx, out: OutRx) -> Outbox {
        let ctx = ctx.clone();
        tokio::task::spawn_local(async move {
            let mut last: HashMap<ChatKey, Instant> = HashMap::new();
            while let Ok(job) = out.recv().await {
                let chat = match &job {
                    Out::Send { chat, .. } | Out::Edit { chat, .. } | Out::File { chat, .. } => chat.clone(),
                };
                if let Some(at) = last.get(&chat) {
                    let due = *at + ctx.endpoints.pace(sender.interval());
                    if due > Instant::now() {
                        tokio::time::sleep_until(due.into()).await;
                    }
                }
                let mut tries = 0;
                loop {
                    tries += 1;
                    let result = match &job {
                        Out::Send { chat, msg, tag } => sender.send(chat, msg).await.map(|id| {
                            if let (Some(tag), Some(id)) = (tag, id) {
                                let _ = ctx.reports.send(Report::Sent { tag: *tag, chat: chat.clone(), id });
                            }
                        }),
                        Out::Edit { chat, id, msg } => sender.edit(chat, id, msg).await,
                        Out::File { chat, name, bytes, caption } => sender.file(chat, name, bytes, caption).await,
                    };
                    match result {
                        Err(ApiError::Retry(wait)) if tries < 4 => tokio::time::sleep(wait).await,
                        Err(ApiError::Offline(_)) if tries < 3 => tokio::time::sleep(backoff(&ctx.endpoints, tries)).await,
                        Err(e) => {
                            log::warn!("{}: couldn’t deliver a message: {e}", ctx.platform.label());
                            ctx.error(format!("Couldn’t deliver a message: {e}"));
                            break;
                        }
                        Ok(()) => break,
                    }
                }
                last.insert(chat, Instant::now());
            }
        });
        Outbox
    }
}
