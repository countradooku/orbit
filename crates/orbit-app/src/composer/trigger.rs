//! What the caret is completing: `@` mentions, `$` skills, `/` commands.
//!
//! Pure text logic, run once per edit on the composer's text: a backwards
//! scan from the caret to the start of the current word, so its cost is the
//! length of one word, not of the message.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerKind {
    /// `@path`: a file or folder of the workspace, or another thread.
    Mention,
    /// `$name`: one of the agent's skills.
    Skill,
    /// `/name` at the very start of the message: a command.
    Command,
}

/// The word being completed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trigger {
    pub kind: TriggerKind,
    /// The whole word, sigil included: what a completion replaces.
    pub range: Range<usize>,
    /// What is typed after the sigil, up to the caret.
    pub query: String,
}

/// The trigger under the caret, if any. `caret` is a byte offset.
pub fn active_trigger(text: &str, caret: usize) -> Option<Trigger> {
    if caret > text.len() || !text.is_char_boundary(caret) {
        return None;
    }
    let start = text[..caret].rfind(char::is_whitespace).map_or(0, |i| i + text[i..].chars().next().map_or(1, char::len_utf8));
    let end = text[caret..].find(char::is_whitespace).map_or(text.len(), |i| caret + i);
    if caret == start {
        return None; // the caret sits before the word
    }
    let word = &text[start..end];
    let sigil = word.chars().next()?;
    let query = text[start + sigil.len_utf8()..caret].to_owned();
    let kind = match sigil {
        '@' => TriggerKind::Mention,
        // `$5` is money, not a skill.
        '$' if !query.starts_with(|c: char| c.is_ascii_digit()) => TriggerKind::Skill,
        '/' if text[..start].trim().is_empty() => TriggerKind::Command,
        _ => return None,
    };
    Some(Trigger { kind, range: start..end, query })
}

/// How a path is written in a message: `@src/main.rs`, or quoted when it
/// holds spaces: `@"docs/design notes.md"`.
pub fn mention_text(path: &str) -> String {
    if path.contains(char::is_whitespace) || path.contains('"') {
        format!("@\"{}\"", path.replace('"', "'"))
    } else {
        format!("@{path}")
    }
}

/// The `@` mentions written in `text`, for showing them as tokens again
/// (restored drafts, recalled prompts). Byte ranges, sigil included.
pub fn mention_ranges(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let at_word_start = i == 0 || text[..i].ends_with(char::is_whitespace);
        if bytes[i] == b'@' && at_word_start {
            if bytes.get(i + 1) == Some(&b'"') {
                if let Some(close) = text[i + 2..].find('"') {
                    let end = i + 2 + close + 1;
                    if close > 0 && !text[i + 2..end - 1].contains('\n') {
                        out.push(i..end);
                        i = end;
                        continue;
                    }
                }
            } else {
                let end = text[i..].find(char::is_whitespace).map_or(text.len(), |e| i + e);
                // Trailing punctuation ends a sentence, not a path.
                let trimmed = text[i..end].trim_end_matches([',', ';', ':', '!', '?', ')']).len() + i;
                let trimmed = if text[i..trimmed].ends_with('.') && !text[i + 1..trimmed - 1].contains('.') {
                    trimmed - 1
                } else {
                    trimmed
                };
                if trimmed > i + 1 {
                    out.push(i..trimmed);
                }
                i = end;
                continue;
            }
        }
        i += text[i..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

/// The path inside a mention's text: `@"a b"` → `a b`, `@src/` → `src/`.
pub fn mention_path(mention: &str) -> &str {
    let rest = mention.strip_prefix('@').unwrap_or(mention);
    rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')).unwrap_or(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text_with_caret: &str) -> Option<Trigger> {
        let caret = text_with_caret.find('|').unwrap();
        let text = text_with_caret.replace('|', "");
        active_trigger(&text, caret)
    }

    #[test]
    fn mentions_start_a_word_anywhere() {
        let t = at("look at @src/ma| please").unwrap();
        assert_eq!(t.kind, TriggerKind::Mention);
        assert_eq!(t.query, "src/ma");
        assert_eq!(t.range, 8..15);
        assert_eq!(at("@|").unwrap().query, "");
        assert!(at("mail me@home|").is_none(), "an email is not a mention");
        assert!(at("look |@src").is_none(), "caret before the sigil");
    }

    #[test]
    fn the_whole_word_is_replaced_even_mid_word() {
        let t = at("@ma|in.rs").unwrap();
        assert_eq!(t.query, "ma");
        assert_eq!(t.range, 0..8);
    }

    #[test]
    fn skills_are_dollar_words_but_not_prices() {
        let t = at("use $rev|").unwrap();
        assert_eq!((t.kind, t.query.as_str()), (TriggerKind::Skill, "rev"));
        assert!(at("costs $5|").is_none());
    }

    #[test]
    fn commands_only_open_the_message() {
        let t = at("  /comp|").unwrap();
        assert_eq!((t.kind, t.query.as_str()), (TriggerKind::Command, "comp"));
        assert!(at("see /usr/bi|").is_none());
        assert!(at("/compact |").is_none(), "after the command word");
    }

    #[test]
    fn non_ascii_text_is_safe() {
        assert_eq!(at("héllo @fïch|").unwrap().query, "fïch");
        assert!(active_trigger("héllo", 2).is_none(), "not a char boundary");
    }

    #[test]
    fn mentions_round_trip_through_text() {
        assert_eq!(mention_text("src/main.rs"), "@src/main.rs");
        assert_eq!(mention_text("docs/design notes.md"), "@\"docs/design notes.md\"");
        let text = "Compare @src/a.rs, @\"docs/b c.md\" and @src/. Mail x@y.z.";
        let found: Vec<&str> = mention_ranges(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(found, vec!["@src/a.rs", "@\"docs/b c.md\"", "@src/"]);
        assert_eq!(mention_path("@\"docs/b c.md\""), "docs/b c.md");
        assert_eq!(mention_path("@src/"), "src/");
    }
}
