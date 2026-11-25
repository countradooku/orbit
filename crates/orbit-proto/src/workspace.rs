//! Workspaces: what a project is set up with (scripts, run targets, shared
//! files, environment, ports), the live git status of every checkout, and
//! read-only access to the files of a project or a thread's worktree.
//!
//! A project's configuration is its own record ([`ProjectConfig`]), logged
//! as [`crate::Event::ProjectConfigChanged`], so the logged [`crate::Project`]
//! keeps its layout. It can be imported from an `orbit.toml` checked into
//! the repository (see `docs/orbit-toml.md`), or from Conductor, Codex and
//! dev container configuration.

use bitcode::{Decode, Encode};

use crate::ids::*;
use crate::model::ThreadConfig;
use crate::settings::EnvVar;

/// The longest file [`WorkspaceCommand::ReadFile`] returns as text.
pub const MAX_READ_TEXT: u32 = 8 * 1024 * 1024;
/// The largest image [`WorkspaceCommand::ReadFile`] returns.
pub const MAX_READ_IMAGE: u32 = 16 * 1024 * 1024;
/// Entries one [`WorkspaceCommand::ListDir`] returns at most.
pub const MAX_DIR_ENTRIES: usize = 5_000;

/// How a project is set up, beyond its name, folder, base branch and
/// worktree scripts (which live on [`crate::Project`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct ProjectConfig {
    /// One emoji or up to two letters shown for the project; empty draws
    /// the monogram of its name.
    pub icon: String,
    /// The agent, model and options a new thread in this project starts
    /// with; `None` follows the last used.
    pub default_config: Option<ThreadConfig>,
    /// New threads get their own worktree; `None` follows Settings ›
    /// Workspace.
    pub worktree_by_default: Option<bool>,
    /// Added to the environment of the project's scripts, run targets and
    /// worktree terminals.
    pub env: Vec<EnvVar>,
    /// Untracked files copied from the main checkout into every new
    /// worktree (gitignore syntax: `.env*`, `config/local.json`), on top of
    /// the repository's `.worktreeinclude`.
    pub include: Vec<String>,
    /// Ports reserved for each worktree, exported as `PORT` (the first) and
    /// `ORBIT_PORT_BASE`/`ORBIT_PORT_COUNT`; 0 uses Orbit's default (10).
    pub ports: u16,
    /// Commands run on demand: dev servers, tests, linters.
    pub targets: Vec<ProjectTarget>,
    /// Hidden from the sidebar and pickers; its threads and data are kept.
    pub archived: bool,
    /// The files the configuration was last imported from, for display.
    pub imported_from: Vec<String>,
}

impl ProjectConfig {
    /// The configuration with every invariant enforced. Idempotent.
    pub fn sanitized(mut self) -> Self {
        self.icon = self.icon.trim().chars().take(4).collect();
        self.env.retain(|v| crate::settings::is_env_name(v.name.trim()));
        for var in &mut self.env {
            var.name = var.name.trim().to_owned();
        }
        // The last definition of a name wins, as in a shell.
        let mut seen: Vec<String> = Vec::new();
        for ix in (0..self.env.len()).rev() {
            if seen.contains(&self.env[ix].name) {
                self.env.remove(ix);
            } else {
                seen.push(self.env[ix].name.clone());
            }
        }
        self.include = self.include.iter().map(|p| p.trim().to_owned()).filter(|p| !p.is_empty() && !p.starts_with('#')).collect();
        self.include.dedup();
        self.ports = self.ports.min(100);
        self.targets.retain(|t| !t.name.trim().is_empty() && !t.command.trim().is_empty());
        let mut names: Vec<String> = Vec::new();
        for target in &mut self.targets {
            target.name = target.name.trim().to_owned();
            target.command = target.command.trim().to_owned();
            target.url = target.url.trim().to_owned();
            // Names are how targets are run and stopped: make them unique.
            let base = target.name.clone();
            let mut n = 2;
            while names.iter().any(|existing| existing.eq_ignore_ascii_case(&target.name)) {
                target.name = format!("{base} {n}");
                n += 1;
            }
            names.push(target.name.clone());
        }
        self
    }

