//! Pure translations between the Agent Client Protocol and Orbit's domain
//! types. Kept free of I/O so every rule is unit-tested.

use orbit_proto::*;
use similar::{ChangeTag, TextDiff};
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

// ---------------------------------------------------------------------------
// Session configuration: models, modes and options

/// Where a session's selectable values come from: a `configOptions` entry
/// (current ACP), or the older `models` / `modes` objects with their own
/// setter methods.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Option(String),
    Legacy,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Choice {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Select {
    pub source: Source,
    pub current: String,
    pub choices: Vec<Choice>,
}

/// A config option other than model and mode (reasoning level, …).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extra {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub current: OptionSetting,
    /// `None` for booleans.
    pub choices: Option<Vec<Choice>>,
}

/// Everything selectable in a session, as the agent last reported it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionConfig {
    pub models: Option<Select>,
    pub modes: Option<Select>,
    pub extras: Vec<Extra>,
}

impl SessionConfig {
    /// Reads `models`, `modes` and `configOptions` from a `session/new`,
    /// `session/load` or `session/resume` result. Config options win over
    /// the legacy objects.
    pub fn from_result(result: &Value) -> Self {
        let mut config = SessionConfig::default();
        if let Some(models) = result.get("models") {
            let choices = array(models, "availableModels")
                .filter_map(|m| {
                    Some(Choice {
                        id: m.get("modelId")?.as_str()?.to_owned(),
                        name: str_of(m, "name").unwrap_or_default(),
                        description: str_of(m, "description"),
                    })
                })
                .collect::<Vec<_>>();
            if !choices.is_empty() {
                config.models = Some(Select {
                    source: Source::Legacy,
                    current: str_of(models, "currentModelId").unwrap_or_default(),
                    choices,
                });
            }
        }
        if let Some(modes) = result.get("modes") {
            let choices = array(modes, "availableModes")
                .filter_map(|m| {
                    Some(Choice {
                        id: m.get("id")?.as_str()?.to_owned(),
                        name: str_of(m, "name").unwrap_or_default(),
                        description: str_of(m, "description"),
                    })
                })
                .collect::<Vec<_>>();
            if !choices.is_empty() {
                config.modes = Some(Select {
                    source: Source::Legacy,
                    current: str_of(modes, "currentModeId").unwrap_or_default(),
                    choices,
                });
            }
        }
        if let Some(options) = result.get("configOptions") {
            config.apply_options(options);
        }
        config
    }

    /// Applies a full `configOptions` list (from a result or a
    /// `config_option_update`).
    pub fn apply_options(&mut self, options: &Value) {
        let Some(list) = options.as_array() else { return };
        self.extras.clear();
        for option in list.iter() {
            let Some(id) = str_of(option, "id") else { continue };
            let category = str_of(option, "category").unwrap_or_default();
            let kind = str_of(option, "type").unwrap_or_else(|| "select".into());
            let name = str_of(option, "name").unwrap_or_else(|| humanize_id(&id));
            let description = str_of(option, "description");
            if kind == "boolean" {
                let on = option.get("currentValue").and_then(|v| v.as_bool()).unwrap_or(false);
                self.extras.push(Extra { id, name, description, current: OptionSetting::Toggle(on), choices: None });
                continue;
            }
            let choices = flatten_choices(option.get("options"));
            let current = str_of(option, "currentValue").unwrap_or_default();
            match category.as_str() {
                "model" if self.models.as_ref().is_none_or(|m| m.source == Source::Legacy || m.source == Source::Option(id.clone())) => {
                    self.models = Some(Select { source: Source::Option(id), current, choices })
                }
                "mode" if self.modes.as_ref().is_none_or(|m| m.source == Source::Legacy || m.source == Source::Option(id.clone())) => {
                    self.modes = Some(Select { source: Source::Option(id), current, choices })
                }
                _ => self.extras.push(Extra {
                    id,
                    name,
                    description,
                    current: OptionSetting::Choice(current),
                    choices: Some(choices),
                }),
            }
        }
    }

