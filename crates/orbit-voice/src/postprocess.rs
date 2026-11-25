//! Tidying a transcript: Whisper's non-speech annotations, filler words,
//! spacing, capitalisation and the vocabulary's "heard as" replacements.

/// What [`clean`] does beyond removing non-speech annotations, which it
/// always does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cleanup {
    /// Drop "um", "uh", "erm" and the like.
    pub remove_fillers: bool,
    /// Fix spacing around punctuation and capitalise sentences.
    pub tidy: bool,
    /// `(heard, written)`: whole-word, case-insensitive replacements.
    pub replacements: Vec<(String, String)>,
}

impl Default for Cleanup {
    fn default() -> Self {
        Self { remove_fillers: true, tidy: true, replacements: Vec::new() }
    }
}

/// Hesitations removed when [`Cleanup::remove_fillers`] is on. Only sounds,
/// never words that can carry meaning ("like", "so", "well").
const FILLERS: &[&str] = &["um", "umm", "ummm", "uh", "uhh", "uhm", "erm", "er", "err", "ah", "ahh", "hmm", "hm", "mm", "mmm"];

pub fn clean(text: &str, cleanup: &Cleanup) -> String {
    let mut text = strip_annotations(text);
    if !cleanup.replacements.is_empty() {
        text = replace_words(&text, &cleanup.replacements);
    }
    if cleanup.remove_fillers {
        text = remove_fillers(&text);
    }
    if cleanup.tidy {
        text = tidy(&text);
    }
    text.trim().to_owned()
}

