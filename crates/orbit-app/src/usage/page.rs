//! The Usage page: what each account has left, then what was spent over a
//! range (tokens and cost per day, and by model, account, project and
//! thread), with CSV export.
//!
//! Stats are read from the engine's usage ledger when the page is shown and
//! when the range changes; nothing polls while it is open.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::chart::BarChart;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::rings::{self, OpenThread};
use super::RefreshUsage;
use crate::actions::OpenSettings;
use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::format;
use crate::shell::{Route, Workspace};
use crate::ui;

/// The ranges the page offers, in days (0: everything).
const RANGES: [(&str, u64); 5] = [("Today", 1), ("7 days", 7), ("30 days", 30), ("90 days", 90), ("All time", 0)];
const DAY_MS: u64 = 86_400_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Breakdown {
    Model,
    Account,
    Project,
    Thread,
}

impl Breakdown {
    const ALL: [Breakdown; 4] = [Breakdown::Model, Breakdown::Account, Breakdown::Project, Breakdown::Thread];

    fn label(self) -> &'static str {
        match self {
            Breakdown::Model => "Models",
            Breakdown::Account => "Accounts",
            Breakdown::Project => "Projects",
            Breakdown::Thread => "Threads",
        }
    }
}

pub struct UsageView {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    /// Index into [`RANGES`].
    range: usize,
    breakdown: Breakdown,
    stats: Option<UsageStats>,
    loading: bool,
    error: Option<String>,
    /// Bumped per request; an older reply is dropped.
    generation: u64,
    exporting: bool,
    /// Debug builds: ORBIT_DEBUG_ROUTE=usage-spend scrolls to the stats once
    /// they are in, for screenshots.
    debug_scroll: bool,
    _subscription: Subscription,
}

