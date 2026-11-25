//! The provider catalog, built from the CLI's `initialize` response: models
//! and their options, modes, slash commands, account and version. Nothing
//! here is hardcoded except Orbit's own two modes.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Mutex;

use orbit_proto::*;
use serde::Deserialize;

/// Option ids the adapter understands.
pub const OPT_EFFORT: &str = "effort";
pub const OPT_FAST: &str = "fast";
pub const OPT_THINKING: &str = "thinking";
/// The effort choice that leaves the level to Claude Code.
pub const EFFORT_DEFAULT: &str = "default";

#[derive(Deserialize)]
pub struct InitResponse<'a> {
    #[serde(borrow, default)]
    pub models: Vec<Model<'a>>,
    #[serde(borrow, default)]
    pub commands: Vec<Command<'a>>,
    #[serde(borrow, default)]
    pub account: Option<Account<'a>>,
    #[serde(borrow, default)]
    pub claude_code_version: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub current_permission_mode: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model<'a> {
    #[serde(borrow)]
    pub value: Cow<'a, str>,
    #[serde(borrow, default)]
    pub resolved_model: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub display_name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub description: Option<Cow<'a, str>>,
    #[serde(default)]
    pub supports_effort: bool,
    #[serde(borrow, default)]
    pub supported_effort_levels: Vec<Cow<'a, str>>,
    #[serde(default)]
    pub supports_fast_mode: bool,
    #[serde(default)]
    pub supports_thinking_off: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Command<'a> {
    #[serde(borrow)]
    pub name: Cow<'a, str>,
    #[serde(borrow, default)]
    pub description: Cow<'a, str>,
    #[serde(borrow, default)]
    pub argument_hint: Cow<'a, str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account<'a> {
    #[serde(borrow, default)]
    pub email: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub subscription_type: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub api_provider: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub token_source: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub api_key_source: Option<Cow<'a, str>>,
}

/// What Orbit knows about the models beyond the provider info: the API model
/// each alias resolves to and context windows learned from results.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub info: ProviderInfo,
    /// Alias (`opus`, `default`) → API model id.
    pub resolved: HashMap<String, String>,
    /// API model id → context window.
    pub windows: HashMap<String, u64>,
}

/// The last catalog any session or probe saw, so a new session can validate
/// options before its own `initialize` answers.
static LAST: Mutex<Option<Catalog>> = Mutex::new(None);

pub fn remember(catalog: &Catalog) {
    *LAST.lock().unwrap() = Some(catalog.clone());
}

pub fn last() -> Option<Catalog> {
    LAST.lock().unwrap().clone()
}

/// Records a context window learned from a result in the shared catalog.
pub fn remember_window(model: &str, window: u64) {
    if let Some(c) = LAST.lock().unwrap().as_mut() {
        c.windows.insert(model.to_owned(), window);
    }
}

impl Catalog {
    pub fn from_init(resp: &InitResponse, binary: Option<String>) -> Self {
        let mut resolved = HashMap::new();
        let models = resp
            .models
            .iter()
            .map(|m| {
                if let Some(r) = &m.resolved_model {
                    resolved.insert(m.value.to_string(), r.to_string());
                }
                ModelInfo {
                    slug: m.value.to_string(),
                    name: m.display_name.as_deref().map(str::to_owned).unwrap_or_else(|| humanize_id(&m.value)),
                    description: m.description.as_deref().filter(|d| !d.is_empty()).map(str::to_owned),
                    context_window: 0,
                    is_default: m.value == "default",
                    options: model_options(m),
                }
            })
            .collect();
        let slash_commands = resp.commands.iter().map(slash_command).collect();
        let (signed_in, account) = account(resp.account.as_ref());
        // Proto structs gain fields; the update keeps this merge-safe.
        #[allow(clippy::needless_update)]
        let info = ProviderInfo {
            kind: ProviderKind::Claude,
            installed: true,
            version: resp.claude_code_version.as_deref().map(str::to_owned),
            signed_in: Some(signed_in),
            account,
            binary,
            models,
            modes: modes(),
            slash_commands,
            message: (!signed_in).then(|| "Not signed in. Run `claude` in a terminal and sign in.".to_owned()),
            ..Default::default()
        };
        Catalog { info, resolved, windows: HashMap::new() }
    }

