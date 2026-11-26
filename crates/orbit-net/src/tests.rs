//! Loopback tests: a real orbitd server and remote client over TCP.

use std::time::{Duration, Instant};

use crossfire::mpsc;
use orbit_core::provider::SessionHandle;
use orbit_core::{EngineHandle, EngineOptions};
use orbit_proto::*;

use crate::{Backend, ConnectionStatus, RemoteClient, Update};

fn engine(dir: &std::path::Path) -> EngineHandle {
    EngineHandle::start_with(
        EngineOptions { data_dir: dir.to_owned(), probe_providers: false },
        Box::new(|_, _, _| {
            let (input, _rx) = mpsc::bounded_blocking(1);
            SessionHandle { input }
        }),
    )
    .unwrap()
}

/// Starts a server on an ephemeral port and returns its URL.
fn server(engine: EngineHandle, token: &str) -> String {
    let token = token.to_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            crate::server::serve_on(engine, listener, token).await.unwrap();
        });
    });
    format!("ws://{}/", rx.recv().unwrap())
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

/// Collects updates until `done` holds.
fn collect(feed: &mut crate::Feed, done: impl Fn(&[Update]) -> bool) -> Vec<Update> {
    block_on(async {
        let mut all = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(&all) {
            assert!(Instant::now() < deadline, "timed out; got {all:?}");
            match tokio::time::timeout(Duration::from_secs(5), feed.next_batch()).await {
                Ok(Some(batch)) => all.extend(batch),
                _ => panic!("feed ended; got {all:?}"),
            }
        }
        all
    })
}

fn has_thread(updates: &[Update], title: &str) -> bool {
    updates.iter().any(|u| match u {
        Update::Events(envs) => envs.iter().any(|e| matches!(&e.event, Event::ThreadUpserted(t) if t.title == title)),
        Update::Reset(s) => s.threads.iter().any(|t| t.title == title),
        _ => false,
    })
}

#[test]
fn remote_client_sees_snapshot_events_and_replies() {
    let dir = tempfile::tempdir().unwrap();
    let url = server(engine(dir.path()), "secret");
    let backend = Backend::Remote(RemoteClient::connect(url, "secret".into()).unwrap());
    let mut feed = backend.feed();
    collect(&mut feed, |u| u.iter().any(|u| matches!(u, Update::Status(ConnectionStatus::Connected))));

    let reply = block_on(backend.call(Command::CreateThread { project: None, config: ThreadConfig::default(), worktree: false }))
        .unwrap()
        .unwrap();
    let Reply::Thread(id) = reply else { panic!("{reply:?}") };
    block_on(backend.call(Command::UpdateThread {
        id,
        patch: ThreadPatch { title: Some("Over the wire".into()), ..Default::default() },
    }))
    .unwrap()
    .unwrap();
    collect(&mut feed, |u| has_thread(u, "Over the wire"));
}

#[test]
fn wrong_token_is_refused_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let url = server(engine(dir.path()), "secret");
    let backend = Backend::Remote(RemoteClient::connect(url, "guess".into()).unwrap());
    let mut feed = backend.feed();
    let updates = collect(&mut feed, |u| {
        u.iter().any(|u| matches!(u, Update::Status(ConnectionStatus::Disconnected { .. })))
    });
    assert!(updates.iter().any(|u| matches!(
        u,
        Update::Status(ConnectionStatus::Disconnected { reason }) if reason == "The access token is wrong."
    )));
    assert!(!updates.iter().any(|u| matches!(u, Update::Reset(_))), "no data before authentication");
}

#[test]
fn local_backend_feed_starts_with_a_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Backend::Local(engine(dir.path()));
    let mut feed = backend.feed();
    collect(&mut feed, |u| u.iter().any(|u| matches!(u, Update::Reset(_))));
    let Reply::Thread(id) = block_on(backend.call(Command::CreateThread {
        project: None,
        config: ThreadConfig::default(),
        worktree: false,
    }))
    .unwrap()
    .unwrap() else {
        panic!()
    };
    block_on(backend.call(Command::UpdateThread { id, patch: ThreadPatch { title: Some("Local".into()), ..Default::default() } }))
        .unwrap()
        .unwrap();
    collect(&mut feed, |u| has_thread(u, "Local"));
}

