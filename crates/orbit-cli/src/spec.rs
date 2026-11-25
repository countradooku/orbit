//! The command specs: the single source of truth for the CLI's parser,
//! `--help`, shell completions, `orbit help --json` and the MCP tools.
//!
//! A spec says what a command takes; [`crate::exec`] says what it does.
//! Adding a command means adding a spec here and an arm there; every
//! surface picks it up.

/// What kind of value an argument holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An on/off switch.
    Flag,
    /// One word or quoted string.
    Str,
    /// A non-negative whole number.
    Int,
    /// One of a fixed set.
    Choice(&'static [&'static str]),
    /// A thread selector: id, unique part of an id or title, `latest`, `self`.
    Thread,
    /// Several thread selectors.
    Threads,
    /// A project selector: id, name, path or `.` (the current folder).
    Project,
    /// A folder or file.
    Path,
    /// Free text; on the command line the remaining words, joined (`-`
    /// reads standard input).
    Text,
    /// A duration such as `90s`, `30m`, `2h` or `1d`.
    Duration,
    /// A command line: every remaining argument verbatim, flags included.
    Argv,
}

/// Where an argument exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Both,
    /// Command line only (streaming, terminal-only or implicit for tools).
    Cli,
}

#[derive(Debug)]
pub struct Arg {
    pub name: &'static str,
    pub kind: Kind,
    pub help: &'static str,
    pub required: bool,
    pub positional: bool,
    pub short: Option<char>,
    pub default: Option<&'static str>,
    /// A different default for MCP tool calls (bounded output for agents).
    pub tool_default: Option<&'static str>,
    pub scope: Scope,
}

/// An MCP tool generated from a spec.
#[derive(Debug)]
pub struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
}

#[derive(Debug)]
pub struct Spec {
    /// `["threads"]`, `["terminal", "run"]`.
    pub path: &'static [&'static str],
    pub about: &'static str,
    /// More detail for `--help` and tool descriptions.
    pub details: &'static str,
    pub args: &'static [Arg],
    pub tool: Option<Tool>,
    /// Needs a running engine.
    pub connects: bool,
    pub aliases: &'static [&'static str],
    pub examples: &'static [&'static str],
}

impl Spec {
    pub fn name(&self) -> String {
        self.path.join(" ")
    }

    pub fn arg(&self, name: &str) -> Option<&Arg> {
        self.args.iter().find(|a| a.name == name)
    }
}

const fn arg(name: &'static str, kind: Kind, help: &'static str) -> Arg {
    Arg {
        name,
        kind,
        help,
        required: false,
        positional: false,
        short: None,
        default: None,
        tool_default: None,
        scope: Scope::Both,
    }
}

const fn pos(name: &'static str, kind: Kind, help: &'static str) -> Arg {
    Arg { positional: true, required: true, ..arg(name, kind, help) }
}

const fn opt_pos(name: &'static str, kind: Kind, help: &'static str) -> Arg {
    Arg { positional: true, ..arg(name, kind, help) }
}

impl Arg {
    const fn short(self, c: char) -> Self {
        Arg { short: Some(c), ..self }
    }
    const fn default(self, value: &'static str) -> Self {
        Arg { default: Some(value), ..self }
    }
    const fn tool_default(self, value: &'static str) -> Self {
        Arg { tool_default: Some(value), ..self }
    }
    const fn cli(self) -> Self {
        Arg { scope: Scope::Cli, ..self }
    }
}

const fn read(name: &'static str, title: &'static str) -> Option<Tool> {
    Some(Tool { name, title, read_only: true, destructive: false, idempotent: true })
}

const fn write(name: &'static str, title: &'static str) -> Option<Tool> {
    Some(Tool { name, title, read_only: false, destructive: false, idempotent: false })
}

const fn spec(path: &'static [&'static str], about: &'static str, args: &'static [Arg]) -> Spec {
    Spec { path, about, details: "", args, tool: None, connects: true, aliases: &[], examples: &[] }
}

pub const STATES: &[&str] = &["working", "needs-you", "idle", "unread", "failed", "rate-limited", "starting"];
pub const UNTIL: &[&str] = &["finished", "idle", "needs-you"];
pub const MODES: &[&str] = &["build", "plan"];
pub const ACCESS: &[&str] = &["readonly", "ask", "auto-edit", "full"];
pub const SHELLS: &[&str] = &["bash", "zsh", "fish", "powershell", "elvish"];
pub const CLIENTS: &[&str] = &["claude", "codex", "all"];
pub const TASKS: &[&str] = &["title", "commit", "pr", "custom"];

