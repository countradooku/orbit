//! Agent manifests: a new agent CLI described as data.
//!
//! One generic mechanism instead of a module per agent. A manifest names
//! the binary, its arguments and how it speaks:
//!
//! * `acp`: the Agent Client Protocol (Gemini CLI, Goose, Kiro, Aider's
//!   ACP mode…), through Orbit's ACP adapter;
//! * `stream-json`: Claude Code's protocol, for Claude-compatible CLIs
//!   (they must accept Claude Code's flags);
//! * `terminal`: any interactive program, run in a terminal tab, with
//!   status from hooks, OSC 7501 and screen heuristics.
//!
//! Chat agents become [`ProviderInstance`]s, listed in the model picker
//! beside the built-in ones; their models and modes come from the agent
//! itself when it reports them, else from the manifest.

use std::collections::BTreeMap;

use orbit_proto::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManifest {
    /// The instance id: letters, digits, `-` and `_`, starting with a
    /// letter; unique across plugins and not a built-in agent's.
    pub id: String,
    pub name: String,
    /// `acp`, `stream-json` or `terminal`.
    pub protocol: String,
    /// The executable: a name looked up on PATH, or an absolute path.
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// How to install it, shown while it is missing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub install: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modes: Vec<ModeEntry>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context_window: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub default: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModeEntry {
    /// `build` or `plan`.
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl AgentManifest {
    pub fn protocol(&self) -> Option<AgentProtocol> {
        match self.protocol.as_str() {
            "acp" => Some(AgentProtocol::Acp),
            "stream-json" => Some(AgentProtocol::StreamJson),
            "terminal" => Some(AgentProtocol::Terminal),
            _ => None,
        }
    }

    /// What is wrong with it, if anything.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !is_instance_id(&self.id) {
            out.push("id must be letters, digits, “-” and “_”, starting with a letter.".into());
        }
        if crate::manifest::RESERVED_AGENT_IDS.contains(&self.id.as_str()) {
            out.push("id is a built-in agent’s.".into());
        }
        if self.name.trim().is_empty() {
            out.push("name is empty.".into());
        }
        if self.protocol().is_none() {
            out.push(format!("protocol “{}” must be acp, stream-json or terminal.", self.protocol));
        }
        if self.command.trim().is_empty() || self.command.contains(['\n', '\0']) {
            out.push("command must name an executable.".into());
        }
        if self.args.iter().any(|a| a.contains('\0')) || self.args.len() > 64 {
            out.push("args: at most 64, without NUL characters.".into());
        }
        for key in self.env.keys() {
            if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                out.push(format!("env name “{key}” must be letters, digits and “_”."));
            }
            if is_sensitive_env(key) {
                out.push(format!("env “{key}” changes how programs load and cannot be set by a plugin."));
            }
        }
        for mode in &self.modes {
            if !matches!(mode.id.as_str(), "build" | "plan") {
                out.push(format!("mode “{}” must be build or plan.", mode.id));
            }
        }
        if self.models.iter().filter(|m| m.default).count() > 1 {
            out.push("only one model can be the default.".into());
        }
        if self.models.iter().any(|m| m.id.trim().is_empty()) {
            out.push("a model has no id.".into());
        }
        out
    }

    /// The provider instance a chat agent runs as; `None` for terminal
    /// agents.
    pub fn instance(&self) -> Option<ProviderInstance> {
        let kind = match self.protocol()? {
            AgentProtocol::Acp => ProviderKind::Acp,
            AgentProtocol::StreamJson => ProviderKind::Claude,
            AgentProtocol::Terminal => return None,
        };
        Some(ProviderInstance {
            id: self.id.clone(),
            kind,
            name: self.name.clone(),
            enabled: true,
            binary: self.command.clone(),
            args: self.args.clone(),
            config_dir: String::new(),
            env: self.env.iter().map(|(name, value)| EnvVar { name: name.clone(), value: value.clone() }).collect(),
        })
    }

    /// The models and modes the manifest declares, for an agent that
    /// reports none of its own.
    pub fn catalog(&self) -> (Vec<ModelInfo>, Vec<ModeInfo>) {
        let models = self
            .models
            .iter()
            .map(|m| ModelInfo {
                slug: m.id.clone(),
                name: if m.name.is_empty() { m.id.clone() } else { m.name.clone() },
                description: (!m.description.is_empty()).then(|| m.description.clone()),
                context_window: m.context_window,
                is_default: m.default,
                options: Vec::new(),
            })
            .collect();
        let modes = self
            .modes
            .iter()
            .map(|m| ModeInfo {
                mode: if m.id == "plan" { Mode::Plan } else { Mode::Build },
                label: if m.label.is_empty() { if m.id == "plan" { "Plan".into() } else { "Build".into() } } else { m.label.clone() },
                description: (!m.description.is_empty()).then(|| m.description.clone()),
            })
            .collect();
        (models, modes)
    }

    /// The command line as one string, quoted where needed (display, and
    /// what a terminal agent types).
    pub fn command_line(&self) -> String {
        std::iter::once(&self.command).chain(&self.args).map(|w| shell_word(w)).collect::<Vec<_>>().join(" ")
    }

    pub fn summary(&self) -> PluginAgent {
        PluginAgent {
            id: self.id.clone(),
            name: self.name.clone(),
            protocol: self.protocol().unwrap_or_default(),
            command: self.command.clone(),
            args: self.args.clone(),
        }
    }
}

