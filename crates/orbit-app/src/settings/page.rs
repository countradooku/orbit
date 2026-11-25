//! The settings page: GPUI Kit's `Settings` (sidebar, search, pages and
//! groups) over [`super::SettingsModel`].
//!
//! Every field reads and writes the model through closures, so the page is
//! rebuilt from the current values on each render and never holds a copy.
//! Item lists are fixed per page (no item appears or disappears with state),
//! because field state is keyed by position.

use std::rc::Rc;

use gpui_kit::component::button::Button;
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::setting::{
    NumberFieldOptions, RenderOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings as SettingsView,
};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::ConnectionStatus;
use orbit_proto::*;

use super::{ZOOM_STEPS, get, update};
use crate::actions::*;
use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::theme;

pub fn render(app: Entity<AppState>, _: &mut Window, cx: &mut App) -> impl IntoElement {
    div().size_full().child(SettingsView::new("orbit-settings").pages([
        general(),
        appearance(cx),
        composer(cx),
        agents(&app, cx),
        workspace(cx),
        notifications(),
        keyboard(),
        remote(&app, cx),
        data(cx),
        about(cx),
    ]))
}

// ── Pages ────────────────────────────────────────────────────────────────

fn general() -> SettingPage {
    SettingPage::new("General").icon(Icon::new(IconName::Settings2)).groups([
        SettingGroup::new().title("New threads").item(
            SettingItem::new(
                "Access",
                choice(
                    Access::ALL.into_iter().filter(|a| *a != Access::Full).map(|a| (a, a.label().into())).collect(),
                    |s| s.agents.default_access,
                    |s, v| s.agents.default_access = v.for_new_thread(),
                    Access::Ask,
                ),
            )
            .description("What an agent may do without asking in a new thread. Full access is always chosen per thread.")
            .keywords(["permission", "approval", "default"]),
        ),
        SettingGroup::new().title("Confirmations").item(
            SettingItem::new(
                "Confirm before deleting a thread",
                switch(|s| s.general.confirm_delete, |s, v| s.general.confirm_delete = v, true),
            )
            .description("Deleting removes the thread’s history, checkpoints and worktree."),
        ),
    ])
}

fn appearance(cx: &App) -> SettingPage {
    let zoom_keys = format!("{0}+= and {0}+- zoom too; {0}+0 resets.", super::mod_key());
    SettingPage::new("Appearance").icon(Icon::new(IconName::Palette)).groups([
        SettingGroup::new().title("Theme").items([
            SettingItem::new(
                "Appearance",
                choice(
                    vec![
                        (ColorMode::System, "Match system".into()),
                        (ColorMode::Light, "Light".into()),
                        (ColorMode::Dark, "Dark".into()),
                    ],
                    |s| s.appearance.mode,
                    |s, v| s.appearance.mode = v,
                    ColorMode::System,
                ),
            )
            .keywords(["dark mode", "light mode", "color"]),
            SettingItem::new("Light theme", theme_field(false, cx)).description("Used while the appearance is light."),
            SettingItem::new("Dark theme", theme_field(true, cx)).description("Used while the appearance is dark."),
        ]),
        SettingGroup::new().title("Text").items([
            SettingItem::new(
                "Interface size",
                choice(
                    ZOOM_STEPS.iter().map(|&z| (z, format!("{z} %").into())).collect(),
                    |s| s.appearance.ui_scale,
                    |s, v| s.appearance.ui_scale = v,
                    100,
                ),
            )
            .description(format!("Scales text, spacing and controls together. {zoom_keys}"))
            .keywords(["zoom", "font size", "scale"]),
            SettingItem::new(
                "Code font size",
                SettingField::number_input(
                    NumberFieldOptions { min: f64::from(CODE_FONT_MIN), max: f64::from(CODE_FONT_MAX), step: 1. },
                    |cx| f64::from(get(cx).appearance.code_font_size),
                    |v, cx| {
                        update(cx, |s| {
                            s.appearance.code_font_size = (v.round() as u16).clamp(CODE_FONT_MIN, CODE_FONT_MAX)
                        })
                    },
                )
                .default_value(13.),
            )
            .description("Code, diffs and command output, in pixels before zoom.")
            .keywords(["monospace", "font"]),
        ]),
        SettingGroup::new().title("Layout").item(
            SettingItem::new(
                "Chat width",
                choice(
                    vec![
                        (ChatWidth::Narrow, "Narrow".into()),
                        (ChatWidth::Comfortable, "Comfortable".into()),
                        (ChatWidth::Wide, "Wide".into()),
                        (ChatWidth::Full, "Full width".into()),
                    ],
                    |s| s.appearance.chat_width,
                    |s, v| s.appearance.chat_width = v,
                    ChatWidth::Comfortable,
                ),
            )
            .description("Width of the conversation, the inbox and the start page.")
            .keywords(["column", "reading width"]),
        ),
        SettingGroup::new().title("Motion").item(
            SettingItem::new(
                "Reduce motion",
                choice(
                    vec![
                        (Motion::System, "Match system".into()),
                        (Motion::Reduce, "Reduce".into()),
                        (Motion::Allow, "Allow".into()),
                    ],
                    |s| s.appearance.motion,
                    |s, v| s.appearance.motion = v,
                    Motion::System,
                ),
            )
            .description("With motion reduced, panels and dialogs appear without animating.")
            .keywords(["animation", "accessibility"]),
        ),
    ])
}