/// With ordered replies, a command's events are applied by the time its
/// reply resolves: a client reading its state after `await` sees them.
#[test]
fn ordered_replies_follow_the_events_they_caused() {
    use std::sync::{Arc, Mutex};

    use crate::state::ClientState;

    let dir = tempfile::tempdir().unwrap();
    let url = server(engine(dir.path()), "secret");
    let client = RemoteClient::connect(url, "secret".into()).unwrap();
    client.order_replies(true);
    let backend = Backend::Remote(client);
    let mut feed = backend.feed();
    let state = Arc::new(Mutex::new(ClientState::default()));
    // Apply every update on its own thread, as the app's pump does, and
    // slowly, like a busy UI: an unordered reply would overtake its events.
    let pump_state = state.clone();
    std::thread::spawn(move || {
        block_on(async move {
            while let Some(batch) = feed.next_batch().await {
                std::thread::sleep(Duration::from_millis(3));
                pump_state.lock().unwrap().apply(batch);
            }
        })
    });
    let connected = Instant::now();
    while !matches!(state.lock().unwrap().connection, Some(ConnectionStatus::Connected)) {
        assert!(connected.elapsed() < Duration::from_secs(5), "never connected");
        std::thread::sleep(Duration::from_millis(5));
    }
    let Ok(Ok(Reply::Thread(id))) =
        block_on(backend.call(Command::CreateThread { project: None, config: ThreadConfig::default(), worktree: false }))
    else {
        panic!("thread")
    };
    assert!(state.lock().unwrap().threads.contains_key(&id), "the new thread is there when the reply is");
    for i in 0..50 {
        let title = format!("Title {i}");
        block_on(backend.call(Command::UpdateThread {
            id,
            patch: ThreadPatch { title: Some(title.clone()), ..Default::default() },
        }))
        .unwrap()
        .unwrap();
        assert_eq!(state.lock().unwrap().threads[&id].title, title, "round {i}");
    }
}

/// The background host stops serving as soon as a restart is due (orbitd's
/// lifetime loop returns and it closes its endpoint first thing). The
/// client that asked still gets its answer, over a control request and over
/// the binary protocol alike.
#[test]
fn a_restart_is_answered_before_the_host_stops() {
    use std::sync::Arc;

    use crate::endpoint::{EndpointOptions, start_with};
    use crate::host::HostControl;

    let root = tempfile::tempdir().unwrap();
    for round in 0..12 {
        let dir = root.path().join(round.to_string());
        let engine = engine(&dir);
        let control = Arc::new(HostControl::default());
        let options = EndpointOptions { control: Some(control.clone()), local: true };
        let endpoint = start_with(engine.clone(), &dir, "host", options).unwrap();
        let info = endpoint.info().clone();
        // What orbitd does once a restart is requested.
        let closer = {
            let control = control.clone();
            std::thread::spawn(move || {
                block_on(async {
                    while control.restart().is_none() {
                        control.changed().await;
                    }
                });
                endpoint.close();
            })
        };
        let when = if round % 4 < 2 { RestartWhen::Now } else { RestartWhen::WhenIdle };
        if round % 2 == 0 {
            let status = crate::host::control(&info, "restart", Some(when)).unwrap_or_else(|e| panic!("round {round}: {e}"));
            assert!(status.restart_pending, "round {round}");
        } else {
            let backend = Backend::Remote(RemoteClient::connect(info.client_url().to_owned(), info.token.clone()).unwrap());
            let mut feed = backend.feed();
            collect(&mut feed, |all| all.iter().any(|u| matches!(u, Update::Status(ConnectionStatus::Connected))));
            let reply = block_on(async { tokio::time::timeout(Duration::from_secs(10), backend.call(Command::Host(HostCommand::Restart { when }))).await });
            assert!(matches!(reply, Ok(Ok(Ok(Reply::Done)))), "round {round}: {reply:?}");
            backend.shutdown();
        }
        closer.join().unwrap();
        assert_eq!(control.restart(), Some(when));
        engine.shutdown();
    }
}