/// Variables that change how every program loads (preloads, interpreter
/// paths); a plugin may not set them for an agent.
fn is_sensitive_env(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.starts_with("LD_")
        || upper.starts_with("DYLD_")
        || matches!(upper.as_str(), "PATH" | "PATHEXT" | "COMSPEC" | "NODE_OPTIONS" | "PYTHONPATH" | "PYTHONSTARTUP" | "BASH_ENV" | "ENV" | "PROMPT_COMMAND")
}

/// A word as the terminal's shell reads it (PowerShell on Windows, a
/// POSIX shell elsewhere): bare when safe, else single-quoted.
pub fn shell_word(word: &str) -> String {
    let safe = !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@+,%".contains(c));
    if safe {
        word.to_owned()
    } else if cfg!(windows) {
        format!("'{}'", word.replace('\'', "''"))
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aider() -> AgentManifest {
        AgentManifest {
            id: "aider".into(),
            name: "Aider".into(),
            protocol: "acp".into(),
            command: "aider".into(),
            args: vec!["--acp".into()],
            models: vec![ModelEntry { id: "sonnet".into(), default: true, ..Default::default() }],
            modes: vec![ModeEntry { id: "plan".into(), ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn chat_agents_become_provider_instances() {
        let instance = aider().instance().unwrap();
        assert_eq!(instance.kind, ProviderKind::Acp);
        assert_eq!(instance.binary, "aider");
        assert_eq!(instance.args, ["--acp"]);
        let claude_like = AgentManifest { protocol: "stream-json".into(), ..aider() };
        assert_eq!(claude_like.instance().unwrap().kind, ProviderKind::Claude);
        let term = AgentManifest { protocol: "terminal".into(), ..aider() };
        assert!(term.instance().is_none());
        let (models, modes) = aider().catalog();
        assert_eq!(models[0].name, "sonnet");
        assert!(models[0].is_default);
        assert_eq!(modes[0].mode, Mode::Plan);
    }

    #[test]
    fn unsafe_manifests_are_refused() {
        assert!(aider().problems().is_empty());
        let reserved = AgentManifest { id: "claude".into(), ..aider() };
        assert!(reserved.problems().iter().any(|p| p.contains("built-in")));
        let mut env = aider();
        env.env.insert("LD_PRELOAD".into(), "/tmp/x.so".into());
        assert!(env.problems().iter().any(|p| p.contains("LD_PRELOAD")));
        let proto = AgentManifest { protocol: "http".into(), ..aider() };
        assert!(proto.problems().iter().any(|p| p.contains("protocol")));
    }

    #[test]
    fn command_lines_quote_what_they_must() {
        let a = AgentManifest { args: vec!["--message".into(), "it's here".into()], ..aider() };
        let quoted = if cfg!(windows) { "'it''s here'" } else { "'it'\''s here'" };
        assert_eq!(a.command_line(), format!("aider --message {quoted}"));
    }
}
