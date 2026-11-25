//! Plugins in the app: what the shown host's plugins add to this window.
//!
//! Plugins are installed and run on the host (`orbit_plugin::host`, in the
//! engine); their UI contributions arrive as data in
//! [`orbit_proto::PluginsState`]. Here they become:
//!
//! * themes in the Appearance pickers ([`theme`]);
//! * keybindings, between Orbit's defaults and the user's file;
//! * palette commands, snippets and slash commands that expand to prompts;
//! * panels in the plugin dock beside the work area ([`dock`]);
//! * the Settings › Plugins page with its browser and consent dialog
//!   ([`page`], [`consent`]).

pub mod consent;
pub mod dock;
pub mod page;
pub mod panel;
pub mod settings;

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::component::{Icon, IconName, ThemeConfig, ThemeSet};
use gpui_kit::*;
use orbit_proto::*;

use crate::assets::Lucide;

/// Runs a plugin's palette command.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = plugin, no_json)]
pub struct RunPluginCommand {
    pub plugin: String,
    pub command: String,
}

/// Opens a plugin's panel in the plugin dock.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = plugin, no_json)]
pub struct OpenPluginPanel {
    pub plugin: String,
    pub panel: String,
}

/// Puts a plugin's snippet in the composer.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = plugin, no_json)]
pub struct InsertSnippet {
    pub plugin: String,
    pub name: String,
}

/// Writes a plugin's project template as the current project's
/// `orbit.toml`.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = plugin, no_json)]
pub struct ApplyTemplate {
    pub plugin: String,
    pub template: String,
}

gpui_kit::actions!(
    plugin,
    [
        /// Opens Settings › Plugins.
        ManagePlugins,
    ]
);

/// The shown host's plugins, as last received, and the themes they add.
#[derive(Default)]
pub struct Plugins {
    pub state: PluginsState,
    themes: HashMap<SharedString, Rc<ThemeConfig>>,
}

impl Global for Plugins {}

/// The plugins state, as last received.
pub fn state(cx: &App) -> &PluginsState {
    static EMPTY: std::sync::OnceLock<PluginsState> = std::sync::OnceLock::new();
    cx.try_global::<Plugins>().map(|p| &p.state).unwrap_or_else(|| EMPTY.get_or_init(PluginsState::default))
}

/// A theme a plugin adds, by name.
pub fn theme(name: &str, cx: &App) -> Option<Rc<ThemeConfig>> {
    cx.try_global::<Plugins>()?.themes.get(name).cloned()
}

/// Every plugin theme.
pub fn themes(cx: &App) -> Vec<Rc<ThemeConfig>> {
    cx.try_global::<Plugins>().map(|p| p.themes.values().cloned().collect()).unwrap_or_default()
}

/// Takes in a new plugins state (a snapshot, or a change on the host).
pub fn receive(state: &PluginsState, cx: &mut App) {
    if cx.try_global::<Plugins>().is_some_and(|p| p.state == *state) {
        return;
    }
    let mut themes: HashMap<SharedString, Rc<ThemeConfig>> = HashMap::new();
    for plugin in state.active() {
        for json in &plugin.ui.themes {
            match sonic_rs::from_str::<ThemeSet>(json) {
                Ok(set) => {
                    for theme in set.themes {
                        // Bundled themes keep their names; the first plugin wins a tie.
                        let bundled = cx.try_global::<gpui_kit::component::ThemeRegistry>().is_some_and(|r| r.themes().contains_key(&theme.name));
                        if bundled {
                            log::warn!("plugins: {} names a theme that exists (“{}”); skipped", plugin.id, theme.name);
                            continue;
                        }
                        themes.entry(theme.name.clone()).or_insert_with(|| Rc::new(theme));
                    }
                }
                Err(e) => log::warn!("plugins: {}’s theme doesn’t load: {e}", plugin.id),
            }
        }
    }
    let themes_changed = cx.try_global::<Plugins>().map(|p| {
        let mut a: Vec<&SharedString> = p.themes.keys().collect();
        let mut b: Vec<&SharedString> = themes.keys().collect();
        a.sort();
        b.sort();
        a != b || p.state.plugins.iter().flat_map(|x| &x.ui.themes).ne(state.plugins.iter().flat_map(|x| &x.ui.themes))
    });
    // Agents from plugins, for the screens that list accounts in this
    // process (the engine has its own copy).
    let agents: Vec<ProviderInstance> = state
        .active()
        .flat_map(|p| p.ui.agents.iter())
        .filter_map(|a| {
            let kind = match a.protocol {
                AgentProtocol::Acp => ProviderKind::Acp,
                AgentProtocol::StreamJson => ProviderKind::Claude,
                AgentProtocol::Terminal => return None,
            };
            Some(ProviderInstance {
                id: a.id.clone(),
                kind,
                name: a.name.clone(),
                enabled: true,
                binary: a.command.clone(),
                args: a.args.clone(),
                ..Default::default()
            })
        })
        .collect();
    orbit_core::provider::presets::set_plugin_agents(agents);
    let bindings = keybindings(state, cx);
    cx.set_global(Plugins { state: state.clone(), themes });
    crate::keymap::set_plugin_bindings(bindings, cx);
    // (Without a theme system, as in tests that don't draw, nothing to apply.)
    if themes_changed.unwrap_or(true) && cx.has_global::<gpui_kit::component::ThemeRegistry>() {
        let appearance = crate::settings::get(cx).appearance.clone();
        crate::theme::apply(&appearance, None, cx);
    }
    cx.refresh_windows();
}

