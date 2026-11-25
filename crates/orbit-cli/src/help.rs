//! Self-description: `orbit help [--json]`, shell completions and the
//! agent guide, all generated from the specs.

use serde_json::{Value, json};

use crate::args;
use crate::error::CliError;
use crate::exec::Outcome;
use crate::spec::{Arg, GLOBAL, Kind, SPECS, Scope, Spec, exit};

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Flag => "flag",
        Kind::Str => "string",
        Kind::Int => "integer",
        Kind::Choice(_) => "choice",
        Kind::Thread => "thread",
        Kind::Threads => "threads",
        Kind::Project => "project",
        Kind::Path => "path",
        Kind::Text => "text",
        Kind::Duration => "duration",
        Kind::Argv => "command",
    }
}

fn arg_json(arg: &Arg) -> Value {
    let mut v = json!({
        "name": arg.name,
        "kind": kind_name(arg.kind),
        "help": arg.help,
        "required": arg.required,
        "positional": arg.positional,
    });
    if let Kind::Choice(choices) = arg.kind {
        v["choices"] = json!(choices);
    }
    if let Some(short) = arg.short {
        v["short"] = json!(format!("-{short}"));
    }
    if let Some(default) = arg.default {
        v["default"] = json!(default);
    }
    if arg.scope == Scope::Cli {
        v["cli_only"] = json!(true);
    }
    v
}

fn spec_json(spec: &Spec) -> Value {
    let mut v = json!({
        "command": spec.name(),
        "about": spec.about,
        "args": spec.args.iter().map(arg_json).collect::<Vec<_>>(),
        "needs_engine": spec.connects,
    });
    if !spec.details.is_empty() {
        v["details"] = json!(spec.details);
    }
    if !spec.aliases.is_empty() {
        v["aliases"] = json!(spec.aliases);
    }
    if !spec.examples.is_empty() {
        v["examples"] = json!(spec.examples);
    }
    if let Some(tool) = &spec.tool {
        v["mcp_tool"] = json!(tool.name);
    }
    v
}

/// The whole CLI as data.
pub fn describe() -> Value {
    json!({
        "name": "orbit",
        "version": env!("CARGO_PKG_VERSION"),
        "about": args::ABOUT,
        "output": "Human text by default. With --json, one line: {\"ok\":true,\"result\":…} or {\"ok\":false,\"error\":{\"code\",\"exit\",\"message\",\"hint\"}}. `log --follow`/`tail` with --json print one JSON event per line.",
        "selectors": {
            "thread": "A full id (thr_…), a unique part of an id or title, `latest`, `self` (ORBIT_THREAD) or `parent`.",
            "project": "An id, a name, a path, or `.` for the project containing the current folder.",
        },
        "environment": {
            "ORBIT_DATA": "Orbit's data folder (where endpoint.json is).",
            "ORBIT_HOST": "Engine URL, instead of the local endpoint.",
            "ORBIT_TOKEN": "Token for ORBIT_HOST.",
            "ORBIT_THREAD": "The thread this process runs in (set by Orbit for its agents).",
            "ORBIT_PARENT_THREAD": "Overrides ORBIT_THREAD as the parent of threads started from here.",
        },
        "global_args": GLOBAL.iter().map(arg_json).collect::<Vec<_>>(),
        "exit_codes": exit::ALL.iter().map(|(code, name, meaning)| json!({"code": code, "name": name, "meaning": meaning})).collect::<Vec<_>>(),
        "commands": SPECS.iter().map(spec_json).collect::<Vec<_>>(),
    })
}

pub fn help(command: Option<&str>, json_out: bool) -> Outcome {
    let words: Vec<&str> = command.map(|c| c.split_whitespace().collect()).unwrap_or_default();
    let spec = (!words.is_empty()).then(|| crate::spec::find(&words)).flatten();
    if json_out {
        let value = match spec {
            Some(spec) => spec_json(spec),
            None => describe(),
        };
        return Outcome { value, exit: exit::OK, human: None };
    }
    let mut cmd = args::command();
    let mut target = &mut cmd;
    for word in &words {
        match target.find_subcommand_mut(word) {
            Some(sub) => target = sub,
            None => {
                return Outcome {
                    value: Value::Null,
                    exit: exit::USAGE,
                    human: Some(format!("No command “{}”. Run `orbit help` for the list.", words.join(" "))),
                };
            }
        }
    }
    let text = target.render_long_help().to_string();
    Outcome { value: Value::Null, exit: exit::OK, human: Some(text.trim_end().to_owned()) }
}

