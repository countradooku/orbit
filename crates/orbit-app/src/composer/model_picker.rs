//! The model pill and its picker: which agent, which model, how hard it
//! thinks.
//!
//! One pill for all of it; the picker opens with the pointer, `/model` or
//! Ctrl+Shift+M, and typing filters models across every agent (↑↓ and Enter
//! pick). Ctrl+Shift+E steps the reasoning effort without opening anything.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, MoveDown, MoveUp};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_core::fuzzy;
use orbit_proto::*;

use super::Composer;
use crate::app_state::AppState;
use crate::ui;

pub(super) struct ModelPicker {
    open: bool,
    search: Entity<InputState>,
    /// Index into the filtered model list.
    selected: usize,
    _subscriptions: Vec<Subscription>,
}

impl ModelPicker {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Composer>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search models"));
        let sub = cx.subscribe_in(&search, window, |this: &mut Composer, _, event: &InputEvent, window, cx| match event {
            InputEvent::Change => {
                this.model_picker.selected = 0;
                cx.notify();
            }
            InputEvent::PressEnter { .. } => this.pick_selected_model(window, cx),
            _ => {}
        });
        Self { open: false, search, selected: 0, _subscriptions: vec![sub] }
    }

    /// Opens the picker with an empty search, focused.
    pub(super) fn open(&mut self, window: &mut Window, cx: &mut Context<Composer>) {
        self.open = true;
        self.selected = 0;
        self.search.update(cx, |search, cx| {
            search.set_value("", window, cx);
            search.focus(window, cx);
        });
        cx.notify();
    }

    pub(super) fn render_pill(
        &self,
        config: &ThreadConfig,
        app: &Entity<AppState>,
        _: &mut Window,
        cx: &mut Context<Composer>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let state = app.read(cx).state();
        let (name, extras) = model_summary(config, &state.providers);
        let providers: Rc<Vec<ProviderInfo>> = Rc::new(state.providers.clone());
        let config = config.clone();
        let this = cx.weak_entity();
        let icon = ui::provider_icon(config.provider, cx);
        let query = self.search.read(cx).value().trim().to_owned();
        let search = self.search.clone();
        let selected = self.selected;
        Popover::new("model-picker")
            .anchor(Anchor::BottomRight)
            .open(self.open)
            .on_open_change(cx.listener(|this, open: &bool, window, cx| {
                if *open {
                    this.model_picker.open(window, cx);
                } else {
                    this.model_picker.open = false;
                    Composer::focus_input(this, window, cx);
                    cx.notify();
                }
            }))
            .trigger(
                Button::new("model-pill")
                    .ghost()
                    .small()
                    .tooltip("Agent and model (Ctrl+Shift+M) · effort (Ctrl+Shift+E)")
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(icon.xsmall())
                            .child(div().font_weight(FontWeight::MEDIUM).child(name))
                            .when_some(extras, |el, extras| el.child(div().text_color(theme.muted_foreground).child(extras))),
                    ),
            )
            .content(move |_, _, cx| render_menu(&providers, &config, &query, selected, &search, &this, cx))
            .into_any_element()
    }
}

