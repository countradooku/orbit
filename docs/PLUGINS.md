# Writing Orbit plugins

A plugin is a folder with an `orbit-plugin.toml` manifest and the files it
names. Plugins are installed on a **host** (the computer whose `orbitd` runs
your agents, local or remote) and contribute to every window that shows it.
They do only what their manifest declares and the user approves, and every
call they make is logged.

In order of trust, a plugin can add:

| Tier | What | Runs code? | Consent |
| --- | --- | --- | --- |
| Data | Themes, snippets, slash commands, automation recipes, keybindings, project templates | No | None: one-click install |
| Agent | Agent CLIs described as data, over ACP, Claude Code's stream-json, or in a terminal | Runs the command lines it names | `agents`, with the command lines shown |
| Panel | Declarative panels drawn with native components, bound to the plugin's storage | No | The capabilities its actions call |
| WebAssembly | Hooks, commands and panel actions in a sandbox | Sandboxed | The capabilities it declares |

- [Quick start](#quick-start)
- [The manifest](#the-manifest)
- [Capabilities and consent](#capabilities-and-consent)
- [Data contributions](#data-contributions)
- [Agent manifests](#agent-manifests)
- [Panels](#panels)
- [WebAssembly](#webassembly)
- [Storage, settings and secrets](#storage-settings-and-secrets)
- [Installing and publishing](#installing-and-publishing)
- [Catalogs](#catalogs)
- [The kill list](#the-kill-list)
- [Limits](#limits)
- [Security model](#security-model)

The examples in [`examples/plugins`](../examples/plugins) are complete
plugins: a theme (`midnight-theme`), agent manifests (`agents`) and a
WebAssembly guard that blocks `rm -rf /` (`guard-rm`).

## Quick start

```sh
orbit plugin new my-plugin          # writes my-plugin/orbit-plugin.toml
orbit plugin validate my-plugin     # checks everything; prints its content hash
orbit plugin dev my-plugin --yes    # loads it in place; edits reload it
orbit plugin logs my-plugin         # what it did, call by call
```

`validate`, `new` and `pack` need no running Orbit. `dev` loads the folder on
this computer's host and reloads it whenever a file changes; a plugin that
stops validating shows as **Broken** with the reason until it validates
again. Settings › Plugins shows the same: on/off, what it asks for, its log.

## The manifest

`orbit-plugin.toml` at the plugin's root. **Unknown keys are refused at every
level**, so a typo is an error, not a silently ignored setting. The JSON
Schema is [`orbit-plugin.schema.json`](orbit-plugin.schema.json) (a test keeps
it in step with the parser).

```toml
id = "my-plugin"                 # kebab-case, 3–64 chars; not orbit or orbit-*
name = "My plugin"
version = "1.0.0"                # semver
description = "One sentence."    # optional, ≤300 chars
publisher = "You"                # optional
license = "MIT"                  # optional
homepage = "https://…"           # optional, https only
categories = ["themes"]          # optional
engines = { orbit = ">=0.1.0" }  # the only version gate
capabilities = []                # see below

# Then any of: [[themes]] [[snippets]] [[slash_commands]] [[recipes]]
# [[keybindings]] [[templates]] [[agents]] [[panels]] [[commands]]
# [[settings]] [wasm]
```

A plugin whose `engines.orbit` doesn't admit the running Orbit installs but
stays off, marked **Incompatible**. Paths are relative to the plugin's root,
with forward slashes and no `..`.

### Commands

Palette commands (`>` in the palette lists them as `Plugin: Title`). Exactly
one of these says what a command does:

```toml
[[commands]]
id = "review"
title = "Review checklist"
prompt = "Review this change for correctness, tests and naming."   # put in the composer

[[commands]]
id = "open-notes"
title = "Open notes"
panel = "notes"            # open a panel

[[commands]]
id = "start-aider"
title = "Start Aider"
agent = "aider"            # start a terminal agent in the thread's terminal

[[commands]]
id = "tidy"
title = "Tidy storage"
wasm = true                # call the module's orbit_command
```

## Capabilities and consent

| Capability | Lets the plugin | Marked sensitive |
| --- | --- | --- |
| `storage` | Keep its own data on the host | |
| `secrets` | Keep its own secrets in the system keychain | |
| `notifications` | Show notifications | |
| `threads.read` | See threads' titles, projects and status | |
| `threads.create` | Start agent threads with its own prompts | yes |
| `agents` | Run the agent command lines it defines | yes |
| `hooks.prompts` | Read every prompt before it is sent, and block it | yes |
| `hooks.tools` | See every agent tool call (commands, paths) and block it | yes |
| `hooks.threads` | Know when a thread finishes a turn | |

- Installing shows a **consent dialog** listing each capability and every
  program the plugin would run. A plugin with no capabilities installs in one
  click.
- Capabilities not granted leave the plugin **Waiting for approval**: it
  contributes nothing and none of its code runs.
- An **update** keeps what was granted for capabilities it still declares; a
  new capability asks again.
- **Every call is checked right before its effect**: the plugin is installed,
  on, not on the kill list, its files still hash to what was installed, and
  the capability is declared and granted. Turning a plugin off takes effect on
  its next call.
- **Every call is logged** in the plugin's log (Settings › Plugins › Log…, or
  `orbit plugin logs <id>`): host API calls with their outcome
  (`allowed`, `denied`, `error`), hook decisions with their timing, the
  module's own log lines, and installs, grants, reloads and kill-list actions.

## Data contributions

### Themes

A theme file is a GPUI Kit theme set, the format of Orbit's bundled themes in
`crates/orbit-app/assets/themes`:

```json
{
  "name": "Midnight",
  "themes": [
    { "name": "Midnight", "mode": "dark",
      "colors": { "background": "#0b1020", "foreground": "#d6deeb", "primary.background": "#2f6fdb", "primary.foreground": "#ffffff",
                  "base.red": "#ff6b81", "base.green": "#7ee0a1" } }
  ]
}
```

- Tokens are GPUI Kit's (`background`, `muted.foreground`,
  `primary.background`, `popover.foreground`, `tab.active.background`, …);
  tokens you leave out fall back to GPUI Kit's defaults. The **terminal**
  takes its ANSI colors from `base.red`, `base.green`, `base.yellow`,
  `base.blue`, `base.magenta` and `base.cyan`.
- Values are colors only: `#rgb`, `#rrggbb`, `#rrggbbaa`, GPUI Kit palette
  names (`green-600`, `blue/50`) or a two-stop `linear-gradient(…)`. Each
  theme may also set `highlight` (syntax colors), `radius`, `radius.lg` and
  `shadow`; fonts and sizes are the user's.
- **Contrast**: `foreground` on `background` must reach 4.5:1 or the theme is
  refused. Other text/surface pairs (`primary.foreground` on
  `primary.background`, popovers, tabs, editor; `muted.foreground` at 3:1) that
  fall short are dropped, so both tokens use the defaults, and
  `orbit plugin validate` warns. Translucent colors are judged as drawn.
- Names starting with `Orbit ` or `Default `, or matching a bundled theme, are
  not taken. Plugin themes appear in Settings › Appearance; turning the
  plugin off returns to Orbit's theme.

```toml
[[themes]]
path = "themes/midnight.json"
```

### Snippets and slash commands

```toml
[[snippets]]                 # palette: "Insert snippet: Checklist"
name = "Checklist"
text = "- tests\n- docs\n"   # or path = "snippets/checklist.md"

[[slash_commands]]           # composer: /explain src/main.rs
name = "explain"
description = "Explain a file"
argument_hint = "<path>"
prompt = "Explain {{args}} to a new contributor."   # or path = "prompts/explain.md"
```

A slash command is expanded in the composer before sending: `{{args}}` is
what follows the command (appended when the prompt doesn't use it).

### Automation recipes

```toml
[[recipes]]
id = "weekly-deps"
title = "Weekly dependency check"
blurb = "Lists outdated dependencies every Monday."
prompt = "List this project's outdated dependencies and what upgrading each would take."
schedule = "FREQ=WEEKLY;BYDAY=MO;BYHOUR=9"   # empty: run by hand
target = "worktree"                          # or "project"
mode = "plan"                                # or "build"
open_pr = false
```

They appear in the automations gallery after Orbit's own; using one opens the
editor filled in, so nothing runs until it is saved.

### Keybindings

```toml
[[keybindings]]
key = "mod-alt-r"              # as keybindings.json writes keys
command = "plugin:review"      # one of the plugin's commands, or orbit::<Action>
when = "Composer"              # optional context
```

They come after Orbit's defaults and before the user's `keybindings.json`,
so the user's rules still win.

### Project templates

```toml
[[templates]]
name = "Rust service"
description = "cargo targets and a worktree setup script"
path = "templates/rust/orbit.toml"
```

The palette offers "Start orbit.toml from Rust service" for the current
project; it writes the file only if the project has none
([`orbit-toml.md`](orbit-toml.md) describes it).

## Agent manifests

One generic mechanism for agent CLIs instead of a module per agent:

```toml
capabilities = ["agents"]

[[agents]]
id = "gemini-flash"            # the instance id; not a built-in agent's
name = "Gemini Flash"
protocol = "acp"               # acp | stream-json | terminal
command = "gemini"             # on PATH, or an absolute path
args = ["--experimental-acp", "--model", "gemini-2.5-flash"]
env = { GEMINI_SYSTEM_MD = "1" }          # optional; PATH, LD_*, DYLD_*… refused
install = "npm install -g @google/gemini-cli"   # shown while it's missing
models = [{ id = "gemini-2.5-flash", name = "Gemini 2.5 Flash", default = true, context_window = 1048576 }]
modes = [{ id = "build" }, { id = "plan", label = "Plan" }]
```

- **`acp`**: the Agent Client Protocol over standard input and output,
  through Orbit's ACP adapter (Gemini CLI, Goose, Kiro, Qwen Code…).
- **`stream-json`**: Claude Code's protocol, for Claude-compatible CLIs. The
  CLI must accept Claude Code's flags; `args` come before Orbit's own.
- **`terminal`**: any interactive program. It isn't a chat agent; a
  `[[commands]]` entry with `agent = "<id>"` starts it in the current
  thread's terminal, and Orbit's status engine follows it (hooks, OSC 7501,
  title and prompt heuristics) like the agents it knows.

Chat agents become provider instances: the model picker lists them beside
the built-in ones, probed the same way (installed, version, models). When
the agent reports no models or modes (an ACP agent before a session, or one
that never does), the manifest's are used. A user's own instance with the
same id (Settings › Agents) overrides or disables it.

## Panels

A panel is UI as data: one component tree in TOML or JSON, drawn with GPUI
Kit's own components, so it matches the theme, focus and keyboard rules of
the rest of Orbit. Panels open as tabs in the plugin dock beside the work
area (from the palette, or a command with `panel = "<id>"`).

```toml
[[panels]]
id = "todo"
title = "Todo"
path = "panels/todo.toml"
```

```toml
# panels/todo.toml
type = "stack"
children = [
  { type = "heading", text = "Todo" },
  { type = "text", text = "{{count}} open", muted = true },
  { type = "list", items = "todos", empty = "Nothing to do", item = { type = "row", children = [
      { type = "text", text = "{{item}}" },
      { type = "spacer" },
      { type = "button", label = "Done", style = "ghost",
        action = { call = "storage.remove", args = { key = "todos", index = "{{index}}" } } },
  ] } },
  { type = "input", bind = "draft", placeholder = "Add a todo",
    submit = { call = "storage.append", args = { key = "todos", value = "{{input.draft}}" } } },
  { type = "switch", bind = "compact", label = "Compact" },
]
```

| Component | Fields |
| --- | --- |
| `stack`, `row` | `children`, `gap` (`none`, `small`, `medium`, `large`) |
| `text` | `text`, `muted`, `size` (`small`, `normal`) |
| `heading` | `text` |
| `spacer` | (none): takes a row's free space, so what follows sits at its end |
| `button` | `label`, `action`, `style` (`primary`, `secondary`, `ghost`, `danger`) |
| `badge` | `text`, `tone` (`neutral`, `info`, `success`, `warning`, `danger`) |
| `card` | `title`, `description`, `children` |
| `list` | `items` (a storage key holding an array), `item` (a component), `empty` |
| `input` | `bind` (its name), `placeholder`, `submit` (an action, on Enter) |
| `switch` | `bind` (a storage key, boolean), `label` |
| `select` | `bind` (a storage key), `options` (`[{ value, label }]`) |
| `progress` | `value` (number or placeholder), `max` (default 100), `label` |
| `markdown` | `text` |
| `code_view` | `text`, `language` |
| `table` | `rows` (a storage key: array of objects), `columns` (`[{ key, title }]`), `empty` |
| `tabs` | `tabs` (`[{ title, children }]`) |
| `empty_state` | `title`, `description`, `button` (`{ label, action }`) |

**Placeholders** `{{key}}` read the plugin's storage (dotted paths such as
`{{settings.limit}}` or `{{todos.0}}`). Inside a `list`, `{{item}}`,
`{{item.field}}` and `{{index}}` name the row. In action arguments,
`{{input.<bind>}}` is what the panel's input holds. There is no logic: a
template can only look values up.

**Actions** call host methods, checked against the plugin's capabilities
when it loads and again on every call:

| `call` | `args` | Needs |
| --- | --- | --- |
| `storage.set` | `key`, `value` (`as` = `number` or `bool` to convert) | `storage` |
| `storage.append` | `key`, `value` (an empty value appends nothing) | `storage` |
| `storage.remove` | `key`, `index` | `storage` |
| `storage.toggle` | `key` | `storage` |
| `storage.clear` | `key` | `storage` |
| `notifications.show` | `title`, `body` | `notifications` |
| `threads.create` | `prompt` | `threads.create` |
| `plugin.action` | `name` (the module's `orbit_action` gets the name and the panel's context) | a `[wasm]` module |

Switches and selects write their storage key directly (`storage`). The app
sends the host only which action fired and the panel's inputs; the host
finds the action in the installed tree again, so nothing a client sends can
make a plugin do what its files don't say.

## WebAssembly

Logic runs as a WebAssembly module in a sandbox with **no ambient
authority**: a module may import exactly two functions, `orbit.log` and
`orbit.call`, and nothing else; WASI is refused, so there are no files,
network, clock, environment or randomness unless the host API hands them out.

```toml
capabilities = ["hooks.tools"]

[wasm]
module = "guard.wasm"
hooks = ["agent.tool.before"]
on_error = "deny"       # allow (default) or deny: what a failed or slow hook means
fuel = 2000000          # instructions per call (default 20M, at most 500M)
memory_mb = 2           # linear memory cap (default 16, at most 128)
timeout_ms = 100        # wall-clock budget per call (default 250, at most 5000)
```

### The ABI

```text
import orbit.log(ptr: i32, len: i32)
import orbit.call(method_ptr: i32, method_len: i32, params_ptr: i32, params_len: i32) -> i64
export memory
export orbit_alloc(len: i32) -> i32
export orbit_hook(ptr: i32, len: i32) -> i64      // hooks
export orbit_command(ptr: i32, len: i32) -> i64   // [[commands]] with wasm = true
export orbit_action(ptr: i32, len: i32) -> i64    // panel actions with call = "plugin.action"
```

Messages are UTF-8 JSON in the module's memory. The host asks `orbit_alloc`
for room, writes the input there and calls the entry point, which returns
`ptr << 32 | len` of its answer, or 0 for none. `orbit.call` answers
`{"ok": …}` or `{"error": {"code": …, "message": …}}` in memory it asks
`orbit_alloc` for; error codes are `invalid_params`, `capability_denied`,
`not_found`, `too_large`, `rate_limited` and `unavailable`.

### Hooks

| Hook | Input (`thread` is `{ "id", "cwd" }`) | Answer |
| --- | --- | --- |
| `agent.prompt.before` | `{ "hook", "thread", "prompt" }` | `{"deny": "reason"}` blocks the message |
| `agent.tool.before` | `{ "hook", "thread", "tool": { "kind": "command", "command", "cwd", "reason" } }`, or `"kind": "file"` with `path`, `diff`, or `"kind": "tool"` with `name`, `input` | `{"deny": "reason"}` refuses the call |
| `agent.tool.after` | `{ "hook", "thread", "tool": { "name", "title", "input" }, "output" (first 8 KiB), "error" }` | ignored (observe only) |
| `thread.finished` | `{ "hook", "thread", "turn", "outcome": "completed" \| "failed" \| "interrupted" \| "rate-limited" }` | ignored (may call the host API) |

A denial is answered before anyone is asked, under any access level: the
agent is told "Blocked by the *Name* plugin: *reason*" and the thread shows
it. Where Orbit sees a tool call before it runs:

- **Claude Code** and stream-json agents: every tool call, through the
  `PreToolUse` hook Orbit registers when a tool hook is on at session start.
- **ACP agents**: every call the agent asks permission for.
- **Codex, OpenCode, Pi**: every call that reaches Orbit as an approval
  request. Under Full access these runtimes run some commands without
  asking; use them at Ask or Auto-edit when a guard must see everything.

### Host API (`orbit.call`)

| Method | Params | Needs |
| --- | --- | --- |
| `storage.get` | `{ "key" }` (dotted) | `storage` |
| `storage.set` | `{ "key", "value" }` (any JSON) | `storage` |
| `storage.delete` | `{ "key" }` | `storage` |
| `storage.keys` | `{}` | `storage` |
| `secrets.get`, `secrets.set`, `secrets.delete` | `{ "key" }`, `{ "key", "value" }` | `secrets` |
| `settings.get` | `{ "key" }` (its own settings; secret ones need `secrets`) | — |
| `notifications.show` | `{ "title", "body" }` | `notifications` |
| `threads.list` | `{}` → `[{ "id", "title", "project", "status", "updated_ms" }]` (newest 200) | `threads.read` |
| `threads.create` | `{ "prompt" }` (at most 3 per call, 10 per ten minutes per plugin) | `threads.create` |

### Limits and failures

Each call gets fresh fuel, so a loop ends; memory can't grow past its cap;
an answer that arrives after `timeout_ms` is dropped; a call may make at
most 256 host calls and 64 log lines, and nothing larger than 1 MiB crosses
the boundary. A call that fails (trap, fuel, timeout, bad answer) is logged,
its instance is discarded (the next call starts fresh), and the hook follows
`on_error`.

### Building a module

Any language that targets `wasm32-unknown-unknown` works. The guard example
is `no_std` Rust with no allocator ([source](../examples/plugins/guard-rm/src/lib.rs)):

```sh
cd examples/plugins/guard-rm
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/orbit_example_guard_rm.wasm guard.wasm
```

### Why wasmi

The sandbox is [wasmi](https://github.com/wasmi-labs/wasmi), an interpreter,
rather than wasmtime. Measured on the same minimal host (load a module, call
an export; release, LTO, stripped, Windows x64):

| | Binary | Clean release build |
| --- | --- | --- |
| No runtime | 119 KB | 2 s |
| wasmi 1.1 (fuel, limits) | 1.23 MB (+1.1 MB) | 20 s |
| wasmtime 51 (Cranelift, component model, WASI p2) | 11.6 MB (+11.5 MB) | 193 s |

Plugin logic is small and short (a hook decides in microseconds), where an
interpreter's speed is ample and wasmtime's JIT buys nothing worth ten times
the size and build time. wasmi meters fuel and caps memory, which are the
limits that matter. The component model and WASI preview 2 aren't needed:
the ABI above is two imports with no ambient capabilities at all, which is
a smaller attack surface than scoping WASI. The runtime sits behind one type
(`orbit_plugin::wasm::Sandbox`), so it can change without touching plugins'
host API.

## Storage, settings and secrets

- **Storage** is one JSON object per plugin (at most 1 MiB), in
  `<data>/plugins/data/<id>/storage.json` on the host. Panels read it;
  actions and the module write it.
- **Settings** are declared and typed; the user edits them in Settings ›
  Plugins (the plugin's menu › Settings…), values are checked on the host
  against their type, and the module reads them with `settings.get`:

  ```toml
  [[settings]]
  key = "limit"
  label = "Most results"
  type = "number"          # text | toggle | number | choice | secret
  default = "20"
  ```

- **Secrets** (`secrets` capability, and `type = "secret"` settings) go to
  the operating system's credential store through `keyring-core`: Windows
  Credential Manager, the macOS Keychain, or the Secret Service on Linux,
  under a service name tied to the data folder. A host without one (a
  headless server, a container, WSL without a Secret Service) keeps them in
  `secrets.json` readable only by the user (mode 0600), which is no stronger
  than the data folder itself; Settings › Plugins says which applies.
  Secrets are never sent to clients: the UI shows only whether one is set.

Removing a plugin deletes its files, storage, settings, secrets and log.

## Installing and publishing

| Source | How |
| --- | --- |
| A folder | `orbit plugin install ./folder`, Settings › Plugins › Install… › From a folder. Packed and sent, so it works with a remote host too. |
| A folder in dev mode | `orbit plugin dev ./folder`; Load a folder in dev mode… (this computer's host only). Loaded in place and reloaded on change. |
| A `.orbitplugin` archive | A zip of the folder: `orbit plugin pack` writes one. Paths that leave the folder, links, more than 512 files or 32 MiB are refused. |
| git | `orbit plugin install <url> --commit <40 hex> [--hash blake3:…]`. https, ssh or a local path; `ext::` and option-like URLs are refused, hooks and filters don't run, line endings aren't converted. |
| A catalog | Settings › Plugins › Browse, or `orbit plugin install <id> --catalog <name>`. |

The **content hash** (`blake3:<hex>`) covers every file's relative path and
bytes in path order (`.git` and a top-level `target` build folder are left
out, of the hash, archives and installs alike), so it is the same on every OS and for
every way a plugin arrives. `orbit plugin validate` and `pack` print it. An
installed plugin whose files later change no longer loads ("reinstall it").

## Catalogs

A catalog is a `catalog.toml` and its minisign signature
`catalog.toml.minisig`, in a folder on the host or at an https address. Its
trusted comment must start with `orbit-catalog`. Users add a catalog with
its location and its publisher's public key; a listing that doesn't verify
is refused and the last good copy stays.

```toml
name = "Team plugins"

[[plugins]]
id = "midnight-theme"
name = "Midnight"
version = "1.0.0"
description = "A calm blue-black theme"
publisher = "Team"
categories = ["themes"]
tier = "data"                    # data | agent | panel | wasm
capabilities = []
hash = "blake3:…"                # what the files must hash to
source = { archive = "midnight-theme-1.0.0.orbitplugin" }
# or source = { git = "https://github.com/o/r", commit = "<40 hex>" }
# or source = { path = "midnight-theme" }   (catalogs in a folder only)
```

Sign it with [minisign](https://jedisct1.github.io/minisign/), the tool
Orbit's releases use ([RELEASING.md](RELEASING.md)):

```sh
minisign -S -s team.key -m catalog.toml -t "orbit-catalog team $(date +%F)"
```

The signature covers each release's hash, so what is installed is exactly
what the publisher signed, whichever server or git host delivered it. A
catalog that lists a newer version of an installed plugin offers "Update".

**Trying a catalog locally**: write a signed catalog of the examples (with
a test key) and add it in Settings › Plugins › Add catalog…, with the
folder as its location and `public-key.txt` as its key:

```sh
ORBIT_FIXTURE_DIR=/tmp/orbit-catalog cargo test -p orbit-plugin --test host make_catalog_fixture -- --ignored
```

## The kill list

`kill-list.toml`, signed with Orbit's release key (trusted comment
`orbit-kill-list …`), names plugins Orbit turns off everywhere:

```toml
[[plugins]]
id = "prompt-stealer"
versions = "<1.2.1"     # semver; "*" for all
reason = "Sent prompts to a third party."
```

The host fetches it every six hours from next to the update manifests
(`engine::plugins::KILL_LIST_URL`) once the build carries a release key, and
keeps the last verified copy in `<data>/plugins/`. A listed plugin is shown
**Blocked** with the reason, never runs, can't be turned back on and can't
be installed. A list that doesn't verify is ignored: a forged list can
neither block nor unblock anything.

## Limits

| | Limit |
| --- | --- |
| Manifest | 64 KiB |
| Theme, panel, prompt, snippet, template file | 256 KiB (themes 128 KiB) |
| WebAssembly module | 8 MiB |
| Plugin | 32 MiB, 512 files |
| Contributions | 32 themes, 256 snippets, 64 slash commands, 32 recipes, 128 keybindings, 32 templates, 16 agents, 16 panels, 64 commands, 64 settings, 4 hooks |
| Panel | 400 components, 12 levels deep; lists and tables show 10,000 rows |
| Storage | 1 MiB per plugin |
| Log | 256 KiB per plugin (older half dropped) |
| Catalog | 4 MiB, 2,048 releases |

## Security model

- **Nothing runs without consent.** Data plugins run no code. Programs
  (agent manifests) and WebAssembly logic need capabilities the user
  grants after seeing them; programs are listed by their full command line.
- **Least authority.** A module can only reach the host API, and only the
  parts its granted capabilities open; every call is checked at the moment
  it takes effect and logged.
- **Pinned content.** Installed files are hashed; catalog releases are pinned
  by hash under the publisher's signature; git sources are pinned to a
  commit. Changed files stop a plugin from loading.
- **Bounded work.** Fuel, memory, time and size limits on every call; a
  misbehaving module costs at most its budget and is restarted fresh.
- **Revocable.** Off takes effect on the next call; the kill list turns a
  plugin off on every host.

What this doesn't do: an agent manifest runs a real program with your
privileges, as any agent CLI does; consent to `agents` is consent to run it.
Theme contrast checks keep text readable but a theme still colors the consent
dialog.
