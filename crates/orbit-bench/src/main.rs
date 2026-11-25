//! `orbit-bench`: measures the engine, protocol, storage and network.
//!
//! ```text
//! cargo run -p orbit-bench --release            # all benchmarks, markdown table
//! cargo run -p orbit-bench --release -- --json  # machine-readable
//! ```
//!
//! Every benchmark drives the real code paths: the engine thread with a fake
//! agent that streams like a real one, the real store on a temp dir, and the
//! real sockudo-ws server and client over loopback TCP.

use std::time::{Duration, Instant};

use crossfire::{BlockingTxTrait, mpsc};
use orbit_core::provider::{AgentEvent, BlockKind, SessionHandle, SessionInput, TurnOutcome};
use orbit_core::{EngineHandle, EngineOptions};
use orbit_net::{Backend, Feed, RemoteClient, Update};
use orbit_proto::*;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct Result {
    name: &'static str,
    value: f64,
    unit: &'static str,
    note: String,
}

fn main() {
    let json = std::env::args().any(|a| a == "--json");
    let mut results = Vec::new();
    results.extend(codec());
    results.extend(engine_streaming());
    results.extend(reducer());
    results.extend(storage());
    results.extend(network());
    if json {
        let rows: Vec<String> = results
            .iter()
            .map(|r| format!("{{\"name\":\"{}\",\"value\":{:.3},\"unit\":\"{}\"}}", r.name, r.value, r.unit))
            .collect();
        println!("[{}]", rows.join(","));
    } else {
        println!("| Benchmark | Result | Notes |\n| --- | ---: | --- |");
        for r in &results {
            println!("| {} | {} {} | {} |", r.name, fmt(r.value), r.unit, r.note);
        }
    }
}

fn fmt(v: f64) -> String {
    if v >= 100.0 { format!("{v:.0}") } else if v >= 10.0 { format!("{v:.1}") } else { format!("{v:.2}") }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}

// ── Codec ────────────────────────────────────────────────────────────────

fn codec() -> Vec<Result> {
    let env = Envelope {
        seq: 0,
        event: Event::ItemDelta { thread: ThreadId::new(), item: ItemId::new(), delta: Delta::Text("token ".into()) },
    };
    let n = 2_000_000;
    let mut enc = Codec::new();
    let mut dec = Codec::new();
    let start = Instant::now();
    let mut bytes = 0;
    for _ in 0..n {
        let encoded = enc.encode(&env);
        bytes += encoded.len();
        let back: Envelope = dec.decode(encoded).unwrap();
        std::hint::black_box(back);
    }
    let elapsed = start.elapsed();

    // The wire path: deltas travel in batches of up to 512 per frame.
    let batch = ServerMsg::Events(vec![env.clone(); 512]);
    let rounds = 20_000;
    let start = Instant::now();
    let mut batch_bytes = 0;
    for _ in 0..rounds {
        let encoded = enc.encode(&batch);
        batch_bytes = encoded.len();
        let back: ServerMsg = dec.decode(encoded).unwrap();
        std::hint::black_box(back);
    }
    let batched = start.elapsed();

    // What the link sends: streamed tokens in their own frames, and `Done`
    // replies in five bytes (`orbit_proto::link`).
    let mut link_enc = orbit_proto::link::ServerEncoder::new();
    let mut link_dec = orbit_proto::link::ServerDecoder::new();
    let start = Instant::now();
    let mut link_bytes = 0;
    for _ in 0..rounds {
        let frames = link_enc.events(vec![env.clone(); 512]);
        link_bytes = frames.iter().map(Vec::len).sum::<usize>();
        for f in &frames {
            std::hint::black_box(link_dec.message(f).unwrap());
        }
    }
    let link_batched = start.elapsed();
    let start = Instant::now();
    for _ in 0..n {
        let frames = link_enc.events(vec![env.clone()]);
        std::hint::black_box(link_dec.message(&frames[0]).unwrap());
    }
    let link_single = start.elapsed();
    let done = ServerMsg::Reply { id: 7, result: Ok(Reply::Done) };
    let calls = 1_000_000;
    let start = Instant::now();
    for _ in 0..calls {
        std::hint::black_box(link_dec.message(&link_enc.message(&done)).unwrap());
    }
    let link_done = start.elapsed();
    vec![
        Result {
            name: "Link: one streamed token",
            value: n as f64 / link_single.as_secs_f64() / 1e6,
            unit: "M/s",
            note: "a lone delta as the host sends it (own frame kind)".into(),
        },
        Result {
            name: "Link: streamed tokens, 512 per frame",
            value: (rounds * 512) as f64 / link_batched.as_secs_f64() / 1e6,
            unit: "M/s",
            note: format!("{:.1} bytes per delta", link_bytes as f64 / 512.0),
        },
        Result {
            name: "Link: `Done` reply encode+decode",
            value: link_done.as_nanos() as f64 / calls as f64,
            unit: "ns",
            note: "5 bytes".into(),
        },
        Result {
            name: "Delta encode+decode, single",
            value: n as f64 / elapsed.as_secs_f64() / 1e6,
            unit: "M/s",
            note: format!("{} bytes per lone delta", bytes / n),
        },
        Result {
            name: "Delta encode+decode, wire batches",
            value: (rounds * 512) as f64 / batched.as_secs_f64() / 1e6,
            unit: "M/s",
            note: format!("{:.1} bytes per delta in a 512-delta frame", batch_bytes as f64 / 512.0),
        },
    ]
}