const THREAD: Arg = pos("thread", Kind::Thread, "The thread: its id, a unique part of its id or title, `latest`, or `self`.");
const REQUEST: Arg = arg("request", Kind::Str, "The request id, when the thread has more than one pending.");
const TIMEOUT: Arg = arg("timeout", Kind::Duration, "Give up after this long (`90s`, `10m`; `0` waits forever).").default("10m");
const WAIT: Arg = arg("wait", Kind::Flag, "Wait until the thread finishes or needs you, then print its last message.");

/// Arguments every command accepts on the command line.
pub static GLOBAL: &[Arg] = &[
    arg("json", Kind::Flag, "Print one line of JSON: {\"ok\":true,\"result\":…} or {\"ok\":false,\"error\":…}."),
    arg("host", Kind::Str, "Engine URL (`ws://host:port/`); default: the local engine from endpoint.json, or ORBIT_HOST."),
    arg("token", Kind::Str, "Access token for --host; default ORBIT_TOKEN."),
    arg("data", Kind::Path, "Orbit's data folder, where endpoint.json is; default ORBIT_DATA or the platform folder."),
];

pub static SPECS: &[Spec] = &[
    Spec {
        details: "Without a thread: the engine, how many threads are working or need you, and the agents installed. \
                  With a thread: its state, pending requests, usage and last message.",
        tool: read("status", "Orbit status"),
        examples: &["orbit status", "orbit status latest --json"],
        ..spec(&["status"], "Show what Orbit and its threads are doing", &[opt_pos("thread", Kind::Thread, "Show this thread in detail.")])
    },
    Spec {
        tool: read("list_projects", "List projects"),
        aliases: &["project"],
        ..spec(&["projects"], "List projects", &[])
    },
    Spec {
        details: "The folder must exist; adding a folder that is already a project returns that project.",
        tool: write("add_project", "Add a project"),
        examples: &["orbit projects add ."],
        ..spec(&["projects", "add"], "Add a folder as a project", &[pos("path", Kind::Path, "The folder.")])
    },
    Spec {
        details: "Active threads by default (not archived or settled), most recently updated first.",
        tool: read("list_threads", "List threads"),
        aliases: &["ls", "thread"],
        examples: &["orbit threads --needs-you", "orbit threads --project . --status working"],
        ..spec(
            &["threads"],
            "List threads",
            &[
                arg("status", Kind::Choice(STATES), "Only threads in this state."),
                arg("needs-you", Kind::Flag, "Only threads waiting on an approval or answer."),
                arg("project", Kind::Project, "Only this project's threads.").short('p'),
                arg("parent", Kind::Thread, "Only threads started by this thread (`self` for yours)."),
                arg("all", Kind::Flag, "Include archived and settled threads."),
                arg("limit", Kind::Int, "Show at most this many (0 for all).").default("0").tool_default("50"),
            ],
        )
    },
    Spec {
        details: "Starts a thread and sends the prompt. Model, effort, mode and access default to the parent thread's \
                  (when started from one) or the user's last used settings. --worktree gives the thread its own git \
                  worktree on a new branch, so parallel threads never edit the same files. Inside an Orbit thread the new \
                  thread is linked to it as its parent, and its access can't exceed the parent's.",
        tool: write("start_thread", "Start a thread"),
        aliases: &["spawn"],
        examples: &[
            "orbit new \"Fix the flaky login test\" --worktree",
            "orbit new --project api --model opus --effort high --wait \"Explain the auth flow\"",
            "git diff | orbit new - --mode plan",
        ],
        ..spec(
            &["new"],
            "Start a thread with a prompt",
            &[
                pos("prompt", Kind::Text, "The first message (`-` reads standard input)."),
                arg("project", Kind::Project, "The project; default: the parent's, else the one containing the current folder.").short('p'),
                arg("worktree", Kind::Flag, "Work in a new git worktree on its own branch.").short('w'),
                arg("provider", Kind::Str, "The agent: `claude`, `codex`, or a configured instance's id."),
                arg("model", Kind::Str, "The model slug (e.g. `opus`, `gpt-5-codex`).").short('m'),
                arg("effort", Kind::Str, "Reasoning effort, as the model reports it (e.g. `low`, `high`).").short('e'),
                arg("mode", Kind::Choice(MODES), "`plan` proposes a plan for approval before acting."),
                arg("access", Kind::Choice(ACCESS), "What the agent may do without asking."),
                arg("title", Kind::Str, "A title; default: from the prompt."),
                arg("parent", Kind::Thread, "Link to this thread as its parent; default: the thread this runs in.").cli(),
                WAIT,
                TIMEOUT,
            ],
        )
    },
    Spec {
        details: "While a turn runs the message is queued for after it, unless --steer injects it into the running turn.",
        tool: write("send_message", "Send a message"),
        examples: &["orbit send latest \"Also update the docs\"", "orbit send thr_1a2b --steer \"Stop, use the v2 API\""],
        ..spec(
            &["send"],
            "Send a message to a thread",
            &[
                THREAD,
                pos("message", Kind::Text, "The message (`-` reads standard input)."),
                arg("steer", Kind::Flag, "Inject into the running turn instead of queueing after it.").short('s'),
                WAIT,
                TIMEOUT,
            ],
        )
    },
    Spec {
        details: "Returns when the threads stop working. --until finished (default) stops at the first of: idle, needs \
                  you, failed. --until idle keeps waiting while someone else answers requests. --until needs-you stops \
                  only when a thread asks for something. The exit code tells what happened (see `orbit help --json`).",
        tool: Some(Tool { name: "wait", title: "Wait for threads", read_only: true, destructive: false, idempotent: true }),
        examples: &["orbit wait latest", "orbit wait thr_1 thr_2 --any --timeout 30m", "orbit wait self --until needs-you"],
        ..spec(
            &["wait"],
            "Wait until threads finish, go idle or need you",
            &[
                pos("threads", Kind::Threads, "One or more threads."),
                arg("until", Kind::Choice(UNTIL), "What to wait for.").default("finished").short('u'),
                arg("any", Kind::Flag, "Return when the first thread is done instead of all."),
                TIMEOUT,
            ],
        )
    },
    Spec {
        details: "Items in order: messages, tool calls, requests and turn summaries. Reading does not mark the thread read.",
        tool: read("read_thread", "Read a thread"),
        aliases: &["show"],
        examples: &["orbit log latest", "orbit log thr_1a2b --follow", "orbit log latest --limit 5 --json"],
        ..spec(
            &["log"],
            "Print a thread's timeline",
            &[
                THREAD,
                arg("limit", Kind::Int, "Only the newest N items (0 for all).").default("40").short('n'),
                arg("after", Kind::Str, "Only items after this item id (for polling)."),
                arg("full", Kind::Flag, "Include reasoning and tool input and output."),
                arg("follow", Kind::Flag, "Keep printing as the thread grows (Ctrl+C to stop).").short('f').cli(),
                arg("until", Kind::Choice(UNTIL), "With --follow: stop like `orbit wait --until`.").cli(),
            ],
        )
    },
    Spec {
        details: "Same as `orbit log --follow`.",
        examples: &["orbit tail latest --until finished"],
        ..spec(
            &["tail"],
            "Stream a thread's timeline as it grows",
            &[
                THREAD,
                arg("limit", Kind::Int, "Start with the newest N items.").default("10").short('n'),
                arg("full", Kind::Flag, "Include reasoning and tool input and output."),
                arg("until", Kind::Choice(UNTIL), "Stop like `orbit wait --until`."),
            ],
        )
    },
    Spec {
        details: "Allows the pending command, file change, tool or plan.",
        tool: write("approve_request", "Approve a request"),
        examples: &["orbit approve latest", "orbit approve thr_1a2b --session"],
        ..spec(
            &["approve"],
            "Approve a thread's pending request",
            &[THREAD, REQUEST, arg("session", Kind::Flag, "Also allow similar requests for the rest of the session.")],
        )
    },
    Spec {
        tool: write("deny_request", "Deny a request"),
        examples: &["orbit deny latest --reason \"Use the staging database\""],
        ..spec(
            &["deny"],
            "Deny a thread's pending request",
            &[THREAD, REQUEST, arg("reason", Kind::Str, "Tell the agent why or what to do instead.").short('r')],
        )
    },
    Spec {
        details: "Answers a question the agent asked: one of its suggested options or free text.",
        tool: write("answer_question", "Answer a question"),
        examples: &["orbit answer latest \"Use SQLite\""],
        ..spec(&["answer"], "Answer a thread's pending question", &[THREAD, pos("text", Kind::Text, "The answer."), REQUEST])
    },
    Spec {
        details: "Interrupts the running turn; queued messages stay queued.",
        tool: write("stop_thread", "Stop a thread"),
        aliases: &["interrupt"],
        ..spec(
            &["stop"],
            "Stop a thread's running turn",
            &[THREAD, arg("force", Kind::Flag, "End the agent's process now, for when a normal stop did not take.")],
        )
    },
    Spec {
        details: "Runs the last failed or rate-limited turn again from where it started; the failed attempt stays in the timeline.",
        tool: write("retry_thread", "Retry a failed turn"),
        ..spec(&["retry"], "Retry a thread's failed turn", &[THREAD])
    },
    Spec {
        details: "Ends the agent's session; the next message starts a fresh one in the same thread (what /clear does).",
        aliases: &["clear"],
        ..spec(&["new-session"], "Start the thread's agent afresh", &[THREAD])
    },
    Spec {
        details: "A readable digest of the thread's messages and replies, for another agent or a person to read.",
        tool: read("export_thread", "Export a thread's digest"),
        ..spec(&["export"], "Print a thread's digest", &[THREAD])
    },
    Spec {
        details: "Merges the thread's branch into another branch locally, without a pull request. Uncommitted work is                   committed first. --dry-run previews commits, files and conflicts without changing anything.",
        tool: Some(Tool { name: "merge_thread", title: "Merge a thread's branch", read_only: false, destructive: true, idempotent: false }),
        examples: &["orbit merge latest --dry-run", "orbit merge thr_1a2b --into main"],
        ..spec(
            &["merge"],
            "Merge a thread's branch locally",
            &[
                THREAD,
                arg("into", Kind::Str, "The branch to merge into; default: the project's base branch."),
                arg("message", Kind::Str, "Commit message for uncommitted work; default: the thread's title.").short('m'),
                arg("dry-run", Kind::Flag, "Preview only."),
            ],
        )
    },
    Spec {
        details: "Fuzzy-finds files and folders in a thread's working folder, or a project's.",
        tool: read("search_files", "Find files"),
        ..spec(
            &["files"],
            "Find files in a thread's or project's folder",
            &[
                pos("query", Kind::Text, "Part of a path."),
                arg("thread", Kind::Thread, "Search this thread's working folder.").short('t'),
                arg("project", Kind::Project, "Search this project's folder; default: the current one.").short('p'),
                arg("limit", Kind::Int, "At most this many.").default("50"),
            ],
        )
    },
    Spec {
        details: "Moves a queued message within the queue (0 = next), or with --now sends it right away (into the                   running turn, or as the next turn).",
        examples: &["orbit queue latest itm_18c0 --now", "orbit queue latest itm_18c0 --to 0"],
        ..spec(
            &["queue"],
            "Reorder or send a queued message",
            &[
                THREAD,
                pos("item", Kind::Str, "The queued message's item id."),
                arg("to", Kind::Int, "Its new place in the queue (0 = next)."),
                arg("now", Kind::Flag, "Send it now instead of waiting."),
            ],
        )
    },
    Spec {
        details: "Keeps an unsent message in the thread's composer (or the new-thread composer with --home) in every                   client; empty text clears it.",
        ..spec(
            &["draft"],
            "Save a draft in a thread's composer",
            &[
                opt_pos("thread", Kind::Thread, "The thread; omit with --home."),
                arg("text", Kind::Str, "The draft (`-` reads standard input); empty clears it."),
                arg("home", Kind::Flag, "The new-thread composer instead of a thread's."),
            ],
        )
    },
    Spec {
        details: "One-shot text from an available agent in a cheap mode: a thread title, a commit message, a pull                   request title and body, or anything with --instructions.",
        examples: &["git diff --staged | orbit generate commit -", "orbit generate custom --instructions \"Summarize\" -"],
        ..spec(
            &["generate"],
            "Generate text with an agent",
            &[
                pos("task", Kind::Choice(TASKS), "What to write."),
                pos("input", Kind::Text, "The material (`-` reads standard input)."),
                arg("instructions", Kind::Str, "For `custom`: what to do with the input."),
                arg("thread", Kind::Thread, "Use this thread's agent.").short('t'),
            ],
        )
    },
    Spec {
        details: "Done for now: kept and searchable, out of the active list. A new message brings it back.",
        tool: write("settle_thread", "Settle a thread"),
        ..spec(&["settle"], "Mark a thread done for now", &[THREAD, arg("undo", Kind::Flag, "Bring it back to the active list.")])
    },
    Spec {
        details: "Hides the thread until the time passes; it keeps running and wakes early if it needs you or fails.",
        examples: &["orbit snooze latest 2h", "orbit snooze latest --wake"],
        ..spec(
            &["snooze"],
            "Hide a thread for a while",
            &[
                THREAD,
                opt_pos("for", Kind::Duration, "How long (`30m`, `2h`, `1d`)."),
                arg("wake", Kind::Flag, "End the snooze now."),
            ],
        )
    },
    Spec {
        tool: Some(Tool { name: "archive_thread", title: "Archive a thread", read_only: false, destructive: false, idempotent: true }),
        ..spec(&["archive"], "Archive a thread (read-only, out of the lists)", &[THREAD, arg("undo", Kind::Flag, "Unarchive it.")])
    },
    Spec {
        details: "The whole thread's changes by default. Human output is a unified diff you can pipe to `git apply`.",
        tool: read("get_diff", "Get a thread's diff"),
        examples: &["orbit diff latest --stat", "orbit diff thr_1a2b --last > last-turn.patch", "orbit diff latest --base main"],
        ..spec(
            &["diff"],
            "Show what a thread changed",
            &[
                THREAD,
                arg("turn", Kind::Int, "Only this turn's changes."),
                arg("since-turn", Kind::Int, "Everything from before this turn to now (what undoing it would revert)."),
                arg("last", Kind::Flag, "Only the last turn's changes."),
                arg("base", Kind::Str, "The working tree against this branch or commit."),
                arg("uncommitted", Kind::Flag, "Uncommitted changes in the thread's working tree."),
                arg("stat", Kind::Flag, "Files and line counts only."),
                arg("max-bytes", Kind::Int, "Cut the patch after this many bytes (0 for no limit).").default("0").tool_default("200000"),
            ],
        )
    },
    Spec {
        details: "Restores the files to how they were before the turn (the last one by default) and every turn after it. \
                  The current state is checkpointed first, so this can itself be undone. The conversation is kept.",
        ..spec(
            &["undo-turn"],
            "Restore the files to before a turn",
            &[
                THREAD,
                arg("turn", Kind::Int, "The turn to undo, with everything after it; default: the last."),
                arg("redo", Kind::Flag, "Undo the last restore instead (put the files back)."),
            ],
        )
    },
    Spec {
        details: "Runs through your shell in a new terminal, in the thread's worktree (--thread), a folder (--cwd) or the \
                  current folder. Streams the output and exits with the command's exit code.",
        examples: &["orbit terminal run --thread latest cargo test", "orbit terminal run --detach npm run dev"],
        ..spec(
            &["terminal", "run"],
            "Run a command in a terminal",
            &[
                pos("command", Kind::Argv, "The command line (one quoted string, or the remaining arguments)."),
                arg("thread", Kind::Thread, "Run in this thread's working folder.").short('t'),
                arg("cwd", Kind::Path, "Run in this folder."),
                arg("detach", Kind::Flag, "Print the terminal id and return; read it later with `orbit terminal read`."),
                arg("raw", Kind::Flag, "Keep escape sequences."),
                arg("timeout", Kind::Duration, "Kill it after this long (0: never).").default("0"),
            ],
        )
    },
    Spec {
        ..spec(
            &["terminal", "read"],
            "Print a terminal's recent output",
            &[
                pos("terminal", Kind::Str, "The terminal id."),
                arg("bytes", Kind::Int, "At most this many bytes from the end (0 for all that is kept).").default("0"),
                arg("raw", Kind::Flag, "Keep escape sequences."),
            ],
        )
    },
    Spec { ..spec(&["terminal", "list"], "List terminals", &[]) },
    Spec {
        tool: read("search", "Search threads"),
        examples: &["orbit search \"rate limit\""],
        ..spec(
            &["search"],
            "Search every thread's messages",
            &[pos("query", Kind::Text, "Words to find."), arg("limit", Kind::Int, "At most this many hits.").default("20")],
        )
    },
    Spec {
        details: "Brings the desktop app forward and shows the thread. Starts the app when it is not running.",
        examples: &["orbit open latest"],
        ..spec(&["open"], "Show a thread in the desktop app", &[opt_pos("thread", Kind::Thread, "The thread to show.")])
    },
    Spec {
        connects: false,
        ..spec(&["agent-context"], "Print a guide for agents driving Orbit", &[])
    },
    Spec {
        details: "With --json: every command, argument, exit code and MCP tool, for agents and tools.",
        examples: &["orbit help new", "orbit help --json"],
        connects: false,
        ..spec(
            &["help"],
            "Show help",
            &[Arg { positional: true, ..arg("command", Kind::Text, "The command.") }],
        )
    },
    Spec {
        connects: false,
        examples: &["orbit completions zsh > ~/.zfunc/_orbit", "orbit completions powershell >> $PROFILE"],
        ..spec(&["completions"], "Print a shell completion script", &[pos("shell", Kind::Choice(SHELLS), "The shell.")])
    },
    Spec {
        details: "Speaks MCP (JSON-RPC 2.0 over standard input and output) so agents can list threads, start threads in \
                  worktrees, send messages, wait and read results. Inside an Orbit thread (ORBIT_THREAD, or \
                  ORBIT_PARENT_THREAD) the threads it starts are linked to that thread.",
        connects: false,
        ..spec(&["mcp"], "Run the MCP server on standard input and output", &[])
    },
    Spec {
        details: "Prints the commands that register `orbit mcp` with Claude Code and Codex. --apply runs `claude mcp add` and \
                  writes ~/.codex/config.toml.",
        connects: false,
        examples: &["orbit mcp install", "orbit mcp install --client codex --apply"],
        ..spec(
            &["mcp", "install"],
            "Register the MCP server with Claude Code and Codex",
            &[
                arg("client", Kind::Choice(CLIENTS), "Which agent.").default("all"),
                arg("apply", Kind::Flag, "Change the agents' configuration instead of printing it."),
            ],
        )
    },
];

