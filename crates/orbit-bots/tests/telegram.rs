//! The Telegram bot against a fake Bot API: pairing, a turn from message to
//! answer, approval buttons, questions, chunking and files, the guard, and
//! rate limits both ways.

mod support;

use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orbit_proto::*;
use sonic_rs::{JsonContainerTrait, JsonValueTrait};
use support::*;

const TOKEN: &str = "123:fake-token";
const NOWHERE: &str = "http://127.0.0.1:9";

struct FakeTelegram {
    http: FakeHttp,
    updates: Arc<Mutex<Vec<String>>>,
    next_update: AtomicI64,
}

impl FakeTelegram {
    /// `limited` sendMessage calls answer 429 before succeeding.
    async fn start(limited: u32) -> Arc<FakeTelegram> {
        let updates: Arc<Mutex<Vec<String>>> = Arc::default();
        let queue = updates.clone();
        let next_id = Arc::new(AtomicI64::new(100));
        let limited = Arc::new(AtomicU32::new(limited));
        let http = FakeHttp::start(move |req| {
            let method = req.path.rsplit('/').next().unwrap_or_default();
            if !req.path.starts_with(&format!("/bot{TOKEN}/")) {
                return Resp::status(401, r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#);
            }
            match method {
                "getMe" => Resp::json(r#"{"ok":true,"result":{"id":1,"is_bot":true,"username":"orbit_test_bot"}}"#),
                "getUpdates" => {
                    let pending: Vec<String> = std::mem::take(&mut *queue.lock().unwrap());
                    if pending.is_empty() {
                        Resp::json(r#"{"ok":true,"result":[]}"#).after(Duration::from_millis(200))
                    } else {
                        Resp::json(format!(r#"{{"ok":true,"result":[{}]}}"#, pending.join(",")))
                    }
                }
                "sendMessage" => {
                    if limited.load(Ordering::SeqCst) > 0 {
                        limited.fetch_sub(1, Ordering::SeqCst);
                        return Resp::status(
                            429,
                            r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 1","parameters":{"retry_after":1}}"#,
                        );
                    }
                    let id = next_id.fetch_add(1, Ordering::SeqCst);
                    Resp::json(format!(r#"{{"ok":true,"result":{{"message_id":{id}}}}}"#))
                }
                _ => Resp::json(r#"{"ok":true,"result":true}"#),
            }
        })
        .await;
        Arc::new(FakeTelegram { http, updates, next_update: AtomicI64::new(1) })
    }

    fn push(&self, update: String) {
        let id = self.next_update.fetch_add(1, Ordering::SeqCst);
        let update = update.replacen('{', &format!("{{\"update_id\":{id},"), 1);
        self.updates.lock().unwrap().push(update);
    }

    fn text(&self, text: &str) {
        let text = sonic_rs::to_string(text).unwrap();
        self.push(format!(
            r#"{{"message":{{"message_id":5,"chat":{{"id":77,"type":"private"}},"from":{{"id":77,"is_bot":false,"first_name":"Ana"}},"text":{text}}}}}"#
        ));
    }

    fn press(&self, data: &str) {
        self.push(format!(
            r#"{{"callback_query":{{"id":"cb1","from":{{"id":77,"is_bot":false,"first_name":"Ana"}},"message":{{"message_id":9,"chat":{{"id":77,"type":"private"}}}},"data":"{data}"}}}}"#
        ));
    }

    fn sent(&self) -> Vec<sonic_rs::Value> {
        self.http.calls("/sendMessage").iter().map(|r| r.json()).filter(|v| v.get("text").is_some()).collect()
    }

    fn sent_texts(&self) -> Vec<String> {
        self.sent().iter().map(|v| v.get("text").and_then(|t| t.as_str()).unwrap_or_default().to_owned()).collect()
    }

    async fn wait_count(&self, secs: u64, needle: &str, count: usize) {
        eventually(secs, needle, || (self.sent_texts().iter().filter(|t| t.contains(needle)).count() >= count).then_some(())).await
    }

    async fn wait_text(&self, secs: u64, needle: &str) -> sonic_rs::Value {
        eventually(secs, needle, || self.sent().into_iter().find(|v| v.get("text").and_then(|t| t.as_str()).is_some_and(|t| t.contains(needle))))
            .await
    }
}

async fn connected(world: &World, telegram: &Arc<FakeTelegram>) -> String {
    world.bots(BotCommand::SetTokens { platform: Platform::Telegram, token: TOKEN.into(), app_token: String::new(), clear: false }).await.unwrap();
    world.bots(BotCommand::Enable { platform: Platform::Telegram, enabled: true }).await.unwrap();
    for _ in 0..250 {
        let state = world.state().await.platform(Platform::Telegram);
        if state.connection == BotConnection::Connected {
            assert_eq!(state.account, "@orbit_test_bot");
            assert_eq!(state.token_hint, "…oken");
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Pair: a stranger gets a code; approving it lets them in.
    telegram.text("hi");
    let reply = telegram.wait_text(5, "Pairing code: ").await;
    let text = reply.get("text").and_then(|t| t.as_str()).unwrap().to_owned();
    let code: String = text["Pairing code: ".len()..].chars().take(6).collect();
    let state = world.state().await;
    assert_eq!(state.pending.len(), 1);
    assert_eq!((state.pending[0].name.as_str(), state.pending[0].code.as_str()), ("Ana", code.as_str()));
    let Reply::Bots(BotReply::Paired(user)) = world.bots(BotCommand::Approve { code: code.clone() }).await.unwrap() else { panic!() };
    assert_eq!((user.platform, user.user_id.as_str()), (Platform::Telegram, "77"));
    telegram.wait_text(5, "You’re paired, Ana").await;
    code
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paired_chat_drives_a_turn_and_strangers_need_a_code() {
    let telegram = FakeTelegram::start(0).await;
    let mut world = World::new();
    world.start_bots(endpoints(&telegram.http.url, NOWHERE, NOWHERE));
    let code = connected(&world, &telegram).await;
    assert!(world.bots(BotCommand::Approve { code }).await.is_err(), "a code works once");

    telegram.text("hello there");
    telegram.wait_text(5, "echo: hello there").await;
    // The status line was posted and then closed.
    eventually(5, "status edit", || {
        telegram.http.calls("/editMessageText").iter().find(|r| r.text().contains("Done · ")).cloned()
    })
    .await;
    let texts = telegram.sent_texts();
    assert!(texts.iter().any(|t| t.starts_with("Working")), "{texts:?}");

    // The thread's timeline says where the message came from.
    let Reply::Timeline { items, .. } = world
        .call(Command::ReadTimeline { id: only_thread(&world).await, after: None, limit: 0 })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(items.iter().any(|i| matches!(&i.body, ItemBody::Notice { text, .. } if text == "Sent via Telegram by Ana")));

    // Commands answer without reaching the agent.
    telegram.text("/status");
    telegram.wait_text(5, "Orbit: ").await;
    telegram.text("/help");
    telegram.wait_text(5, "/new [project] [message]").await;
}

async fn only_thread(world: &World) -> ThreadId {
    let mut feed = orbit_net::Backend::Local(world.engine.clone()).feed();
    let Some(batch) = feed.next_batch().await else { panic!() };
    for update in batch {
        if let orbit_net::Update::Reset(snapshot) = update {
            return snapshot.threads[0].id;
        }
    }
    panic!("no snapshot")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approvals_and_questions_come_with_buttons() {
    let telegram = FakeTelegram::start(0).await;
    let mut world = World::new();
    world.start_bots(endpoints(&telegram.http.url, NOWHERE, NOWHERE));
    connected(&world, &telegram).await;

    telegram.text("ask");
    let asked = telegram.wait_text(5, "Run this command?").await;
    let keyboard = asked.get("reply_markup").and_then(|k| k.get("inline_keyboard")).unwrap();
    let row: Vec<(String, String)> = keyboard.as_array().unwrap().iter().next().unwrap().as_array().unwrap().iter()
        .map(|b| (b.get("text").and_then(|v| v.as_str()).unwrap().to_owned(), b.get("callback_data").and_then(|v| v.as_str()).unwrap().to_owned()))
        .collect();
    assert_eq!(row.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(), vec!["Yes", "No", "Always"]);
    assert!(row.iter().all(|(_, d)| d.len() <= 64), "Telegram's callback limit");
    telegram.press(&row[2].1);
    telegram.wait_text(5, "decision: AllowForSession").await;
    // The request message lost its buttons and says how it was answered.
    eventually(5, "answered edit", || {
        telegram.http.calls("/editMessageText").iter().find(|r| r.text().contains("Approved for this session")).cloned()
    })
    .await;

    // A question: options as buttons, descriptions in the text.
    telegram.text("pick");
    let question = telegram.wait_text(5, "Which colour?").await;
    let text = question.get("text").and_then(|t| t.as_str()).unwrap();
    assert!(text.contains("1. Red — Warm") && text.contains("2. Blue — Calm"), "{text}");
    let buttons = question.get("reply_markup").and_then(|k| k.get("inline_keyboard")).and_then(|k| k.as_array()).unwrap().len();
    assert_eq!(buttons, 3);
    // Answering by number works too.
    telegram.text("2");
    telegram.wait_text(5, "decision: Answer { text: \"Blue\" }").await;

    // Text replies approve a lone request.
    telegram.text("ask");
    telegram.wait_count(5, "Run this command?", 2).await;
    telegram.text("no");
    telegram.wait_text(5, "decision: Deny").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_guard_keeps_approvals_for_the_app() {
    let telegram = FakeTelegram::start(0).await;
    let mut world = World::new();
    world.start_bots(endpoints(&telegram.http.url, NOWHERE, NOWHERE));
    connected(&world, &telegram).await;

    telegram.text("push");
    let asked = telegram.wait_text(5, "Guarded: Force push").await;
    let keyboard = asked.get("reply_markup").and_then(|k| k.get("inline_keyboard")).unwrap();
    let labels: Vec<String> = keyboard.as_array().unwrap().iter().flat_map(|r| r.as_array().unwrap().iter().map(|b| b.get("text").and_then(|v| v.as_str()).unwrap().to_owned()).collect::<Vec<_>>()).collect();
    assert_eq!(labels, vec!["No"], "only denying is offered");
    // Typing yes is refused by the engine and explained.
    telegram.text("yes");
    telegram.wait_text(5, "approve this in the Orbit app").await;
    // The app approves it.
    let thread = only_thread(&world).await;
    let pending = loop {
        let Reply::Timeline { items, .. } = world.call(Command::ReadTimeline { id: thread, after: None, limit: 0 }).await.unwrap() else { panic!() };
        if let Some(r) = items.iter().find_map(|i| match &i.body {
            ItemBody::Request(r) if r.resolution.is_none() => Some(r.id),
            _ => None,
        }) {
            break r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    world.call(Command::Resolve { thread, request: pending, decision: Decision::AllowForSession }).await.unwrap();
    telegram.wait_text(5, "decision: Allow").await;
    assert!(!telegram.sent_texts().iter().any(|t| t.contains("decision: AllowForSession")), "guarded: allowed once only");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_answers_are_chunked_and_long_code_is_attached() {
    let telegram = FakeTelegram::start(0).await;
    let mut world = World::new();
    world.start_bots(endpoints(&telegram.http.url, NOWHERE, NOWHERE));
    connected(&world, &telegram).await;
    telegram.text("long");
    telegram.wait_text(10, "Line 299 of the answer").await;
    let chunks: Vec<String> = telegram.sent_texts().into_iter().filter(|t| t.contains("of the answer")).collect();
    assert!(chunks.len() >= 3, "{} chunks", chunks.len());
    assert!(chunks.iter().all(|c| c.chars().count() <= 4096));
    let file = eventually(5, "document", || telegram.http.calls("/sendDocument").into_iter().next()).await;
    let body = file.text();
    assert!(body.contains("filename=\"snippet-1.rs\"") && body.contains("let x59 = 59;"), "{}", &body[..body.len().min(400)]);
    assert!(telegram.sent_texts().iter().any(|t| t.contains("Attached snippet-1.rs · 60 lines")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limits_are_kept_both_ways() {
    // Telegram answers the first two sends with 429: the outbox waits and
    // delivers.
    let telegram = FakeTelegram::start(2).await;
    let mut world = World::new();
    world.start_bots(endpoints(&telegram.http.url, NOWHERE, NOWHERE));
    connected(&world, &telegram).await;
    assert!(telegram.http.calls("/sendMessage").len() >= 4, "two refusals, then the code and the welcome");

    // A flood from one person: the first burst goes through, then one
    // warning, then silence.
    for i in 0..14 {
        telegram.text(&format!("/status {i}"));
    }
    telegram.wait_text(10, "Slow down").await;
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let statuses = telegram.sent_texts().iter().filter(|t| t.starts_with("Orbit: ")).count();
    assert!((7..=9).contains(&statuses), "{statuses} answered");
    assert_eq!(telegram.sent_texts().iter().filter(|t| t.contains("Slow down")).count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_token_stops_until_it_changes() {
    let telegram = FakeTelegram::start(0).await;
    let mut world = World::new();
    world.start_bots(endpoints(&telegram.http.url, NOWHERE, NOWHERE));
    world.bots(BotCommand::SetTokens { platform: Platform::Telegram, token: "999:wrong".into(), app_token: String::new(), clear: false }).await.unwrap();
    world.bots(BotCommand::Enable { platform: Platform::Telegram, enabled: true }).await.unwrap();
    let state = loop {
        let s = world.state().await.platform(Platform::Telegram);
        if s.connection == BotConnection::Rejected {
            break s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(state.error.unwrap().contains("refused the token"));
    // Turning off shows Off; a new token reconnects.
    world.bots(BotCommand::SetTokens { platform: Platform::Telegram, token: TOKEN.into(), app_token: String::new(), clear: false }).await.unwrap();
    for _ in 0..250 {
        if world.state().await.platform(Platform::Telegram).connection == BotConnection::Connected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("never reconnected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn needs_you_from_other_threads_is_pushed_to_paired_chats() {
    let telegram = FakeTelegram::start(0).await;
    let mut world = World::new();
    world.start_bots(endpoints(&telegram.http.url, NOWHERE, NOWHERE));
    connected(&world, &telegram).await;
    let options = BotOptions { notify: BotNotify::Always, ..BotOptions::default() };
    world.bots(BotCommand::SetOptions(options)).await.unwrap();
    // A thread started in the app, not from the chat, asks for approval.
    let start = ThreadStart { prompt: Some("ask".into()), title: "From the app".into(), ..Default::default() };
    world.call(Command::StartThread(start)).await.unwrap();
    let pushed = telegram.wait_text(5, "Run this command?").await;
    assert!(pushed.get("text").and_then(|t| t.as_str()).unwrap().contains("From the app"));
    let yes = pushed.get("reply_markup").and_then(|k| k.get("inline_keyboard")).and_then(|k| k.as_array()).unwrap().iter().next().unwrap()
        .as_array().unwrap().iter().next().unwrap().get("callback_data").and_then(|v| v.as_str()).unwrap().to_owned();
    telegram.press(&yes);
    eventually(5, "the request answered", || {
        telegram.http.calls("/editMessageText").iter().find(|r| r.text().contains("→ Approved")).cloned()
    })
    .await;
    // The chat isn't driving that thread: its answer stays in the app.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!telegram.sent_texts().iter().any(|t| t.contains("decision: Allow")));

    // While the app is in front (and the setting says so), nothing is pushed.
    world.bots(BotCommand::SetOptions(BotOptions::default())).await.unwrap();
    world.bots(BotCommand::AppActive(true)).await.unwrap();
    let start = ThreadStart { prompt: Some("ask".into()), title: "Quiet".into(), ..Default::default() };
    world.call(Command::StartThread(start)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!telegram.sent_texts().iter().any(|t| t.contains("Quiet")));
}