// ── Engine ───────────────────────────────────────────────────────────────

/// A fake agent that streams `deltas` tokens for every prompt.
fn streaming_launcher(deltas: usize) -> orbit_core::engine::Launcher {
    Box::new(move |_kind, spec, events| {
        let (input, rx) = mpsc::bounded_blocking_async::<SessionInput>(64);
        let thread = spec.thread;
        tokio::spawn(async move {
            let _ = events.send((thread, AgentEvent::Ready { session_id: "bench".into(), model: None }));
            while let Ok(msg) = rx.recv().await {
                if let SessionInput::Prompt { .. } = msg {
                    let _ = events.send((thread, AgentEvent::TurnStarted { prompt: None }));
                    for _ in 0..deltas {
                        let _ = events.send((
                            thread,
                            AgentEvent::Delta { block: "b".into(), kind: BlockKind::Text, text: "tok ".into() },
                        ));
                    }
                    let _ = events.send((
                        thread,
                        AgentEvent::TurnFinished { outcome: TurnOutcome::Completed, usage: Usage::default(), duration_ms: 1 },
                    ));
                }
            }
        });
        SessionHandle { input }
    })
}

fn wait_for(feed: &mut Feed, mut done: impl FnMut(&Update) -> bool) {
    block_on(async {
        loop {
            let batch = feed.next_batch().await.expect("feed open");
            if batch.iter().any(&mut done) {
                return;
            }
        }
    })
}

fn engine_streaming() -> Vec<Result> {
    let deltas = 200_000;
    let dir = tempfile::tempdir().unwrap();
    let engine = EngineHandle::start_with(
        EngineOptions { data_dir: dir.path().to_owned(), probe_providers: false },
        streaming_launcher(deltas),
    )
    .unwrap();
    let backend = Backend::Local(engine.clone());
    let mut feed = backend.feed();
    wait_for(&mut feed, |u| matches!(u, Update::Reset(_)));
    let Ok(Ok(Reply::Thread(id))) = engine
        .call(Command::CreateThread { project: None, config: ThreadConfig::default(), worktree: false })
        .recv()
    else {
        panic!("thread")
    };
    engine.call(Command::OpenThread { id }).recv().unwrap().unwrap();
    let start = Instant::now();
    engine.send(Command::Send { thread: id, text: "go".into(), attachments: vec![], steer: false });
    let mut seen = 0usize;
    wait_for(&mut feed, |u| {
        if let Update::Events(envs) = u {
            for env in envs {
                match &env.event {
                    Event::ItemDelta { .. } => seen += 1,
                    Event::ThreadUpserted(t) if t.turns == 1 && t.status == ThreadStatus::Idle => return true,
                    _ => {}
                }
            }
        }
        false
    });
    let elapsed = start.elapsed();
    engine.shutdown();
    vec![Result {
        name: "Engine streaming (agent → engine → UI feed)",
        value: seen as f64 / elapsed.as_secs_f64() / 1e6,
        unit: "M deltas/s",
        note: format!("{seen} deltas delivered in {:.0} ms", elapsed.as_secs_f64() * 1e3),
    }]
}

// ── Reducer ──────────────────────────────────────────────────────────────

fn reducer() -> Vec<Result> {
    use orbit_net::state::ClientState;
    let mut state = ClientState::default();
    let thread = ThreadId::new();
    let item = Item { id: ItemId::new(), turn: 1, status: ItemStatus::Streaming, body: ItemBody::Assistant { text: String::new() } };
    state.set_timeline(thread, vec![item.clone()]);
    let n = 1_000_000;
    let batch: Vec<Envelope> = (0..1000)
        .map(|_| Envelope { seq: 0, event: Event::ItemDelta { thread, item: item.id, delta: Delta::Text("tok ".into()) } })
        .collect();
    let start = Instant::now();
    for _ in 0..n / 1000 {
        std::hint::black_box(state.apply(vec![Update::Events(batch.clone())]));
    }
    let elapsed = start.elapsed();
    vec![Result {
        name: "UI state apply (deltas)",
        value: n as f64 / elapsed.as_secs_f64() / 1e6,
        unit: "M deltas/s",
        note: "includes cloning each 1,000-delta batch".into(),
    }]
}

// ── Storage ──────────────────────────────────────────────────────────────

