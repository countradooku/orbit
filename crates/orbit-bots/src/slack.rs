//! Slack: Socket Mode over a websocket, the Web API for replies.
//!
//! `apps.connections.open` (with the app-level `xapp-` token) hands out a
//! websocket URL; events arrive on it as envelopes that must be
//! acknowledged within three seconds, so each is acknowledged before it is
//! handled. In channels the bot answers @mentions, in a thread under the
//! message; in its direct messages it answers everything. Replies use
//! `mrkdwn` section blocks and buttons. Slack asks clients to reconnect
//! now and then (`disconnect`); that is not an error.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use orbit_proto::*;
use sonic_rs::{JsonContainerTrait, JsonValueMutTrait, JsonValueTrait, Value, json};

use crate::chat::*;
use crate::net::{self, NetError};
use crate::render::{self, Flavor};
use crate::store::ChatKey;
use crate::transport::{ApiError, Ctx, OutRx, Outbox, Sender};

/// Characters per section block.
pub const SECTION_LIMIT: usize = 3000;
/// Section blocks per message (Slack allows 50 blocks).
const SECTIONS_PER_MESSAGE: usize = 10;
const CHAT_INTERVAL: Duration = Duration::from_millis(1000);

#[derive(Clone)]
pub struct Api {
    base: String,
    token: String,
    /// Display names by user id.
    names: Rc<RefCell<HashMap<String, String>>>,
}

impl Api {
    pub fn new(endpoints: &Endpoints, token: &str) -> Self {
        Self { base: endpoints.slack.trim_end_matches('/').to_owned(), token: token.to_owned(), names: Rc::default() }
    }

    async fn call(&self, method: &str, body: &Value) -> Result<Value, ApiError> {
        self.call_with(method, &self.token, body).await
    }

    async fn call_with(&self, method: &str, token: &str, body: &Value) -> Result<Value, ApiError> {
        let headers = [
            ("authorization", format!("Bearer {token}")),
            ("content-type", "application/json; charset=utf-8".to_owned()),
        ];
        let url = format!("{}/{method}", self.base);
        let response = net::request("POST", &url, &headers, Some(sonic_rs::to_vec(body).unwrap_or_default()), Duration::from_secs(30))
            .await
            .map_err(|NetError(e)| ApiError::Offline(e))?;
        Self::result(response)
    }

    async fn form(&self, method: &str, pairs: &[(&str, &str)]) -> Result<Value, ApiError> {
        let headers = [
            ("authorization", format!("Bearer {}", self.token)),
            ("content-type", "application/x-www-form-urlencoded".to_owned()),
        ];
        let url = format!("{}/{method}", self.base);
        let response = net::request("POST", &url, &headers, Some(net::form(pairs)), Duration::from_secs(30))
            .await
            .map_err(|NetError(e)| ApiError::Offline(e))?;
        Self::result(response)
    }