impl Composer {
    fn focus_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Picks the highlighted model of the filtered list.
    fn pick_selected_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.model_picker.search.read(cx).value().trim().to_owned();
        let providers = self.app.read(cx).state().providers.clone();
        let matches = filtered(&providers, &query);
        if let Some((kind, slug)) = matches.get(self.model_picker.selected).map(|(p, m)| (p.kind, m.slug.clone())) {
            self.choose_model(kind, slug, cx);
            self.model_picker.open = false;
            self.focus_input(window, cx);
        }
    }

    fn choose_model(&mut self, kind: ProviderKind, slug: String, cx: &mut Context<Self>) {
        self.update_config(
            |c| {
                if c.provider != kind {
                    c.options.clear();
                }
                c.provider = kind;
                c.model = slug;
            },
            cx,
        );
    }

    fn move_model_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let query = self.model_picker.search.read(cx).value().trim().to_owned();
        let count = filtered(&self.app.read(cx).state().providers, &query).len();
        if count > 0 {
            let next = (self.model_picker.selected as isize + delta).rem_euclid(count as isize);
            self.model_picker.selected = next as usize;
            cx.notify();
        }
    }

    /// Steps the model's reasoning effort to the next level (wrapping).
    pub(super) fn cycle_effort(&mut self, cx: &mut Context<Self>) {
        let providers = self.app.read(cx).state().providers.clone();
        let Some(model) = selected_model(&self.config, &providers) else { return };
        let Some((option, choices)) = model.options.iter().find_map(|o| match &o.kind {
            OptionKind::Select { choices } if o.id.to_lowercase().contains("effort") && !choices.is_empty() => {
                Some((o, choices))
            }
            _ => None,
        }) else {
            return;
        };
        let current = chosen(&self.config, option);
        let ix = choices.iter().position(|c| OptionSetting::Choice(c.id.clone()) == current).map_or(0, |i| i + 1);
        let next = choices[ix % choices.len()].id.clone();
        let id = option.id.clone();
        self.update_config(|c| set_option(c, id, OptionSetting::Choice(next)), cx);
    }
}

/// Models matching `query` across agents, in catalog order when the query
/// is empty, best match first otherwise.
fn filtered<'a>(providers: &'a [ProviderInfo], query: &str) -> Vec<(&'a ProviderInfo, &'a ModelInfo)> {
    let all = providers.iter().flat_map(|p| p.models.iter().map(move |m| (p, m)));
    if query.is_empty() {
        return all.collect();
    }
    let scored = all.filter_map(|(p, m)| {
        let haystack = format!("{} {} {}", m.name, m.slug, p.kind.label());
        fuzzy::score(query, &haystack)
            // Descriptions match words, not scattered letters.
            .or_else(|| m.description.as_deref().filter(|d| d.to_lowercase().contains(&query.to_lowercase())).map(|_| -200))
            .map(|s| (s, (p, m)))
    });
    fuzzy::top(scored, 200)
}

pub(super) fn selected_model<'a>(config: &ThreadConfig, providers: &'a [ProviderInfo]) -> Option<&'a ModelInfo> {
    let provider = providers.iter().find(|p| p.kind == config.provider)?;
    provider
        .models
        .iter()
        .find(|m| m.slug == config.model)
        .or_else(|| if config.model.is_empty() { provider.models.iter().find(|m| m.is_default) } else { None })
}

fn chosen(config: &ThreadConfig, option: &OptionDescriptor) -> OptionSetting {
    config.options.iter().find(|v| v.id == option.id).map(|v| v.value.clone()).unwrap_or_else(|| option.default_setting())
}

fn set_option(config: &mut ThreadConfig, id: String, value: OptionSetting) {
    match config.options.iter_mut().find(|o| o.id == id) {
        Some(slot) => slot.value = value,
        None => config.options.push(OptionValue { id, value }),
    }
}

/// The current model and its non-default options, read from the provider's
/// catalog: ("Opus 5.5", "High").
fn model_summary(config: &ThreadConfig, providers: &[ProviderInfo]) -> (String, Option<String>) {
    let name = ui::model_label(config, providers);
    let Some(model) = selected_model(config, providers) else { return (name, None) };
    let mut extras = Vec::new();
    for option in &model.options {
        match (&option.kind, chosen(config, option)) {
            (OptionKind::Select { choices }, OptionSetting::Choice(id)) if id != "default" => {
                if let Some(c) = choices.iter().find(|c| c.id == id) {
                    extras.push(c.label.clone());
                }
            }
            (OptionKind::Toggle { default }, OptionSetting::Toggle(on)) if on != *default && on => {
                extras.push(option.label.clone());
            }
            _ => {}
        }
    }
    (name, (!extras.is_empty()).then(|| extras.join(" · ")))
}

