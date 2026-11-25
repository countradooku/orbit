# Orbit ↔ T3 Code parity checklist

Every user-facing T3 Code feature from `docs/research/t3code.md` §1, one row each, with Orbit's status verified by reading Orbit's code. T3's known bugs (§5 and the per-section lists) are in section F at the end.

**Snapshot.** Orbit's code was read on 2026-11-25 between 03:05 and 03:30, while other work was landing (`sidebar.rs`, `thread_menu.rs`, `orchestrator.rs`, `model.rs` and `wire.rs` changed during the read). Line numbers are from that snapshot and may drift by a few lines.

**Path prefixes.** Orbit: `app/` = `crates/orbit-app/src/`, `core/` = `crates/orbit-core/src/`, `proto/` = `crates/orbit-proto/src/`, `net/` = `crates/orbit-net/src/`, `term/` = `crates/orbit-terminal/src/`, `orbitd/` = `crates/orbitd/`. T3 (repo-relative): `web/` = `apps/web/src/`, `srv/` = `apps/server/src/`, `desk/` = `apps/desktop/src/`, `ct/` = `packages/contracts/src/`, `sh/` = `packages/shared/src/`, `doc/` = `docs/user/`. `#nnnn` is a T3 GitHub issue.

**Status.** **Done**: works end to end in the app. **Partial**: some of it exists (often engine or library code with no UI, or a narrower UI); the "Better" column says what is missing. **Missing**: nothing usable. **N/A**: tied to T3's hosted cloud or accounts, or to the mobile app; each N/A row says why.

**Priority.** **P0**: core daily workflow. **P1**: frequent. **P2**: occasional.

## Summary

