# T3 Code (pingdotgg/t3code): research report for a Rust/native rewrite

- **Source:** the shallow clone at `~/nsh-research/t3code`, HEAD `a49ccc1` (2026-11-24). Paths are repo-relative, and `A:` means the main adapter file of the section you are in.
- **Ignore `.repos/`:** it is vendored third-party code (effect, alchemy).
- **Issue data:** fetched with `gh` and the GitHub search API on 2026-11-25. The repo has 1,325 open and 9,232 closed issues.
- **How this was written:** sections 1-4 were compiled from five parallel source reads, each of which cited file:line. I spot-checked several claims myself: plan mode, node-pty, the terminal renderer, and the default keybindings. Section 5 is mine.

> **Version warning:** this codebase is far beyond the early-2026 T3 Code that many people remember.
> - **Server and protocol:** Orchestration **V2** (event-sourced), Effect RPC over one WebSocket, and SQLite through `node:sqlite`.
> - **Providers and source control:** 11 provider drivers and 8 source-control hosts.
> - **Other features:** a mobile app, a T3 Connect relay, a preview browser, workflows, scheduled tasks, an MCP server that exposes about 95 tools to agents, and more.
>
> "Every functionality" now covers a very large surface. Section 1 marks what is core and what is peripheral.

## Contents
1. Feature inventory (web, server, desktop; mobile in brief)
2. Server ↔ client protocol, domain model, persistence, client state reconstruction
3. Provider integration: 3a Codex, 3b Claude Code, 3c shared adapter abstraction, ACP, Cursor, OpenCode, Pi, Grok, Kiro, Muse, fx, Antigravity, T3 MCP injection
4. Performance-relevant design and inefficiencies
5. Known bugs (GitHub issues grouped into themes, plus bugs found in the code) and how to avoid them in the rewrite

## Key facts at a glance

**Process model**
- The server (`apps/server`, Node + Effect) owns provider processes, PTYs, git, checkpoints and the SQLite DB.
- Clients are web (React), desktop (Electron, which bundles the server) and mobile (Expo). All of them talk to the server over the same authenticated RPC.

**Wire**
- **Transport:** `GET /ws?wsTicket=…&orchestrationProtocol=2` carrying Effect RPC with JSON serialization. Envelopes are `_tag` = Request / Chunk / Exit / Ack / Interrupt / Ping / Pong.
- **Core RPCs:** `orchestration.dispatchCommand` (returns `{sequence}`), plus the streams `subscribeShell` (sidebar rows), `subscribeThread` (one thread) and `subscribeCommandOutput` (live command output, kept out of the event log).
- **Cold reads:** snapshots and history pages also have REST routes under `/api/orchestration/...`.

**State**
- **Commands, events, projections:** commands are serialized per thread and decided without I/O. The resulting events are **full-entity snapshots**.
- **One transaction:** events, projections, the command receipt and outbox effects commit together in `statev2.sqlite` (WAL).
- **Side effects:** an effect worker runs them after commit (provider turns, checkpoints, PR sync).
- **Retention:** the event log keeps only a bounded window, so projections are the source of truth.

**Client reconstruction**
- Load a bounded thread snapshot (10 user turns / 75 rows / 1 MiB), then replay events from `afterSequence` (at most 128 events / 1 MiB, otherwise a fresh snapshot) until a `synchronized` marker.
- Older history comes through cursor pages over HTTP.
- The shell (sidebar) uses the same pattern with deltas.

**Checkpoints**
- Stored as hidden git commits at `refs/t3/orchestration-v2/checkpoints/<scopeKey>/ordinal/<n>`.
- Built with a temp index and a private object dir (`add -A`, `write-tree`, `commit-tree`, `update-ref`).
- Diffs are `git diff` between refs.

**Codex**
- **Process:** one `codex app-server -c thread_unload_delay_secs=0` per provider instance, shared by all its threads.
- **Transport:** NDJSON JSON-RPC over stdio with no `jsonrpc` field; `initialize` (with `experimentalApi:true`) followed by `initialized`.
- **Methods:** `thread/start|resume|fork|revert|compact/start|inject_items`, and `turn/start|steer|interrupt`.
- **Approvals:** server→client requests `item/commandExecution/requestApproval` and `item/fileChange/requestApproval`, answered with `accept|acceptForSession|decline|cancel`.

**Claude**
- **Process:** `@anthropic-ai/claude-agent-sdk` `query()` with a streaming-input prompt queue: one `claude` CLI process per thread, speaking stream-json NDJSON.
- **Approvals:** the `can_use_tool` control request (SDK `canUseTool`).
- **Resume:** `--resume <session> --resume-session-at <assistant uuid>`.
- **Steering:** a user message with `priority:"next"`.
- **Live changes:** `set_model`, `set_permission_mode` and `apply_flag_settings`; anything else restarts the CLI with resume.

**Other providers**
- **ACP** (newline-delimited JSON-RPC over stdio) is the generic path. Grok, Kiro, fx, Antigravity and all ACP Registry agents are small "flavors" of one ACP adapter.
- **OpenCode** is HTTP plus SSE to `opencode serve`.
- **Pi** is `pi --mode rpc` JSONL plus an injected TS extension.
- **Cursor and Muse** are Node SDKs running in a worker or sidecar.

**Top bug themes**
1. Long-thread and large-DB slowness: event-log write amplification, unbounded DB growth, SQLite blocking the event loop.
2. Turn-state desyncs: empty threads, stuck "running", Stop that does not stop.
3. Provider lifecycle problems: idle release and lost context, the shared Codex app-server dying, Claude token usage, steering cancelling tools.
4. Windows process and console issues, plus corrupt credential stores.
5. Checkpoint capture on large repos: 30 s timeouts, no pruning, disk filling up.
6. Terminal input and escape-sequence issues.
7. Worktree cleanup and branch drift.
8. Reconnect flapping.

---

# 1. Feature inventory

## T3 Code — user-facing feature inventory (web + server + desktop; mobile brief)

Source: shallow clone `~/nsh-research/t3code` (read-only). Paths are repo-relative. Compiled from four parallel source reads (A: projects/threads/sidebar, B: chat/composer/providers/modes, C: diff/checkpoints/terminal/git/PR/preview/workflows/schedules/editors, D: keybindings/palette/settings/themes/notifications/usage/remote/telemetry/updates/desktop/mobile). Each part ends with its own "Bugs / fragile UX" list.

Headline facts for a Rust reimplementation:
- Server: Node + Effect, WebSocket RPC (`t3` protocol), SQLite state (`statev2.sqlite`), orchestration-v2 event model (`packages/contracts/src/orchestrationV2.ts`).
- Providers (11 drivers): Codex = `codex app-server` JSON-RPC; Claude = Claude Agent SDK; Cursor/Muse = own SDKs; Grok/fx/Kiro/Antigravity/ACP-registry = ACP; OpenCode = HTTP server; Pi = `pi --mode rpc` JSONL.
- Terminal: server `node-pty` (apps/server/package.json:70); web renderer is libghostty-vt compiled to WASM drawing to Canvas 2D (not xterm.js).
- Checkpoints: hidden git refs `refs/t3/orchestration-v2/checkpoints/<hash>/ordinal/N` built via temp index + write-tree/commit-tree.
- Diffs: `@pierre/diffs` in a 2-6 worker pool. Markdown: `@pierre/words` + KaTeX + Mermaid. Composer: Tiptap.
- Keybindings: defaults in `packages/shared/src/keybindings.ts:58-151`, user overrides `<baseDir>/userdata/keybindings.json`, VS Code-style `when` clauses.
- Verified notable gap: web composer always sends `interactionMode: DEFAULT_INTERACTION_MODE` (`useComposerSend.ts` ~L1491), so plan mode is not user-toggleable in web except via provider `agent=plan` options / scheduled tasks / MCP.

---


<!-- ===== Part A ===== -->

## Section A — Projects, Threads, Sidebar

Paths are relative to the repo root. "Env" means an environment: one machine running a T3 server. The client can connect to several at once. Thread and project keys are scoped per env.

### Projects: model & persistence

- **Project entity**: `{id, title, workspaceRoot, repositoryIdentity, faviconPath, projectIcon, defaultModelSelection, defaultThreadEnvMode (local|worktree), autoPull, scripts[], trackedFolders[≤128], pinnedAt, archivedAt, deletedAt}`. It is stored on the server. Updates go through the `ProjectUpdatePayload` patch (title, icon, favicon, env mode, scripts, trackedFolders replace the whole list, pinned, archived). `packages/contracts/src/project.ts`, `apps/server/src/project/ProjectService.ts`.
- **Logical project grouping**: checkouts of the same repo, on one machine or several, merge into one logical project in the UI. Modes: `repository` (default: group by remote identity), `repository_path` (identity plus sub-path inside the repo), `separate` (one per physical path). A global client setting `sidebarProjectGroupingMode` can be overridden per checkout (`sidebarProjectGroupingOverrides`) from Settings → Projects → Checkouts ("Use global default" / "Group by repository" / "Group by repository path" / "Keep separate"). `packages/client-runtime/src/state/projectGrouping.ts`, `apps/web/src/sidebarProjectGrouping.ts`, `components/settings/ProjectSettingsPanel.tsx:90-145`.
- **t3.json (checked-in config)**: `iconPath`, `defaultThreadEnvMode`, `worktreeSubmodules` (recursive|top-level|none), `ignoreRepositories[]`, `worktreeLayout{create,remove}` (each up to 4096 chars), `worktreeSparsePatternsPath`, `scripts[≤50]`. Resolution order is project override → env setting → t3.json → built-in default. `packages/contracts/src/t3ProjectFile.ts`, `apps/server/src/project/T3ProjectFileLoader.ts`.
- **Folder sync**: the server rescans tracked folders about every 30 s (`FOLDER_SYNC_INTERVAL_MS`). `ProjectService.ts:193`.

### Projects: adding

- **Add project (command palette)**: three entries.
  - "Local folder": browse disk with a path-autocomplete query. Relative paths are resolved against the active project.
  - "New project": "Start a new Git repository from a name". The server creates `<baseDir>/projects/<name>` and runs `git init --initial-branch`. It can optionally create a private repo on a connected source-control host and clone it ("Create & Clone").
  - "{source} repository / Clone from a remote URL": Git URL, GitHub-style `owner/name` shorthand, or a provider picker. A confirm step lets you choose the destination folder.
  - Also available: "Open WSL folder", and unarchiving an archived project from the palette.
  - Files: `components/CommandPalette.tsx` (~1100-2650), `CommandPaletteAddProject.tsx`, `packages/client-runtime/src/operations/projects.ts`, `apps/server/src/project/ManagedProjectFolders.ts:393-510`.
- **Clone progress toasts**: a toast shows "Cloning {name}" with Cancel. On success: "Cloned {name}" with "Open project". On failure: Retry or "Remove project". The server tracks clone state, keeps finished entries for 30 s, and flags a stall after 20 s with no git output. Commands on the project are rejected while it clones. `components/ProjectCloneToastCoordinator.tsx`, `apps/server/src/project/ProjectCloneTracker.ts:71-75,489`.
- **Add to another machine**: dialog with source machine → target machine select, SSH/HTTPS protocol toggle, an editable destination folder, and an optional "Copy ignored files" multi-select with search. Limits: at most 200 files, each at most 1 MiB. Folders git ignores whole, like `node_modules`, are skipped. Files are copied after the clone finishes. `components/AddProjectToMachineDialog.tsx`, `apps/web/src/lib/addProjectToMachine.ts:25-28`.
- **Drop folders on the sidebar (desktop only)**: a dropped folder becomes a project. Dropped files are rejected with "Drop files on a thread to attach them." `components/sidebar/SidebarFolderDrop.tsx`.
- **Welcome-page import**: on first run, the app scans Claude Code, Codex, and Pi history for project directories. Pre-checked candidates are git repos active in the last 30 days with at least 3 conversations. "Review all" offers Select all / Select none. Clones with the same remote collapse into one group. Import keeps up to 200 messages per conversation (the first user prompt plus the newest messages) and reads up to 100 conversations / 4 GiB per project per attempt. Tool output and attachments are dropped. The page can be reopened from Settings → About. `components/onboarding/WelcomePage.tsx`, `FirstRunGate.tsx`, `apps/server/src/project/AgentSessionScanner.ts:85-92`, `AgentSessionImporter.ts`, `docs/user/welcome-page.md`.
- **Claude history worker**: a subprocess reads Claude SDK session messages and does native `forkSession` for imported or forked Claude threads. `apps/server/src/claudeHistoryWorker.ts`.

### Projects: settings, icons, pin/archive, removal

- **Settings → Projects panel**: Name (with reset to default), Icon, Pin, Archive/Unarchive, Checkouts list (per env and path, each with a grouping rule and "Remove checkout"), Repositories, Actions, Worktree layout, and a Danger zone. Remove checkout, or "Remove this project everywhere", deletes the project entries and their threads but never touches files. The server refuses to delete a project that still has threads unless `force` is set. `components/settings/ProjectSettingsPanel.tsx`, `ProjectsSettings.tsx`, `ProjectService.ts:777`. The route `/projects/$projectKey` redirects to `/settings/projects?project=`.
- **Settings scope sentence**: an "Applying settings for {project} on {env}" picker at the top of Settings pages. It supports bulk edit across all envs, project overrides with a layers icon showing the inheritance chain, a "Mixed" state when envs disagree, and Reset / Reset all. `components/settings/SettingsScopeSentence.tsx`, `settingsScope.ts`, `SettingInheritance.tsx`, `docs/user/project-settings.md`.
- **Icons**:
  - Override kinds: `lucide{name,color}`, `emoji`, `monogram{text,color}` in an 18-colour palette, plus a custom image file (`faviconPath`, must be .avif/gif/icns/ico/jpg/png/svg/webp). "Automatic" re-runs detection.
  - Auto-detection checks t3.json `iconPath`, then a candidate list (`favicon.svg/.ico/.png`, `public/…`, `app/icon.svg`, `src/app/…`, `assets/logo.svg`, `.idea/icon.svg`), then `<link rel=icon>` parsed from `index.html` / `__root.tsx` / `root.tsx`.
  - Fallback is a 2-grapheme monogram derived from the name, e.g. "Silver Orchard" → "SO", with a palette colour.
  - Files: `apps/server/src/project/ProjectFaviconResolver.ts:41-80`, `components/ProjectFavicon.tsx`, `ProjectMonogram.tsx`, `settings/ProjectIconPickerDialog.tsx`, `ProjectFaviconPickerDialog.tsx`.
- **Pin project**: pinned projects sort first in every project picker, the sidebar filter, and boring-sidebar folders. Toggle it with the row pin button, `mod+shift+p` on a highlighted row, or the palette entry "Pin project". An open list keeps its order until reopened. Pinning or archiving a project that lives on several machines requires all of them to be connected. `components/ProjectPinButton.tsx`.
- **Archive project**: hides the project from pickers. Threads, settings, and files stay. Settings shows it marked "Archived".
- **Multi-repo projects**:
  - A project covers every git repo inside its folder: one level down, and two levels when the first level is a plain folder. Worktrees of other repos are excluded.
  - "Add folder" tracks a repo anywhere on the machine, up to 128 folders. Folders listed in a `.code-workspace` file are offered with an "Add" button. "Remove" untracks a folder and never deletes files.
  - Repositories are listed as Git repository / Folder / Missing.
  - Files: `components/settings/ProjectRepositoriesSettings.tsx`, `trackedFolders.logic.ts`, `docs/user/multi-repo-projects.md`.
- **Auto-pull**: opt-in per project or env. It fast-forwards the default-branch checkout only when the checkout is clean and has no local commits.

### Project scripts ("Actions")

- **Script model**: `{id, name, command, icon (play|test|lint|configure|build|debug), runOnWorktreeCreate, async (wait-for-finish toggle), runOnSettle, stopOnSettle, previewUrl, autoOpenPreview}`. Scripts can be imported from t3.json via an "Import" button. Env-level `defaultProjectScripts` act as a shared list: editing a project's actions creates its own list, and reset returns to the shared one. `packages/contracts/src/project.ts:26-48`, `components/settings/ProjectActionsSettings.tsx`, `ProjectActionsList.tsx`, `useProjectScriptSettings.ts`.
- **Top-bar ProjectScriptsControl**: a primary "Run {name}" button with a dropdown of all actions, "Add project action…", and an edit/delete pencil per action. Read-only connections get "This connection cannot change project actions." `components/ProjectScriptsControl.tsx`.
- **Script editor dialog**: fields are Name, Icon picker, Command (with a "Test" run), Keybinding (records a shortcut, bound as the `script.<id>.run` command), and Preview URL. Toggles: "Open preview automatically", "Run automatically on worktree creation", "Wait for it to finish before the agent starts", "Run in the thread's worktree when the thread settles", "Stop this action when the thread settles". Validation messages: "Name is required." / "Command is required." `components/projectScriptEditor.tsx`.
- **Setup-script runner**: runs `runOnWorktreeCreate` scripts in a T3 terminal using bracketed paste and a completion sentinel `__T3_SETUP_DONE__`. Streamed output lines are capped at 400 chars. Unsupported shells return an error. Scripts do not run for a worktree an agent requested below Full access. `apps/server/src/project/ProjectSetupScriptRunner.ts:138-183`, `WorktreeSetupTracker.ts`.

### Threads: creation & drafts

- **New thread**:
  - `mod+n` / `mod+shift+o` (`chat.new`) opens a project picker when more than one non-archived project is open. Shift-click, or `mod+shift+n` (`chat.newLocal`), starts in the current project. A project-filtered sidebar starts in that project.
  - `mod+alt+n` (`chat.newWithoutProject`) starts a scratch thread.
  - The new thread inherits the model and mode unless the project has a default model.
  - Files: `components/Sidebar.logic.ts:816-852`, `packages/shared/src/keybindings.ts:113-116`.
- **Drafts**: a draft lives at `/_chat/draft/$draftId`, and ChatView stays mounted while the draft is promoted to a real thread. Drafts persist in localStorage under `t3code:composer-drafts:v1` and appear in the sidebar as "Unsent draft" rows. "Discard draft" can be undone, which restores text and attachments. The draft menu offers Copy path/branch, Project settings, and Discard. The index route `/` auto-opens a draft in the most recent project. `apps/web/src/composerDraftStore.ts:119`, `components/sidebar/SidebarDraftBlock.tsx`, `threadActionMenu.logic.ts:35-93`, `routes/_chat.index.tsx`.
- **Background send / fan-out**: `mod+enter` on a draft (`composer.sendBackground`) starts the thread and opens a fresh draft that keeps the workspace mode and base branch. Shift-clicking several models in the picker makes one send start one thread plus one worktree per model (Git projects only).
- **Server thread.create**: carries `{projectId, title, modelSelection, runtimeMode, interactionMode, branch, worktreePath, importedNativeThread?, initialMessage{titleSeed, workspaceStrategy}}`. Workspace strategies are `root`, `existing_worktree`, `worktree`. The thread and its first run commit atomically. `packages/contracts/src/orchestrationV2.ts:3312-3357, 784-806`.
- **Scratch ("No project") threads**: each one gets its own folder `~/.t3/scratch/<date>-<first-5-words>-<id>`. Branch, diff, and worktree controls are hidden. The folder is kept when the thread is deleted. "Turn into project" (`thread.project.adopt-folder`) promotes the folder in place and moves every other thread in that folder with it. The option is disabled while the thread is running. `ManagedProjectFolders.ts:135-160`, `ScratchThreadPromotion.ts`, `threadActionMenu.logic.ts:251-261`.
- **Prompt links**:
  - `/new?prompt=&repo=&provider=&model=&pr=` opens a draft and never auto-sends. Prompts are limited to 8,000 chars, and invisible or tag characters are stripped with a warning.
  - `repo` resolves to the most recently used project for that repo, or offers Clone / Use without project. `pr` opens the thread linked to that PR, or a draft in a new worktree after confirmation.
  - A thread link with `?prompt=` fills that thread's composer and asks before replacing an existing draft.
  - "Copy prompt link [with repo and model]" in the palette and a per-message button generate links.
  - On the hosted app, a "where to open" chooser appears with a "Remember my choice" option.
  - Files: `components/promptLinks/*`, `packages/shared/src/promptLinks.ts`, `routes/_chat.new.tsx`, `docs/user/prompt-links.md`.
- **Thread share link**: `/threads/$env/$thread` redirects to `/$env/$thread`. "Copy → Thread" copies the link, or the PR URL via `thread.copyReference` (`mod+shift+c`). Pasted into a composer, the link becomes a thread chip.

### Threads: titles, fork, side/linked threads

- **Titles**:
  - The seed title is the normalized first prompt with mentions and file links flattened and truncated. Fallbacks are "Attached image(s)", "File: {name}", then "New thread". The server then generates a title with the env's text-generation model.
  - "Regenerate title" (the `thread.regenerateTitle` command has no default key; also bulk "Regenerate titles (n)") shows "Regenerating…" and is disabled while generation is in progress.
  - Inline rename: double-click the title or choose "Rename thread". Empty titles are rejected.
  - Per-env or per-project "Thread title instructions" are available under Settings → Threads → Text generation.
  - Title links resolve issue/PR refs.
  - Files: `packages/client-runtime/src/operations/threadTitle.ts`, `apps/server/src/textGeneration/ThreadTitleLinks.ts`, `components/threadTitleRegenerationToast.ts`, `thread.metadata.update{title,regenerateTitle}`.
- **Fork**: "Fork from this response" on an assistant message creates `thread.fork` with a sourcePoint run, titled "{title} fork". Strategies are `native_fork` (when the provider supports it) or `fork_delta_context` (portable handoff). `thread.merge_back` merges a fork back into its parent and requires a completed run in the fork. Capability flags: `canForkThread`, `canForkFromTurn`, `canForkWhileSourceRunning`. `components/chat/TimelineAssistantRows.tsx:126-170`, `useTimelineRewind.ts:188-210`, `ThreadRelationshipsControl.tsx`, `orchestrationV2.ts:226-250,3759-3776`.
- **Side chats**: `/side` or `/btw` (or the right panel's + → Side chat) creates `thread.fork{sideChat:true}`. Side chats are hidden from the sidebar and listed under thread details "Side chats". They open as a right-panel tab with their own composer. "Add to chat", or `@`-mentioning one in the parent, links it into the parent. Closing an unused side chat deletes it. Archiving or deleting the parent cascades to its side chats. `components/chat/useSideChat.ts`, `docs/user/thread-sidebar.md#side-chats`.
- **Opening threads beside the current one**: subagents, side chats, and linked-machine threads open as a right-panel tab by default (`sideThreadOpenTarget` = panel|main). Cmd/Ctrl-click or middle-click opens them in the main view instead. Linked-machine threads are read-only ("Connect to interact"). `routes/_chat.linked.$viaEnvironmentId.$linkedEnvironmentId.$linkedThreadId.tsx`.
- **Portable handoff / provider switch**: a budgeted selection of history (default 16k tokens, `T3CODE_CONTEXT_HANDOFF_TOKEN_CAP` clamped to 1,024–64,000) goes to the new provider session. `docs/user/portable-handoffs.md`.
- **V1→V2 migration**: copies `state.sqlite` to `statev2.sqlite` and migrates it once. Title, project, model, modes, branch/worktree, archive/settle/snooze/pin state, linked PR, and messages are kept. A loading toast "Restoring your threads… Migrating N threads" stays up while the migration runs. `components/LegacyThreadMigrationToast.ts`, `docs/user/thread-migration.md`.

### Threads: lifecycle states (pin / settle / snooze / archive / delete / unread)

- **Pin**: `thread.pin` / `thread.unpin` (`mod+shift+p`). Pinned order is a server-stored fractional `orderKey` (`thread.pin.reorder`). Settling a thread removes its pin. Pinning keeps the sidebar's scroll position.
- **Settle** (soft done, different from archive):
  - Triggers: `thread.settle` (`mod+shift+s`) or auto-settle. Auto-settle happens after N inactive days (default 7, range 1–90), on PR merge, or optionally on PR close. Work in progress, pending questions, and background waits block it.
  - Per-thread "Auto-settle behavior": Follow settings / Never (`thread.auto-settle.set`).
  - Settled threads stay in search. Any new message un-settles them.
  - Settling closes idle terminals and runs `runOnSettle` scripts.
  - Offline settles queue as "Pending / Waiting to sync".
  - Older settled threads are paged from the server, 50 per page ("Could not load older threads. Retry").
  - Files: `orchestrationV2.ts:3387-3427,4268-4320`, `contracts/settings.ts:89-98`.
- **Snooze**:
  - Menu presets come from env settings. Defaults: 1 h, 3 h, today 18:00, tomorrow 09:00, Monday 09:00. Presets are editable under Settings → Threads → Snooze options. Past "today" options are hidden, and picking a stale one toasts "That snooze time has passed."
  - "Custom…" opens a dialog to pick a local date/time or enter a duration in minutes/hours/days.
  - A snoozed thread keeps running, hidden. It wakes early on an approval, a new question, a failure, or a user message, then shows a "Woke" badge that can be dismissed.
  - "Snooze until reset" is offered for usage-limited threads.
  - Files: `components/Sidebar.snooze.ts`, `CustomSnoozeDialog.tsx`, `settings/SnoozePresetsSettings.tsx`, `contracts/settings.ts:131-137`.
- **Archive**: `thread.archive` / `thread.unarchive`. Archived threads leave the sidebar and search, refuse new messages (including from agents and scheduled tasks), and are disabled while running. Confirmation is optional (`confirmThreadArchive`, default off). Settings → Archive lists archived threads across envs with Unarchive / Delete. `components/settings/ArchivedThreadsPanel.tsx`, `routes/_chat.settings.archived.tsx`.
- **Delete**: confirmation defaults to on (`confirmThreadDelete`): "This permanently clears conversation history". If the thread is the only one using its worktree, and the client has source-control write scope, and auto-cleanup is off, a second confirm asks "Delete the worktree too?". After deleting, the app navigates to the newest non-settled, non-snoozed thread in the same project. Deleting a thread also deletes its side chats and any scheduled tasks that post into it. `hooks/useThreadActions.ts:538-640`, `Sidebar.logic.ts:1336-1380`.
- **Undo**: unpin, settle, snooze, archive, and discard-draft show an Undo toast for 5 s ("Settled," / "Snoozed 3," …). `mod+z` (`thread.undo`) triggers the latest Undo even from the composer. `components/sidebar/SidebarThreadUndoNotice.tsx`.
- **Unread / visited**: `thread.visit{opened, searchQuery}` moves a server read watermark. "Mark unread" (`thread.mark-unread`) is only offered after a completed run while idle. The client also keeps a local `threadLastVisitedAtById`. "Done" means completed and unseen. `uiStateStore.ts`, `Sidebar.logic.ts:769-792`.
- **Tags (server-only)**: `thread.tag.set{tag,tagged}`. Tags are up to 64 chars, single line, case-sensitive. Env settings `hideTaggedThreads` + `hiddenThreadTags` hide matching threads and their children, and suppress their notifications. No web UI sets tags (only the server handles the command). `packages/contracts/src/threadTags.ts`, `client-runtime/src/state/threadTagFilter.ts`, `apps/server/src/orchestration-v2/Orchestrator.ts:5177`.
- **PR / issue links**: the PR for a thread's saved branch is linked automatically. Right-clicking a PR link offers "Link to thread" / "Unlink from thread". Linked issues show as a badge "{n} linked issues".

### Worktrees per thread

- **Workspace (env mode) selector**: the BranchToolbar options are "Local / current checkout", "New worktree", and "Previous worktree" (`composer.previousWorktree` `mod+shift+l`). Shortcuts: `mod+shift+x` (workspace), `mod+shift+g` (branch). With several models selected: "Each model starts in its own worktree." "New thread in this worktree" / "New thread on {branch}" reuse an existing worktree. `components/BranchToolbarEnvModeSelector.tsx`, `BranchToolbar.tsx`, `PreviousWorktreeItemContent.tsx`.
- **Base-branch picker**: a searchable ref list that loads refs lazily ("Loading more refs..."). `components/WorktreeBaseBranchPicker.tsx`.
- **Multi-repo worktree picker**: a checklist of repos with filter, Select all, and Select none. All repos are selected by default when there are 8 or fewer. The choice is remembered per project in localStorage (`t3code:worktree-repository-choices:v1`). A base branch field covers plain folders ("Each repository's default branch" fallback). `components/WorktreeRepositoriesControl.tsx`, `worktreeRepositoryChoiceStore.ts`.
- **Worktree layout approval**: the first worktree thread in a project whose t3.json declares `worktreeLayout` shows a modal "Let this project run its worktree commands?" with the full create and remove text, and "Approve and continue" / Cancel. Approvals are stored per env, per project, per exact command text. Settings shows Off / Approved / Needs approval with Revoke. Each command has 10 minutes, and only one runs at a time per project. `components/WorktreeLayoutApprovalDialogHost.tsx`, `settings/ProjectWorktreeLayoutSettings.tsx`, `apps/server/src/workspace/LayoutCommandApprovals.ts`.
- **Lifecycle**: branch names come from a static prefix (`t3/`), a semantic model-chosen prefix, or custom instructions. A missing worktree folder is recreated on the next turn, with setup scripts re-run. Cleanup policies: inactive days, after settle, after merge, unchanged+settled, or delete with thread. Settings let you set a worktree location, submodules mode, sparse patterns, and macOS APFS fast checkout (Labs). `apps/server/src/workspace/restoreMissingWorktree.ts`, `project/worktreeContainer.ts`, `apps/server/src/storageCleanup.ts`, `components/settings/WorktreesSettings.tsx`.

### Sidebar

- **Sections**: Pinned → Active → Working (beta, default on, collapsed; holds threads running or waiting on background work) → Snoozed shelf → Settled shelf. Shelf open/closed state is stored in localStorage. Empty-state copy: "No projects yet / Add project", "No active threads in {project}". `components/Sidebar.tsx:3685-3810`, `Sidebar.logic.ts:201-213`.
- **Ordering**: active and pinned threads use server-stored order keys (`thread.active.reorder` / `thread.pin.reorder`). New threads land above arranged ones. Activity does not reorder threads. With the Working section on, the active list is ordered by when each thread last came back to you, and drag-reorder is disabled. Project sort order (`updated_at` | `created_at` | `manual`) and thread sort order (`updated_at` | `created_at`) are client settings that pickers and the palette use. `client-runtime/src/state/thread-sort`, `settings/SettingsPanels.tsx:147,935`.
- **Drag and drop** (dnd-kit):
  - Reorder within the Pinned or Active section. Crossing sections applies Pin / Unpin / Settle / Un-settle / Wake, and a badge on the dragged row shows the verb.
  - Threads cannot be dropped into Snoozed. Dragging to the top edge pins a thread when nothing is pinned yet.
  - A shift guard drops clicks on rows that moved under the pointer.
  - Dragging from a row's Settle / Un-settle / Wake button sweeps the action over a range of rows. Escape cancels.
  - OS files dropped on a row attach to that thread's composer.
  - Files: `components/Sidebar.drag.ts`, `Sidebar.logic.ts:256-565`, `Sidebar.pointer.ts`, `Sidebar.shiftGuard.ts`, `Sidebar.motion.ts`.
- **Boring sidebar (Labs: group by project)**: active threads go into per-project folders. Pinned projects come first, and archived projects come last (shown only while they have active threads). A closed folder shows its thread count and an urgency dot (attention > failed > unread > working). The folder pen button or right-click starts a new thread in that project. Drag-reorder and the Working section are off in this mode. There is an optional multiline-rows setting. `components/sidebar/SidebarProjectFolderHeader.tsx`, `Sidebar.logic.ts:1032-1100`, `contracts/settings.ts` (`boringSidebarEnabled`, `boringSidebarMultilineRows`).
- **Project filter**: a multi-select "Projects: {list}" menu with search, "All projects", an "Add project" item, and a per-project settings gear. It is persisted in `uiStateStore.sidebarProjectScopeKeys`. A thread's menu offers "Filter by {project}" / "Show all projects". `Sidebar.tsx:1032,3140-3300`, `Sidebar.logic.ts:1186-1225`.
- **Search**: a sidebar search box searches titles, branches, PRs, and project names locally with fuzzy ranking. The server's message search adds an "In messages" group (`searchThreads`, `titlesOnly` option, query up to 200 chars recorded on visit). `ThreadSearchMatch` renders a "{field}: …" hint with highlighted ranges. The palette (`mod+k`) searches across envs and boosts recently or frequently opened threads, plus the last pick for the same query for 3 weeks. File-content search is a separate dialog (`projectSearch.toggle`). `Sidebar.logic.ts:1142-1185`, `components/ThreadSearchMatch.tsx`, `components/search/ProjectContentSearchDialog.tsx`.
- **Row status**:
  - States: approval, input, request (guest waiting), preparing ("Setting up"), working, waiting, failed, limited, ready. Ready rows show a "Done" dot when unseen. Other labels: "Woke" after a snooze wake, "Goal", "Pending" for an offline queued change. Once opened, a failed row settles to ready.
  - Idle or seen rows "recede" (dim).
  - The trailing area shows the PR badge and status, terminal-running state, and linked issues.
  - With `screenReaderWindowTitle` on, the status is also written into the window title.
  - Files: `Sidebar.logic.ts:919-1030`, `components/ThreadStatusIndicators.tsx`, `sidebar/SidebarThreadRow.tsx:821-872`.
- **Hover card**: shows title, PR mini-list, project with favicon, branch (with "You're currently checked out on another branch" when it differs), model · provider, earlier providers, worktree path, and last error. `sidebar/SidebarThreadRow.tsx:275-421`, `components/ThreadHoverCard.tsx`.
- **Thread action menu (right-click / ⋯)**: Find in thread, New thread on {branch}, Pin/Unpin, Settle/Un-settle, Snooze ▸ presets + Custom…, Wake, Rename, Regenerate title, Mark unread, Share with guest, Filter by project, Turn into project, Auto-settle behavior ▸ Follow settings / Never, Copy ▸ Path / Branch / Thread / Thread ID, Project settings, Archive, Delete. Read-only clients get every mutating item disabled, but triage actions stay enabled when `canTriage` is set. `components/threadActionMenu.logic.ts`.
- **Multi-select**: Cmd (mac) or Ctrl-click toggles a row, and Shift-click selects a range from the anchor within the ordered list. The bulk menu offers Settle (n), Snooze (n) ▸ presets/Custom, Mark unread (n), Regenerate titles (n), Unpin (n), and Delete (n) with "Delete N threads?" confirmation. Selection is in memory only (zustand). `apps/web/src/threadSelectionStore.ts`, `Sidebar.tsx:1617-1636,2395-2460`, `Sidebar.logic.ts:567-642`.
- **Keyboard**: `mod+b` toggles the sidebar. `mod+shift+[` / `mod+shift+]` go to the previous / next thread. `mod+1..9` jump to a visible thread (desktop only, with hints shown after a 200 ms hold). `packages/shared/src/keybindings.ts:59,129-139`, `Sidebar.logic.ts:75`.
- **Width**: resizable, persisted in localStorage `chat_thread_sidebar_width`. Default 256 px, minimum max(208 px, brand width), maximum viewport − 640 px. Reset removes the stored key. `components/threadSidebarWidth.ts`, `AppSidebarLayout.tsx:440-463`.
- **Sidebar chrome**: back / forward buttons, home, Settings, Pull requests, Issues, Usage, plus update pills (handled in another section). `components/sidebar/SidebarChrome.tsx`.
- **Notifications and badges**: status transitions in every connected env produce a sound, an in-app toast, or a desktop `Notification`. The dock badge counts pending notifications. Notifications are suppressed for snoozed or hidden-tag threads, and closed when the thread is opened. `components/ThreadNotificationCoordinator.tsx:55-120`.
- **Reopen closed view**: `mod+shift+t` (`view.reopenClosed`) restores the last closed right-panel tab or browser tab, from a stack of 20 persisted in `t3code:closed-views:v2`. Incognito browser tabs are not persisted. `apps/web/src/closedViewStore.ts:77,92`, `components/ReopenClosedViewShortcut.tsx`.
- **Saved views**: `SavedViewsMenu` is not for threads. It saves named filter presets for the Pull requests and Issues lists in localStorage, and shows an error if storage is full. `components/SavedViewsMenu.tsx`.

### Outside agents (MCP)

- **MCP server at `/mcp`**: OAuth sign-in with approval levels Read only / Supervised … Full access. A sign-in lasts 30 days and can be revoked under Settings → Sharing & access. Tools include `t3_thread_launch/wait/read/list/search/send`. The CLI offers `t3 thread start|send|show|list` with `--worktree`, `--generate-title`, `--provider/--model/--option`, `--wait`, and `--json`. Exit code 1 means fail or stop, and 2 means the thread waits on the user. Events cover the last 1,000 milestones, with webhook subscriptions up to 1 h. `docs/user/outside-agents.md`, `apps/server/src/mcp/`, `apps/server/src/cli/`.

#### Bugs / fragile UX

- `apps/web/src/threadSelectionStore.ts:42`: Cmd-clicking the anchor row to deselect it leaves `anchorThreadKey` pointing at the now-unselected row, so the next Shift-click ranges from a thread that is not selected.
- `apps/web/src/hooks/useThreadActions.ts:595-609`: after the user confirms the thread delete, a second plain OK/Cancel dialog asks "Delete the worktree too?". Cancel does not cancel the delete; it only keeps the worktree. It is easy to misread.
- `apps/web/src/hooks/useThreadActions.ts:611-616`: the running session is stopped before `deleteThreadMutation`. If the delete then fails, the agent has been stopped anyway.
- `apps/web/src/components/threadActionMenu.logic.ts:291`: "Copy → Path" is always enabled for threads, and a thread with no path only gets a "Path unavailable" error toast (`Sidebar.tsx:2892-2901`). The draft menu, by contrast, disables the item (`threadActionMenu.logic.ts:55`).
- `apps/web/src/components/threadActionMenu.logic.ts:306`: the "Copy → Thread" label carries the translation context "copy the thread contents", but the action copies a thread link or PR reference (`Sidebar.tsx:2910-2914`). This risks mistranslation.
- Thread tags have server commands and env settings (`hideTaggedThreads`, `hiddenThreadTags`), but no web or mobile UI dispatches `thread.tag.set`. Only the server's Orchestrator handles it, so in the GUI they are a hidden, unreachable feature.
- `apps/web/src/components/Sidebar.tsx` is 3,830 lines and `sidebar/SidebarThreadRow.tsx` is 1,986 lines, with many ref-snapshot workarounds (`orderedThreadKeysRef`, `planForwardNavigation`) to dodge stale state during drag and park animations. A native port should own this state in one model instead.

---

<!-- ===== Part B ===== -->

## Section B: Chat timeline, composer, models/providers, modes, approvals

The data model is the server's orchestration-v2 projection. A thread is a list of **turn items**, `OrchestrationV2TurnItem` in `packages/contracts/src/orchestrationV2.ts:1899`. The item types are: `user_message`, `assistant_message` (with `phase` and `delivery:"async"`), `reasoning`, `proposed_plan`, `todo_list`, `user_input_request`, `file_change`, `command_execution`, `file_search`, `web_search`, `approval_request`, `checkpoint`, `run_interrupt_request/result`, `system_notice` (with optional `mcpSignIn`), `error` (failure + retry), `compaction`, `handoff`, `fork`, `thread_created`, `secret_request`, `link_request`, `feedback_request`, `subagent`, `dynamic_tool` and `notification`. The web client turns these into timeline rows in `apps/web/src/session-logic.ts` (`deriveTimelineEntriesFromVisibleTurnItems`) and `apps/web/src/components/chat/MessagesTimeline.logic.ts`. The row kinds are: message, sent-message, assistant-meta, work, work-live, work-toggle, viewed-images, thinking, working, turn-fold, attempt-fold, event, proposed-plan, html-render, mcp-app, context-compaction and worktree-setup.

### Chat timeline: rendering and navigation
- **Virtualized message list**: built on LegendList. **Load earlier turns** pages history in, and "Loading work history" can be retried by clicking. The list follows new output while you are at the bottom; otherwise a **Scroll to end** pill appears. Scroll position is restored per visit, and opening a disclosure (expanding a section) keeps the scroll anchored. Files: `chat/MessagesTimeline.tsx`, `useTimelineFollow.ts`, `timelineScrollAnchoring.ts`, `useTimelinePositionRestore.ts`, `TimelineOverlays.tsx`.
- **Timeline minimap**: a rail of user-message ticks ("Jump to message: …") with Previous/Next turn, plus wheel handling. Files: `chat/TimelineMinimap.tsx`, `timelineMinimapItems.ts`.
- **Find in thread** (`mod+f`, command `chat.find`): a find bar with next/previous match, highlights, and searching/failed/retry states. Search runs on the server. Files: `chat/ThreadFindBar.tsx`, `threadFind*.ts`, `useThreadFind*.ts`.
- **Visit watermark**: marks where new activity starts since your last visit. File: `chat/ThreadVisitWatermark.tsx`.
- **User message row**: shows author ("You", "Agent", "Automation", "{name} (guest)"). Long messages get "Show full message/Show less". Attachments get preview/download. Snapshot attachments copy as text, with a too-large guard. Hover actions are **Copy prompt link** and **Edit from here**. Steer badges read "Steer · waiting to be read", "Steered the active turn" and "Originally queued, then promoted to steer". Shows "Sent by T3 Code" for system-sent compaction. Context chips render inline: terminal excerpt, review comment, preview annotation/crop, PR, thread and file. Files: `chat/TimelineUserMessageRow.tsx`, `UserMessageContextChips.tsx`.
- **Assistant message row**: markdown body. A hover meta bar holds a status badge (when not completed), **Copy message**, **Fork from this response**, **Reply to this response** and a day-aware timestamp with a full-time tooltip. Reply inserts a response-context chip into the composer. Timestamp format is locale, 12-hour or 24-hour (`settings.ts:67`). An empty reply shows "(empty response)". A reply cut off by a steer is labelled "Cut off by a steer". Files: `chat/TimelineAssistantRows.tsx`, `MessageCopyButton.tsx`.
- **Cite selection**: select text inside one assistant response and a toolbar offers **Cite**. It inserts a quote chip into the composer with an optional comment, and the chip links back to its source. Over-long selections show "Shorten selection". Files: `chat/AssistantSelectionToolbar.tsx`, `AssistantCitationChip.tsx`, `AssistantCitationCommentEditor.tsx`, `citationProcedure.ts`, `contracts/src/assistantCitations.ts`.
- **Markdown rendering** (`components/ChatMarkdown.tsx`, about 2.1k lines):
  - Parsed with `@pierre/words` into hast, then JSX (`chat/markdownRenderer.ts`); KaTeX renders math; output streams block by block (`markdownStreamBlocks.ts`).
  - Code blocks have a language label, **Copy code**, wrap toggle and **Run in terminal** (`MarkdownCodeBlock.tsx`).
  - Also: Mermaid diagrams, tables, `<details>`, task-list checkboxes that toggle (edits the source), and heading anchors.
  - File links open in the in-app file viewer; out-of-workspace files open read-only, with a "not found on {machine}" error when missing.
  - Thread links render as chips; external links have a context menu.
  - PR links can be linked to or unlinked from the thread.
  - Inline images and audio, plus a media gallery.
  - Codex artifact templates ("Use template").
  - Skill names inside text get special rendering (`SkillInlineText.tsx`).
  - Text direction is auto-detected (RTL) per block.
- **Image/video viewer**: an expanded dialog with previous/next across the media in a message, zoom (`ZoomableImage`), and save/copy-path from the context menu. For snapshot attachments it can "Show extracted text" and "Show accessibility JSON". Files: `chat/ExpandedImageDialog.tsx`, `ZoomableImage.tsx`, `markdownImageGallery.ts`.
- **Timestamps and elapsed time**: a live elapsed counter while the agent works (`AgentElapsed.tsx`, `ThreadStatusLine.tsx`).

### Chat timeline: work log, tools, reasoning, subagents
- **Work groups**: consecutive tool calls collapse into a "Tool calls"/"Activity" group with a summary line, an icon per tool surface, a hidden-count toggle and failure state ("{label}, tool call failed"). Images the agent viewed stay visible while the group is collapsed (row kind `viewed-images`). Live rows read "Working...", "Compacting…" and "Setting up worktree…". Files: `chat/TimelineWorkGroupRows.tsx`, `TimelineWorkEntryRow.tsx`, `WorkLog.tsx`, `TimelineToolActivityIcon.tsx`.
- **Tool-call visibility setting**: `showAll` (default), `hideSuccessful`, `hideAll` or `zen` (`contracts/src/settings.ts:363`).
- **Reasoning/thinking**: `reasoning` items render as a collapsible "Thinking" row (live) that becomes "Thought" when done. While a run is only waiting on children, "Thinking" is replaced by a subagent count (`MessagesTimeline.logic.ts:572`).
- **Turn and attempt folds**: a finished turn's work collapses behind a `turn-fold` label. Superseded retry attempts collapse into an `attempt-fold` (`MessagesTimeline.logic.ts:598-615`).
- **Item inspector**: clicking a tool row opens its input and output. Shown inside it:
  - Command output: lazily fetched when `outputOmitted` is set; "Earlier output not shown" marks truncation.
  - Search results and tool-output images.
  - **Open diff** for file changes.
  - Files: `chat/V2ItemInspector.tsx`, `CommandOutputPanel.tsx`, `ShellCommandBlock.tsx`, `HighlightedCodeLines.tsx`.
- **Command execution**: input command, streamed output, exit code, failure flag, and `monitor` / `completionWake` (wakes the agent when a background command finishes).
- **File changes**: each row shows the file name with +/- stats (`DiffStatLabel.tsx`) and a diff string or old/new strings. Turn `checkpoint` items carry per-file summaries that open the diff panel.
- **Lifecycle rows**:
  - "New session" (after `/clear`), "Context handoff" or "Context handoff pending" (provider switch, with strategy and summary), and "Conversation fork" / "Forked from conversation" with open links.
  - "Created thread" (child thread spawned), "Run interrupted", and "{title} {before} → {after} tokens" for compaction.
  - Files: `chat/V2LifecycleRow.tsx`, `TimelineSystemDivider.tsx`, `TimelineV2EventRow.tsx`.
- **Todo lists / Tasks**: `todo_list` items carry steps and an explanation, shown as a Tasks row you can open. Claude needs `CLAUDE_CODE_ENABLE_TODO_TOOLS=1`.
- **Subagents**: `subagent` items are either `provider_native` (Claude Task / Codex sub-agents) or `app_owned` (T3's `delegate_task` MCP creates a child thread). Each row has prompt, progress and result, with "Open subagent thread".
  - Native children open a read-only transcript dialog ("Subagent work", Refresh).
  - A child thread shows a parent bar ("{model} subagent: {status}", Open parent, "Runs on its own").
  - Files: `chat/NativeChildWorkDialog.tsx`, `ProviderSubagentBar.tsx`, `SubagentTooltipContent.tsx`.
- **Background work panel**: lists running or waiting commands, monitors, subagents and background tasks, with Details and Open subagent (`chat/BackgroundWorkDetails.tsx`).
- **Errors**:
  - `error` items show the provider failure and a **Retry** action; usage-limit errors show "Retry after {resetTime}".
  - The thread error banner offers "Copy request for admin" on ChatGPT workspace limits (`ThreadErrorBanner.tsx`).
  - The usage-limit recovery banner offers **Resume at reset**, Resume now, Snooze until reset, Wake now and Cancel auto-resume (`UsageLimitRecoveryBanner.tsx`).
- **Stop/Force stop**: a stop request is a `run_interrupt_request` item. If the provider does not confirm in time, `unresponsiveAt` is set and the UI shows "Agent not responding to Stop / Force stop", which kills the provider's processes. Files: `ThreadActivityBanners.tsx`, `useForceStopThread.ts`.
- **Worktree setup card**: shows branch and path, a setup-script log, Open terminal, Cancel and "Work locally" (`WorktreeSetupCard.tsx`).
- **Workflow opening block**: "Workflow started: {title}" with a "Show full script" expander (`WorkflowOpeningBlock.tsx`).
- **Request cards in the timeline**:
  - `secret_request`: a masked field, Save securely or Decline (`SecretRequestCard.tsx`).
  - `link_request`: link a machine via pairing code or address (`LinkRequestCard.tsx`).
  - `feedback_request`: send a report to the T3 team, optionally with the conversation (`FeedbackRequestCard.tsx`).
- **Screen-reader announcer**: an aria-live region reads turn status, in-between messages or every step (setting `screenReaderAnnouncements`: status, messages or everything) (`ThreadActivityAnnouncer.tsx`, `turnDetailAnnouncement.ts`).
- **Background/hidden threads**: threads kept mounted under Settings/Usage or shown as side chats turn off their global shortcuts (`chat/backgroundThread.ts`).
- **Side chats**: `/side <q>` or `/btw <q>` asks a side question in a new side chat. It opens in the right panel or the main view (setting `sideThreadOpenTarget`, default `panel`). Files: `useSideChat.ts`, `SideThreadPane.tsx`.
- **Handoff and compaction on cold threads**:
  - When a Claude thread's prompt cache has expired (over 200k tokens), a **Compact / Keep** chip sits next to Send. "Compact and send" sends `/compact` first (`useComposerSend.ts:1518`, `ComposerPrimaryActions.tsx`, `compaction.logic.ts`).
  - Switching provider mid-thread creates a `handoff` with a budgeted transcript. The strategies are `delta_since_target_last_seen`, `fork_delta_summary`, `full_thread_summary`, `checkpoint_summary` and `manual_context` (`orchestrationV2.ts:2056`; docs `docs/user/portable-handoffs.md`).

### Composer: editor
- **Tiptap rich-text editor** (`components/ComposerPromptEditorTiptap.tsx`, about 2.1k lines):
  - Built on StarterKit plus custom nodes: `composer-mention`, `composer-skill`, `composer-model`, `composer-citation` and `composer-context-reference`, a chip-range selection, undo grouping (`composer-undo-grouping.ts`) and code-block highlighting (`composerCodeBlockHighlight.ts`).
  - It writes markdown. When the caret is on styled text, the source markers show (`composer-markdown-source.ts`).
  - Typing `- `, `1. `, `- [ ] `, `> `, `# ` or `---` creates the matching block, and ```` ``` ```` + Enter opens a code block with a language picker (`ComposerCodeBlockLanguagePicker.tsx`, `composer-code-languages.ts`).
  - In lists, Shift+Enter continues the list and Tab / Shift+Tab nest and un-nest (`composer-list-continuation.ts`).
  - In code blocks, Enter auto-indents, Tab indents the selection, and Backspace at the start unwraps the block (`composer-code-block.ts`). Huge code blocks are shown without highlighting.
  - Toggle: Settings → Chat → Rich text composer (`composerRichTextEnabled`, default true).
- **Per-block text direction**: RTL/LTR is detected per top-level block; list and quote lines follow their block, and code stays LTR (`composer-text-direction.ts:41-66`).
- **Limits**: 120,000 characters per message, 100 attachments, 10 MiB per image, 80 MiB of images in total, 50 MiB per other file (`contracts/src/chatAttachment.ts:13-17`). Over-length drafts stay in the composer with validation (`ComposerPromptLengthValidation.tsx`).
- **Triggers**: `@` (path, threads, environments, models), `/` (commands and skills), `$` (skill) and `#` (pull request) (`composer-logic.ts:28`, `chat/useComposerMenuItems.ts`, `ComposerCommandMenu.tsx`).
  - **@ mentions**: workspace files and folders (fuzzy, server-side search) become file chips. Threads on the same server or a linked environment insert a thread-link reference that the agent reads on demand (`MarkdownThreadLink`, `composerThreadImport.ts`). Machines can also be mentioned. Models (`@opus`, `@opus:opencode`) name a model for delegation without switching (`composerModelMentions.ts`, `ModelMentionChip.tsx`).
  - **# PRs**: lists the newest PRs in the project repository, with thread-linked PRs first. Typing digits filters by number; an exact number is resolved even when old; a single word runs a text search. 180 ms debounce (`useComposerMenuItems.ts:153-200`).
  - **$ skills**: provider skills ranked by name, then label, short description, description and scope, with fuzzy fallback (`apps/web/src/providerSkillSearch.ts:13-70`). The list is deduplicated by name and shows only user-invocable skills. Skills also appear in the slash menu as `/skill:<name>` unless `showSkillsInSlashMenu` is off.
- **Context chips**: inline atoms for terminal excerpts, diff review comments, preview annotations, picked elements, PRs (state-colored), files, images (which also sit on a thumbnail shelf), threads and citations. They can be cut and pasted between drafts, and pasting re-fetches what they point to. A dashed outline marks a chip that is unresolved or unavailable. Files: `components/composerContextPresentation.tsx`, `composerInlineChip.ts`, `composerInlineTokenPaste.ts`, `contracts/src/composerContext*.ts`, `chat/TerminalContextInlineChip.tsx`, `FileTagChip.tsx`.
- **Paste handling**:
  - A paste of 32 KiB or more, or one that would exceed the character limit, becomes a text-file attachment.
  - `mod+shift+v` (desktop event `t3:paste-as-text`) keeps a large paste editable (`lib/desktopPasteAsText.ts`, `ChatComposer.tsx:1187`).
  - Image paste supports GIF/HEIC/HEIF/JPEG/PNG/WebP; HEIC is converted to JPEG and oversize photos are resized.
  - Typing anywhere while the composer is unfocused redirects into it (`usePasteToFocusComposer.ts`, `composerFocusRedirect.ts`).
- **Drag and drop**: files and folders from the OS, files from the workspace tree, and threads from the sidebar (a multi-selection drops together). A thread from a non-linked environment is attached as a transcript file. Files: `TimelineOverlays` ("Drop files to attach"), `folderDrop.ts`, `workspaceFileDrop.ts`, `threadContextDrop.logic.ts`, `ThreadContextDragGhost.tsx`.
- **Attachments strip**: thumbnails with progress, Retry upload, "Attach again" for expired uploads, Remove, and a "Draft attachment may not persist" warning. Uploads start immediately and Send waits for them (`ComposerAttachmentStrip.tsx`, `useComposerAttachmentIntake.ts`, `composerImagePersistence.ts`).

### Composer: commands, sending, queue, drafts
- **Slash commands** (registry `chat/useComposerMenuItems.ts:256-330`, handlers `ChatComposer.tsx:2090-2140`):
  - Built-in commands:
    - `/model`: opens the model picker.
    - `/side` and `/btw`: ask a side question in a new side chat (shown only when side chat is available).
    - `/clear`: starts a fresh provider session in the same thread; earlier messages stay visible. Shown only when a fresh session is available; it replaces the provider's own `/clear`.
  - Provider commands, from the selected instance's `slashCommands`:
    - `/compact` (`provider-core/src/server/snapshotProbe.ts:61`): Codex, Claude, Pi and others. Hidden when compaction is unavailable.
    - Codex adds `/goal <objective|pause|resume|clear>` and `/feedback [desc]`, which uploads logs to OpenAI (`server/src/provider/CodexProvider.ts:1162-1173`).
    - Claude lists the commands from the SDK's init message, including `/goal` and `/context` (`ClaudeProvider.ts:600,802`).
    - `/usage-limits` opens the usage-limits panel (`shared/src/usageLimits.ts:854`).
    - ACP agents advertise their own commands, with an input hint.
  - Provider commands, `/side`, `/btw` and `/clear` work only at the start of the message (`composerSlashCommandSearch.ts:15-30`). Commands sent mid-turn wait for the turn to end; some goal commands interrupt the turn instead.
- **Send keys**: setting `sendShortcut` is `enter` (default), `mod-enter-multiline` or `mod-enter`.
  - `mod+enter` with a turn running sends with the opposite follow-up behavior (`composer.sendAlternate`).
  - On a draft route, `mod+enter` / `mod+alt+enter` send in the background (`composer.sendBackground`); elsewhere `mod+alt+enter` sends and opens a new thread (`composer.sendAndNewThread`).
  - Defaults are in `packages/shared/src/keybindings.ts:94-110`.
- **Steer vs queue**: `followUpBehavior` is `queue` or `steer`, default **steer** (`settings.ts:539`). The user-message intents are `turn_start`, `queued_turn`, `steer` and `promoted_queued_to_steer` (`orchestrationV2.ts:1750`).
  - The primary button switches between Interrupt (empty draft), Queue, Steer, Update queued message and Resume thread; holding mod flips the action.
  - **Queued messages** are stored on the server. They appear as dashed rows at the end of the timeline, each with Send now, Edit (in the composer with highlight; Save or Cancel), Remove and Move command to front.
  - Shortcuts: `mod+shift+enter` steers the oldest queued message; `alt+up` edits the latest one.
  - Stopping the agent or a server restart holds the queue until **Resume**.
  - Files: `QueuedRunsControl.tsx`, `queuedMessageEdit.ts`, `useQueuedRunEdit.ts`, `useComposerResume.ts`, `ComposerHeldSendBanner.tsx`.
- **Multi-model fan-out**: Shift-click several models in a new thread's picker ("Choose models"). Send then starts one thread and worktree per model (`useMultipleModelSend.ts`, `ProviderModelPicker.tsx:259`).
- **Drafts**: kept per thread and per draft in localStorage `t3code:composer-drafts:v1`, including model, runtime mode, attachments and context (`composerDraftStore.ts:119`, about 4.5k lines). Draft threads have their own route (`/draft/$draftId`).
- **Prompt stash** (`mod+s`): saves the prompt and its attachments; up to 20 entries, about 2.7 MB of attachment data each; localStorage `t3code:prompt-stash:v2` (`promptStashStore.ts:11-17`). On an empty composer, `mod+s` restores a single stash or opens the stash menu. Uploaded files are retained for 24 h. Files: `ComposerStashMenu.tsx`, `ComposerStashBadge.tsx`, `useComposerStash.ts`.
- **Prompt history recall**: ArrowUp/ArrowDown in an empty composer walks the prompts sent in this thread. Only the text comes back (`composerPromptHistory.ts`).
- **Suggested next prompt** (Labs): for Claude and Codex, a ghost suggestion appears; Tab accepts, Esc dismisses (`usePromptSuggestion.ts`).
- **Edit from here (rewind)**: rewinds the provider history to before a message. **Revert files too** is offered only for worktree threads with no other users of that directory. The prompt returns to the composer (`useTimelineRewind.ts`, `CodexThreadRevert.ts`).
- **Prompt links**: `/new?prompt=&repo=&provider=&model=&pr=` opens a ready-to-send draft (prompt up to 8,000 characters). "Copy prompt link" is available on sent messages and in the command palette (`docs/user/prompt-links.md`).
- **Voice input** (`apps/web/src/speech`, `apps/server/src/speech`):
  - Start with the mic button or `mod+shift+d`. The shortcut mode is auto, hold or toggle. Recordings last up to 5 min; Esc cancels.
  - The transcript is inserted at the caret or replaces the selection, and the editor is read-only while recording. Pressing Send while recording queues the send until the transcript lands.
  - Audio streams as PCM from an AudioWorklet (`pcmWorklet.ts`) to the chosen environment over the authenticated connection. Transcription there uses the `transcribe-cpp` native addon in a child worker with GGUF models from `speech/catalog.json` (Parakeet, Whisper, Canary, Qwen3-ASR, Voxtral…) and a Silero voice-activity detector. Alternatives: cloud OpenAI, Grok or OpenRouter (`CloudSpeech.ts`), or Apple Speech on macOS 26 desktop (`appleSpeech.ts`).
  - Options: language or auto, translate to English, a personal dictionary plus a project dictionary (up to 100 hints), silence filtering, filler removal, optional LLM transcript cleanup with a correction cue (`postProcessing.ts`), streaming live preview, and a microphone priority list.
  - UI: `ComposerSpeechButton.tsx`, `VoiceInputPill.tsx`, `VoiceInputSetup.tsx` (model download), `GlobalVoiceInputIndicator.tsx`. Speech also works in other text fields via `useTextFieldSpeech.tsx`.
- **Composer banners**: provider status (unauthenticated, broken or unsupported version, Refresh), server update and auto-balance update, usage limits with account reveal and "Manage usage", fresh-session notice, project clone. Files: `ComposerBannerStack.tsx`, `ProviderStatusBanner.tsx`, `ComposerUsageLimits.tsx`, `useSystemComposerBanners.tsx`.

### Models, providers, options
- **Drivers** (`apps/server/src/provider/builtInDrivers.ts:33-45`), with the transport each one uses:
  - **Codex**: `codex app-server` JSON-RPC over stdio (`packages/effect-codex-app-server`, `codexLaunchArgs.ts`). Supports a managed ChatGPT login and a shadow `CODEX_HOME` for extra accounts.
  - **Claude**: `@anthropic-ai/claude-agent-sdk` (`orchestration-v2/Adapters/ClaudeAdapterV2.ts`). A separate `CLAUDE_CONFIG_DIR` per instance.
  - **Cursor**: `@cursor/sdk` local-agent runtime.
  - **Grok**: ACP via `grok agent stdio`, where the permission mode maps to spawn arguments (`provider-grok/src/server/acpSupport.ts:52-56`).
  - **fx**: ACP via `fx acp`.
  - **Kiro**: ACP via `kiro-cli acp --agent-engine=v3`.
  - **Antigravity**: Google's ACP agent, with T3-managed install and sign-in (`provider/acp/Antigravity*`).
  - **OpenCode**: HTTP server via `@opencode-ai/sdk/v2` (1.x) or `@opencode/client` (2.x). Local or external server URL plus a password.
  - **Pi**: `pi --mode rpc`, LF-delimited JSON over stdio (`provider-pi/src/server/rpc.ts`).
  - **Muse Code**: `@muse-code/sdk`.
  - **ACP Registry**: any registry agent or local ACP command, ACP v2 preview with v1 fallback (`packages/provider-acp*`, `effect-acp`).
- **Provider instances**: `ProviderInstanceConfig {driver, displayName, accentColor, environment[], enabled, config}`.
  - Environment variables can be plain values or secret references marked sensitive. Driver-specific config covers binary path, launch arguments, home or config directory and custom models.
  - Instances are managed with create, upsert and remove mutations (`contracts/src/providerInstance.ts:74-100`). The default instance id equals the driver kind.
- **Model selection**: the wire form is `{instanceId, model, options:[{id,value}]}`; a legacy `provider` key and an options object are coerced (`contracts/src/modelSelection.ts`, `model.ts:74-100`).
  - Options are generic descriptors, either `select` (choices with isDefault, `promptInjectedValues`, `role:"reasoning"`) or `boolean`.
  - Claude ids: `effort` (low, medium, high, xhigh, max, ultrathink, the last prompt-injected), `thinking`, `fastMode`, `ultracode`, `contextWindow` (200k or 1M) (`provider/model-manifest.json`, `ClaudeModelCatalog.ts:65-108`, `claudeModelOptions.ts`).
  - Codex ids: `reasoningEffort`, `serviceTier` (Standard, Fast, Ultrafast), `cyberAccessProgram` (Standard or Daybreak) (`CodexProvider.ts:291-334`, `codexModelOptions.ts`).
  - Others: OpenCode and Kiro have an `agent` select (including `plan`); fx has a backend and Effort.
- **Default models** (`model.ts:144`): Codex `gpt-6-astra`, Claude `claude-opus-5-5`, Cursor `auto`, OpenCode `openai/gpt-5`, and the others `default` (defer to the agent's own configuration). Text-generation defaults differ, for example `claude-haiku-5-5` and `gpt-6-luna`. Model slug aliases exist for Codex and Cursor.
- **Model picker** (`mod+shift+m`; `modelPicker.jump.1-9`; `mod+shift+up/down` switches provider):
  - A provider sidebar with a Favorites tab, model search, favorite stars and "New" badges.
  - "Show more" reveals older models; backends are grouped with an "Unavailable" state (fx); "Set up {provider}" appears for unconfigured providers.
  - Unavailable saved models are kept with an "(Unavailable)" label.
  - Account switching within the same `CODEX_HOME` or `CLAUDE_CONFIG_DIR`.
  - Files: `ProviderModelPicker.tsx`, `ModelPickerContent.tsx`, `ModelPickerSidebar.tsx`, `ModelListRow.tsx`, `modelSelection.ts`, `modelOrdering.ts`, `useApplyThreadModelPick.ts`.
- **Traits / effort controls**: an effort glyph with `mod+shift+e`, `composer.increaseEffort` and `composer.decreaseEffort`, plus an Options popover with booleans (Fast, Thinking, Ultracode) and selects (context window, agent, service tier). A Fast or Ultrafast speed badge shows when on. Files: `TraitsPicker.tsx`, `ComposerModelEffort.tsx`, `TraitsSpeed.tsx`, `EffortGlyph.tsx`, `composerProviderState.tsx`. Unset reasoning or tier uses the provider's own configuration.
- **Model defaults**: the last provider, model and options are remembered for new threads; a project-configured model takes precedence. Custom models (slug, name, capabilities) are added in Settings → Providers → Models (`customModelEditor.logic.ts`).
- **Context window / usage**: thread details show context usage once the provider reports it (`ThreadDetailsUsage.tsx`). Claude has an auto-compact threshold (100k–1M) and an idle-compaction mode (When you return, Before cache expires, Never).
- **Provider updates**: "Update all" updates every outdated provider on all connected environments. It skips providers whose instances only offer a manual command or disagree on the update command. After an update, model discovery refreshes (`components/ProviderUpdatesAction.tsx`; `docs/user/updating.md:175-192`).
- **Provider MCP sign-in**: a `system_notice.mcpSignIn` notice shows **Sign in**, opens the authorization URL, then asks you to paste the final redirect address and choose **Finish sign-in** (`chat/McpSignInAction.tsx`, `server/src/mcpApps/McpSignIn.ts`, `CodexMcpElicitation.ts`).

### Permission modes, plan mode, approvals, questions
- **Runtime modes**: `approval-required` (Supervised), `auto-accept-edits`, `auto` (provider auto-review), `full-access` (default) (`contracts/src/providerPolicy.ts:25-43`).
  - Set per thread from the footer or the model picker's "Permissions" entry (`mod+shift+a`, `composer.mode`). The default comes from Settings → Threads, with a per-project override.
  - A managed policy can cap the allowed modes (`isRuntimeModeAbove`).
  - Per-provider fallbacks: Muse and Kiro offer only Supervised and Full access; Grok has no auto-accept-edits; fx full access needs 0.0.14 or later; OpenCode Auto asks (`docs/user/permission-modes.md`).
  - Mapping: Codex receives an approval policy plus a sandbox policy and `approvalsReviewer`; Claude's permission mode becomes default, acceptEdits, auto, bypassPermissions, dontAsk or plan (`ClaudeAdapterV2.ts:3706`).
  - Files: `chat/runtimeModeConfig.ts`, `ComposerFooterControls.tsx`, `ComposerPermissionsSection`.
- **Plan mode**:
  - `ProviderInteractionMode` is `default` or `plan` (`providerPolicy.ts:45`). The server maps `plan` to Codex `collaborationMode` (`CodexAdapterV2.ts:1018`) and Claude `permissionMode:"plan"`.
  - Scheduled tasks and the orchestrator MCP (`create_threads`, `delegate_task`) can set it. **The web composer always sends `default`** (`useComposerSend.ts:1491,1524,1545,1595`). In the composer UI, plan comes from a provider `agent=plan` option (OpenCode/Kiro), which is filtered out unless selected (`providerModels.ts:102`, `modelSelection.ts:403`).
  - **Proposed plan card**: renders `proposed_plan` markdown (Codex plan output, Claude ExitPlanMode) with a "Plan" badge, its title, an expand/collapse control and a menu: Copy to clipboard, Download as markdown, **Save to workspace** (path dialog, `<title>.md`) (`ProposedPlanCard.tsx`, `proposedPlan.ts`). No "implement plan" button exists in this card.
- **Approvals**:
  - Request kinds: command, file-read, file-change, mcp-elicitation, permission. Decisions: `accept`, `acceptForSession`, `acceptAlways`, `decline`, `cancel` (`providerPolicy.ts:49-78`).
  - Default buttons are Approve, Decline, "Always allow this session" and Cancel. Provider-supplied options can relabel them and add warnings, and extra options go under a "More" menu (`ComposerPendingApprovalActions.tsx:28-40`).
  - `fullAccessOverride` (`ask_rule` or `provider_check`) explains why a full-access thread still asked.
  - Timeline row: "Waiting for approval"; when the provider process is gone, "Interrupt or restart the run to respond" (`TimelinePendingApprovalRow.tsx`).
  - Subagent and child-thread approvals surface in the parent as "Approval from {title}" (`ChildThreadApprovals.tsx`).
  - Claude: typing a text reply while an approval is pending declines it with feedback.
  - A thread banner offers Open approval / Open question.
- **User-input questions** (AskUserQuestion-like):
  - `user_input_request` carries questions `{id, header, question, options[{label, description, value, preview}], multiSelect, allowCustomAnswer, initialAnswer, required}` (`orchestrationV2.ts:1569`).
  - The composer swaps to a question panel with previous/next navigation, option selection (single or multi), a custom answer, show/hide and **dismiss without answering** (Codex async questions; `responseMode:"message"` answers become a new message).
  - Per-question file and image attachments (up to 100 per answer set) go as `attachmentsByQuestionId` (`providerPolicy.ts:80-97`; `ComposerPendingUserInputPanel.tsx`, `pendingUserInput.ts`, `questionAttachments.ts`).
- **MCP apps in chat**: a tool result carrying an MCP Apps UI renders a sandboxed iframe below the tool call (Codex, and Claude Code 2.1.301 or later).
  - The app can call its server's tools (the user is asked unless the tool is marked read-only) and post messages (asked every time). It can go full screen (Esc returns it; clicks within 5 s of other UI activity are ignored). Its latest model-context note rides along with your next message.
  - It needs the thread's agent to be running.
  - Files: `chat/McpAppFrame.tsx`, `components/mcp/mcpApps.ts`, `mcpGate.ts`, `server/src/mcpApps/*`, `contracts/src/mcpApps.ts`.
- **HTML renders (visual replies)**: the agent calls T3's MCP tool `html_preview` (a PNG, contentHeight and console output from a T3-owned headless Chrome, about 120 MB downloaded once), then `html_render`.
  - The page shows inline above the reply in a themed, sandboxed frame. Height is 80–2000 px, fitting up to 20,000 px; column width is 728 px (`shared/src/htmlRender.ts:9-26`).
  - Local images are embedded at publish time. Network access is public-only, with private-IP DNS blocked (`htmlRender/publicProxy.ts`).
  - A full-screen dialog has zoom controls.
  - Files: `server/src/htmlRender/HtmlRender.ts`, `headlessChrome.ts`, `mcp/toolkits/html/tools.ts`, `chat/HtmlRenderFrame.tsx`, `HtmlRenderDialog.tsx`.
  - Related T3 MCP tools: `send_to_user` (a verbatim markdown message mid-turn), `t3_thread_send_attachments`, `request_secret` and `t3_feedback` (`mcp/toolkits/*`).

#### Bugs / fragile UX
- `apps/web/src/components/chat/useComposerSend.ts:1491,1524,1545,1595`: every web send hard-codes `interactionMode: DEFAULT_INTERACTION_MODE`. The contract-level `plan` mode, which the Codex and Claude adapters support, cannot be reached from the web composer. The draft store still persists `interactionMode` (`composerDraftStore.ts:353,3578`), so that state is half-dead.
- `apps/web/src/components/CommandBlock.tsx:35`: setup commands render with `truncate` and have no wrap or tooltip, so long install commands cannot be read in full in the UI (copy still works).
- `apps/web/src/components/chat/ChatComposer.tsx:2113`: choosing `/side` or `/btw` while side chat is disabled returns silently and leaves the `/…` trigger text in place. The only feedback is the menu row's description text (`useComposerMenuItems.ts:272`).
- `apps/web/src/components/chat/useComposerMenuItems.ts:323-327`: a provider's `/clear` is hidden only while T3's fresh-session `/clear` is available. When that is unavailable, the provider's raw `/clear` reappears in the menu. By the code's own comment, sending it would clear a session T3 still treats as live.
- `packages/contracts/src/settings.ts:539-541`: `followUpBehavior` defaults to `steer`. For Claude a steer waits until the running tool calls finish and can be dropped and re-sent as the next turn (`docs/user/providers-claude.md`, "Steering"), so the default ordering of follow-ups is surprising.

---

<!-- ===== Part C ===== -->

## Section C — Code / dev-tooling features

Paths are relative to the repo root. Web UI = `apps/web/src`, server = `apps/server/src`, desktop = `apps/desktop/src`.

### Diff panel (right-panel "diff" surface)

- **Diff sources (scope picker)**: a dropdown in the panel header picks the source. Options: **Changes** (branch vs a base ref, the default `{kind:"branch", baseRef:null}`), **Uncommitted** (working tree), **Latest turn**, a **Turn ▸** submenu listing each agent turn, **Commits** (one local commit at a time, newest 100 in the branch comparison; a merge commit diffs against its first parent), and a single **tool-call** diff opened from a file-change item in the timeline. The selection is persisted per thread in localStorage (`t3code:diff-panel-state:v1`). Files: `apps/web/src/diffPanelStore.ts`, `components/DiffPanel.tsx` (scope menu ~L983-1060), `components/chat/useRightPanelActions.ts:312`.
- **Turn diffs vs whole-thread diffs**: turn diffs come from the checkpoint refs (`orchestration.getTurnDiff {fromTurnCount,toTurnCount}`). The whole-thread diff is `getFullThreadDiff {toTurnCount}`. Both return the thread folder's patch plus one patch per tracked repository (multi-repo projects). A turn's diff leaves out files that git brought in through pull/merge/rebase (`listAuthoredPaths`). Contract: `packages/contracts/src/checkpointDiff.ts`; server: `checkpointing/CheckpointDiffQuery.ts`, `checkpointing/Diffs.ts`.
- **Base-ref comparison picker**: "Comparing {head} against {base}" with a searchable ref combobox and a "Use remote version of {ref}" toggle (remote-only refs). Backed by `review.getDiffPreview`, `review.getBranchCommits`, `review.getDiffFileContents` (`review/ReviewService.ts`, `lib/baseRefChoices.ts`).
- **Repository picker**: in multi-repo threads it narrows the diff to one repository. Paths are relative to the thread folder (`DiffPanel.tsx` ~L1184-1220).
- **Toolbar toggles**: stacked vs split layout (`diffStyle: split|unified`), line wrap, hide whitespace (session override of the setting), **Hide test files** (shown only when test files are present; `@t3tools/shared/changedFileKind`), file-tree show/hide, refresh, and collapse/expand all files. Live auto-refresh is throttled (`useLiveRefresh`, `LIVE_REFRESH_MIN_INTERVAL_MS`).
- **File tree**: a collapsible folder tree of changed files with add/remove stats and expand/collapse-all-folders. Clicking a file reveals it in the code view, and scroll position is restored per scope. Files: `components/diffs/DiffFileTree.tsx`, `diffFileTree.logic.ts`, `useCodeViewFileReveal.ts`, `useCodeViewScrollRestore.ts`.
- **Rendering**: the `@pierre/diffs` library with Shiki-style highlighting, run in a shared **Web Worker pool** of `clamp(cores/2, 2, 6)` workers. The pool is disposed 30 s after its last consumer and follows the theme (`pierre-dark` / `t3-light`). Large files load lazily (`DiffFileLoadingBoundary`, `lib/diffFileContents.ts`) and can expand context lines. Files: `components/DiffWorkerPoolProvider.tsx`, `components/diffs/StyledDiffCodeView.tsx`.
- **Review comments → agent**: select line(s) in a diff to get "Comment on lines {range}". Comments are stored in the composer draft's `reviewComments` (⌘/Ctrl+Enter saves) and go to the agent as context chips with the next message. Each record carries the file, range label, the fenced diff excerpt, an inferred fence language and optional PR metadata. Whole-file comments also exist (`buildFileReviewComment`). Files: `reviewCommentContext.ts`, `components/diffs/AnnotatableCodeView.tsx`, `DiffCommentAnnotation.tsx`, `composerDraftStore.ts`.
- **File actions**: clicking a file header opens it in the in-app file preview (right panel) when a thread is active, or else in the preferred editor. Absolute paths (from Claude/ACP) are converted to workspace-relative paths and `..` paths are rejected. The context menu offers Open, Reveal in folder, and Open with ▸ {editor}, and there is a copy-path button. Files: `diffFileActions.ts`, `fileContextMenu.ts`, `components/DiffFilePathCopyButton.tsx`.
- **Commit links**: a code-host commit URL opens in the Diff pane when the commit exists locally. A PR commit link opens the PR. Other links follow the "Open links in" setting.
- **Settings → Diffs**: default layout (stacked/split), ignore whitespace by default, default file state (expanded/collapsed), "Open pull requests and changes panels proactively", and "Open pull requests in T3 Code". `components/settings/SettingsPanels.tsx` ~L1709-1850, route `routes/_chat.settings.diffs.tsx`.

### Checkpoints / revert / restore

- **Mechanism (hidden git refs, not a shadow repo)**: before each turn a baseline snapshot of the workspace is taken, and after the turn another. A snapshot builds a commit from a **temporary index** (`$GIT_COMMON_DIR/t3-checkpoint-index-*`) with `write-tree` and `commit-tree` (author "T3 Code"). Objects are written to a private quarantine object dir, then published. The commit is pinned with `update-ref` under `refs/t3/orchestration-v2/checkpoints/<b64(sha256(scopeId)[:32])>/ordinal/<N>`. Untracked-file blobs are reused when unchanged (stat cache). A folder that matches HEAD exactly is snapshotted from HEAD without reading any files. Nested repositories get their own scopes (max 6 concurrent), and the baseline has a 5 s budget. Files: `orchestration-v2/CheckpointService.ts:35-41,292`, `checkpointing/CheckpointStore.ts`, `vcs/GitVcsDriver.ts` ~L1236-2070, `vcs/checkpointUntrackedBlobs.ts`.
- **Diff limits**: checkpoint diffs are capped at 10 MB and end with a `[truncated]` marker (`vcs/VcsDriver.ts:43`, `checkpointDiff.ts:8`).
- **Revert-to-turn UX ("Edit from here?")**: on a user message the dialog offers **Revert and keep changes** (rewinds the chat only; the prompt and attachments go back to the composer), **Revert files too** (also restores files from the checkpoint), or, in multi-repo worktrees, **Revert repositories too**. A restore across several folders is all-or-nothing: folders are snapshotted first and rolled back if any restore fails. Files: `components/chat/ChatOverlayHosts.tsx:220-300`, `components/chat/useTimelineRewind.ts`, `CheckpointService.ts:821-900`.
- **Branch-mismatch banner**: "Branch changed from X". Sending continues on the current branch; **Restore branch** switches back (`components/chat/ThreadActivityBanners.tsx:300-350`).
- Checkpoint refs are deleted when no longer needed (`deleteCheckpointRefs`).

### Terminal

- **PTY**: `node-pty ^1.2.0-beta.15` on the server (`terminal/NodePtyAdapter.ts`), loaded with `createRequire` so single-executable builds work. Default shell: `$SHELL` or bash on POSIX. On Windows it is pwsh.exe, then powershell.exe, then cmd.exe; PowerShell gets a UTF-8 setup `-Command`. Override with `T3CODE_TERMINAL_SHELL`. fish starts with `no-query-term`. Default size is 120×30. Some environment variables are blocked from the PTY env. Files: `terminal/Manager.ts:100-140,540-690`, `docs/user/terminal.md`.
- **Renderer: not xterm.js**. The web terminal is a custom adapter over **libghostty-vt compiled to WASM** (`ghostty-vt.wasm` plus a 112-byte PTY-reply trampoline), drawn on Canvas 2D. Input, IME, selection, scrolling, links and cursor blink live in `surface.ts`. A bundled Nerd Font symbols file renders prompt glyphs. Font size range is capped at 32. Files: `apps/web/src/terminal/ghostty/{runtime,core,renderer,surface,keyCodes}.ts`, README there. Mobile uses the same C ABI.
- **Tabs / splits**: a per-thread bottom drawer (or right-panel terminal surface) holds terminal *groups*. Each group is Single, Side by side (horizontal) or Stacked (vertical), with **max 4 terminals per group** (`apps/web/src/types.ts:36`). Default keys: mod+J toggle, mod+D split, mod+Shift+D split vertical, mod+N new, mod+W close (`packages/shared/src/keybindings.ts:67-75`). A focused terminal keeps Ctrl chords such as tmux prefixes (`keybindings.ts:410`). Files: `components/ThreadTerminalDrawer.tsx`, `terminalUiStateStore.ts`, `components/chat/useChatTerminalActions.ts`.
- **Labels**: a tab shows the running foreground subprocess name, found by polling the process table every 1 s, backing off to 60 s; on Windows it queries PowerShell. Otherwise the label is "Terminal N" (`Manager.ts:395-410`, `@t3tools/shared/terminalLabels`).
- **Persistence**: terminals keep running when you switch threads (`components/PersistentThreadTerminals.tsx`). The server keeps 5,000 lines / 8 MiB of scrollback per terminal and writes it to `<logs>/terminals/` files with a 250 ms debounce. Saved history is replayed on reconnect or restart with inherited alt-screen and mode state neutralized. Up to 128 inactive sessions are retained (`Manager.ts:100-120,1208-1240`; `config.ts:175`).
- **Context menu / keys**: Add to chat (sends the selection as composer context), Copy, Paste, Select all, Jump to latest (`ThreadTerminalDrawer.tsx:414-441`). Shift+PgUp/PgDn scrolls, Ctrl/Cmd+Shift+Home/End jumps to start/end, Cmd+A / Ctrl+Shift+A selects all, and a clear shortcut is available. On iOS, long-press gives Paste/Select Text.
- **Links**: detects http(s) URLs and file paths, including `file:line:col`, TypeScript `(l,c): error TS`, PowerShell prompt paths, quoted paths, and paths wrapped across lines. It skips scheme URLs, SCP git remotes and HTTP request routes. Paths open in the editor or file preview; URLs open in the preview browser or the system browser (`terminal-links.ts`, `components/preview/openTerminalLinkInPreview.ts`).
- RPC: `terminal.open/attach/observe/write/resize/clear/close`. The output protocol is in `terminal/OutputProtocol.ts`. Read-only observers have Paste disabled.

### Git / VCS (local)

- **Status**: VCS status is streamed by subscription (`vcs/VcsStatusBroadcaster.ts`), with an automatic `git fetch` every **30 s** by default (`DEFAULT_AUTOMATIC_GIT_FETCH_INTERVAL`, `contracts/settings.ts:1110`). RPCs: `vcs.refreshStatus/refreshBranch/listRefs/createRef/switchRef/pull/init/createWorktree/removeWorktree`.
- **Branch toolbar** (under the composer): a **Workspace** selector chooses the current checkout, "New worktree", or "Previous worktree". In multi-model mode, "Each model starts in its own worktree". The **branch selector** lists refs with paging and search, badges (current / worktree / remote / default), "Start from remote", create-and-switch, and Copy branch name. Files: `components/BranchToolbar*.tsx`, `BranchPicker.tsx`, `WorktreeBaseBranchPicker.tsx`, `WorktreeRepositoriesControl.tsx`.
- **Worktrees**: these are git worktrees under a worktrees directory (`worktreesDirectory.ts`, `vcs/worktreeRemoval.ts`, `worktreeCleanup.ts`). The setting "Start new worktrees from remote by default" is in `settings/WorktreesSettings.tsx`. **Branch naming**: Static prefix / Semantic prefix / Custom instructions, plus a branch prefix and instructions; the name is generated from the first message (`settings/BranchNamingSettings.tsx`).
- **Git actions control (stacked actions)**: actions are `commit | push | create_pr | commit_push | commit_push_pr` (`contracts/git.ts:19`). The menu shows the context-dependent primary action: Commit / Commit & push / Commit, push & PR / Push & create PR / Pull / Sync ref / Publish repository / Initialize Git. Disabled actions get hints, e.g. "Branch is behind upstream", "Add an origin remote", "Detached HEAD". The commit dialog has a per-file include checklist (`filePaths`), an optional message ("Leave empty to auto-generate") and **Commit on new branch** (`featureBranch`). Acting on the default branch asks for confirmation, with an option to check out a feature branch and continue. Progress streams through phases branch→commit→push→pr with stdout/stderr and an elapsed timer. Files: `components/GitActionsControl.tsx`, `GitActionsControl.logic.ts`, `git/GitWorkflowService.ts`, `git/GitManager.ts`.
- **Publish repository wizard**: Provider → Repository → Summary, choosing visibility (Private/Public) and protocol. It creates the remote repository, adds `origin` and pushes (`GitActionsControl.tsx` ~L558-990).
- **Clone / new project**: the "Add Project" palette clones from a provider or a URL, including branch/file page URLs. It falls back to HTTPS when SSH is refused, offers ghq-style `<host>/<owner>/<repo>` layout, and shows a progress toast with cancel/retry. "New project" creates `~/.t3/projects/<slug>` with a README and first commit, optionally as a private GitHub repo. RPC: `sourceControl.cloneRepository/publishRepository/lookupRepository`; `vcs/CheckoutCloner.ts`.
- A created PR is linked to the thread automatically, as is a PR the agent opens with `gh pr create` or `glab mr create` (`git/linkCreatedPullRequest.ts`).

### Source-control providers & auth

- **Hosts** (registry order, `sourceControl/sourceControlDefinitions.ts`): **GitHub**, **Cursor Origin**, **GitLab**, **GitCafe**, **TanStack Source** (behind the `T3CODE_FEATURE_FLAGS=tanstack_source` flag), **Forgejo/Gitea** (one entry), **Bitbucket**, **Azure DevOps**. Each has a package under `packages/source-control-*`.
- **Auth per host**:
  - GitHub talks to the REST and GraphQL API directly. Credential precedence: a token saved in Settings (server secret store), then `GH_TOKEN`/`GH_ENTERPRISE_TOKEN`+`GH_HOST`, then `gh` CLI ≥2.81 auth. Users can pick an account per host. There is also an optional **GitHub App device-flow sign-in**, enabled only when the `githubAppClientId` feature flag is set (`source-control-github/src/server/GitHubAppSignIn.ts`).
  - GitLab uses the `glab` CLI.
  - Forgejo/Gitea use the `fj` CLI first, then `tea` ≥0.16.
  - Azure DevOps uses `az` with the devops extension.
  - Cursor Origin uses the `origin` CLI.
  - GitCafe uses a saved token, then `CAFE_TOKEN`, then the `cafe` CLI.
  - TanStack Source uses a saved token, then `SRC_TOKEN`, then the `src` CLI.
  - Bitbucket uses a saved access token, or Atlassian email + API token, or the env vars `T3CODE_BITBUCKET_*`.
  - "Rescan" re-detects. Files: `sourceControl/SourceControlDiscovery.ts`, `SourceControlSignIn.ts`, `sourceControlHostSecrets.ts`, `settings/SourceControl*Settings.tsx`.
- **Remove agent credits when merging** (default off): strips agent co-author and generated-by trailers from GitHub merge/squash messages.
- **PR sharing / routing across environments**: per-environment **Read PRs** or **Read and act**, so one machine's credentials can serve another (`settings/PullRequestRoutingSettings.tsx`).

### Pull requests page & detail

- **List** (`routes/_chat.pull-requests.tsx`, `components/pullRequest/PullRequestsPage.tsx`): grouped list filtered by involvement (all/reviewing/authored), state, draft, review decision, checks, labels, authors and project/server scope. **Saved views** (local only, not synced). Shift shows quick actions, including drag-to-close several PRs. Stack position badges.
- **Detail tabs**: Summary (editable title/description, tickable checkboxes), Timeline/activity (comments, reactions, resolve threads, replies), **Code** (diff with line comments that stay drafts until **Submit review** as comment/approve/request-changes; per-file **viewed** marks, synced to GitHub's or stored on the server for other hosts), Checks popover (logs, rerun), and **Guide** (beta). Files: `PullRequestDetailPanel.tsx`, `PullRequest{Summary,Timeline,Code,Guide}Tab.tsx`, `filesViewedStore.ts`.
- **Actions** (`contracts/pullRequest.ts:59-80`): merge (merge/squash/rebase/fast-forward), ready, draft, close, reopen, delete-branch, update-branch (merge/rebase), enable/disable auto-merge (GitHub, GitLab, Azure), revert (GitHub), approve-workflows (GitHub forks). Also request reviewers, set labels, and react. A merge or review is pinned to the head commit you loaded; "Load new commits" refreshes it. GitHub stacks offer **Merge stack** and **Rebase stack**.
- **Work on PR / checkout**: `git.preparePullRequestThread` in mode `local|worktree` opens a thread with the PR attached (`git/GitManager.ts`, `pullRequestCheckoutToast.ts`).
- **Linked PRs/issues on threads**: a Linked items panel and the "Link pull request" palette entry (`LinkPullRequestDialog.tsx`, `ThreadPullRequestsPanel.tsx`, `pullRequest/linkedThreads.ts`). Agent tools: `link_pull_request`, `link_issue`, `watch_pull_request`.
- **PR watch**: the server polls every 2 min and wakes the agent on failed checks, approval-needed, checks passing, comments, conflicts or merge-queue drops. GitHub uses conditional requests. The watch stops after 10 comment-only wakes in a row, 8 failed reads, Stop, or settle (`pullRequest/PullRequestWatchService.ts`).
- **Review guide** (beta): a coding agent (Claude or Codex only) reads the PR in a temporary checkout, allowed only `git diff/log/show`. It writes ordered "stops", with "Look closely" flags and mechanical changes grouped at the end, plus suggested questions and "Ask about this stop". Settings provide a review guide model and instructions. Files: `pullRequest/PullRequestReviewGuideService.ts`, `components/pullRequest/guide/`.

### Issues & triage

- **Issues page** (`routes/_chat.issues.tsx`, `components/issue/IssuesPage.tsx`): issues from GitHub, GitLab, Azure DevOps and Forgejo projects plus **Linear** (API-key accounts and per-project team binding; `packages/issue-tracker-linear`, GraphQL). It is grouped per tracker account with filters for open/closed, involvement and tracker, a per-project hide list, tracker-side search, Load more and saved views. The detail panel lets you comment, edit, change labels/assignees, react, and change state (GitHub issue statuses / Linear workflow states). Issues can be linked to a thread. Server: `issue/IssueService.ts`, `IssueTrackerRegistry.ts`.
- **Triage an issue**: a palette entry opens a project-less thread with the triage playbook. The server shallow-clones `github.com/pingdotgg/t3code` at the running version (no-prompt git env) so the agent can read the source and logs and draft a GitHub issue. CLI fallback: `npx t3 triage [--agent codex|antigravity]` (`triage/TriageService.ts:19-100`, `triagePrompt.ts`).

### Text generation (commit messages, PR bodies, branch names, titles)

- Tasks: `generateCommitMessage`, `generatePrContent`, `generateBranchName`, `generateThreadTitle`, `generateReviewGuide`, `generateTranscriptionPostProcessing` (`packages/provider-core/src/server/textGeneration.ts:122-145`).
- Backends: by default a coding-agent session (Codex, Claude, Antigravity, Grok, OpenCode drivers in `textGeneration/*TextGeneration.ts`), or **text generators**: OpenAI, Anthropic, OpenRouter, or a custom OpenAI-compatible base URL such as Ollama or LM Studio (`textGeneration/textGeneratorDrivers.ts`, `TextGenerators.ts`). Settings has a per-task model picker with a **Test** button. Review guides always need a coding agent.
- Writing style (Settings → Source control → Commits & pull requests): **Repository conventions** / **Conventional Commits** / **Custom instructions**, plus "Follow pull request templates". PR descriptions also draw on the thread's work and the checks it ran (`settings/SourceControlWritingSettings.tsx:49-69`).

### Preview browser

- **Two backends**: on desktop, Electron webviews with a shared CDP attachment (`apps/desktop/src/preview/{Manager,DesktopBrowserHost,CdpRelay,BrowserSession}.ts`). On web or headless servers, a **server browser**: Playwright driving a pinned, sha256-verified **chrome-headless-shell 158** download, streamed to the client as a screencast at ~30 fps (`server/preview/{ServerBrowser,PreviewBrowser,ServerBrowserStream}.ts`, web `browser/ServerBrowserSurface`).
- **UI**: tabs in the right panel with back/forward/refresh, a URL bar ("Search or enter URL", choice of search engine), find-on-page, zoom, a device toolbar with viewport presets, color-scheme emulation, open in system or environment browser, and an empty state with discovered local servers and recent URLs. "Stop" kills a discovered dev server (`preview.stopLocalServer`). Files: `components/preview/Preview{View,Panel,ChromeRow,MoreMenu,FindBar,EmptyState,LocalServerCard}.tsx`, `browserHistoryStore.ts`, `browserFaviconStore.ts`.
- **Floating / PiP / pop-out**: "Float preview over chat" mini-player with gestures, a separate preview window, and a picture-in-picture frame stream (`ThreadPreviewMiniPlayer.tsx`, `previewMiniPlayerStore.ts`, `apps/desktop/src/preview-pip-preload.ts`). A setting auto-shows the floating preview.
- **Element picking & annotation**: "Annotate elements, regions, and drawings" draws strokes or picks elements, with optional voice input (`PreviewAnnotationSpeech.tsx`). A picked element sends to chat its page URL/title, tag, CSS selector, truncated outerHTML, styles, React component name, and source/stack frames (`desktop/preview/{PickPreload,PickTarget,PickedElementPayload,Annotation*}.ts`).
- **Capture**: screenshot (copy image or path); Shift-click records video with cursor and input overlay, with frame-rate and input settings (`RecordingInput.ts`, `RecordingCursor.ts`, `contracts/previewCapture.ts`).
- **Agent automation** (MCP tools for agents, with ownership agent/human/unclaimed per tab): status, open, navigate, snapshot, click, type, press, scroll, evaluate, waitFor, recordingStart/Stop, resize, setColorScheme, dialog, close, upload, hover, select, drag (`contracts/previewAutomation.ts:20-50`). An agent cursor overlay shows its activity (`AgentBrowserCursor.tsx`). Settings → Browser → Agent access.
- **Port discovery & forwarding**: the server scans loopback listeners (lsof, /proc, Windows tables) and attributes them to threads via an env var the agent inherits (`server/preview/PortScanner.ts`). Remote-environment localhost pages render locally through a **SOCKS5 → WebSocket tunnel** to `/api/preview/tunnel`, keeping their origin, HMR and TLS (`desktop/preview/LoopbackTunnel.ts`, `server/preview/PreviewTunnel.ts`). The "Forwarded ports" palette submenu offers Forward localhost:{port}, Copy address, Open in browser and Stop forwarding (`components/CommandPalettePortForwardItems.tsx`).
- **Profiles & import**: browser profiles (persistent partitions). The one-time cookie import from other browsers is desktop-only: Safari needs Full Disk Access; on Windows only Firefox/Zen/Helium work, because Chromium browsers use app-bound encryption (`desktop/preview/BrowserImport/`, `settings/BrowserImportWizard.tsx`). Passkeys use Touch ID or Windows Hello (`Passkeys*.ts`). On macOS the app can be the **default browser**: external links open as a preview tab or a new project-less thread (`docs/user/default-browser.md`).

### SnapShot (desktop window capture → draft attachment)

- A global shortcut captures the **foreground window** and attaches a PNG to the current draft, creating a draft if needed, then focuses T3. The attachment includes the app name, window title, icon and optional **accessibility tree** (controls, text, positions). Pending captures survive a crash or restart. Off by default. Files: `apps/desktop/src/snapShot/{DesktopSnapShot,ActiveWindow,SnapShotAccessibility*}.ts`, `settings/SnapShotSettings.tsx`, `SnapShotSetupDialog.tsx`, route `settings.snap-shot`.
- **Platforms**:
  - macOS: JXA `osascript` finds the window and `screencapture -l <id>` takes the image. It needs Screen Recording, and Accessibility only when "Include app text" is on.
  - Windows: a helper child process plus `@crowecawcaw/xa11y`.
  - Linux (Wayland only): a bundled GNOME extension, KDE helper, Hyprland and Niri config edits (diff review, backup, reload), or the screenshot portal with a manual picker fallback.
  - Files: `MacSnapShot.ts`, `RegionSnapShot*.ts`, `{Gnome,Kde,Hyprland,Niri,Linux,Portal}*.ts`.
- **Shortcut**: the default is both Shift keys together (macOS/Windows). Modifier pairs or key chords are allowed; shortcuts that collide with app keybindings or OS reservations are refused (`CaptureShortcutConfig.ts`, `GlobalShiftShortcut*.ts`). The capture sound, window flash and fly-in animation can each be toggled.

### Workflows (Labs, off by default)

- These are durable TypeScript scripts in `~/.t3/workflows/` (or `T3CODE_WORKFLOWS`), hot-reloaded. Globals provided: `T3` (`workflow, step, ask, wait, sleep, stop, skip, title, tags, invoke, agent`…), `Effect`, `Schema` and `Option`. Each step result is journaled to SQLite, so a run resumes after restart. Versions can coexist. Built on Effect durable deferreds. Files: `server/workflows/{workflow,WorkflowEngine,WorkflowRuntime,WorkflowStore,triggers,workflowAgent}.ts`.
- **Triggers**: `every`, `daily` (skipped if missed by >10 min), webhooks (a secret path, dedupe on delivery header, 1 MiB body cap, 60 runs/min then 429), and new issues/PRs polled about once a minute (`WorkflowForge.ts`).
- **Agent steps** run in hidden ordinary threads, which can use a worktree, runtime and interaction mode, typed `returns`, or a linked environment. Failure handling: code steps retry at 5/20/80 min; agent failures pause for resume; cancel cascades to children.
- **UI**: the Workflows page (palette "Open workflows") has filters Needs you / Running / All, question kinds, a run feed, chips for linked issues and PRs, a definition editor, and answer/resume/cancel (`components/workflows/*`). CLI: `t3 workflow start|list|show|resume|cancel|questions|signal|hooks|store`. Skipped-run retention defaults to 500.

### Scheduled tasks

- A task is a prompt run on a project with a model, runtime and interaction mode, and a workspace strategy. **Schedules**: `interval` (≥1 min), `fixed_time` (time of day plus weekdays), `once` (ISO time with offset), or `webhook`.
- **Thread modes**: new thread each run / continue a thread / fresh session in a thread.
- **Webhook options**: HMAC-SHA256 signature check (header, hex/base64, prefix), filters (equals/notEquals/startsWith/exists on body/headers/query paths), `{{body.x}}` prompt templates, replay-key dedupe, coalesce key and window, max delivery age (relay holds requests up to 24 h while the environment is offline), a delivery log with bodies, and URL token rotation.
- Settings has run-now, enable/pause, and status (never/running/succeeded/failed, next/last run).
- Files: `contracts/scheduledTask.ts`, `server/scheduledTasks/*`, `server/scheduling/Scheduler.ts`, `settings/ScheduledTasksSettings.tsx`, route `settings.scheduled-tasks`.

### Open in editor

- **Supported editors** (`packages/contracts/src/editor.ts`): Cursor (`cursor --classic`), Trae, Kiro (`kiro ide`), VS Code, VS Code Insiders, VSCodium, Zed, Antigravity, IntelliJ IDEA, Aqua, CLion, DataGrip, DataSpell, GoLand, PhpStorm, PyCharm, Rider, RubyMine, RustRover, WebStorm, and File Manager (reveal in Finder / Explorer / Files).
- **Launch styles**: `goto` (`file:line:col`), `line-column`, and `direct-path`. For remote environments, URL schemes (`vscode://`, `cursor://`, `jetbrains://`) are used for remote-capable editors.
- The preferred editor is remembered per environment (`editorPreferences.ts`, `editorLabels.ts`, `editorMenuIcons.tsx`, `components/JetBrainsIcons.tsx`). The "Open with ▸" menu lists the installed editors.

#### Bugs / fragile UX

- **"Revert files too" is disabled outside isolated worktrees.** `ChatOverlayHosts.tsx:234-236` (the server refuses file restore otherwise). A thread running directly in the project folder can only rewind the chat, and the dialog does not explain why the button is greyed out.
- **A slow workspace can lose its turn baseline.** The snapshot has a 5 s budget, after which "a workspace too slow for this starts without one" (`orchestration-v2/CheckpointService.ts:37-39`). On large repositories that turn's diff and restore may be missing.
- **Large diffs are cut off.** Turn and checkpoint diffs stop at 10 MB with only an appended `[truncated]` marker (`vcs/VcsDriver.ts:43`, `contracts/checkpointDiff.ts:8`).
- **Diff-panel state leaks after archiving.** It is persisted per thread in localStorage and only cleared when a thread is deleted (`hooks/useThreadActions.ts:269-273`). Archived threads keep their entries in `t3code:diff-panel-state:v1` (`diffPanelStore.ts:190`).
- **Turn fallback depends on list order.** `reconcileTurnSelection` replaces a vanished turn with `availableTurnIds[0]`, named "latestTurnId" (`diffPanelStore.ts:160-176`). It only works if callers order turns newest first.
- **Hard split limit.** Terminal splits stop at 4 per group (`types.ts:36`), and the UI only says "(max 4 per group)".
- **Terminal paste can fail silently.** Pasting before the terminal is ready drops the text with only a toast: "Terminal paste interrupted. Paste again…" (`ThreadTerminalDrawer.tsx:1021`).
- **Saved views do not sync.** PR and issue saved views live in the browser or app only (`pullRequestSavedViews.ts`, `issueSavedViews.ts`).
- **Host gaps.** GitCafe PRs have no line comments; Bitbucket cannot reopen declined PRs; Azure comments must be edited on the website (`docs/user/source-control.md`).

---

<!-- ===== Part D ===== -->

## D. Settings, keybindings, app shell, remote, desktop

### Keybindings

- **Config file** — JSON array of `{ key, command, when? }` at `<baseDir>/userdata/keybindings.json` (baseDir defaults to `~/.t3`; `T3CODE_HOME` / `--base-dir`; dev server uses `<baseDir>/dev/`). Lives on the *server/environment* machine, not the client. Max 256 rules, key ≤64 chars, `when` ≤256 chars, nesting ≤64. `apps/server/src/config.ts:162`, `packages/contracts/src/keybindings.ts`.
- **Lifecycle** — On first start the server writes all defaults. On later starts it backfills defaults for commands not present in the file, *prepending* them so user rules win; a default whose exact key+when is already taken by another command is skipped (logged only). One-shot "late default" migrations (e.g. `mod+enter` → `composer.sendBackground` on drafts) are recorded so they apply once. File is watched (`fs.watch`, 100 ms debounce) and changes are pushed live to clients. Malformed file → defaults used; invalid entries dropped individually. `apps/server/src/keybindings.ts:97-115,470-610`.
- **Key syntax** — `+`-joined tokens, case-insensitive: modifiers `mod` (Cmd on macOS, Ctrl elsewhere), `cmd`/`meta`, `ctrl`/`control`, `alt`/`option`, `shift`; key tokens incl. `space`, `esc`, `f6`, `arrowup`, `[`, `=`, literal `+` via `mod++`. Matching is keyboard-layout aware (event-code map, US shifted→unshifted table). `packages/shared/src/keybindings.ts:26-210`, `packages/shared/src/keybindingMatching.ts`.
- **`when` language** — identifiers, `!`, `&&`, `||`, parentheses; `&&` binds tighter than `||`; literals `true`/`false`. Unknown identifiers evaluate to false. Context keys: `terminalFocus`, `terminalOpen`, `previewFocus`, `previewOpen`, `rightPanelOpen`, `modelPickerOpen`, `usagePageOpen`, `composerFocus`, `draftThreadRoute`, `turnRunning`, `composerEmpty`, `editableFocus`, `isWeb`, `isDesktop`. Parser `packages/shared/src/keybindings.ts:211-363`; evaluator `apps/web/src/keybindings.ts:86-106`.
- **Precedence** — rules scanned last→first; the last rule whose key and `when` match wins, regardless of command. `apps/web/src/keybindings.ts:194-221`. Special: when Send shortcut requires a modifier, a persisted plain `mod+enter` for sendAlternate/sendBackground is resolved as `mod+shift+enter` at runtime (`apps/web/src/keybindings.ts:125-145`).
- **Commands** — static list + project scripts `script.<id>.run` (id `[a-z0-9][a-z0-9-]*`, ≤24). Unbound-by-default commands: `thread.stop`, `thread.regenerateTitle`, `composer.cycleHost`, `composer.increaseEffort`, `composer.decreaseEffort`, `rightPanel.toggleMaximized`. `packages/contracts/src/keybindings.ts:37-125`.
- **Settings → Keybindings UI** — grouped (Navigation, Threads, Composer, Terminal, Preview & diff, Appearance, Project scripts, Other); record shortcut, edit `when`, **Unassign** (writes the rule with `when: "false"`), **Reset to default**; inline "Conflicts with X" warning when same resolved chord and identical/empty `when`. `apps/web/src/components/settings/KeybindingsSettings.tsx`, `KeybindingsSettings.logic.ts:219-300`. Server RPCs upsert/remove (with replace target matched by parsed identity, so `cmd+k`≡`meta+k`).
- **Non-configurable keys** — diff find `mod+f` inside a diff; F2 rename thread; Shift+F10/ContextMenu thread menu; Up/Down/Enter/Left/Right sidebar tree nav; Esc closes palette child search; desktop quit Cmd/Ctrl+Q (Hold 1.2 s or double-press within 500 ms; modes direct/hold/double-click, `confirmQuit`). `docs/user/keybindings.md`, `apps/desktop/src/window/QuitHold.ts`, `apps/web/src/components/QuitHoldOverlay.tsx`.
- **Workspace focus (F6)** — cycles Threads sidebar → conversation → composer → visible panels, wraps, restores last-focused control per area; in Settings cycles nav ↔ section. `docs/user/keyboard-focus.md`.

#### Default bindings (`packages/shared/src/keybindings.ts:58-151`)

| Key | Command | when |
|---|---|---|
| mod+b | sidebar.toggle | — (never takes Ctrl chords from focused terminal) |
| f6 / mod+f6 | workspace.focusNext | — |
| shift+f6 / mod+shift+f6 | workspace.focusPrevious | — |
| mod+[ / mod+] | navigation.back / navigation.forward | !terminalFocus |
| mod+j | terminal.toggle | — |
| mod+alt+b | rightPanel.toggle | — |
| mod+shift+b | threadPanel.toggle | !terminalFocus |
| mod+t | rightPanel.new | rightPanelOpen && !terminalFocus |
| mod+d | terminal.split | terminalFocus |
| mod+shift+d | terminal.splitVertical | terminalFocus |
| mod+shift+d | composer.dictation | !terminalFocus |
| mod+n | terminal.new | terminalFocus |
| mod+w | terminal.close | terminalFocus |
| mod+w | rightPanel.close | !terminalFocus |
| mod+shift+t | view.reopenClosed | — |
| mod+d | diff.toggle | !terminalFocus |
| mod+shift+j | preview.toggle | — |
| mod+r | preview.refresh | previewFocus |
| mod+l | preview.focusUrl | previewFocus |
| mod+= / mod++ | preview.zoomIn | previewFocus |
| mod+- / mod+0 | preview.zoomOut / preview.resetZoom | previewFocus |
| mod+f | preview.find | previewFocus |
| mod+k | commandPalette.toggle | !terminalFocus |
| mod+p | filePicker.toggle | !terminalFocus |
| mod+shift+f | projectSearch.toggle | !terminalFocus |
| mod+u | usage.open | !terminalFocus |
| mod+alt+a | theme.select | !terminalFocus |
| mod+alt+shift+a | appearance.cycle | !terminalFocus |
| mod+alt+shift+t | themeEditor.toggle | — |
| mod+s | composer.stash | !terminalFocus |
| mod+shift+enter | thread.steerQueuedMessage | !terminalFocus |
| alt+arrowup | thread.editQueuedMessage | composerFocus |
| mod+enter | composer.sendAlternate (opposite queue/steer) | composerFocus && turnRunning |
| mod+enter / mod+alt+enter | composer.sendBackground | composerFocus && draftThreadRoute |
| mod+alt+enter | composer.sendAndNewThread | composerFocus && !draftThreadRoute |
| mod+n / mod+shift+o | chat.new | !terminalFocus |
| mod+shift+n | chat.newLocal | !terminalFocus |
| mod+alt+n | chat.newWithoutProject | !terminalFocus |
| mod+f | chat.find | !terminalFocus && !previewFocus |
| mod+shift+m | modelPicker.toggle | !terminalFocus |
| mod+shift+h / e / a | composer.host / composer.effort / composer.mode | !terminalFocus |
| mod+shift+x / g / l | composer.workspace / composer.branch / composer.previousWorktree | !terminalFocus |
| mod+shift+k | pullRequest.copyNumber | !terminalFocus |
| mod+shift+arrowup / arrowdown | modelPicker.previousProvider / nextProvider | modelPickerOpen |
| mod+o | editor.openFavorite | — |
| mod+shift+[ / mod+shift+] | thread.previous / thread.next | — |
| mod+shift+c | thread.copyReference (copies PR URL when PR open) | !terminalFocus |
| mod+shift+s | thread.settle | !terminalFocus |
| mod+shift+p | thread.pin | !terminalFocus |
| mod+z | thread.undo (sidebar undo notice, 5 s) | !terminalFocus && !editableFocus |
| mod+1..9 | thread.jump.1..9 | !modelPickerOpen && isDesktop |
| mod+1..9 | modelPicker.jump.1..9 | modelPickerOpen && isDesktop |
| c / t / l | usage.cost / usage.tokens / usage.limits | usagePageOpen |
| mod+shift+1..4 | usage.period.day / week / month / quarter | usagePageOpen |

### Command palette

- **Modes** — `commandPalette.toggle` (root: commands, projects, threads), `filePicker.toggle` (Go to file), `projectSearch.toggle` (search project contents); each shortcut toggles its mode; Esc in a child mode returns to root. Query starting with `>` shows actions only. Number keys 1–9 pick entries while open. Placeholder modes: root, root-browse (type a path), submenu, submenu-browse. `apps/web/src/shortcuts/commandPaletteCommands.ts`, `apps/web/src/components/CommandPalette.logic.ts:811-846,922`, `apps/web/src/commandPaletteStore.ts`.
- **Groups** — *Actions*, *Recent Threads* (sorted by last written/opened), plus project/thread search results, *Settings* items ("Settings · {section}", from `settingsSearch.ts`), submenus *Projects*, *Environments*, *Sources*, *Change theme*, *Forwarded ports*, *Options*. `CommandPalette.logic.ts:811-830`.
- **Actions** (`apps/web/src/components/CommandPalette.tsx:1356-1880`): New thread; New thread in… (project submenu); New thread without a project; Triage an issue; Find in current thread; Find on page in browser; port-forward item (Forward localhost:N / Forward port N from env / Forwarded ports → Open in browser, Copy address, Stop forwarding — `CommandPalettePortForwardItems.tsx`); Copy prompt link / Copy prompt link with repo and model; Copy thread reference; Link pull request to thread; Show linked items; Share with guest; Restart agent session (confirm); Go to file; Search project contents; New project (clone/existing, env picker, "Select where to clone"); Add project (local folder / connect environment); Open WSL folder; Open pull requests; Open issues; Use T3 Code in another app (MCP); Open workflows; Open usage; Focus Next/Previous Workspace Area; Open settings; Pin/Unpin project; Project settings; Change theme / Change appearance (`CommandPaletteThemeItems.tsx`); unarchive project.

### Settings

- **Storage & scope** — *Client* settings: per device (web `localStorage` key `t3code:client-settings:v1`, desktop via IPC `desktop:get/set-client-settings`). *Server* settings: `<baseDir>/userdata/settings.json` on each environment; settings UI has an environment/project scope picker with inheritance and "reset to default" per row (`SettingsScopeContext.tsx`, `SettingInheritance.tsx`). Managed policy overlays defaults/locks at read time (`apps/server/src/serverSettings.ts:398-450`). Schemas: `packages/contracts/src/settings.ts:416-623` (client), `1393-1647` (server).
- **Pages** (`apps/web/src/routes/_chat.settings.*.tsx`, nav in `SettingsSidebarNav.tsx:95-123`): General, Appearance, Chat, Diffs, Threads, Projects, Keybindings, Voice, Snap Shot, Providers, Browser (old `/integrations`), Devices, Scheduled tasks, Source control, Worktrees, Environments (`/connections`), Sharing & access (`/access`), Secrets (preview, only if an env supports it), Maintenance (old `/storage`; contains Diagnostics), Labs (old `/beta`), About (contains Open source licenses), Archived threads. Settings search feeds the palette.
- **General/Chat/Threads/Diffs rows** (`SettingsPanels.tsx:846-2380`): Notifications; In-app notifications; Language; Timestamp format; Sidebar (project grouping, project order, Working section); Confirmations (archive, delete, unpin, terminal close, Quit shortcut behavior); Display/Composer (rich text composer, send shortcut, follow-up behavior, show skills in slash menu, tool calls in chat, response streaming); Subagents and side chats open in; proactive PR/changes panels; Screen reader (announcements, window title); Diffs (colors, layout, whitespace, default file state); Pull requests; New projects (base directory, ghq host-folder clone layout); Snooze presets; Usage limits (snooze/resume limited threads); Auto-settle (inactive after N days, on merge, keep threads with PR); Compact idle threads; Text generation.
- **Client keys (defaults)**: `notificationMode` off (off|notifications|sound|notifications-and-sound), `inAppNotificationsEnabled` false, `language` "en" (or "system"), `timestampFormat` locale, `confirmQuit` hold, `confirmThreadArchive` false, `confirmTerminalClose` true, `confirmThreadDelete` true, `confirmThreadUnpin` false; display: `chatWidth` comfortable, `htmlPreviewWidth` chat, `effortDisplay` label, `appearanceContrast` 100, `glassOpacity` 80 (40–100), `panelAnimationDurationMs` 0 (≤400), `environmentIdentificationMode` artwork, `fontSizeInterface` 16, `fontSizePrompt` 14, `fontSizeCode` 13, `fontSizeTerminal` 12, `fontFamilyCode/Composer/Sans/Terminal` "", `fontSmoothing` true, `wordWrap` true, `mathRenderingEnabled` false, `persistComposerContextStrip` false; diffs: `diffColorScheme` red-green, `diffLayout` stacked, `diffFilesCollapsed` true, `diffIgnoreWhitespace` true; composer: `composerRichTextEnabled` true, `sendShortcut` enter (enter|mod-enter-multiline|mod-enter), `followUpBehavior` steer, `showSkillsInSlashMenu` true, `toolCallVisibility` showAll, `favorites`, `providerModelPreferences`; sidebar: `sidebarWorkingShelfEnabled` true, `sidebarProjectGroupingMode` repository (+ per-project overrides), `sidebarProjectSortOrder`/`sidebarThreadSortOrder` updated_at, `boringSidebarEnabled`, `boringSidebarMultilineRows`, `sideThreadOpenTarget` panel, `proactivePanelsEnabled` false, `threadDetailsItems`, `threadDetailsUsageEnabled` (retired); a11y: `screenReaderAnnouncements` status, `screenReaderWindowTitle` false; browser: `browserDefaultViewport` fill, `browserDefaultZoomFactor`, `browserDefaultAppearance`, `browserRecordingFrameRate`, `browserRecordingShowKeyPresses/MousePresses` false, `browserLinkTarget` system, `browserSearchEngine` duckduckgo, `browserCustomSearchUrl`, `browserAutoShowFloatingPreview` true, `browserProfiles`, `browserDefaultProfileId`; PR: `pullRequestMergeMethodOverrides`, `pullRequestLinksOpenInApp` true; load balancing: `loadBalancingEnabled` false, `loadBalancingWeights`; voice: `voiceMicrophones` (ordered), `voiceShortcutMode` auto, `speechLanguage` auto, `speechTranslateToEnglish` false, `speechCustomWords`, `speechSilenceFiltering`/`speechStreaming`/`speechRemoveFillerWords` true, `speechPostProcessingPrompt`, `speechCorrectionWord`; Snap Shot: `snapShotEnabled` false, `snapShotIncludeAccessibility`, `snapShotShortcut`, `snapShotPlaySound`, `snapShotSound`, `snapShotFlash`, `snapShotAnimations`; `onboardingCompletedAt`.
- **Server keys (defaults)**: models — `defaultModelSelection`, `fallbackModelSelection`, `textGenerationModelSelection` (codex `gpt-6-luna`), `favorites`, `providerModelPreferences`, `defaultRuntimeMode` full-access; providers — `providerInstances`, `textGenerators`, `enableProviderUpdateChecks` true, `providerHealthRefreshInterval` 5 min; threads — `responseStreamingMode` paragraph (turn|paragraph), `continueThreadsAfterServerUpdate` true, `keepAwakeWhileAgentsWork` true, `sidebarAutoSettleAfterDays` 7, `sidebarAutoSettleOnMerge` true, `sidebarAutoSettleKeepPullRequests` false, `compactIdleThreads`/`idleCompaction`, `snoozeLimitedThreads` false, `autoResumeLimitedThreads` false, `snoozePresets` (+1h, +3h, today 18:00, tomorrow 09:00, Monday 09:00), `hiddenThreadTags`, `hideTaggedThreads` false, `threadTitleInstructions`; projects/worktrees — `worktreesDirectory`, `previousWorktreesDirectories`, `fastWorktreeCheckout` false (macOS), `worktreeCleanup`, `storageCleanup{worktreeKeepWhen uncommitted-changes, worktreeAfterDays, worktreeSettledAfterDays, worktreeOnMerge, worktreeOnDelete, worktreeUnchanged, browserArtifactsAfterDays, logsAfterDays}`, `defaultThreadEnvMode`, `newWorktreesStartFromOrigin` true, `worktreeSubmodules`, `addProjectBaseDirectory`, `ghqCloneLayout` false, `defaultAutoPull` false, `defaultProjectScripts`, `projectScriptOverrides`, `projectAutoPullOverrides`, `projectSettingsOverrides`; source control — `branchNamingMode` static, `branchNamePrefix` "t3", `branchNameInstructions`, `removeAgentCreditsOnMerge`, `sourceControlWritingStyle`, `sourceControlWriterModelSelection`, `reviewGuidesEnabled`, `reviewGuideModelSelection`, `reviewGuideInstructions`, `pullRequestMergeMethod`, `sourceControlHosts`, `offeredSourceControlProviders`, `automaticGitFetchInterval` 30 s; access — `requireConnectDeviceApproval` false, `enableAgentBrowserAccess` true (+ per-project overrides), `enablePreviewTunnel` true, `enableAgentDeviceAccess` false, `enableDeviceSupport` false, `deviceHosts`/`easDeviceHosts`/`simlockDeviceHosts`(+`simlockDeviceHostsEnabled`); speech — `speechModelId` parakeet-0.6b-gguf, `speechAcceleration` auto, `speechModelUnloadTimeout` 15 min, `cloudSpeechCredentials{openai,xai,openrouter}`, `speechProjectCustomWords`, `speechPostProcessingModelSelection`; Labs/beta — `guestSharingEnabled`, `linkedEnvironmentsEnabled`, `workflowsEnabled`, `workflowSkippedRunsKept` 500, `promptSuggestionsEnabled`, `agentFeedback.mode` off (off|on-request|auto); appearance — `defaultTheme`, `defaultAppearance` (+`*SetAt` nonces), `environmentIcon`; background — `backgroundActivityProfile` balanced (balanced|performance|battery-saver|custom) + `backgroundActivity.overrides` (git fetch, health refresh, power monitor intervals, pause when locked/low-power/battery); usage — `usageLimitSources`, `usageHistoryRetentionDays` 92 (or "unlimited"), `usagePriceOverrides`, `usageModelAliases`, `usageModelFilter`; `secretProviders`; `observability{otlpTracesUrl,otlpMetricsUrl,otlpLogsUrl}`.
- **Desktop-only settings file** — `localEnvironmentEnabled`, `linuxPasswordStore`, window bounds/maximized, `serverExposureMode` (local-only|network-accessible), `tailscaleServeEnabled`/`Port`, `updateChannel` (latest|nightly), `wslBackendEnabled`, `wslMode`, `wslDistro`, `wslOnly`. `apps/desktop/src/settings/DesktopAppSettings.ts:98-112`.
- **Managed policy** — admin JSON (`schemaVersion`, `organization`, `supportUrl`, `settings{key:{value,locked}}`, `limits{maxRuntimeMode, clients[desktop|local-browser|remote], connect, webhookTasks, browserCookieImport, previewTunnel, worktreeLayoutCommands, telemetry, providers{allowedDrivers, instances{locked, environment, binaryPath, launchArgs}}, updates{mode, feedUrl, allowNightly}}`). Locations: macOS profile `com.t3tools.t3code` or `/Library/Application Support/T3Code/policy.json`; Windows `HKLM\SOFTWARE\Policies\T3Tools\T3Code` (ADMX or `Policy` REG_SZ) or `C:\Program Files\T3Code\policy.json`; Linux `/etc/t3code/policy.json`. Read only at server start; refuses to start on any unparseable/unknown key or two sources; locked rows show "Managed by <org>". CLI `t3 policy check|show|templates`. `docs/user/managed-policy.md`, `apps/server/src/policy/ManagedPolicy.ts`, `apps/desktop/src/app/DesktopManagedPolicy.ts`.

### Appearance, themes, i18n

- **Themes** — built-ins `t3-chat`, `grove`, `ocean`, `ember`, `iris` (`packages/shared/src/themePalettes.ts`); appearance mode system/light/dark; separate theme per light/dark half; selection stored per device in localStorage (`t3code:themes:v1`, `theme-halves`, `theme-appearance-mode`). `apps/web/src/themePalette.ts:47-52`.
- **Theme editor** — palette of ~69 color roles (canvas, chrome, toolbar…), "vivid" generation from canvas+accent, eyedropper-style area picker, Custom CSS (sanitised: no `@import`, `url()` except `data:`/`#id`), JSON export/import, import VS Code theme JSON (`vscodeThemeImport.ts`) or search Open VSX marketplace (`openVsxThemes.ts`, `ThemeSearchSection.tsx`). `components/settings/ThemeEditorPanel.tsx`, `ThemeCssEditor.tsx`, `themeCss.ts`.
- **Environment themes** — server publishes `~/.t3/userdata/themes/<id>.json` (full or short `{name,appearance,canvas,accent,colors,css}`) + optional background image `<id>.jpg|png|webp` ≤16 MB exposed as `--theme-background-image`; `t3 theme set|clear|show`; clients apply a server default once per `defaultThemeSetAt`. Agents in full-access can publish themes. `docs/user/appearance.md`, `apps/server/src/environmentTheme.ts`, `apps/web/src/themeBackground.ts`.
- **Other appearance** — contrast, glass opacity, chat width, HTML preview width, effort display, environment identification (artwork), font family pickers + sizes (`appearanceFonts.ts`, `FontFamilyPicker.tsx`), panel animation slider, render math (KaTeX), keep composer context visible. `AppearanceSettingsPanel.tsx`.
- **i18n** — gt-react; locales `en` (default) + `de, es, fr, ja, ko, pt-BR, ru, sv, zh` (`apps/web/gt.config.json`), "system" follows browser/OS preferred languages (desktop IPC `get-system-locale`, `get-preferred-system-languages`). Desktop menus translated separately (`apps/desktop/src/i18n`). `apps/web/src/i18n/locales.ts`.

### Notifications

- **Desktop/web** — `startThreadNotifications` watches every connected environment; kinds `input` (approval/question needs attention, deduped per request id) and `completion` (settled turn; background wake completions wait a grace period). Suppressed for archived, snoozed, quiet runs, first sighting, hidden-tag threads. Outputs per `notificationMode`: OS `Notification` (click focuses window and opens thread; tag per env+thread), sound (`assets/notification-completion.mp3`, `notification-input.mp3` via WebAudio, unlocked on first input), in-app toasts (closed when thread opened). Badge = count of pending OS notifications: favicon canvas badge on web, `desktop:set-notification-badge` (dock badge; image overlay on Windows). Cleared on window focus. `apps/web/src/components/ThreadNotificationCoordinator.tsx`, `.logic.ts`, `apps/web/src/threadNotifications.ts`, `components/settings/NotificationSettings.tsx`.
- **Mobile push** — via T3 Connect only; requires host's **Publish agent activity** (`apps/server/src/relay/AgentAwarenessRelay.ts`); alerts for finish/fail/approval/input, iOS Live Activities + widget, Android Live Updates/ongoing notification. `docs/user/mobile-notifications.md`.

### Usage & limits

- **Usage page** (`/usage`, `mod+u`) — aggregates history from Codex, Claude Code, Grok, OpenCode, Antigravity, Pi, Muse, Cursor across environments: tokens, cache savings, model breakdown, API-equivalent cost (Premium for fast tiers, Other), periods day/week/month/quarter, env filter with per-env scanning state, views Cost/Tokens/Limits (keys c/t/l). `apps/web/src/components/usage/UsagePage.tsx`, `apps/server/src/usage/UsageService.ts`, `usageTranscriptReader.ts`, `usagePricing.ts`, `usageScanCache.ts`.
- **Model prices** — per-env price overrides (input/output/cache read/write per M tokens), "Map to" aliases, multi-destination save with per-env result and retry. `UsagePriceOverrides.tsx`, `UsageModelDialog.tsx`.
- **Limits** — pools subscription accounts per provider; windows session/weekly/monthly/other with segmented bars, reset times, banked "Use reset" credits (Claude/Codex), auto-check ≥5 min apart; external sources (`cliproxy`, `http` hubs) via Add usage limit source. `/usage-limits` composer command; thread-details limit rows + context window bar. `components/usage/UsageLimitsPooled.tsx`, `apps/server/src/usage/UsageLimitSources.ts`, `packages/contracts/src/providerUsageLimits.ts`, `components/settings/AddUsageLimitSourceDialog.tsx`.

### Remote access, auth, sharing

- **T3 Connect** — account sign-in (Clerk) + Cloudflare-tunnel relay (outbound port 7844) giving each environment a hosted address; `t3 up [--connect|--lan]`, `t3 connect`, `t3 status`; device-code browser approval for headless CLI; free 3 machines, more with linked T3 Chat plan. Optional **Require approval for new devices** (code like `K7QM-2XFD`, 10 min expiry, `t3 auth connect-device list|approve|revoke`). Troubleshooting diagnosis UI (tunnel offline, port blocked, DNS, clock skew, old host). `apps/server/src/cloud/*` (CloudLink, ConnectReachability, managedTunnelStartup), `components/settings/ConnectDeviceApprovalSettings.tsx`, `ConnectionDiagnosisDetails.tsx`, `routes/connect.tsx` (CLI authorize), `docs/user/t3-connect-troubleshooting.md`.
- **Direct pairing** — enable Network access (desktop restarts) or `t3 serve --lan`; one-time pairing link/QR (`/pair` route, `t3code://pair` app link with confirm); `t3 pair [--tailscale] [--scope ...]`; sessions expire after 30 days unused; Authorized clients list with revoke; scoped bearer tokens (`t3 auth pairing create`, `t3 auth session issue|list|revoke`). `apps/server/src/auth/*` (PairingGrantStore, SessionStore, dpop.ts, RpcAuthorization), `components/settings/AuthorizedClientsSettings.tsx`, `routes/pair.tsx`.
- **Routes & environments map** — each saved machine has ordered routes (LAN, Tailscale, VPN, public URL, SSH, Connect); first answering wins, earlier routes re-probed on network change/every minute; learned LAN/Tailscale routes; per-route "Permissions" view; map/list view with links, stacks, search. Load balancing across machines (Prefer / Normal / Less often / Manual only). `components/settings/ConnectionsSettings.tsx`, `EnvironmentsMap.tsx`, `EnvironmentRoutesList.tsx`, `LoadBalancingSettings.tsx`, `AddEnvironmentDialog.tsx`.
- **Tailscale HTTPS** — toggle in Sharing & access / `t3 serve --tailscale-serve` → `https://machine.tailnet.ts.net/`; detects `100.64/10` vs other VPNs. `packages/tailscale/src/tailscale.ts`, `apps/desktop/src/backend/tailscaleEndpointProvider.ts`.
- **Linked environments (beta)** — machine↔machine one-way links with access level; agents delegate tasks (`delegate_task`), read-only streamed view of linked threads (`/linked/...` route), link several, link back, Forget. `apps/server/src/peer/*`, `components/settings/LinkedEnvironmentsSettings.tsx`, `LinkSeveralEnvironments.tsx`, `LinkAccessPicker.tsx`.
- **Outside agents** — MCP server + OAuth for Claude Code/Codex/ChatGPT etc. (`/connect-agent`, `/approve-agent` routes; `apps/server/src/auth/McpOAuth.ts`, `apps/server/src/mcp`).
- **Guest links (beta)** — share one thread with a no-account browser guest via T3 Connect address; 24 h, one browser; guest messages/attachments queue as **Request** until Send/Dismiss; Stop sharing. `apps/server/src/guest/GuestShareService.ts`, `routes/guest.tsx`, `docs/user/guest-links.md`.
- **Preview tunnel / port forwarding** — clients reach host loopback ports via `/api/preview/tunnel` (`enablePreviewTunnel`); palette port-forward; desktop `desktop:port-forward-*`.
- **Secrets** — 1Password provider via `op` CLI (desktop sign-in, service account, or Connect server with `OP_CONNECT_HOST/TOKEN`); credential fields have source picker Value vs secret reference; agents request secrets through a private card. `apps/server/src/secrets/*`, `components/settings/SecretsSettings.tsx`, `SecretSourceInput.tsx`, `docs/user/secrets*.md`.
- **Background service** — `t3 service install|uninstall|status|restart|protect`; systemd user service (lingering) / launchd / Windows; `service.env`; LAN opt-in `t3 up --lan/--no-lan`. `apps/server/src/serviceLauncher.ts`, `apps/server/src/cloud/bootService.ts`, `docs/user/background-service.md`.
- **Devices** — Device panel streaming iOS Simulator/Android Emulator (local, SSH device hosts, EAS, Simlock alpha), agent device access, toolchain auto-updates. `apps/server/src/device/*`, `components/settings/DeviceSourcesSettings.tsx`, `docs/user/devices.md`.

### Telemetry, updates, diagnostics, maintenance

- **Telemetry** — server product events to `ingest.t3.codes` (→ PostHog) with hashed install/account ids; opt out with `T3CODE_TELEMETRY_ENABLED=false` (desktop reads it from login shell); relay traces to Axiom; mobile crash reports toggle; agent feedback modes in Labs; user OTLP export per env (traces/metrics/logs URLs, Test, `T3CODE_OTLP_*`, `OTEL_EXPORTER_OTLP_*`). `apps/server/src/telemetry/AnalyticsService.ts`, `apps/desktop/src/telemetry/DesktopTelemetryPublisher.ts`, `components/settings/TelemetryExportSettings.tsx`, `docs/user/telemetry.md`.
- **Updates** — sidebar update button updates providers → servers → desktop app in one flow with progress/retry/countdown/Later; per-server actions Update server / Update the desktop app / Copy update command / Copy relaunch command; protocol mismatch notice; desktop channels Stable (`latest`) / Nightly ("Update track" in About); `t3 update <version>`; policy feed mirror. `apps/web/src/components/ServerUpdateAction.tsx`, `components/sidebar/DesktopUpdateStatusIcon.tsx`, `state/desktopUpdate.ts`, `apps/desktop/src/updates/*` (updateMachine, DesktopRemoteUpdates, updateChannels), `apps/server/src/desktopUpdate/*`, `apps/server/src/cloud/selfUpdate.ts`.
- **Maintenance page** — Updates (check provider versions, continue threads after restarts), Background activity profile + keep awake, storage cleanup (worktrees, rotated logs, browser artifacts; Delete now; last-run stats), Diagnostics link. **Diagnostics** — live process tree with CPU/memory, send signals, resource history, desktop resource monitor (thermal, I/O), open logs folder, trace ID copy. `SettingsPanels.tsx:2379-2525`, `StorageSettings.tsx`, `DiagnosticsSettings.tsx`, `ResourceTelemetryDiagnostics.tsx`, `apps/server/src/storageCleanup.ts`, `apps/server/src/diagnostics`.
- **About** — version (copy), organization (policy), mobile app setup, privacy, open-source licenses.

### Desktop (Electron)

- **Shell** — custom protocol `t3code://app` (dev `t3code-dev://app`) serves the web UI; single-instance with `second-instance`/`open-url` deep links (pair links), Linux AppImage URL-handler registration; native app menu (About, Check for Updates, Settings…, Edit incl. Paste as Text, View zoom/reload/devtools/fullscreen, Window); no tray found. Closing last window stops the local server. Window bounds/maximized persisted. `apps/desktop/src/electron/ElectronProtocol.ts`, `app/DesktopClerk.ts:120-140`, `app/DesktopLinuxUrlHandler.ts`, `window/DesktopApplicationMenu.ts`, `window/DesktopWindow.ts`.
- **Local backend** — spawns/recovers the server (backend pool, recovery worker), local env auth bearer token, exposure mode local-only/network-accessible, advertised endpoints; login-shell env capture with retries (PATH for provider CLIs). `apps/desktop/src/backend/*`, `shell/DesktopShellEnvironment.ts`.
- **IPC surface** (`apps/desktop/src/ipc/channels.ts`, bridge type `packages/contracts/src/ipc.ts:1111+`): folder/file/theme/favicon pickers, path translation to backend (WSL), notification badge, native theme, context menus, open external, read media, system settings (macOS Full Disk Access), probe remote editors, paste-as-text, menu actions, fullscreen sync, trackpad scroll, quit shortcut; app activation (`t3 app` CLI via local socket); updates (state/check/download/install/channel); CLI install/uninstall/add-to-PATH; branding, system locale, relay-telemetry flag; local environment enable/bootstraps/bearer token; client settings; Snap Shot (global capture shortcut, permissions, pending shots, animations); connection catalog; SSH (discover hosts from ssh config, resolve, ensure/disconnect env, bearer session, password prompts); server exposure + Tailscale serve; WSL (state, backend enabled, distro, WSL-only); embedded browser preview (~45 channels: tabs, navigation, zoom, find, devtools, cookies/cache, cookie import from other browsers, element picker, screenshots, annotations, PiP, recordings, tunnel access requests); port forwarding; macOS permission helper (screen recording, accessibility); Apple on-device speech (available/prepare/transcribe); provider auth callback (Codex OAuth); legacy localStorage take/complete; restart audio service; native file drag.
- **SSH remote hosts** — Add environment → SSH alias; installs server runtime to `~/.t3/runtime` on Linux/Apple-Silicon host, port-forwards, reuses ControlMaster sessions, password prompt dialogs. `apps/desktop/src/ssh/DesktopSshEnvironment.ts`, `packages/ssh/src/{tunnel,auth,config}.ts`.
- **WSL** — run agents/projects in a chosen distro, optional WSL-only, auto-installs server in distro, Open WSL folder. `apps/desktop/src/wsl/*`.
- **Misc** — Linux safeStorage backend forcing (gnome-libsecret/kwallet by `XDG_CURRENT_DESKTOP`) `linuxSecretStorage.ts`; merge of v1 "T3 Code (Alpha)" localStorage (prompt stash etc.) `legacyLocalStorageMerge.ts`; Quit-hold overlay; Snap Shot screen capture per platform (mac, GNOME, KDE, Hyprland, Niri, portal, Windows) `apps/desktop/src/snapShot/*`; passkeys in embedded browser `preview/Passkeys.ts`.

### Mobile

- Expo 58 / React Native 0.88 app (`apps/mobile`) that is a remote-only client: pairs to environments (QR/pairing URL, T3 Connect sign-in, Tailscale/LAN routes), lists projects/threads, chat composer with voice transcription, diffs/review, terminal, files, PRs/issues, triage, workflows, usage, devices, browser via preview tunnel, its own themes (incl. Android Material You), push notifications/Live Activities/widgets via T3 Connect, hardware-keyboard shortcuts on iPad/Android; native modules in `apps/mobile/modules/*` (t3-terminal, t3-composer-editor, t3-agent-notifications, t3-review-diff, t3-voice-transcription, t3-preview-tunnel, t3-subscription-widget…).

#### Bugs / fragile UX

- `when` typo silently changes meaning: unknown identifiers evaluate to false (`apps/web/src/keybindings.ts:91`), so `!terminalFocsu` is always true and the rule fires everywhere.
- Settings conflict warning only compares identical or empty `when` strings (`apps/web/src/components/settings/KeybindingsSettings.logic.ts:219-222`); overlapping-but-different conditions (e.g. `!terminalFocus` vs `composerFocus`) are not flagged.
- Startup default backfill is skipped entirely if the file has any invalid entry (`apps/server/src/keybindings.ts:492-500`), and defaults skipped for a shortcut conflict are only logged (`:543`). The `keybindings.*` `ServerConfigIssue` kinds (`packages/contracts/src/server.ts:34-48`) had no consumer in `apps/web/src` when I searched, so users probably never see these.
- Over-limit rule sets are trimmed with `slice(-256)` (`packages/shared/src/keybindings.ts:390,412`), which silently drops the oldest rules (the backfilled defaults).
- Default `defaultRuntimeMode` is `full-access` (`packages/contracts/src/providerPolicy.ts:32`). That is an aggressive default for new installs.
- Desktop/web notification kinds are only `input` and `completion` (`ThreadNotificationCoordinator.logic.ts:6`), so there is no distinct failed-turn alert. Mobile push does have one.
- Pairing links made in Settings can be copied only while that Sharing & access page stays open (documented in `docs/user/remote-access.md`). Leaving or reloading loses them.

---

---

# 2. Server <-> client protocol, persistence, client state

## T3 Code: protocol, persistence, client state, performance

Source: shallow clone at `~/nsh-research/t3code`. All paths are relative to the repo root. `file:line` citations point at the code as read. **V2** means "orchestration v2". It is the only live orchestration path; `legacy/` and V1 tables exist only so old data can be imported.

---

### 0. TL;DR for a Rust reimplementation

| Concern | What T3 does | Rust equivalent |
|---|---|---|
| Transport | One WebSocket at `GET /ws`, carrying **Effect RPC** with **JSON** serialization (`RpcSerialization.layerJson`), plus REST/HTTP for cold snapshots and history pages | axum/tokio-tungstenite plus a hand-rolled JSON RPC envelope that matches Effect's `_tag` messages (or your own) |
| Framing | One JSON object per WS text message. Optional splitting into 48 KiB parts with a `\u001e` prefix. App-level heartbeat `{"_tag":"Pong"}` every 20 s. Per-stream ACK flow control | same |
| Auth | Upgrade query `?wsTicket=<5-min ticket>&orchestrationProtocol=2` (+DPoP proof), or a cookie/bearer. Each RPC method is then scope-checked | same |
| Domain | Event-sourced: commands → events (each event is a **full-entity snapshot**, not a delta) → SQL projection rows (`payload_json` + index columns), committed in one tx with receipts and outbox | sqlx/rusqlite with WAL |
| Streams | `subscribeShell` (sidebar rows) and `subscribeThread` (one thread's projection + events), both with `afterSequence` resume and a `{kind:"synchronized"}` marker | tokio broadcast + DB catch-up |
| Live text | Provider deltas → adapter text coalescer (50 ms) → "paragraph mode" filter (markdown boundaries, ≥400 ms, 800 ms idle flush) → event store → per-subscriber 50 ms coalescer | same pipeline |
| Checkpoints | Hidden git commits at `refs/t3/orchestration-v2/checkpoints/<scopeKey>/ordinal/<n>`, built with a temp index + private object dir; diffs come from `git diff --numstat`/`--patch` between refs | git2 or shelling out to git |

---

### A. Server ↔ client protocol

#### A.1 Transport and framing

- **Route**: `HttpRouter.add("GET", "/ws", …)` (`apps/server/src/ws.ts:3801-3803`).
- **Protocol version gate**: the query param `orchestrationProtocol` must equal `ORCHESTRATION_PROTOCOL_VERSION = 2` (`packages/contracts/src/environment.ts:13-16`; check at `ws.ts:814-819`). Otherwise the server answers HTTP **426** with `{code:"orchestration_protocol_incompatible", …}` (`ws.ts:3807-3816`). The header form is `x-t3-orchestration-protocol`.
- `clientCapabilities=<comma list>` (`environment.ts:18-23`) carries feature opt-ins such as `settingsSecretReferences` and `httpUsageLimitSources` (`ws.ts:821-827`).
- **RPC framework**: Effect RPC (`effect/rpc`). The server calls `RpcServer.makeProtocolWithHttpEffectWebsocket` and then `RpcServer.make(ServerWsRpcGroup, {disableTracing:true, disableFatalDefects:true})` (`ws.ts:3778-3781`, `3840-3845`), with **`RpcSerialization.layerJson`** (`ws.ts:3876`). The client side is `RpcClient.makeProtocolSocket` + `RpcSerialization.layerJson` (`packages/client-runtime/src/rpc/session.ts:265-291`). There is no msgpack.
- **Wire envelope** (Effect RPC encoded messages, `.repos/effect/packages/effect/src/rpc/RpcMessage.ts:76-517`):
  ```jsonc
  // client → server
  {"_tag":"Request","id":"17","tag":"orchestration.subscribeThread","payload":{...},"headers":[]}
  {"_tag":"Ack","requestId":"17"}        // stream flow-control ack
  {"_tag":"Interrupt","requestId":"17"}  // cancel request/stream
  {"_tag":"Ping"} / {"_tag":"Eof"}
  // server → client
  {"_tag":"Chunk","requestId":"17","values":[item, item, ...]}   // stream items (batched)
  {"_tag":"Exit","requestId":"17","exit":{"_tag":"Success","value":...} | {"_tag":"Failure","cause":{"_tag":"Fail","error":{...tagged error...}}}}
  {"_tag":"Defect",...} / {"_tag":"ClientProtocolError",...} / {"_tag":"Pong"}
  ```
  Unary RPCs return one `Exit`. `stream: true` RPCs return N `Chunk`s and then an `Exit`. Errors are `Schema.TaggedError`s whose `_tag` the client decodes.
- **Heartbeat**: the server writes `{"_tag":"Pong"}` every 20 s (`apps/server/src/socketHeartbeat.ts:7-24`) so that proxies with a 60 s idle timeout keep the socket. The client pings every 5 s and drops the socket after **15 s with no frame** (`client-runtime/src/rpc/protocol.ts:10-12`).
- **Large-frame splitting** (opt-in with the `?frameParts` query param, `packages/shared/src/socketFrameParts.ts:6`): a text frame over 48 KiB of UTF-8 is cut into parts. Every part except the last starts with `\u001e`. The client joins the parts and, for each partial part, injects a synthetic `{"_tag":"Pong"}` to count as liveness (`socketFrameParts.ts:10-80`). It is wired in at `ws.ts:3924-3934`.
- **Flow control (`withOutputWindow`)**, `apps/server/src/terminal/OutputProtocol.ts:16-34,71-140`: per (client, requestId), at most **8 unacked chunks**, and for terminal streams also **64 KiB** in flight. Count-windowed streams are `subscribeServerConfig`, `orchestration.subscribeShell` and `orchestration.subscribeThread`. Byte-windowed streams are `terminal.attach` and `terminal.observe`. Below the window the server sends the ACK itself; above it, the stream waits for client ACKs.
- **perMessageDeflate: true** is set on the HTTP server (`apps/server/src/server.ts:409`). It is disabled for `desktop-bootstrap` sessions (`ws.ts:3830-3836`).
- **Session lifetime**: the socket races the RPC server against `sessions.awaitInvalidation(sessionId)`. Revoking a session closes the socket. Connect and disconnect are recorded with `markConnected`/`markDisconnected` (`ws.ts:3940-3955`).

#### A.2 Auth at upgrade

`EnvironmentAuth.authenticateWebSocketUpgrade(request,"rpc")` (`apps/server/src/auth/EnvironmentAuth.ts:1577-1629`):
1. If the query has a `wsTicket`, the server calls `sessions.verifyWebSocketToken`, rejects the `mcp-client` audience, and checks the ticket's `audience === "rpc"`. If the session is DPoP-bound, a DPoP proof is required, either in the `dpop` header or in the `dpop` query param (browsers cannot set WS headers). The proof is verified against the ticket's hash, and its jti is consumed.
2. Otherwise the request falls back to `authenticateRequest`, which accepts a cookie or bearer token.
- Tickets are minted over HTTP at `POST /api/auth/websocket-ticket` and `/api/auth/websocket-ticket/:use` (`packages/contracts/src/environmentHttp.ts:619-633`), with a default TTL of **5 min** (`apps/server/src/auth/SessionStore.ts:493`).
- Per-method authorization: the whole `WsRpcGroup` is wrapped in the `RpcScopeAuthorization` middleware (`packages/contracts/src/rpc.ts:2738`). The server implementation `RpcAuthorization.layer(session.scopes, …)` is at `ws.ts:3846-3848`. Scopes include `orchestration:read|operate`, `settings:write`, `providers:manage`, `terminal:read|operate`, `filesystem:read|write`, `source-control:write`, `preview:operate`, `access:read|write`, `relay:*`, `diagnostics:read` and `environment:maintain` (`contracts/src/auth.ts:31-50`). Client-side guards are in `contracts/src/clientRpcPermissions.ts:20+`.

#### A.3 HTTP endpoints for orchestration (cold/bulk reads)

`packages/contracts/src/environmentHttp.ts:741-812`, implemented in `apps/server/src/orchestration-v2/http.ts:775-815`:

| GET | Query | Returns |
|---|---|---|
| `/api/orchestration/shell` | `omitIdleSubagents`, `omitOlderSettled`, `resumeFrom=<seq>` | `OrchestrationV2ShellHttpSnapshot` or `{resumable:true}` |
| `/api/orchestration/threads/:threadId` | – | full `ThreadDetailSnapshot` |
| `/api/orchestration/threads/:threadId/bounded` | `deferWork`, `compactTurnItems`, `dedupeBodies`, `compactIds` | `ThreadBoundedSnapshot` (window + cursor) |
| `/api/orchestration/threads/:threadId/history` | `cursor`(req), `throughEntryId`, `view=conversation\|activity\|work`, `deferWork`, `compactIds` | `ThreadHistoryPage` |
| `/api/orchestration/threads/:threadId/transcript` | – | full transcript (export) |

Other HTTP routes: `/api/projects` and `/api/projects/mutate`, `/api/pull-requests/diff`, `/api/voice/*`, asset and attachment-upload routes (`apps/server/src/http.ts:526,585`), the OTLP proxy, and static assets.

#### A.4 RPC method catalogue (`WS_METHODS`, `rpc.ts:489-746`; `ORCHESTRATION_V2_WS_METHODS`, `orchestrationV2.ts:4092-4116`)

Streaming methods (`stream: true`) are marked **[S]**.

**Orchestration (V2)**, schemas at `orchestrationV2.ts:4761-4846`, handlers at `ws.ts:2058-2410`:

| Method | Payload | Result |
|---|---|---|
| `orchestration.dispatchCommand` | `OrchestrationV2Command` (see A.6) | `{sequence}` = sequence of the committed receipt |
| `orchestration.launchThread` | `{commandId, projectId, title, generateTitle?, modelSelection, runtimeMode, interactionMode, workspaceStrategy, initialMessage?{messageId?,text,context?,attachments}, threadId?, reuseExistingThread?, creationSource?}` | `{threadId, projection, resumed, sequence?}` |
| `orchestration.subscribeShell` **[S]** | `{afterSequence?, httpSnapshotFallback?, knownProjectIdentities?, requestCompletionMarker?, omitIdleSubagents?, omitOlderSettled?}` (`:4206`) | `ShellStreamItem` |
| `orchestration.subscribeThread` **[S]** | `{threadId, afterSequence?, requestCompletionMarker?, acceptBoundedSnapshot?, acceptDeferredWork?, acceptCompactTurnItems?, acceptDedupedBodies?, acceptCompactIds?}` (`:4327`) | `ThreadStreamItem` |
| `orchestration.subscribeCommandOutput` **[S]** | `{threadId, itemId}` | `{kind:"replace"\|"append", text, truncated, running}` (`:4127`) |
| `orchestration.subscribePromptSuggestion` **[S]** | `{threadId}` | `null \| {runId, text}` |
| `orchestration.getThreadProjection` | `{threadId}` | `ThreadProjection` |
| `orchestration.getTurnItem` | `{threadId,itemId}` | full (un-slimmed) item |
| `orchestration.getTurnDiff` | `{threadId, fromTurnCount,toTurnCount, ignoreWhitespace?}` (`checkpointDiff.ts:47`) | `ThreadTurnDiff` |
| `orchestration.getFullThreadDiff` | `{threadId, toTurnCount, ignoreWhitespace?}` | `ThreadTurnDiff` |
| `orchestration.searchThreads` / `searchThread` / `searchThreadStream`**[S]** | query… | matches |
| `orchestration.getArchivedShellSnapshot` / `getThreadFamily` / `getThreadTitles` / `getThreadLinkMetadata` / `getWorktreeThreadIds` / `getOlderSettledThreads{before:{settledAt,threadId?},projectIds?,limit≤50}` | – | shell rows |
| `orchestration.stopWorkflow{threadId,subagentId}` / `forceStopThread` / `getWorkflowScript` / `getNativeChildWork` | – | – |

**Server/config**: `server.probe` (returns `{serverTime?}`, used as a liveness probe), `server.getConfig`, `subscribeServerConfig` **[S]** (opt-ins: `environmentThemes, usageLimitSources, usageLimitsCommand, workspaceSnapshotsOnDemand, providerCatalogsOnDemand, providerCatalogDigests, providerStatusDeltas`; events: snapshot / keybindingsUpdated / providerStatuses / providerStatusDeltas / settingsUpdated / environmentThemesUpdated / usageLimitSourcesUpdated; `rpc.ts:2301-2323`, `server.ts:914-922`), `subscribeServerLifecycle` **[S]** (welcome/ready/legacyThreadMigration, `server.ts:985`), `server.getSettings`, `server.updateSettings{patch, providerInstanceMutation?}` (`rpc.ts:1029-1036`), `server.upsertKeybinding` / `removeKeybinding`, `server.refreshProviders{instanceId?,cwd?,fresh?,refreshModels?,modelCatalogOnly?,...}` (`rpc.ts:773-810`), `server.updateProvider`, `server.updateServer(WithProgress)`, `server.commitDesktopUpdate`, `server.getNpmRelease`, storage cleanup and DB storage (`runStorageCleanup`, `getStorageCleanupReport`, `getDatabaseStorage`, `reclaimDatabaseSpace`), diagnostics (`getTraceDiagnostics`, `getProcessDiagnostics`, `getHostResources`, `get*ResourceHistory`, `subscribeResourceTelemetry`**[S]**), `server.signalProcess`, `server.reportClientActivity`, usage (`getUsageSummary`, `refreshUsageRates`), `listTextGeneratorModels`, `prepareTriage`, `checkOtlpEndpoint`, `checkSecretSource`, `discoverSourceControl`, and the ACP registry (`server.searchAcpRegistry`, `prepareAcpRegistryAgent`, `…Sessions`, `…Providers`, `logoutAcpRegistry`, …).

**Providers**: `provider.auth.{start,respond,complete,cancel,removeMethod,logout,subscribe[S]}`, `provider.install.{start,cancel,subscribe[S],remove}`, `provider.chatgpt.{reconnect-profile,import-profile,handoff.subscribe[S]}`, `provider.consumeResetCredit`, `provider.uploadFeedback`, `provider.mcpSignIn.{start,complete}`.

**Projects/FS/workspace**: `projects.{listEntries,listIgnoredFiles,readFile,writeFile,searchEntries,searchContents,mutate(ProjectMutation: project.create|update|delete, project.ts:242-262),ensureScratch,createNew,createFromThread}`, `filesystem.{browse,getMetadata}`, `workspace.{listFolders,scanFolders,taskFolder,addTaskFolderRepository,layoutCommands,approveLayoutCommands}`, `shell.openInEditor`, `agentSessions.{scan,import}`, `assets.{createUrl,persistChatAttachments}`, `attachments.{createUploadUrl,delete}`, `projectClone.{start,cancel,retry}` + `subscribeProjectClones`**[S]**.

**VCS/git**: `subscribeVcsStatus`**[S]** (events `snapshot{local,remote} | localUpdated{local} | remoteUpdated{remote}`, `git.ts:281-292`), `subscribeVcsBranch`**[S]**, `vcs.{refreshBranch,refreshStatus,pull,listRefs{cwd,query?,cursor?,refKind?,limit?},createWorktree,removeWorktree,createRef,switchRef,init}`, `git.runStackedAction{actionId,cwd,action:commit|push|create_pr|commit_push|commit_push_pr,commitMessage?,featureBranch?,filePaths?,threadId?,projectId?}` (`git.ts:19-25,134-143`, progress events `git.ts:501`), `git.resolvePullRequest`, `git.preparePullRequestThread`, `subscribeWorktreeSetup`**[S]**, `worktreeSetup.cancel`, `review.{getBranchCommits,getDiffPreview,getDiffFileContents}`.

**Terminal** (`contracts/src/terminal.ts`): `terminal.open{threadId,terminalId,cwd,worktreePath?,cols?,rows?,env?,providerInstanceId?,stopOnSettle?}` (`:161`), `terminal.attach`**[S]** (adds `restartIfNotRunning?, resume?, maxHistoryBytes?`, `:173-187`), `terminal.observe`**[S]**, `terminal.write{data, binary?, reply?}` (`:196`), `terminal.resize{cols,rows}`, `terminal.clear`, `terminal.close{terminalId?,deleteHistory?}`, `subscribeTerminalMetadata`**[S]** (`snapshot{terminals[]}|upsert{terminal}|remove`, `:282-298`). Attach-stream events: `started{snapshot}`, `output{data}`, `exited{exitCode,exitSignal}`, `closed`, `error`, `cleared`, `restarted`, `activity{hasRunningSubprocess,label,cols?,rows?}` (`:313-379`). `TerminalSessionSnapshot` carries `{threadId,terminalId,cwd,worktreePath,status:starting|running|exited|error,pid,history,exitCode,exitSignal,label,updatedAt,sequence?,cols?,rows?}` (`:245-262`).

**Other groups** (largely product surface): `pullRequests.*` (about 35 methods, including `subscribeRefreshes`**[S]**), `issues.*`, `issueTrackers.*`, `sourceControl.*`, `preview.*` + `subscribePreviewEvents`**[S]** / `subscribeDiscoveredLocalServers`**[S]**, `device.*` + `subscribeDeviceState`**[S]**, `workflows.*` (`subscribe`**[S]**, `counts`**[S]**), `scheduledTasks.*` (`subscribe`**[S]**), `peerLinks.*` (`subscribeThread`**[S]**), `mcpApps.*`, `secrets.answerRequest`, `agentFeedback.answerRequest`, `guestShare.create`, `cloud.*`, `subscribeAuthAccess`**[S]**.

#### A.5 Stream item shapes

```ts
// orchestrationV2.ts:2582-2621
type ShellStreamItem =
  | {kind:"synchronized"}                    // catch-up done (if requestCompletionMarker)
  | {kind:"snapshot-required"}               // only when httpSnapshotFallback:true and cursor unusable
  | {kind:"snapshot", snapshot: ShellSnapshot, resolvedRepositoryIdentityRoots?: string[]}
  | {kind:"project.updated", sequence, project: OrchestrationProjectShell}
  | {kind:"project.removed", sequence, projectId}
  | {kind:"thread.updated", sequence, location:"active"|"archive", thread: ThreadShell}
  | {kind:"thread.removed", sequence, location, threadId};

// orchestrationV2.ts:4440-4467  (wrapped in withRecordIdTable codec when acceptCompactIds)
type ThreadStreamItem =
  | {kind:"synchronized"}
  | {kind:"snapshot", snapshotSequence, projection: ThreadProjection,
     historyCursor?: string|null, hasMoreHistory?, latestLocalTurnOrdinal?: number|null,
     turnItemsOmitLocalVisible?: true, idTable?: true}
  | {kind:"event", sequence, event: DomainEvent}
  | {kind:"unknown-event", sequence, eventType}   // client-side decode of a future event type
```
**Wire compactions**, all opt-in:
- `acceptCompactIds`: every `id`/`*Id`/`*Ids` value in a record is replaced by an index into an `ids` table (`contracts/src/orchestrationV2IdTable.ts:1-80`).
- `acceptDedupedBodies`: repeated `text`/`prompt`/`result`/`files` bodies are blanked and flagged with `textOmitted:true`/`promptOmitted:true`/… (`orchestrationV2.ts:1000-1006`). The client restores them from its previous copy (`client-runtime/src/state/orchestrationV2Projection.ts:69,433`).
- `acceptCompactTurnItems`: `turnItems` already present in `visibleTurnItems` are omitted.
- `acceptDeferredWork`: long runs of work items fold into a single item carrying `deferredWork:{cursor,itemCount,lastOrdinal}` (`:1773-1779`).

Whatever the opt-ins, the **wire projection** is always slimmed (`apps/server/src/orchestration-v2/WireProjection.ts:26-31`): at most 200 checkpoint files, 32 KiB detail strings, 16 KiB dynamic-tool values, raw command output omitted (`outputOmitted`). Full bodies are fetched on demand with `getTurnItem`.

#### A.6 Orchestration commands (`OrchestrationV2Command`, `orchestrationV2.ts:3299-3833`)

Every command carries `commandId` (client-generated, used for idempotency) and, almost always, `threadId`.

| type | key fields |
|---|---|
| `thread.create` | `createdBy, creationSource, projectId, title, titleRenamed?, modelSelection, runtimeMode, interactionMode, branch, worktreePath, importedNativeThread?{ref,metadata}`. Server-only fields: `linkOrigin, delegatedFrom, workflowRun, initialMessage{messageId,text,attachments,context?,titleSeed?,scheduledTaskId?,senderThreadId?,outputSchema?,workspaceStrategy?}` |
| `thread.stop` | `reason?, stopDelegatedTasks?` |
| `thread.archive` / `unarchive` / `delete{onlyIfUnused?}` | |
| `thread.settle{expectedLatestRunId?,settledAt?}` / `thread.auto-settle{snapshotAt,settledAt?}` / `thread.unsettle{reason:"user",restoreUnsettledAt?}` / `thread.auto-settle.set{enabled}` | |
| `thread.snooze{snoozedUntil}` / `thread.unsnooze{reason:"user"}` | |
| `thread.pin{orderKey?}` / `unpin` / `pin.reorder{orderKey}` / `active.reorder{orderKey}` / `tag.set{tag,tagged}` | |
| `thread.visit{visitedAt, opened?, searchQuery?}` / `thread.mark-unread` | |
| `thread.metadata.update` | `title?, regenerateTitle?, branch?, worktreePath?, expectedWorktreePath?, expectedBranch?, continuation?{messageId,text}, expectedEmpty?, limitRecovery?, linkedPullRequest?` |
| `thread.pull-request.{link,unlink,watch,sync}`, `thread.pull-request-link.sync`, `thread.issue.{link,unlink}` | PR/issue keys |
| `thread.title.regeneration.complete{requestId,title?,outcome?}` | |
| `thread.runtime-mode.set{runtimeMode}` / `thread.interaction-mode.set{interactionMode}` / `thread.model-selection.set{modelSelection}` / `provider.switch{modelSelection}` | |
| `provider-session.detach{providerSessionId,reason?}` | |
| **`message.dispatch`** | `messageId, text, attachments[], context?, titleSeed?, modelSelection?, createdBy, creationSource, dispatchMode: {type:"start_immediately"} \| {type:"queue_after_active"} \| {type:"steer_active",targetRunId} \| {type:"restart_active",targetRunId} \| {type:"defer_start",workspaceStrategy?}`, plus `deliveryIntent?: auto\|steer\|restart`, `freshContext?`, `outputSchema?`, `sourcePlanRef?`, `notification?`, `guestMessageId?`, continuation ids (`restart/usageLimit/manualContinuationOfRunId`, `steerFollowUpOfRunId`, …), `delegatedCompletion?` (`:3598-3645`) |
| `prepared-run.{release,progress{phase:worktree\|setup},fail{failure},retry}` | `runId` |
| `notification.delivery.accept{messageId}` | |
| `run.interrupt{runId,reason?,holdQueue?}` / `subagent.stop{runId,subagentId}` | |
| `queued-message.promote-to-steer{queuedRunId,targetRunId}` / `queue.resume` / `queued-run.{start,reorder{beforeRunId},cancel,edit{text,attachments?,modelSelection?,context?}}` | |
| **`runtime-request.respond`** | `requestId, decision?: accept\|acceptForSession\|acceptAlways\|decline\|cancel, feedback?, answers?: Record<string,unknown>, attachmentsByQuestionId?` |
| `thread.user-input.dismiss{requestId}` | |
| **`checkpoint.rollback`** | `scopeId, checkpointId, restoreFiles?` |
| `thread.fork` / `thread.merge_back` | `sourceThreadId, targetThreadId, sourcePoint, title?, sideChat?, createdAt?` |
| `delegated_task.{request,wake-policy,completion-delivery.acknowledge,completion-delivery.dispose}`, `thread.created.record` | subagent delegation |

**Internal (server-only) commands** (`:3835-4087`): `run.output.submit`, `thread.guest-share.{open,redeem}`, `thread.guest-message.submit`, `thread.project.adopt-folder`, `thread.pull-request-watch.{checks-reopen,sync}`, `delegated_task.remote.{request,complete,stop}`, `checkpoint.rollback.fail`, `runtime-request.delivery.fail`, `thread.interrupt.unresponsive`, `thread.background-work.settle`, `secret_request.record`, `feedback_request.record`, `link_request.record`.

#### A.7 Domain events (`OrchestrationV2DomainEvent`, `orchestrationV2.ts:2170-2337`)

Common base: `{id: EventId, threadId, runId?, nodeId?, driver?, providerInstanceId?, rawEventId?, occurredAt, type, payload}`. **Every payload is the full current entity, not a diff.** This matters for storage (B.3) and for client application, which is a plain upsert by id.

| type(s) | payload |
|---|---|
| `thread.created`, `thread.{archived,unarchived,deleted,settled,unsettled,snoozed,unsnoozed,pinned,unpinned,auto-settle-set,pin-reordered,active-reordered,tags-updated,visited,marked-unread,metadata-updated,pull-request-synced,issue-synced,runtime-mode-updated,interaction-mode-updated,model-selection-updated,provider-switched,guest-share-updated}` | `AppThread` |
| `thread.pull-request-link-updated` | `ThreadPullRequestLink` |
| `thread.visit-recorded` | `{lastVisitedAt, opens}` |
| `run.created`, `run.updated` | `Run` |
| `run.background-work-cancelled` | `{runId, restartCancelledBackgroundWork[]}` |
| `run.output-submitted` | `{runId, output{value,submittedAt}}` |
| `run-attempt.created/updated` | `RunAttempt` |
| `node.updated` | `ExecutionNode` |
| `subagent.updated` | `Subagent` |
| `provider-session.attached/updated` · `provider-session.detached` | `ProviderSession` · `{providerSessionId,detachedAt,reason?}` |
| `provider-thread.updated` · `provider-turn.updated` · `runtime-request.updated` | entity |
| `message.updated` · `turn-item.updated` · `plan.updated` | `ConversationMessage` · `TurnItem` · `PlanArtifact` |
| `checkpoint-scope.created` · `checkpoint.captured` · `checkpoint.rollback-requested{scopeId,checkpointId,requestedAt}` | entity |
| `context-handoff.updated` · `context-transfer.created/updated` | entity |

Stored form: `StoredEvent = {sequence, commandId|null, event}` (`:2623`). Project events (`project.created|meta-updated|deleted`) share the same global sequence space (`persistence/OrchestrationEventStore.ts:117-121`).

#### A.8 Domain model (fields abridged; enums complete)

```ts
AppThread (orchestrationV2.ts:500-590) {
  id, projectId, title, createdBy: "user"|"agent"|"system", creationSource: "web"|"mobile"|"mcp"|"provider"|"server",
  providerInstanceId, modelSelection{instanceId, model, options?}, runtimeMode, interactionMode,
  branch|null, worktreePath|null, linkedPullRequest?, pullRequests?, branchPullRequest?, issues?,
  activeProviderThreadId|null, historyOrigin?: "native"|"v1_import", linkOrigin?, delegatedFrom?, workflowRun?,
  lineage{parentThreadId|null, relationshipToParent: "fork"|"subagent"|null, rootThreadId, sideChat?, workflowStep?},
  forkedFrom: null|{type:"run",threadId,runId}|{type:"node",nodeId}|{type:"provider_thread",providerThreadId,providerTurnId?},
  createdAt, updatedAt, archivedAt|null, deletedAt|null,
  settledOverride: "settled"|"active"|null, settledAt|null, unsettledAt?, snoozedUntil?, snoozedAt?, pinnedAt?, pinOrderKey?, activeOrderKey?,
  tags?, lastVisitedAt|null, opens?, titleRegeneration?, lastTitleRegeneration?, titleRenamed?, limitRecovery?, guestShare?,
  rollbackRequestId?, rollbackFailure?, rollbackUnrestored?
}
RuntimeMode = "approval-required"|"auto-accept-edits"|"auto"|"full-access"   (providerPolicy.ts:25-32, default full-access)
InteractionMode = "default"|"plan"                                          (providerPolicy.ts:45)

Run (:819-864) { id, threadId, ordinal, providerInstanceId, modelSelection, providerThreadId|null, userMessageId,
  rootNodeId|null, activeAttemptId|null, status: RunStatus, queuePosition?, queueHeld?, steerWhenReadyRunId?,
  requestedAt, startedAt|null, completedAt|null, workStartedAt?, checkpointId|null, contextHandoffId|null,
  restart/manual/usageLimitContinuationOfRunId?, delegatedTaskId?, sourcePlanRef?, delegatedCompletion?,
  workspacePreparation?, freshContext?, outputSchema?, output? }
RunStatus = preparing|queued|starting|running|waiting|completed|interrupted|failed|cancelled|rolled_back  (:718)
RunAttempt (:878) { id, runId, attemptOrdinal, rootNodeId, providerInstanceId, providerThreadId, providerTurnId|null,
  reason: initial|steering_restart|retry|provider_recovery,
  status: pending|running|completed|interrupted|failed|cancelled|superseded, startedModes? }
ExecutionNode (:907) { id, threadId, runId, parentNodeId, rootNodeId,
  kind: root_turn|assistant_message|reasoning|plan|todo_list|tool_call|approval_request|user_input_request|subagent|hook|system,
  status: idle|pending|running|waiting|completed|interrupted|failed|cancelled|rolled_back, countsForRun,
  providerThreadId, providerTurnId, nativeItemRef{driver,nativeId,strength:strong|weak|none}, runtimeRequestId, checkpointScopeId, … }
Subagent (:1033) { id(NodeId), threadId, runId, parentNodeId, origin: provider_native|app_owned, driver, providerInstanceId,
  providerThreadId, childThreadId|null, remoteChild?, prompt, title, model, status: idle|pending|running|waiting|completed|failed|cancelled|interrupted,
  progress?, workflow?{phases[],agents[]}, result|null, completionWake?: always|settled_only, completionDelivery?{state: pending|claimed|acknowledged|delivered|disposed} … }
ProviderSession (:1106) { id, driver, providerInstanceId, status: starting|ready|running|waiting|stopped|error, cwd, model,
  capabilities, extensionStatuses[], startupPhase?, lastError, createdAt, updatedAt }
ProviderThread (:1257) { id, driver, providerInstanceId, providerSessionId, appThreadId, ownerNodeId, nativeThreadRef, nativeConversationHeadRef,
  status: not_loaded|idle|active|archived|closed|error, first/lastRunOrdinal, handoffIds[], forkedFrom, pendingBackgroundTasks[],
  contextUsage, nativeMetadata, goal?{objective,status:active|paused|blocked|usage_limited|budget_limited|complete,…} }
ProviderTurn (:1351) { id, providerThreadId, nodeId, runAttemptId, nativeTurnRef, ordinal, status: pending|running|completed|interrupted|failed|cancelled,
  tokenUsage?, turnTokenUsage?, model?, promptDropped? }
RuntimeRequest  (pending approval / question) (:1415) { id, nodeId, providerTurnId, nativeRequestRef,
  kind: command|file-read|file-change|mcp-elicitation|permission|dynamic_tool_call|user_input|auth_refresh,
  status: pending|resolved|expired|cancelled,
  responseCapability: {type:"live",providerSessionId,supportsFeedback?}|{type:"message"}|{type:"not_resumable",reason},
  createdAt, resolvedAt, decision?, feedback?, answers? }
ConversationMessage (:1525) { id, threadId, runId, nodeId, role: user|assistant|system, text, context?, attachments[], streaming,
  createdAt, updatedAt, createdBy, creationSource, notification?, scheduledTaskId?, senderThreadId?, idleCompaction?, … }
PlanArtifact (ProposedPlan/Todo) (:1597) { id, threadId, runId, nodeId, status: draft|active|completed|superseded,
  kind:"proposed_plan", markdown } | { …, kind:"todo_list", steps[{id,text,status:pending|running|completed}], explanation? }
CheckpointScope (:1083) { id, threadId, runId, nodeId, parentScopeId, providerThreadId, kind: root_run|subagent|tool|provider_thread|manual,
  ordinalWithinParent, advancesAppRunCount, cwd, label?, repositoryPath?, createdAt }
Checkpoint (:1629) { id, threadId, scopeId, runId, nodeId, parentCheckpointId, ordinalWithinScope, appRunOrdinal, ref,
  status: ready|missing|error|stale, files[{path,kind,additions,deletions}], filesTotal?, capturedAt }
ContextHandoff (:1307) — cross-provider context summary { strategy: delta_since_target_last_seen|fork_delta_summary|full_thread_summary|checkpoint_summary|manual_context,
  status: pending|ready|failed|superseded, summaryText, … }
```

**TurnItem** (`:1899-2157`): the timeline atom. Base fields: `{id, threadId, runId, nodeId, providerThreadId, providerTurnId, nativeItemRef, parentItemId, ordinal, status: idle|pending|running|waiting|completed|failed|cancelled|interrupted, title, startedAt, completedAt, updatedAt, deferredWork?, toolSurface?, toolIcon?, toolSource?}`. The `type` discriminator takes these values:
`notification` · `user_message{messageId,text,attachments,inputIntent: turn_start|queued_turn|steer|promoted_queued_to_steer, modelSwitch?, …}` · `assistant_message{messageId,text,phase?: commentary|partial_answer|final_answer, streaming, delivery?:"async"}` · `reasoning{text,streaming}` · `proposed_plan{planId,markdown,streaming}` · `todo_list{planId,steps}` · `user_input_request{requestId,questions[{id,header,question,options[],multiSelect?,…}]}` · `file_change{fileName,additions?,deletions?,diffStr?,oldStr?,newStr?,changes?}` · `command_execution{input,output?,outputOmitted?,exitCode?,monitor?}` · `file_search` · `web_search` · `approval_request{requestId,requestKind,prompt?,reason?,options?}` · `checkpoint{checkpointId,scopeId,files}` · `run_interrupt_request` · `run_interrupt_result` · `system_notice{message,level?}` · `error{failure{class: usage_limit|provider_error|transport_error|permission_error|validation_error|unknown,message,code,retryable,resetAt?},retry?}` · `compaction` · `handoff` · `fork` · `thread_created` · `secret_request` · `link_request` · `feedback_request` · `subagent{subagentId,childThreadId,prompt,result,…}` · `dynamic_tool{toolName,input,output?}`.

Unknown `type`s decode to a placeholder and are filtered out, which keeps old clients forward-compatible (`:2159-2178`, `ForwardCompatibleUnionArray`).

**ThreadProjection** (`:2339-2365`), the full per-thread read model sent to clients:
`{thread, runs[], attempts[], nodes[], subagents[], providerSessions[], providerThreads[], providerTurns[], runtimeRequests[], messages[], plans[], turnItems[], checkpointScopes[], checkpoints[], contextHandoffs[], contextTransfers[], visibleTurnItems: ProjectedTurnItem[], updatedAt}`. `ProjectedTurnItem = {position, visibility:"local"|"inherited"|"synthetic", sourceThreadId, sourceItemId, item}` (`:2161`). `visibleTurnItems` is the rendered timeline, including items inherited from fork parents.

**ThreadShell** (`:2402-2531`), a sidebar row derived server-side: thread metadata plus `latestRunId, activeRunId, activityRunStatus, status: "idle"|RunStatus, lastError*, pendingRuntimeRequest{id,kind,createdAt,blocking?,userOnly?}|null, latestVisibleMessage{id,role,text,updatedAt}|null, latestUserMessageAt, hasActionableProposedPlan, pendingBackgroundTasks[], goal?, itemCount, visibleItemCount, …`. `ShellSnapshot = {schemaVersion, snapshotSequence, threads[], archivedThreads[], projects: ProjectShell[], olderSettled?}` (`:2551-2565`).

---

### B. Persistence

#### B.1 SQLite setup
- Driver: Node's built-in **`node:sqlite` (`DatabaseSync`)**, wrapped in a custom Effect SqlClient. **A writer worker thread runs alongside a pool of reader worker threads** (`packages/shared/src/nodeSqliteClient.ts:1-60, 354-420, 953-1013`; options at `apps/server/src/persistence/Sqlite.ts:108-117`). Reads wrapped in `NodeSqliteClient.readOnly` go to readers; everything else, including any `withTransaction` that lacks `readOnly`, goes to the writer.
- File: `<stateDir>/statev2.sqlite` (`apps/server/src/config.ts:154`). On first start, a V1 `state.sqlite` is copied over with `VACUUM INTO` in a worker (`persistence/initializeV2Database.ts:30-106`).
- PRAGMAs (`Sqlite.ts:72-95`): `busy_timeout=5000`, `foreign_keys=ON`, `auto_vacuum=INCREMENTAL` (new DBs only), **`journal_mode=WAL`**, **`synchronous=NORMAL`**, `checkpoint_fullfsync=ON`, `temp_store=MEMORY`, `journal_size_limit=128MiB`. After migrating, the server deletes `sqlite_stat1` and runs `ANALYZE sqlite_schema` (it prefers indexes over stale stats). Many queries force a plan with `INDEXED BY` (`OrchestrationEventStore.ts:427-433, 664-688`).
- Migrations: 116 numbered files plus an `OrchestrationV2/` sub-series (`persistence/Migrations/`, runner in `Migrations.ts`). The V2 base schema is `055_OrchestrationV2.ts`.

#### B.2 Tables (orchestration-relevant)

| Table | Purpose | Key columns |
|---|---|---|
| `orchestration_events` (`001_*.ts:8-23`; V2 rows marked `application_event_version=2`) | **the event log**, shared by project and thread events | `sequence INTEGER PK AUTOINCREMENT` (global cursor), `event_id UNIQUE`, `aggregate_kind 'project'\|'thread'`, `stream_id`(=threadId/projectId), `stream_version`, `event_type`, `occurred_at`, `command_id`, `actor_kind client\|server\|provider`, `payload_json`, `metadata_json{runId,nodeId,driver,providerInstanceId,rawEventId}` |
| `orchestration_v2_events` (`055:20-39`) | legacy; rows migrated into `orchestration_events` (`OrchestrationV2/ApplicationEventSource.ts:35-120`) | |
| `orchestration_v2_command_receipts` (`055:42-52`) | idempotency | `command_id PK, thread_id, command_type, accepted_at, result_sequence, status accepted\|rejected, error` |
| `orchestration_v2_effect_outbox` (`OrchestrationV2/Foundation.ts:102-120`) | side-effect queue | `effect_id PK, command_id, thread_id, effect_type, payload_json, status pending\|running\|succeeded\|failed\|cancelled, attempt_count, available_at, lease_owner, lease_expires_at, last_error, …` |
| `orchestration_v2_projection_{threads,runs,run_attempts,nodes,subagents,provider_sessions,provider_threads,provider_turns,runtime_requests,messages,plans,turn_items,checkpoint_scopes,checkpoints,context_handoffs,context_transfers}` (`055:55-313`) | **read model**: one row per entity, holding indexed scalar columns plus **`payload_json` = the full entity JSON** | e.g. turn_items: `turn_item_id PK, thread_id, run_id, node_id, provider_turn_id, parent_item_id, ordinal, type, status, updated_at, payload_json, inline_work_output`. Messages also carry `search_text` |
| `orchestration_v2_turn_item_positions` (`Foundation.ts:123-148`) | stable timeline ordinals | `(thread_id, turn_item_id) PK, UNIQUE(thread_id, ordinal)`. Ordinal = `runOrdinal*1e6 + rowNumber` |
| `orchestration_v2_projection_{thread,turn}_item_counts`, `…provider_session_bindings`, `orchestration_v2_thread_launch_workflows`, `orchestration_v2_legacy_imports` | counters, bindings, misc | |
| `orchestration_v2_projection_metadata` (`Foundation.ts:151-171`) | `projection_name PK, schema_version, last_sequence` | holds `thread-projections`, `event-log-pruned-through`, `event-log-prune-mark` |
| `projection_projects` (+ V1 `projection_*` leftovers), `auth_sessions`, `auth_pairing_links`, `provider_session_runtime`, `checkpoint_diff_blobs`, `scheduled_tasks*`, `workflow_*`, `task_folders`, … | other subsystems | |

Settings and keybindings are JSON files (`settingsPath`, `keybindingsConfigPath` in `config.ts:38-42`), not SQLite.

#### B.3 Write path: command → events → projection → receipt → outbox (one tx)

1. `ws.ts:2058-2111` → `startup.enqueueCommand(ThreadMessageIntake.dispatchCommand(...))` → `Orchestrator.dispatch` = `dispatchWithReceipt` (`orchestration-v2/Orchestrator.ts:16828-16870, 17424`).
2. **Serialization**: a per-thread lock (`threadDispatch.withLock(threadId, …)` from `ThreadCommandExecutor`, `Orchestrator.ts:1691, 16846`). Forks also take the source thread's lock. Delete, archive and unarchive cascade the lock to side chats. Subagent follow-ups take the parent's lock first (`:16812-16820`).
3. **Idempotency**: `commandReceipts.getByCommandId`. If a receipt exists, the stored result is replayed and `replayed:true` is returned (`:16540-16550`). A rejection is recorded as a `rejected` receipt (`commitRejectedCommand`, `:16620-16640`), so retrying it fails the same way. Exception: mode-ceiling refusals are not recorded.
4. **Decide**: `dispatchOnce(command)` reads projections and returns `{events, effects, cancelUnsettledEffects?}`. A command that produces no events is an error unless it is on a whitelist (`:16577-16600`).
5. **Commit**: `EventSink.commitCommand` → `commitThenPublishNow` (`orchestration-v2/EventSink.ts:329-350`) runs one `sql.withTransaction`: `eventStore.append` (one `INSERT … RETURNING sequence` per event, sequentially; `stream_version = MAX+1` via subquery, `persistence/OrchestrationEventStore.ts:457-560`), then `applyStoredEvents` (projection upserts: `INSERT … ON CONFLICT DO UPDATE SET … payload_json=excluded.payload_json`, `ProjectionStore.ts:3120-3245`), then the receipt insert, then `effectOutbox.enqueue`.
6. **Publish after commit**, under a global `publishLane` semaphore so publication order equals commit order (`EventSink.ts:328-350`): `eventStore.publishCommitted` (global app PubSub, used by shell streams) and `publishLiveEvents` (V2 PubSub, used by thread streams; `:294-308`). Both PubSubs are **unbounded**.
7. The RPC returns `{sequence: receipt.resultSequence}`. Per the architecture doc, an ack means "intent committed", not "effect done" (`docs/internals/01_architecture.md`).

**Provider events** take a different path. `ProviderEventIngestor` normalizes adapter events into domain events and writes them with optimistic guards (`writeIfRunCurrent`, `writeIfProviderThreadOwner`, `writeIfTurnItemUnchanged` …: a `SELECT` guard inside the tx, `EventSink.ts:560-720`). Writes are grouped by **`ProviderCommitBatch`** (`orchestration-v2/ProviderCommitBatch.ts:13-20,111-140`): up to 64 snapshots or 1 MiB per shared transaction, held at most 100 ms while other writers wait. A queued snapshot of an entity is **superseded** by a later snapshot of the same entity in the same stream, provided no new entity appeared in between. Viewed threads rank first, and subagents yield until a 30 s starvation bound. Each job gets a savepoint (`EventSink.ts:1170-1200`).

**Subagent prompt dedupe in the log**: repeated `subagent.updated`/`turn-item.updated(subagent)` rows store `prompt:""` plus `promptSequence:<anchor seq>`. Reads re-join the prompt with a SQL `CASE` (`OrchestrationEventStore.ts:308-319, 466-480`). Pruning keeps anchor rows alive (`ProjectionMaintenance.ts:70-115`).

#### B.4 Outbox and effect worker
- Effect types (`orchestration-v2/EffectOutbox.ts:33-142`): `provider-runtime.continue`, `provider-session.{detach,apply-model-selection,apply-thread-title}`, `provider-turn.{start,interrupt,steer,restart}`, `runtime-request.respond`, `provider-thread.rollback`, `checkpoint.capture{runId,scopeId}`, `{scheduled-tasks,terminal,preview,attachment}.cleanup`, `thread-title.generate`, `delegated-tasks.stop`.
- **Per-thread FIFO lane**: a candidate is claimable only if no other effect on the same thread is `running` and no earlier `pending` one exists (by rowid). Title generation runs in its own lane (`EffectOutbox.ts:343-395`). Claiming is an atomic `UPDATE … WHERE effect_id=(SELECT … LIMIT 1) RETURNING *` (`:583-630`). A row that cannot be decoded is failed so that its lane does not block (`:618-629`).
- Worker daemon (`orchestration-v2/EffectWorker.ts:1043-1125`): **4 concurrent slots**. Each slot is woken by an in-memory queue (`notifyAvailable`), falls back to a 30 s liveness poll, and wakes at `available_at` for retry backoff. After 30 s a slot gives up waiting on a stuck effect and moves on; the `running` row keeps blocking only its own thread. At most `concurrency-1` title generations run at once.
- Results feed back as commands (for example `checkpoint.captured` via an EventSink write, `prepared-run.fail`).

#### B.5 Bounded replay window (event pruning)
- `RETAINED_EVENT_HISTORY = 1 day` (`orchestration-v2/ProjectionMaintenance.ts:41`). Each pass prunes through the *mark the previous pass left*, once that mark is more than a day old (`:255-292`). It keeps: V1 rows, every `thread.created`, `run.updated` with status `running`, each stream's head event, and prompt anchors (`:97-140`). Receipts are kept for good. The boundary is published *before* deleting, in `event-log-pruned-through` (`OrchestrationEventStore.ts:24`).
- Triggered from `DatabaseStorage.ts:152` / `DatabaseMaintenance.ts:247` (a storage maintenance job).
- Consequence (documented): projections **cannot be rebuilt from the log**. The projection tables are the source of truth, and the log serves only reconnect catch-up and command-retry lookup.

#### B.6 Checkpoints (hidden git refs)
- Ref name: `refs/t3/orchestration-v2/checkpoints/<Base64Url(hex(sha256(scopeId))[0..32])>/ordinal/<n>`, plus `…/pre-rewind` before a rollback (`orchestration-v2/CheckpointService.ts:35, 283-295, 861`). Ordinal 0 is the baseline. Ordinal n is captured at the end of the n-th counted run in the scope. Repository scopes (multi-folder projects) get child scopes.
- Capture (`apps/server/src/vcs/GitVcsDriver.ts:1204-1960`) never touches the user's index or branch:
  - It creates a temp index `<gitCommonDir>/t3-checkpoint-index-<uuid>` and a private object dir `objects/tmp_objdir-t3-checkpoint-*`, with `GIT_ALTERNATE_OBJECT_DIRECTORIES` pointing back at the shared store, a quarantine path, and a private git-lfs storage (`:1236-1345`).
  - It seeds the temp index from the user's index (or `read-tree HEAD`), runs `git add -A` with the excludes, `write-tree`, then `commit-tree <tree> -p HEAD [-p turnStart]` with author `T3 Code` (`:1388-1852`).
  - It moves the private objects into the shared store and runs `update-ref <checkpointRef> <commit>` (`:1864-1955`).
  - The capture is serialized per cwd (`withCaptureLock`, `CheckpointService.ts:621`) and has a timeout. Once one capture in a folder times out, later queued captures in that folder are skipped and marked `error` (`:664-676`).
- When: a run becomes `completed` when the provider turn ends. A `checkpoint.capture` effect is then queued on the thread's lane, and until it commits, command decisions treat the run as `waiting` (`01_architecture.md` "Turn completion"; `Orchestrator.ts:1897`).
- Diff: `git diff --numstat -z` (for the file summary stored on the checkpoint) or `--patch` between `<from>^{commit}` and `<toRef>^{commit}`, with options such as `--no-ext-diff --no-textconv --submodule=short` (`GitVcsDriver.ts:2312-2371`; summary at `CheckpointService.ts:747-766`; RPC side `checkpointing/CheckpointDiffQuery.ts:145-220`). Patch blobs may be cached in `checkpoint_diff_blobs`.
- Rollback: `checkpoint.rollback` → `provider-thread.rollback` effect → `CheckpointRollbackService`. It snapshots `pre-rewind`, restores the tree (`restoreCheckpoint`, `GitVcsDriver.ts:2077`) and rolls back the provider conversation. The rollback is refused before any filesystem change if the provider cannot roll back.

#### B.7 Startup recovery
- `EffectOutbox.reconcileAfterProcessLoss` (`EffectOutbox.ts:549-580`) **cancels** pending or running process-bound effects (`provider-turn.*`, `runtime-request.respond`, `provider-session.apply-*`, `:157-165`) and **requeues** running replay-safe ones (`checkpoint.capture`, `provider-thread.rollback`, cleanups, title generation, …, `:144-155`).
- `ProviderRuntimeRecoveryService.reconcile("startup")` (`ProviderRuntimeRecoveryService.ts:990-1100`):
  - For each recovery-candidate thread (selected by index seek), it terminalizes runs, stops sessions and closes requests in one `commitCommand("provider-runtime.reconcile")`, cancelling unsettled process-bound effects.
  - If the "continue after restart" setting is on, it enqueues `provider-runtime.continue` with a `processLossCount` crash allowance.
- `Orchestrator.resumeQueuedRuns` / `recoverDelegatedTasks` (`Orchestrator.ts:3714, 17139`).

---

### C. Client state reconstruction (`packages/client-runtime`)

State library: **Effect Atoms** (`effect/reactivity` `Atom`, consumed through `@effect/atom-react` `useAtomValue`). Domain and stream state lives in `SubscriptionRef`s inside Effect fibers, and atoms are exposed per (environmentId, threadId) (`state/threads.ts:1258-1310`, `apps/web/src/state/threads.ts`). **zustand** is used only for local UI stores (composer drafts, diff panel, command palette, etc.; `apps/web/src/composerDraftStore.ts`, `diffPanelStore.ts`, …).

#### C.1 Connection layer
- One `EnvironmentSupervisor` per environment owns retries (`connection/supervisor.ts`). Backoff is jittered exponential: base 1 s, cap 300 s, reset after 30 s up (`:46-73, 160-163`). The establishment timeout is 15 s and the sync timeout 75 s. Foreground and network-change events *probe* the existing session (`server.probe`) instead of reconnecting (`docs/internals/09_connection-runtime.md`).
- `supervisor.prepared` holds the current session. Subscriptions use `subscribeDynamicWithSession(method, makeInput, {resubscribe, onDefect, …})`, which re-runs `makeInput` on every new session, so the cursor is re-read (`state/shell.ts:646-735`, `state/threads.ts:1086-1240`). **Mutations are never auto-replayed.**
- A socket can be healthy while a subscription fails. The sync status of each subscription is kept separately.

#### C.2 Shell (sidebar) sync, `state/shell.ts`, `state/shellReducer.ts`
1. **Early HTTP load**: `GET /api/orchestration/shell?omitIdleSubagents=1&omitOlderSettled=1[&resumeFrom=<seq>]`. If the client holds cached rows from the same server build and omission flags (`canResumeFrom`), it sends `resumeFrom`, and the server may answer `{resumable:true}` (`shell.ts:554-566`). The client waits only a short grace period for it (`awaitEarlyLoad`, `:610-621`).
2. **Subscribe** `orchestration.subscribeShell` with `afterSequence = snapshot.snapshotSequence`, `requestCompletionMarker:true`, and `httpSnapshotFallback:true` when resuming held rows (`:703-722`).
   - Server side (`ws.ts:1167-1400`, `orchestration-v2/ShellStream.ts:45-86`): the cursor is resumable when `gap ≤ 1000` events, the cursor is ≥ `prunedThrough`, and replay is ≤ 8 MiB of stored payload. Otherwise the server sends a `snapshot` (or `snapshot-required` with fallback, and the client re-fetches HTTP).
3. Apply `thread.updated`/`thread.removed`/`project.*` with `applyShellStreamEvents`. Events with `sequence <= snapshotSequence` are dropped and the cursor advances (`shellReducer.ts:132-270`). Unchanged rows are reused by reference (`reuseUnchangedThreadShells`, `:29`).
4. Snapshots are **partial**: idle subagents and older-settled threads are omitted and loaded on demand (`getThreadFamily`, `getOlderSettledThreads`, search). A missing row ≠ deleted.

#### C.3 Thread detail sync, `state/threads.ts:247-1250`
State: `EnvironmentThreadState {status: empty|cached|synchronizing|live|deleted, data: Option<ThreadProjection>, error, history: {historyCursor, hasMoreHistory, loading, error, expanded, latestLocalTurnOrdinal}, sequence}` (`state/threadState.ts`). Cursor: `lastSequence: SubscriptionRef<number>` (`threads.ts:314`).

Open sequence (`followThread`, `threads.ts:1081-1192`):
1. **Warm cache**: the atom family keeps a `resumeAtom` with `idleTTL = 5 min` (`threadRetention.ts`, `threads.ts:1272-1282`), and there is also persistent storage (`EnvironmentCacheStore`). The initial cursor comes from the cached `snapshotSequence` (`:294-299`).
2. **Cold open**: if the server advertises `threadSnapshotPagination`, the client tries a **socket-only** bounded snapshot first. Otherwise it does `GET …/threads/:id/bounded` via `ThreadSnapshotLoader` (`:1102-1160`). A 404 means "not created yet / deleted", and the client retries with backoff.
3. Subscribe with `{threadId, afterSequence? (only if data held), requestCompletionMarker:true, acceptBoundedSnapshot/DeferredWork/CompactTurnItems/CompactIds (if history-capable), acceptDedupedBodies:true}` (`:1176-1191`).
4. Server decision (`ws.ts:930-1164`):
   - No `afterSequence` → snapshot (bounded window if accepted) + `synchronized` + live tail.
   - With `afterSequence`, the server reads `highWater` (thread's latest sequence) and `prunedThrough`. A cursor past the head, or older than pruning, gets a snapshot. Otherwise it gets replay stats: **> 128 events or > 1 MiB raw payload** → snapshot (`ThreadStream.ts:18-27`). A replay range containing `thread.created` for a thread that still exists → snapshot.
   - Otherwise it replays `(afterSequence, highWater]`, coalesced, and trims to the last `thread.deleted`. A post-read re-check of the pruning boundary guards against a concurrent prune. Encoded size is checked again (`decideThreadResume`, ≤128 events and ≤1 MiB). Then it sends replay + `synchronized` + live from `highWater`.
   - Live = `EventSink.stream` ⇒ subscribe to the live PubSub, then read the global latest sequence, then replay from the DB through it, then the live tail filtered by `> highWater` (`EventSink.ts:1018-1062`). This is gap-free, because publish order is serialized after commit.
5. Client apply (`receiveItems`, `:737-782`): items are buffered until `synchronized` arrives, so a resume renders once. Each item with `sequence <= lastSequence` is skipped (`applyEventsLocked`, `:547-579`). Events go through `applyOrchestrationV2ProjectionEvent` (`state/orchestrationV2Projection.ts:286-480`): an immutable **upsert by id** into the right array (`upsertEntity` is O(n) via `findIndex`; turn items are searched from the end). `visibleTurnItems` gets an ordered insert by `ordinal`. With a partial (paged) window, items below the window watermark are dropped (`shouldDropMissingPartialTurnItem`).
6. Persist: a sliding queue of size 1 writes `{snapshotSequence, projection, history}` to the cache. The cursor and data are committed together only after an apply finishes (`:324-420`).

#### C.4 History paging
- Bounded snapshot policy (`orchestration-v2/threadHistoryPaging.ts:16-34`): newest **10 user turns / 75 rows (500 inside one turn) / 1 MiB**. Control state (active plans, latest todo, pending handoffs, pending request cards) is always included, and the timeline window gets the remaining budget (`buildBoundedThreadProjection`, `:661-787`). Non-visible plans and handoffs are sent with bodies stripped and `detailInTurnItem:true`.
- Older pages: `loadEarlier` → `GET /api/orchestration/threads/:id/history?cursor=…&view=conversation|activity|work[&throughEntryId][&deferWork][&compactIds]` (`threads.ts:784-945`). The cursor is an opaque string, `{v:1, seq, st, si, p, inclusive?}` (`threadHistoryPaging.ts:46-53, 137-196`).
- Results are merged with `mergeOlderHistoryIntoProjection` / `mergeOlderSubagentsIntoProjection` (`state/threadHistoryMerge.ts:86-215`) under the same `applyLock`, so live events and paging do not clobber each other. Pending deferred work is reconciled with `getTurnItem` (`reconcilePendingWork`, `threads.ts:946`).
- `ThreadHistoryController` is a registry of handlers keyed by thread (`state/threadHistoryController.ts`), which lets UI code trigger `loadEarlier`.

#### C.5 Command output
Live command stdout never enters the event store. Adapters push it to `CommandOutputHub` (`orchestration-v2/CommandOutputHub.ts:32-41`: 100 ms frames, a 500 ms replace interval, 16 KiB appends, 64 live and 16 finished commands kept). The client subscribes to `subscribeCommandOutput{threadId,itemId}` only while a command row is expanded (`docs/internals/16_performance-regressions.md`).

---


---

# 3. Provider integration

> 3a = Codex, 3b = Claude Code, 3c = shared abstraction + ACP + other providers + T3 MCP injection. Real recorded provider transcripts (useful as Rust test fixtures) live in `apps/server/src/orchestration-v2/testkit/fixtures/*/` (e.g. `claude_transcript.ndjson`).

## 3a. Codex
### T3 Code: Codex provider integration (reimplementation reference)

Path abbreviations used in citations:
- `A` = `apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts` (9659 lines, the runtime adapter)
- `PR` = `packages/effect-codex-app-server/src/protocol.ts` (JSON-RPC transport)
- `CL` = `packages/effect-codex-app-server/src/client.ts` (typed client over PR)
- `CP` = `apps/server/src/provider/CodexProvider.ts` (probe/snapshot, model list, account)
- `CD` = `apps/server/src/provider/Drivers/CodexDriver.ts` (driver wiring, "existing" setup mode)
- `CHL` = `apps/server/src/provider/Drivers/CodexHomeLayout.ts`
- `CI` = `apps/server/src/provider/CodexInstallation.ts` (managed binary install / local discovery)
- `GEN` = `packages/effect-codex-app-server/src/_generated/{meta,schema}.gen.ts` (generated from Codex's JSON schema)

T3 talks to **`codex app-server`** (the Rust Codex CLI's JSON-RPC server, the same API the Codex IDE extension uses). It does NOT use `codex exec` or `codex proto` for interactive threads. One app-server process is shared by all threads of one provider instance (`A:1876` "One app-server serves every Codex thread on an instance"); per-thread state is passed per-request.

---

#### 1. Spawn

##### 1.1 Executable resolution
- Setting `binaryPath` (default `"codex"`, `packages/contracts/src/settings.ts:733`), `~` expanded via `expandHomePath` (`CD:221`, `A:2081`). Adapter falls back to `"codex"` if empty (`A:1983`).
- Two setup modes (`CD:171`): `"existing"` (user's Codex, default) and `"managed"` (T3-downloaded binary + T3-owned ChatGPT OAuth; `Drivers/CodexManagedProvider.ts`, `CodexManagedRuntime.ts`).
- Managed binary: pinned release table `CI:78-120` (v0.166.0, GitHub `codex-package-<target>.tar.gz` with sha256, targets `aarch64/x86_64-apple-darwin`, `*-unknown-linux-musl`, `*-pc-windows-msvc`), installed under `<baseDir>/tools/codex/<version>/`, pointer `active.json` (`CI:208-210`). Upstream latest checked via `https://registry.npmjs.org/@openai/codex/latest` (`CI:39`) at most hourly. Executable name `codex.exe` on win32 (`CI:211`); validation runs `codex --version` and expects exactly `codex-cli <version>` (`CI:606-607`).
- Local discovery (`CI:498-560`): `resolveCommandPath("codex")` on PATH, rejects paths inside the managed dir, classifies `native` (needs sibling `codex-code-mode-host[.exe]`) vs `launcher` (volta-shim/mise/aqua-proxy, Windows non-.exe or `.bunx`/`.shim` marker, POSIX `#!` shebang). Probe: `codex --version` must match `/^codex-cli (\d+\.\d+\.\d+)$/`, then `codex app-server --help` must succeed, 5 s timeout; result cached in `<baseDir>/caches/codex-local-probe.json` keyed by a fingerprint (realpath:size:mtime:ino:npmVersion).
- Generic spawn resolver `packages/shared/src/shell.ts:1157-1223` (`resolveSpawnCommand`): on non-Windows returns command as-is, `shell:false`. On Windows: resolves via PATH+PATHEXT (cached), if the result is `.cmd`/`.bat` (npm shim `codex.cmd`), builds a single escaped command line and spawns with `shell:true` (avoids Node DEP0190); otherwise `shell:false`.

##### 1.2 Command line
`codexAppServerArgs` (`apps/server/src/provider/codexLaunchArgs.ts:16-21`):
```
codex app-server -c thread_unload_delay_secs=0 <user launchArgs tokenized>
```
- `thread_unload_delay_secs=0` because T3 itself unsubscribes threads (`thread/unsubscribe`) when it unloads them.
- `launchArgs` = per-instance setting, **entirely replaced** by env `T3CODE_CODEX_LAUNCH_ARGS` if set (`codexLaunchArgs.ts:5-11`). Tokenized by `tokenizeCliArgs` (shell-like quoting).
- Shadow-home (authOverlay) mode prepends `-c "marketplaces.openai-bundled.source=\"<shadow>/.tmp/bundled-marketplaces/openai-bundled\""` (`CHL:80-88`).
- Managed mode launchArgs (`CodexManagedRuntime.ts:23-33`), each wrapped as `-c "<json-quoted>"`:
  ```
  model_provider="openai_token_sharing"
  model_providers.openai_token_sharing={name="OpenAI Token Sharing",base_url="https://api.openai.com/v1",model_catalog_url="https://api.openai.com/v1/models",wire_api="responses",requires_openai_auth=false,supports_websockets=false,auth={command="<t3 self>",args=[...,"__managed-chatgpt-token",endpoint,instanceId,environmentId,profileKey],timeout_ms=80000,refresh_interval_ms=0}}
  features.api_key_model_discovery=true
  features.multi_agent=false
  features.multi_agent_v2=false
  ```
- Per-thread config is NOT on the command line; it goes into `thread/start|resume|fork` `config` (see 3.2).

##### 1.3 Environment
- `CODEX_INTERNAL_ORIGINATOR_OVERRIDE="T3 Code"` always set (`CP:607-612`) so the backend tailors the model catalog to this originator (and the shared `models_cache.json` isn't overwritten with plain-CLI models).
- `CODEX_HOME=<homePath>` when configured (`A:1979-1981`; probe path expands `~` explicitly `CP:631-638` because "spawn passes a literal ~ that Codex rejects"). If the instance has no homePath but the process env has `CODEX_HOME`, that is used (`CD:193-197`). Default home `~/.codex` (`CHL:44-47`).
- Env = host env merged with per-instance env (`mergeProviderInstanceEnvironment`, `CD:189`), `extendEnv: true` in probe spawns.
- Managed mode deletes `ACCESS_TOKEN`, `T3CODE_CODEX_LAUNCH_ARGS`, `OPENAI_API_KEY`, `OPENAI_BASE_URL` (`CodexManagedRuntime.ts:118-125`).
- Thread id for child shells: NOT an env var on the process (shared server); passed per thread as config `shell_environment_policy.set.T3CODE_THREAD_ID=<threadId>` plus `mcp_servers.<name>.env.T3CODE_THREAD_ID` for each user stdio MCP server (`A:1726-1737`). (`PortScanner.AGENT_THREAD_ENV` is the variable name; used for port discovery.)

##### 1.4 cwd / process group / kill
- Adapter spawn passes **no cwd** (`A:1781-1810`, `open` at `A:1976`): the app-server inherits the T3 server cwd; per-thread cwd is a request param. Probes use `cwd: input.cwd` (`CP:657-664`).
- `forceKillAfter: "2 seconds"` (SIGTERM then SIGKILL) (`A:1779`, `CP:139`).
- Linux: `AgentScope.wrap` (`apps/server/src/process/agentScope.ts:426-470`) wraps the command in `systemd-run --user --scope --slice=... [--property=OOMPolicy=...]` when a systemd user manager exists (XDG_RUNTIME_DIR + systemd-run + systemctl), unit name `t3code-codex-<uuid8>.scope`, `shared: true`. Otherwise plain spawn.
- Probe processes: after a probe, T3 waits (≤1 min, polling `pgrep -g <pid>` every 500 ms) for Codex's startup git/ssh children before closing the scope (`CP:684-704`).

##### 1.5 Version checks
- `initialize` response `userAgent` is parsed with `/\/(\S+)/` for version (`A:2252`, `CP:756`). Fallback `codex --version` with `/(?:^|\s)v?(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)(?=\s|$)/` (`CP:148`, `CP:829-865`).
- Compatibility ranges from `apps/server/src/provider/model-manifest.json` `compatibility[]` (driver `codex`, T3 `>=0.0.44`): `>=0.156.0` supported, `>=0.149.0 <0.156.0` unsupported, `<0.149.0` broken; recommended `>=0.159.0`. `brokenRuntimeError` (`providerCompatibility.ts:112`) refuses a session when status is broken (`A:2253-2262`). Rollback needs paginated history ("Codex 0.156 cannot revert" legacy history, `A:9497`).

---

#### 2. Transport (PR, CL)

- **Framing**: newline-delimited JSON (JSONL) over child stdin/stdout. Each outgoing message is `JSON.stringify(msg) + "\n"` (`PR` `encodeWireMessage`, ~L112). Incoming: chunks decoded as UTF-8, split on `\n`, trailing `\r` stripped, blank lines ignored, a line > 128 MiB is a transport error (`PR:22`, `PR:454-490`). A final unterminated line at EOF is still parsed.
- **No `"jsonrpc":"2.0"` field** is ever sent. Outgoing request: `{"id":<int>,"method":"...","params":{...}}` (params omitted if undefined, `PR:557-561`); notification: `{"method":"...","params":...}` (`PR:571`); response: `{"id":..,"result":..}` or `{"id":..,"error":{code,message,data?}}` (`PR:152-160`).
- **Ids**: client request ids are integers starting at 1, incremented per request (`PR:173`); pending map keyed by `String(id)`. Server request ids may be number or string (`_internal/shared.ts` `JsonRpcId`).
- **Routing** (`PR:409-415`): has `method` and `id` → server request; has `method` and no `id` → notification; matches `{id, result?, error?}` → response; else parse error (fatal termination).
- **Server requests**: run concurrently on a FiberMap keyed by request id (`PR:385`), max 32 concurrent (`PR:338`, extra ones answered with error `-32001` "overloaded"). Handler failure → error response; handler defect → `-32603` internal error (`PR:352-363`); unknown method → `-32601` (`CL:223`). A `serverRequest/resolved` notification `{requestId}` interrupts the still-running handler fiber (`PR:395-400`) — no response is then sent.
- **Notifications are dispatched inline on the stdout reader** (`PR:401-407`, called from the read loop `PR:477`). Consequence: a notification handler that awaits a client request response deadlocks the reader; the adapter forks such work (comments at `A:7785`, `A:6976`, `A:7527`). A Rust port should keep ordered notification processing but must never block the reader on a response.
- Notification params are schema-decoded; undecodable ones are dropped with one warning per (method, issue kinds) (`CL:157-200`).
- **Termination**: stdout EOF/read error/write EPIPE → all pending requests fail; waits ≤250 ms for exit status to build `ProcessExitedError{code,pid,stderr excerpt}` (`PR:23`, `_internal/stdio.ts` stderr tail 8 KiB, excerpt 512 chars).
- **Handshake** (`A:2208-2265`, guarded by a semaphore because "Codex rejects a second initialize on the shared app-server"):
  ```json
  {"id":1,"method":"initialize","params":{
    "clientInfo":{"name":"T3 Code","title":"T3 Code","version":"<t3 package.json version>"},
    "capabilities":{"experimentalApi":true,
                    "optOutNotificationMethods":["turn/diff/updated"],
                    "extensions":{"<MCP_APP_EXTENSION_ID>":{"mimeTypes":["<MCP_APP_MIME_TYPE>"]}}}}}
  ```
  Response `{codexHome, platformFamily, platformOs, userAgent}` (GEN schema `V1InitializeResponse`). Error `-32600 "Already initialized"` is tolerated. 30 s timeout (`A:333`) → session retired. Then client sends `{"method":"initialized"}` (no params). Probe clients send only `capabilities:{experimentalApi:true}` (`CP:614-626`).
- **Deadlines**: setup/turn-start requests wrapped with a 2-minute deadline; expiry retires the app-server (`A:625`, `A:2290-2302`). Retired session rejects new work (`A:2273-2288`).
- `turn/start` is encoded with a patched schema adding `collaborationMode`, `additionalContext`, `cyberAccessProgram` (generated schema omits them) (`CL:89-98`).
- Some methods are called through `client.raw.request` (untyped, not in generated schema): `thread/sample`, `thread/backgroundTerminals/terminate|list`, `thread/resume`/`thread/read` in places where extra fields (`historyMode`, `status`) are read.

---

#### 3. Client → server methods actually used

| Method | Where | Params as sent |
|---|---|---|
| `initialize` / `initialized` | `A:2218-2263`, `CP:678-679` | see §2 |
| `thread/start` | `A:8450` | `{cwd?, model?, config:{...}, threadSource?:"subagent"}` |
| `thread/resume` | `A:7992`, `A:8488-8560`, `A:3907` | `{threadId, excludeTurns:true, cwd?, model?, config}` |
| `thread/fork` | `A:9552` | `{threadId, excludeTurns:true, lastTurnId?, cwd?, model?, config}` |
| `thread/read` | `A:1263`, `A:9444`, `CodexThreadRevert.ts:39` | `{threadId, includeTurns:false|true}` |
| `thread/turns/list` | `CodexThreadRevert.ts:24` | `{threadId, cursor, limit:min(n,100), sortDirection:"desc", itemsView:"summary"}` |
| `thread/revert` | `CodexThreadRevert.ts:40` | `{threadId, beforeTurnId}` |
| `thread/unarchive` / `thread/archive` / `thread/unsubscribe` | `A:7998`, `A:8753`, `A:8759` | `{threadId}` |
| `thread/name/set` | `A:2407` | `{threadId, name}` (10 s timeout, remembered per thread) |
| `thread/settings/update` | `A:8364` | `{threadId, serviceTier}` (live service-tier change) |
| `thread/compact/start` | `A:8569` | `{threadId}` |
| `thread/inject_items` | `A:2168`, `A:8597` | `{threadId, items:[ResponseItem...]}` |
| `thread/goal/set|get|clear` | `A:8122-8200`, `A:5557` | `{threadId, objective?, status:"active"|"paused"}` |
| `thread/sample` (raw) | `A:2593` | `{threadId, expectedTurnId, prompt:<suggestion prompt>, effort:"low"}` → `{text}` |
| `thread/backgroundTerminals/terminate|list` (raw) | `A:3116-3170` | `{threadId, processId}` / `{threadId, cursor?}` |
| `turn/start` | `A:8069` | see 3.1 |
| `turn/steer` | `A:8690` | `{threadId, expectedTurnId, input:[...]}` |
| `turn/interrupt` | `A:9176` etc. | `{threadId, turnId}` |
| `config/read` | `A:2306`, `A:8029`, `CP:784` | `{cwd?, includeLayers:false}` |
| `model/list` | `CP:518-534` | `{}` or `{cursor}` (paginated via `nextCursor`) |
| `account/read` | `CP:759` | `{}` |
| `account/rateLimits/read` | `CP:711`, `A:5369` | `null` params |
| `account/rateLimitResetCredit/consume` | `CD:369` | `{idempotencyKey}` |
| `skills/list` | `CP:773`, `CP:918` | `{cwds:[cwd], forceReload?:true}` |
| `mcpServerStatus/list`, `mcpServer/tool/call`, `mcpServer/resource/read`, `mcpServer/oauth/login` | `A:8290-8340`, `A:9336-9354` | MCP passthroughs (`{name, threadId}` for oauth login) |
| `feedback/upload` | `A:9306` | `{classification:"bug", includeLogs:true, reason?, threadId}` |

Not used: `review/start`, `account/login/*`, `account/logout`, `command/exec*`, `fs/*`, `thread/list`, `thread/shellCommand`.

##### 3.1 `turn/start` params (`buildCodexTurnStartParams`, `A:913-1026`)
```json
{
  "threadId": "<native thread id>",
  "input": [{"type":"text","text":"..."}, {"type":"image","url":"data:image/png;base64,..."}],
  "additionalContext": {
    "t3_code_orchestration_01": {"kind":"application","value":"..."},
    "t3_code_runtime": {"kind":"application","value":"..."},
    "t3_code_tools": {"kind":"application","value":"..."},
    "t3_code_workspace": {"kind":"application","value":"<extra roots list>"},
    "<appContextKey>": {"kind":"untrusted","value":"..."}
  },
  "cwd": "/abs/project",
  "model": "gpt-6.1-sol",
  "summary": "auto",
  "approvalsReviewer": "user",
  "approvalPolicy": "on-request",
  "sandboxPolicy": {"type":"workspaceWrite","writableRoots":["/extra/root"]},
  "effort": "high",
  "serviceTier": "fast",
  "cyberAccessProgram": "standard",
  "collaborationMode": {"mode":"plan","settings":{"model":"gpt-6.1-sol","reasoning_effort":"high"}}
}
```
- `summary`: user's `model_reasoning_summary` from `config/read` (read per turn, `A:8027-8036`) else `"auto"` (Bedrock rejects `detailed`).
- `effort`: model option `reasoningEffort`, else override `"medium"` for model family `gpt-6-astra` (`CP:252-254`).
- `serviceTier`: option `serviceTier` or legacy boolean `fastMode` → `"fast"` (`apps/server/src/codexModelOptions.ts`); omitted in managed (token-sharing) mode (`A:8050`).
- `approvalPolicy`/`sandboxPolicy`: explicit runtimePolicy override if present, else runtime-mode defaults (§5.4). `workspace.extraRoots` are merged into `writableRoots` only for `workspaceWrite`.
- `collaborationMode` is **always** sent (both `plan` and `default`) so leaving Plan mode clears it (`A:1016-1023`). `developer_instructions` is NOT set (Codex supplies mode text; T3 instructions go in `additionalContext` because "model-catalog mode text overrides client developer_instructions", `CodexDeveloperInstructions.ts`).
- `additionalContext` T3 entries only when the T3 MCP session exists; values capped ~4000 bytes each, orchestration text split into numbered keys (`CodexDeveloperInstructions.ts`). Untrusted app context restored after compaction as `<external_KEY>..</external_KEY>` user messages and application context as `<KEY>..</KEY>` developer messages via `thread/inject_items` (`A:2160-2194`).
- **Input items** (`toCodexInput`, `A:4079-4113`): one `text` item = message text with skill mentions normalized (any currency sign before a token → `$name`, `A:555`, `packages/shared/src/composerInlineTokens.ts:31-35`) plus attachment file paths appended (`providerMessageTextWithAttachmentPaths`); then each native image attachment as `{"type":"image","url":"data:<mime>;base64,<bytes>"}` read from `attachmentsDir` (`A:4052-4077`). `localImage`/`skill`/`mention` item types are not used. Empty input → protocol error. Continuations (resume/capacity retry) send `input: []` (`A:8641-8643`, `A:7715`).
- Before turn/start: `applyThreadName(nativeThreadId, appThread.title)` (`A:8636-8639`).

##### 3.2 Per-thread `config` (`codexThreadRuntimeParams`, `A:1671-1755`)
```json
{
  "tools.update_plan.enabled": true,
  "mcp_optional_startup_grace_ms": 10000,
  "features.concurrent_reasoning_summaries": true,
  "shell_environment_policy.set.T3CODE_THREAD_ID": "<t3 thread id>",
  "mcp_servers.<userStdioServer>.env.T3CODE_THREAD_ID": "<t3 thread id>",
  "mcp_servers.t3-code": {"url":"<endpoint>","tool_timeout_sec":3900,"http_headers":{"Authorization":"Bearer ..."},"disabled_tools":["t3_thread_wait","delegate_task"]},
  "mcp_servers.t3-code-direct": {"url":"...","tool_timeout_sec":3900,"http_headers":{...},"enabled_tools":["t3_thread_wait","delegate_task"],"omit_tools_from":["code_mode","deferred"]}
}
```
User stdio MCP server names come from `config/read` `mcp_servers` entries with a string `command` and no `.` in the name (`A:1686-1699`). Approval/sandbox are NOT set at thread level; they're per turn.

---

#### 4. Server → client notifications handled (`A:5238-7800`)

All projected into T3 provider events: `node.updated`, `turn_item.updated`, `message.updated`, `plan.updated`, `provider_turn.updated`, `runtime_request.updated`, `command_output.delta`, `turn.terminal`, `prompt_suggestion.updated`, `subagent.updated`, etc. (`packages/provider-core/src/server/ProviderAdapter.ts:101-250`). Ids derive deterministically from native ids (`idAllocator.derive.*FromProviderItem`).

| Notification | Handling |
|---|---|
| `turn/started` `{threadId, turn:{id,startedAt}}` | `A:5497-5581`. If turn context exists: resolve its `nativeStartReady`. Else adopt pending root turn for that thread (`pendingRootTurns[threadId]`) → `registerRootTurn` (emits `provider_turn.updated` running). Else goal-continuation hold → new provider turn (ordinal+1). Else orphan goal turn → `thread/goal/set paused` + `turn/interrupt` (forked). Else subagent turn bookkeeping. |
| `turn/completed` `{threadId, turn:{id,status,completedAt,error?{message,codexErrorInfo}}}` | `A:7776-7814`. Restores additionalContext if compaction happened. Status map `completed|interrupted|failed|inProgress` (`A:494-505`); `completed` while T3 is interrupting → `interrupted`. `serverOverloaded` failure → capacity retry (§6.6), else `finalizeCodexTurn` → `turn.terminal`. |
| `item/started` `{threadId,turnId,item,startedAtMs}` | `A:5775-5929`. `contextCompaction`→compaction turn item running; `userMessage`→subagent prompt; `subAgentActivity`→subagent registry; `fileChange`→remember `path→kind` for approval prompt; `agentMessage`→record `phase`/`delivery:"async"`, final-answer ids; `imageGeneration`; `commandExecution`→`command_execution` item + track running (processId); `mcpToolCall`/`dynamicToolCall`→`dynamic_tool` item; `webSearch`→`web_search` item. |
| `item/completed` | `A:5931-6298`. `reasoning`→complete reasoning; `contextCompaction` completed; `commandExecution` (exit code, aggregated output, user-stopped/unknown outcome); `mcpToolCall`/`dynamicToolCall`(+ MCP app capture); `fileChange`→`file_change` item with a synthesized multi-file git patch (`codexMultiFilePatch`, `A:182-209`: adds `diff --git` headers, converts add/delete raw contents into hunks); `webSearch`; `plan`→finalize proposed plan text; `collabAgentToolCall` (spawnAgent etc.)→subagents; `imageGeneration` (savedPath); `imageView`→`dynamic_tool view_image`; `agentMessage`→final text via coalescer; async message with `questions` → user-input request with `responseMode:"message"` (§7). |
| `item/agentMessage/delta` `{turnId,itemId,delta}` | coalesced every 50 ms (`A:362`, `A:4285`) → `node.updated`/`turn_item.updated` (`assistant_message`), `message.updated` on completion. Only the first final-answer item streams; duplicates suppressed. |
| `item/reasoning/summaryTextDelta` (`summaryIndex`) / `item/reasoning/textDelta` (`contentIndex`) | `appendReasoning` → `reasoning` turn item (`A:4250-4283`, `A:5272-5277`). |
| `item/plan/delta` | proposed plan coalescer, 400 ms (`A:364`, `A:4923-4957`). |
| `turn/plan/updated` `{turnId, explanation?, plan:[{step,status}]}` | `todo_list` plan artifact id `turn-plan:<turnId>`, steps `step-N`, status `pending|inProgress→running|completed` (`A:5294-5332`, `A:715-725`). |
| `item/commandExecution/outputDelta` | `command_output.delta {itemId, chunk}`; also for settled turns (background commands) (`A:5252-5270`). |
| `thread/tokenUsage/updated` `{threadId,turnId,tokenUsage:{total,last,modelContextWindow}}` | per-turn accumulator (`CodexTurnTokenUsage.ts`, deltas of thread-wide `total`, fallback `last` on counter reset) + `provider_turn.updated` with `{usedTokens:last.totalTokens,maxTokens:modelContextWindow,...}` (`A:299-312`, `A:5385-5422`). |
| `account/rateLimits/updated` | merge snapshot, fill `resetAt` on usage-limit error items, push to provider snapshot (`A:5334-5362`). |
| `error` `{turnId, willRetry, error:{message,additionalDetails?,codexErrorInfo?}}` | `willRetry:false` → stash as latest failure for the terminal; `true` → retry turn item ("running"), parses "attempt N/M" progress, deadline `PROVIDER_RETRY_BUDGET_MS` then fails it (`A:5598-5689`). Classes: usage_limit / transport_error (codes `http*`, `responseStream*`) / provider_error. |
| `thread/settings/updated`, `model/rerouted` | track actual model/effort/serviceTier (subagent selection, reroutes) (`A:5424-5441`). |
| `mcpServer/startupStatus/updated` | startup phase reporting; failures with sign-in detection (`failureReason=="reauthenticationRequired"` or error text containing `mcp login`) (`A:1052-1058`, `A:5443-5470`). |
| `mcpServer/oauthLogin/completed` | resolves pending MCP sign-in (`A:5472`). |
| `hook/completed` | records stop-hook reason for the turn (`A:5481-5495`). |
| `thread/goal/updated` / `thread/goal/cleared` | goal state (`A:5583-5596`). |
| `thread/closed` | resolves unsubscribe waiters; interrupts running command items of settled turns (`A:5745-5773`). |
| `serverRequest/resolved` | only at protocol layer: cancels the pending handler (§2). |

Ignored (no handler): `thread/started`, `thread/status/changed`, `thread/compacted`, `turn/diff/updated` (opted out at initialize), `item/fileChange/outputDelta`, `item/fileChange/patchUpdated`, `item/reasoning/summaryPartAdded`, `item/mcpToolCall/progress`, `account/updated`, `account/login/completed`, etc.

---

#### 5. Approvals

##### 5.1 Server requests handled (`A:6300-6806`)
All share a pattern: `awaitActiveTurn(turnId)` (no context → error response), build `approval_request` node/turn item + `runtime_request` (pending, `responseCapability:{type:"live",providerSessionId}`), park a `Deferred` in `pendingRuntimeRequests[requestId]`, emit events, await the UI decision via `respondToRuntimeRequest` (`A:9250-9299`), then respond.

| Request | Params (GEN) | Response |
|---|---|---|
| `item/commandExecution/requestApproval` | `{threadId,turnId,itemId,approvalId?,command?,cwd?,reason?,networkApprovalContext?{protocol,host},proposedExecpolicyAmendment?,proposedNetworkPolicyAmendments?,commandActions?,startedAtMs}` | `{"decision":"accept"|"acceptForSession"|"decline"|"cancel"}` (T3 never sends `acceptWithExecpolicyAmendment`/`applyNetworkPolicyAmendment`) |
| `item/fileChange/requestApproval` | `{threadId,turnId,itemId,grantRoot?,reason?,startedAtMs}` (file list comes from the earlier `item/started` fileChange) | same decision set |
| `item/permissions/requestApproval` | `{threadId,turnId,itemId,reason?,permissions:{fileSystem?{read?,write?,entries?},network?{enabled}}}` | accept: `{"permissions":<echo requested>,"scope":"turn"|"session"}`; deny: `{"permissions":{},"scope":"turn"}` (`A:760-773`) |
| `mcpServer/elicitation/request` | `{threadId,turnId?,serverName,mode:"form"|"url",message,requestedSchema?,elicitationId?,_meta?}` | `{"action":"accept"|"decline"|"cancel","content"?,"_meta"?:{"persist":"session"|"always"}}` (`CodexMcpElicitation.ts:180-229`) |
| `item/tool/requestUserInput` | `{threadId,turnId,itemId,isBlocking,autoResolutionMs?,questions:[{id,header,question,options?[{label,description}],isOther?,isSecret?}]}` | `{"answers":{"<questionId>":{"answers":["..."]}}}` (`A:782-796`) |
| legacy `execCommandApproval` | `{conversationId,callId,approvalId?,command:[...],cwd,parsedCmd,reason?}` | `{"decision":"approved"|"approved_for_session"|{"denied":{"rejection":"User declined the request."}}|"abort"}` (`A:728-743`) |
| legacy `applyPatchApproval` | `{conversationId,callId,fileChanges,grantRoot?,reason?}` | same legacy decisions |

Not handled → `-32601`: `item/tool/call` (dynamic tool callbacks), `account/chatgptAuthTokens/refresh`, `attestation/generate` (`GEN meta.gen.ts:118-129`).

##### 5.2 Decision mapping
T3 decisions: `accept | acceptForSession | acceptAlways | decline | cancel`. `acceptAlways` is sent to Codex as `acceptForSession`. On `acceptForSession`/`acceptAlways` T3 also records a per-native-thread allowance for the kind (`command`/`file-change`), keyed with the runtime mode; later requests of that kind auto-answer `acceptForSession` without asking (`A:6322-6327`, `A:2392-2433`) — because "Codex acceptForSession covers only the exact command or patch". Allowance resets when the runtime mode changes (`A:2411-2421`) or the thread unloads (`A:8750`).

##### 5.3 MCP elicitation specifics (`CodexMcpElicitation.ts`)
- URL-mode elicitations are always answered `cancel` (T3 cannot complete them; `decline` would be persisted by Codex's browser plugin as a site denial).
- Form mode: fills content by picking option values matching `once|accept|approve|allow...` for plain accept, or values containing `session`/`always|permanent|forever|persistent` for persistent decisions; boolean "persist" fields set to `decision==="acceptAlways"`; defaults applied; any missing required field → `cancel`.
- Options offered: Cancel, Decline, (Always allow this session), (Always allow), Approve; app name from `_meta.app_name|appName|app|target.*|tool_params.*`, `/^Allow ChatGPT to use (.+?)\?$/`, connector name, or serverName.
- In `full-access` mode form elicitations are auto-accepted (`A:6544`); with no turn context → `cancel`.

##### 5.4 Runtime modes → Codex policy (`codexRuntimeModeTurnDefaults`, `A:851-896`)
| T3 runtimeMode | approvalPolicy | approvalsReviewer | sandboxPolicy |
|---|---|---|---|
| `approval-required` (supervised) | `"untrusted"` | `"user"` | `{"type":"readOnly"}` |
| `auto-accept-edits` | `"on-request"` | `"user"` | `{"type":"workspaceWrite"}` |
| `auto` | `"on-request"` | `"auto_review"` | `{"type":"workspaceWrite"}` |
| `full-access` | `{"granular":{"mcp_elicitations":true,"request_permissions":false,"rules":false,"sandbox_approval":false,"skill_approval":false}}` | `"user"` | `{"type":"dangerFullAccess"}` |
- Full access uses granular rather than `"never"` because `never` "also rejects browser and app access prompts before the client sees them" (`A:883`). In full access, command approvals Codex still raises are auto-`accept`ed unless they carry network context/amendments (`A:6309-6321`).
- `approvalsReviewer` is always sent ("Omitting the reviewer on resume preserves the previous Auto reviewer", `A:1009`).

---

#### 6. Session lifecycle, resume, fork, rollback, steer, interrupt, compaction

##### 6.1 Identity / persistence
- T3 persists `providerThread.nativeThreadRef.nativeId` = Codex thread id (`A:1076-1122`); provider thread id is derived from the native id. Native turn ids stored in `providerTurn.nativeTurnRef`. Codex persists the rollout itself under `CODEX_HOME/sessions` (+ sqlite). Capabilities declare `pendingRequestsSurviveRestart:false`, `approvalCallbacksAreLiveOnly:true` (`A:346-435`).
- `continuationIdentity.continuationKey = "codex:home:<sharedHomePath>"` (`CHL:49-72`): instances sharing a CODEX_HOME (directly or via shadow home) can continue each other's threads.
- Shadow home (`CHL:18-41`, `materializeCodexShadowHome`): a per-account dir whose shared entries (`sessions`, `archived_sessions`, `sqlite`, `shell_snapshots`, `worktrees`, `skills`, `plugins`, `cache`, `logs`, `mcp-oauth-locks`, `thread-writer-locks`, `.sqlite-maintenance.lock`; other top-level entries are linked too per `materializeCodexShadowHome` `CHL:366-455`, not line-by-line verified) are symlinks into the shared home, while `auth.json`, `auth.lock`, `models_cache.json` are private real files; `log`, `memories`, `tmp`, `.DS_Store` are local.

##### 6.2 New thread / resume
- New: `thread/start` (§3) → `providerThread` status idle (`A:8440-8487`).
- Resume (`A:8488-8560`): `thread/resume {threadId, excludeTurns:true, cwd, model, config}` through `resumeNativeThread` (`A:7988-8013`) which on error text `/\bsession \S+ is archived\b|\bcodex unarchive\b/i` calls `thread/unarchive` then retries; on `/\bthread \S+ is closing\b/i` waits (≤10 s) for `thread/closed` then retries. `-32600 "...already has an active writer"` → `ThreadInUseError` ("open in another Codex client", `A:455-468`). Model selection for load strips `reasoningEffort` (effort applies per turn, `A:282-287`).
- Unload: `thread/unsubscribe` (or `thread/archive` when archiving; archive unloads inline without `thread/closed`) (`A:8746-8790`). Unloaded threads with an unconfirmed Stop are finalized `interrupted`.
- Session retirement: event stream failure (app-server exit, initialize timeout, 2-min unresponsive request) makes the session manager drop this runtime; next turn spawns a new app-server.

##### 6.3 Fork (`A:9548-9640`)
`thread/fork {threadId, excludeTurns:true, lastTurnId?, cwd, model, config}`. `lastTurnId` is inclusive (fork up to and including that native turn). If the boundary turn has no native turn id, fork whole thread then `revertCodexThread(forked, N)` where N = terminal provider turns after the boundary (`A:1206-1247`); requires `historyMode:"paginated"`. Archived source → unarchive + retry.

##### 6.4 Rollback (`A:9474-9546`, `CodexThreadRevert.ts`)
- N = count of terminal provider turns (with native refs) after the target turn, or all for `thread_start` (`A:1280-1310`). N=0 → no-op.
- `thread/read` (raw) to get `historyMode` and `status.type` (`notLoaded`); legacy history → error. Not loaded → `thread/resume` first.
- `revertCodexThread`: page `thread/turns/list` desc until N turns seen; `beforeTurnId` = the N-th newest; `thread/revert {threadId, beforeTurnId}` (removes that turn and everything after). Clears token-usage state.

##### 6.5 Steering (`A:8657-8728`)
`turn/steer {threadId, expectedTurnId:<active native turn>, input}`. Waits for in-flight capacity retries / held goal turns first. Since `turn/steer` has no `additionalContext`, new/changed app-context notes are prepended as text `<external_KEY>..</external_KEY>`. Applies service tier change via `thread/settings/update` first.

##### 6.6 Interrupt (`A:8792-9248`)
- Targets: the active root turn + all descendant (subagent) turns in its lineage + goal turns; also settled turns with running background commands.
- Sends `turn/interrupt {threadId, turnId}` per target (waits ≤10 s for `turn/started` on queued turns whose `turn/start` returned `startedAt:null`), terminates tracked background terminals via `thread/backgroundTerminals/terminate`, then waits ≤10 s (`CODEX_STOP_CONFIRM_TIMEOUT_MS`, `A:176`) for `turn/completed`. On timeout returns `InterruptTimeoutError` (UI offers Force stop) and keeps the turn "interrupting"; a later `turn/completed` settles it. `completed` received while interrupting is mapped to `interrupted`.
- Capacity retries: `turn/completed` failed with `codexErrorInfo` code `serverOverloaded` → up to 5 retries, delay `min(2000*2^(n-1), 30000)` ms, re-`turn/start` with `input:[]` and ordinal+1 (`A:7630-7774`). Not for subagents or a literal `/compact`.

##### 6.7 Compaction
`compactThread` → `thread/compact/start {threadId}`; the resulting native turn is adopted through `pendingRootTurns` + `turn/started` (`A:8562-8592`). `contextCompaction` items render as compaction turn items. After `item/completed contextCompaction`, at the following `turn/completed` T3 re-injects its additionalContext with `thread/inject_items` because "Codex drops client developer messages during compaction but only resends additionalContext when it changes" (`A:2160-2194`, `A:7776-7787`).

##### 6.8 Handoff / injected history
`injectHistory` → `thread/inject_items {threadId, items}` where items = `[{type:"message",role:"user",content:[{type:"input_text",text:<context>}]}, ...messages as input_text/output_text]` (`packages/provider-core/src/server/handoffBudget.ts:184-200`); `-32601` → returns false (unsupported) (`A:8593-8617`).

---

#### 7. Plan mode, proposed plans, questions

- Plan mode = `collaborationMode.mode = "plan"` on `turn/start` (`ClientRequest__ModeKind = "plan"|"default"`), `settings:{model, reasoning_effort?}` (`A:1016-1023`). UI toggle enabled via `showInteractionModeToggle:true` (`CP:150-154`).
- Proposed plan card: `item/plan/delta` streams → `proposed_plan` plan artifact + `proposed_plan` turn item (`markdown`, `streaming`), status stays `"active"` "until Implement consumes it"; finalized on `item/completed` `type:"plan"` with `item.text` (`A:4858-4957`, `A:6133-6140`). Implementing = a normal next turn sent in `default` mode.
- Todo list: `turn/plan/updated` → `todo_list` artifact (§4). `tools.update_plan.enabled=true` forced in thread config.
- Blocking questions: `item/tool/requestUserInput` (§5.1) → `user_input_request` node + runtime request kind `user_input`; question ids empty → `question-N`; options with empty descriptions use the label (`A:5136-5236`). Answers map back to native ids, each answer value stringified into an array.
- Async questions: an `agentMessage` with `delivery:"async"` and `questions:[{title, options?}]` → user-input request with `responseCapability:{type:"message"}`, native id `async:<itemId>`; the answer is sent as a new user message (steer if turn active, else new turn) (`A:6208-6247`, `docs/user/providers-codex.md` "Answer questions while Codex works").
- `/goal <objective>` commands are intercepted (`parseGoalCommand`, `A:8623`) and mapped to `thread/goal/*` (objective ≤ 4000 Unicode scalars, `schema.ts`); goal continuation turns are chained into the same run.

---

#### 8. Auth

- **Existing login** (default): Codex's own `auth.json` in CODEX_HOME (or OS keychain). T3 only reads status: `account/read {}` → `{account: {type:"chatgpt",email,planType}|{type:"apiKey"}|{type:"amazonBedrock"}|null, requiresOpenaiAuth}`; unauthenticated message tells user to run `codex login` with that CODEX_HOME (`CP:976-1010`). T3 watches the home dir for `auth.json` changes (250 ms debounce) and re-probes (`CD:406-416`). Plan labels from `planType` (`CP:211-249`). Usage limits only probed when routing is native OpenAI (`codexRoutesSpendNativeQuota`, `CP:133-137`; custom `model_provider`/`openai_base_url` routes detected from `config/read`).
- **Managed ChatGPT** (`CodexChatGptAuth.ts`): T3 runs its own OAuth 2 + PKCE (S256) against `https://auth.openai.com` (discovery `/.well-known/openid-configuration`), loopback redirect `http://127.0.0.1:<port>/auth/callback`, scope `openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`, `resource=https://api.openai.com/v1`, dynamic client `dynamic_agent_client` → registered `oaiapp_*` client id, `ext_agent_host_id`, `agent_name_hint:"T3 Code"` (`CodexChatGptAuth.ts:37-39`, `585-611`). Tokens (+refresh) stored in T3's secret store per instance; manual paste of the callback URL supported for remote machines.
- **Token broker**: Codex obtains the bearer via the `model_providers.openai_token_sharing.auth.command` (§1.2) = T3 re-invoking itself with `__managed-chatgpt-token <endpoint> <instanceId> <environmentId> <profileKey>` (`apps/server/src/binMain.ts:22-24`, `cli/managedChatGptToken.ts`). That CLI connects to a private local socket (Windows named pipe `\\.\pipe\t3-token-<uuid>`, POSIX unix socket chmod 600, `CodexManagedTokenBroker.ts:53-81,207`), sends one JSON line `{"version":1,"instanceId","environmentId","profileKey":<64 hex>}`, receives `{"token":"..."}` line, prints the bare token to stdout (Codex auth.command protocol). Frame limits 1 KiB/64 KiB, 5 s idle, 80 s command timeout (`CodexManagedTokenProtocol.ts`). Managed mode omits `serviceTier` and disables multi_agent features; model catalog fetched from `https://api.openai.com/v1/models` with the access token (`CodexChatGptModels.ts`).
- T3 never calls `account/login/start` and never answers `account/chatgptAuthTokens/refresh`.

---

#### 9. Models and reasoning effort

- Source of truth: `model/list` from the app-server (paginated; `CP:518-534`). Each model: `{model, displayName, isDefault, defaultReasoningEffort, supportedReasoningEfforts:[{reasoningEffort,description}], serviceTiers?|additionalSpeedTiers?, defaultServiceTier?, availableAccessPrograms?.cyber}`.
- Mapped to option descriptors (`mapCodexModelCapabilities`, `CP:256-345`): `reasoningEffort` select (labels none/minimal/low/medium/high/xhigh "Extra High"/max/ultra), default = model default except `gpt-6-astra` → `medium`; `serviceTier` select (`default` "Standard" + tiers; `fast`, `ultrafast` "Even faster, more expensive"); `cyberAccessProgram` select (`standard`, `daybreakBlue`, `daybreakRed`).
- ChatGPT accounts: after first `model/list`, T3 polls `CODEX_HOME/models_cache.json` `fetched_at` (100 ms, ≤3 s) until it is newer than spawn, then lists again (originator-tailored catalog) (`CP:572-604`).
- Overlay catalog `model-manifest.json` providers.codex: current `gpt-6.1-sol` (min Codex 0.153.0), `gpt-6-astra` (0.153.0), `gpt-6-luna` (0.155.0); legacy `gpt-6-sol`, `gpt-5.6-sol|terra|luna`, `gpt-5.5`, `gpt-5.4-mini`, `gpt-5.3-codex-spark`. Models newer than the installed CLI show as "update required" (`codexModelCatalog.ts`). Custom models from settings are appended.
- Probe sequence (`CP:722-827`): spawn → initialize → `account/read` → in parallel `skills/list {cwds}`, models, `config/read {includeLayers:false}` (+ `account/rateLimits/read` if native route), all under a probe deadline (default 20 s, env `T3CODE_CODEX_PROBE_TIMEOUT_MS`).

---

#### 10. Bugs, races, fragile logic

1. **Launch-arg override drops shadow-home override.** `resolveCodexLaunchArgs` replaces the whole instance `launchArgs` with `T3CODE_CODEX_LAUNCH_ARGS` when set (`codexLaunchArgs.ts:9-10`), and it is applied in the adapter *after* `codexHomeLaunchArgs` prepended the `marketplaces.openai-bundled.source` override (`A:2083`, `A:1984-1986`). In authOverlay mode with that env var, bundled plugins resolve against the wrong home. Managed mode deletes the env var for this reason (`CodexManagedRuntime.ts:123`); existing mode does not.
2. **Busy-wait turn lookup on the reader.** `awaitRegisteredTurn` spins up to 1000 `Effect.yieldNow` iterations (`A:2862-2873`) and is called from notification handlers that run inline on the stdout reader (`PR:401-407`, `PR:477`). The turn it waits for may only register after a `turn/start` response that the blocked reader cannot read; every notification for an untracked turn (other clients' turns, late items) burns 1000 yields and then is silently dropped. Port with an explicit "turn registered" signal and a separate dispatch task.
3. **Notification handlers must not await requests** (deadlock): handled by convention (`Effect.forkIn(scope)` comments `A:7785`, `A:6976`, `A:7527`, `A:3442`). Any new handler that inlines a `client.request` hangs the session until the 2-min deadline retires it.
4. **`pendingRootTurns` is keyed by native thread id** (`A:2362`, `A:8051-8055`, `A:5506`): an unrelated `turn/started` on the same thread (goal continuation, another client, compaction racing a user turn) while a `turn/start` is in flight gets adopted as the user's turn. Mitigated only by the attempt-id check after the response (`A:8074-8085`).
5. **`serverRequest/resolved` leaves UI state stale.** The protocol interrupts the handler fiber (`PR:395-400`); the `Effect.ensuring` removes the pending runtime request from the map, but no `runtime_request.updated` (resolved/expired) event is emitted, so the approval card can stay "pending" until turn terminalization cleans up (not verified that terminalization always does).
6. **Rollback counts T3 turns, Codex reverts native turns.** `resolveCodexRollbackTurnCount` counts T3's terminal provider turns (`A:1124-1130`, `A:1280-1310`) but `revertCodexThread` walks native turns (`CodexThreadRevert.ts:24-37`). Turns added to the same thread outside T3 (Codex CLI/IDE using the same CODEX_HOME, untracked goal turns) shift the boundary and remove the wrong turns.
7. **User-input question id collisions**: empty native ids become `question-N` locally but map back to `""` (`A:5155-5160`), so multiple id-less questions collapse to one answer key.
8. **Unanswered requests on unknown turn** respond with a JSON-RPC error via `Effect.orDie` → `-32603` internal error (`A:6303-6308` + `PR:352-363`) rather than `decline`/`cancel`; Codex behavior on an internal error for an approval is undefined from T3's side.
9. **Unhandled server requests** `item/tool/call`, `account/chatgptAuthTokens/refresh`, `attestation/generate` get `-32601` (`CL:206-224`) even though capabilities claim `supportsDynamicToolCallbacks:true` (`A:398`).
10. **Global 32-handler cap** per app-server (`PR:338`) is shared by all threads of the instance; many concurrent pending approvals across threads cause Codex to receive `-32001 overloaded` for further approvals.
11. **Thread name set before every turn** races with the user renaming (`appliedThreadNames` cache; failures forgotten) (`A:2399-2410`).
12. **Version parse from userAgent** `/\/(\S+)/` takes the first `/` segment (`A:2252`); a userAgent with an earlier slash (e.g. originator containing `/`) mis-parses and may wrongly refuse/allow.
13. Windows: `.cmd` shims spawn with `shell:true` and a manually escaped command line (`shell.ts:1209-1222`); killing a `cmd.exe` wrapper with `forceKillAfter` may orphan the real `codex.exe` (I saw no job object / tree kill in this path; the effect ChildProcess killer was not inspected, so treat as a risk to check).

---

#### Minimal Rust reimplementation checklist
1. Spawn `codex app-server -c thread_unload_delay_secs=0 [launchArgs]` with `CODEX_HOME`, `CODEX_INTERNAL_ORIGINATOR_OVERRIDE`; stdin/stdout JSONL, no `jsonrpc` field; integer ids.
2. `initialize` (clientInfo, `experimentalApi:true`, `optOutNotificationMethods:["turn/diff/updated"]`) → `initialized`; check version from userAgent against ranges.
3. Reader task: route responses to oneshots; push notifications to an ordered per-session queue; spawn server-request handlers concurrently; honor `serverRequest/resolved`.
4. `thread/start|resume|fork` with per-thread `config`; per-turn `turn/start` with approvalPolicy/sandboxPolicy/approvalsReviewer/effort/summary/collaborationMode/additionalContext; `turn/steer`, `turn/interrupt`.
5. Project `turn/*`, `item/*`, deltas, plan, tokenUsage, error, rateLimits into your own turn-item model; coalesce deltas (50 ms text, 400 ms plan).
6. Answer approvals with `accept|acceptForSession|decline|cancel` (v2) or legacy `approved|approved_for_session|{denied}|abort`; implement session allowance cache yourself.
7. Rollback via `thread/turns/list` + `thread/revert`; compaction via `thread/compact/start` + re-inject context.

---

## 3b. Claude Code
### T3 Code: Claude Code provider integration (for a Rust reimplementation)

All paths are relative to `~/nsh-research/t3code`. `A:` = `apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts` (15,620 lines). Line numbers are from the shallow clone.

**Source caveat.** `node_modules` is not installed in the clone, so I could not read the SDK's own `sdk.mjs`/`sdk.d.ts`. Anything marked **[SDK-inferred]** comes from what I know of `@anthropic-ai/claude-agent-sdk`'s transport. Check it against the installed SDK before relying on it. Everything else is cited from T3 source, its tests, or the recorded replay fixtures (`apps/server/src/orchestration-v2/testkit/fixtures/*/claude_transcript.ndjson`). Those fixtures hold real CLI frames from Claude Code 2.1.111+.

- SDK dependency: `"@anthropic-ai/claude-agent-sdk": "^0.3.321"` (`apps/server/package.json:38`). Some fixtures were recorded on SDK 0.2.111 (`fixtures/simple/claude_transcript.ndjson` line 1).
- Driver kind: `"claudeAgent"` (`A:261`).
- The architecture is one T3 "provider session" per thread. It holds **one live `query()` (= one `claude` CLI process)** at a time (`queryContext` Ref). It uses streaming input (an AsyncIterable prompt fed by a queue) for multi-turn, and opens a fresh process whenever the policy, model or cwd changes in an incompatible way.

---

#### 1. Launch

##### 1.1 Executable resolution
- Setting `binaryPath` defaults to `"claude"` (`packages/contracts/src/settings.ts:809`). `~` is expanded (`A:15590`).
- **Windows** (`provider/Drivers/ClaudeExecutable.ts:44-79`): the SDK does no PATH/PATHEXT lookup, and Node ≥20.12 cannot spawn `.cmd/.bat/.ps1`. T3 resolves the name through PATH/PATHEXT. If the result is a shim, it probes, relative to the shim's dir:
  1. `node_modules/@anthropic-ai/claude-code/bin/claude.exe`
  2. `node_modules/@anthropic-ai/claude-code/cli.js`
  3. `../Packages/Anthropic.ClaudeCode_Microsoft.Winget.Source_8wekyb3d8bbwe/claude.exe` (WinGet)
  It returns the first file that exists. Otherwise it logs a warning and passes the bare path.
- On POSIX the bare name is passed through. The `/bin/sh` wrapper `exec "$@"` resolves it via PATH (see 1.4).
- `claudeProgramReader` (`ClaudeExecutable.ts:86-100`) records `realpath(resolved binary)` at open. If the realpath later differs (`claude update` repointed the symlink), the next turn may restart the CLI on the new binary when no background work is running (`A:13592-13600`).
- Script entries (`.js .mjs .ts .tsx .jsx`, `A:384`) are run by the SDK through node. Native binaries may be wrapped by an "agent scope" (`agentScope.wrap`, `A:1740-1761`), which sets `executableArgs`.

##### 1.2 Environment (`provider/Drivers/ClaudeHome.ts`)
- `mergeClaudeProcessEnvironment` (`ClaudeHome.ts:62-82`) works as follows:
  - Start from the host env and set `CLAUDE_CODE_ENTRYPOINT="t3-code"`. A `CLAUDE_CODE_ENTRYPOINT` in the settings `env` overrides it.
  - **Delete inherited host-session vars:** `CLAUDECODE, CLAUDE_CODE_SESSION_ID, CLAUDE_CODE_HOST_SESSION_ID, CLAUDE_CODE_CHILD_SESSION, CLAUDE_CODE_SESSION_ATTENDED, CLAUDE_CODE_MESSAGING_SOCKET, CLAUDE_CODE_MESSAGING_TOKEN, CLAUDE_CODE_EXECPATH, CLAUDE_CODE_DESKTOP_APP_VERSION, CLAUDE_EFFORT, CLAUDE_PID, CLAUDE_AGENT_SDK_VERSION`. Also delete `ELECTRON_RUN_AS_NODE`.
  - Merge the per-instance env vars from settings on top.
- `makeClaudeEnvironment` (`ClaudeHome.ts:44-56`): when `homePath` is set, add `CLAUDE_CONFIG_DIR=<resolved>`. It never changes `HOME`, because that would hide macOS Keychain creds.
- Per query, `claudeQueryEnvironment` (`A:2384-2407`) adds:
  - `CLAUDE_CODE_PROMPT_CACHE_TTL=<5m|1h>` for delegated subagent threads.
  - `MCP_CONNECTION_NONBLOCKING=0` on resume. Otherwise the first resumed request goes out before MCP connects.
- Per open, the runner adds (`A:1726-1738`):
  - `CLAUDE_CODE_RESUME_INTERRUPTED_TURN=1` and `CLAUDE_CODE_RESUME_REASON=user_resume` when re-opening for a resume of an interrupted turn, unless the history ends on a failed request (last assistant `model:"<synthetic>"`, `A:1439-1444`).
  - `PortScanner.AGENT_THREAD_ENV=<threadId>`.
- Model alias pin: a custom model named `opus|sonnet|haiku|fable` sets `settings.env.ANTHROPIC_DEFAULT_<ALIAS>_MODEL=<alias>` (`A:2621-2627`). Settings env also carries `CLAUDE_CODE_AUTO_COMPACT_WINDOW` and `ENABLE_TOOL_SEARCH=true` when the user's `ENABLE_TOOL_SEARCH` is `auto`/`auto:N` with 0<N<100 (`A:2409-2420`).

##### 1.3 `query()` options actually passed (`makeClaudeQueryOptions`, `A:2523-2745`; call site `A:13701-13750`)
```ts
query({ prompt: AsyncIterable<SDKUserMessage>  /* Queue-backed, A:1696-1702 */,
  options: {
    model: compiled.apiModelId,              // e.g. "claude-opus-5-5" or "<slug>[1m]" via modelSuffixes
    title: appThread.title,                   // goes in the initialize control request, not argv (test A.test:3453-3458)
    tools: {type:"preset",preset:"claude_code"} | ["Read","Glob","Grep"] (read-only sandbox),
    permissionMode,                           // see §3.4; launch-arg --permission-mode/--dangerously-skip-permissions override
    includePartialMessages: true,             // always
    promptSuggestions: bool,                  // false for delegated threads
    effort?: "low"|"medium"|"high"|"xhigh"|"max",   // after effortMap; omitted for "ultrathink"
    sessionId: <uuid> | resume: <uuid>,       // exactly one (A:2588-2590)
    resumeSessionAt?: <assistant msg uuid>,   // rollback head / trailing-abort trim
    allowedTools?, disallowedTools?,          // MCP pre-approval: "mcp__t3-code__*" or read-only list (A:2822-2880)
    canUseTool,                               // ALWAYS installed (A:13743), even under bypass
    allowDangerouslySkipPermissions?: true,   // only with bypassPermissions
    thinking?: {type:"adaptive", display:"summarized"}, // unless thinking=false or --thinking-display omitted
    settings: {...launchArg --settings JSON, ...{alwaysThinkingEnabled,fastMode,ultracode}, showThinkingSummaries:true,
               autoCompactWindow?, env?:{...}},   // object, or the file path string unchanged
    onUserDialog, onElicitation, supportedDialogKinds: ["resume_return"],
    pathToClaudeCodeExecutable: binaryPath, env, mcpServers?: {"t3-code":{type:"http",url,headers:{Authorization},timeout:3_900_000}},
    systemPrompt: {type:"preset", preset:"claude_code", append: <T3 runtime instructions> + launch-arg append text},
    hooks?: { SubagentStart:[{hooks:[cb]}] /*device instructions*/, Stop:[{hooks:[onStop]}] /*goal loop guard*/ },
    plugins?: [{type:"local",path}],          // from --plugin-dir
    extraArgs?: {...remaining launch args, "thinking-display":"summarized"},
    additionalDirectories: [cwd, ...workspace.extraRoots, attachmentsDir, ...--add-dir],
    cwd,
    spawnClaudeCodeProcess,                   // custom spawner (1.4)
}})
```
- **Not passed** (`grep`): `settingSources`, `maxThinkingTokens`, `abortController`, `forkSession`, `persistSession`, `enableFileCheckpointing`, `maxTurns`, `strictMcpConfig`. The SDK default for `settingSources` decides which settings files load. Only `resolveSettings()` calls pass `["user","project","local"]`, or `["project","local"]` for a custom home (`A:6057,6090`).
- Launch-arg folding (`A:2560-2571`): `parseCliArgs(settings.launchArgs)` pulls out `add-dir`, `plugin-dir`, `permission-mode`, `dangerously-skip-permissions`, `append-system-prompt(-file)` and `settings`. The rest become `extraArgs`, e.g. `--chrome`. Append text is file-text then inline text, because "Claude Code overwrites its CLI append text with the SDK's initialize appendSystemPrompt" (`A:2572-2579`).
- **MCP servers go over stdin, never argv/env.** `mcpServers` is stripped before `query()` (`A:1703-1704`). After `initializationResult()`, with a 5 min boot timeout, T3 calls `queryRuntime.setMcpServers(mcpServers)` with a 90 s timeout (`A:1855-1895`). On the wire that is control request `mcp_set_servers` (test `A.test:3449-3451`). A registration timeout reopens once, with a **fresh `sessionId`** when not resuming (`A:2213-2229`).
- `process.emitWarning` is monkey-patched around `query()` to drop `CLAUDE_SDK_CAN_USE_TOOL_SHADOWED` (`A:1600-1618`).

##### 1.4 Spawning (`A:1774-1829`, `spawnClaudeProcessGroup` `A:429-481`)
**POSIX:** `/bin/sh -c <script> t3-claude <cwd> <command> <args...>`, with `stdio: [pipe,pipe,pipe,pipe(fd3),pipe(fd4)]` and `detached:true` (new process group). The script (`A:414-421`):
```sh
read -r _ <&4 || exit 1        # gate: wait until server registered the pid
exec 4<&-
{ trap '' TERM; read -r _ <&3; kill -TERM 0; sleep 5; kill -KILL 0; } </dev/null >/dev/null 2>&1 &   # watchdog: EOF on fd3 (server died/closed) kills the group
cd -P -- "$1" || exit 125      # chdir in the child, so a hung mount can't freeze the server
shift; exec "$@" 3<&-
```
- On child `exit`: `SIGKILL` the whole group (MCP grandchildren). The server writes `ready\n` to fd4 after `providerProcesses.register(pid)`, so a restarted server can reap orphans. stderr is always drained (2 KB tail, used only before the first message).
- A descendant tracker polls `ps -A -o pid=,ppid=,pgid=,lstart=` every 1 s while the CLI works. After exit it kills recorded descendants that escaped the group (setsid daemons), matched by pid+start time (`A:1642-1685`).

**Windows:** `SpawnWorker.spawnPiped(command,args,{cwd,env,signal,windowsHide:true})` runs CreateProcess off the main thread (`A:1796-1803`). There is no process group, no descendant tracking, no fd gate, and stderr goes to `options.stderr` (unset, so it is dropped). See §8.

##### 1.5 Resulting CLI argv / wire **[SDK-inferred]**
The SDK's ProcessTransport runs roughly:
`<claude> --output-format stream-json --verbose --input-format stream-json --model <m> --permission-mode <mode> --permission-prompt-tool stdio [--session-id <u>|--resume <u>] [--resume-session-at <uuid>] --include-partial-messages [--allowedTools a,b] [--disallowedTools ..] [--tools default|a,b] [--add-dir d]... [--settings <json>] [--allow-dangerously-skip-permissions] [--plugin-dir p] [--effort x] [--thinking ...] [--<extraArg> <v>]...`
- With `canUseTool` set, the SDK adds `--permission-prompt-tool stdio`. T3 always sets `canUseTool`.
- `systemPrompt.append`, `hooks` (as callback ids), `title` and `agents` travel in the `initialize` control request, not argv. T3's test confirms this for `title`, and that `--title` and `--mcp-config` are absent from argv (`A.test:3453-3459`).

**Control protocol (stdin/stdout NDJSON)**, confirmed by T3's fake CLI (`A.test:3331-3376`):
```json
// SDK -> CLI
{"type":"control_request","request_id":"req_1_ab","request":{"subtype":"initialize","title":"Existing T3 title", "...":"hooks, appendSystemPrompt, ..."}}
{"type":"control_request","request_id":"req_2_cd","request":{"subtype":"mcp_set_servers","servers":{"t3-code":{"type":"http","url":"http://127.0.0.1:43123/mcp","headers":{"Authorization":"Bearer …"}}}}}
// CLI -> SDK
{"type":"control_response","response":{"subtype":"success","request_id":"req_2_cd","response":{"added":[],"removed":[],"errors":{}}}}
// user prompt line (what T3 offers; A.test fixture prompt.offer)
{"type":"user","message":{"role":"user","content":"..."},"parent_tool_use_id":null,"uuid":"adf55c08-…","priority":"next"}
```
SDK→CLI subtypes T3 uses:
- `initialize`, `interrupt`, `set_model{model}`, `set_permission_mode{mode}`, `apply_flag_settings{settings:{effortLevel,fastMode,ultracode}}`, `stop_task{task_id}`, background-tasks (`Query.backgroundTasks(toolUseId)`), `mcp_status`, `mcp_set_servers`, `mcp_reconnect`, read-MCP-resource.
- Raw ones sent through the private `Query.request` (`A:1035-1054`): `rename_session{title,source:"host"}`, `mcp_authenticate{serverName}` → `{authUrl,requiresUserAction}`, `mcp_oauth_callback_url{serverName,callbackUrl}`, `mcp_call{tool,arguments,result_not_for_model:true}`, `list_permission_rules` → `{state:{rules,errors}}`.

CLI→SDK requests, mapped by the SDK to callbacks **[SDK-inferred names]**:
- `can_use_tool{tool_name,input,permission_suggestions,decision_reason,tool_use_id,…}` → `canUseTool`.
- `hook_callback{callback_id,input}` → hooks.
- Elicitation → `onElicitation`; user dialog → `onUserDialog`.
- `control_cancel_request{request_id}` → aborts the callback's `AbortSignal`.
- Closing stdin ends the CLI.

---

#### 2. Inbound message types and mapping
Real frame types seen in the fixtures (counts in parentheses): `system:init`(85), `assistant`(208), `user`(64), `result:success`(83), `result:error_during_execution`, `stream_event` (`message_start`, `content_block_start{text|thinking|tool_use}`, `content_block_delta{text_delta|thinking_delta|input_json_delta|signature_delta}`, `content_block_stop`, `message_delta`, `message_stop`), `command_lifecycle`(37), `rate_limit_event`, `system:{status,api_retry,compact_boundary,hook_started,hook_response,task_started,task_progress,task_updated,task_notification,background_tasks_changed,commands_changed,thinking_tokens}`.

Processing pipeline:
- Each frame runs under `sdkMessageLock` (1 permit) (`A:13840`).
- Pre-tap: record root uuids, first-init state, permissionMode, modelUsage ledger (`A:13842-13915`).
- `handleSdkMessage`, the prompt-echo gate (`A:12737-12885`).
- `handleRoutedSdkMessage`, which releases buffered subagent frames (`A:12699-12712`).
- `handleSdkMessageFrame` (`A:11095-12670`).

| Frame | Handling |
|---|---|
| `system/init` | Emitted at **every native turn start** (`isClaudeTurnStartMessage`, `A:4147`). On the first per process, store `permissionMode`, `capabilities`, `claude_code_version` and trigger `onSessionInit(cwd)`. `mcp_servers` with status `pending/needs-auth` arm the MCP sign-in notice. If `session_id` ≠ requested id (e.g. after `/clear`), T3 records the remap in `clearedNativeSessions` (`A:13871-13878`). |
| `stream_event` root (`parent_tool_use_id` null) | `message_start`: reset block maps, take usage, emit context usage (with `requestedAt` for the cache TTL). `content_block_start{thinking}` → reasoning item id `<msgId>:thinking:<n>`; `{text}` → assistant item `<msgId>:text:<n>`. `thinking_delta` / `text_delta` → append to the 50 ms coalescer (`ProviderTextDeltaCoalescer.ts`, `A:9593,9683`). `content_block_stop` → complete the item. `message_delta.usage` is merged cumulatively (`A:358-382`). **`input_json_delta` and `tool_use` block starts are ignored**: a tool card appears only on the `assistant` snapshot (`A:11357-11460`). |
| `assistant` root | A snapshot per completed content block. Text blocks reconcile with streamed items by index. Unstreamed text is published whole. Thinking blocks complete reasoning items. `tool_use` blocks start tool items: `TodoWrite` → `todo_list` plan, `Agent` → pending subagent launch, others → `ensureToolCallStarted` (`A:12198-12241`). Sets `nativeMessageCursor = message.uuid`, the rollback/fork cursor (`A:11556`). Sets `executedModel` unless `model:"<synthetic>"` (CLI notices). `error:"rate_limit"` / `"authentication_failed"` feed the failure hint. |
| `assistant/user` with `parent_tool_use_id` | Subagent frames are buffered until `task_started` registers the owning task (cap 256 frames, 64 launches). They are projected into a **child thread** (subagent conversation messages and thinking). |
| `user` (`tool_result` blocks) | `settleToolCall` (`A:9274-9325`): status from `tool_result.is_error` and `tool_result_meta[].non_execution_kind` (`cancelled/interrupted/user-rejected`, `A:4683-4698`). MCP app capture. Task-tool lists (`TaskCreate/TaskUpdate/TaskList`) re-read the persisted list. |
| `system/status` | `status:"requesting"` arms an "unanswered request" watchdog (`PROVIDER_RETRY_BUDGET_MS`) and reports startup phase "connecting". `status:"compacting"` → compaction item `running`. `compact_result:success/failed` closes it (`A:11591-11626`). `permissionMode` updates the live mode. |
| `system/compact_boundary` | Compaction item `completed` with `beforeTokenCount=pre_tokens` and `afterTokenCount=post_tokens`. Resets app-context delivery and the attachment budget (`A:11638-11721`). |
| `conversation_reset` | Usage set to 0; app context forgotten (`A:11629-11636`). |
| `system/api_retry` | Provider-retry turn item `running` (attempt/max/delay). A failure fires after the retry budget. Cleared by the next non-synthetic assistant or `message_start` (`A:11760-11807`). |
| `system/informational` (level≠info), `model_refusal_fallback` | `system_notice` turn item. Stop-hook `"Stop says: "` notices are suppressed (`A:11723-11758`). |
| `rate_limit_event` | `onUsageLimits` → provider snapshot. `status:"rejected"` without overage → usage-limit `system_notice`, then a `usage_limit` failure class on result (`A:11143-11216`). |
| `system/task_started` | `local_bash` foreground with a matching Bash tool → **tail `<tmp>/claude-<uid>/<proj>/<session>/tasks/<task_id>.output`** every 250 ms as `command_output.delta` (`ClaudeTaskOutputTail.ts`, `A:11928-11966`). Other task types → subagent node (child thread, model, effort, workflow). |
| `task_progress / task_updated / task_notification / background_tasks_changed` | Subagent node updates and outcome (`completed/failed/stopped`). Background-task roster (`pendingBackgroundTasks`). Between turns these are buffered for "wake" turns (§4.7). |
| `command_lifecycle{command_uuid,state:queued|started|completed|cancelled|discarded|refused}` | Prompt and steer acceptance (§4.4). `completed` without `started` → turn failed "acknowledged but never started" (`A:12814-12831`). |
| `prompt_suggestion` | `prompt_suggestion.updated`, only after the turn settled and no turn is active (`A:11106-11126`). |
| `system/session_title_changed` + `result.local_command:"rename"` | T3 thread rename (`A:12052-12076`). |
| `result` | Ends the turn (§2.1). |

##### 2.1 `result` → terminal status (`A:4953-4984`, `A:12494-12668`)
Real shape (fixture, trimmed):
```json
{"type":"result","subtype":"success","is_error":false,"api_error_status":null,"num_turns":2,"result":"STARTED",
 "stop_reason":"end_turn","terminal_reason":"completed","user_message_uuid":"adf55c08-…","user_message_uuids":["adf55c08-…"],
 "origin":{"kind":"task-notification"} /*only on wake turns*/, "usage":{…,"iterations":[{…,"type":"message"}]},
 "modelUsage":{"claude-sonnet-4-6":{"inputTokens":7,"outputTokens":178,"cacheReadInputTokens":36734,"cacheCreationInputTokens":38099,"contextWindow":200000,"maxOutputTokens":32000}},
 "fast_mode_state":"off","permission_denials":[],"session_id":"…","uuid":"…"}
```
- `terminal_reason ∈ {aborted_tools, aborted_streaming}` → `interrupted`.
- `subtype:"success"` → `failed` when any of these hold: `api_error_status` is 529 or 429, `terminal_reason` maps to an error (`api_error, malformed_tool_use_exhausted, budget_exhausted, structured_output_retry_exhausted, tool_deferred_unavailable, turn_setup_failed, blocking_limit, rapid_refill_breaker, prompt_too_long, image_error, model_error`, `A:4916-4947`), or `is_error` with a hint. Otherwise `completed`.
- Non-success subtypes are matched on `errors[]` text: contains "interrupt" → interrupted, "cancel" → cancelled, else failed. `[ede_diagnostic]` lines are stripped from user-facing errors (`A:4903-4911`).
- Turns T3 interrupted are always `interrupted`.
- Failure classes: `usage_limit` (with `resetAt` taken from rate-limit info, or parsed from gateway text `"(reset after 1h2m3s)"`), `provider_error`, `transport_error`. There is special text for a 403 and for proxies that rewrite the version (`A:5140-5225`).
- Fallback reply: if nothing streamed, `result.result` becomes the assistant message (`A:12477-12492`).
- Token usage: `normalizeClaudeTurnTokenUsage` (`provider/ClaudeTurnTokenUsage.ts`) sums steered segments. A per-model `modelUsage` ledger delta (it grows for the process lifetime) gives per-turn billing (`ClaudeTurnTokenUsage.ts:66-127`).

---

#### 3. Approvals

##### 3.1 `canUseTool` (`A:12887-13154`)
Signature `(toolName, input, {signal, suggestions, decisionReason, matchedAskRule, suppressAlwaysAllowRule, title, description, toolUseID, requestId})`.
1. No active turn → `{behavior:"deny", message:"Claude V2 adapter has no active turn…"}`.
2. Start the tool row (except `Agent`) so the card has a parent.
3. **`AskUserQuestion`** → T3 `user_input` runtime request. Questions are parsed from `input.questions[]`: `{question, header, options[{label,description,preview?}], multiSelect}`; id = question text (`A:5712-5760`). The result is
   `{behavior:"allow", updatedInput:{questions, answers:{[questionText]: "label" | "l1, l2"}}, toolUseID}` (`A:13001-13010`, `A:5762-5775`). On abort: deny "User cancelled tool execution."
4. **`ExitPlanMode`** → emit a `proposed_plan` plan artifact from `input.plan` markdown (`A:5959-5966`). Then **always deny** with
   `"The client captured your proposed plan. Stop here and wait for the user's feedback or implementation request in a later turn."` (`A:13013-13033`). Implementation is a new T3 turn in another interaction mode. The live process is restored from `plan` via `set_permission_mode(openedPermissionMode)`, falling back to `default` if bypass is refused (`A:13627-13645`).
5. Plan mode + `approvalPolicy:"never"` → deny.
6. `!requiresClaudeApproval` (no callback wanted and not bypass) → `{behavior:"allow", updatedInput: input}` (`A:13046-13052`).
7. Under bypass, Bash "dangerous rm" safety asks (regex on `decisionReason`) are auto-denied with a long instruction message (`A:13054-13073`).
8. Otherwise create a T3 approval runtime request. The prompt is `input.command`, else title, description, reason, or a summary. Options are Decline/Approve only when `suppressAlwaysAllowRule` is set (`A:4706-4743`). Await the `Deferred` or the abort signal.
9. Response (`permissionResultFromDecision`, `A:4745-4787`):
```json
{"behavior":"allow","updatedInput":{…original…},"toolUseID":"toolu_…","decisionClassification":"user_temporary"}
{"behavior":"allow","updatedInput":{…},"toolUseID":"…","decisionClassification":"user_permanent",
 "updatedPermissions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}]}   // acceptForSession: suggestions re-targeted to destination "session"; setMode removed under an admin ceiling
// no suggestions -> [{"type":"addRules","rules":[{"toolName":"Bash"}],"behavior":"allow","destination":"session"}]
{"behavior":"deny","message":"User declined tool execution.","toolUseID":"…","decisionClassification":"user_reject"}
{"behavior":"deny","message":"The user declined this tool call and said:\n<feedback>",…}
{"behavior":"deny","message":"User cancelled tool execution.","interrupt":true,…}   // decision "cancel"
```
Real CLI request as recorded (fixture `tool_call_denied_write`):
`{"toolName":"Write","input":{"file_path":"…","content":"approval fixture"},"options":{"suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}],"displayName":"Write","description":".claude-denied-write.txt","toolUseID":"toolu_016…"}}`.
`respondToRuntimeRequest` resolves the Deferred (`A:15333-15386`).

##### 3.2 Request kinds
Tool → request kind / item type (`A:3965-4009`):
- `bash` → command / command_execution
- `edit, multiedit, write, notebookedit` → file-change / file_change
- `read, glob, grep, ls` → file-read / dynamic_tool
- `webfetch, websearch` → web_search
- everything else → dynamic_tool / command

##### 3.3 MCP elicitation and user dialogs
- `onElicitation` (`A:13156-13318`): only form-mode schemas T3 can fill are shown, with schema `default`s stripped. Options are Cancel / Decline / Always allow this session / Approve. A session grant is keyed by `serverName\0message`. It never returns null.
- `onUserDialog` with `dialogKind:"resume_return"`: the "resume large idle session" prompt, routed as an AskUserQuestion. Returns `{behavior:"completed", result:"compact"|"continue"|"never"}`. Settings `idleCompaction:"off"` → `continue` (`A:13320-13375`).

##### 3.4 Runtime mode → `permissionMode` (`A:3706-3753`, `A:3773-3817`)
| T3 policy | Claude mode |
|---|---|
| interactionMode `plan` | `plan` (callback always installed) |
| approvalPolicy `never` + sandbox readOnly | `dontAsk` (tools = Read/Glob/Grep) |
| approvalPolicy `never` + dangerFullAccess/externalSandbox | `bypassPermissions` |
| approvalPolicy `never` + workspaceWrite/none | approval-required→`dontAsk`, auto-accept-edits→`acceptEdits`, else `bypassPermissions` |
| explicit approvalPolicy ≠ never | `default` |
| runtimeMode `approval-required` | `default` |
| `auto-accept-edits` | `acceptEdits` |
| `auto` | `auto` |
| `full-access` | `bypassPermissions` + `allowDangerouslySkipPermissions:true` |

- If settings contain `permissions.disableBypassPermissionsMode:"disable"`, bypass is downgraded to `auto`, or to `default` when `disableAutoMode`. T3 resolves this from SDK `resolveSettings`, the home `settings.json`, `remote-settings.json` and the launch `--settings`. A one-time `system_notice` follows (`A:6045-6074`, `A:14182-14231`).
- The permission mode is part of `queryPolicyKey`. Changing it **restarts the CLI** (with `resume`) unless only plan→opened needs restoring. `supportsRuntimeModeSwitchInSession:false` (`A:630`).

---

#### 4. Sessions, resume, fork, rollback, interrupt, steering

##### 4.1 Identity and persistence
- `ensureThread` allocates a UUIDv4 native session id up front (`A:14904`). It is persisted as `providerThread.nativeThreadRef.nativeId`.
- The first open uses `sessionId`. Later opens use `resume` when any of these hold: the id was opened in this server lifetime (`openedNativeThreads`), the thread has persisted turns (`nativeThreadHasTurns ?? providerTurnOrdinal>1`), or there is a pinned head (`A:13695-13700`). Without this the CLI fails with "already in use".
- `resumeThread` after a restart checks `~/.claude/projects/*/<id>.jsonl`. Missing/empty transcripts fail, and the orchestrator then hands off to a fresh session with a summary (`A:14934-15012`). "No conversation found with session ID:" on stderr/result → `ClaudeConversationNotFoundError` (`A:410,1963-1966`).
- Idle release: `ProviderSessionManager` closes sessions idle for 2 min when `supportsNativeResume` (`orchestration-v2/ProviderSessionManager.ts:81-87`). The next turn reopens with `resume`.
- On every resume open, the runner reads the full history (`getSessionMessages` in a worker, §4.8). It trims trailing `"[Request interrupted by user]"` rows and denied-tool rows into `resumeSessionAt` (`A:1446-1495`, `A:1706-1724`).

##### 4.2 Rollback (`A:15388-15461`, `A:3164-3211`)
- There is no file checkpointing in Claude. T3 checkpoints the filesystem itself (`appCanCheckpointFilesystem`).
- Conversation rollback is done by setting `providerThread.nativeConversationHeadRef = <assistant uuid>` (the target turn's `nativeTurnRef.nativeId`, which is the last root assistant `uuid`). The next open passes `resumeSessionAt` = that head and `resume`.
- Rolling back to thread start allocates a new session id, unless the thread was forked (then the head is `forkHeadMessageId`).
- Rolled-back turns stay in Claude's jsonl, and resumes keep pinning the head.

##### 4.3 Fork (`A:15463-15532`, runner `A:2231-2290`)
- SDK `forkSession(sourceId, {upToMessageId?})` → `{sessionId}`. Then `getSessionMessages(new).at(-1).uuid` = `forkHeadMessageId` (the SDK remaps uuids).
- With a custom `CLAUDE_CONFIG_DIR`, this runs in a child process (`claude-history-worker.ts` / `claudeHistoryWorker.ts`, argv `[method, sessionId, jsonOptions]`, env + `ELECTRON_RUN_AS_NODE=1`, 30 s timeout), because the SDK reads `CLAUDE_CONFIG_DIR` from `process.env`.
- For Rust: implement transcript copy and uuid remap yourself, or shell out to Node.

##### 4.4 Prompt identity and echo gating (`A:12737-12885`)
- Each user prompt gets a fresh UUID `uuid` (`A:13982-13993`). Claude ignores uuids it has already recorded.
- The CLI acknowledges prompts through `command_lifecycle{command_uuid}` and through `user_message_uuid(s)` on `result`/`thinking_tokens`.
- Root frames that arrive before the echo are held (`heldRootFrames`). A result whose echoed uuids do not include this prompt is a **foreign turn**: a background wake turn that started before the prompt was read. It is dropped if `num_turns===0`, or buffered as a wake.
- `promptEchoMode` (`unknown|acknowledged|early|result_only`) is learned per process.

##### 4.5 Steering (`steerTurn`, `A:14585-14745`)
1. Background running foreground subagents via `backgroundTasks(toolUseId)` (≤5 s deadline), so the steer is not blocked.
2. Apply a live model change if one is possible.
3. Offer `SDKUserMessage{priority:"next", uuid}` on the same stdin queue (`A:14653`). `"now"` would cancel in-flight tools.
4. `steer.updated pending`. Then `completed` on `command_lifecycle started` or an echo (`A:9793-9844`). `cancelled/discarded/refused` → the steer stays queued for the orchestrator as the next turn, and the attachment budget is refunded.
- If a `result` arrives while steers are pending and it echoes uuids, it is **held** (`heldResult`). The final answer is published and the turn waits for the steer's own result (`A:12636-12658`).
- An `aborted_streaming/aborted_tools` result on a steered turn is a segment boundary, not an end (`A:12512-12534`).

##### 4.6 Interrupt (`A:14434-14520`)
- `query.interrupt()` with a 10 s timeout, then **`query.close()` (kills the CLI) on every Stop**. Then wait up to 10 s for exit; otherwise raise `ProviderAdapterInterruptTimeoutError` (UI offers Force stop).
- The next turn reopens with resume. The `CLAUDE_CODE_RESUME_INTERRUPTED_TURN` env and `resumeTurn` send an **empty-content** user message with a uuid (`A:14068-14074`).
- `requestRuntimeRestart` with no active turn just closes the live query.

##### 4.7 Background tasks, subagents, wakes
- Agent/Task tool → `task_started` (`task_type`, `is_backgrounded`, `spawn_depth`, `tool_use_id`, `prompt`, `description`). Subagents project to child threads. Workflow tasks (`task_type:"local_workflow"`) parse `workflow_progress` (`claudeWorkflowProgress.ts`).
- Stopping uses `stop_task{task_id}`, and the outcome is confirmed by `task_notification` within 2 s (`A:15268-15331`).
- Between turns the CLI starts turns by itself when background work ends (result `origin:{kind:"task-notification"}`). T3 buffers those frames (`wakeBuffers`) and asks the orchestrator for a continuation turn (`continuationRequests.offer`, `A:10651-10709`). The continuation `startTurn` has `promptUuid=null`. It replays the buffer instead of sending a prompt (`A:14253-14415`), and finalizes after 2 min if no native wake ever started.
- A model or setting change that needs a new CLI while background work runs fails with `ClaudeBackgroundWorkBlocksQueryReplacementError` (`A:13648-13658`).
- Stale process: if a terminal `claude --resume` appended to the jsonl since the process last emitted, the live process is replaced (tail read of 2 MB, then 32 MB, `A:13434-13475`).

##### 4.8 Compaction
- `/compact` is just a prompt (`compactThread`, `A:15017-15021`). Auto-compaction is driven by settings `autoCompactWindow` plus env `CLAUDE_CODE_AUTO_COMPACT_WINDOW`.
- T3 reports `autoCompactThreshold = window - 33_000` (`A:281-296`) and resolves the window through the settings precedence chain (`A:6158-6222`).
- Idle "resume return" dialog: §3.3.

---

#### 5. Models, effort, thinking, fast mode, usage
- The catalog comes from `apps/server/src/provider/model-manifest.json` → `providers.claudeAgent` (models like `claude-opus-5-5` with aliases `opus`, profile `opus-5-5`, `adapter.claudeCode.minVersion`). Profiles define `optionDescriptors` (effort `low…max`, `ultracode`, `ultrathink`; `fastMode` boolean; `thinking`; `contextWindow`) and `adapter.claudeCode.{effortMap, modelSuffixes, contextWindowTokens, fixedContextWindowTokens}` (`provider/ClaudeModelManifest.ts:6-49`).
- `compileClaudeModelSelection` (`apps/server/src/claudeModelOptions.ts:31-81`) returns:
  - `apiModelId`: catalog `apiModel` (from routes) or slug, plus a suffix from `modelSuffixes`, e.g. `[1m]`.
  - `effort`: after `effortMap`, so `ultracode`→`xhigh` and `ultrathink`→none.
  - `promptEffort`: `"ultrathink"` → prompt prefixed with `"Ultrathink:\n"` unless it is a slash command (`packages/shared/src/model.ts:580-596`).
  - `settings{alwaysThinkingEnabled?, fastMode?, ultracode?}`.
  - `pinnedAlias`.
  - `queryIdentity` (JSON).
- `ultracode` is exposed as a separate boolean (`ClaudeModelCatalog.ts:67-110`).
- **Live switching** (`A:2356-2378`, `A:13480-13501`): `set_model(apiModelId)` if the model changed, then `apply_flag_settings{effortLevel, fastMode, ultracode}`.
  - Allowed only when `alwaysThinkingEnabled` is unchanged, `pinnedAlias` is unchanged, and effort is not being cleared.
  - Otherwise the CLI restarts with resume.
  - `applyModelSelection` applies eagerly between turns, but only for same-model option changes (`A:15102-15131`).
  - Steers may carry a selection (`A:14632-14644`).
- Model routes: the capability probe's `init.models` (`value`/`resolvedModel`) remaps slugs where the backend wants different ids. It is keyed by a hash of model-relevant settings and env prefixes `ANTHROPIC_ CLAUDE_CODE_USE_ AWS_ CLOUD_ML_ VERTEX_` (`provider/ClaudeCodeModelRoutes.ts`).
- Context usage: `provider_turn.updated.tokenUsage` (`A:6785-6838`, `A:316-353`).
  - `usedTokens = input+cache_creation+cache_read+output` of the **last `iterations[type=message]`**.
  - `maxTokens`: catalog fixed window, else `modelUsage[m].contextWindow`.
  - `promptCacheTtlMs`: 1 h if `ephemeral_1h_input_tokens>0`, else 5 m. `promptCacheExpiresAt` = message_start time + TTL.
  - Emitted on `message_start`, `message_delta`, the assistant snapshot, compaction and reset.
- Capability probe (`provider/ClaudeProvider.ts:271-302,495-611`) is a separate `query()` with:
  - a never-yielding prompt, `persistSession:false`, `settingSources:["user","project","local"]`, `settings:{disableAllHooks:true}`, `allowedTools:[]`, `mcpServers:{}`, `strictMcpConfig:true` (unless managed-mcp.json exists);
  - env `ENABLE_CLAUDEAI_MCP_SERVERS=false`, `CLAUDE_CODE_AUTO_CONNECT_IDE=0`;
  - it reads `initializationResult()` (account email/subscription/apiKeySource, `commands`, `models`) and `usage_EXPERIMENTAL…({skipBehaviors:true})` for rate limits;
  - timeout 25 s.

#### 6. Attachments (`A:3216-3627`, `packages/provider-core/src/server/attachmentPrompt.ts:160-235`)
- Images (`image/gif|jpeg|png|webp` only, others are a protocol error) are read from `attachmentsDir` → `{type:"image",source:{type:"base64",media_type,data}}`.
- PDFs → `{type:"document",source:{type:"base64",media_type:"application/pdf",data}}`, only when the page count can be read from an uncompressed page tree (`estimateClaudePdfPages`), ≤20 MB, and within the budget.
- **Every** attachment is also referenced in the text: `[Attached image "x.png" is saved at: /abs/path (text extracted to: …)]`. `attachmentsDir` is in `additionalDirectories`, so Claude can `Read` files.
- Captured-window accessibility JSON is appended as "Untrusted captured-window data…".
- Block order: `[leadingText?] [images/docs in attachment order] [text | /skill command]` (`A:3608-3626`).
- Per-session budget is 28 MB (base64-adjusted, `4*ceil(n/3)+128`) and 100 PDF pages. It is persisted in `nativeMetadata.claudeAttachmentBudget`, freed on full compaction, refunded when a steer is dropped (`A:3357-3411`). Over-budget images are silently skipped and stay path-only.
- App context notes are prepended as `<external_KEY>text</external_KEY>` text blocks, once per changed value (`A:3413-3439`).

#### 7. Skills and slash commands
- Discovery (`provider/Drivers/ClaudeSkills.ts:778-903`) scans these roots, first name wins:
  - managed dir `.claude/skills`
  - `$CLAUDE_CONFIG_DIR/skills`
  - `<cwd>/.claude/skills`
  - `--add-dir`'s `.claude/skills`
  - `--plugin-dir` / `CLAUDE_CODE_PLUGIN_DIRS` plugins
  - installed/enabled plugins (`plugins/installed_plugins.json`, `enabledPlugins`), plus claude.ai-synced plugins (`plugins/synced`)
- Each skill is `<dir>/<name>/SKILL.md` with YAML frontmatter `name, description, disable-model-invocation (→ userInvocationOnly), user-invocable:false`. Plugin skills are prefixed `plugin:name`. `skillOverrides` (`on|name-only|user-invocable-only|off`) come from the settings files.
- Dispatch (`ClaudeSkillDispatch.ts`): the composer writes `$name` (any currency symbol, regex at `:8-9`). Claude expands only `/name` **at the start of the last text block**, and everything after it becomes ARGUMENTS. So T3 sends:
  - text block 1 = the whole prompt with `$x` rewritten to `/x`, plus `[The user invoked this skill. Follow the instructions at: <path>]` for earlier user-only skills;
  - last block = `/lastSkill <trailing text>`.
  - `/goal` prompts stay a single string (`A:3231-3266`).
- Slash commands for the composer come from the probe's `init.commands` (`name, description, argumentHint`), plus `/compact`, deduped case-insensitively (`ClaudeProvider.ts:394-454,802`). They are sent as plain text prompts.

#### 8. Bugs, races and fragile logic
1. **Windows has degraded diagnostics and cleanup.**
   - The win32 spawn branch pipes stderr to `options.stderr`, which T3 never sets. So `stderrTail` stays empty, and root-refusal / "No conversation found" / startup-exit guidance from stderr never trigger (`A:1796-1803` vs `A:1949-1966`). (Result-error detection at `A:1975-1979` still works.)
   - There is also no process group kill, no `providerProcesses.register`, no descendant reaping, and no task-output tail (`claudeTaskOutputRoot` needs `getuid`, `ClaudeTaskOutputTail.ts:16-31`).
2. **Every Stop kills the CLI** (`interrupt` then `close`, `A:14476-14485`). Background tasks and subagents die, and the next turn pays a full resume (which re-reads the whole jsonl in a worker, 30 s cap, `A:1589`, `A:1706-1724`).
3. **Private SDK API**: `Reflect.get(queryRuntime,"request")` for five control subtypes (`A:1035-1054`). It breaks silently on SDK refactors. Global `process.emitWarning` is monkey-patched (`A:1604-1618`).
4. **String-matching status**: non-success results map to interrupted/cancelled by substring `"interrupt"`/`"cancel"` in `errors` (`A:4975-4981`). An error message containing "cancel" would be misclassified. Dangerous-rm detection uses regexes on `decisionReason` prose (`A:13063-13066`). Gateway reset times are parsed with regexes over provider error text (`A:5148-5205`).
5. **"Always allow" is broad**: with no CLI suggestions, `acceptForSession` adds the rule `{toolName}` with no content. That allows every future call of e.g. `Bash` in the session (`A:4789-4808`).
6. **AskUserQuestion encoding**:
   - Multi-select answers are joined with `", "`, which is ambiguous when labels contain commas (`A:5769`).
   - The question text is the id, so duplicate questions collide (`A:5752`).
   - The abort-path deny has no `toolUseID` (`A:13001-13004`).
7. **Tool requests outside a turn are auto-denied** (`A:12893-12900`). A background subagent that needs approval while no T3 turn is active fails its tool call.
8. **Prompt-echo heuristics** (`promptEchoMode`, held frames, foreign-result detection, `A:5046-5061`, `A:12737-12885`) depend on CLI-version-specific echo behavior. On CLIs that never echo (`"unknown"`), a wake result racing a prompt can settle the wrong turn. The code explicitly accepts that.
9. `modelCatalog` is a mutable closure variable, reassigned at every `startTurn` and `steerTurn` (`A:13976`, `A:14587`). Message handlers read it concurrently (e.g. `resolveClaudeReportedModelSlug`, `A:11551`).
10. `locateClaudeTranscript` stats `<projects>/*/<id>.jsonl` for every project dir on each turn that reuses a live process (`A:13420-13431`). That is O(#projects) I/O per turn.
11. Tool inputs are not streamed: `input_json_delta` is ignored, so long Write/Edit inputs show nothing until the block completes (`A:11357-11460`).
12. MCP registration timeout reopens with a **new session id** for a fresh thread (`A:2219-2227`). The adapter then adopts the id via `init.session_id`, a path that also relies on `clearedNativeSessions` bookkeeping (`A:13871-13878`).
13. The fixtures (SDK 0.2.111, CLI 2.1.111) predate the declared SDK (^0.3.321). Wire shapes such as `command_lifecycle`, `user_message_uuids`, `background_tasks_changed` and `tool_result_meta` are newer, undocumented CLI fields that T3 reads with `Reflect.get`. Expect drift.

##### Rust reimplementation notes
- Spawn `claude` with the flags in §1.5 (verify them with `strings` on the installed SDK's `sdk.mjs`, or by running `claude --help`).
- Speak NDJSON on stdin/stdout:
  - Send `initialize` first, then `mcp_set_servers`.
  - Write `{"type":"user",…,"uuid"}` lines for prompts and steers (`priority:"next"`).
  - Answer `can_use_tool` with `{behavior,updatedInput|message,updatedPermissions?,toolUseID}`.
  - Honor `control_cancel_request`.
- Keep one CLI per thread with a restart key of (permissionMode, tools, allowedTools, MCP servers, workspace roots, thinking on/off, pinned alias).
- Persist the native session id and the last root-assistant `uuid` per turn. They serve as the resume and rollback cursor (`--resume <id> --resume-session-at <uuid>`).

---

## 3c. Shared provider abstraction, ACP and other providers
### T3 Code: shared provider abstraction, ACP, and non-Codex/Claude providers

Repo root is `~/nsh-research/t3code`. Paths below are repo-relative. Line numbers come from the shallow clone as read. The stack is Effect-TS throughout: `Context.Service` = DI interface, `Effect.Effect<A,E,R>` = async fallible op, `Stream` = async stream, `Scope` = resource lifetime. In Rust these map to traits, `async fn -> Result`, `Stream`, and Drop-guards/cancellation tokens.

---

#### 1. Shared provider abstraction

##### 1.1 Layering

```
ProviderDriver (per kind: codex, claudeAgent, cursor, grok, fx, opencode, antigravity, kiro, pi, muse, acpRegistry)
   .create(config, env) -> ProviderInstance { snapshot, orchestrationAdapter: ProviderAdapterV2, textGeneration, auth?, acpSessionManagement? }
ProviderInstanceRegistry   (settings -> live instances, one Scope per instance id; rebuild on config change)
ProviderAdapterRegistryV2  (instanceId -> adapter, wraps openSession with version + credential gates)
ProviderSessionManagerV2   (pool of live sessions, idle unload, MCP credential issue/revoke, event fan-out)
ProviderAdapterV2.openSession -> ProviderAdapterV2SessionRuntime (ensureThread/startTurn/steer/interrupt/... + events Stream)
```

- Driver list is fixed in `apps/server/src/provider/builtInDrivers.ts:33-45`. The ACP Registry driver is a single generic driver that covers every registry or local ACP command.
- `ProviderDriver` interface: `packages/provider-core/src/server/driver.ts:117-128`. It has `driverKind`, `metadata {displayName, supportsMultipleInstances, hasDefaultInstance}`, `configSchema`, `defaultConfig()`, `create(input) -> Effect<ProviderInstance>`, and optional `usage`.
- `ProviderInstance` (`driver.ts:41-90`) has these fields:
  - `instanceId`, `driverKind`, `continuationIdentity {driverKind, continuationKey}`. The continuation key decides whether a thread can move between instances.
  - `snapshot: ManagedServerProvider`, defined in `snapshot.ts:18-31`. It provides `getSnapshot`, `refresh`, `streamChanges`, `applyUsageLimits`, `resolveMaintenance`, plus optional `snapshotForCwd`, `refreshModels`, `invalidateCaches`, `consumeResetCredit`.
  - `orchestrationAdapter`, `textGeneration`, optional `auth: ProviderAuthController`.
  - `acpSessionManagement {listSessions, logout, deleteSession, listProviders, setProvider, disableProvider}` for ACP agents.
- `adapterDriver.ts:24-35` is a thinner `ProviderAdapterDriver` (configSchema/defaultConfig/create → adapter). Drivers use it internally.
- `ProviderHost` (`ProviderHost.ts:33-65`) is the host-services bag given to every driver:
  - `paths {cwd, baseDir, stateDir, providerStatusCacheDir, attachmentsDir}`
  - `settings.get/withSnapshot/changes/subscribe`
  - `shouldRunBackgroundWork`, `awaitServerReady`
  - `resolveAttachmentPath(attachment) -> string|null`. It returns null when the attachment id escapes `attachmentsDir`.
  - `resolveExtractedTextPath`
  - `trackAgent()` counts a running agent so port discovery can map a `T3CODE_THREAD_ID` to its thread.
  - `credentials(namespace, bindingId)` is a keychain-like blob store. Instances with equal `binding.key` share credentials.
  - `compatibility`, a minimum-version policy lookup.
- `ModelCatalog` (`ModelCatalog.ts:7-55`) holds a bundled plus remotely refreshed catalog of `{slug, name, aliases, status current|legacy, capabilities}` per driver. `classifyCatalogModels`/`applyCatalogDefault` mark live models as legacy/default, optionally by "family" matching.
- `processGroup.ts:2-9` provides `signalProcessGroup(pid, sig)`, which is `process.kill(-pid)`. It refuses pid ≤ 1 so it cannot hit the server's own group (`kill(0)`) or every user process (`kill(-1)`).

##### 1.2 `ProviderAdapterV2` interface (`packages/provider-core/src/server/ProviderAdapter.ts`)

The adapter service (`:882-903`) has:

| member | meaning |
|---|---|
| `instanceId`, `driver` | identity |
| `checksVersionAtHandshake?` | adapter rates the version itself during `openSession`, so the registry skips its probe-based gate (`ProviderAdapterRegistry.ts:93-98`) |
| `getCapabilities()` | → `OrchestrationV2ProviderCapabilities` (below) |
| `planSelectionTransition(input)` | decides whether a model/mode change applies in-session, on the next turn, or needs a restart |
| `openSession(input)` (Scoped) | → `ProviderAdapterV2SessionRuntime` |

`openSession` input (`:567-577`) carries `threadId`, `providerSessionId`, `modelSelection`, `runtimePolicy`, `promptSuggestions?`, `resumeFromSession?`, `initialNativeThreadId?`.

`RuntimePolicy` (`:65-77`) has these fields:
- `runtimeMode` ∈ {approval-required, auto-accept-edits, auto, full-access}
- `interactionMode` (e.g. `plan`)
- `cwd`
- `approvalPolicy?`, `sandboxPolicy?`. These are Codex-shaped: `{type: readOnly|workspaceWrite{writableRoots}|dangerFullAccess|externalSandbox}`.
- `maxRuntimeMode?`, the admin ceiling.
- `workspace? {folders[], extraRoots[]}`

`ProviderAdapterV2SessionRuntime` (`:765-879`):

| method | notes |
|---|---|
| `events: Stream<Event>` / `subscribeEvents?` | the single normalized event stream |
| `ensureThread({threadId,title,modelSelection,runtimePolicy,existingProviderThread?,forceNewNativeThread?})` | create or attach a native thread → `OrchestrationV2ProviderThread` |
| `resumeThread({providerThread,...,importedNativeSession?})` | load an existing native thread |
| `injectHistory?` | push historical context into a fresh native thread |
| `startTurn(TurnInput)` | see below |
| `resumeTurn?` | continue an interrupted turn without a new message |
| `compactThread?` | native compaction |
| `steerTurn({providerTurnId,message,...})` | mid-turn user message |
| `applyModelSelection?`, `applyThreadTitle?` | in-session model change; rename |
| `interruptTurn({providerThread,providerTurnId,requestRuntimeRestart?})` | stop |
| `stopTask?`, `awaitBackgroundTasks?`, `stopSubagent?`, `hasPendingBackgroundWork[ForThread]?` | background work and subagents |
| `unloadThread?({archive?})` | detach a native thread from the process |
| `respondToRuntimeRequest({requestId, decision?, feedback?, answers?})` | answer approvals and questions |
| `readThreadSnapshot`, `rollbackThread({target: thread_start \| provider_turn})`, `forkThread` | history ops |
| `mcpApps?` (listTools/callTool/readResource), `mcpSignIn?`, `uploadFeedback?` | extras |
| `getModelContextWindow?`, `canReuseContextUsage?`, `normalizeThreadLoadModelSelection?`, `canResumeAfterRestart?`, `backgroundTasksEndWithProcess?` | hints |

`TurnInput` (`:589-608`) carries `appThread`, `threadId`, `runId`, `runOrdinal`, `providerTurnOrdinal`, `attemptId`, `rootNodeId`, `providerThread`, `message {messageId,text,attachments[],createdBy,...}`, `modelSelection`, `runtimePolicy`, `appContext?: [{key,text}]`, `agentFeedback?`, `resume?`, `nativeThreadHasTurns?`.

Errors are a tagged union (`:543-565`): Capabilities, OpenSession, CloseSession, ResumeThread, ThreadInUse, ResumeUnsupported, CompactionUnsupported, EnsureThread, ReadThreadSnapshot, RollbackThread, ForkThread, TurnStart, SteerRunUnsupported, SteerDeclined, SteerRun, Interrupt, InterruptTimeout, RuntimeRequestResponse, EventStream, Protocol(reason), SessionStopped.

##### 1.3 Capability flags (`packages/contracts/src/orchestrationV2.ts:300-448`)

`OrchestrationV2ProviderCapabilities` contains:
- **sessions**: supportsMultipleProviderThreadsPerSession, supportsModelSwitchInSession, supportsProviderSwitchingViaHandoff, supportsRuntimeModeSwitchInSession, pendingRequestsSurviveRestart.
- **threads**: canCreateEmptyThread, canReadThreadSnapshot, canRollbackThread, canForkThread, canForkFromTurn, canForkWhileSourceRunning?, canForkFromSubagentThread, exposesNativeThreadId.
- **turns**: supportsNativeGoals?, exposesNativeTurnId, emitsTurnStarted, emitsTurnCompleted, supportsInterrupt, supportsNativeResume?, supportsActiveSteering, activeSteeringInterruptsTools?, activeSteeringAcceptsAttachments?, activeSteeringDuringCompaction?, supportsSteeringByInterruptRestart, supportsQueuedMessages, terminalStatusQuality (strong|weak|none).
- **streaming**: streamsAssistantText, streamsReasoning, streamsToolOutput, streamsPlanText, emitsMessageCompleted.
- **tools**: exposesToolItemIds, emitsToolStarted, emitsToolCompleted, emitsToolOutput, supportsMcpTools, supportsDynamicToolCallbacks, supportsBackgroundWait?.
- **approvals**: supportsCommandApproval, supportsFileReadApproval, supportsFileChangeApproval, supportsApplyPatchApproval, approvalsHaveNativeRequestIds, approvalCallbacksAreLiveOnly, approvalsCanOriginateFromSubagents.
- **planning**: emitsPlanUpdated, emitsTodoList, emitsProposedPlan, supportsStructuredQuestions, planDeltasHaveItemIds.
- **subagents**: supportsSubagents, exposesSubagentThreadIds, emitsSubagentLifecycle, canWaitForSubagents, canCloseSubagents, canForkSubagentThread.
- **context**: acceptsSystemContext, acceptsDeveloperContext, acceptsSyntheticUserContext, canGenerateSummaries, canConsumeHandoffSummaries, supportsDeltaHandoff, supportsFullThreadHandoff, maxRecommendedHandoffChars.
- **checkpointing**: appCanCheckpointFilesystem, supportsNestedCheckpointScopes, providerCanRollbackConversation, providerRollbackReturnsSnapshot, providerCanReadConversationSnapshot.
- **identity**: nativeThreadIds / nativeTurnIds / nativeItemIds / nativeRequestIds, each ∈ strong|weak|… (`OrchestrationV2NativeRefStrength`).
- **runtimePolicy.enforcement**: `native` (the agent enforces the sandbox) or `client-boundary` (T3 answers permission requests itself). Replay defaults to `client-boundary`.

The session manager uses these flags. For example, the idle-unload timeout is 2 min when `supportsNativeResume`, else 30 min (`apps/server/src/orchestration-v2/ProviderSessionManager.ts:81-84,581-588`). The max idle pin is 4 h, and the interrupt timeout is 15 s (`:87`).

##### 1.4 Normalized events adapters emit (`ProviderAdapter.ts:101-251`)

Every event carries `driver`.

| `type` | payload |
|---|---|
| `app_thread.created` | `appThread`, `createOnly?` (native subagent children become app threads) |
| `provider_session.updated` | `providerSession` (status starting/ready/running/waiting/stopped/error, model, capabilities) |
| `provider_thread.updated` | `providerThread`, `pendingBackgroundWork?`, `sessionScoped?`, `retainedNativeTurnIds?` |
| `thread_model_selection.updated` | `threadId, instanceId, changes:[{id,from,to}]`. The agent changed its own option, e.g. left plan mode; it only applies where the thread still holds `from` |
| `provider_turn.updated` | `providerTurn {id, providerThreadId, nodeId, runAttemptId, nativeTurnRef, ordinal, status pending/running/completed/interrupted/failed/cancelled, startedAt, completedAt, tokenUsage?, model?, promptDropped?}`, `retainedRuntimeRequestIds?` |
| `node.updated` / `subagent.updated` / `message.updated` | execution-tree node / subagent / conversation message |
| `turn_item.updated` | `turnItem` (see below), `sessionScoped?` |
| `steer.updated` | `{threadId, runId, messageId, status pending/completed/cancelled}` |
| `background_work.updated` / `background_work.lost` | `{providerTurnId, pending}` / tasks lost when the process exited |
| `command_output.delta` | `{threadId, itemId, chunk}`, unpersisted live terminal output |
| `prompt_suggestion.updated` | `{threadId, runId, text}` |
| `runtime_request.updated` | `runtimeRequest {id, nodeId, providerTurnId, nativeRequestRef, kind, status pending/resolved/expired/cancelled, responseCapability live{providerSessionId}\|message\|not_resumable, decision?, answers?}` |
| `plan.updated` | plan artifact |
| `turn.terminal` | `{providerThreadId, providerTurnId, runOrdinal, status completed\|interrupted\|cancelled, failure:null, reason?, threadDisposition reusable\|broken, promptDropped?, retainedRuntimeRequestIds?}` or `{status:"failed", failure {class usage_limit\|provider_error\|transport_error\|permission_error\|validation_error\|unknown, message, code, retryable, resetAt?}, failureItemOrdinal, retry?, childTurnsContinue?}` |

Runtime request kinds (`contracts/src/providerPolicy.ts:49-55`, `orchestrationV2.ts:1419-1422`) are `command`, `file-read`, `file-change`, `mcp-elicitation`, `permission`, `dynamic_tool_call`, `user_input`, `auth_refresh`. Decisions (`providerPolicy.ts:58-64`) are `accept | acceptForSession | acceptAlways | decline | cancel`.

**Turn items** (`orchestrationV2.ts:1899-2135`) share base fields (`:1774-1801`): `id, threadId, runId, nodeId, providerThreadId, providerTurnId, nativeItemRef, parentItemId, ordinal, status (idle/pending/running/waiting/completed/failed/cancelled/interrupted), title, startedAt, completedAt, updatedAt, toolSurface?/toolIcon?/toolSource?`. The variants are:
- `user_message`; `assistant_message {messageId, text, streaming, phase?, delivery?:"async"}`; `reasoning {text, streaming}`
- `proposed_plan {planId, markdown}`; `todo_list {planId, steps, explanation?}`
- `user_input_request {requestId, questions, responseMode?}`
- `file_change {fileName, additions?, deletions?, diffStr?, oldStr?, newStr?, changes?[{operation,path,oldPath?}]}`
- `command_execution {input, output?, exitCode?, outputIndicatesFailure?, monitor?, completionWake?}`
- `file_search {pattern, results}`; `web_search {patterns, results}`
- `approval_request {requestId, requestKind, prompt?, reason?, options?}`
- `checkpoint`, `run_interrupt_request/result`, `system_notice {level}`, `error`, `compaction`, `handoff`, `fork`, `thread_created`
- secret/link/feedback request items
- `subagent {subagentId, origin provider_native|app_owned, childThreadId, prompt, progress?, result}`
- `dynamic_tool {toolName, input, output?}`

Adapters re-emit the whole item on every change; deltas are coalesced (`Adapters/ProviderTextDeltaCoalescer.ts`).

##### 1.5 Registries and gating

- `ProviderInstanceRegistry` (`apps/server/src/provider/ProviderInstanceRegistry.ts:34-46`) exposes `getInstance/listInstances/listUnavailable/streamChanges/subscribeChanges`. `makeReconcile` (`:410+`) diffs the settings config map. It closes changed or removed instance scopes *before* building replacements, so there is one live scope per id. Missing or blocked drivers become "unavailable" snapshots (`:254-300`). Secrets are resolved via SecretResolver; configs whose vault is locked are parked (`secretsUnreadable`).
- `ProviderAdapterRegistryV2.get` (`orchestration-v2/ProviderAdapterRegistry.ts:65-180`) wraps `openSession` with:
  1. A broken-version refusal. It uses the cached snapshot version first, then forces `snapshot.refresh` before refusing.
  2. A refusal if any instance sharing the same `auth.credentialBinding` is mid credential change.
  3. A wrap in each peer's `auth.withAccess(...)`, so a shared credential change interrupts startup.
- `ProviderRegistry` (`provider/ProviderRegistry.ts:60-136`) handles UI-facing snapshots: refresh, refreshInstance, workspace snapshots per cwd, maintenance and update state.
- `ProviderSessionManagerV2` (`ProviderSessionManager.ts:246-325`) exposes `open/get/isLive/close/closeInstance/release/detach/releaseUnlessBusy/otherWorkingThreads/shutdown`. Each `LiveSessionEntry` (`:327-349`) holds `attachedThreadIds`, `mcpCredentialIdByThread`, `eventSubscribers` (fan-out queues), `idleFiber`, `busyTurns`, `idleThreadUnloads`. It uses keyed locks per session id and per (session, thread) (`:575-580`).

---

#### 2. T3 MCP server injection

**Server.** It is an MCP Streamable-HTTP endpoint at `/mcp` (`apps/server/src/httpCors.ts:16`) on the T3 HTTP server, built with Effect's `McpServer.layerHttp` (`apps/server/src/mcp/McpHttpServer.ts:1499-1508`). It sits outside environment auth and uses its own bearer auth (`layerMcpAuthMiddleware`).
- Tokens containing `.` are signed session tokens. Bare base64url strings are per-thread registry tokens (`McpHttpServer.ts:221`).
- A mode-limit header (`McpInvocationContext.MODE_LIMIT_HEADER`, value `runtimeMode/interactionMode`) narrows what a caller may do (`:224-245`).
- There is a workaround for Go-SDK `notifications/roots/list_changed` (`:190-218`).

**Credential issue** (`ProviderSessionManager.ts:620-695`):
1. Capabilities start as {orchestration, worktree, pull-requests, issues}. They add `preview` if browser tools are allowed, `device` if device tools exist, and `background-wait` if the adapter reports `tools.supportsBackgroundWait`.
2. If a credential exists for the thread and still matches these capabilities and the subagent flag, it is reused. Otherwise the manager runs `revokeThread` then `McpSessionRegistry.issue`, which yields a 32 random bytes base64url token. Only the sha256 is stored (`mcp/McpSessionRegistry.ts:116-160`).
3. The endpoint is `http://<host>:<port>/mcp`, with fallback `http://127.0.0.1/mcp` (`:112-114`).
4. Liveness window is 24 h (`:66`).
5. The config `{environmentId, threadId, providerSessionId, providerInstanceId, endpoint, authorizationHeader:"Bearer …", browserToolsAvailable, capabilities, agentDeviceEnvironment?, hasPendingWake}` is stored in `McpProviderSessions` (`packages/provider-core/src/server/mcpSession.ts:10-22`). Adapters read it at session open or turn start.
6. Reservations (`:594-600`) stop revocation between issuance and adapter consumption.

**Per-provider injection:**

| provider | mechanism |
|---|---|
| ACP (generic, Grok, Kiro, Fx, Antigravity, registry) | `acpMcpContext` (`packages/provider-acp/src/server/adapter.ts:1115-1159`) builds three candidates. `sessionMcpServers` (`AcpSessionRuntime.ts:2374-2388`) picks `acp` if `agentCapabilities.mcpCapabilities.acp`, else `http` if `mcpCapabilities.http` and the flavor opts in (`mcpOverHttp`), else stdio. (a) **stdio**: `{name:"t3-code", command:<T3 binary>, args:[<entry>,"acp-mcp-bridge"], env:[ELECTRON_RUN_AS_NODE=1, T3_ACP_MCP_ENDPOINT, T3_ACP_MCP_AUTHORIZATION]}`. The bridge relays JSON-RPC lines to HTTP verbatim (`apps/server/src/mcp/AcpMcpStdioBridge.ts`). (b) **http**: `{type:"http", name:"t3-code", url, headers:[Authorization, t3-tool-exposure:deferred?]}`. (c) **MCP-over-ACP**: `{type:"acp", name:"t3-code", serverId:"t3-code"}`. The agent then calls client methods `mcp/connect`, `mcp/message`, `mcp/disconnect`, which T3 proxies to `/mcp` (`provider-acp/src/server/mcpOverAcpBridge.ts`, 16 connections max, 8 MiB messages). The agent process env also gets `T3CODE_THREAD_ID, T3_ACP_MCP_ENDPOINT, T3_ACP_MCP_AUTHORIZATION, T3_ACP_MCP_NODE, T3_ACP_MCP_CALL` (`adapter.ts:1144-1157`). On Windows a `.cmd` launcher is written to `stateDir\acp\acp-mcp-call.cmd` (`:1098-1113`). |
| Cursor | SDK `mcpServers: {"t3-code": {type:"http", url, headers:{Authorization}}}` (`packages/provider-cursor/src/server/adapter.ts:224-238`), passed per send |
| OpenCode 1.x | `client.mcp.add({name:"t3-code", config:{type:"remote", url, headers, oauth:false}})` on its private `opencode serve` (`provider-opencode/src/server/adapter.ts:1101-1112`). It is skipped for external servers. |
| OpenCode 2.x | one shared server, so the name is per-thread: `t3-code-<threadId sanitized>`, or `t3-code-<sha256[:16]>` if over 64 chars (`v2/adapter.ts:436-444`). `client.mcp.add({server, location:{directory}, config: RemoteConfig})` runs at each turn prep (`:4442-4495`). Permission rules isolate threads: `t3-code-* deny` then `<name>_* allow` (`:447-453`). Before 2.2.6 there is a sleep because tools load 100 ms after connect. |
| Pi | Pi has no MCP client. T3 writes a TypeScript Pi extension (`provider-pi/src/server/mcpExtensionSource.ts`) that speaks MCP 2025-06-18 to `T3_MCP_URL` with `T3_MCP_BEARER_TOKEN` and registers each MCP tool as a Pi tool. It is passed via `--extension <path>` plus env (`mcpInjection.ts:262-323`). |
| Muse | `session/start` `config.mcpServers["t3-code"] = {transport:"streamableHttp", mode:"optional", url, headers}`. It requires the granted `sessionMcp` capability (`provider-muse/src/server/adapter.ts:1951-1967`). |

**Tools exposed** (`McpHttpServer.ts:1511-1528`; names from `apps/server/src/mcp/toolkits/*/tools.ts`):

| group | tools |
|---|---|
| orchestrator | orchestrator_capabilities, delegate_task, task_status, task_cancel, schedule_task, list_scheduled_tasks, delete_scheduled_task, request_secret, create_threads |
| thread | t3_thread_organize (pin/unpin/snooze/settle/archive/mark_unread…), t3_queue (edit/cancel/reorder/promote_to_steer), t3_pending_request, t3_thread_configure, t3_thread_transfer, t3_thread_search, run_scheduled_task_now, t3_submit_output, t3_thread_launch/list/read/send/update/wait/interrupt, t3_background_wait |
| workflow | t3_workflow (hooks/start/signal/resume/cancel/store_set/store_delete), t3_workflow_stop |
| project/worktree/env | t3_project_{list,read,create,update,delete,clone}, t3_worktree_{list,status,handoff}, t3_environment_{read,link,links,unlink,preferences_update} |
| PR/issues | pull_request_{read,list,create,update,respond,review,merge}, ci_logs, ci_rerun, (un)link_pull_request(s), list_thread_pull_requests, (un)watch_pull_request, write/read_pull_request_guide(s); list_issues, read_issue, create_issue, comment_on_issue, set_issue_state, (un)link_issue, list_thread_issues |
| preview (browser) | preview_{status,open,dialog,navigate,resize,set_appearance,snapshot,act,evaluate,wait_for,recording_start,recording_stop}, t3_preview_list, t3_preview_close |
| device | device_{list,open,screenshot,close} |
| misc | html_preview (html_render), t3_attachment_prepare_upload, t3_attachment_discard, t3_thread_send_attachments, t3_feedback, t3_theme_read/set, tool_search |

**Deferred exposure.** With header `t3-tool-exposure: deferred` (`mcpSession.ts:8`), only `orchestrator_capabilities, delegate_task, task_status, tool_search` are listed (`McpToolExposure.ts:7-12`). `tool_search` returns up to 10 scored matches and "loads" them for that provider session, remembering up to 256 sessions.

**Result recovery.** `McpProviderSessions` (`provider-core/src/server/McpProviderSessions.ts`) keeps the last 64 tool results and HTML renders per thread. This lets an adapter attach a T3 tool's structured output to the provider's tool item when the provider only reports the input. Matching is by (canonical tool, sha256(stableStringify(input)), output). The epoch is bumped per turn to drop late handlers (`:270-300`).

---

#### 3. ACP (Agent Client Protocol)

##### 3.1 Transport: `packages/effect-acp`

- Transport is newline-delimited JSON-RPC 2.0 over the child's stdio (`adapter.ts:153` `ACP_PROTOCOL = "acp.ndjson-jsonrpc"`, `effect-acp/src/_internal/stdio.ts:9-18`). Stderr is drained or forwarded to a flavor handler.
- **Dual protocol generation.** `PROTOCOL_VERSION = 2` (`_generated/meta.gen.ts:40`), the ACP v2 preview. `initialize` sends a hybrid request with v2 fields `{protocolVersion:2, info, capabilities}` and v1 fields `{clientCapabilities, clientInfo}` (`client.ts:695-717`). The response is treated as v2 iff it has `info`. Antigravity reports `protocolVersion:2` with a v1 body, so the version number is ignored (`client.ts:1105-1120`). All later calls branch on `negotiatedProtocolGeneration`.
- Agent methods, v2 (`meta.gen.ts:1-28`): `initialize, auth/login, auth/logout, providers/list|set|disable, session/new, session/resume, session/fork, session/list, session/delete, session/close, session/prompt, session/cancel (notification), session/set_config_option, mcp/message, nes/*, document/did*`.
- v1-only agent methods (`rpc.ts:10-16`): `authenticate, logout, session/load, session/set_model, session/set_mode`.
- Client methods (agent→T3), v2: `session/request_permission, session/update (notif), mcp/connect|message|disconnect, elicitation/create, elicitation/complete`. v1 adds `session/elicitation, _session/elicitation, session/elicitation/complete, fs/read_text_file, fs/write_text_file, terminal/create|output|release|wait_for_exit|kill` (`rpc.ts:18-29`). Unknown `_ext` requests and notifications get pluggable handlers.
- v1 `session/update` is normalized to the v2 shape (`client.ts:514-560`). Examples: v1 `name:null` means "unchanged" and is stripped; malformed `notice` becomes `_t3_unknown`.
- Update variants seen across both generations:
  - messages: `agent_message_chunk | agent_message | agent_thought_chunk | agent_thought | user_message_chunk | user_message`
  - tools: `tool_call | tool_call_update | tool_call_content_chunk`
  - plans: `plan | plan_update | plan_removed`
  - session state: `available_commands_update | current_mode_update | config_option_update | session_info_update | usage_update | state_update`
  - compaction: `compaction_update | compaction_summary_chunk`
  - terminals: `terminal_update | terminal_output_chunk`
  - `notice`

  Sources: `_generated/schema.gen.ts`, `schema-v1.gen.ts`, `runtimeModel.ts:1327-1360`.

##### 3.2 Session runtime (`packages/provider-acp/src/server/AcpSessionRuntime.ts`)

**Spawn** (`:1465-1612`):
- Command resolution respects `shell:false`. Env is `extendEnv` unless an explicit env is given.
- On Linux the agent is wrapped in a cgroup lease (`wrapCommandForLinuxCgroup`). Optionally it gets its own detached process group, killed via `signalProcessGroup`.
- On Windows the tree is killed with `System32\taskkill.exe` (`:1051-1060`).
- `transformStdout` and `onStderr` hooks let Antigravity scrape login URLs.

**Initialize** (`:1887-1902`, `:2324-2338`):

```json
{"jsonrpc":"2.0","id":0,"method":"initialize","params":{
  "protocolVersion":2,"info":{"name":"t3-code","version":"…"},"capabilities":{…},
  "clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false,
     "session":{"notices":{},"compaction":{}},"elicitation":{…},"_meta":{…flavor}},
  "clientInfo":{…},"_meta":{…initializeMeta}}}
```

- fs and terminal are off by default. They are enabled per flavor: Antigravity uses `readTextFile` (`apps/server/src/provider/acp/AntigravityAcpSupport.ts:131-138`), Devin uses client terminals.
- Startup timeouts: 90 s per startup request, `session/load` 90 s, a replay idle gap of 2 s, and a cancel timeout of 15 s (`:88-92`).

**Session setup** (`:2453-2510`):
- If there is a `resumeSessionId` and `agentCapabilities.loadSession` and `resumeMethod !== "resume"`, T3 sends `session/load`. It waits for the replay to go idle, because agents stream history as `session/update` with `_meta.isReplay` and some (Grok) never answer load (`sessionLoadReplayIdleReadiness`).
- Else, if `sessionCapabilities.resume` is set, it sends `session/resume`.
- Else it errors with `-32601`.
- A new session sends `session/new {cwd, mcpServers, additionalDirectories?, _meta?}`. `additionalDirectories` holds the workspace `extraRoots`, sent only when the agent advertises it.

**Auth**:
- If setup fails with auth-required, T3 picks the configured `authMethodId`, or the advertised method, which must be `type:"agent"` (`:2405-2450`). It sends `authenticate`/`auth/login {methodId}` and retries setup.
- `authenticateEagerly` (Antigravity) authenticates before setup, with a method it may not even list (`:2496-2508`).

**After setup** the runtime parses `modes`/`configOptions` (category `mode`/`model`/reasoning) into refs (`:2528-2530`, `runtimeModel.ts:413-497`).

**Prompt**: `session/prompt {sessionId, prompt: ContentBlock[]}` → `{stopReason: end_turn|max_tokens|max_turn_requests|refusal|cancelled}`.
- Prompts are serialized by a dispatch semaphore. `dispatched`/`released` deferreds let a cancel target the right prompt (`:1190-1200`).
- Prompt blocks are built in `adapter.ts:9200-9360`:
  1. `{type:"text", text}`: user text with attachment file paths, T3 instructions and app context prepended (re-sent only after compaction or a change), plus a separate runtime-instructions text block.
  2. `{type:"image", data:<base64>, mimeType}`, only when `promptCapabilities.image`.
- Non-success stop reasons become notices (`adapter.ts:1227-1240`).

**Cancel** (`:2617-2685`):
- Default behaviour interrupts the local prompt fiber, then sends `session/cancel {sessionId, _meta?}`. `joinPrompt` synthesizes `{stopReason:"cancelled"}`.
- `wait-for-prompt` (Fx, Antigravity) or `interruptPromptOnCancel:false` (Kiro) works differently. T3 waits until the prompt is queued (≤ 15 s), sends cancel, and waits for the real prompt response. If none arrives within 15 s it kills the process (`retireRuntime`). This path holds the dispatch permit.

**Mode, model, and config**:
- `setMode` prefers a `category:"mode"` select config option via `session/set_config_option {sessionId, configId, value}`. It falls back to v1 `session/set_mode {sessionId, modeId}` for agents like gemini-cli that only advertise `modes` (`:2959-2990`).
- `setSessionModel` uses v1 `session/set_model`. `setConfigOption` refuses unlisted select values unless `allowUnlistedValue` is set (custom model ids).

##### 3.3 ACP adapter (`provider-acp/src/server/adapter.ts`, 11k lines) and flavors

- `makeAcpAdapterV2({instanceId, flavor, selfInvocation, clientTerminals?, …})` (`:2091`). `AcpAdapterV2Flavor` (`:429-560`) is the per-agent customization hook set:
  - `driver`, `capabilities`, `makeRuntime`, `promptFailure`, `buildAttachmentPrompt`, `rewriteUserText`
  - `normalizeSessionUpdate`/`ignoreSessionUpdate`/`normalizeToolCall`
  - `extractPermissionQuestion`, `permissionDisposition`, `approvalOptions`, `answerablePermissionOptions`
  - `extractElicitationForm`, `onUrlElicitation`
  - `steering {send, injectedSteerId}`, `steersWithPrompt`, `forkFromTurn`
  - `compactViaAdvertisedCommand` (send `/compact` as a prompt)
  - `mcpOverHttp`, `deferMcpTools`, `clientFileSystem {readTextFile, writeTextFile}`
  - `alwaysSetModel`, `modelOptionArrivesLate`, `lockSessionModeToPolicy`, `sessionModeForPolicy`, `sessionConfigForPolicy`, `runtimeModeForSessionMode`
  - `extractSubagentUpdate`, `subagentsOutlivingPrompt`, `enablePostSettleContinuation`, `deferFinalizeForBackgroundWork`, `wakeOnDetachedRequest`, `registerExtensions`, `watchPromptOutcome`
- **Default capabilities** (`:872-960`) for a plain ACP agent:
  - Supported: interrupt, queue, steer-by-interrupt-restart, streaming text/reasoning/tool output, command/file-read/file-change approvals (no native request ids, live only), todo list, structured questions, handoffs.
  - Not supported: in-session model switch, multiple threads per session, native resume, active steering, snapshot read, fork, subagents.
  - Rollback is "fresh session + replay kept history" (`providerCanRollbackConversation:false`).
  - Identity: thread ids strong, turn and item ids weak.
- **Tool mapping** (`runtimeModel.ts:808-824`) by ACP `kind`: `execute`→command_execution; `edit|delete|move`→file_change; `search|fetch`→web_search; `read`/other→dynamic_tool. Tool output is bounded (terminal output max 320 KiB, `runtimeModel.ts:65`). Embedded terminals decode base64 frames.
- **Permissions** (`adapter.ts:7518-7700`). On `session/request_permission {sessionId, toolCall:{toolCallId,title,kind,rawInput,locations}, options:[{optionId,name,kind: allow_once|allow_always|reject_once|reject_always}]}`:
  1. If the flavor turns it into a question, a `user_input_request` is shown.
  2. Otherwise T3 computes a disposition from the runtime policy (`clientPolicy.ts:143-188`):
     - reads → allowed unless policy requires approval
     - `approval-required` → ask
     - sandbox `readOnly` → deny
     - `workspaceWrite` → allow mutations only if every `location` is inside cwd/writableRoots/extraRoots
     - `dangerFullAccess`/`externalSandbox` → allow
     - no sandbox + `auto-accept-edits` → ask for non-mutations
  3. `allow` answers `{outcome:{outcome:"selected", optionId:<allow_once preferred>}}`. `deny` picks `reject_once`. If no option fits, the answer is `{outcome:{outcome:"cancelled"}}`.
  4. `ask` emits `runtime_request.updated` plus an `approval_request` item. The user decision maps through `selectPermissionOptionId` (`:1663-1674`): accept→allow_once; acceptForSession→allow_always, falling back to allow_once; decline→reject_once.
  5. Kiro omits `kind` on permission requests, so it is borrowed from the tracked `tool_call` of the same id (`:7579-7590`).
- **Elicitation**: `elicitation/create` form mode → user_input_request; URL mode → the auth consent UI. Codex-style MCP tool approval elicitations auto-allow unless in approval mode (`clientPolicy.ts:204-218`).
- **Terminals**: by default T3 only displays agent-reported terminals. For Devin T3 implements `terminal/*` itself (`clientTerminals.ts`). Shell source runs through Git-for-Windows bash on Windows and respects `outputByteLimit`.

##### 3.4 ACP Registry driver (`packages/provider-acp-registry`, `docs/user/providers-acp.md`)

- The registry index is `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json` (`AcpRegistrySupport.ts:53`), cached as `registry.json`.
- Agent schema (`:139-160`):

  ```json
  {"id","name","version","description","license?","repository?","icon?",
   "distribution":{"binary?":{"<platform>":{"archive":"https://…","cmd":"./bin/x","sha256?":"…","args?":[],"env?":{}}},
                   "npx?":{"package":"pkg@1.2.3","args?","env?"},"uvx?":{"package":"pkg==1.2.3",…}}}
  ```

  Packages must pin exact versions (`:90-108`). URLs must be HTTPS without credentials.
- Platforms are darwin-aarch64, darwin-x86_64, linux-aarch64, linux-x86_64, windows-aarch64, windows-x86_64. windows-aarch64 falls back to x86_64 (`:231-262`). Preference order in auto mode is binary → npx → uvx (`:448`).
- **Install**:
  - The install path is `tools/<agent-id>/<version>/` under T3 home. A per-agent in-process semaphore guards it (`:701-706`).
  - Binary installs download, verify sha256 if present, list the archive and reject unsafe paths, then extract with `tar -xzf/-xjf/-xf` or `unzip`. They check the `cmd` exists inside the root, `rm` the old root, and `rename` the staging dir (`:1578-1698`).
  - npx: `npm install --global <pkg@ver>` with `npm_config_prefix=<root>/npm`.
  - uvx: `uv tool install --force <pkg==ver>` with `UV_TOOL_DIR`/`UV_TOOL_BIN_DIR` (`:1345-1386`). A receipt JSON records the resolved executable.
- **Launch**: the registry `cmd + args + env`, or an "Executable override" with the same args and env. A "Local ACP command" option runs an arbitrary executable plus literal args with no shell expansion.
- **Probe** (`probe.ts`): spawns the agent, runs `initialize`, and runs `session/new` to read models and config options. It then calls `session/delete` if advertised. DeepSeek gets a temp session-persistence patch because it persists `session/new` without delete (`probe.ts:480-495`).
- **Flavor detection** (`adapter.ts:200-445`) is by registry id or local command basename:
  - claude-acp: forces `CLAUDE_AGENT_ACP_EXPERIMENTAL_V2=1` and tracks subagents from `Agent`/`Task` tools.
  - copilot: HTTP MCP plus deferred tools; `alwaysSetModel`; no steer-by-restart because cancel kills background agents; restarts the runtime on a lost session.
  - devin: client terminals; `_meta` `cognition.ai/subagentSupport`; filtered permission options.
  - droid / omp / opencode: elicitation form extractors.
  - kimi: prompt-outcome watcher on `KIMI_CODE_HOME`.
  - glm: per-turn usage.
  - mistral-vibe: extensions and rate-limit mapping.
  - agy (Antigravity CLI) and Hermes: post-settle continuation, because background exits wake the root without a prompt response.
- **Sessions UI** (`apps/server/src/provider/AcpRegistrySessions.ts`) uses `session/list` and imports native sessions as a deterministic thread id `derive(driver, instanceId, sessionId)` (`:31-39`).
- **Live config**: `AcpRegistryRuntimeCoordinator` publishes `available_commands_update` (slash commands; `$`-prefixed ones become skills) and config, mode, and model updates to the picker without re-probing.

---

#### 4. Per-provider summary

| provider | transport | spawn | auth | session create/resume | prompt / stream | approvals | cancel | models |
|---|---|---|---|---|---|---|---|---|
| **Grok** (`packages/provider-grok`) | ACP stdio + xAI extensions | `grok [--permission-mode auto\|default] agent [--always-approve] stdio`, chosen by runtime mode (`acpSupport.ts:47-58`) | method `xai.api_key` if `XAI_API_KEY`, else `cached_token` (`:21-25,77-81`) | ACP; `session/load` with replay-idle readiness because load may never answer (`:144-145`) | ACP. `_meta.clientType:"extension"` on initialize (`:101`) makes Auto refusals prompt. Completion fallback via `x.ai/session/prompt_complete` (`xaiAcpExtension.ts:1502`). Ext notifications: `x.ai/session_notification`, `x.ai/task_backgrounded/completed`, `x.ai/ask_user_question`, `x.ai/exit_plan_mode` (each also `_x.ai/…`) | ACP permission; Grok options include `allow-edits-session` (session-scoped) vs project-persistent allow-always (`:103-120`) | `session/cancel` with `_meta.cancelTrigger:"ctrl_c"` (`:98`) | ACP config options; default slug `grok-build` |
| **Kiro** (`provider-kiro`) | ACP stdio | `kiro-cli acp --agent-engine=v3 --auth-method=cli` (`acpSupport.ts:23,83-84`) | `kiro-cli login` or `KIRO_API_KEY` | ACP. Fork-at-turn via `_meta.kiro.messageId` on Kiro ≥ 2.27 (`adapter.ts:68`). Hooks opt-in `v2` so Kiro runs them itself (`:554-556`) | ACP. Steering via `_session/steer` (string only, so images restart the turn) (`:74-79`). `model` config option arrives after `session/new` | ACP; kind borrowed from tool_call | `interruptPromptOnCancel:false`, because Kiro 2.27.1 can drop an early cancel (`:578-579`) | ACP `model` config option + reasoning levels; "Agent" select |
| **fx** (`provider-fx`) | ACP stdio | `fx acp` (`acpSupport.ts:363-364`) | machine `fx login`, shared across instances | ACP | ACP; backends via ACP `providers/list`/`providers/set` (backend is part of model choice) | ACP | `cancelBehavior:"wait-for-prompt"` (`acpSupport.ts:64`) | per-backend model lists; Effort option |
| **Antigravity** (`apps/server/src/provider/Antigravity*.ts`, `acp/Antigravity*.ts`, `orchestration-v2/Adapters/AntigravityAdapterV2.ts`) | ACP stdio (v1 body with version 2) | `<agy> acp --gemini-dir <profile.geminiHome> --app-data-dir antigravity-acp`, `extendEnv:false` (`antigravityAuthSupport.ts:546-573`). Managed install from the manifest at `https://antigravity-cli-auto-updater-…run.app/manifests` → `storage.googleapis.com/antigravity-public/antigravity-cli/<ver>-…` with sha512 (`antigravityCliRelease.ts:4-54`) | `authenticateEagerly` with `oauth-personal` (or API key / Vertex / Enterprise); browser login via `agy acp --login`, the code is pasted back from `https://antigravity.google/oauth-callback` (`antigravityAuthSupport.ts:575`) | `resumeMethod:"resume"`. `_meta.antigravity.systemPrompt.append` when advertised. `clientInfo` spoofs `zed` on old versions to unlock non-Gemini models (`AntigravityAcpSupport.ts:96-111`) | ACP. Client `fs/read_text_file` is served by T3 for cwd and attachments only; it does not advertise writes because that swaps diffs for whole-file edits (`:131-138`). Stdout transform scrapes the auth URL | modes `default/auto_edit/auto/yolo` ↔ T3 modes (`:156-167,228+`); agy rejects some, so they stay `default` and T3 answers | `wait-for-prompt` | ACP; in-session model + mode switch enabled (`AntigravityAdapterV2.ts` caps) |
| **Cursor** (`provider-cursor`) | **not ACP**. Child process `<T3 binary> [entry] __cursor-sdk-worker` (`ProviderOrchestrationAdapterInfrastructure.ts:41-58`) runs `@cursor/sdk` and speaks NDJSON over stdin/stdout | worker env `ELECTRON_RUN_AS_NODE=1` | browser login via `Cursor.auth.login({onLoginUrl})`, which creates an API key named `T3 Code - <name>` (`auth.ts:205-215`); or `CURSOR_API_KEY` | worker requests `open {operation create\|resume, agentId?, options: AgentOptions{model, local{cwd, autoReview, sandboxOptions, settingSources}}}` (`CursorAgentSdkWorkerProtocol.ts:52-62`) | `send {message, options{model,mcpServers}}` → `delta` frames carrying SDK `InteractionUpdate` (`text-delta, thinking-delta/completed, tool-call-started/partial/completed/delta, shell-output-delta, step-completed, turn-ended, summary-completed, task`) then `runDone`; also `steer`, `cancel`, `listMessages`, `rewindAfterRun`, `readContextUsage` | none (SDK has no callbacks). `runtimePolicy.enforcement:"native"`: autoReview + sandbox flags (`adapter.ts:191-220`) | `cancel` request | REST `https://api.cursor.com/v1/models`, `/v1/me` with the API key (`CursorSdkCatalog.ts:111-150`) |
| **OpenCode 1.x** (`provider-opencode/src/server/adapter.ts`, `OpenCodeRuntime.ts`) | HTTP + SSE via `@opencode-ai/sdk/v2` | per session `opencode serve --hostname=127.0.0.1 --port=<free>`, detached; readiness = stdout `server listening on http://…`. Env `OPENCODE_SERVER_PASSWORD` (generated 32 B if unset), `OPENCODE_CONFIG_CONTENT` (`OpenCodeRuntime.ts:613-646,373`) or an external `serverUrl` | basic auth password | `session.create`, `session.get`, `session.fork`, `session.children` | `session.promptAsync` + `event.subscribe` SSE: `message.part.delta/updated`, `message.updated`, `session.status/idle/error/compacted`, `permission.asked/replied`, `question.asked/replied/rejected`, `todo.updated` | `permission.reply`, `question.reply` | `session.abort` | provider list from the server |
| **OpenCode 2.x** (`v2/OpenCode2Client.ts`, `OpenCode2Server.ts`, `v2/adapter.ts`) | HTTP + SSE via `@opencode/client/effect`; Basic auth user `opencode` (`OpenCode2Client.ts:22`); SSE `GET /api/event` with a 45 s silence timeout (heartbeats 10-15 s) (`:157-200`). Unknown event types are tolerated except execution ends, which are synthesized | one shared server per instance; env `OPENCODE_PASSWORD` (drops the inherited 1.x var) (`OpenCode2Server.ts:51-58`); readiness = `server.info()`. Version is probed and v1 or v2 selected per open (`driver.ts:148-200`) | password | `session.create({location:{directory}, model, permissions:[rules], title})` (`v2/adapter.ts:4915`); `session.move`, `session.fork`, `session.revert(.stage/.commit)` | `session.prompt({sessionID, id, text, files, skills, delivery?:"steer"})`; events `session.execution.started/succeeded/failed/interrupted`, `session.step.*`, `session.text.started/delta/ended`, `session.reasoning.*`, `session.tool.input.started/called/progress/success/failed`, `session.compaction.*`, `session.retry.scheduled`, `session.inbox.*`, `permission.asked/replied`, `form.*`, `shell.*` | permission rules are native (`sessionRules`, `:455-482`); `permission.reply({decision:"once"\|"reject", message?})`; decline also sends `session.synthetic(steer)` explaining the denial; forms via `session.form.reply/cancel` (`:5446-5480`) | `session.interrupt` | `model.list`, `agent.list`, `command.list`, `skill.list` |
| **Pi** (`provider-pi`) | custom JSONL RPC over stdio, LF-delimited (U+2028-safe; `rpc.ts:1`) | `pi --mode rpc [--session <file>\|--no-session] <user args> [--offline] --extension <t3 ext>`; env `T3_MCP_URL`, `T3_MCP_BEARER_TOKEN`, `T3_PI_RUNTIME_MODE`, `T3CODE_THREAD_ID` (`mcpInjection.ts:262-323`; `adapter.ts:541-570`) | Pi's own login | session = file; fork via `--fork <file>`, `new_session`, `switch_session` | requests `{id, type, …}` → `{type:"response", id, success, data\|error}` (`rpc.ts:313-336`). Types: `prompt {message, images, streamingBehavior?:"steer"}`, `abort`, `get_state`, `get_messages`, `get_entries`, `set_model`, `set_thinking_level`, `compact`, `clear_queue`, `get_available_models`, `get_session_stats`, `set_session_name`, `extension_ui_response`. Events: `agent_start`, `message_start/update/end`, `tool_execution_start/update/end`, `auto_retry_*`, `compaction_*`, `agent_settled`, `extension_ui_request` | the T3 extension hooks `tool_call` and calls `ctx.ui.confirm`, which arrives as `extension_ui_request{method:confirm\|select\|input\|editor}`; answered with `extension_ui_response` (`mcpExtensionSource.ts:365-380`) | `abort` | `get_available_models` (slug = `provider/id`, ambiguous splits resolved against the reported list, `rpc.ts:47-60`) |
| **Muse** (`provider-muse`) | "MSP" JSON-RPC via `@muse-code/sdk` `spawnMspConnection` over stdio | `muse serve --trust-workspace [--disable-sandbox]`; read-only text-gen: `--disable-shell --disable-write --no-session-log` (`sdk.ts:146-157`). `initialize {clientInfo, capabilities:{requestedCapabilities:["sessionMcp"]}}` | `muse login` or instance `META_API_KEY`; the inherited server env one is ignored | `session/start {workspaceRoot, modelId, providerId:"meta", approvalMode, permissionProfile ":ask-me"\|":unrestricted", config.mcpServers}` / `session/resume {excludeItems:true}` (`adapter.ts:1972-1983`) | `turn/start {input:[{type:"text",…}]}`, `turn/steer`; notifications `turn/started/completed`, `item/started/delta/updated/completed` (MuseItem: `itemId, kind, revision, status, text, tool, args, commandText, exitCode, …`), `session/tokenUsage`, `session/todoListChanged`, `turn/retryScheduled` | `approval/requested` → `approval/decide {approvalId, requirementId, choiceId}` (`adapter.ts:1181-1185`); `userInput/request` → `userInput/answer` | `turn/interrupt` | `model/list` (`modelCatalog.ts:126`) |

Supporting notes for the table:
- **Grok, Kiro, fx, Antigravity** all reuse `makeAcpAdapterV2` with a flavor. Their runtimes are `AcpSessionRuntime.layer({...spawn})` plus provider meta.
- **OpenCode 2 approvals** use native permission rules: `rule(action, effect)` with `shell ask`, `edit ask|allow`, `external_directory ask`, plus allow rules for extraRoots. Plan mode adds `edit deny`. Full access is `* allow`. "Allow for workspace" pushes a grant and rewrites the rules.

---

#### 5. Bugs, races, and fragile logic

1. **The default ACP cancel desyncs from the agent.** It interrupts the local prompt fiber and fabricates `stopReason:"cancelled"` before the agent acknowledges (`AcpSessionRuntime.ts:2606-2616,2636-2646`). If the agent ignores or drops `session/cancel` (a known Kiro 2.27.1 issue, `provider-kiro/.../adapter.ts:578`), it keeps running while T3 marks the turn done. Later updates can land on the next turn. Only flavors that opt into `wait-for-prompt` are safe.
2. **Stale-generation permission requests never get answered.** `return yield* Effect.never` (`provider-acp/src/server/adapter.ts:7625,7695,7741,7778`) leaves the agent's JSON-RPC request pending forever. That is fine only if the runtime is always torn down. Any generation bump without process teardown leaks a hung agent request.
3. **`acceptForSession` silently degrades to allow-once** when the agent offers no `allow_always` (`adapter.ts:1669-1672`). The UI says "for session" but the agent will ask again, unless T3's own `AcpClientPolicyGrants` covers it.
4. **`McpProviderSessions.set` is not atomic.** It does `Ref.get` then two separate `Ref.update`s (`McpProviderSessions.ts:186-198`). Two concurrent `set`s for one thread can clear caches wrongly or interleave. There is also a reservation map outside Effect state (`ProviderSessionManager.ts:594-600`, a plain `Map` mutated in callbacks).
5. **Windows PATH case.** `withAgentDeviceEnvironment` reads `PATH ?? Path` but writes only `PATH` (`provider-core/src/server/mcpSession.ts:30-38`). On Windows the spread env can carry both `Path` (old) and `PATH` (new). Which one the child sees depends on Node's env-block ordering, so the device shim dir may be ignored.
6. **MCP endpoint fallback** `http://127.0.0.1/mcp` with no port when the server address is not inet (`McpSessionRegistry.ts:112-114`). Agents would fail to reach tools with no visible error.
7. **The MCP bearer token leaks to the whole agent process tree.** `T3_ACP_MCP_AUTHORIZATION` is put in every ACP agent's env, and Pi gets `T3_MCP_BEARER_TOKEN` (`adapter.ts:1144-1157`, `mcpInjection.ts:313-319`). Every tool or shell command the agent runs inherits a 24 h token. The `subagent`/capabilities scope limits damage, but any `npm install` postinstall script can call T3 tools.
8. **Registry install is not cross-process safe.** It does `remove(installRoot)` then `rename(staging)` (`AcpRegistrySupport.ts:1697-1698`) with only an in-process semaphore (`:701-706`). Two servers sharing T3 home can race. A crash between remove and rename leaves no binary.
9. **Probes create real native sessions.** The registry probe runs `session/new` to discover models and only deletes when the agent advertises delete (`probe.ts:652`). Agents without delete accumulate junk sessions; DeepSeek is special-cased (`:487`).
10. **OpenCode 2 MCP before 2.2.6** relies on a fixed 250 ms sleep (`MCP_TOOL_RELOAD_SETTLE`, `v2/adapter.ts:704`) to wait for tools to load 100 ms after connect (`v2/adapter.ts:4491-4494`). This is timing-based.
11. **OpenCode 2 SSE.** A 45 s silence counts as stuck (`OpenCode2Client.ts:149`). A server that pauses heartbeats under load will spuriously reconnect. Undecodable event frames are dropped, except execution ends (`:162-190`), so new event types are silently lost.
12. **ACP replay detection is heuristic.** `session/load` readiness waits for a 2 s idle gap in `isReplay`-marked updates (`AcpSessionRuntime.ts:91,2152-2210`). A slow agent replaying large history can have a >2 s pause and be considered ready mid-replay.
13. **Antigravity spoofs `clientInfo` as `zed`** on old versions to unlock models (`AntigravityAcpSupport.ts:106-111`). This is fragile if Google enforces the allowlist differently.
14. **ACP v1/v2 detection** is a structural guess (`"info" in response`) (`effect-acp/src/client.ts:719-723`). An agent sending both shapes would be misclassified.
15. **Cursor sandbox priming** caches sandbox support per worker process ("SDK caches sandbox support per process", `CursorAgentSdk.ts:548-573`). A single shared worker (`shared:true`) across instances means one instance's sandbox state can affect another.

---

#### 6. Rust reimplementation pointers

- Model the adapter as a trait with `open_session(...) -> Box<dyn SessionRuntime>`. Each runtime yields one `mpsc` stream of the §1.4 events, and consumers rebuild state from full-item upserts (`turn_item.updated`).
- Implement one generic ACP client:
  - NDJSON over stdio, using the hybrid initialize and the generation flag.
  - The method table from §3.1.
  - load vs resume vs new.
  - Auth-required retry.
  - A prompt queue with a per-session dispatch mutex, so a cancel waits for its own prompt.
  - A permission-policy function as in §3.3.

  Most providers (Grok, Kiro, fx, Antigravity, every registry agent) are only flavor hooks on top of it.
- Cursor (Node SDK), OpenCode (HTTP/SSE), Pi (JSONL RPC + TS extension), and Muse (Node SDK) need bespoke clients. Cursor and Muse require a Node sidecar because their protocols live in npm SDKs (`@cursor/sdk`, `@muse-code/sdk`).
- The MCP server is one Streamable-HTTP endpoint with per-thread bearer tokens. Inject it per provider using the §2 table, and provide a stdio bridge subcommand for ACP agents that support only stdio MCP.

---

# 4. Performance-relevant design

(See also §2 for the bounded snapshot/replay design. Section 5.1-5.2 lists the user-visible perf bugs.)

### D. Performance-relevant design

#### D.1 Streaming pipeline (server)
1. **Adapter delta coalescer**: provider text deltas are buffered per (turn, item) and flushed every **50 ms**, or 400 ms for Codex plans (`Adapters/ProviderTextDeltaCoalescer.ts`; `CodexAdapterV2.ts:314-316`; `ClaudeAdapterV2.ts:9594`). Each flush emits a **full-text** `message.updated` / `turn_item.updated` snapshot.
2. **Paragraph-mode filter** (`orchestration-v2/assistantStreaming.ts:114-243`, default setting `responseStreamingMode="paragraph"`, `contracts/src/settings.ts:1151,1411`):
   - A streaming assistant or reasoning snapshot is released only up to a **markdown block boundary**: a closed fence, a blank line, a list-item start or an ATX heading. Partial lines are never released, and a section title is held until content follows it.
   - Releases are at most **every 400 ms**. A held snapshot is flushed whole after **800 ms idle**.
   - `turn` mode suppresses all intermediate snapshots.
   - The scan is incremental (`advanceScan` resumes from the previous text when the new text is a prefix extension).
3. **ProviderCommitBatch** supersedes same-entity snapshots inside a write batch (B.3).
4. Persist: **every surviving snapshot is a full-entity row** in `orchestration_events` *and* an upsert of `payload_json` in the projection (write amplification of roughly O(textLength × updates) per message, bounded by step 2's ~2.5 updates/s).
5. **Per-subscriber live coalescer** (`ThreadLiveEventCoalescer.ts:18-56, 70-210`): 50 ms window, max 512 pending. Only *running* streaming text and tool items (command/file_change/file_search/web_search/dynamic_tool) coalesce by key `(thread, run, type, itemType, id)`. Any non-coalescible event flushes first, which preserves order. Replay batches use the same logic (`coalesceLiveUpdatedEvents`).
6. **LiveStreamBudget**: each subscriber buffers ≤ 1000 items or 8 MiB serialized. Overflow fails the stream with "resume from last sequence" (`LiveStreamBudget.ts:18-19, 54-75`). Serialized sizes are cached in a `WeakMap` keyed by the shared event object, and wire projections are shared across subscribers (`WireProjection.ts:340-367`).
7. **Shell stream**: application events are reduced to `{sequence, threadId}`, grouped with `Stream.groupedWithin(512, 50ms)`, and deduped per thread. The **thread shell row is re-read from SQL** for each batch (a batch `getShellSnapshot` when >1 thread), and rows equal to the last one sent are skipped for up to 5 s (`ws.ts:1250-1300`, `ShellStream.ts:575-590, 753-790`). Subagent transcript events are filtered out entirely (`ShellStream.ts:535-562`).

#### D.2 Client rendering (web)
- **Virtualization**: `@legendapp/list` `LegendList` (`apps/web/src/components/chat/MessagesTimeline.tsx:54, 1491-1530`), with `estimatedItemSize=90`, `maintainScrollAtEnd` (smooth while working), `maintainVisibleContentPosition`, `initialScrollAtEnd`, and `keyExtractor=row.id`. `getItemType` buckets messages by `log2(len/1000)`, and messages over ~4k chars each get their **own type** (`message:<id>`) to avoid height-estimate loops (`:1648-1660`). Rows that hold a native text selection are pinned with `alwaysRender`, since recycling would move the selection (`:1602-1640`).
- **Row derivation**: `deriveMessagesTimelineRows` (`MessagesTimeline.logic.ts:1619`) runs on each projection change. A fast path, `replaceStreamingMessageRows`, only swaps the streaming message row (`:2745-2756`). `computeStableMessagesTimelineRows` keeps unchanged rows by identity so that the memoized `TimelineRowContent` skips them (`:2758-2780`, `MessagesTimeline.tsx:1662`).
- **Markdown**: `@pierre/words` (a WASM markdown → HTML renderer) → parsed to hast → `hast-util-to-jsx-runtime` (`apps/web/src/markdown/wordsHast.ts:1-10`, `components/chat/markdownRenderer.ts`). A global LRU of rendered elements is keyed by text, about 500k chars (~10 MB) (`markdownRenderer.ts:14-36`). While streaming, `splitMarkdownBlocks` cuts the text at blank lines that close every block, so **only the last block re-renders** (`markdownStreamBlocks.ts:24`, `ChatMarkdown.tsx:2085-2100`).
- **Code highlighting**: `@pierre/diffs` `getSharedHighlighter` with `preferredHighlighter:"highlights"` (hand-written WASM lexers, described as "10-40x faster than Shiki", `apps/web/src/lib/syntaxHighlighting.ts:17-44`). The highlighter is still Shiki-API compatible (`codeToTokens`, `codeToHtml`). Streaming code blocks use an incremental highlighted document (`MarkdownCodeBlock.tsx:332-345`). Diffs use `@pierre/diffs` workers. Mermaid uses `pele/lazy`, and math uses KaTeX.
- Store: Effect atoms. Each thread atom has `idleTTL 0`, and its resume cache has 5 min. `runningThreadKeepAliveAtom` keeps detail streams mounted for running threads until they are reported stopped (`apps/web/src/state/threads.ts:60-134`).

#### D.3 Inefficiencies, bugs and races observed

1. **Full-snapshot events = write amplification.** Every streaming update stores the whole message text twice: once as an event row and once in the projection `payload_json` upsert, plus JSON encoding and decoding. A 20 KB answer updated ~50 times writes ~2 MB of event payload. The paragraph filter and batch supersession reduce this but do not remove it. **For Rust**: store append-only text deltas, or keep events as snapshots but exclude streaming bodies from the event log (keep only the final state).
2. **Thread live fan-out is O(subscribers × all events).** Every `subscribeThread` subscribes to the single global unbounded `liveEvents` PubSub and filters by threadId (`EventSink.ts:294, 1034-1060`). **For Rust**: use a per-thread broadcast channel.
3. **The shell stream re-queries SQL per subscriber per batch.** N clients × every 50 ms batch × shell-row recompute (`ws.ts:1250-1290`). The rows are derived (status, latest message, pending request, counts), so this is real work. **For Rust**: compute the shell row once at commit (or memoize per sequence) and broadcast it.
4. **Unbounded PubSubs** (`committedEvents`, `liveEvents`, `OrchestrationEventStore.ts:306` / `EventSink.ts:294`). Per-subscriber budgets bound memory per stream, but publication into an unbounded queue with a stalled subscriber depends on that subscriber's budget failing the stream promptly.
5. **Effect RPC JSON overhead.** The JSON in each `Chunk` is re-stringified for size accounting (`LiveStreamBudget.serializedSize`, cached) and for replay byte checks (`threadReplayEncodedBytes` calls `JSON.stringify` per item, `ThreadStream.ts:26-35`). Small cost, but it doubles serialization on resume.
6. **Client upserts are O(n) with full array copies** (`upsertEntity` spread copy + `findIndex`, `orchestrationV2Projection.ts:57-67`), and every event allocates a new projection object. This is fine within the 75-row windows, but `messages`/`runs`/`nodes` grow over a long session as live events append without trimming. I found no trimming of out-of-window entities on the live path. **For Rust**: use indexed maps (id → slot).
7. **Unknown-event cursor skip**: `loadReplayThrough(..., skipUnknownEventTypes:true)` (`ws.ts:982-990`) silently drops rows the server build cannot decode. This is intentional, so that downgrades keep working, but a client then advances its cursor past data it never saw. Benign, since the event is unknowable either way.
8. **The resume re-check race is handled.** Pruning publishes its boundary before deleting, and `subscribeOrchestrationV2Thread` re-reads `prunedThroughSequence` after loading the replay (`ws.ts:1100-1125`). The window between `getAgentReplayStats` and `loadReplayThrough` is covered by that re-check.
9. **Possible double-flush ordering edge in `ThreadLiveEventCoalescer`**: a replaced item is moved to the end of `pendingUpdates` (Set insertion order). Ordering stays sequence-ascending only because the replacement is always newer. Any future coalescible event emitted out of sequence order would break the client's `sequence <= lastSequence` skip, and older events would be dropped. This is fragile but correct today.
10. **The EffectWorker stuck-slot release** (`EffectWorker.ts:1046-1080`) leaves the hung fiber running and the row `running`. An effect that never settles (for example a provider that never answers) blocks its **whole thread lane** indefinitely, and only process-loss reconcile clears it.
11. **Event rows are inserted one statement per event, sequentially** (`appendAgentEvents`, `concurrency: 1`), each with a `MAX(stream_version)` subquery. This is acceptable with index seeks, but a multi-row insert would cut the number of worker round-trips. Every statement crosses a worker-thread boundary (`nodeSqliteClient` writer worker).
12. Web: there is no explicit client-side frame throttle on the thread data path that I could find. React re-render frequency is bounded by the server's ≤400 ms paragraph releases plus the 50 ms coalescer. Under `turn` mode, or on a big replay, a single large batch is applied at once.

#### D.4 Constants cheat-sheet
| Constant | Value | Where |
|---|---|---|
| Heartbeat (server→client) | 20 s | `socketHeartbeat.ts:7` |
| Client ping / dead timeout | 5 s / 15 s | `client-runtime/src/rpc/protocol.ts:10-12` |
| Frame part size | 48 KiB | `shared/src/socketFrameParts.ts:10` |
| Stream window | 8 chunks (64 KiB terminal) | `terminal/OutputProtocol.ts:16-17` |
| WS ticket TTL | 5 min | `auth/SessionStore.ts:493` |
| Adapter delta flush | 50 ms | `CodexAdapterV2.ts:314`, `ClaudeAdapterV2.ts:9594` |
| Paragraph min interval / idle flush | 400 ms / 800 ms | `assistantStreaming.ts:203,114` |
| Commit batch | 64 events / 1 MiB / 100 ms hold | `ProviderCommitBatch.ts:13-16` |
| Live coalesce window | 50 ms, 512 pending | `ThreadLiveEventCoalescer.ts:18-19` |
| Live buffer budget | 1000 items / 8 MiB | `LiveStreamBudget.ts:18-19` |
| Thread resume replay limit | 128 events / 1 MiB | `ThreadStream.ts:18-27` |
| Shell resume gap / payload | 1000 events / 8 MiB | `ShellStream.ts:45-47` |
| Bounded snapshot | 10 user turns / 75 rows / 500 in-turn / 1 MiB | `threadHistoryPaging.ts:16-21` |
| Event retention | ≥ 1 day | `ProjectionMaintenance.ts:41` |
| Effect worker | 4 slots, 30 s poll, 30 s slot wait | `EffectWorker.ts:1043-1048` |
| Command output frames | 100 ms, 16 KiB append | `CommandOutputHub.ts:32-36` |
| Thread resume cache TTL (client) | 5 min | `client-runtime/src/state/threadRetention.ts` |
| Reconnect backoff | 1 s base → 300 s cap, jitter, reset after 30 s | `connection/supervisor.ts:46-73,160` |

---

# 5. Known bugs and user complaints

## Method
- **Open-issue sample:** `gh issue list -R pingdotgg/t3code --state open --limit 200`. Those 200 are the newest open issues (#40466-#42494), and most are narrow, very detailed reports, many of them clearly agent-filed.
- **Impact ranking:** the open sample alone would mislead, so I also pulled the 200 **most-commented issues across all states** and ran keyword searches in titles (lag, slow, memory, CPU, freeze, Windows, terminal, streaming, worktree, scroll, stuck, flicker, sqlite, GPU/battery, reconnect, approval, checkpoint, interrupt/stop).
- **Repo totals:** 1,325 open (241 labelled `bug`) and 9,232 closed.
- **Notation:** `(o)` = open, `(c)` = closed (fixed or superseded), `cN` = comment count.
- **Why closed bugs matter:** most of them are regressions of a design weakness that still exists. For a rewrite they show *where the architecture hurts*, not just what is broken today.

## 5.1 Performance and lag on long threads, large state, and the UI

### Client
- **Replay and loading:**
  - #4596 (c): reopening a thread with a large event backlog replays every event one frame at a time. Each event re-derives the whole timeline, which is quadratic.
  - #26018 (c c5): long threads load and sync slowly.
  - #20427 (c c11): scrolling to the top of a 13k-item thread costs about 2.4 s of server time because every history page re-reads the whole thread.
  - #41148 (o): thread open blocks 100-424 ms on an IndexedDB read before `subscribeThread`.
- **Many threads:**
  - #25823 (o): about 13k threads freeze the renderer for about a minute on thread open (`environment-quiet-thread-shell-list` is rebuilt about 14k times).
  - #41083 (o): sidebar latency from an O(rows × drafts) New Thread scan, a focusin DOM scan, and a blocking settle-advance.
  - #42338 (o): Command Palette thread search misses its WeakMap caches on every keystroke.
- **Typing and input:**
  - #440 (c c14): multi-second view transitions and chat input lag on an M3 Max.
  - #12689 (c): the Windows desktop app lags while typing even in an empty chat, while the browser UI on the same backend is smooth. This points at Electron/GPU compositing.
  - #30657 (c): typing after a selected skill lags, worse on long prompts.
  - #28489 (o): the `/` and `$` menus lag with hundreds of skills.
- **Diff panel:** #42438 (o) scrolls at about 28 fps on large diffs, and file-header Buttons cost about a quarter of each frame.
- **Mobile:** #10847 (c) freezes on long threads, #40791 (o) re-renders the whole thread screen on every sync change, and #14649 (c) runs Android out of memory on a 200k-line diff.
- **Crash:** #996 (c c15) crashes the app on a heavy thread; the only fix was deleting `~/.t3`.
- **Scroll anchoring** (very common and keeps regressing):
  - #20366 (c c13): chat snaps back to the bottom while you are scrolled up during a turn (regression of #4668).
  - Scroll-to-end pill: #12372 stays hidden after a thread switch, #26338 and #26385 jump around or flicker, and #30515 (o) never appears after opening a thread from search.
  - #15469 (c): iOS jumps far above the latest message.
  - #12094 (c): timeline rows overlap while a tool group is streaming.

### Server: the event store is the root cause of most perf bugs
- **Write amplification from streaming:**
  - #5110 (c): one 10.5k-char reply created **3,024 durable events** (median delta 3 chars). Clearing history restored responsiveness, and the DB grew from 282 KB to 218 MB in 25 h.
  - #5719 and #4008 (c): every streamed text delta caused a full-thread projection or activity scan.
  - #23365 (o c6): paragraph streaming still stores every full reasoning snapshot as a permanent event (OpenCode: 93k rows, 2.2 GB in a day).
  - #21154 (c): `orchestration_events` grows without bound.
- **Unbounded DB growth:**
  - #28903 (c c17): **`statev2.sqlite` reached 33 GB in 7 weeks**: events 15.6 GB, turn items 6.8 GB, nodes 2.2 GB, indexes 3 GB.
  - #17845 (o c20): deleting a thread says it clears history, but V2 keeps every event and projection row.
- **SQLite blocking the event loop:**
  - #30065 (c): SQLite writes ran on the main event loop, so one slow transaction disconnected every client.
  - #15707 (c): event writes stall the server on a disk with a 156 ms fsync, and the environment flips Offline.
  - #23482 (c): any open read transaction freezes every write once the WAL passes 32 MB.
  - #26224 (c): the event loop freezes 6-28 s on a 5.9 GB DB.
  - #24660 (c): all reads shared one reader with one permit.
  - #41997 (o): the event loop blocks 9-21 s on an idle machine.
  - #40987 (o): request authentication waits on a synced disk write, adding 10-35 s per request on a busy host.
  - #38159 (o): a 100 ms idle WAL checkpoint doubles disk writes (about 200 MiB/min while agents run).
  - #35995 (o): startup is blocked about 7 min by migrations, and clients get an empty 503.
- **Other server stalls:**
  - #21289 (c): every git spawn blocks the event loop longer as server memory grows (Node `fork` on Linux).
  - #16636 (c): one unresponsive provider holds all 4 effect-worker slots and stalls every thread. The code still has this risk per thread lane; see §4 D.3 item 10.
  - #695 (c c27): "T3 Code is significantly slower than Codex for the same task (20+ min vs about 3.5 min)". End-to-end orchestration overhead is visible to users.

## 5.2 Memory, CPU, GPU, battery
- **Memory leaks:**
  - #2767 (c): 63 GB RAM after macOS sleep.
  - #29638 (c): the desktop app grows to about 139 GB.
  - #25172 (c): the server grows about 1.7 GB/hour because each 1-minute PR sync sweep is retained.
- **Idle cost:**
  - #3143 (c c12): 12 h energy use is 136× that of comparable Electron apps.
  - #40581 (o): constant GPU load, about 60 W on a MacBook, and about 41 W in the web UI.
  - #2646, #4074, #4182 (c): high CPU on macOS, Linux and Windows.
  - #4773 (c): the idle VCS/provider refresh loop makes the backend CPU-bound and causes reconnects.
- **Preview browser:**
  - #14730 (o): an inactive macOS preview uses 228% GPU and about 10 GB of graphics memory.
  - #37987 (o): a preview renderer loops on dom-ready and grows to about 11 GB.
  - #18165 (o): on Windows, headless Chrome's SwiftShader GPU process busy-spins about 5 cores while idle.
- **Visual effects:** backdrop blur and glass effects cause high-refresh flicker or GPU cost: #40809, #30759, #7766 (o "Flickering sidebar makes it unusable").
- **External API quota:** #13496 (c): PR details are re-fetched for every displayed PR on focus, visibility change and a 5-minute tick, which exhausts GitHub GraphQL quota.
- **Agent workloads:** #5248 (c): systemd-oomd kills the whole T3 scope when an agent's child workload exhausts memory. #20329 (c): agent memory limits apply on Linux only.

## 5.3 Streaming, turn-state, and Stop glitches
- **Empty thread views** (the #1 most-commented recent class):
  - #27443 (c **c69**): the thread view stays empty for the whole run when the client requests history before the thread exists.
  - Siblings: #27623 (the draft route marks the thread deleted on a 404 before `launchThread`), #27615, #27527, #27704 (chat messages disappear), #4647.
- **Stuck "running":**
  - #15197 (c): a Claude run stays "running" forever after `result/success`.
  - #4713 (c): stuck in `running` after interrupt, and Stop becomes a no-op.
  - #2234 (c c21): a thread cannot be stopped.
  - #1048 (c): "waiting for 0s".
  - #2644 (c): OpenCode shows "working…" after the CLI has finished.
  - #15124 (o): a waiting run shows "Agent is working" when checkpoint capture stalls.
  - #14572 (c): messages stay marked as streaming after a server restart mid-response.
- **Queue and steer:**
  - #18805 (c): a queued message stays stuck after Stop.
  - #41914 (o): a steered message won't send after a manual cancel.
  - #21895 (c c23): agent steers on Claude cancel tools and look like user refusals.
  - #41407 (o): Codex auto-sends wait behind later explicit messages.
- **Folding and ordering:**
  - #8879 (c c22): turn folding hides the substantive answer when a turn ends with a short progress note.
  - #41320 (o): completed answers fold under "Continued for".
  - #5297 (o): a Claude stop-hook continuation hides the original response.
  - #42028 (o): two numbered lists in one reply merge into one.
- **Background work and delegation** (a large cluster):
  - #21837, #24297, #16603, #17512, #15888 (c), #22925 and #26527 (o): a delegated child or subagent finishes but its parent is never woken, or the parent finalizes on a stale result.
  - #41890 (o): interrupting a parent stops its background subagents.
  - #25585 (c): a thread shows "done" at every intermediate stop of a multi-stage job.

## 5.4 Provider quirks

### Claude Code
- **Token usage:**
  - #7338 (c **c81**, the most-commented issue ever): usage runs 3-5× higher than the native Claude app.
  - Likely contributors in code: every Stop kills the CLI and forces a full `--resume`, the 2-min idle release re-reads the jsonl and drops the prompt cache, and T3 appends system-prompt text and MCP tools to every turn. See §3b and §8 of the Claude section.
- **Lost context:**
  - #2256 (c c27): session context is lost after about an hour idle.
  - #24152 (c): resumes the pre-`/clear` session.
  - #17467 (c c16): Enter on a stale thread runs `/compact` first.
  - #2121, #2034 (c): compaction and context-window calculation are wrong.
- **Lifecycle:**
  - #15854 (c): Stop before `system/init` bricks the thread (it resumes a session that was never created).
  - #41331 (o): runs are interrupted by T3 at the first stream event.
  - #40762 (o): the stalled-turn sweep interrupts turns after macOS sleep, counting sleep as silence.
  - #41125 (o): rollback to a turn that was stopped before any reply fails with "no SDK assistant message cursor".
  - #16000 (c): changing runtime mode, instance or worktree kills live subagents.
  - #40590 (o): crashes on Windows with exit code 4294967295.
- **MCP and hooks:**
  - #23151 (c): the 90 s `setMcpServers` limit is shorter than SessionStart hooks.
  - #40549 (o): every `.mcp.json` server starts without approval.
  - #40083 (o): a sign-in notice appears for every plugin MCP server.
  - #31015 (c): `/mcp` sent `transfer-encoding: chunked` with an unchunked body, so agents got no T3 tools.
- **Auth and permissions:**
  - #2653, #41195, #40466: false "not authenticated", or the profile is signed out at token refresh.
  - #15353 (o): Auto mode runs ask-rule commands without prompting.

### Codex
- **Shared app-server fragility:** #16502 (c): sub-agents keep failing after the shared app-server is killed by SIGKILL. #17045 (c): threads stay broken after the app-server exits. One process per instance means one crash breaks every thread.
- **Writer locks:** #8259 (c), #30630 (o): "already has an active writer" after switching instances or accounts.
- **Idle release:** #42123 (o): the 2-minute idle release (#41174) makes the first turn after a pause take 20-60 s on long-context sessions. This trades memory for latency.
- **Sandbox and approvals:** #13987 (o): turns reset sandbox network access to false in Auto modes. #37298 (c): Full access still prompts for forced `rm` under granular approvals.
- **Connection and capacity:** #42142 (o): 6 retries end in "provider didn't reach" (connection reset). #41249 and #41250 (o): T3 interrupts Codex during a native capacity reconnect, and model changes during backoff retry the old model.
- **Binary discovery and probes:** #234, #269 (c): not found on PATH (Homebrew; `'codex' is not recognized` on Windows). #27230 (c): the provider check times out after 20 s. #6116 (c): on Windows, an invalid `initialize` payload. #40696 (o): a wrong binary gives an opaque probe failure.
- **Text generation:** #41799 (o): Codex text generation runs user notify hooks. #40529 (o): it fails when launch args set `mcp_servers`.

### OpenCode, Cursor, ACP-family
- **OpenCode:**
  - #2691 (c): SSE events silently dropped, freezing the first message.
  - #11790 (c): inventory times out.
  - #11458 (o c17): Auto ignores the user's permission patterns.
  - #4795 (c): skill permission requests are silently dropped.
  - #41870 (o): a poisoned resume loop.
  - #23962 (c): a stall of the shared server fails every thread.
  - #42213 (o): the Build/Plan agent selector is never exposed.
- **Cursor:**
  - #40503 (c): stuck on "Starting Cursor" for 7-9 min.
  - #42127 (o): full git clones of every plugin repo.
  - #41929 (o): Linux `worker.register failed`.
  - #19112 (c): SDK retries replay a finished turn and re-run its tools.
  - #40502 (o): no AGENTS.md or rules loaded.
- **ACP / Antigravity / Grok:** dozens of reports, for example:
  - #36105: background commands end turns early.
  - #41467: native `schedule` pins turns at running.
  - #36508: a numeric enum in a T3 tool schema makes Gemini return 400 on every turn.
  - #38123 (o): Antigravity drops T3 MCP tools larger than 1,280 B.
  - #4109: BigInt RequestId crash.
  - #40713: an Antigravity account was suspended for ToS during delegation.
  - Code-found: ACP cancel is marked locally before the agent acknowledges (§3c bug 1).
- **Cross-provider:** #42024 (o): changing permissions mid-turn fails the turn on providers that cannot switch modes in session. #2736 (c c33): skill discovery returns zero for Cursor and OpenCode.

## 5.5 Windows
- **Process and console:**
  - #2537 (c c13): frequent cmd.exe/conhost flashes from provider probes and VCS kill paths.
  - #980 (c): the app frequently goes "Not responding".
  - #4182 (c): excessive CPU.
  - #12689 (c): typing lag in the desktop app.
  - #9650 (c c23): Antigravity health checks leave large `_MEI*` folders in `%TEMP%`.
- **Credential and state files** (bricking bugs):
  - #8341 (o c26): an orphaned safeStorage key makes `connection-catalog.json` permanently undecryptable.
  - #15001 (c): V2 seeds Local State from a stale profile, with the same result.
  - #4750 (o): `connection-catalog.json` is written non-atomically every ~3 s, and a corrupt copy bricks the app permanently.
- **Updates and services:** #25450 (o): updates fail ("Failed to uninstall old application files: 2") when the main process outlives the quit. #41165 (c): the Stable→Nightly npm lookup times out. #14070 (c c31): T3 Connect provisioning fails repeatedly on Win11.
- **WSL:** #192 (c, 145 reactions): WSL support request. #14393, #13738 (o): stuck "Connecting to WSL…". #19735 (c): the old server survives an update. #17090 (c): previews in WSL are laggy.
- **Previews:** #23139 (c c22): snapshots stall while the window is covered. #16921 (c): timeouts evict the shared browser host.
- **Git cost:** #40498 (o): each turn diff runs the same `git log` twice, 4 git calls (270 ms) where 3 would do. Process spawn is expensive on Windows, so batch git work.
- **Terminal:** #731 (c): Ctrl+W does not close the terminal on Windows.
- **Other:** #2115 (c): approval stuck "Pending" on Windows. #10942 (c): WinGet-installed `glab` not detected. #41284 (o): SSH hangs with OpenSSH 7.7.
- **Code-found:**
  - The Claude adapter's win32 branch drops stderr and has no process-tree kill or descendant reaping (§3b bug 1).
  - Codex `.cmd` shims run with `shell:true`, and killing the wrapper may orphan `codex.exe` (§3a bug 13).
  - `PATH` and `Path` can both reach the child env (§3c bug 5).
  - For the rewrite: use Job Objects for tree-kill, `CREATE_NO_WINDOW`, resolve `.cmd` shims to the real exe, and write files atomically (temp file + `MoveFileEx`/`ReplaceFile`).

## 5.6 Terminal
- **Input and keys:** #13046 (c): `@` cannot be typed with a German layout on macOS. #6362 (c): Escape does not reach nvim (the app intercepts it). #731 (c): Ctrl+W on Windows. #12756 (c): slow keystroke echo on Android.
- **Escape-sequence handling:** #6058 (c): no reply to the Primary Device Attribute query, so fish stalls 10 s on every start. #8574 (c): raw mouse event sequences are shown.
- **UI:** #19536 (c): the floating toolbar covers the first line. #16114 (c): selecting text spawns menus.
- **Lifecycle:** #24154 (c): "cwd does not exist" after storage cleanup removes a worktree.
- **Implementation notes:** the current stack is server `node-pty` (patched), a web renderer of libghostty-vt WASM on Canvas 2D, and scrollback persisted to disk and replayed. In Rust, `portable-pty` plus a VT engine such as `alacritty_terminal` or libghostty-vt gives DA/DSR replies and proper key encoding for free.

## 5.7 Git, worktrees, checkpoints
- **Checkpoint capture on large repos** (open and impactful):
  - #26227 (o c30): capture hits the 30 s git timeout and never converges, and every killed attempt leaves a `tmp_objdir-bulk-fsync-*` directory.
  - #3646 (c): the same failure retried every turn means permanent CPU burn and `tmp_pack` litter.
  - #38102 (o): **checkpoint refs are never pruned and have no size guard**. Untracked build caches captured every turn filled a 96 GB disk.
  - #21328 (c): new worktrees stay racily clean, so every capture re-hashes the checkout (2 s instead of 0.2 s).
  - For the rewrite: honor `.gitignore` plus a size cap, skip huge untracked files, prune refs per thread, make capture incremental, and clean temp dirs on kill.
- **Rollback and revert:**
  - #15295, #585 (c): "Edit from here" missing because of a checkpoint race.
  - #17187 (c): rollback is hidden in non-git workspaces.
  - #41219 (o): rollback past a question turn leaves the question card stuck.
  - #41125 (o): Claude rollback before any reply fails.
  - Code-found: Codex rollback counts T3 turns but reverts native turns (§3a bug 6).
- **Worktrees:**
  - #17813 (c): cleanup never removes a worktree whose branch was renamed or detached.
  - #13836 (c): worktrees made by the `t3.json` setup action are never cleaned up.
  - #11735 (c): a locked `.git/config` leaves an orphan worktree and branch.
  - #19124 (c): a thread cannot start after a merge removes its worktree.
  - #11078 (o): the sidebar keeps the stale branch label.
  - #272 (c): hardcoded `t3code/` branch prefix.
  - #40911 (o): Mix deps under gitignored `deps/` break restore.
  - #42402 (o): PR checkout offers a second worktree for a branch that is already checked out.
  - Handoff bugs: #15993, #22971, #15136.
- **Diff and commit:**
  - #4022 (c): the diff panel's Working tree/Branch scopes are empty in packaged builds (path guard compared against `$HOME`).
  - #10949 (c): staged files hidden.
  - #1194 (c): git commit timeout.
  - #4970 (c): a reused branch stays tied to an old merged PR.
  - #16183 (c): scheduled tasks always branch from `main`.

## 5.8 Connection, remote, process ownership
- **Reconnect flapping:**
  - #2366 (c c14): "Disconnected… Reconnecting" on every message.
  - #3054 (c): the WebSocket flaps on lossy or reordering links. The current client declares the socket dead after 15 s without a frame.
  - #3553 (c): a foreground health check timeout puts the local environment into reconnecting.
  - #4589 (c): threads silently stop updating until restart.
  - Cause: SQLite stalls (§5.1) also present as disconnects.
- **Relay and pairing:** #16810 (c c54) returns 504 with a bare "failed", and #3746 (c c25) fails to connect. #33183, #42190: sessions expire after a hard-coded 30 days. #40725: DPoP fails behind a proxy.
- **Ownership and multiple servers:**
  - #22983 (c): quitting leaves the server running, so the next launch cannot start one.
  - #6097 (c): the desktop starts a second backend against the service DB.
  - #14115 (c): multiple servers share one `--base-dir` and resume each other's threads.
  - #2614 (c): orphaned `t3 serve` processes.
  - #22820 (o c34), #5749, #24225: confusion between the background service and the local backend.
  - For the rewrite: use a single-instance lock on the data dir (with an OS lock file) and clear ownership semantics.

## 5.9 Approvals and permission UX
- **Missing detail:** #42390 (o): file-edit approvals do not show the change. #42025 (o): ACP cards say "Changed files" without naming the file.
- **Delegated children:** #15082 (c c21), #22925 (o): approvals and questions are not surfaced to the parent. #21935 (c): they stay pending after cancel.
- **Restarts and elicitation:** #856 (c): plan questions get stuck after restart. #14878 (c): Claude MCP elicitation is declined without a prompt.
- **Mode inheritance:** #28110, #29966 (c): side chats ignore the parent's permission mode. #41235 (o): `t3 thread start` ignores `defaultRuntimeMode`.
- **Accepted on hidden threads:** #41220 (o): `message.dispatch` is accepted on deleted or archived threads and starts live provider turns.
- **Code-found:**
  - Claude "always allow" with no suggestion allows the whole tool (for example all of Bash) for the session (§3b bug 5).
  - ACP "for session" silently degrades to once (§3c bug 3).
  - A Codex approval card can stay pending after `serverRequest/resolved` (§3a bug 5).
  - New installs default to `full-access` (§1 Section D bugs).

## 5.10 Usage and limits display
- #7338 (token usage, above).
- #19807 (c): the Limits hub blanks Claude accounts because it re-probes `/api/oauth/usage` on every refresh and shows 429s as read failures.
- #14169 (c): the usage reset tracker is broken.
- #40483 (o): agents still show as failed after the limit resets.
- #41583 (o): a subagent finishing cancels the parent's auto-resume.
- #9352 and #33639 (c): the context window indicator is hidden by default (45 reactions).

## 5.11 Most-wanted features
- **Reaction counts:** #402 Pi (198), #193 Copilot CLI (185), #192 WSL (145), #539 OpenCode (130), #231 Steer/Queue modes (77), #3056 Antigravity (37), #538 subagents as nested threads (37), #228 usage visibility (37), #1404 fork from a message (34), #1453 multi-repo projects (33), #330 / #207 import existing Codex sessions (28 / 22), #10697 confirm close while agents run (8), #1784 LaTeX (17 comments).
- **Status:** almost all of these are implemented now, so treat them as must-have features.

## 5.12 Consolidated code-level bugs from sections 1-4
The details, with file:line, are in each section's bug list.
- **Codex** (§3a.10):
  - The 1000-iteration busy-wait turn lookup runs on the stdout reader.
  - `pendingRootTurns` is keyed by thread, so a foreign `turn/started` gets adopted as the user's turn.
  - A global 32-handler cap per app-server is shared by every thread.
  - Unhandled `item/tool/call` despite `supportsDynamicToolCallbacks:true`.
  - The version is parsed from the userAgent with a naive regex.
- **Claude** (§3b.8):
  - Private `Query.request` via `Reflect`.
  - Interrupted/cancelled is decided by substring matching on error text.
  - Tool requests outside a turn are auto-denied, so background subagents fail.
  - `input_json_delta` is ignored, so long Write/Edit inputs are invisible until done.
  - O(#projects) stat calls per turn in `locateClaudeTranscript`.
  - The recorded fixtures (CLI 2.1.111) predate the pinned SDK (^0.3.321).
- **ACP and others** (§3c.5):
  - Stale-generation permission requests are never answered (`Effect.never`).
  - The MCP bearer token is exported into the agent's whole process tree.
  - Registry installs do a non-atomic remove-then-rename.
  - Replay readiness waits on a 2 s idle heuristic.
  - OpenCode 2 waits a fixed 250 ms sleep for MCP tools.
- **Orchestration** (§4 D.3):
  - Full-entity snapshot events cause write amplification.
  - Every thread subscriber filters one global unbounded PubSub.
  - The shell stream re-queries SQL per subscriber per 50 ms batch.
  - Client upserts are O(n) array copies with no trimming of out-of-window entities.
  - An effect that never settles blocks its thread lane forever.
- **UI** (§1 bug lists):
  - The delete-worktree dialog's Cancel does not cancel the thread delete.
  - The agent is stopped before the delete, so a failed delete leaves it stopped.
  - A typo in a negated `when` key makes a shortcut fire everywhere.
  - One invalid keybinding rule blocks new defaults.
  - Keybinding config errors are never shown.
  - `Sidebar.tsx` is about 3,800 lines.
- **Plan mode:** the web composer always sends `interactionMode: DEFAULT_INTERACTION_MODE`. I verified this at `apps/web/src/components/chat/useComposerSend.ts:1491,1524,1545,1595`, `useMultipleModelSend.ts:191`, `useThreadLifecycle.tsx:575`. Plan mode is therefore not user-toggleable from the web composer at this commit, only through provider "agent=plan" model options, scheduled tasks or MCP. The proposed-plan card has copy, download and save but no "implement" button. Early T3 Code had a Chat/Plan toggle, so restore it.

## 5.13 Design implications for the Rust rewrite (fix the themes, not the symptoms)
1. **Streaming text is ephemeral.**
   - Broadcast deltas over per-thread channels.
   - Persist only coalesced checkpoints (for example at item completion, or every N seconds) and the final item.
   - Store deltas or rows, never full-entity snapshots per update.
   - Put retention, vacuum and size caps in from day one, and make thread delete actually delete.
2. **Keep the DB off the UI and network paths.**
   - Use a dedicated writer thread with batched multi-row transactions, a read pool, short read transactions and a bounded WAL (`journal_size_limit`, periodic `wal_checkpoint(TRUNCATE)` when idle).
   - Never fsync on a request auth path.
3. **Make turn state an explicit state machine owned by the server.**
   - Every terminal path (completion, Stop, error, process exit, app restart) settles items, requests and subagents.
   - The client only renders server state.
   - Stop must be gated on the provider's turn-started signal and must always converge within a timeout.
4. **Supervise processes.**
   - Run one provider process per thread, or make the shared Codex process crash-tolerant with auto-respawn and resume of every attached thread.
   - Tree-kill with Job Objects or process groups.
   - Make idle release configurable, keep the prompt cache warm, and don't kill the CLI on every Stop if `interrupt` is enough.
5. **Use a native timeline.**
   - Use virtualized rows with height caching and stable ids, and re-layout only the last markdown block while streaming.
   - Use explicit scroll-anchor state: follow mode turns off on any user scroll-up and back on only at the bottom.
   - Do incremental syntax highlighting (tree-sitter or syntect) off the UI thread.
6. **Make checkpoints safe on big repos.**
   - Use a size guard, gitignore awareness, timeouts with cleanup, per-thread ref pruning, and incremental capture. Or switch to a libgit2 tree build that only hashes changed files, using the index stat cache.
7. **Write every config, credential and state file atomically**, and recover from undecryptable stores instead of bricking.
8. **Let the terminal answer DA/DSR queries**, pass Escape and Ctrl+W through when the terminal has focus, and use layout-aware key handling.
