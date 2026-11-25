//! Discord: the gateway websocket for events, REST for replies.
//!
//! The gateway says hello with a heartbeat interval; the bot identifies
//! (or resumes a dropped session from the last sequence number it saw)
//! and heartbeats on time. A heartbeat that goes unacknowledged means a
//! dead connection: the bot reconnects and resumes, so nothing said in
//! between is lost. In servers the bot answers @mentions; in direct
//! messages, everything. Button presses arrive as interactions and are
//! acknowledged at once.

use std::time::Duration;

use orbit_proto::*;
use sonic_rs::{JsonContainerTrait, JsonValueMutTrait, JsonValueTrait, Value, json};

use crate::chat::*;
use crate::net::{self, NetError, WsWriter};
use crate::render::{self, Flavor};
use crate::store::ChatKey;
use crate::transport::{ApiError, Ctx, OutRx, Outbox, Sender};

/// Characters per message: an embed's description holds 4096.
pub const LIMIT: usize = 4000;
/// Plain message content holds 2000.
const CONTENT_LIMIT: usize = 2000;
const CHAT_INTERVAL: Duration = Duration::from_millis(300);
/// GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT.
pub const INTENTS: u64 = (1 << 9) | (1 << 12) | (1 << 15);
const USER_AGENT: &str = concat!("DiscordBot (https://github.com/orbit-dev/orbit, ", env!("CARGO_PKG_VERSION"), ")");

#[derive(Clone)]
pub struct Api {
    base: String,
    token: String,
}

impl Api {
    pub fn new(endpoints: &Endpoints, token: &str) -> Self {
        Self { base: endpoints.discord.trim_end_matches('/').to_owned(), token: token.to_owned() }
    }

    fn headers(&self, content_type: &str) -> [(&'static str, String); 3] {
        [
            ("authorization", format!("Bot {}", self.token)),
            ("content-type", content_type.to_owned()),
            ("user-agent", USER_AGENT.to_owned()),
        ]
    }

    async fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        let body = body.map(|b| sonic_rs::to_vec(b).unwrap_or_default());
        let url = format!("{}{path}", self.base);
        let response = net::request(method, &url, &self.headers("application/json"), body, Duration::from_secs(30))
            .await
            .map_err(|NetError(e)| ApiError::Offline(e))?;
        Self::result(response)
    }

    fn result(response: net::Response) -> Result<Value, ApiError> {
        let json = response.json().unwrap_or_default();
        match response.status {
            200..=299 => Ok(json),
            401 => Err(ApiError::Rejected("Discord refused the bot token.".into())),
            429 => {
                let secs = json.get("retry_after").and_then(|v| v.as_f64()).unwrap_or(1.0);
                Err(ApiError::Retry(Duration::from_millis((secs * 1000.0).ceil().max(100.0) as u64)))
            }
            403 => Err(ApiError::Failed("Discord says the bot lacks permission in that channel.".into())),
            500.. => Err(ApiError::Offline(format!("Discord is having trouble ({}).", response.status))),
            status => Err(ApiError::Failed(format!(
                "Discord refused the request ({status}: {}).",
                json.get("message").and_then(|v| v.as_str()).unwrap_or("no details")
            ))),
        }
    }

    fn payloads(msg: &Msg) -> Vec<Value> {
        let rendered = if msg.markdown {
            render::render(&msg.text, Flavor::Markdown, LIMIT).chunks
        } else {
            render::plain(&msg.text, Flavor::Markdown, LIMIT)
        };
        let chunks = if rendered.is_empty() { vec!["…".to_owned()] } else { rendered };
        let count = chunks.len();
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, text)| {
                let mut body = if text.chars().count() <= CONTENT_LIMIT {
                    json!({"content": text, "embeds": []})
                } else {
                    json!({"content": "", "embeds": [{"description": text}]})
                };
                let obj = body.as_object_mut().unwrap();
                obj.insert("allowed_mentions", json!({"parse": []}));
                let rows: Vec<Value> = if i + 1 == count { components(&msg.buttons) } else { Vec::new() };
                obj.insert("components", Value::from(rows));
                body
            })
            .collect()
    }

    async fn ack(&self, id: &str, token: &str) {
        let path = format!("/interactions/{id}/{token}/callback");
        if let Err(e) = self.call("POST", &path, Some(&json!({"type": 6}))).await {
            log::info!("Discord: couldn’t acknowledge a button: {e}");
        }
    }
}