/// The active plugins' keybindings, resolved into bindings; rules that
/// don't resolve are logged and skipped.
fn keybindings(state: &PluginsState, cx: &App) -> Vec<KeyBinding> {
    let mut out = Vec::new();
    for plugin in state.active() {
        for k in &plugin.ui.keybindings {
            let action: Box<dyn Action> = match k.command.strip_prefix("plugin:") {
                Some(command) => Box::new(RunPluginCommand { plugin: plugin.id.clone(), command: command.to_owned() }),
                None => match cx.build_action(&k.command, None) {
                    Ok(action) => action,
                    Err(_) => {
                        log::warn!("plugins: {}’s keybinding {} names an unknown command {}", plugin.id, k.key, k.command);
                        continue;
                    }
                },
            };
            let context = match (!k.when.is_empty()).then(|| crate::keymap::check_when(&k.when)).transpose() {
                Ok(predicate) => predicate.map(Rc::new),
                Err(e) => {
                    log::warn!("plugins: {}’s keybinding {}: {e}", plugin.id, k.key);
                    continue;
                }
            };
            match KeyBinding::load(&crate::actions::expand_mod(&k.key), action, context, false, None, cx) {
                Ok(binding) => out.push(binding),
                Err(e) => log::warn!("plugins: {}’s keybinding “{}” isn’t a key: {e}", plugin.id, k.key),
            }
        }
    }
    out
}

/// Palette rows: each active plugin's commands, panels and snippets.
pub fn palette_commands(cx: &App) -> Vec<(String, Icon, Box<dyn Action>)> {
    let mut out: Vec<(String, Icon, Box<dyn Action>)> = Vec::new();
    for plugin in state(cx).active() {
        for c in &plugin.ui.commands {
            let icon = match &c.kind {
                PluginCommandKind::Prompt(_) => Icon::new(Lucide::SquarePen),
                PluginCommandKind::Panel(_) => Icon::new(IconName::PanelRight),
                PluginCommandKind::Terminal(_) => Icon::new(Lucide::Terminal),
                PluginCommandKind::Wasm => Icon::new(Lucide::Puzzle),
            };
            let action = RunPluginCommand { plugin: plugin.id.clone(), command: c.id.clone() };
            out.push((format!("{}: {}", plugin.name, c.title), icon, Box::new(action)));
        }
        for p in &plugin.ui.panels {
            // Panels a command already opens are listed once.
            if plugin.ui.commands.iter().any(|c| c.kind == PluginCommandKind::Panel(p.id.clone())) {
                continue;
            }
            let action = OpenPluginPanel { plugin: plugin.id.clone(), panel: p.id.clone() };
            out.push((format!("{}: {}", plugin.name, p.title), Icon::new(IconName::PanelRight), Box::new(action)));
        }
        for t in &plugin.ui.templates {
            let action = ApplyTemplate { plugin: plugin.id.clone(), template: t.name.clone() };
            out.push((format!("Start orbit.toml from {}", t.name), Icon::new(Lucide::FileCode), Box::new(action)));
        }
        for s in &plugin.ui.snippets {
            let action = InsertSnippet { plugin: plugin.id.clone(), name: s.name.clone() };
            out.push((format!("Insert snippet: {}", s.name), Icon::new(Lucide::TextQuote), Box::new(action)));
        }
    }
    out
}

/// Slash commands plugins add, for the composer's menu.
pub fn slash_commands(cx: &App) -> Vec<SlashCommand> {
    state(cx)
        .active()
        .flat_map(|p| p.ui.slash_commands.iter())
        .map(|s| SlashCommand { name: s.name.clone(), description: s.description.clone(), argument_hint: s.argument_hint.clone() })
        .collect()
}

