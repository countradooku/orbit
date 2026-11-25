//! Find in thread: the matching rules, kept free of UI so they are tested.
//!
//! * The open timeline is searched in memory, locally: no server round trip,
//!   results as you type.
//! * Smart case: a query with a capital letter matches case exactly; an
//!   all-lowercase query ignores case (Unicode-aware).
//! * Replies are searched as they read (the rendered Markdown, not its
//!   source), so `**bold**` is found by typing `bold` and the highlight lands
//!   on the words the reader sees.
//! * Matches are in reading order, so Next always moves down the page.

use std::ops::Range;

use orbit_proto::*;

/// What a query matches.
pub struct Needle {
    text: String,
    exact_case: bool,
}

impl Needle {
    /// `None` for a blank query.
    pub fn new(query: &str) -> Option<Self> {
        if query.trim().is_empty() {
            return None;
        }
        let exact_case = query.chars().any(char::is_uppercase);
        let text = if exact_case { query.to_owned() } else { query.to_lowercase() };
        Some(Self { text, exact_case })
    }

    /// Byte ranges of every non-overlapping match in `haystack`.
    pub fn find_all(&self, haystack: &str) -> Vec<Range<usize>> {
        if self.exact_case {
            return haystack.match_indices(&self.text).map(|(at, m)| at..at + m.len()).collect();
        }
        if haystack.is_ascii() && self.text.is_ascii() {
            // Lowercasing ASCII keeps every byte offset.
            let folded = haystack.to_ascii_lowercase();
            return folded.match_indices(&self.text).map(|(at, m)| at..at + m.len()).collect();
        }
        // Lowercasing can change byte lengths, so map each folded byte back
        // to the original character it came from.
        let mut folded = String::with_capacity(haystack.len());
        let mut origin = Vec::with_capacity(haystack.len() + 1);
        for (at, ch) in haystack.char_indices() {
            let start = folded.len();
            folded.extend(ch.to_lowercase());
            origin.extend(std::iter::repeat_n(at, folded.len() - start));
        }
        origin.push(haystack.len());
        folded
            .match_indices(&self.text)
            .filter_map(|(at, m)| {
                let (start, end) = (origin[at], origin[at + m.len()]);
                (end > start).then_some(start..end)
            })
            .collect()
    }
}

/// Which text of an item a match is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    /// The message, reply or notice text (a reply's rendered text).
    Body,
    /// A tool call's one-line title.
    Title,
    /// One step of a plan.
    Step(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub item: ItemId,
    pub part: Part,
    pub range: Range<usize>,
}

/// The texts of an item that find searches, in reading order. `rendered` is
/// a reply's text as displayed, when known.
pub fn searchable<'a>(item: &'a Item, rendered: Option<&'a str>) -> Vec<(Part, &'a str)> {
    match &item.body {
        ItemBody::User(message) => vec![(Part::Body, message.text.as_str())],
        ItemBody::Assistant { text } => vec![(Part::Body, rendered.unwrap_or(text))],
        ItemBody::Tool(call) => vec![(Part::Title, if call.title.is_empty() { &call.name } else { &call.title })],
        ItemBody::Notice { text, .. } => vec![(Part::Body, text.as_str())],
        ItemBody::Plan { steps } => steps.iter().enumerate().map(|(ix, s)| (Part::Step(ix), s.text.as_str())).collect(),
        ItemBody::Request(request) => match &request.kind {
            RequestKind::Question { question, .. } => vec![(Part::Body, question.as_str())],
            _ => Vec::new(),
        },
        ItemBody::Reasoning { .. } | ItemBody::TurnEnd(_) | ItemBody::Handoff(_) => Vec::new(),
    }
}

/// Every match of `needle` in one item.
pub fn search_item(needle: &Needle, item: &Item, rendered: Option<&str>) -> Vec<Hit> {
    searchable(item, rendered)
        .into_iter()
        .flat_map(|(part, text)| {
            needle.find_all(text).into_iter().map(move |range| Hit { item: item.id, part, range })
        })
        .collect()
}

/// The match to show first: the first one at or below the row the reader is
/// looking at, so a search never yanks them back to the top; else the last.
pub fn nearest(hits: &[Hit], row_of: impl Fn(ItemId) -> usize, top_row: usize) -> Option<usize> {
    if hits.is_empty() {
        return None;
    }
    Some(hits.iter().position(|h| row_of(h.item) >= top_row).unwrap_or(hits.len() - 1))
}

/// The next match, wrapping around.
pub fn step(current: Option<usize>, len: usize, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        None if forward => 0,
        None => len - 1,
        Some(ix) if forward => (ix + 1) % len,
        Some(ix) => (ix + len - 1) % len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found<'a>(query: &str, text: &'a str) -> Vec<&'a str> {
        Needle::new(query).unwrap().find_all(text).into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn lowercase_queries_ignore_case_and_capitals_match_exactly() {
        assert_eq!(found("error", "Error: an error, ERROR"), vec!["Error", "error", "ERROR"]);
        assert_eq!(found("Error", "Error: an error, ERROR"), vec!["Error"]);
        assert!(Needle::new("   ").is_none());
    }

    #[test]
    fn unicode_case_folding_keeps_original_byte_ranges() {
        // 'Ä' and 'ä' are two bytes; 'ß' stays as is; 'İ' lowercases to two chars.
        assert_eq!(found("äpfel", "ÄPFEL und äpfel"), vec!["ÄPFEL", "äpfel"]);
        assert_eq!(found("straße", "Die STRAßE"), vec!["STRAßE"]);
        let text = "İstanbul istanbul";
        for range in Needle::new("stanbul").unwrap().find_all(text) {
            assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end));
        }
    }

    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(found("aa", "aaaa"), vec!["aa", "aa"]);
    }

    #[test]
    fn replies_are_searched_as_rendered_and_tools_by_title() {
        let needle = Needle::new("bold").unwrap();
        let reply = Item {
            id: ItemId(1),
            turn: 1,
            status: ItemStatus::Done,
            body: ItemBody::Assistant { text: "a **bold** move".into() },
        };
        let hits = search_item(&needle, &reply, Some("a bold move"));
        assert_eq!(hits, vec![Hit { item: ItemId(1), part: Part::Body, range: 2..6 }]);
        let plan = Item {
            id: ItemId(2),
            turn: 1,
            status: ItemStatus::Done,
            body: ItemBody::Plan {
                steps: vec![
                    PlanStep { text: "Read".into(), state: StepState::Done },
                    PlanStep { text: "Be bold".into(), state: StepState::Active },
                ],
            },
        };
        assert_eq!(search_item(&needle, &plan, None)[0].part, Part::Step(1));
    }

    #[test]
    fn the_first_match_shown_is_where_the_reader_is() {
        let hit = |item| Hit { item: ItemId(item), part: Part::Body, range: 0..1 };
        let hits = vec![hit(1), hit(5), hit(9)];
        let row = |id: ItemId| id.0 as usize;
        assert_eq!(nearest(&hits, row, 0), Some(0));
        assert_eq!(nearest(&hits, row, 4), Some(1));
        assert_eq!(nearest(&hits, row, 10), Some(2), "below the last match: the last one");
        assert_eq!(step(Some(2), 3, true), Some(0));
        assert_eq!(step(Some(0), 3, false), Some(2));
        assert_eq!(step(None, 0, true), None);
    }
}
