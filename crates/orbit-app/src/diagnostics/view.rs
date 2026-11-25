//! The crash notice and report viewer, and the Collect diagnostics dialog.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;

use super::bundle::{self, BundleInput};
use super::crash::{self, CrashReport};
use super::{CollectDiagnostics, ShowAbout};

/// Marks the crash notice.
struct CrashNote;

/// "Orbit closed unexpectedly" with View report, Copy and Dismiss. Closing
/// the notice any way dismisses the report: it is offered once.
pub fn offer_crash_report(report: CrashReport, window: &mut Window, cx: &mut App) {
    let title = if report.closed { "Orbit closed unexpectedly" } else { "Orbit hit an internal error" };
    let dismissed = report.clone();
    let note = Notification::warning("A crash report was saved on this computer. Nothing was sent.")
        .id::<CrashNote>()
        .title(title)
        .autohide(false)
        .on_close(move |_, _| {
            let _ = crash::dismiss(&dismissed);
        })
        .content(move |_, _, cx| {
            let (view, copy) = (report.clone(), report.text.clone());
            h_flex()
                .mt_2()
                .gap_2()
                .child(Button::new("crash-view").small().label("View report…").on_click(cx.listener(
                    move |note, _, window, cx| {
                        open_report(view.clone(), window, cx);
                        note.dismiss(window, cx);
                    },
                )))
                .child(Button::new("crash-copy").small().outline().icon(IconName::Copy).label("Copy").on_click(cx.listener(
                    move |_, _, window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                        window.push_notification("Crash report copied", cx);
                    },
                )))
                .child(
                    Button::new("crash-dismiss")
                        .small()
                        .ghost()
                        .label("Dismiss")
                        .on_click(cx.listener(|note, _, window, cx| note.dismiss(window, cx))),
                )
                .into_any_element()
        });
    window.push_notification(note, cx);
}

/// The report itself, with Copy and Show in folder.
fn open_report(report: CrashReport, window: &mut Window, cx: &mut App) {
    let reveal = report.path.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let (text, copy, reveal) = (report.text.clone(), report.text.clone(), reveal.clone());
        dialog
            .title("Crash report")
            .w(px(720.))
            .child(
                div()
                    .id("crash-report-text")
                    .h(rems(26.))
                    .overflow_y_scrollbar()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().muted)
                    .child(
                        div()
                            .p_3()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .whitespace_normal()
                            .child(text),
                    ),
            )
            .footer(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Button::new("crash-reveal")
                            .ghost()
                            .icon(IconName::FolderOpen)
                            .label(reveal_label())
                            .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
                    )
                    .child(div().flex_1())
                    .child(Button::new("crash-copy-all").outline().icon(IconName::Copy).label("Copy").on_click(
                        move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            window.push_notification("Crash report copied", cx);
                        },
                    ))
                    .child(DialogClose::new().child(Button::new("crash-done").primary().label("Done"))),
            )
    });
}

pub fn reveal_label() -> &'static str {
    if cfg!(target_os = "windows") {
        "Show in Explorer"
    } else if cfg!(target_os = "macos") {
        "Show in Finder"
    } else {
        "Show in file manager"
    }
}

/// Values in the settings that must never appear in a bundle.
fn known_secrets(cx: &App) -> Vec<String> {
    let settings = crate::settings::get(cx);
    let mut secrets: Vec<String> = settings
        .agents
        .providers
        .iter()
        .flat_map(|p| p.env.iter().map(|e| e.value.clone()))
        .chain(settings.agents.instances.iter().flat_map(|i| i.env.iter().map(|e| e.value.clone())))
        .collect();
    secrets.extend(crate::hosts::tokens(cx));
    let integrations = crate::forge::integrations::get(cx);
    secrets.push(integrations.linear_api_key.clone());
    secrets.push(integrations.jira_api_token.clone());
    secrets.retain(|s| !s.trim().is_empty());
    secrets
}

