//! Prints a data folder's projects, threads and timelines, for debugging:
//! `cargo run -p orbit-core --example dump -- <data dir> [all]`
//! Without `all`, only the newest thread's timeline.
use orbit_core::store::Store;
use orbit_proto::*;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: dump <data dir>");
    let mut store = Store::open(&dir).unwrap();
    let mut threads: Vec<Thread> = store.threads().cloned().collect();
    threads.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
    for p in store.projects() {
        println!("project {:?} {} base={:?}", p.name, p.path, p.base_branch);
    }
    let all = std::env::args().nth(2).as_deref() == Some("all");
    for thread in threads.iter().take(if all { usize::MAX } else { 1 }).cloned().collect::<Vec<_>>() {
        dump_thread(&mut store, &thread);
    }
}

fn dump_thread(store: &mut Store, newest: &Thread) {
    println!(
        "
thread {:?} {} status={:?} turns={} project={:?} workspace={:?}",
        newest.title, newest.id, newest.status, newest.turns, newest.project, newest.workspace
    );
    for item in store.load_timeline(newest.id).unwrap().items() {
        let summary = match &item.body {
            ItemBody::User(m) => format!("user {:?}", m.text.chars().take(80).collect::<String>()),
            ItemBody::Assistant { text } => format!("assistant {} chars {:?}", text.len(), text.chars().take(80).collect::<String>()),
            ItemBody::Reasoning { text } => format!("reasoning {} chars", text.len()),
            ItemBody::Tool(c) => format!(
                "tool {:?} {:?} title={:?} out={}B files={:?} input={:?}",
                c.kind,
                c.name,
                c.title,
                c.output.len(),
                c.files,
                c.input.chars().take(160).collect::<String>()
            ),
            ItemBody::Request(r) => format!("request {:?} resolved={:?}", r.kind, r.resolution),
            ItemBody::Plan { steps } => format!("plan {} steps", steps.len()),
            ItemBody::Notice { level, text } => format!("notice {level:?} {text:?}"),
            ItemBody::TurnEnd(s) => format!(
                "turn-end {}ms base={:?} checkpoint={:?} files={:?}",
                s.duration_ms, s.base_checkpoint, s.checkpoint, s.files
            ),
            ItemBody::Handoff(h) => format!("handoff {} -> {} ({} tokens)", h.from, h.to, h.tokens),
        };
        println!("  t{} {:?} {}", item.turn, item.status, summary);
    }
}
