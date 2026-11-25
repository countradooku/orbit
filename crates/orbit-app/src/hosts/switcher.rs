//! The host switcher at the foot of the sidebar: the host this window
//! shows, how its connection stands (latency, reconnecting, why it failed),
//! and a menu of every host with what waits there.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::ConnectionStatus;

use super::{Tone, dot, kind_icon, status_line};
use crate::app_state::AppState;
use crate::assets::Lucide;

/// The footer control.
pub fn footer(app: &Entity<AppState>, cx: &App) -> impl IntoElement {
    let state = app.read(cx);
    let entry = state.active_entry().clone();
    let summary = state.summary(&entry.id);
    let (tone, status) = status_line(&entry, &summary);
    // The window's own host reads as before: just where agents run.
    let label = match (&summary.connection, entry.is_local()) {
        (Some(ConnectionStatus::Connected) | None, true) => entry.label(),
        (Some(ConnectionStatus::Connected), false) => match summary.latency {
            Some(latency) => format!("{} · {}", entry.label(), super::latency_text(latency)),
            None => entry.label(),
        },
        _ => format!("{} · {}", entry.label(), status),
    };
    let tooltip = match &summary.connection {
        Some(ConnectionStatus::Disconnected { reason }) => format!("{}: {reason}", entry.label()),
        _ => format!("{}: {status}. Switch hosts or add one.", entry.label()),
    };
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let app = app.clone();
    Button::new("host-switcher")
        .ghost()
        .small()
        .min_w_0()
        .tooltip(tooltip)
        .child(
            h_flex()
                .min_w_0()
                .gap_1p5()
                .text_xs()
                .text_color(muted)
                .child(dot(tone, cx))
                .child(div().min_w_0().truncate().child(label)),
        )
        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, cx| host_menu(menu, &app, cx))
}

fn host_menu(
    mut menu: gpui_kit::component::menu::PopupMenu,
    app: &Entity<AppState>,
    cx: &mut Context<gpui_kit::component::menu::PopupMenu>,
) -> gpui_kit::component::menu::PopupMenu {
    let state = app.read(cx);
    let active = state.active_entry().id.clone();
    let hosts = state.hosts().hosts.clone();
    menu = menu.min_w(px(280.)).label("Show threads on");
    for entry in hosts {
        let summary = app.read(cx).summary(&entry.id);
        let (tone, status) = status_line(&entry, &summary);
        let counts = match (summary.working, summary.waiting) {
            (0, 0) => None,
            (w, 0) => Some(format!("{w} working")),
            (0, n) => Some(format!("{n} need you")),
            (w, n) => Some(format!("{w} working · {n} need you")),
        };
        let detail = if matches!(tone, Tone::Good) { counts.unwrap_or(status) } else { status };
        let id = SharedString::from(entry.id.clone());
        let icon = kind_icon(&entry);
        let name = entry.label();
        let kind = (!entry.is_local()).then(|| entry.kind_label());
        menu = menu.item(
            PopupMenuItem::element(move |_, cx| {
                let theme = cx.theme();
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .child(Icon::new(icon).small().text_color(theme.muted_foreground))
                    .child(div().min_w_0().truncate().child(name.clone()))
                    .when_some(kind, |el, kind| el.child(div().flex_none().text_xs().text_color(theme.muted_foreground).child(kind)))
                    .child(div().flex_1())
                    .child(dot(tone, cx))
                    .child(div().flex_none().max_w(px(150.)).truncate().text_xs().text_color(theme.muted_foreground).child(detail.clone()))
            })
            .checked(entry.id == active)
            .on_click(move |_, window, cx| super::switch_to(id.clone(), window, cx)),
        );
    }
    let retry = app.clone();
    let waiting = !matches!(app.read(cx).connection(), Some(ConnectionStatus::Connected));
    menu.separator()
        .when(waiting && !app.read(cx).active_entry().is_local(), |menu| {
            menu.item(PopupMenuItem::new("Reconnect now").icon(Lucide::RefreshCw).on_click(move |_, _, cx| retry.read(cx).retry()))
        })
        .item(PopupMenuItem::new("Add host…").icon(IconName::Plus).on_click(|_, window, cx| super::add::open(window, cx)))
        .item(PopupMenuItem::new("Manage hosts…").icon(Lucide::Server).on_click(|_, window, cx| super::page::open(window, cx)))
}