/// `agents.txt`: what the provider probe found.
fn agents_text(cx: &App) -> String {
    let Some(workspace_app) = crate::onboarding::app_state(cx) else { return "Unknown\n".to_owned() };
    let state = workspace_app.read(cx).state();
    let mut text = String::new();
    for info in &state.providers {
        let status = match (info.installed, info.signed_in) {
            (false, _) => "not found".to_owned(),
            (true, Some(false)) => "signed out".to_owned(),
            (true, Some(true)) => "signed in".to_owned(),
            (true, None) => "installed".to_owned(),
        };
        text.push_str(&format!(
            "{}: {status}{}{}{}\n",
            info.display_name(),
            info.version.as_deref().map(|v| format!(", version {v}")).unwrap_or_default(),
            info.binary.as_deref().map(|b| format!(", at {b}")).unwrap_or_default(),
            info.message.as_deref().map(|m| format!(" ({m})")).unwrap_or_default(),
        ));
    }
    if text.is_empty() {
        text.push_str("No agents reported yet.\n");
    }
    text
}

fn default_folder() -> std::path::PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(std::path::PathBuf::from);
    match home {
        Some(home) if home.join("Downloads").is_dir() => home.join("Downloads"),
        Some(home) => home,
        None => std::env::temp_dir(),
    }
}

/// "Collect diagnostics": what goes in, then Save….
pub fn collect_dialog(window: &mut Window, cx: &mut App) {
    let data_dir = crate::settings::data_dir(cx);
    let (host, _) = crate::settings::connected_host(cx);
    let input = BundleInput {
        data_dir: data_dir.clone(),
        secrets: known_secrets(cx),
        agents: agents_text(cx),
        engine: host.map(|h| format!("orbitd at {h}")).unwrap_or_else(|| "In this app".to_owned()),
    };
    let files = bundle::preview(&data_dir);
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme().clone();
        let input = input.clone();
        dialog
            .title("Collect diagnostics")
            .w(px(520.))
            .child(
                v_flex()
                    .gap_3()
                    .child(div().text_sm().child(
                        "Orbit saves a zip of its logs, crash reports and version information where you choose. \
                         Tokens, API keys, passwords, environment values and your home folder are removed first. \
                         Your threads, prompts and code are not included, and nothing is sent.",
                    ))
                    .child(
                        v_flex()
                            .gap_1()
                            .p_3()
                            .rounded(theme.radius)
                            .bg(theme.muted)
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .children(files.iter().map(|name| div().child(name.clone()))),
                    ),
            )
            .footer(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("diag-cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("diag-save").primary().icon(IconName::ArrowDown).label("Save…"))),
            )
            .on_ok(move |_, window, cx| {
                save_bundle(input.clone(), window, cx);
                true
            })
    });
}

fn save_bundle(input: BundleInput, window: &mut Window, cx: &mut App) {
    let picked = cx.prompt_for_new_path(&default_folder(), Some(&bundle::default_file_name()));
    window
        .spawn(cx, async move |cx| {
            let Ok(Ok(Some(dest))) = picked.await else { return };
            let job_dest = dest.clone();
            let result = cx.background_spawn(async move { bundle::write_bundle(&input, &job_dest) }).await;
            let _ = cx.update(|window, cx| match result {
                Ok(_) => {
                    let reveal = dest.clone();
                    window.push_notification(
                        Notification::success(dest.display().to_string()).title("Diagnostics saved").action(move |_, _, cx| {
                            let reveal = reveal.clone();
                            Button::new("diag-reveal").label(reveal_label()).on_click(cx.listener(move |note, _, window, cx| {
                                cx.reveal_path(&reveal);
                                note.dismiss(window, cx);
                            }))
                        }),
                        cx,
                    );
                }
                Err(err) => window.push_notification(
                    Notification::error(format!("{err}")).title("Couldn’t save the diagnostics"),
                    cx,
                ),
            });
        })
        .detach();
}

/// The diagnostics actions, on the workspace's root element.
pub fn register_actions<E: InteractiveElement>(el: E) -> E {
    el.on_action(|_: &CollectDiagnostics, window, cx| collect_dialog(window, cx))
        .on_action(|_: &ShowAbout, window, cx| super::open_about(window, cx))
}
