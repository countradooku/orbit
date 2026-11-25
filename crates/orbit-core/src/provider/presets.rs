//! The provider instances Orbit knows without configuration: the built-in
//! instance of each runtime, and the ACP agents it can find on PATH.
//!
//! Claude Code and Codex are always listed (with an install hint when
//! missing). The others appear once their command is found, so nobody sees
//! a wall of "Not installed" entries. A configured instance with the same
//! id replaces a preset, and can disable it.

use orbit_proto::*;

/// A known instance, and whether it is listed even when not installed.
pub struct Preset {
    pub instance: ProviderInstance,
    pub always_listed: bool,
}

fn acp(id: &str, name: &str, binary: &str, args: &[&str]) -> Preset {
    Preset {
        instance: ProviderInstance {
            id: id.into(),
            kind: ProviderKind::Acp,
            name: name.into(),
            enabled: true,
            binary: binary.into(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            ..Default::default()
        },
        always_listed: false,
    }
}

/// Every preset, in picker order.
pub fn all() -> Vec<Preset> {
    let builtin = |kind, always_listed| Preset { instance: ProviderInstance::builtin(kind), always_listed };
    vec![
        builtin(ProviderKind::Claude, true),
        builtin(ProviderKind::Codex, true),
        builtin(ProviderKind::OpenCode, false),
        builtin(ProviderKind::Pi, false),
        acp("gemini", "Gemini CLI", "gemini", &["--experimental-acp"]),
        acp("qwen", "Qwen Code", "qwen", &["--experimental-acp"]),
        acp("grok", "Grok", "grok", &["agent", "stdio"]),
        acp("kiro", "Kiro", "kiro-cli", &["acp"]),
        acp("cursor", "Cursor", "cursor-agent", &["acp"]),
        acp("goose", "Goose", "goose", &["acp"]),
        acp("antigravity", "Antigravity", "agy", &["acp"]),
    ]
}

/// The effective instance list: presets overridden or extended by the
/// user's configured instances, disabled ones dropped. The flag says
/// whether the instance is listed before its probe finds it installed.
pub fn effective(configured: &[ProviderInstance]) -> Vec<(ProviderInstance, bool)> {
    let mut out: Vec<(ProviderInstance, bool)> = all()
        .into_iter()
        .map(|p| match configured.iter().find(|c| c.id == p.instance.id) {
            Some(c) => (c.clone(), true),
            None => (p.instance, p.always_listed),
        })
        .collect();
    for c in configured {
        if !out.iter().any(|(i, _)| i.id == c.id) {
            out.push((c.clone(), true));
        }
    }
    out.retain(|(i, _)| i.enabled);
    out
}

/// The instance with `id`, if it exists and is enabled.
pub fn find(configured: &[ProviderInstance], id: &str) -> Option<ProviderInstance> {
    effective(configured).into_iter().map(|(i, _)| i).find(|i| i.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_instances_override_and_extend_presets() {
        let work = ProviderInstance {
            id: "claude-work".into(),
            kind: ProviderKind::Claude,
            enabled: true,
            config_dir: "~/.claude-work".into(),
            ..Default::default()
        };
        let no_gemini = ProviderInstance { id: "gemini".into(), kind: ProviderKind::Acp, enabled: false, ..Default::default() };
        let list = effective(&[work.clone(), no_gemini]);
        assert!(list.iter().any(|(i, listed)| *i == work && *listed));
        assert!(!list.iter().any(|(i, _)| i.id == "gemini"), "a disabled preset is gone");
        assert!(list.iter().any(|(i, listed)| i.id == "claude" && *listed));
        assert!(list.iter().any(|(i, listed)| i.id == "qwen" && !*listed), "found only when installed");
        assert_eq!(find(&[], "grok").unwrap().args, ["agent", "stdio"]);
    }
}
