//! The constellation: every live agent as a dot.
//!
//! * Color is the state, from the status vocabulary (working has no hue).
//! * Size is the work's size: changed lines when known, else tokens, on a
//!   log scale.
//! * Lines join an architect to its workers; a dashed red line joins two
//!   agents whose work touches the same file (the review engine's
//!   cross-worktree conflicts and the teams' own overlaps).
//! * Dots cluster by project.
//!
//! The mini map is docked at the bottom of the sidebar and becomes a single
//! line ("All quiet · 12 settled") when nothing is live; the galaxy fills
//! the window, labels every dot, and walks them with the arrow keys. Nothing
//! animates: the map redraws only when a thread, team or race changes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::f32::consts::PI;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::state::ClientState;
use orbit_proto::*;

use crate::app_state::{AppEvent, AppState};
use crate::assets::Lucide;
use crate::shell::{Route, Workspace};
use crate::thread::display_title;
use crate::ui::Glyph;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstellationMode {
    /// 220 × 64, docked in the sidebar.
    Mini,
    /// The full view.
    Galaxy,
}

/// One dot.
#[derive(Clone, Debug, PartialEq)]
pub struct Star {
    pub thread: ThreadId,
    pub project: Option<ProjectId>,
    /// The architect it works for, when it is a team worker.
    pub lead: Option<ThreadId>,
    pub glyph: Glyph,
    /// Changed lines, or tokens when no diff is known.
    pub weight: u64,
}

/// A line between two dots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Crew(ThreadId, ThreadId),
    /// Both edit the same file.
    Clash(ThreadId, ThreadId),
}

/// The live agents and their links, from the client state.
pub fn stars(state: &ClientState) -> (Vec<Star>, Vec<Link>) {
    let mut lead_of: HashMap<ThreadId, ThreadId> = HashMap::new();
    let mut active_team: HashSet<ThreadId> = HashSet::new();
    for team in state.teams.values() {
        let live = team.workers.iter().any(|w| !w.state.is_finished());
        if live {
            active_team.insert(team.architect);
        }
        for w in &team.workers {
            if let Some(t) = w.thread {
                // A sub-worker hangs off the worker that spawned it.
                lead_of.insert(t, w.parent);
                if !w.state.is_finished() {
                    active_team.insert(t);
                }
            }
        }
    }
    let racing: HashSet<ThreadId> = state
        .races
        .values()
        .filter(|r| r.state == RaceState::Running)
        .flat_map(|r| r.candidates.iter().filter(|c| c.finished.is_none()).map(|c| c.thread))
        .collect();
    let race_files: HashMap<ThreadId, u64> = state
        .races
        .values()
        .flat_map(|r| r.candidates.iter().map(|c| (c.thread, (c.additions() + c.deletions()) as u64)))
        .collect();
    let mut out: Vec<Star> = state
        .threads
        .values()
        .filter(|t| !t.archived && t.settled_ms.is_none())
        .filter(|t| {
            t.status.is_busy()
                || !t.pending.is_empty()
                || matches!(t.status, ThreadStatus::Failed { .. })
                || active_team.contains(&t.id)
                || racing.contains(&t.id)
        })
        .map(|t| {
            let lines: u64 = state.workspace.get(&t.id).map(|f| f.iter().map(|c| (c.additions + c.deletions) as u64).sum()).unwrap_or(0);
            let usage = state.usage(t.id);
            let weight = if lines > 0 {
                lines
            } else if let Some(n) = race_files.get(&t.id).filter(|n| **n > 0) {
                *n
            } else {
                (usage.input_tokens + usage.output_tokens) / 200
            };
            Star { thread: t.id, project: t.project, lead: lead_of.get(&t.id).copied(), glyph: Glyph::live(t, state), weight }
        })
        .collect();
    out.sort_by_key(|s| s.thread);
    let present: HashSet<ThreadId> = out.iter().map(|s| s.thread).collect();
    let mut links: Vec<Link> = out
        .iter()
        .filter_map(|s| s.lead.filter(|l| present.contains(l)).map(|l| Link::Crew(l, s.thread)))
        .collect();
    let mut clashes: HashSet<(ThreadId, ThreadId)> = HashSet::new();
    let overlaps = state.conflicts.iter().map(|c| &c.threads).chain(state.teams.values().flat_map(|t| t.overlaps.iter().map(|o| &o.threads)));
    for threads in overlaps {
        for (i, a) in threads.iter().enumerate() {
            for b in &threads[i + 1..] {
                if present.contains(a) && present.contains(b) {
                    clashes.insert(((*a).min(*b), (*a).max(*b)));
                }
            }
        }
    }
    let mut clashes: Vec<_> = clashes.into_iter().collect();
    clashes.sort();
    links.extend(clashes.into_iter().map(|(a, b)| Link::Clash(a, b)));
    (out, links)
}

