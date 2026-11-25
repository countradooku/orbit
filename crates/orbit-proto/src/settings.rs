//! User settings.
//!
//! One record, owned by the engine and persisted in its meta log, so a client
//! that connects to a remote host sees (and edits) that host's settings,
//! including the purely visual ones: the same theme, zoom and submit key on
//! every machine that drives the host.
//!
//! Every section has a `Default` that is a sensible first-run value, and
//! [`Settings::sanitized`] is the single place that enforces invariants the
//! interface also promises (a new thread never defaults to full access, sizes
//! stay in a usable range). The engine sanitizes every update it accepts.
//!
//! [`RemoteSettings`] is the exception: which host to connect to has to be
//! known before any engine is reachable, so the client keeps it in its own
//! file and it never travels in [`Settings`].

use bitcode::{Decode, Encode};

use crate::model::{Access, ProviderKind};

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Settings {
    pub general: GeneralSettings,
    pub appearance: AppearanceSettings,
    pub composer: ComposerSettings,
    pub agents: AgentSettings,
    pub workspace: WorkspaceSettings,
    pub review: ReviewSettings,
    pub notifications: NotificationSettings,
    pub advanced: AdvancedSettings,
}

impl Settings {
    /// The settings with every invariant enforced. Idempotent.
    pub fn sanitized(mut self) -> Self {
        let a = &mut self.appearance;
        a.ui_scale = a.ui_scale.clamp(UI_SCALE_MIN, UI_SCALE_MAX);
        a.code_font_size = a.code_font_size.clamp(CODE_FONT_MIN, CODE_FONT_MAX);
        trim(&mut a.light_theme);
        trim(&mut a.dark_theme);

        let agents = &mut self.agents;
        agents.default_access = agents.default_access.for_new_thread();
        let mut providers: Vec<ProviderSettings> = Vec::with_capacity(ProviderKind::ALL.len());
        for kind in ProviderKind::ALL {
            let mut p = agents.providers.iter().find(|p| p.kind == kind).cloned().unwrap_or_else(|| ProviderSettings::new(kind));
            trim(&mut p.binary);
            p.env.retain(|v| is_env_name(&v.name));
            for var in &mut p.env {
                trim(&mut var.name);
            }
            // The last definition of a name wins, as in a shell.
            let mut seen = Vec::new();
            for ix in (0..p.env.len()).rev() {
                if seen.contains(&p.env[ix].name) {
                    p.env.remove(ix);
                } else {
                    seen.push(p.env[ix].name.clone());
                }
            }
            if !p.is_default() {
                providers.push(p);
            }
        }
        agents.providers = providers;

        trim(&mut self.workspace.worktree_root);
        trim(&mut self.advanced.git_binary);
        trim(&mut self.advanced.gh_binary);
        self
    }

    /// The settings for one provider (defaults when none are stored).
    pub fn provider(&self, kind: ProviderKind) -> ProviderSettings {
        self.agents.providers.iter().find(|p| p.kind == kind).cloned().unwrap_or_else(|| ProviderSettings::new(kind))
    }

    /// Mutable settings for one provider, created on first use.
    pub fn provider_mut(&mut self, kind: ProviderKind) -> &mut ProviderSettings {
        let providers = &mut self.agents.providers;
        let ix = match providers.iter().position(|p| p.kind == kind) {
            Some(ix) => ix,
            None => {
                providers.push(ProviderSettings::new(kind));
                providers.len() - 1
            }
        };
        &mut providers[ix]
    }
}

fn trim(s: &mut String) {
    let trimmed = s.trim();
    if trimmed.len() != s.len() {
        *s = trimmed.to_owned();
    }
}

/// A portable environment variable name: a letter or `_`, then letters,
/// digits or `_`.
pub fn is_env_name(name: &str) -> bool {
    let name = name.trim();
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ── General ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct GeneralSettings {
    /// Ask before deleting a thread (its timeline, checkpoints and worktree).
    pub confirm_delete: bool,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self { confirm_delete: true }
    }
}

// ── Appearance ───────────────────────────────────────────────────────────

