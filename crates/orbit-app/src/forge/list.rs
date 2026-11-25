//! The pull request list and the master–detail split around it.

use std::ops::Range;

use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::{ForgeView, glyph};
use crate::assets::Lucide;
use crate::{format, ui};

/// Rows of placeholder text while a list loads for the first time.
pub fn skeleton(cx: &App) -> Div {
    let theme = cx.theme();
    v_flex().p_3().gap_3().children((0..6).map(|i| {
        h_flex()
            .gap_2()
            .child(Skeleton::new().size_4().rounded_full())
            .child(
                v_flex()
                    .flex_1()
                    .gap_1p5()
                    .child(Skeleton::new().h_3p5().w(relative(if i % 2 == 0 { 0.8 } else { 0.6 })))
                    .child(Skeleton::new().h_3().w(relative(0.4)).bg(theme.muted)),
            )
    }))
}

/// The master–detail layout shared by both pages.
pub fn split(view: &ForgeView, id: &'static str, list: impl IntoElement, detail: impl IntoElement, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div().flex_1().min_h_0().child(
        h_resizable(id)
            .with_state(&view.split)
            .child(
                resizable_panel()
                    .size(px(380.))
                    .size_range(px(280.)..px(620.))
                    .child(v_flex().size_full().border_r_1().border_color(theme.border).child(list)),
            )
            .child(resizable_panel().child(div().size_full().min_w_0().child(detail))),
    )
}

pub fn render_pull_requests(view: &mut ForgeView, window: &mut Window, cx: &mut Context<ForgeView>) -> impl IntoElement {
    let filter_ix = PrFilter::ALL.iter().position(|f| *f == view.filter).unwrap_or(0);
    let noun = view.forge_kind().review_noun();
    let filters = h_flex().flex_none().px_3().py_2().child(
        TabBar::new("forge-filter")
            .segmented()
            .xsmall()
            .selected_index(filter_ix)
            .on_click(cx.listener(|this, ix: &usize, _, cx| this.set_filter(PrFilter::ALL[*ix], cx)))
            .children(PrFilter::ALL.iter().map(|f| Tab::new().label(f.label()))),
    );
    let body = match (&view.prs.value, &view.prs.error) {
        (None, Some(error)) => {
            ui::empty_state(Icon::new(IconName::CircleAlert), format!("Couldn’t load {noun}s"), error.clone(), None, cx).into_any_element()
        }
        (None, None) => skeleton(cx).into_any_element(),
        (Some(list), _) if list.items.is_empty() => {
            let what = match view.filter {
                PrFilter::Open => format!("No open {noun}s"),
                PrFilter::Mine => format!("No open {noun}s involve you"),
                PrFilter::Merged => format!("No merged {noun}s"),
                PrFilter::Closed => format!("No closed {noun}s"),
            };
            ui::empty_state(
                Icon::new(Lucide::GitPullRequest),
                what,
                "Pull requests open from a thread’s review once an agent has changes to ship. Switch the filter to see others.",
                Some(crate::onboarding::teach::action("forge-new-thread", Lucide::SquarePen, "New thread", Box::new(crate::actions::NewThread), cx)),
                cx,
            )
            .into_any_element()
        }
        (Some(list), _) => uniform_list("forge-pr-rows", list.items.len(), cx.processor(ForgeView::render_pr_rows))
            .track_scroll(&view.scroll)
            .flex_1()
            .into_any_element(),
    };
    let list = v_flex().size_full().child(filters).child(body);
    let detail = super::detail::render(view, window, cx);
    split(view, "forge-prs", list, detail, cx)
}

impl ForgeView {
    fn render_pr_rows(&mut self, range: Range<usize>, _: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme().clone();
        let now = now_ms();
        let sigil = self.forge_kind().review_sigil();
        let Some(list) = &self.prs.value else { return Vec::new() };
        list.items[range.start.min(list.items.len())..range.end.min(list.items.len())]
            .iter()
            .map(|pr| {
                let number = pr.number;
                let selected = self.selected_pr == Some(number);
                let meta = format!("{sigil}{} · {} · {}", pr.number, pr.author, format::relative_time(pr.updated_ms, now));
                div()
                    .px_1p5()
                    .py_0p5()
                    .child(
                        h_flex()
                            .id(("forge-pr-row", number))
                            .w_full()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .items_start()
                            .rounded(theme.radius)
                            .cursor_default()
                            .when(selected, |el| el.bg(theme.list_active))
                            .when(!selected, |el| el.hover(|s| s.bg(theme.list_hover)))
                            .on_click(cx.listener(move |this, _, _, cx| this.select_pr(number, cx)))
                            .child(div().pt_0p5().child(glyph::pull_request(("forge-pr-glyph", number), pr, cx)))
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
                                            .child(pr.title.clone()),
                                    )
                                    .child(div().truncate().text_xs().text_color(theme.muted_foreground).child(meta)),
                            )
                            .child(review_mark(pr.review, &theme)),
                    )
                    .into_any_element()
            })
            .collect()
    }
}

/// A trailing mark for a decided review; nothing while it is pending.
fn review_mark(review: ReviewDecision, theme: &gpui_kit::component::Theme) -> Div {
    let slot = h_flex().flex_none().w_4().pt_0p5().justify_center();
    match review {
        ReviewDecision::Approved => slot.child(Icon::new(IconName::Check).xsmall().text_color(theme.success)),
        ReviewDecision::ChangesRequested => slot.child(Icon::new(IconName::CircleAlert).xsmall().text_color(theme.warning)),
        _ => slot,
    }
}