    pub fn model(&self, slug: &str) -> Option<&ModelInfo> {
        let slug = if slug.is_empty() { "default" } else { slug };
        self.info.models.iter().find(|m| m.slug == slug)
    }

    /// The API model id a slug resolves to.
    pub fn api_model<'s>(&'s self, slug: &'s str) -> &'s str {
        let slug = if slug.is_empty() { "default" } else { slug };
        self.resolved.get(slug).map(String::as_str).unwrap_or(slug)
    }

    /// Fills in context windows learned earlier (by other sessions).
    pub fn with_windows(mut self, known: &HashMap<String, u64>) -> Self {
        for (model, window) in known {
            self.learn_window(model, *window);
        }
        self
    }

    /// Records a context window; returns true when the catalog changed.
    pub fn learn_window(&mut self, api_model: &str, window: u64) -> bool {
        if window == 0 || self.windows.get(api_model) == Some(&window) {
            return false;
        }
        self.windows.insert(api_model.to_owned(), window);
        remember_window(api_model, window);
        for m in &mut self.info.models {
            let api = self.resolved.get(&m.slug).map(String::as_str).unwrap_or(&m.slug);
            if api == api_model {
                m.context_window = window;
            }
        }
        true
    }
}

pub fn slash_command(c: &Command) -> SlashCommand {
    SlashCommand {
        name: c.name.to_string(),
        description: c.description.to_string(),
        argument_hint: c.argument_hint.to_string(),
    }
}

fn model_options(m: &Model) -> Vec<OptionDescriptor> {
    let mut options = Vec::new();
    if m.supports_effort && !m.supported_effort_levels.is_empty() {
        let mut choices = vec![OptionChoice {
            id: EFFORT_DEFAULT.to_owned(),
            label: "Default".to_owned(),
            description: Some("Claude Code's default for this model".to_owned()),
            is_default: true,
        }];
        choices.extend(m.supported_effort_levels.iter().map(|level| OptionChoice {
            id: level.to_string(),
            label: humanize_id(level),
            description: None,
            is_default: false,
        }));
        options.push(OptionDescriptor {
            id: OPT_EFFORT.to_owned(),
            label: "Effort".to_owned(),
            description: Some("How much the model reasons before it answers".to_owned()),
            kind: OptionKind::Select { choices },
        });
    }
    if m.supports_thinking_off {
        options.push(OptionDescriptor {
            id: OPT_THINKING.to_owned(),
            label: "Thinking".to_owned(),
            description: Some("Let the model think before it answers".to_owned()),
            kind: OptionKind::Toggle { default: true },
        });
    }
    if m.supports_fast_mode {
        options.push(OptionDescriptor {
            id: OPT_FAST.to_owned(),
            label: "Fast mode".to_owned(),
            description: Some("Faster output at a higher price".to_owned()),
            kind: OptionKind::Toggle { default: false },
        });
    }
    options
}

fn modes() -> Vec<ModeInfo> {
    vec![
        ModeInfo { mode: Mode::Build, label: "Build".to_owned(), description: Some("Work on the task directly".to_owned()) },
        ModeInfo {
            mode: Mode::Plan,
            label: "Plan".to_owned(),
            description: Some("Research and propose a plan before changing anything".to_owned()),
        },
    ]
}

/// Whether the account can run, and how to show it.
fn account(a: Option<&Account>) -> (bool, Option<String>) {
    let Some(a) = a else { return (false, None) };
    let third_party = a.api_provider.as_deref().is_some_and(|p| p != "firstParty");
    let key = a.api_key_source.as_deref().is_some_and(|s| s != "none");
    let token = a.token_source.as_deref().is_some_and(|s| s != "none");
    let signed_in = a.email.is_some() || a.subscription_type.is_some() || third_party || key || token;
    let shown = match (a.email.as_deref(), a.subscription_type.as_deref()) {
        (Some(email), Some(plan)) => Some(format!("{email} ({plan})")),
        (Some(email), None) => Some(email.to_owned()),
        (None, Some(plan)) => Some(plan.to_owned()),
        (None, None) if third_party => a.api_provider.as_deref().map(str::to_owned),
        _ => None,
    };
    (signed_in, shown)
}

