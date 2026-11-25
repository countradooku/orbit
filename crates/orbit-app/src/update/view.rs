//! How updates show: the Updates settings page, "Restart to update" in the
//! title bar, the release notes sheet, What's new after an update, and the
//! notice when an update was rolled back.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::progress::Progress as ProgressBar;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::manifest::Channel;
use super::{Phase, Updater, VERSION, host, install, notes_for, updater};
use crate::assets::Lucide;

// ── Title bar ────────────────────────────────────────────────────────────

/// "Restart to update" once an update is ready; nothing otherwise. Checks
/// and downloads are quiet: they show in Settings › Updates.
pub fn title_bar_item(cx: &App) -> Option<AnyElement> {
    let updater = updater(cx)?;
    let Phase::Ready { release, .. } = updater.read(cx).phase() else { return None };
    let tooltip: SharedString = format!("Orbit {} is ready to install", release.version).into();
    Some(
        Button::new("title-restart-to-update")
            .small()
            .outline()
            .icon(Icon::new(Lucide::CircleArrowUp).text_color(cx.theme().primary))
            .label("Restart to update")
            .tooltip(tooltip)
            .on_click(|_, window, cx| window.dispatch_action(Box::new(super::RestartToUpdate), cx))
            .into_any_element(),
    )
}

// ── Sheets ───────────────────────────────────────────────────────────────

fn notes_body(id: &'static str, notes: Option<String>, cx: &App) -> AnyElement {
    match notes {
        Some(notes) => div()
            .id(id)
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .child(div().px_1().pb_4().child(TextView::markdown(id, notes).selectable(true)))
            .into_any_element(),
        None => div()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child("This release came without notes.")
            .into_any_element(),
    }
}

fn sheet_title(title: String, meta: Option<String>, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_0p5()
        .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child(title))
        .when_some(meta, |el, meta| el.child(div().text_xs().text_color(cx.theme().muted_foreground).child(meta)))
}

