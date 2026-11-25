//! Pure parts of first run: the sample prompt chosen for a project, the
//! agents' fix-it steps, and reading a clone URL.

use std::path::Path;

use orbit_proto::ProviderKind;

/// What kind of project a folder holds, from the files at its top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectKind {
    Rust,
    Node,
    Python,
    Go,
    /// Code of another kind, or a README only.
    Other,
    /// No project folder (a scratch thread), or an empty folder.
    Empty,
}

impl ProjectKind {
    pub fn detect(folder: Option<&Path>) -> Self {
        let Some(folder) = folder else { return ProjectKind::Empty };
        let has = |name: &str| folder.join(name).exists();
        if has("Cargo.toml") {
            ProjectKind::Rust
        } else if has("package.json") {
            ProjectKind::Node
        } else if has("pyproject.toml") || has("setup.py") || has("requirements.txt") {
            ProjectKind::Python
        } else if has("go.mod") {
            ProjectKind::Go
        } else if std::fs::read_dir(folder).map(|mut d| d.any(|e| e.is_ok_and(|e| !e.file_name().to_string_lossy().starts_with('.')))).unwrap_or(false) {
            ProjectKind::Other
        } else {
            ProjectKind::Empty
        }
    }
}

/// A first task that is useful, quick and safe for this kind of project: it
/// reads before it writes, and the change it proposes is small enough to
/// review in a minute.
pub fn sample_prompt(kind: ProjectKind) -> &'static str {
    match kind {
        ProjectKind::Rust => {
            "Give me a short tour of this crate: what it does, how it’s organized, and how to build and test it. Then \
             pick one public function that has no test and add a focused unit test for it."
        }
        ProjectKind::Node => {
            "Give me a short tour of this project: what it does, its main entry points, and how to run and test it. \
             Then pick one exported function that has no test and add a focused test for it."
        }
        ProjectKind::Python => {
            "Give me a short tour of this project: what it does, how it’s laid out, and how to run it. Then pick one \
             function that has no test and add a focused pytest test for it."
        }
        ProjectKind::Go => {
            "Give me a short tour of this module: what it does, its packages, and how to build and test it. Then pick \
             one exported function that has no test and add a table-driven test for it."
        }
        ProjectKind::Other => {
            "Give me a short tour of this codebase: what it does, how it’s structured, and how to run it. Then suggest \
             three small improvements and make the smallest one."
        }
        ProjectKind::Empty => {
            "Write a small command-line program in Python that prints the first 20 Fibonacci numbers, with a test, \
             and explain how to run both."
        }
    }
}

/// How to install an agent and sign in, exactly as typed in a terminal.
pub struct FixSteps {
    pub install: &'static str,
    pub sign_in: &'static str,
    /// What to do once `sign_in` runs, when it isn't obvious.
    pub sign_in_note: Option<&'static str>,
}

pub fn fix_steps(kind: ProviderKind) -> Option<FixSteps> {
    Some(match kind {
        ProviderKind::Claude => FixSteps {
            install: "npm install -g @anthropic-ai/claude-code",
            sign_in: "claude",
            sign_in_note: Some("then type /login and follow the browser prompt"),
        },
        ProviderKind::Codex => FixSteps { install: "npm install -g @openai/codex", sign_in: "codex login", sign_in_note: None },
        ProviderKind::OpenCode => {
            FixSteps { install: "npm install -g opencode-ai", sign_in: "opencode auth login", sign_in_note: None }
        }
        ProviderKind::Pi => FixSteps {
            install: "npm install -g @mariozechner/pi-coding-agent",
            sign_in: "pi",
            sign_in_note: Some("then type /login, or set your provider’s API key"),
        },
        ProviderKind::Acp => return None,
    })
}

/// A clone URL Orbit accepts, and the folder name it clones into.
#[derive(Debug, PartialEq, Eq)]
pub struct CloneUrl {
    pub url: String,
    pub folder: String,
}

