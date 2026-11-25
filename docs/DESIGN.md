# Orbit — product and architecture

Orbit is a native desktop control surface for coding agents (Claude Code,
Codex, and any agent that speaks ACP). It is written in Rust on GPUI Kit and
GPUI Fast. It is not a port of T3 Code's interface: it keeps every capability
T3 Code has, removes the failure modes users report across T3 Code, Codex,
Claude desktop, Conductor, Cursor and others (see
[research/orchestrator-feedback.md](research/orchestrator-feedback.md)), and is
designed around the work people actually do: start parallel tasks, answer what
the agents need, and review what they produced.

## 1. The task, before the screens

| Question | Answer |
| --- | --- |
| Primary task | Get correct changes out of several agents with as little reading and waiting as possible. |
| Objects | **Project** (a repository), **Thread** (one agent conversation, optionally on its own worktree), **Turn** (one request and everything the agent did for it), **Change** (files the turn touched), **Request** (an approval or question the agent is blocked on). |
| Always available | New thread, send / steer / queue, stop, approve or deny, open review, switch thread, command palette. |
| Information needed to decide | What the agent is doing now, what it is asking, what it changed, how much context and quota remain. |
| States | Empty project, no provider installed, provider signed out, connecting, running, waiting on you, rate-limited (with reset time), failed, interrupted, offline remote host, readonly (archived). |
| Keyboard path | `Ctrl+K` palette → `Ctrl+N` new thread → type → `Enter` send (configurable) → `Ctrl+Enter` steer while running → `Ctrl+.` stop → `Ctrl+I` inbox → `A` / `D` approve / deny → `Ctrl+R` review → `Ctrl+Shift+[ / ]` previous / next thread. |

## 2. Shell

A **sidebar workspace** with an optional inspector, all regions resizable with
persisted, clamped widths:

```
┌ title bar ─ project ▸ branch ─────────────── context ▮▮▮▯ 62% · $0.84 · quota ┐
│ sidebar          │ thread                                 │ inspector       │
│  Inbox (3)       │  timeline (virtualized, inverted)      │  Review         │
│  Fleet           │   turn ▸ message, reasoning, tools,    │   files ▸ hunks │
│  ─ project A     │          file changes, plan, request   │   per-hunk undo │
│    ● thread 1    │                                        │  Terminal       │
│    ◐ thread 2    │  composer (steer / queue, model,       │  Plan           │
│  ─ project B     │            effort, mode, attachments)  │                 │
└──────────────────┴────────────────────────────────────────┴─────────────────┘
```

* **Inbox** is the attention queue: every request, question, finish, failure
  and rate-limit hit across all threads, each answerable inline with the
  context needed to decide (the command, the file, the diff excerpt). It
  replaces context-free badges.
* **Fleet** is a status board, not kanban: agent, branch, state, last activity,
  diff size, cost, conflicts, with subagents nested under their parents.
* Thread rows show state through a fixed status lane (working, waiting on you,
  unread result, failed); waiting-on-you never looks like working.
* The context, cost and quota meter is always visible and never hidden by
  default.

## 3. Capability inventory

Everything T3 Code offers, plus what users ask every tool for. **New** marks
what T3 Code does not have.

### Threads and turns
- Multi-provider threads: Claude Code (CLI stream-json control protocol),
  Codex (`codex app-server` JSON-RPC), ACP agents. Model, reasoning effort,
  and interaction mode (chat / plan / auto) per thread; the last used values
  become the next thread's defaults (T3 #6508).
- Access levels: Readonly, Ask (default), Auto-edit, Full. A new thread never
  defaults to Full.
- Streaming text, reasoning, tool calls, command output, file changes, plans,
  todo lists, subagent tasks, images.
