# Agent-orchestrator GUIs: interface patterns, and what Orbit should build (2026-11-25)

**Scope.** This report covers concrete interface and interaction design: layout, how parallel agents are shown, status and attention, the composer and model picker, diff review, keyboard flows, motion, onboarding and empty states. It also covers features missing from `docs/research/orchestrator-feedback.md` (the "feedback report"). Complaints and requests already listed there are not repeated. When a recommendation overlaps an item in the feedback report, it is included only because it adds a substantially more specific design, and it names the feedback item it supersedes.

**Sources.** Official docs, changelogs, READMEs and source trees, read between 2026-11-24 and 2026-11-25. Every bullet ends with its source. The full Orca inventory, about 500 features with reimplementation detail, is in `research/orca.md`.

**Name changes and status as of Nov 2026, so the citations make sense:**
- Superset is now **Paradev**.
- Emdash's open-source app is now **Emdash Classic**; new work goes to a cloud "Emdash 2.0".
- Crystal became **Nimbalyst**.
- Codex became a mode of the **ChatGPT desktop app** in July 2026.
- **Vibe Kanban** is sunsetting.
- **Synthcoda** is marked discontinued.
- **Conductor** has a changelog entry titled "Conductor is joining Meta".
- **Doithoo/gooey** returns 404. A different, active app, **GooeyPi** (am-will/gooey-pi), is covered in its place.

---

## 1. Per-tool patterns

### PandaCode (aicodeking/pandacode; Tauri 2 + Rust)
- **Layout.** Three columns: a task sidebar, the thread with its composer, and a collapsible inspector. The inspector has a VS Code-style icon bar (Files, Search, Changes, Browser, PR, Simulators, VM). A floating **action dock** (Panels, Agents, Run, Deploy) sits over the thread and takes no layout space. The view menu switches between Conversation, Board and GitHub. https://github.com/aicodeking/pandacode/blob/main/docs/design.md
- **Tiles.** Up to **six threads** run side by side. Add one by dragging a thread from the sidebar, ⌘-clicking it, or choosing "Open beside". The focused tile shows the full thread and the others keep updating live. ⌘⌥←/→ moves between tiles. https://github.com/aicodeking/pandacode
- **Board.** Four columns: Pending → In progress → Review → Settled, in "quiet stage colors". A right panel shows one task at a time. Errors stay In progress with a retry button. There is no archive column. https://github.com/aicodeking/pandacode/blob/main/docs/task-board.md
- **Turn cards.**
  - One card per turn. The header shows the step currently running and the elapsed time. The latest reply has a "Read more" fold, and changed files in the footer open the diff.
  - Tool calls collapse under "Worked for …", in groups of at most five. Each group shows a call count, categories, and running/failed counts.
  - https://github.com/aicodeking/pandacode/blob/main/docs/conversation-presentation.md
- **Approvals.** These are one-line rows in a slim bar above the prompt, with **Approve all** when several are pending. https://github.com/aicodeking/pandacode
- **Composer.**
  - Model picker: searchable, with agent harnesses on the left, then models with their context sizes, then an effort control.
  - While an agent runs, messages can be queued or used to steer.
  - `/btw` opens side threads in a "By the way" tab.
  - The empty state reads "What should we build in ‹project›?", and the project name is itself the picker.
  - https://github.com/aicodeking/pandacode/blob/main/docs/design.md
- **Agent teams.** The composer's **Team** button offers automatic routing (Cheap/Balanced/Best) or "Classic team setup": an architect model plus worker profiles.
  - **Workers panel:** a tally ("3 working · 2 done") above a progress bar, one row per worker sorted by status, and sub-workers indented under their parent.
  - **Waiting is explained.** A queued worker shows why: a dependency, no free slot, or a file-scope conflict. A parent that is waiting shows "Waiting on sub-workers".
  - **Shared memory:** `pandacode_team_notes` is a blackboard for API shapes and decisions. New briefs include the newest notes plus "N more notes".
  - **Interrupts:** `urgent: true` messages steer a running worker at most once per minute.
  - **Council mode:** read-only reviewers score the work out of 10, and rounds repeat until the lowest score reaches the target.
  - https://github.com/aicodeking/pandacode/blob/main/docs/architect-workers.md
- **Bots (Telegram, Slack, Discord, WhatsApp, local web).** Each chat is an ordinary task thread.
  - Approve by replying `yes`, `no` or `always`. Commands: `/stop` (kill switch), `/new`, `/help`.
  - An unknown sender gets a 4-digit pairing code, which the owner approves in the app.
  - A **Guard** always asks before risky actions (sudo, force push, rm -rf, anything involving money), even when a broad allow rule would cover them.
  - Sidebar groups: Needs you / Pinned / Bots / Hidden.
  - Slack setup uses a prefilled app manifest.
  - In shared `#channels`, an `@mention` routes work to a specific bot.
  - https://github.com/aicodeking/pandacode/blob/main/docs/bots.md
- **Motion and delight.**
  - Menus fade from their trigger with a small translate and scale over 170 ms and dismiss in 110 ms. With reduced motion they only fade.
  - A global quick launcher opens with ⌘/Ctrl+⇧+Space.
  - Eight procedural "live scenes" (aurora, dunes and more) follow the local clock, and each tile keeps its own scene.
  - A picture-in-picture "visual progress" timeline of screenshots.
  - A menu-bar list of tasks with plan usage.
  - https://github.com/aicodeking/pandacode

### Conductor (conductor.build)
- **The loop.** "The workspace is the unit of delegation." The cycle is: create (from a branch, PR, GitHub issue or Linear issue) → verify (Run / Spotlight) → Diff (⌘⇧D) → Checks tab (git, PR, CI, deployments, threads, todos) → PR (⌘⇧P) → merge (⌘⇧M) → archive, with the chat restorable from History. https://www.conductor.build/docs/concepts/workflow
- **Diff.**
  - **Mark as viewed** resets to "Ready for review" when the file changes again. https://www.conductor.build/changelog/0.29.1-customize-prompts-mark-files-as-viewed
  - Claude can leave comments on the diff, and draft comments survive restarts. https://www.conductor.build/changelog/0.29.0-claude-can-now-comment-on-your-code
  - Files can be edited inside the diff (0.84). https://www.conductor.build/changelog/0.84.0-edit-files-directly-in-diffs
  - Review, Create PR, Fix errors and Resolve conflicts use prompts you can customize per repo. https://www.conductor.build/changelog/0.44.0-new-sidebar-rebuilt-composer-codex-checkpoints
- **Checkpoints.** Revert the chat to any earlier turn, and the files roll back with it. Checkpoints are stored as private git refs. https://conductor.build/docs/reference/checkpoints
- **Sidebar.**
  - Workspaces sit in groups (Backlog, In Progress, In Review, Done), and you change a workspace's status by dragging it.
  - Rows show merged / failing CI / conflict / needs-permission.
  - A pencil icon marks an unsent draft.
  - Rows animate when they reorder.
  - Hovering a row shows its metadata and next actions.
  - Clicking the workspace that is already selected jumps to its oldest unread chat.
  - https://www.conductor.build/changelog/0.44.0-new-sidebar-rebuilt-composer-codex-checkpoints , https://www.conductor.build/changelog/0.95.0-agents-that-manage-your-local-workspaces
- **Composer.**
  - A **context dial**; hovering it shows a breakdown.
  - **Loadouts** of favourite models: ⌃⌘1–5 switches model, ⌘⇧/ sets effort, ⌘⇧E toggles Fast.
  - A toast warns that switching model mid-chat will make the next response slower.
  - Queued messages say *why* they are queued.
  - Selecting text and pressing ⌘L quotes it into the composer.
  - https://www.conductor.build/changelog/0.85.0-sections-routines-and-a-new-model-picker
- **Keys.**
  - j/k navigate the chat, and [ / ] toggle the sidebars.
  - Zen mode hides both sidebars.
  - ⌘⇧T is Big Terminal Mode.
  - The shortcut search lets you press a key combination to find what it does.
  - https://www.conductor.build/changelog/0.48.0-big-terminal-mode
- **Delight.**
  - Workspaces get city names, and a "passport" counts the cities you have visited.
  - Completion sounds include an SNCF jingle, a Paris Métro chime and a "Fahhhh".
  - Toasts show a countdown ring.
  - Error toasts have a Copy button.
  - Colorblind palettes.
  - https://www.conductor.build/changelog/0.52.0-sounds-colors , https://www.conductor.build/changelog/0.73.0-pr-page

