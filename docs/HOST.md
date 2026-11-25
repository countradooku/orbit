# The background host

Closing Orbit no longer stops its agents and terminals. On a desktop the
engine (the store, agent sessions, PTYs, automations, chat bots) runs in a
per-user host process, `orbitd --local`, and the window is a client of it.
The window can close, crash or be replaced by an update; the host keeps the
work going and the next window picks it up: timelines catch up through the
usual epoch/seq replay, and terminals re-attach with their scrollback.

```
orbit-desktop (window)  ──named pipe / Unix socket──▶  orbitd --local (host)
orbit (CLI, MCP)        ──loopback WebSocket────────▶    engine, PTYs, agents, bots
```

## Processes and lifetime

**Starting.** The app looks for a live engine on its data folder
(`<data>/endpoint.json`, see [CLI.md](CLI.md)). If there is none it starts
`orbitd --local --data <dir>` from next to its own executable
(`ORBIT_HOST_BIN` overrides). The window does not wait: the connection
waits for the host's endpoint on its own thread while the interface sets
itself up. The host is detached: its own process group (and session on
Unix), no console, outside the app's job object on Windows, so closing the
window or the terminal that launched Orbit does not take it down. It logs to
`<data>/logs/orbitd.log`, beside the app's `orbit-desktop.log` (moved to
`orbitd.log.old` past 1 MB); a panic is logged there too.

**Single instance.** The store's lock file is the single-instance guard: a
second host started at the same moment waits up to five seconds for the
lock (a restarting host releases it) and otherwise exits. The window's own
single-instance lock (`app.lock`) is unchanged.

**Attaching.** Every window sends `Command::Host(HostCommand::AttachUi)` on
each connection. The host counts attached windows per connection; a window
that disappears (closed, killed, crashed) detaches when its pipe closes.

**Exiting.** With no window attached, the preference *Settings › General ›
Background › Keep agents running after closing Orbit* decides, read from
`<data>/host.json` each time:

| Choice | The host exits |
| --- | --- |
| Always | never on its own |
| While working (default) | once nothing is working for 60 s: no running turn, no terminal running a command, no automation due in the next 10 minutes |
| Never | 5 s after the last window closed (enough for the window to restart) |

"A terminal running a command" means the shell is not at its prompt: on Unix
the PTY's foreground process group is not the shell's, on Windows the
shell's job object holds more than the shell. An idle shell does not keep the
host up. While a window is attached the lifetime loop sleeps on a notifier
(no timer at all); without one it checks once a second.

**Tray.** The host has no tray or menu-bar icon. GPUI has no status-item API
that works without a window, and the host is deliberately headless (it does
not link GPUI). Instead, when Orbit opens on a host that was already running
and is still busy, the window says so: "Kept running while Orbit was closed:
2 agents are working."

**Fallback.** If the host binary is missing or cannot be started (the
spawn fails), the window runs the engine in-process, as before, and shows
"No background host" with the reason. A host that starts and then exits
(the end of its log says why) or does not serve within 10 s is retried by
the connection, which shows the reason in the connection status.
`--in-process` or `ORBIT_IN_PROCESS=1` asks for the in-process engine
directly (tests, benchmarks, debugging). A remote host (Settings › Remote,
`ORBIT_HOST`) is used as before.

## Transport

The window talks to the host over a named pipe on Windows and a Unix domain
socket on macOS and Linux (`orbit_net::transport`): the same frames as the
WebSocket link, each prefixed by its length. The CLI and remote clients keep
using WebSockets; the host serves both, and `endpoint.json` lists both
(`url` and `local`).

* Windows: `\\.\pipe\orbit-host-<64 random bits>`, created as the first
  instance of its name (a squatter makes the host fail instead of
  intercepting the window), remote clients rejected.
* Unix: `<data>/host-<random>.sock` (or the temp folder when that path is
  too long for a socket), mode 0600.
* The first frame must still carry the host's token, as on every transport.

Measured on the reference Windows machine (release builds):

| | Named pipe | Loopback WebSocket (TCP) |
| --- | ---: | ---: |
| Raw round trip of a 36-byte frame, p50 / p99 (`cargo run -p orbit-net --release --example transport_rtt`) | **10.4 µs** / 20 µs | 18.2 µs / 43 µs |
| Orbit command round trip including the engine, p50 / p99 (`orbit-bench`) | **30.5 µs** / 58 µs | 35 µs / 78 µs |
| Streaming tokens engine → client state (`orbit-bench`) | 4.7 M deltas/s | 4.4 M deltas/s |
| In-process command round trip, for reference | 0.4 µs | |

