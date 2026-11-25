//! The guard: actions that always need a person at the Orbit app.
//!
//! Some actions are too consequential to approve from a phone or to let an
//! access level wave through: force pushes, recursive deletes, `sudo`,
//! deploys and publishes, and anything that reads or writes secrets. The
//! guard is a list of patterns the engine checks every agent request
//! against. A matching request is always asked, even under Full access, is
//! answered only in the app (never from a chat bot or other remote origin),
//! and "allow for this session" approves it once.
//!
//! Patterns are plain text with `*` for any run of characters, matched
//! without regard to case anywhere in a command, path or tool input, but
//! only from the start of a word: `rm -rf` matches `cd x && rm -rf y` and
//! not `perform -rf`. Whitespace runs count as one space.

use bitcode::{Decode, Encode};

use crate::model::RequestKind;

/// One guarded pattern and the reason shown when it matches.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct GuardRule {
    pub pattern: String,
    /// Short interface copy: "Force push", "Reads secrets".
    pub label: String,
}

impl GuardRule {
    pub fn new(pattern: impl Into<String>, label: impl Into<String>) -> Self {
        Self { pattern: pattern.into(), label: label.into() }
    }
}

/// The guard list. Kept by the engine in its own file (`guard.bin`) and
/// handed to every agent session it starts.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct GuardSettings {
    pub enabled: bool,
    pub rules: Vec<GuardRule>,
}

impl Default for GuardSettings {
    fn default() -> Self {
        Self { enabled: true, rules: default_rules() }
    }
}

/// A request the guard holds for a person, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardHit {
    pub pattern: String,
    pub label: String,
}

/// The rules a new install starts with.
pub fn default_rules() -> Vec<GuardRule> {
    const RULES: &[(&str, &str)] = &[
        ("git push*--force", "Force push"),
        ("git push* -f ", "Force push"),
        ("git push* +", "Force push"),
        ("git reset --hard", "Discards work"),
        ("git clean -*f", "Deletes untracked files"),
        ("rm -rf", "Deletes recursively"),
        ("rm -fr", "Deletes recursively"),
        ("rm -r -f", "Deletes recursively"),
        ("Remove-Item*-Recurse", "Deletes recursively"),
        ("rmdir /s", "Deletes recursively"),
        ("sudo ", "Runs as administrator"),
        ("npm publish", "Publishes a package"),
        ("pnpm publish", "Publishes a package"),
        ("yarn publish", "Publishes a package"),
        ("yarn npm publish", "Publishes a package"),
        ("cargo publish", "Publishes a package"),
        ("twine upload", "Publishes a package"),
        ("gem push", "Publishes a package"),
        ("docker push", "Publishes an image"),
        ("gh release create", "Publishes a release"),
        ("kubectl apply", "Deploys"),
        ("kubectl delete", "Deploys"),
        ("helm install", "Deploys"),
        ("helm upgrade", "Deploys"),
        ("terraform apply", "Deploys"),
        ("terraform destroy", "Deploys"),
        ("vercel*--prod", "Deploys"),
        ("netlify deploy", "Deploys"),
        ("fly deploy", "Deploys"),
        ("firebase deploy", "Deploys"),
        ("wrangler deploy", "Deploys"),
        ("wrangler publish", "Deploys"),
        ("serverless deploy", "Deploys"),
        ("run deploy", "Deploys"),
        (".env", "Touches secrets"),
        ("id_rsa", "Touches secrets"),
        ("id_ed25519", "Touches secrets"),
        (".ssh/", "Touches secrets"),
        (".aws/credentials", "Touches secrets"),
        (".git-credentials", "Touches secrets"),
        (".netrc", "Touches secrets"),
        (".npmrc", "Touches secrets"),
        ("*.pem", "Touches secrets"),
        ("*.p12", "Touches secrets"),
        ("secrets.", "Touches secrets"),
        ("credentials.json", "Touches secrets"),
    ];
    RULES.iter().map(|(p, l)| GuardRule::new(*p, *l)).collect()
}

impl GuardSettings {
    /// Trimmed patterns, no empty or duplicate ones; a missing label gets
    /// the pattern itself. Idempotent.
    pub fn sanitized(mut self) -> Self {
        let mut seen = Vec::<String>::new();
        self.rules.retain_mut(|rule| {
            // A trailing space is meaningful ("sudo " is not "sudoku"); keep one.
            let ends_in_space = rule.pattern.ends_with(char::is_whitespace) && !rule.pattern.trim().is_empty();
            rule.pattern = rule.pattern.trim().to_owned();
            if ends_in_space {
                rule.pattern.push(' ');
            }
            rule.label = rule.label.trim().to_owned();
            let key = rule.pattern.trim_end().to_lowercase();
            if rule.pattern.is_empty() || seen.contains(&key) {
                return false;
            }
            seen.push(key);
            true
        });
        for rule in &mut self.rules {
            if rule.label.is_empty() {
                rule.label = rule.pattern.trim_end().to_owned();
            }
        }
        self
    }

    /// Whether any rule could match: enabled and not empty.
    pub fn active(&self) -> bool {
        self.enabled && !self.rules.is_empty()
    }

    /// The first rule matching `text` (a command, path or tool input).
    pub fn check_text(&self, text: &str) -> Option<GuardHit> {
        if !self.active() || text.is_empty() {
            return None;
        }
        let subject = normalize(text, true);
        self.rules
            .iter()
            .find(|r| matches(&normalize(&r.pattern, false), &subject))
            .map(|r| GuardHit { pattern: r.pattern.clone(), label: r.label.clone() })
    }