fn composer(cx: &App) -> SettingPage {
    let m = super::mod_key();
    let (_, alternate) = super::send_keys(cx);
    SettingPage::new("Composer").icon(Icon::new(Lucide::SquarePen)).group(
        SettingGroup::new().title("Sending").items([
            SettingItem::new(
                "Send with",
                choice(
                    vec![
                        (SubmitKey::Enter, "Enter".into()),
                        (SubmitKey::ModEnter, format!("{m}+Enter").into()),
                    ],
                    |s| s.composer.submit,
                    |s, v| s.composer.submit = v,
                    SubmitKey::Enter,
                ),
            )
            .description(format!(
                "With Enter, Shift+Enter starts a new line. With {m}+Enter, Enter starts a new line."
            ))
            .keywords(["submit", "enter", "newline", "keyboard"]),
            SettingItem::new(
                "While an agent works",
                choice(
                    vec![(FollowUp::Queue, "Queue the message".into()), (FollowUp::Steer, "Steer the turn".into())],
                    |s| s.composer.follow_up,
                    |s, v| s.composer.follow_up = v,
                    FollowUp::Queue,
                ),
            )
            .description(format!("What sending does during a turn; {alternate} does the other."))
            .keywords(["steer", "queue", "follow-up"]),
        ]),
    )
}

fn agents(app: &Entity<AppState>, cx: &App) -> SettingPage {
    SettingPage::new("Agents")
        .icon(Icon::new(IconName::Bot))
        // ACP agents are instances (presets or configured), not one program.
        .groups(ProviderKind::ALL.into_iter().filter(|k| *k != ProviderKind::Acp).map(|kind| provider_group(app, kind, cx)))
}

fn provider_group(app: &Entity<AppState>, kind: ProviderKind, cx: &App) -> SettingGroup {
    let info = app.read(cx).state().provider(kind).cloned().unwrap_or_default();
    let program = match kind {
        ProviderKind::Claude => "claude",
        ProviderKind::Codex => "codex",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Pi => "pi",
        ProviderKind::Acp => "agent",
    };
    let mut facts = Vec::new();
    if let Some(version) = &info.version {
        facts.push(version.clone());
    }
    if let Some(account) = &info.account {
        facts.push(account.clone());
    }
    if info.installed {
        facts.push(format!("{} model{}", info.models.len(), if info.models.len() == 1 { "" } else { "s" }));
    }
    if let Some(message) = &info.message {
        facts.push(message.clone());
    }
    let found = match (&info.binary, info.installed) {
        (Some(path), true) => format!("Found {path}."),
        _ => "Not found on PATH or in the usual install locations.".to_owned(),
    };
    let key = |part: &str| SharedString::from(format!("agent-{program}-{part}"));
    let detected: SharedString = info.binary.clone().unwrap_or_else(|| program.to_owned()).into();
    let app = app.clone();
    SettingGroup::new().title(kind.label()).items([
        SettingItem::new("Status", provider_status(app, kind))
            .description(if facts.is_empty() { "Checking…".to_owned() } else { facts.join(" · ") }),
        SettingItem::new(
            "Program",
            text_field(
                key("binary"),
                detected,
                false,
                move |cx| get(cx).provider(kind).binary.clone(),
                move |v, cx| update(cx, |s| s.provider_mut(kind).binary = v),
            ),
        )
        .description(format!("Path to the {program} executable. Leave empty to find it automatically. {found}"))
        .keywords(["binary", "path", "executable", "cli"]),
        SettingItem::new("Environment", env_field(key("env"), kind))
            .layout(Axis::Vertical)
            .description(
                "One NAME=value per line, added to the agent’s environment, for example a config folder, a proxy \
                 or an API base URL. Stored in plain text in Orbit’s data folder.",
            )
            .keywords(["env", "variables", "proxy", "CLAUDE_CONFIG_DIR", "CODEX_HOME"]),
    ])
}

