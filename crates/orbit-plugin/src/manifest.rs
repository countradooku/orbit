//! `orbit-plugin.toml`: the manifest, parsed strictly.
//!
//! Unknown keys are refused at every level, so a typo is an error rather
//! than a silently ignored setting, and a manifest written for a newer
//! Orbit says so instead of half-working. `engines.orbit` is the only
//! version gate. The published JSON Schema (`docs/orbit-plugin.schema.json`)
//! describes the same shape; a test keeps the two in step.

use std::collections::{BTreeMap, HashSet};

use orbit_proto::{Capability, PluginTier};
use serde::{Deserialize, Serialize};

use crate::agent::AgentManifest;

/// The manifest's file name at the plugin's root.
pub const MANIFEST: &str = "orbit-plugin.toml";

/// Per-plugin caps on contributions and files.
pub mod limits {
    pub const THEMES: usize = 32;
    pub const SNIPPETS: usize = 256;
    pub const SLASH_COMMANDS: usize = 64;
    pub const RECIPES: usize = 32;
    pub const KEYBINDINGS: usize = 128;
    pub const TEMPLATES: usize = 32;
    pub const AGENTS: usize = 16;
    pub const PANELS: usize = 16;
    pub const COMMANDS: usize = 64;
    pub const SETTINGS: usize = 64;
    pub const HOOKS: usize = 4;
    /// The manifest itself.
    pub const MANIFEST_BYTES: usize = 64 * 1024;
    /// A theme, panel, snippet, prompt or template file.
    pub const TEXT_FILE_BYTES: usize = 256 * 1024;
    /// The WebAssembly module.
    pub const WASM_BYTES: usize = 8 * 1024 * 1024;
    /// Everything in the plugin's folder.
    pub const PLUGIN_BYTES: u64 = 32 * 1024 * 1024;
    pub const PLUGIN_FILES: usize = 512;
    /// Fuel (roughly, WebAssembly instructions) per call: the default and
    /// the most a manifest may ask for.
    pub const FUEL_DEFAULT: u64 = 20_000_000;
    pub const FUEL_MAX: u64 = 500_000_000;
    /// Linear memory, in MiB.
    pub const MEMORY_MB_DEFAULT: u32 = 16;
    pub const MEMORY_MB_MAX: u32 = 128;
    /// Wall-clock budget per call, in milliseconds.
    pub const TIMEOUT_MS_DEFAULT: u32 = 250;
    pub const TIMEOUT_MS_MAX: u32 = 5_000;
}

/// Hooks a WebAssembly plugin can register.
pub const HOOKS: &[(&str, Capability)] = &[
    ("agent.prompt.before", Capability::HooksPrompts),
    ("agent.tool.before", Capability::HooksTools),
    ("agent.tool.after", Capability::HooksTools),
    ("thread.finished", Capability::HooksThreads),
];

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Kebab-case, unique among installed plugins: `midnight-theme`.
    pub id: String,
    pub name: String,
    /// Semantic version of the plugin.
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub publisher: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub license: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub homepage: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    pub engines: Engines,
    /// Capability names (`storage`, `hooks.tools`…).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub themes: Vec<ThemeEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snippets: Vec<SnippetEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slash_commands: Vec<SlashCommandEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipes: Vec<RecipeEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keybindings: Vec<KeybindingEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub templates: Vec<TemplateEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentManifest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panels: Vec<PanelEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CommandEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wasm: Option<WasmEntry>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Engines {
    /// A semver requirement on Orbit's version: `>=0.1.0`.
    pub orbit: String,
}

/// A GPUI Kit theme-set JSON file.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeEntry {
    pub path: String,
}

/// Text to insert, inline or from a file.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnippetEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// `/name args` in the composer becomes `prompt` with `{{args}}` replaced.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SlashCommandEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub argument_hint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeEntry {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub blurb: String,
    pub prompt: String,
    /// An RRULE; empty for manual runs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub schedule: String,
    /// `worktree` (default) or `project`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// `build` (default) or `plan`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub open_pr: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KeybindingEntry {
    pub key: String,
    /// `orbit::<Action>` or `plugin:<command id>`.
    pub command: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub when: String,
}