pub fn completions(shell: &str) -> Result<String, CliError> {
    use clap_complete::Shell;
    let shell = match shell {
        "bash" => Shell::Bash,
        "zsh" => Shell::Zsh,
        "fish" => Shell::Fish,
        "powershell" => Shell::PowerShell,
        "elvish" => Shell::Elvish,
        other => return Err(CliError::usage(format!("No completions for “{other}”."))),
    };
    let mut buf = Vec::new();
    clap_complete::generate(shell, &mut args::command(), "orbit", &mut buf);
    Ok(String::from_utf8_lossy(&buf).trim_end().to_owned())
}

/// A concise guide for an agent driving Orbit; also the MCP server's
/// instructions.
pub fn agent_context() -> String {
    let mut out = String::from(
        "# Driving Orbit\n\n\
Orbit runs coding-agent threads (Claude Code, Codex). Each thread is one conversation, optionally in its own git \
worktree. You can start threads, message them, wait for them and read what they did, from the `orbit` CLI or the \
same operations as MCP tools (`orbit mcp`).\n\n\
## Delegating work (agent teams)\n\
1. Split the work into independent tasks with clear edit scopes.\n\
2. Start one thread per task with a self-contained prompt (goal, files, constraints, how to verify): \
`orbit new --worktree \"<prompt>\"` / tool `start_thread {prompt, worktree: true}`. Use a worktree whenever threads \
could touch the same files. Threads you start are linked to you and can't get more access than you have.\n\
3. Wait for all of them in one call instead of polling: `orbit wait <id> <id>` / tool `wait {threads}`. Its result \
has each thread's state and last message.\n\
4. If a thread needs an approval or answer, its request is in the result. Answer questions with `answer_question`; \
leave approvals to the user unless you have full access.\n\
5. Review with `orbit diff <id> --stat` / `get_diff`, follow up with `send_message`, and `archive_thread` helpers \
you no longer need.\n\n\
## Rules\n\
- Never poll in a loop: `wait` blocks until something changes (default timeout 10 minutes).\n\
- `send` queues after the running turn; `--steer` (tool: `steer: true`) injects into it.\n\
- Reading (`log`, `read_thread`, `status`) never marks a thread read for the user.\n\
- Thread selectors: full id, a unique part of an id or title, `latest`, `self`.\n\n\
## Exit codes (CLI)\n",
    );
    for (code, name, meaning) in exit::ALL {
        out.push_str(&format!("- {code} {name}: {meaning}\n"));
    }
    out.push_str("\n## Commands\n");
    for spec in SPECS.iter().filter(|s| s.connects) {
        let tool = spec.tool.as_ref().map(|t| format!(" (tool `{}`)", t.name)).unwrap_or_default();
        out.push_str(&format!("- `orbit {}`{tool}: {}\n", spec.name(), spec.about));
    }
    out.push_str("\nEvery command takes --json. `orbit help --json` describes every argument.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shell_gets_completions() {
        for shell in crate::spec::SHELLS {
            let script = completions(shell).unwrap();
            assert!(script.contains("orbit"), "{shell}");
            assert!(script.contains("needs-you"), "{shell} completes flags");
        }
    }

    #[test]
    fn json_help_lists_commands_tools_and_exit_codes() {
        let v = describe();
        let commands = v["commands"].as_array().unwrap();
        assert!(commands.iter().any(|c| c["command"] == "terminal run"));
        assert!(commands.iter().any(|c| c["mcp_tool"] == "start_thread"));
        assert_eq!(v["exit_codes"].as_array().unwrap().len(), exit::ALL.len());
        assert!(agent_context().contains("start_thread"));
    }

    #[test]
    fn help_for_one_command_is_its_long_help() {
        let out = help(Some("terminal run"), false);
        assert!(out.human.unwrap().contains("--detach"));
        assert_eq!(help(Some("nope"), false).exit, exit::USAGE);
    }
}
