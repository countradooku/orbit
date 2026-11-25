//! Calm notifications: tell the user when an agent needs them, fails or
//! finishes, and nothing more.
//!
//! * **Transitions only.** A thread notifies when it gets a request it did
//!   not have, fails, or finishes a turn; never on the first load, never
//!   twice for the same request.
//! * **Not for what you are looking at.** Threads on screen in a focused
//!   window (the open thread and every tile) are skipped.
//! * **Bursts coalesce.** Notes that arrive within [`QUIET`] of each other
//!   become one notification ("3 threads need you"), flushed at most
//!   [`MAX_WAIT`] after the first.
//! * **The window decides the channel.** In front: an in-app toast. In the
//!   background: a system notification, the taskbar or dock asks for
//!   attention, and (if the user allows it) one short system sound.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use orbit_proto::*;

use crate::thread::display_title;
use crate::ui::{Attention, request_line};

/// A burst ends after this long without a new note.
pub const QUIET: Duration = Duration::from_millis(450);
/// A burst is shown no later than this after its first note.
pub const MAX_WAIT: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NoteKind {
    /// Most urgent first: the order also ranks a burst's headline.
    NeedsYou,
    Failed,
    Finished,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub thread: ThreadId,
    pub kind: NoteKind,
    pub title: String,
    pub detail: String,
}

struct Seen {
    attention: Attention,
    requests: Vec<RequestId>,
}

/// Remembers what each thread last looked like, to report transitions.
#[derive(Default)]
pub struct Tracker {
    seen: HashMap<ThreadId, Seen>,
    primed: bool,
}

impl Tracker {
    /// Compares the threads with what was seen before and returns the notes
    /// the user asked for. `looking` names threads on screen in a focused
    /// window: they update the record but never notify.
    pub fn observe<'a>(
        &mut self,
        threads: impl Iterator<Item = &'a Thread>,
        prefs: &NotificationSettings,
        looking: impl Fn(ThreadId) -> bool,
    ) -> Vec<Note> {
        let first = !self.primed;
        self.primed = true;
        let mut notes = Vec::new();
        let mut live = HashSet::new();
        for thread in threads {
            live.insert(thread.id);
            let attention = Attention::of(thread);
            let requests: Vec<RequestId> = thread.pending.iter().map(|p| p.request.id).collect();
            let before = self.seen.insert(thread.id, Seen { attention, requests: requests.clone() });
            let Some(before) = before else {
                // A thread seen for the first time (just created, or the first
                // load) has nothing to transition from.
                continue;
            };
            if first || thread.archived || thread.snoozed_until.is_some() || looking(thread.id) {
                continue;
            }
            let title = display_title(thread).to_owned();
            let fresh = thread.pending.iter().find(|p| !before.requests.contains(&p.request.id));
            let note = match (fresh, attention) {
                (Some(pending), _) if prefs.requests => {
                    Some((NoteKind::NeedsYou, request_line(&pending.request.kind)))
                }
                (None, Attention::Failed | Attention::RateLimited) if before.attention != attention && prefs.failed => {
                    let detail = match &thread.status {
                        ThreadStatus::Failed { message } => message.clone(),
                        ThreadStatus::RateLimited { resets_ms } => crate::format::resets_at(*resets_ms, now_ms()),
                        _ => "The turn failed.".into(),
                    };
                    Some((NoteKind::Failed, detail))
                }
                (None, Attention::Unread) if before.attention == Attention::Working && prefs.finished => {
                    Some((NoteKind::Finished, "Finished. The result is ready.".into()))
                }
                _ => None,
            };
            if let Some((kind, detail)) = note {
                notes.push(Note { thread: thread.id, kind, title, detail });
            }
        }
        self.seen.retain(|id, _| live.contains(id));
        notes
    }
}

/// What one notification for a burst says, and where clicking it goes.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub kind: NoteKind,
    pub title: String,
    pub body: String,
    /// The thread to open; `None` opens the Inbox.
    pub open: Option<ThreadId>,
    /// Every thread in the burst, so their toasts can be closed when one of
    /// them is opened.
    pub threads: Vec<ThreadId>,
}

