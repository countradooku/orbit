//! Prints threads and the newest thread's timeline from a data folder:
//! `cargo run -p orbit-core --example dump -- <data dir>`
use orbit_core::store::Store;
use orbit_proto::*;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: dump <data dir>");
    let mut store = Store::open(&dir).unwrap();
    let mut threads: Vec<Thread> = store.threads().cloned().collect();
    threads.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
    let Some(newest) = threads.first().cloned() else { return };
    println!("newest: {:?} status={:?} turns={} session={:?}", newest.title, newest.status, newest.turns, newest.session);
    for item in store.load_timeline(newest.id).unwrap().items() {
        let summary = match &item.body {
            ItemBody::User(m) => format!("user {:?}", m.text.chars().take(80).collect::<String>()),
            ItemBody::Assistant { text } => format!("assistant {} chars {:?}", text.len(), text.chars().take(80).collect::<String>()),
            ItemBody::Reasoning { text } => format!("reasoning {} chars", text.len()),
            ItemBody::Tool(c) => format!("tool {:?} {:?} title={:?} out={}B", c.kind, c.name, c.title, c.output.len()),
            ItemBody::Request(r) => format!("request {:?} resolved={:?}", r.kind, r.resolution),
            ItemBody::Plan { steps } => format!("plan {} steps", steps.len()),
            ItemBody::Notice { level, text } => format!("notice {level:?} {text:?}"),
            ItemBody::TurnEnd(s) => format!("turn-end {:?}", s.duration_ms),
            ItemBody::Handoff(h) => format!("handoff {} -> {} ({} tokens)", h.from, h.to, h.tokens),
        };
        println!("  t{} {:?} {}", item.turn, item.status, summary);
    }
}