    pub fn target(&self, name: &str) -> Option<&ProjectTarget> {
        self.targets.iter().find(|t| t.name == name)
    }
}

/// A command a project runs on demand in a thread's workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct ProjectTarget {
    /// Shown on the Run button and in the palette (`Dev`, `Test`).
    pub name: String,
    /// Run through the user's shell, in the thread's folder.
    pub command: String,
    pub kind: TargetKind,
    /// The address it serves, offered to the preview browser; `$PORT` is
    /// the worktree's first reserved port. Empty: found in its output.
    pub url: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum TargetKind {
    /// A long-running server.
    #[default]
    Dev,
    Test,
    Lint,
    Build,
    Other,
}

impl TargetKind {
    pub const ALL: [TargetKind; 5] = [TargetKind::Dev, TargetKind::Test, TargetKind::Lint, TargetKind::Build, TargetKind::Other];

    pub fn label(self) -> &'static str {
        match self {
            TargetKind::Dev => "Dev server",
            TargetKind::Test => "Tests",
            TargetKind::Lint => "Lint",
            TargetKind::Build => "Build",
            TargetKind::Other => "Other",
        }
    }

    /// Guessed from a target's name (`dev`, `test:unit`, `lint`).
    pub fn guess(name: &str) -> Self {
        let name = name.to_ascii_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| name.contains(w));
        if has(&["test", "spec", "e2e"]) {
            TargetKind::Test
        } else if has(&["lint", "check", "clippy", "format", "fmt", "typecheck"]) {
            TargetKind::Lint
        } else if has(&["build", "compile", "bundle"]) {
            TargetKind::Build
        } else if has(&["dev", "serve", "start", "watch", "storybook", "preview", "run"]) {
            TargetKind::Dev
        } else {
            TargetKind::Other
        }
    }
}

/// The folder a workspace command reads: a project's main checkout, or a
/// thread's own workspace (its worktree, else its project's folder).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum WorkspaceRoot {
    Project(ProjectId),
    Thread(ThreadId),
}

/// Where a checkout stands, as the sidebar and the thread header show it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct GitSummary {
    /// The folder is a git checkout.
    pub repo: bool,
    /// The checked-out branch; `None` when HEAD is detached.
    pub branch: Option<String>,
    /// HEAD's commit, abbreviated.
    pub head: Option<String>,
    /// What `ahead` and `behind` count against: the upstream, else the
    /// project's base branch.
    pub compared_to: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Files changed against HEAD (staged, unstaged and untracked).
    pub changed: u32,
    pub additions: u32,
    pub deletions: u32,
    /// Files with unresolved merge conflicts.
    pub conflicts: u32,
}

impl GitSummary {
    pub fn is_clean(&self) -> bool {
        self.changed == 0 && self.conflicts == 0
    }
}

/// A file read by [`WorkspaceCommand::ReadFile`].
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct FileContent {
    /// Relative to the root, `/`-separated.
    pub path: String,
    /// The file's absolute path on the host.
    pub absolute: String,
    /// Bytes on disk.
    pub size: u64,
    pub modified_ms: u64,
    pub body: FileBody,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum FileBody {
    /// UTF-8 text (invalid sequences replaced); `truncated` when only the
    /// first `max_bytes` were read.
    Text { text: String, truncated: bool },
    /// An image the client can draw, whole.
    Image { mime: String, data: Vec<u8> },
    /// Not text and not a known image.
    Binary,
}

/// One entry of a folder, as [`WorkspaceCommand::ListDir`] lists it:
/// ignored files and `.git` are left out.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    /// Bytes, for files.
    pub size: u64,
}