/// A project template: an `orbit.toml` to start from.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub path: String,
}

/// A declarative panel: a component tree in TOML or JSON.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PanelEntry {
    pub id: String,
    pub title: String,
    pub path: String,
}

/// A palette command. Exactly one of `prompt`, `panel`, `agent` or `wasm`
/// says what it does.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandEntry {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Puts this text in the composer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Opens this panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<String>,
    /// Starts this terminal agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Calls the module's `orbit_command` with the command's id.
    #[serde(default, skip_serializing_if = "is_false")]
    pub wasm: bool,
}

/// A setting the user edits on the plugin's page.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettingEntry {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// `text`, `toggle`, `number`, `choice` or `secret`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WasmEntry {
    /// The `.wasm` file, relative to the plugin's root.
    pub module: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<String>,
    /// What a failed or slow hook means: `allow` (default) or `deny`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub on_error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuel: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u32>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Built-in agent and runtime ids a plugin agent may not take.
pub const RESERVED_AGENT_IDS: &[&str] = &[
    "claude", "codex", "acp", "opencode", "pi", "gemini", "qwen", "grok", "kiro", "cursor", "goose", "antigravity",
];

/// Plugin ids that are refused, so nothing can pose as part of Orbit.
pub const RESERVED_IDS: &[&str] = &["orbit", "builtin", "official", "system", "core"];

/// What a WebAssembly module's failure means for a hook.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnError {
    #[default]
    Allow,
    Deny,
}

/// A manifest that passed validation, with its names resolved to types.
#[derive(Clone, Debug, PartialEq)]
pub struct Validated {
    pub manifest: Manifest,
    pub version: semver::Version,
    pub capabilities: Vec<Capability>,
    pub tier: PluginTier,
    pub hooks: Vec<String>,
    pub on_error: OnError,
    /// `engines.orbit` does not admit this Orbit: the requirement.
    pub incompatible: Option<String>,
}

impl Validated {
    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn fuel(&self) -> u64 {
        self.manifest.wasm.as_ref().and_then(|w| w.fuel).unwrap_or(limits::FUEL_DEFAULT)
    }

    pub fn memory_bytes(&self) -> usize {
        self.manifest.wasm.as_ref().and_then(|w| w.memory_mb).unwrap_or(limits::MEMORY_MB_DEFAULT) as usize * 1024 * 1024
    }

    pub fn timeout_ms(&self) -> u32 {
        self.manifest.wasm.as_ref().and_then(|w| w.timeout_ms).unwrap_or(limits::TIMEOUT_MS_DEFAULT)
    }
}

impl Manifest {
    /// Parses `orbit-plugin.toml`, refusing unknown keys, then validates.
    pub fn parse(text: &str) -> Result<Validated, Vec<String>> {
        if text.len() > limits::MANIFEST_BYTES {
            return Err(vec![format!("{MANIFEST} is larger than {} KiB.", limits::MANIFEST_BYTES / 1024)]);
        }
        let manifest: Manifest = toml::from_str(text).map_err(|e| vec![toml_error(&e)])?;
        manifest.validate()
    }

