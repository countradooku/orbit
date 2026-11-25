//! Telegram: the Bot API over long polling.
//!
//! `getUpdates` holds the request open for up to 25 seconds until a message
//! or button press arrives, so an idle bot costs one quiet request every
//! 25 seconds and no open port. Replies use Telegram's HTML subset with
//! inline keyboards; button presses are acknowledged at once so the
//! client's spinner stops.

use std::time::Duration;

use orbit_proto::*;
use sonic_rs::{JsonContainerTrait, JsonValueMutTrait, JsonValueTrait, Value, json};

use crate::chat::*;
use crate::net::{self, NetError};
use crate::render::{self, Flavor};
use crate::store::ChatKey;
use crate::transport::{ApiError, Ctx, OutRx, Outbox, Sender};

/// Characters per message.
pub const LIMIT: usize = 4096;
const POLL_SECONDS: u64 = 25;
/// Messages to one chat at most this often (Telegram's guidance).
const CHAT_INTERVAL: Duration = Duration::from_millis(1000);

#[derive(Clone)]
pub struct Api {
    base: String,
    pace: Duration,
}

impl Api {
    pub fn new(endpoints: &Endpoints, token: &str) -> Self {
        Self { base: format!("{}/bot{}", endpoints.telegram.trim_end_matches('/'), token), pace: endpoints.pace(CHAT_INTERVAL) }
    }

    async fn call(&self, method: &str, body: &Value, timeout: Duration) -> Result<Value, ApiError> {
        let body = sonic_rs::to_vec(body).unwrap_or_default();
        let url = format!("{}/{method}", self.base);
        let response = net::request("POST", &url, &[("content-type", "application/json".into())], Some(body), timeout)
            .await
            .map_err(|NetError(e)| ApiError::Offline(e))?;
        Self::result(response)
    }

    fn result(response: net::Response) -> Result<Value, ApiError> {
        let json = response.json().unwrap_or_default();
        if json.get("ok").and_then(|v| v.as_bool()) == Some(true) {
            return Ok(json.get("result").cloned().unwrap_or_default());
        }
        let description = json.get("description").and_then(|v| v.as_str()).unwrap_or("no details").to_owned();
        match response.status {
            401 | 404 => Err(ApiError::Rejected(format!("Telegram refused the token ({description})."))),
            429 => {
                let secs = json
                    .get("parameters")
                    .and_then(|p| p.get("retry_after"))
                    .and_then(|v| v.as_u64())
                    .or(response.retry_after)
                    .unwrap_or(1);
                Err(ApiError::Retry(Duration::from_secs(secs.max(1))))
            }
            409 => Err(ApiError::Failed(
                "Another program is reading this bot’s updates (or it has a webhook). Stop it, then Orbit retries.".into(),
            )),
            500.. => Err(ApiError::Offline(format!("Telegram is having trouble ({}).", response.status))),
            _ => Err(ApiError::Failed(description)),
        }
    }

    async fn multipart(&self, method: &str, fields: &[(&str, &str)], name: &str, bytes: &[u8]) -> Result<Value, ApiError> {
        let (content_type, body) = net::multipart(fields, "document", name, bytes);
        let url = format!("{}/{method}", self.base);
        let response = net::request("POST", &url, &[("content-type", content_type)], Some(body), Duration::from_secs(60))
            .await
            .map_err(|NetError(e)| ApiError::Offline(e))?;
        Self::result(response)
    }
}

fn keyboard(rows: &[Vec<Button>]) -> Value {
    let rows: Vec<Value> = rows
        .iter()
        .map(|row| Value::from(row.iter().map(|b| json!({"text": b.label, "callback_data": b.data})).collect::<Vec<_>>()))
        .collect();
    json!({"inline_keyboard": rows})
}

fn chat_id(chat: &ChatKey) -> Value {
    match chat.chat.parse::<i64>() {
        Ok(n) => json!(n),
        Err(_) => json!(chat.chat),
    }
}

impl Sender for Api {
    fn interval(&self) -> Duration {
        CHAT_INTERVAL
    }

    async fn send(&self, chat: &ChatKey, msg: &Msg) -> Result<Option<String>, ApiError> {
        let (chunks, files) = chunks(msg);
        let mut last = None;
        for (i, text) in chunks.iter().enumerate() {
            let mut body = json!({
                "chat_id": chat_id(chat),
                "text": text,
                "parse_mode": "HTML",
                "link_preview_options": {"is_disabled": true},
            });
            if i + 1 == chunks.len() && !msg.buttons.is_empty() {
                body.as_object_mut().unwrap().insert("reply_markup", keyboard(&msg.buttons));
            }
            let sent = match self.call("sendMessage", &body, Duration::from_secs(30)).await {
                // Markup Telegram can't parse: send the text as typed.
                Err(ApiError::Failed(e)) if e.contains("parse entities") => {
                    let obj = body.as_object_mut().unwrap();
                    obj.remove(&"parse_mode");
                    obj.insert("text", json!(strip_tags(text)));
                    self.call("sendMessage", &body, Duration::from_secs(30)).await?
                }
                other => other?,
            };
            last = sent.get("message_id").and_then(|v| v.as_i64()).map(|n| n.to_string());
            if i + 1 < chunks.len() {
                tokio::time::sleep(self.pace).await;
            }
        }
        for (name, bytes) in files {
            self.file(chat, &name, &bytes, "").await?;
        }
        Ok(last)
    }