fn provider_status(app: Entity<AppState>, kind: ProviderKind) -> SettingField<SharedString> {
    SettingField::render(move |_, _, cx| {
        let theme = cx.theme().clone();
        let info = app.read(cx).state().provider(kind).cloned().unwrap_or_default();
        let checking = info.message.as_deref() == Some("Checking…");
        let (label, color) = match (info.installed, info.signed_in) {
            _ if checking => ("Checking", theme.muted_foreground),
            (false, _) => ("Not found", theme.muted_foreground),
            (true, Some(false)) => ("Signed out", theme.warning),
            (true, _) => ("Ready", theme.success),
        };
        let app = app.clone();
        h_flex()
            .gap_3()
            .child(h_flex().gap_1p5().text_sm().child(div().size_2().rounded_full().bg(color)).child(label))
            .child(
                Button::new(("check-provider", kind as usize))
                    .outline()
                    .small()
                    .icon(IconName::RefreshCw)
                    .label("Check again")
                    .on_click(move |_, window, cx| {
                        app.update(cx, |app, cx| app.run(Command::RefreshProviders, window, cx))
                    }),
            )
    })
}

fn workspace(cx: &App) -> SettingPage {
    let default_root = super::data_dir(cx).join("worktrees").display().to_string();
    SettingPage::new("Workspace & Git").icon(Icon::new(Lucide::GitBranch)).groups([
        SettingGroup::new().title("Worktrees").items([
            SettingItem::new(
                "Start new threads on a worktree",
                switch(|s| s.workspace.worktree_by_default, |s, v| s.workspace.worktree_by_default = v, false),
            )
            .description("Each new thread in a project gets its own branch and folder, so parallel agents never edit the same files.")
            .keywords(["isolation", "branch"]),
            SettingItem::new(
                "Worktree folder",
                text_field(
                    "worktree-root".into(),
                    default_root.clone(),
                    false,
                    |cx| get(cx).workspace.worktree_root.clone(),
                    |v, cx| update(cx, |s| s.workspace.worktree_root = v),
                ),
            )
            .description(format!(
                "New worktrees go in <folder>/<project>/<branch>. Leave empty for {default_root}. Existing worktrees stay where they are."
            ))
            .keywords(["location", "path", "disk"]),
        ]),
        SettingGroup::new().title("Review").items([
            SettingItem::new("Side-by-side diffs", switch(|s| s.review.split, |s, v| s.review.split = v, false))
                .description("Show old and new text in two columns instead of one.")
                .keywords(["split", "unified", "diff"]),
            SettingItem::new(
                "Collapse files when a review opens",
                switch(|s| s.review.collapse_files, |s, v| s.review.collapse_files = v, false),
            )
            .description("Start with every file folded to its header, to scan a large change first."),
        ]),
        SettingGroup::new().title("Git").items([
            SettingItem::new(
                "Git program",
                text_field(
                    "git-binary".into(),
                    "git",
                    false,
                    |cx| get(cx).advanced.git_binary.clone(),
                    |v, cx| update(cx, |s| s.advanced.git_binary = v),
                ),
            )
            .description("Leave empty to use git from PATH."),
            SettingItem::new(
                "GitHub CLI program",
                text_field(
                    "gh-binary".into(),
                    "gh",
                    false,
                    |cx| get(cx).advanced.gh_binary.clone(),
                    |v, cx| update(cx, |s| s.advanced.gh_binary = v),
                ),
            )
            .description("Used to open pull requests with your own sign-in. Leave empty to use gh from PATH.")
            .keywords(["gh", "pull request"]),
            SettingItem::new(
                "Checkpoint file limit",
                SettingField::number_input(
                    NumberFieldOptions { min: 0., max: 100_000., step: 10. },
                    |cx| f64::from(get(cx).advanced.checkpoint_file_limit_mb),
                    |v, cx| update(cx, |s| s.advanced.checkpoint_file_limit_mb = v.max(0.).round() as u32),
                )
                .default_value(50.),
            )
            .description("New untracked files larger than this many MB are left out of turn checkpoints, so a stray dataset can’t fill the disk. 0 keeps every file.")
            .keywords(["size", "large files", "MB"]),
        ]),
    ])
}