    /// Every problem with the manifest, or its validated form.
    pub fn validate(self) -> Result<Validated, Vec<String>> {
        let mut problems = Vec::new();
        let mut bad = |text: String| problems.push(text);

        if !is_plugin_id(&self.id) {
            bad(format!(
                "id “{}” must be 3 to 64 lowercase letters, digits and single hyphens, starting with a letter.",
                self.id
            ));
        } else if RESERVED_IDS.contains(&self.id.as_str()) || self.id.starts_with("orbit-") {
            bad(format!("id “{}” is reserved for Orbit itself.", self.id));
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 64 {
            bad("name must be 1 to 64 characters.".into());
        }
        let version = semver::Version::parse(&self.version);
        if version.is_err() {
            bad(format!("version “{}” is not a semantic version such as 1.2.0.", self.version));
        }
        if self.description.chars().count() > 300 {
            bad("description must be at most 300 characters.".into());
        }
        if !self.homepage.is_empty() && !self.homepage.starts_with("https://") {
            bad("homepage must be an https:// address.".into());
        }
        let incompatible = match semver::VersionReq::parse(&self.engines.orbit) {
            Ok(req) => {
                let orbit = semver::Version::parse(crate::ORBIT_VERSION).expect("Orbit's own version");
                (!req.matches(&orbit)).then(|| self.engines.orbit.clone())
            }
            Err(_) => {
                bad(format!("engines.orbit “{}” is not a version requirement such as >=0.1.0.", self.engines.orbit));
                None
            }
        };

        let mut capabilities = Vec::new();
        for key in &self.capabilities {
            match Capability::from_key(key) {
                Some(c) if capabilities.contains(&c) => bad(format!("capability “{key}” is listed twice.")),
                Some(c) => capabilities.push(c),
                None => bad(format!(
                    "unknown capability “{key}”; known: {}.",
                    Capability::ALL.iter().map(|c| c.key()).collect::<Vec<_>>().join(", ")
                )),
            }
        }

        let counts = [
            ("themes", self.themes.len(), limits::THEMES),
            ("snippets", self.snippets.len(), limits::SNIPPETS),
            ("slash_commands", self.slash_commands.len(), limits::SLASH_COMMANDS),
            ("recipes", self.recipes.len(), limits::RECIPES),
            ("keybindings", self.keybindings.len(), limits::KEYBINDINGS),
            ("templates", self.templates.len(), limits::TEMPLATES),
            ("agents", self.agents.len(), limits::AGENTS),
            ("panels", self.panels.len(), limits::PANELS),
            ("commands", self.commands.len(), limits::COMMANDS),
            ("settings", self.settings.len(), limits::SETTINGS),
        ];
        for (what, n, max) in counts {
            if n > max {
                bad(format!("at most {max} {what} (found {n})."));
            }
        }

        let mut paths: Vec<(&str, &str)> = Vec::new();
        paths.extend(self.themes.iter().map(|t| ("themes.path", t.path.as_str())));
        paths.extend(self.templates.iter().map(|t| ("templates.path", t.path.as_str())));
        paths.extend(self.panels.iter().map(|p| ("panels.path", p.path.as_str())));
        paths.extend(self.snippets.iter().filter_map(|s| s.path.as_deref()).map(|p| ("snippets.path", p)));
        paths.extend(self.slash_commands.iter().filter_map(|s| s.path.as_deref()).map(|p| ("slash_commands.path", p)));
        paths.extend(self.wasm.iter().map(|w| ("wasm.module", w.module.as_str())));
        for (what, path) in paths {
            if !is_relative_path(path) {
                bad(format!("{what} “{path}” must be a relative path inside the plugin, with forward slashes."));
            }
        }

        for s in &self.snippets {
            if s.name.trim().is_empty() {
                bad("a snippet has no name.".into());
            }
            if s.text.is_some() == s.path.is_some() {
                bad(format!("snippet “{}” needs exactly one of text or path.", s.name));
            }
        }
        unique(self.snippets.iter().map(|s| s.name.as_str()), "snippet", &mut bad);
        for s in &self.slash_commands {
            if !is_slug(&s.name) {
                bad(format!("slash command “{}” must be lowercase letters, digits and hyphens.", s.name));
            }
            if s.prompt.is_some() == s.path.is_some() {
                bad(format!("slash command “{}” needs exactly one of prompt or path.", s.name));
            }
        }
        unique(self.slash_commands.iter().map(|s| s.name.as_str()), "slash command", &mut bad);
        for r in &self.recipes {
            if !is_slug(&r.id) {
                bad(format!("recipe id “{}” must be lowercase letters, digits and hyphens.", r.id));
            }
            if r.title.trim().is_empty() || r.prompt.trim().is_empty() {
                bad(format!("recipe “{}” needs a title and a prompt.", r.id));
            }
            if !matches!(r.target.as_str(), "" | "worktree" | "project") {
                bad(format!("recipe “{}”: target must be worktree or project.", r.id));
            }
            if !matches!(r.mode.as_str(), "" | "build" | "plan") {
                bad(format!("recipe “{}”: mode must be build or plan.", r.id));
            }
            if !r.schedule.is_empty() && !r.schedule.to_ascii_uppercase().contains("FREQ=") {
                bad(format!("recipe “{}”: schedule must be an RRULE such as FREQ=DAILY;BYHOUR=9.", r.id));
            }
        }
        unique(self.recipes.iter().map(|r| r.id.as_str()), "recipe", &mut bad);
        for t in &self.templates {
            if t.name.trim().is_empty() {
                bad("a template has no name.".into());
            }
        }

        let mut agent_ids = HashSet::new();
        for agent in &self.agents {
            for problem in agent.problems() {
                bad(format!("agent “{}”: {problem}", agent.id));
            }
            if !agent_ids.insert(agent.id.as_str()) {
                bad(format!("agent “{}” is defined twice.", agent.id));
            }
        }
        if !self.agents.is_empty() && !capabilities.contains(&Capability::Agents) {
            bad("agents need the “agents” capability: the consent dialog shows their command lines.".into());
        }

        for p in &self.panels {
            if !is_slug(&p.id) {
                bad(format!("panel id “{}” must be lowercase letters, digits and hyphens.", p.id));
            }
            if p.title.trim().is_empty() {
                bad(format!("panel “{}” has no title.", p.id));
            }
        }
        unique(self.panels.iter().map(|p| p.id.as_str()), "panel", &mut bad);

        for c in &self.commands {
            if !is_slug(&c.id) {
                bad(format!("command id “{}” must be lowercase letters, digits and hyphens.", c.id));
            }
            if c.title.trim().is_empty() {
                bad(format!("command “{}” has no title.", c.id));
            }
            let actions =
                usize::from(c.prompt.is_some()) + usize::from(c.panel.is_some()) + usize::from(c.agent.is_some()) + usize::from(c.wasm);
            if actions != 1 {
                bad(format!("command “{}” needs exactly one of prompt, panel, agent or wasm.", c.id));
            }
            if let Some(panel) = &c.panel
                && !self.panels.iter().any(|p| &p.id == panel)
            {
                bad(format!("command “{}” opens panel “{panel}”, which the plugin does not define.", c.id));
            }
            if let Some(agent) = &c.agent
                && !self.agents.iter().any(|a| &a.id == agent && a.protocol == "terminal")
            {
                bad(format!("command “{}” starts “{agent}”, which is not one of the plugin’s terminal agents.", c.id));
            }
            if c.wasm && self.wasm.is_none() {
                bad(format!("command “{}” calls WebAssembly, but the plugin has no [wasm] module.", c.id));
            }
        }
        unique(self.commands.iter().map(|c| c.id.as_str()), "command", &mut bad);

        for k in &self.keybindings {
            if k.key.trim().is_empty() {
                bad("a keybinding has no key.".into());
            }
            match k.command.strip_prefix("plugin:") {
                Some(id) if !self.commands.iter().any(|c| c.id == id) => {
                    bad(format!("keybinding {} runs “{id}”, which the plugin does not define.", k.key))
                }
                Some(_) => {}
                None if k.command.starts_with("orbit::") => {}
                None => bad(format!("keybinding {}: command must be orbit::<Action> or plugin:<command id>.", k.key)),
            }
        }

        for s in &self.settings {
            if !is_setting_key(&s.key) {
                bad(format!("setting key “{}” must be letters, digits, “_”, “-” and “.”.", s.key));
            }
            if !matches!(s.kind.as_str(), "text" | "toggle" | "number" | "choice" | "secret") {
                bad(format!("setting “{}”: type must be text, toggle, number, choice or secret.", s.key));
            }
            if s.kind == "choice" && s.choices.is_empty() {
                bad(format!("setting “{}”: a choice needs choices.", s.key));
            }
            if s.kind == "secret" && !s.default.is_empty() {
                bad(format!("setting “{}”: a secret cannot have a default.", s.key));
            }
            if s.kind == "secret" && !capabilities.contains(&Capability::Secrets) {
                bad(format!("setting “{}” is a secret: it needs the “secrets” capability.", s.key));
            }
            if let Err(e) = check_setting(s, &s.default).map(|_| ()).or_else(|e| if s.default.is_empty() { Ok(()) } else { Err(e) }) {
                bad(format!("setting “{}”: default {e}", s.key));
            }
        }
        unique(self.settings.iter().map(|s| s.key.as_str()), "setting", &mut bad);

        let mut hooks = Vec::new();
        let mut on_error = OnError::Allow;
        if let Some(wasm) = &self.wasm {
            if wasm.hooks.len() > limits::HOOKS {
                bad(format!("at most {} hooks.", limits::HOOKS));
            }
            for hook in &wasm.hooks {
                match HOOKS.iter().find(|(name, _)| name == hook) {
                    Some((_, cap)) if !capabilities.contains(cap) => {
                        bad(format!("hook “{hook}” needs the “{}” capability.", cap.key()))
                    }
                    Some(_) if hooks.contains(hook) => bad(format!("hook “{hook}” is listed twice.")),
                    Some(_) => hooks.push(hook.clone()),
                    None => bad(format!(
                        "unknown hook “{hook}”; known: {}.",
                        HOOKS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
                    )),
                }
            }
            on_error = match wasm.on_error.as_str() {
                "" | "allow" => OnError::Allow,
                "deny" => OnError::Deny,
                other => {
                    bad(format!("wasm.on_error “{other}” must be allow or deny."));
                    OnError::Allow
                }
            };
            if wasm.fuel.is_some_and(|f| f == 0 || f > limits::FUEL_MAX) {
                bad(format!("wasm.fuel must be between 1 and {}.", limits::FUEL_MAX));
            }
            if wasm.memory_mb.is_some_and(|m| m == 0 || m > limits::MEMORY_MB_MAX) {
                bad(format!("wasm.memory_mb must be between 1 and {}.", limits::MEMORY_MB_MAX));
            }
            if wasm.timeout_ms.is_some_and(|t| t == 0 || t > limits::TIMEOUT_MS_MAX) {
                bad(format!("wasm.timeout_ms must be between 1 and {}.", limits::TIMEOUT_MS_MAX));
            }
        }
        if hooks.is_empty() && capabilities.iter().any(|c| matches!(c, Capability::HooksPrompts | Capability::HooksTools | Capability::HooksThreads)) {
            bad("a hooks.* capability is declared, but [wasm] registers no hook that uses it.".into());
        }

        if !problems.is_empty() {
            return Err(problems);
        }
        let tier = if self.wasm.is_some() {
            PluginTier::Wasm
        } else if !self.panels.is_empty() {
            PluginTier::Panel
        } else if !self.agents.is_empty() {
            PluginTier::Agent
        } else {
            PluginTier::Data
        };
        Ok(Validated {
            version: version.expect("checked above"),
            capabilities,
            tier,
            hooks,
            on_error,
            incompatible,
            manifest: self,
        })
    }

    /// The declared setting `key`.
    pub fn setting(&self, key: &str) -> Option<&SettingEntry> {
        self.settings.iter().find(|s| s.key == key)
    }
}

/// Checks a setting value against its declaration; returns it normalized.
pub fn check_setting(entry: &SettingEntry, value: &str) -> Result<String, String> {
    match entry.kind.as_str() {
        "toggle" => match value {
            "true" | "on" | "yes" => Ok("true".into()),
            "false" | "off" | "no" => Ok("false".into()),
            _ => Err("must be true or false.".into()),
        },
        "number" => value.trim().parse::<f64>().map(|n| n.to_string()).map_err(|_| "must be a number.".into()),
        "choice" => {
            if entry.choices.iter().any(|c| c == value) {
                Ok(value.to_owned())
            } else {
                Err(format!("must be one of {}.", entry.choices.join(", ")))
            }
        }
        _ if value.len() > 16 * 1024 => Err("must be at most 16 KiB.".into()),
        _ => Ok(value.to_owned()),
    }
}

/// A TOML error as one line with its position.
fn toml_error(e: &toml::de::Error) -> String {
    let message = e.message().trim_end_matches('.').to_owned();
    match e.span() {
        Some(span) => format!("{MANIFEST}: {message} (at byte {}).", span.start),
        None => format!("{MANIFEST}: {message}."),
    }
}

fn unique<'a>(names: impl Iterator<Item = &'a str>, what: &str, bad: &mut impl FnMut(String)) {
    let mut seen = HashSet::new();
    for name in names {
        if !seen.insert(name) {
            bad(format!("{what} “{name}” is defined twice."));
        }
    }
}