/// An instruction file an agent working in the folder reads.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct MemoryFile {
    /// Absolute path on the host.
    pub absolute: String,
    /// Relative to the root when inside it (`CLAUDE.md`, `web/AGENTS.md`),
    /// else the absolute path with the home folder as `~`.
    pub label: String,
    pub scope: MemoryScope,
    pub kind: MemoryKind,
    pub is_dir: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum MemoryScope {
    /// In the folder itself: loaded at start.
    Root,
    /// In a subfolder: loaded when the agent works there.
    Nested,
    /// In a folder above it.
    Parent,
    /// The user's own, for every project.
    User,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum MemoryKind {
    /// `CLAUDE.md`, `CLAUDE.local.md`: Claude Code.
    Claude,
    /// `AGENTS.md`: Codex, OpenCode, Pi and most others.
    Agents,
    /// `.claude/`: settings, commands, agents and skills.
    ClaudeFolder,
}

/// A run target started in a thread's workspace.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct TargetRun {
    pub thread: ThreadId,
    pub target: String,
    /// The terminal it runs in; its output arrives as
    /// [`crate::Event::TerminalOutput`].
    pub term: Option<TermId>,
    pub status: TargetStatus,
    /// The address it serves, when the target names one.
    pub url: Option<String>,
    pub started_ms: u64,
    pub ended_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum TargetStatus {
    Running,
    /// Ended on its own.
    Exited { code: Option<i32> },
    /// Stopped by the user.
    Stopped,
    /// Never started.
    Failed { message: String },
}

impl TargetStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, TargetStatus::Running)
    }

    /// Ended with an error.
    pub fn is_failure(&self) -> bool {
        match self {
            TargetStatus::Exited { code } => code.is_some_and(|c| c != 0),
            TargetStatus::Failed { .. } => true,
            _ => false,
        }
    }
}

