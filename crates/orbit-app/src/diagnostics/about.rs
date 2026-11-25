//! About Orbit: what this build is, and where help is.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::dialog::DialogClose;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::build;
use crate::assets::Lucide;

/// Orbit's documentation. A placeholder (`.invalid` never resolves) until
/// the docs are published; the link stays hidden while it is one. See
/// `docs/RELEASING.md`.
pub const DOCS_URL: &str = "https://docs.orbit.invalid/";

fn docs_url() -> Option<&'static str> {
    (!DOCS_URL.contains(".invalid")).then_some(DOCS_URL)
}

/// Everything About shows, as one copyable block.
fn facts(cx: &App) -> Vec<(&'static str, String)> {
    let (host, _) = crate::settings::connected_host(cx);
    vec![
        ("Version", build::VERSION.to_owned()),
        ("Commit", build::COMMIT.to_owned()),
        ("Build", build::profile().to_owned()),
        ("Renderer", build::backend().to_owned()),
        ("Platform", build::TARGET.to_owned()),
        ("System", build::system()),
        ("Protocol", format!("OWP/{}", orbit_proto::PROTOCOL_VERSION)),
        ("Engine", host.map(|h| format!("orbitd at {h}")).unwrap_or_else(|| "In this app".to_owned())),
    ]
}

pub fn open_about(window: &mut Window, cx: &mut App) {
    let rows = facts(cx);
    let copy: SharedString = rows.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n").into();
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme().clone();
        let header = v_flex()
            .items_center()
            .gap_1()
            .pb_2()
            .child(Icon::new(Lucide::Orbit).large().text_color(theme.primary))
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Orbit"))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("A native control surface for coding agents"),
            );
        let table = v_flex().gap_1().children(rows.iter().map(|(key, value)| {
            h_flex()
                .gap_3()
                .text_sm()
                .child(div().w_24().flex_shrink_0().text_color(theme.muted_foreground).child(*key))
                .child(div().min_w_0().truncate().font_family(theme.mono_font_family.clone()).text_xs().child(value.clone()))
        }));
        let copy = copy.clone();
        dialog
            .w(px(460.))
            .child(
                v_flex()
                    .gap_3()
                    .child(header)
                    .child(
                        v_flex()
                            .gap_2()
                            .p_3()
                            .rounded(theme.radius)
                            .bg(theme.muted)
                            .child(table)
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(Clipboard::new("about-copy").value(copy))
                                    .child("Copy these details"),
                            ),
                    ),
            )
            .footer(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Button::new("about-shortcuts")
                            .ghost()
                            .small()
                            .icon(Lucide::Keyboard)
                            .label("Keyboard shortcuts")
                            .on_click(|_, window, cx| {
                                window.close_dialog(cx);
                                window.dispatch_action(Box::new(crate::actions::ShowShortcuts), cx);
                            }),
                    )
                    .child(
                        Button::new("about-whats-new").ghost().small().label("What’s new").on_click(|_, window, cx| {
                            window.close_dialog(cx);
                            window.dispatch_action(Box::new(crate::update::ShowWhatsNew), cx);
                        }),
                    )
                    .when_some(docs_url(), |el, url| {
                        el.child(
                            Button::new("about-docs")
                                .ghost()
                                .small()
                                .icon(IconName::BookOpen)
                                .label("Documentation")
                                .on_click(move |_, _, cx| cx.open_url(url)),
                        )
                    })
                    .child(div().flex_1())
                    .child(DialogClose::new().child(Button::new("about-close").primary().small().label("Done"))),
            )
    });
}