    /// The agent's plan mode, if it has one (`plan`, `architect`, or a URL
    /// id ending in `#plan`).
    pub fn plan_mode(&self) -> Option<&str> {
        self.modes.as_ref()?.choices.iter().map(|c| c.id.as_str()).find(|id| {
            let tail = id.rsplit('#').next().unwrap_or(id).to_ascii_lowercase();
            tail == "plan" || tail == "architect"
        })
    }

    /// The mode a thread with `config` should run in, given the mode the
    /// session started in (`build`). `None` leaves the mode alone.
    pub fn wanted_mode(&self, config: &ThreadConfig, build: Option<&str>) -> Option<String> {
        let modes = self.modes.as_ref()?;
        if config.mode == Mode::Plan
            && let Some(plan) = self.plan_mode()
        {
            return Some(plan.to_owned());
        }
        let find = |names: &[&str]| {
            modes
                .choices
                .iter()
                .find(|c| {
                    let tail = c.id.rsplit('#').next().unwrap_or(&c.id).to_ascii_lowercase();
                    names.contains(&tail.as_str())
                })
                .map(|c| c.id.clone())
        };
        let by_access = match config.access {
            Access::Full => find(&["bypasspermissions", "yolo", "full-access", "full_access", "autopilot"]),
            Access::AutoEdit => find(&["acceptedits", "autoedit", "auto_edit", "auto-edit"]),
            Access::Readonly => find(&["read-only", "readonly", "read_only"]),
            Access::Ask => None,
        };
        by_access.or_else(|| build.map(str::to_owned)).or_else(|| {
            // Leaving plan mode without a remembered build mode: the first
            // mode that is not plan.
            let plan = self.plan_mode();
            modes.choices.iter().find(|c| Some(c.id.as_str()) != plan).map(|c| c.id.clone())
        })
    }

    /// Orbit's view of the session's models, with the extra options on
    /// every model (ACP options belong to the session, not a model).
    pub fn model_infos(&self) -> Vec<ModelInfo> {
        let options: Vec<OptionDescriptor> = self.extras.iter().map(descriptor).collect();
        match &self.models {
            Some(models) => models
                .choices
                .iter()
                .map(|c| ModelInfo {
                    slug: c.id.clone(),
                    name: if c.name.is_empty() { c.id.clone() } else { c.name.clone() },
                    description: c.description.clone(),
                    context_window: 0,
                    is_default: c.id == models.current,
                    options: options.clone(),
                })
                .collect(),
            None => vec![default_model(options)],
        }
    }

