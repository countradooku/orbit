//! The background host's engine side: activity, terminal re-attach with
//! scrollback, held output, sleep and wake.

use super::*;

fn host(h: &Harness, command: HostCommand) -> Reply {
    h.call(Command::Host(command))
}

fn activity(h: &Harness) -> HostActivity {
    match host(h, HostCommand::Activity) {
        Reply::Host(HostReply::Activity(a)) => a,
        other => panic!("{other:?}"),
    }
}

fn attach(h: &Harness, thread: ThreadId, cwd: &std::path::Path) -> (TermSession, Vec<u8>) {
    attach_with(h, thread, cwd, true)
}

fn attach_with(h: &Harness, thread: ThreadId, cwd: &std::path::Path, wake: bool) -> (TermSession, Vec<u8>) {
    match host(h, HostCommand::AttachTerminal { thread, cwd: cwd.display().to_string(), cols: 100, rows: 30, wake }) {
        Reply::Host(HostReply::Terminal { session, scrollback }) => (session, scrollback),
        other => panic!("{other:?}"),
    }
}

/// Collects `term`'s output until it contains `needle`, answering the
/// cursor-position query ConPTY sends when a shell starts. Returns what was
/// seen, and whether the terminal went to sleep or woke meanwhile.
fn output_until(h: &mut Harness, term: TermId, needle: &str, within: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + within;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match h.events.try_recv() {
            Ok(Envelope { event: Event::TerminalOutput { term: t, data }, .. }) if t == term => {
                if memchr::memmem::find(&data, b"\x1b[6n").is_some() {
                    h.call(Command::TerminalInput { term, data: b"\x1b[1;1R".to_vec() });
                }
                seen.extend(data);
                if String::from_utf8_lossy(&seen).contains(needle) {
                    return Some(seen);
                }
            }
            Ok(Envelope { event: Event::TerminalExited { term: t, .. }, .. }) if t == term => {
                panic!("the terminal exited: {}", String::from_utf8_lossy(&seen))
            }
            Ok(env) => h.apply(env.event),
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    None
}

fn type_line(h: &Harness, term: TermId, line: &str) {
    h.call(Command::TerminalInput { term, data: format!("{line}\r").into_bytes() });
}

#[test]
fn activity_counts_running_turns() {
    let mut h = Harness::new();
    let thread = h.new_thread();
    assert!(!activity(&h).is_live(now_ms()));
    h.send(thread, "slow");
    h.wait("the turn to start", |h| h.thread(thread).status.is_busy());
    let a = activity(&h);
    assert_eq!(a.working, vec![thread]);
    assert!(a.is_live(now_ms()));
    assert_eq!(a.protocol, PROTOCOL_VERSION);
    h.call(Command::Interrupt { thread });
    h.wait("the turn to stop", |h| !h.thread(thread).status.is_busy());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !activity(&h).working.is_empty() {
        assert!(Instant::now() < deadline, "the turn still counts as working");
        std::thread::sleep(Duration::from_millis(10));
    }
    // In-process there is no host to restart; holds and windows are no-ops.
    assert!(h.engine.call(Command::Host(HostCommand::Restart { when: RestartWhen::Now })).recv().unwrap().is_err());
    assert_eq!(host(&h, HostCommand::AttachUi), Reply::Done);
}

#[test]
fn a_terminal_reattaches_with_its_scrollback_and_nothing_is_lost_or_doubled() {
    let mut h = Harness::new();
    let thread = h.new_thread();
    let dir = tempfile::tempdir().unwrap();

    // The first screen opens the thread's terminal.
    let (first, scrollback) = attach(&h, thread, dir.path());
    assert!(scrollback.is_empty());
    assert_eq!(first.thread, Some(thread));
    assert!(!first.sleeping);
    let term = first.term;
    host(&h, HostCommand::TerminalReady { term });
    type_line(&h, term, "echo orbit-before");
    output_until(&mut h, term, "orbit-before", Duration::from_secs(20)).expect("first output");

    // The app closes and comes back: the same terminal, with what it showed.
    let (again, scrollback) = attach(&h, thread, dir.path());
    assert_eq!(again.term, term, "the thread's terminal is found again");
    assert!(String::from_utf8_lossy(&scrollback).contains("orbit-before"));
    let listed = match host(&h, HostCommand::Terminals) {
        Reply::Host(HostReply::Terminals(list)) => list,
        other => panic!("{other:?}"),
    };
    assert_eq!(listed.iter().map(|s| (s.term, s.thread)).collect::<Vec<_>>(), vec![(term, Some(thread))]);

    // Output after the reply is held until the screen is ready…
    type_line(&h, term, "echo orbit-after");
    assert!(
        output_until(&mut h, term, "orbit-after", Duration::from_millis(700)).is_none(),
        "output reached clients before the screen said it was ready"
    );
    // …and then sent once, in full.
    host(&h, HostCommand::TerminalReady { term });
    let after = output_until(&mut h, term, "orbit-after", Duration::from_secs(10)).expect("held output");
    // What the screen drew (the scrollback, then what was held and what
    // followed) is exactly what the terminal wrote: nothing lost or doubled.
    let Reply::TerminalTail { data: written, .. } = h.call(Command::ReadTerminal { term, max_bytes: 0 }) else { panic!() };
    let drawn = [scrollback.as_slice(), after.as_slice()].concat();
    assert!(
        written.starts_with(&drawn),
        "drawn {:?}\nwritten {:?}",
        String::from_utf8_lossy(&drawn),
        String::from_utf8_lossy(&written)
    );
    h.call(Command::CloseTerminal { term });
}

#[test]
fn a_sleeping_terminal_keeps_its_scrollback_and_wakes_on_input() {
    let mut h = Harness::new();
    let thread = h.new_thread();
    let dir = tempfile::tempdir().unwrap();
    let (session, _) = attach(&h, thread, dir.path());
    let term = session.term;
    host(&h, HostCommand::TerminalReady { term });
    type_line(&h, term, "echo orbit-asleep");
    output_until(&mut h, term, "orbit-asleep", Duration::from_secs(20)).expect("output");

    host(&h, HostCommand::SleepTerminal { term });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut slept = false;
    // Its shell stops; the exit is not reported as one.
    while Instant::now() < deadline && !(slept && activity(&h).terminals == 0) {
        match h.events.try_recv() {
            Ok(Envelope { event: Event::TerminalSleep { term: t, sleeping: true }, .. }) if t == term => slept = true,
            Ok(Envelope { event: Event::TerminalExited { term: t, .. }, .. }) if t == term => panic!("a sleeping terminal was reported as exited"),
            Ok(env) => h.apply(env.event),
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    assert!(slept, "no sleep event");
    let a = activity(&h);
    assert_eq!((a.terminals, a.sleeping_terminals), (0, 1), "no shell runs while it sleeps");

    // A screen restored at startup shows it asleep, with its scrollback.
    let (restored, scrollback) = attach_with(&h, thread, dir.path(), false);
    assert_eq!(restored.term, term);
    assert!(restored.sleeping);
    assert!(String::from_utf8_lossy(&scrollback).contains("orbit-asleep"));
    host(&h, HostCommand::TerminalReady { term });

    // Input wakes it (as focusing or attaching would) and reaches the new shell.
    type_line(&h, term, "echo orbit-awake");
    let woke = output_until(&mut h, term, "orbit-awake", Duration::from_secs(20)).expect("the new shell answers");
    assert!(String::from_utf8_lossy(&woke).contains("Woke from sleep"));
    let (session, scrollback) = attach(&h, thread, dir.path());
    assert_eq!(session.term, term, "it keeps its id");
    assert!(!session.sleeping);
    let text = String::from_utf8_lossy(&scrollback);
    assert!(text.contains("orbit-asleep") && text.contains("orbit-awake"), "scrollback: {text}");

    // Closing a sleeping terminal reports its exit (there is no shell to).
    host(&h, HostCommand::TerminalReady { term });
    host(&h, HostCommand::SleepTerminal { term });
    let deadline = Instant::now() + Duration::from_secs(10);
    while activity(&h).sleeping_terminals == 0 || activity(&h).terminals != 0 {
        assert!(Instant::now() < deadline, "never fell asleep");
        while let Ok(env) = h.events.try_recv() {
            h.apply(env.event);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    h.call(Command::CloseTerminal { term });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "no exit for the closed sleeping terminal");
        match h.events.try_recv() {
            Ok(Envelope { event: Event::TerminalExited { term: t, .. }, .. }) if t == term => break,
            Ok(env) => h.apply(env.event),
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    assert_eq!(activity(&h).sleeping_terminals, 0);
}