/// A chosen option value, if the model reports the option.
pub fn option<'c>(config: &'c ThreadConfig, model: Option<&ModelInfo>, id: &str) -> Option<&'c OptionSetting> {
    if let Some(m) = model
        && !m.options.iter().any(|o| o.id == id)
    {
        return None;
    }
    config.options.iter().find(|o| o.id == id).map(|o| &o.value)
}

/// The effort level to request, `None` for Claude Code's default.
pub fn effort(config: &ThreadConfig, model: Option<&ModelInfo>) -> Option<String> {
    match option(config, model, OPT_EFFORT)? {
        OptionSetting::Choice(c) if c != EFFORT_DEFAULT && !c.is_empty() => {
            // Only levels the model reports, when the catalog is known.
            let valid = model.is_none_or(|m| {
                m.options.iter().any(|o| match &o.kind {
                    OptionKind::Select { choices } => o.id == OPT_EFFORT && choices.iter().any(|ch| &ch.id == c),
                    OptionKind::Toggle { .. } => false,
                })
            });
            valid.then(|| c.clone())
        }
        _ => None,
    }
}

pub fn toggle(config: &ThreadConfig, model: Option<&ModelInfo>, id: &str) -> Option<bool> {
    match option(config, model, id)? {
        OptionSetting::Toggle(on) => Some(*on),
        OptionSetting::Choice(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INIT: &str = r#"{"commands":[{"name":"review","description":"Review a PR","argumentHint":"[pr]","builtin":true}],
      "models":[{"value":"default","resolvedModel":"claude-opus-5-5","displayName":"Default (recommended)","description":"Opus 5.5","supportsEffort":true,"supportedEffortLevels":["low","high","xhigh"],"supportsFastMode":true,"supportsThinkingOff":false},
                {"value":"claude-haiku-4-5","resolvedModel":"claude-haiku-4-5","displayName":"Haiku 4.5","supportsThinkingOff":true}],
      "account":{"email":"a@b.c","subscriptionType":"Claude Max","apiProvider":"firstParty"},"claude_code_version":"2.1.326"}"#;

    #[test]
    fn builds_the_catalog_from_initialize() {
        let resp: InitResponse = sonic_rs::from_str(INIT).unwrap();
        let c = Catalog::from_init(&resp, None);
        assert_eq!(c.info.version.as_deref(), Some("2.1.326"));
        assert_eq!(c.info.signed_in, Some(true));
        assert_eq!(c.info.account.as_deref(), Some("a@b.c (Claude Max)"));
        assert_eq!(c.info.slash_commands[0].argument_hint, "[pr]");
        let def = c.model("").unwrap();
        assert!(def.is_default);
        let OptionKind::Select { choices } = &def.options[0].kind else { panic!() };
        let ids: Vec<_> = choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["default", "low", "high", "xhigh"]);
        assert_eq!(choices[3].label, "Extra high");
        assert_eq!(def.options[1].id, OPT_FAST);
        let haiku = c.model("claude-haiku-4-5").unwrap();
        assert_eq!(haiku.options.len(), 1);
        assert_eq!(haiku.options[0].id, OPT_THINKING);
        assert_eq!(c.api_model(""), "claude-opus-5-5");
    }

    #[test]
    fn signed_out_account_is_reported() {
        let resp: InitResponse =
            sonic_rs::from_str(r#"{"models":[],"account":{"tokenSource":"none","apiProvider":"firstParty"}}"#).unwrap();
        let c = Catalog::from_init(&resp, None);
        assert_eq!(c.info.signed_in, Some(false));
        assert!(c.info.message.is_some());
    }

    #[test]
    fn options_the_model_lacks_are_ignored() {
        let resp: InitResponse = sonic_rs::from_str(INIT).unwrap();
        let c = Catalog::from_init(&resp, None);
        let config = ThreadConfig {
            model: "claude-haiku-4-5".into(),
            options: vec![OptionValue { id: OPT_EFFORT.into(), value: OptionSetting::Choice("high".into()) }],
            ..Default::default()
        };
        assert_eq!(effort(&config, c.model(&config.model)), None);
        let config = ThreadConfig { model: String::new(), ..config };
        assert_eq!(effort(&config, c.model(&config.model)).as_deref(), Some("high"));
        let bad = ThreadConfig {
            options: vec![OptionValue { id: OPT_EFFORT.into(), value: OptionSetting::Choice("max".into()) }],
            ..Default::default()
        };
        assert_eq!(effort(&bad, c.model("")), None);
    }
}