    fn result(response: net::Response) -> Result<Value, ApiError> {
        if response.status == 429 {
            return Err(ApiError::Retry(Duration::from_secs(response.retry_after.unwrap_or(1).max(1))));
        }
        if response.status >= 500 {
            return Err(ApiError::Offline(format!("Slack is having trouble ({}).", response.status)));
        }
        let json = response.json().unwrap_or_default();
        if json.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            return Ok(json);
        }
        let error = json.get("error").and_then(|v| v.as_str()).unwrap_or("unknown_error").to_owned();
        match error.as_str() {
            "invalid_auth" | "not_authed" | "account_inactive" | "token_revoked" | "token_expired" | "not_allowed_token_type" => {
                Err(ApiError::Rejected(format!("Slack refused the token ({error}).")))
            }
            "ratelimited" => Err(ApiError::Retry(Duration::from_secs(response.retry_after.unwrap_or(1).max(1)))),
            _ => Err(ApiError::Failed(format!("Slack said {error}."))),
        }
    }

    async fn name_of(&self, user: &str) -> String {
        if let Some(name) = self.names.borrow().get(user) {
            return name.clone();
        }
        let name = match self.form("users.info", &[("user", user)]).await {
            Ok(info) => {
                let u = info.get("user");
                let profile = u.and_then(|u| u.get("profile"));
                profile
                    .and_then(|p| p.get("display_name"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .or_else(|| profile.and_then(|p| p.get("real_name")).and_then(|v| v.as_str()))
                    .or_else(|| u.and_then(|u| u.get("name")).and_then(|v| v.as_str()))
                    .unwrap_or(user)
                    .to_owned()
            }
            Err(_) => user.to_owned(),
        };
        self.names.borrow_mut().insert(user.to_owned(), name.clone());
        name
    }

    fn blocks(msg: &Msg) -> Vec<(String, Vec<Value>)> {
        let chunks = if msg.markdown {
            render::render(&msg.text, Flavor::Mrkdwn, SECTION_LIMIT).chunks
        } else {
            render::plain(&msg.text, Flavor::Mrkdwn, SECTION_LIMIT)
        };
        let chunks = if chunks.is_empty() { vec!["…".to_owned()] } else { chunks };
        let groups: Vec<&[String]> = chunks.chunks(SECTIONS_PER_MESSAGE).collect();
        let count = groups.len();
        groups
            .into_iter()
            .enumerate()
            .map(|(i, group)| {
                let mut blocks: Vec<Value> =
                    group.iter().map(|text| json!({"type": "section", "text": {"type": "mrkdwn", "text": text}})).collect();
                if i + 1 == count {
                    for (r, row) in msg.buttons.iter().enumerate() {
                        let elements: Vec<Value> = row
                            .iter()
                            .enumerate()
                            .map(|(b, button)| {
                                let label: String = button.label.chars().take(75).collect();
                                let mut el = json!({
                                    "type": "button",
                                    "text": {"type": "plain_text", "text": label},
                                    "action_id": format!("orbit_{r}_{b}"),
                                    "value": button.data,
                                });
                                match button.style {
                                    Style::Primary => {
                                        el.as_object_mut().unwrap().insert("style", json!("primary"));
                                    }
                                    Style::Danger => {
                                        el.as_object_mut().unwrap().insert("style", json!("danger"));
                                    }
                                    Style::Normal => {}
                                }
                                el
                            })
                            .collect();
                        blocks.push(json!({"type": "actions", "elements": elements}));
                    }
                }
                // The notification text: the first section, cut.
                let fallback: String = group.first().map(|t| t.chars().take(300).collect()).unwrap_or_default();
                (fallback, blocks)
            })
            .collect()
    }
}

impl Sender for Api {
    fn interval(&self) -> Duration {
        CHAT_INTERVAL
    }

    async fn send(&self, chat: &ChatKey, msg: &Msg) -> Result<Option<String>, ApiError> {
        let mut last = None;
        let files = if msg.markdown { render::render(&msg.text, Flavor::Mrkdwn, SECTION_LIMIT).files } else { Vec::new() };
        for (text, blocks) in Self::blocks(msg) {
            let mut body = json!({"channel": chat.chat, "text": text, "blocks": blocks, "unfurl_links": false});
            if !chat.thread.is_empty() {
                body.as_object_mut().unwrap().insert("thread_ts", json!(chat.thread));
            }
            let sent = self.call("chat.postMessage", &body).await?;
            last = sent.get("ts").and_then(|v| v.as_str()).map(str::to_owned);
        }
        for (name, bytes) in files {
            self.file(chat, &name, &bytes, "").await?;
        }
        Ok(last)
    }

    async fn edit(&self, chat: &ChatKey, id: &str, msg: &Msg) -> Result<(), ApiError> {
        let (text, blocks) = Self::blocks(msg).into_iter().next().unwrap_or_default();
        let body = json!({"channel": chat.chat, "ts": id, "text": text, "blocks": blocks});
        self.call("chat.update", &body).await.map(drop)
    }

    async fn file(&self, chat: &ChatKey, name: &str, bytes: &[u8], caption: &str) -> Result<(), ApiError> {
        let length = bytes.len().to_string();
        let ticket = self.form("files.getUploadURLExternal", &[("filename", name), ("length", &length)]).await?;
        let (Some(url), Some(id)) = (
            ticket.get("upload_url").and_then(|v| v.as_str()).map(str::to_owned),
            ticket.get("file_id").and_then(|v| v.as_str()).map(str::to_owned),
        ) else {
            return Err(ApiError::Failed("Slack gave no upload address.".into()));
        };
        let uploaded = net::request(
            "POST",
            &url,
            &[("content-type", "application/octet-stream".to_owned())],
            Some(bytes.to_vec()),
            Duration::from_secs(60),
        )
        .await
        .map_err(|NetError(e)| ApiError::Offline(e))?;
        if uploaded.status >= 400 {
            return Err(ApiError::Failed(format!("Slack refused the upload ({}).", uploaded.status)));
        }
        let mut body = json!({"files": [{"id": id, "title": name}], "channel_id": chat.chat});
        let obj = body.as_object_mut().unwrap();
        if !chat.thread.is_empty() {
            obj.insert("thread_ts", json!(chat.thread));
        }
        if !caption.is_empty() {
            obj.insert("initial_comment", json!(caption));
        }
        self.call("files.completeUploadExternal", &body).await.map(drop)
    }
}

/// Runs the bot until the task is dropped.
pub async fn run(ctx: Ctx, out: OutRx, bot_token: String, app_token: String) {
    let api = Api::new(&ctx.endpoints, &bot_token);
    Outbox::spawn(api.clone(), &ctx, out);
    let mut attempt = 0u32;
    let mut me: Option<(String, String)> = None;
    loop {
        ctx.connection(BotConnection::Connecting, None);
        if me.is_none() {
            match api.call("auth.test", &json!({})).await {
                Ok(auth) => {
                    let id = auth.get("user_id").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
                    let name = auth.get("user").and_then(|v| v.as_str()).unwrap_or("orbit").to_owned();
                    me = Some((id, name));
                }
                Err(ApiError::Rejected(e)) => return ctx.rejected(e),
                Err(e) => {
                    attempt += 1;
                    ctx.retry(attempt, e.to_string()).await;
                    continue;
                }
            }
        }
        let url = match api.call_with("apps.connections.open", &app_token, &json!({})).await {
            Ok(open) => open.get("url").and_then(|v| v.as_str()).unwrap_or_default().to_owned(),
            Err(ApiError::Rejected(e)) => return ctx.rejected(format!("{e} Check the app-level token (xapp-…) and its connections:write scope.")),
            Err(e) => {
                attempt += 1;
                ctx.retry(attempt, e.to_string()).await;
                continue;
            }
        };
        let (mut reader, mut writer) = match net::ws_connect(&url).await {
            Ok(ws) => ws,
            Err(NetError(e)) => {
                attempt += 1;
                ctx.retry(attempt, e).await;
                continue;
            }
        };
        let (bot_id, bot_name) = me.clone().unwrap_or_default();
        let mut refresh = false;
        while let Some(frame) = net::ws_next(&mut reader).await {
            let text = match frame {
                Ok(text) => text,
                Err(e) => {
                    log::info!("Slack: socket {e}");
                    break;
                }
            };
            let Ok(envelope) = sonic_rs::from_str::<Value>(&text) else { continue };
            match envelope.get("type").and_then(|v| v.as_str()).unwrap_or_default() {
                "hello" => {
                    attempt = 0;
                    ctx.connection(BotConnection::Connected, Some(format!("@{bot_name}")));
                    continue;
                }
                // Slack rotates sockets; open a new one now.
                "disconnect" => {
                    refresh = true;
                    break;
                }
                _ => {}
            }
            if let Some(id) = envelope.get("envelope_id").and_then(|v| v.as_str()) {
                let ack = sonic_rs::to_string(&json!({"envelope_id": id})).unwrap_or_default();
                if net::ws_send(&mut writer, ack).await.is_err() {
                    break;
                }
            }
            if let Some(incoming) = parse_envelope(&envelope, &bot_id, &api).await {
                ctx.incoming(incoming);
            }
        }
        if !refresh {
            attempt += 1;
            ctx.retry(attempt, "Lost the Slack connection.".into()).await;
        }
    }
}

async fn parse_envelope(envelope: &Value, bot_id: &str, api: &Api) -> Option<Incoming> {
    let payload = envelope.get("payload")?;
    match envelope.get("type").and_then(|v| v.as_str())? {
        "events_api" => {
            let event = payload.get("event")?;
            let kind = event.get("type").and_then(|v| v.as_str())?;
            if event.get("bot_id").is_some() || event.get("subtype").is_some() {
                return None;
            }
            let user = event.get("user").and_then(|v| v.as_str())?.to_owned();
            if user == bot_id {
                return None;
            }
            let channel = event.get("channel").and_then(|v| v.as_str())?.to_owned();
            let ts = event.get("ts").and_then(|v| v.as_str()).unwrap_or_default();
            let thread_ts = event.get("thread_ts").and_then(|v| v.as_str());
            let text = strip_mention(event.get("text").and_then(|v| v.as_str()).unwrap_or_default(), bot_id);
            let (chat, direct) = match kind {
                // A mention in a channel: answer in a thread under it.
                "app_mention" => (ChatKey::new(Platform::Slack, channel).in_thread(thread_ts.unwrap_or(ts)), false),
                "message" if event.get("channel_type").and_then(|v| v.as_str()) == Some("im") => {
                    (ChatKey::new(Platform::Slack, channel).in_thread(thread_ts.unwrap_or_default()), true)
                }
                _ => return None,
            };
            let name = api.name_of(&user).await;
            Some(Incoming { chat, user_id: user, user_name: name, direct, kind: InKind::Text(unescape(&text)) })
        }
        "interactive" => {
            if payload.get("type").and_then(|v| v.as_str()) != Some("block_actions") {
                return None;
            }
            let user = payload.get("user")?;
            let user_id = user.get("id").and_then(|v| v.as_str())?.to_owned();
            let channel = payload.get("channel").and_then(|c| c.get("id")).and_then(|v| v.as_str())?.to_owned();
            let thread = payload
                .get("message")
                .and_then(|m| m.get("thread_ts"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            let data = payload.get("actions")?.as_array()?.iter().next()?.get("value").and_then(|v| v.as_str())?.to_owned();
            let name = api.name_of(&user_id).await;
            let direct = channel.starts_with('D');
            Some(Incoming {
                chat: ChatKey::new(Platform::Slack, channel).in_thread(thread),
                user_id,
                user_name: name,
                direct,
                kind: InKind::Button(data),
            })
        }
        "slash_commands" => {
            let user_id = payload.get("user_id").and_then(|v| v.as_str())?.to_owned();
            let channel = payload.get("channel_id").and_then(|v| v.as_str())?.to_owned();
            let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or_default().trim();
            let name = api.name_of(&user_id).await;
            let direct = channel.starts_with('D');
            Some(Incoming {
                chat: ChatKey::new(Platform::Slack, channel),
                user_id,
                user_name: name,
                direct,
                kind: InKind::Text(if text.is_empty() { "/help".into() } else { format!("/{text}") }),
            })
        }
        _ => None,
    }
}

fn strip_mention(text: &str, bot_id: &str) -> String {
    text.replace(&format!("<@{bot_id}>"), "").trim().to_owned()
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_and_escapes_are_undone() {
        assert_eq!(strip_mention("<@U1> fix it", "U1"), "fix it");
        assert_eq!(unescape("a &lt;b&gt; &amp; c"), "a <b> & c");
        let msg = Msg::plain("x".repeat(SECTION_LIMIT * 12))
            .with_buttons(vec![vec![Button::new("Yes", "y", Style::Primary)]]);
        let messages = Api::blocks(&msg);
        assert_eq!(messages.len(), 2, "ten sections a message");
        assert_eq!(messages[0].1.len(), 10);
        assert_eq!(messages[1].1.last().unwrap().get("type").and_then(|v| v.as_str()), Some("actions"));
    }
}