impl UsageView {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        // Limits arrive as the board changes; the page redraws with them.
        let subscription = cx.observe(&app, |_, _, cx| cx.notify());
        Self {
            app,
            workspace,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            range: 1,
            breakdown: Breakdown::Model,
            stats: None,
            loading: false,
            error: None,
            generation: 0,
            exporting: false,
            debug_scroll: cfg!(debug_assertions) && std::env::var("ORBIT_DEBUG_ROUTE").is_ok_and(|r| r == "usage-spend"),
            _subscription: subscription,
        }
    }

    pub fn focus(view: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let focus = view.read(cx).focus.clone();
        focus.focus(window, cx);
    }

    /// The page came on screen: fresh stats, and fresh limits if stale.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        super::front(&self.app, cx);
        self.load(cx);
    }

    fn query(&self) -> UsageQuery {
        let days = RANGES[self.range].1;
        let from_ms = match days {
            0 => 0,
            // Today and the days before it, from local midnight.
            days => local_midnight(now_ms()).saturating_sub((days - 1) * DAY_MS),
        };
        UsageQuery { from_ms, to_ms: 0, project: None, instance: None, tz: orbit_core::usage::local_time_zone() }
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        let task = self.app.update(cx, |app, cx| app.call(Command::Usage(UsageCommand::Stats(self.query())), cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(Reply::Usage(reply)) => {
                        if let UsageReply::Stats(stats) = *reply {
                            this.stats = Some(stats);
                            this.error = None;
                        }
                    }
                    Ok(_) => this.error = Some("The engine answered something unexpected.".into()),
                    Err(message) => this.error = Some(message),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set_range(&mut self, range: usize, cx: &mut Context<Self>) {
        if self.range != range {
            self.range = range;
            self.load(cx);
        }
    }

    /// Asks where to save, then writes the range's turns as CSV there.
    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.exporting {
            return;
        }
        let query = self.query();
        let name = format!("orbit-usage-{}.csv", RANGES[self.range].0.to_lowercase().replace(' ', "-"));
        let dir = dirs_download().unwrap_or_else(std::env::temp_dir);
        let picked = cx.prompt_for_new_path(&dir, Some(&name));
        let app = self.app.clone();
        self.exporting = true;
        cx.spawn_in(window, async move |this, cx| {
            let path = match picked.await {
                Ok(Ok(Some(path))) => path,
                _ => {
                    let _ = this.update(cx, |this, _| this.exporting = false);
                    return;
                }
            };
            let task = cx.update(|_, cx| app.update(cx, |app, cx| app.call(Command::Usage(UsageCommand::Export(query)), cx)));
            let result = match task {
                Ok(task) => task.await,
                Err(_) => return,
            };
            let written = match result {
                Ok(Reply::Text(csv)) => {
                    let rows = csv.lines().count().saturating_sub(1);
                    let target = path.clone();
                    cx.background_executor()
                        .spawn(async move { std::fs::write(&target, csv).map(|_| rows) })
                        .await
                        .map_err(|e| format!("Couldn’t write {}: {e}", path.display()))
                }
                Ok(_) => Err("The engine answered something unexpected.".into()),
                Err(message) => Err(message),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.exporting = false;
                let note = match written {
                    Ok(rows) => Notification::success(format!("{rows} turns saved to {}.", path.display())).title("Usage exported"),
                    Err(message) => Notification::error(message).title("Couldn’t export usage"),
                };
                window.push_notification(note, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ── Rendering ────────────────────────────────────────────────────────

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        h_flex()
            .flex_none()
            .h_11()
            .px_3()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .child(Icon::new(Lucide::ChartColumn).small().text_color(theme.muted_foreground))
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Usage"))
            .child(div().flex_1())
            .child(
                TabBar::new("usage-range")
                    .segmented()
                    .xsmall()
                    .selected_index(self.range)
                    .on_click(cx.listener(|this, ix: &usize, _, cx| this.set_range(*ix, cx)))
                    .children(RANGES.iter().map(|(label, _)| Tab::new().label(*label))),
            )
            .child(
                Button::new("usage-export")
                    .small()
                    .icon(Lucide::Download)
                    .label("Export CSV…")
                    .loading(self.exporting)
                    .disabled(self.exporting)
                    .on_click(cx.listener(|this, _, window, cx| this.export(window, cx))),
            )
            .child(
                Button::new("usage-refresh")
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .tooltip_with_action("Read limits and stats again", &RefreshUsage, None)
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(RefreshUsage), cx)),
            )
    }

    fn open_thread(&self) -> OpenThread {
        let workspace = self.workspace.clone();
        Rc::new(move |thread, window, cx| {
            let _ = workspace.update(cx, |ws, cx| ws.navigate(Route::Thread(thread), window, cx));
        })
    }

    fn render_limits(&self, wide: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let now = now_ms();
        let board = self.app.read(cx).state().quota.clone();
        let policy = super::settings(cx).failover;
        let cards: Vec<AnyElement> = board
            .accounts
            .iter()
            .map(|a| {
                div()
                    .when(wide, |el| el.w(relative(0.5)).p_1p5())
                    .when(!wide, |el| el.w_full().py_1p5())
                    .child(
                        div()
                            .size_full()
                            .p_4()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.group_box)
                            .child(rings::account(a, now, Some(self.open_thread()), cx)),
                    )
                    .into_any_element()
            })
            .collect();
        let policy_line = match policy {
            FailoverPolicy::Ask => "When an account reaches its limit, its thread offers to continue on another account.",
            FailoverPolicy::Auto => "When an account reaches its limit, its threads move to another account of the same agent.",
            FailoverPolicy::Never => "When an account reaches its limit, its threads wait for the reset.",
        };
        v_flex()
            .gap_1()
            .child(section_title("Limits", cx))
            .when(cards.is_empty(), |el| {
                el.child(
                    div()
                        .py_2()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("No account with plan limits yet. Claude Code and Codex report theirs once they are installed and signed in."),
                )
            })
            .child(h_flex().flex_wrap().mx(if wide { rems(-0.375) } else { rems(0.) }).children(cards))
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(policy_line)
                    .child(
                        Button::new("usage-settings")
                            .ghost()
                            .xsmall()
                            .label("Settings…")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenSettings), cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_spend(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let header = section_title("Tokens and cost", cx);
        let Some(stats) = &self.stats else {
            let body = match &self.error {
                Some(error) => div().text_sm().text_color(theme.danger).child(error.clone()),
                None => div().text_sm().text_color(theme.muted_foreground).child("Reading the usage ledger…"),
            };
            return v_flex().gap_2().child(header).child(body).into_any_element();
        };
        let t = stats.totals;
        if t.turns == 0 {
            return v_flex()
                .gap_2()
                .child(header)
                .child(ui::empty_state(
                    Icon::new(Lucide::ChartColumn),
                    "Nothing spent in this range",
                    "Every finished turn is counted here, with its tokens and the cost the agent reports.",
                    None,
                    cx,
                ))
                .into_any_element();
        }
        let tile = |label: &'static str, value: String, detail: String| {
            v_flex()
                .flex_1()
                .min_w(rems(9.))
                .gap_0p5()
                .p_3()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .child(div().text_xs().text_color(theme.muted_foreground).child(label))
                .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(value))
                .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(detail))
        };
        let cost = match format::cost(t.cost_micros) {
            c if c.is_empty() => "—".to_owned(),
            c => c,
        };
        let cached = t.cache_read_tokens;
        let tiles = h_flex()
            .gap_3()
            .flex_wrap()
            .child(tile("Tokens", format::tokens(t.tokens()), format!("{} in · {} out", format::tokens(t.input_tokens), format::tokens(t.output_tokens))))
            .child(tile("Cost", cost, "As the agents report it".into()))
            .child(tile("Turns", t.turns.to_string(), format!("{} a turn", format::tokens(t.tokens() / t.turns.max(1) as u64))))
            .child(tile(
                "Cache reads",
                format::tokens(cached),
                match t.tokens() {
                    0 => String::new(),
                    total => format!("{}% of all tokens", cached * 100 / total),
                },
            ));
        v_flex()
            .gap_3()
            .child(header)
            .child(tiles)
            .child(self.render_chart(stats, cx))
            .child(self.render_breakdown(stats, cx))
            .into_any_element()
    }

    fn render_chart(&self, stats: &UsageStats, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let days: Vec<DayUsage> = stats.days.clone();
        let short = days.len() <= 8;
        let fill = theme.chart_2;
        let ticks = days.len().clamp(2, 8);
        v_flex()
            .gap_2()
            .p_4()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Tokens per day"))
            .child(
                div().h(rems(13.)).w_full().child(
                    BarChart::new(days)
                        .id("usage-days")
                        .band(move |d: &DayUsage| day_label(&d.date, short))
                        .value(|d: &DayUsage| d.totals.tokens() as f64)
                        .fill(move |_, _, _, _| fill)
                        .corner_radii(Corners::all(px(2.)))
                        .value_axis(true)
                        .value_tick_count(4)
                        .value_tick_format(|v| format::tokens(v.max(0.0) as u64))
                        .band_tick_count(ticks)
                        .tooltip_title(|d: &DayUsage| SharedString::from(day_title(&d.date)))
                        .tooltip_value(|d: &DayUsage, _| {
                            let t = d.totals;
                            let cost = format::cost(t.cost_micros);
                            let turns = if t.turns == 1 { "1 turn".to_owned() } else { format!("{} turns", t.turns) };
                            SharedString::from(match cost.is_empty() {
                                true => format!("{} tokens · {turns}", format::tokens(t.tokens())),
                                false => format!("{} tokens · {cost} · {turns}", format::tokens(t.tokens())),
                            })
                        }),
                ),
            )
            .into_any_element()
    }

    fn render_breakdown(&self, stats: &UsageStats, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let groups: &[UsageGroup] = match self.breakdown {
            Breakdown::Model => &stats.models,
            Breakdown::Account => &stats.accounts,
            Breakdown::Project => &stats.projects,
            Breakdown::Thread => &stats.threads,
        };
        let total = stats.totals.tokens().max(1);
        let selected = Breakdown::ALL.iter().position(|b| *b == self.breakdown).unwrap_or(0);
        let open = self.open_thread();
        let threads = self.breakdown == Breakdown::Thread;
        let head = |label: &'static str, width: f32| div().w(rems(width)).text_right().child(label);
        v_flex()
            .gap_2()
            .p_4()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().text_sm().font_weight(FontWeight::MEDIUM).child("Breakdown"))
                    .child(
                        TabBar::new("usage-breakdown")
                            .segmented()
                            .xsmall()
                            .selected_index(selected)
                            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                this.breakdown = Breakdown::ALL[*ix];
                                cx.notify();
                            }))
                            .children(Breakdown::ALL.iter().map(|b| Tab::new().label(b.label()))),
                    ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .pb_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
                    .child(div().flex_1().child(self.breakdown.label().trim_end_matches('s').to_owned()))
                    .child(div().w(rems(8.)).child("Share"))
                    .child(head("Tokens", 4.5))
                    .child(head("Cost", 4.))
                    .child(head("Turns", 3.5)),
            )
            .children(groups.iter().map(|g| {
                let share = g.totals.tokens() as f32 / total as f32;
                let id: Option<ThreadId> = if threads { g.key.parse().ok() } else { None };
                let open = open.clone();
                h_flex()
                    .id(SharedString::from(format!("usage-row-{}-{}", self.breakdown.label(), g.key)))
                    .gap_3()
                    .py_1()
                    .px_1()
                    .mx(rems(-0.25))
                    .rounded(theme.radius)
                    .text_sm()
                    .when(id.is_some(), |el| el.hover(|s| s.bg(theme.list_hover)))
                    .when_some(id, |el, id| el.on_click(move |_, window, cx| open(id, window, cx)))
                    .child(div().flex_1().min_w_0().truncate().child(g.label.clone()))
                    .child(
                        div()
                            .w(rems(8.))
                            .child(Progress::new(SharedString::from(format!("usage-share-{}", g.key))).value(share * 100.0).color(theme.chart_2)),
                    )
                    .child(div().w(rems(4.5)).text_right().child(format::tokens(g.totals.tokens())))
                    .child(div().w(rems(4.)).text_right().text_color(theme.muted_foreground).child(match format::cost(g.totals.cost_micros) {
                        c if c.is_empty() => "—".to_owned(),
                        c => c,
                    }))
                    .child(div().w(rems(3.5)).text_right().text_color(theme.muted_foreground).child(g.totals.turns.to_string()))
            }))
            .into_any_element()
    }
}

