//! Review state that outlives the sheet: view preferences, and per thread
//! the reviewed hunks and the comments not yet sent.

use std::collections::{HashMap, HashSet};

use gpui_kit::{App, Global};
use orbit_proto::ThreadId;

use super::model::Comment;

/// How the review sheet shows diffs, beyond the saved settings (side by
/// side and collapsed files live in Settings → Review). Kept for the
/// session; the next sheet starts the way the last one was left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewPrefs {
    pub wrap: bool,
    /// Blue and orange instead of green and red.
    pub colorblind: bool,
    pub tree: bool,
    pub hide_tests: bool,
}

impl Default for ReviewPrefs {
    fn default() -> Self {
        Self { wrap: false, colorblind: false, tree: true, hide_tests: false }
    }
}

impl Global for ReviewPrefs {}

/// What the user has done in review, per thread, for this session.
#[derive(Default)]
pub struct ReviewMarks {
    threads: HashMap<ThreadId, ThreadMarks>,
}

impl Global for ReviewMarks {}

#[derive(Default)]
pub struct ThreadMarks {
    /// Hunk and file ids ([`orbit_proto::diff::hunk_id`], [`orbit_proto::diff::file_id`])
    /// marked reviewed. A hunk that changes gets a new id, so it is
    /// unreviewed again.
    pub reviewed: HashSet<u64>,
    /// Comments waiting to be sent to the agent.
    pub comments: Vec<Comment>,
    /// The turn the last restore went back to, while it can be undone.
    pub restored: Option<u32>,
}

impl ReviewMarks {
    /// Reads a thread's marks without copying them.
    pub fn read<R>(cx: &App, thread: ThreadId, f: impl FnOnce(&ThreadMarks) -> R) -> R {
        static EMPTY: std::sync::OnceLock<ThreadMarks> = std::sync::OnceLock::new();
        let marks = cx.try_global::<ReviewMarks>().and_then(|m| m.threads.get(&thread));
        f(marks.unwrap_or_else(|| EMPTY.get_or_init(ThreadMarks::default)))
    }

    pub fn update<R>(cx: &mut App, thread: ThreadId, f: impl FnOnce(&mut ThreadMarks) -> R) -> R {
        let marks = cx.default_global::<ReviewMarks>();
        f(marks.threads.entry(thread).or_default())
    }
}

pub fn prefs(cx: &App) -> ReviewPrefs {
    cx.try_global::<ReviewPrefs>().cloned().unwrap_or_default()
}

pub fn update_prefs(cx: &mut App, f: impl FnOnce(&mut ReviewPrefs)) {
    f(cx.default_global::<ReviewPrefs>());
}
