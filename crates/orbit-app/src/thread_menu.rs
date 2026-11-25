//! The thread menu: one definition used by the sidebar's context menu and the
//! thread header's overflow menu, so both always offer the same commands with
//! the same words.

use chrono::{Datelike, Duration as Days, Local, NaiveTime, TimeZone, Weekday};
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::{WindowExt as _, h_flex};
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::thread::display_title;

fn patch(app: &Entity<AppState>, id: ThreadId, patch: ThreadPatch, window: &mut Window, cx: &mut App) {
    app.update(cx, |app, cx| app.run(Command::UpdateThread { id, patch }, window, cx));
}

/// Snooze presets in local time: (label, Unix ms). Presets already in the
/// past (an evening that has begun) are left out.
pub fn snooze_presets() -> Vec<(&'static str, u64)> {
    let now = Local::now();
    let at = |date: chrono::NaiveDate, hour: u32| {
        Local
            .from_local_datetime(&date.and_time(NaiveTime::from_hms_opt(hour, 0, 0).unwrap()))
            .single()
            .map(|t| t.timestamp_millis() as u64)
    };
    let mut out = vec![
        ("For 1 hour", (now + Days::hours(1)).timestamp_millis() as u64),
        ("For 3 hours", (now + Days::hours(3)).timestamp_millis() as u64),
    ];
    if let Some(evening) = at(now.date_naive(), 18)
        && evening > now.timestamp_millis() as u64 + 30 * 60_000
    {
        out.push(("Until this evening", evening));
    }
    if let Some(tomorrow) = at(now.date_naive() + Days::days(1), 9) {
        out.push(("Until tomorrow morning", tomorrow));
    }
    let days_to_monday = (7 - now.weekday().num_days_from_monday()) % 7;
    let monday = now.date_naive() + Days::days(if days_to_monday == 0 { 7 } else { days_to_monday as i64 });
    if monday.weekday() == Weekday::Mon
        && let Some(monday) = at(monday, 9)
    {
        out.push(("Until Monday morning", monday));
    }
    out
}

pub fn rename(app: Entity<AppState>, thread: &Thread, window: &mut Window, cx: &mut App) {
    let id = thread.id;
    let input = cx.new(|cx| InputState::new(window, cx).default_value(thread.title.clone()));
    let focus = input.clone();
    window.defer(cx, move |window, cx| {
        let handle = focus.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    });
    window.open_dialog(cx, move |dialog, _, _| {
        let (input, app) = (input.clone(), app.clone());
        dialog
            .title("Rename thread")
            .w(px(420.))
            .child(Input::new(&input))
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("rename").primary().label("Rename"))),
            )
            .on_ok(move |_, window, cx| {
                let title = input.read(cx).value().trim().to_owned();
                if title.is_empty() {
                    return false;
                }
                patch(&app, id, ThreadPatch { title: Some(title), ..Default::default() }, window, cx);
                true
            })
    });
}

/// Deletes a thread, asking first unless Settings › General turned the
/// confirmation off.
pub fn confirm_delete(app: Entity<AppState>, thread: &Thread, window: &mut Window, cx: &mut App) {
    let id = thread.id;
    if !crate::settings::get(cx).general.confirm_delete {
        app.update(cx, |app, cx| app.run(Command::DeleteThread { id }, window, cx));
        return;
    }
    let title = display_title(thread).to_owned();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let app = app.clone();
        alert
            .title(format!("Delete “{title}”?"))
            .description("Its messages, attachments and checkpoints are removed from this computer. Files in your project are not touched.")
            .confirm()
            .ok_text("Delete")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, window, cx| {
                app.update(cx, |app, cx| app.run(Command::DeleteThread { id }, window, cx));
                true
            })
    });
}

/// A menu item's command.
type MenuAction = Box<dyn Fn(&mut Window, &mut App)>;

