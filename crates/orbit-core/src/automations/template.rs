//! Prompt templates: `{name}` variables filled in when a run starts.

/// The variables a prompt can use, with what they hold (for the editor's
/// chips and `orbit help`).
pub const VARIABLES: &[(&str, &str)] = &[
    ("date", "Today's date (2026-11-25)"),
    ("time", "The time the run started (09:00)"),
    ("branch", "The branch the event is about, or the project's current branch"),
    ("pr", "The pull request the event is about (#42 Title, with its link)"),
    ("issue", "The issue the event is about (#7 Title, with its link)"),
    ("event", "What started the run"),
    ("last_run_summary", "The previous run's last message"),
    ("project", "The project's name"),
    ("name", "The automation's name"),
];

/// Replaces `{variable}` with its value. Unknown names and lone braces stay
/// as they are, and `{{` / `}}` are literal braces, so prompts that contain
/// code survive.
pub fn render(template: &str, mut value: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
            continue;
        }
        if tail.starts_with('{')
            && let Some(end) = tail.find('}')
        {
            let name = &tail[1..end];
            let valid = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            if valid && let Some(v) = value(name) {
                out.push_str(&v);
                rest = &tail[end + 1..];
                continue;
            }
        }
        out.push_str(&tail[..1]);
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// Variables used in `template` that Orbit does not know.
pub fn unknown(template: &str) -> Vec<String> {
    let mut found = Vec::new();
    render(template, |name| {
        if !VARIABLES.iter().any(|(v, _)| *v == name) && !found.iter().any(|f| f == name) {
            found.push(name.to_owned());
        }
        Some(String::new())
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_are_filled_and_code_survives() {
        let value = |n: &str| match n {
            "date" => Some("2026-11-25".to_owned()),
            "pr" => Some("#42 Fix login".to_owned()),
            _ => None,
        };
        assert_eq!(render("Review {pr} on {date}.", value), "Review #42 Fix login on 2026-11-25.");
        assert_eq!(render("fn x() { {{a}} } {nope} {", value), "fn x() { {a} } {nope} {");
        assert_eq!(render("{date}{date}", value), "2026-11-252026-11-25");
        assert_eq!(unknown("Hi {name}, {colour} and {colour} {date}"), vec!["colour"]);
    }
}