fn storage() -> Vec<Result> {
    use orbit_core::store::Store;
    let dir = tempfile::tempdir().unwrap();
    let threads = 1_000;
    let items_per_thread = 3_000;
    let big = ThreadId::new();
    {
        let mut store = Store::open(dir.path()).unwrap();
        for i in 0..threads {
            let id = if i == 0 { big } else { ThreadId::new() };
            store
                .commit(Event::ThreadUpserted(Box::new(Thread { id, title: format!("Thread {i}"), ..Default::default() })))
                .unwrap();
        }
        for i in 0..items_per_thread {
            let body = if i % 2 == 0 {
                ItemBody::User(UserMessage { text: "Please refactor the parser and add tests.".into(), ..Default::default() })
            } else {
                ItemBody::Assistant { text: "Done. I split the tokenizer out and added 14 tests.\n".repeat(8) }
            };
            store
                .commit(Event::ItemUpserted {
                    thread: big,
                    item: Box::new(Item { id: ItemId::new(), turn: i / 2, status: ItemStatus::Done, body }),
                })
                .unwrap();
        }
        store.sync().unwrap();
    }
    let start = Instant::now();
    let mut store = Store::open(dir.path()).unwrap();
    let cold = start.elapsed();
    let start = Instant::now();
    let n = store.load_timeline(big).unwrap().items().len();
    let open = start.elapsed();
    let log_size = std::fs::metadata(dir.path().join("threads").join(format!("{big}.log"))).unwrap().len();
    vec![
        Result {
            name: "Cold start, 1,000 threads",
            value: cold.as_secs_f64() * 1e3,
            unit: "ms",
            note: "open the store and rebuild every thread record".into(),
        },
        Result {
            name: "Open a 3,000-item thread",
            value: open.as_secs_f64() * 1e3,
            unit: "ms",
            note: format!("{n} items, {:.1} MB on disk", log_size as f64 / 1e6),
        },
    ]
}

// ── Network ──────────────────────────────────────────────────────────────

fn network() -> Vec<Result> {
    let dir = tempfile::tempdir().unwrap();
    let engine = EngineHandle::start_with(
        EngineOptions { data_dir: dir.path().to_owned(), probe_providers: false },
        streaming_launcher(100_000),
    )
    .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let server_engine = engine.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            orbit_net::server::serve_on(server_engine, listener, "bench-token".into()).await.unwrap();
        });
    });
    let url = format!("ws://{}/", rx.recv().unwrap());
    let backend = Backend::Remote(RemoteClient::connect(url, "bench-token".into()).unwrap());
    let mut feed = backend.feed();
    wait_for(&mut feed, |u| matches!(u, Update::Status(orbit_net::ConnectionStatus::Connected)));

    // Round trips: a cheap command answered by the engine.
    let Ok(Ok(Reply::Thread(id))) =
        block_on(backend.call(Command::CreateThread { project: None, config: ThreadConfig::default(), worktree: false }))
    else {
        panic!("thread")
    };
    // A command that is pure engine work (an empty patch changes nothing).
    let noop = || Command::UpdateThread { id, patch: ThreadPatch::default() };
    let mut samples = Vec::with_capacity(5_000);
    for _ in 0..5_000 {
        let start = Instant::now();
        backend.call(noop()).recv().unwrap().unwrap();
        samples.push(start.elapsed());
    }
    samples.sort();
    let mut local = Vec::with_capacity(5_000);
    for _ in 0..5_000 {
        let start = Instant::now();
        engine.call(noop()).recv().unwrap().unwrap();
        local.push(start.elapsed());
    }
    local.sort();

    // Streaming throughput over the socket.
    block_on(backend.call(Command::OpenThread { id })).unwrap().unwrap();
    let start = Instant::now();
    backend.send(Command::Send { thread: id, text: "go".into(), attachments: vec![], steer: false });
    let mut seen = 0usize;
    wait_for(&mut feed, |u| {
        if let Update::Events(envs) = u {
            for env in envs {
                match &env.event {
                    Event::ItemDelta { .. } => seen += 1,
                    Event::ThreadUpserted(t) if t.turns == 1 && t.status == ThreadStatus::Idle => return true,
                    _ => {}
                }
            }
        }
        false
    });
    let elapsed = start.elapsed();
    backend.shutdown();
    engine.shutdown();
    vec![
        Result {
            name: "In-process command round trip p50",
            value: percentile(&local, 0.5).as_secs_f64() * 1e6,
            unit: "µs",
            note: format!("p99 {:.1} µs, UI thread → engine thread → reply", percentile(&local, 0.99).as_secs_f64() * 1e6),
        },
        Result {
            name: "WebSocket round trip p50",
            value: percentile(&samples, 0.5).as_secs_f64() * 1e6,
            unit: "µs",
            note: format!("p99 {:.0} µs, loopback, includes the engine", percentile(&samples, 0.99).as_secs_f64() * 1e6),
        },
        Result {
            name: "WebSocket streaming",
            value: seen as f64 / elapsed.as_secs_f64() / 1e6,
            unit: "M deltas/s",
            note: format!("{seen} deltas engine → socket → client state"),
        },
    ]
}
