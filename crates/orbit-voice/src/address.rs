//! Voice steering: finding which agent a spoken instruction is for.
//!
//! "API, run the tests then open a PR", "at api run the tests", "tell the
//! docs agent to fix the typo" and a typed "@api run the tests" all address
//! the thread named `api`; the rest of the sentence is the message. A
//! sentence that names no agent addresses none: the caller then treats it
//! as ordinary dictation.

/// A thread that can be addressed, by the names it answers to (its title,
/// its branch or workspace name).
#[derive(Clone, Debug)]
pub struct Candidate<K> {
    pub key: K,
    pub names: Vec<String>,
}

/// An instruction and who it is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addressed<K> {
    pub key: K,
    /// The name as matched.
    pub name: String,
    pub message: String,
}

/// Words that introduce an address and are dropped with it.
const LEADS: &[&[&str]] = &[&["hey"], &["ok"], &["okay"], &["at"], &["tell"], &["ask"], &["to"]];
/// Words between a name and the message that are dropped: "tell api to run".
const JOINERS: &[&str] = &["to", "please", "agent", "thread"];

fn words(text: &str) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        let word = c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/');
        match (word, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push((normalize(&text[s..i]), s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((normalize(&text[s..]), s, text.len()));
    }
    out
}

fn normalize(word: &str) -> String {
    word.trim_end_matches(['.', '/']).to_lowercase()
}

/// A name as the words someone would say: `orbit-app` is "orbit app",
/// `fix_login` is "fix login".
fn spoken(name: &str) -> Vec<String> {
    name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// Finds the candidate `text` is addressed to and the message for it.
pub fn resolve<K: Clone>(text: &str, candidates: &[Candidate<K>]) -> Option<Addressed<K>> {
    let trimmed = text.trim_start();
    let offset = text.len() - trimmed.len();
    let typed = trimmed.starts_with('@');
    let body = trimmed.trim_start_matches('@');
    let base = offset + (trimmed.len() - body.len());
    let ws = words(body);
    // Skip a lead ("hey", "tell"), and an article.
    let mut first = 0;
    if !typed && ws.first().is_some_and(|w| LEADS.iter().any(|l| l[0] == w.0)) {
        first = 1;
    }
    if ws.get(first).is_some_and(|w| w.0 == "the") {
        first += 1;
    }
    // The longest name (in words) that the sentence starts with wins. A name
    // may arrive as one word or several: "orbit-app", "orbit app".
    let mut best: Option<(usize, usize, &Candidate<K>, &str)> = None;
    for candidate in candidates {
        for name in &candidate.names {
            let name_words = spoken(name);
            if name_words.is_empty() {
                continue;
            }
            for take in 1..=name_words.len().min(ws.len().saturating_sub(first)) {
                let said = ws[first..first + take].iter().map(|w| w.0.as_str()).collect::<Vec<_>>().join(" ");
                if spoken(&said) == name_words && best.is_none_or(|b| name_words.len() > b.0) {
                    best = Some((name_words.len(), take, candidate, name));
                }
            }
        }
    }
    let (_, take, candidate, name) = best?;
    let mut rest = first + take;
    while ws.get(rest).is_some_and(|w| JOINERS.contains(&w.0.as_str())) {
        rest += 1;
    }
    let start = ws.get(rest).map(|w| base + w.1)?;
    let message = text[start..].trim();
    if message.is_empty() {
        return None;
    }
    let mut message = message.to_owned();
    if let Some(c) = message.chars().next() {
        message.replace_range(..c.len_utf8(), &c.to_uppercase().to_string());
    }
    Some(Addressed { key: candidate.key.clone(), name: name.to_owned(), message })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn threads() -> Vec<Candidate<u32>> {
        vec![
            Candidate { key: 1, names: vec!["api".into(), "Add rate limiting".into()] },
            Candidate { key: 2, names: vec!["docs".into()] },
            Candidate { key: 3, names: vec!["orbit-app".into()] },
            Candidate { key: 4, names: vec!["docs site".into()] },
        ]
    }

    fn say(text: &str) -> Option<(u32, String)> {
        resolve(text, &threads()).map(|a| (a.key, a.message))
    }

    #[test]
    fn spoken_and_typed_addresses() {
        assert_eq!(say("API, run the tests then open a PR."), Some((1, "Run the tests then open a PR.".into())));
        assert_eq!(say("@api run the tests"), Some((1, "Run the tests".into())));
        assert_eq!(say("At api: run the tests"), Some((1, "Run the tests".into())));
        assert_eq!(say("Hey docs, fix the typo."), Some((2, "Fix the typo.".into())));
        assert_eq!(say("Tell the docs agent to fix the typo"), Some((2, "Fix the typo".into())));
        assert_eq!(say("Ask orbit app to rebuild"), Some((3, "Rebuild".into())));
        assert_eq!(say("orbit-app: rebuild"), Some((3, "Rebuild".into())));
    }

    #[test]
    fn the_longest_name_wins() {
        assert_eq!(say("Docs site, publish it"), Some((4, "Publish it".into())));
        assert_eq!(say("Add rate limiting, also cover the login route"), Some((1, "Also cover the login route".into())));
    }

    #[test]
    fn no_address_is_plain_dictation() {
        assert_eq!(say("Run the tests then open a PR"), None);
        assert_eq!(say("Refactor the api client"), None);
        // A name alone is no instruction.
        assert_eq!(say("API."), None);
        assert_eq!(say(""), None);
    }
}
