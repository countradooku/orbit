# Status: how Orbit knows what an agent is doing

Orbit shows one status per agent: working, waiting for you, done, failed,
rate-limited, or unverified. The rule behind all of them is that Orbit never
guesses "working" or "idle". When the evidence is weak, Orbit says so. When
there is no evidence, the agent is **unverified**.

The code lives in `crates/orbit-core/src/status/` (the state pieces, time
passed in), `crates/orbit-core/src/engine/status.rs` (the engine side), and
`crates/orbit-proto/src/status.rs` (the protocol types and the glyph).

## Two layers

| Layer | Type | Stored | Says |
|---|---|---|---|
| Durable status | `ThreadStatus` on `Thread` | Logged | What the engine last decided: `Idle`, `Starting`, `Running`, `Waiting`, `RateLimited`, `Failed`. |
| Live evidence | `ThreadLive` | Never logged. Sent as `Event::ThreadLive` and in `Snapshot::live` | How fresh that decision is (`Liveness`), and which agents run in the thread's terminals (`TerminalAgent`). |

`LiveStatus::of(thread, live)` combines both layers into the glyph that every surface
draws. `ClientState::live_status` is the same call for clients. The glyphs, from most
to least urgent, are `Permission`, `NeedsYou`, `Failed`, `RateLimited`,
`Working`, `Quiet`, `Unverified`, `DoneUnread` and `Idle`. A thread's glyph is
the most urgent of its own glyph and the glyphs of its terminal agents.
The app draws these with `ui::Look` and `ui::StatusGlyph` (rows, cards, tiles, the thread header): `Quiet` is the working arc standing still and faded, `Unverified` the dashed ring, and a glyph decided by a terminal agent Orbit only read from the screen (heuristic evidence) is drawn faded and named "Probably: …". The hover adds `liveness_line` ("No output for 4m · cargo test") and one `agent_line` per terminal agent. The thread's terminal panel sends `Command::BindTerminal` with its thread when its shell starts.

Live evidence is ephemeral by design. After a restart nothing is running, so
nothing needs it. Live status arrived with wire protocol version 3
(`PROTOCOL_VERSION`, now 4). It changed no logged layout: `Event` gained two
variants but stays in its packing class (17 to 19 variants, test
`durable_events_logged_before_live_status_still_decode`); meta schema 4 came
later, for question option descriptions and the review settings.

## Thread sessions (structured protocols)

```
            send                 agent events            TurnFinished
  Idle ─────────────▶ Running ◀──────────────▶ (Live) ───────────────▶ Idle / Failed / RateLimited
                        │  ▲                                              ▲
            Request     ▼  │ answer                                       │ exit, lost contact
                      Waiting ────────────────────────────────────────────┘
  Freshness while Running:  Live ──silence≥threshold──▶ Quiet ──any event──▶ Live
                            Live/Quiet ──sleep or clock jump──▶ Unverified ──any event──▶ Live
```

| State | Evidence that sets it | What clears it |
|---|---|---|
| `Running`, `Live` | A turn started (`start_turn`, `TurnStarted`) and the session reported within the threshold. | The turn ends. |
| `Running`, `Quiet { since, tool }` | No agent event for the provider's threshold: Claude, OpenCode and Pi 2 min, Codex 90 s, ACP 3 min. The process is known to be alive: its exit is watched. `tool` names the open tool call, as in "No output for 4m · cargo test". | Any agent event. |
| `Running`, `Unverified { reason }` | The tick saw the machine sleep or the clock jump. | Any agent event. |
| `Waiting` | The agent raised a request. Silence here is expected and never ages. | The answer, the request being withdrawn, the turn ending, or the agent exiting. |
| `Failed` | The turn failed, the process exited during the turn, or the session vanished. | The next message. |
| `RateLimited` | The provider reported its quota exhausted. | The reset, which the timer catches up after sleep. |
| `Idle` (+ `unread` = done) | The turn finished. | The next message. |

### The lies this fixes