/// Folds a burst into one notification. Several notes for one thread keep
/// the most urgent.
pub fn summarize(notes: &[Note]) -> Option<Summary> {
    let mut per_thread: Vec<&Note> = Vec::new();
    for note in notes {
        match per_thread.iter_mut().find(|n| n.thread == note.thread) {
            Some(existing) if note.kind < existing.kind => *existing = note,
            Some(_) => {}
            None => per_thread.push(note),
        }
    }
    per_thread.sort_by_key(|n| n.kind);
    let first = *per_thread.first()?;
    let threads: Vec<ThreadId> = per_thread.iter().map(|n| n.thread).collect();
    if per_thread.len() == 1 {
        return Some(Summary {
            kind: first.kind,
            title: first.title.clone(),
            body: first.detail.clone(),
            open: Some(first.thread),
            threads,
        });
    }
    let count = |kind: NoteKind| per_thread.iter().filter(|n| n.kind == kind).count();
    let (needs, failed, finished) = (count(NoteKind::NeedsYou), count(NoteKind::Failed), count(NoteKind::Finished));
    let n = per_thread.len();
    let title = if needs == n {
        format!("{n} threads need you")
    } else if failed == n {
        format!("{n} turns failed")
    } else if finished == n {
        format!("{n} threads finished")
    } else {
        let mut parts = Vec::new();
        if needs > 0 {
            parts.push(format!("{needs} need{} you", if needs == 1 { "s" } else { "" }));
        }
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        if finished > 0 {
            parts.push(format!("{finished} finished"));
        }
        parts.join(" · ")
    };
    let mut body: Vec<&str> = per_thread.iter().take(3).map(|n| n.title.as_str()).collect();
    let more = n.saturating_sub(3);
    let more_text = format!("and {more} more");
    if more > 0 {
        body.push(&more_text);
    }
    Some(Summary { kind: first.kind, title, body: body.join(", "), open: None, threads })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(title: &str) -> Thread {
        Thread { id: ThreadId::new(), title: title.into(), ..Default::default() }
    }

    fn request(kind: RequestKind) -> PendingRequest {
        PendingRequest {
            item: ItemId::new(),
            request: AgentRequest { id: RequestId::new(), provider_ref: String::new(), kind, resolution: None },
            created_ms: 0,
        }
    }

    fn command(cmd: &str) -> PendingRequest {
        request(RequestKind::Command { command: cmd.into(), cwd: String::new(), reason: String::new() })
    }

    #[test]
    fn transitions_notify_once_and_never_on_first_load() {
        let prefs = NotificationSettings::default();
        let mut tracker = Tracker::default();
        let mut a = thread("Fix the parser");
        a.status = ThreadStatus::Waiting;
        a.pending = vec![command("cargo test")];
        assert!(tracker.observe([&a].into_iter(), &prefs, |_| false).is_empty(), "first load is quiet");

        // The same request again: nothing new.
        assert!(tracker.observe([&a].into_iter(), &prefs, |_| false).is_empty());

        // A second request on the same thread is new.
        a.pending.push(command("cargo build"));
        let notes = tracker.observe([&a].into_iter(), &prefs, |_| false);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].kind, NoteKind::NeedsYou);
        assert!(notes[0].detail.contains("cargo build"), "{}", notes[0].detail);

        // Working → unread result is a finish.
        a.pending.clear();
        a.status = ThreadStatus::Running;
        assert!(tracker.observe([&a].into_iter(), &prefs, |_| false).is_empty());
        a.status = ThreadStatus::Idle;
        a.unread = true;
        let notes = tracker.observe([&a].into_iter(), &prefs, |_| false);
        assert_eq!(notes.iter().map(|n| n.kind).collect::<Vec<_>>(), vec![NoteKind::Finished]);
    }

    #[test]
    fn what_you_are_looking_at_and_what_you_silenced_stay_quiet() {
        let mut prefs = NotificationSettings::default();
        let mut tracker = Tracker::default();
        let mut a = thread("Open thread");
        let mut b = thread("Snoozed thread");
        let mut c = thread("Background thread");
        tracker.observe([&a, &b, &c].into_iter(), &prefs, |_| false);
        b.snoozed_until = Some(u64::MAX);
        for t in [&mut a, &mut b, &mut c] {
            t.status = ThreadStatus::Failed { message: "boom".into() };
        }
        let looking = a.id;
        let notes = tracker.observe([&a, &b, &c].into_iter(), &prefs, |id| id == looking);
        assert_eq!(notes.iter().map(|n| n.thread).collect::<Vec<_>>(), vec![c.id]);

        // Turned off in settings: nothing.
        prefs.requests = false;
        c.status = ThreadStatus::Waiting;
        c.pending = vec![command("rm -rf target")];
        assert!(tracker.observe([&a, &b, &c].into_iter(), &prefs, |_| false).is_empty());
    }

    #[test]
    fn bursts_fold_into_one_notification() {
        let (a, b, c) = (ThreadId::new(), ThreadId::new(), ThreadId::new());
        let note = |thread, kind, title: &str| Note { thread, kind, title: title.into(), detail: "d".into() };
        let one = summarize(&[note(a, NoteKind::Finished, "A")]).unwrap();
        assert_eq!((one.title.as_str(), one.open), ("A", Some(a)));

        let same = summarize(&[note(a, NoteKind::NeedsYou, "A"), note(b, NoteKind::NeedsYou, "B")]).unwrap();
        assert_eq!(same.title, "2 threads need you");
        assert_eq!(same.open, None, "a burst opens the Inbox");

        // One thread finishing and then asking keeps the question.
        let mixed = summarize(&[
            note(a, NoteKind::Finished, "A"),
            note(a, NoteKind::NeedsYou, "A"),
            note(b, NoteKind::Failed, "B"),
            note(c, NoteKind::Finished, "C"),
        ])
        .unwrap();
        assert_eq!(mixed.kind, NoteKind::NeedsYou);
        assert_eq!(mixed.title, "1 needs you · 1 failed · 1 finished");
        assert_eq!(mixed.body, "A, B, C");
        assert!(summarize(&[]).is_none());
    }
}