pub const UI_SCALE_MIN: u16 = 75;
pub const UI_SCALE_MAX: u16 = 150;
pub const CODE_FONT_MIN: u16 = 10;
pub const CODE_FONT_MAX: u16 = 20;

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AppearanceSettings {
    pub mode: ColorMode,
    /// Theme used in light mode; empty means Orbit Light.
    pub light_theme: String,
    /// Theme used in dark mode; empty means Orbit Dark.
    pub dark_theme: String,
    /// Interface zoom in percent. Scales the base font, so text, spacing,
    /// controls and icons grow together.
    pub ui_scale: u16,
    /// Code, diff and terminal-style text, in pixels at 100 % zoom.
    pub code_font_size: u16,
    pub chat_width: ChatWidth,
    pub motion: Motion,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            mode: ColorMode::System,
            light_theme: String::new(),
            dark_theme: String::new(),
            ui_scale: 100,
            code_font_size: 13,
            chat_width: ChatWidth::Comfortable,
            motion: Motion::System,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
pub enum ColorMode {
    #[default]
    System,
    Light,
    Dark,
}

/// Width of the reading column in threads, the inbox and home.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
pub enum ChatWidth {
    Narrow,
    #[default]
    Comfortable,
    Wide,
    /// Use the whole main area.
    Full,
}

impl ChatWidth {
    pub const ALL: [ChatWidth; 4] = [ChatWidth::Narrow, ChatWidth::Comfortable, ChatWidth::Wide, ChatWidth::Full];

    /// Column width in rem; `None` for the full width.
    pub fn rems(self) -> Option<f32> {
        match self {
            ChatWidth::Narrow => Some(44.),
            ChatWidth::Comfortable => Some(52.),
            ChatWidth::Wide => Some(66.),
            ChatWidth::Full => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
pub enum Motion {
    /// Follow the operating system's reduced-motion setting.
    #[default]
    System,
    Reduce,
    Allow,
}

// ── Composer ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct ComposerSettings {
    pub submit: SubmitKey,
    /// What sending does while a turn runs; the alternate key does the other.
    pub follow_up: FollowUp,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
pub enum SubmitKey {
    /// Enter sends, Shift+Enter inserts a new line.
    #[default]
    Enter,
    /// Enter inserts a new line, Ctrl+Enter (Cmd+Enter on macOS) sends.
    ModEnter,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
pub enum FollowUp {
    /// Hold the message until the running turn ends.
    #[default]
    Queue,
    /// Inject the message into the running turn.
    Steer,
}

// ── Agents ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentSettings {
    /// Access level new threads start with. Never [`Access::Full`].
    pub default_access: Access,
    /// Per-provider overrides; a provider at its defaults is not stored.
    pub providers: Vec<ProviderSettings>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self { default_access: Access::Ask, providers: Vec::new() }
    }
}

impl Access {
    /// The level a new thread may start with: full access is lowered to
    /// auto-edit, so it is always an explicit per-thread choice.
    pub fn for_new_thread(self) -> Access {
        match self {
            Access::Full => Access::AutoEdit,
            other => other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ProviderSettings {
    pub kind: ProviderKind,
    /// Explicit path to the CLI; empty finds it on PATH and in the usual
    /// install locations.
    pub binary: String,
    /// Extra environment for the CLI (for example `CLAUDE_CONFIG_DIR`,
    /// `CODEX_HOME`, a proxy, or an API base URL).
    pub env: Vec<EnvVar>,
}

impl ProviderSettings {
    pub fn new(kind: ProviderKind) -> Self {
        Self { kind, binary: String::new(), env: Vec::new() }
    }

    pub fn is_default(&self) -> bool {
        self.binary.is_empty() && self.env.is_empty()
    }

    /// The environment as (name, value) pairs.
    pub fn env_pairs(&self) -> Vec<(String, String)> {
        self.env.iter().map(|v| (v.name.clone(), v.value.clone())).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

impl EnvVar {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self { name: name.into(), value: value.into() }
    }
}

/// Parses `NAME=value` lines; blank lines, `#` comments and lines without a
/// valid name are skipped. The value keeps everything after the first `=`.
pub fn parse_env(text: &str) -> Vec<EnvVar> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (name, value) = line.split_once('=')?;
            let name = name.trim();
            is_env_name(name).then(|| EnvVar::new(name, value.trim_end_matches(['\r', '\n'])))
        })
        .collect()
}

/// The inverse of [`parse_env`].
pub fn format_env(env: &[EnvVar]) -> String {
    env.iter().map(|v| format!("{}={}", v.name, v.value)).collect::<Vec<_>>().join("\n")
}

// ── Workspace and git ────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct WorkspaceSettings {
    /// Where new worktrees go (`<root>/<project>/<branch>`); empty means
    /// `<data folder>/worktrees`.
    pub worktree_root: String,
    /// New threads in a project start on their own worktree and branch.
    pub worktree_by_default: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct ReviewSettings {
    /// Show diffs side by side instead of unified.
    pub split: bool,
    /// Open the review with every file collapsed to its header.
    pub collapse_files: bool,
}

// ── Notifications ────────────────────────────────────────────────────────

/// Operating-system notifications, posted while Orbit is in the background.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct NotificationSettings {
    /// An agent asks for approval or has a question.
    pub requests: bool,
    /// A turn finished with a result.
    pub finished: bool,
    /// A turn failed or hit a usage limit.
    pub failed: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self { requests: true, finished: true, failed: true }
    }
}

// ── Advanced ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AdvancedSettings {
    /// Explicit git binary; empty uses `git` from PATH.
    pub git_binary: String,
    /// Explicit GitHub CLI binary; empty uses `gh` from PATH.
    pub gh_binary: String,
    /// New untracked files larger than this (MB) are left out of turn
    /// checkpoints; 0 keeps every file.
    pub checkpoint_file_limit_mb: u32,
}

impl Default for AdvancedSettings {
    fn default() -> Self {
        Self { git_binary: String::new(), gh_binary: String::new(), checkpoint_file_limit_mb: 50 }
    }
}

// ── Remote ───────────────────────────────────────────────────────────────

/// Which engine the client drives. Kept by the client, never by an engine.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct RemoteSettings {
    /// `ws://host:port/` of an `orbitd`; empty runs the engine in-process.
    pub host: String,
    /// The host's access token (`orbitd --print-token`).
    pub token: String,
}

