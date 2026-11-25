//! The command line, built from the specs with clap's builder API, and the
//! conversion of both command lines and MCP tool arguments into one form:
//! a JSON object keyed by argument name.

use clap::builder::PossibleValuesParser;
use clap::{ArgAction, ArgMatches};
use serde_json::{Map, Value};

use crate::error::CliError;
use crate::spec::{self, Arg, GLOBAL, Kind, SPECS, Scope, Spec};

pub const ABOUT: &str = "Drive Orbit from the command line, scripts and agents.";

/// Descriptions of command groups that have no command of their own.
fn group_about(name: &str) -> &'static str {
    match name {
        "terminal" => "Run commands in terminals and read their output",
        _ => "",
    }
}

/// The whole command line.
pub fn command() -> clap::Command {
    let mut root = clap::Command::new("orbit")
        .version(env!("CARGO_PKG_VERSION"))
        .about(ABOUT)
        .disable_help_subcommand(true)
        .max_term_width(100)
        .after_help(
            "Agents: `orbit agent-context` is a short guide; `orbit help --json` describes every command; \
             `orbit mcp` serves them as MCP tools.",
        );
    for arg in GLOBAL {
        root = root.arg(build_arg(arg).global(true));
    }
    let mut groups: Vec<&str> = Vec::new();
    for spec in SPECS {
        if !groups.contains(&spec.path[0]) {
            groups.push(spec.path[0]);
        }
    }
    for group in groups {
        let mut cmd = match spec::find(&[group]) {
            Some(spec) => build_command(spec, group),
            None => clap::Command::new(group).about(group_about(group)).subcommand_required(true),
        };
        for child in SPECS.iter().filter(|s| s.path.len() == 2 && s.path[0] == group) {
            cmd = cmd.subcommand(build_command(child, child.path[1]));
        }
        root = root.subcommand(cmd);
    }
    root
}

fn build_command(spec: &'static Spec, name: &'static str) -> clap::Command {
    let mut cmd = clap::Command::new(name).about(spec.about).visible_aliases(spec.aliases.iter().copied());
    if !spec.details.is_empty() {
        cmd = cmd.long_about(format!("{}.\n\n{}", spec.about, spec.details));
    }
    if !spec.examples.is_empty() {
        let mut text = String::from("Examples:\n");
        for example in spec.examples {
            text.push_str("  ");
            text.push_str(example);
            text.push('\n');
        }
        cmd = cmd.after_long_help(text);
    }
    for arg in spec.args {
        cmd = cmd.arg(build_arg(arg));
    }
    cmd
}

fn value_name(arg: &Arg) -> String {
    match arg.kind {
        Kind::Duration => "DURATION".into(),
        Kind::Int => "N".into(),
        _ => arg.name.to_uppercase().replace('-', "_"),
    }
}

fn build_arg(arg: &'static Arg) -> clap::Arg {
    let mut a = clap::Arg::new(arg.name).help(arg.help);
    if arg.positional {
        a = a.value_name(value_name(arg)).required(arg.required);
        match arg.kind {
            Kind::Text | Kind::Threads => a = a.num_args(1..),
            Kind::Argv => a = a.num_args(1..).trailing_var_arg(true).allow_hyphen_values(true),
            _ => {}
        }
    } else {
        a = a.long(arg.name);
        if arg.kind == Kind::Flag {
            a = a.action(ArgAction::SetTrue);
        } else {
            a = a.value_name(value_name(arg)).action(ArgAction::Set);
        }
    }
    if let Some(short) = arg.short {
        a = a.short(short);
    }
    match arg.kind {
        Kind::Int => a = a.value_parser(clap::value_parser!(u64)),
        Kind::Choice(choices) => a = a.value_parser(PossibleValuesParser::new(choices.iter().copied())),
        Kind::Path | Kind::Project => a = a.value_hint(clap::ValueHint::AnyPath),
        Kind::Argv => a = a.value_hint(clap::ValueHint::CommandWithArguments),
        _ => {}
    }
    if let Some(default) = arg.default {
        a = a.default_value(default);
    }
    a
}

/// What a command line asked for.
pub struct Parsed {
    pub spec: &'static Spec,
    pub args: Map<String, Value>,
    pub global: Map<String, Value>,
}

/// Parses `argv` (without the program name). Help and `--version` come
/// back as errors whose exit code is 0, as clap reports them.
pub fn parse(argv: &[String]) -> Result<Parsed, clap::Error> {
    let matches = command().try_get_matches_from(std::iter::once("orbit".to_owned()).chain(argv.iter().cloned()))?;
    let global = collect(GLOBAL, &matches);
    let Some((top, sub)) = matches.subcommand() else {
        // Bare `orbit`: the status overview.
        return Ok(Parsed { spec: spec::find(&["status"]).unwrap(), args: Map::new(), global });
    };
    let (spec, leaf) = match sub.subcommand() {
        Some((child, leaf)) => (spec::SPECS.iter().find(|s| s.path.len() == 2 && s.path[0] == top && s.path[1] == child), leaf),
        None => (spec::find(&[top]), sub),
    };
    let spec = spec.expect("every clap command comes from a spec");
    Ok(Parsed { spec, args: collect(spec.args, leaf), global })
}

