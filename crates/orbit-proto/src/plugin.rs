//! Plugins on the wire: what the host tells clients about installed
//! plugins, and the commands that install, enable and drive them.
//!
//! Plugins live in the host (`<data>/plugins/`, see `orbit-plugin`); their
//! engine-side parts (agent manifests, hooks, WASM logic, storage) run
//! there. A client receives everything its UI contributions need as data
//! in [`PluginsState`]: themes, snippets, slash commands, keybindings,
//! palette commands, panels and project templates. Nothing here is logged:
//! the state travels in the snapshot and in ephemeral events.

use bitcode::{Decode, Encode};

/// A permission a plugin declares and the user grants. Each host API call
/// is re-checked against it right before its effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum Capability {
    /// Keep its own data (`storage.*`).
    Storage,
    /// Keep its own secrets in the system keychain (`secrets.*`).
    Secrets,
    /// Show notifications.
    Notifications,
    /// Read threads' titles, projects and status.
    ThreadsRead,
    /// Start agent threads with its own prompts.
    ThreadsCreate,
    /// Add agent CLIs: run the command lines its agent manifests name.
    Agents,
    /// See every prompt before it is sent, and block it.
    HooksPrompts,
    /// See every agent tool call (commands, file paths) and block it.
    HooksTools,
    /// Know when a thread's turn finishes.
    HooksThreads,
}

impl Capability {
    pub const ALL: [Capability; 9] = [
        Capability::Storage,
        Capability::Secrets,
        Capability::Notifications,
        Capability::ThreadsRead,
        Capability::ThreadsCreate,
        Capability::Agents,
        Capability::HooksPrompts,
        Capability::HooksTools,
        Capability::HooksThreads,
    ];

    /// The name manifests use (`threads.read`).
    pub fn key(self) -> &'static str {
        match self {
            Capability::Storage => "storage",
            Capability::Secrets => "secrets",
            Capability::Notifications => "notifications",
            Capability::ThreadsRead => "threads.read",
            Capability::ThreadsCreate => "threads.create",
            Capability::Agents => "agents",
            Capability::HooksPrompts => "hooks.prompts",
            Capability::HooksTools => "hooks.tools",
            Capability::HooksThreads => "hooks.threads",
        }
    }

    pub fn from_key(key: &str) -> Option<Capability> {
        Capability::ALL.into_iter().find(|c| c.key() == key)
    }

    /// What granting it allows, as the consent dialog says it.
    pub fn consent(self) -> &'static str {
        match self {
            Capability::Storage => "Keep its own data on the host",
            Capability::Secrets => "Keep its own secrets in the system keychain",
            Capability::Notifications => "Show notifications",
            Capability::ThreadsRead => "See your threads’ titles, projects and status",
            Capability::ThreadsCreate => "Start agent threads with its own prompts",
            Capability::Agents => "Run the agent command lines it defines",
            Capability::HooksPrompts => "Read every prompt before it is sent, and block it",
            Capability::HooksTools => "See every agent tool call, including commands and file paths, and block it",
            Capability::HooksThreads => "Know when a thread finishes a turn",
        }
    }

    /// Whether the dialog marks it as sensitive (reads what agents and the
    /// user do, or runs programs).
    pub fn sensitive(self) -> bool {
        matches!(self, Capability::Agents | Capability::HooksPrompts | Capability::HooksTools | Capability::ThreadsCreate)
    }
}

/// The most powerful kind of contribution a plugin has, in order of trust.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum PluginTier {
    /// Themes, snippets, slash commands, recipes, keybindings, templates.
    #[default]
    Data,
    /// Agent manifests: a command line Orbit runs.
    Agent,
    /// Declarative panels whose actions call the host API.
    Panel,
    /// Logic in a sandboxed WebAssembly module.
    Wasm,
}

impl PluginTier {
    pub fn label(self) -> &'static str {
        match self {
            PluginTier::Data => "Data",
            PluginTier::Agent => "Agent",
            PluginTier::Panel => "Panel",
            PluginTier::Wasm => "WebAssembly",
        }
    }
}

/// Whether an installed plugin runs, and if not, why.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum PluginStatus {
    #[default]
    Active,
    /// Turned off by the user.
    Disabled,
    /// Declares capabilities the user has not granted (a new install, or an
    /// update that asks for more).
    NeedsConsent,
    /// On the signed kill list; it stays off whatever the user chooses.
    Killed { reason: String },
    /// Its files no longer validate (a dev folder mid-edit, a damaged copy).
    Broken { error: String },
    /// Made for another version of Orbit.
    Incompatible { requires: String },
}

impl PluginStatus {
    pub fn is_active(&self) -> bool {
        matches!(self, PluginStatus::Active)
    }

