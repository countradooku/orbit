//! Installing a plugin: picking a source, the consent dialog that shows
//! what it asks to do, and the plugin's log.
//!
//! A source is previewed on the host first (nothing is installed). A plugin
//! that asks for no capabilities (themes, snippets, prompts) installs at
//! once; anything else shows the consent dialog, which lists every
//! capability and every program it would run. Folders and archives are
//! packed here and sent whole, so the same flow works for a remote host.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;
use crate::assets::Lucide;

fn call(app: &Entity<AppState>, command: PluginCommand, cx: &mut App) -> Task<Result<PluginReply, String>> {
    let task = app.update(cx, |app, cx| app.call(Command::Plugin(Box::new(command)), cx));
    cx.background_spawn(async move {
        match task.await? {
            Reply::Plugin(reply) => Ok(*reply),
            _ => Err("Unexpected reply.".into()),
        }
    })
}

fn notify_error(window_handle: AnyWindowHandle, message: String, cx: &mut AsyncApp) {
    let _ = window_handle.update(cx, |_, window, cx| window.push_notification(Notification::error(message), cx));
}

/// Install from a folder here: packed and sent (works for any host).
pub fn install_folder(app: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    pick(app, true, window, cx);
}

/// Install a `.orbitplugin` archive.
pub fn install_archive(app: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    pick(app, false, window, cx);
}

fn pick(app: &Entity<AppState>, folder: bool, window: &mut Window, cx: &mut App) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: !folder,
        directories: folder,
        multiple: false,
        prompt: Some("Install".into()),
    });
    let (app, handle) = (app.clone(), window.window_handle());
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = picked.await else { return };
        let Some(path) = paths.into_iter().next() else { return };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let read = cx
            .background_spawn(async move {
                if path.is_dir() {
                    orbit_plugin::package::pack(&path)
                } else {
                    std::fs::read(&path).map_err(|e| format!("Couldn’t read {}: {e}", path.display()))
                }
            })
            .await;
        match read {
            Ok(bytes) => {
                let source = InstallSource::Archive { name, bytes };
                let _ = handle.update(cx, |_, window, cx| preview(&app, source, window, cx));
            }
            Err(message) => notify_error(handle, message, cx),
        }
    })
    .detach();
}

/// Load a folder on this computer in place, reloading when it changes
/// (this computer's host only).
pub fn load_dev_folder(app: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    let picked = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Load".into()) });
    let (app, handle) = (app.clone(), window.window_handle());
    cx.spawn(async move |cx| {
        let Ok(Ok(Some(paths))) = picked.await else { return };
        let Some(path) = paths.into_iter().next() else { return };
        let source = InstallSource::Folder { path: path.display().to_string(), dev: true };
        let _ = handle.update(cx, |_, window, cx| preview(&app, source, window, cx));
    })
    .detach();
}

/// Install from a git repository at a pinned commit.
pub fn install_git(app: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    let url = cx.new(|cx| InputState::new(window, cx).placeholder("https://github.com/owner/plugin"));
    let commit = cx.new(|cx| InputState::new(window, cx).placeholder("Full commit id (40 hex digits)"));
    let hash = cx.new(|cx| InputState::new(window, cx).placeholder("blake3:… (optional)"));
    url.update(cx, |i, cx| i.focus(window, cx));
    let app = app.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme();
        let (app, url, commit, hash) = (app.clone(), url.clone(), commit.clone(), hash.clone());
        let label = |text: &'static str| div().text_sm().font_weight(FontWeight::MEDIUM).child(text);
        dialog
            .title("Install from git")
            .w(px(520.))
            .child(
                v_flex()
                    .gap_3()
                    .child(v_flex().gap_1().child(label("Repository")).child(Input::new(&url)))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(label("Commit"))
                            .child(Input::new(&commit))
                            .child(div().text_xs().text_color(theme.muted_foreground).child("A branch or tag can change under you, so Orbit installs one exact commit.")),
                    )
                    .child(v_flex().gap_1().child(label("Content hash")).child(Input::new(&hash))),
            )
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("git-cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("git-ok").primary().label("Continue"))),
            )
            .on_ok(move |_, window, cx| {
                let source = InstallSource::Git {
                    url: url.read(cx).value().trim().to_owned(),
                    commit: commit.read(cx).value().trim().to_owned(),
                    hash: Some(hash.read(cx).value().trim().to_owned()).filter(|h| !h.is_empty()),
                };
                let app = app.clone();
                window.defer(cx, move |window, cx| preview(&app, source, window, cx));
                true
            })
    });
}

