//! Placing a transcript into text the user is editing: at the caret, or
//! over the selection, spaced and capitalised to fit where it lands.

use std::ops::Range;

/// The text after insertion and where the transcript sits in it (bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inserted {
    pub text: String,
    pub range: Range<usize>,
}

/// Inserts `transcript` into `text` over `selection` (a byte range; empty
/// for a caret).
///
/// - A space is added between the transcript and a word it would touch, and
///   none before closing punctuation.
/// - Mid-sentence, the transcript's leading capital is lowered ("Fix the" →
///   "fix the"), unless the word is an acronym or "I".
/// - At the start of a sentence, its first letter is raised.
pub fn insert(text: &str, selection: Range<usize>, transcript: &str) -> Inserted {
    let start = floor_char(text, selection.start.min(text.len()));
    let end = floor_char(text, selection.end.clamp(start, text.len()));
    let (before, after) = (&text[..start], &text[end..]);
    let mut body = transcript.trim().to_owned();
    if body.is_empty() {
        return Inserted { text: format!("{before}{after}"), range: start..start };
    }

    let prev = before.chars().next_back();
    let sentence_start = match before.trim_end().chars().next_back() {
        None => true,
        Some(c) => matches!(c, '.' | '!' | '?' | '\n') || before.ends_with('\n'),
    };
    body = if sentence_start { capitalize(&body) } else { decapitalize(&body) };

    let first = body.chars().next();
    let lead = prev.is_some_and(|p| !p.is_whitespace() && !matches!(p, '(' | '[' | '{' | '"' | '\'' | '`' | '/'))
        && first.is_some_and(|f| !matches!(f, ',' | '.' | '!' | '?' | ';' | ':' | ')'));
    let next = after.chars().next();
    let trail = next.is_some_and(|n| n.is_alphanumeric() || matches!(n, '(' | '"' | '`'));
    // A sentence-final period before text that continues the sentence reads
    // wrong ("fix this. and that"): drop it.
    let continues = after.trim_start().chars().next().is_some_and(|n| n.is_alphanumeric() && n.is_lowercase());
    if continues && body.ends_with('.') && !body.ends_with("..") {
        body.pop();
    }

    let mut out = String::with_capacity(text.len() + body.len() + 2);
    out.push_str(before);
    if lead {
        out.push(' ');
    }
    let at = out.len();
    out.push_str(&body);
    let range = at..out.len();
    if trail {
        out.push(' ');
    }
    out.push_str(after);
    Inserted { text: out, range }
}

fn floor_char(text: &str, mut i: usize) -> usize {
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn decapitalize(s: &str) -> String {
    let word = s.split(|c: char| !c.is_alphanumeric() && c != '\'').next().unwrap_or("");
    let keep = word == "I"
        || word.starts_with("I'")
        || word.chars().filter(|c| c.is_uppercase()).count() > 1
        || word.chars().skip(1).any(|c| c.is_uppercase());
    if keep {
        return s.to_owned();
    }
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, caret: usize, t: &str) -> String {
        insert(text, caret..caret, t).text
    }

    #[test]
    fn into_empty_text() {
        let r = insert("", 0..0, " fix the parser.");
        assert_eq!(r.text, "Fix the parser.");
        assert_eq!(r.range, 0..15);
    }

    #[test]
    fn at_the_caret_with_spacing() {
        assert_eq!(at("Please", 6, "Fix the parser."), "Please fix the parser.");
        assert_eq!(at("Please ", 7, "Fix it."), "Please fix it.");
        assert_eq!(at("Done. ", 6, "next step"), "Done. Next step");
        assert_eq!(at("Call  now", 5, "Bob"), "Call bob now");
        assert_eq!(at("say(", 4, "Hello"), "say(hello");
        assert_eq!(at("Hello", 5, ", world."), "Hello, world.");
    }

    #[test]
    fn mid_sentence_keeps_acronyms_and_i() {
        assert_eq!(at("Then ", 5, "GPUI renders."), "Then GPUI renders.");
        assert_eq!(at("Then ", 5, "I'll check."), "Then I'll check.");
        assert_eq!(at("Use ", 4, "SpeechState here."), "Use SpeechState here.");
    }

    #[test]
    fn before_lowercase_text_the_period_goes() {
        assert_eq!(at("Please  and ship", 7, "Fix it."), "Please fix it and ship");
        assert_eq!(at("x  Then", 2, "y."), "x y. Then");
    }

    #[test]
    fn over_a_selection() {
        let r = insert("Please fix the bug now", 7..18, "Update the docs");
        assert_eq!(r.text, "Please update the docs now");
        assert_eq!(&r.text[r.range.clone()], "update the docs");
    }

    #[test]
    fn after_a_newline_starts_a_sentence() {
        assert_eq!(at("Line one\n", 9, "line two"), "Line one\nLine two");
    }

    #[test]
    fn empty_transcript_only_removes_the_selection() {
        let r = insert("abc", 1..2, "  ");
        assert_eq!(r.text, "ac");
        assert_eq!(r.range, 1..1);
    }

    #[test]
    fn non_ascii_offsets_are_safe() {
        let text = "Grüße";
        // A caret inside the two-byte "ü" snaps back to its start.
        let r = insert(text, 3..3, "x");
        assert!(r.text.is_char_boundary(r.range.start));
    }
}
