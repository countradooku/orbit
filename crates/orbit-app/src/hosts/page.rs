//! Settings › Hosts: the hosts this window can show (their state, rename,
//! remove), adding one, and sharing this computer.

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::remote::{HostKind, LinkPhase};

use super::{dot, kind_icon, status_line};
use crate::app_state::AppState;
use crate::assets::Lucide;

/// Where Settings › Hosts is among the pages.
pub const PAGE: usize = 11;

#[derive(Default)]
struct Request(bool);

impl Global for Request {}

/// Opens Settings on this page.
pub fn open(window: &mut Window, cx: &mut App) {
    cx.set_global(Request(true));
    window.dispatch_action(Box::new(crate::actions::OpenSettings), cx);
}

/// Whether Settings should open on this page (once).
pub fn take_request(cx: &mut App) -> bool {
    std::mem::take(&mut cx.default_global::<Request>().0)
}

pub fn page(app: &Entity<AppState>, cx: &App) -> SettingPage {
    let list_app = app.clone();
    SettingPage::new("Hosts").icon(Icon::new(Lucide::Server)).resettable(false).groups([
        SettingGroup::new()
            .description(
                "Computers this window shows threads from: this one, others over SSH, WSL distros, and computers \
                 shared with you. Switch at the foot of the sidebar; the others stay connected in the background.",
            )
            .item(
                SettingItem::new("Computers", SettingField::render(move |_, window, cx| hosts_list(&list_app, window, cx)))
                    .layout(Axis::Vertical)
                    .keywords(["remote", "ssh", "wsl", "server", "orbitd", "connection"]),
            ),
        super::share::group(app, cx),
    ])
}

fn hosts_list(app: &Entity<AppState>, _: &mut Window, cx: &mut App) -> AnyElement {
    let theme = cx.theme().clone();
    let state = app.read(cx);
    let active = state.active_entry().id.clone();
    let rows: Vec<_> = state
        .hosts()
        .hosts
        .iter()
        .map(|entry| {
            let summary = state.summary(&entry.id);
            let ephemeral = state.is_ephemeral(&entry.id);
            (entry.clone(), summary, ephemeral)
        })
        .collect();
    v_flex()
        .w_full()
        .gap_1()
        .children(rows.into_iter().map(|(entry, summary, ephemeral)| {
            let (tone, status) = status_line(&entry, &summary);
            let shown = entry.id == active;
            let label = entry.label();
            // The address, unless the name already is it.
            let address = match &entry.kind {
                HostKind::Local => String::new(),
                HostKind::Ssh { target, port, identity } => {
                    let mut text = format!("{target}{}", port.map(|p| format!(":{p}")).unwrap_or_default());
                    if !identity.is_empty() {
                        text.push_str(&format!(" · {identity}"));
                    }
                    text
                }
                HostKind::Wsl { distro } => distro.clone(),
                HostKind::Url { url, .. } => url.clone(),
            };
            let detail = if address.is_empty() || address == label { entry.kind_label().to_owned() } else { format!("{} · {address}", entry.kind_label()) };
            let version = match &summary.phase {
                Some(LinkPhase::Ready(info)) => Some(format!("orbitd {}", info.version)),
                _ => None,
            };
            let id = SharedString::from(entry.id.clone());
            let (switch_id, rename_id, remove_id) = (id.clone(), entry.id.clone(), entry.id.clone());
            let (rename_app, remove_app) = (app.clone(), app.clone());
            let name = label;
            let removable = !entry.is_local() && !ephemeral;
            h_flex()
                .w_full()
                .gap_3()
                .py_1()
                .text_sm()
                .child(Icon::new(kind_icon(&entry)).small().text_color(theme.muted_foreground))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().min_w_0().truncate().font_weight(FontWeight::MEDIUM).child(name.clone()))
                                .when(shown, |el| el.child(div().text_xs().text_color(theme.muted_foreground).child("Shown"))),
                        )
                        .child(
                            h_flex()
                                .gap_1p5()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(dot(tone, cx))
                                .child(div().min_w_0().truncate().child(format!("{status} · {detail}")))
                                .when_some(version, |el, v| el.child(div().flex_none().child(format!("· {v}")))),
                        ),
                )
                .when(!shown, |el| {
                    el.child(Button::new(SharedString::from(format!("host-show-{id}"))).outline().small().label("Show").on_click(
                        move |_, window, cx| {
                            super::switch_to(switch_id.clone(), window, cx);
                        },
                    ))
                })
                .when(!ephemeral, |el| {
                    el.child(
                        Button::new(SharedString::from(format!("host-rename-{id}")))
                            .ghost()
                            .small()
                            .icon(Icon::new(Lucide::Pencil))
                            .tooltip("Rename")
                            .on_click(move |_, window, cx| rename(&rename_app, &rename_id, window, cx)),
                    )
                })
                .when(removable, |el| {
                    el.child(
                        Button::new(SharedString::from(format!("host-remove-{id}")))
                            .ghost()
                            .small()
                            .icon(Icon::new(Lucide::Trash))
                            .tooltip(if shown { "Show another host to remove this one" } else { "Remove" })
                            .disabled(shown)
                            .on_click(move |_, window, cx| remove(&remove_app, &remove_id, window, cx)),
                    )
                })
        }))
        .child(
            h_flex().pt_2().child(
                Button::new("hosts-add")
                    .outline()
                    .small()
                    .icon(Icon::new(IconName::Plus))
                    .label("Add host…")
                    .on_click(|_, window, cx| super::add::open(window, cx)),
            ),
        )
        .into_any_element()
}

fn rename(app: &Entity<AppState>, id: &str, window: &mut Window, cx: &mut App) {
    let Some(entry) = app.read(cx).hosts().get(id).cloned() else { return };
    let input = cx.new(|cx| InputState::new(window, cx).placeholder(entry.label()).default_value(entry.name.clone()));
    input.update(cx, |i, cx| i.focus(window, cx));
    let (app, id) = (app.clone(), id.to_owned());
    window.open_dialog(cx, move |dialog, _, _| {
        let (app, id, input) = (app.clone(), id.clone(), input.clone());
        dialog
            .title("Rename host")
            .w(px(420.))
            .child(v_flex().gap_2().child(Input::new(&input)).child(div().text_xs().child("Leave empty for its address.")))
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("rename-cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("rename-ok").primary().label("Rename"))),
            )
            .on_ok(move |_, _, cx| {
                let name = input.read(cx).value().to_string();
                app.update(cx, |app, cx| app.rename_host(&id, &name, cx));
                true
            })
    });
}

fn remove(app: &Entity<AppState>, id: &str, window: &mut Window, cx: &mut App) {
    let Some(entry) = app.read(cx).hosts().get(id).cloned() else { return };
    let (app, id) = (app.clone(), id.to_owned());
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (app, id) = (app.clone(), id.clone());
        alert
            .title(format!("Remove “{}”?", entry.label()))
            .description(
                "This window stops showing its threads. Nothing on that computer changes: its agents, threads and \
                 orbitd stay, and you can add it again.",
            )
            .confirm()
            .ok_text("Remove")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, _, cx| {
                app.update(cx, |app, cx| app.remove_host(&id, cx));
                true
            })
    });
}