    pub fn label(&self) -> &'static str {
        match self {
            PluginStatus::Active => "On",
            PluginStatus::Disabled => "Off",
            PluginStatus::NeedsConsent => "Needs approval",
            PluginStatus::Killed { .. } => "Blocked",
            PluginStatus::Broken { .. } => "Broken",
            PluginStatus::Incompatible { .. } => "Incompatible",
        }
    }
}

/// An installed plugin as clients see it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub publisher: String,
    pub homepage: String,
    pub tier: PluginTier,
    /// Where it came from, as shown: a folder, an archive, `url@commit`, a
    /// catalog.
    pub source: String,
    /// Loaded from its folder in place and reloaded when files change.
    pub dev: bool,
    pub status: PluginStatus,
    /// Declared, in manifest order.
    pub capabilities: Vec<Capability>,
    /// Granted by the user (a subset of `capabilities` while active).
    pub granted: Vec<Capability>,
    /// `blake3:<hex>` of the installed files.
    pub hash: String,
    pub installed_ms: u64,
    /// What it adds, for the plugin's row ("2 themes · 1 agent").
    pub summary: String,
    /// The settings it declares, with their values.
    pub settings: Vec<PluginSetting>,
    pub ui: PluginUi,
}

/// A setting a plugin declares, as the settings dialog shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginSetting {
    pub key: String,
    pub label: String,
    pub description: String,
    /// `text`, `toggle`, `number`, `choice` or `secret`.
    pub kind: String,
    pub choices: Vec<String>,
    /// The value; for a secret, empty, with `set` saying whether one is
    /// stored (secrets never leave the host).
    pub value: String,
    pub set: bool,
}

/// A plugin's contributions to the app, as data. Empty while the plugin is
/// not active.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginUi {
    /// GPUI Kit theme-set JSON documents, already validated.
    pub themes: Vec<String>,
    pub snippets: Vec<PluginSnippet>,
    pub slash_commands: Vec<PluginSlashCommand>,
    pub keybindings: Vec<PluginKeybinding>,
    pub commands: Vec<PluginCommandInfo>,
    pub panels: Vec<PluginPanel>,
    pub templates: Vec<PluginTemplate>,
    pub recipes: Vec<PluginRecipe>,
    /// Agent CLIs: their instance ids and how they run.
    pub agents: Vec<PluginAgent>,
    /// The plugin's storage (a JSON object) that panels bind to.
    pub storage: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginSnippet {
    pub name: String,
    pub description: String,
    pub text: String,
}

/// A slash command that expands to a prompt; `{{args}}` in the prompt is
/// what follows the command.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginSlashCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    pub prompt: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginKeybinding {
    /// Keystrokes as `keybindings.json` writes them (`mod-shift-r`).
    pub key: String,
    /// An Orbit action (`orbit::NewThread`) or one of the plugin's own
    /// commands (`plugin:<command id>`).
    pub command: String,
    /// A `when` context, as in `keybindings.json`; empty for anywhere.
    pub when: String,
}

/// An automation recipe for the gallery: what the editor is filled with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginRecipe {
    pub id: String,
    pub title: String,
    pub blurb: String,
    pub prompt: String,
    /// An RRULE (`FREQ=DAILY;BYHOUR=9`), or empty for manual runs.
    pub rule: String,
    /// Runs in a new worktree rather than the project folder.
    pub worktree: bool,
    pub plan_mode: bool,
    pub open_pr: bool,
}

/// What a palette command does.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum PluginCommandKind {
    /// Put this text in the composer.
    Prompt(String),
    /// Open the plugin's panel with this id.
    Panel(String),
    /// Start an agent CLI defined by the plugin in a terminal.
    Terminal(String),
    /// Call the plugin's WebAssembly `command` entry point.
    #[default]
    Wasm,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginCommandInfo {
    pub id: String,
    pub title: String,
    pub description: String,
    pub kind: PluginCommandKind,
}

/// A declarative panel: its component tree as JSON (see
/// `orbit_plugin::panel`), drawn with native components.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginPanel {
    pub id: String,
    pub title: String,
    pub tree: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginTemplate {
    pub name: String,
    pub description: String,
    /// An `orbit.toml` to start a project with.
    pub orbit_toml: String,
}

/// How an agent CLI from a manifest speaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum AgentProtocol {
    /// The Agent Client Protocol over standard input and output.
    #[default]
    Acp,
    /// Claude Code's `stream-json` protocol.
    StreamJson,
    /// A terminal program; status comes from hooks, OSC 7501 and heuristics.
    Terminal,
}