/// The picker: a search field, agents and their models, then the selected
/// model's options as segmented choices and switches.
fn render_menu(
    providers: &[ProviderInfo],
    config: &ThreadConfig,
    query: &str,
    selected: usize,
    search: &Entity<InputState>,
    this: &WeakEntity<Composer>,
    cx: &mut App,
) -> Div {
    let theme = cx.theme().clone();
    let matches = filtered(providers, query);
    let highlighted = (!matches.is_empty()).then(|| selected.min(matches.len() - 1));
    let mut list = v_flex().gap_0p5();
    let mut shown_provider: Option<ProviderKind> = None;
    for (ix, (provider, model)) in matches.iter().enumerate() {
        if shown_provider != Some(provider.kind) {
            shown_provider = Some(provider.kind);
            list = list.child(
                h_flex()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .gap_1p5()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
                    .child(ui::provider_icon(provider.kind, cx).xsmall())
                    .child(provider.kind.label())
                    .when(!provider.installed, |el| el.child(div().flex_1().text_right().child("Not installed"))),
            );
            if !provider.installed && query.is_empty() {
                list = list.child(
                    div()
                        .px_2()
                        .pb_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .whitespace_normal()
                        .child(provider.message.clone().unwrap_or_else(|| "Install its command-line tool to use it.".into())),
                );
            }
        }
        let current =
            config.provider == provider.kind && (config.model == model.slug || (config.model.is_empty() && model.is_default));
        let is_highlighted = highlighted == Some(ix) && !query.is_empty();
        let (kind, slug, this) = (provider.kind, model.slug.clone(), this.clone());
        list = list.child(
            h_flex()
                .id(SharedString::from(format!("model-{}-{}", provider.kind.label(), model.slug)))
                .px_2()
                .py_1p5()
                .gap_2()
                .rounded(theme.radius)
                .cursor_default()
                .when(current || is_highlighted, |el| el.bg(theme.accent))
                .when(!current && !is_highlighted, |el| el.hover(|s| s.bg(theme.muted)))
                .on_click(move |_, window, cx| {
                    let slug = slug.clone();
                    let _ = this.update(cx, |this, cx| {
                        this.choose_model(kind, slug, cx);
                        this.model_picker.open = false;
                        this.focus_input(window, cx);
                    });
                })
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().child(model.name.clone()))
                        .when_some(model.description.clone(), |el, d| {
                            el.child(div().text_xs().text_color(theme.muted_foreground).truncate().child(d))
                        }),
                )
                .when(current, |el| el.child(Icon::new(IconName::Check).xsmall().text_color(theme.primary))),
        );
    }
    if matches.is_empty() {
        list = list.child(
            div().px_2().py_2().text_sm().text_color(theme.muted_foreground).child(format!("No models match “{query}”")),
        );
    }
    // Models scroll; the search stays above and the options below.
    let models = div().id("model-list").max_h(rems(19.)).overflow_y_scroll().child(list);
    let up = this.clone();
    let down = this.clone();
    let mut menu = v_flex()
        .w(rems(22.))
        .capture_action(move |_: &MoveUp, _, cx| {
            let _ = up.update(cx, |this, cx| this.move_model_selection(-1, cx));
            cx.stop_propagation();
        })
        .capture_action(move |_: &MoveDown, _, cx| {
            let _ = down.update(cx, |this, cx| this.move_model_selection(1, cx));
            cx.stop_propagation();
        })
        .child(div().p_1().child(Input::new(search).small().prefix(Icon::new(IconName::Search).xsmall())))
        .child(models);
    if query.is_empty()
        && let Some(model) = selected_model(config, providers)
        && !model.options.is_empty()
    {
        menu = menu.child(div().my_1().h_px().bg(theme.border));
        for option in &model.options {
            menu = menu.child(render_option(option, config, this, cx));
        }
    }
    menu
}

