//! The Discord bot against a fake REST API and a fake gateway: identify
//! with the right intents, heartbeats, messages and button interactions,
//! and resuming a dropped session from the last sequence number.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use orbit_proto::*;
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};
use support::*;

const NOWHERE: &str = "http://127.0.0.1:9";
const TOKEN: &str = "discord-token";

struct FakeDiscord {
    http: FakeHttp,
    ws: FakeWs,
}

impl FakeDiscord {
    async fn start() -> FakeDiscord {
        let ws = FakeWs::start().await;
        let ws_url = ws.url.clone();
        let ids = Arc::new(AtomicU32::new(1));
        let http = FakeHttp::start(move |req| {
            if req.header("authorization") != Some(&format!("Bot {TOKEN}")) {
                return Resp::status(401, r#"{"message":"401: Unauthorized","code":0}"#);
            }
            assert!(req.header("user-agent").is_some_and(|ua| ua.starts_with("DiscordBot (")));
            match (req.method.as_str(), req.path.as_str()) {
                ("GET", "/users/@me") => Resp::json(r#"{"id":"BOT","username":"orbit"}"#),
                ("GET", "/gateway/bot") => Resp::json(format!(r#"{{"url":"{ws_url}","shards":1}}"#)),
                ("POST", p) if p.starts_with("/channels/") => {
                    Resp::json(format!(r#"{{"id":"m{}"}}"#, ids.fetch_add(1, Ordering::SeqCst)))
                }
                ("POST", p) if p.starts_with("/interactions/") => Resp::status(204, ""),
                _ => Resp::json("{}"),
            }
        })
        .await;
        FakeDiscord { http, ws }
    }

    fn posts(&self) -> Vec<Value> {
        self.http.requests().into_iter().filter(|r| r.method == "POST" && r.path.ends_with("/messages")).map(|r| r.json()).collect()
    }

    fn text_of(post: &Value) -> String {
        let content = post.get("content").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
        let embeds: Vec<String> = post
            .get("embeds")
            .and_then(|e| e.as_array())
            .map(|e| e.iter().filter_map(|x| x.get("description").and_then(|d| d.as_str()).map(str::to_owned)).collect())
            .unwrap_or_default();
        format!("{content}{}", embeds.join(""))
    }

    async fn wait_post(&self, secs: u64, needle: &str) -> Value {
        eventually(secs, needle, || self.posts().into_iter().find(|p| Self::text_of(p).contains(needle))).await
    }
}

fn dispatch(seq: u64, kind: &str, data: &str) -> String {
    format!(r#"{{"op":0,"s":{seq},"t":"{kind}","d":{data}}}"#)
}

fn dm(seq: u64, text: &str) -> String {
    let text = sonic_rs::to_string(text).unwrap();
    dispatch(seq, "MESSAGE_CREATE", &format!(r#"{{"id":"x{seq}","channel_id":"DM1","author":{{"id":"U1","username":"ana","global_name":"Ana"}},"content":{text}}}"#))
}

/// Says hello and returns the client's opening frame (identify or resume).
async fn open(conn: &mut WsConn, interval: u64) -> Value {
    conn.send(format!(r#"{{"op":10,"d":{{"heartbeat_interval":{interval}}}}}"#));
    conn.recv_where(5, |v| matches!(v.get("op").and_then(|o| o.as_u64()), Some(2 | 6))).await.expect("identify or resume")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gateway_identifies_heartbeats_and_resumes() {
    let mut discord = FakeDiscord::start().await;
    let mut world = World::new();
    world.start_bots(endpoints(NOWHERE, NOWHERE, &discord.http.url));
    world.bots(BotCommand::SetTokens { platform: Platform::Discord, token: TOKEN.into(), app_token: String::new(), clear: false }).await.unwrap();
    world.bots(BotCommand::Enable { platform: Platform::Discord, enabled: true }).await.unwrap();

    let mut conn = discord.ws.accept(5).await.expect("gateway opened");
    assert!(conn.path.contains("v=10") && conn.path.contains("encoding=json"), "{}", conn.path);
    let identify = open(&mut conn, 300).await;
    assert_eq!(identify.get("op").and_then(|v| v.as_u64()), Some(2));
    let d = identify.get("d").unwrap();
    assert_eq!(d.get("token").and_then(|v| v.as_str()), Some(TOKEN));
    assert_eq!(d.get("intents").and_then(|v| v.as_u64()), Some((1 << 9) | (1 << 12) | (1 << 15)));
    conn.send(dispatch(1, "READY", &format!(r#"{{"session_id":"S1","resume_gateway_url":"{}","user":{{"id":"BOT"}}}}"#, discord.ws.url)));
    // Heartbeats carry the last sequence number; acknowledge them.
    let beat = conn.recv_where(3, |v| v.get("op").and_then(|o| o.as_u64()) == Some(1)).await.expect("a heartbeat");
    assert_eq!(beat.get("d").and_then(|v| v.as_u64()), Some(1));
    conn.send(r#"{"op":11}"#);
    for _ in 0..250 {
        if world.state().await.platform(Platform::Discord).connection == BotConnection::Connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(world.state().await.platform(Platform::Discord).account, "@orbit");

    // Keep acknowledging in the background of this connection.
    conn.send(dm(2, "hi"));
    let reply = discord.wait_post(5, "Pairing code: ").await;
    let code: String = FakeDiscord::text_of(&reply)["Pairing code: ".len()..].chars().take(6).collect();
    world.bots(BotCommand::Approve { code }).await.unwrap();
    discord.wait_post(5, "You’re paired, Ana").await;
    conn.send(r#"{"op":11}"#);
    conn.send(dm(3, "hello"));
    let answer = discord.wait_post(5, "echo: hello").await;
    assert_eq!(answer.get("allowed_mentions").and_then(|a| a.get("parse")).and_then(|p| p.as_array()).map(|p| p.len()), Some(0), "never pings anyone");

    // The connection drops: the bot resumes the session where it was.
    conn.close(4000, "unknown error");
    let mut resumed = discord.ws.accept(5).await.expect("reconnected");
    let resume = open(&mut resumed, 300).await;
    assert_eq!(resume.get("op").and_then(|v| v.as_u64()), Some(6), "resume, not identify");
    let d = resume.get("d").unwrap();
    assert_eq!(d.get("session_id").and_then(|v| v.as_str()), Some("S1"));
    assert_eq!(d.get("seq").and_then(|v| v.as_u64()), Some(3));
    resumed.send(dispatch(4, "RESUMED", "{}"));

    // A button press arrives as an interaction, acknowledged at once.
    resumed.send(r#"{"op":11}"#);
    resumed.send(dm(5, "ask"));
    let asked = discord.wait_post(5, "Run this command?").await;
    let buttons = asked.get("components").and_then(|c| c.as_array()).unwrap().iter().next().unwrap().get("components").and_then(|c| c.as_array()).unwrap().clone();
    let labels: Vec<&str> = buttons.iter().map(|b| b.get("label").and_then(|v| v.as_str()).unwrap()).collect();
    assert_eq!(labels, vec!["Yes", "No", "Always"]);
    let no = buttons[1].get("custom_id").and_then(|v| v.as_str()).unwrap().to_owned();
    resumed.send(dispatch(
        6,
        "INTERACTION_CREATE",
        &format!(r#"{{"id":"I1","token":"T1","type":3,"channel_id":"DM1","user":{{"id":"U1","username":"ana"}},"data":{{"custom_id":"{no}","component_type":2}}}}"#),
    ));
    discord.wait_post(5, "decision: Deny").await;
    assert!(discord.http.calls("/interactions/I1/T1/callback").len() == 1);

    // Heartbeats left unacknowledged mean a dead link: reconnect and resume.
    let mut zombie_free = discord.ws.accept(5).await.expect("reconnected after missed acks");
    let resume = open(&mut zombie_free, 300).await;
    assert_eq!(resume.get("op").and_then(|v| v.as_u64()), Some(6));
    assert_eq!(resume.get("d").and_then(|d| d.get("seq")).and_then(|v| v.as_u64()), Some(6));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_token_or_intent_stops_the_bot() {
    let mut discord = FakeDiscord::start().await;
    let mut world = World::new();
    world.start_bots(endpoints(NOWHERE, NOWHERE, &discord.http.url));
    world.bots(BotCommand::SetTokens { platform: Platform::Discord, token: TOKEN.into(), app_token: String::new(), clear: false }).await.unwrap();
    world.bots(BotCommand::Enable { platform: Platform::Discord, enabled: true }).await.unwrap();
    let mut conn = discord.ws.accept(5).await.unwrap();
    open(&mut conn, 30_000).await;
    conn.close(4014, "Disallowed intent(s).");
    let state = loop {
        let s = world.state().await.platform(Platform::Discord);
        if s.connection == BotConnection::Rejected {
            break s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(state.error.unwrap().contains("Message Content Intent"));
    assert!(discord.ws.accept(1).await.is_none(), "no reconnect loop");
}