/// Where each star sits, in `0..1` of the map's width and height. Projects
/// get equal columns. In each, leads (architects first, by crew size) sit on
/// a golden-angle spiral stretched to the column, and their workers on a
/// ring around them; sub-workers ring their own lead. `aspect` is width over
/// height, so rings stay round.
pub fn layout(stars: &[Star], aspect: f32) -> HashMap<ThreadId, (f32, f32)> {
    let mut clusters: BTreeMap<Option<ProjectId>, Vec<&Star>> = BTreeMap::new();
    for s in stars {
        clusters.entry(s.project).or_default().push(s);
    }
    let n = clusters.len().max(1) as f32;
    let mut out = HashMap::new();
    for (ci, (_, members)) in clusters.into_iter().enumerate() {
        let cx = (ci as f32 + 0.5) / n;
        // The column's half extents, in height units.
        let (rx, ry) = (0.5 * aspect / n * 0.84, 0.38);
        let in_cluster: HashSet<ThreadId> = members.iter().map(|s| s.thread).collect();
        let crew = |t: ThreadId| members.iter().filter(|s| s.lead == Some(t)).count();
        let mut roots: Vec<&Star> =
            members.iter().copied().filter(|s| s.lead.is_none_or(|l| !in_cluster.contains(&l))).collect();
        roots.sort_by_key(|s| (std::cmp::Reverse(crew(s.thread)), s.thread));
        let k = roots.len().max(1) as f32;
        let to_unit = |dx: f32, dy: f32| ((cx + dx / aspect).clamp(0.03, 0.97), (0.5 + dy).clamp(0.08, 0.92));
        // Ring radius: room for the crew without crowding the next lead.
        let ring = (rx.min(ry) * 1.4 / k.sqrt()).min(ry * 0.8);
        let place = |thread: ThreadId, dx: f32, dy: f32, out: &mut HashMap<ThreadId, (f32, f32)>| {
            out.insert(thread, to_unit(dx, dy));
        };
        for (i, root) in roots.iter().enumerate() {
            let (dx, dy) = if roots.len() == 1 {
                (0.0, 0.0)
            } else {
                let r = ((i as f32 + 0.5) / k).sqrt();
                let a = i as f32 * 2.399_963;
                (rx * r * a.cos(), ry * r * a.sin())
            };
            place(root.thread, dx, dy, &mut out);
            // Workers ring their lead; their own workers ring them, smaller.
            let mut stack = vec![(root.thread, dx, dy, ring, -PI / 2.0)];
            while let Some((lead, lx, ly, radius, start)) = stack.pop() {
                let children: Vec<&Star> = members.iter().copied().filter(|s| s.lead == Some(lead) && s.thread != lead).collect();
                let m = children.len().max(1) as f32;
                for (j, child) in children.iter().enumerate() {
                    if out.contains_key(&child.thread) {
                        continue;
                    }
                    let a = start + 2.0 * PI * j as f32 / m;
                    let (x, y) = (lx + radius * a.cos(), ly + radius * a.sin());
                    place(child.thread, x, y, &mut out);
                    stack.push((child.thread, x, y, radius * 0.45, a));
                }
            }
        }
    }
    out
}

/// A point in `0..1` of the map's width and height.
type Spot = (f32, f32);

/// A dot's radius for `weight`, on a log scale.
fn radius(weight: u64, mode: ConstellationMode) -> f32 {
    let (min, span) = match mode {
        ConstellationMode::Mini => (2.5, 2.5),
        ConstellationMode::Galaxy => (5.0, 7.0),
    };
    min + span * ((weight as f32 + 1.0).log10() / 3.0).min(1.0)
}

fn star_color(glyph: Glyph, cx: &App) -> Hsla {
    let theme = cx.theme();
    match glyph {
        Glyph::Approval | Glyph::Question | Glyph::RateLimited => theme.warning,
        Glyph::Failed => theme.danger,
        Glyph::Unread => theme.primary,
        Glyph::Working | Glyph::Quiet | Glyph::Unverified => theme.foreground,
        Glyph::Settled | Glyph::Idle => theme.muted_foreground,
    }
}

pub struct Constellation {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    mode: ConstellationMode,
    focus: FocusHandle,
    selected: Option<ThreadId>,
    _subscriptions: Vec<Subscription>,
}