    async fn edit(&self, chat: &ChatKey, id: &str, msg: &Msg) -> Result<(), ApiError> {
        let (chunks, _) = chunks(msg);
        let text = chunks.into_iter().next().unwrap_or_default();
        let body = json!({
            "chat_id": chat_id(chat),
            "message_id": id.parse::<i64>().unwrap_or_default(),
            "text": text,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
            "reply_markup": keyboard(&msg.buttons),
        });
        match self.call("editMessageText", &body, Duration::from_secs(30)).await {
            Err(ApiError::Failed(e)) if e.contains("not modified") => Ok(()),
            other => other.map(drop),
        }
    }

    async fn file(&self, chat: &ChatKey, name: &str, bytes: &[u8], caption: &str) -> Result<(), ApiError> {
        let id = chat.chat.clone();
        let caption: String = caption.chars().take(1000).collect();
        self.multipart("sendDocument", &[("chat_id", &id), ("caption", &caption)], name, bytes).await.map(drop)
    }
}

fn chunks(msg: &Msg) -> (Vec<String>, Vec<(String, Vec<u8>)>) {
    if msg.markdown {
        let r = render::render(&msg.text, Flavor::Html, LIMIT);
        (if r.chunks.is_empty() { vec!["…".into()] } else { r.chunks }, r.files)
    } else {
        let c = render::plain(&msg.text, Flavor::Html, LIMIT);
        (if c.is_empty() { vec!["…".into()] } else { c }, Vec::new())
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&amp;", "&")
}

/// Runs the bot until the task is dropped: checks the token, then polls.
pub async fn run(ctx: Ctx, out: OutRx, token: String) {
    let api = Api::new(&ctx.endpoints, &token);
    Outbox::spawn(api.clone(), &ctx, out);
    let mut attempt = 0u32;
    let mut offset: i64 = 0;
    let mut account = String::new();
    loop {
        if account.is_empty() {
            ctx.connection(BotConnection::Connecting, None);
            match api.call("getMe", &json!({}), Duration::from_secs(30)).await {
                Ok(me) => {
                    account = format!("@{}", me.get("username").and_then(|v| v.as_str()).unwrap_or("bot"));
                    ctx.connection(BotConnection::Connected, Some(account.clone()));
                    attempt = 0;
                }
                Err(ApiError::Rejected(e)) => {
                    ctx.rejected(e);
                    return;
                }
                Err(e) => {
                    attempt += 1;
                    ctx.retry(attempt, e.to_string()).await;
                    continue;
                }
            }
        }
        let body = json!({"offset": offset, "timeout": POLL_SECONDS, "allowed_updates": ["message", "callback_query"]});
        match api.call("getUpdates", &body, Duration::from_secs(POLL_SECONDS + 15)).await {
            Ok(updates) => {
                if attempt > 0 {
                    attempt = 0;
                    ctx.connection(BotConnection::Connected, Some(account.clone()));
                }
                for update in updates.as_array().map(|a| a.iter().collect::<Vec<_>>()).unwrap_or_default() {
                    if let Some(id) = update.get("update_id").and_then(|v| v.as_i64()) {
                        offset = offset.max(id + 1);
                    }
                    if let Some(incoming) = parse_update(update, &api).await {
                        ctx.incoming(incoming);
                    }
                }
            }
            Err(ApiError::Rejected(e)) => {
                ctx.rejected(e);
                return;
            }
            Err(ApiError::Retry(wait)) => tokio::time::sleep(wait).await,
            Err(e) => {
                attempt += 1;
                ctx.retry(attempt, e.to_string()).await;
            }
        }
    }
}

async fn parse_update(update: &Value, api: &Api) -> Option<Incoming> {
    if let Some(message) = update.get("message") {
        let text = message.get("text").and_then(|v| v.as_str())?.to_owned();
        let chat = message.get("chat")?;
        let from = message.get("from")?;
        if from.get("is_bot").and_then(|v| v.as_bool()) == Some(true) {
            return None;
        }
        return Some(Incoming {
            chat: ChatKey::new(Platform::Telegram, id_text(chat.get("id")?)),
            user_id: id_text(from.get("id")?),
            user_name: name_of(from),
            direct: chat.get("type").and_then(|v| v.as_str()) == Some("private"),
            kind: InKind::Text(text),
        });
    }
    let query = update.get("callback_query")?;
    let query_id = query.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
    // Stop the button's spinner at once; the result arrives as a message.
    let _ = api.call("answerCallbackQuery", &json!({"callback_query_id": query_id}), Duration::from_secs(10)).await;
    let from = query.get("from")?;
    let chat = query.get("message")?.get("chat")?;
    Some(Incoming {
        chat: ChatKey::new(Platform::Telegram, id_text(chat.get("id")?)),
        user_id: id_text(from.get("id")?),
        user_name: name_of(from),
        direct: chat.get("type").and_then(|v| v.as_str()) == Some("private"),
        kind: InKind::Button(query.get("data").and_then(|v| v.as_str()).unwrap_or_default().to_owned()),
    })
}

fn id_text(value: &Value) -> String {
    value.as_i64().map(|n| n.to_string()).or_else(|| value.as_str().map(str::to_owned)).unwrap_or_default()
}

fn name_of(user: &Value) -> String {
    let first = user.get("first_name").and_then(|v| v.as_str()).unwrap_or_default();
    let last = user.get("last_name").and_then(|v| v.as_str()).unwrap_or_default();
    let name = format!("{first} {last}").trim().to_owned();
    match user.get("username").and_then(|v| v.as_str()) {
        Some(handle) if name.is_empty() => format!("@{handle}"),
        _ if name.is_empty() => "Someone".to_owned(),
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_html_falls_back_to_text() {
        assert_eq!(strip_tags("<b>a &lt; b</b> &amp; <code>c</code>"), "a < b & c");
    }
}
