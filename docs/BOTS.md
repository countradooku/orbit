# Bots: drive Orbit from Telegram, Slack and Discord

Orbit's chat bots let you start threads, follow them, answer approvals and
questions, and stop agents from a phone. They run inside the process that
runs the engine (the desktop app, or `orbitd` on a server) and make only
outbound connections:

| Platform | How Orbit connects | Port opened |
|---|---|---|
| Telegram | Bot API long polling (`getUpdates`, 25 s hold) | None |
| Slack | Socket Mode websocket from `apps.connections.open` | None |
| Discord | Gateway websocket (heartbeat, resume) plus REST | None |

No Orbit cloud is involved. Tokens are kept by the engine in
`<data>/integrations.bin`, which only your user can read. They never go into the
settings record or the event log, never reach a client (the settings page
only sees "…x9Q2"), and `Debug` output redacts them.

## Setting up

Open **Settings › Bots**. Each platform has a token field, an **Enabled**
switch, a status dot with the last error, and the steps below.

### Telegram

1. In Telegram, open **@BotFather** and send `/newbot`. Pick a name and a
   username ending in `bot`.
2. Paste the token BotFather replies with into **Bot token** and press Save.
3. Turn **Enabled** on. The status reads "Connected as @your_bot".
4. Message the bot. It answers with a six-digit pairing code. Approve the
   code under **People**, or run `orbit bot pair <code>`.

### Slack

1. Open <https://api.slack.com/apps>, choose **Create New App › From a
   manifest** and pick your workspace.
2. Paste the manifest from **Copy manifest**. It turns on Socket Mode and
   interactivity, adds the `/orbit` command, and asks only for
   `app_mentions:read`, `chat:write`, `commands`, `im:history`, `im:read`,
   `im:write`, `files:write` and `users:read`.
3. Install the app to the workspace. Copy the **Bot User OAuth Token**
   (`xoxb-…`) into **Bot token**.
4. Under **Basic Information › App-Level Tokens**, generate a token with the
   `connections:write` scope. Save it as **App-level token** (`xapp-…`).
5. Turn **Enabled** on, then send the bot a direct message and approve the
   code. In channels, invite the bot and `@mention` it. It answers in a
   thread under your message, and that Slack thread is one Orbit thread.

### Discord

1. In the [Developer Portal](https://discord.com/developers/applications),
   create an application. Under **Bot**, reset and copy the token.
2. On the same page, turn on **Message Content Intent** under Privileged
   Gateway Intents. Without it Discord closes the gateway with 4014; Orbit
   stops and says so.
3. Under **OAuth2 › URL Generator**, pick the scopes `bot` and
   `applications.commands`. Pick the permissions Send Messages, Read
   Message History and Attach Files. Open the URL to invite the bot.
4. Paste the token into **Bot token** and turn **Enabled** on.
5. Send the bot a direct message and approve the code. In servers,
   `@mention` it.

## Using it

Each chat drives one Orbit thread: a Telegram chat, a Discord channel or DM,
a Slack DM, or a Slack thread in a channel. The first message starts a
thread. It goes in the default project from **Chats › Project for new
chats**, or the only project. With several projects and no default, the bot
asks with buttons. With no projects, the thread uses a scratch folder.
Threads started from a chat never get Full access; they use the access level
chosen under **Chats**.

| Command | What it does |
|---|---|
| any text | Sent to the chat's thread, queued behind a running turn. With **Steer running turns** on, it steers instead. |
| `/new [project] [message]` | Start a new thread here (the first word picks a project by name). |
| `/projects` | List projects, with buttons to start a thread in one. |
| `/threads` | Recent threads and their status. Tap one to drive it from this chat. |
| `/switch <title>` | Drive another thread from this chat. |
| `/status` | Fleet summary ("2 working · 1 need you") and this chat's thread. |
| `/stop`, `/stop force`, `/stop all` | Kill switch: stop this thread, force-stop it, or stop every running thread. |
| `/diff` | What changed in the thread: a short stat, with the patch attached. |
| `/clear` | Fresh agent session in the same thread. |
| `/help` | The list above. |

Slack treats `/…` as its own commands. There, use `!status`, `!new …` and so
on, or the `/orbit status` slash command. `!` works on every platform.

**While the agent works**, the bot posts one status line and edits it as
the work goes: "Working · editing src/main.rs · 2m". The line uses the same
honest status as the app (see [STATUS.md](STATUS.md)). A silent agent reads
"Working · No output for 4m · cargo test". After sleep the line says
"Unverified" rather than guessing. When the turn ends, the line becomes
"Done · 3m" (or "Stopped" or "Failed"). The answer then arrives as a new
message, formatted for the platform:

- Telegram gets its HTML subset.
- Slack gets `mrkdwn` blocks.
- Discord gets markdown, in an embed when long.

Long answers are split at line breaks to each platform's limit (Telegram
4096, Slack 3000 per block, Discord 4000). Code blocks over 1200 characters
or 30 lines are sent as attached files. A summary of the changed files
follows the answer.