| Lie | Fix | Test |
|---|---|---|
| The agent died, but a tool it started kept stdout open, so the turn showed "working" forever. | `AgentProcess` reaps the exit in its own task (no polling). `JsonlReader::until_exit` ends the stream `EXIT_DRAIN` (750 ms) after the exit, whoever holds the pipe. OpenCode's loop watches the exit too. | `tests/status.rs` `an_agent_that_dies_while_a_child_holds_its_stdout_still_ends_the_turn` |
| The session task vanished without an `Exited` (a panic, a bug), and the thread was stuck "working". | Each tick checks whether the session's input channel is closed. On two sightings a tick apart, the turn fails with "Orbit lost contact". | `engine/tests/status.rs` `a_session_that_vanishes_without_a_word_fails_its_turn` |
| The provider stopped streaming but is still alive, and the thread showed "working" as if all were well. | A per-turn last-signal clock and the `Quiet` freshness, naming the open tool. | `a_silent_turn_shows_no_output_instead_of_working`, `a_quiet_turn_names_the_tool_it_is_running` |
| Waiting for an answer looked like silence. | Waiting turns, and turns held for Orbit's own snapshot, never age. | `waiting_for_the_user_is_never_called_silence` |
| A crash during an approval left a live approval card for a dead agent. | Requests still open when a turn ends are marked `Cancelled`. | `a_crash_during_an_approval_closes_the_request` |
| An agent exited outside a turn with a request open, and the thread stayed "waiting" forever. | Its requests are settled ("The agent stopped before this was answered") and the thread goes idle with a notice. | `an_exit_outside_a_turn_settles_requests_left_behind` |
| A missed result, or a turn that ended while the engine was busy elsewhere, left the status out of step with reality. | `repair_statuses` runs on every tick: `Running` with no turn becomes `Idle`, `Waiting` with nothing pending becomes `Running` or `Idle`, and `Idle` with a live turn becomes `Running`. Each repair is logged. | `impossible_statuses_are_repaired_on_the_tick` |
| After sleep or resume, statuses were taken as still true. | `ClockWatch` compares consecutive ticks. A gap of 30 s or more, or a wall-versus-monotonic skew of 10 s or more, counts as sleep or a jump. Running turns and working terminal agents become unverified until they report. | `after_sleep_running_turns_are_unverified_until_the_agent_reports`, `status::tests::sleep_and_clock_jumps_are_noticed` |
| After sleep, snoozes and quota resets fired late, because timers count monotonic time, which stops during sleep on Linux and macOS. | The re-verify step wakes overdue snoozes and resumes overdue quota resets. | `after_sleep_overdue_snoozes_wake` |
| Orbit crashed while a thread was busy. | Already handled: the store resets busy threads on open, and `recover` denies unanswerable requests. | `store` tests |

## Agents in terminals

When the user types `claude`, `codex`, `gemini` or a similar command in an Orbit
terminal, `TermWatch` (`status/term.rs`) tracks the agent. A terminal opened in
the folder of exactly one open thread (or one thread overall) counts toward that
thread. `Command::BindTerminal` sets the thread explicitly. Every change is sent
as `Event::TerminalAgent` and as part of the thread's `ThreadLive`.

The evidence for a terminal agent ranks hook, then OSC 7501, then heuristic. A
weaker source never overrides a stronger one. There are two exceptions, and both
only move toward "not working":

- The user presses Esc or Ctrl+C while the agent works. No hook fires on an
  interrupt, so the agent becomes "probably stopped".
- The shell prints its prompt again (`OSC 133;A`), which proves the agent has left.

Liveness comes from output. A working TUI redraws its spinner many times a
second, so an agent that is "working" but prints nothing for 2 minutes becomes
`Unverified`. Fresh output restores the previous state. Finished and waiting
states do not age.

### (a) Hooks

- **Endpoint.** The endpoint is a Unix socket at `<data>/hooks.sock`, or a named
  pipe `\\.\pipe\orbit-hooks-<hash of data dir>` on Windows. It starts with the
  first terminal, so an engine without terminals does nothing. Each terminal gets
  a random `ORBIT_TERM_TOKEN`, and calls without a live terminal's token are
  ignored.
