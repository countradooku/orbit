# The `orbit` command line and MCP server

`orbit` drives a running Orbit (the desktop app or `orbitd`) from a shell,
scripts and agents. `orbit mcp` serves the same operations as MCP tools, so
an agent inside an Orbit thread can start helper threads in their own
worktrees, message them, wait for them and read their results.

The desktop app's binary is `orbit-desktop`; `orbit` is the CLI.

## Connecting

Every engine also listens on a random loopback port with a fresh token and
writes `<data>/endpoint.json`:

```json
{"url":"ws://127.0.0.1:53817/","token":"…","pid":4242,"version":"0.1.0","kind":"desktop","protocol":2}
```

* The file is owner-only (0600 on Unix; on Windows it inherits the per-user
  profile folder's ACL), removed on a clean exit, and ignored when its pid is
  gone. The token is never put in an agent's environment.
* The listener costs nothing while idle: one thread parked in `accept`.
* `orbit` looks in `--data`, else `ORBIT_DATA`, else the platform folder
  (`%APPDATA%\Orbit`, `~/Library/Application Support/Orbit`,
  `~/.local/share/Orbit`). `--host`/`--token` (or `ORBIT_HOST`/`ORBIT_TOKEN`)
  name another engine instead, such as a remote `orbitd`.
* Agents started by Orbit get `ORBIT_THREAD` (their thread) and `ORBIT_DATA`.
* This is the engine's endpoint. The desktop app also keeps its
  single-instance hand-off (`app.endpoint`), which opens a folder or thread in
  the running window; `orbit open` uses that one, by running
  `orbit-desktop --thread <id>`, which hands over to the running app (or
  starts it) and exits.

Startup, measured on Windows 11 with a release build against a running
`orbitd`: first byte of `orbit threads` in 13.5 ms median from process
spawn, of which about 12 ms is creating the process itself (`orbit --version`
takes 12.0 ms); connecting, the snapshot and printing add about 1.5 ms. The
client avoids sockudo-ws's stream type, whose clock calibration costs up to
200 ms on first use (see `orbit-net/src/ws_lite.rs`).

## Commands

Run `orbit help <command>` for details and examples, `orbit help --json` for
a machine-readable description of every command, argument and exit code,
and `orbit agent-context` for a short guide written for agents.

| Command | What it does | MCP tool |
| --- | --- | --- |
| `status [thread]` | Engine, counts, providers; or one thread in detail | `status` |
| `projects`, `projects add <path>` | List or add projects | `list_projects`, `add_project` |
| `threads [--status S] [--needs-you] [--project P] [--parent T] [--all]` | List threads | `list_threads` |
| `new <prompt> [--project] [--worktree] [--provider] [--model] [--effort] [--mode] [--access] [--title] [--wait]` | Start a thread | `start_thread` |
| `send <thread> <message> [--steer] [--wait]` | Queue (or steer) a message | `send_message` |
| `wait <thread>… [--until finished\|idle\|needs-you] [--any] [--timeout]` | Block until threads stop | `wait` |
| `log <thread> [--limit] [--after] [--full] [--follow]`, `tail <thread> [--until]` | Read or stream a timeline | `read_thread` |
| `approve`, `deny [--reason]`, `answer <text>` | Resolve a pending request | `approve_request`, `deny_request`, `answer_question` |
| `stop [--force]`, `retry`, `new-session` | Control a turn or session | `stop_thread`, `retry_thread` |
| `settle [--undo]`, `snooze <for> \| --wake`, `archive [--undo]` | Triage | `settle_thread`, `archive_thread` |
| `diff <thread> [--turn N \| --last \| --since-turn N \| --base REF \| --uncommitted] [--stat]` | What changed | `get_diff` |
| `undo-turn <thread> [--turn N] [--redo]` | Restore files to before a turn, or undo that | |
| `merge <thread> [--into B] [--dry-run]` | Merge the thread's branch locally | `merge_thread` |
| `export <thread>` | A readable digest of the thread | `export_thread` |
| `files <query> [--thread \| --project]` | Find files in a workspace | `search_files` |
| `queue <thread> <item> --to N \| --now`, `draft <thread> --text T` | Composer queue and drafts | |
| `generate title\|commit\|pr\|custom <input>` | One-shot text from an agent | |
| `terminal run [--thread T \| --cwd D] [--detach] <command…>`, `terminal read <id>`, `terminal list` | Run commands in engine terminals | |
| `pr list\|show\|merge\|checkout\|fix-checks` | Pull requests on GitHub or GitLab | `list_pull_requests`, `get_pull_request` |
| `issue list\|show\|start` | Issues on GitHub, GitLab, Linear or Jira; start a thread from one | `list_issues`, `get_issue` |
| `ship [thread]` | Where a thread's work stands (uncommitted → merged) | `ship_status` |
| `automation list\|show\|create\|run\|pause\|resume\|delete\|history` | Scheduled and event-triggered agent runs (see `orbit help automation create`) | `list_automations`, `get_automation`, `create_automation`, `run_automation`, `pause_automation`, `resume_automation`, `delete_automation`, `automation_history` |
| `search <query>` | Full-text search across threads | `search` |
| `bot status`, `bot pair <code> [--reject]`, `bot unpair <platform> <user>`, `bot allow <platform> <user> [--name N]` | Chat bots: connections, and who may drive Orbit from Telegram, Slack or Discord (see [BOTS.md](BOTS.md)). Pairing is refused inside an agent's thread | |
| `open [thread]` | Bring the desktop app forward (starting it if needed) and show the thread | |
| `agent-context`, `help [--json]`, `completions <shell>` | Self-description | |
| `mcp`, `mcp install [--client claude\|codex\|all] [--apply]` | MCP server and its registration | |

Thread selectors: a full id (`thr_…`), a unique part of an id or title,
`latest`, `self` (`ORBIT_THREAD`) or `parent`. Project selectors: id, name,
path, or `.`. Free text given as `-` is read from standard input.

## Output

Human text by default. With `--json`, one line on stdout:
`{"ok":true,"result":{…}}` or
`{"ok":false,"error":{"code":"not_found","exit":4,"message":"…","hint":"…"}}`.
`log --follow`/`tail` with `--json` print one JSON event per line.
Reading never marks a thread read for the user.

## Exit codes

| Code | Name | Meaning |
| --- | --- | --- |
| 0 | ok | Success; for `wait`, the awaited state was reached |
| 1 | error | The engine refused or failed the command |
| 2 | usage | Bad arguments |
| 3 | unavailable | No running engine, or it couldn't be reached or refused the token |
| 4 | not_found | No (or more than one) thread, project, request or terminal matches |
| 5 | timeout | `wait` (or `--wait`) gave up |
| 6 | needs_you | `wait` stopped because a thread needs an approval or answer |
| 7 | failed | `wait` stopped because a thread failed or hit its rate limit |
| 8 | idle | `wait --until needs-you` stopped because a thread finished without asking |

`terminal run` exits with the command's own exit code.

## MCP

`orbit mcp` speaks MCP (JSON-RPC 2.0, newline-delimited stdio; protocol
2025-11-25, 2025-06-18, 2025-03-26 and 2024-11-05) with the tools above,
generated from the same specs as the CLI. Tool calls run concurrently, so a
long `wait` never blocks a `list_threads`. Results are returned as
`structuredContent` and as JSON text.

Inside an Orbit thread (`ORBIT_THREAD`, or `ORBIT_PARENT_THREAD` to
override) the server acts for that thread:

* threads it starts are linked to it as their parent, default to its
  project and agent settings, and never get more access than it has;
* it may message its descendants and ancestors, stop, settle, archive,
  deny or answer only its descendants, and approve their requests only if
  it has full access itself.

Register it with `orbit mcp install` (prints the `claude mcp add` command and
the `~/.codex/config.toml` table) or `orbit mcp install --apply` (runs and
writes them).

## Design

* `crates/orbit-cli/src/spec.rs` holds the command specs; `args.rs` builds the
  clap parser and MCP input schemas from them; `help.rs` generates help,
  completions and the agent guide; `exec.rs` runs commands.
* Waiting and streaming are client-side, from the event stream: the engine
  keeps no per-client state. A state must hold for 200 ms before `wait`
  believes it, so a turn ending and a queued message starting are never taken
  for the end.
* Engine additions (`crates/orbit-core/src/engine/cli.rs`): `StartThread`,
  `ReadTimeline`, `RunInTerminal`, `ReadTerminal` and `ListTerminals`. A
  command terminal answers ConPTY's start-up cursor query itself, since no
  screen is attached to it.
* The CLI depends on `orbit-net` without its `engine` feature, so it never
  links the engine.
