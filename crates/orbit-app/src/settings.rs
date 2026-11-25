//! Settings: appearance, agents, and where your data lives.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;
use crate::theme::{self, Appearance};

/// App-wide preferences that are not engine state.
#[derive(Default)]
pub struct Preferences {
    pub appearance: Appearance,
}

impl Global for Preferences {}

pub fn appearance(cx: &App) -> Appearance {
    cx.try_global::<Preferences>().map(|p| p.appearance).unwrap_or_default()
}

pub fn open(app: Entity<AppState>, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme().clone();
        let state = app.read(cx).state();
        let current = appearance(cx);
        let providers = state.providers.clone();
        let refresh = app.clone();
        dialog.title("Settings").w(px(560.)).child(
            v_flex()
                .gap_5()
                .child(
                    v_flex().gap_2().child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Appearance")).child(
                        RadioGroup::horizontal("appearance")
                            .child("Match system")
                            .child("Light")
                            .child("Dark")
                            .selected_index(Some(match current {
                                Appearance::System => 0,
                                Appearance::Light => 1,
                                Appearance::Dark => 2,
                            }))
                            .on_change(|ix: &usize, window, cx| {
                                let appearance = match ix {
                                    1 => Appearance::Light,
                                    2 => Appearance::Dark,
                                    _ => Appearance::System,
                                };
                                cx.set_global(Preferences { appearance });
                                theme::apply(appearance, window, cx);
                            }),
                    ),
                )
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            h_flex()
                                .child(div().flex_1().text_sm().font_weight(FontWeight::MEDIUM).child("Agents"))
                                .child(
                                    Button::new("refresh-providers")
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::RefreshCw)
                                        .label("Check again")
                                        .on_click(move |_, window, cx| {
                                            refresh.update(cx, |app, cx| app.run(Command::RefreshProviders, window, cx))
                                        }),
                                ),
                        )
                        .children(providers.iter().map(|p| {
                            let (color, status) = match (p.installed, p.signed_in) {
                                (false, _) => (theme.muted_foreground, p.message.clone().unwrap_or_else(|| "Not installed".into())),
                                (true, Some(false)) => (theme.warning, "Signed out — sign in from its own CLI".into()),
                                (true, _) => (
                                    theme.success,
                                    match (&p.version, &p.account) {
                                        (Some(v), Some(a)) => format!("{v} · {a}"),
                                        (Some(v), None) => v.clone(),
                                        _ => "Ready".into(),
                                    },
                                ),
                            };
                            h_flex()
                                .gap_2()
                                .px_3()
                                .py_2()
                                .rounded(theme.radius)
                                .bg(theme.muted)
                                .child(div().size_2().rounded_full().bg(color))
                                .child(div().w(rems(7.)).text_sm().child(p.kind.label()))
                                .child(div().flex_1().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(status))
                                .when(p.installed, |el| {
                                    el.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(format!("{} models", p.models.len())),
                                    )
                                })
                        })),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Data"))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .font_family(theme.mono_font_family.clone())
                                        .child(orbit_core::EngineOptions::default_data_dir().display().to_string()),
                                )
                                .child(Button::new("reveal-data").outline().xsmall().label("Show in Explorer").on_click(
                                    |_, _, cx| cx.reveal_path(&orbit_core::EngineOptions::default_data_dir()),
                                )),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Orbit keeps everything on this computer. There is no account and no telemetry."),
                        ),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Remote host"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Run agents on another machine or in WSL: start `orbitd` there, then launch Orbit with ORBIT_HOST=ws://host:7840/ and ORBIT_TOKEN from `orbitd --print-token`."),
                        ),
                )

        )
    });
}