/// The ready update's notes, with Restart to update.
pub fn open_release_sheet(window: &mut Window, cx: &mut App) {
    let Some(updater) = updater(cx) else { return };
    let Phase::Ready { release, .. } = updater.read(cx).phase().clone() else {
        // Nothing ready: say where things stand instead.
        window.dispatch_action(Box::new(crate::actions::OpenSettings), cx);
        return;
    };
    let at_risk = host::agents_at_risk(cx);
    let title = format!("Orbit {}", release.version);
    let meta = Some(match &release.pub_date {
        Some(date) => format!("{} channel · {}", release.channel.label(), date.split('T').next().unwrap_or(date)),
        None => format!("{} channel", release.channel.label()),
    });
    let notes = Some(release.notes.clone()).filter(|n| !n.trim().is_empty());
    window.open_sheet(cx, move |sheet, _, cx| {
        let updater = updater.clone();
        let footer = v_flex()
            .w_full()
            .gap_3()
            .when(at_risk > 0, |el| {
                el.child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(Icon::new(IconName::TriangleAlert).small().text_color(cx.theme().warning))
                        .child(format!(
                            "{at_risk} agent{} working. Restarting stops {}; each thread keeps everything up to its last step.",
                            if at_risk == 1 { " is" } else { "s are" },
                            if at_risk == 1 { "it" } else { "them" }
                        )),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(Button::new("update-later").outline().label("Later").on_click(|_, window, cx| window.close_sheet(cx)))
                    .child(
                        Button::new("update-restart")
                            .primary()
                            .label("Restart to update")
                            .on_click(move |_, _, cx| updater.update(cx, |u, cx| u.restart(cx))),
                    ),
            );
        sheet
            .title(sheet_title(title.clone(), meta.clone(), cx))
            .size(px(520.))
            .child(notes_body("update-notes", notes.clone(), cx))
            .footer(footer)
    });
}

/// This version's notes: shown once after an update, and from the palette.
pub fn open_whats_new(window: &mut Window, cx: &mut App) {
    let Some(updater) = updater(cx) else { return };
    let notes = notes_for(updater.read(cx).data_dir(), VERSION);
    window.open_sheet(cx, move |sheet, _, cx| {
        sheet
            .title(sheet_title(format!("What’s new in Orbit {VERSION}"), None, cx))
            .size(px(520.))
            .child(notes_body("whats-new-notes", notes.clone(), cx))
            .footer(
                h_flex()
                    .w_full()
                    .justify_end()
                    .child(Button::new("whats-new-done").primary().label("Done").on_click(|_, window, cx| window.close_sheet(cx))),
            )
    });
}

/// Tells how the last update went.
pub fn announce_outcome(outcome: install::Outcome, window: &mut Window, cx: &mut App) {
    match outcome {
        install::Outcome::Installed { .. } => {
            let updater = updater(cx);
            // Show the notes once the window has settled.
            window.defer(cx, move |window, cx| {
                if updater.is_some() {
                    open_whats_new(window, cx);
                }
            });
        }
        install::Outcome::RolledBack { from, to, reason } => window.push_notification(
            Notification::warning(format!(
                "{reason} Orbit {from} is back, and {to} won’t be installed automatically. Check for updates in Settings to try a newer release."
            ))
            .title(format!("Orbit {to} didn’t start"))
            .autohide(false),
            cx,
        ),
        install::Outcome::Failed { to, reason } => window.push_notification(
            Notification::error(reason).title(format!("Orbit {to} wasn’t installed")).autohide(false),
            cx,
        ),
    }
}

/// Tells the answer to a check the user asked for, or a failed download.
pub fn announce_notice(notice: &super::UpdateNotice, window: &mut Window, cx: &mut App) {
    use super::UpdateNotice::*;
    let note = match notice {
        UpToDate { channel } => {
            Notification::success(format!("Orbit {VERSION} is the newest {} release.", channel.label())).title("Up to date")
        }
        CheckFailed(message) => Notification::error(message.clone()).title("Couldn’t check for updates"),
        Downloading(version) => Notification::info(format!(
            "Orbit {version} is downloading in the background. Restart to update appears in the title bar when it’s ready."
        ))
        .title("Update found"),
        DownloadFailed { version, message } => {
            Notification::error(message.clone()).title(format!("Orbit {version} wasn’t downloaded"))
        }
    };
    window.push_notification(note, cx);
}

// ── Settings › Updates ───────────────────────────────────────────────────

fn with_updater<R>(cx: &App, read: impl FnOnce(&Updater) -> R) -> Option<R> {
    updater(cx).map(|u| read(u.read(cx)))
}

fn update_prefs(cx: &mut App, edit: impl FnOnce(&mut super::UpdatePrefs) + 'static) {
    if let Some(updater) = updater(cx) {
        updater.update(cx, |u, cx| u.set_prefs(edit, cx));
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// One line saying where updates stand, for the Status row's description.
fn status_line(cx: &App) -> String {
    let Some((phase, last)) = with_updater(cx, |u| (u.phase().clone(), u.prefs().last_check_ms)) else {
        return String::new();
    };
    let checked = last.map(|ms| crate::format::relative_time(ms, orbit_proto::now_ms()));
    match phase {
        Phase::Unconfigured => {
            "This build has no release signing key, so it never downloads updates. Official releases update themselves."
                .to_owned()
        }
        Phase::Idle | Phase::UpToDate => match checked {
            Some(when) if when == "now" => format!("Orbit {VERSION}. Checked just now."),
            Some(when) => format!("Orbit {VERSION}. Checked {when} ago."),
            None => format!("Orbit {VERSION}."),
        },
        Phase::Checking => format!("Orbit {VERSION}. Checking the channel…"),
        Phase::Available(release) => format!("Orbit {} is available. You have {VERSION}.", release.version),
        Phase::Downloading { release, progress } => {
            let (done, total) = progress.read();
            match total {
                Some(total) => format!("Downloading Orbit {}: {} of {}.", release.version, megabytes(done), megabytes(total)),
                None => format!("Downloading Orbit {}: {}.", release.version, megabytes(done)),
            }
        }
        Phase::Ready { release, .. } => {
            format!("Orbit {} is downloaded and verified. It installs when Orbit restarts.", release.version)
        }
        Phase::Restarting => "Restarting…".to_owned(),
        Phase::Failed { message, .. } => message,
    }
}

fn status_field() -> SettingField<SharedString> {
    SettingField::render(|_, _, cx| {
        let theme = cx.theme().clone();
        let Some(updater) = updater(cx) else { return div().into_any_element() };
        let phase = updater.read(cx).phase().clone();
        let check = |label: &'static str, loading: bool| {
            let updater = updater.clone();
            Button::new("updates-check")
                .outline()
                .small()
                .icon(IconName::RefreshCw)
                .label(label)
                .loading(loading)
                .disabled(loading)
                .on_click(move |_, _, cx| updater.update(cx, |u, cx| u.check(true, cx)))
        };
        let label = |text: String, color: Hsla| div().text_sm().text_color(color).child(text);
        match phase {
            Phase::Unconfigured => label("Off".into(), theme.muted_foreground).into_any_element(),
            Phase::Idle => h_flex().gap_3().child(check("Check now", false)).into_any_element(),
            Phase::UpToDate => h_flex()
                .gap_3()
                .child(
                    h_flex()
                        .gap_1p5()
                        .text_sm()
                        .child(Icon::new(IconName::CircleCheck).small().text_color(theme.success))
                        .child("Up to date"),
                )
                .child(check("Check now", false))
                .into_any_element(),
            Phase::Checking => h_flex().gap_3().child(check("Checking…", true)).into_any_element(),
            Phase::Available(release) => {
                let updater = updater.clone();
                Button::new("updates-download")
                    .outline()
                    .small()
                    .icon(Lucide::Download)
                    .label(format!("Download {}", release.version))
                    .on_click(move |_, _, cx| updater.update(cx, |u, cx| u.download(release.clone(), cx)))
                    .into_any_element()
            }
            Phase::Downloading { progress, .. } => {
                let (done, total) = progress.read();
                let percent = total.map_or(0.0, |t| done as f32 / t.max(1) as f32 * 100.0);
                let updater = updater.clone();
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .w_40()
                            .child(ProgressBar::new("updates-progress").value(percent).accessibility_label("Download progress")),
                    )
                    .child(
                        Button::new("updates-cancel")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(move |_, _, cx| updater.update(cx, |u, cx| u.cancel_download(cx))),
                    )
                    .into_any_element()
            }
            Phase::Ready { .. } => Button::new("updates-restart")
                .small()
                .icon(Lucide::CircleArrowUp)
                .label("Restart to update…")
                .on_click(|_, window, cx| open_release_sheet(window, cx))
                .into_any_element(),
            Phase::Restarting => h_flex().gap_2().child(Spinner::new().xsmall()).child(label("Restarting".into(), theme.muted_foreground)).into_any_element(),
            Phase::Failed { release, .. } => {
                let updater = updater.clone();
                Button::new("updates-retry")
                    .outline()
                    .small()
                    .icon(IconName::RefreshCw)
                    .label("Try again")
                    .on_click(move |_, _, cx| {
                        let release = release.clone();
                        updater.update(cx, |u, cx| match release {
                            Some(release) => u.download(release, cx),
                            None => {
                                u.phase = Phase::Idle;
                                u.check(true, cx);
                            }
                        })
                    })
                    .into_any_element()
            }
        }
    })
}

/// Settings › Updates.
pub fn settings_page(cx: &App) -> SettingPage {
    let configured = with_updater(cx, Updater::is_configured).unwrap_or(false);
    let channels = Channel::ALL.into_iter().map(|c| (SharedString::from(c.id()), SharedString::from(c.label()))).collect();
    SettingPage::new("Updates").icon(Icon::new(Lucide::CircleArrowUp)).resettable(false).groups([
        SettingGroup::new().title("Orbit").items([
            SettingItem::new("Status", status_field()).description(status_line(cx)).keywords(["update", "upgrade", "version"]),
            SettingItem::new(
                "Channel",
                SettingField::dropdown(
                    channels,
                    |cx| SharedString::from(with_updater(cx, |u| u.prefs().channel).unwrap_or_default().id()),
                    |picked: SharedString, cx| {
                        if let Some(channel) = Channel::ALL.into_iter().find(|c| c.id() == picked.as_ref()) {
                            update_prefs(cx, move |p| p.channel = channel);
                        }
                    },
                )
                .default_value(SharedString::from(Channel::Stable.id())),
            )
            .description(
                "Stable gets tested releases. Beta gets the next release a few weeks early. Nightly gets every day’s build \
                 and its rough edges.",
            )
            .disabled(!configured)
            .keywords(["beta", "nightly", "stable", "release"]),
            SettingItem::new(
                "Check automatically",
                SettingField::switch(
                    |cx| with_updater(cx, |u| u.prefs().auto_check).unwrap_or(true),
                    |on, cx| update_prefs(cx, move |p| p.auto_check = on),
                )
                .default_value(true),
            )
            .description("Looks for a new release when Orbit starts and every few hours.")
            .disabled(!configured),
            SettingItem::new(
                "Download automatically",
                SettingField::switch(
                    |cx| with_updater(cx, |u| u.prefs().auto_download).unwrap_or(true),
                    |on, cx| update_prefs(cx, move |p| p.auto_download = on),
                )
                .default_value(true),
            )
            .description("Downloads and verifies new releases in the background. Nothing changes until you restart.")
            .disabled(!configured),
        ]),
        SettingGroup::new().title("Release notes").item(
            SettingItem::new(
                format!("Orbit {VERSION}"),
                SettingField::render(|_, _, _| {
                    Button::new("updates-whats-new")
                        .outline()
                        .small()
                        .label("What’s new…")
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(super::ShowWhatsNew), cx))
                }),
            )
            .description("The notes for the version you’re running."),
        ),
    ])
}

/// The update actions, on the workspace's root element.
pub fn register_actions<E: InteractiveElement>(el: E) -> E {
    el.on_action(|_: &super::CheckForUpdates, window, cx| {
        let Some(updater) = updater(cx) else { return };
        let configured = updater.read(cx).is_configured();
        if !configured {
            window.push_notification(
                Notification::info("This build has no release signing key, so it can’t update itself.")
                    .title("Updates are off"),
                cx,
            );
            return;
        }
        updater.update(cx, |u, cx| u.check(true, cx));
    })
    .on_action(|_: &super::RestartToUpdate, window, cx| open_release_sheet(window, cx))
    .on_action(|_: &super::ShowWhatsNew, window, cx| open_whats_new(window, cx))
}
