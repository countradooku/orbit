//! A pull request: what it is, whether it can merge, its checks, and the
//! conversation, with the actions that move it on.

use gpui_kit::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::{ForgeView, glyph};
use crate::assets::Lucide;
use crate::{format, ui};

pub fn render(view: &mut ForgeView, _: &mut Window, cx: &mut Context<ForgeView>) -> AnyElement {
    let Some(detail) = view.pr.value.clone() else {
        return match (&view.pr.error, view.selected_pr) {
            (Some(error), _) => ui::empty_state(Icon::new(IconName::CircleAlert), "Couldn’t load it", error.clone(), None, cx).into_any_element(),
            (None, Some(_)) => super::list::skeleton(cx).into_any_element(),
            (None, None) => div().into_any_element(),
        };
    };
    let theme = cx.theme().clone();
    let kind = view.forge_kind();
    let pr = &detail.pr;
    let now = now_ms();
    let number = pr.number;

    let header = v_flex()
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .items_start()
                .child(div().flex_1().min_w_0().text_lg().font_weight(FontWeight::SEMIBOLD).child(pr.title.clone()))
                .child(state_tag(pr.state, cx)),
        )
        .child(
            h_flex()
                .flex_wrap()
                .gap_x_1p5()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(format!("{}{}", kind.review_sigil(), number))
                .child("·")
                .child(div().text_color(theme.foreground).child(pr.author.clone()))
                .child("wants to merge")
                .child(branch_chip(&pr.head, cx))
                .child("into")
                .child(branch_chip(&pr.base, cx))
                .child("·")
                .child(format!("updated {}", format::relative_time(pr.updated_ms, now))),
        )
        .when(!pr.labels.is_empty(), |el| {
            el.child(h_flex().gap_1().flex_wrap().children(pr.labels.iter().map(|l| Tag::secondary().small().child(l.clone()))))
        });

    let actions = render_actions(view, &detail, cx);
    let merge_line = merge_status(&detail, cx);
    let checks = render_checks(&detail, cx);
    let conversation = render_conversation(&detail, cx);

    v_flex()
        .id(("forge-pr-detail", number))
        .size_full()
        .overflow_y_scroll()
        .child(
            v_flex()
                .w_full()
                .max_w(rems(60.))
                .px_6()
                .py_5()
                .gap_6()
                .child(v_flex().gap_4().child(header).child(actions).child(merge_line))
                .child(checks)
                .child(conversation),
        )
        .into_any_element()
}

fn state_tag(state: PrState, cx: &App) -> Tag {
    let color = glyph::pr_state_color(state, cx);
    let theme = cx.theme();
    Tag::custom(color.opacity(0.14), color, color.opacity(0.3)).small().child(state.label()).when(state == PrState::Draft, |t| {
        t.text_color(theme.muted_foreground)
    })
}

fn branch_chip(name: &str, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .px_1()
        .rounded(theme.radius)
        .bg(theme.muted)
        .text_color(theme.foreground)
        .font_family(theme.mono_font_family.clone())
        .text_xs()
        .child(name.to_owned())
}

