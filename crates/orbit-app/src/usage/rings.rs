//! One ring per account in the title bar; the popover behind them lists each
//! account's windows with their resets, pace and projected run-out, and the
//! threads using it most. [`account`] draws one account the same way on the
//! Usage page.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use std::rc::Rc;

use gpui_kit::*;
use orbit_proto::*;

use super::ring::{Level, Meter};
use super::{GoUsage, pace_line, reset_time, time_until};
use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::format;
use crate::shell::{Route, Workspace};
use crate::ui;

/// Accounts worth a ring: the ones with windows to show.
fn shown(board: &QuotaBoard) -> Vec<&AccountQuota> {
    board.accounts.iter().filter(|a| !a.windows.is_empty()).collect()
}

/// `Claude Code: 5-hour 42%, resets 15:40` for each account.
fn summary(accounts: &[&AccountQuota], now: u64) -> String {
    accounts
        .iter()
        .map(|a| match a.tightest() {
            Some(w) if w.window.rejected => format!("{}: {} limit reached, {}", a.name, w.label, reset_time(w.window.resets_ms, now).to_lowercase()),
            Some(w) => {
                let soon = a.windows.iter().filter_map(|w| w.runs_out_ms.filter(|at| *at > now)).min();
                let pace = soon.map(|at| format!(", on pace to run out {}", time_until(at, now))).unwrap_or_default();
                format!("{}: {} {}%{pace}", a.name, w.label, (w.used() * 100.0).round())
            }
            None => a.name.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The title bar's rings, or nothing while no account has windows.
pub fn title_bar(app: &Entity<AppState>, cx: &mut Context<Workspace>) -> Option<AnyElement> {
    let now = now_ms();
    let board = &app.read(cx).state().quota;
    let accounts = shown(board);
    if accounts.is_empty() {
        return None;
    }
    let several_agents = accounts.iter().any(|a| a.kind != accounts[0].kind);
    let tooltip = summary(&accounts, now);
    let rings = h_flex().gap_2().children(accounts.iter().map(|a| {
        let tight = a.tightest();
        h_flex()
            .gap_1()
            .when(several_agents, |el| el.child(ui::provider_icon(a.kind, cx).xsmall()))
            .child(
                Meter::new(SharedString::from(format!("quota-{}", a.instance)), tight.map(QuotaWindowState::used))
                    .refused(a.limited(now))
                    .running_out(a.windows.iter().any(|w| w.runs_out_ms.is_some_and(|at| at > now))),
            )
    }));
    let app = app.clone();
    let workspace = cx.weak_entity();
    let opened = app.clone();
    Some(
        Popover::new("quota-rings")
            .anchor(Anchor::TopRight)
            // Debug builds: ORBIT_DEBUG_ROUTE=quota opens it, for screenshots.
            .default_open(cfg!(debug_assertions) && std::env::var("ORBIT_DEBUG_ROUTE").is_ok_and(|r| r == "quota"))
            .on_open_change(move |open, _, cx| {
                if *open {
                    super::front(&opened, cx);
                }
            })
            .trigger(Button::new("quota-rings-button").ghost().small().tooltip(tooltip).child(rings))
            .content(move |_, _, cx| {
                // A thread picked from the list opens, and the popover closes.
                let (popover, workspace) = (cx.entity().downgrade(), workspace.clone());
                let open: OpenThread = Rc::new(move |thread, window, cx| {
                    let _ = popover.update(cx, |state, cx| state.dismiss(window, cx));
                    let _ = workspace.update(cx, |ws, cx| ws.navigate(Route::Thread(thread), window, cx));
                });
                roster(&app, open, cx)
            })
            .into_any_element(),
    )
}

/// Opens a thread from an account's top consumers.
pub type OpenThread = Rc<dyn Fn(ThreadId, &mut Window, &mut App)>;

fn roster(app: &Entity<AppState>, open: OpenThread, cx: &mut App) -> AnyElement {
    let theme = cx.theme().clone();
    let now = now_ms();
    let board = app.read(cx).state().quota.clone();
    let accounts: Vec<AccountQuota> = board.accounts;
    let refresh_app = app.clone();
    v_flex()
        .w(rems(26.))
        .gap_3()
        .children(accounts.iter().enumerate().map(|(ix, a)| {
            v_flex()
                .gap_3()
                .when(ix > 0, |el| el.pt_3().border_t_1().border_color(theme.border))
                .child(account(a, now, Some(open.clone()), cx))
        }))
        .when(accounts.is_empty(), |el| {
            el.child(div().text_sm().text_color(theme.muted_foreground).child("No account with plan limits yet."))
        })
        .child(
            h_flex()
                .pt_2()
                .gap_1()
                .border_t_1()
                .border_color(theme.border)
                .child(
                    Button::new("quota-refresh")
                        .ghost()
                        .small()
                        .icon(IconName::RefreshCw)
                        .label("Refresh")
                        .on_click(move |_, window, cx| super::refresh(&refresh_app, None, window, cx)),
                )
                .child(div().flex_1())
                .child(
                    Button::new("quota-open-usage")
                        .ghost()
                        .small()
                        .icon(Lucide::ChartColumn)
                        .label("Usage")
                        .tooltip_with_action("Tokens, cost and limits", &GoUsage, None)
                        .on_click(|_, window, cx| window.dispatch_action(Box::new(GoUsage), cx)),
                ),
        )
        .into_any_element()
}

/// One account: name and plan, each window with its bar, reset and pace,
/// and the threads using it most, which `open` opens.
pub fn account(a: &AccountQuota, now: u64, open: Option<OpenThread>, cx: &App) -> AnyElement {
    let theme = cx.theme().clone();
    let updated = match (a.updated_ms, &a.status) {
        (_, QuotaStatus::Loading) => Some("Reading…".to_owned()),
        (0, _) => None,
        (at, _) => Some(format!("Updated {}", format::relative_time(at, now)).replace("Updated now", "Updated just now")),
    };
    let message = match &a.status {
        QuotaStatus::NoPlan(m) | QuotaStatus::Unavailable(m) => Some(m.clone()),
        QuotaStatus::Unknown if a.windows.is_empty() => Some("Limits appear after the next turn, or once Orbit reads them.".into()),
        _ => None,
    };
    let instance = a.instance.clone();
    v_flex()
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .child(ui::provider_icon(a.kind, cx).small())
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).truncate().child(a.name.clone()))
                .when_some(a.plan.clone(), |el, plan| el.child(div().text_xs().text_color(theme.muted_foreground).child(plan)))
                .child(div().flex_1())
                .when_some(updated, |el, updated| el.child(div().text_xs().text_color(theme.muted_foreground).child(updated))),
        )
        .when_some(message, |el, message| {
            el.child(div().text_xs().text_color(theme.muted_foreground).child(message))
        })
        .children(a.windows.iter().map(|w| {
            let used = w.used();
            let level = Level::of(used, w.window.rejected);
            let color = level.color(&theme);
            let figure = if w.window.rejected { "Limit reached".to_owned() } else { format!("{}%", (used * 100.0).round()) };
            let reset = match w.window.resets_ms {
                0 => None,
                at if at > now => Some(format!("{} ({})", reset_time(at, now), time_until(at, now))),
                _ => None,
            };
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().w(rems(7.5)).text_sm().truncate().child(w.label.clone()))
                        .child(
                            div().flex_1().child(
                                Progress::new(SharedString::from(format!("quota-bar-{instance}-{}", w.window.window)))
                                    .value(used * 100.0)
                                    .color(if level == Level::Room { theme.primary } else { color }),
                            ),
                        )
                        .child(
                            div()
                                .min_w(rems(3.))
                                .text_right()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(if level == Level::Room { theme.foreground } else { color })
                                .child(figure),
                        ),
                )
                .child(
                    v_flex()
                        .pl(rems(8.))
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .when_some(reset, |el, reset| el.child(reset))
                        .when_some(pace_line(w, now), |el, pace| el.child(pace)),
                )
        }))
        .when(!a.top.is_empty(), |el| {
            el.child(
                v_flex()
                    .gap_0p5()
                    .child(div().pt_1().text_xs().font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child("Most used"))
                    .children(a.top.iter().map(|c| {
                        let (thread, open) = (c.thread, open.clone());
                        h_flex()
                            .id(SharedString::from(format!("quota-top-{}-{}", instance, c.thread)))
                            .gap_2()
                            .px_1()
                            .mx(rems(-0.25))
                            .rounded(theme.radius)
                            .text_xs()
                            .when(open.is_some(), |el| el.hover(|s| s.bg(theme.list_hover)))
                            .when_some(open, |el, open| el.on_click(move |_, window, cx| open(thread, window, cx)))
                            .child(div().flex_1().min_w_0().truncate().child(c.title.clone()))
                            .child(div().text_color(theme.muted_foreground).child(format!("{} tokens", format::tokens(c.tokens))))
                    })),
            )
        })
        .into_any_element()
}