/// Previews `source` on the host, then installs at once (no capabilities)
/// or asks consent.
pub fn preview(app: &Entity<AppState>, source: InstallSource, window: &mut Window, cx: &mut App) {
    let task = call(app, PluginCommand::Preview(source.clone()), cx);
    let (app, handle) = (app.clone(), window.window_handle());
    cx.spawn(async move |cx| match task.await {
        Ok(PluginReply::Preview { plugin, replaces }) => {
            let _ = handle.update(cx, |_, window, cx| {
                if plugin.capabilities.is_empty() {
                    install(&app, source, Vec::new(), window, cx);
                } else {
                    open_consent(&app, *plugin, replaces, Some(source), window, cx);
                }
            });
        }
        Ok(_) => notify_error(handle, "Unexpected reply.".into(), cx),
        Err(message) => notify_error(handle, message, cx),
    })
    .detach();
}

fn install(app: &Entity<AppState>, source: InstallSource, grant: Vec<Capability>, window: &mut Window, cx: &mut App) {
    let task = call(app, PluginCommand::Install { source, grant }, cx);
    let handle = window.window_handle();
    cx.spawn(async move |cx| match task.await {
        Ok(PluginReply::Installed(info)) => {
            let text = match &info.status {
                PluginStatus::Active => format!("Installed {} {}.", info.name, info.version),
                PluginStatus::Incompatible { requires } => format!("Installed {}, but it needs Orbit {requires}, so it stays off.", info.name),
                other => format!("Installed {} ({}).", info.name, other.label().to_lowercase()),
            };
            let _ = handle.update(cx, |_, window, cx| window.push_notification(Notification::success(text), cx));
        }
        Ok(_) => {}
        Err(message) => notify_error(handle, message, cx),
    })
    .detach();
}

/// Approves an installed plugin that is waiting for consent.
pub fn review(app: &Entity<AppState>, plugin: &PluginInfo, window: &mut Window, cx: &mut App) {
    open_consent(app, plugin.clone(), None, None, window, cx);
}

/// The consent dialog. With a `source` it installs; without, it grants an
/// installed plugin's capabilities.
fn open_consent(app: &Entity<AppState>, plugin: PluginInfo, replaces: Option<String>, source: Option<InstallSource>, window: &mut Window, cx: &mut App) {
    let entry = app.read(cx).active_entry();
    let host = if entry.is_local() { "this computer".to_owned() } else { entry.label() };
    let app = app.clone();
    let title = match (&source, &replaces) {
        (None, _) => format!("Approve “{}”?", plugin.name),
        (Some(_), Some(old)) if *old != plugin.version => format!("Update “{}” to {}?", plugin.name, plugin.version),
        (Some(_), _) => format!("Install “{}”?", plugin.name),
    };
    let ok = if source.is_some() { "Install" } else { "Approve" };
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme().clone();
        let (app, plugin, source) = (app.clone(), plugin.clone(), source.clone());
        let meta = [
            Some(plugin.version.clone()),
            Some(if plugin.publisher.is_empty() { "Unknown publisher".to_owned() } else { plugin.publisher.clone() }),
            Some(plugin.source.clone()).filter(|s| !s.is_empty()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let capability_rows = plugin.capabilities.iter().map(|c| {
            let granted = plugin.granted.contains(c);
            h_flex()
                .gap_2()
                .items_start()
                .child(
                    Icon::new(if c.sensitive() { Lucide::ShieldAlert } else { Lucide::ShieldCheck })
                        .small()
                        .mt_0p5()
                        .text_color(if c.sensitive() { theme.warning } else { theme.muted_foreground }),
                )
                .child(
                    v_flex()
                        .child(div().text_sm().child(c.consent()))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(if granted {
                            format!("{} · already granted", c.key())
                        } else {
                            c.key().to_owned()
                        })),
                )
        });
        let agents = plugin.ui.agents.clone();
        let programs: Vec<String> = if agents.is_empty() {
            // A preview carries no contributions: the summary says how many.
            Vec::new()
        } else {
            agents.iter().map(|a| format!("{}  ({})", std::iter::once(&a.command).chain(&a.args).cloned().collect::<Vec<_>>().join(" "), a.name)).collect()
        };
        let short_hash: String = plugin.hash.chars().take("blake3:".len() + 16).collect();
        let grant = plugin.capabilities.clone();
        let id = plugin.id.clone();
        dialog
            .title(title.clone())
            .w(px(520.))
            .child(
                v_flex()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_xs().text_color(theme.muted_foreground).child(meta))
                            .when(!plugin.description.is_empty(), |el| el.child(div().text_sm().child(plugin.description.clone())))
                            .child(div().text_sm().text_color(theme.muted_foreground).child(format!("Adds {}.", plugin.summary.to_lowercase()))),
                    )
                    .child(v_flex().gap_2().child(div().text_sm().font_weight(FontWeight::MEDIUM).child("It asks to")).children(capability_rows))
                    .when(!programs.is_empty(), |el| {
                        el.child(
                            v_flex().gap_1().child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Programs it runs")).children(programs.iter().map(|p| {
                                div().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(p.clone())
                            })),
                        )
                    })
                    .child(
                        v_flex()
                            .gap_0p5()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("It runs on {host}. Turn it off or remove it any time in Settings › Plugins."))
                            .child(div().font_family(theme.mono_font_family.clone()).child(format!("{short_hash}…"))),
                    ),
            )
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("consent-cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("consent-ok").primary().label(ok))),
            )
            .on_ok(move |_, window, cx| {
                match &source {
                    Some(source) => install(&app, source.clone(), grant.clone(), window, cx),
                    None => {
                        let task = call(&app, PluginCommand::Grant { id: id.clone(), grant: grant.clone() }, cx);
                        let handle = window.window_handle();
                        cx.spawn(async move |cx| {
                            if let Err(message) = task.await {
                                notify_error(handle, message, cx);
                            }
                        })
                        .detach();
                    }
                }
                true
            })
    });
}

