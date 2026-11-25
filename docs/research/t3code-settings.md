# T3 Code settings, mapped to Orbit

Inventory of every setting T3 Code exposes, from `packages/contracts/src/settings.ts`
(client schema `ClientSettingsSchema` at lines 416-623, server schema
`ServerSettings` at 1393-1647, per-driver provider settings at 720-950),
`packages/contracts/src/providerInstance.ts`, the desktop-only settings file
(`apps/desktop/src/settings/DesktopAppSettings.ts`) and the settings pages under
`apps/web/src/components/settings/`. See `t3code.md` section D for context.

Orbit's model is `crates/orbit-proto/src/settings.rs`; the page is
`crates/orbit-app/src/settings/page.rs`.

Status:

* **Implemented**: Orbit has the setting (possibly under a clearer name).
* **Changed**: the need is met differently, on purpose; the reason is given.
* **N/A**: the feature the setting configures does not exist in Orbit by
  design (no browser, no voice, no cloud relay, ...), so there is nothing to
  configure.
* **Deferred**: Orbit should have it but does not yet; listed so it is not
  forgotten.

A row with several keys counts once per key.

## How Orbit organizes settings

T3 Code splits settings into per-device *client* settings (localStorage or a
desktop IPC file) and per-environment *server* settings (`settings.json`), with
a scope picker, per-project overrides, inheritance and a managed-policy
overlay. Users reported not knowing which scope a value lived in.

Orbit keeps **one record per engine**: everything, including theme, zoom and
the submit key, lives in the engine's meta log and syncs to every client of
that engine (a remote host shows the same appearance everywhere). The only
per-device value is which host to connect to, which must be known before any
engine is reachable (`<data>/connection`, see Remote). Pages: General,
Appearance, Composer, Agents, Workspace & Git, Notifications, Keyboard,
Remote, Data & Privacy, About. Search covers titles, descriptions and
keywords.

## Client settings (`ClientSettingsSchema`)

