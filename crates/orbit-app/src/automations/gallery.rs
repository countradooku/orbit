//! The recipe gallery: ready-made automations as cards. Using one opens the
//! editor filled in, so nothing runs until it is saved.

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_core::automations::recipes::{RECIPES, Recipe};
use orbit_core::automations::describe;
use orbit_proto::ThreadConfig;

use super::AutomationsView;
use crate::assets::Lucide;

fn icon(recipe: &Recipe) -> Icon {
    match recipe.icon {
        "package" => Icon::new(Lucide::Package),
        "wrench" => Icon::new(Lucide::Wrench),
        "inbox" => Icon::new(IconName::Inbox),
        "sunrise" => Icon::new(Lucide::Sunrise),
        "git-branch" => Icon::new(Lucide::GitBranch),
        "eye" => Icon::new(IconName::Eye),
        "shield-check" => Icon::new(Lucide::ShieldCheck),
        _ => Icon::new(Lucide::CalendarClock),
    }
}

/// The cards in `columns` columns.
pub fn render(view: WeakEntity<AutomationsView>, columns: u16, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let cards = RECIPES.iter().map(|recipe| {
        let spec = recipe.spec(None, &ThreadConfig::default());
        let when: Vec<String> = spec.triggers.iter().map(describe).collect();
        let id = recipe.id;
        let view = view.clone();
        v_flex()
            .id(SharedString::from(format!("recipe-{id}")))
            .w_full()
            .p_4()
            .gap_3()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_none()
                            .p_2()
                            .rounded(theme.radius)
                            .bg(theme.primary.opacity(0.1))
                            .child(icon(recipe).small().text_color(theme.primary)),
                    )
                    .child(div().flex_1().min_w_0().text_sm().font_weight(FontWeight::SEMIBOLD).child(recipe.title)),
            )
            .child(div().flex_1().text_sm().text_color(theme.muted_foreground).child(recipe.blurb))
            .child(
                h_flex()
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(if spec.triggers.iter().all(|t| t.is_schedule()) { Lucide::CalendarClock } else { Lucide::Zap }).xsmall())
                    .child(div().truncate().child(capital(&when.join(", ")))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("recipe-use-{id}")))
                            .small()
                            .outline()
                            .label("Use recipe…")
                            .on_click(move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| this.open_recipe(id, window, cx));
                            }),
                    )
                    .when(!recipe.needs.is_empty(), |el| {
                        el.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("Needs {}", lower_first(recipe.needs))),
                        )
                    }),
            )
    });
    // Recipes plugins add, after Orbit's own.
    let plugin_cards = crate::plugins::recipes(cx).into_iter().map(|(plugin, recipe)| {
        let view = view.clone();
        let schedule = if recipe.rule.is_empty() { "Runs when you start it".to_owned() } else { capital(&describe(&orbit_proto::Trigger::Rule { rule: recipe.rule.clone(), tz: String::new() })) };
        v_flex()
            .id(SharedString::from(format!("plugin-recipe-{}", recipe.id)))
            .w_full()
            .p_4()
            .gap_3()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                h_flex()
                    .gap_3()
                    .child(div().flex_none().p_2().rounded(theme.radius).bg(theme.primary.opacity(0.1)).child(Icon::new(Lucide::Puzzle).small().text_color(theme.primary)))
                    .child(div().flex_1().min_w_0().text_sm().font_weight(FontWeight::SEMIBOLD).child(recipe.title.clone())),
            )
            .child(div().flex_1().text_sm().text_color(theme.muted_foreground).child(recipe.blurb.clone()))
            .child(div().truncate().text_xs().text_color(theme.muted_foreground).child(format!("{schedule} · from {plugin}")))
            .child(
                h_flex().child(
                    Button::new(SharedString::from(format!("plugin-recipe-use-{}", recipe.id))).small().outline().label("Use recipe…").on_click(
                        move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.open_plugin_recipe(&recipe, window, cx));
                        },
                    ),
                ),
            )
    });
    let cards: Vec<AnyElement> = cards.map(|c| c.into_any_element()).chain(plugin_cards.map(|c| c.into_any_element())).collect();
    v_flex()
        .gap_3()
        .child(
            v_flex()
                .gap_0p5()
                .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child("Start from a recipe"))
                .child(div().text_sm().text_color(theme.muted_foreground).child("Each one opens filled in; change anything before you create it.")),
        )
        .child(div().grid().grid_cols(columns).gap_3().children(cards))
}

fn capital(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        // Keep names (GitHub) as they are.
        Some(c) if text.split_whitespace().next().is_some_and(|w| w.chars().skip(1).all(|c| c.is_lowercase())) => {
            c.to_lowercase().chain(chars).collect()
        }
        Some(_) => text.to_owned(),
        None => String::new(),
    }
}