Medians of three runs on an idle machine. The pipe is picked: it meets the
50 µs budget for a whole call with room, its tail is a quarter shorter, and
it needs no port. Most of the remaining 30 µs is the four thread hops of a
call (window → client thread → host connection → engine thread and back),
not the transport.

## Versions, restarts and updates

`PROTOCOL_VERSION` is 12 (11 was usage, 10 workspaces). A window and a host of the same protocol talk
normally; the window checks the version in `endpoint.json` before it
connects.

**A host of another protocol** (an update installed while the old host kept
running) cannot be spoken to with the binary protocol, so every host also
answers a **control request**: a JSON first frame instead of `Hello`,
`{"token","op":"status"|"restart","when":"idle"|"now"}`, answered with a JSON
status (pid, version, protocol, windows, working turns, busy terminals,
sleeping terminals, next automation, restart pending). Its fields are only
ever added, so any later window can ask any host since protocol 10 what it is
doing and tell it to restart. The window then:

* restarts it right away when nothing is working;
* otherwise asks: **Restart the background host?** "It runs Orbit 0.1.0, and
  2 agents are working. Restarting now stops them; their threads keep what
  they did so far." with **Restart now** and **Restart when finished**.

Once the old host exits, the window starts its own and connects; the
snapshot replaces its state (a new epoch).

**A host of the same protocol but another build** is asked to restart when
idle (`HostCommand::Restart { when: WhenIdle }`); the window reconnects to the
new host transparently.

**For the updater:**

* The app installs `host::HostRestart` as the updater's
  `update::host::RestartHost` whenever the engine runs in the background
  host: no agents are at risk, and `prepare` flushes settings, holds the host
  for a minute and detaches the window, so installing an update replaces
  only the window. The new window finds the old host still running: of the
  same protocol, it is asked to restart into the new binary once idle; of
  another, the user is asked as above. The installer moves the running
  `orbitd` aside (`.orbit-old`); a still-running old image is left for the
  next cleanup.
* `Command::Host(HostCommand::Restart { when })` drains and restarts the
  host: `WhenIdle` waits until no turn runs and no terminal runs a command,
  `Now` stops them. The host closes its endpoint first (so a reconnecting
  window starts the next host, which waits for the store lock), persists and
  exits. The window that is connected starts the new binary when it
  reconnects. The same is available as the control request `restart`.
* `HostCommand::Hold { ms }` keeps the host up for `ms` with no window
  attached, whatever the preference.
* `orbit-app`'s `host::relaunch(app, exe, cx)` restarts the window without
  stopping the host: it holds the host for a minute, starts `exe` with
  `--wait-pid <this pid>` (the new window waits for the old one to exit, then
  takes the single-instance lock), and quits. Terminals stay running and the
  new window re-attaches to them.

## Terminals that survive

PTYs live in the host. Every interactive terminal (opened by a thread's
panel, not a CLI `RunInTerminal` command) is a session in
`engine/host.rs`:

* **Scrollback ring.** The host keeps the newest 256 KiB of each terminal's
  raw output (`terminal::Scrollback`, one copy shared with
  `orbit terminal read`). A panel that comes back
  (`HostCommand::AttachTerminal { thread, cwd, cols, rows, wake }`) finds its
  thread's terminal and gets the scrollback to draw into a fresh screen. When
  older output was dropped, the replay starts at a line or escape-sequence
  boundary and first re-enters the private modes that were on at the cut
  (alternate screen, mouse reporting, bracketed paste, hidden cursor, focus
  events, win32 input mode), so a full-screen program is not drawn into the
  normal screen. Queries the program sent (cursor position, device
  attributes, colors) are left out of the replay: answering them again would
  type the answers into the shell.
* **Nothing lost or doubled.** Output after the attach reply is held by the
  host until the panel has drawn the scrollback and says
  `HostCommand::TerminalReady` (released after 5 s regardless), then sent as
  one chunk. A full-screen program attached at the same size is nudged to
  redraw (a one-row resize and back).