fn section_title(text: &'static str, cx: &App) -> Div {
    div().pb_1().text_base().font_weight(FontWeight::SEMIBOLD).text_color(cx.theme().foreground).child(text)
}

/// Midnight before `ms`, local time.
fn local_midnight(ms: u64) -> u64 {
    use chrono::TimeZone as _;
    let local = chrono::Local.timestamp_millis_opt(ms as i64).single().unwrap_or_default();
    let midnight = local.date_naive().and_hms_opt(0, 0, 0).unwrap_or_default();
    chrono::Local.from_local_datetime(&midnight).earliest().map(|d| d.timestamp_millis().max(0) as u64).unwrap_or(ms)
}

/// `Mon 24` for a week, `24 Nov` for longer ranges.
fn day_label(date: &str, short: bool) -> String {
    match chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        Ok(d) if short => d.format("%a %-d").to_string(),
        Ok(d) => d.format("%-d %b").to_string(),
        Err(_) => date.to_owned(),
    }
}

/// `Monday, 24 November`.
fn day_title(date: &str) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").map(|d| d.format("%A, %-d %B").to_string()).unwrap_or_else(|_| date.to_owned())
}

fn dirs_download() -> Option<std::path::PathBuf> {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
    let downloads = std::path::Path::new(&home).join("Downloads");
    downloads.is_dir().then_some(downloads)
}

