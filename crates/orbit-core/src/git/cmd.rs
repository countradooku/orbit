//! Spawning git and gh: bounded, windowless, non-interactive.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::Ordering;

use tokio::io::AsyncWriteExt;

use super::{Git, GitError, Result};

/// Variables that would point git at another repository or index.
const SCRUBBED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_PREFIX",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

#[derive(Default, Clone, Copy)]
pub(crate) struct Opts<'a> {
    pub env: &'a [(&'a str, &'a OsStr)],
    pub stdin: Option<&'a [u8]>,
}

pub(crate) struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_owned()
    }
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }
}

pub(crate) fn args<I, S>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter().map(|a| a.as_ref().to_owned()).collect()
}

impl Git {
    /// Runs git and returns its output whatever the exit code.
    pub(crate) async fn git_output<I, S>(&self, cwd: &Path, argv: I, opts: Opts<'_>) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let git = self.inner.git.clone().ok_or(GitError::NotInstalled("git"))?;
        self.exec("git", &git, cwd, args(argv), opts).await
    }

    /// Runs git and returns stdout, or a [`GitError::Failed`].
    pub(crate) async fn git<I, S>(&self, cwd: &Path, argv: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.git_with(cwd, argv, Opts::default()).await
    }

    pub(crate) async fn git_with<I, S>(&self, cwd: &Path, argv: I, opts: Opts<'_>) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let argv = args(argv);
        let out = self.git_output(cwd, &argv, opts).await?;
        if out.ok() {
            Ok(out.stdout)
        } else {
            Err(failed("git", &argv, &out))
        }
    }

    /// Runs any program (gh) under the same limits.
    pub(crate) async fn exec(
        &self,
        name: &'static str,
        program: &Path,
        cwd: &Path,
        argv: Vec<OsString>,
        opts: Opts<'_>,
    ) -> Result<Output> {
        let _permit = self.inner.permits.acquire().await.expect("semaphore is never closed");
        self.inner.spawned.fetch_add(1, Ordering::Relaxed);
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&argv)
            .current_dir(cwd)
            .stdin(if opts.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for key in SCRUBBED_ENV {
            cmd.env_remove(key);
        }
        // A folder without a repository: git works on Orbit's shadow store,
        // with the folder as its working tree.
        if name == "git" && let Some(shadow) = self.shadow_for(cwd) {
            cmd.env("GIT_DIR", &shadow.git_dir).env("GIT_WORK_TREE", &shadow.root);
        }
        // Never wait on a prompt nobody can see; English messages so errors
        // can be recognized; no opportunistic index writes behind the user.
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1");
        for (k, v) in opts.env {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                GitError::NotInstalled(name)
            } else {
                GitError::Io(e)
            }
        })?;
        let stdin = child.stdin.take();
        let feed = async {
            if let (Some(mut pipe), Some(data)) = (stdin, opts.stdin) {
                // A failed write means the process exited early; its exit
                // status and stderr tell why.
                let _ = pipe.write_all(data).await;
            }
        };
        let (_, out) = tokio::join!(feed, child.wait_with_output());
        let out = out?;
        Ok(Output { code: out.status.code(), stdout: out.stdout, stderr: out.stderr })
    }
}

pub(crate) fn failed(program: &str, argv: &[OsString], out: &Output) -> GitError {
    let mut command = program.to_owned();
    for a in argv.iter().take(6) {
        command.push(' ');
        command.push_str(&a.to_string_lossy());
    }
    if argv.len() > 6 {
        command.push_str(" …");
    }
    let mut stderr = out.stderr_text();
    if stderr.is_empty() {
        stderr = out.stdout_text();
    }
    if stderr.len() > 2000 {
        let mut cut = 2000;
        while !stderr.is_char_boundary(cut) {
            cut -= 1;
        }
        stderr.truncate(cut);
    }
    GitError::Failed { command, code: out.code, stderr }
}

/// Splits NUL-terminated output into UTF-8 fields.
pub(crate) fn nul_fields(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes.split(|&b| b == 0).filter(|f| !f.is_empty()).map(|f| String::from_utf8_lossy(f).into_owned())
}

/// The first line of output, trimmed.
pub(crate) fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines().next().unwrap_or_default().trim().to_owned()
}
