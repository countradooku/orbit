//! Running `gh` and `glab`: windowless, non-interactive, bounded and timed.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

use super::ForgeError;

/// Variables that would point a CLI at another repository or make it ask.
const SCRUBBED_ENV: &[&str] = &["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GH_REPO", "GITLAB_REPO"];

/// A host CLI call: the program, where to run it and what to feed it.
pub(crate) struct Call<'a> {
    /// Shown in errors: `gh`, `glab`.
    pub name: &'static str,
    pub program: &'a Path,
    pub cwd: &'a Path,
    pub args: Vec<String>,
    pub stdin: Option<&'a [u8]>,
    pub timeout: Duration,
}

pub(crate) struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// stderr, or stdout when stderr is empty (some CLIs print errors there).
    pub fn message(&self) -> String {
        let text = if self.stderr.trim().is_empty() { self.stdout_text() } else { self.stderr.clone() };
        let text = text.trim();
        // Long messages are cut at a char boundary.
        match text.char_indices().nth(1500) {
            Some((at, _)) => format!("{}…", &text[..at]),
            None => text.to_owned(),
        }
    }
}

/// Runs a call under `permits`. A timeout kills the process.
pub(crate) async fn run(permits: &Semaphore, call: Call<'_>) -> Result<Output, ForgeError> {
    let _permit = permits.acquire().await.expect("semaphore is never closed");
    let mut cmd = tokio::process::Command::new(call.program);
    cmd.args(&call.args)
        .current_dir(call.cwd)
        .stdin(if call.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in SCRUBBED_ENV {
        cmd.env_remove(key);
    }
    // Never wait on a prompt nobody can see, never page, never color.
    cmd.env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_PAGER", "")
        .env("NO_PROMPT", "1")
        .env("GLAB_CHECK_UPDATE", "false")
        .env("GLAB_NO_PROMPT", "1")
        .env("PAGER", "")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ForgeError::NotInstalled { program: call.name.into(), fix: install_hint(call.name) },
        _ => ForgeError::Failed(format!("Couldn’t start {}: {e}", call.name)),
    })?;
    let stdin = child.stdin.take();
    let feed = async {
        if let (Some(mut pipe), Some(data)) = (stdin, call.stdin) {
            let _ = pipe.write_all(data).await;
        }
    };
    let work = async { tokio::join!(feed, child.wait_with_output()).1 };
    let out = tokio::time::timeout(call.timeout, work)
        .await
        .map_err(|_| ForgeError::Failed(format!("{} didn’t answer within {} seconds.", call.name, call.timeout.as_secs())))?
        .map_err(|e| ForgeError::Failed(format!("{} failed: {e}", call.name)))?;
    Ok(Output { code: out.status.code(), stdout: out.stdout, stderr: String::from_utf8_lossy(&out.stderr).into_owned() })
}

pub(crate) fn install_hint(program: &str) -> String {
    match program {
        "gh" => "Install the GitHub CLI from https://cli.github.com, then run `gh auth login`.".into(),
        "glab" => "Install the GitLab CLI from https://gitlab.com/gitlab-org/cli, then run `glab auth login`.".into(),
        other => format!("Install {other}, or set its path in Settings."),
    }
}

/// Removes ANSI escape sequences (CI logs keep their colors).
pub(crate) fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // Parameters and intermediates, then one final byte.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(strip_ansi("\u{1b}[31merror\u{1b}[0m: x"), "error: x");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