### Superset → Paradev (superset.sh → paradev.com)
- **Layout (2.0.11).**
  - An arrangeable navigation rail with **one quota ring per provider** at its foot; hovering a ring shows its reset time.
  - Workspace tabs live in the title bar and shrink before they start scrolling.
  - Each pane area is a card.
  - New-workspace page: the prompt box is centered, with its pickers above it.
  - https://github.com/superset-sh/superset/blob/main/apps/marketing/content/changelog/2026-11-22-new-layout-mobile-home-skills.mdx
- **Status colors.**
  - Amber means working, yellow (red before Aug 2026) means needs permission, and green means ready for review.
  - Clicking acknowledges: ready-for-review goes to idle, and permission goes back to working.
  - PR icon colors: green = open, violet = merged, red = closed, muted = draft.
  - https://github.com/superset-sh/superset/blob/main/apps/marketing/content/changelog/2026-01-06-sidebar-workspaces-status.mdx
- **Activity strip.** Each row has chips for running agents and open ports. Hovering a chip lists them with stop buttons, plus a "stop all" action. Agent chips only appear when more than one agent is running. https://github.com/superset-sh/superset/blob/main/apps/docs/content/docs/agent-status.mdx
- **Workspaces page.** Views: All / Needs me / Working, with saved views and a List or Board display. Board lanes: Idle / Working / Needs attention / Needs review / Merged / Deleted. A deleted workspace keeps its card and can be restored. https://github.com/superset-sh/superset/blob/main/apps/docs/content/docs/workspaces.mdx
- **Ship pill.** A diff-stats pill in the top bar walks **Commit → Push → Create PR**, then turns into the PR badge.
  - It also covers image before/after diffs.
  - **Focus mode** shows one file at a time with Prev/Next, a section dropdown, and a "2/5 in Staged" counter.
  - https://github.com/superset-sh/superset/blob/main/apps/marketing/content/changelog/2026-09-06-changes-pane-ship-flow-image-diffs.mdx , https://github.com/superset-sh/superset/blob/main/apps/docs/content/docs/diff-viewer.mdx
- **Workspace creation and shortcuts.**
  - A workspace is prompt-first: it opens immediately and is named in the background from the prompt.
  - A CLI/Chat toggle switches a running session between views.
  - A Scripts bar launches scripts with Ctrl+1–9.
  - Sidebar rows show keycap-style shortcut hints.
  - https://github.com/superset-sh/superset/blob/main/apps/docs/content/docs/chat.mdx

### Emdash Classic (generalaction/emdash)
- **Empty state.** It lists the ways to create a task, with an **Enter keycap** on the selected one. https://github.com/generalaction/emdash/blob/main/apps/emdash-desktop/src/core/features/projects/browser/components/task-view/task-list-empty-state.tsx
- **Best-of-N (legacy).** A "Runs per provider" field creates N worktrees labeled "Claude Code #1", "#2" and so on. https://github.com/generalaction/emdash/pull/362
- **Auto-promoting kanban (legacy).** A task moves to In-progress on activity, and to Ready when it goes idle, when a PR exists, or when its branch is ahead. https://github.com/generalaction/emdash/pull/265
- **Status.** A spinner means working, and a **blue filled dot** means unread (waiting, complete or error). An optional app badge counts unread tasks. A sound profile picker includes the "Gilfoyle Bitcoin Alert". https://github.com/generalaction/emdash/pull/1608
- **Chat.** Consecutive tool calls collapse into "Ran N commands". An issue pill lets you edit the issue context before the first message is sent. Shortcuts: ⌘⇧1/2/3 open the panels, and ⌘⌥↑/↓ move to the previous/next task. https://emdash.com/changelog , https://emdash.com/docs/keyboard-shortcuts

### Nimbalyst (formerly Crystal)
- **Attention.**
  - Rail badges: orange = waiting on you, green = running, blue = unread.
  - A shortcut jumps through unanswered questions.
  - Answering "Always" on one prompt resolves the matching prompts in other sessions.
  - A thin context bar sits under each session.
  - https://nimbalyst.com/changelog/
- **Fleet surfaces.**
  - A macOS menu-bar fleet panel shows starting, finished, blocked, failed and unresponsive sessions.
  - An **iPhone Live Activity / Dynamic Island** ranks sessions by how long they have waited on you.
  - A ⌘⇧K board (Backlog → Planning → Implementing → Validating → Complete) shows a live transcript peek when you hover a card.
  - https://nimbalyst.com/docs/session-management/agent-window-and-session-management/
- **Document diffs.** Edits are written to disk first and reviewed afterwards. A review bar with a change counter offers Keep or Revert per change, and doing nothing counts as Keep. https://nimbalyst.com/docs/visual-editors-powered-by-ai/markdown-wysiwyg/ai-editing-red-green-diff-approval/
- **Mockups.** `.mockup.html` files can be edited by drawing on them. The agent receives a screenshot, the HTML, your marks and the selected element. https://nimbalyst.com/docs/visual-editors-powered-by-ai/mockups/

### OpenAI Codex app (Codex mode of ChatGPT desktop)
- **Attention keys.**
  - ⌘⌥A jumps to the next chat needing attention, ⇧Esc clears all unread, and ⌘⇧U marks a chat unread.
  - ⌘1–9 jumps to a chat, and ⌘⌥1–6 to a recent one.
  - With a request open, Enter approves and Esc declines it.
  - ↑ in an empty composer restores the previous prompt.
  - https://learn.chatgpt.com/docs/reference/commands
- **Activity view and notifications.** The Activity view (bell, ⌘⌥U) has Work, Chat, Pinned and Scheduled filters. Turn-complete notifications are set to Never, Background only, or Always. https://learn.chatgpt.com/docs/notifications
- **Pets.** Optional floating companions with four states.
  - **States, in priority order:** when chats compete, the pet shows Needs input first, then Blocked, then Ready, then Running.
  - **Modes:** a "Mini" mode shows only the controls, and reduced motion freezes the pet on a still frame.
  - **Shortcuts:** Option+Space or Win+Alt+P opens Quick Chat, and pressing both ⌘ keys sends an "Appshot" of the current app.
  - https://learn.chatgpt.com/docs/pets
- **Review pane (⌃⇧G).**
  - Scope tabs: Unstaged / Staged / Commit / Branch / Last turn.
  - Hover a line and click **+** to comment; comments are sent with the next follow-up.
  - Stage or revert everything, per file, or per hunk.
  - `/review` can run in a **Detached** chat.
  - https://learn.chatgpt.com/docs/code-review?surface=app
- **Handoff and worktrees.**
  - The chat header has **Hand off** (Local ⇄ Worktree ⇄ Cloud), "Create branch here" and Open.
  - The 15 most recent worktrees are kept, and a worktree is snapshotted before it is deleted.
  - Composer chips set the target: Local / Worktree / Cloud.
  - https://learn.chatgpt.com/docs/environments/git-worktrees
- **Input.** Hold Ctrl+M to dictate; you can edit the transcript before sending. ⌃⇧V starts voice chat. https://developers.openai.com/codex/app/features

### Claude Code desktop (Apr 2026 redesign)
- **Panes.** Every pane (chat, diff, browser, terminal, file, plan, tasks, subagent, iOS Simulator) can be dragged by its tab into a grid.
  - Diff and terminal can pop out into their own window and dock back.
  - A session details rail holds cards for environment, changes, background tasks and context.
  - Panes can be minimized into the rail or into a floating corner widget.
  - https://code.claude.com/docs/en/desktop
- **Composer row.**
  - Controls: permission mode (⌘⇧M), worktree toggle, model (⌘⇧I), effort (⌘⇧E) and a usage ring.
  - Pressing **1–9** picks an item from an open menu.
  - Suggested prompts appear as gray ghost text; Tab accepts one.
  - Ctrl+O cycles the transcript view: Normal / Thinking / Verbose.
  - https://code.claude.com/docs/en/desktop
- **Review.**
  - Clicking `+12 -1` opens the diff.
  - Comments go in with + and Enter, and ⌘Enter submits them all.
  - `/code-review` shows a findings card. "Walk through in changes" steps through the findings one at a time with "Fix this one" or Dismiss.
  - The PR card's menu has **Auto-fix**, which fixes CI and review comments, and **Merge when ready**.
  - https://code.claude.com/docs/en/desktop