impl RemoteSettings {
    pub fn is_remote(&self) -> bool {
        !self.host.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};

    #[test]
    fn settings_round_trip() {
        let mut s = Settings::default();
        s.appearance.dark_theme = "Tokyo Night".into();
        s.provider_mut(ProviderKind::Codex).binary = "C:/tools/codex.exe".into();
        s.provider_mut(ProviderKind::Codex).env.push(EnvVar::new("CODEX_HOME", "D:/codex"));
        let back: Settings = decode(&encode(&s)).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn new_threads_never_default_to_full_access() {
        let mut s = Settings::default();
        s.agents.default_access = Access::Full;
        assert_eq!(s.sanitized().agents.default_access, Access::AutoEdit);
    }

    #[test]
    fn sanitizing_clamps_sizes_and_drops_bad_env() {
        let mut s = Settings::default();
        s.appearance.ui_scale = 400;
        s.appearance.code_font_size = 2;
        let claude = s.provider_mut(ProviderKind::Claude);
        claude.binary = "  /opt/claude  ".into();
        claude.env = vec![EnvVar::new("1BAD", "x"), EnvVar::new("A", "1"), EnvVar::new("A", "2")];
        s.provider_mut(ProviderKind::Codex);
        let s = s.sanitized();
        assert_eq!(s.appearance.ui_scale, UI_SCALE_MAX);
        assert_eq!(s.appearance.code_font_size, CODE_FONT_MIN);
        let claude = s.provider(ProviderKind::Claude);
        assert_eq!(claude.binary, "/opt/claude");
        assert_eq!(claude.env, vec![EnvVar::new("A", "2")]);
        // Codex is at its defaults, so nothing is stored for it.
        assert_eq!(s.agents.providers.len(), 1);
        assert_eq!(s.clone().sanitized(), s);
    }

    #[test]
    fn env_text_parses_and_formats() {
        let env = parse_env("# proxy\nHTTPS_PROXY=http://p:8080\n\nBAD LINE\nURL=a=b\n 9X=no");
        assert_eq!(env, vec![EnvVar::new("HTTPS_PROXY", "http://p:8080"), EnvVar::new("URL", "a=b")]);
        assert_eq!(parse_env(&format_env(&env)), env);
    }
}