/// Builds the thread's menu into `menu`.
pub fn build(menu: PopupMenu, app: &Entity<AppState>, thread: &Thread, project_path: Option<String>) -> PopupMenu {
    let id = thread.id;
    let t = thread.clone();
    let item = |label: &'static str, icon: Lucide, f: MenuAction| {
        PopupMenuItem::new(label).icon(icon).on_click(move |_, window, cx| f(window, cx))
    };
    let beside = id.0;
    let mut menu = menu
        .item(
            PopupMenuItem::new("Open beside")
                .icon(Lucide::Columns2)
                .on_click(move |_, window, cx| {
                    window.dispatch_action(Box::new(crate::actions::OpenThreadBeside { thread: beside }), cx)
                }),
        )
        .separator();
    let a = app.clone();
    menu = menu.item(item(
        "Rename…",
        Lucide::Pencil,
        Box::new({
            let t = t.clone();
            move |window, cx| rename(a.clone(), &t, window, cx)
        }),
    ));
    let a = app.clone();
    let pinned = thread.pinned;
    menu = menu.item(item(
        if pinned { "Unpin" } else { "Pin" },
        Lucide::Pin,
        Box::new(move |window, cx| patch(&a, id, ThreadPatch { pinned: Some(!pinned), ..Default::default() }, window, cx)),
    ));
    let a = app.clone();
    let settled = thread.settled_ms.is_some();
    menu = menu.item(item(
        if settled { "Bring back" } else { "Settle" },
        Lucide::ListChecks,
        Box::new(move |window, cx| patch(&a, id, ThreadPatch { settled: Some(!settled), ..Default::default() }, window, cx)),
    ));
    if thread.snoozed_until.is_some() {
        let a = app.clone();
        menu = menu.item(item(
            "Wake now",
            Lucide::Clock,
            Box::new(move |window, cx| patch(&a, id, ThreadPatch { snooze_until: Some(None), ..Default::default() }, window, cx)),
        ));
    }
    let snooze_app = app.clone();
    menu = menu.label("Snooze");
    for (label, until) in snooze_presets() {
        let a = snooze_app.clone();
        menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
            patch(&a, id, ThreadPatch { snooze_until: Some(Some(until)), ..Default::default() }, window, cx)
        }));
    }
    if let ThreadStatus::RateLimited { resets_ms } = thread.status
        && resets_ms > 0
    {
        let a = app.clone();
        menu = menu.item(PopupMenuItem::new("Until the quota resets").on_click(move |_, window, cx| {
            patch(&a, id, ThreadPatch { snooze_until: Some(Some(resets_ms)), ..Default::default() }, window, cx)
        }));
    }
    menu = menu.separator();
    if !thread.unread && thread.turns > 0 && !thread.status.is_busy() {
        let a = app.clone();
        menu = menu.item(PopupMenuItem::new("Mark unread").on_click(move |_, window, cx| {
            patch(&a, id, ThreadPatch { unread: Some(true), ..Default::default() }, window, cx)
        }));
    }
    let id_text = id.to_string();
    menu = menu.item(PopupMenuItem::new("Copy thread ID").icon(gpui_kit::component::IconName::Copy).on_click(
        move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(id_text.clone())),
    ));
    if let Some(path) = thread.workspace.worktree.clone().or(project_path) {
        menu = menu.item(PopupMenuItem::new("Copy path").on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()))
        }));
    }
    if let Some(branch) = thread.workspace.branch.clone() {
        menu = menu.item(PopupMenuItem::new("Copy branch").on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(branch.clone()))
        }));
    }
    let a = app.clone();
    let archived = thread.archived;
    menu = menu.separator().item(item(
        if archived { "Unarchive" } else { "Archive" },
        Lucide::Archive,
        Box::new(move |window, cx| patch(&a, id, ThreadPatch { archived: Some(!archived), ..Default::default() }, window, cx)),
    ));
    let a = app.clone();
    menu.item(item("Delete…", Lucide::Trash, Box::new(move |window, cx| confirm_delete(a.clone(), &t, window, cx))))
}