- **Other.**
  - ⌥-click a session opens split view.
  - Side chat (⌘; or `/btw`) receives context from the main thread but sends nothing back.
  - Sessions archive themselves when their PR merges.
  - https://claude.com/blog/claude-code-desktop-redesign

### Zed (Agent Panel, Parallel Agents, Terminal Threads)
- **Threads sidebar.** Threads are grouped by project, and each row shows title, status and agent. `+` on a project header starts a new thread there.
  - Archiving a worktree thread saves its git state and deletes the worktree; restoring the thread recreates it.
  - New worktrees start on a **detached HEAD**.
  - ⌘⌥J toggles the sidebar, ⌘F searches it, ⇧⌫ archives a thread, and Ctrl+Tab cycles threads.
  - https://zed.dev/docs/ai/parallel-agents
- **Layout.** The new "Agentic" layout is opt-in for existing users, and "Classic" stays available. Zed chose a sidebar over tabs for parallel agents. https://zed.dev/blog/parallel-agents
- **Terminal Threads.** Any CLI agent becomes a sidebar entry titled after the running process, with notifications and the same keyboard navigation as other threads. https://zed.dev/blog/terminal-threads
- **Review and follow.**
  - An accordion lists the files the agent edited. "Review Changes" opens a multibuffer with keep/reject per hunk.
  - **Follow the agent** (crosshair icon, or ⌘ held while submitting) makes the editor jump to each file the agent touches.
  - "Restore Checkpoint" appears on every message that edited files.
  - Double-Enter means "Send Now" while a turn is running.
  - Favourite models cycle with ⌥Tab.
  - https://zed.dev/docs/ai/agent-panel
- **Type.** `.ZedSans` maps to IBM Plex Sans and `.ZedMono` to Lilex. GPUI apps should bundle their fonts: the system fallbacks have different metrics and break icon/label spacing. https://zed.dev/docs/visual-customization , https://github.com/penso/arbor/pull/36

### Cursor 3 Agents Window ("Glass")
- **Tiled layout (3.1).** Split the window, drag agents into tiles, and expand a tile to focus it. The layout persists. A reviewer described it as "a small Kubernetes dashboard" rather than a chat window. https://cursor.com/changelog/3-1 , https://dev.to/gabrielanhaia/cursor-3-glass-replaced-composer-with-an-agents-window-1pcg
- **Best-of-N.**
  - 2.0 ran up to 8 agents per prompt.
  - 2.2 added **judging**: after the runs, Cursor recommends one with a comment explaining why.
  - 3.x adds `/best-of-n m1,m2 <task>` and `/apply-worktree`.
  - https://forum.cursor.com/t/cursor-2-2-multi-agent-judging/145826 , https://cursor.com/docs/configuration/worktrees
- **Queue.** Queued messages stack under the task and can be reordered by dragging. Enter queues, ⌘Enter sends now, and Enter-Enter steers. https://cursor.com/docs/agent/overview
- **Voice.** Hold Ctrl+M to record; the composer shows a waveform, timer, cancel and confirm. Voice chat (Nov 2026) puts a "Start voice chat" button in the Send slot whenever the agent is idle and the input is empty. https://cursor.com/changelog
- **Design Mode (⌘⇧D).** ⇧-drag selects an area, ⌘L adds an element to the chat, and the keyboard walks the element tree. https://cursor.com/changelog/3-1

### Palot (itswendell/palot, OpenCode client)
- **Rail and peek.** A navigation rail (Inbox, Projects, Scheduled, Usage, Worktrees, PRs, Settings) marks attention with a **dot on the rail icon**. Hovering the rail **peeks** a hidden column as an overlay. https://github.com/ItsWendell/palot/blob/main/DESIGN.md
- **Inbox triage.**
  - Row-hover actions: **Settle / Snooze / Wake / Unsettle**, each with an Undo toast.
  - Bulk "Settle idle tasks" by age.
  - Drag across rows to select many at once.
  - Rows never reorder while streaming and **freeze while the pointer is over the list**.
  - https://github.com/ItsWendell/palot/blob/main/DESIGN.md
- **Attention signals.**
  - "Needs input takes priority over Working".
  - A sub-agent's permission requests appear in its parent's request area.
  - The Dock badge counts only tasks that are waiting on you.
  - Sounds are synthesized at runtime: quiet, previewable cues per event.
  - https://github.com/ItsWendell/palot/blob/main/CHANGELOG.md
- **Composer.**
  - Model and effort are separate controls; ⇧Mod+D cycles effort.
  - The model list shows Favorites, Recent (the last 5 per project) and a **New** badge on models released in the last 30 days.
  - A **context ring** turns to warning at 70% and destructive at 90%, next to "101k · $4.55".
  - Queued messages stack as Queued / Steering / Sending / Failed, with Steer now, Edit and Delete.
  - Mod+S stashes a prompt.
  - https://github.com/ItsWendell/palot/blob/main/DESIGN.md
- **Diff.**
  - J/K step through files with an "N of M" counter.
  - Split or unified is chosen automatically from the pane width.
  - A PR **Guide** tab gives numbered review stops with "Look closely" callouts.
  - https://github.com/ItsWendell/palot/blob/main/DESIGN.md
- **Motion.** One **gliding highlight** follows both pointer and keyboard. Springs come from three shared tiers, and the app has its own Reduce Motion setting. https://github.com/ItsWendell/palot/blob/main/CHANGELOG.md

### pi-gui (minghinmatthewlam/pi-gui)
- **Threads and workbench.** Each thread has a tabbed workbench (Terminal, Files, Review, extensions) whose layout is remembered per task. Rows are about 32 px, with "running=green, failed=red". https://github.com/minghinmatthewlam/pi-gui/releases
- **Keyboard.**
  - **Holding ⌘ shows 1–9 badges** on threads, pinned threads first.
  - Ctrl+Tab is an MRU switcher, and a quick tap flips to the previous thread.
  - Enter queues and ⌘Enter steers.
  - https://github.com/minghinmatthewlam/pi-gui
- **Transcript and review.**
  - A 768 px reading column, "Worked for Ns" separators, and a prompt-navigation rail.
  - Skeleton loading and designed empty states.
  - Review compares uncommitted, branch-vs-base or a single turn, and "reviewed" marks survive staging.
  - https://github.com/minghinmatthewlam/pi-gui/releases

### Synthcoda (discontinued), GooeyPi, Opcode
- **Synthcoda.**
  - **Time Machine** snapshots files before each agent edit, even outside git.
  - **Draft Mode** makes Enter not send.
  - "Unbound Workspaces" need no project folder.
  - https://synthcoda.com/
- **GooeyPi.**
  - A harness switcher sits top-left.
  - An Activity view lists running, finished, failed and waiting work.
  - Browser element annotations attach to the next prompt.
  - An animated desktop pet.
  - ⌘J terminal, ⌘⇧B browser.
  - https://github.com/am-will/gooey-pi
- **Opcode TimelineNavigator.**
  - A recursive checkpoint tree indented 24 px per level, with branch connectors only where a node forks.
  - Nodes slide in with a delay that grows with depth.
  - Each card has a "Current" badge, an 8-character ID, token and file counts, and Restore (which auto-saves first), Fork and Compare (pick two).
  - Checkpoint strategy: Manual / After each prompt / After tool use / Smart.
  - https://raw.githubusercontent.com/getAsterisk/opcode/main/src/components/TimelineNavigator.tsx

### Vibe Kanban
- **Layout.** A workspace sidebar, the conversation, a context panel (Changes / Logs / Preview with device viewports) and a details sidebar (git, terminal, auto-saving notes). A **draggable floating Context Bar** opens the IDE, copies the path or toggles the dev server. https://www.vibekanban.com/docs/workspaces/interface.md
- **Comments and preview.**
  - Inline comments are **batched**: a count badge shows how many will be sent with the next message.
  - Preview has "Select element as context".
  - https://vibekanban.com/docs/core-features/reviewing-code-changes
- **Keyboard.** Linear-style **two-key chords** with a 500 ms window:
  - `G S` and `G N` to go places.
  - `V C`, `V L` and `V P` to switch views.
  - `X P` (PR), `X M` (merge), `X R` (rebase) and `X U` (push).
  - `Y P` copies the path.
  - https://vibekanban.com/docs/configuration-customisation/keyboard-shortcuts