/// A plugin's log: every host API call, hook decision and log line.
pub fn show_logs(app: &Entity<AppState>, plugin: &PluginInfo, window: &mut Window, cx: &mut App) {
    let task = call(app, PluginCommand::Logs { id: Some(plugin.id.clone()), limit: 300 }, cx);
    let (name, handle) = (plugin.name.clone(), window.window_handle());
    cx.spawn(async move |cx| {
        let lines = match task.await {
            Ok(PluginReply::Logs(lines)) => lines,
            Ok(_) => Vec::new(),
            Err(message) => return notify_error(handle, message, cx),
        };
        let _ = handle.update(cx, move |_, window, cx| {
            window.open_dialog(cx, move |dialog, window, cx| {
                let theme = cx.theme().clone();
                let now = now_ms();
                let rows: Vec<AnyElement> = lines
                    .iter()
                    .rev()
                    .map(|l| {
                        let bad = matches!(l.outcome.as_str(), "denied" | "error");
                        h_flex()
                            .gap_3()
                            .items_start()
                            .py_1()
                            .text_xs()
                            .child(div().w(rems(5.5)).flex_none().text_color(theme.muted_foreground).child(crate::format::relative_time(l.at_ms, now)))
                            .child(div().w(rems(4.)).flex_none().text_color(if bad { theme.danger } else { theme.muted_foreground }).child(l.outcome.clone()))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().font_family(theme.mono_font_family.clone()).child(l.call.clone()))
                                    .when(!l.detail.is_empty(), |el| el.child(div().text_color(theme.muted_foreground).child(l.detail.clone()))),
                            )
                            .into_any_element()
                    })
                    .collect();
                let empty = rows.is_empty();
                dialog
                    .title(format!("{name} log"))
                    .w(window.rem_size() * 44.)
                    .child(
                        v_flex()
                            .id("plugin-log")
                            .max_h(rems(28.))
                            .overflow_y_scroll()
                            .when(empty, |el| el.child(div().py_4().text_sm().text_color(theme.muted_foreground).child("Nothing logged yet.")))
                            .children(rows),
                    )
            })
        });
    })
    .detach();
}

/// Adds a catalog: where it is and the key it is signed with.
pub fn add_catalog(app: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    let name = cx.new(|cx| InputState::new(window, cx).placeholder("Team plugins"));
    let location = cx.new(|cx| InputState::new(window, cx).placeholder("https://example.com/plugins  or a folder on the host"));
    let key = cx.new(|cx| InputState::new(window, cx).placeholder("RWQ… (the second line of minisign.pub)"));
    name.update(cx, |i, cx| i.focus(window, cx));
    let app = app.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme();
        let (app, name, location, key) = (app.clone(), name.clone(), location.clone(), key.clone());
        let label = |text: &'static str| div().text_sm().font_weight(FontWeight::MEDIUM).child(text);
        dialog
            .title("Add catalog")
            .w(px(520.))
            .child(
                v_flex()
                    .gap_3()
                    .child(v_flex().gap_1().child(label("Name")).child(Input::new(&name)))
                    .child(v_flex().gap_1().child(label("Location")).child(Input::new(&location)))
                    .child(
                        v_flex()
                            .gap_1()
                            .child(label("Public key"))
                            .child(Input::new(&key))
                            .child(div().text_xs().text_color(theme.muted_foreground).child("Its catalog.toml must be signed with this minisign key; Orbit refuses it otherwise.")),
                    ),
            )
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("catalog-cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("catalog-ok").primary().label("Add"))),
            )
            .on_ok(move |_, window, cx| {
                let command = PluginCommand::AddCatalog {
                    name: name.read(cx).value().trim().to_owned(),
                    location: location.read(cx).value().trim().to_owned(),
                    public_key: key.read(cx).value().trim().to_owned(),
                };
                let task = call(&app, command, cx);
                let handle = window.window_handle();
                cx.spawn(async move |cx| {
                    if let Err(message) = task.await {
                        notify_error(handle, message, cx);
                    }
                })
                .detach();
                true
            })
    });
}

/// Sends a command whose only interesting result is failure.
pub fn run(app: &Entity<AppState>, command: PluginCommand, window: &mut Window, cx: &mut App) {
    let task = call(app, command, cx);
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        if let Err(message) = task.await {
            notify_error(handle, message, cx);
        }
    })
    .detach();
}
