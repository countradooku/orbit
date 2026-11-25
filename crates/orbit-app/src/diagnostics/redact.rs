//! Removes secrets from text that may leave the machine (the diagnostics
//! bundle, a copied crash report).
//!
//! Three layers, each tested:
//!
//! 1. **Known secrets.** Exact values the app knows are secret (agent
//!    environment values, the host token, tracker tokens) are replaced
//!    wherever they appear.
//! 2. **Recognizable tokens.** Well-known token shapes (`sk-ant-…`,
//!    `ghp_…`, `github_pat_…`, `glpat-…`, `xoxb-…`, `AKIA…`, JWTs, …) and
//!    `Bearer`/`Basic` credentials.
//! 3. **Secret-looking assignments.** The value after `NAME=`, `name: ` or
//!    `"name": ` when the name says token, key, secret, password, auth,
//!    cookie, session or credential, and the value of any `UPPER_CASE=`
//!    environment assignment; also `user:password@` in URLs.
//!
//! Finally the home folder is shortened to `~`, which hides the user name.

const REDACTED: &str = "[redacted]";

/// Prefixes of well-known token formats.
const TOKEN_PREFIXES: &[&str] = &[
    "sk-ant-", "sk-proj-", "sk-", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "glpat-", "gldt-", "xoxb-", "xoxp-",
    "xoxa-", "xoxe-", "xapp-", "AKIA", "ASIA", "AIza", "lin_api_", "npm_", "pypi-", "hf_", "dop_v1_", "sq0atp-", "shpat_",
    "rk_live_", "sk_live_", "pk_live_", "eyJ",
];

/// Words that make a name's value a secret.
const SECRET_WORDS: &[&str] = &[
    "token", "secret", "password", "passwd", "pwd", "apikey", "api_key", "api-key", "auth", "credential", "cookie", "session",
    "private_key", "access_key", "client_secret", "signature",
];

/// Characters of the tokens above (URL-safe base64 and separators). A slash
/// ends a word, so a token in a URL path is still found.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

/// Replaces every occurrence of each known secret (values shorter than six
/// characters are too common to replace safely).
fn redact_known(text: &str, secrets: &[String]) -> String {
    let mut secrets: Vec<&str> = secrets.iter().map(|s| s.trim()).filter(|s| s.chars().count() >= 6).collect();
    // Longest first, so a secret containing another is replaced whole.
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = text.to_owned();
    for secret in secrets {
        if out.contains(secret) {
            out = out.replace(secret, REDACTED);
        }
    }
    out
}

/// Replaces token-shaped words.
fn redact_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((start, _)) = rest.char_indices().find(|(_, c)| is_token_char(*c)) {
        out.push_str(&rest[..start]);
        let word_end = rest[start..].find(|ch: char| !is_token_char(ch)).map_or(rest.len(), |e| start + e);
        let word = &rest[start..word_end];
        // A token starts a word: not `task-` inside `my-task-123`.
        let prefix = TOKEN_PREFIXES.iter().find(|p| word.starts_with(**p));
        let looks_like_token = match prefix {
            Some(&"eyJ") => word.matches('.').count() >= 2 && word.len() >= 30,
            Some(p) => word.len() >= p.len() + 12 && word[p.len()..].chars().any(|c| c.is_ascii_digit()),
            None => false,
        };
        if looks_like_token {
            out.push_str(REDACTED);
        } else {
            out.push_str(word);
        }
        rest = &rest[word_end..];
    }
    out.push_str(rest);
    out
}

fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|w| lower.contains(w))
}

fn is_env_name(name: &str) -> bool {
    name.len() >= 2
        && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name.chars().any(|c| c.is_ascii_uppercase())
}

/// Redacts the value of `name=value`, `name: value` and `"name": "value"`
/// assignments whose name is secret-looking or an environment variable,
/// and `Bearer`/`Basic` credentials, one line at a time.
fn redact_assignments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&redact_line(line));
    }
    out
}

