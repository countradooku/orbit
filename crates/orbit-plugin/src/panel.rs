//! Declarative panels: UI described as data, drawn by the app with GPUI
//! Kit's own components.
//!
//! A panel file (TOML or JSON) is one [`Node`] tree. Text may hold
//! `{{key}}` placeholders read from the plugin's storage (a JSON object);
//! inside a `list` or `table`, `{{item.field}}` and `{{index}}` name the
//! row. Inputs are the panel's own and reach actions as `{{input.name}}`;
//! switches and selects write their storage key directly.
//!
//! An [`Action`] calls a host API method. The app sends only which action
//! fired and the panel's inputs; the host finds the action in the
//! installed tree again, so a client can never make a plugin do something
//! its files do not say, and checks the capability right before the call.

use std::collections::BTreeMap;

use orbit_proto::Capability;
use serde::{Deserialize, Serialize};
use sonic_rs::{JsonContainerTrait as _, JsonValueMutTrait as _, JsonValueTrait as _, Value};

use crate::template;

pub const MAX_DEPTH: usize = 12;
pub const MAX_NODES: usize = 400;
/// Serialized storage, per plugin.
pub const MAX_STORAGE_BYTES: usize = 1024 * 1024;
pub const MAX_LIST: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    /// Children top to bottom.
    Stack {
        #[serde(default, skip_serializing_if = "is_default")]
        gap: Gap,
        children: Vec<Node>,
    },
    /// Children left to right.
    Row {
        #[serde(default, skip_serializing_if = "is_default")]
        gap: Gap,
        children: Vec<Node>,
    },
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "is_default")]
        muted: bool,
        #[serde(default, skip_serializing_if = "is_default")]
        size: TextSize,
    },
    Heading {
        text: String,
    },
    /// Takes the free space in a row (pushes what follows to the end).
    Spacer {},
    Button {
        label: String,
        action: Action,
        #[serde(default, skip_serializing_if = "is_default")]
        style: ButtonStyle,
    },
    Badge {
        text: String,
        #[serde(default, skip_serializing_if = "is_default")]
        tone: Tone,
    },
    Card {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        title: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        description: String,
        children: Vec<Node>,
    },
    /// One `item` per element of the array at storage key `items`.
    List {
        items: String,
        item: Box<Node>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        empty: String,
    },
    /// A text field; its value reaches actions as `{{input.<bind>}}`.
    Input {
        bind: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        placeholder: String,
        /// Runs on Enter.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        submit: Option<Action>,
    },
    /// A boolean at storage key `bind`.
    Switch {
        bind: String,
        label: String,
    },
    /// One of `options` at storage key `bind`.
    Select {
        bind: String,
        options: Vec<Choice>,
    },
    /// `value` (a number or a placeholder) out of `max`.
    Progress {
        value: String,
        #[serde(default = "hundred")]
        max: f64,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        label: String,
    },
    Markdown {
        text: String,
    },
    CodeView {
        text: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        language: String,
    },
    /// Rows from the array of objects at storage key `rows`.
    Table {
        rows: String,
        columns: Vec<Column>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        empty: String,
    },
    Tabs {
        tabs: Vec<Tab>,
    },
    EmptyState {
        title: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        description: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        button: Option<Labelled>,
    },
}