fn render_actions(view: &ForgeView, detail: &PullRequestDetail, cx: &mut Context<ForgeView>) -> Div {
    let pr = &detail.pr;
    let busy = view.busy;
    let failing = detail.checks.iter().filter(|c| c.state == CheckState::Failed).count();
    let methods: Vec<MergeMethod> =
        if detail.merge_methods.is_empty() { MergeMethod::ALL.to_vec() } else { detail.merge_methods.clone() };
    let method = view.method;
    let url = pr.url.clone();
    let mut row = h_flex().gap_2().flex_wrap();

    // Drafts merge (or auto-merge) only once marked ready, on the host.
    if pr.state == PrState::Open {
        let can_merge = detail.mergeable.can_merge() && pr.state != PrState::Draft;
        let auto = detail.auto_merge.is_some();
        let view_handle = cx.weak_entity();
        let menu_methods = methods.clone();
        let main = if can_merge {
            Button::new("forge-merge")
                .primary()
                .small()
                .icon(Lucide::GitMerge)
                .label(format!("{}…", method.label()))
                .loading(busy == Some("merge"))
                .on_click(cx.listener(move |this, _, window, cx| this.confirm_merge(method, window, cx)))
        } else if auto {
            Button::new("forge-merge")
                .small()
                .label("Disable auto-merge")
                .loading(busy == Some("auto-off"))
                .on_click(cx.listener(|this, _, window, cx| this.set_auto_merge(false, window, cx)))
        } else {
            Button::new("forge-merge")
                .small()
                .icon(Lucide::GitMerge)
                .label("Enable auto-merge")
                .loading(busy == Some("auto"))
                .tooltip(format!("Merge with “{}” once checks and reviews pass.", method.label()))
                .on_click(cx.listener(|this, _, window, cx| this.set_auto_merge(true, window, cx)))
        };
        row = row.child(
            DropdownButton::new("forge-merge-menu")
                .button(main)
                .disabled(busy.is_some())
                .menu_accessibility_label("Merge options")
                .dropdown_menu(move |mut menu, _, _| {
                    for m in &menu_methods {
                        let (m, view) = (*m, view_handle.clone());
                        menu = menu.item(PopupMenuItem::new(m.label()).checked(m == method).on_click(move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| {
                                this.method = m;
                                cx.notify();
                            });
                        }));
                    }
                    let view = view_handle.clone();
                    menu.separator().item(
                        PopupMenuItem::new(if auto { "Disable auto-merge" } else { "Enable auto-merge" }).on_click(move |_, window, cx| {
                            let _ = view.update(cx, |this, cx| this.set_auto_merge(!auto, window, cx));
                        }),
                    )
                }),
        );
    }
    if failing > 0 {
        row = row.child(
            Button::new("forge-fix-checks")
                .small()
                .icon(Lucide::Wrench)
                .label(if failing == 1 { "Fix the failing check".to_owned() } else { format!("Fix {failing} failing checks") })
                .tooltip("Send the failing checks’ logs to the agent on this branch")
                .loading(busy == Some("fix"))
                .disabled(busy.is_some())
                .on_click(cx.listener(|this, _, window, cx| this.fix_checks(window, cx))),
        );
    }
    if pr.state.is_open() {
        row = row.child(
            Button::new("forge-checkout")
                .small()
                .outline()
                .icon(Lucide::GitBranch)
                .label("Open in a thread")
                .tooltip("Check out the branch on its own worktree, or go to the thread that has it")
                .loading(busy == Some("checkout"))
                .disabled(busy.is_some())
                .on_click(cx.listener(|this, _, window, cx| this.checkout(window, cx))),
        );
    }
    row.child(div().flex_1()).child(
        Button::new("forge-open-pr")
            .ghost()
            .small()
            .icon(IconName::ExternalLink)
            .label(format!("Open on {}", view.forge_kind().label()))
            .on_click(move |_, _, cx| ForgeView::open_url(&url, cx)),
    )
}

/// One line on whether it can merge now, and why not.
fn merge_status(detail: &PullRequestDetail, cx: &App) -> Div {
    let theme = cx.theme();
    let pr = &detail.pr;
    let (icon, color, text) = match pr.state {
        PrState::Merged => (Icon::new(Lucide::GitMerge), theme.magenta, "Merged".to_owned()),
        PrState::Closed => (Icon::new(IconName::CircleX), theme.muted_foreground, "Closed without merging".to_owned()),
        _ => {
            let review = match pr.review {
                ReviewDecision::Approved => " · approved",
                ReviewDecision::ChangesRequested => " · changes requested",
                ReviewDecision::ReviewRequired => " · review required",
                ReviewDecision::None => "",
            };
            let auto = detail.auto_merge.map(|m| format!(" · auto-merge on ({})", m.label().to_lowercase())).unwrap_or_default();
            let (icon, color) = match detail.mergeable {
                Mergeability::Clean => (Icon::new(IconName::CircleCheck), theme.success),
                Mergeability::Unstable | Mergeability::Blocked | Mergeability::Behind => (Icon::new(IconName::CircleAlert), theme.warning),
                Mergeability::Conflicting => (Icon::new(IconName::CircleX), theme.danger),
                Mergeability::Unknown | Mergeability::Draft => (Icon::new(IconName::Info), theme.muted_foreground),
            };
            (icon, color, format!("{}{review}{auto}", detail.mergeable.label()))
        }
    };
    h_flex()
        .gap_2()
        .text_sm()
        .child(icon.small().text_color(color))
        .child(text)
        .when(detail.changed_files > 0, |el| {
            el.child(div().text_color(theme.muted_foreground).child(format!(
                "· {} file{}",
                detail.changed_files,
                if detail.changed_files == 1 { "" } else { "s" }
            )))
        })
        .when(pr.additions + pr.deletions > 0, |el| {
            el.child(div().text_color(theme.success).child(format!("+{}", pr.additions)))
                .child(div().text_color(theme.danger).child(format!("−{}", pr.deletions)))
        })
}

fn section(title: impl Into<SharedString>, trailing: Option<String>, cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .gap_2()
        .pb_2()
        .border_b_1()
        .border_color(theme.border)
        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(title.into()))
        .when_some(trailing, |el, t| el.child(div().text_xs().text_color(theme.muted_foreground).child(t)))
}