impl Focusable for UsageView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for UsageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let wide = window.viewport_size().width > px(1100.);
        if self.debug_scroll && self.stats.is_some() {
            self.debug_scroll = false;
            let scroll = self.scroll.clone();
            cx.on_next_frame(window, move |_, _, _| scroll.scroll_to_bottom());
        }
        v_flex()
            .size_full()
            .key_context("Usage")
            .track_focus(&self.focus)
            .bg(theme.background)
            .on_action(cx.listener(|this, _: &RefreshUsage, window, cx| {
                super::refresh(&this.app, None, window, cx);
                this.load(cx);
            }))
            .child(self.render_toolbar(cx))
            .child(
                div().id("usage-scroll").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.scroll).child(
                    v_flex()
                        .w_full()
                        .max_w(rems(64.))
                        .mx_auto()
                        .px_6()
                        .py_5()
                        .gap_6()
                        .child(self.render_limits(wide, cx))
                        .child(self.render_spend(cx)),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{DAY_MS, day_label, day_title, local_midnight};
    use orbit_proto::now_ms;

    #[test]
    fn days_read_short_for_a_week_and_by_date_for_longer() {
        assert_eq!(day_label("2026-11-23", true), "Mon 23");
        assert_eq!(day_label("2026-11-23", false), "23 Nov");
        assert_eq!(day_title("2026-11-23"), "Monday, 23 November");
        assert_eq!(day_label("not a date", true), "not a date");
    }

    #[test]
    fn ranges_start_at_local_midnight() {
        let now = now_ms();
        let midnight = local_midnight(now);
        assert!(midnight <= now && now - midnight < DAY_MS);
        assert_eq!(local_midnight(midnight), midnight);
    }
}
