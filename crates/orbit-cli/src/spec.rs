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
pub const PR_FILTERS: &[&str] = &["open", "mine", "merged", "closed"];
pub const MERGE_METHODS: &[&str] = &["squash", "merge", "rebase"];
pub const TRACKERS: &[&str] = &["github", "gitlab", "linear", "jira"];
pub const PLATFORMS: &[&str] = &["telegram", "slack", "discord"];

const PROJECT: Arg = arg("project", Kind::Project, "The project; default: the one containing the current folder.").short('p');
const PR: Arg = pos("number", Kind::Str, "The pull request number (`42`, `#42`).");
const REFRESH: Arg = arg("refresh", Kind::Flag, "Fetch fresh data instead of a seconds-old result.");
const TRACKER: Arg = arg("tracker", Kind::Choice(TRACKERS), "The tracker; default: the project's host.");
const KEY: Arg = pos("key", Kind::Str, "The issue (`42`, `ENG-123`, `PROJ-7`).");

pub const AUTOMATION_EVENTS: &[&str] =
    &["checks-failed", "my-checks-failed", "review-requested", "issue-labeled", "branch-pushed", "watch-pr", "after"];
pub const CHAIN_WHEN: &[&str] = &["succeeded", "failed", "finished"];
pub const TARGETS: &[&str] = &["worktree", "project", "pr"];
pub const NOTIFY: &[&str] = &["always", "failures", "never"];
/// Built-in recipe ids (checked against the engine's gallery in tests).
pub const RECIPES: &[&str] =
    &["nightly-deps", "fix-ci", "triage-issues", "standup", "keep-rebased", "review-requests", "watch-pr"];