impl Constellation {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, mode: ConstellationMode, cx: &mut Context<Self>) -> Self {
        let subs = vec![
            cx.observe(&app, |_, _, cx| cx.notify()),
            cx.subscribe(&app, |_, _, event: &AppEvent, cx| {
                if matches!(event, AppEvent::Teams | AppEvent::Workspace(_)) {
                    cx.notify();
                }
            }),
        ];
        Self { app, workspace, mode, focus: cx.focus_handle(), selected: None, _subscriptions: subs }
    }

    fn open(&self, thread: ThreadId, beside: bool, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.workspace.update(cx, |ws, cx| {
            if beside {
                ws.open_beside(thread, window, cx);
            } else {
                ws.navigate(Route::Thread(thread), window, cx);
            }
        });
    }

    fn open_galaxy(&self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.workspace.update(cx, |ws, cx| {
            ws.teams_page().update(cx, |page, cx| page.show(super::TeamsPageKind::Galaxy, cx));
            ws.navigate(Route::Teams, window, cx);
        });
    }

    /// Arrow keys walk to the nearest star that way; Enter opens it.
    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.app.read(cx).state();
        let (stars, _) = stars(state);
        if stars.is_empty() {
            return;
        }
        let places = layout(&stars, 2.0);
        let key = event.keystroke.key.as_str();
        if key == "enter" {
            if let Some(t) = self.selected {
                self.open(t, false, window, cx);
            }
            return;
        }
        let dir = match key {
            "left" => (-1.0, 0.0),
            "right" => (1.0, 0.0),
            "up" => (0.0, -1.0),
            "down" => (0.0, 1.0),
            _ => return,
        };
        let from = self.selected.and_then(|t| places.get(&t).copied());
        let next = match from {
            None => stars.first().map(|s| s.thread),
            Some((x, y)) => places
                .iter()
                .filter(|(t, _)| Some(**t) != self.selected)
                .filter(|(_, (px, py))| (px - x) * dir.0 + (py - y) * dir.1 > 0.001)
                .min_by(|a, b| {
                    let d = |(px, py): (f32, f32)| {
                        let along = (px - x) * dir.0 + (py - y) * dir.1;
                        let across = ((px - x) * dir.1 - (py - y) * dir.0).abs();
                        along + 2.0 * across
                    };
                    d(*a.1).total_cmp(&d(*b.1))
                })
                .map(|(t, _)| *t),
        };
        if let Some(next) = next {
            self.selected = Some(next);
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn render_map(&self, stars: &[Star], links: &[Link], aspect: f32, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mode = self.mode;
        let places = layout(stars, aspect);
        let state = self.app.read(cx).state();
        // Painted: links under dots. Colors are resolved now, from the theme.
        let paint_links: Vec<(Spot, Spot, bool)> = links
            .iter()
            .filter_map(|l| {
                let (a, b, clash) = match *l {
                    Link::Crew(a, b) => (a, b, false),
                    Link::Clash(a, b) => (a, b, true),
                };
                Some((*places.get(&a)?, *places.get(&b)?, clash))
            })
            .collect();
        let leads: HashSet<ThreadId> = state.teams.values().map(|t| t.architect).collect();
        // Where, radius, color, working, architect.
        let paint_stars: Vec<(Spot, f32, Hsla, bool, bool)> = stars
            .iter()
            .filter_map(|s| {
                let p = *places.get(&s.thread)?;
                let working = matches!(s.glyph, Glyph::Working);
                Some((p, radius(s.weight, mode), star_color(s.glyph, cx), working, leads.contains(&s.thread)))
            })
            .collect();
        let (crew_color, clash_color, halo) = (theme.muted_foreground.opacity(0.45), theme.danger, theme.foreground.opacity(0.07));
        let lead_ring = theme.muted_foreground.opacity(0.7);
        let selected = self.selected.and_then(|t| places.get(&t).copied().map(|p| (p, stars.iter().find(|s| s.thread == t).map(|s| radius(s.weight, mode)).unwrap_or(4.0))));
        let ring = theme.ring;
        let map = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let at = |(x, y): (f32, f32)| point(bounds.origin.x + bounds.size.width * x, bounds.origin.y + bounds.size.height * y);
                for (a, b, clash) in &paint_links {
                    let mut path = PathBuilder::stroke(px(if *clash { 1.25 } else { 1.0 }));
                    if *clash {
                        path = path.dash_array(&[px(3.), px(2.)]);
                    }
                    let (from, to) = (at(*a), at(*b));
                    path.move_to(from);
                    if *clash {
                        // Bowed, so a clash never hides behind a crew line.
                        let (dx, dy) = (to.x - from.x, to.y - from.y);
                        let mid = point(from.x + dx * 0.5 - dy * 0.22, from.y + dy * 0.5 + dx * 0.22);
                        path.curve_to(to, mid);
                    } else {
                        path.line_to(to);
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, if *clash { clash_color } else { crew_color });
                    }
                }
                for (p, r, color, working, lead) in &paint_stars {
                    let c = at(*p);
                    if *lead {
                        // Architects wear a ring: the hub of their crew.
                        let o = r + 2.5;
                        window.paint_quad(quad(
                            Bounds::new(point(c.x - px(o), c.y - px(o)), size(px(2. * o), px(2. * o))),
                            px(o),
                            transparent_black(),
                            px(1.),
                            lead_ring,
                            BorderStyle::default(),
                        ));
                    }
                    if *working {
                        // A soft halo: work in progress, without motion.
                        let h = r * 2.2;
                        window.paint_quad(fill(Bounds::new(point(c.x - px(h), c.y - px(h)), size(px(2. * h), px(2. * h))), halo).corner_radii(px(h)));
                    }
                    window.paint_quad(fill(Bounds::new(point(c.x - px(*r), c.y - px(*r)), size(px(2. * r), px(2. * r))), *color).corner_radii(px(*r)));
                }
                if let Some((p, r)) = selected {
                    let c = at(p);
                    let r = r + 3.0;
                    window.paint_quad(
                        quad(
                            Bounds::new(point(c.x - px(r), c.y - px(r)), size(px(2. * r), px(2. * r))),
                            px(r),
                            transparent_black(),
                            px(1.5),
                            ring,
                            BorderStyle::default(),
                        ),
                    );
                }
            },
        )
        .size_full();
        // Hit targets (and labels in the galaxy) over the painted dots.
        let now = now_ms();
        let targets = stars.iter().filter_map(|s| {
            let (x, y) = *places.get(&s.thread)?;
            let thread = state.threads.get(&s.thread)?;
            let title = display_title(thread).to_owned();
            let role = match (s.lead, state.teams.values().any(|t| t.architect == s.thread)) {
                (_, true) => " · architect",
                (Some(_), _) => " · worker",
                _ => "",
            };
            let peek: SharedString = format!(
                "{title}\n{}{role} · {}",
                s.glyph.label(),
                crate::format::relative_time(thread.updated_ms, now)
            )
            .into();
            let id = s.thread;
            let hit = match mode {
                ConstellationMode::Mini => rems(0.875),
                ConstellationMode::Galaxy => rems(1.75),
            };
            Some(
                v_flex()
                    .id(("star", id.0))
                    .absolute()
                    .left(relative(x))
                    .top(relative(y))
                    .items_center()
                    .child(
                        div()
                            .id(("star-hit", id.0))
                            .size(hit)
                            .mt(-hit / 2.)
                            .ml(-hit / 2.)
                            .rounded_full()
                            .cursor_default()
                            .tooltip(move |window, cx| Tooltip::new(peek.clone()).build(window, cx))
                            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                                this.selected = Some(id);
                                let beside = event.modifiers().secondary(cx);
                                this.open(id, beside, window, cx);
                            })),
                    )
                    .when(mode == ConstellationMode::Galaxy, |el| {
                        el.child(
                            h_flex().mt_2().ml(-rems(5.)).w(rems(10.)).justify_center().child(
                                div()
                                    .max_w_full()
                                    .px_1()
                                    .rounded(theme.radius)
                                    .bg(theme.background.opacity(0.85))
                                    .truncate()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(title),
                            ),
                        )
                    }),
            )
        });
        div().relative().size_full().child(map).children(targets).into_any_element()
    }
}