fn hundred() -> f64 {
    100.
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Gap {
    None,
    Small,
    #[default]
    Medium,
    Large,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSize {
    Small,
    #[default]
    Normal,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonStyle {
    Primary,
    #[default]
    Secondary,
    Ghost,
    Danger,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Neutral,
    Info,
    Success,
    Warning,
    Danger,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    /// The row field shown.
    pub key: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tab {
    pub title: String,
    pub children: Vec<Node>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Labelled {
    pub label: String,
    pub action: Action,
}

/// A host API call. Argument values are templates.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub call: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, String>,
}

/// Methods a panel action may call: name, required arguments, capability
/// (`None`: the plugin's WebAssembly module).
pub const METHODS: &[(&str, &[&str], Option<Capability>)] = &[
    ("storage.set", &["key", "value"], Some(Capability::Storage)),
    ("storage.append", &["key", "value"], Some(Capability::Storage)),
    ("storage.remove", &["key", "index"], Some(Capability::Storage)),
    ("storage.toggle", &["key"], Some(Capability::Storage)),
    ("storage.clear", &["key"], Some(Capability::Storage)),
    ("notifications.show", &["title"], Some(Capability::Notifications)),
    ("threads.create", &["prompt"], Some(Capability::ThreadsCreate)),
    ("plugin.action", &["name"], None),
];

impl Node {
    /// Parses a panel file: JSON when it starts with `{`, else TOML.
    pub fn parse(text: &str) -> Result<Node, String> {
        if text.len() > crate::manifest::limits::TEXT_FILE_BYTES {
            return Err("the panel file is too large.".into());
        }
        if text.trim_start().starts_with('{') {
            sonic_rs::from_str(text).map_err(|e| format!("not a valid panel ({e})."))
        } else {
            toml::from_str(text).map_err(|e| format!("not a valid panel ({}).", e.message().trim_end_matches('.')))
        }
    }

    /// The tree as JSON (what travels to clients).
    pub fn to_json(&self) -> String {
        sonic_rs::to_string(self).unwrap_or_default()
    }

    /// Children, for walking the tree.
    pub fn children(&self) -> Vec<&Node> {
        match self {
            Node::Stack { children, .. } | Node::Row { children, .. } | Node::Card { children, .. } => children.iter().collect(),
            Node::List { item, .. } => vec![item],
            Node::Tabs { tabs } => tabs.iter().flat_map(|t| t.children.iter()).collect(),
            _ => Vec::new(),
        }
    }

    /// This node's own explicit actions.
    fn own_actions(&self) -> Vec<&Action> {
        match self {
            Node::Button { action, .. } => vec![action],
            Node::Input { submit: Some(action), .. } => vec![action],
            Node::EmptyState { button: Some(b), .. } => vec![&b.action],
            _ => Vec::new(),
        }
    }

    /// Every explicit action, in tree order; an action's id is `a<index>`.
    pub fn actions(&self) -> Vec<&Action> {
        let mut out = Vec::new();
        fn walk<'a>(node: &'a Node, out: &mut Vec<&'a Action>) {
            out.extend(node.own_actions());
            for child in node.children() {
                walk(child, out);
            }
        }
        walk(self, &mut out);
        out
    }

    /// Storage keys switches and selects write, with the allowed values
    /// (`None`: a boolean).
    pub fn bound_keys(&self) -> Vec<(&str, Option<Vec<&str>>)> {
        let mut out = Vec::new();
        fn walk<'a>(node: &'a Node, out: &mut Vec<(&'a str, Option<Vec<&'a str>>)>) {
            match node {
                Node::Switch { bind, .. } => out.push((bind, None)),
                Node::Select { bind, options } => out.push((bind, Some(options.iter().map(|o| o.value.as_str()).collect()))),
                _ => {}
            }
            for child in node.children() {
                walk(child, out);
            }
        }
        walk(self, &mut out);
        out
    }

    /// Problems with the tree, given what the plugin may do.
    pub fn problems(&self, capabilities: &[Capability], has_wasm: bool) -> Vec<String> {
        let mut out = Vec::new();
        let mut count = 0;
        fn walk(node: &Node, depth: usize, count: &mut usize, out: &mut Vec<String>) {
            *count += 1;
            if depth > MAX_DEPTH {
                out.push(format!("nested more than {MAX_DEPTH} deep."));
                return;
            }
            match node {
                Node::Select { options, .. } if options.is_empty() => out.push("a select has no options.".into()),
                Node::Table { columns, .. } if columns.is_empty() => out.push("a table has no columns.".into()),
                Node::Progress { max, .. } if !(max.is_finite() && *max > 0.) => out.push("progress max must be above 0.".into()),
                Node::Input { bind, .. } if !is_key(bind) => out.push(format!("input “{bind}” needs a simple name.")),
                Node::List { items: key, .. } | Node::Table { rows: key, .. } | Node::Switch { bind: key, .. } | Node::Select { bind: key, .. }
                    if !is_key(key) =>
                {
                    out.push(format!("“{key}” isn’t a storage key (letters, digits, “_”, “-”, dots)."))
                }
                _ => {}
            }
            for child in node.children() {
                walk(child, depth + 1, count, out);
            }
        }
        walk(self, 0, &mut count, &mut out);
        if count > MAX_NODES {
            out.push(format!("more than {MAX_NODES} components."));
        }
        for action in self.actions() {
            match METHODS.iter().find(|(name, ..)| *name == action.call) {
                None => out.push(format!(
                    "action “{}” isn’t a host method; known: {}.",
                    action.call,
                    METHODS.iter().map(|m| m.0).collect::<Vec<_>>().join(", ")
                )),
                Some((name, required, capability)) => {
                    for arg in *required {
                        if !action.args.contains_key(*arg) {
                            out.push(format!("action “{name}” needs args.{arg}."));
                        }
                    }
                    match capability {
                        Some(c) if !capabilities.contains(c) => {
                            out.push(format!("action “{name}” needs the “{}” capability.", c.key()))
                        }
                        None if !has_wasm => out.push(format!("action “{name}” needs a [wasm] module.")),
                        _ => {}
                    }
                }
            }
        }
        if !self.bound_keys().is_empty() && !capabilities.contains(&Capability::Storage) {
            out.push("switches and selects write storage: they need the “storage” capability.".into());
        }
        out
    }
}

fn is_key(key: &str) -> bool {
    (1..=128).contains(&key.len()) && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// What a panel action asks of the engine after its storage part is done.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Storage changed (or nothing more to do).
    None,
    Notify { title: String, body: String },
    CreateThread { prompt: String },
    /// Call the WebAssembly module's `orbit_action`.
    Wasm { name: String, input: Value },
}

/// The context templates of an action see: the storage's keys, `input`,
/// and in a list row `index` and `item`.
pub fn context(storage: &Value, inputs: &[(String, String)], list: Option<(&str, usize)>) -> Value {
    let mut ctx = if storage.is_object() { storage.clone() } else { Value::new_object() };
    let mut input = Value::new_object();
    for (k, v) in inputs {
        if k != "index" && k != "value" {
            input.as_object_mut().expect("object").insert(k, Value::from(v.as_str()));
        }
    }
    if let Some(obj) = ctx.as_object_mut() {
        obj.insert("input", input);
        if let Some((key, index)) = list {
            obj.insert("index", Value::from(index as u64));
            if let Some(item) = template::lookup(storage, key).and_then(|a| a.as_array()).and_then(|a| a.get(index)) {
                obj.insert("item", item.clone());
            }
        }
    }
    ctx
}

/// The list a node belongs to, for actions inside `list` rows: the
/// storage key of the innermost list containing action `index`.
pub fn list_of_action(root: &Node, index: usize) -> Option<&str> {
    fn walk<'a>(node: &'a Node, list: Option<&'a str>, n: &mut usize, target: usize) -> Option<Option<&'a str>> {
        for _ in node.own_actions() {
            if *n == target {
                return Some(list);
            }
            *n += 1;
        }
        let inner = match node {
            Node::List { items, .. } => Some(items.as_str()),
            _ => list,
        };
        for child in node.children() {
            if let Some(found) = walk(child, inner, n, target) {
                return Some(found);
            }
        }
        None
    }
    walk(root, None, &mut 0, index).flatten()
}

/// Applies an action's storage part to `storage` and says what else to
/// do. `inputs` are the panel's inputs plus `index` for list rows.
pub fn apply(root: &Node, action_id: &str, inputs: &[(String, String)], storage: &mut Value) -> Result<(String, Effect), String> {
    // Switches and selects: `set:<key>` with the new value in `value`.
    if let Some(key) = action_id.strip_prefix("set:") {
        let (_, allowed) = root.bound_keys().into_iter().find(|(k, _)| *k == key).ok_or("That control isn’t in the panel.")?;
        let raw = inputs.iter().find(|(k, _)| k == "value").map(|(_, v)| v.as_str()).unwrap_or_default();
        let value = match allowed {
            None => Value::from(matches!(raw, "true" | "on" | "1")),
            Some(values) if values.contains(&raw) => Value::from(raw),
            Some(_) => return Err("That isn’t one of the choices.".into()),
        };
        set_path(storage, key, value)?;
        return Ok(("storage.set".into(), Effect::None));
    }
    let index: usize = action_id.strip_prefix('a').and_then(|n| n.parse().ok()).ok_or("Unknown action.")?;
    let action = *root.actions().get(index).ok_or("That action isn’t in the panel.")?;
    let row: Option<usize> = inputs.iter().find(|(k, _)| k == "index").and_then(|(_, v)| v.parse().ok());
    let list = list_of_action(root, index).zip(row);
    let ctx = context(storage, inputs, list);
    let arg = |name: &str| action.args.get(name).map(|t| template::render_json(t, &ctx)).unwrap_or_default();
    let typed = |text: String| match action.args.get("as").map(String::as_str) {
        Some("number") => text.trim().parse::<f64>().ok().and_then(Value::new_f64).ok_or_else(|| format!("“{text}” isn’t a number.")),
        Some("bool") => Ok(Value::from(matches!(text.as_str(), "true" | "on" | "1"))),
        _ => Ok(Value::from(text.as_str())),
    };
    let effect = match action.call.as_str() {
        "storage.set" => {
            set_path(storage, &arg("key"), typed(arg("value"))?)?;
            Effect::None
        }
        "storage.append" => {
            let value = arg("value");
            if value.trim().is_empty() {
                return Ok((action.call.clone(), Effect::None));
            }
            let key = arg("key");
            let mut list = template::lookup(storage, &key).filter(|v| v.is_array()).cloned().unwrap_or_else(Value::new_array);
            let array = list.as_array_mut().expect("array");
            if array.len() >= MAX_LIST {
                return Err(format!("“{key}” already holds {MAX_LIST} entries."));
            }
            array.push(typed(value)?);
            set_path(storage, &key, list)?;
            Effect::None
        }
        "storage.remove" => {
            let key = arg("key");
            let at: usize = arg("index").trim().parse().map_err(|_| "No row to remove.".to_owned())?;
            let mut list = template::lookup(storage, &key).filter(|v| v.is_array()).cloned().unwrap_or_else(Value::new_array);
            let array = list.as_array_mut().expect("array");
            if at < array.len() {
                array.remove(at);
            }
            set_path(storage, &key, list)?;
            Effect::None
        }
        "storage.toggle" => {
            let key = arg("key");
            let on = template::lookup(storage, &key).and_then(|v| v.as_bool()).unwrap_or(false);
            set_path(storage, &key, Value::from(!on))?;
            Effect::None
        }
        "storage.clear" => {
            remove_path(storage, &arg("key"));
            Effect::None
        }
        "notifications.show" => Effect::Notify { title: arg("title"), body: arg("body") },
        "threads.create" => Effect::CreateThread { prompt: arg("prompt") },
        "plugin.action" => Effect::Wasm { name: arg("name"), input: ctx.clone() },
        other => return Err(format!("“{other}” isn’t a host method.")),
    };
    check_size(storage)?;
    Ok((action.call.clone(), effect))
}

/// Sets a dotted key, creating objects on the way.
pub fn set_path(root: &mut Value, key: &str, value: Value) -> Result<(), String> {
    if !is_key(key) {
        return Err(format!("“{key}” isn’t a storage key."));
    }
    if !root.is_object() {
        *root = Value::new_object();
    }
    let parts: Vec<&str> = key.split('.').filter(|p| !p.is_empty()).collect();
    let mut node = root;
    for (i, part) in parts.iter().enumerate() {
        let obj = node.as_object_mut().ok_or_else(|| format!("“{key}” goes through a value that isn’t an object."))?;
        if i + 1 == parts.len() {
            obj.insert(part, value);
            return Ok(());
        }
        if !obj.get(part).is_some_and(|v| v.is_object()) {
            obj.insert(part, Value::new_object());
        }
        node = obj.get_mut(part).expect("inserted");
    }
    Err("Empty key.".into())
}

fn remove_path(root: &mut Value, key: &str) {
    let parts: Vec<&str> = key.split('.').filter(|p| !p.is_empty()).collect();
    let Some((last, path)) = parts.split_last() else { return };
    let mut node = root;
    for part in path {
        match node.as_object_mut().and_then(|o| o.get_mut(part)) {
            Some(next) => node = next,
            None => return,
        }
    }
    if let Some(obj) = node.as_object_mut() {
        obj.remove(last);
    }
}

pub fn check_size(storage: &Value) -> Result<(), String> {
    if storage.to_string().len() > MAX_STORAGE_BYTES {
        return Err(format!("The plugin’s storage would pass {} KiB.", MAX_STORAGE_BYTES / 1024));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const TODO: &str = r#"
type = "stack"
children = [
  { type = "heading", text = "Todo" },
  { type = "text", text = "{{count}} open", muted = true },
  { type = "list", items = "todos", empty = "Nothing to do", item = { type = "row", children = [
      { type = "text", text = "{{item}}" },
      { type = "button", label = "Done", style = "ghost", action = { call = "storage.remove", args = { key = "todos", index = "{{index}}" } } },
  ] } },
  { type = "input", bind = "draft", placeholder = "Add a todo", submit = { call = "storage.append", args = { key = "todos", value = "{{input.draft}}" } } },
  { type = "switch", bind = "settings.compact", label = "Compact" },
  { type = "select", bind = "sort", options = [ { value = "new", label = "Newest" }, { value = "old", label = "Oldest" } ] },
]
"#;

    #[test]
    fn panels_parse_from_toml_and_json_alike() {
        let tree = Node::parse(TODO).unwrap();
        let again = Node::parse(&tree.to_json()).unwrap();
        assert_eq!(tree, again);
        assert_eq!(tree.actions().len(), 2);
        assert!(tree.problems(&[Capability::Storage], false).is_empty(), "{:?}", tree.problems(&[Capability::Storage], false));
    }

    #[test]
    fn unknown_components_and_fields_are_refused() {
        assert!(Node::parse(r#"{"type":"iframe","src":"https://x"}"#).is_err());
        assert!(Node::parse(r#"{"type":"text","text":"x","onclick":"y"}"#).unwrap_err().contains("onclick"));
    }

    #[test]
    fn actions_need_known_methods_and_capabilities() {
        let tree = Node::parse(TODO).unwrap();
        let problems = tree.problems(&[], false).join("\n");
        assert!(problems.contains("“storage” capability"), "{problems}");
        let bad = Node::parse(r#"{"type":"button","label":"x","action":{"call":"shell.run","args":{"cmd":"rm -rf /"}}}"#).unwrap();
        assert!(bad.problems(&Capability::ALL, true)[0].contains("isn’t a host method"));
        let wasm = Node::parse(r#"{"type":"button","label":"x","action":{"call":"plugin.action","args":{"name":"go"}}}"#).unwrap();
        assert!(wasm.problems(&[], false)[0].contains("[wasm]"));
    }

    #[test]
    fn actions_change_storage_through_the_installed_tree() {
        let tree = Node::parse(TODO).unwrap();
        let mut storage: Value = sonic_rs::from_str(r#"{"count":1,"todos":["write tests"]}"#).unwrap();
        // The input's submit action (a1) appends; row actions get their index.
        apply(&tree, "a1", &[("draft".into(), "ship it".into())], &mut storage).unwrap();
        assert_eq!(storage["todos"].as_array().unwrap().len(), 2);
        apply(&tree, "a0", &[("index".into(), "0".into())], &mut storage).unwrap();
        assert_eq!(template::text_of(&storage["todos"][0]), "ship it");
        apply(&tree, "set:settings.compact", &[("value".into(), "true".into())], &mut storage).unwrap();
        assert_eq!(storage["settings"]["compact"].as_bool(), Some(true));
        assert!(apply(&tree, "set:sort", &[("value".into(), "random".into())], &mut storage).is_err());
        // Keys and actions that the tree does not have are refused.
        assert!(apply(&tree, "set:admin", &[("value".into(), "true".into())], &mut storage).is_err());
        assert!(apply(&tree, "a9", &[], &mut storage).is_err());
    }
}