fn redact_line(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut copied = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // Credentials after an authorization scheme.
        let scheme = |len: usize, word: &str| line.get(i..i + len).is_some_and(|s| s.eq_ignore_ascii_case(word));
        if (c == b'B' || c == b'b')
            && (scheme(7, "bearer ") || scheme(6, "basic "))
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        {
            let skip = if scheme(7, "bearer ") { 7 } else { 6 };
            let value_start = i + skip;
            let value_end = line[value_start..].find(|ch: char| ch.is_whitespace() || ch == '"' || ch == '\'' || ch == ',').map_or(line.len(), |e| value_start + e);
            if value_end > value_start {
                out.push_str(&line[copied..value_start]);
                out.push_str(REDACTED);
                copied = value_end;
                i = value_end;
                continue;
            }
        }
        if c == b'=' || c == b':' {
            // The name before the separator (allowing one closing quote).
            let mut name_end = i;
            if name_end > 0 && (bytes[name_end - 1] == b'"' || bytes[name_end - 1] == b'\'') {
                name_end -= 1;
            }
            let name_start = line[..name_end]
                .rfind(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.'))
                .map_or(0, |p| p + 1);
            let name = &line[name_start..name_end];
            // `https://` and `C:\` are not assignments.
            let is_url = c == b':' && line[i + 1..].starts_with("//");
            let secret = !name.is_empty() && !is_url && (is_secret_name(name) || (c == b'=' && is_env_name(name)));
            if secret {
                let mut value_start = i + 1;
                while value_start < bytes.len() && bytes[value_start] == b' ' {
                    value_start += 1;
                }
                let quote = bytes.get(value_start).copied().filter(|q| *q == b'"' || *q == b'\'');
                let word_end = |from: usize| {
                    line[from..]
                        .find(|ch: char| ch.is_whitespace() || ch == '&' || ch == ',' || ch == ';' || ch == '}' || ch == ')')
                        .map_or(line.len(), |e| from + e)
                };
                let value_end = match quote {
                    Some(q) => line[value_start + 1..].find(q as char).map_or(line.len(), |e| value_start + 1 + e + 1),
                    None => {
                        let end = word_end(value_start);
                        // `Authorization: Bearer <credential>`: the scheme
                        // and the credential go together.
                        let word = &line[value_start..end];
                        let is_scheme = ["bearer", "basic", "token"].iter().any(|s| word.eq_ignore_ascii_case(s));
                        if is_scheme && end < line.len() { word_end(end + 1) } else { end }
                    }
                };
                let value = &line[value_start..value_end];
                let already = value.trim_matches(|q| q == '"' || q == '\'') == REDACTED;
                if value_end > value_start && !already && !value.trim_matches(|q| q == '"' || q == '\'').is_empty() {
                    out.push_str(&line[copied..value_start]);
                    match quote {
                        Some(q) => {
                            out.push(q as char);
                            out.push_str(REDACTED);
                            out.push(q as char);
                        }
                        None => out.push_str(REDACTED),
                    }
                    copied = value_end;
                    i = value_end;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&line[copied..]);
    out
}

/// `scheme://user:password@host` → `scheme://[redacted]@host`.
fn redact_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("://") {
        let after = pos + 3;
        out.push_str(&rest[..after]);
        let tail = &rest[after..];
        let authority_end = tail.find(|c: char| c == '/' || c.is_whitespace() || c == '"' || c == '\'').unwrap_or(tail.len());
        let authority = &tail[..authority_end];
        match authority.rfind('@') {
            Some(at) if authority[..at].contains(':') => {
                out.push_str(REDACTED);
                out.push_str(&authority[at..]);
            }
            _ => out.push_str(authority),
        }
        rest = &tail[authority_end..];
    }
    out.push_str(rest);
    out
}

/// Shortens the home folder to `~` (in both slash styles on Windows).
fn redact_home(text: &str, home: Option<&str>) -> String {
    let Some(home) = home.map(|h| h.trim_end_matches(['/', '\\'])).filter(|h| h.len() > 3) else { return text.to_owned() };
    let mut out = text.replace(home, "~");
    let forward = home.replace('\\', "/");
    if forward != home {
        out = out.replace(&forward, "~");
    }
    let doubled = home.replace('\\', "\\\\");
    if doubled != home {
        out = out.replace(&doubled, "~");
    }
    out
}

/// Everything above, in order.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let home = dirs_home();
    redact_with_home(text, secrets, home.as_deref())
}

pub fn redact_with_home(text: &str, secrets: &[String], home: Option<&str>) -> String {
    let text = redact_known(text, secrets);
    let text = redact_url_credentials(&text);
    let text = redact_assignments(&text);
    let text = redact_tokens(&text);
    redact_home(&text, home)
}

fn dirs_home() -> Option<String> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(|h| h.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(text: &str) -> String {
        redact_with_home(text, &[], None)
    }

    #[test]
    fn known_secrets_are_removed_wherever_they_appear() {
        let secrets = vec!["hunter2-very-secret".to_owned(), "abc".to_owned()];
        let out = redact_with_home("proxy said hunter2-very-secret twice: hunter2-very-secret; abc stays", &secrets, None);
        assert_eq!(out, "proxy said [redacted] twice: [redacted]; abc stays");
    }

    #[test]
    fn well_known_tokens_are_removed() {
        for token in [
            "sk-ant-api03-AbCdEf0123456789xyz",
            "ghp_0123456789abcdefABCDEF0123456789abcd",
            "github_pat_11ABCDEFG0123456789_abcdefghijk",
            "glpat-0123456789abcdefghij",
            "xoxb-123456789012-1234567890123-AbCdEfGh",
            "AKIAIOSFODNN7EXAMPLE1",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        ] {
            let out = r(&format!("using {token} now"));
            assert_eq!(out, "using [redacted] now", "{token}");
        }
        // Ordinary words with the same beginnings survive.
        assert_eq!(r("sk-learn and ghp_ and eyJ alone and task-runner"), "sk-learn and ghp_ and eyJ alone and task-runner");
    }

    #[test]
    fn secret_assignments_lose_their_values() {
        assert_eq!(r("ANTHROPIC_API_KEY=sk-whatever-123"), "ANTHROPIC_API_KEY=[redacted]");
        assert_eq!(r("api_key: 12345 next"), "api_key: [redacted] next");
        assert_eq!(r(r#"{"token": "abc def", "name": "orbit"}"#), r#"{"token": "[redacted]", "name": "orbit"}"#);
        assert_eq!(r("https://h/x?access_token=zzz&page=2"), "https://h/x?access_token=[redacted]&page=2");
        assert_eq!(r("Authorization: Bearer abc.def.ghi"), "Authorization: [redacted]");
        assert_eq!(r("header bearer 1234567"), "header bearer [redacted]");
        assert_eq!(r("password='p@ss w0rd'"), "password='[redacted]'");
        // Environment assignments, whatever their name.
        assert_eq!(r("env HTTPS_PROXY=http://user:pw@proxy:8080 started"), "env HTTPS_PROXY=[redacted] started");
        assert_eq!(r("CLAUDE_CONFIG_DIR=/home/me/.claude"), "CLAUDE_CONFIG_DIR=[redacted]");
        // Not secrets.
        assert_eq!(r("exit code=1 status: ok"), "exit code=1 status: ok");
        assert_eq!(r("see https://example.com/a:b"), "see https://example.com/a:b");
        assert_eq!(r("C:\\Users\\x"), "C:\\Users\\x");
    }

    #[test]
    fn url_credentials_are_removed() {
        assert_eq!(r("clone https://me:s3cret@github.com/a/b.git"), "clone https://[redacted]@github.com/a/b.git");
        assert_eq!(r("ssh://git@github.com/a/b"), "ssh://git@github.com/a/b");
    }

    #[test]
    fn the_home_folder_becomes_a_tilde() {
        let home = if cfg!(windows) { "C:\\Users\\Jane Doe" } else { "/home/jane" };
        let text = format!("{home}{0}code{0}app and {1}/x", std::path::MAIN_SEPARATOR, home.replace('\\', "/"));
        let out = redact_with_home(&text, &[], Some(home));
        assert!(!out.contains("Jane") && !out.contains("jane"), "{out}");
        assert!(out.starts_with('~'));
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = r("TOKEN=abc ghp_0123456789abcdefABCDEF0123456789abcd https://a:b@c");
        assert_eq!(r(&once), once);
    }
}