- **Invoker.** The invoker is `orbit-hook` (a binary of `orbit-app`, found next to
  Orbit's executable). It reads the hook payload and sends the event, tool,
  `notification_type` and message. It parses large payloads lazily, never prints,
  times out in 500 ms and always exits 0. When the endpoint is unreachable, for
  example with an agent on an SSH host, it writes the same report as `OSC 7501`
  to `/dev/tty` on Unix.
- **Injection.** Injection uses the terminal's environment only. Orbit never
  writes to the user's own agent configuration:
  - Claude Code gets `--settings <data>/hooks/claude-settings.json`. The file
    registers `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`,
    `PostToolUse`, `Notification`, `Stop` and `PreCompact`. Subcommands such as
    `claude mcp` and `claude update` run unchanged.
  - Codex gets `-c notify=['<orbit-hook>','codex']`, but only when the user has
    no top-level `notify` of their own.
  - On Unix, the wrapper is a `PATH` shim in `<data>/hooks/bin`.
  - On Windows, the wrapper is a pair of PowerShell functions passed with
    `-NoExit -EncodedCommand`. An inline command runs even where the execution
    policy blocks scripts.
- **Mapping.** `hook_effect` maps hook events to states. `UserPromptSubmit`,
  `PreToolUse` and `PostToolUse` mean Working, with the tool as detail.
  `Notification` maps by `notification_type`: `permission_prompt` means
  approval, `elicitation_dialog` means a question, and `idle_prompt` means done.
  `Stop` and Codex's `agent-turn-complete` mean done. `StopFailure` means failed,
  or rate-limited when it says so. `SessionStart` and `SessionEnd` start and end
  the agent.

### (b) OSC 7501

`status/osc.rs` scans PTY output with `memchr`, without copying. A sequence that
spans two reads is buffered, up to 4 KiB, and longer sequences are dropped. A
report is `ESC ] 7501 ; key=value;… (BEL | ESC \)`. The keys are `state`
(`working`, `blocked`/`waiting`, `idle`/`done`, `failed`, `ratelimited`,
`exited`), `kind` (`permission`, `question`, `auth`), `agent` and `msg`. Values
use `%XX` escapes. A bare first word is read as the state, and unknown keys and
values are ignored. The same scanner reads window titles (`OSC 0` and `OSC 2`)
and shell prompt marks (`OSC 133;A`). A `?` probe registers an agent-like
program without a state.

### (c) Heuristics (always marked as guesses)

- **Typed command.** Orbit recognises a known agent typed at the prompt,
  including `FOO=1 claude`, `npx @openai/codex` and full paths. Lines edited with
  history or completion are not trusted. A heuristic agent becomes "probably
  working" when a prompt is submitted to it.
- **Title.** A braille spinner in front of the title means working, and Claude
  Code's `✳` means idle. A spinner alone, as from a build tool, does not count as
  an agent.
- **Approval prompts.** Orbit looks for the prompt text that agents print, such
  as "Do you want to proceed?" or "Would you like to run the following command".
  A match means "probably waiting for your approval".

## Cost

- **Idle engine.** There are no new timers. The 500 ms tick does a few map
  lookups, plus one clock comparison.
- **Turns.** Each agent event updates one timestamp. Events go out only when the
  freshness changes, or when a turn starts or ends.
- **Terminals.** Each output chunk gets one `memchr` pass for ESC. Prompt
  patterns are only searched while a heuristic-only agent runs. Keystrokes are
  inspected only while no agent runs, with a 512-byte line buffer.

## Verified, and not

**Verified by tests on Windows:**

- Every lie listed above.
- The OSC scanner at every split point, with every pair of split points, with
  200 KB of random noise, and with oversized and interrupted sequences.
- The hook wire format and payload parsing.
- The real invoker process reaching the endpoint, and never hanging without one.
- A full terminal round trip: a fake Claude, started from a real PowerShell
  terminal, fires the hooks Orbit injected through the command in the injected
  settings file. Orbit reports working, needs approval, done and ended, and the
  `claude` wrapper function is present in that terminal.

**Not verified:**

- **Unix shim.** The shim was checked by hand with `sh`. The end-to-end test runs
  it on Linux and macOS CI only.
- **macOS login shells.** On macOS, `path_helper` in a login shell can move
  system directories ahead of the shim. If `claude` lives there, its hooks are not
  injected, and the agent falls back to OSC 7501 and heuristics, marked as such.
- **Claude Code's own OSC 7501 reports (v2.1.295 and later).** Claude Code
  reports only after the terminal answers the `OSC 7501 ; ?` probe. The reply
  format is not published, so Orbit does not answer yet. The parser accepts the
  documented fields if they arrive.
- **ConPTY.** Whether ConPTY passes `OSC 7501` through to Orbit is untested. The
  hook endpoint does not depend on it.
- **Codex.** `notify` reports only turn completion. Codex's "working" in a
  terminal is a guess based on typed input and output. Codex's newer hooks system
  is not injected.
- **Agents with no hooks injected** (Gemini, OpenCode and others). These rely on
  OSC 7501 when the agent emits it, and on heuristics otherwise.
- **Packaging.** `orbit-hook` must ship next to the Orbit executable. `orbitd`
  finds it only when both are installed in the same folder. Without it, hooks are
  not injected and everything else still works.