623 feature rows (section E's 10 design-only rows are not counted).

| Status | Rows | P0 | P1 | P2 |
|---|---:|---:|---:|---:|
| Done | 84 | 42 | 31 | 11 |
| Partial | 129 | 11 | 82 | 36 |
| Missing | 397 | 4 | 87 | 306 |
| N/A | 13 | 0 | 0 | 13 |
| **Total** | **623** | **57** | **200** | **366** |

Where the gaps are:

* **P0 is mostly there.** 42 of 57 P0 rows are done: projects, threads, streaming, tools, approvals, plan mode, steer and queue, Stop, model and access, context meter, review scopes, Inbox. The 4 missing P0 rows are **drafts that survive a restart**, **saved settings**, **OS notifications**, and **failure notifications**. The 11 partial P0 rows are worktrees (library done, engine refuses), message search (engine done, no UI), base-branch diff, revert files (engine done, no UI), checkpoint pinning, question answers (no free text or multi-select), edit-from-here, scroll anchoring, error retry, row states and diff rendering.
* **P1 is where the work is**: 82 partial and 87 missing. Most partial P1 rows are engine features with no UI yet: fork, restore, commit and PR, branches, terminal, worktrees, provider instances, search.
* **Whole areas are missing**: pull requests and issues, text generation, terminal UI, notifications, usage page, keybinding config, additional providers (ACP, OpenCode, Pi, Cursor), preview browser, workflows, schedules, voice, SnapShot, MCP server and CLI. All except notifications, terminal and providers are P2.
* **Section F.14 lists 13 Orbit defects** found while verifying, including checkpoint trees that `git gc` can delete, work lost on restart (queue, streaming items, pending requests, snooze timers), and no data-folder lock.

## Build order

Ten work packages, each sized for one engineer. **WP1 runs first and alone**: it freezes the protocol additions everyone else needs and splits `orchestrator.rs` (1,500 lines) into per-area engine modules, so that **WP2–WP9 can then run in parallel without editing the same files**. WP10 runs last.

Shared touchpoints are owned by one package each: `proto/*` by WP1 (later additions go through the WP1 owner); `app/main.rs` and `app/actions.rs` by WP7 (other packages export an `init(cx)` and define their own actions in their own files); `app/assets/orbit-theme.json` by WP8. Cross-package dependencies are listed per package.

### WP1 — Engine foundation and state correctness (first, sequential)

Fixes the defects in F.14 and unblocks every other package.

* Split `core/engine/orchestrator.rs` into `core/engine/{threads,turns,agent,timers,search}.rs`, plus empty owned modules for later packages: `workspaces.rs` (WP2), `review.rs` (WP3), `forge.rs` (WP4), `files.rs` (WP6), `sessions.rs` (WP9).
* Data-folder lock (single engine per store). Restart reconciliation: rebuild the run queue from Queued items, settle `Streaming` items as Cancelled, clear or mark stale pending requests with a notice, re-arm snooze timers. Reject `Send` on archived threads.
* Pin turn checkpoints with `checkpoint()` refs and prune them; check `turn.interrupted` before releasing a held prompt; bounded baseline wait with "started without a baseline" notice.
* Fix the timeline-open race (sequence the `OpenThread` reply with the feed).
* Atomic `CreateThread { initial_message }`. Host-side storage for drafts and settings (commands and events), `ReadBlob` for large tool output.
* Freeze the protocol additions other packages need: thread links (PRs, issues), tags, auto-settle policy, side-chat and subagent parentage, new item kinds (subagent, compaction, handoff, fork, session reset, worktree setup), context-chip type for messages, file search, worktree options on `CreateThread`.
* Engine timers for auto-settle.

Files: `proto/model.rs`, `proto/wire.rs`, `core/engine/orchestrator.rs` → `core/engine/*.rs`, `core/engine/mod.rs`, `core/store.rs`, `net/state.rs`, `app/app_state.rs`, `crates/orbit-core/src/engine/tests.rs`.

### WP2 — Workspaces: worktrees, project scripts, terminal, editors

Worktree per thread (selector, base-branch picker, "new thread in this worktree", recreate missing, cleanup policies, location, ports, `.worktreeinclude`), live VCS status and `WorkspaceChanged`, conflict detection, thread branch display, project scripts (model, editor dialog, Run button, setup before the agent starts, teardown), project settings panel (name, icon, scripts, remove with confirmation), terminal drawer (tabs, splits, keys, Add to chat, file links, scrollback persistence), open in editor and file manager.

Files: `core/engine/workspaces.rs`, `core/git/worktree.rs`, `core/git/watch.rs`, `core/git/conflicts.rs`, `core/git/status.rs`, `core/terminal.rs`, `term/*`, `app/home.rs`, new `app/workspace_bar.rs`, new `app/terminal_panel.rs`, new `app/project_settings.rs`, new `app/editor.rs`.

### WP3 — Review and checkpoints

Every diff scope in the UI (any turn, base ref with remote toggle, single commit, single tool call), file tree, wrap, whitespace, hide tests, collapse all, scroll restore, live refresh, per-hunk revert, review comments sent to the agent, whole-file comments, file actions, "Revert files too" and undo restore, multi-repo patches, diff size handling.

Files: `app/review.rs` → `app/review/*.rs`, `core/engine/review.rs`, `core/git/diff.rs`, `core/git/checkpoint.rs`, `proto/diff.rs`.
Depends on: WP2 for `WorkspaceChanged` (live refresh); WP6's chip API for comments.

### WP4 — Source control and forge

Git actions control (commit, push, commit-push-PR, pull, publish, default-branch confirm, progress), commit dialog, PR creation linked to the thread (including agent-run `gh pr create`), PR list, detail tabs, checks, merge and review, PR watch with back-off, work on a PR, linked items, issues page and Linear, more hosts (GitLab, Forgejo, Azure, Bitbucket), commit, PR and branch-name generation with writing styles.

Files: `core/engine/forge.rs`, `core/git/remote.rs`, new `core/forge/*.rs`, new `app/git_actions.rs`, new `app/pull_requests.rs`, new `app/issues.rs`.
Depends on: WP9's one-shot text-generation API.

### WP5 — Timeline

Scroll-to-end pill and anchoring tests, find in thread, minimap, visit watermark, assistant hover bar (fork, reply, timestamp), cite selection, code-block actions, file links, image viewer, live elapsed counter, item inspector with full output, open diff from a tool row, subagent rows and transcript dialog, lifecycle rows (fork, handoff, compaction, new session), error Retry, usage-limit recovery banner, branch-changed banner, worktree setup card, full question panel (custom answer, multi-select, several questions), approval buttons with feedback, plan card menu, edit-from-here with "revert files too".

Files: `app/thread/mod.rs`, `app/thread/rows.rs`, new `app/thread/{find,minimap,inspector,subagent,banners}.rs`.

### WP6 — Composer

Persisted drafts, `@` file mentions (with `core/engine/files.rs` search), `@` thread and model mentions, `$` skills, `/` command menu (with `/clear`, `/compact`, `/model`, `/side`), context chips, paste handling (images, large paste as file on request), drag and drop, attachment strip with thumbnails and retry, limits, send-key setting, queued-message panel (edit, send now, move to front), prompt history, stash, model picker favourites and search, effort keys, rich-text editing, voice input (last).

Files: `app/composer.rs` → `app/composer/{mod,mentions,slash,paste,drafts,history,queue,model_picker}.rs`, `core/engine/files.rs`.
Touchpoint: one line in `app/thread/mod.rs` (WP5) to mount the queue panel.

### WP7 — Shell, navigation and attention

Sidebar: multi-select and bulk actions, project filter, search, drag and drop, hover card, richer row states, PR and terminal badges, persisted shelves and width. Thread menu additions, custom snooze dialog, undo toasts. Palette modes, groups and actions. User keybindings file with live reload, `mod` on macOS, fixed keys (F2, arrows, Shift+F10), quit confirmation, next and previous in sidebar order. OS notifications (input, completion, failure) with suppression, sounds and taskbar badge. Window state, single-instance deep links, app menu.

Files: `app/shell.rs`, `app/sidebar.rs`, `app/thread_menu.rs`, `app/palette.rs`, `app/actions.rs`, `app/inbox.rs`, `app/fleet.rs`, `app/main.rs`, new `app/keymap.rs`, new `app/notify.rs`.

### WP8 — Settings, appearance, usage and limits

Saved preferences, settings pages (General, Appearance, Chat, Diffs, Threads, Providers, Archive, Maintenance, About), confirmations, chat width, fonts and zoom, themes and theme editor, colour-blind diff palette, translations, usage page and price overrides, limits panel, storage maintenance and diagnostics.

Files: `app/settings.rs` → `app/settings/*.rs`, `app/theme.rs`, `app/assets/orbit-theme.json`, `app/ui.rs`, `app/format.rs`, new `app/usage.rs`, new `core/usage.rs`.

### WP9 — Providers and sessions

ACP driver and registry (then Grok, Kiro, fx, Antigravity flavours), OpenCode, Pi, Cursor; provider instances (binary, args, env, config dir, custom models), several accounts, idle-session policy, provider switch with a portable handoff, `/clear` as a fresh session, compaction controls, emitting subagent, compaction and handoff items, one-shot text generation API, provider update checks, MCP sign-in, login-shell PATH capture.

Files: `core/provider/**`, `core/engine/sessions.rs`, new `core/provider/{acp,opencode,pi,cursor}/`, new `core/provider/oneshot.rs`.

### WP10 — Remote, automation and extended surfaces (last)

Host management UI (pairing link or QR, several hosts, SSH and WSL one-click hosts, authorised clients, revoke), background service, MCP server and `orbit` CLI, scheduled tasks, workflows, preview browser, SnapShot, voice back end, update flow.

Files: `net/client.rs`, `net/server.rs`, `net/lib.rs`, `orbitd/src/main.rs`, new `crates/orbit-cli/`, new `core/automation/*.rs`, new `core/mcp/*.rs`, new `app/hosts.rs`, new `app/preview/*.rs`.
Depends on: WP7 (palette and menu entries) and WP8 (settings pages).

---

## A. Projects, threads, sidebar

### Projects: model and persistence

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Project record persisted on the host (id, name, path, order) | ct/project.ts, srv/project/ProjectService.ts | Done | proto/model.rs:14, core/engine/orchestrator.rs:617, core/store.rs:1 | Small meta log, not an event store that reached 33 GB (#28903). | P0 |
| Per-project default model | ct/project.ts (`defaultModelSelection`) | Missing | | A project default wins over "last used", shown in the composer so it never surprises. | P1 |
| Per-project default workspace mode (local or worktree) | ct/project.ts (`defaultThreadEnvMode`) | Missing | | One setting, honoured by every entry point (home, palette, prompt link, schedule). | P1 |
| Logical project grouping by repository identity (repository / repository_path / separate) | client-runtime/state/projectGrouping.ts, web/sidebarProjectGrouping.ts | Missing | | Worktrees and clones of one repo fold under one project automatically. | P2 |
| Per-checkout grouping override | web/components/settings/ProjectSettingsPanel.tsx:90-145 | Missing | | Override lives on the checkout row, no separate settings maze. | P2 |
| Checked-in project file (icon, workspace mode, submodules, ignoreRepositories, worktreeLayout, sparse patterns, scripts) | ct/t3ProjectFile.ts, srv/project/T3ProjectFileLoader.ts | Missing | | Read an `orbit.toml`, and also read `t3.json` so teams can switch without rewriting config. | P2 |
| Settings resolution order: project, environment, checked-in file, default | doc/project-settings.md | Missing | | Show the winning layer next to each value. | P2 |
| Tracked-folder rescan (every 30 s) | srv/project/ProjectService.ts:193 | Missing | | File-watcher driven (core/git/watch.rs), never a timer; idle refresh loops caused CPU burn (#4773). | P2 |

### Projects: adding

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Add a local folder as a project (folder picker) | web/components/CommandPalette.tsx, CommandPaletteAddProject.tsx | Done | app/shell.rs:185, core/engine/orchestrator.rs:617 | Canonicalised and de-duplicated; uses the existing checkout, no clone or OAuth (Conductor complaint). | P0 |
| Add folder by typed path with autocomplete (palette browse mode) | web/components/CommandPalette.logic.ts | Missing | | Path completion on the host, so it works for remote hosts too. | P2 |
| Relative path resolved against the active project | web/components/CommandPalette.tsx | Missing | | | P2 |
| New project: create a named repo (`git init --initial-branch`) under a base folder | srv/project/ManagedProjectFolders.ts:393-510 | Missing | | Deterministic action, no agent in the loop (Cursor `/worktree` complaint). | P2 |
| New project with a private repo created on the host and cloned ("Create & Clone") | srv/project/ManagedProjectFolders.ts | Missing | | Through the user's own `gh`/`glab`, least privilege. | P2 |
| Clone from URL, `owner/name` shorthand or provider picker, with destination confirm | web/components/CommandPaletteAddProject.tsx, srv/vcs/CheckoutCloner.ts | Missing | | SSH with HTTPS fallback; ghq layout optional. | P2 |
| Clone progress toast with Cancel / Retry / Remove project, stall detection after 20 s | web/components/ProjectCloneToastCoordinator.tsx, srv/project/ProjectCloneTracker.ts:71-75 | Missing | | Progress from git's own output; commands on a cloning project are queued, not rejected. | P2 |
| Open WSL folder | web/components/CommandPalette.tsx | Partial | orbitd/src/main.rs:1, app/main.rs:33, app/settings.rs:134 | Today: run `orbitd` in WSL by hand and set ORBIT_HOST. Target: one-click "Open in WSL" that starts the host in the distro (#192, 145 reactions; #14393 stuck "Connecting to WSL"). | P1 |
| Unarchive a project from the palette | web/components/CommandPalette.tsx | Missing | | | P2 |
| Add project to another machine (clone there, copy up to 200 ignored files) | web/components/AddProjectToMachineDialog.tsx, web/lib/addProjectToMachine.ts:25-28 | Missing | | | P2 |
| Drop a folder on the sidebar to add it | web/components/sidebar/SidebarFolderDrop.tsx | Missing | | | P1 |
| Dropped files on the sidebar rejected with guidance | web/components/sidebar/SidebarFolderDrop.tsx | Missing | | Offer to attach to the hovered thread instead of only refusing. | P2 |
| Welcome-page import: scan Claude Code / Codex / Pi history for active repos | web/components/onboarding/WelcomePage.tsx, srv/project/AgentSessionScanner.ts:85-92 | Missing | | Import is the top migration path (#330, #207 import existing sessions). | P1 |
| Import review: pre-checked candidates, Select all / none, clones grouped, 200 messages per conversation | srv/project/AgentSessionImporter.ts, doc/welcome-page.md | Missing | | Keep full history by reference to the provider's own files instead of truncating to 200 messages. | P2 |
| Reopen the welcome page from Settings / About | web/components/onboarding/FirstRunGate.tsx | Missing | | | P2 |
| Native fork of imported Claude sessions (history worker) | srv/claudeHistoryWorker.ts | Missing | | | P2 |
| Import T3 Code's own threads (statev2.sqlite) | doc/thread-migration.md | Missing | | Lets T3 users switch without losing history. | P2 |

### Projects: settings, icons, pin, archive, removal

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Project settings panel (name with reset to default) | web/components/settings/ProjectSettingsPanel.tsx | Partial | proto/wire.rs:129 (`UpdateProject`), core/engine/orchestrator.rs:548 | Engine command exists; no UI. | P1 |
| Remove a project (entries and threads; files never touched) | srv/project/ProjectService.ts:777 | Partial | core/engine/orchestrator.rs:555 | Engine deletes every thread with no UI and no confirmation; T3 refuses without `force`. Target: confirm with the thread count, offer archive instead. | P1 |
| Remove one checkout / remove the project on every machine | web/components/settings/ProjectSettingsPanel.tsx | Missing | | | P2 |
| Settings scope sentence ("Applying settings for {project} on {env}", Mixed, Reset all) | web/components/settings/SettingsScopeSentence.tsx, SettingInheritance.tsx | Missing | | | P2 |
| Project icon override (Lucide, emoji, monogram, custom image) | web/components/settings/ProjectIconPickerDialog.tsx | Missing | | | P2 |
| Project icon auto-detection (favicon candidates, `<link rel=icon>`) | srv/project/ProjectFaviconResolver.ts:41-80 | Missing | | Detected once on add and on file-watch change, not per render. | P2 |
| Two-letter monogram fallback with palette colour | web/components/ProjectMonogram.tsx | Missing | | | P2 |
| Pin a project (sorts first in pickers and sidebar) | web/components/ProjectPinButton.tsx | Missing | | | P2 |
| Manual project order | client settings `sidebarProjectSortOrder=manual` | Partial | proto/model.rs:14 (`order`) | Field exists, no reorder UI. Manual order broke in T3 (#27062); keep it in the host record. | P2 |
| Archive a project (hidden from pickers, data kept) | ct/project.ts (`archivedAt`) | Missing | | | P2 |
| Multi-repo projects (repos one or two levels down) | doc/multi-repo-projects.md, web/components/settings/ProjectRepositoriesSettings.tsx | Missing | | #1453 (33 reactions); one review across repos. | P2 |
| Track extra folders anywhere (up to 128), suggestions from `.code-workspace` | web/components/settings/trackedFolders.logic.ts | Missing | | | P2 |
| Repository list states (Git repository / Folder / Missing) | web/components/settings/ProjectRepositoriesSettings.tsx | Missing | | | P2 |
| Auto-pull default branch when clean and without local commits | ct/settings.ts (`defaultAutoPull`) | Missing | | Triggered on fetch result, not a poll. | P2 |

### Project scripts ("Actions")

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Script model (name, command, icon) | ct/project.ts:26-48 | Partial | proto/model.rs:28 (`ProjectScripts { setup, teardown, run }`) | Model only; no icon, no UI. | P1 |
| Run a script automatically on worktree creation | ct/project.ts (`runOnWorktreeCreate`) | Partial | core/git/worktree.rs:175 | Setup script runs inside `create_worktree`, which the engine never calls. | P1 |
| "Wait for it to finish before the agent starts" | web/components/projectScriptEditor.tsx | Missing | | The held-prompt mechanism (core/engine/orchestrator.rs:933) can gate on setup too. | P1 |
| Run on settle / stop on settle | ct/project.ts (`runOnSettle`, `stopOnSettle`) | Missing | | | P2 |
| Preview URL per script, open preview automatically | ct/project.ts (`previewUrl`, `autoOpenPreview`) | Missing | | Use the worktree's own port block so URLs never collide. | P2 |
| Import scripts from the checked-in project file | web/components/settings/ProjectActionsSettings.tsx | Missing | | | P2 |
| Environment-level default script list with per-project override and reset | web/components/settings/useProjectScriptSettings.ts | Missing | | | P2 |
| Top-bar "Run {name}" button with dropdown, add, edit, delete | web/components/ProjectScriptsControl.tsx | Missing | | | P1 |
| Script editor dialog (name, icon, command with Test, keybinding, validation messages) | web/components/projectScriptEditor.tsx | Missing | | | P1 |
| Per-script keybinding (`script.<id>.run`) | sh/keybindings.ts | Missing | | | P2 |
| Setup runner in a terminal with completion sentinel and 400-char line cap | srv/project/ProjectSetupScriptRunner.ts:138-183 | Partial | core/git/worktree.rs:11 (line-by-line output, 8 KiB tail) | Runs outside a terminal; target: run in the thread's terminal tab with exit code, no sentinel parsing. | P1 |
| Scripts refused for a worktree an agent requested below Full access | srv/project/WorktreeSetupTracker.ts | Missing | | | P2 |
| Teardown script on worktree removal | ct/t3ProjectFile.ts (`worktreeLayout.remove`) | Partial | core/git/worktree.rs:265 | Library only. | P2 |

### Threads: creation and drafts

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| New thread (`mod+n`) | web/components/Sidebar.logic.ts:816-852, sh/keybindings.ts:113-116 | Done | app/actions.rs:28, app/shell.rs:145, app/home.rs:82 | No picker step: one key, then type; picker is inline in the header. T3 view transitions lagged seconds (#440). | P0 |
| Project picker when several projects are open | web/components/Sidebar.logic.ts:816 | Done | app/home.rs:135 | Defaults to the most recently used project (app/home.rs:68). | P0 |
| New thread in the current project (`chat.newLocal`) | sh/keybindings.ts | Partial | app/sidebar.rs:281 (project row button), app/palette.rs:91 | Reachable by mouse and palette, no key. | P1 |
| New thread without a project (scratch) | sh/keybindings.ts (`chat.newWithoutProject`) | Done | app/home.rs:151, core/engine/orchestrator.rs:871 | | P1 |
| New thread inherits the last used model, options, mode and access | web/components/Sidebar.logic.ts | Done | core/engine/orchestrator.rs:669, app/home.rs:45 | Fixes #6508 by design: defaults are host state. | P0 |
| Draft promoted to a real thread without losing the view | web/routes/_chat.draft.$draftId.tsx | Partial | app/home.rs:82-117 | Text is restored on failure, attachments are not. T3's #27443 (69 comments, empty thread view) cannot happen: the thread is created before the first send. | P1 |
| Drafts persisted per thread (text, attachments, model, context) across restarts | web/composerDraftStore.ts:119 | Missing | app/shell.rs:33 (only 6 live views keep composer state) | Persist on the host atomically; switching through 7 threads loses a draft today. Losing a prompt is a top complaint (Crystal #183). | P0 |
| "Unsent draft" rows in the sidebar | web/components/sidebar/SidebarDraftBlock.tsx | Missing | | | P2 |
| Discard draft with Undo (restores text and attachments) | web/components/sidebar/SidebarThreadUndoNotice.tsx | Missing | | | P2 |
| Draft menu: copy path / branch, project settings, discard | web/components/threadActionMenu.logic.ts:35-93 | Missing | | | P2 |
| Home opens a draft in the most recent project | web/routes/_chat.index.tsx | Done | app/home.rs:68 | | P1 |
| Background send (`mod+enter` on a draft starts it and opens a fresh draft) | sh/keybindings.ts (`composer.sendBackground`) | Missing | | Keeps workspace mode and base branch for the next draft. | P1 |
| Multi-model fan-out (one thread and one worktree per selected model) | web/components/chat/useMultipleModelSend.ts | Missing | | Add a side-by-side diff comparison of the results (best-of-N request). | P2 |
| Thread and first run created atomically | ct/orchestrationV2.ts:3312-3357 | Partial | app/home.rs:90-111 | Two calls (`CreateThread`, then `Send`); a failed send leaves an empty thread. Target: `CreateThread { initial_message }`. | P1 |
| Scratch threads get their own folder | srv/project/ManagedProjectFolders.ts:135-160 | Done | core/engine/orchestrator.rs:878 | | P1 |
| Scratch threads hide branch, diff and worktree controls | web/components/threadActionMenu.logic.ts | Partial | core/engine/orchestrator.rs:175 | Review still offered and fails with a message. | P2 |
| Scratch folder kept when the thread is deleted | srv/project/ManagedProjectFolders.ts | Partial | core/engine/orchestrator.rs:729 | Kept only because nothing deletes it; no way to find or clean it. | P2 |
| "Turn into project" (promote scratch folder in place, moves sibling threads) | srv/project/ScratchThreadPromotion.ts | Missing | | | P2 |
| Prompt links `/new?prompt=&repo=&provider=&model=&pr=` (never auto-send) | sh/promptLinks.ts, doc/prompt-links.md | Missing | | `orbit://new?...` deep link through the single-instance handler. | P2 |
| Prompt-link sanitising (8,000 chars, invisible and tag characters stripped with warning) | sh/promptLinks.ts | Missing | | | P2 |
| Thread link with `?prompt=` fills that composer, asks before replacing a draft | web/components/promptLinks/* | Missing | | | P2 |
| Copy prompt link (palette and per message, optionally with repo and model) | web/components/CommandPalette.tsx | Missing | | | P2 |
| Hosted-app "where to open" chooser for prompt links | web/components/promptLinks/* | N/A | | Only exists for T3's hosted web app. | P2 |
| Copy thread reference (`mod+shift+c`, copies the PR URL when one is open) | web/components/Sidebar.tsx:2910-2914 | Partial | app/thread_menu.rs:165 (Copy thread ID) | | P2 |
| Pasted thread link becomes a thread chip | web/components/composerInlineTokenPaste.ts | Missing | | | P2 |

### Threads: titles, fork, side and linked threads

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Seed title from the first prompt | client-runtime/operations/threadTitle.ts | Done | core/engine/orchestrator.rs:832, :1504 | | P1 |
| Title fallbacks for attachment-only messages ("Attached image", "File: {name}") | client-runtime/operations/threadTitle.ts | Partial | app/thread/mod.rs:360 | Falls back to "New thread". | P2 |
| Generated title | srv/textGeneration/* | Done | core/engine/textgen.rs, core/provider/oneshot.rs | After the first completed turn, one cheap call (Claude Haiku or Codex at low effort) on the thread's own account; never replaces a title the user or the agent set; off in `agents.generate_titles`. | P1 |
| Regenerate title (single, bulk) | ct/orchestrationV2.ts (`thread.metadata.update`) | Missing | | | P2 |
| Rename thread (dialog, empty rejected) | web/components/threadActionMenu.logic.ts | Done | app/thread_menu.rs:54 | | P1 |
| Inline rename by double-click or F2 | doc/keybindings.md | Missing | | | P2 |
| Thread-title instructions per environment or project | Settings → Threads → Text generation | Missing | | | P2 |
| Title links resolve issue and PR refs | srv/textGeneration/ThreadTitleLinks.ts | Missing | | | P2 |
| Fork from this response | web/components/chat/TimelineAssistantRows.tsx:126-170 | Partial | core/engine/orchestrator.rs:752 | Engine resumes the provider's own history at the fork point; no button. #1404 (34 reactions). | P1 |
| Fork strategies: native fork or portable context handoff | ct/orchestrationV2.ts:226-250 | Partial | core/provider/claude.rs:313-315, core/provider/codex/session.rs:891 | Native only. | P2 |
| Merge a fork back into its parent | ct/orchestrationV2.ts:3759-3776 | Missing | | | P2 |
| Fork while the source is still running | ct/orchestrationV2.ts (`canForkWhileSourceRunning`) | Missing | | | P2 |
| Side chats (`/side`, `/btw`): hidden thread in a side panel with its own composer | web/components/chat/useSideChat.ts, doc/thread-sidebar.md | Missing | | Inherit the parent's access level (T3 #28110, #29966 ignored it). | P1 |
| Link a side chat into the parent ("Add to chat", @-mention) | web/components/chat/useSideChat.ts | Missing | | | P2 |
| Unused side chat deleted on close; parent archive or delete cascades | doc/thread-sidebar.md | Missing | | | P2 |
| Open subagent, side and linked threads in a side panel or main view (`sideThreadOpenTarget`) | web/routes/_chat.linked.*.tsx | Missing | | | P2 |
| Provider switch mid-thread with a budgeted portable handoff | doc/portable-handoffs.md | Missing | | Changing the provider in the composer today sends `Configure` to the old provider's live session (core/engine/orchestrator.rs:720); needs a real handoff. | P1 |

### Threads: lifecycle (pin, settle, snooze, archive, delete, unread)

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Pin and unpin a thread | ct/orchestrationV2.ts (`thread.pin`) | Done | app/thread_menu.rs:121, app/sidebar.rs:63 | | P1 |
| Pinned threads in manual order | `thread.pin.reorder` | Missing | | | P2 |
| Pin shortcut (`mod+shift+p`) | sh/keybindings.ts | Missing | | Orbit binds Ctrl+Shift+P to the palette (app/actions.rs:30); pick a free chord. | P2 |
| Settle (soft done) | ct/orchestrationV2.ts:3387-3427 | Done | app/thread_menu.rs:128, core/engine/orchestrator.rs:699 | Settling also unpins and unsnoozes, as in T3. | P1 |
| Settle shortcut (`mod+shift+s`) | sh/keybindings.ts | Missing | | | P2 |
| Auto-settle after N inactive days (default 7, 1–90) | ct/settings.ts:89-98 | Missing | | | P2 |
| Auto-settle on PR merge, optionally on PR close | ct/settings.ts | Missing | | | P2 |
| Auto-settle blocked by running work, pending questions, background waits | ct/orchestrationV2.ts:4268-4320 | Missing | | | P2 |
| Per-thread auto-settle behaviour (Follow settings / Never) | `thread.auto-settle.set` | Missing | | | P2 |
| Settled threads stay searchable | doc/thread-sidebar.md | Done | core/engine/orchestrator.rs:1430 | | P2 |
| Any new message un-settles | doc/thread-sidebar.md | Done | core/engine/orchestrator.rs:838 | | P1 |
| Settling closes idle terminals and runs on-settle scripts | doc/thread-sidebar.md | Missing | | | P2 |
| Offline settle queued as "Pending / Waiting to sync" | web/components/Sidebar.logic.ts | Missing | | | P2 |
| Older settled threads paged (50 per page, retry) | web/components/Sidebar.tsx | Done | app/sidebar.rs:69, :505 | Thread records are tiny and the list is virtualised, so no paging is needed; T3 froze for a minute at 13k threads (#25823). | P2 |
| Snooze presets (1 h, 3 h, this evening, tomorrow 09:00, Monday 09:00) | web/components/Sidebar.snooze.ts | Partial | app/thread_menu.rs:24, core/engine/orchestrator.rs:707-719 | Wake timer is an in-memory sleep, not re-armed after restart: a snoozed thread never wakes by itself after Orbit restarts. | P1 |
| Past "today" presets hidden; stale pick reported | web/components/Sidebar.snooze.ts | Partial | app/thread_menu.rs:36, core/engine/orchestrator.rs:708 | A past time is silently ignored instead of reported. | P2 |
| Editable snooze presets (Settings → Threads → Snooze options) | web/components/settings/SnoozePresetsSettings.tsx | Missing | | | P2 |
| Custom snooze (date/time or duration dialog) | web/components/CustomSnoozeDialog.tsx | Missing | | | P1 |
| Snoozed thread keeps running, hidden in the Snoozed shelf | web/components/Sidebar.snooze.ts | Done | app/sidebar.rs:58 | | P1 |
| Early wake on approval, question, failure or user message | doc/thread-sidebar.md | Done | core/engine/orchestrator.rs:1336, :1173, :838 | | P1 |
| "Woke" badge, cleared when seen | web/components/ThreadStatusIndicators.tsx | Done | app/sidebar.rs:325, core/engine/orchestrator.rs:695 | | P2 |
| Wake now | thread action menu | Done | app/thread_menu.rs:134 | | P2 |
| Snooze until the usage limit resets | doc/thread-sidebar.md | Done | app/thread_menu.rs:150 | | P2 |
| Archive and unarchive | `thread.archive` | Done | app/thread_menu.rs:180, app/sidebar.rs:151 | | P1 |
| Archived threads refuse new messages (from users, agents and schedules) | doc/thread-sidebar.md | Missing | core/engine/orchestrator.rs:811 | `Send` does not check `archived`; same class as T3 #41220 (messages accepted on archived threads start live turns). | P1 |
| Optional archive confirmation | ct/settings.ts (`confirmThreadArchive`) | Missing | | Undo toast instead of a confirm. | P2 |
| Archived threads list (Unarchive / Delete) | web/components/settings/ArchivedThreadsPanel.tsx | Partial | app/palette.rs:38 | Archived threads are reachable only through the palette. | P2 |
| Delete with confirmation; history really deleted | web/hooks/useThreadActions.ts:538-640 | Done | app/thread_menu.rs:86, core/engine/orchestrator.rs:729, core/store.rs:452 | Log, unreferenced blobs and checkpoint refs go; T3 V2 kept every event (#17845, 20 comments). | P1 |
| Delete the thread's worktree too when nothing else uses it | web/hooks/useThreadActions.ts:595-609 | Missing | | One dialog with a checkbox; Cancel cancels everything (T3's second dialog's Cancel did not). | P1 |
| After delete, go to the newest active thread in the project | web/components/Sidebar.logic.ts:1336-1380 | Partial | app/shell.rs:65-72 | Goes to Home. | P2 |
| Deleting a thread deletes its side chats and the schedules that post into it | web/hooks/useThreadActions.ts | Missing | | | P2 |
| Undo toast for unpin, settle, snooze, archive, discard (5 s) and `mod+z` | web/components/sidebar/SidebarThreadUndoNotice.tsx | Missing | | | P1 |
| Unread / read watermark | web/uiStateStore.ts | Done | core/engine/orchestrator.rs:796, :1179, app/app_state.rs:141 | | P0 |
| Mark unread (only after a finished run, while idle) | `thread.mark-unread` | Done | app/thread_menu.rs:159 | | P2 |
| "Done" dot for finished and unseen | web/components/Sidebar.logic.ts:769-792 | Done | app/ui.rs:91 | | P0 |
| Thread tags and "hide tagged threads" (also mutes their notifications) | ct/threadTags.ts | Missing | | Give tags a UI; T3's are server-only and unreachable. | P2 |
| PR for the thread's branch linked automatically; Link / Unlink | web/components/pullRequest/linkedThreads.ts | Missing | | | P1 |
| Linked issues badge | web/components/ThreadStatusIndicators.tsx | Missing | | | P2 |

### Worktrees per thread

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Workspace selector: current checkout / new worktree / previous worktree | web/components/BranchToolbarEnvModeSelector.tsx | Partial | core/git/worktree.rs:175, core/engine/orchestrator.rs:655 | Library is complete (ports, `.worktreeinclude`, setup), but `CreateThread { worktree: true }` returns "Worktrees are not available in this build" and Home always sends `false` (app/home.rs:90). | P0 |
| Workspace / branch / previous-worktree shortcuts (`mod+shift+x/g/l`) | sh/keybindings.ts | Missing | | | P2 |
| "New thread in this worktree" / "New thread on {branch}" | web/components/BranchToolbar.tsx | Missing | | | P1 |
| Searchable base-branch picker with lazy ref loading | web/components/WorktreeBaseBranchPicker.tsx | Partial | core/engine/orchestrator.rs:280, core/git/status.rs:127 | `Branches` command exists; no UI. | P1 |
| Multi-repo worktree picker (checklist, remembered per project) | web/components/WorktreeRepositoriesControl.tsx | Missing | | | P2 |
| Approval of checked-in worktree commands (exact text, revoke, 10 min timeout) | web/components/WorktreeLayoutApprovalDialogHost.tsx, srv/workspace/LayoutCommandApprovals.ts | Missing | | | P2 |
| Branch naming: static prefix / model-chosen / custom instructions | web/components/settings/BranchNamingSettings.tsx | Missing | | No hard-coded prefix (#272). | P2 |
| Missing worktree folder recreated on the next turn, setup re-run | srv/workspace/restoreMissingWorktree.ts | Missing | | Covers #19124 (thread cannot start after merge removed its worktree) and #24154. | P1 |
| Cleanup policies (inactive days, after settle, after merge, unchanged and settled, with thread) | srv/storageCleanup.ts | Partial | core/git/worktree.rs:265 | Removal deletes the branch the worktree has now (#17813); no policies yet. Users reclaimed 500 GB from T3 worktrees. | P1 |
| Worktree location setting | web/components/settings/WorktreesSettings.tsx | Partial | core/git/mod.rs:178 | Configurable in code, no UI (Codex #10599). | P2 |
| Submodules mode and sparse patterns | ct/t3ProjectFile.ts | Missing | | | P2 |
| Fast APFS checkout (macOS, Labs) | ct/settings.ts (`fastWorktreeCheckout`) | Missing | | | P2 |

### Sidebar

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Sections: Pinned, Active, Working, Snoozed shelf, Settled shelf | web/components/Sidebar.tsx:3685-3810 | Done | app/sidebar.rs:33-73, :193 | Adds "Needs you" and "Recent"; a row that changes section slides in so the eye can follow (app/sidebar.rs:350). | P0 |
| Shelf open / closed state persisted | web/components/Sidebar.logic.ts:201-213 | Partial | app/sidebar.rs:97-98 | In memory only. | P2 |
| Empty states ("No projects yet / Add project") | web/components/Sidebar.tsx | Done | app/sidebar.rs:480 | | P1 |
| Server-stored manual order for active and pinned threads | `thread.active.reorder` | Missing | | | P2 |
| New threads land on top; activity does not reorder | client-runtime/state/thread-sort | Partial | app/sidebar.rs:152 | Orbit sorts by last activity; moves are animated but still reorder under the pointer. | P1 |
| Project and thread sort-order settings | web/components/settings/SettingsPanels.tsx:147,935 | Missing | | | P2 |
| Drag to reorder within a section | web/components/Sidebar.drag.ts | Missing | | | P2 |
| Drag across sections applies pin / unpin / settle / wake with a verb badge | web/components/Sidebar.logic.ts:256-565 | Missing | | | P2 |
| Drag-sweep a row action over a range of rows | web/components/Sidebar.pointer.ts | Missing | | | P2 |
| Click guard while rows move under the pointer | web/components/Sidebar.shiftGuard.ts | Missing | | Needed now that rows animate between sections. | P2 |
| OS files dropped on a row attach to that thread's composer | web/components/Sidebar.drag.ts | Missing | | | P2 |
| Group by project (boring sidebar) | web/components/sidebar/SidebarProjectFolderHeader.tsx | Done | app/sidebar.rs:172, :460 | | P2 |
| Closed project folder shows count and urgency dot | web/components/Sidebar.logic.ts:1032-1100 | Partial | app/sidebar.rs:279 | Count only. | P2 |
| New thread from a project folder header | web/components/sidebar/SidebarProjectFolderHeader.tsx | Done | app/sidebar.rs:281 | | P2 |
| Multiline rows option | ct/settings.ts (`boringSidebarMultilineRows`) | Missing | | | P2 |
| Project filter (multi-select, search, persisted) | web/components/Sidebar.tsx:1032,3140-3300 | Missing | | | P1 |
| "Filter by {project}" from a thread's menu | web/components/threadActionMenu.logic.ts | Missing | | | P2 |
| Sidebar search over titles, branches, PRs, projects (fuzzy) | web/components/Sidebar.logic.ts:1142-1185 | Partial | app/palette.rs:85 | Palette matches titles and project names only. | P1 |
| Message search ("In messages" group, field hint, highlighted ranges) | web/components/ThreadSearchMatch.tsx, `searchThreads` | Partial | core/engine/orchestrator.rs:1430, proto/wire.rs:168 | Engine full-text search with excerpts runs off the engine thread; nothing in the UI calls it. | P0 |
| Palette ranks recent and frequent threads and remembers the last pick per query | web/components/CommandPalette.logic.ts | Partial | app/palette.rs:39 | Sorted by recency only. | P2 |
| Project content search dialog | web/components/search/ProjectContentSearchDialog.tsx | Missing | | | P1 |
| Row status states (approval, question, guest request, setting up, working, waiting, failed, limited, ready) | web/components/Sidebar.logic.ts:919-1030 | Partial | app/ui.rs:19-53 | Six states; approval and question share "Needs you"; no "Setting up". Waiting never looks like working (Emdash #531). | P0 |
| A failed row returns to ready once opened | web/components/Sidebar.logic.ts | Missing | | Failed stays until the next turn (compare T3 #40483, failed after limit reset). | P1 |
| Seen and idle rows recede | web/components/sidebar/SidebarThreadRow.tsx | Partial | app/sidebar.rs:319-320 | Settled rows dim, unread rows are bold. | P2 |
| Trailing PR badge, terminal-running mark, linked issues | web/components/sidebar/SidebarThreadRow.tsx:821-872 | Missing | | | P1 |
| Status written into the window title for screen readers | ct/settings.ts (`screenReaderWindowTitle`) | Missing | | | P2 |
| Hover card (PRs, project, branch mismatch, model, worktree path, last error) | web/components/ThreadHoverCard.tsx | Missing | | | P1 |
| Thread action menu (rename, pin, settle, snooze, wake, mark unread, copy, archive, delete…) | web/components/threadActionMenu.logic.ts | Partial | app/thread_menu.rs:105 | One definition for sidebar and header. Missing: find in thread, new thread on branch, regenerate title, custom snooze, filter by project, turn into project, auto-settle, project settings. "Copy path" is hidden when there is no path (T3 showed it and failed). | P1 |
| Read-only connections disable mutating items (triage allowed) | web/components/threadActionMenu.logic.ts | Missing | | | P2 |
| Multi-select (Ctrl-click toggle, Shift-click range) | web/threadSelectionStore.ts | Missing | | Re-anchor when the anchor is deselected (T3 bug at threadSelectionStore.ts:42). | P1 |
| Bulk actions (settle, snooze, mark unread, regenerate titles, unpin, delete with count confirm) | web/components/Sidebar.tsx:2395-2460 | Missing | | | P1 |
| Toggle sidebar (`mod+b`) | sh/keybindings.ts:59 | Done | app/actions.rs:33, app/shell.rs:167 | | P1 |
| Previous / next thread | sh/keybindings.ts (`thread.previous/next`) | Partial | app/actions.rs:38-39, app/shell.rs:226 | Bound to Ctrl+Tab; walks project order, not the sidebar's section order, so "next" jumps unpredictably. | P1 |
| Jump to visible thread `mod+1..9` with hold hints | sh/keybindings.ts:129-139 | Missing | | | P2 |
| Resizable width, persisted, clamped, reset | web/components/threadSidebarWidth.ts | Partial | app/shell.rs:396-397 | Resizable 208–440 px, not persisted. | P2 |
| Sidebar chrome (back / forward, home, settings, PRs, issues, usage) | web/components/sidebar/SidebarChrome.tsx | Partial | app/sidebar.rs:424-527 | New thread, search, Inbox, Fleet, Settings. | P1 |
| Reopen closed view (`mod+shift+t`, stack of 20) | web/closedViewStore.ts:77,92 | Missing | | | P2 |

### Outside agents (MCP and CLI)

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| MCP server for other agents, with approval levels and revocable sign-ins | doc/outside-agents.md, srv/mcp/ | Partial | cli/src/mcp.rs:1, net/endpoint.rs:1 | `orbit mcp` over stdio; loopback endpoint with a per-run token in endpoint.json, never exported to agents. Agents act only on their own helpers; approvals need full access. No revocable sign-ins yet. | P2 |
| Thread tools (launch, wait, read, list, search, send) | srv/mcp/ | Done | cli/src/spec.rs:1 | Generated from the CLI's command specs; children link to the calling thread (ORBIT_THREAD) and are capped at its access. | P2 |
| CLI `thread start / send / show / list` with `--wait`, `--json`, exit codes | srv/cli/ | Done | cli/src/exec.rs:1, docs/CLI.md | `orbit new / send / wait / log / threads`, completions, `help --json`; exit codes documented. | P2 |
| Milestone event feed (last 1,000) and webhook subscriptions | doc/outside-agents.md | Missing | | | P2 |

## B. Chat timeline, composer, models and providers, modes, approvals

### Chat timeline: rendering and navigation

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Virtualised message list | web/components/chat/MessagesTimeline.tsx | Done | app/thread/mod.rs:376 | Opening a 3,000-item thread takes 2.5 ms (docs/BENCHMARKS.md); T3 long threads load slowly (#26018) and replay quadratically (#4596). | P0 |
| Load earlier turns (history paging, retry) | web/components/chat/MessagesTimeline.tsx | Partial | core/store.rs:341 | The whole thread log loads on open. Fine for thousands of items; add paging from disk past a size cap (T3 #20427: 2.4 s to reach the top of 13k items). | P1 |
| Follow output at the bottom, "Scroll to end" pill when scrolled up | web/components/chat/useTimelineFollow.ts, TimelineOverlays.tsx | Partial | app/thread/mod.rs:239, :376 | Tail-follow from the message scroller; no verified pill. Scroll anchoring is T3's most regressed area (#20366, #12372, #30515). | P0 |
| Scroll position restored per visit | web/components/chat/useTimelinePositionRestore.ts | Partial | app/shell.rs:33 | Kept while one of 6 live views; lost after eviction. | P1 |
| Expanding a disclosure keeps the scroll anchored | web/components/chat/timelineScrollAnchoring.ts | Partial | app/thread/mod.rs:244 | Row is remeasured; anchoring not verified. | P1 |
| Timeline minimap (user-message ticks, previous / next turn) | web/components/chat/TimelineMinimap.tsx | Missing | | | P2 |
| Find in thread (`mod+f`, next / previous, highlights) | web/components/chat/ThreadFindBar.tsx | Missing | | Search the in-memory timeline locally; no server round trip for an open thread. | P1 |
| Visit watermark (where new activity starts) | web/components/chat/ThreadVisitWatermark.tsx | Missing | | | P2 |
| Message author label (You, Agent, Automation, guest) | web/components/chat/TimelineUserMessageRow.tsx | Partial | app/thread/rows.rs:113 | All user rows look like "you". | P2 |
| Long messages collapse ("Show full message") | web/components/chat/TimelineUserMessageRow.tsx | Missing | | | P2 |
| Attachment preview and download in a sent message | web/components/chat/TimelineUserMessageRow.tsx | Partial | app/thread/rows.rs:137 | File names only. | P1 |
| Copy a sent prompt | web/components/chat/TimelineUserMessageRow.tsx | Done | app/thread/rows.rs:160 | | P1 |
| Edit from here (rewind and resend) | web/components/chat/useTimelineRewind.ts | Partial | app/thread/rows.rs:167, :616, core/engine/orchestrator.rs:996 | Rewinds the provider's own history at the cursor. Edits in a dialog, drops attachments, and offers no "revert files too". | P0 |
| Steer badges ("waiting to be read", "Steered the active turn", "promoted from queue") | web/components/chat/TimelineUserMessageRow.tsx | Partial | app/thread/rows.rs:134, :158 | Single "Steered" label and ring. | P2 |
| "Sent by T3 Code" marker on system-sent messages | web/components/chat/TimelineUserMessageRow.tsx | Missing | | | P2 |
| Inline context chips in sent messages (terminal excerpt, review comment, preview crop, PR, thread, file) | web/components/chat/UserMessageContextChips.tsx | Missing | | | P1 |
| Assistant Markdown body | web/components/ChatMarkdown.tsx | Done | app/thread/rows.rs:189, app/thread/mod.rs:166 | Streaming re-parses only the last block on a background task. | P0 |
| Assistant hover bar (status badge, copy, fork, reply, timestamp) | web/components/chat/TimelineAssistantRows.tsx | Partial | app/thread/rows.rs:195 | Copy only. | P1 |
| Day-aware timestamps (locale, 12 h / 24 h, full-time tooltip) | ct/settings.ts:67 | Missing | | | P2 |
| "(empty response)" label | web/components/chat/TimelineAssistantRows.tsx | Partial | app/thread/rows.rs:52 | Empty replies are hidden, not labelled. | P2 |
| "Cut off by a steer" label | web/components/chat/TimelineAssistantRows.tsx | Missing | | | P2 |
| Cite a selection into the composer (quote chip with comment, links back) | web/components/chat/AssistantSelectionToolbar.tsx | Missing | | | P2 |
| Code blocks: language label, copy, wrap toggle | web/components/MarkdownCodeBlock.tsx | Partial | app/thread/rows.rs:189 | Highlighted by the Markdown view (tree-sitter); copy and wrap controls not verified. | P1 |
| Run a code block in the terminal | web/components/MarkdownCodeBlock.tsx | Missing | | | P2 |
| Math (KaTeX) | web/components/ChatMarkdown.tsx | Missing | | #1784. | P2 |
| Mermaid diagrams | web/components/ChatMarkdown.tsx | Missing | | | P2 |
| Tables, details, toggling task lists, heading anchors | web/components/ChatMarkdown.tsx | Partial | app/thread/rows.rs:189 | Tables render; task lists are read-only. | P2 |
| File links open in an in-app viewer (read-only outside the workspace, "not found on {machine}") | web/components/ChatMarkdown.tsx | Missing | | | P1 |
| Thread links as chips; context menu on external links | web/components/MarkdownThreadLink.tsx | Missing | | | P2 |
| PR links can be linked or unlinked from the thread | web/components/ChatMarkdown.tsx | Missing | | | P2 |
| Inline images, audio, media gallery | web/components/chat/markdownImageGallery.ts | Missing | | | P2 |
| Codex artifact templates ("Use template") | web/components/ChatMarkdown.tsx | Missing | | | P2 |
| Skill names rendered specially | web/components/SkillInlineText.tsx | Missing | | | P2 |
| Per-block RTL detection | web/components/ChatMarkdown.tsx | Missing | | | P2 |
| Image and video viewer (zoom, previous / next, save, copy path) | web/components/chat/ExpandedImageDialog.tsx | Missing | | | P2 |
| Live elapsed counter while the agent works | web/components/AgentElapsed.tsx | Partial | app/thread/rows.rs:356 | Duration appears only when the turn ends. | P1 |

### Chat timeline: work log, tools, reasoning, subagents

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Consecutive tool calls grouped with summary, per-tool icon, failure count | web/components/chat/TimelineWorkGroupRows.tsx | Done | app/thread/rows.rs:247-287, :394 | | P0 |
| Tool card appears while its input streams | (T3 ignores `input_json_delta`, §5.12) | Done | core/provider/claude/parser.rs:9-11, core/engine/orchestrator.rs:1233 | Long Write/Edit inputs are visible as they stream; T3 shows them only when done. | P1 |
| Images the agent viewed stay visible in a collapsed group | row kind `viewed-images` | Missing | | | P2 |
| Live labels ("Working…", "Compacting…", "Setting up worktree…") | web/components/chat/WorkLog.tsx | Partial | app/thread/rows.rs:256 | "Working · n steps" only. | P1 |
| Tool-call visibility setting (show all, hide successful, hide all, zen) | ct/settings.ts:363 | Missing | | | P2 |
| Reasoning row ("Thinking" live, "Thought" when done) | web/components/chat/MessagesTimeline.logic.ts | Done | app/thread/rows.rs:200-246 | | P1 |
| Subagent count replaces "Thinking" while waiting on children | web/components/chat/MessagesTimeline.logic.ts:572 | Missing | | | P2 |
| Finished turn's work folds away | web/components/chat/MessagesTimeline.logic.ts:598-615 | Done | app/thread/rows.rs:251-254 | Only the work log folds; the answer is never folded (T3 #8879, #41320 hid the answer). | P1 |
| Superseded retry attempts fold | row kind `attempt-fold` | Missing | | | P2 |
| Item inspector: tool input and output | web/components/chat/V2ItemInspector.tsx | Partial | app/thread/rows.rs:470 | Output expands inline; input JSON is not shown. | P1 |
| Full command output fetched on demand ("Earlier output not shown") | web/components/chat/CommandOutputPanel.tsx | Partial | core/engine/orchestrator.rs:1286-1296 | Output past 64 KiB is stored as a blob but cannot be opened. | P1 |
| Search results and tool-output images in the inspector | web/components/chat/V2ItemInspector.tsx | Missing | | | P2 |
| "Open diff" from a file-change item | web/components/chat/V2ItemInspector.tsx | Missing | | | P1 |
| Command execution: command, streamed output, exit code, failure | web/components/chat/ShellCommandBlock.tsx | Done | app/thread/rows.rs:408-488, core/engine/orchestrator.rs:1277 | | P0 |
| Background command monitor that wakes the agent on completion | ct/orchestrationV2.ts (`completionWake`) | Partial | core/engine/orchestrator.rs:1222 | Agent-started turns are tracked; no monitor UI. | P2 |
| File change rows with +/− stats | web/components/DiffStatLabel.tsx | Done | app/thread/rows.rs:461 | | P0 |
| Per-turn file summary that opens the diff | turn `checkpoint` items | Partial | app/thread/rows.rs:368 | Files and +/− shown in the turn footer; not clickable. | P1 |
| "New session" row after `/clear` | web/components/chat/V2LifecycleRow.tsx | Missing | | | P2 |
| Context handoff rows (pending, strategy, summary) | web/components/chat/V2LifecycleRow.tsx | Missing | | | P2 |
| Fork rows ("Conversation fork" / "Forked from") with links | web/components/chat/V2LifecycleRow.tsx | Missing | | | P2 |
| "Created thread" and "Run interrupted" rows | web/components/chat/TimelineV2EventRow.tsx | Partial | app/thread/rows.rs:366 | "Stopped" in the turn footer. | P2 |
| Compaction row ("before → after tokens") | web/components/chat/TimelineSystemDivider.tsx | Partial | core/provider/claude/parser.rs:864, core/provider/codex/session.rs:1204 | Notice with the "before" count only. | P2 |
| Todo list / Tasks row | ct/orchestrationV2.ts (`todo_list`) | Done | core/provider/claude/tools.rs:223, app/thread/rows.rs:288 | No env var needed (T3 needs `CLAUDE_CODE_ENABLE_TODO_TOOLS=1`). | P1 |
| Subagent rows (prompt, progress, result) | web/components/chat/SubagentTooltipContent.tsx | Partial | core/provider/claude/parser.rs:734, core/provider/claude/tools.rs:156 | Shown as one Task tool card with steps as progress. Subagents as nested threads: #538 (37 reactions). | P1 |
| Open a subagent's transcript (read-only dialog, refresh) | web/components/chat/NativeChildWorkDialog.tsx | Missing | | | P1 |
| Child thread parent bar ("{model} subagent: {status}", Open parent) | web/components/chat/ProviderSubagentBar.tsx | Missing | | | P2 |
| Background work panel (commands, monitors, subagents) | web/components/chat/BackgroundWorkDetails.tsx | Missing | | | P2 |
| Error items with Retry | web/components/chat/TimelineV2EventRow.tsx | Partial | app/thread/rows.rs:332, core/engine/orchestrator.rs:1171 | Error notice and Failed status, no Retry. | P0 |
| Usage-limit error "Retry after {reset time}" | web/components/chat/ThreadErrorBanner.tsx | Partial | app/inbox.rs:171, app/format.rs:47 | Reset time shown in the Inbox. | P1 |
| "Copy request for admin" on ChatGPT workspace limits | web/components/chat/ThreadErrorBanner.tsx | Missing | | | P2 |
| Usage-limit recovery banner (Resume at reset, Resume now, Snooze until reset, Wake now, Cancel auto-resume) | web/components/UsageLimitRecoveryBanner.tsx | Partial | core/engine/orchestrator.rs:1193, :514, app/thread_menu.rs:150 | Queued messages resume at reset automatically; no banner, no Resume now, no cancel. Codex #28931 (86 reactions). | P1 |
| Stop, and force stop when the agent does not respond | web/components/ThreadActivityBanners.tsx, useForceStopThread.ts | Done | app/composer.rs:417, app/actions.rs:37, core/provider/claude.rs:35, :444, core/provider/codex/session.rs:22 | Interrupt, then kill after 10 s automatically; the Claude process is kept on a normal stop (T3 killed it and re-paid `--resume`, #7338). Fixes #2234, #4713. | P0 |
| Worktree setup card (log, open terminal, cancel, work locally) | web/components/WorktreeSetupCard.tsx | Missing | | | P1 |
| Workflow opening block | web/components/WorkflowOpeningBlock.tsx | Missing | | | P2 |
| Secret request card (masked, save securely, decline) | web/components/SecretRequestCard.tsx | Missing | | | P2 |
| Link request card (pair a machine) | web/components/LinkRequestCard.tsx | Missing | | | P2 |
| Feedback request card (send a report to the T3 team) | web/components/FeedbackRequestCard.tsx | N/A | | Posts to T3's hosted feedback service. Orbit's equivalent is "Copy diagnostics" (Maintenance). | P2 |
| Screen-reader announcer (status, messages, everything) | web/components/ThreadActivityAnnouncer.tsx | Missing | | | P2 |
| Compact / Keep chip when the Claude prompt cache expired | web/components/chat/compaction.logic.ts | Missing | | Always ask; never compact on Enter (#17467). | P2 |

### Composer: editor

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Rich-text Markdown editor (source markers on caret) | web/components/ComposerPromptEditorTiptap.tsx | Partial | app/composer.rs:46 | Plain auto-growing text area. | P2 |
| Block shortcuts (`- `, `1. `, `- [ ] `, `> `, `# `, `---`, code fence with language picker) | web/components/ComposerCodeBlockLanguagePicker.tsx | Missing | | | P2 |
| List continuation and nesting (Shift+Enter, Tab) | composer-list-continuation.ts | Missing | | | P2 |
| Code-block editing (auto-indent, Tab, Backspace unwraps) | composer-code-block.ts | Missing | | | P2 |
| Rich-text on / off setting | ct/settings.ts (`composerRichTextEnabled`) | Missing | | | P2 |
| Per-block text direction | composer-text-direction.ts:41-66 | Missing | | | P2 |
| Limits (120,000 chars, 100 attachments, image size caps) with over-length validation | ct/chatAttachment.ts:13-17 | Missing | | Validate before storing blobs. | P1 |
| `@` file and folder mentions (fuzzy host-side search) as chips | web/components/chat/useComposerMenuItems.ts | Missing | | Index from the watcher, not a scan per keystroke. | P1 |
| `@` thread mentions (agent reads the thread on demand) | web/composerThreadImport.ts | Missing | | Users cannot @-reference another agent's results in Cursor. | P2 |
| `@` machine mentions | web/components/chat/useComposerMenuItems.ts | Missing | | | P2 |
| `@model` mentions for delegation | web/composerModelMentions.ts | Missing | | | P2 |
| `#` PR mentions (newest first, number filter, debounced search) | web/components/chat/useComposerMenuItems.ts:153-200 | Missing | | | P2 |
| `$` skill mentions with ranking | web/providerSkillSearch.ts:13-70 | Missing | | | P1 |
| `/` command and skill menu | web/components/chat/useComposerMenuItems.ts:256-330 | Partial | core/provider/claude/parser.rs:885, core/provider/codex/session.rs:835, proto/model.rs:516 | Commands and skills are collected from the provider; no menu. Must stay fast with hundreds of skills (#28489). | P1 |
| Context chips (terminal excerpt, review comment, preview annotation, picked element, PR, file, image, thread, citation) | web/components/composerContextPresentation.tsx | Missing | | | P1 |
| Chips cut and pasted between drafts, re-fetched on paste, dashed when unresolved | web/components/composerInlineTokenPaste.ts | Missing | | | P2 |
| Large paste (32 KiB or more) becomes a text attachment | web/components/chat/ChatComposer.tsx:1187 | Missing | | Offer, do not force (Codex #25144, 88 reactions against auto-conversion). | P1 |
| Paste as text (`mod+shift+v`) | web/lib/desktopPasteAsText.ts | Missing | | | P2 |
| Image paste (GIF, HEIC, JPEG, PNG, WebP; HEIC converted, large photos resized) | web/components/chat/useComposerAttachmentIntake.ts | Missing | app/composer.rs:440 | Paste falls through to plain text only. | P1 |
| Typing anywhere redirects into the composer | web/usePasteToFocusComposer.ts | Missing | | | P2 |
| Drag and drop files and folders from the OS | web/folderDrop.ts | Missing | | | P1 |
| Drag threads from the sidebar as context | web/threadContextDrop.logic.ts | Missing | | | P2 |
| Attachment strip (thumbnails, progress, retry, remove) | web/components/ComposerAttachmentStrip.tsx | Partial | app/composer.rs:352 | Chips with remove and "Storing n…". | P1 |
| Uploads start immediately; Send waits for them | web/components/useComposerAttachmentIntake.ts | Done | app/composer.rs:113, :132, core/store.rs:431 | Stored once by BLAKE3 hash, never inlined as base64 (T3 #26591: images were 46% of the DB). | P1 |

### Composer: commands, sending, queue, drafts

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| `/model` opens the model picker | web/components/chat/ChatComposer.tsx:2090-2140 | Missing | | | P2 |
| `/side`, `/btw` side questions | web/components/chat/useSideChat.ts | Missing | | Visible error when unavailable (T3 leaves the trigger text silently). | P2 |
| `/clear`: fresh provider session in the same thread, history stays visible | web/components/chat/useComposerMenuItems.ts | Missing | | Never resume the pre-clear session (#24152); hide the provider's raw `/clear`. | P1 |
| Provider `/compact` | provider-core/server/snapshotProbe.ts:61 | Partial | core/provider/claude/parser.rs:864 | Typed text passes through to the provider; no menu entry, no availability check. Manual compact: Codex #11325 (158 reactions). | P1 |
| Codex `/goal` and `/feedback` | srv/provider/CodexProvider.ts:1162-1173 | Missing | | | P2 |
| `/usage-limits` opens the limits panel | sh/usageLimits.ts:854 | Missing | | | P2 |
| ACP agents' own commands with input hints | packages/provider-acp | Missing | | | P2 |
| Provider commands only at the start; mid-turn commands wait for the turn | composerSlashCommandSearch.ts:15-30 | Missing | | | P2 |
| Send-key setting (Enter / Mod+Enter for multiline / Mod+Enter) | ct/settings.ts (`sendShortcut`) | Missing | app/composer.rs:49 | Enter always sends. Claude #89673 (167 reactions). | P1 |
| Opposite follow-up while running (`composer.sendAlternate`) | sh/keybindings.ts:94-110 | Done | app/composer.rs:108, :412 | Enter queues and Ctrl+Enter steers, and the composer says so before you press (app/composer.rs:414). | P0 |
| Send in background / send and open a new thread (`mod+alt+enter`) | sh/keybindings.ts | Missing | | | P2 |
| Follow-up behaviour setting (steer or queue) | ct/settings.ts:539 | Partial | app/composer.rs:108 | Fixed to queue by default, which avoids T3's surprising steer default on Claude (§1 B bugs); no setting. | P1 |
| Primary button adapts (Interrupt / Queue / Steer / Update queued / Resume) | web/components/ComposerPrimaryActions.tsx | Partial | app/composer.rs:417-429 | Stop when empty and running; otherwise Send. | P1 |
| Queued messages stored on the host and shown at the end | web/components/QueuedRunsControl.tsx | Done | core/engine/orchestrator.rs:847, app/thread/mod.rs:312 | The run queue is in memory: after a restart the Queued items stay but never send (see bugs). | P0 |
| Queued: Send now (promote to steer) | web/components/QueuedRunsControl.tsx | Missing | | | P1 |
| Queued: Edit in the composer (Save / Cancel) | web/queuedMessageEdit.ts | Partial | core/engine/orchestrator.rs:978 | Engine supports text edits; UI only removes. Codex #44781 (editing a queued follow-up errors). | P1 |
| Queued: Remove | web/components/QueuedRunsControl.tsx | Done | app/thread/mod.rs:343-350 | | P1 |
| Queued: Move to front | web/components/QueuedRunsControl.tsx | Missing | | | P2 |
| Steer the oldest queued (`mod+shift+enter`), edit the latest (`alt+up`) | sh/keybindings.ts | Missing | | | P2 |
| Stop or restart holds the queue until Resume | web/useComposerResume.ts, ComposerHeldSendBanner.tsx | Partial | core/engine/orchestrator.rs:1184 | After Stop the queue continues automatically; after a restart it is lost. T3 #18805 (queued stuck after Stop). | P1 |
| Prompt stash (`mod+s`, 20 entries with attachments) | web/promptStashStore.ts:11-17 | Missing | | | P2 |
| Prompt history recall (Up / Down in an empty composer) | web/composerPromptHistory.ts | Missing | | | P1 |
| Suggested next prompt (ghost text, Tab accepts) | web/usePromptSuggestion.ts | Missing | | | P2 |
| Voice input: mic button and shortcut (auto, hold, toggle), 5 min, Esc cancels | web/speech, web/components/ComposerSpeechButton.tsx | Missing | | | P2 |
| Voice: transcript at the caret or over the selection; Send while recording waits | web/components/VoiceInputPill.tsx | Missing | | | P2 |
| Voice: local models (Parakeet, Whisper…) with VAD, or cloud / Apple Speech | srv/speech, speech/catalog.json | Missing | | Run in-process with a native ASR crate; no 120 MB helper. | P2 |
| Voice options (language, translate, dictionaries, filler removal, LLM cleanup, live preview, microphone priority) | web/speech/postProcessing.ts | Missing | | | P2 |
| Voice in other text fields | web/useTextFieldSpeech.tsx | Missing | | | P2 |
| Provider status banner (signed out, broken, unsupported version, Refresh) | web/components/ProviderStatusBanner.tsx | Partial | app/composer.rs:191, app/settings.rs:74 | Status appears in the model menu and Settings, not as a banner where you type. | P1 |
| Server update and auto-balance banners | web/components/useSystemComposerBanners.tsx | Missing | | | P2 |
| Usage-limit banner with account reveal and "Manage usage" | web/components/ComposerUsageLimits.tsx | Partial | app/shell.rs:270-325 | Quota shown in the title bar. | P1 |
| Fresh-session and project-clone banners | web/components/ComposerBannerStack.tsx | Missing | | | P2 |

### Models, providers, options

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Codex driver (`codex app-server` JSON-RPC) | packages/effect-codex-app-server | Done | core/provider/codex.rs:1, core/provider/codex/session.rs:24-56 | One supervised app-server per thread with restart and `thread/resume`; T3 shares one, so a crash breaks every thread (#16502, #17045). | P0 |
| Claude Code driver | srv/orchestration-v2/Adapters/ClaudeAdapterV2.ts | Done | core/provider/claude.rs:1 | CLI stream-json held for the whole session; live `set_model` and `set_permission_mode` (claude.rs:533-546) instead of restarts. | P0 |
| Cursor driver | `@cursor/sdk` | Partial | core/provider/presets.rs (`cursor-agent acp`) | Native, through Cursor's ACP mode and the ACP adapter; T3's V2 `@cursor/sdk` worker needs Node, so SDK-only rewind and snapshots are not available. | P2 |
| OpenCode driver (HTTP + SSE) | `@opencode-ai/sdk` | Done | core/provider/opencode.rs, tests/opencode.rs | One `opencode serve` per session on a random loopback port and password; Build / Plan are OpenCode's own agents (#42213); access levels are session permission rules; Stop waits for idle. v2 (`/api/*`) not yet. | P1 |
| Pi driver (`pi --mode rpc`) | provider-pi/server/rpc.ts | Done | core/provider/pi.rs, tests/pi.rs | Native steering, live model and thinking level, resume by session file; Pi has no approvals, so the thread says so once. | P1 |
| ACP driver and ACP Registry (any ACP agent, v2 with v1 fallback) | packages/provider-acp*, effect-acp | Partial | core/provider/acp.rs, core/provider/acp/, tests/acp.rs | ACP v1 with resume/load, sign-in, config options, plan mode; cancel ends the turn only when the agent acknowledges (T3 §3c bug 1). No v2 and no registry download yet. | P1 |
| Grok, fx, Kiro, Antigravity (ACP flavours, Antigravity managed install and sign-in) | provider-grok/server/acpSupport.ts, provider/acp/Antigravity* | Partial | core/provider/presets.rs | Presets for Gemini CLI, Qwen Code, Grok, Kiro, Cursor, Goose and Antigravity appear when installed; flavour quirks (Grok `_meta`, Antigravity eager sign-in and managed install, Kiro steer) not yet. | P2 |
| Muse Code driver | `@muse-code/sdk` | Missing | | | P2 |
| Provider instances (name, accent, env with secret refs, enabled, binary path, launch args, config dir, custom models) | ct/providerInstance.ts:74-100 | Partial | proto/settings.rs (`ProviderInstance`), core/engine/sessions.rs, app/composer.rs | Instances in settings with binary, args, config dir, env and enabled; per-runtime settings apply to every instance; probed in parallel; picked in the model popover. No editor UI, accent, secret refs or custom models yet. | P1 |
| Several accounts per provider (extra `CODEX_HOME` / `CLAUDE_CONFIG_DIR`) | doc/providers-*.md | Done | proto/settings.rs, core/engine/sessions.rs | Claude #18435 (1,462 reactions). An instance's config dir becomes `CLAUDE_CONFIG_DIR` / `CODEX_HOME`; switching ends the old process first, so no writer-lock clash (#8259, #30630), and hands the context off. | P1 |
| Model selection with generic option descriptors | ct/modelSelection.ts | Done | proto/model.rs:64-178 | Options not reported by the new model are ignored, so a model switch never sends invalid flags. | P0 |
| Claude options from the CLI catalog (effort, thinking, fast mode, context window) | srv/provider/ClaudeModelCatalog.ts:65-108 | Done | core/provider/claude/catalog.rs:1 | Nothing hard-coded. | P0 |
| Codex options from `model/list` (reasoning effort, service tier) | srv/provider/CodexProvider.ts:291-334 | Done | core/provider/codex/map.rs:20 | | P0 |
| Provider default model | ct/model.ts:144 | Done | app/ui.rs:156 | | P1 |
| Model slug aliases | ct/model.ts | Missing | | | P2 |
| Model picker grouped by provider | web/components/ProviderModelPicker.tsx | Done | app/composer.rs:175 | | P0 |
| Model picker favourites, search, stars, "New" badges | web/components/ModelPickerSidebar.tsx | Missing | | | P2 |
| "Show more" older models, Unavailable state, "Set up {provider}" | web/components/ModelPickerContent.tsx | Partial | app/composer.rs:191 | "Not installed" note only. | P2 |
| Unavailable saved model kept with "(Unavailable)" | web/modelSelection.ts | Partial | app/ui.rs:166 | Shows the raw slug, no marker. | P2 |
| Account switching inside one config dir | web/components/ProviderModelPicker.tsx | Missing | | | P2 |
| Model picker keys (`mod+shift+m`, jump 1–9, previous / next provider) | sh/keybindings.ts | Missing | | | P2 |
| Effort control and increase / decrease effort keys | web/components/TraitsPicker.tsx | Partial | app/composer.rs:244 | Dropdown, no keys. | P1 |
| Boolean and select options (Fast, Thinking, context window) | web/components/TraitsPicker.tsx | Done | app/composer.rs:223-296 | | P1 |
| Fast / Ultrafast speed badge | web/components/TraitsSpeed.tsx | Partial | app/composer.rs:277 | Toggle visible, no badge elsewhere. | P2 |
| Custom models (slug, name, capabilities) | web/customModelEditor.logic.ts | Missing | | | P2 |
| Context usage shown once reported | web/components/ThreadDetailsUsage.tsx | Done | app/ui.rs:100, app/thread/mod.rs:256, app/fleet.rs:94 | Always visible in header, composer and Fleet; T3 hid it by default (#9352, 45 reactions; Codex #23794). | P0 |
| Claude auto-compact threshold and idle-compaction mode | ct/settings.ts (`compactIdleThreads`) | Missing | | | P2 |
| "Update all" providers | web/components/ProviderUpdatesAction.tsx | Missing | | | P2 |
| Provider health refresh | ct/settings.ts (`providerHealthRefreshInterval`) | Done | app/settings.rs:64, core/engine/orchestrator.rs:438 | On demand and at start, not every 5 minutes. | P1 |
| Provider MCP sign-in (open URL, paste redirect, finish) | web/components/chat/McpSignInAction.tsx | Missing | | One notice per server, not per plugin (#40083). | P2 |

### Permission modes, plan mode, approvals, questions

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Access levels per thread | ct/providerPolicy.ts:25-43 | Done | proto/model.rs:144, app/composer.rs:325, core/provider/claude.rs:126, core/provider/codex/map.rs:186 | Readonly / Ask / Auto-edit / Full; new threads default to Ask, never Full (T3 defaults to full-access). | P0 |
| "Auto" mode (provider auto-review) | ct/providerPolicy.ts | Missing | | | P2 |
| Default access from settings, with per-project override | Settings → Threads | Partial | core/engine/orchestrator.rs:669 | Default is the last used value. | P1 |
| Managed policy caps the allowed modes | srv/policy/ManagedPolicy.ts | Missing | | | P2 |
| Per-provider mode fallbacks | doc/permission-modes.md | Partial | app/composer.rs:300 | Plan is hidden when the provider reports no plan mode. | P2 |
| Access mapped to Codex approval policy and sandbox, Claude permission mode | srv/orchestration-v2/Adapters/ClaudeAdapterV2.ts:3706 | Done | core/provider/claude.rs:126, core/provider/codex/map.rs:186 | Elicitations always reach the user (map.rs:186). | P0 |
| Access shortcut (`mod+shift+a`) | sh/keybindings.ts | Missing | | | P2 |
| Plan mode toggle in the composer | ct/providerPolicy.ts:45 | Done | app/composer.rs:307 | T3's web composer always sends `default` (§5.12), so plan mode is unreachable there. | P0 |
| Proposed plan card (Markdown, Plan badge, expand) | web/components/ProposedPlanCard.tsx | Partial | app/thread/rows.rs:508, :288 | Plan request card with the plan text, and a steps row. | P1 |
| Plan card menu (copy, download Markdown, save to workspace) | web/proposedPlan.ts | Missing | | | P2 |
| Implement the plan | (missing in T3) | Done | app/thread/rows.rs:550-559, core/provider/codex/session.rs:604, core/provider/claude/parser.rs:1254 | "Start building" / "Keep planning". T3 has no implement button. | P0 |
| Approval kinds (command, file read, file change, MCP elicitation, permissions) | ct/providerPolicy.ts:49-78 | Done | proto/model.rs:422, core/provider/codex/session.rs:623-688 | | P0 |
| Decisions (accept, accept for session, always, decline, cancel) | ct/providerPolicy.ts | Partial | proto/model.rs:438 | Allow, Allow for session, Deny with feedback; no "always" or "cancel". "For session" is scoped to the command or file, never the whole tool (T3 §3b bug 5). | P1 |
| Provider-supplied option labels, warnings, "More" menu | web/components/ComposerPendingApprovalActions.tsx:28-40 | Partial | app/thread/rows.rs:561-577 | Fixed buttons. | P1 |
| Deny with feedback; typing a reply while an approval is pending declines with that text | web/components/chat/ChatComposer.tsx | Partial | proto/model.rs:442, app/thread/rows.rs:567 | Feedback field exists in the protocol; UI always sends an empty one. | P1 |
| Explanation when a Full-access thread still asks | ct/providerPolicy.ts (`fullAccessOverride`) | Missing | | | P2 |
| "Waiting for approval"; "Interrupt or restart the run to respond" when the process is gone | web/components/TimelinePendingApprovalRow.tsx | Partial | core/engine/orchestrator.rs:1061 | Error toast on a dead session; the card stays. | P1 |
| File-edit approval shows the change | (T3 #42390 does not) | Done | app/thread/rows.rs:502 | Diff preview in the card. | P0 |
| Subagent and child approvals surface in the parent | web/components/ChildThreadApprovals.tsx | Partial | core/provider/claude/parser.rs:18 | Claude subagent and background approvals appear in the thread (never auto-denied); no "from {child}" label. #15082 (21 comments). | P1 |
| Thread banner "Open approval / Open question" | web/components/ThreadActivityBanners.tsx | Done | app/inbox.rs:117-153, app/shell.rs:300 | Inbox answers commands, edits and tools inline from any screen; title-bar "n need you". | P0 |
| User-input questions: header, options with descriptions and previews, multi-select, custom answer, required | ct/orchestrationV2.ts:1569 | Partial | proto/model.rs:432, app/thread/rows.rs:539 | Options are buttons; no free-text answer, no multi-select UI, no descriptions. | P0 |
| Several questions with previous / next | web/components/ComposerPendingUserInputPanel.tsx | Partial | core/provider/claude/parser.rs:15 | Parser handles several questions; UI shows one card. | P1 |
| Dismiss a question without answering | web/pendingUserInput.ts | Missing | | | P2 |
| Per-question attachments | web/questionAttachments.ts | Missing | | | P2 |
| Answer questions from the Inbox | (Orbit Inbox) | Partial | app/inbox.rs:146 | "Answer in thread" for questions and plans. | P1 |
| MCP Apps rendered in chat (sandboxed) | web/components/chat/McpAppFrame.tsx | Missing | | | P2 |
| HTML renders (visual replies via `html_preview` / `html_render`) | srv/htmlRender/HtmlRender.ts | Missing | | | P2 |
| T3 MCP tools for the agent (`send_to_user`, `send_attachments`, `request_secret`) | srv/mcp/toolkits/* | Missing | | Never export the host token to the agent's whole process tree (T3 §3c). | P2 |

## C. Code and dev tooling

### Diff panel

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Review panel beside the thread | web/components/DiffPanel.tsx | Done | app/review.rs:191, app/actions.rs:34 | Resizable sheet (Ctrl+R). | P0 |
| Scope: Changes (branch against a base ref) | web/diffPanelStore.ts | Done | app/review/mod.rs (`Scope::Branch`), core/git/diff.rs | Any local or remote branch from a searchable picker, defaulting to the project's base. | P0 |
| Scope: Uncommitted | web/components/DiffPanel.tsx | Done | app/review.rs:30, core/git/diff.rs:86 | | P0 |
| Scope: Latest turn | web/components/DiffPanel.tsx | Done | app/review.rs:43, core/engine/orchestrator.rs:219 | | P0 |
| Scope: any earlier turn (Turn ▸ submenu) | web/components/DiffPanel.tsx:983-1060 | Done | app/review/mod.rs (`render_turn_picker`), proto/wire.rs (`DiffScope::SinceTurn`) | Every turn labelled with its prompt, plus "Include later turns" (a turn and everything since, Emdash #1635). Follows the latest turn until another is picked. | P1 |
| Scope: one local commit (merge commits against first parent) | review/ReviewService.ts | Missing | | | P2 |
| Scope: one tool call's diff, opened from the timeline | web/components/chat/useRightPanelActions.ts:312 | Missing | | | P1 |
| Scope persisted per thread | web/diffPanelStore.ts | Missing | | Keep it with the thread on the host; T3 leaks entries for archived threads (§1 C bugs). | P2 |
| Whole-thread diff | `getFullThreadDiff` | Done | app/review.rs:44, core/engine/orchestrator.rs:223 | | P0 |
| One patch per tracked repository (multi-repo) | ct/checkpointDiff.ts | Missing | | | P2 |
| Turn diff leaves out files brought in by pull / merge / rebase | checkpointing/CheckpointDiffQuery.ts (`listAuthoredPaths`) | Missing | | | P2 |
| Base-ref picker ("Comparing {head} against {base}", use remote version) | web/lib/baseRefChoices.ts | Done | app/review/mod.rs (`load_branches`) | "Comparing orbit/x with main"; remote branches (`origin/main`) are in the same list. | P1 |
| Repository picker (multi-repo threads) | web/components/DiffPanel.tsx:1184-1220 | Missing | | | P2 |
| Split / unified toggle | web/components/DiffPanel.tsx | Done | app/review.rs:141 | | P0 |
| Line wrap toggle | web/components/DiffPanel.tsx | Done | app/review/mod.rs (View menu, Alt+Z) | | P1 |
| Hide whitespace toggle | web/components/DiffPanel.tsx | Missing | | | P2 |
| Hide test files | sh/changedFileKind | Done | app/review/model.rs (`is_test_file`) | Conventions across languages, unit-tested. | P2 |
| File tree (folders, +/− stats, reveal file) | web/components/diffs/DiffFileTree.tsx | Done | app/review/mod.rs (`render_tree_panel`), app/review/model.rs | Filterable, keyboard-driven (arrows; Alt+Up/Down walk files from the diff), single-child folders merged, status letter, counts, reviewed and overlap marks; clicking a diff line selects its file. | P1 |
| Refresh and throttled live refresh | web/components/DiffPanel.tsx (`useLiveRefresh`) | Done | core/engine/review.rs, app/review/mod.rs (`refresh_live`) | One watcher per repository shared by open threads; a change costs one `git status`; the sheet reloads at most every 400 ms, skips identical patches and keeps the line under the pointer. | P1 |
| Collapse / expand all files | web/components/DiffPanel.tsx | Done | app/review/mod.rs (View menu) | Reviewed files collapse on their own. | P2 |
| Scroll position restored per scope | web/components/diffs/useCodeViewScrollRestore.ts | Missing | | | P2 |
| Highlighted rendering, lazy large files, expandable context | web/components/DiffWorkerPoolProvider.tsx, StyledDiffCodeView.tsx | Done | app/review/mod.rs, core/git/diff.rs | One `git diff` for all files, parsed off the UI thread; tree-sitter highlighting (12 languages); one virtualized list with expandable context. 802 files / 49,609 changed lines: fetch and parse 290 ms, shown in 3 ms; wheel scrolling p99 13.6 ms, no frame over 18 ms (docs/BENCHMARKS.md). | P0 |
| Review comments sent to the agent (select lines, comment, chips with next message) | web/reviewCommentContext.ts, web/components/diffs/AnnotatableCodeView.tsx | Done | app/review/mod.rs (`send_comments`), app/review/model.rs (`comments_message`) | Comments stay with the thread across sheet opens and go as one follow-up with `path:line`, the quoted diff and the note (queued if the agent is busy). Not yet chips in the composer (WP6). | P1 |
| Whole-file comments | `buildFileReviewComment` | Done | app/review/mod.rs (file menu) | | P2 |
| File actions (open in preview or editor, reveal, open with, copy path) | web/diffFileActions.ts | Partial | app/review/mod.rs (`file_menu`) | Open with the system, show in folder, copy path, revert file, comment; no editor picker yet (WP2). | P1 |
| Commit links open in the diff pane | web/components/DiffPanel.tsx | Missing | | | P2 |
| Settings → Diffs (default layout, ignore whitespace, collapsed files, proactive panels, open PRs in app) | web/components/settings/SettingsPanels.tsx:1709-1850 | Missing | | | P2 |
| Diff colour scheme (red-green or colour-blind safe) | ct/settings.ts (`diffColorScheme`) | Done | app/review/mod.rs (`palette`) | Blue/orange from theme tokens for lines, word changes, tree and counts; signs keep +/− shapes. Session preference, not yet in Settings. | P1 |

### Checkpoints, revert, restore

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Snapshot before the first turn and after every turn, without touching the user's index or HEAD | srv/orchestration-v2/CheckpointService.ts:35-41 | Partial | core/engine/orchestrator.rs:933-947, :1144-1164, core/git/checkpoint.rs:83 | Persistent private index: rehashes only what changed, no temp object dirs, no timeout loop (#26227, #3646, #21328). But the engine stores bare tree ids from `snapshot()`, not pinned `checkpoint()` refs, so `git gc` may delete them (see bugs). | P0 |
| Large untracked files left out of snapshots | (T3 #38102 filled a 96 GB disk) | Done | core/git/checkpoint.rs:221, core/git/mod.rs:170 | 50 MB cap per new untracked file. | P0 |
| Nested repositories snapshotted as their own scopes | CheckpointService.ts:292 | Missing | | | P2 |
| Diff size limit with `[truncated]` marker | vcs/VcsDriver.ts:43 | Missing | | Stream and virtualise instead of truncating at 10 MB. | P2 |
| Revert and keep changes (rewind the chat only) | web/components/chat/ChatOverlayHosts.tsx:220-300 | Done | core/engine/orchestrator.rs:996 | Same as Edit from here. | P0 |
| Revert files too (restore files from a turn's checkpoint) | web/components/chat/useTimelineRewind.ts | Partial | app/review/dialogs.rs (`confirm_restore`, `edit_message`) | Review sheet: "Restore to before turn N…" names every file that changes. `edit_message` adds "Restore files too" to edit-and-resend; the thread view (WP5) still has to call it. Works outside worktrees. | P0 |
| Revert repositories too (multi-repo, all-or-nothing) | CheckpointService.ts:821-900 | Missing | | | P2 |
| Undo a restore | (not in T3) | Done | core/engine/review.rs (`UndoRestore`), app/review/dialogs.rs | Undo in the notification and the sheet's menu; undoing again swaps back. | P1 |
| Branch-changed banner with "Restore branch" | web/components/chat/ThreadActivityBanners.tsx:300-350 | Missing | | | P1 |
| Checkpoint refs deleted when no longer needed | `deleteCheckpointRefs` | Partial | core/engine/orchestrator.rs:729-737, core/git/checkpoint.rs:93, :131 | Removed on thread delete; `prune` and `prune_orphans` exist but nothing calls them. | P1 |

### Terminal

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| PTY on the host with default shell resolution (pwsh, powershell, cmd; `$SHELL`) | srv/terminal/Manager.ts:100-140 | Done | core/terminal.rs:126, :176 | ConPTY with a Job Object; the whole tree dies on close. | P1 |
| Shell override | `T3CODE_TERMINAL_SHELL` | Partial | core/terminal.rs:92 | `Shell` parameter exists; always `None` from the engine. | P2 |
| Terminal renderer | web/terminal/ghostty/* (libghostty-vt WASM on Canvas 2D) | Partial | term/lib.rs:1, term/view.rs:121 | Native `alacritty_terminal` view exists and is tested, but no Orbit screen mounts it. | P1 |
| Per-thread terminal drawer with groups and splits | web/components/ThreadTerminalDrawer.tsx | Missing | | No hard limit of 4 per group (§1 C bugs). | P1 |
| Terminal keys (toggle, split, split vertical, new, close) | sh/keybindings.ts:67-75 | Missing | | | P1 |
| Focused terminal keeps Ctrl chords, Escape and Ctrl+W | sh/keybindings.ts:410 | Partial | term/keys.rs:1 | Encoder is complete; app key contexts not wired. T3 #6362 (Escape never reached nvim), #731 (Ctrl+W on Windows). | P1 |
| Tab label from the foreground process | srv/terminal/Manager.ts:395-410 | Partial | term/view.rs:198 | OSC title available; no process lookup (T3 polls the process table every second). | P2 |
| Terminals keep running across thread switches | web/components/PersistentThreadTerminals.tsx | Partial | core/terminal.rs:1 | Engine-owned processes; no UI. | P1 |
| Scrollback saved to disk and replayed after restart (5,000 lines / 8 MiB) | srv/terminal/Manager.ts:1208-1240 | Missing | | | P2 |
| Context menu: Add to chat, Copy, Paste, Select all, Jump to latest | web/components/ThreadTerminalDrawer.tsx:414-441 | Partial | term/view.rs:447-485 | Copy, paste, clear, select all; no "Add to chat". | P1 |
| Scroll keys (Shift+PgUp / PgDn, jump to start / end) | web/components/ThreadTerminalDrawer.tsx | Done | term/lib.rs:80-85 | | P2 |
| Link detection (URLs, `file:line:col`, TypeScript errors, wrapped paths) | web/terminal-links.ts | Partial | term/links.rs:1, term/view.rs:623 | URLs only. | P1 |
| Read-only observers with paste disabled | srv/terminal/OutputProtocol.ts | Missing | | | P2 |
| Answers device-attribute and cursor queries | (T3 #6058: fish stalled 10 s) | Done | term/lib.rs:17-20, core/terminal.rs:19-22 | | P1 |
| Layout-aware input (AltGr, dead keys, IME) | (T3 #13046: `@` on German layout) | Done | term/keys.rs:1-6 | | P1 |

### Git and VCS (local)

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Live VCS status | srv/vcs/VcsStatusBroadcaster.ts | Partial | core/git/status.rs:44, core/git/watch.rs:105, proto/wire.rs:65 | Status, a debounced watcher and a `WorkspaceChanged` event all exist; nothing connects them. No 30 s polling (T3 #40051: 15–17 git processes per thread every 30 s). | P1 |
| Automatic `git fetch` every 30 s | ct/settings.ts:1110 | Missing | | Back off when idle, on battery, or when the window is hidden. | P2 |
| Branch selector (search, badges, start from remote, create and switch) | web/components/BranchPicker.tsx | Partial | core/engine/orchestrator.rs:280, core/git/status.rs:127 | `Branches` command only. Codex users asked to "put the branch selection BACK" (#49532). | P1 |
| Current branch shown on the thread | web/components/BranchToolbar.tsx | Partial | app/thread/mod.rs:256 | Header renders `workspace.branch`, but nothing sets it outside worktrees, so it never shows. Stale labels: T3 #11078. | P1 |
| Copy branch name | web/components/BranchToolbar.tsx | Done | app/thread_menu.rs:174 | | P2 |
| "Start new worktrees from remote by default" | web/components/settings/WorktreesSettings.tsx | Missing | | | P2 |
| Git actions control (Commit / Commit & push / Commit, push & PR / Push & create PR / Pull / Sync / Publish / Init) | web/components/GitActionsControl.tsx | Partial | core/engine/orchestrator.rs:265-279, core/git/remote.rs:26-74 | Commit, push and `gh pr create` in the engine; no UI. Deterministic buttons, not agent steps (Cursor `/worktree` complaint). | P1 |
| Disabled-action hints (behind upstream, no origin, detached HEAD) | web/components/GitActionsControl.logic.ts | Missing | | | P2 |
| Commit dialog (file checklist, optional message, commit on a new branch) | web/components/GitActionsControl.tsx | Missing | | | P1 |
| Confirmation when acting on the default branch, with "check out a feature branch" | web/components/GitActionsControl.tsx | Missing | | | P1 |
| Progress phases (branch, commit, push, PR) with output and timer | srv/git/GitWorkflowService.ts | Missing | | | P2 |
| Publish repository wizard (provider, repo, visibility, protocol) | web/components/GitActionsControl.tsx:558-990 | Missing | | | P2 |
| Pull | `vcs.pull` | Missing | | | P2 |
| Created PRs linked to the thread, including ones the agent opens with `gh pr create` | srv/git/linkCreatedPullRequest.ts | Missing | | | P1 |

### Source-control hosts and auth

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| GitHub (token, `GH_TOKEN`, `gh` auth, account per host) | packages/source-control-github | Partial | core/git/remote.rs:74 | Uses the user's own `gh` for PR creation; no OAuth app (Conductor's broad scopes complaint). | P1 |
| GitHub App device-flow sign-in | source-control-github/server/GitHubAppSignIn.ts | Missing | | | P2 |
| GitLab (`glab`) | packages/source-control-gitlab | Missing | | Detect WinGet installs (#10942). | P2 |
| Forgejo / Gitea (`fj`, `tea`) | packages/source-control-forgejo | Missing | | | P2 |
| Azure DevOps (`az devops`) | packages/source-control-azure | Missing | | | P2 |
| Bitbucket (token or Atlassian email + API token) | packages/source-control-bitbucket | Missing | | | P2 |
| Cursor Origin, GitCafe, TanStack Source | sourceControl/sourceControlDefinitions.ts | Missing | | | P2 |
| Rescan host detection | sourceControl/SourceControlDiscovery.ts | Missing | | | P2 |
| Remove agent credits when merging | ct/settings.ts (`removeAgentCreditsOnMerge`) | Missing | | | P2 |
| PR routing across environments (Read PRs / Read and act) | web/components/settings/PullRequestRoutingSettings.tsx | Missing | | | P2 |

### Pull requests

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| PR list with filters (involvement, state, draft, review, checks, labels, authors, scope) | web/components/pullRequest/PullRequestsPage.tsx | Missing | | Fetch on demand with conditional requests; T3 re-fetched every PR on focus and burned GraphQL quota (#13496). | P1 |
| Saved views for PR lists | web/pullRequestSavedViews.ts | Missing | | Store on the host so they sync (T3's are browser-only). | P2 |
| Quick actions on Shift, drag-to-close, stack badges | web/components/pullRequest/PullRequestsPage.tsx | Missing | | | P2 |
| Summary tab (edit title and body, tick checkboxes) | PullRequestSummaryTab.tsx | Missing | | | P2 |
| Activity tab (comments, reactions, resolve threads, reply) | PullRequestTimelineTab.tsx | Missing | | | P2 |
| Code tab (draft line comments, Submit review, viewed marks) | PullRequestCodeTab.tsx, filesViewedStore.ts | Missing | | | P1 |
| Checks (logs, rerun) | PullRequestDetailPanel.tsx | Missing | | | P1 |
| Merge (merge, squash, rebase, fast-forward), ready / draft, close / reopen, delete branch, update branch | ct/pullRequest.ts:59-80 | Missing | | | P1 |
| Auto-merge, revert, approve fork workflows | ct/pullRequest.ts | Missing | | | P2 |
| Request reviewers, labels, reactions | ct/pullRequest.ts | Missing | | | P2 |
| Merge pinned to the loaded head commit ("Load new commits") | PullRequestDetailPanel.tsx | Missing | | | P2 |
| Merge stack / Rebase stack | PullRequestDetailPanel.tsx | Missing | | | P2 |
| Work on a PR (check out locally or in a worktree, thread with the PR attached) | srv/git/GitManager.ts (`preparePullRequestThread`) | Missing | | Reuse a worktree that already has the branch (#42402). | P1 |
| Linked items panel, "Link pull request", agent tools to link and watch | web/components/LinkPullRequestDialog.tsx, ThreadPullRequestsPanel.tsx | Missing | | A reused branch must not stay tied to an old merged PR (#4970). | P1 |
| PR watch: wake the agent on failed checks, approvals, comments, conflicts | srv/pullRequest/PullRequestWatchService.ts | Missing | | ETag requests with back-off; T3's sweep leaked 1.7 GB per hour (#25172). | P1 |
| Review guide (agent writes ordered stops in a temporary checkout) | srv/pullRequest/PullRequestReviewGuideService.ts | Missing | | | P2 |

### Issues and triage

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Issues page (GitHub, GitLab, Azure DevOps, Forgejo, Linear; filters, search, load more, saved views) | web/components/issue/IssuesPage.tsx | Missing | | | P2 |
| Issue detail (comment, edit, labels, assignees, reactions, state) | srv/issue/IssueService.ts | Missing | | | P2 |
| Link an issue to a thread; start a thread from an issue | srv/issue/IssueService.ts | Missing | | Vibe Kanban #1677 (issues import). | P2 |
| Linear accounts with per-project team binding | packages/issue-tracker-linear | Missing | | Free, not paywalled (Superset complaint). | P2 |
| Triage an issue (project-less thread with a playbook and the app's own source) | srv/triage/TriageService.ts:19-100 | Missing | | Orbit version: a thread on Orbit's own source and logs that drafts a bug report. | P2 |

### Text generation

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Commit message generation | provider-core/server/textGeneration.ts:122-145 | Partial | core/engine/textgen.rs (`Command::GenerateText`) | Engine API: no tools, hooks, slash commands or MCP servers (#41799, #40529); the review sheet does not call it yet. | P1 |
| PR title and body generation (from the thread's work and checks) | provider-core/server/textGeneration.ts | Partial | core/engine/textgen.rs (`TextTask::PullRequest`) | Engine API; not wired to the pull request form yet. | P1 |
| Branch name generation | provider-core/server/textGeneration.ts | Missing | | | P2 |
| Backends: coding agent, or OpenAI / Anthropic / OpenRouter / OpenAI-compatible (Ollama, LM Studio) | srv/textGeneration/textGeneratorDrivers.ts | Missing | | | P2 |
| Per-task model picker with Test | Settings → Text generation | Missing | | | P2 |
| Writing style (repository conventions, Conventional Commits, custom; follow PR templates) | web/components/settings/SourceControlWritingSettings.tsx:49-69 | Missing | | | P2 |

### Preview browser

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Preview panel with tabs, back / forward / refresh, URL bar, search engine | web/components/preview/PreviewView.tsx | Missing | | Off unless opened; T3's idle preview used 228% GPU (#14730) and 11 GB (#37987). | P2 |
| Find on page, zoom, device presets, colour-scheme emulation | web/components/preview/PreviewFindBar.tsx | Missing | | | P2 |
| Empty state with discovered local servers and recent URLs; stop a dev server | web/components/preview/PreviewEmptyState.tsx | Missing | | | P2 |
| Floating mini-player, picture-in-picture, pop-out window | web/components/ThreadPreviewMiniPlayer.tsx | Missing | | | P2 |
| Element picking and annotation sent to chat | desk/preview/PickPreload.ts | Missing | | | P2 |
| Screenshot and screen recording with cursor overlay | desk/preview/RecordingInput.ts | Missing | | | P2 |
| Agent browser automation (MCP tools, tab ownership, cursor overlay) | ct/previewAutomation.ts:20-50 | Missing | | | P2 |
| Port discovery attributed to threads | srv/preview/PortScanner.ts | Missing | | Each worktree already reserves a port block (core/git/worktree.rs:8-10), so attribution is exact. | P2 |
| Remote localhost tunnel and port forwarding | desk/preview/LoopbackTunnel.ts, srv/preview/PreviewTunnel.ts | Missing | | | P2 |
| Browser profiles, cookie import, passkeys | desk/preview/BrowserImport/ | Missing | | | P2 |
| Default browser on macOS | doc/default-browser.md | Missing | | | P2 |
| Headless server browser streamed as a screencast | srv/preview/ServerBrowser.ts | Missing | | | P2 |

### SnapShot (window capture into a draft)

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Global shortcut captures the foreground window into the draft | desk/snapShot/DesktopSnapShot.ts | Missing | | | P2 |
| Optional accessibility tree and app text | desk/snapShot/SnapShotAccessibility*.ts | Missing | | | P2 |
| Per-platform capture (macOS, Windows, Linux Wayland variants) | desk/snapShot/{Mac,Gnome,Kde,Hyprland,Niri,Portal}*.ts | Missing | | | P2 |
| Shortcut configuration with collision checks; sound, flash, animation toggles | desk/snapShot/CaptureShortcutConfig.ts | Missing | | | P2 |
| Pending captures survive a crash | desk/snapShot/DesktopSnapShot.ts | Missing | | | P2 |

### Workflows (Labs)

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Durable scripted workflows (journaled steps, resume after restart, hot reload) | srv/workflows/WorkflowEngine.ts | Missing | | | P2 |
| Triggers (every, daily, webhook, new issues and PRs) | srv/workflows/triggers.ts | Missing | | | P2 |
| Agent steps in hidden threads, typed returns, retry / pause / cancel cascade | srv/workflows/workflowAgent.ts | Missing | | | P2 |
| Workflows page (Needs you / Running / All, run feed, editor, answer / resume / cancel) | web/components/workflows/* | Missing | | Workflow questions go to the Inbox like any other request. | P2 |
| Workflow CLI | srv/cli/ | Missing | | | P2 |

### Scheduled tasks

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Scheduled prompt on a project with model, mode and workspace strategy | ct/scheduledTask.ts | Missing | | Branch from the configured base, not always `main` (#16183). | P2 |
| Schedules: interval, time of day with weekdays, once, webhook | srv/scheduling/Scheduler.ts | Missing | | | P2 |
| Thread modes: new thread each run, continue a thread, fresh session in a thread | ct/scheduledTask.ts | Missing | | | P2 |
| Webhook options (HMAC check, filters, `{{body.x}}` templates, dedupe, coalesce, delivery log, token rotation) | srv/scheduledTasks/* | Missing | | | P2 |
| Relay holds webhook deliveries while the environment is offline | ct/scheduledTask.ts | N/A | | Depends on T3 Connect's hosted relay. | P2 |
| Run now, enable / pause, last and next run status | web/components/settings/ScheduledTasksSettings.tsx | Missing | | | P2 |

### Open in editor

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Open in editor or file manager (Cursor, VS Code, Zed, JetBrains IDEs, …) | ct/editor.ts | Partial | app/settings.rs:123 | Only "Show in Explorer" for the data folder. | P1 |
| Open at `file:line:col` (goto, line-column, direct-path styles) | ct/editor.ts | Missing | | | P1 |
| Remote editors through URL schemes (`vscode://`, `cursor://`, `jetbrains://`) | ct/editor.ts | Missing | | | P2 |
| Preferred editor remembered; "Open with ▸" installed editors; `mod+o` opens the favourite | web/editorPreferences.ts | Missing | | | P1 |

## D. Settings, keybindings, app shell, remote, desktop

### Keybindings

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| User keybindings file (`{key, command, when}`) | srv/config.ts:162, ct/keybindings.ts | Missing | app/actions.rs:26-43 | Bindings are hard-coded. | P1 |
| Defaults backfilled on start; file watched and applied live | srv/keybindings.ts:97-115,470-610 | Missing | | Never drop all new defaults because one rule is invalid; show config errors (T3 hides them, §1 D bugs). | P2 |
| `mod` token (Cmd on macOS, Ctrl elsewhere), layout-aware matching | sh/keybindings.ts:26-210 | Partial | app/actions.rs:28-41 | All bindings say `ctrl-`, so macOS gets Ctrl instead of Cmd. | P1 |
| `when` clauses with context keys | sh/keybindings.ts:211-363 | Partial | app/actions.rs:34, :37, :40 | GPUI key contexts (`Workspace`). Reject unknown identifiers instead of treating them as false (T3 typo bug). | P2 |
| Project-script commands (`script.<id>.run`) | ct/keybindings.ts:37-125 | Missing | | | P2 |
| Settings → Keybindings (record, edit `when`, Unassign, Reset, conflict warnings) | web/components/settings/KeybindingsSettings.tsx | Missing | | Flag overlapping conditions, not only identical ones (T3 bug). | P2 |
| Fixed keys: F2 rename, Shift+F10 / Menu key for the thread menu, arrow-key sidebar navigation | doc/keybindings.md | Missing | | | P1 |
| Quit confirmation (hold or double-press Ctrl+Q) | desk/window/QuitHold.ts, web/components/QuitHoldOverlay.tsx | Missing | app/actions.rs:41 | Ctrl+Q quits at once, even with agents running. #10697 asks for a confirm while agents run. | P1 |
| F6 cycles focus between sidebar, conversation, composer and panels | doc/keyboard-focus.md | Missing | | | P2 |
| Command palette (`mod+k`) | sh/keybindings.ts | Done | app/actions.rs:29-30 | | P0 |
| New thread variants (`mod+n`, `mod+shift+n` in project, `mod+alt+n` without project) | sh/keybindings.ts:113-116 | Partial | app/actions.rs:28 | `Ctrl+N` only. | P1 |
| Stop the turn | `thread.stop` (unbound in T3) | Done | app/actions.rs:37 | `Ctrl+.` by default. | P0 |
| Review / diff toggle | `diff.toggle` (`mod+d`) | Done | app/actions.rs:34 | `Ctrl+R`. | P0 |
| Focus composer | | Done | app/actions.rs:40 | `Ctrl+L`. | P1 |
| Back / forward (`mod+[`, `mod+]`) | sh/keybindings.ts | Missing | | | P2 |
| Right panel and thread panel toggles, new tab, close tab | sh/keybindings.ts | Missing | | | P2 |
| Go to file (`mod+p`) | `filePicker.toggle` | Missing | | | P1 |
| Search project contents (`mod+shift+f`) | `projectSearch.toggle` | Missing | app/actions.rs:32 | Ctrl+Shift+F is taken by Fleet. | P1 |
| Usage page (`mod+u`) and its keys (c / t / l, periods) | sh/keybindings.ts | Missing | | | P2 |
| Theme picker, appearance cycle, theme editor keys | sh/keybindings.ts | Missing | | | P2 |
| Composer keys (stash, steer queued, edit queued, send in background, send and new thread) | sh/keybindings.ts:94-110 | Missing | | | P2 |
| Model picker, host, effort, access, workspace, branch, previous-worktree keys | sh/keybindings.ts | Missing | | | P2 |
| Copy PR number, open favourite editor | sh/keybindings.ts | Missing | | | P2 |
| Thread keys (copy reference, settle, pin, undo, jump 1–9) | sh/keybindings.ts:129-139 | Missing | | | P2 |
| Previous / next thread (`mod+shift+[ / ]`) | sh/keybindings.ts | Partial | app/actions.rs:38-39 | Ctrl+Tab / Ctrl+Shift+Tab; docs/DESIGN.md says Ctrl+Shift+[ / ]. | P1 |

### Command palette

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Palette with commands, threads and projects | web/components/CommandPalette.tsx | Done | app/palette.rs:25-112 | Built once per open from the client state; T3 missed its caches on every keystroke (#42338). | P0 |
| Modes: root, Go to file, Search contents; Esc back to root; `>` for actions only; number keys pick | web/components/CommandPalette.logic.ts:811-846 | Missing | | | P1 |
| Groups: Recent threads, Settings items, submenus (Projects, Environments, Sources, Theme, Forwarded ports, Options) | web/components/CommandPalette.logic.ts:811-830 | Partial | app/palette.rs:82-93 | Commands, Threads, New thread in. | P1 |
| Thread and project actions (find in thread, copy prompt link, copy reference, link PR, linked items, pin project, project settings, unarchive) | web/components/CommandPalette.tsx:1356-1880 | Missing | app/palette.rs:30-37 | Six commands today. | P1 |
| Restart agent session (with confirm) | web/components/CommandPalette.tsx | Missing | | Restart resumes with the same session id, so context is kept (#2256). | P1 |
| Navigation actions (PRs, issues, workflows, usage, settings, focus areas) | web/components/CommandPalette.tsx | Partial | app/palette.rs:31-36 | Inbox, Fleet, Settings, toggle sidebar. | P1 |
| Project actions (New project, Add project, Open WSL folder, connect environment) | web/components/CommandPalette.tsx | Partial | app/palette.rs:34 | Add project only. | P1 |
| Settings search results in the palette | web/settingsSearch.ts | Missing | | | P2 |

### Settings

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Settings persisted (per device and per host) | ct/settings.ts:416-623, :1393-1647 | Missing | app/settings.rs:15, app/main.rs:57 | Preferences live in a global that is reset on every launch, so even the appearance choice is lost. Write files atomically (T3 #4750 bricked on a torn write). | P0 |
| Settings pages (General, Appearance, Chat, Diffs, Threads, Projects, Keybindings, Providers, Source control, Worktrees, Environments, Maintenance, About, Archive…) | web/components/settings/SettingsSidebarNav.tsx:95-123 | Partial | app/settings.rs:25-148 | One dialog: Appearance, Agents, Data, Remote host. | P1 |
| Environment / project scope picker with inheritance and reset per row | web/components/settings/SettingsScopeContext.tsx | Missing | | | P2 |
| Managed policy (admin JSON, locks, CLI check) | srv/policy/ManagedPolicy.ts, doc/managed-policy.md | Missing | | Refuse unknown keys with a readable error instead of refusing to start. | P2 |
| Language and timestamp format | ct/settings.ts | Missing | | | P2 |
| Confirmations (archive, delete, unpin, terminal close, quit) | ct/settings.ts | Partial | app/thread_menu.rs:86 | Delete always confirms; no other confirmations or settings. | P2 |
| Chat width and HTML preview width | ct/settings.ts (`chatWidth`) | Missing | app/thread/rows.rs:28 | Fixed 52 rem column. Codex #16669. | P2 |
| Font sizes and families (interface, prompt, code, terminal), smoothing, word wrap | web/appearanceFonts.ts | Partial | app/theme.rs:38 | One base size scales everything, no UI. Claude #48237 (font size). | P1 |
| Contrast, glass opacity, panel animation duration | ct/settings.ts | Missing | | Honour reduced motion; no blur effects (T3 flicker reports #7766). | P2 |
| Response streaming mode (per turn or per paragraph) | ct/settings.ts (`responseStreamingMode`) | Missing | | Token streaming costs one row re-render, so the paragraph mode is not needed for performance. | P2 |
| Keep the computer awake while agents work | ct/settings.ts (`keepAwakeWhileAgentsWork`) | Missing | | | P1 |
| Continue threads after an update or restart | ct/settings.ts (`continueThreadsAfterServerUpdate`) | Missing | core/store.rs:213 | Busy threads become Idle on start; nothing resumes them. | P1 |
| Background activity profile (balanced, performance, battery saver) | ct/settings.ts (`backgroundActivityProfile`) | Missing | | Orbit has no polling loops, so idle cost is near zero; a profile only needs to gate fetch and PR watch. | P2 |
| Storage cleanup policy (worktrees, logs, browser artifacts) | ct/settings.ts (`storageCleanup`) | Missing | | | P2 |
| New-project defaults (base directory, ghq layout, workspace mode, start from origin) | ct/settings.ts | Missing | | | P2 |
| Providers page (instances, env, binary, custom models, update checks) | web/components/settings/ProvidersSettings.tsx | Partial | app/settings.rs:57-105 | Status, version, account, model count and "Check again". | P1 |
| Labs toggles (guest sharing, linked environments, workflows, prompt suggestions) | ct/settings.ts | Missing | | | P2 |
| Usage settings (retention, price overrides, aliases, model filter) | ct/settings.ts | Missing | | | P2 |
| Desktop settings (exposure mode, Tailscale serve, update channel, WSL backend and distro) | desk/settings/DesktopAppSettings.ts:98-112 | Missing | | | P2 |
| Window size, position and maximised state remembered | desk/window/DesktopWindow.ts | Missing | app/main.rs:66-71 | Window opens centred at 1280×820 every time. | P1 |
| Settings search | web/settingsSearch.ts | Missing | | | P2 |

### Appearance, themes, i18n

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Appearance: system / light / dark | web/themePalette.ts:47-52 | Done | app/settings.rs:36-55, app/theme.rs:45, app/shell.rs:59 | Follows system changes live; the choice is not saved (see Settings). | P1 |
| Built-in themes (t3-chat, grove, ocean, ember, iris) | sh/themePalettes.ts | Missing | app/assets/orbit-theme.json | One light and one dark theme. | P2 |
| Separate theme for the light and dark halves | web/themePalette.ts | Missing | | | P2 |
| Theme editor (69 roles, vivid generation, picker, sanitised custom CSS, JSON import / export) | web/components/settings/ThemeEditorPanel.tsx | Missing | | Claude #79305 (custom themes). | P2 |
| Import VS Code themes, search Open VSX | web/vscodeThemeImport.ts, openVsxThemes.ts | Missing | | | P2 |
| Host-published themes with background image; `theme set / clear / show` CLI | srv/environmentTheme.ts | Missing | | | P2 |
| Effort display style, environment identification artwork | ct/settings.ts | Missing | | | P2 |
| Math rendering toggle; keep composer context strip visible | ct/settings.ts | Missing | | | P2 |
| Translations (de, es, fr, ja, ko, pt-BR, ru, sv, zh) and "system" language | web/i18n/locales.ts | Missing | | | P2 |

### Notifications

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| OS notification when a thread needs input or finishes (click opens the thread) | web/components/ThreadNotificationCoordinator.tsx:55-120 | Missing | | Promised by docs/DESIGN.md. #780 (64 reactions). | P0 |
| Notification when a turn fails | (missing on T3 desktop, §1 D bugs) | Missing | | T3 desktop has only input and completion kinds. | P0 |
| Notification sounds (completion, input) | web/threadNotifications.ts | Missing | | | P2 |
| In-app toasts for thread events, closed when the thread is opened | web/components/ThreadNotificationCoordinator.tsx | Missing | app/app_state.rs:162 | Toasts exist for command errors only. | P1 |
| Suppression (archived, snoozed, hidden tags, first sighting) and de-duplication per request | web/components/ThreadNotificationCoordinator.logic.ts | Missing | | | P1 |
| Dock / taskbar badge with the pending count, cleared on focus | `desktop:set-notification-badge` | Partial | app/shell.rs:300-315, app/sidebar.rs:450 | In-window "n need you" pill and Inbox badge only. | P1 |
| Mobile push, Live Activities, widgets | srv/relay/AgentAwarenessRelay.ts | N/A | | Mobile app over T3 Connect. | P2 |

### Usage and limits

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Usage page across providers (tokens, cache savings, model breakdown, cost, periods, environment filter) | web/components/usage/UsagePage.tsx, srv/usage/UsageService.ts | Missing | | #228; Claude #102264 (110 reactions, usage panel). | P1 |
| Model price overrides and "Map to" aliases | web/components/usage/UsagePriceOverrides.tsx | Missing | | | P2 |
| Tokens and cost per turn and per thread | (thread details) | Done | app/thread/rows.rs:356-364, app/thread/mod.rs:256, app/fleet.rs:95 | | P0 |
| Limits per account (session, weekly, other windows; reset times) | web/components/usage/UsageLimitsPooled.tsx | Partial | app/shell.rs:270-325, core/provider/claude/parser.rs:939, core/provider/codex/session.rs:772, :1080 | Title bar shows the most-used window from provider events, never by re-probing (T3 #19807 blanked accounts on 429s). | P1 |
| Banked "Use reset" credits | srv/usage/UsageLimitSources.ts | Missing | | | P2 |
| External limit sources (cliproxy, HTTP hubs) | web/components/settings/AddUsageLimitSourceDialog.tsx | Missing | | | P2 |

### Remote access, auth, sharing

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| T3 Connect (account sign-in and hosted relay; free tier of 3 machines) | srv/cloud/* | N/A | | Hosted T3 cloud and accounts. | P2 |
| Require approval for new devices (Connect device codes) | web/components/settings/ConnectDeviceApprovalSettings.tsx | N/A | | Part of T3 Connect. | P2 |
| Connection troubleshooting | web/components/ConnectionDiagnosisDetails.tsx | Partial | app/sidebar.rs:531-545 | Status pill with the disconnect reason. | P2 |
| Direct connection to a remote host with a token | srv/auth/*, web/routes/pair.tsx | Partial | orbitd/src/main.rs:1-60, app/main.rs:33-40 | Works through ORBIT_HOST / ORBIT_TOKEN env vars; no in-app UI. | P1 |
| One-time pairing link or QR, `pair` CLI | web/routes/pair.tsx | Missing | | Links survive leaving the page (T3 loses them, §1 D bugs). | P1 |
| Authorised clients with revoke; scoped tokens | web/components/settings/AuthorizedClientsSettings.tsx | Missing | | No hard 30-day expiry (#33183, #42190). | P2 |
| Reconnect without losing events | client-runtime connection layer | Done | net/client.rs:66-172, core/engine/bus.rs:1-17 | Replays from the last sequence; never a blank state. T3 flaps on lossy links (#3054, #2366). | P1 |
| Several hosts in one window | client-runtime | Missing | net/lib.rs:1 | One backend per window. | P1 |
| Saved machines with ordered routes (LAN, Tailscale, VPN, SSH), re-probed on network change | web/components/settings/ConnectionsSettings.tsx | Missing | | | P2 |
| SSH hosts (install runtime, port-forward, reuse ControlMaster) | desk/ssh/DesktopSshEnvironment.ts | Missing | | Codex #10450 (879 reactions, remote development). | P1 |
| WSL backend (run in a distro, install automatically, WSL-only mode) | desk/wsl/* | Partial | orbitd/src/main.rs:1 | Manual. | P1 |
| Load balancing across machines | web/components/settings/LoadBalancingSettings.tsx | Missing | | | P2 |
| Tailscale HTTPS serve | packages/tailscale | Missing | | | P2 |
| Linked environments (delegation between machines, read-only linked threads) | srv/peer/* | Missing | | | P2 |
| Guest links (share one thread with a browser guest) | srv/guest/GuestShareService.ts | N/A | | Served through T3 Connect's hosted address. | P2 |
| Secrets (1Password via `op`, secret references in credential fields) | srv/secrets/* | Missing | | | P2 |
| Background service (systemd, launchd, Windows service) | srv/serviceLauncher.ts | Missing | | One owner per data folder (T3 #22820 service vs local backend confusion). | P2 |
| Devices panel (iOS Simulator, Android Emulator streaming) | srv/device/* | Missing | | | P2 |
| Protocol mismatch notice | web/components/ServerUpdateAction.tsx | Done | net/server.rs:94-96, proto/lib.rs:33 | Refused with a readable reason. | P2 |

### Telemetry, updates, diagnostics, maintenance

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Product telemetry to `ingest.t3.codes` | srv/telemetry/AnalyticsService.ts | N/A | app/settings.rs:131 | T3's hosted analytics. Orbit has none (T3 #4123, 46 reactions). | P2 |
| Relay traces and mobile crash reports | srv/telemetry/* | N/A | | Hosted T3 services and mobile app. | P2 |
| Agent feedback modes (send to the T3 team) | ct/settings.ts (`agentFeedback.mode`) | N/A | | Hosted T3 feedback. | P2 |
| User OTLP export (traces, metrics, logs, Test) | web/components/settings/TelemetryExportSettings.tsx | Missing | | Opt-in, local endpoint only. | P2 |
| One update flow for providers, hosts and the app (progress, retry, Later) | web/components/ServerUpdateAction.tsx | Missing | | | P2 |
| Update channels (Stable, Nightly) | desk/updates/updateChannels.ts | Missing | | Quit fully before replacing files (#25450). | P2 |
| Storage maintenance (Delete now, last-run stats) | web/components/settings/StorageSettings.tsx | Partial | core/store.rs:386-452 | Logs compact and blobs are collected automatically; no UI. | P2 |
| Diagnostics (process tree with CPU and memory, signals, resource history, open logs, copy trace id) | web/components/settings/DiagnosticsSettings.tsx | Partial | app/settings.rs:107-126 | Data folder path and "Show in Explorer". | P2 |
| About (version, licences, privacy) | Settings → About | Missing | | | P2 |

### Desktop app

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Single instance per data folder, deep links (`t3code://`) | desk/electron/ElectronProtocol.ts | Missing | | A data-folder lock prevents two engines on one store (#22983, #14115, #6097). | P1 |
| Native app menu (About, Settings, Edit with Paste as text, View zoom, Window) | desk/window/DesktopApplicationMenu.ts | Missing | | | P2 |
| Quitting stops agents and the local engine | desk/window/DesktopWindow.ts | Done | app/main.rs:58-63, core/engine/orchestrator.rs:422 | Every session gets Shutdown; Job Objects kill descendants (core/provider/process.rs:1-12). | P1 |
| PATH for provider CLIs (login-shell environment capture) | desk/shell/DesktopShellEnvironment.ts | Partial | core/provider/process.rs:1-12, core/provider/claude.rs:82-104 | PATH, PATHEXT and known install folders; no login-shell capture on macOS / Linux GUI launches. | P1 |
| Install CLI / add to PATH | desk/ipc/channels.ts | Missing | | | P2 |
| Interface zoom | desk/window/DesktopApplicationMenu.ts | Partial | app/theme.rs:36-38 | One base size drives every rem; no zoom command. | P2 |

### Mobile

| Feature | T3 reference | Orbit status | Orbit location | Better than T3 | Pri |
|---|---|---|---|---|---|
| Mobile app (pairing, threads, composer, voice, diffs, terminal, PRs, usage) | apps/mobile | N/A | | Mobile app. A later web client served by orbitd would cover approvals on the go. | P2 |
| Push notifications, Live Activities, widgets | apps/mobile/modules/* | N/A | | Mobile app via T3 Connect. | P2 |
| Hardware-keyboard shortcuts on iPad and Android | apps/mobile | N/A | | Mobile app. | P2 |

## E. Beyond T3: promises in docs/DESIGN.md

Not T3 features, so they are not in the counts above, but the design commits to them.

| Feature | Orbit status | Orbit location | Note |
|---|---|---|---|
| Inbox: every request, failure, quota wait and result across threads, answerable in place | Done | app/inbox.rs:1-181 | Questions and plans still open the thread. |
| Fleet status board | Done | app/fleet.rs:1-147 | No subagent nesting, branch, diff size or conflicts columns yet. |
| Per-file revert | Done | app/review.rs:79-100 | |
| Per-hunk revert | Done | app/review/mod.rs (`revert_hunk`), proto/diff.rs (`invert_patch`) | From each hunk's marker; every hunk or file revert can be undone. |
| Conflict detection between live worktrees | Done | core/engine/review.rs (`schedule_conflicts`) | Emitted when overlap changes; shown in the review sheet, its tree and the merge dialog. |
| Unique port block per worktree (`PORT`, `ORBIT_PORT_BASE`) | Partial | core/git/worktree.rs:101, core/engine/orchestrator.rs:890-892 | Exported when set; never set without worktrees. |
| `.worktreeinclude` copies `.env` and untracked files | Partial | core/git/worktree.rs:5-7 | Library only. |
| Worktree disk usage | Partial | core/git/worktree.rs:412 | Library only. |
| Merge locally without a PR | Done | core/git/merge.rs, core/engine/review.rs, app/review/dialogs.rs (`merge`) | Preview with `merge-tree` (no side effects), conflicts refused before anything is committed, the target's checkout follows. |
| Undo a checkpoint restore | Done | core/engine/review.rs | |

## F. T3 Code's known bugs: does Orbit avoid them?

"Avoided" means the current code (not just the design) prevents it. **Yes** = avoided, **Partly** = avoided in part or by design but not verified, **No** = Orbit has the same problem today, **Not yet** = the feature does not exist in Orbit, so the bug cannot occur yet; the note says what the implementation must do.

### F.1 Per-section "Bugs / fragile UX" lists (§1)

| T3 bug | Avoided? | Why |
|---|---|---|
| Cmd-click deselecting the anchor leaves Shift-range anchored on an unselected row (threadSelectionStore.ts:42) | Not yet | No multi-select. WP7 must move the anchor on deselect. |
| "Delete the worktree too?" second dialog: Cancel does not cancel the delete | Not yet | No worktrees. Use one dialog with a checkbox (WP2). |
| Agent stopped before the delete; a failed delete leaves it stopped | Partly | `delete_thread` shuts the session and emits the removal in one engine step (core/engine/orchestrator.rs:729-749); a failed log write is only logged, so the thread can reappear after restart with its agent stopped. |
| "Copy → Path" always enabled, fails with a toast when there is no path | Yes | Hidden when there is no path (app/thread_menu.rs:169). |
| "Copy → Thread" label mistranslation risk | Yes | Label says what it copies ("Copy thread ID"); no i18n yet. |
| Thread tags exist on the server with no UI | Not yet | No tags; when added, ship UI with them. |
| 3,800-line sidebar with ref-snapshot workarounds | Yes | Sidebar state is one entity (app/sidebar.rs, 545 lines) and the thread menu is one shared builder (app/thread_menu.rs). |
| Plan mode unreachable from the web composer | Yes | Plan toggle in the composer (app/composer.rs:307). |
| Long setup commands truncated with no wrap or tooltip | Partly | Approval cards wrap and scroll (app/thread/rows.rs:596); tool rows truncate titles with no tooltip (app/thread/rows.rs:456). |
| `/side` while side chat is disabled fails silently | Not yet | No slash menu. |
| Provider's raw `/clear` reappears and clears a session T3 thinks is live | Yes | `/clear`, `/new` and `/reset` start a fresh session in the same thread and never reach the agent (core/engine/sessions.rs). |
| `followUpBehavior` defaults to steer, which waits for tools on Claude | Yes | Enter queues by default; steering is explicit (app/composer.rs:108). |
| "Revert files too" disabled outside isolated worktrees without explanation | Yes | Restore works in any repository, names what changes and can be undone (app/review/dialogs.rs). |
| Slow workspace loses its turn baseline after a 5 s budget | Partly | No budget, so the baseline is never skipped; but the first prompt is held until the snapshot finishes with no timeout (core/engine/orchestrator.rs:933-947), so a pathological repo can hold the turn indefinitely. |
| Large diffs cut at 10 MB with only a `[truncated]` marker | Partly | No truncation; also no cap, so a huge diff is held in memory. |
| Diff-panel state leaks for archived threads | Yes | Review state is not persisted per thread. |
| Turn fallback depends on list order | Yes | Review asks for the turn by number (app/review.rs:40-47). |
| Hard limit of 4 terminals per split group | Not yet | No terminal UI. |
| Terminal paste before ready drops the text | Partly | Engine input goes through a writer thread per terminal (core/terminal.rs:7-10); not mounted in the UI. |
| PR and issue saved views do not sync | Not yet | Store them on the host. |
| Host gaps (GitCafe line comments, Bitbucket reopen, Azure comment edit) | Not yet | |
| A `when` typo silently makes a rule fire everywhere | Partly | GPUI key contexts are fixed in code; no user file to mistype yet. |
| Conflict warning only compares identical `when` strings | Not yet | |
| One invalid keybinding entry blocks all new defaults; errors never shown | Not yet | |
| Over-limit rule sets silently drop the oldest (the defaults) | Not yet | |
| New installs default to Full access | Yes | `Access::Ask` is the default (proto/model.rs:144). |
| No failed-turn notification on desktop | No | No OS notifications at all yet. |
| Pairing links lost when leaving the Settings page | Yes | The host token is stored in `<data>/token` and printable at any time (orbitd/src/main.rs:62). |

### F.2 Performance and lag (§5.1)

| T3 bug | Avoided? | Why |
|---|---|---|
| #4596 reopening a thread replays every event, re-deriving the timeline each time | Yes | A thread opens from its item log; feed batches are applied once per batch with one notify (app/app_state.rs:66-97). |
| #26018 long threads load and sync slowly | Yes | 3,000-item thread opens in 2.5 ms (docs/BENCHMARKS.md). |
| #20427 each history page re-reads the whole thread | Partly | No paging at all: the whole thread log is read on open (core/store.rs:341). Fine until very large threads. |
| #41148 IndexedDB read blocks thread open | Yes | No browser storage. |
| #25823 13k threads freeze the renderer | Partly | Rows are virtualised, but the sidebar and Fleet re-sort every thread on each shell change (app/sidebar.rs:149-152, app/fleet.rs:37-52); unmeasured at 13k. |
| #41083 O(rows × drafts) sidebar scan | Yes | No such scan. |
| #42338 palette cache misses per keystroke | Yes | Palette items built once per open (app/palette.rs:25-58). |
| #440, #12689 multi-second transitions and typing lag | Yes | Native GPUI; a token re-renders one row (app/thread/mod.rs:133-164). |
| #30657, #28489 skill and slash menus lag with hundreds of skills | Not yet | No menu. |
| #42438 diff scrolls at 28 fps | Yes | 802-file, 49,609-line review: normal wheel scrolling never drops below 60 fps (p99 13.6 ms); a 3× fling has p50 7–8 ms, p95 24 ms (docs/BENCHMARKS.md). |
| #996 crash on a heavy thread, fixed only by deleting `~/.t3` | Partly | CRC-framed logs truncate torn records on recovery (proto/lib.rs:13-15); no crash-loop guard. |
| #20366, #12372, #26338, #26385, #30515 scroll anchoring and "scroll to end" pill regressions | Partly | Message scroller follows the tail and appends without resetting (app/thread/mod.rs:204-219); no pill and no tests for scroll-up during a turn. |
| #12094 rows overlap while a tool group streams | Yes | Each updated row is remeasured (app/thread/mod.rs:149-161). |

### F.3 Server storage and event loop (§5.1)

| T3 bug | Avoided? | Why |
|---|---|---|
| #5110 one 10.5k-char reply created 3,024 durable events | Yes | Deltas are ephemeral (proto/wire.rs:78-88); a streaming item is rewritten at most every 2 s (core/engine/orchestrator.rs:47, :408-420). |
| #5719, #4008 every delta re-projected the whole thread | Yes | A delta appends to one string in one item (core/engine/orchestrator.rs:384-405). |
| #23365 every reasoning snapshot stored permanently | Yes | Same mechanism. |
| #21154 event table grows without bound | Yes | Logs compact when dead records outweigh live ones (core/store.rs:386-412). |
| #28903 33 GB database in 7 weeks | Yes | Per-thread logs, blobs by hash, compaction. |
| #17845 delete keeps every event | Yes | Delete removes the log, unreferenced blobs and checkpoint refs (core/engine/orchestrator.rs:729-749). |
| #30065, #15707, #41997 storage writes block the event loop | Partly | Writes are synchronous on the engine thread (core/engine/orchestrator.rs:349-361); the UI never blocks, but a slow disk delays every command and stream. |
| #23482, #24660, #38159 WAL and reader-pool stalls | Yes | No SQLite. |
| #26224 6–28 s freezes on a 5.9 GB DB | Yes | |
| #40987 request auth waits on a synced disk write | Yes | Token compared in memory (net/server.rs). |
| #35995 startup blocked 7 min by migrations | Partly | No migrations; startup replays the meta log (core/store.rs:193-211), which compaction keeps small. |
| #21289 git spawns slow down as server memory grows | Yes | Bounded async spawns through one semaphore (core/git/mod.rs:11-13, :228). |
| #16636 one stuck provider holds every effect slot | Yes | Each session is its own task; slow commands run as spawned futures (core/engine/mod.rs:120-130). |
| #695 slower than Codex for the same task | Partly | Direct app-server per thread with no extra orchestration layer; not measured. |

### F.4 Memory, CPU, GPU, battery (§5.2)

| T3 bug | Avoided? | Why |
|---|---|---|
| #2767, #29638 tens of GB after sleep or over time | Partly | Timelines are released when no view shows them (app/app_state.rs:113-122); but every thread that ever ran keeps its provider process until Orbit quits (no idle policy), so memory grows with thread count. |
| #25172 PR sync sweep retained 1.7 GB/h | Not yet | No PR sync. |
| #3143, #40581 high idle energy and GPU | Partly | No polling and no idle animation; the working spinner and "Thinking…" shimmer animate continuously while agents run (app/ui.rs:78-87, app/thread/rows.rs:221). |
| #4773 idle VCS / provider refresh loop | Yes | No timers except a one-per-minute sidebar repaint (app/sidebar.rs:115-122). |
| #14730, #37987, #18165 preview browser GPU and memory | Not yet | |
| #40809, #30759, #7766 blur and glass flicker | Yes | No blur effects. |
| #13496 PR details refetched on every focus | Not yet | |
| #5248 oomd kills the whole app scope | Partly | Each agent tree is its own process group or Job Object (core/provider/process.rs:4-8); no memory limits. |
| #20329 agent memory limits only on Linux | No | No memory limits. |

### F.5 Streaming, turn state and Stop (§5.3)

| T3 bug | Avoided? | Why |
|---|---|---|
| #27443, #27623, #27615, #27527, #27704, #4647 empty thread view / vanishing messages | Partly | Threads exist before the first send. Remaining race: `OpenThread`'s reply and the event feed are separate channels, and item events for a thread whose timeline is not loaded yet are dropped (net/state.rs:265-270), so an item written between the snapshot and its arrival can be missing until it is upserted again. Verify and fix in WP1. |
| #15197 stuck "running" after `result/success` | Yes | Outcome comes from `terminal_reason` / `subtype` (core/provider/claude/parser.rs:12-13); every terminal path calls `finish_turn`, including process exit (core/engine/orchestrator.rs:1385-1396). |
| #4713, #2234 Stop becomes a no-op | Yes | Interrupt with a 10 s deadline, then kill (core/provider/claude.rs:444, core/provider/codex/session.rs:473). |
| #15124 "Agent is working" while checkpoint capture stalls | Partly | Status goes idle at turn end; queued turns wait for the snapshot with no visible reason (core/engine/orchestrator.rs:967-970). |
| #14572 messages still marked streaming after a restart | No | Busy threads are reset to Idle (core/store.rs:213-217), but items saved as `Streaming` keep that status, so their rows shimmer forever. |
| #18805 queued message stuck after Stop | Partly | After Stop the queue continues automatically; after a restart the in-memory queue is gone and Queued items never send (core/engine/orchestrator.rs:90-91). |
| #41914 steered message will not send after cancel | Partly | A steer outside a turn becomes a new turn (core/engine/orchestrator.rs:824); Codex re-queues unsteerable prompts (core/provider/codex/session.rs:809). |
| #21895 steering on Claude cancels tools and reads as a refusal | Partly | Steers use `priority: "next"` (core/provider/claude.rs:651) instead of interrupting; not covered by a replay test of a running tool. |
| #41407 auto-sends wait behind later explicit messages | Not yet | |
| #8879, #41320, #5297 folding hides the real answer | Yes | Only the work log folds (app/thread/rows.rs:251-254). |
| #42028 two numbered lists merge | Partly | Depends on the Markdown parser; untested. |
| #21837 et al. delegated child finishes but the parent never wakes | Partly | Agent-started turns are tracked (core/engine/orchestrator.rs:1219-1231); no app-owned child threads yet. |
| #41890 interrupting the parent stops its background subagents | Yes | Interrupt keeps the Claude process and its background tasks (core/provider/claude.rs:10-13). |
| #25585 "done" shown at every intermediate stop | Partly | |

### F.6 Provider quirks (§5.4)

| T3 bug | Avoided? | Why |
|---|---|---|
| #7338 Claude usage 3–5× the native app | Yes | One long-lived CLI per thread; Stop interrupts instead of killing; no idle release; no injected system prompt (core/provider/claude.rs:4-13). |
| #2256 context lost after an hour idle | Yes | No idle release; sessions resume by id. |
| #24152 resumes the pre-`/clear` session | Yes | `/clear` drops the session id, rollback cursor and pending handoff (core/engine/sessions.rs). |
| #17467 Enter on a stale thread runs `/compact` first | Yes | Never compacts on its own. |
| #2121, #2034 compaction and context-window maths wrong | Partly | Context comes from the provider's own usage (core/engine/orchestrator.rs:1355-1362). |
| #15854 Stop before `system/init` bricks the thread | Partly | Different race: Stop while the baseline snapshot is still running marks the turn interrupted, but the held prompt is still sent when the snapshot lands (core/engine/orchestrator.rs:463-478). |
| #41331 runs interrupted at the first stream event; #40762 stall sweep counts sleep as silence | Yes | No silence-based sweep. |
| #41125 rollback before any reply fails | Yes | With no cursor, edit-and-resend starts a fresh session (core/engine/orchestrator.rs:1025-1031). |
| #16000 changing mode or instance kills live subagents | Partly | Model and permission mode change live; tool set, thinking and effort clearing restart, but only between turns (core/provider/claude.rs:4-9). |
| #40590 Windows crash with exit code 4294967295 | Partly | stderr tail is kept and shown (core/provider/process.rs:9-10). |
| #23151 MCP setup timeout shorter than SessionStart hooks | Partly | 60 s init timeout (core/provider/claude.rs:39) can still be shorter than slow hooks. |
| #40549, #40083 MCP servers start unapproved; sign-in notice per plugin | Not yet | |
| #2653, #41195, #40466 false "not authenticated" | Partly | Signed-in state comes from the CLI's own `initialize` (core/provider/claude.rs:661). |
| #15353 Auto mode runs ask-rule commands | Not yet | No Auto mode. |
| #16502, #17045 shared Codex app-server dies and breaks every thread | Yes | One supervised app-server per thread with restart and resume (core/provider/codex.rs:4-28). |
| #8259, #30630 "already has an active writer" on instance or account switch | Yes | The old session is shut down before the new instance starts, each account with its own `CODEX_HOME`. |
| #42123 idle release makes the next turn take 20–60 s | Yes | No idle release (the trade-off is memory, see F.4). |
| #13987, #37298 sandbox and approval mapping errors | Partly | Mapping is pure and unit-tested (core/provider/codex/map.rs:1-2, :186); not checked against these cases. |
| #42142, #41249, #41250 connection resets, capacity reconnect, stale model on retry | Partly | Restart with back-off, at most 3 per 5 minutes (core/provider/codex/session.rs:24-56). |
| #234, #269 CLI not found on PATH (Homebrew, Windows) | Yes | PATH, PATHEXT and known install folders (core/provider/process.rs:11-12). |
| #27230 provider check times out after 20 s | Partly | Same 20 s probe timeout (core/provider/claude.rs:37). |
| #40696 wrong binary gives an opaque failure | Partly | stderr tail in the message. |
| #41799, #40529 Codex text generation runs notify hooks / fails with `mcp_servers` | Not yet | No text generation. |
| OpenCode, Cursor and ACP-family issues (#2691, #11458, #40503, #19112, #36105, #38123 …) | Partly | ACP cancel waits for the acknowledgement; OpenCode runs one server per session with its own password. Not checked against each issue. |

### F.7 Windows (§5.5)

| T3 bug | Avoided? | Why |
|---|---|---|
| #2537 console windows flash | Yes | `CREATE_NO_WINDOW` everywhere (core/git/mod.rs:19-20, core/provider/process.rs:3-4). |
| #980, #4182, #12689 "Not responding", CPU, typing lag | Yes | Native UI; engine on its own thread; no polling. |
| #9650 `_MEI*` folders left in `%TEMP%` | Yes | No PyInstaller helpers. |
| #8341, #15001, #4750 corrupt or undecryptable state files brick the app | Yes | No encrypted catalog; logs are append-only with CRC frames; compaction and blobs write a temp file and rename (core/store.rs:96-112, :436-438). |
| #25450 update fails while the old process lives on | Not yet | No updater. |
| #192, #14393, #13738, #19735, #17090 WSL support and connection | Partly | orbitd can run in WSL by hand. |
| #40498 duplicate `git log` per turn diff | Yes | One diff per tree pair, cached and shared (core/git/mod.rs:14-16). |
| #731 Ctrl+W does not close the terminal | Not yet | |
| #2115 approval stuck "Pending" | Partly | Request cards settle on answer, withdrawal and turn end (core/engine/orchestrator.rs:1067-1087, :1180). |
| Claude adapter drops stderr on win32 and has no tree kill | Yes | stderr drained; Job Object with kill-on-close (core/provider/process.rs:4-10). |
| Codex `.cmd` shim run through a shell; killing the wrapper orphans `codex.exe` | Yes | Shims resolved through PATHEXT; the tree is in a Job Object. |
| `PATH` and `Path` both reach the child | Partly | Git env is scrubbed (core/git/cmd.rs:13); provider env merging not verified. |

### F.8 Terminal (§5.6)

| T3 bug | Avoided? | Why |
|---|---|---|
| #13046 `@` cannot be typed on a German layout | Yes | Text comes through platform input (term/keys.rs:1-6). |
| #6362 Escape does not reach nvim; #731 Ctrl+W | Partly | Encoder sends them; app key contexts not wired yet. |
| #6058 no reply to device-attribute queries (fish stalls 10 s) | Yes | The view answers DA, DSR and colour queries (term/lib.rs:17-20). |
| #8574 raw mouse sequences shown | Yes | Mouse reported only in modes the program requested. |
| #19536, #16114 toolbar covers line one; selection spawns menus | Not yet | |
| #24154 "cwd does not exist" after cleanup removed a worktree | Not yet | Recreate the worktree before opening (WP2). |

### F.9 Git, worktrees, checkpoints (§5.7)

| T3 bug | Avoided? | Why |
|---|---|---|
| #26227, #3646 capture hits a 30 s timeout, retries forever, leaves temp object dirs | Yes | Persistent private index, no temp object dir, no kill-and-retry (core/git/checkpoint.rs:1-11). |
| #38102 refs never pruned, no size guard, disk filled | Partly | 50 MB untracked-file cap (core/git/checkpoint.rs:221). The engine records bare tree ids from `snapshot()` instead of pinned `checkpoint()` refs, so there is no ref growth but `git gc` can delete old turns' trees; `prune` exists but is never called. |
| #21328 racily clean worktrees rehash everything | Yes | Index stat cache persists between snapshots. |
| #15295, #585 "Edit from here" missing after a checkpoint race | Yes | Edit is offered whenever the thread is idle, independent of checkpoints (app/thread/rows.rs:161). |
| #17187 rollback hidden in non-git workspaces | Yes | Chat rewind needs no git. |
| #41219 rollback past a question leaves its card stuck | Yes | Every item after the edited message is removed (core/engine/orchestrator.rs:1015-1017). |
| Codex rollback counts T3 turns but reverts native turns | Yes | Rollback uses Codex's own turn id as the cursor (core/provider/codex.rs:44-46). |
| #17813 cleanup misses worktrees whose branch was renamed | Partly | `remove_worktree` deletes the current branch (core/git/worktree.rs:13-15); not wired. |
| #13836, #11735, #19124, #11078, #272, #40911, #42402 worktree leaks, orphans, stale labels, hard-coded prefix | Not yet | |
| #4022 diff scopes empty in packaged builds (path guard) | Yes | No home-directory path guard. |
| #10949 staged files hidden | Yes | Snapshots include the index and untracked files. |
| #1194 commit timeout | Partly | Bounded process runner; no UI yet. |
| #4970 reused branch stays tied to an old merged PR | Not yet | |
| #16183 schedules always branch from main | Not yet | |

### F.10 Connection, remote, process ownership (§5.8)

| T3 bug | Avoided? | Why |
|---|---|---|
| #2366, #3054, #3553, #4589 reconnect flapping, silent stalls | Yes | Back-off reconnect with replay from the last sequence (net/client.rs:66-172); the engine never stalls on storage locks. |
| #16810, #3746 relay 504s | Not applicable | No relay (T3 Connect is N/A). |
| #33183, #42190 sessions expire after a hard-coded 30 days | Partly | The token never expires, but it cannot be revoked or scoped either. |
| #22983, #6097, #14115, #2614 several servers on one data folder resume each other's threads | No | No data-folder lock: the app and `orbitd` can open the same store at once. |
| #22820 confusion between background service and local backend | Not yet | |

### F.11 Approvals and permissions (§5.9)

| T3 bug | Avoided? | Why |
|---|---|---|
| #42390 file-edit approvals do not show the change | Yes | Diff preview in the card (app/thread/rows.rs:502). |
| #42025 ACP approval says "Changed files" without naming them | Yes | Paths come from diffs, `locations` and `rawInput`, merged across updates (core/provider/acp/map.rs). |
| #15082, #22925 child approvals not surfaced to the parent | Partly | Claude subagent and background approvals appear in the thread; no child threads yet. |
| #21935 child approvals stay pending after cancel | Yes | Turn end clears pending requests; withdrawals settle cards (core/engine/orchestrator.rs:1180, :1345-1354). |
| #856 plan questions stuck after restart | No | Pending requests persist on the thread record, but after a restart there is no session to answer, so the card stays and answering fails. |
| #14878 Claude MCP elicitation declined without a prompt | Yes | Elicitations are routed to the user (core/provider/codex/map.rs:186, core/provider/codex/session.rs:679). |
| #28110, #29966 side chats ignore the parent's permission mode | Not yet | |
| #41235 CLI thread start ignores the default mode | Not yet | |
| #41220 messages accepted on deleted or archived threads | No | Deleted: rejected. Archived: accepted and run (core/engine/orchestrator.rs:811-853). |
| Claude "always allow" without a suggestion allows the whole tool | Yes | Session rules are scoped to the command or file (core/provider/claude/parser.rs:14-15, :1404). |
| ACP "for session" silently degrades to once | Yes | Without an `allow_always` option the grant is kept client-side for the session (core/provider/acp/session.rs). |
| Codex approval card pending after `serverRequest/resolved` | Yes | Handled as a withdrawal (core/provider/codex/session.rs:1105-1114). |

### F.12 Usage and limits (§5.10)

| T3 bug | Avoided? | Why |
|---|---|---|
| #19807 limits hub blanks accounts by re-probing on every refresh | Yes | Quota comes from events the provider pushes (core/engine/orchestrator.rs:1363-1374). |
| #14169 reset tracker broken | Partly | Reset time is stored and shown; untested across time zones. |
| #40483 threads still shown failed after the limit resets | Partly | Rate-limited threads return to idle at reset (core/engine/orchestrator.rs:514-524); a separate Failed status stays until the next turn. |
| #41583 a subagent finishing cancels the parent's auto-resume | Partly | Auto-resume only fires if the thread is still rate-limited. |
| #9352, #33639 context indicator hidden by default | Yes | Always visible (app/ui.rs:97-99). |

### F.13 Consolidated code-level bugs (§5.12)

| T3 bug | Avoided? | Why |
|---|---|---|
| Codex: busy-wait turn lookup on the stdout reader; foreign `turn/started` adopted; shared 32-handler cap | Yes | Reader never waits inline; one app-server per thread (core/provider/codex/rpc.rs:1-9). |
| Codex: unhandled `item/tool/call`; naive version regex | Partly | Not verified. |
| Claude: private SDK internals; outcome from error substrings; out-of-turn requests auto-denied; `input_json_delta` ignored; O(projects) stats per turn | Yes | Own protocol implementation (core/provider/claude/parser.rs:1-19). |
| Claude: recorded fixtures older than the pinned SDK | Partly | Replay fixtures exist (crates/orbit-core/tests/fixtures/claude); no check that they match the installed CLI version. |
| Orchestration: full-entity snapshot events | Partly | Items are rows, not thread snapshots, but every thread change emits the whole `Thread` record, including its pending requests (core/engine/orchestrator.rs:363-371). |
| Orchestration: every subscriber filters one global stream; shell re-queried per subscriber | Partly | One bounded channel per subscriber with every event (core/engine/bus.rs:1-6); fine locally, costly with many remote clients. |
| Orchestration: an effect that never settles blocks its lane | Yes | Slow commands are spawned futures; nothing waits on them in the engine loop. |
| UI: delete-worktree dialog, stop-before-delete, `when` typos, keybinding errors, 3,800-line sidebar | See F.1 | |
| Plan mode unreachable; no "implement plan" | Yes | app/composer.rs:307, app/thread/rows.rs:550-559. |

### F.14 Orbit-specific defects found during this audit

Not T3 bugs, but found while verifying the rows above. All belong in WP1 unless noted.

1. **Checkpoint trees can be garbage-collected.** The engine stores `git.snapshot()` tree ids (core/engine/orchestrator.rs:939, :1150), which are not pinned by any ref; `git gc` may drop them after the grace period, breaking older turn diffs and restores. Use `checkpoint()` (pinned refs) and call `prune`.
2. **Restart loses work in flight.** The run queue is in memory (core/engine/orchestrator.rs:90-91), items saved as `Streaming` stay streaming, pending requests stay pending with no session, and snooze timers are not re-armed (core/engine/orchestrator.rs:712-719). Reconcile all four on start.
3. **Stop during the baseline snapshot still sends the prompt** (core/engine/orchestrator.rs:463-478): check `turn.interrupted` before releasing `held_prompt`.
4. ~~**Provider switch on a live session** sends `Configure` to the old provider's adapter.~~ Fixed in WP9: the session ends and the context is handed off (core/engine/sessions.rs).
5. **No data-folder lock**: two engines can write one store.
6. **Archived threads accept messages** (core/engine/orchestrator.rs:811).
7. **Preferences are never saved** (app/main.rs:57, app/settings.rs:15) (WP8).
8. **Every thread keeps its provider process until quit**: no idle policy, so memory grows with the number of threads that ever ran (WP9).
9. **Possible timeline-open race** between the `OpenThread` reply and the event feed (net/state.rs:265-270, app/app_state.rs:124-139).
10. **Thread branch never shown**: `workspace.branch` is only set for worktrees (WP2).
11. **Remove project deletes every thread without confirmation** at engine level (core/engine/orchestrator.rs:555-563) (WP8 UI must confirm).
12. **macOS gets Ctrl bindings** instead of Cmd (app/actions.rs:28-41) (WP7).
13. **Next / previous thread follows project order**, not the sidebar's visible order (app/shell.rs:226-245) (WP7).
