//! The Slack bot against a fake Web API and a fake Socket Mode server:
//! envelopes are acknowledged, mentions answer in a thread, buttons resolve
//! requests, the status line follows the agent, and a `disconnect` opens a
//! fresh socket.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use orbit_proto::*;
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};
use support::*;

const NOWHERE: &str = "http://127.0.0.1:9";

struct FakeSlack {
    http: FakeHttp,
    ws: FakeWs,
}

impl FakeSlack {
    async fn start() -> FakeSlack {
        let ws = FakeWs::start().await;
        let ws_url = format!("{}/link?ticket=1", ws.url);
        let ts = Arc::new(AtomicU32::new(1));
        let base = Arc::new(std::sync::Mutex::new(String::new()));
        let base2 = base.clone();
        let http = FakeHttp::start(move |req| {
            let method = req.path.trim_start_matches('/');
            let auth = req.header("authorization").unwrap_or_default();
            if method == "apps.connections.open" {
                return if auth == "Bearer xapp-1" {
                    Resp::json(format!(r#"{{"ok":true,"url":"{ws_url}"}}"#))
                } else {
                    Resp::json(r#"{"ok":false,"error":"invalid_auth"}"#)
                };
            }
            if method != "upload" && auth != "Bearer xoxb-1" {
                return Resp::json(r#"{"ok":false,"error":"invalid_auth"}"#);
            }
            match method {
                "auth.test" => Resp::json(r#"{"ok":true,"user_id":"UBOT","user":"orbit"}"#),
                "users.info" => Resp::json(r#"{"ok":true,"user":{"name":"ana","profile":{"display_name":"Ana","real_name":"Ana B"}}}"#),
                "chat.postMessage" => Resp::json(format!(r#"{{"ok":true,"ts":"900.{}"}}"#, ts.fetch_add(1, Ordering::SeqCst))),
                "files.getUploadURLExternal" => {
                    Resp::json(format!(r#"{{"ok":true,"upload_url":"{}/upload","file_id":"F1"}}"#, base2.lock().unwrap()))
                }
                "upload" => Resp::status(200, "OK"),
                _ => Resp::json(r#"{"ok":true}"#),
            }
        })
        .await;
        *base.lock().unwrap() = http.url.clone();
        FakeSlack { http, ws }
    }

    fn posts(&self) -> Vec<Value> {
        self.http.calls("/chat.postMessage").iter().map(|r| r.json()).collect()
    }

    fn text_of(post: &Value) -> String {
        post.get("blocks")
            .and_then(|b| b.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.get("text")).and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    async fn wait_post(&self, secs: u64, needle: &str) -> Value {
        eventually(secs, needle, || self.posts().into_iter().find(|p| Self::text_of(p).contains(needle))).await
    }
}

fn envelope(id: &str, kind: &str, payload: &str) -> String {
    format!(r#"{{"envelope_id":"{id}","type":"{kind}","accepts_response_payload":false,"payload":{payload}}}"#)
}

fn dm(id: &str, text: &str) -> String {
    let text = sonic_rs::to_string(text).unwrap();
    envelope(
        id,
        "events_api",
        &format!(r#"{{"event":{{"type":"message","channel_type":"im","channel":"D1","user":"U1","text":{text},"ts":"100.{id}"}}}}"#),
    )
}

fn mention(id: &str, text: &str, thread: Option<&str>) -> String {
    let text = sonic_rs::to_string(&format!("<@UBOT> {text}")).unwrap();
    let thread = thread.map(|t| format!(r#","thread_ts":"{t}""#)).unwrap_or_default();
    envelope(
        id,
        "events_api",
        &format!(r#"{{"event":{{"type":"app_mention","channel":"C1","user":"U1","text":{text},"ts":"200.{id}"{thread}}}}}"#),
    )
}

async fn hello(conn: &mut WsConn) {
    conn.send(r#"{"type":"hello","num_connections":1}"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_mode_mentions_threads_buttons_and_reconnects() {
    let mut slack = FakeSlack::start().await;
    let mut world = World::new();
    world.start_bots(endpoints(NOWHERE, &slack.http.url, NOWHERE));
    world
        .bots(BotCommand::SetTokens { platform: Platform::Slack, token: "xoxb-1".into(), app_token: String::new(), clear: false })
        .await
        .unwrap();
    world.bots(BotCommand::Enable { platform: Platform::Slack, enabled: true }).await.unwrap();
    assert_eq!(world.state().await.platform(Platform::Slack).connection, BotConnection::NeedsToken, "Socket Mode needs the app token too");
    world
        .bots(BotCommand::SetTokens { platform: Platform::Slack, token: String::new(), app_token: "xapp-1".into(), clear: false })
        .await
        .unwrap();
    let mut conn = slack.ws.accept(5).await.expect("socket opened");
    assert!(conn.path.starts_with("/link?ticket=1"), "{}", conn.path);
    hello(&mut conn).await;
    for _ in 0..250 {
        if world.state().await.platform(Platform::Slack).connection == BotConnection::Connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(world.state().await.platform(Platform::Slack).account, "@orbit");

    // A stranger's DM: acknowledged, answered with a code.
    conn.send(dm("e1", "hi"));
    let ack = conn.recv_where(5, |v| v.get("envelope_id").is_some()).await.expect("ack");
    assert_eq!(ack.get("envelope_id").and_then(|v| v.as_str()), Some("e1"));
    let reply = slack.wait_post(5, "Pairing code: ").await;
    assert_eq!(reply.get("channel").and_then(|v| v.as_str()), Some("D1"));
    let code: String = FakeSlack::text_of(&reply)["Pairing code: ".len()..].chars().take(6).collect();
    world.bots(BotCommand::Approve { code }).await.unwrap();
    slack.wait_post(5, "You’re paired, Ana").await;

    // A mention in a channel: answered in a thread under it.
    conn.send(mention("e2", "hello", None));
    let answer = slack.wait_post(5, "echo: hello").await;
    assert_eq!(answer.get("channel").and_then(|v| v.as_str()), Some("C1"));
    assert_eq!(answer.get("thread_ts").and_then(|v| v.as_str()), Some("200.e2"));

    // An approval in that thread, answered with a button.
    conn.send(mention("e3", "ask", Some("200.e2")));
    let asked = slack.wait_post(5, "Run this command?").await;
    assert_eq!(asked.get("thread_ts").and_then(|v| v.as_str()), Some("200.e2"), "same Slack thread, same Orbit thread");
    let actions = asked.get("blocks").and_then(|b| b.as_array()).unwrap().iter().find(|b| b.get("type").and_then(|t| t.as_str()) == Some("actions")).cloned().unwrap();
    let yes = actions.get("elements").and_then(|e| e.as_array()).unwrap().iter().next().unwrap().get("value").and_then(|v| v.as_str()).unwrap().to_owned();
    conn.send(envelope(
        "e4",
        "interactive",
        &format!(r#"{{"type":"block_actions","user":{{"id":"U1","username":"ana"}},"channel":{{"id":"C1"}},"message":{{"ts":"900.5","thread_ts":"200.e2"}},"actions":[{{"action_id":"orbit_0_0","value":"{yes}"}}]}}"#),
    ));
    slack.wait_post(5, "decision: Allow").await;
    eventually(5, "the request message updated", || {
        slack.http.calls("/chat.update").iter().find(|r| r.text().contains("Approved")).cloned()
    })
    .await;

    // Slack rotates the socket: a new one opens at once, no error shown.
    conn.send(r#"{"type":"disconnect","reason":"refresh_requested"}"#);
    let mut again = slack.ws.accept(5).await.expect("a fresh socket");
    hello(&mut again).await;
    again.send(envelope("e5", "slash_commands", r#"{"command":"/orbit","text":"status","user_id":"U1","user_name":"ana","channel_id":"D1"}"#));
    slack.wait_post(5, "Orbit: ").await;
    assert_eq!(slack.http.calls("/apps.connections.open").len(), 2);

    // The status line follows the agent's tool ("Working · editing …").
    again.send(mention("e6", "tool", Some("200.e2")));
    eventually(12, "a status edit naming the tool", || {
        slack.http.calls("/chat.update").iter().find(|r| r.text().contains("Working · editing src/main.rs")).cloned()
    })
    .await;
    slack.wait_post(10, "edited").await;
}