| T3 key | Default | Status | Orbit |
| --- | --- | --- | --- |
| `notificationMode` | off | Changed | Notifications page: one switch per kind (needs you, finished, failed), all on by default. Posted to the OS notification center only while Orbit is in the background; the OS plays its own sound. T3's off-by-default hid the feature users asked for most (T3 #780). |
| `inAppNotificationsEnabled` | false | Changed | The Inbox is the in-app attention queue, always on; no transient toasts to dismiss. |
| `diffColorScheme` | red-green | Changed | Diff colors come from the theme's semantic tokens, so a colorblind-friendly palette is a theme choice. |
| `chatWidth` | comfortable | Implemented | Appearance › Chat width: Narrow, Comfortable, Wide, Full width. Applies to threads, inbox and home; the timeline re-measures. |
| `htmlPreviewWidth` | chat | N/A | No HTML preview pane. |
| `effortDisplay` | label | N/A | The composer shows the provider's own option labels (Orbit never hardcodes efforts). |
| `loadBalancingEnabled`, `loadBalancingWeights` | false | N/A | One client drives one engine; no pool of environments. |
| `appearanceContrast` | 100 | Changed | Contrast is a property of the theme; 21 themes plus Orbit's pair are bundled. |
| `panelAnimationDurationMs` | 0 | Changed | Appearance › Reduce motion (Match system / Reduce / Allow) through GPUI Kit's reduced-motion preference; durations stay the design system's. |
| `browserDefaultViewport`, `browserDefaultZoomFactor`, `browserDefaultAppearance`, `browserRecordingFrameRate`, `browserRecordingShowKeyPresses`, `browserRecordingShowMousePresses`, `browserLinkTarget`, `browserSearchEngine`, `browserCustomSearchUrl`, `browserAutoShowFloatingPreview`, `browserProfiles`, `browserDefaultProfileId` | | N/A | No embedded browser; links open in the system browser. |
| `confirmQuit` | hold | Deferred | Ctrl+Q quits at once (pending settings are flushed). A hold-to-quit guard is worth adding while turns run. |
| `confirmThreadArchive` | false | Changed | Archiving only hides a thread (nothing is deleted), so it never asks. |
| `confirmTerminalClose` | true | Deferred | Terminals close without asking. |
| `confirmThreadDelete` | true | Implemented | General › Confirm before deleting a thread. |
| `confirmThreadUnpin` | false | Changed | Unpinning is reversible; never asks. |
| `diffFilesCollapsed` | true | Implemented | Workspace & Git › Collapse files when a review opens (off by default: most turns are small). |
| `diffIgnoreWhitespace` | true | Deferred | The engine diffs exactly what changed; a whitespace toggle needs `git diff -w` support in the git layer. |
| `diffLayout` | stacked | Implemented | Workspace & Git › Side-by-side diffs (and the toggle in the review). |
| `environmentIdentificationMode` | artwork | N/A | One environment per window; the title bar and Remote page name it. |
| `glassOpacity` | 80 | N/A | No translucent surfaces. |
| `fontSizeInterface` | 16 | Implemented | Appearance › Interface size (75-150 %), scaling the base font so text, spacing and controls zoom together; also Ctrl+= / Ctrl+- / Ctrl+0. |
| `fontSizePrompt` | 14 | Changed | The composer follows the interface size; one zoom axis keeps hierarchy intact. |
| `fontSizeCode` | 13 | Implemented | Appearance › Code font size. |
| `fontSizeTerminal` | 12 | Changed | Follows the code font size. |
| `fontFamilyCode`, `fontFamilyComposer`, `fontFamilySans`, `fontFamilyTerminal` | "" | Changed | Fonts come from the theme (platform UI font and monospace). A theme file can set them. |
| `fontSmoothing` | true | N/A | The renderer always uses platform text rendering. |
| `onboardingCompletedAt` | null | N/A | No onboarding flow to track. |
| `persistComposerContextStrip` | false | N/A | The context meter is always visible (design rule: never hidden). |
| `favorites` | [] | Changed | The model picker lists the provider's catalog; the last used model and options become the next thread's defaults (T3 #6508). |
| `providerModelPreferences` | {} | Changed | Same as above: last used per thread, not a separate preference list. |
| `pullRequestMergeMethodOverrides` | {} | Deferred | Commit / push / PR exist; merge method choice belongs to the future merge flow. |
| `pullRequestLinksOpenInApp` | true | N/A | PR links open in the browser. |
| `composerRichTextEnabled` | true | Changed | Plain text composer with Markdown; nothing to toggle. |
| `sendShortcut` | enter | Implemented | Composer › Send with: Enter, or Ctrl+Enter (Cmd+Enter on macOS). T3's `mod-enter-multiline` variant is covered by the Ctrl+Enter choice. |
| `followUpBehavior` | steer | Implemented | Composer › While an agent works: Queue (default) or Steer; the alternate key does the other. |
| `sideThreadOpenTarget` | panel | N/A | Subagents appear nested in Fleet and in their parent's timeline. |
| `proactivePanelsEnabled` | false | N/A | Review opens on request (Ctrl+R) or from the thread. |
| `showSkillsInSlashMenu` | true | Changed | Slash commands and skills reported by the provider are always passed through intact. |
| `boringSidebarEnabled`, `boringSidebarMultilineRows` | false | N/A | One sidebar design with a fixed status lane. |
| `toolCallVisibility` | showAll | Changed | Tool calls are grouped per turn and collapsed; expand on demand. |
| `screenReaderAnnouncements` | status | Deferred | Accessibility names exist; live announcements need GPUI accessibility support. |
| `screenReaderWindowTitle` | false | Deferred | As above. |
| `threadDetailsUsageEnabled`, `threadDetailsItems` | | Changed | Usage, cost and context are always shown in the composer meter and the title bar. |
| `sidebarWorkingShelfEnabled` | true | Changed | Fleet and the Inbox replace the working shelf. |
| `sidebarProjectGroupingMode`, `sidebarProjectGroupingOverrides` | repository | N/A | Projects are local checkouts; one sidebar entry per checkout. |
| `sidebarProjectSortOrder`, `sidebarThreadSortOrder` | updated_at | Changed | Projects keep user order; threads sort pinned first, then most recent. |
| `timestampFormat` | locale | Changed | Relative times ("5m ago"), which need no format choice. |
| `voiceMicrophone`, `voiceMicrophones`, `voiceShortcutMode`, `speechLanguage`, `speechTranslateToEnglish`, `speechCustomWords`, `speechSilenceFiltering`, `speechStreaming`, `speechRemoveFillerWords`, `speechPostProcessingPrompt`, `speechCorrectionWord` | | N/A | No voice dictation. |
| `language` | en | Deferred | English only for now. |
| `snapShotEnabled`, `snapShotIncludeAccessibility`, `snapShotShortcut`, `snapShotPlaySound`, `snapShotSound`, `snapShotFlash`, `snapShotAnimations` | | N/A | No screen-capture feature. |
| `wordWrap` | true | Changed | Messages always wrap; diffs follow the review's own wrap control. |
| `mathRenderingEnabled` | false | N/A | No KaTeX. |

## Server settings (`ServerSettings`)

| T3 key | Default | Status | Orbit |
| --- | --- | --- | --- |
| `favorites`, `providerModelPreferences` | null | Changed | See the client rows: last used becomes default. |
| `worktreeCleanup`, `storageCleanup` (`worktreeKeepWhen`, `worktreeAfterDays`, `worktreeSettledAfterDays`, `worktreeOnMerge`, `worktreeOnDelete`, `worktreeUnchanged`, `browserArtifactsAfterDays`, `logsAfterDays`) | | Changed | Deleting a thread deletes its worktree, checkpoints and blobs; archiving keeps them. No time-based sweeper, so nothing disappears unexpectedly. Data & Privacy shows storage used. |
| `worktreesDirectory` | "" | Implemented | Workspace & Git › Worktree folder (empty = `<data>/worktrees`); goes into the engine's `GitConfig.worktree_root`. |
| `fastWorktreeCheckout` | false | N/A | macOS clone-file trick; Orbit uses plain `git worktree add` on every platform. |
| `previousWorktreesDirectories` | [] | Changed | Existing worktrees are tracked in the worktree registry by path, so moving the folder needs no history. |
| `responseStreamingMode` | paragraph | Changed | Always streams; Markdown is re-parsed for the last block only, so streaming is cheap. |
| `agentFeedback` | off | N/A | No feedback upload of any kind. |
| `enableProviderUpdateChecks` | true | Changed | Agents › Check again re-probes; Orbit never updates provider CLIs itself. |
| `continueThreadsAfterServerUpdate` | true | N/A | No self-updating server. |
| `keepAwakeWhileAgentsWork` | true | Deferred | Worth adding as a General switch. |
| `requireConnectDeviceApproval` | false | N/A | No T3 Connect relay; remote access is `orbitd` with a token. |
| `enableAgentBrowserAccess`, `projectAgentBrowserAccessOverrides` | true | N/A | No embedded browser. |
| `enablePreviewTunnel` | true | N/A | No preview tunnel. |
| `cloudSpeechCredentials`, `speechModelId`, `speechAcceleration`, `speechModelUnloadTimeout`, `speechProjectCustomWords`, `speechPostProcessingModelSelection` | | N/A | No voice. |
| `defaultAutoPull`, `projectAutoPullOverrides` | false | N/A | Orbit never pulls on its own. |
| `defaultProjectScripts`, `projectScriptOverrides` | [] | Changed | Setup, teardown and run scripts are per project (`Project.scripts`), edited with the project. |
| `defaultModelSelection`, `fallbackModelSelection` | null | Changed | Last used model is the default; a failed model surfaces as an error instead of a silent fallback. |
| `defaultRuntimeMode` | full-access | Implemented | General › Access for new threads: Readonly, Ask first (default), Auto-edit. **Full access is not offered**: `Settings::sanitized` lowers it to Auto-edit, so it is always an explicit per-thread choice (T3 defaulted to full access, `providerPolicy.ts:32`). |
| `projectSettingsOverrides`, `projectSettingsFolded` | {} | Changed | No per-project override layer; per-project data (scripts, base branch) lives on the project. |
| `enableAgentDeviceAccess`, `enableDeviceSupport`, `deviceOnboardingCompleted`, `deviceHosts`, `easDeviceHosts`, `simlockDeviceHosts`, `simlockDeviceHostsEnabled` | | N/A | No device streaming. |
| `sidebarAutoSettleAfterDays`, `sidebarAutoSettleOnMerge`, `sidebarAutoSettleKeepPullRequests` | 7 / true / false | Changed | Threads leave attention when read; archive is manual. |
| `compactIdleThreads`, `idleCompaction` | | Changed | Storage compacts by itself (dead bytes > live bytes); agent context compaction is the provider's `/compact`. |
| `snoozeLimitedThreads`, `snoozePresets` | | Changed | Rate-limited threads show the reset time in the Inbox. |
| `autoResumeLimitedThreads` | false | Changed | Always on: a rate-limited thread resumes its queue when the quota resets (T3 asked users to opt in). |
| `hiddenThreadTags`, `hideTaggedThreads` | | N/A | No thread tags. |
| `guestSharingEnabled`, `linkedEnvironmentsEnabled`, `workflowsEnabled`, `workflowSkippedRunsKept`, `promptSuggestionsEnabled` | false | N/A | Labs features Orbit does not have. |
| `backgroundActivity`, `backgroundActivityProfile`, `automaticGitFetchInterval`, `providerHealthRefreshInterval` | balanced / 30 s / 5 min | Changed | Nothing polls: git refreshes from a debounced file watcher and providers are probed at start and on request, so there is no activity profile to tune. |
| `defaultTheme`, `defaultThemeSetAt`, `defaultAppearance`, `defaultAppearanceSetAt` | | Implemented | Appearance › Appearance and Light/Dark theme live in the engine's settings, so a host's appearance reaches every client. |
| `environmentIcon` | null | N/A | One environment per window. |
| `defaultThreadEnvMode` | | Implemented | Workspace & Git › Start new threads on a worktree. |
| `newWorktreesStartFromOrigin` | true | Changed | New worktrees fork from the project's detected base branch. |
| `worktreeSubmodules` | | Deferred | Submodules are not initialized in new worktrees yet. |
| `addProjectBaseDirectory`, `ghqCloneLayout` | "" / false | N/A | Orbit never clones; projects are existing checkouts. |
| `textGenerationModelSelection`, `threadTitleInstructions`, `textGenerators` | | Changed | Titles come from the first message; no extra model calls on the user's quota. |
| `branchNamingMode`, `branchNamePrefix`, `branchNameInstructions` | static / "t3" | Deferred | Worktree branches use a fixed scheme; a prefix setting fits Workspace & Git. |
| `removeAgentCreditsOnMerge`, `sourceControlWritingStyle`, `sourceControlWriterModelSelection` | | Changed | Commit messages are written by the user or the agent in the thread; Orbit adds nothing. |
| `reviewGuidesEnabled`, `reviewGuideModelSelection`, `reviewGuideInstructions` | false | N/A | No AI review guides. |
| `pullRequestMergeMethod` | null | Deferred | See `pullRequestMergeMethodOverrides`. |
| `providerInstances` (`enabled`, `displayName`, `accentColor`, `environment`, `config`) | | Implemented | Agents page, one group per provider: status (installed, version, account, model count, message) with Check again, **Program** (binary path, `config.binaryPath`) and **Environment** (`NAME=value` lines, `environment`). Both reach `SessionSpec.binary`/`env` and `provider::probe`. Multiple instances per driver, display names and accent colors are not offered: Orbit has one instance per provider. |
| `config.homePath` (CODEX_HOME, CLAUDE_CONFIG_DIR), `config.shadowHomePath` | | Changed | Set them as environment variables on the provider; one mechanism instead of three fields. |
| `config.launchArgs` | | Changed | Not offered: Orbit owns the CLI flags of the protocol it speaks (stream format, permission mode), and arbitrary arguments can break it. Use environment variables, or point Program at a wrapper script. |
| `config.customModels` | [] | Changed | Models come from the provider's live catalog. |
| `config.autoCompactWindow`, `config.idleCompaction` (Claude) | | Changed | The provider's own settings and `/compact` apply; Orbit passes them through. |
| `config.authMethod`, `apiKey`, `gcpProject`, `gcpLocation` (Antigravity) | | N/A | Provider not supported; account sign-in stays in each provider's own CLI. |
| `observability` (`otlpTracesUrl`, `otlpMetricsUrl`, `otlpLogsUrl`) | "" | N/A | Orbit emits no telemetry, so there is nothing to export (Data & Privacy › Telemetry: None). |
| `sourceControlHosts`, `offeredSourceControlProviders` | | Changed | Git hosting goes through the user's own `git` and `gh` (Workspace & Git › Git program, GitHub CLI program). |
| `usageLimitSources`, `usageHistoryRetentionDays`, `usagePriceOverrides`, `usageModelAliases`, `usageModelFilter` | | Changed | Usage and quota come from the providers' own reports and are shown live; no usage database to configure. |
| `secretProviders` | {} | N/A | No secret manager integration; provider credentials stay in each CLI's own store. |

## Desktop settings file and policy

| T3 key | Status | Orbit |
| --- | --- | --- |
| `localEnvironmentEnabled` | Implemented | Remote › Host empty runs the engine in-process. |
| `serverExposureMode` (local-only / network-accessible), `tailscaleServeEnabled`, `tailscaleServePort` | Changed | `orbitd --listen ADDR`, loopback by default; Remote › Set up a host explains SSH tunnels and Tailscale. |
| `wslBackendEnabled`, `wslMode`, `wslDistro`, `wslOnly` | Changed | Run `orbitd` inside WSL and connect to it from Remote. |
| `linuxPasswordStore` | N/A | Orbit stores no credentials of its own besides the host token. |
| window bounds / maximized | Deferred | The window opens at a default size. |
| `updateChannel` | N/A | No self-updater. |
| Managed policy (`policy.json`, registry, locked rows) | N/A | No organization management. |
| Telemetry (`T3CODE_TELEMETRY_ENABLED`, PostHog) | Changed | There is no telemetry to turn off: Data & Privacy says so plainly (T3 #4123). |

## Keybindings, themes and other settings pages

| T3 feature | Status | Orbit |
| --- | --- | --- |
| Settings › Keybindings (view) | Implemented | Keyboard page lists every command with the key the live keymap resolves, plus the composer keys that follow the Send with setting. |
| Rebinding, `when` clauses, `keybindings.json` | Deferred | Read-only for now; rebinding needs a persisted keymap. |
| Theme editor, custom CSS, VS Code theme import, Open VSX search | Changed | 23 bundled themes (Orbit Light/Dark plus GPUI Kit's collection), chosen separately for light and dark; GPUI Kit theme JSON is the extension format. |
| Server-published environment themes | Implemented | Theme choices are engine settings, so a host's theme reaches every client. |
| Settings scope picker and per-row reset | Changed | One scope. Each page has GPUI Kit's Reset all for values that differ from defaults. |
| Settings search in the command palette | Changed | Settings has its own search; the palette opens Settings. |
| Archived threads page | Deferred | Archived threads are hidden; a page to browse and unarchive them is still to come. |
| Maintenance › Diagnostics | Deferred | |
| About › version, licenses | Implemented (version, protocol, engine, platform) | Open-source license list deferred. |

## Additions from the feedback research

Orchestrator users asked for these across tools
([orchestrator-feedback.md](orchestrator-feedback.md)); all are in Orbit:

| Request | Orbit |
| --- | --- |
| Enter vs Ctrl+Enter to send (Claude #8967, 167 reactions) | Composer › Send with |
| Default access, never Full (T3 full-access default) | General › Access for new threads |
| OS notifications for requests, finishes, failures (T3 #780) | Notifications page; failures are a separate kind T3 lacks |
| Font size / interface zoom (Claude #48237) | Appearance › Interface size, Code font size, Ctrl+= / Ctrl+- / Ctrl+0 |
| Chat width (Codex #16669) | Appearance › Chat width |
| Themes (Claude #79305) | Appearance › Appearance, Light theme, Dark theme |
| Reduced motion | Appearance › Reduce motion |
| Worktree location (Claude #10599) and worktree by default | Workspace & Git › Worktree folder, Start new threads on a worktree |
| Provider binary paths and extra env (the inherited-env 400s) | Agents › Program, Environment |
| Telemetry: none | Data & Privacy › Telemetry: None |
| Remote host | Remote › Host, Access token |
| Keybinding viewer | Keyboard page |

## Totals

Counted per key in the three T3 tables above (client, server, desktop and
policy) plus the feature table:

| Status | Count |
| --- | --- |
| Implemented | 25 |
| Changed | 89 |
| N/A | 94 |
| Deferred | 18 |
| **Total** | **226** |