/// Removes `[BLANK_AUDIO]`, `(music)`, `*coughs*` and `♪` that Whisper writes
/// for sounds. An unclosed bracket drops the rest: that is what Whisper meant.
fn strip_annotations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut closer: Option<char> = None;
    for c in text.chars() {
        match closer {
            Some(end) if c == end => closer = None,
            Some(_) => {}
            None => match c {
                '[' => closer = Some(']'),
                '(' if looks_like_annotation(text, &out) => closer = Some(')'),
                '*' => closer = Some('*'),
                '♪' | '♫' => {}
                _ => out.push(c),
            },
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parentheses hold annotations only when they open the text or follow
/// a sentence end; "call foo (the helper)" keeps its aside.
fn looks_like_annotation(_text: &str, before: &str) -> bool {
    let before = before.trim_end();
    before.is_empty() || before.ends_with(['.', '!', '?'])
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\'' || c == '-' || c == '_'
}

/// Splits into alternating word and non-word runs.
fn runs(text: &str) -> Vec<(bool, &str)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut kind = None;
    for (i, c) in text.char_indices() {
        let w = is_word_char(c);
        if kind.is_some_and(|k| k != w) {
            out.push((kind.unwrap_or(false), &text[start..i]));
            start = i;
        }
        kind = Some(w);
    }
    if let Some(k) = kind {
        out.push((k, &text[start..]));
    }
    out
}

fn replace_words(text: &str, replacements: &[(String, String)]) -> String {
    let mut out = text.to_owned();
    for (heard, written) in replacements {
        let heard_words: Vec<String> = heard.split_whitespace().map(str::to_lowercase).collect();
        if heard_words.is_empty() {
            continue;
        }
        let words: Vec<(usize, usize, String)> = {
            let mut v = Vec::new();
            let mut offset = 0;
            for (is_word, run) in runs(&out) {
                if is_word {
                    v.push((offset, offset + run.len(), run.to_lowercase()));
                }
                offset += run.len();
            }
            v
        };
        let mut result = String::new();
        let mut copied = 0;
        let mut i = 0;
        while i + heard_words.len() <= words.len() {
            if words[i..i + heard_words.len()].iter().map(|w| &w.2).eq(heard_words.iter()) {
                let (start, end) = (words[i].0, words[i + heard_words.len() - 1].1);
                result.push_str(&out[copied..start]);
                result.push_str(written);
                copied = end;
                i += heard_words.len();
            } else {
                i += 1;
            }
        }
        result.push_str(&out[copied..]);
        out = result;
    }
    out
}

fn remove_fillers(text: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    for token in text.split_whitespace() {
        let core = token.trim_matches(|c: char| !is_word_char(c));
        if !FILLERS.contains(&core.to_lowercase().as_str()) {
            kept.push(token.to_owned());
            continue;
        }
        // The filler goes with the comma that set it off ("I think, uh, we"
        // keeps one comma); a sentence end it carried moves to the word
        // before ("It works, um." ends at "works.").
        let end = token.rfind(['.', '!', '?']).map(|i| &token[i..i + 1]);
        if let (Some(end), Some(last)) = (end, kept.last_mut()) {
            let bare = last.trim_end_matches([',', ';', ':']).len();
            if !last[..bare].ends_with(['.', '!', '?']) {
                last.truncate(bare);
                last.push_str(end);
            }
        }
    }
    kept.join(" ")
}

fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut capitalize = true;
    let chars: Vec<char> = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        // No space before closing punctuation.
        if c == ' ' && chars.get(i + 1).is_some_and(|n| matches!(n, ',' | '.' | '!' | '?' | ';' | ':' | ')')) {
            continue;
        }
        // Collapse doubled punctuation from joined segments (",," or ". .").
        if matches!(c, ',' | '.') && out.ends_with(c) && c == ',' {
            continue;
        }
        if capitalize && c.is_alphabetic() {
            out.extend(c.to_uppercase());
            capitalize = false;
            continue;
        }
        if matches!(c, '.' | '!' | '?') {
            // "e.g." and "v1.2" are not sentence ends.
            let next = chars.get(i + 1);
            capitalize = next.is_none_or(|n| *n == ' ');
            if chars.get(i.wrapping_sub(2)) == Some(&'.') || chars.get(i.wrapping_sub(1)).is_some_and(|p| p.is_ascii_digit()) {
                capitalize = capitalize && next.is_none();
            }
        } else if c.is_alphanumeric() {
            capitalize = false;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(text: &str) -> String {
        clean(text, &Cleanup::default())
    }

    #[test]
    fn annotations_are_removed() {
        assert_eq!(c(" [BLANK_AUDIO]"), "");
        assert_eq!(c("(music) Hello there. ♪"), "Hello there.");
        assert_eq!(c("Okay. (laughs) Next one."), "Okay. Next one.");
        assert_eq!(c("Call the helper (the parser) first."), "Call the helper (the parser) first.");
        assert_eq!(c("*coughs* Right."), "Right.");
    }

    #[test]
    fn fillers_and_their_commas_go() {
        assert_eq!(c("Um, so I think, uh, we should ship it."), "So I think, we should ship it.");
        assert_eq!(c("Uh. Let's go."), "Let's go.");
        assert_eq!(c("It works, um."), "It works.");
        // Words that look like fillers inside words stay.
        assert_eq!(c("The umbrella and the uhaul."), "The umbrella and the uhaul.");
        let keep = Cleanup { remove_fillers: false, ..Cleanup::default() };
        assert_eq!(clean("Um, hello.", &keep), "Um, hello.");
    }

    #[test]
    fn tidy_fixes_spacing_and_capitals() {
        assert_eq!(c("  hello ,  world .  how are you ?"), "Hello, world. How are you?");
        assert_eq!(c("version 1.2 is out. ship it"), "Version 1.2 is out. Ship it");
        assert_eq!(c("use e.g. this"), "Use e.g. this");
        let raw = Cleanup { tidy: false, remove_fillers: false, replacements: vec![] };
        assert_eq!(clean(" hello ,  world", &raw), "hello , world");
    }

    #[test]
    fn replacements_are_whole_word_and_case_insensitive() {
        let cleanup = Cleanup {
            replacements: vec![("soccer do".into(), "sockudo".into()), ("gee pui".into(), "GPUI".into())],
            ..Cleanup::default()
        };
        assert_eq!(clean("Use Soccer Do with gee pui.", &cleanup), "Use sockudo with GPUI.");
        assert_eq!(clean("Soccer done.", &cleanup), "Soccer done.");
    }

    #[test]
    fn cleaning_is_idempotent() {
        for text in ["Um, so I think, uh, we should ship it.", "hello ,  world . (music)", "Okay. Next one."] {
            let once = c(text);
            assert_eq!(c(&once), once);
        }
    }
}