fn components(rows: &[Vec<Button>]) -> Vec<Value> {
    rows.iter()
        .flat_map(|row| row.chunks(5))
        .take(5)
        .map(|row| {
            let buttons: Vec<Value> = row
                .iter()
                .map(|b| {
                    let style = match b.style {
                        Style::Primary => 1,
                        Style::Normal => 2,
                        Style::Danger => 4,
                    };
                    let label: String = b.label.chars().take(80).collect();
                    json!({"type": 2, "style": style, "label": label, "custom_id": b.data})
                })
                .collect();
            json!({"type": 1, "components": buttons})
        })
        .collect()
}

impl Sender for Api {
    fn interval(&self) -> Duration {
        CHAT_INTERVAL
    }

    async fn send(&self, chat: &ChatKey, msg: &Msg) -> Result<Option<String>, ApiError> {
        let mut last = None;
        let path = format!("/channels/{}/messages", chat.chat);
        for body in Self::payloads(msg) {
            let sent = self.call("POST", &path, Some(&body)).await?;
            last = sent.get("id").and_then(|v| v.as_str()).map(str::to_owned);
        }
        if msg.markdown {
            for (name, bytes) in render::render(&msg.text, Flavor::Markdown, LIMIT).files {
                self.file(chat, &name, &bytes, "").await?;
            }
        }
        Ok(last)
    }

    async fn edit(&self, chat: &ChatKey, id: &str, msg: &Msg) -> Result<(), ApiError> {
        let body = Self::payloads(msg).into_iter().next().unwrap_or_default();
        self.call("PATCH", &format!("/channels/{}/messages/{id}", chat.chat), Some(&body)).await.map(drop)
    }

    async fn file(&self, chat: &ChatKey, name: &str, bytes: &[u8], caption: &str) -> Result<(), ApiError> {
        let payload = sonic_rs::to_string(&json!({
            "content": caption.chars().take(CONTENT_LIMIT).collect::<String>(),
            "allowed_mentions": {"parse": []},
            "attachments": [{"id": 0, "filename": name}],
        }))
        .unwrap_or_default();
        let (content_type, body) = net::multipart(&[("payload_json", &payload)], "files[0]", name, bytes);
        let url = format!("{}/channels/{}/messages", self.base, chat.chat);
        let response = net::request("POST", &url, &self.headers(&content_type), Some(body), Duration::from_secs(60))
            .await
            .map_err(|NetError(e)| ApiError::Offline(e))?;
        Self::result(response).map(drop)
    }
}

/// A resumable gateway session.
#[derive(Default)]
struct Session {
    id: String,
    seq: Option<u64>,
    resume_url: String,
}

enum Ended {
    /// Reconnect and resume at once (op 7, a dead heartbeat).
    Resume,
    /// The session is gone; identify afresh after a short pause.
    Reidentify,
    /// Connection lost; back off, then resume.
    Lost(String),
    /// The token or setup is wrong; stop.
    Fatal(String, bool),
}

