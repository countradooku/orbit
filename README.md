# Orbit

Orbit is a native desktop control surface for coding agents: Claude Code,
Codex, and any agent that speaks ACP. Start several agent threads in
parallel (each optionally on its own git worktree), answer what they ask for
from one inbox, and review what they changed hunk by hunk, with an integrated
terminal, checkpoints and revert, and commit/PR from the app. Dictate prompts
or speak to an agent by name with on-device Whisper ([docs/VOICE.md](docs/VOICE.md)).
It is written in
Rust on [GPUI Kit](https://github.com/longbridge/gpui-kit) and
[GPUI Fast](https://github.com/longbridge/gpui-fast), and runs on Windows,
macOS and Linux (X11 and Wayland). See [docs/DESIGN.md](docs/DESIGN.md).

| Crate | What it is |
| --- | --- |
| `orbit-proto` | Domain model, commands, events and the binary frame codec |
| `orbit-core` | The engine: storage, agent processes, git, checkpoints, terminals |
| `orbit-net` | The protocol over WebSockets: `orbitd`'s server and the remote client |
| `orbitd` | Headless host: runs the engine and serves it to remote clients |
| `orbit-terminal` | The terminal view (alacritty_terminal on GPUI Kit) |
| `orbit-app` | The desktop app (`orbit` binary) |
| `orbit-plugin` | Plugins: manifests, consent, signed catalogs, the WebAssembly sandbox ([docs/PLUGINS.md](docs/PLUGINS.md)) |
| `orbit-voice` | On-device dictation: Whisper on candle, VAD, model installs ([docs/VOICE.md](docs/VOICE.md)) |
| `orbit-bench` | Benchmarks |

## Build

You need a recent stable Rust (edition 2024), `git`, and the agent CLIs you
want to use (`claude`, `codex`) installed.

- **Windows:** the MSVC toolchain.
- **macOS:** the Xcode command line tools (`xcode-select --install`).
- **Linux (Debian/Ubuntu):** (`libasound2-dev` is also what dictation's
  microphone capture needs)

  ```sh
  sudo apt-get install -y build-essential pkg-config clang \
    libasound2-dev libfontconfig-dev libfreetype-dev libssl-dev \
    libvulkan1 libwayland-dev libx11-xcb-dev libxcb1-dev \
    libxkbcommon-dev libxkbcommon-x11-dev libzstd-dev
  ```

```sh
cargo build --release
cargo test --workspace    # live agent tests are #[ignore]d: add -- --ignored
```

## Run

```sh
cargo run -p orbit-app --release
```

Data lives in `%APPDATA%\Orbit` (Windows), `~/Library/Application Support/Orbit`
(macOS) or `~/.local/share/Orbit` (Linux); set `ORBIT_DATA` to use another
folder.

### Compare rendering backends

`orbit-app` renders on GPUI Fast by default. To build the same app on the
upstream GPUI that GPUI Kit pins:

```sh
cargo run -p orbit-app --release --no-default-features
```

### Run the engine elsewhere (`orbitd`)

`orbitd` runs the engine headless, for example on a build machine or in WSL:

```sh
cargo run -p orbitd --release -- --listen 127.0.0.1:7840   # also: --data <dir>
cargo run -p orbitd --release -- --print-token
```

Then point the app at it:

```sh
ORBIT_HOST=ws://host:7840/ ORBIT_TOKEN=<token> cargo run -p orbit-app --release
```

It listens on loopback by default; reach it from other machines through an
SSH tunnel or a private network such as Tailscale.

### Command line and MCP (`orbit`)

While the app or `orbitd` runs, the `orbit` CLI drives it, and `orbit mcp`
lets agents start and supervise threads of their own:

```sh
cargo run -p orbit-cli --release -- threads --needs-you
orbit new "Fix the flaky login test" --worktree --wait
orbit mcp install            # print the Claude Code and Codex registration
```

See [docs/CLI.md](docs/CLI.md).

## Benchmarks

```sh
cargo run -p orbit-bench --release             # markdown table
cargo run -p orbit-bench --release -- --json   # machine-readable
```

Desktop numbers (window-up time, memory and idle CPU of the whole process
tree) come from a Windows PowerShell script:

```powershell
./scripts/measure-desktop.ps1 -Label "Orbit (GPUI Fast)" -Exe target/release/orbit-desktop.exe
```

Results are in [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## Release bundles

`scripts/bundle.sh` (Linux `.tar.gz`, macOS `Orbit.app` in a `.zip`) and
`scripts/bundle.ps1` (Windows `.zip` with `orbit-desktop.exe`, the `orbit` CLI and `orbitd.exe`) build
and package a release into `dist/`. The Release workflow runs them on every
OS for `v*` tags.

## License

MIT. See [LICENSE](LICENSE).