fn notifications() -> SettingPage {
    SettingPage::new("Notifications").icon(Icon::new(IconName::Bell)).group(
        SettingGroup::new()
            .title("Notifications")
            .description(
                "Shown in the window and posted to the system notification center; clicking one opens the thread.                  Nothing is posted for the thread you are looking at.",
            )
            .items([
                SettingItem::new(
                    "Agent needs you",
                    switch(|s| s.notifications.requests, |s, v| s.notifications.requests = v, true),
                )
                .description("An approval or a question is waiting.")
                .keywords(["approval", "question", "request"]),
                SettingItem::new(
                    "Turn finished",
                    switch(|s| s.notifications.finished, |s, v| s.notifications.finished = v, true),
                )
                .description("An agent finished and its result is ready to read.")
                .keywords(["done", "completion"]),
                SettingItem::new(
                    "Turn failed",
                    switch(|s| s.notifications.failed, |s, v| s.notifications.failed = v, true),
                )
                .description("A turn failed, or an agent hit its usage limit.")
                .keywords(["error", "rate limit", "quota"]),
            ]),
    )
}

fn keyboard() -> SettingPage {
    let m = super::mod_key();
    let shortcut = |title: &'static str, action: Box<dyn Action>| {
        SettingItem::new(title, bound_key(action)).keywords(["shortcut", "keybinding"])
    };
    SettingPage::new("Keyboard").icon(Icon::new(Lucide::Keyboard)).resettable(false).groups([
        SettingGroup::new().title("Navigation").items([
            shortcut("Search and commands", Box::new(OpenPalette)),
            shortcut("New thread", Box::new(NewThread)),
            shortcut("Inbox", Box::new(GoInbox)),
            shortcut("Fleet", Box::new(GoFleet)),
            shortcut("Next thread", Box::new(NextThread)),
            shortcut("Previous thread", Box::new(PreviousThread)),
            shortcut("Add project", Box::new(AddProject)),
            shortcut("Settings", Box::new(OpenSettings)),
        ]),
        SettingGroup::new().title("Threads").items([
            shortcut("Focus the composer", Box::new(FocusComposer)),
            shortcut("Stop the running turn", Box::new(StopTurn)),
            shortcut("Review changes", Box::new(ToggleReview)),
        ]),
        SettingGroup::new()
            .title("Composer")
            .description("These follow Composer › Send with.")
            .items([
                SettingItem::new("Send", composer_key(false)).keywords(["submit", "enter"]),
                SettingItem::new("Queue or steer instead", composer_key(true))
                    .description("While an agent works, does the opposite of sending.")
                    .keywords(["steer", "queue"]),
                SettingItem::new(
                    "New line",
                    SettingField::render(|_, _, cx| match get(cx).composer.submit {
                        SubmitKey::Enter => key_chip("shift-enter"),
                        SubmitKey::ModEnter => key_chip("enter"),
                    }),
                ),
            ]),
        SettingGroup::new().title("View").items([
            shortcut("Toggle sidebar", Box::new(ToggleSidebar)),
            shortcut("Zoom in", Box::new(ZoomIn)),
            shortcut("Zoom out", Box::new(ZoomOut)),
            shortcut("Actual size", Box::new(ResetZoom)),
            shortcut("Quit", Box::new(Quit)),
        ]),
    ])
    .description(format!("Shortcuts shown as this keyboard types them. {m} is the command modifier on this platform."))
}