/// `/name args` typed in the composer, when a plugin defines `name`: its
/// prompt with `{{args}}` replaced. `None` for anything else.
pub fn expand_slash(text: &str, cx: &App) -> Option<String> {
    let rest = text.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(n, a)| (n, a.trim()));
    let command = state(cx).active().flat_map(|p| p.ui.slash_commands.iter()).find(|s| s.name == name)?;
    Some(expand(&command.prompt, args))
}

/// A prompt template with `{{args}}` filled in; arguments given to a
/// template that doesn't use them are appended.
pub fn expand(prompt: &str, args: &str) -> String {
    if prompt.contains("{{args}}") || prompt.contains("{{ args }}") {
        orbit_plugin::template::render(prompt, &|name| (name == "args").then(|| args.to_owned()))
    } else if args.is_empty() {
        prompt.to_owned()
    } else {
        format!("{prompt}\n\n{args}")
    }
}

/// A plugin recipe's automation for `project`, on the user's agent.
pub fn recipe_spec(recipe: &PluginRecipe, project: Option<ProjectId>, defaults: &ThreadConfig) -> AutomationSpec {
    AutomationSpec {
        name: recipe.title.clone(),
        project,
        target: if recipe.worktree { RunTarget::Worktree } else { RunTarget::Project },
        config: ThreadConfig { mode: if recipe.plan_mode { Mode::Plan } else { Mode::Build }, ..defaults.clone() },
        prompt: recipe.prompt.clone(),
        triggers: if recipe.rule.is_empty() { Vec::new() } else { vec![Trigger::Rule { rule: recipe.rule.clone(), tz: String::new() }] },
        on_finish: OnFinish { open_pr: recipe.open_pr, ..Default::default() },
        recipe: format!("plugin:{}", recipe.id),
        ..Default::default()
    }
}

/// Active plugins' automation recipes: (plugin name, recipe).
pub fn recipes(cx: &App) -> Vec<(String, PluginRecipe)> {
    state(cx).active().flat_map(|p| p.ui.recipes.iter().map(move |r| (p.name.clone(), r.clone()))).collect()
}

/// A snippet's text.
pub fn snippet(plugin: &str, name: &str, cx: &App) -> Option<String> {
    state(cx).plugin(plugin)?.ui.snippets.iter().find(|s| s.name == name).map(|s| s.text.clone())
}

/// A plugin's palette command.
pub fn command<'a>(plugin: &str, command: &str, cx: &'a App) -> Option<(&'a PluginInfo, &'a PluginCommandInfo)> {
    let p = state(cx).plugin(plugin)?;
    Some((p, p.ui.commands.iter().find(|c| c.id == command)?))
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use orbit_proto::{PluginInfo, PluginStatus, PluginUi, PluginsState};

    fn with_theme(status: PluginStatus) -> PluginsState {
        let json = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/plugins/midnight-theme/themes/midnight.json")).unwrap();
        let json = orbit_plugin::theme::check(&json).unwrap().json;
        PluginsState {
            plugins: vec![PluginInfo {
                id: "midnight-theme".into(),
                status,
                ui: PluginUi { themes: vec![json], ..Default::default() },
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[gpui_kit::test]
    fn plugin_themes_come_and_go_with_their_plugin(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::install(cx);
            super::receive(&with_theme(PluginStatus::Active), cx);
            assert!(crate::theme::names(true, cx).iter().any(|n| n == "Midnight"));
            assert!(crate::theme::names(false, cx).iter().any(|n| n == "Midnight Daybreak"));
            assert_eq!(crate::theme::resolve("Midnight", true, cx), "Midnight");
            assert_eq!(crate::theme::resolve("Midnight", false, cx), crate::theme::ORBIT_LIGHT, "a dark theme isn't a light one");
            // Turned off: gone from the pickers, and a choice of it falls back.
            super::receive(&with_theme(PluginStatus::Disabled), cx);
            assert!(!crate::theme::names(true, cx).iter().any(|n| n == "Midnight"));
            assert_eq!(crate::theme::resolve("Midnight", true, cx), crate::theme::ORBIT_DARK);
        });
    }

    #[test]
    fn slash_prompts_take_their_arguments() {
        assert_eq!(super::expand("Explain {{args}} simply.", "src/main.rs"), "Explain src/main.rs simply.");
        assert_eq!(super::expand("Review the diff.", "carefully"), "Review the diff.\n\ncarefully");
        assert_eq!(super::expand("Review the diff.", ""), "Review the diff.");
    }
}
