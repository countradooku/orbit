# `orbit.toml`: project configuration in the repository

A project can carry its Orbit setup in an `orbit.toml` at the root of the
repository, so everyone who adds the project gets the same worktree scripts,
run targets, shared files and environment.

* When a project is **added**, an `orbit.toml` is applied at once.
* Later, **Settings › Projects › Import…** reads it again
  and shows what would change before anything is saved.
* What is set in Orbit afterwards is kept on the host; the file is never
  written by Orbit.

Every key is optional. Unknown keys are reported, not silently ignored, so
a typo such as `setpu` does not go unnoticed.

```toml
# Top-level keys come before any [table].
name = "Shop"                    # shown in the sidebar
icon = "🛒"                      # one emoji or up to two letters
base_branch = "main"             # what new worktrees branch from
worktree = true                  # new threads start on their own worktree
ports = 10                       # ports reserved for each worktree
include = [".env", ".env.local", "config/local.json"]

[scripts]
# Run in a new worktree before the agent starts (queued messages wait).
setup = "pnpm install"
# Run before a worktree is removed.
teardown = "docker compose down"

[env]
# Exported to the setup and teardown scripts and the run targets.
NODE_ENV = "development"

[[targets]]
name = "Dev"
command = "pnpm dev --port $PORT"
kind = "dev"                     # dev, test, lint, build or other
url = "http://localhost:$PORT"   # offered to the preview browser

[[targets]]
name = "Test"
command = "pnpm test"

[agent]
# The agent new threads in this project start with.
provider = "claude"              # claude, codex, opencode, pi, acp, or an instance id
model = "claude-sonnet-4-5"
mode = "build"                   # build or plan
access = "ask"                   # readonly, ask or auto-edit; never full
```

## Keys

| Key | Meaning |
|---|---|
| `name` | The project's name in Orbit. |
| `icon` | One emoji or up to two letters drawn in place of the monogram. |
| `base_branch` | The branch new worktrees start from. |
| `worktree` | `true`: a new thread in the project gets its own worktree and branch. |
| `ports` | How many ports each worktree reserves (default 10, at most 100). |
| `include` | Untracked files copied from the main checkout into every new worktree, in gitignore syntax (`.env*`, `!secret.env`). Added to the repository's `.worktreeinclude`. |
| `scripts.setup` | Runs in a new worktree, before the agent's first turn. |
| `scripts.teardown` | Runs in a worktree before it is removed. |
| `env` | Variables for the project's scripts and run targets. |
| `targets` | Commands run on demand from the thread header, the palette or the terminal dock. `kind` is guessed from the name when absent. |
| `agent` | The provider, model, mode and access of new threads. A checked-in file never grants full access. |

## The environment of scripts and targets

Everything a project runs in a worktree sees:

| Variable | Value |
|---|---|
| `PORT`, `ORBIT_PORT_BASE` | The first of the worktree's reserved ports. |
| `ORBIT_PORT_COUNT` | How many ports it reserved. |
| `ORBIT_WORKTREE` | The worktree's folder. |
| `ORBIT_ROOT_PATH` | The project's main checkout (Conductor's `CONDUCTOR_ROOT_PATH`). |
| `ORBIT_BRANCH` | The worktree's branch. |
| `ORBIT_THREAD` | The thread's id. |

Then the project's own `env`, which may override any of them.

## Other tools' files

Import also reads, where present (weakest first; `orbit.toml` wins):

| File | Read as |
|---|---|
| `package.json` | `dev`, `start`, `test`, `lint`, `typecheck`, `build`, `storybook` scripts as targets, run with the package manager its lockfile names, and its install as setup. Suggestions only: they never replace what is set. |
| `Cargo.toml` | `cargo test` and `cargo clippy` targets, as suggestions. |
| `.devcontainer/devcontainer.json` | `postCreateCommand` as setup, `containerEnv` and `remoteEnv` as environment, the first `forwardPorts` entry as the dev server's address. |
| `.codex/environments/*.toml` | `[setup] script` as setup, `[[actions]]` (`name`, `command`) as targets. |
| `conductor.json` | `scripts.setup` as setup, `scripts.archive` as teardown, `scripts.run` as a `Run` target. |