/// Runs the bot until the task is dropped.
pub async fn run(ctx: Ctx, out: OutRx, token: String) {
    let api = Api::new(&ctx.endpoints, &token);
    Outbox::spawn(api.clone(), &ctx, out);
    let mut attempt = 0u32;
    let mut me: Option<(String, String)> = None;
    let mut session = Session::default();
    let mut gateway = String::new();
    loop {
        ctx.connection(BotConnection::Connecting, None);
        if me.is_none() {
            match api.call("GET", "/users/@me", None).await {
                Ok(user) => {
                    let id = user.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
                    let name = user.get("username").and_then(|v| v.as_str()).unwrap_or("orbit").to_owned();
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
        if gateway.is_empty() {
            match api.call("GET", "/gateway/bot", None).await {
                Ok(info) => gateway = info.get("url").and_then(|v| v.as_str()).unwrap_or_default().to_owned(),
                Err(ApiError::Rejected(e)) => return ctx.rejected(e),
                Err(e) => {
                    attempt += 1;
                    ctx.retry(attempt, e.to_string()).await;
                    continue;
                }
            }
        }
        let resuming = !session.id.is_empty();
        let base = if resuming && !session.resume_url.is_empty() { &session.resume_url } else { &gateway };
        let url = format!("{}/?v=10&encoding=json", base.trim_end_matches('/'));
        let (me_id, me_name) = me.clone().unwrap_or_default();
        let ended = match net::ws_connect(&url).await {
            Ok((reader, writer)) => {
                connection(&ctx, &api, &token, &me_id, &me_name, reader, writer, &mut session, &mut attempt).await
            }
            Err(NetError(e)) => Ended::Lost(e),
        };
        match ended {
            Ended::Resume => {}
            Ended::Reidentify => {
                session = Session::default();
                tokio::time::sleep(Duration::from_millis(1000 + now_ms() % 4000)).await;
            }
            Ended::Lost(e) => {
                attempt += 1;
                ctx.retry(attempt, e).await;
            }
            Ended::Fatal(e, rejected) => {
                if rejected {
                    return ctx.rejected(e);
                }
                ctx.error(e);
                ctx.connection(BotConnection::Rejected, None);
                return;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn connection(
    ctx: &Ctx,
    api: &Api,
    token: &str,
    me_id: &str,
    me_name: &str,
    mut reader: net::WsReader,
    mut writer: WsWriter,
    session: &mut Session,
    attempt: &mut u32,
) -> Ended {
    // Hello first.
    let interval = match net::ws_next(&mut reader).await {
        Some(Ok(text)) => {
            let hello: Value = sonic_rs::from_str(&text).unwrap_or_default();
            if hello.get("op").and_then(|v| v.as_u64()) != Some(10) {
                return Ended::Lost("Discord’s gateway didn’t say hello.".into());
            }
            hello.get("d").and_then(|d| d.get("heartbeat_interval")).and_then(|v| v.as_u64()).unwrap_or(41_250)
        }
        Some(Err(e)) => return Ended::Lost(e),
        None => return Ended::Lost("Discord’s gateway closed at once.".into()),
    };
    let opening = if session.id.is_empty() {
        json!({"op": 2, "d": {
            "token": token,
            "intents": INTENTS,
            "properties": {"os": std::env::consts::OS, "browser": "orbit", "device": "orbit"},
        }})
    } else {
        json!({"op": 6, "d": {"token": token, "session_id": session.id, "seq": session.seq}})
    };
    if let Err(e) = net::ws_send(&mut writer, sonic_rs::to_string(&opening).unwrap_or_default()).await {
        return Ended::Lost(e);
    }
    let period = Duration::from_millis(interval.max(1000));
    // The first beat is jittered, as Discord asks.
    let first = Duration::from_millis(interval * (now_ms() % 1000) / 1000);
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + first, period);
    let mut acked = true;
    loop {
        tokio::select! {
            _ = beat.tick() => {
                if !acked {
                    log::info!("Discord: heartbeat not acknowledged; resuming");
                    return Ended::Resume;
                }
                acked = false;
                let frame = sonic_rs::to_string(&json!({"op": 1, "d": session.seq})).unwrap_or_default();
                if let Err(e) = net::ws_send(&mut writer, frame).await {
                    return Ended::Lost(e);
                }
            }
            frame = net::ws_next(&mut reader) => {
                let text = match frame {
                    Some(Ok(text)) => text,
                    Some(Err(e)) => return closed(&e, session),
                    None => return Ended::Lost("Discord closed the connection.".into()),
                };
                let Ok(event) = sonic_rs::from_str::<Value>(&text) else { continue };
                match event.get("op").and_then(|v| v.as_u64()).unwrap_or(u64::MAX) {
                    0 => {
                        if let Some(s) = event.get("s").and_then(|v| v.as_u64()) {
                            session.seq = Some(s);
                        }
                        let data = event.get("d").cloned().unwrap_or_default();
                        match event.get("t").and_then(|v| v.as_str()).unwrap_or_default() {
                            "READY" => {
                                session.id = data.get("session_id").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
                                session.resume_url =
                                    data.get("resume_gateway_url").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
                                *attempt = 0;
                                ctx.connection(BotConnection::Connected, Some(format!("@{me_name}")));
                            }
                            "RESUMED" => {
                                *attempt = 0;
                                ctx.connection(BotConnection::Connected, Some(format!("@{me_name}")));
                            }
                            "MESSAGE_CREATE" => {
                                if let Some(incoming) = parse_message(&data, me_id) {
                                    ctx.incoming(incoming);
                                }
                            }
                            "INTERACTION_CREATE" => {
                                if let Some(incoming) = parse_interaction(&data, api).await {
                                    ctx.incoming(incoming);
                                }
                            }
                            _ => {}
                        }
                    }
                    1 => {
                        let frame = sonic_rs::to_string(&json!({"op": 1, "d": session.seq})).unwrap_or_default();
                        if let Err(e) = net::ws_send(&mut writer, frame).await {
                            return Ended::Lost(e);
                        }
                    }
                    7 => return Ended::Resume,
                    9 => {
                        let resumable = event.get("d").and_then(|v| v.as_bool()).unwrap_or(false);
                        return if resumable { Ended::Resume } else { Ended::Reidentify };
                    }
                    11 => acked = true,
                    _ => {}
                }
            }
        }
    }
}

/// What a close code means.
fn closed(error: &str, session: &mut Session) -> Ended {
    match net::close_code(error) {
        Some(4004) => Ended::Fatal("Discord refused the bot token.".into(), true),
        Some(4014) => Ended::Fatal(
            "Discord refused the intents: turn on Message Content Intent under Bot › Privileged Gateway Intents.".into(),
            false,
        ),
        Some(code @ (4010..=4013)) => Ended::Fatal(format!("Discord closed the gateway ({code}): {error}"), false),
        Some(4007 | 4009) => {
            *session = Session::default();
            Ended::Lost(format!("Discord ended the session ({error}); starting a new one."))
        }
        _ => Ended::Lost(format!("Discord’s gateway {error}.")),
    }
}

fn parse_message(data: &Value, me_id: &str) -> Option<Incoming> {
    let author = data.get("author")?;
    if author.get("bot").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    let content = data.get("content").and_then(|v| v.as_str()).unwrap_or_default();
    let direct = data.get("guild_id").is_none();
    let mentioned = data
        .get("mentions")
        .and_then(|m| m.as_array())
        .is_some_and(|m| m.iter().any(|u| u.get("id").and_then(|v| v.as_str()) == Some(me_id)))
        || content.contains(&format!("<@{me_id}>"))
        || content.contains(&format!("<@!{me_id}>"));
    if !direct && !mentioned {
        return None;
    }
    let text = content.replace(&format!("<@{me_id}>"), "").replace(&format!("<@!{me_id}>"), "").trim().to_owned();
    Some(Incoming {
        chat: ChatKey::new(Platform::Discord, data.get("channel_id").and_then(|v| v.as_str())?),
        user_id: author.get("id").and_then(|v| v.as_str())?.to_owned(),
        user_name: display_name(author),
        direct,
        kind: InKind::Text(text),
    })
}

async fn parse_interaction(data: &Value, api: &Api) -> Option<Incoming> {
    if data.get("type").and_then(|v| v.as_u64()) != Some(3) {
        return None;
    }
    let id = data.get("id").and_then(|v| v.as_str())?;
    let token = data.get("token").and_then(|v| v.as_str())?;
    api.ack(id, token).await;
    let user = data.get("member").and_then(|m| m.get("user")).or_else(|| data.get("user"))?;
    Some(Incoming {
        chat: ChatKey::new(Platform::Discord, data.get("channel_id").and_then(|v| v.as_str())?),
        user_id: user.get("id").and_then(|v| v.as_str())?.to_owned(),
        user_name: display_name(user),
        direct: data.get("guild_id").is_none(),
        kind: InKind::Button(data.get("data")?.get("custom_id").and_then(|v| v.as_str())?.to_owned()),
    })
}

fn display_name(user: &Value) -> String {
    user.get("global_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| user.get("username").and_then(|v| v.as_str()))
        .unwrap_or("Someone")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_messages_need_a_mention() {
        let me = "42";
        let dm: Value = sonic_rs::from_str(r#"{"channel_id":"9","author":{"id":"7","username":"ana"},"content":"hi"}"#).unwrap();
        assert_eq!(parse_message(&dm, me).unwrap().kind, InKind::Text("hi".into()));
        let guild: Value =
            sonic_rs::from_str(r#"{"guild_id":"1","channel_id":"9","author":{"id":"7","username":"ana"},"content":"hi"}"#).unwrap();
        assert!(parse_message(&guild, me).is_none());
        let mention: Value = sonic_rs::from_str(
            r#"{"guild_id":"1","channel_id":"9","author":{"id":"7","username":"ana","global_name":"Ana"},"content":"<@42> fix it","mentions":[{"id":"42"}]}"#,
        )
        .unwrap();
        let m = parse_message(&mention, me).unwrap();
        assert_eq!((m.kind, m.user_name.as_str(), m.direct), (InKind::Text("fix it".into()), "Ana", false));
        let long = Msg::plain("y".repeat(LIMIT + 10)).with_buttons(vec![vec![Button::new("No", "n", Style::Danger)]]);
        let payloads = Api::payloads(&long);
        assert_eq!(payloads.len(), 2);
        assert!(payloads[0].get("embeds").and_then(|e| e.as_array()).is_some_and(|e| e.len() == 1));
        assert_eq!(payloads[1].get("components").and_then(|c| c.as_array()).map(|c| c.len()), Some(1));
    }
}