impl Render for Constellation {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let (stars, links) = stars(state);
        let settled = state.threads.values().filter(|t| !t.archived && t.settled_ms.is_some()).count();
        match self.mode {
            ConstellationMode::Mini => {
                if stars.is_empty() {
                    return h_flex()
                        .id("constellation-quiet")
                        .h_7()
                        .px_2()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .cursor_default()
                        .on_click(cx.listener(|this, _, window, cx| this.open_galaxy(window, cx)))
                        .child(Icon::new(Lucide::Orbit).xsmall())
                        .child(format!("All quiet · {settled} settled"))
                        .into_any_element();
                }
                let clashes = links.iter().filter(|l| matches!(l, Link::Clash(..))).count();
                v_flex()
                    .id("constellation")
                    .gap_1()
                    .child(
                        h_flex()
                            .px_2()
                            .gap_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(Lucide::Orbit).xsmall())
                            .child(div().flex_1().child(format!("{} live", stars.len())))
                            .when(clashes > 0, |el| {
                                el.child(div().text_color(theme.danger).child(if clashes == 1 { "1 clash".to_owned() } else { format!("{clashes} clashes") }))
                            })
                            .child(
                                div()
                                    .id("constellation-expand")
                                    .cursor_default()
                                    .hover(|s| s.text_color(theme.foreground))
                                    .tooltip(|window, cx| Tooltip::new("Show the constellation").build(window, cx))
                                    .on_click(cx.listener(|this, _, window, cx| this.open_galaxy(window, cx)))
                                    .child(Icon::new(Lucide::Maximize2).xsmall()),
                            ),
                    )
                    .child(
                        div()
                            .h(rems(4.))
                            .w_full()
                            .rounded(theme.radius)
                            .bg(theme.muted.opacity(0.5))
                            .child(self.render_map(&stars, &links, 220. / 64., cx)),
                    )
                    .into_any_element()
            }
            ConstellationMode::Galaxy => {
                if stars.is_empty() {
                    return crate::ui::empty_state(
                        Icon::new(Lucide::Orbit),
                        format!("All quiet · {settled} settled"),
                        "Live agents show here as stars: color for state, size for the work, lines for teams and clashes.",
                        None,
                        cx,
                    )
                    .into_any_element();
                }
                let viewport = window.viewport_size();
                let aspect = (viewport.width / viewport.height).clamp(0.8, 3.0);
                let legend = |color: Hsla, label: &'static str| {
                    h_flex().gap_1p5().child(div().size_2().rounded_full().bg(color)).child(label)
                };
                v_flex()
                    .id("galaxy")
                    .key_context("Constellation")
                    .track_focus(&self.focus)
                    .on_key_down(cx.listener(Self::on_key))
                    .flex_1()
                    .min_h_0()
                    .child(div().flex_1().min_h_0().m_6().child(self.render_map(&stars, &links, aspect, cx)))
                    .child(
                        h_flex()
                            .flex_none()
                            .px_6()
                            .pb_4()
                            .gap_4()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(legend(theme.foreground, "Working"))
                            .child(legend(theme.warning, "Needs you"))
                            .child(legend(theme.danger, "Failed"))
                            .child(legend(theme.primary, "New result"))
                            .child(h_flex().gap_1p5().child(div().w_4().h_0p5().rounded_full().bg(theme.muted_foreground)).child("Team"))
                            .child(h_flex().gap_1p5().child(div().w_4().h_0p5().rounded_full().bg(theme.danger)).child("Same file"))
                            .child(div().flex_1())
                            .child("Arrow keys move · Enter opens · Ctrl-click opens beside"),
                    )
                    .into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConstellationMode, Star, layout, radius};
    use crate::ui::Glyph;
    use orbit_proto::*;

    fn star(id: u64, project: u64, lead: Option<u64>) -> Star {
        Star { thread: ThreadId(id), project: Some(ProjectId(project)), lead: lead.map(ThreadId), glyph: Glyph::Working, weight: 10 }
    }

    #[test]
    fn projects_cluster_and_workers_ring_their_architect() {
        let stars = vec![star(1, 1, None), star(2, 1, Some(1)), star(3, 1, Some(1)), star(4, 2, None)];
        let at = layout(&stars, 3.0);
        assert_eq!(at.len(), 4);
        let (a, w1, w2, other) = (at[&ThreadId(1)], at[&ThreadId(2)], at[&ThreadId(3)], at[&ThreadId(4)]);
        assert!(a.0 < 0.5 && other.0 > 0.5, "two projects, two columns: {a:?} {other:?}");
        let d = |p: (f32, f32)| (((p.0 - a.0) * 3.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
        assert!((d(w1) - d(w2)).abs() < 1e-4, "workers on one ring");
        for (x, y) in at.values() {
            assert!((0.0..=1.0).contains(x) && (0.0..=1.0).contains(y));
        }
    }

    #[test]
    fn bigger_work_draws_bigger_dots() {
        assert!(radius(0, ConstellationMode::Mini) < radius(100, ConstellationMode::Mini));
        assert_eq!(radius(1_000_000, ConstellationMode::Mini), radius(10_000_000, ConstellationMode::Mini));
    }
}