fn render_option(option: &OptionDescriptor, config: &ThreadConfig, this: &WeakEntity<Composer>, cx: &mut App) -> AnyElement {
    let theme = cx.theme().clone();
    let current = chosen(config, option);
    let option_id = option.id.clone();
    match &option.kind {
        OptionKind::Select { choices } => {
            let mut group = h_flex().gap_0p5().p_0p5().rounded(theme.radius).bg(theme.muted);
            for choice in choices {
                let selected = current == OptionSetting::Choice(choice.id.clone());
                let (this, option_id, id) = (this.clone(), option_id.clone(), choice.id.clone());
                group = group.child(
                    div()
                        .id(SharedString::from(format!("choice-{}-{}", option.id, choice.id)))
                        .flex_1()
                        .py_1()
                        .rounded(theme.radius)
                        .text_center()
                        .text_xs()
                        .cursor_default()
                        .when(selected, |el| el.bg(theme.background).shadow_xs().font_weight(FontWeight::MEDIUM))
                        .when(!selected, |el| {
                            el.text_color(theme.muted_foreground).hover(|s| s.text_color(theme.foreground))
                        })
                        .when_some(choice.description.clone(), |el, d| {
                            el.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(d.clone()).build(window, cx))
                        })
                        .on_click(move |_, _, cx| {
                            let (option_id, id) = (option_id.clone(), id.clone());
                            let _ = this.update(cx, |this, cx| {
                                this.update_config(|c| set_option(c, option_id, OptionSetting::Choice(id)), cx)
                            });
                        })
                        .child(choice.label.clone()),
                );
            }
            let is_effort = option.id.to_lowercase().contains("effort");
            v_flex()
                .px_2()
                .py_1()
                .gap_1()
                .child(
                    h_flex()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(option.label.clone())
                        .when(is_effort, |el| el.child(div().flex_1()).child("Ctrl+Shift+E")),
                )
                .child(group)
                .into_any_element()
        }
        OptionKind::Toggle { .. } => {
            let on = current == OptionSetting::Toggle(true);
            let this = this.clone();
            h_flex()
                .px_2()
                .py_1p5()
                .child(
                    v_flex().flex_1().child(div().text_sm().child(option.label.clone())).when_some(
                        option.description.clone(),
                        |el, d| el.child(div().text_xs().text_color(theme.muted_foreground).child(d)),
                    ),
                )
                .child(Switch::new(SharedString::from(format!("switch-{}", option.id))).small().checked(on).on_click(
                    move |checked: &bool, _, cx| {
                        let (option_id, checked) = (option_id.clone(), *checked);
                        let _ = this.update(cx, |this, cx| {
                            this.update_config(|c| set_option(c, option_id, OptionSetting::Toggle(checked)), cx)
                        });
                    },
                ))
                .into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use orbit_proto::{ModelInfo, ProviderInfo, ProviderKind};

    use super::filtered;

    fn model(slug: &str, name: &str) -> ModelInfo {
        ModelInfo { slug: slug.into(), name: name.into(), description: None, context_window: 0, is_default: false, options: vec![] }
    }

    #[test]
    fn search_spans_agents_and_ranks_names_first() {
        let providers = vec![
            ProviderInfo { kind: ProviderKind::Claude, models: vec![model("opus", "Opus 5.5"), model("haiku", "Haiku 5.5")], ..Default::default() },
            ProviderInfo { kind: ProviderKind::Codex, models: vec![model("gpt-6", "GPT-6 Astra")], ..Default::default() },
        ];
        assert_eq!(filtered(&providers, "").len(), 3);
        let names = |q: &str| filtered(&providers, q).into_iter().map(|(_, m)| m.name.clone()).collect::<Vec<_>>();
        assert_eq!(names("haiku"), vec!["Haiku 5.5"]);
        assert_eq!(names("codex"), vec!["GPT-6 Astra"], "the agent's name finds its models");
        assert!(names("zzz").is_empty());
    }
}