fn remote(app: &Entity<AppState>, cx: &App) -> SettingPage {
    let (host, from_env) = super::connected_host(cx);
    let saved = super::remote(cx);
    let pending = saved.host.trim() != host.as_deref().unwrap_or("") && !from_env;
    let app = app.clone();
    SettingPage::new("Remote").icon(Icon::new(IconName::Globe)).resettable(false).groups([
        SettingGroup::new()
            .title("Connection")
            .description("Run agents on another machine or in WSL with orbitd, and drive them from here.")
            .items([
                SettingItem::new(
                    "Engine",
                    SettingField::render(move |_, _, cx| {
                        let theme = cx.theme().clone();
                        let (host, from_env) = super::connected_host(cx);
                        let status = match app.read(cx).connection() {
                            Some(ConnectionStatus::Connected) | None => "Connected".to_owned(),
                            Some(ConnectionStatus::Connecting) => "Connecting…".to_owned(),
                            Some(ConnectionStatus::Disconnected { reason }) => format!("Reconnecting: {reason}"),
                        };
                        let text = match host {
                            None => "This computer".to_owned(),
                            Some(host) if from_env => format!("{host} (from ORBIT_HOST) · {status}"),
                            Some(host) => format!("{host} · {status}"),
                        };
                        div().text_sm().text_color(theme.muted_foreground).child(text)
                    }),
                )
                .description(if pending {
                    "The saved host takes effect the next time Orbit starts."
                } else {
                    "Where threads, worktrees and agents run."
                }),
                SettingItem::new(
                    "Host",
                    text_field(
                        "remote-host".into(),
                        "ws://host:7840/",
                        false,
                        |cx| super::remote(cx).host,
                        |v, cx| super::set_remote(cx, |r| r.host = v),
                    ),
                )
                .description("Leave empty to run agents on this computer. Applies the next time Orbit starts; ORBIT_HOST overrides it.")
                .disabled(from_env)
                .keywords(["orbitd", "server", "wsl", "ssh", "url"]),
                SettingItem::new(
                    "Access token",
                    text_field(
                        "remote-token".into(),
                        "Token",
                        true,
                        |cx| super::remote(cx).token,
                        |v, cx| super::set_remote(cx, |r| r.token = v),
                    ),
                )
                .description("Printed by orbitd --print-token on the host. Kept in Orbit’s data folder on this computer.")
                .disabled(from_env)
                .keywords(["password", "secret"]),
            ]),
        SettingGroup::new().title("Set up a host").item(
            SettingItem::new("Start orbitd", copy_text("copy-orbitd", "orbitd --listen 127.0.0.1:7840"))
                .description(
                    "Run this on the other machine, then reach it through an SSH tunnel or a private network such as \
                     Tailscale. It listens on loopback only unless you choose another address.",
                ),
        ),
    ])
}

fn data(cx: &App) -> SettingPage {
    let dir = super::data_dir(cx);
    let (host, _) = super::connected_host(cx);
    let reveal_label = if cfg!(target_os = "windows") {
        "Show in Explorer"
    } else if cfg!(target_os = "macos") {
        "Show in Finder"
    } else {
        "Show in file manager"
    };
    let folder_note = match &host {
        Some(host) => format!("{}. Threads and worktrees live on {host}.", dir.display()),
        None => dir.display().to_string(),
    };
    SettingPage::new("Data & Privacy").icon(Icon::new(IconName::HardDrive)).resettable(false).groups([
        SettingGroup::new().title("Data").items([
            SettingItem::new(
                "Data folder",
                SettingField::render(move |_, _, _| {
                    let dir = dir.clone();
                    Button::new("reveal-data")
                        .outline()
                        .small()
                        .icon(IconName::FolderOpen)
                        .label(reveal_label)
                        .on_click(move |_, _, cx| cx.reveal_path(&dir))
                }),
            )
            .description(folder_note)
            .keywords(["location", "storage"]),
            SettingItem::new(
                "Storage used",
                SettingField::render(|_, _, cx| {
                    let text = match super::storage(cx) {
                        Some(bytes) => format_bytes(bytes),
                        None => "Measuring…".to_owned(),
                    };
                    muted_value(text, cx)
                }),
            )
            .description("Threads, attachments, checkpoints and the worktrees Orbit created.")
            .keywords(["disk", "size"]),
        ]),
        SettingGroup::new().title("Privacy").items([
            SettingItem::new("Telemetry", SettingField::render(|_, _, cx| muted_value("None".to_owned(), cx)))
                .description(
                    "Orbit collects nothing about you or how you use it, and has no account. It talks only to the agent \
                     CLIs you installed, which use your own sign-in, and to git hosts through your own git and gh.",
                )
                .keywords(["analytics", "tracking", "privacy"]),
        ]),
    ])
}

