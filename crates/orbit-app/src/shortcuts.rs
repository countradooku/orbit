//! The keyboard shortcuts sheet (`?` outside a text field, or `mod-/`
//! anywhere): every command with the keys that run it now, read from the
//! live keymap so the user's own bindings show. Typing filters it.

use std::rc::Rc;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::actions::*;
use crate::ui;

/// A row: what it does, and either a command (keys from the keymap) or a
/// fixed key description.
struct Row {
    label: &'static str,
    keys: Keys,
}

enum Keys {
    Action(Box<dyn Action>),
    Fixed(&'static [&'static str]),
}

fn row(label: &'static str, action: impl Action) -> Row {
    Row { label, keys: Keys::Action(Box::new(action)) }
}

fn fixed(label: &'static str, keys: &'static [&'static str]) -> Row {
    Row { label, keys: Keys::Fixed(keys) }
}

fn groups() -> Vec<(&'static str, Vec<Row>)> {
    vec![
        (
            "Go to",
            vec![
                row("Search threads and commands", OpenPalette),
                row("New thread", NewThread),
                row("Inbox", GoInbox),
                row("Next thread that needs you", NextAttention),
                row("Fleet", GoFleet),
                row("Next thread", NextThread),
                row("Previous thread", PreviousThread),
                fixed("Threads 1–9 in the sidebar", &["mod-1", "mod-9"]),
                row("Settings", OpenSettings),
                row("Add project", AddProject),
            ],
        ),
        (
            "Thread",
            vec![
                row("Focus the composer", FocusComposer),
                row("Stop the running turn", StopTurn),
                row("Review changes", ToggleReview),
                row("Terminal", ToggleTerminal),
                row("Rename", RenameThread),
                row("Settle", SettleThread),
                row("Pin or unpin", PinThread),
                row("Open beside, as a tile", OpenBeside),
            ],
        ),
        (
            "Inbox",
            vec![
                row("Next card", DeckNext),
                row("Previous card", DeckPrevious),
                fixed("Pick an answer", &["1", "9"]),
                row("Allow, or take the recommended answer", DeckApprove),
                row("Allow every request of the same kind", DeckApproveAllOfKind),
                row("Deny", DeckDeny),
                row("Reply", DeckReply),
                row("Snooze for an hour", DeckSnooze),
                row("Settle", DeckSettle),
                row("Open the thread", DeckOpen),
            ],
        ),
        (
            "Tiles",
            vec![
                row("Focus the tile to the left", FocusTileLeft),
                row("Focus the tile to the right", FocusTileRight),
                row("Focus the tile above", FocusTileUp),
                row("Focus the tile below", FocusTileDown),
                row("Maximize or restore the tile", MaximizeTile),
                row("Close the tile", CloseTile),
                fixed("Open a thread beside", &["mod-click"]),
            ],
        ),
        (
            "Sidebar",
            vec![
                fixed("Select a range of threads", &["shift-click"]),
                fixed("Add a thread to the selection", &["mod-shift-click"]),
                row("Clear the selection", ClearSelection),
                row("Show or hide the sidebar", ToggleSidebar),
            ],
        ),
        (
            "View",
            vec![
                row("Keyboard shortcuts", ShowShortcuts),
                row("Zoom in", ZoomIn),
                row("Zoom out", ZoomOut),
                row("Actual size", ResetZoom),
                row("Quit", Quit),
            ],
        ),
    ]
}

/// Opens the sheet.
pub fn open(window: &mut Window, cx: &mut App) {
    let search = cx.new(|cx| InputState::new(window, cx).placeholder("Filter shortcuts"));
    crate::voice::register_field(crate::voice::Field::input(&search), cx);
    let query: Rc<std::cell::RefCell<String>> = Rc::default();
    {
        let query = query.clone();
        cx.subscribe(&search, move |input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                *query.borrow_mut() = input.read(cx).value().trim().to_lowercase();
                cx.refresh_windows();
            }
        })
        .detach();
    }
    let focus = search.clone();
    window.defer(cx, move |window, cx| {
        let handle = focus.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    });
    let groups = Rc::new(groups());
    let width = window.rem_size() * 58.;
    window.open_dialog(cx, move |dialog, _, _| {
        let (search, query, groups) = (search.clone(), query.clone(), groups.clone());
        dialog.title("Keyboard shortcuts").w(width).content(move |content, _, cx| {
            let theme = cx.theme().clone();
            let query = query.borrow().clone();
            let mut columns = h_flex().flex_wrap().items_start().gap_y_4();
            let mut any = false;
            for (title, rows) in groups.iter() {
                let rows: Vec<&Row> = rows
                    .iter()
                    .filter(|r| query.is_empty() || r.label.to_lowercase().contains(&query) || title.to_lowercase().contains(&query))
                    .collect();
                if rows.is_empty() {
                    continue;
                }
                any = true;
                let mut group = v_flex().w(relative(0.5)).pr_5().gap_0p5().child(ui::section_label(*title, cx).px_0().pt_0());
                for row in rows {
                    let keys: AnyElement = match &row.keys {
                        Keys::Action(action) => ui::keycaps(action.as_ref(), cx).unwrap_or_else(|| {
                            div().text_xs().text_color(theme.muted_foreground).child("Not bound").into_any_element()
                        }),
                        Keys::Fixed(keys) => fixed_keys(keys, cx),
                    };
                    group = group.child(
                        h_flex()
                            .h_7()
                            .gap_3()
                            .child(div().flex_1().min_w_0().truncate().text_sm().child(row.label))
                            .child(keys),
                    );
                }
                columns = columns.child(group);
            }
            content
                .child(
                    Input::new(&search)
                        .small()
                        .prefix(Icon::new(IconName::Search).xsmall().text_color(theme.muted_foreground)),
                )
                .child(
                    div()
                        .id("shortcuts-list")
                        .max_h(rems(30.))
                        .overflow_y_scroll()
                        .pr_2()
                        .child(columns)
                        .when(!any, |el| {
                            el.child(div().py_6().text_sm().text_color(theme.muted_foreground).child("No shortcut matches."))
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("Change any of these in {} (Command palette › Edit keybindings).", crate::keymap::FILE_NAME)),
                )
        })
    });
}

/// Keys that are not one command's binding: a range ("1–9") or a click.
fn fixed_keys(keys: &[&'static str], cx: &App) -> AnyElement {
    let theme = cx.theme();
    let cap = |key: &'static str| -> AnyElement {
        match key.strip_suffix("-click") {
            Some(modifiers) => h_flex()
                .gap_1()
                .child(ui::keycap(match modifiers {
                    "mod" => crate::actions::platform("cmd", "ctrl"),
                    "mod-shift" => crate::actions::platform("cmd-shift", "ctrl-shift"),
                    other => other,
                }))
                .child(div().text_xs().text_color(theme.muted_foreground).child("click"))
                .into_any_element(),
            None => ui::keycap(match key {
                "mod-1" => crate::actions::platform("cmd-1", "ctrl-1"),
                "mod-9" => crate::actions::platform("cmd-9", "ctrl-9"),
                other => other,
            }),
        }
    };
    match keys {
        [one] => cap(one),
        [first, last] => h_flex()
            .gap_1()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(cap(first))
            .child("–")
            .child(cap(last))
            .into_any_element(),
        _ => div().into_any_element(),
    }
}