fn collect(args: &[Arg], matches: &ArgMatches) -> Map<String, Value> {
    let mut out = Map::new();
    for arg in args {
        let value = match arg.kind {
            Kind::Flag => Some(Value::Bool(matches.get_flag(arg.name))),
            Kind::Int => matches.get_one::<u64>(arg.name).map(|n| Value::from(*n)),
            Kind::Text => matches
                .get_many::<String>(arg.name)
                .map(|words| Value::String(words.cloned().collect::<Vec<_>>().join(" "))),
            Kind::Argv => matches.get_many::<String>(arg.name).map(|words| Value::String(join_argv(words.cloned().collect()))),
            Kind::Threads => matches
                .get_many::<String>(arg.name)
                .map(|ids| Value::Array(ids.cloned().map(Value::String).collect())),
            _ => matches.get_one::<String>(arg.name).cloned().map(Value::String),
        };
        if let Some(value) = value {
            out.insert(arg.name.to_owned(), value);
        }
    }
    out
}

/// One argument is taken as the whole command line; several are joined,
/// quoting the ones a shell would split.
fn join_argv(words: Vec<String>) -> String {
    if words.len() == 1 {
        return words.into_iter().next().unwrap();
    }
    words
        .into_iter()
        .map(|w| {
            if w.is_empty() || w.chars().any(|c| c.is_whitespace() || c == '"') {
                format!("\"{}\"", w.replace('"', "\\\""))
            } else {
                w
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Checks an MCP tool call's arguments against the spec and fills in
/// defaults, so tools and the command line reach [`crate::exec`] alike.
pub fn from_tool(spec: &Spec, input: Option<&Value>) -> Result<Map<String, Value>, CliError> {
    let empty = Map::new();
    let input = match input {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(map)) => map,
        Some(_) => return Err(CliError::usage("Arguments must be an object.")),
    };
    let tool_args: Vec<&Arg> = spec.args.iter().filter(|a| a.scope == Scope::Both).collect();
    for key in input.keys() {
        if !tool_args.iter().any(|a| a.name == key.as_str() || a.name.replace('-', "_") == *key) {
            return Err(CliError::usage(format!("Unknown argument “{key}”.")));
        }
    }
    let mut out = Map::new();
    for arg in tool_args {
        let given = input.get(arg.name).or_else(|| input.get(&arg.name.replace('-', "_")));
        let value = match given {
            Some(Value::Null) | None => None,
            Some(value) => Some(check(arg, value)?),
        };
        let value = value.or_else(|| arg.tool_default.or(arg.default).map(|d| typed_default(arg, d)));
        match value {
            Some(value) => {
                out.insert(arg.name.to_owned(), value);
            }
            None if arg.required => return Err(CliError::usage(format!("“{}” is required.", tool_key(arg)))),
            None if arg.kind == Kind::Flag => {
                out.insert(arg.name.to_owned(), Value::Bool(false));
            }
            None => {}
        }
    }
    Ok(out)
}

/// The property name a tool uses for an argument: snake_case.
pub fn tool_key(arg: &Arg) -> String {
    arg.name.replace('-', "_")
}

fn typed_default(arg: &Arg, default: &str) -> Value {
    match arg.kind {
        Kind::Int => default.parse::<u64>().map(Value::from).unwrap_or(Value::Null),
        Kind::Flag => Value::Bool(default == "true"),
        _ => Value::String(default.to_owned()),
    }
}

fn check(arg: &Arg, value: &Value) -> Result<Value, CliError> {
    let key = tool_key(arg);
    let bad = |want: &str| CliError::usage(format!("“{key}” must be {want}."));
    match arg.kind {
        Kind::Flag => value.as_bool().map(Value::Bool).ok_or_else(|| bad("true or false")),
        Kind::Int => match value {
            Value::Number(n) if n.as_u64().is_some() => Ok(value.clone()),
            Value::String(s) => s.trim().parse::<u64>().map(Value::from).map_err(|_| bad("a whole number")),
            _ => Err(bad("a whole number")),
        },
        Kind::Threads => match value {
            Value::String(s) => Ok(Value::Array(vec![Value::String(s.clone())])),
            Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_string) => Ok(value.clone()),
            _ => Err(bad("a list of thread ids")),
        },
        Kind::Choice(choices) => match value.as_str() {
            Some(s) if choices.contains(&s) => Ok(value.clone()),
            _ => Err(bad(&format!("one of {}", choices.join(", ")))),
        },
        Kind::Duration => match value {
            // Numbers are seconds.
            Value::Number(n) if n.as_u64().is_some() => Ok(Value::String(format!("{}s", n.as_u64().unwrap()))),
            Value::String(_) => Ok(value.clone()),
            _ => Err(bad("a duration such as \"90s\" or \"10m\"")),
        },
        _ => match value {
            Value::String(s) if arg.required && s.trim().is_empty() => Err(bad("non-empty")),
            Value::String(_) => Ok(value.clone()),
            Value::Number(n) => Ok(Value::String(n.to_string())),
            _ => Err(bad("a string")),
        },
    }
}

/// The JSON Schema of a tool's input.
pub fn input_schema(spec: &Spec) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for arg in spec.args.iter().filter(|a| a.scope == Scope::Both) {
        let mut schema = match arg.kind {
            Kind::Flag => serde_json::json!({"type": "boolean"}),
            Kind::Int => serde_json::json!({"type": "integer", "minimum": 0}),
            Kind::Choice(choices) => serde_json::json!({"type": "string", "enum": choices}),
            Kind::Threads => serde_json::json!({"type": "array", "items": {"type": "string"}, "minItems": 1}),
            _ => serde_json::json!({"type": "string"}),
        };
        let mut help = arg.help.to_owned();
        if arg.kind == Kind::Duration {
            help.push_str(" Seconds, or a string such as \"90s\", \"10m\", \"2h\".");
        }
        schema["description"] = Value::String(help);
        if let Some(default) = arg.tool_default.or(arg.default) {
            schema["default"] = typed_default(arg, default);
        }
        if arg.required {
            required.push(Value::String(tool_key(arg)));
        }
        properties.insert(tool_key(arg), schema);
    }
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn clap_tree_is_valid() {
        command().debug_assert();
    }

    #[test]
    fn parses_flags_after_free_text() {
        let p = parse(&argv("new fix the login test --worktree -p api --json")).unwrap();
        assert_eq!(p.spec.name(), "new");
        assert_eq!(p.args["prompt"], "fix the login test");
        assert_eq!(p.args["worktree"], true);
        assert_eq!(p.args["project"], "api");
        assert_eq!(p.args["timeout"], "10m");
        assert_eq!(p.global["json"], true);
    }

    #[test]
    fn terminal_run_keeps_the_command_verbatim() {
        let p = parse(&argv("terminal run --thread latest cargo test -- --nocapture")).unwrap();
        assert_eq!(p.spec.name(), "terminal run");
        assert_eq!(p.args["command"], "cargo test -- --nocapture");
        assert_eq!(p.args["thread"], "latest");
        let words = vec!["echo".to_owned(), "two words".to_owned()];
        assert_eq!(join_argv(words), "echo \"two words\"");
    }

    #[test]
    fn group_with_and_without_subcommand() {
        assert_eq!(parse(&argv("projects")).unwrap().spec.name(), "projects");
        assert_eq!(parse(&argv("projects add .")).unwrap().spec.name(), "projects add");
        assert_eq!(parse(&argv("mcp")).unwrap().spec.name(), "mcp");
        assert_eq!(parse(&argv("mcp install --apply")).unwrap().args["apply"], true);
        assert!(parse(&argv("terminal")).is_err());
        assert_eq!(parse(&argv("")).unwrap().spec.name(), "status");
        assert_eq!(parse(&argv("ls --needs-you")).unwrap().spec.name(), "threads");
    }

    #[test]
    fn bad_choices_are_usage_errors() {
        let err = parse(&argv("threads --status sleeping")).err().unwrap();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn tool_arguments_are_checked_and_defaulted() {
        let spec = spec::tool("list_threads").unwrap();
        let args = from_tool(spec, Some(&serde_json::json!({"needs_you": true}))).unwrap();
        assert_eq!(args["needs-you"], true);
        assert_eq!(args["limit"], 50, "tool default");
        assert!(from_tool(spec, Some(&serde_json::json!({"bogus": 1}))).is_err());
        assert!(from_tool(spec, Some(&serde_json::json!({"status": "asleep"}))).is_err());
        let wait = spec::tool("wait").unwrap();
        assert!(from_tool(wait, None).is_err(), "threads is required");
        let args = from_tool(wait, Some(&serde_json::json!({"threads": "thr_1", "timeout": 30}))).unwrap();
        assert_eq!(args["threads"], serde_json::json!(["thr_1"]));
        assert_eq!(args["timeout"], "30s");
        let schema = input_schema(spec::tool("start_thread").unwrap());
        assert_eq!(schema["required"], serde_json::json!(["prompt"]));
        assert!(schema["properties"].get("parent").is_none(), "parent is implicit for tools");
    }
}
