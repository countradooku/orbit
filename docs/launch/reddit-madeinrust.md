**Title:** Orbit: a native Rust desktop app for running coding agents (T3 Code alternative, no Electron). Built entirely by AI

---

Hey r/madeinrust,

I want to share **Orbit**, a desktop app for running coding agents (Claude Code, Codex, and anything that speaks ACP) side by side. It's written in Rust on GPUI, with no Electron, no webview and no Node.

**Repo (MIT):** https://github.com/countradooku/orbit

**Full disclosure up front:** this was built entirely by AI. I directed it, made the product and stack decisions and tested it, but the code was written by Claude Code. I'm telling you up front because I think you should know.

### Why I made it

- **The recent controversy around T3 Code and user data.** It made me want a tool like this that I can actually read and trust, that keeps everything local, and that I control.
- **I wanted something that isn't Electron.** An app that sits open all day managing agents shouldn't need a gigabyte of RAM and burn CPU while it does nothing.

### Numbers

Windows 11, release builds, median of 5 runs, against T3 Code on the same machine:

| | Orbit | T3 Code (Electron) |
|---|---:|---:|
| Window up | 238 ms | 461 ms |
| Memory (whole process tree) | ~93 MB | ~1,035 MB |
| Processes | 2 (app + background host) | 7 |
| Idle CPU | 0.00 % | 7.66 % |
| On disk | ~43 MB | ~235 MB |

The methodology and the script to reproduce this are in `docs/BENCHMARKS.md`.

### What it does

- **Parallel agent threads,** each optionally in its own git worktree. Approve or deny tool calls, steer mid-turn, interrupt.
- **Review and undo:** inline diffs for every edit an agent makes, a review sheet per turn or per thread, and checkpoints with undo. This works even in folders that aren't git repos (it keeps a shadow repo).
- **A background host:** agents keep running after you close the window. The built-in terminal has scrollback and hibernation.
- **Remote hosts** over your own `ssh` (your keys, agent, ProxyJump), plus WSL. There's a host switcher, so several machines can run in one window.
- **Teams of agents,** a usage and quota board with failover, automations, a CLI (`orbit`) that doubles as an MCP server, and a built-in browser.
- **Plugins:**
  - Data plugins: themes, prompt templates and recipes.
  - Agent manifests: add any agent CLI with a TOML file.
  - Native panels.
  - WebAssembly hooks in a wasmi sandbox (e.g. a guard that blocks `rm -rf /`), behind capability consent and an audit log.
- **Voice dictation** with on-device Whisper on candle (pure Rust): Ctrl+M in any text field; about 0.5 s from releasing the key to text on a desktop CPU.

A lot of the inspiration comes from **T3 Code** (the original idea and much of the UX), **Orca**, **PandaCode**, and the Cursor and Codex desktop apps. Huge respect to the people behind those.

### Stack, for the Rust folks

- **UI:** GPUI + GPUI Kit, on gpui-fast (Longbridge's GPUI fork, the one that actually idles at 0 % CPU).
- **Channels and websockets:** crossfire channels instead of tokio's, and sockudo-ws for websockets.
- **Wire format:** wincode, with a small framing layer that batches streaming tokens.
- **Plugins:** wasmi, chosen over wasmtime: about 1.2 MB against 11.6 MB, and a 20 s build against 193 s.
- **Voice:** candle, with a custom KV-cached Whisper decoder.

### Honest status

- It's been developed and tested mostly on **Windows**. macOS and Linux are supported, and CI builds and tests all three (green), but they've had much less hands-on testing. Expect rough edges.
- It's early. Bug reports are very welcome.

### What's next

I'm planning something like **T3 Connect**: reaching your agents and machines from anywhere, done in a way that keeps your data yours.

**If people want this, I'm happy to maintain it long term.** Issues, ideas and PRs are welcome, and if you'd use it, a star or a comment tells me it's worth continuing.

Thanks for reading!
