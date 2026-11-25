//! Settings › Usage: what happens when an account reaches its limit, the
//! 80 % and 95 % alerts, reading limits from the agents, and account pins.
//!
//! These settings are the engine's (`usage.bin`), read from the quota board
//! and saved through [`super::update_settings`].

use std::rc::Rc;

use gpui_kit::component::button::Button;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::*;
use orbit_core::provider::presets;
use orbit_proto::*;

use super::{settings, update_settings};
use crate::app_state::AppState;
use crate::assets::Lucide;

pub fn page(app: &Entity<AppState>, _: &mut App) -> SettingPage {
    let pins_app = app.clone();
    SettingPage::new("Usage").icon(Icon::new(Lucide::Gauge)).groups([
        SettingGroup::new().title("Limits").items([
            SettingItem::new("When an account reaches its limit", policy_field())
                .description(
                    "Ask offers another account of the same agent (or another agent, with a warning) in the thread. \
                     Automatically moves the thread to the account of the same agent with the most room and sends the \
                     message again; pinned projects and automations are only offered. Never waits for the reset. A \
                     move always leaves a note in the thread.",
                )
                .keywords(["failover", "rate limit", "quota", "account", "switch"]),
            SettingItem::new(
                "Read limits from the agents",
                SettingField::switch(|cx| settings(cx).poll, |v, cx| update_settings(cx, |s| s.poll = v)).default_value(true),
            )
            .description(
                "While Orbit is in front, ask Claude Code’s and Codex’s usage services for each account’s windows, with \
                 the sign-in they already have. Off: only what running threads report.",
            )
            .keywords(["poll", "oauth", "rate limits", "quota"]),
        ]),
        SettingGroup::new()
            .title("Alerts")
            .description("Under Notifications › Turn failed, once per window period.")
            .items([
                SettingItem::new(
                    "At 80% of a window",
                    SettingField::switch(|cx| settings(cx).alert_80, |v, cx| update_settings(cx, |s| s.alert_80 = v)).default_value(true),
                )
                .keywords(["alert", "notification", "quota"]),
                SettingItem::new(
                    "At 95% of a window",
                    SettingField::switch(|cx| settings(cx).alert_95, |v, cx| update_settings(cx, |s| s.alert_95 = v)).default_value(true),
                )
                .keywords(["alert", "notification", "quota"]),
            ]),
        SettingGroup::new()
            .title("Pinned accounts")
            .description("New threads of a pinned project, and runs of a pinned automation, start on its account and never move automatically.")
            .items([SettingItem::new("Projects and automations", SettingField::render(move |_, _, cx| pins(&pins_app, cx)))
                .layout(Axis::Vertical)
                .keywords(["pin", "account", "project", "automation"])]),
    ])
}

fn policy_field() -> SettingField<SharedString> {
    let options = FailoverPolicy::ALL.iter().enumerate().map(|(ix, p)| (SharedString::from(ix.to_string()), SharedString::from(p.label()))).collect();
    SettingField::dropdown(
        options,
        |cx| {
            let policy = settings(cx).failover;
            SharedString::from(FailoverPolicy::ALL.iter().position(|p| *p == policy).unwrap_or(0).to_string())
        },
        |picked: SharedString, cx| {
            if let Some(&policy) = picked.parse::<usize>().ok().and_then(|ix| FailoverPolicy::ALL.get(ix)) {
                update_settings(cx, |s| s.failover = policy);
            }
        },
    )
    .default_value(SharedString::from("0"))
}

/// Every project and automation, each with a menu of the accounts of the
/// agents that have them.
fn pins(app: &Entity<AppState>, cx: &App) -> AnyElement {
    let theme = cx.theme().clone();
    let state = app.read(cx).state();
    let configured = &crate::settings::get(cx).agents.instances;
    // Accounts worth pinning to: instances of agents with plan limits.
    let accounts: Rc<Vec<(String, String)>> = Rc::new(
        presets::effective(configured)
            .into_iter()
            .filter(|(i, listed)| *listed && orbit_core::usage::readable(i.kind))
            .map(|(i, _)| {
                let name = state.providers.iter().find(|p| p.instance_id() == i.id).map(|p| p.display_name().to_owned());
                (i.id.clone(), name.unwrap_or_else(|| i.display_name().to_owned()))
            })
            .collect(),
    );
    let current = settings(cx);
    let mut rows: Vec<(PinTarget, String)> = state.projects.iter().map(|p| (PinTarget::Project(p.id), p.name.clone())).collect();
    rows.extend(state.automations.iter().map(|a| (PinTarget::Automation(a.id), format!("{} (automation)", a.spec.name))));
    if rows.is_empty() {
        return div().text_sm().text_color(theme.muted_foreground).child("Add a project to pin it to an account.").into_any_element();
    }
    v_flex()
        .w_full()
        .gap_1()
        .children(rows.into_iter().map(|(target, label)| {
            let pinned = current.pin(target).map(str::to_owned);
            let shown = match &pinned {
                Some(id) => accounts.iter().find(|(i, _)| i == id).map(|(_, n)| n.clone()).unwrap_or_else(|| id.clone()),
                None => "Any account".to_owned(),
            };
            let key = match target {
                PinTarget::Project(id) => format!("pin-project-{id}"),
                PinTarget::Automation(id) => format!("pin-automation-{id}"),
            };
            let accounts = accounts.clone();
            h_flex()
                .w_full()
                .gap_3()
                .text_sm()
                .child(div().flex_1().min_w_0().truncate().child(label))
                .child(Button::new(SharedString::from(key)).outline().small().label(shown).dropdown_caret(true).dropdown_menu(
                    move |mut menu, _, _| {
                        menu = menu.item(
                            PopupMenuItem::new("Any account")
                                .checked(pinned.is_none())
                                .on_click(move |_, _, cx| update_settings(cx, |s| s.set_pin(target, None))),
                        );
                        menu = menu.separator();
                        for (id, name) in accounts.iter() {
                            let id = id.clone();
                            let checked = pinned.as_deref() == Some(id.as_str());
                            menu = menu.item(
                                PopupMenuItem::new(name.clone())
                                    .checked(checked)
                                    .on_click(move |_, _, cx| update_settings(cx, |s| s.set_pin(target, Some(id.clone())))),
                            );
                        }
                        menu
                    },
                ))
        }))
        .into_any_element()
}