const AUTOMATION: Arg = pos("automation", Kind::Str, "The automation: its id, or its name or a unique part of it.");
pub const MAIL_KINDS: &[&str] = &["message", "question", "handoff", "status", "done"];
const TEAM_AS: Arg = arg("as", Kind::Thread, "Act as this team member; default: the thread this runs in.").cli();

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
        tool: read("list_pull_requests", "List pull requests"),
        examples: &["orbit pr list --filter mine"],
        ..spec(
            &["pr", "list"],
            "List the project's pull requests",
            &[PROJECT, arg("filter", Kind::Choice(PR_FILTERS), "Which ones.").default("open"), REFRESH],
        )
    },
    Spec {
        details: "Checks, reviews, mergeability and comments.",
        tool: read("get_pull_request", "Get a pull request"),
        ..spec(&["pr", "show"], "Show a pull request", &[PR, PROJECT, REFRESH])
    },
    Spec {
        details: "Merges on the host (GitHub or GitLab), or with --auto merges once checks and reviews pass.",
        ..spec(
            &["pr", "merge"],
            "Merge a pull request",
            &[
                PR,
                PROJECT,
                arg("method", Kind::Choice(MERGE_METHODS), "How.").default("squash"),
                arg("auto", Kind::Flag, "Merge when its checks and reviews pass."),
                arg("delete-branch", Kind::Flag, "Delete the branch after merging."),
            ],
        )
    },
    Spec {
        details: "Opens the pull request's branch in a thread on its own worktree, or returns the thread that has it.",
        ..spec(&["pr", "checkout"], "Work on a pull request in a thread", &[PR, PROJECT])
    },
    Spec {
        details: "Sends the failing checks' names and log excerpts to a thread (the one on the branch by default) to fix.",
        ..spec(
            &["pr", "fix-checks"],
            "Have an agent fix a pull request's failing checks",
            &[PR, PROJECT, arg("thread", Kind::Thread, "Send them to this thread.").short('t')],
        )
    },
    Spec {
        details: "Issues assigned to you, or matching --query.",
        tool: read("list_issues", "List issues"),
        examples: &["orbit issue list", "orbit issue list --tracker linear --query \"login\""],
        ..spec(
            &["issue", "list"],
            "List issues",
            &[PROJECT, TRACKER, arg("query", Kind::Str, "Search text instead of your assigned issues.").short('q'), REFRESH],
        )
    },
    Spec {
        tool: read("get_issue", "Get an issue"),
        ..spec(&["issue", "show"], "Show an issue", &[KEY, PROJECT, TRACKER, REFRESH])
    },
    Spec {
        details: "Starts a thread on its own branch and worktree (unless --here) with the issue as its first prompt.",
        examples: &["orbit issue start ENG-123 --tracker linear"],
        ..spec(
            &["issue", "start"],
            "Start a thread from an issue",
            &[KEY, PROJECT, TRACKER, arg("here", Kind::Flag, "Work in the project's checkout instead of a new worktree.")],
        )
    },
    Spec {
        details: "Uncommitted, committed, pushed, checks running or failing, mergeable, merged: where a thread's work stands.",
        tool: read("ship_status", "Where a thread's work stands"),
        ..spec(&["ship"], "Show where a thread's work stands", &[opt_pos("thread", Kind::Thread, "The thread; default: latest."), REFRESH])
    },
    // ── Automations (scheduled and event-triggered runs) ────────────────
    Spec {
        details: "Every automation with its schedule or events, when it runs next and how its last run went.",
        tool: read("list_automations", "List automations"),
        aliases: &["automations"],
        examples: &["orbit automation list", "orbit automation list --project api --json"],
        ..spec(&["automation", "list"], "List automations", &[arg("project", Kind::Project, "Only this project's automations.").short('p')])
    },
    Spec {
        details: "Its prompt, triggers, checks, limits, what happens after a run, and its newest runs.",
        tool: read("get_automation", "Show an automation"),
        ..spec(&["automation", "show"], "Show an automation", &[AUTOMATION])
    },
    Spec {
        details: "Starts an agent on a schedule (--cron, --rule, --every) or when something happens (--on). Each run is a \
                  thread: in a new worktree by default, in the project folder (--target project), in the pull request's \
                  branch (--target pr) or as a new message in one thread (--thread). The prompt can use {date}, {time}, \
                  {branch}, {pr}, {issue}, {event}, {last_run_summary}, {project} and {name}. --recipe starts from a \
                  built-in recipe; other options change it.",
        tool: write("create_automation", "Create an automation"),
        examples: &[
            "orbit automation create --name \"Nightly deps\" --cron \"0 2 * * *\" --open-pr \"Update outdated dependencies\"",
            "orbit automation create --rule \"FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=9\" --target project \"Standup for {date}\"",
            "orbit automation create --on my-checks-failed --target pr \"Fix the failing checks on {pr}\"",
            "orbit automation create --recipe triage-issues --label bug",
        ],
        ..spec(
            &["automation", "create"],
            "Create an automation",
            &[
                Arg { required: false, ..pos("prompt", Kind::Text, "What the agent does each run (`-` reads standard input).") },
                arg("name", Kind::Str, "Its name; default: the prompt's first words."),
                PROJECT,
                arg("recipe", Kind::Choice(RECIPES), "Start from a built-in recipe."),
                arg("cron", Kind::Str, "A five-field cron schedule (`0 9 * * 1-5`)."),
                arg("rule", Kind::Str, "A recurrence rule (`FREQ=WEEKLY;BYDAY=MO;BYHOUR=9`)."),
                arg("every", Kind::Duration, "Run at this interval (`30m`, `2h`)."),
                arg("tz", Kind::Str, "The schedule's time zone (`Europe/London`); default: this computer's."),
                arg("on", Kind::Choice(AUTOMATION_EVENTS), "Run when this happens."),
                arg("label", Kind::Str, "For --on issue-labeled: the label."),
                arg("branch", Kind::Str, "For --on branch-pushed: the branch."),
                arg("pr", Kind::Int, "For --on watch-pr: the pull request number."),
                arg("after", Kind::Str, "For --on after: the automation whose runs start this one."),
                arg("when", Kind::Choice(CHAIN_WHEN), "For --on after: which endings count.").default("succeeded"),
                arg("target", Kind::Choice(TARGETS), "Where each run works; default: a new worktree."),
                arg("thread", Kind::Thread, "Continue this thread on every run instead.").short('t'),
                arg("provider", Kind::Str, "The agent (`claude`, `codex`, or an instance id); default: your last used."),
                arg("model", Kind::Str, "The model."),
                arg("effort", Kind::Str, "Reasoning effort (`low`, `medium`, `high`…)."),
                arg("mode", Kind::Choice(MODES), "Build or plan."),
                arg("access", Kind::Choice(ACCESS), "What the agent may do without asking; default: auto-edit."),
                arg("only-if-changed", Kind::Flag, "Skip runs when the project hasn't changed since the last one."),
                arg("only-if-clean", Kind::Flag, "Skip runs while the project has uncommitted changes."),
                arg("check", Kind::Str, "A shell command that must succeed before each run."),
                arg("overlap", Kind::Flag, "Start a run even while the previous one is still going."),
                arg("max-per-day", Kind::Int, "At most this many runs a day."),
                arg("timeout", Kind::Duration, "Stop a run after this long (`0` for never); default: 60m."),
                arg("grace", Kind::Duration, "Still start a run missed while asleep if it is at most this late; default: 30m."),
                arg("notify", Kind::Choice(NOTIFY), "When a finished run asks for attention; default: always."),
                arg("settle-if-unchanged", Kind::Flag, "Settle a run's thread when it changed nothing."),
                arg("open-pr", Kind::Flag, "Commit, push and open a pull request with a run's changes (worktree runs)."),
                arg("paused", Kind::Flag, "Create it paused."),
            ],
        )
    },
    Spec {
        details: "Starts a run now, without its checks or daily limit. With --wait, waits for the run's thread to finish.",
        tool: write("run_automation", "Run an automation now"),
        examples: &["orbit automation run \"Nightly deps\" --wait"],
        ..spec(&["automation", "run"], "Run an automation now", &[AUTOMATION, WAIT, TIMEOUT])
    },
    Spec {
        details: "Paused automations start no runs; runs waiting to start are dropped.",
        tool: write("pause_automation", "Pause an automation"),
        ..spec(&["automation", "pause"], "Pause an automation", &[AUTOMATION])
    },
    Spec {
        details: "Its schedule counts from now: runs it would have had while paused don't happen.",
        tool: write("resume_automation", "Resume an automation"),
        ..spec(&["automation", "resume"], "Resume a paused automation", &[AUTOMATION])
    },
    Spec {
        details: "Deletes the automation and its run history. Threads its runs made stay.",
        tool: Some(Tool { name: "delete_automation", title: "Delete an automation", read_only: false, destructive: true, idempotent: true }),
        ..spec(&["automation", "delete"], "Delete an automation", &[AUTOMATION])
    },
    Spec {
        details: "Newest first: when each run was due, how it went, how long it took, its tokens, thread and summary.",
        tool: read("automation_history", "An automation's runs"),
        examples: &["orbit automation history \"Nightly deps\" --limit 5"],
        ..spec(
            &["automation", "history"],
            "Show an automation's runs",
            &[AUTOMATION, arg("limit", Kind::Int, "At most this many (0 for all).").default("20")],
        )
    },
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
        details: "Brings the desktop app forward and shows the thread, through the app's own single-instance hand-off                   (the same as launching `orbit-desktop --thread <id>`). Starts the app when it is not running.",
        examples: &["orbit open latest"],
        ..spec(&["open"], "Show a thread in the desktop app", &[opt_pos("thread", Kind::Thread, "The thread to show.")])
    },
    // ── Chat bots (orbit-bots) ────────────────────────────────────────────
    // Not MCP tools: letting people in is for the owner, never an agent.
    Spec {
        details: "Each platform's connection (and its last error), the people paired with the bots, and pairing codes \
                  waiting for approval.",
        examples: &["orbit bot status", "orbit bot status --json"],
        ..spec(&["bot", "status"], "Show the chat bots and who may use them", &[])
    },
    Spec {
        details: "Someone who writes to a bot gets a six-digit code; this lets them in (or turns them away with \
                  --reject). Codes expire after 15 minutes. Refused inside an agent's thread.",
        examples: &["orbit bot pair 482913"],
        ..spec(
            &["bot", "pair"],
            "Approve a pairing code",
            &[pos("code", Kind::Str, "The code the bot sent them."), arg("reject", Kind::Flag, "Turn them away instead.")],
        )
    },
    Spec {
        details: "Their chats stop driving Orbit at once. Their user id is in `orbit bot status`.",
        examples: &["orbit bot unpair telegram 123456789"],
        ..spec(
            &["bot", "unpair"],
            "Remove a paired person",
            &[pos("platform", Kind::Choice(PLATFORMS), "Where they are."), pos("user", Kind::Str, "Their user id there.")],
        )
    },
    Spec {
        details: "Lets a person in by their platform user id, without a code (Telegram: a number; Slack: U…; Discord: a \
                  snowflake). Refused inside an agent's thread.",
        examples: &["orbit bot allow slack U024BE7LH --name Ana"],
        ..spec(
            &["bot", "allow"],
            "Allow a person by user id",
            &[
                pos("platform", Kind::Choice(PLATFORMS), "Where they are."),
                pos("user", Kind::Str, "Their user id there."),
                arg("name", Kind::Str, "How to show them; default: the id."),
            ],
        )
    },
    // ── End of chat bots ─────────────────────────────────────────────────
    // ── Agent teams and races ───────────────────────────────────────────
    Spec {
        details: "Starts a thread as the architect of a new agent team. Its sessions get the team tools (team_spawn_worker, \
                  team_message, team_post_note, team_read_notes, team_status, team_merge_worker) as Orbit MCP tools, for \
                  that session only. Workers run in their own worktrees off the architect's branch.",
        examples: &["orbit team start \"Add OAuth sign-in across the API and the web app\" --max-workers 3"],
        ..spec(
            &["team", "start"],
            "Start a thread as a team's architect",
            &[
                pos("prompt", Kind::Text, "The architect's first message (`-` reads standard input)."),
                PROJECT,
                arg("here", Kind::Flag, "Lead from the project's checkout instead of a new worktree."),
                arg("provider", Kind::Str, "The architect's agent: `claude`, `codex`, or a configured instance's id."),
                arg("model", Kind::Str, "The architect's model.").short('m'),
                arg("access", Kind::Choice(ACCESS), "What the architect (and so its workers) may do without asking."),
                arg("max-workers", Kind::Int, "Workers working at once.").default("4"),
                arg("max-failures", Kind::Int, "Failed workers before spawning stops.").default("3"),
                arg("max-cost", Kind::Str, "Spend (in dollars) before spawning stops; default: no limit."),
            ],
        )
    },
    Spec {
        details: "Adds a worker to your team. It starts in its own worktree off the architect's branch as soon as the \
                  workers it depends on are done, a slot is free and no working teammate is editing its scope; until \
                  then it waits, with the reason in team_status. Its brief gets the newest team notes. When it finishes, \
                  its summary comes to you as team mail between your turns: end your turn instead of polling.",
        tool: write("team_spawn_worker", "Spawn a team worker"),
        examples: &["orbit team spawn --title \"Users API\" --scope src/api \"Add GET /users …\"", "orbit team spawn --depends-on w1 --title UI \"…\""],
        ..spec(
            &["team", "spawn"],
            "Spawn a worker on your team",
            &[
                pos("brief", Kind::Text, "The task, self-contained: goal, constraints, what done looks like (`-` reads stdin)."),
                arg("title", Kind::Str, "A short title; default: the brief's first line.").short('t'),
                arg("depends-on", Kind::Str, "Worker keys that must finish first, comma-separated (`w1,w2`)."),
                arg("scope", Kind::Str, "Paths the worker will edit, comma-separated (`src/api,docs`)."),
                arg("provider", Kind::Str, "The worker's agent; default: the team's worker agent, else yours."),
                arg("model", Kind::Str, "The worker's model.").short('m'),
                TEAM_AS,
            ],
        )
    },
    Spec {
        details: "Stores a message for a teammate and delivers it between their turns (never typed into their \
                  terminal). `to` is `architect`, `parent`, a worker key (`w2`) or `@all` (every working worker). \
                  --urgent steers a working recipient right away, at most once a minute.",
        tool: write("team_message", "Message a teammate"),
        examples: &["orbit team message w2 \"The users table has an email column now\"", "orbit team message architect --kind question \"Which port?\""],
        ..spec(
            &["team", "message"],
            "Mail a teammate",
            &[
                pos("to", Kind::Str, "`architect`, `parent`, a worker key or `@all`."),
                pos("text", Kind::Text, "The message (`-` reads standard input)."),
                arg("kind", Kind::Choice(MAIL_KINDS), "What it is.").default("message"),
                arg("urgent", Kind::Flag, "Steer a working recipient now instead of at its next turn boundary."),
                TEAM_AS,
            ],
        )
    },
    Spec {
        details: "Posts to the team's shared notes board: API shapes, decisions, conventions. New workers' briefs include \
                  the newest notes.",
        tool: write("team_post_note", "Post a team note"),
        examples: &["orbit team note \"GET /users returns [{id, name, email}]\""],
        ..spec(&["team", "note"], "Post to the team's notes board", &[pos("text", Kind::Text, "The note (`-` reads stdin)."), TEAM_AS])
    },
    Spec {
        details: "The team's notes, newest first.",
        tool: read("team_read_notes", "Read team notes"),
        ..spec(
            &["team", "notes"],
            "Read the team's notes board",
            &[arg("limit", Kind::Int, "At most this many.").default("20"), TEAM_AS],
        )
    },
    Spec {
        details: "Every worker with its state (waiting, working, done, failed, merged), why a waiting one waits, its \
                  thread, scope and summary; the circuit breaker; files two working members both changed.",
        tool: read("team_status", "Team status"),
        ..spec(&["team", "status"], "Show your team's workers", &[TEAM_AS])
    },
    Spec {
        details: "Merges a finished worker's branch into the architect's worktree. Always previewed first: when the merge \
                  would conflict, the conflicts are reported and nothing changes. --dry-run only previews.",
        tool: Some(Tool { name: "team_merge_worker", title: "Merge a worker", read_only: false, destructive: true, idempotent: false }),
        examples: &["orbit team merge w1 --dry-run", "orbit team merge w1"],
        ..spec(
            &["team", "merge"],
            "Merge a finished worker into the architect's branch",
            &[pos("worker", Kind::Str, "The worker's key (`w1`)."), arg("dry-run", Kind::Flag, "Preview only."), TEAM_AS],
        )
    },
    Spec {
        details: "Runs the same prompt several times, or once per agent, each in its own worktree off the same base. \
                  When a candidate finishes, its diff stats are taken and the project's test command (a run script named \
                  test or check, or --test) runs in its worktree; --judge asks an agent to name a winner. Compare and keep \
                  the winner in the app, or with `orbit merge`.",
        examples: &["orbit race \"Make the cache thread-safe\" -n 3", "orbit race \"Fix #12\" --provider claude,codex --judge"],
        ..spec(
            &["race"],
            "Race one prompt across several candidates",
            &[
                pos("prompt", Kind::Text, "The prompt (`-` reads standard input)."),
                arg("n", Kind::Int, "How many candidates, when --provider/--model name fewer.").short('n').default("3"),
                arg("provider", Kind::Str, "Agents, comma-separated, one candidate each (`claude,codex`)."),
                arg("model", Kind::Str, "Models, comma-separated, one candidate each.").short('m'),
                arg("test", Kind::Str, "The test command; default: the project's run script named test or check."),
                arg("judge", Kind::Flag, "Ask an agent for a verdict once every candidate settled."),
                arg("base", Kind::Str, "The branch to start from and merge into; default: the project's base branch."),
                PROJECT,
            ],
        )
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

    #[test]
    fn automation_recipes_match_the_engine() {
        let engine: Vec<&str> = orbit_core::automations::recipes::RECIPES.iter().map(|r| r.id).collect();
        assert_eq!(RECIPES, engine.as_slice());
    }

    #[test]
    fn every_automation_command_is_an_mcp_tool() {
        let commands: Vec<&Spec> = SPECS.iter().filter(|s| s.path.first() == Some(&"automation")).collect();
        let names: Vec<&str> = commands.iter().map(|s| s.path[1]).collect();
        assert_eq!(names, ["list", "show", "create", "run", "pause", "resume", "delete", "history"]);
        for spec in commands {
            let tool = spec.tool.as_ref().unwrap_or_else(|| panic!("{} has no tool", spec.name()));
            assert_eq!(tool.read_only, matches!(spec.path[1], "list" | "show" | "history"), "{}", spec.name());
            assert_eq!(tool.destructive, spec.path[1] == "delete");
        }
        let create = find(&["automation", "create"]).unwrap();
        for name in ["cron", "rule", "every", "on", "recipe", "target", "open-pr", "only-if-changed", "check", "max-per-day"] {
            assert!(create.arg(name).is_some(), "create --{name}");
        }
    }
}