    pub fn mode_infos(&self) -> Vec<ModeInfo> {
        let mut modes = vec![ModeInfo { mode: Mode::Build, label: "Build".into(), description: None }];
        if let Some(plan) = self.plan_mode() {
            let choice = self.modes.as_ref().and_then(|m| m.choices.iter().find(|c| c.id == plan));
            modes.push(ModeInfo {
                mode: Mode::Plan,
                label: choice.map(|c| c.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Plan".into()),
                description: choice.and_then(|c| c.description.clone()),
            });
        }
        modes
    }
}

/// The one entry an agent without a model list gets, so it can be picked.
pub fn default_model(options: Vec<OptionDescriptor>) -> ModelInfo {
    ModelInfo {
        slug: String::new(),
        name: "Default model".into(),
        description: Some("Whatever the agent is configured to use".into()),
        context_window: 0,
        is_default: true,
        options,
    }
}

fn descriptor(extra: &Extra) -> OptionDescriptor {
    let kind = match (&extra.choices, &extra.current) {
        (Some(choices), current) => OptionKind::Select {
            choices: choices
                .iter()
                .map(|c| OptionChoice {
                    id: c.id.clone(),
                    label: if c.name.is_empty() { humanize_id(&c.id) } else { c.name.clone() },
                    description: c.description.clone(),
                    is_default: OptionSetting::Choice(c.id.clone()) == *current,
                })
                .collect(),
        },
        (None, OptionSetting::Toggle(on)) => OptionKind::Toggle { default: *on },
        (None, OptionSetting::Choice(_)) => OptionKind::Toggle { default: false },
    };
    OptionDescriptor { id: extra.id.clone(), label: extra.name.clone(), description: extra.description.clone(), kind }
}

fn flatten_choices(options: Option<&Value>) -> Vec<Choice> {
    let mut out = Vec::new();
    let Some(list) = options.and_then(|o| o.as_array()) else { return out };
    for entry in list.iter() {
        if entry.get("group").is_some() || entry.get("options").and_then(|o| o.as_array()).is_some() {
            out.extend(flatten_choices(entry.get("options")));
        } else if let Some(value) = str_of(entry, "value") {
            out.push(Choice {
                name: str_of(entry, "name").unwrap_or_else(|| humanize_id(&value)),
                description: str_of(entry, "description"),
                id: value,
            });
        }
    }
    out
}

/// `available_commands_update` → slash commands.
pub fn commands(update: &Value) -> Vec<SlashCommand> {
    array(update, "availableCommands")
        .filter_map(|c| {
            Some(SlashCommand {
                name: str_of(c, "name")?,
                description: str_of(c, "description").unwrap_or_default(),
                argument_hint: c.get("input").and_then(|i| str_of(i, "hint")).unwrap_or_default(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tool calls

/// A tool call as accumulated from `tool_call` and its updates.
#[derive(Clone, Debug, Default)]
pub struct Tool {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub raw_input: Option<String>,
    pub raw_output: Option<String>,
    /// Text from `content` blocks.
    pub text: String,
    pub diffs: Vec<FileDiffContent>,
    pub locations: Vec<String>,
    /// `rawInput` path fields, for tools that report no locations.
    pub input_paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiffContent {
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: String,
}

impl Tool {
    /// Merges a `tool_call` or `tool_call_update` payload. Absent and
    /// `null` fields leave the current value alone.
    pub fn merge(&mut self, update: &Value) {
        if let Some(id) = str_of(update, "toolCallId") {
            self.id = id;
        }
        if let Some(title) = str_of(update, "title") {
            self.title = title;
        }
        if let Some(kind) = str_of(update, "kind") {
            self.kind = kind;
        }
        if let Some(status) = str_of(update, "status") {
            self.status = status;
        }
        if let Some(input) = update.get("rawInput").filter(|v| !v.is_null()) {
            self.raw_input = sonic_rs::to_string(input).ok();
            self.input_paths = ["path", "file_path", "filePath", "absolute_path", "notebook_path"]
                .iter()
                .filter_map(|k| str_of(input, k))
                .collect();
        }
        if let Some(output) = update.get("rawOutput").filter(|v| !v.is_null()) {
            self.raw_output = Some(match output.as_str() {
                Some(s) => s.to_owned(),
                None => sonic_rs::to_string(output).unwrap_or_default(),
            });
        }
        if let Some(content) = update.get("content").and_then(|c| c.as_array()) {
            // `content` replaces the previous content.
            self.text.clear();
            self.diffs.clear();
            for block in content.iter() {
                match str_of(block, "type").as_deref() {
                    Some("diff") => {
                        if let Some(path) = str_of(block, "path") {
                            self.diffs.push(FileDiffContent {
                                path,
                                old_text: str_of(block, "oldText"),
                                new_text: str_of(block, "newText").unwrap_or_default(),
                            });
                        }
                    }
                    Some("content") => {
                        if let Some(text) = block.get("content").and_then(content_text) {
                            if !self.text.is_empty() {
                                self.text.push('\n');
                            }
                            self.text.push_str(&text);
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(locations) = update.get("locations").and_then(|l| l.as_array()) {
            self.locations = locations.iter().filter_map(|l| str_of(l, "path")).collect();
        }
    }

    pub fn finished(&self) -> bool {
        self.status == "completed" || self.status == "failed"
    }

    pub fn tool_kind(&self) -> ToolKind {
        match self.kind.as_str() {
            "read" => ToolKind::Read,
            "edit" | "delete" | "move" => ToolKind::Edit,
            "search" => ToolKind::Search,
            "execute" => ToolKind::Command,
            "fetch" => ToolKind::Web,
            _ if self.title.starts_with("mcp__") => ToolKind::Mcp,
            _ => ToolKind::Other,
        }
    }

    /// Files the call touched, named even when the agent sent no diff
    /// (T3 Code showed "Changed files" without names, #42025).
    pub fn files(&self) -> Vec<FileChange> {
        let mut files: Vec<FileChange> = self.diffs.iter().map(file_change).collect();
        if matches!(self.kind.as_str(), "edit" | "delete" | "move") {
            let kind = match self.kind.as_str() {
                "delete" => ChangeKind::Deleted,
                "move" => ChangeKind::Renamed,
                _ => ChangeKind::Modified,
            };
            for path in self.locations.iter().chain(&self.input_paths) {
                if !files.iter().any(|f| &f.path == path) {
                    files.push(FileChange { path: path.clone(), kind, additions: 0, deletions: 0 });
                }
            }
        }
        files
    }

    pub fn call(&self) -> ToolCall {
        let title = if self.title.is_empty() { humanize_id(if self.kind.is_empty() { "tool" } else { &self.kind }) } else { self.title.clone() };
        ToolCall {
            call_id: self.id.clone(),
            kind: self.tool_kind(),
            name: if self.kind.is_empty() { "other".into() } else { self.kind.clone() },
            title,
            input: self.raw_input.clone().unwrap_or_default(),
            output: String::new(),
            output_blob: None,
            exit_code: None,
            files: self.files(),
        }
    }

    pub fn output(&self) -> String {
        if !self.text.is_empty() {
            return self.text.clone();
        }
        self.raw_output.clone().unwrap_or_default()
    }

    /// What the user is asked about when the agent requests permission.
    pub fn request_kind(&self, cwd: &str) -> RequestKind {
        let input: Option<Value> = self.raw_input.as_deref().and_then(|s| sonic_rs::from_str(s).ok());
        if self.kind == "execute" {
            let command = input
                .as_ref()
                .and_then(|i| {
                    i.get("command").and_then(|c| match c.as_str() {
                        Some(s) => Some(s.to_owned()),
                        None => c.as_array().map(|a| a.iter().filter_map(|p| p.as_str()).collect::<Vec<_>>().join(" ")),
                    })
                })
                .unwrap_or_else(|| self.title.clone());
            let dir = input.as_ref().and_then(|i| str_of(i, "cwd")).unwrap_or_else(|| cwd.to_owned());
            return RequestKind::Command { command, cwd: dir, reason: self.title.clone() };
        }
        if let Some(diff) = self.diffs.first() {
            return RequestKind::FileChange { path: diff.path.clone(), diff: unified(diff) };
        }
        RequestKind::Tool { name: self.call().title, input: self.raw_input.clone().unwrap_or_default() }
    }
}

fn file_change(diff: &FileDiffContent) -> FileChange {
    let old = diff.old_text.as_deref().unwrap_or("");
    let (mut additions, mut deletions) = (0, 0);
    for change in TextDiff::from_lines(old, &diff.new_text).iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {}
        }
    }
    let kind = if diff.old_text.is_none() { ChangeKind::Added } else { ChangeKind::Modified };
    FileChange { path: diff.path.clone(), kind, additions, deletions }
}

/// A unified diff of an ACP diff block, for approval previews.
pub fn unified(diff: &FileDiffContent) -> String {
    let old = diff.old_text.as_deref().unwrap_or("");
    TextDiff::from_lines(old, &diff.new_text)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{}", diff.path), &format!("b/{}", diff.path))
        .to_string()
}

/// The text of a content block, if it has one.
pub fn content_text(block: &Value) -> Option<String> {
    match str_of(block, "type").as_deref() {
        Some("text") | None => str_of(block, "text"),
        Some("resource") => block.get("resource").and_then(|r| str_of(r, "text")),
        Some("resource_link") => str_of(block, "uri"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Plans, usage, outcomes

pub fn plan(update: &Value) -> Vec<PlanStep> {
    array(update, "entries")
        .filter_map(|e| {
            Some(PlanStep {
                text: str_of(e, "content")?,
                state: match str_of(e, "status").as_deref() {
                    Some("in_progress") => StepState::Active,
                    Some("completed") => StepState::Done,
                    _ => StepState::Pending,
                },
            })
        })
        .collect()
}

/// `usage_update`: context fill and, when priced in dollars, cost.
pub fn usage_update(update: &Value) -> Usage {
    let mut usage = Usage {
        context_tokens: u64_of(update, "used"),
        context_window: u64_of(update, "size"),
        ..Default::default()
    };
    if let Some(cost) = update.get("cost")
        && str_of(cost, "currency").is_none_or(|c| c.eq_ignore_ascii_case("usd"))
        && let Some(amount) = cost.get("amount").and_then(|a| a.as_f64())
    {
        usage.cost_micros = (amount * 1_000_000.0).round().max(0.0) as u64;
    }
    usage
}

/// The `usage` of a `session/prompt` result.
pub fn prompt_usage(usage: &Value) -> Usage {
    Usage {
        input_tokens: u64_of(usage, "inputTokens"),
        output_tokens: u64_of(usage, "outputTokens") + u64_of(usage, "thoughtTokens"),
        cache_read_tokens: u64_of(usage, "cachedReadTokens"),
        cache_write_tokens: u64_of(usage, "cachedWriteTokens"),
        ..Default::default()
    }
}

/// How a `stopReason` ends the turn, and a note for the user if it ended
/// early.
pub fn outcome(stop_reason: &str) -> (crate::provider::TurnOutcome, Option<&'static str>) {
    use crate::provider::TurnOutcome;
    match stop_reason {
        "cancelled" => (TurnOutcome::Interrupted, None),
        "max_tokens" => (TurnOutcome::Completed, Some("The agent stopped: it reached its output limit.")),
        "max_turn_requests" => (TurnOutcome::Completed, Some("The agent stopped: it reached its limit of steps for one turn.")),
        "refusal" => (TurnOutcome::Failed { message: "The agent declined to continue.".into() }, None),
        _ => (TurnOutcome::Completed, None),
    }
}

/// The option id to answer a permission request with. `cancelled` when
/// nothing fits.
pub fn pick_option(options: &[(String, String)], decision: &Decision) -> Option<String> {
    let find = |kind: &str| options.iter().find(|(_, k)| k == kind).map(|(id, _)| id.clone());
    match decision {
        Decision::Allow | Decision::Answer { .. } => find("allow_once").or_else(|| find("allow_always")),
        Decision::AllowForSession => find("allow_always").or_else(|| find("allow_once")),
        Decision::Deny { .. } => find("reject_once").or_else(|| find("reject_always")),
    }
}

// ---------------------------------------------------------------------------
// Small JSON helpers

pub fn str_of(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

pub fn u64_of(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64))).unwrap_or(0)
}

pub fn array<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    value.get(key).and_then(|v| v.as_array()).into_iter().flat_map(|a| a.iter())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonic_rs::json;

    #[test]
    fn config_options_win_over_legacy_models_and_flatten_groups() {
        let result = json!({
            "sessionId": "s",
            "models": {"currentModelId": "old", "availableModels": [{"modelId": "old", "name": "Old"}]},
            "modes": {"currentModeId": "default", "availableModes": [
                {"id": "default", "name": "Default"}, {"id": "plan", "name": "Plan", "description": "Read only"}
            ]},
            "configOptions": [
                {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "pro",
                 "options": [{"group": "g", "name": "Gemini", "options": [
                     {"value": "pro", "name": "Gemini Pro"}, {"value": "flash", "name": "Gemini Flash"}]}]},
                {"id": "effort", "name": "Reasoning", "category": "thought_level", "type": "select", "currentValue": "high",
                 "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
                {"id": "autopilot", "name": "Autopilot", "type": "boolean", "currentValue": true}
            ]
        });
        let config = SessionConfig::from_result(&result);
        let models = config.model_infos();
        assert_eq!(models.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(), ["pro", "flash"]);
        assert!(models[0].is_default);
        assert_eq!(models[0].options.len(), 2);
        assert_eq!(config.models.as_ref().unwrap().source, Source::Option("model".into()));
        assert!(matches!(models[0].options[1].kind, OptionKind::Toggle { default: true }));
        assert_eq!(config.plan_mode(), Some("plan"));
        assert_eq!(config.mode_infos()[1].description.as_deref(), Some("Read only"));
    }

    #[test]
    fn modes_follow_plan_and_access() {
        let config = SessionConfig::from_result(&json!({"modes": {"currentModeId": "default", "availableModes": [
            {"id": "default"}, {"id": "autoEdit"}, {"id": "yolo"}, {"id": "https://x/modes#plan"}
        ]}}));
        let plan = ThreadConfig { mode: Mode::Plan, ..Default::default() };
        assert_eq!(config.wanted_mode(&plan, Some("default")).as_deref(), Some("https://x/modes#plan"));
        let full = ThreadConfig { access: Access::Full, ..Default::default() };
        assert_eq!(config.wanted_mode(&full, Some("default")).as_deref(), Some("yolo"));
        let edit = ThreadConfig { access: Access::AutoEdit, ..Default::default() };
        assert_eq!(config.wanted_mode(&edit, Some("default")).as_deref(), Some("autoEdit"));
        assert_eq!(config.wanted_mode(&ThreadConfig::default(), Some("default")).as_deref(), Some("default"));
        assert!(SessionConfig::default().model_infos()[0].slug.is_empty(), "a model-less agent is still pickable");
    }

    #[test]
    fn tool_updates_merge_and_name_their_files() {
        let mut tool = Tool::default();
        tool.merge(&json!({"toolCallId": "c1", "title": "Edit main.rs", "kind": "edit", "status": "pending",
                           "rawInput": {"file_path": "/w/src/main.rs"}}));
        assert_eq!(tool.files()[0].path, "/w/src/main.rs", "named before any diff arrives");
        tool.merge(&json!({"toolCallId": "c1", "status": "completed", "title": null, "content": [
            {"type": "diff", "path": "/w/src/main.rs", "oldText": "a\nb\n", "newText": "a\nc\nd\n"},
            {"type": "content", "content": {"type": "text", "text": "edited"}}
        ]}));
        assert!(tool.finished());
        assert_eq!(tool.title, "Edit main.rs", "null keeps the previous title");
        let files = tool.files();
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].additions, files[0].deletions, files[0].kind), (2, 1, ChangeKind::Modified));
        assert_eq!(tool.output(), "edited");
        let RequestKind::FileChange { path, diff } = tool.request_kind("/w") else { panic!() };
        assert_eq!(path, "/w/src/main.rs");
        assert!(diff.contains("+c") && diff.contains("-b"), "{diff}");
    }

    #[test]
    fn execute_requests_show_the_command() {
        let mut tool = Tool::default();
        tool.merge(&json!({"toolCallId": "c", "title": "Run tests", "kind": "execute",
                           "rawInput": {"command": ["cargo", "test"], "cwd": "/w/crate"}}));
        assert_eq!(
            tool.request_kind("/w"),
            RequestKind::Command { command: "cargo test".into(), cwd: "/w/crate".into(), reason: "Run tests".into() }
        );
        assert_eq!(tool.tool_kind(), ToolKind::Command);
    }

    #[test]
    fn permission_options_degrade_honestly() {
        let both = vec![("a1".into(), "allow_once".into()), ("aa".into(), "allow_always".into()), ("r1".into(), "reject_once".into())];
        assert_eq!(pick_option(&both, &Decision::AllowForSession).as_deref(), Some("aa"));
        assert_eq!(pick_option(&both, &Decision::Allow).as_deref(), Some("a1"));
        assert_eq!(pick_option(&both, &Decision::Deny { feedback: String::new() }).as_deref(), Some("r1"));
        assert_eq!(pick_option(&[], &Decision::Allow), None);
    }

    #[test]
    fn usage_and_plans_map() {
        let usage = usage_update(&json!({"used": 1200, "size": 200000, "cost": {"amount": 0.25, "currency": "USD"}}));
        assert_eq!((usage.context_tokens, usage.context_window, usage.cost_micros), (1200, 200_000, 250_000));
        let steps = plan(&json!({"entries": [{"content": "Read", "status": "completed", "priority": "high"},
                                            {"content": "Write", "status": "in_progress"}]}));
        assert_eq!(steps[1].state, StepState::Active);
        assert_eq!(outcome("cancelled").0, crate::provider::TurnOutcome::Interrupted);
    }
}