### 2026 standouts
- **VS Code Agents window.**
  - New sessions run on this computer, over SSH, through a dev tunnel or in the cloud, isolated by Branch or Worktree.
  - Alt+Enter starts a session in the background.
  - Changes can be scoped to **Branch / Uncommitted / Session / Last Turn**.
  - **Inbox Mode** columns: Inbox / Working / Ready to merge / On hold / Acknowledged / Done. Snooze has an "Until New Activity" option, and opening a session does not acknowledge it.
  - **Compare Models** runs 2–10 attempts side by side.
  - https://code.visualstudio.com/docs/agents/run/sessions/organize-sessions , https://code.visualstudio.com/docs/agents/run/compare-models
- **Google Antigravity 2.**
  - Artifacts (plans, diffs, screenshots, browser recordings) take **inline comments** without stopping the agent.
  - A mic icon on every text input.
  - `/grill-me` asks multiple-choice clarifying questions.
  - https://antigravity.google/docs/artifacts , https://antigravity.google/blog/introducing-google-antigravity-2?hl=en
- **Warp.**
  - Notifications come in three kinds (Complete, Request, Error).
  - **At most two toasts** show at once, and hover pauses them.
  - A mailbox holds All / Unread / Errors.
  - An orchestration **pill bar** above the agent header shows its child agents.
  - https://docs.warp.dev/agents/capabilities/agent-notifications.md
- **T3 Code Sidebar V2.**
  - The inbox replaces the project tree.
  - A "Settled" section collapses to a count.
  - New activity wakes a settled thread, threads auto-settle after 3 idle days, and merged PRs always settle.
  - Snooze options: 1 h, tomorrow 9:00, or Monday 9:00.
  - https://flaviocopes.com/t3-code (secondary)
- **Sculptor.** Pairing Mode mirrors the agent's container branch into your checkout in real time and restores your branch when you turn it off. https://docs.imbue.com/features/pairing-mode
- **Happy (mobile).** Real-time voice with session context, a mute control, and confirmation before sending. https://www.mintlify.com/slopus/happy/features/voice-control

---

## 2. Orca (stablyai/orca): UI and UX patterns

This section is the UI view only. Engine detail, the CLI, the cloud and all ~500 features are in **`research/orca.md`**. ✅ marks a feature Orbit should have; P-levels match orca.md.

**Layout and navigation**
- ✅ **P0** — Per-worktree pane tree with drag-to-split, unified tabs of every type (agent, terminal, browser, editor, diff, markdown), and a reopen-closed-tab action. Source: `docs/model/tabs-panes-splits.mdx`.
- ✅ **P0** — **Jump palette** (⌘K). With an empty query it lists up to 6 recent chats and terminals, **ranked needs-you > done > idle**, each with Mod+1–6, followed by recent worktrees. Typing also searches PR `#123` and MR `!123` numbers, plus open-tab titles and content. Tab adds host or repo filter chips, and a "Create worktree" row sits at the bottom. Source: `docs/model/quick-open.mdx`.
- ✅ **P1** — Floating terminal workspace (⌘⌥A), quake-style.
- ✅ **P1** — Worktree history back and forward (⌘⌥←/→).
- ✅ **P1** — MRU tab switcher.
- ✅ **P1** — Customizable "+" new-tab menu.
- **P2** — App rail.

**Status and attention**
- ✅ **P0** — **Glyph vocabulary**, used everywhere:

  | Glyph | Meaning |
  |---|---|
  | Spinner | working |
  | Amber `?` | needs you |
  | Emerald check | done |
  | Red dot | failed or blocked |
  | Gray | idle |
  | None | plain shell |

  Source: `docs/model/agents-sessions.mdx`.
- ✅ **P0** — **Smart attention sort.** Order: Needs you > recent unrequested Done > Working > stale-with-live-PTY > Idle. Hooks outrank title heuristics. On cold start the persisted order is kept until the first live signal. Source: `src/shared/sidebar-list/sorting/smart-attention.ts`.
- ✅ **P0** — Header bell inbox with a Dock badge.
- ✅ **P1** — **Agents feed**: a threaded chronological activity feed with response previews.
- ✅ **P1** — **Agent Dashboard**, a board with Needs You / Working / Done / Idle columns that opens in-window or as a pop-out.
- ✅ **P1** — Subagent and Agent Teams children as expandable rows under the lead agent.

**Composer and chat**
- ✅ **P1** — One session can be viewed as **Chat or Terminal** (⌘⇧J switches). "Continue in chat/terminal" copies a conversation into the other form.
- ✅ **P1** — "Hand off to agent" seeds a fresh agent with a bounded summary of the conversation.
- ✅ **P0** — Native chat **type scale** enforced by lint: 13/12/11/10 px, 12 px code. The chat column is at most 46 rem wide with a 60 em measure. Text size and code size are separate settings.
- ✅ **P1** — "Add to chat from anywhere": terminal selection, file, diff line or browser element.

**Review and ship**
- ✅ **P0** — Combined worktree diff against any compare base. "Committed on Branch" scope.
- ✅ **P0** — **Review notes**: a ⌘⇧A note on a line, "Send notes to" any live agent or a new one, and unresolved notes are re-sent.
- ✅ **P0** — AI commit message, AI PR details, **Fix with AI** after a failing pre-commit, **Resolve with AI** for conflicts. Each action is driven by a per-repo recipe.
- ✅ **P1** — Stacked PRs that merge atomically.
- ✅ **P1** — Git graph and git blame.
- **P1** — "AI vs human line attribution". This is documented but no implementation was found.

**Teams, bots and automation**
- ✅ **P0** — **Crew strip** under the coordinator's chat: workers with status, plus follow, read and stop actions. `task_…` IDs in terminal output are clickable links.
- ✅ **P1** — **Bot Home**: Needs you / In progress / Waiting / Planned / Closed / Automations / Suggestions / Activity.
- ✅ **P1** — **Needs-you cards**, a question of at most 140 characters with quick-reply options and a recommended choice. Approvals always come first, at most 5 rows show, and the rest sit behind a footer.
- ✅ **P1** — "Look around": a read-only scout every 4 h on weekdays from 8:00 to 20:00 that suggests work.
- ✅ **P1** — Automations page with run history.
- ✅ **P0** — Worktree **checkpoint comment** ("action; next step"), set by agents and shown on the card.

**Other surfaces**
- ✅ **P0** — Status bar with **per-provider usage**. Its roster popover sorts providers tightest-first and shows a warning chip at 80%.
- ✅ **P1** — Resource manager showing CPU and memory per workspace.
- ✅ **P2** — Mobile companion with E2EE pairing via QR.
- ✅ **P2** — On-device dictation (⌘E).
- **P2** — Built-in browser with Design Mode.
- **P2** — Computer use.
- **P2** — Emulator bridge.
- **P2** — Plugins catalog.
- **Skip** — Pets.

**Onboarding**
- ✅ **P0** — A five-step wizard: language, theme carousel, agent pick, permissions, notifications and integrations.
- ✅ **P0** — An **activation checklist**: addedRepo, choseAgent, ranFirstAgent, *ranSecondAgentOnSameTask*, triedCmdJ, reviewedDiff, openedPr, and so on.
- The documented golden path is a "**race**": the same prompt runs in 3 worktrees with 3 agents.
- Source: `src/shared/onboarding`, `docs/first-session.mdx`.

**Visual language.** Orca uses monochrome chrome, Geist with SF Mono, a 10 px base radius, and three elevations. Its state hues are working yellow-500, question orange-600, unread blue-500 and AI violet-500. Motion is 180 ms `cubic-bezier(0.16,1,0.3,1)`. The full tokens are in orca.md, Part C, "Design tokens".

