//! Prompt history: Up and Down in an empty composer walk earlier prompts.
//!
//! A thread recalls its own messages; Home recalls what was sent anywhere
//! in this session. Recall starts only from an empty composer (or one still
//! showing a recalled prompt, unedited), so the arrows never fight normal
//! caret movement in a message being written.

use gpui_kit::{App, Global};

/// Prompts sent from any composer in this session, oldest first.
#[derive(Default)]
pub struct SessionPrompts(Vec<String>);

impl Global for SessionPrompts {}

/// Most prompts kept for Home's recall.
const SESSION_LIMIT: usize = 200;

pub fn remember(text: &str, cx: &mut App) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let prompts = cx.default_global::<SessionPrompts>();
    prompts.0.retain(|p| p != text);
    prompts.0.push(text.to_owned());
    if prompts.0.len() > SESSION_LIMIT {
        prompts.0.remove(0);
    }
}

pub fn session_prompts(cx: &App) -> Vec<String> {
    cx.try_global::<SessionPrompts>().map(|p| p.0.clone()).unwrap_or_default()
}

/// Where the composer is in its history walk.
#[derive(Debug, Default)]
pub struct Recall {
    /// The prompts being walked, oldest first; taken when the walk starts.
    entries: Vec<String>,
    /// Index into `entries` of the prompt shown, while walking.
    shown: Option<usize>,
}

impl Recall {
    /// Whether a walk is in progress and the text is still the recalled one.
    pub fn is_showing(&self, text: &str) -> bool {
        self.shown.and_then(|ix| self.entries.get(ix)).is_some_and(|p| p == text)
    }

    /// Starts or continues a walk backwards. `entries` is consulted only
    /// when a walk starts. Returns the prompt to show.
    pub fn older(&mut self, entries: impl FnOnce() -> Vec<String>) -> Option<&str> {
        let next = match self.shown {
            None => {
                self.entries = dedup(entries());
                self.entries.len().checked_sub(1)?
            }
            Some(0) => return None,
            Some(ix) => ix - 1,
        };
        self.shown = Some(next);
        self.entries.get(next).map(String::as_str)
    }

    /// Steps forwards. `Some("")` means the walk ended back at an empty
    /// composer; `None` means there was no walk.
    pub fn newer(&mut self) -> Option<&str> {
        let ix = self.shown?;
        if ix + 1 >= self.entries.len() {
            self.reset();
            return Some("");
        }
        self.shown = Some(ix + 1);
        self.entries.get(ix + 1).map(String::as_str)
    }

    pub fn reset(&mut self) {
        self.shown = None;
        self.entries.clear();
    }
}

/// Drops repeats of the same prompt in a row and blank prompts.
fn dedup(entries: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for e in entries {
        let e = e.trim();
        if !e.is_empty() && out.last().is_none_or(|last| last != e) {
            out.push(e.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompts() -> Vec<String> {
        ["first", "second", "second", " ", "third"].map(String::from).to_vec()
    }

    #[test]
    fn up_walks_back_and_down_returns_to_empty() {
        let mut recall = Recall::default();
        assert_eq!(recall.newer(), None, "no walk yet");
        assert_eq!(recall.older(prompts), Some("third"));
        assert!(recall.is_showing("third"));
        assert_eq!(recall.older(prompts), Some("second"));
        assert_eq!(recall.older(prompts), Some("first"));
        assert_eq!(recall.older(prompts), None, "the oldest stays");
        assert!(recall.is_showing("first"));
        assert_eq!(recall.newer(), Some("second"));
        assert_eq!(recall.newer(), Some("third"));
        assert_eq!(recall.newer(), Some(""));
        assert!(!recall.is_showing(""));
    }

    #[test]
    fn edits_end_the_walk() {
        let mut recall = Recall::default();
        recall.older(prompts);
        assert!(!recall.is_showing("third, edited"));
    }

    #[test]
    fn nothing_to_recall() {
        let mut recall = Recall::default();
        assert_eq!(recall.older(Vec::new), None);
        assert_eq!(recall.newer(), None);
    }
}
