//! The thread's preview browser: a web page beside the conversation, for
//! the dev server the agent runs.
//!
//! * One [`BrowserView`] per thread, created the first time the panel opens;
//!   no browser process exists before that.
//! * A hidden page (panel closed, or another thread on screen) is hidden at
//!   once and released after [`SUSPEND_AFTER`]; the address is kept and the
//!   page loads again when the panel shows.
//! * Addresses the agent's replies, its commands and the thread's terminal
//!   print (`localhost:5173`, `http://127.0.0.1:8000`) are offered: in the
//!   header as "Open preview", and on the empty page. A worktree's reserved
//!   port is offered too.
//! * The address and whether the panel was open are remembered per thread
//!   for the session, so a thread opens the way it was left.

use std::collections::HashMap;
use std::time::Duration;

use gpui_kit::*;
use orbit_browser::{Url, detect_urls, port_url};
use orbit_proto::*;

/// A hidden page is released (and its browser process with it) after this.
pub const SUSPEND_AFTER: Duration = Duration::from_secs(5 * 60);

/// Addresses offered at most.
const MAX_SUGGESTIONS: usize = 4;

/// A running step's output is searched only in its last bytes: a dev server
/// prints its address when it starts, and a token must not rescan 64 KiB.
const STREAMING_TAIL: usize = 2048;

/// The browser of each thread, as it was last left (this session only).
#[derive(Default)]
struct Remembered(HashMap<ThreadId, Memory>);

impl Global for Remembered {}

/// What is kept of a thread's browser when its view goes away.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Memory {
    pub url: Option<Url>,
    pub open: bool,
}

pub fn remembered(thread: ThreadId, cx: &App) -> Memory {
    cx.try_global::<Remembered>().and_then(|r| r.0.get(&thread)).cloned().unwrap_or_default()
}

pub fn remember(thread: ThreadId, memory: Memory, cx: &mut App) {
    cx.default_global::<Remembered>().0.insert(thread, memory);
}

/// Dev-server addresses seen in a thread, newest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Suggestions {
    urls: Vec<Url>,
}

impl Suggestions {
    /// Adds addresses found in newer output. Returns whether anything
    /// changed.
    pub fn add(&mut self, found: impl IntoIterator<Item = Url>) -> bool {
        let before = self.urls.clone();
        for url in found {
            self.urls.retain(|u| *u != url);
            self.urls.insert(0, url);
        }
        self.urls.truncate(MAX_SUGGESTIONS);
        self.urls != before
    }

    /// The address seen last, if any.
    pub fn latest(&self) -> Option<&Url> {
        self.urls.first()
    }

    /// What the empty page offers: what was seen, then the worktree's
    /// reserved port when nothing was seen on it.
    pub fn offered(&self, port_base: Option<u16>) -> Vec<Url> {
        let mut urls = self.urls.clone();
        if let Some(url) = port_base.and_then(|port| port_url([("PORT", port.to_string())]))
            && !urls.iter().any(|u| u.port() == url.port())
        {
            urls.push(url);
        }
        urls
    }
}

/// Addresses an item shows: a finished reply or step in full, a running
/// step by the end of its output. Replies still streaming are read when
/// they finish.
pub fn item_urls(item: &Item) -> Vec<Url> {
    let streaming = item.status == ItemStatus::Streaming;
    match &item.body {
        ItemBody::Assistant { text } if !streaming => detect_urls(text),
        ItemBody::Tool(call) if call.kind == ToolKind::Command => {
            let output = call.output.as_str();
            if streaming && output.len() > STREAMING_TAIL {
                let mut start = output.len() - STREAMING_TAIL;
                while !output.is_char_boundary(start) {
                    start += 1;
                }
                detect_urls(&output[start..])
            } else {
                detect_urls(output)
            }
        }
        _ => Vec::new(),
    }
}

/// Finds addresses in terminal output that arrives in chunks. Text after
/// the last whitespace waits for the next chunk, so an address split
/// between two reads whole (not `localhost:80` from `localhost:80|80`);
/// each address is reported once.
#[derive(Default)]
pub struct OutputScanner {
    pending: Vec<u8>,
    seen: Vec<Url>,
}

impl OutputScanner {
    /// An unfinished word longer than this is read as it is.
    const MAX_PENDING: usize = 512;

    pub fn feed(&mut self, data: &[u8]) -> Vec<Url> {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(data);
        let end = match bytes.iter().rposition(|b| b.is_ascii_whitespace()) {
            Some(ix) => ix + 1,
            None if bytes.len() > Self::MAX_PENDING => bytes.len(),
            None => {
                self.pending = bytes;
                return Vec::new();
            }
        };
        self.pending = bytes[end..].to_vec();
        if self.pending.len() > Self::MAX_PENDING {
            self.pending.clear();
        }
        let text = String::from_utf8_lossy(&bytes[..end]);
        let found: Vec<Url> = detect_urls(&text).into_iter().filter(|u| !self.seen.contains(u)).collect();
        self.seen.extend(found.iter().cloned());
        found
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputScanner, Suggestions, item_urls};
    use orbit_proto::*;

    fn url(s: &str) -> orbit_browser::Url {
        orbit_browser::Url::parse(s).unwrap()
    }

    #[test]
    fn suggestions_put_the_newest_first_without_repeats() {
        let mut s = Suggestions::default();
        assert!(s.add([url("http://localhost:3000/")]));
        assert!(s.add([url("http://localhost:5173/")]));
        assert!(!s.add(Vec::new()));
        assert!(s.add([url("http://localhost:3000/")]), "seen again: moves to the front");
        assert_eq!(s.latest(), Some(&url("http://localhost:3000/")));
        assert_eq!(s.offered(None).len(), 2);
        // The worktree's port is offered after what was seen, unless seen.
        assert_eq!(s.offered(Some(4100)).last(), Some(&url("http://localhost:4100/")));
        assert_eq!(s.offered(Some(5173)).len(), 2);
    }

    #[test]
    fn replies_are_read_when_done_and_running_commands_by_their_tail() {
        let reply = |status| Item {
            id: ItemId(1),
            turn: 1,
            status,
            body: ItemBody::Assistant { text: "Running at http://localhost:5173/ now.".into() },
        };
        assert_eq!(item_urls(&reply(ItemStatus::Done)), vec![url("http://localhost:5173/")]);
        assert!(item_urls(&reply(ItemStatus::Streaming)).is_empty());
        let output = format!("{}\n  ➜  Local:   http://localhost:4321/\n", "x".repeat(10_000));
        let call = ToolCall {
            call_id: String::new(),
            kind: ToolKind::Command,
            name: "Bash".into(),
            title: "npm run dev".into(),
            input: String::new(),
            output,
            output_blob: None,
            exit_code: None,
            files: Vec::new(),
        };
        let step = Item { id: ItemId(2), turn: 1, status: ItemStatus::Streaming, body: ItemBody::Tool(call) };
        assert_eq!(item_urls(&step), vec![url("http://localhost:4321/")]);
    }

    #[test]
    fn terminal_addresses_split_across_chunks_are_found_once() {
        let mut scanner = OutputScanner::default();
        assert!(scanner.feed(b"ready on http://localhost:80").is_empty());
        assert_eq!(scanner.feed(b"80/ in 300ms\r\n"), vec![url("http://localhost:8080/")]);
        assert!(scanner.feed(b"GET / 200 http://localhost:8080/\r\n").is_empty(), "reported once");
    }
}