/// The spec for a command path, e.g. `["terminal", "run"]`.
pub fn find(path: &[&str]) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.path == path)
}

pub fn tool(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.tool.as_ref().is_some_and(|t| t.name == name))
}

/// Process exit codes, documented in `orbit help --json` and docs/CLI.md.
pub mod exit {
    pub const OK: i32 = 0;
    pub const ERROR: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const UNAVAILABLE: i32 = 3;
    pub const NOT_FOUND: i32 = 4;
    pub const TIMEOUT: i32 = 5;
    pub const NEEDS_YOU: i32 = 6;
    pub const FAILED: i32 = 7;
    pub const IDLE: i32 = 8;

    pub const ALL: &[(i32, &str, &str)] = &[
        (OK, "ok", "Success; for `wait`, the awaited state was reached."),
        (ERROR, "error", "The engine refused or failed the command."),
        (USAGE, "usage", "Bad arguments."),
        (UNAVAILABLE, "unavailable", "No running Orbit engine, or it could not be reached or refused the token."),
        (NOT_FOUND, "not_found", "No thread, project, request or terminal matches, or more than one does."),
        (TIMEOUT, "timeout", "`wait` (or --wait) gave up after --timeout."),
        (NEEDS_YOU, "needs_you", "`wait` stopped because a thread needs an approval or answer."),
        (FAILED, "failed", "`wait` stopped because a thread failed or hit its rate limit."),
        (IDLE, "idle", "`wait --until needs-you` stopped because a thread finished without needing you."),
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_are_consistent() {
        let mut tools = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        for spec in SPECS {
            assert!(names.insert(spec.name()), "duplicate command {}", spec.name());
            if let Some(tool) = &spec.tool {
                assert!(tools.insert(tool.name), "duplicate tool {}", tool.name);
                assert!(spec.connects);
            }
            let mut args = std::collections::HashSet::new();
            let mut optional_positional = false;
            for arg in spec.args {
                assert!(args.insert(arg.name), "{}: duplicate argument {}", spec.name(), arg.name);
                assert!(!GLOBAL.iter().any(|g| g.name == arg.name), "{}: {} shadows a global", spec.name(), arg.name);
                if arg.positional {
                    assert!(!(optional_positional && arg.required), "{}: required after optional", spec.name());
                    optional_positional |= !arg.required;
                }
                if let (Some(default), Kind::Choice(choices)) = (arg.default, arg.kind) {
                    assert!(choices.contains(&default), "{}: default {default} not a choice", spec.name());
                }
            }
            assert!(!spec.about.ends_with('.'), "{}: about is a phrase", spec.name());
        }
    }
}
