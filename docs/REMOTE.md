# Remote hosts

Orbit can run agents on other computers: over SSH, in a WSL distro, or on a
computer shared with this one. The window stays here; threads, worktrees,
agents and terminals live on the host, in that host's background
`orbitd --local` (see [HOST.md](HOST.md)), so they keep going when the window
closes or the network drops, exactly as on this computer.

```
orbit-desktop ──pipe──────────────────────────▶ orbitd --local            (this computer)
              ──ssh host orbitd --local --stdio ─▶ orbitd --local         (an SSH host)
              ──wsl.exe -d Ubuntu -- … --stdio ─▶ orbitd --local         (a WSL distro)
              ──ws://100.x.y.z:7840/ + token ──▶ orbitd --local, shared  (a paired computer)
```

## Hosts

The window's hosts are a list kept by this client in `<data>/hosts.json`
(owner-only: a direct host's token is in it). It is never sent to an engine.

| Kind | What it is | Reached with |
| --- | --- | --- |
| This computer | The local background host | The named pipe or Unix socket |
| SSH | `user@host`, `host:port` or an alias from `~/.ssh/config`; an identity file optionally | The system `ssh` |
| WSL | A distro on this Windows computer | `wsl.exe -d <distro> --` |
| Direct | An `orbitd` listening on the network, with a token | WebSocket |

**Adding** (the switcher's menu, or Settings › Hosts › Add host): the SSH tab
lists the computers in `~/.ssh/config` (`Host` blocks with plain names;
patterns, `Match` and git services such as GitHub are left out; `Include` is
followed). The WSL tab lists `wsl.exe -l -q`. The Direct tab takes an address
and a pairing code or a token. Adding a host switches the window to it.

`ORBIT_HOST` and `ORBIT_TOKEN` still name a direct host for one run; it is
shown but not saved. The single remote host older versions kept in
`<data>/connection` joins the list once.

## One host at a time, the others in the background

The window shows one host's threads at a time and switches between hosts;
every other host it connected to stays connected in the background.

Why not merge every host's threads into one list: every id in the protocol
(threads, projects, terminals, items, drafts) and every path is local to its
engine, and `ClientState` and about a hundred views key on them. Namespacing
them all would touch every view and command for a list that mixes paths from
different machines. Instead:

* `AppState` keeps one session per host (its backend, `ClientState` and
  pump). The shown session is applied as before; a background session keeps
  only its records current. No timeline is open there, so agents' token
  streams don't reach it; it costs a connection and a few kilobytes.
* The host switcher (the sidebar's foot, where "This computer" was) lists
  every host with its state and **what waits there** ("2 working · 1 need
  you"), so nothing on another host goes unnoticed.
* Switching is instant for a connected host: no reconnect, no snapshot. The
  window closes the shown host's views first, so their drafts and timelines
  go back to that host and its terminals keep running there, then shows the
  other host in a fresh workspace, where its window was last (route and
  history are remembered per host).
* **Run on**: the Home composer reads "Working in *project* on *host*"; the
  host picker switches the window, so the next thread starts there.

The window remembers the host it showed last and opens on it.

## SSH

Orbit runs the system's `ssh` and `scp` (`ORBIT_SSH` and `ORBIT_SCP`
override them), never a library of its own, so the user's config, keys,
agent, `ProxyJump`, `ControlMaster` and `known_hosts` apply as in a
terminal. Every call carries `BatchMode=yes`, a 15 s connect timeout and
keepalives (`ServerAliveInterval=15`, three misses): nothing ever waits on a
prompt, and a dead network is noticed within a minute.

**Connecting** probes first, in one round trip:

```
~/.orbit/bin/orbitd --probe || orbitd --probe || uname -sm
```

`orbitd --probe` prints one JSON line (version, protocol, platform, its own
path, home). A matching protocol connects with
`ssh -T host <that path> --local --stdio`; anything else asks for an install,
with the platform from `uname`.

**`orbitd --local --stdio`** makes sure that computer's background host runs
on its data folder (starting it detached, in its own session, so it outlives
the SSH session), then relays length-prefixed frames between stdin/stdout and
the host's local socket, putting the host's token into the client's first
frame: whoever can run programs there as that user has what the token
protects. A host of another protocol (just updated) is restarted into the new
binary when nothing runs on it; otherwise it is asked to restart once its
work is done and the client is told so. The next connection attaches to the
same host, its agents and terminals (the host's lifetime rules apply: by
default it exits a minute after the last window left with nothing running).

**When SSH fails**, its stderr says why and the window says what to do:

| SSH says | The window shows |
| --- | --- |
| `Permission denied` | Sign in once: the exact `ssh` command to run in a terminal (an **Open in terminal** button runs it in the system terminal), and `ssh-add` for a key with a passphrase |
| `Host key verification failed`, `REMOTE HOST IDENTIFICATION HAS CHANGED` | Check and accept the key by connecting once by hand (or `ssh-keygen -R` when it changed and that was expected) |
| `Could not resolve hostname`, `Connection refused`, `timed out` | What is wrong; Orbit keeps retrying (backoff up to 30 s) |
| `ssh` not found | That OpenSSH is needed |

Orbit never types a secret or answers a prompt. A failure only the user can
fix is not retried until they press **Try again** (or after five minutes).

**Editors.** Paths on an SSH host open in VS Code, Cursor or Windsurf with
`--remote ssh-remote+<host>` (Remote - SSH), in Zed as `ssh://host/path`;
"Open in terminal" is `ssh -t host "cd <dir> && exec $SHELL -l"` in the
system terminal. Other editors can't reach the files and say so.

## WSL

A distro is a host reached with `wsl.exe -d <distro> -- <command>`: the same
probe, install (a Linux `orbitd`, copied with `cat >` through `wsl.exe`) and
`--stdio` relay. Its projects have Linux paths. **Add project** on any host
that is not this computer browses that host's folders through its engine
(`HostCommand::ListDir`: any absolute path, `~`, the home folder by default),
since the system file dialog only sees this computer. Files open in VS Code
with `--remote wsl+<distro>`.

## Installing `orbitd`

When a host has no `orbitd` (or one of another protocol), the window offers
to install it to `~/.orbit/bin/orbitd` there: `mkdir -p`, `scp` to
`orbitd.new` (WSL: `cat >`), `chmod 755`, `mv` into place, and `--probe` to
check it runs. Nothing else on the host changes; it runs as the user. The
binary comes from:

1. **This computer's own `orbitd`**, when both run the same platform.
2. **A build shipped beside Orbit**: `remote/<target triple>/orbitd` next
   to the executable (for packagers).
3. **The release manifest** the updater reads ([RELEASING.md](RELEASING.md)):
   the bundle for `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, … is
   downloaded, checked against its SHA-256 and minisign signature (trusted
   comment `orbit <version> <triple>`), and `orbitd` taken out of it. Only a
   build with a release key downloads anything.
4. **A file the user picks** ("bring your own binary"), with the exact
   `cargo build --release -p orbitd --target <triple>` to make one when
   nothing else is available, which is the case until releases publish
   Linux and macOS bundles.

## Share this computer

Settings › Hosts › **Share this computer** lets Orbit on the user's other
devices reach this computer's background host over the network. It is off
until turned on, and only for this computer (a remote host is not shared from
here).

* **Listening.** The host listens on the address picked: Tailscale's
  (100.64.0.0/10, found from the route to Tailscale's DNS address) when there
  is one, the local network's, or every network; port 7840. While sharing is
  on the host keeps running without a window, for the devices. It serves the
  same protocol as its loopback endpoint, to **paired devices only**: the host's
  own token does not work there.
* **Pairing.** "Show a pairing code" makes a code (`ABCD-EFGH`, 40 bits from
  an unambiguous alphabet) that works for ten minutes, for one device, and
  stops after five wrong tries. On the other device, Add host › Direct with
  the address and the code. The device sends the code and a random nonce; the
  host answers with its nonce; both derive the same six digits from the code
  and both nonces and show them. The person at this computer allows the
  device if the digits match; only then does it get its own token (128 random
  bits). The host keeps only the token's SHA-256 (`<data>/share.json`,
  owner-only).
* **Devices** are listed with when they were last seen, and removed one by
  one: removing closes their connections at once. A paired device can do
  everything the window can, except manage sharing.

**Threat model.** The listener is plain WebSocket, not TLS or Noise. On a
network that encrypts by itself (Tailscale's WireGuard, a VPN) that is fine;
on a local network anyone on the path can read the traffic, including a
device's token, and reuse the token until it is removed. The page says so
when the address is not Tailscale's, and recommends Tailscale or SSH. What
pairing does protect against: someone who saw the code can't pair silently
(the user allows each device, after comparing the digits on both screens),
codes expire and are single-use, guessing is capped, tokens at rest are
digests, and devices are revocable. What it does not: an attacker who can
already read and alter traffic on the network during pairing. SSH hosts don't
have this problem; prefer them.

## Status and reconnecting

The switcher shows the shown host's state at the sidebar's foot: connected
(with the round trip, measured every 15 s with `HostCommand::Ping`, every
60 s for background hosts), connecting, reconnecting with the reason, or what
is needed (orbitd to install, a sign-in, a host key). A host not reached yet
shows a panel in place of its (empty) views, with the fix and **Try again**
(which skips the backoff). Connections retry with backoff: up to 5 s for
sockets, 30 s for SSH (each attempt is a new `ssh` process).

## Protocol

Protocol 13 adds `HostCommand::Ping` (answered at once), `ListDir` (a folder
anywhere on the host, `HostReply::Dir`) and `Share` (`HostReply::Share`).
None of them is logged. The JSON control request gained optional `code`,
`device` and `nonce` fields for `op: "pair"`; its fields are still only ever
added. Programs reached through stdio use `exec:` addresses in
`orbit_net::transport` (a JSON argument list); the client keeps the program's
stderr to say why a connection ended.

## Tests

`crates/orbitd/tests/remote.rs` has its own `main`: the test binary is also a
fake `ssh` and `scp` that treat a temporary folder as the remote home and run
the remote command here with a small POSIX-shell subset. With it, the real
client drives the real `orbitd`:

* the stdio transport round-trips through `orbitd --local --stdio` (about
  0.25 ms a call in a debug build, no network), lists folders, runs a turn,
  and the host it started outlives the connection and is attached again;
* a thread runs on a "remote" host reached through the fake `ssh`;
* a host without `orbitd` is probed (`uname`), refuses a file that is not
  `orbitd`, is installed to with the fake `scp`, and connects;
* SSH failures (no key, unknown host key, refused, no `ssh`) become their
  fixes, also in the connection status;
* a link waits for the install without probing, then connects;
* sharing: off by default, no pairing without a code, a wrong code refused,
  matching digits on both sides, a device token that works, a wrong one
  refused, a device that may not manage sharing, revoking that cuts the
  device off, and a listener that stops.

`orbit-net` tests `hosts.json` (round trip, owner-only, a damaged or odd file
made valid, this computer always first), `~/.ssh/config` fixtures (`Include`
with globs, loops, patterns, `Match`, first value wins, git services), WSL's
UTF-16 output, typed SSH targets (ports, IPv6, refused shell characters),
SSH's errors, shell quoting, the probe line and `uname`, the pairing codes,
digits and token digests. `orbit-app` tests two hosts in one window: switching,
the background session's records following, timelines closed on the host
left, the saved active host, and removing.

To look at it by hand without a remote machine, copy the test binary as
`ssh.exe` and `scp.exe`, set `ORBIT_FAKE_REMOTE_HOME` to a folder, and point
`ORBIT_SSH`/`ORBIT_SCP` at the copies (debug builds also take
`ORBIT_DEBUG_ROUTE=add-host`, `install-host` and `switch:<host id>`).

## Not done yet

* Windows computers as SSH hosts: the remote commands are POSIX shell.
* No published `orbitd` builds for other platforms yet, so installing needs
  this computer's own binary (same platform) or a file the user provides.
* Ports opened on a remote host (dev servers) are not forwarded; the
  preview browser sees this computer's localhost.
* Appearance settings live in each engine's settings, so a host shows its own
  theme.
* No QR code or `orbit pair` command for pairing; no TLS on the shared
  listener (see the threat model).
