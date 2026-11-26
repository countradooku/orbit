//! The vocabulary prompt: words Whisper should expect.
//!
//! Whisper conditions on a text prompt as if it were the transcript before
//! the audio, so naming the project's terms ("GPUI, crossfire, keymap.rs")
//! makes it spell them that way instead of "GPU I, cross fire, keymap dot
//! RS". The prompt holds the user's own vocabulary first, then the project's
//! names, then distinctive file names, within Whisper's prompt budget.

use std::collections::{HashMap, HashSet};

/// About 200 tokens: Whisper keeps the last 223 tokens of a prompt, and the
/// transcript before the current window shares that budget.
pub const MAX_PROMPT_CHARS: usize = 600;

/// Most file names a prompt takes.
const MAX_FILE_TERMS: usize = 40;

/// The user's vocabulary as written in Settings: one entry per line or
/// comma. `heard => written` (or `heard -> written`) entries also teach a
/// replacement for what Whisper hears.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Vocabulary {
    pub terms: Vec<String>,
    /// `(heard, written)`.
    pub replacements: Vec<(String, String)>,
}

impl Vocabulary {
    pub fn parse(text: &str) -> Self {
        let mut out = Self::default();
        for entry in text.split(['\n', ',']).map(str::trim).filter(|e| !e.is_empty()) {
            let pair = entry.split_once("=>").or_else(|| entry.split_once("->"));
            match pair {
                Some((heard, written)) => {
                    let (heard, written) = (heard.trim(), written.trim());
                    if !heard.is_empty() && !written.is_empty() {
                        out.replacements.push((heard.to_owned(), written.to_owned()));
                        out.terms.push(written.to_owned());
                    }
                }
                None => out.terms.push(entry.to_owned()),
            }
        }
        out
    }
}

/// Words too common to be worth prompting.
const COMMON: &[&str] = &[
    "index", "main", "mod", "lib", "test", "tests", "readme", "license", "changelog", "src", "build", "config", "utils",
    "util", "types", "app", "style", "styles", "package", "cargo", "makefile", "dockerfile", "license-mit", "gitignore",
];

/// Distinctive terms from file paths: names spelled in ways speech would not
/// guess (camelCase, snake_case, dots, digits), most frequent first.
pub fn file_terms<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for path in paths {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let stem = name.split('.').next().unwrap_or(name);
        if name.starts_with('.') || stem.len() < 3 {
            continue;
        }
        if COMMON.contains(&stem.to_lowercase().as_str()) {
            continue;
        }
        let distinctive = name.contains(['_', '-', '.'])
            || stem.chars().skip(1).any(|c| c.is_uppercase())
            || stem.chars().any(|c| c.is_ascii_digit());
        if distinctive {
            *counts.entry(name.to_owned()).or_default() += 1;
        }
    }
    let mut terms: Vec<(String, usize)> = counts.into_iter().collect();
    terms.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.len().cmp(&b.0.len())).then(a.0.cmp(&b.0)));
    terms.into_iter().take(MAX_FILE_TERMS).map(|t| t.0).collect()
}

/// The prompt for `vocabulary`, then `project` names, then `files` terms,
/// deduplicated and cut to [`MAX_PROMPT_CHARS`] at a whole term.
pub fn build(vocabulary: &Vocabulary, project: &[String], files: &[String]) -> String {
    let mut seen = HashSet::new();
    let mut out = String::new();
    for term in vocabulary.terms.iter().chain(project).chain(files) {
        let term = term.trim();
        if term.is_empty() || !seen.insert(term.to_lowercase()) {
            continue;
        }
        let extra = if out.is_empty() { term.len() } else { term.len() + 2 };
        if out.len() + extra + 1 > MAX_PROMPT_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(term);
    }
    if !out.is_empty() {
        out.push('.');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_parses_terms_and_replacements() {
        let v = Vocabulary::parse("GPUI, crossfire\n soccer do => sockudo\nnix -> Nix\n\n , ");
        assert_eq!(v.terms, ["GPUI", "crossfire", "sockudo", "Nix"]);
        assert_eq!(v.replacements, [("soccer do".to_owned(), "sockudo".to_owned()), ("nix".to_owned(), "Nix".to_owned())]);
    }

    #[test]
    fn file_terms_pick_distinctive_names() {
        let paths = [
            "crates/orbit-app/src/keymap.rs",
            "crates/orbit-app/src/main.rs",
            "crates/orbit-app/src/composer/mod.rs",
            "crates/orbit-voice/src/model_picker.rs",
            "web/src/useTextFieldSpeech.tsx",
            "README.md",
            ".gitignore",
            "docs/research/voice.md",
            "a.rs",
        ];
        let terms = file_terms(paths);
        assert!(terms.contains(&"keymap.rs".to_owned()));
        assert!(terms.contains(&"model_picker.rs".to_owned()));
        assert!(terms.contains(&"useTextFieldSpeech.tsx".to_owned()));
        for skipped in ["main.rs", "mod.rs", "README.md", ".gitignore", "a.rs"] {
            assert!(!terms.contains(&skipped.to_owned()), "{skipped}");
        }
    }

    #[test]
    fn prompt_orders_dedupes_and_fits_the_budget() {
        let v = Vocabulary::parse("GPUI, Orbit");
        let project = vec!["orbit".to_owned(), "orbit-app".to_owned()];
        let files: Vec<String> = (0..200).map(|i| format!("file_number_{i}.rs")).collect();
        let prompt = build(&v, &project, &files);
        assert!(prompt.starts_with("GPUI, Orbit, orbit-app, file_number_0.rs"), "{prompt}");
        assert!(prompt.len() <= MAX_PROMPT_CHARS);
        assert!(prompt.ends_with(".rs."));
        assert_eq!(build(&Vocabulary::default(), &[], &[]), "");
    }
}