**Where Orca's UI is weak, and where Orbit can win.**
- It is Electron. Its own repo lists 1,577 known main-thread blocking calls, and users report GPU leaks and idle GPU load.
- It has a single window (#6074).
- It has no multi-root diff (#1099).
- It has no LSP (about 183 reactions across issues).
- The breadth of features shows as clutter: 40 agents, a browser with extensions and passwords, and pets.

---

## 3. What every serious tool now converges on (table stakes)

These are no longer differentiators; Orbit simply needs them.
- A "needs input" state that outranks "working", plus a key to jump to it.
- Enter to queue and ⌘Enter to steer.
- A context ring in the composer.
- Diff scopes per turn, session and branch.
- Line comments batched into the next prompt.
- `/btw` side chats.
- ⌘1–9 to jump to a session, and Ctrl+Tab to switch in most-recently-used order.
- Local / Worktree / Cloud as a per-session choice.
- Settle or archive for finished work.
- A reduced-motion fallback.
- A shortcut cheatsheet (⌘/).

---

## 4. Ranked recommendations for Orbit (25)

The ranking weighs daily impact, how far each item gets ahead of the field, and how well it fits a native GPUI app. Each entry gives what to build (the interaction and the visual), why it beats what exists, and where the idea comes from. "Supersedes FR #n" means the entry replaces a less specific item n in the feedback report.

### 1. The Needs-You Deck: a triage surface built on decision cards
**Build.**
- A left-rail entry (badge = count of *waiting* items only) opens a vertical deck of cards, one per blocked agent.
- Each card shows:
  - the agent glyph, workspace and branch;
  - the question (at most 140 characters);
  - three lines of context: the last tool call and the file it touched;
  - option buttons numbered **1–9**, with the agent's recommended option outlined.
- **Keys:**
  - j/k move between cards, 1–9 answer, and `a` approves.
  - `A` means "approve all of this kind" and resolves matching prompts across sessions.
  - `r` opens an inline reply.
  - `s` snoozes: 1 h / tomorrow 9:00 / until new activity.
  - `e` settles.
  - Enter opens the full session.
- **Ordering:** approvals first, then questions, then failures, then done-unread. Rows never reorder while the pointer is over the deck or while any card is focused. New items slide in at the top with a 160 ms ease-out, and an "N new" pill appears if you have scrolled.
- **Settling:** settled items collapse into a count. New activity wakes a settled item, and a merged PR auto-settles its items.

**Why it beats what exists.** Every tool has an inbox, but none combines answering inline with stable ordering and one-key triage. Claude Code users report "needs input" dots that can't be dismissed. VS Code's Inbox Mode has the right states but needs the mouse. *Supersedes FR #4.*

**Inspired by:** Orca's Needs-you cards, Palot's settle/snooze and pointer freeze, VS Code Inbox Mode, T3 Sidebar V2, Codex ⌘⌥A, and Nimbalyst's "Always resolves the rest".

### 2. Honest status engine and one glyph vocabulary
**Build (engine).**
- Status comes from hooks first: a loopback HTTP endpoint whose port and token live in an endpoint file that hooks re-source on every call, so agents survive app restarts.
- OSC 7501 parsed from the PTY is the second source.
- Process and title heuristics come last.
- An agent with a live PTY but no signal for more than 2 minutes becomes **Unverified**; Orbit never guesses "idle".

**Build (UI).** One 12 px glyph set, used on rows, tabs, tiles, the tray and the mini-map. Every state differs by shape as well as color, so the set is colorblind-safe.

| State | Glyph |
|---|---|
| Working | Thin neutral arc, rotating at 1 rev/1.2 s, throttled to 10 fps when the window is unfocused |
| Needs you | Amber filled circle with `?` |
| Permission | Amber filled circle with a shield |
| Failed | Red `!` |
| Done-unread | Blue dot |
| Done-read | Hollow check |
| Unverified | Dashed gray ring |
| Idle | No glyph |

Hovering any glyph shows the elapsed time, the last event and the token count.

**Why it beats what exists.** Claude desktop's status panes disagree with each other. Emdash users asked for an approval icon that differs from the spinner. Orca is the only tool with an explicit unverifiable state. *Supersedes the icon part of FR #4.*

**Inspired by:** Orca's agent-hooks README and state ranking, Superset's status colors, and Emdash #531.

### 3. Turn cards with a "Worked for" fold
**Build.** The transcript is a list of turn cards.
- **Header:** the user prompt, clamped to 2 lines, plus a live "now: Editing `src/auth.rs` · 1m 12s" line while the turn runs.
- **Body:** a single collapsed row reads "Worked for 2m 14s · 23 tool calls · 4 edits · 1 failed". Expanding it shows grouped tool rows with status pips. Failed calls are always visible, never folded.
- **Reply:** the final reply, with a "Read more" fold past 12 lines.
- **Footer:** changed-file chips (`auth.rs +42 −7`). A chip opens the **turn diff**, and a "Restore to here" action sits on the right.
- **Navigation:** a thin prompt rail on the transcript's right edge has one tick per user prompt. Hovering a tick previews the prompt, and ⌘⌥[ / ] jump between ticks.
- **Rendering:** only visible cards are laid out, and tool output is loaded lazily when a card expands.

**Why it beats what exists.** Raw transcripts, as in Claude desktop and Codex, bury the outcome under tool noise. Turn cards make "what changed and did it work" readable in one glance, and they are also the cheapest thing to render.

**Inspired by:** PandaCode conversation-presentation, pi-gui "Worked for Ns", Emdash "Ran N commands", Zed Restore Checkpoint, and Palot's turn jumps.

### 4. Composer control row with loadouts and a visible queue
**Build.** One row under the input holds, left to right:
- **Target chip:** Local / Worktree / SSH host / Cloud.
- **Permission chip:** Ask / Auto-edit / Plan / Auto. The chip's color only appears in Auto.
- **Model trigger:** a searchable list of Favorites, Recent (last 5 for this project), and models marked **New** (released in the last 30 days).
- **Effort trigger:** a separate control; ⇧⌘D cycles effort.
- **Loadouts:** ⌃1–5 switch between saved model+effort+permission sets.
- **Context ring:** with the text "101k · $4.55". The ring turns amber at 70% and red at 90%. Hovering it shows the breakdown: system, tools, files, history.

While a run is active:
- Enter queues and ⌘Enter steers.
- Queued messages stack above the input as chips (Queued / Steering / Sending / Failed). Each chip has Edit, Delete and Steer now, and chips can be reordered by dragging.
- Every queued chip says why it waits, for example "after current tool call".

In any open menu, 1–9 picks an item. Mod+S stashes the draft. ↑ in an empty input recalls the last prompt.

**Why it beats what exists.** Every competitor has some of these controls, but none has all of them, and none makes the queue visible and editable. Codex's queued-edit bugs and the requests for T3 Steer/Queue show the demand. *Supersedes FR #6 and FR #10 with a concrete layout.*

**Inspired by:** Conductor Loadouts and its queue reasons, Palot's context ring, queue stack and New badge, Claude desktop's composer row and 1–9 menus, and Cursor's drag-to-reorder queue.

### 5. Agent Teams: an Architect with a Crew (delegation)
**Build.**
- A **Team** toggle in the composer picks an architect model and worker profiles, or "Auto" with a Cheap / Balanced / Best routing choice.
- The architect writes a plan of tasks as a dependency graph. Each worker runs in its own child worktree branched from the architect's.
- **Crew strip** under the architect's thread:
  - a header tally reads "3 working · 2 done · 1 blocked", above a segmented progress bar;
  - each worker gets one row: glyph, task title, elapsed time, tokens, files touched, and an expand chevron;
  - sub-workers are indented under their parent;
  - a queued worker says why it waits: a dependency, a free slot, or a file-scope conflict with another worker.
- **Team Notes** is a shared blackboard pane for API shapes and decisions. Workers read it through an `orbit notes` CLI, and every brief includes the newest notes plus "N more".
- **Mail** between agents is typed (question / handoff / status / done) and stored in a database. Orbit wakes an idle TUI agent with a one-line pointer telling it to check its mail, and never types the mail itself into the TUI. Urgent mail can steer a worker at most once per minute.
- Workers finish with `worker_done`. The architect then shows a merged diff of all worker branches, and conflicts are flagged per worker.
- **Council** review is optional: N read-only reviewers score the work, and the loop continues until the lowest score reaches the target.
- `task_…` IDs anywhere in the transcript or terminal are links to that worker.

**Why it beats what exists.** Today, Claude subagents and Cursor's best-of-N are invisible or flat. PandaCode has the best UI for teams and Orca has the most robust engine; Orbit combines both natively. Orbit currently has nothing like it. *Goes beyond FR #13/#20.*

**Inspired by:** PandaCode architect-workers, Orca orchestration (mailbox and pointer, circuit breaker, 4-worker default), and Warp's pill bar.

### 6. The Constellation: a live mini-map of every agent
**Build.** A 220×64 px strip docked at the bottom-left of the sidebar, expandable to a full-window "galaxy" view with ⌘⇧Space.
- **Nodes:** each running agent is a dot clustered by project.
  - Color = state, from the glyph vocabulary.
  - An outer ring arc = context fill.
  - Dot size = diff size (log scale).
  - A soft halo whose brightness follows tokens per second over the last 10 s.
- **Links:** lines connect an architect to its workers. A dotted red line joins two workspaces whose live diffs touch the same file, giving early conflict warning.
- **Interaction:**
  - Hovering a node shows a peek card: the last tool line, the question if one is waiting, and elapsed time.
  - Clicking a node focuses that session; ⌘-click adds it as a tile.
  - In galaxy view, arrow keys move between nodes and Enter opens one.
- **Idle cost:** when nothing is running, the strip collapses to a single line, "All quiet · 12 settled", with no animation.

**Why it beats what exists.** Lists don't scale past about 10 agents. A mini-map shows the shape of the whole fleet at once: who is stuck, who is burning tokens, and which agents are about to collide. No competitor has one. Nimbalyst's menu-bar panel and Superset's activity chips are the closest. The Orbit name earns it.

**Inspired by:** Nimbalyst's fleet panel and Dynamic Island, Superset's activity strip, PandaCode tiles, and Codex pets' priority order.

### 7. Race and Compare (best-of-N, done properly)
**Build.**
- An **×N** stepper in the composer, plus chips for agent and model, so you can race the same agent three times or three agents once each. Each candidate gets its own worktree from the same base.
- The **Compare view** has one column per candidate, showing:
  - diff stats, time, cost and tokens;
  - test results from the repo's `check` script, run automatically in each worktree;
  - an optional **judge** verdict naming a recommended candidate with a 2-line reason;
  - a file-by-file **cross-candidate diff**, for example A vs B on `auth.rs`;
  - "Take this hunk from B", which cherry-picks a hunk into the winner.
- **Keep winner** merges or adopts the winner's branch and archives the losers' worktrees, which stay restorable for 7 days.

**Why it beats what exists.** Cursor judges but has no cross-candidate diff. Emdash only creates labeled worktrees. Orca's "race" is a manual recipe. VS Code compares chats, not code plus tests. *Supersedes FR #21.*

**Inspired by:** Cursor 2.2 judging, VS Code Compare Models, Orca's race recipe and onboarding, and Emdash runs-per-provider.

### 8. Tiles: watch up to 6 agents live
**Build.**
- Drag any session, or ⌘-click it, into a tile grid. Orbit picks 1×2, 2×2 or 2×3 automatically from the window aspect ratio.
- The focused tile is fully interactive. The other tiles show a live compact view: the header glyph, the current step line, the last 8 lines of streaming output, and the pending question with its 1–9 options.
- ⌘⌥←/→/↑/↓ moves focus between tiles. ⇧⌘Enter maximizes the focused tile.
- **Follow** mode is per tile: the tile's mini-editor jumps to each file the agent touches.
- The layout is saved per window. Multi-window is native, so tiles can live on a second monitor.

**Why it beats what exists.** Cursor 3.1 and Claude split view tile chats, but unfocused tiles are either full UIs (expensive) or blank. Compact live tiles are cheap in GPUI and readable at a glance. Multi-window is also Orca's #2 most-requested gap (#6074).

**Inspired by:** PandaCode tiles, Cursor 3.1 tiled layout, Claude split view, and Zed follow-the-agent.

### 9. Bots: run agents from Telegram, Slack and Discord, with no open ports
**Build.**
- A Bots settings page with numbered setup steps for each channel.
  - **Telegram:** paste a BotFather token. The desktop **long-polls the Telegram Bot API directly**, so no Orbit cloud is needed.
  - **Slack:** a prefilled app manifest using Socket Mode, which is also outbound only.
  - **Discord:** gateway.
- Each chat maps to an Orbit thread in a chosen project.
- Commands: `/new <prompt>`, `/status` (a compact fleet summary), `/stop`, `/approve`, and `/diff` (a stat summary plus a link).
- Approvals arrive as messages with inline **Yes / No / Always** buttons.
- Strangers get a 4-digit pairing code that the owner approves on the desktop.
- A **Guard list** always asks a human, even under Auto: force-push, `rm -rf`, sudo, deploys, payments.
- Quiet hours, and a digest mode that sends one message per finished run.
- An optional bot persona folder (`SOUL.md`, `MEMORY.md`) for long-lived bots.

**Why it beats what exists.** Orca's bots depend on its closed cloud connector service, and Happy and Omnara need their own app. A direct Telegram or Slack Socket Mode link is private, cheap and works today. Orbit has nothing remote yet. *Goes beyond FR #9 (mobile client) by using chat apps people already have.*

**Inspired by:** PandaCode bots (pairing code, Guard, yes/no/always), and Orca Bots (SOUL/MEMORY, Needs-you, outbound-only polling).

### 10. Peek and Quick Reply from anywhere
**Build.**
- **Hover peek:** hovering any session row, tile, mini-map node or tray item for 300 ms opens a 360 px card. It shows the last 6 transcript lines, the current step, the question with 1–9 options, and buttons for Open, Settle and Stop.
- **Global quick launcher:** ⌘⇧Space (Ctrl+Shift+Space on Windows and Linux) works even when Orbit is in the background. It opens a floating command bar:
  - type `@workspace` to target a session, then a message;
  - Tab to queue or steer, Enter to send;
  - an empty query lists items that need you.

**Why it beats what exists.** Conductor's hover only shows metadata, and Codex Quick Chat isn't tied to an existing session. Answering an agent without switching context is the most common micro-task in multi-agent work.

**Inspired by:** Conductor's hover card, Nimbalyst's transcript peek, PandaCode's ⌘⇧Space launcher, and Codex pets/Quick Chat.

### 11. Session Timeline and Replay
**Build.**
- A 28 px horizontal **timeline** sits under every session. It has ticks for prompts, tool calls (bucketed), edits, approvals, test runs and failures, color-coded by kind.
- **Scrub** by dragging. A ghost cursor shows "at 14:02: Edit auth.rs", and the file panel shows the workspace *at that moment*, reconstructed from automatic checkpoints stored as private git refs per turn.
- **Range compare:** drag across two points to diff them.
- **Fork from here** starts a new branch session from that point.
- **Replay** plays the session back at 1–20× speed. Edits animate into the file view, terminal output streams, and approvals pause playback.
- Export a self-contained HTML replay to share in a PR or postmortem.

**Why it beats what exists.** Checkpoints exist elsewhere (Conductor, Zed, Opcode), but they are lists. A scrubbable timeline answers "when did it go wrong?" in seconds, and replay is unique among these tools. Orbit lacks it entirely.

**Inspired by:** Opcode's TimelineNavigator (fork and compare), Conductor's checkpoints, Synthcoda's Time Machine, Antigravity's browser recordings, and PandaCode's visual-progress PiP.

### 12. Voice: push-to-talk dictation and voice steering
**Build.**
- **Push to talk:** hold a key (default Ctrl+M, configurable to a modifier double-tap) anywhere in Orbit.
  - The composer shows a waveform, a timer, and Cancel (Esc) and Confirm (release).
  - Speech is transcribed **on-device** (whisper or parakeet via a Rust crate), and the text lands editable in the composer.
- **Addressed commands:** in the quick launcher, "@api run the tests then open a PR" is sent to the session named `api`.
- **Mic button:** when an agent is idle and the input is empty, the Send button becomes a mic.
- **Privacy:** no cloud by default, and the mic indicator stays in the title bar while recording.

**Why it beats what exists.** Cursor and Codex hold-to-dictate only work in the focused composer, and Orca's dictation is a separate setting. Voice that can address any agent fits a "manager of agents" workflow. Orbit has no voice input today.

**Inspired by:** Cursor 3.1 voice (waveform, timer, Send slot), Codex Ctrl+M, Antigravity's mic on every input, Orca's local sherpa-onnx STT, and Happy's session-aware voice.

### 13. Review Walkthrough mode
**Build.** A review mode inside the diff panel, opened with ⌃⇧G.
- **Scope tabs:** Uncommitted / Last turn / Session / Branch / Against any ref.
- **Focus mode** shows one file at a time with "3 / 11" and J/K to move. **Mark viewed** (`v`) resets automatically if the file changes later.
- A "**Guide**" lane, generated by the agent, lists numbered review stops with "Look closely" callouts, in an order chosen for risk.
- Comments use `c` on a line or range. They batch into a draft with a count badge ("4 notes will be sent"), and ⌘Enter sends them to a picker of live agents or a new one.
- After the agent's next turn, every note shows **addressed / not addressed**, computed from whether the commented lines changed.
- "Fix this one" per finding.

**Why it beats what exists.** It puts the best piece from each tool into one keyboard flow: Conductor's mark-viewed reset, Palot's Guide, Vibe Kanban's batch badge, and Orca's "send notes to" picker. Note-addressed tracking is new. *Supersedes the review parts of FR #3.*

**Inspired by:** Conductor, Palot, Superset Focus mode, VS Code scopes, Vibe Kanban, Orca, and Claude's "Walk through in changes".

### 14. Ship pill: one control from commit to merge
**Build.** A pill in the session header shows `+128 −40 · 6 files`. Its action button walks through **Commit → Push → Open PR → Merge when ready**.
- Commit opens a popover with an AI-drafted message, editable, generated by the user's own agent CLI.
- Once a PR exists, the pill becomes a PR badge (open / draft / merged / closed in four distinct hues) with a CI mini-bar.
- When checks fail, a "Fix checks" item sends the logs to the agent.
- **Auto-fix** (opt-in) handles failing CI and review comments, with a maximum of 2 attempts.
- Stacked PRs show as a chain of pills.
- If a pre-commit hook fails, "Fix with AI" appears in the failure toast.

**Why it beats what exists.** It is one stateful control instead of four buttons, and it always shows the next step. *Expands the commit/PR line of FR #3.*

**Inspired by:** Superset's ship pill, Claude's Auto-fix and Merge when ready, Orca's Fix with AI and stacked PRs, and Conductor ⌘⇧P/⌘⇧M.

### 15. Workspace rows with an activity strip
**Build.** Each sidebar row is 32 px when collapsed and 52 px in comfortable density. It shows:
- the glyph, the name (auto-named from the first prompt, with a city-style fallback), and the branch in muted mono text;
- right-aligned diff stats;
- a PR badge;
- a pencil icon if there is an unsent draft;
- a 2 px context-fill bar along the bottom edge;
- chips for running ports (`:3000`) and extra agents (`×2`), which list their items with stop buttons on hover;
- a single "checkpoint" line ("Next: wire refresh token"), written by the agent through `orbit workspace note`. The line keeps its history as a timeline.

Rows reorder with a 180 ms slide, but never while the pointer is over the list.

**Why it beats what exists.** It merges Superset's chips, Conductor's draft pencil, Nimbalyst's context bar and Orca's checkpoint comment, with history added. *Detailed design for FR #13.*

**Inspired by:** Superset, Conductor, Nimbalyst, Orca, and Palot.

### 16. Keyboard grammar
**Build.**
- **Hold ⌘ for 400 ms:** keycap badges 1–9 appear on sessions, pinned sessions first, and ⌘1–9 jumps to them.
- **Ctrl+Tab:** most-recently-used switcher; a quick tap flips to the previous session.
- **⌘⌥A:** next item needing attention.
- **Two-key chords** with a 500 ms window:
  - `G I` inbox, `G D` deck, `G C` constellation;
  - `V D` diff, `V T` terminal, `V P` preview;
  - `X C` commit, `X P` PR, `X M` merge;
  - `Y P` copy path.
- **Discoverability:** ⌘/ opens a searchable cheatsheet where pressing a combination finds it. Every menu item shows its keycap.
- **Keymap file:** user overrides go in `keymap.json`, in the same format as Orca's `keybindings.json`, so Orca keymaps can be imported. Conflicts raise warnings.

**Why it beats what exists.** Most competitors only have a palette. Chords make dozens of actions reachable without leaving the home row. *Supersedes FR #12 with a grammar.*

**Inspired by:** pi-gui's hold-⌘ badges, Vibe Kanban's chords, Codex ⌘⌥A, and Conductor's key-press search.

### 17. Side chats that can be promoted
**Build.** ⌘; opens a 420 px slide-over on the right: a side chat that inherits the main thread's context read-only and uses a cheaper model by default.
- Nothing flows back to the main thread unless you click **Promote**. Promote summarizes the side chat into a single note and queues it into the main thread.
- Side chats are listed under their parent as "By the way" entries.

**Why it beats what exists.** The Claude and Codex side chats are one-way and their answers get lost. Promote closes the loop without polluting the main context.

**Inspired by:** Claude ⌘;, Codex ⌘⌥S (which now carries attachments and comments), PandaCode's "By the way" tab, and the Juggler "aside threads" request.

### 18. Chat and Terminal as two views of one session
**Build.** Every session has a **Chat** view (structured, from the SDK or app-server) and a **Terminal** view (the real CLI TUI). ⌘⇧J flips between them with no restart.
- If the session was started as a TUI, Orbit parses the transcript file to build the chat view.
- "Continue in the other form" offers a **fork-at-message** variant.

**Why it beats what exists.** It keeps the native agent's power (slash commands, keybindings) next to the GUI's readability. Superset and Orca have the toggle, but Orca copies the conversation into a new tab instead of flipping the same session. *More specific than FR #5's "TUI fallback".*

**Inspired by:** Superset's CLI/Chat toggle, Orca's Continue in chat/terminal, Emdash's per-agent Chat/TUI memory, and Zed Terminal Threads.

### 19. Quota rings with burn-rate projection and account failover
**Build.**
- The foot of the rail has one ring per provider account, with the plan name and window on hover.
- Hovering also projects "At this pace you'll hit the 5 h limit at 15:40", based on the last 30 minutes of tokens.
- When a limit is near, the composer shows **Switch to account B** or **Queue until reset 16:10**.
- Accounts are isolated by giving each one its own `CLAUDE_CONFIG_DIR` / `CODEX_HOME` folder, so switching takes one click and you never log out. An account can be pinned per project.

**Why it beats what exists.** Rings exist in Superset, Palot and Claude, but nobody projects burn, and failover is an open request at Orca (#20512, plus #11557 and #9496). *More specific than FR #6 and FR #16.*

**Inspired by:** Superset's rail rings, the Orca usage roster and account homes, and Palot's context ring.

### 20. Plans and notes as commentable artifacts
**Build.**
- Plan mode renders the plan in a **Plan pane**, with one block per step.
- Hover a step to comment on it, edit it inline, or reorder it by dragging.
- **Approve plan** starts execution, and each step's checkbox is ticked live from hook events.
- Comments made during execution are delivered as steering without stopping the agent.
- Plans are saved as markdown in `.orbit/plans/`.
- A per-workspace **Notes** tab holds todos. A setting decides whether unchecked todos block merging.

**Why it beats what exists.** Antigravity proved that commenting on artifacts works. Cursor's plan tabs and Conductor's todos are static. Live-ticked plans let you see progress against intent.

**Inspired by:** Antigravity artifacts, Cursor plan tabs, Conductor Notes and todos, Synthcoda's editable Plan Mode, and PandaCode's checkpoint panel.

### 21. Onboarding: a first race in 90 seconds, and empty states that teach
**Build.**
- **First run** auto-detects installed agent CLIs and signed-in accounts, so you don't have to configure them. It asks only three questions: theme, default permission, notifications.
- The next screen offers "Try it": pick a repo or use the bundled sample repo, type one prompt, and Orbit races two agents on it. Diff, compare and ship are taught by coach marks in place.
- **Every empty state** names its action and keycap. Examples: "No sessions yet · ⌘N new session · ⌘⇧N race", and "Deck clear · agents will appear here when they need you".
- An **activation checklist** in Settings: ran 2 agents on one task, reviewed a diff, used ⌘K, answered from the Deck.

**Why it beats what exists.** Orca's wizard has five steps and the race is only documented. Emdash and pi-gui have good empty states but no guided first win.

**Inspired by:** Orca's onboarding and activation checklist, Conductor's sample-project flow, Emdash's keycap empty state, and pi-gui's designed empty states.

### 22. Calm notifications: sounds, toasts and tray
**Build.**
- **Toasts:** at most **2 visible**. Hover pauses them, a countdown ring shows on the close button, clicking jumps to the item, and error toasts have a Copy button.
- **Mailbox:** All / Unread / Errors, in the Deck.
- **OS notifications:** only when Orbit is in the background, and grouped per workspace.
- **Sounds:** short tones synthesized per event (done, needs-you, failed), each previewable, with a volume control and Off.
- **Tray and menu bar:** counts of waiting and running, and the top 5 waiting items with 1–9 quick answers.
- **Dock badge:** counts only waiting items.

**Why it beats what exists.** Notification fatigue is the hidden cost of parallel agents. These rules cap the noise.

**Inspired by:** Warp's two-toast cap and mailbox, Conductor's countdown ring and copy, Palot's synthesized sounds and waiting-only badge, and Nimbalyst's menu bar.

### 23. Scouts and triggers (proactive agents with guardrails)
**Build.**
- A **Scout** is a scheduled, read-only agent run (for example weekdays 9:00 and 14:00). It looks at failing CI, new issues and stale PRs, then posts **suggestions** to the Deck as cards: "CI on main flaky in `auth_test`; start a fix?" with Start and Dismiss.
- Read-only is enforced by a tool allow-list at the hook layer, not by the prompt.
- **Triggers** start a templated session on events: a PR opened, a check failed, an issue labeled.
- Run history is a list with results, cost and a "Run now" button.

**Why it beats what exists.** Claude Routines and Codex Scheduled run blind. A scout that only suggests, enforced in code, earns trust. *Supersedes FR #22 with a safer design.*

**Inspired by:** Orca's look-around and event triggers, Codex Scheduled, and Claude Routines.

### 24. Follow the agent, with file heat
**Build.**
- **Follow** (`f` or a crosshair icon) makes the editor or preview jump to each file the agent reads or edits, with a 1.2 s highlight on the changed lines.
- In the file tree, files touched in the current turn show a **heat dot** that fades over 5 minutes:
  - blue = read;
  - amber = edited;
  - red = edited by two sessions at once (conflict risk).

**Why it beats what exists.** Zed's follow mode is loved but only exists in Zed. Heat makes the agent's footprint visible without reading the transcript.

**Inspired by:** Zed follow-the-agent, Orca's editor flash token (1.2 s), and Superset's activity chips.

### 25. Preview with element pick
**Build.**
- A per-workspace preview pane, a native webview via wry, with a dev-server menu read from `orbit.toml` and auto-allocated ports.
- **Pick element:** ⌘⇧C, then hover to outline elements and click to attach. The attachment carries the HTML, computed styles, selector, a cropped screenshot, and the React or Vue source location when available. ⇧-drag attaches a region screenshot.
- Device viewport chips (desktop, tablet, phone).

**Why it beats what exists.** Every major tool now has this (Claude, Cursor, Superset, Orca, Vibe Kanban). It ranks low only because it is table stakes for front-end users and harder in a native app. Orbit should still ship it.

**Inspired by:** Cursor Design Mode, Orca Design Mode, Vibe Kanban's element context, and Claude's ⌘⇧S picker.

---

## 5. Visual language

What the best-looking of these apps (Zed, Conductor, Palot, Orca, PandaCode, pi-gui) do in common, written as rules Orbit can apply directly.

**Density**
- Three densities: **Compact** (28 px rows), **Default** (32 px) and **Comfortable** (40 px). pi-gui uses 32 px and Superset went denser in 2026.
- Headers are 36 px and tab rows 32 px, matching Orca.
- Lists and sidebars are dense. Reading surfaces (the transcript and Plan) are airy:
  - a max line length of 68–72 ch (Orca 60 em; pi-gui 768 px);
  - 1.55 line height for prose and 1.4 for code;
  - 16 px between turn cards.
- Put metadata on hover or in a details rail rather than on the row. Every row may show at most **one** line of secondary text.

**Typography**
- **Bundle the fonts.** GPUI's fallbacks break icon/label alignment (penso/arbor #36).
  - UI: **Inter Display/Inter** or **Geist** (Orca), or IBM Plex Sans (Zed).
  - Mono: **Lilex** (Zed) or Geist Mono / JetBrains Mono.
  - Ship a Nerd-symbols fallback for TUIs, as Orca does.
- **Scale:** 14 px for prose and inputs, 13 px for rows and buttons, 12 px for metadata, 11 px for uppercase section labels (semibold, +0.05 em tracking), and 10 px only for badges.
- Use **tabular numerals** for every counter, timer, cost and diff stat so live numbers don't jitter.
- Monospace is for identifiers only: branch names, paths, IDs, ports.
- Text size and code size are separate user settings, as in Orca's chat.

**Color**
- **Monochrome chrome; color means state.** This is Orca's explicit rule and Conductor's practice.
  - Dark theme: background #0B0B0C, panels #141416, raised #1B1B1E, hairline borders `rgba(255,255,255,0.07)`, text #EDEDED with muted #9A9A9F.
  - Light theme mirrors these values.
- **Fixed semantic hues, never themeable away:**

  | Meaning | Hue |
  |---|---|
  | Needs you | amber #F5A524 |
  | Failed | red #F04438 |
  | Done-unread | blue #3B82F6 |
  | Ready for review / PR open | green #22A06B |
  | Merged | violet #8B5CF6 |
  | AI action accent | violet at 60% |

- **Working has no hue.** It is a neutral animated arc, so color always means "act" (a deliberate break from Orca and Superset, who use yellow or amber for working).
- Every state has a **shape** as well as a color (colorblind-safe, per FR #23).
- **Tints:** mix the state color at 8–12% into the surface for selected and attention rows (Orca's `color-mix` approach), never full saturation.
- **Diff colors:** use a colorblind-safe pair (blue/orange option, as in Conductor's colorblind palettes) and word-level highlight at 25% alpha.
- **Security surface:** permission and approval dialogs use a fixed token set that themes and plugins cannot restyle (Orca's `surface="security"`).

**Elevation and shape**
- Three levels:
  1. A hairline border.
  2. A border plus a 1 px shadow for cards.
  3. Floating: `0 10px 24px rgba(0,0,0,.18)` for popovers, peeks and palette.
- Use native vibrancy for menus on macOS and Mica on Windows 11.
- **Radii:** 6 px for controls, 8 px for cards, 10 px for popovers, full for badges and glyphs.

**Motion**
- **Default timing:** 160–180 ms with `cubic-bezier(0.16, 1, 0.3, 1)` (ease-out-expo, Orca) for enter. Exits are faster at 110–120 ms (PandaCode menus: 170 ms in, 110 ms out, scale 0.98 to 1 from the trigger origin). Press feedback takes 120 ms.
- **Springs:** use one shared set of three spring tiers (Palot) rather than ad-hoc durations.
- **Gliding highlight:** a single selection highlight that glides between rows for both pointer and keyboard (Palot).
- **Reorder:** rows reorder with a 180 ms slide, frozen under the pointer.
- **Attention:** new attention items pulse **once** (a 600 ms ring) and never loop.
- **Idle budget:** at most one animated element per visible session (the working arc). Arcs throttle to 10 fps when the window is unfocused and stop when the app is hidden. This avoids the "GPU spike from a tiny useless animation" complaint against Codex (#16857) and Orca's idle GPU load (#9576).
- **Reduced motion:** honor both the OS setting and an in-app setting. Everything then becomes an opacity fade at ≤100 ms.
- **Delight, used sparingly:**
  - city or star names for workspaces (Conductor's passport, or Orbit "constellations");
  - one optional subtle ambient background per tile (PandaCode scenes), off by default;
  - completion sounds with personality, but always previewable and mutable.

**Iconography**
- **Lucide**, or a Lucide-compatible set, at 16 px with 1.5 px stroke, and 14 px in compact rows (Orca: lucide only, size-4/3.5).
- One spinner design everywhere.
- **Agent logos** are monochrome glyphs at rest and turn brand-colored only on hover or selection, so 6 agent logos don't turn the sidebar into confetti.
- **Keycap hints** appear in menus, empty states and on hold-⌘. Their style is 11 px mono on a 1 px bordered chip, with the platform's own symbols (⌘⌥⇧ or Ctrl+).
- **Tooltips** name the control and its shortcut only. They appear after 600 ms, on top, offset 4 px (Orca rules).

**Copy and feedback timing (Orca's UX rules, worth adopting verbatim)**

| Action duration | Feedback |
|---|---|
| 0–100 ms | Nothing |
| ≤1 s | Disabled control |
| 1–3 s | Disabled plus spinner |
| More than 3 s | Staged labels ("Creating worktree… Running setup… Starting agent…") |

- Cancel, Dismiss and Close are never colored or destructive.
- Copy never overclaims ("Sent to agent", not "Fixed").
- Interfaces must stay usable at 50–200 ms of SSH latency.