fn about(cx: &App) -> SettingPage {
    let version = env!("CARGO_PKG_VERSION");
    let (host, _) = super::connected_host(cx);
    let engine = match host {
        Some(host) => format!("orbitd at {host}"),
        None => "In this app".to_owned(),
    };
    let platform = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    SettingPage::new("About").icon(Icon::new(IconName::Info)).resettable(false).group(
        SettingGroup::new().title("Orbit").items([
            SettingItem::new("Version", copy_text("copy-version", version)),
            SettingItem::new("Protocol", static_value(format!("OWP/{PROTOCOL_VERSION}"))),
            SettingItem::new("Engine", static_value(engine)),
            SettingItem::new("Platform", static_value(platform)),
        ]),
    )
}

// ── Fields ───────────────────────────────────────────────────────────────

fn switch(
    value: impl Fn(&Settings) -> bool + 'static,
    set: impl Fn(&mut Settings, bool) + 'static,
    default: bool,
) -> SettingField<bool> {
    SettingField::switch(move |cx| value(get(cx)), move |v, cx| update(cx, |s| set(s, v))).default_value(default)
}

fn position<T: PartialEq>(values: &[T], value: &T) -> usize {
    values.iter().position(|v| v == value).unwrap_or(0)
}

/// A dropdown over a small set of values, stored by position.
fn choice<T: Copy + PartialEq + 'static>(
    options: Vec<(T, SharedString)>,
    value: impl Fn(&Settings) -> T + 'static,
    set: impl Fn(&mut Settings, T) + 'static,
    default: T,
) -> SettingField<SharedString> {
    let values: Rc<Vec<T>> = Rc::new(options.iter().map(|(v, _)| *v).collect());
    let items = options.into_iter().enumerate().map(|(ix, (_, label))| (SharedString::from(ix.to_string()), label)).collect();
    let default_ix = position(&values, &default).to_string();
    let read = values.clone();
    SettingField::dropdown(
        items,
        move |cx| SharedString::from(position(&read, &value(get(cx))).to_string()),
        move |picked: SharedString, cx| {
            if let Some(&v) = picked.parse::<usize>().ok().and_then(|ix| values.get(ix)) {
                update(cx, |s| set(s, v));
            }
        },
    )
    .default_value(default_ix)
}

fn theme_field(dark: bool, cx: &App) -> SettingField<SharedString> {
    let items = theme::names(dark, cx).into_iter().map(|name| (name.clone(), name)).collect();
    let orbit = if dark { theme::ORBIT_DARK } else { theme::ORBIT_LIGHT };
    SettingField::scrollable_dropdown(
        items,
        move |cx| {
            let a = &get(cx).appearance;
            theme::resolve(if dark { &a.dark_theme } else { &a.light_theme }, dark, cx)
        },
        move |picked: SharedString, cx| {
            let name = if picked.as_ref() == orbit { String::new() } else { picked.to_string() };
            update(cx, |s| {
                if dark {
                    s.appearance.dark_theme = name;
                } else {
                    s.appearance.light_theme = name;
                }
            });
        },
    )
    .default_value(orbit)
}

struct TextFieldState {
    input: Entity<InputState>,
    _change: Subscription,
}