/// Workspace commands; see [`crate::Command::Workspace`].
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum WorkspaceCommand {
    /// Reads a file under the root: text up to `max_bytes` (0: up to
    /// [`MAX_READ_TEXT`]), images whole, anything else as binary. Paths
    /// that leave the root are refused. Replies [`WorkspaceReply::File`].
    ReadFile { root: WorkspaceRoot, path: String, max_bytes: u32 },
    /// A folder's entries (`""` is the root), folders first, ignored files
    /// left out. Replies [`WorkspaceReply::Dir`].
    ListDir { root: WorkspaceRoot, path: String },
    /// The checkout's status now. Replies [`WorkspaceReply::Status`].
    Status { root: WorkspaceRoot },
    /// The instruction files that apply to the folder. Replies
    /// [`WorkspaceReply::Memory`].
    MemoryFiles { root: WorkspaceRoot },
    /// Replaces a project's configuration (sanitized), broadcast as
    /// [`crate::Event::ProjectConfigChanged`].
    SetConfig { project: ProjectId, config: Box<ProjectConfig> },
    /// Reads `orbit.toml` and other tools' configuration from the project's
    /// folder without applying it. Replies [`WorkspaceReply::Imported`].
    Import { project: ProjectId },
    /// Starts a run target in the thread's workspace, restarting it if it
    /// runs. Replies [`WorkspaceReply::Run`].
    Run { thread: ThreadId, target: String, cols: u16, rows: u16 },
    /// Stops a running target.
    Stop { thread: ThreadId, target: String },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum WorkspaceReply {
    File(Box<FileContent>),
    Dir(Vec<DirEntry>),
    Status(Box<GitSummary>),
    Memory(Vec<MemoryFile>),
    /// What the repository's files configure, merged over the project's
    /// record (name, base branch, scripts) and configuration; `sources`
    /// names the files that contributed.
    Imported { project: Box<crate::model::Project>, config: Box<ProjectConfig>, sources: Vec<String> },
    Run(Box<TargetRun>),
}

/// Replaces `$PORT` / `${PORT}` in a target's address with the worktree's
/// port; an address without a port variable is returned as it is.
pub fn expand_port(url: &str, port: Option<u16>) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    if !url.contains("$PORT") && !url.contains("${PORT}") {
        return Some(url.to_owned());
    }
    let port = port?.to_string();
    Some(url.replace("${PORT}", &port).replace("$PORT", &port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};

    #[test]
    fn sanitizing_keeps_names_unique_and_drops_empty_targets() {
        let config = ProjectConfig {
            icon: "  🚀🚀🚀🚀🚀 ".into(),
            env: vec![EnvVar::new(" A ", "1"), EnvVar::new("A", "2"), EnvVar::new("1BAD", "x")],
            include: vec![" .env ".into(), "".into(), "# comment".into()],
            targets: vec![
                ProjectTarget { name: "Dev".into(), command: "npm run dev".into(), ..Default::default() },
                ProjectTarget { name: "dev".into(), command: "vite".into(), ..Default::default() },
                ProjectTarget { name: "Empty".into(), command: "  ".into(), ..Default::default() },
            ],
            ports: 1000,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(config.icon.chars().count(), 4);
        assert_eq!(config.env, vec![EnvVar::new("A", "2")]);
        assert_eq!(config.include, vec![".env".to_owned()]);
        let names: Vec<&str> = config.targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["Dev", "dev 2"]);
        assert_eq!(config.ports, 100);
        assert_eq!(config.clone().sanitized(), config);
    }

    #[test]
    fn kinds_are_guessed_from_names() {
        assert_eq!(TargetKind::guess("dev"), TargetKind::Dev);
        assert_eq!(TargetKind::guess("test:unit"), TargetKind::Test);
        assert_eq!(TargetKind::guess("lint"), TargetKind::Lint);
        assert_eq!(TargetKind::guess("typecheck"), TargetKind::Lint);
        assert_eq!(TargetKind::guess("build"), TargetKind::Build);
        assert_eq!(TargetKind::guess("release-notes"), TargetKind::Other);
    }

    #[test]
    fn ports_expand_in_addresses() {
        assert_eq!(expand_port("http://localhost:$PORT", Some(41000)).as_deref(), Some("http://localhost:41000"));
        assert_eq!(expand_port("http://localhost:${PORT}/app", Some(5)).as_deref(), Some("http://localhost:5/app"));
        assert_eq!(expand_port("http://localhost:3000", None).as_deref(), Some("http://localhost:3000"));
        assert_eq!(expand_port("http://localhost:$PORT", None), None);
        assert_eq!(expand_port("  ", Some(1)), None);
    }

    #[test]
    fn workspace_messages_round_trip() {
        let command = crate::Command::Workspace(WorkspaceCommand::ReadFile {
            root: WorkspaceRoot::Thread(ThreadId(9)),
            path: "src/main.rs".into(),
            max_bytes: 0,
        });
        assert_eq!(decode::<crate::Command>(&encode(&command)).unwrap(), command);
        let event = crate::Event::ProjectConfigChanged {
            project: ProjectId(3),
            config: Box::new(ProjectConfig {
                icon: "🛰".into(),
                targets: vec![ProjectTarget { name: "Dev".into(), command: "npm run dev".into(), kind: TargetKind::Dev, url: "http://localhost:$PORT".into() }],
                ..Default::default()
            }),
        };
        assert!(event.is_durable());
        assert_eq!(decode::<crate::Event>(&encode(&event)).unwrap(), event);
        let status = crate::Event::GitStatus {
            root: WorkspaceRoot::Project(ProjectId(3)),
            status: Box::new(GitSummary { repo: true, branch: Some("main".into()), changed: 2, ..Default::default() }),
        };
        assert!(!status.is_durable());
        assert_eq!(decode::<crate::Event>(&encode(&status)).unwrap(), status);
    }
}