fn render_checks(detail: &PullRequestDetail, cx: &App) -> Div {
    let theme = cx.theme();
    let s = detail.pr.checks;
    let mut parts = Vec::new();
    if s.failed > 0 {
        parts.push(format!("{} failing", s.failed));
    }
    if s.pending > 0 {
        parts.push(format!("{} running", s.pending));
    }
    if s.passed > 0 {
        parts.push(format!("{} passed", s.passed));
    }
    if s.other > 0 {
        parts.push(format!("{} skipped", s.other));
    }
    let now = now_ms();
    let rows = detail.checks.iter().enumerate().map(|(i, c)| {
        let duration = match (c.started_ms, c.finished_ms) {
            (0, _) => String::new(),
            (start, 0) if c.state.is_pending() => format::duration(now.saturating_sub(start)),
            (start, end) if end >= start => format::duration(end - start),
            _ => String::new(),
        };
        let url = c.url.clone();
        h_flex()
            .id(("forge-check", i))
            .h_8()
            .px_2()
            .gap_2()
            .rounded(theme.radius)
            .cursor_default()
            .when(!url.is_empty(), |el| {
                el.hover(|s| s.bg(theme.list_hover)).on_click(move |_, _, cx| ForgeView::open_url(&url, cx))
            })
            .child(glyph::check(("forge-check-glyph", i), c.state, cx))
            .child(div().flex_1().min_w_0().truncate().text_sm().child(c.name.clone()))
            .child(div().max_w(rems(14.)).truncate().text_xs().text_color(theme.muted_foreground).child(c.group.clone()))
            .child(div().w(rems(4.5)).flex_none().text_right().text_xs().text_color(theme.muted_foreground).child(duration))
            .child(
                h_flex()
                    .w_4()
                    .flex_none()
                    .justify_center()
                    .when(!c.url.is_empty(), |el| el.child(Icon::new(IconName::ExternalLink).xsmall().text_color(theme.muted_foreground))),
            )
    });
    v_flex()
        .gap_1()
        .child(section("Checks", (!parts.is_empty()).then(|| parts.join(" · ")), cx))
        .when(detail.checks.is_empty(), |el| {
            el.child(div().py_2().text_sm().text_color(theme.muted_foreground).child("No checks ran on this commit."))
        })
        .children(rows)
}

fn render_conversation(detail: &PullRequestDetail, cx: &App) -> Div {
    let theme = cx.theme();
    let number = detail.pr.number;
    // Reviews and comments in one timeline, oldest first.
    enum Entry<'a> {
        Review(&'a Review),
        Comment(&'a Comment),
    }
    let mut entries: Vec<(u64, Entry)> = detail
        .reviews
        .iter()
        .filter(|r| !r.body.trim().is_empty() || r.state != ReviewState::Commented)
        .map(|r| (r.submitted_ms, Entry::Review(r)))
        .chain(detail.comments.iter().map(|c| (c.created_ms, Entry::Comment(c))))
        .collect();
    entries.sort_by_key(|(at, _)| *at);
    let now = now_ms();
    let body = if detail.body.trim().is_empty() {
        div().text_sm().text_color(theme.muted_foreground).child("No description.").into_any_element()
    } else {
        TextView::markdown(SharedString::from(format!("forge-pr-body-{number}")), detail.body.clone()).selectable(true).into_any_element()
    };
    let items = entries.into_iter().enumerate().map(|(i, (at, entry))| {
        let (author, verdict, body, location) = match entry {
            Entry::Review(r) => {
                let verdict = match r.state {
                    ReviewState::Approved => Some(("approved", theme.success)),
                    ReviewState::ChangesRequested => Some(("requested changes", theme.warning)),
                    ReviewState::Dismissed => Some(("review dismissed", theme.muted_foreground)),
                    _ => Some(("reviewed", theme.muted_foreground)),
                };
                (r.author.clone(), verdict, r.body.clone(), None)
            }
            Entry::Comment(c) => {
                let location = c.path.as_ref().map(|p| match c.line {
                    Some(line) => format!("{p}:{line}"),
                    None => p.clone(),
                });
                (c.author.clone(), None, c.body.clone(), location)
            }
        };
        v_flex()
            .gap_1p5()
            .child(
                h_flex()
                    .gap_1p5()
                    .text_sm()
                    .child(div().font_weight(FontWeight::MEDIUM).child(author))
                    .when_some(verdict, |el, (text, color)| el.child(div().text_color(color).child(text)))
                    .when_some(location, |el, loc| {
                        el.child(div().text_xs().font_family(theme.mono_font_family.clone()).text_color(theme.muted_foreground).child(loc))
                    })
                    .child(div().text_xs().text_color(theme.muted_foreground).child(format::relative_time(at, now))),
            )
            .when(!body.trim().is_empty(), |el| {
                el.child(TextView::markdown(SharedString::from(format!("forge-pr-{number}-note-{i}")), body).selectable(true))
            })
    });
    v_flex()
        .gap_4()
        .child(v_flex().gap_2().child(section("Description", None, cx)).child(body))
        .child(
            v_flex()
                .gap_4()
                .child(section("Conversation", Some(format!("{}", detail.reviews.len() + detail.comments.len())), cx))
                .when(detail.reviews.is_empty() && detail.comments.is_empty(), |el| {
                    el.child(div().text_sm().text_color(theme.muted_foreground).child("No reviews or comments yet."))
                })
                .children(items),
        )
}
