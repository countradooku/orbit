//! Issues from the project's host, Linear or Jira, and starting a thread
//! from one.

use std::ops::Range;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::Input;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::{ForgeView, glyph, integrations, list};
use crate::{format, ui};

/// The trackers the switcher offers: the project's own host, then the
/// account trackers.
fn trackers(view: &ForgeView) -> [TrackerKind; 3] {
    let own = if view.forge_kind() == ForgeKind::GitLab { TrackerKind::GitLab } else { TrackerKind::GitHub };
    [own, TrackerKind::Linear, TrackerKind::Jira]
}

pub fn render(view: &mut ForgeView, window: &mut Window, cx: &mut Context<ForgeView>) -> impl IntoElement {
    let choices = trackers(view);
    let selected = choices.iter().position(|t| *t == view.tracker).unwrap_or(0);
    let head = v_flex()
        .flex_none()
        .px_3()
        .py_2()
        .gap_2()
        .child(
            TabBar::new("forge-tracker")
                .segmented()
                .xsmall()
                .selected_index(selected)
                .on_click(cx.listener(move |this, ix: &usize, _, cx| this.set_tracker(choices[*ix], cx)))
                .children(choices.iter().map(|t| Tab::new().label(t.label()))),
        )
        .child(
            Input::new(&view.query)
                .small()
                .prefix(Icon::new(IconName::Search).xsmall())
                .cleanable(true),
        );
    let configured = integrations::get(cx).configured(view.tracker);
    let body = if !configured {
        let (what, fix) = match view.tracker {
            TrackerKind::Linear => ("Connect Linear", "Paste a personal API key from Linear › Settings › Security & access."),
            _ => ("Connect Jira", "Add your Jira site, email and API token."),
        };
        ui::empty_state(
            Icon::new(IconName::Settings2),
            what,
            format!("{fix} Orbit keeps it in its data folder and never writes it to logs."),
            Some(
                Button::new("forge-open-settings")
                    .label("Open settings…")
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(crate::actions::OpenSettings), cx))
                    .into_any_element(),
            ),
            cx,
        )
        .into_any_element()
    } else {
        match (&view.issues.value, &view.issues.error) {
            (None, Some(error)) => {
                ui::empty_state(Icon::new(IconName::CircleAlert), "Couldn’t load issues", error.clone(), None, cx).into_any_element()
            }
            (None, None) => list::skeleton(cx).into_any_element(),
            (Some(list), _) if list.items.is_empty() => {
                let searching = !view.query.read(cx).value().trim().is_empty();
                let (title, help) = match (searching, view.tracker.is_forge()) {
                    (true, _) => ("No matching issues", "Try other words, or clear the search."),
                    (false, true) => ("No open issues", "Search to include closed ones."),
                    (false, false) => ("Nothing assigned to you", "Search to find any issue."),
                };
                ui::empty_state(Icon::new(IconName::CircleCheck), title, help, None, cx).into_any_element()
            }
            (Some(list), _) => uniform_list("forge-issue-rows", list.items.len(), cx.processor(ForgeView::render_issue_rows))
                .track_scroll(&view.scroll)
                .flex_1()
                .into_any_element(),
        }
    };
    let list_panel = v_flex().size_full().child(head).child(body);
    let detail = render_detail(view, window, cx);
    list::split(view, "forge-issues", list_panel, detail, cx)
}