- **Steer or queue** follow-ups while a turn runs; edit and cancel queued
  messages (T3 #231, Codex #44781).
- **Edit and resend**, **fork from any message**, real delete that deletes.
- **Auto-resume** after a rate-limit reset, with the reset time shown.
- Slash commands and skills reported by the provider, passed through intact.
- Attachments: images and files, stored once by content hash.
- Full-text search across all threads.

### Review
- Diff of the last turn, of the whole thread, or against any base branch.
- File tree with additions and deletions; unified or split; word-level
  highlights; wrap toggle; colorblind-safe palette option.
- **Per-hunk and per-file revert**, inline comments sent back to the agent as
  a follow-up.
- Checkpoints per turn and revert to any checkpoint (non-destructive: the
  current state is checkpointed first).
- Commit, push, create PR; merge locally without a PR.
- **Conflict detection** between live worktrees that touch the same files.

### Workspace
- Projects are existing local checkouts (no clone, no OAuth).
- Worktree per thread on request, with `.worktreeinclude` (copy `.env` and
  untracked files), setup and teardown scripts, a unique port range exported
  as `PORT`/`ORBIT_PORT_BASE`, disk usage shown, and cleanup on archive.
- Multiple integrated terminals per project or thread (ConPTY on Windows).
- Open in editor / file manager.

### Shell and platform
- Command palette for every command; every command also has a menu path and
  a key binding where natural.
- Native notifications for requests, finishes and failures; taskbar badge.
- Light and dark themes from the system; interface zoom through the base font.
- Remote host: run `orbitd` on another machine or in WSL and connect over the
  Orbit protocol on WebSockets (sockudo-ws) with a token, reconnecting with
  `since: seq` replay so nothing is lost.
- Local-first: no account, no telemetry.

## 4. Engine architecture

```
 orbit-app (GPUI) ── crossfire ──┐                ┌── claude.exe (stream-json)
                                 ├─ orbit-core ───┼── codex app-server (JSON-RPC)
 remote clients ── sockudo-ws ───┘   (engine)     ├── git (batched, cached)
                   via orbitd                     └── ConPTY terminals
```

* **orbit-proto** — the Orbit Wire Protocol: ids, domain model, commands,
  events, bitcode payloads in CRC-checked frames. The same types travel the
  in-process bus, the disk log and the WebSocket.
* **orbit-core** — the engine. A small Tokio runtime (one worker) owns all
  I/O: provider processes, git, terminals, storage. Commands arrive on a
  crossfire MPSC channel; events leave through one bounded crossfire channel
  per subscriber, so a slow client never blocks the engine (it is dropped and
  re-syncs from its last sequence number).
* **orbitd** — headless host exposing the engine over sockudo-ws.
* **orbit-app** — the GPUI client. One background task drains the event
  channel in batches and applies each batch to the store entity with a single
  `notify`, so a burst of 500 deltas costs one render, not 500.

### Storage that stays small
T3 Code's event store grew to 26 GB with 2–50 s thread switches. Orbit:
* keeps thread metadata in one small log and each thread's timeline in its own
  log, loaded only when the thread is opened and evicted when it is not shown;
* never persists streaming deltas: an item is written once when it completes
  (and checkpointed every few seconds while it streams, overwritten, not
  appended);
* stores images and large tool outputs once in a content-addressed blob store;
* compacts a thread log when its dead bytes exceed its live bytes;
* deletes a thread's log, blobs it alone references, checkpoints and worktree
  when the thread is deleted.

### Git without process storms
T3 Code spawned ~1,650 git processes to show 272 files on Windows. Orbit
computes one `git diff` per change event for all files, parses it in Rust,
caches by tree id, and refreshes from a debounced file watcher, never on a
timer and never per scrolled file.

### Rendering
* Timelines and lists are virtualized; only visible rows exist.
* Markdown is parsed once per completed block; streaming text appends to the
  last block only.
* Animations explain change (enter, expand, status transitions), run on GPUI
  Fast's retained mode, stop when idle, and honor reduced motion. Nothing
  animates while nothing changes, so the idle app costs ~0% CPU.

## 5. Targets

| Metric | Target |
| --- | --- |
| Idle memory (one window, 20 threads indexed) | < 60 MB |
| Idle CPU | 0% (no timers when nothing changes) |
| Cold start to interactive window | < 150 ms |
| Open a 3,000-message thread | < 50 ms, 120 fps scroll |
| Engine delta throughput | > 1 M deltas/s |
| WebSocket round trip (loopback) | < 100 µs |
| Install size | < 25 MB |

The benchmark suite in `crates/orbit-bench` measures each of these for Orbit
on GPUI Fast, on upstream GPUI, and for T3 Code on the same machine.