impl AgentProtocol {
    pub fn label(self) -> &'static str {
        match self {
            AgentProtocol::Acp => "ACP",
            AgentProtocol::StreamJson => "stream-json",
            AgentProtocol::Terminal => "Terminal",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginAgent {
    /// The provider instance id (chat agents) or the terminal command's id.
    pub id: String,
    pub name: String,
    pub protocol: AgentProtocol,
    /// The command line, for display and for terminal agents.
    pub command: String,
    pub args: Vec<String>,
}

/// A configured plugin catalog and what it lists.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct CatalogInfo {
    pub name: String,
    /// A URL or a file on the host.
    pub location: String,
    /// When it was last read, and the error if that failed.
    pub fetched_ms: u64,
    pub error: Option<String>,
    pub entries: Vec<CatalogEntry>,
}

/// One plugin release a catalog lists.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub publisher: String,
    pub categories: Vec<String>,
    pub tier: PluginTier,
    pub capabilities: Vec<Capability>,
    /// `blake3:<hex>` the installed files must hash to.
    pub hash: String,
}

/// One line of a plugin's log: host API calls (the audit trail), hook
/// decisions, its own log output and lifecycle changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginLogEntry {
    pub at_ms: u64,
    pub plugin: String,
    /// `audit`, `hook`, `log`, `lifecycle`.
    pub kind: String,
    /// The API method or hook (`storage.set`, `agent.tool.before`).
    pub call: String,
    /// `allowed`, `denied`, `error`, `info`.
    pub outcome: String,
    pub detail: String,
    /// Time spent, for calls into the plugin.
    pub micros: u64,
}

/// Everything clients know about plugins. In the snapshot and in
/// [`crate::Event::Plugins`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub struct PluginsState {
    pub plugins: Vec<PluginInfo>,
    pub catalogs: Vec<CatalogInfo>,
    /// Where secrets are kept: `keychain` or `file`.
    pub secrets: String,
}

impl PluginsState {
    pub fn plugin(&self, id: &str) -> Option<&PluginInfo> {
        self.plugins.iter().find(|p| p.id == id)
    }

    /// Active plugins.
    pub fn active(&self) -> impl Iterator<Item = &PluginInfo> {
        self.plugins.iter().filter(|p| p.status.is_active())
    }
}

/// Where to install a plugin from.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum InstallSource {
    /// A folder on the host. `dev` loads it in place and reloads it when
    /// its files change; otherwise it is copied.
    Folder { path: String, dev: bool },
    /// A `.orbitplugin` archive (a zip), sent whole.
    Archive { name: String, bytes: Vec<u8> },
    /// A git repository at a pinned commit (40 hex digits), optionally
    /// checked against a content hash.
    Git { url: String, commit: String, hash: Option<String> },
    /// A release listed by a configured catalog.
    Catalog { catalog: String, id: String },
}

/// Client → host.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum PluginCommand {
    /// Reads what a source holds without installing it, for the consent
    /// dialog: answers [`PluginReply::Preview`].
    Preview(InstallSource),
    /// Installs (or updates) and grants `grant`. Capabilities not granted
    /// leave the plugin needing approval.
    Install { source: InstallSource, grant: Vec<Capability> },
    Enable { id: String },
    Disable { id: String },
    /// Grants capabilities (the consent dialog's answer).
    Grant { id: String, grant: Vec<Capability> },
    Remove { id: String },
    /// Reloads a plugin (or every plugin) from disk.
    Reload { id: Option<String> },
    /// The newest log lines of one plugin, or of all.
    Logs { id: Option<String>, limit: u32 },
    /// Runs a palette command of a plugin.
    RunCommand { id: String, command: String },
    /// A panel's action: the action's index in the panel's tree order, and
    /// the panel's current input values (`bind` key → text).
    PanelAction { id: String, panel: String, action: String, inputs: Vec<(String, String)> },
    /// Adds or replaces a catalog: a URL or host file, and the minisign
    /// public key its listing must be signed with.
    AddCatalog { name: String, location: String, public_key: String },
    RemoveCatalog { name: String },
    /// Reads every catalog again.
    RefreshCatalogs,
    /// Writes a plugin's project template as the project's `orbit.toml`
    /// (refused when it already has one). Replies `Message`.
    ApplyTemplate { id: String, template: String, project: crate::ids::ProjectId },
    /// Reads or writes a plugin setting declared in its manifest. `None`
    /// reads.
    Setting { id: String, key: String, value: Option<String> },
}

/// Host → client, for [`PluginCommand`].
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum PluginReply {
    Done,
    /// What installing a source would add: the plugin as it would appear,
    /// and whether it replaces an installed version.
    Preview { plugin: Box<PluginInfo>, replaces: Option<String> },
    Installed(Box<PluginInfo>),
    Logs(Vec<PluginLogEntry>),
    /// A command's or action's result to show (may be empty).
    Message(String),
    /// A terminal agent's command line, to type into a new terminal.
    Terminal { command: String },
    Setting(String),
}