**Approvals** arrive with **Yes**, **No** and **Always** buttons. **Always** is
Orbit's "allow for this session". **Questions** list their options with the
agent's descriptions and show one button each. You can also reply in text:
`yes`, `no` or `always` answers a lone approval. An option number (`2`, or
`1, 3` for a multi-select) or your own words answers a question. When the
request is answered anywhere, the message loses its buttons and says how
it was answered.

**Notifications.** Requests on threads that no chat drives, and failures,
are pushed to each paired person's own chat. By default this happens when no
Orbit window is in front. You can choose Always or Off instead. Chats always
hear about their own threads.

Everything a chat does appears in the thread's timeline with its origin:
"Sent via Telegram by Ana", "Approved via Slack by Dan", "Stopped via
Discord by Ana".

## Security

- **Pairing.** Only people you let in can use the bots. Anyone else who
  messages one gets a six-digit code and nothing more. In channels, they are
  only told to send a direct message. A code expires after 15 minutes. At
  most 20 people can wait at once. Approve or reject codes in **Settings ›
  Bots › People**, or with `orbit bot pair <code> [--reject]`. `orbit bot allow
  <platform> <user-id>` lets someone in by id, and `orbit bot unpair` or
  **Revoke** removes them at once. Pairing commands refuse to run inside an
  agent's thread (`ORBIT_THREAD` set), so a prompt can't ask an agent to let
  someone in. This is a speed bump, not a wall: an agent with a shell can
  reach the local endpoint like any program of yours.
- **The guard.** Some actions always need a person at the Orbit app, even
  at Full access:
  - force pushes, `git reset --hard`;
  - recursive deletes, `sudo`;
  - publishing and deploying (`npm publish`, `cargo publish`, `kubectl
    apply`, `terraform apply` and the like);
  - anything touching secrets (`.env`, SSH keys, `*.pem`, credentials files).

  The list is editable under **Settings › Bots › Guard**. It is a general
  engine policy, kept in `<data>/guard.bin` and handed to every agent
  session:
  - Claude Code: a `PreToolUse` hook holds matching calls for a person in
    every permission mode, `bypassPermissions` included.
  - Codex: at Full access, every command that isn't known to be read-only
    is raised (`untrusted`), still without a sandbox, and Orbit approves all
    but the guarded ones itself.
  - ACP agents: Orbit's own auto-approval skips guarded requests.
  - OpenCode: an `ask` rule is added per pattern.

  A guarded request can't be approved from a chat. The bot shows only **No**,
  and the engine refuses a remote approval. In the app, "allow for this
  session" on a guarded request approves it once.
- **Rate limits.** Each person can send a burst of 8 messages, then one
  every 3 seconds (20 a minute). After that the bot says "Slow down" once and
  drops the rest. Outgoing messages are paced per chat: 1 a second on
  Telegram and Slack, about 3 a second on Discord. When a platform answers
  429, Orbit waits as long as the platform says.
- **Reconnects.**
  - All platforms back off from 1 s, doubling to 60 s, with jitter.
  - Slack's routine socket rotation (`disconnect`) reconnects at once.
  - Discord resumes a dropped session from its last sequence number. It
    reconnects when a heartbeat goes unacknowledged.
  - A token the platform refuses stops the bot ("Token rejected") until the
    token changes.

## Limits

- The bots run where the engine runs. A desktop app connected to a remote
  `orbitd` shows that host's bots.
- Codex at Auto-edit runs commands inside its workspace sandbox without
  asking anyone. The guard sees only what leaves the sandbox. Use Ask first,
  or Full (which routes every command through Orbit), for full coverage.
  ACP agents in a bypass mode, and Pi, which has no permission prompts, run
  commands without asking Orbit, so the guard can't hold them.
- Guard edits apply to agent sessions started afterwards.
- Pairing codes live in memory; restarting the host clears codes that were
  not yet approved.

## For developers

The code is in `crates/orbit-bots`. The hub (`hub.rs`) is an ordinary
engine client. It follows events through `orbit_net::state::ClientState`
and acts with `Command::Via { via, action }`. Transports
(`telegram.rs`, `slack.rs`, `discord.rs`) feed it messages and send
through paced outboxes (`transport.rs`). Rendering and chunking are in
`render.rs`.

The engine side is in `crates/orbit-core/src/engine`:

- `bots.rs`: `Command::Bots` forwarding, tokens, `Via`.
- `guard.rs`: the guard.
- `integrations.rs`: the integrations file, now version 2, which still reads
  version 1.

Tests run against loopback fakes: an HTTP server for the REST APIs and a
sockudo-ws server for Socket Mode and the gateway (`tests/support`). They
never contact a real platform.

`ORBIT_DEBUG_ROUTE=settings ORBIT_DEBUG_SETTINGS_PAGE=6` opens a debug
build on this page.