* **Closing the window** leaves terminals running: the window detaches
  before its views are released, so panels close without closing their
  shells. Closing a terminal panel (or its thread) still closes the shell.
  When Orbit opens again, a thread whose terminal the host kept shows it
  again, unfocused.
* **Reconnects.** When the connection to the host comes back (the host
  restarted, or the pipe broke), panels attach again on a fresh screen, and
  the window tells the engine again that it is in front (usage limits are
  read only while a window was in front lately).
* **Run targets** (`orbit.toml`) run in engine-owned terminals, so in the
  host they survive the window too, keep the host up while they run, and
  their output stays in the same scrollback ring the run dock replays.

## Hibernation

Idle agent processes are released after 15 minutes (unchanged); the thread
resumes its session on the next message. Terminals now sleep too: a terminal
with no input or output for 30 minutes (`ORBIT_TERMINAL_SLEEP_SECS`, 0 turns
it off) whose shell runs nothing, with no agent in it and whose thread is not
working, has its shell stopped: no process, no PTY, a few hundred KB of
scrollback. Its panel shows a small **Sleeping** mark. Focusing the panel,
typing into it, or attaching with `wake` starts a fresh shell in the same
folder under the same terminal id, after a dim "Woke from sleep" line; the
scrollback stays above it. A panel restored when Orbit opens shows a
sleeping terminal asleep until it is focused. `HostCommand::SleepTerminal`
and `WakeTerminal` do the same on demand. Sleeping terminals of deleted
threads are dropped.

## Memory and CPU

`scripts/measure-desktop.ps1 -Runs 5` (median of five; the host counted as
part of the app's process tree), seeded data folder, release builds, on the
same machine and day, idle:

| | Window up | Processes | Working set | Private | Idle CPU |
| --- | ---: | ---: | ---: | ---: | ---: |
| Orbit, engine in the background host | 238 ms | 2 | 92.8 MB | 121.5 MB | 0.00 % |
| of which the host (`orbitd --local`) | | 1 | 20.3 MB | 27.6 MB | 0.00 % |
| Orbit, engine in-process (`--in-process`) | 245 ms | 1 | 79.1 MB | 104.1 MB | 0.00 % |

The split costs about 14 MB of working set and 17 MB private in total (a
second process's runtime, allocator arenas and stacks); the window's own
process is 6.6 MB / 10 MB smaller than before since the engine left it.
Window-up time is unchanged: the host starts while the interface sets
itself up. The host started in about 125 ms in a debug build (spawn,
opening the store, publishing the endpoint). An idle host uses no CPU:
with a window attached its lifetime loop waits on a notifier, and the engine
ticks only as it did in-process.

## Tests

* `crates/orbitd/tests/host.rs` runs the real `orbitd --local` with a
  scripted agent (`ORBIT_HOST_FAKE_AGENT_MS`) through `LocalHost`:
  * an agent turn and a terminal survive the window closing; on return the
    turn has finished, its answer is in the timeline, and the terminal
    replays its output and still runs;
  * a windowless host stays while a turn runs and exits after the grace
    period once it ends; an idle one exits after the grace period; `always`
    keeps it;
  * a restart request (`WhenIdle`) waits for the turn, then the window starts
    and reconnects to a new host;
  * a window of a newer protocol asks an older host for its status over the
    control request, restarts it and connects to the new one;
  * with no host binary (or one that cannot run) the engine runs in-process
    with a reason.
* `crates/orbit-core/src/engine/tests/host.rs`: activity, re-attach with
  scrollback and held output (checked byte for byte against what the
  terminal wrote), sleep, restored-asleep attach, wake on input, closing a
  sleeping terminal.
* `crates/orbit-core/src/terminal.rs`: the ring, mode restoration, query
  stripping, and `busy` telling a running command from a prompt.
* `crates/orbit-net`: the local transport round-trips frames up to 1 MiB, a
  second host cannot take a live pipe name, the policy file, control frames
  told apart from `Hello`.

## Not done yet

* Scrollback does not survive the host itself exiting (a reboot, or an idle
  host that exited): Orca also checkpoints screens to disk; Orbit keeps them
  in memory only.
* The CLI still connects over WebSockets, not the pipe.
* Stopping or restarting the host by hand from the interface (a "Background
  host" panel listing what it runs) is not there; the control request and
  `HostCommand::Restart` are.