/// Reads what the user typed: an `https://`, `ssh://`, `git://` or
/// `git@host:owner/repo` address, or `owner/repo` for GitHub. The URL is
/// passed to git as one argument after `--`, never through a shell.
pub fn parse_clone_url(text: &str) -> Result<CloneUrl, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Paste a repository address.".to_owned());
    }
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("A repository address has no spaces.".to_owned());
    }
    let url = if text.contains("://") || text.starts_with("git@") {
        text.to_owned()
    } else if text.split('/').count() == 2 && text.split('/').all(|p| !p.is_empty() && !p.starts_with('.')) {
        format!("https://github.com/{}", text.trim_end_matches(".git"))
    } else {
        return Err("Use an address like https://github.com/owner/repo or git@github.com:owner/repo.git.".to_owned());
    };
    let scheme_ok = ["https://", "http://", "ssh://", "git://", "git@"].iter().any(|s| url.starts_with(s));
    if !scheme_ok {
        return Err("Use an https://, ssh:// or git@ address.".to_owned());
    }
    // A host alone names no repository.
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map(|(_, p)| p),
        None => url.split_once(':').map(|(_, p)| p),
    };
    if path.is_none_or(|p| p.trim_matches('/').is_empty()) {
        return Err("That address doesn’t name a repository.".to_owned());
    }
    let last = url.trim_end_matches('/').rsplit(['/', ':']).next().unwrap_or_default();
    let folder = last.strip_suffix(".git").unwrap_or(last).to_owned();
    let valid = !folder.is_empty()
        && folder != "."
        && folder != ".."
        && folder.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid {
        return Err("That address doesn’t end in a repository name.".to_owned());
    }
    Ok(CloneUrl { url, folder })
}

/// The last progress line of `git clone --progress` output, without the
/// carriage returns git redraws it with: "Receiving objects:  45% (…)".
pub fn clone_progress(output: &str) -> Option<String> {
    output
        .split(['\r', '\n'])
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(|l| l.trim_start_matches("remote: ").to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_urls_are_read_strictly() {
        assert_eq!(
            parse_clone_url(" https://github.com/acme/rocket.git ").unwrap(),
            CloneUrl { url: "https://github.com/acme/rocket.git".into(), folder: "rocket".into() }
        );
        assert_eq!(parse_clone_url("git@github.com:acme/rocket.git").unwrap().folder, "rocket");
        assert_eq!(parse_clone_url("ssh://git@host:22/team/app").unwrap().folder, "app");
        assert_eq!(parse_clone_url("acme/rocket").unwrap().url, "https://github.com/acme/rocket");
        assert_eq!(parse_clone_url("https://gitlab.com/group/sub/proj/").unwrap().folder, "proj");
        for bad in ["", "rocket", "--upload-pack=evil", "https://x/a b", "file:///etc", "../x", "https://host/"] {
            assert!(parse_clone_url(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn clone_progress_is_the_last_line() {
        let out = "Cloning into 'x'...\nremote: Counting objects: 10% (1/10)\rremote: Counting objects: 100% (10/10)\nReceiving objects:  45% (5/11)\r";
        assert_eq!(clone_progress(out).unwrap(), "Receiving objects:  45% (5/11)");
        assert_eq!(clone_progress("remote: Enumerating objects: 3\n").unwrap(), "Enumerating objects: 3");
        assert_eq!(clone_progress(""), None);
    }

    #[test]
    fn projects_get_a_prompt_for_their_kind() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(ProjectKind::detect(Some(dir.path())), ProjectKind::Empty);
        std::fs::write(dir.path().join("README.md"), "hi").unwrap();
        assert_eq!(ProjectKind::detect(Some(dir.path())), ProjectKind::Other);
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(ProjectKind::detect(Some(dir.path())), ProjectKind::Node);
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(ProjectKind::detect(Some(dir.path())), ProjectKind::Rust);
        assert_eq!(ProjectKind::detect(None), ProjectKind::Empty);
        assert!(sample_prompt(ProjectKind::Rust).contains("test"));
        for kind in [ProviderKind::Claude, ProviderKind::Codex, ProviderKind::OpenCode, ProviderKind::Pi] {
            assert!(fix_steps(kind).unwrap().install.starts_with("npm install -g "));
        }
    }
}