/// `midnight-theme`: 3 to 64 chars of `a-z`, `0-9` and single hyphens,
/// starting with a letter and not ending with a hyphen.
pub fn is_plugin_id(id: &str) -> bool {
    (3..=64).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && !id.ends_with('-')
        && !id.contains("--")
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Lowercase letters, digits and hyphens, 1 to 64 chars.
pub fn is_slug(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_setting_key(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// A path inside the plugin: relative, forward slashes, no `..`, no
/// drive letters or empty segments.
pub fn is_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 255
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && path.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// A starting manifest for `orbit plugin new`.
pub fn scaffold(id: &str, name: &str) -> String {
    format!(
        r#"# Orbit plugin manifest. Reference: docs/PLUGINS.md; schema: docs/orbit-plugin.schema.json.
id = "{id}"
name = "{name}"
version = "0.1.0"
description = ""
engines = {{ orbit = ">={orbit}" }}
capabilities = []

# [[snippets]]
# name = "Review checklist"
# text = "Review this change for correctness, tests and naming."

# [[slash_commands]]
# name = "explain"
# description = "Explain a file"
# prompt = "Explain {{{{args}}}} to a new contributor."
"#,
        orbit = crate::ORBIT_VERSION
    )
}

/// A plugin's declared settings as `key → default`.
pub fn setting_defaults(manifest: &Manifest) -> BTreeMap<String, String> {
    manifest.settings.iter().filter(|s| s.kind != "secret").map(|s| (s.key.clone(), s.default.clone())).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const MINIMAL: &str = r#"
id = "midnight"
name = "Midnight"
version = "1.0.0"
engines = { orbit = ">=0.1.0" }
"#;

    #[test]
    fn a_minimal_manifest_is_a_data_plugin() {
        let v = Manifest::parse(MINIMAL).unwrap();
        assert_eq!(v.id(), "midnight");
        assert_eq!(v.tier, PluginTier::Data);
        assert!(v.capabilities.is_empty());
        assert_eq!(v.incompatible, None);
    }

    #[test]
    fn unknown_keys_are_refused_at_every_level() {
        let top = format!("{MINIMAL}\ncolour = \"blue\"\n");
        let err = Manifest::parse(&top).unwrap_err().join("\n");
        assert!(err.contains("unknown field `colour`"), "{err}");

        let nested = format!("{MINIMAL}\n[[snippets]]\nname = \"a\"\ntext = \"b\"\ntitle = \"c\"\n");
        let err = Manifest::parse(&nested).unwrap_err().join("\n");
        assert!(err.contains("unknown field `title`"), "{err}");

        let engines = MINIMAL.replace("engines = { orbit = \">=0.1.0\" }", "engines = { orbit = \">=0.1.0\", node = \"20\" }");
        assert!(Manifest::parse(&engines).unwrap_err().join("\n").contains("unknown field `node`"));

        let agent = format!(
            "{MINIMAL}\ncapabilities = [\"agents\"]\n[[agents]]\nid = \"aider\"\nname = \"Aider\"\nprotocol = \"acp\"\ncommand = \"aider\"\nshell = true\n"
        );
        assert!(Manifest::parse(&agent).unwrap_err().join("\n").contains("unknown field `shell`"));
    }

    #[test]
    fn bad_values_are_all_reported() {
        let text = r#"
id = "Bad_Id"
name = ""
version = "one"
engines = { orbit = "soon" }
capabilities = ["storage", "storage", "network"]
[[themes]]
path = "../escape.json"
[[commands]]
id = "go"
title = "Go"
"#;
        let problems = Manifest::parse(text).unwrap_err();
        let all = problems.join("\n");
        for needle in ["id “Bad_Id”", "name must", "version “one”", "engines.orbit", "listed twice", "unknown capability “network”", "../escape.json", "exactly one of prompt"] {
            assert!(all.contains(needle), "missing {needle} in:\n{all}");
        }
    }

    #[test]
    fn reserved_ids_and_newer_orbits() {
        let orbit = MINIMAL.replace("\"midnight\"", "\"orbit-tools\"");
        assert!(Manifest::parse(&orbit).unwrap_err()[0].contains("reserved"));
        let future = MINIMAL.replace(">=0.1.0", ">=99.0.0");
        assert_eq!(Manifest::parse(&future).unwrap().incompatible.as_deref(), Some(">=99.0.0"));
    }

    #[test]
    fn hooks_need_their_capability_and_a_module() {
        let text = format!("{MINIMAL}\n[wasm]\nmodule = \"guard.wasm\"\nhooks = [\"agent.tool.before\"]\n");
        assert!(Manifest::parse(&text).unwrap_err()[0].contains("hooks.tools"));
        let ok = format!("{MINIMAL}\ncapabilities = [\"hooks.tools\"]\n[wasm]\nmodule = \"guard.wasm\"\nhooks = [\"agent.tool.before\"]\non_error = \"deny\"\n");
        let v = Manifest::parse(&ok).unwrap();
        assert_eq!(v.tier, PluginTier::Wasm);
        assert_eq!(v.on_error, OnError::Deny);
        assert_eq!(v.hooks, ["agent.tool.before"]);
        let unused = format!("{MINIMAL}\ncapabilities = [\"hooks.prompts\"]\n");
        assert!(Manifest::parse(&unused).unwrap_err()[0].contains("registers no hook"));
    }

    #[test]
    fn agents_need_consent_to_run_programs() {
        let text = format!("{MINIMAL}\n[[agents]]\nid = \"aider\"\nname = \"Aider\"\nprotocol = \"acp\"\ncommand = \"aider\"\n");
        assert!(Manifest::parse(&text).unwrap_err()[0].contains("“agents” capability"));
    }

    #[test]
    fn paths_stay_inside_the_plugin() {
        for good in ["themes/a.json", "a.wasm"] {
            assert!(is_relative_path(good), "{good}");
        }
        for bad in ["", "/etc/passwd", "../x", "a/../b", "C:/x", "a\\b", "a//b", "./a"] {
            assert!(!is_relative_path(bad), "{bad}");
        }
    }

    #[test]
    fn the_scaffold_validates() {
        Manifest::parse(&scaffold("my-plugin", "My plugin")).unwrap();
    }

    #[test]
    fn settings_are_typed() {
        let entry = SettingEntry { key: "n".into(), label: "N".into(), kind: "number".into(), ..Default::default() };
        assert_eq!(check_setting(&entry, "3").unwrap(), "3");
        assert!(check_setting(&entry, "x").is_err());
        let choice = SettingEntry { kind: "choice".into(), choices: vec!["a".into(), "b".into()], ..entry.clone() };
        assert!(check_setting(&choice, "c").is_err());
        let toggle = SettingEntry { kind: "toggle".into(), ..entry };
        assert_eq!(check_setting(&toggle, "on").unwrap(), "true");
    }
}