impl ForgeView {
    fn render_issue_rows(&mut self, range: Range<usize>, _: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let now = now_ms();
        let Some(list) = &self.issues.value else { return Vec::new() };
        list.items[range.start.min(list.items.len())..range.end.min(list.items.len())]
            .iter()
            .map(|issue| {
                let key = issue.key.clone();
                let selected = self.selected_issue.as_ref() == Some(&issue.key);
                let mut meta = vec![issue.key.clone()];
                if !issue.status.is_empty() && !issue.tracker.is_forge() {
                    meta.push(issue.status.clone());
                }
                meta.push(format::relative_time(issue.updated_ms, now));
                div()
                    .px_1p5()
                    .py_0p5()
                    .child(
                        h_flex()
                            .id(SharedString::from(format!("forge-issue-{key}")))
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .items_start()
                            .rounded(theme.radius)
                            .cursor_default()
                            .when(selected, |el| el.bg(theme.list_active))
                            .when(!selected, |el| el.hover(|s| s.bg(theme.list_hover)))
                            .on_click(cx.listener(move |this, _, _, cx| this.select_issue(key.clone(), cx)))
                            .child(div().pt_0p5().child(glyph::issue(issue.state, cx)))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .truncate()
                                            .text_sm()
                                            .when(selected, |el| el.font_weight(FontWeight::MEDIUM))
                                            .child(issue.title.clone()),
                                    )
                                    .child(div().truncate().text_xs().text_color(theme.muted_foreground).child(meta.join(" · "))),
                            ),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

fn render_detail(view: &mut ForgeView, _: &mut Window, cx: &mut Context<ForgeView>) -> AnyElement {
    let Some(detail) = view.issue.value.clone() else {
        return match (&view.issue.error, &view.selected_issue) {
            (Some(error), _) => ui::empty_state(Icon::new(IconName::CircleAlert), "Couldn’t load it", error.clone(), None, cx).into_any_element(),
            (None, Some(_)) => list::skeleton(cx).into_any_element(),
            (None, None) => div().into_any_element(),
        };
    };
    let theme = cx.theme().clone();
    let issue = &detail.issue;
    let now = now_ms();
    let url = issue.url.clone();
    let has_project = view.project.is_some();
    let mut facts = vec![issue.key.clone()];
    if !issue.status.is_empty() {
        facts.push(issue.status.clone());
    }
    if !issue.author.is_empty() {
        facts.push(format!("opened by {}", issue.author));
    }
    facts.push(match &issue.assignee {
        Some(a) => format!("assigned to {a}"),
        None => "unassigned".into(),
    });
    facts.push(format!("updated {}", format::relative_time(issue.updated_ms, now)));
    let branch_hint = issue.branch.clone().unwrap_or_else(|| orbit_core::forge::prompt::issue_branch(issue));
    let key_slug = issue.key.clone();
    let comments = detail.comments.iter().enumerate().map(|(i, c)| {
        v_flex()
            .gap_1p5()
            .child(
                h_flex()
                    .gap_1p5()
                    .text_sm()
                    .child(div().font_weight(FontWeight::MEDIUM).child(c.author.clone()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(format::relative_time(c.created_ms, now))),
            )
            .child(TextView::markdown(SharedString::from(format!("forge-issue-{key_slug}-c{i}")), c.body.clone()).selectable(true))
    });
    let busy = view.busy;
    v_flex()
        .id(SharedString::from(format!("forge-issue-detail-{}", issue.key)))
        .size_full()
        .overflow_y_scroll()
        .child(
            v_flex()
                .w_full()
                .max_w(rems(60.))
                .px_6()
                .py_5()
                .gap_6()
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(issue.title.clone()))
                        .child(div().text_sm().text_color(theme.muted_foreground).child(facts.join(" · ")))
                        .when(!issue.labels.is_empty(), |el| {
                            el.child(h_flex().gap_1().flex_wrap().children(issue.labels.iter().map(|l| Tag::secondary().small().child(l.clone()))))
                        }),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .flex_wrap()
                        .child(
                            Button::new("forge-start-thread")
                                .primary()
                                .small()
                                .icon(IconName::Plus)
                                .label("Start a thread")
                                .tooltip("Send the issue to a new agent thread as its first message")
                                .loading(busy == Some("start"))
                                .disabled(busy.is_some() || !has_project)
                                .on_click(cx.listener(|this, _, window, cx| this.start_from_issue(window, cx))),
                        )
                        .child(
                            Checkbox::new("forge-start-worktree")
                                .label(format!("On a new worktree ({branch_hint})"))
                                .checked(view.on_worktree)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.on_worktree = *checked;
                                    cx.notify();
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("forge-open-issue")
                                .ghost()
                                .small()
                                .icon(IconName::ExternalLink)
                                .label(format!("Open in {}", issue.tracker.label()))
                                .on_click(move |_, _, cx| ForgeView::open_url(&url, cx)),
                        ),
                )
                .child(if detail.body.trim().is_empty() {
                    div().text_sm().text_color(theme.muted_foreground).child("No description.").into_any_element()
                } else {
                    TextView::markdown(SharedString::from(format!("forge-issue-body-{}", issue.key)), detail.body.clone())
                        .selectable(true)
                        .into_any_element()
                })
                .when(!detail.comments.is_empty(), |el| {
                    el.child(
                        v_flex()
                            .gap_4()
                            .child(
                                h_flex()
                                    .pb_2()
                                    .border_b_1()
                                    .border_color(theme.border)
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("Comments · {}", detail.comments.len())),
                            )
                            .children(comments),
                    )
                }),
        )
        .into_any_element()
}