/// A single-line text field with a placeholder (the built-in input field
/// has none). It writes on every change and follows outside changes while
/// it is not focused.
fn text_field(
    key: SharedString,
    placeholder: impl Into<SharedString>,
    masked: bool,
    value: impl Fn(&App) -> String + 'static,
    set: impl Fn(String, &mut App) + 'static,
) -> SettingField<SharedString> {
    let placeholder: SharedString = placeholder.into();
    let set = Rc::new(set);
    SettingField::render(move |options: &RenderOptions, window, cx| {
        let current: SharedString = value(cx).into();
        let state = window.use_keyed_state(key.clone(), cx, {
            let (set, current, placeholder) = (set.clone(), current.clone(), placeholder.clone());
            move |window, cx| {
                let input =
                    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(current).masked(masked));
                let _change = cx.subscribe(&input, move |_, input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        let text = input.read(cx).value().to_string();
                        set(text, cx);
                    }
                });
                TextFieldState { input, _change }
            }
        });
        let input = state.read(cx).input.clone();
        let presentation = input.read(cx).presentation();
        if !presentation.focus_handle().is_focused(window) && input.read(cx).value() != current {
            input.update(cx, |input, cx| input.set_value(current.clone(), window, cx));
        }
        if presentation.placeholder() != &placeholder {
            input.update(cx, |input, cx| input.set_placeholder(placeholder.clone(), window, cx));
        }
        Input::new(&input)
            .with_size(options.size())
            .disabled(options.is_disabled())
            .when(masked, |input| input.mask_toggle())
            .map(|input| if options.layout() == Axis::Horizontal { input.w_80() } else { input.w_full() })
    })
}

struct EnvFieldState {
    input: Entity<TextareaState>,
    _change: Subscription,
}

/// `NAME=value` lines for one provider's environment.
fn env_field(key: SharedString, kind: ProviderKind) -> SettingField<SharedString> {
    SettingField::render(move |options: &RenderOptions, window, cx| {
        let env = get(cx).provider(kind).env;
        let state = window.use_keyed_state(key.clone(), cx, {
            let text = format_env(&env);
            move |window, cx| {
                let input = cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(2, 10)
                        .placeholder("HTTPS_PROXY=http://proxy.example:8080")
                        .default_value(text)
                });
                let _change = cx.subscribe(&input, move |_, input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        let env = parse_env(&input.read(cx).value());
                        update(cx, |s| s.provider_mut(kind).env = env);
                    }
                });
                EnvFieldState { input, _change }
            }
        });
        let input = state.read(cx).input.clone();
        let focused = input.read(cx).presentation().focus_handle().is_focused(window);
        if !focused && parse_env(&input.read(cx).value()) != env {
            input.update(cx, |input, cx| input.set_value(format_env(&env), window, cx));
        }
        div()
            .w_full()
            .font_family(cx.theme().mono_font_family.clone())
            .child(Textarea::new(&input).disabled(options.is_disabled()))
    })
}

/// The key an action is bound to, read from the live keymap.
fn bound_key(action: Box<dyn Action>) -> SettingField<SharedString> {
    SettingField::render(move |_, window, cx| match Kbd::binding_for_action(action.as_ref(), Some("Workspace"), window) {
        Some(kbd) => kbd.into_any_element(),
        None => muted_value("Not bound".to_owned(), cx).into_any_element(),
    })
}

fn key_chip(keys: &'static str) -> AnyElement {
    match Keystroke::parse(keys) {
        Ok(stroke) => Kbd::new(stroke).into_any_element(),
        Err(_) => div().child(keys).into_any_element(),
    }
}

/// The composer's send key, or its alternate.
fn composer_key(alternate: bool) -> SettingField<SharedString> {
    SettingField::render(move |_, _, cx| match (get(cx).composer.submit, alternate) {
        (SubmitKey::Enter, false) => key_chip("enter"),
        (SubmitKey::Enter, true) | (SubmitKey::ModEnter, false) => key_chip(platform("cmd-enter", "ctrl-enter")),
        (SubmitKey::ModEnter, true) => key_chip(platform("cmd-shift-enter", "ctrl-shift-enter")),
    })
}

fn muted_value(text: String, cx: &App) -> Div {
    div().text_sm().text_color(cx.theme().muted_foreground).child(text)
}

fn static_value(text: String) -> SettingField<SharedString> {
    SettingField::render(move |_, _, cx| muted_value(text.clone(), cx))
}

fn copy_text(id: &'static str, text: &'static str) -> SettingField<SharedString> {
    SettingField::render(move |_, _, cx| {
        h_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(cx.theme().muted_foreground)
                    .child(text),
            )
            .child(Clipboard::new(id).value(text))
    })
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024. && unit + 1 < UNITS.len() {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} bytes") } else { format!("{value:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bytes_read_naturally() {
        assert_eq!(super::format_bytes(512), "512 bytes");
        assert_eq!(super::format_bytes(1536), "1.5 KB");
        assert_eq!(super::format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