    /// The first rule a request matches. Questions and plans are never
    /// guarded: answering them does nothing by itself.
    pub fn check(&self, kind: &RequestKind) -> Option<GuardHit> {
        match kind {
            RequestKind::Command { command, .. } => self.check_text(command),
            RequestKind::FileChange { path, .. } => self.check_text(path),
            RequestKind::Tool { name, input } => self.check_text(name).or_else(|| self.check_text(input)),
            RequestKind::Question { .. } | RequestKind::Plan { .. } => None,
        }
    }
}

/// Lowercase, whitespace runs as one space, backslashes as slashes (so
/// `.ssh\id_rsa` and `.ssh/id_rsa` read alike). A subject always gets a
/// trailing space, so a pattern ending in a space (`sudo `, `push* -f `)
/// also matches at the very end; a pattern keeps one only if it had one.
fn normalize(text: &str, subject: bool) -> String {
    let mut out = String::with_capacity(text.len() + 1);
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.extend(if c == '\\' { '/' } else { c }.to_lowercase());
    }
    if subject || (space && !out.is_empty()) {
        out.push(' ');
    }
    out
}

/// Whether `pattern` (with `*` wildcards) occurs in `subject` starting at a
/// word boundary. Both are normalized.
fn matches(pattern: &str, subject: &str) -> bool {
    if pattern.trim().is_empty() {
        return false;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let first = parts[0];
    let anchored_at = |start: usize| -> bool {
        start == 0 || !subject[..start].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_')
    };
    let rest = &parts[1..];
    if first.is_empty() {
        return glob_from(rest, subject);
    }
    let mut from = 0;
    while let Some(pos) = subject[from..].find(first) {
        let start = from + pos;
        if anchored_at(start) && glob_from(rest, &subject[start + first.len()..]) {
            return true;
        }
        from = start + subject[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Whether the `*`-separated `parts` (each preceded by a wildcard) occur in
/// order in `subject`.
fn glob_from(parts: &[&str], mut subject: &str) -> bool {
    for part in parts {
        if part.is_empty() {
            continue;
        }
        match subject.find(part) {
            Some(pos) => subject = &subject[pos + part.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(text: &str) -> Option<String> {
        GuardSettings::default().check_text(text).map(|h| h.label)
    }

    #[test]
    fn the_default_rules_catch_what_they_promise() {
        assert_eq!(hit("git push --force origin main").as_deref(), Some("Force push"));
        assert_eq!(hit("git push origin main -f").as_deref(), Some("Force push"));
        assert_eq!(hit("git  push   origin +main").as_deref(), Some("Force push"));
        assert_eq!(hit("git push --force-with-lease").as_deref(), Some("Force push"));
        assert_eq!(hit("cd target && rm -rf build").as_deref(), Some("Deletes recursively"));
        assert_eq!(hit("Remove-Item -Path x -Recurse -Force").as_deref(), Some("Deletes recursively"));
        assert_eq!(hit("SUDO apt install").as_deref(), Some("Runs as administrator"));
        assert_eq!(hit("npm publish --access public").as_deref(), Some("Publishes a package"));
        assert_eq!(hit("terraform apply -auto-approve").as_deref(), Some("Deploys"));
        assert_eq!(hit("cat .env").as_deref(), Some("Touches secrets"));
        assert_eq!(hit("C:\\Users\\me\\.ssh\\id_ed25519").as_deref(), Some("Touches secrets"));
        assert_eq!(hit("certs/server.pem").as_deref(), Some("Touches secrets"));
    }

    #[test]
    fn ordinary_work_passes() {
        for text in [
            "git push origin main",
            "git push -u origin feature-flags",
            "cargo test -p orbit-core",
            "perform -rf",
            "npm run build",
            "rm build.log",
            "src/environment.rs",
            "pseudo code",
        ] {
            assert_eq!(hit(text), None, "{text}");
        }
    }

    #[test]
    fn requests_are_checked_by_kind() {
        let guard = GuardSettings::default();
        let cmd = RequestKind::Command { command: "rm -rf /".into(), cwd: ".".into(), reason: String::new() };
        assert!(guard.check(&cmd).is_some());
        let file = RequestKind::FileChange { path: "app/.env.local".into(), diff: String::new() };
        assert!(guard.check(&file).is_some());
        let tool = RequestKind::Tool { name: "Bash".into(), input: r#"{"command":"git push --force"}"#.into() };
        assert!(guard.check(&tool).is_some());
        let question = RequestKind::Question { question: "rm -rf?".into(), options: vec![], multi_select: false };
        assert_eq!(guard.check(&question), None);
        let off = GuardSettings { enabled: false, ..GuardSettings::default() };
        assert_eq!(off.check(&cmd), None);
    }

    #[test]
    fn sanitizing_drops_blanks_and_duplicates() {
        let guard = GuardSettings {
            enabled: true,
            rules: vec![
                GuardRule::new("  rm -rf", ""),
                GuardRule::new("RM -RF  ", "dup"),
                GuardRule::new(" ", "x"),
                GuardRule::new("sudo   ", ""),
            ],
        }
        .sanitized();
        assert_eq!(guard.rules, vec![GuardRule::new("rm -rf", "rm -rf"), GuardRule::new("sudo ", "sudo")]);
        assert!(guard.check_text("sudo make").is_some());
        assert!(guard.check_text("sudoku").is_none());
        assert_eq!(guard.clone().sanitized(), guard);
    }

    #[test]
    fn wildcards_and_word_starts() {
        let guard = GuardSettings { enabled: true, rules: vec![GuardRule::new("deploy*prod", "Deploys")] };
        assert!(guard.check_text("./deploy.sh --env prod").is_some());
        assert!(guard.check_text("redeploy prod").is_none());
        let leading = GuardSettings { enabled: true, rules: vec![GuardRule::new("*.key", "Key")] };
        assert!(leading.check_text("tls/site.key").is_some());
    }
}
