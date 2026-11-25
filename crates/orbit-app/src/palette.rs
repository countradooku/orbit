//! The command palette (`mod-k`): threads, commands and projects by name.
//!
//! Three modes, chosen by the first character typed:
//!
//! * **Search everything** (no prefix). With an empty query: the threads
//!   that need you, then the ones you opened last, then a few commands.
//!   Typing ranks threads by how well they match, nudged by what they need
//!   from you and how recently you opened them; commands and projects
//!   follow. Archived threads are found here too.
//! * **`>` Commands only**, each with its key binding.
//! * **`#` Search messages**: full-text search across every thread's
//!   messages, run by the engine, with the matching excerpt.
//!
//! Escape on a bare mode prefix returns to search everything; Escape on an
//! empty query closes the palette.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::command::{Command as Palette, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::actions::*;
use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::shell::{Route, Workspace};
use crate::thread::display_title;
use crate::ui::{self, Attention, Glyph, StatusGlyph};

/// What confirming a row does.
#[derive(Clone)]
enum Target {
    Thread(ThreadId),
    NewThreadIn(ProjectId),
    Action(Rc<dyn Fn() -> Box<dyn Action>>),
    /// Switch the palette's mode.
    Query(&'static str),
}

struct CommandDef {
    label: &'static str,
    icon: Icon,
    keywords: &'static str,
    build: Rc<dyn Fn() -> Box<dyn Action>>,
}

fn command(label: &'static str, icon: impl Into<Icon>, keywords: &'static str, action: impl Action + Clone) -> CommandDef {
    CommandDef { label, icon: icon.into(), keywords, build: Rc::new(move || action.boxed_clone()) }
}

fn commands() -> Vec<CommandDef> {
    vec![
        command("New thread", Lucide::SquarePen, "start create", NewThread),
        command("Inbox", IconName::Inbox, "needs you deck approvals questions", GoInbox),
        command("Next thread that needs you", Lucide::BellRing, "attention waiting", NextAttention),
        command("Fleet", IconName::LayoutDashboard, "status board all threads", GoFleet),
        command("Pull requests", Lucide::GitPullRequest, "forge issues github pr", crate::forge::GoPullRequests),
        command("Automations", Lucide::CalendarClock, "schedule cron recurring triggers recipes", GoAutomations),
        command("New automation…", Lucide::CalendarClock, "schedule cron create recipe", crate::automations::NewAutomation),
        command("Usage", Lucide::ChartColumn, "limits quota rate tokens cost accounts export csv", crate::usage::GoUsage),
        command("Refresh usage limits", Lucide::Gauge, "quota rate limits accounts", crate::usage::RefreshUsage),
        command("Add project…", Lucide::FolderPlus, "folder open repository", AddProject),
        command("Open beside", Lucide::Columns2, "tile split side by side", OpenBeside),
        command("Maximize or restore the tile", Lucide::Maximize2, "tile zoom", MaximizeTile),
        command("Close the tile", Lucide::SquareX, "tile", CloseTile),
        command("Close the other tiles", Lucide::SquareX, "tile single", CloseOtherTiles),
        command("Rename thread…", Lucide::Pencil, "title", RenameThread),
        command("Settle thread", Lucide::ListChecks, "done", SettleThread),
        command("Pin or unpin thread", Lucide::Pin, "", PinThread),
        command("Archive thread", Lucide::Archive, "", ArchiveThread),
        command("Review changes", Lucide::FileDiff, "diff", ToggleReview),
        command("Terminal", Lucide::Terminal, "shell console", ToggleTerminal),
        command("Open file…", Lucide::FileSearch, "quick open find file go to", crate::files::QuickOpen),
        command("Show or hide files", Lucide::FolderTree, "explorer tree worktree memory", crate::files::ToggleFiles),
        command("Run or stop the first target", IconName::Play, "dev server test lint script", crate::project::runs::RunDefaultTarget),
        command("Project settings…", IconName::Settings2, "scripts targets worktree env ports orbit.toml", crate::thread::panels::ProjectSettings),
        command("Stop the running turn", Lucide::CircleStop, "interrupt cancel", StopTurn),
        command("Next thread", IconName::ArrowDown, "", NextThread),
        command("Previous thread", IconName::ArrowUp, "", PreviousThread),
        command("Show or hide the sidebar", IconName::PanelLeft, "toggle", ToggleSidebar),
        command("Keyboard shortcuts", Lucide::Keyboard, "keys cheat sheet help", ShowShortcuts),
        command("Edit keybindings…", Lucide::Keyboard, "keymap shortcuts json", OpenKeybindings),
        command("Reload keybindings", IconName::RefreshCw, "keymap", ReloadKeybindings),
        command("Settings…", IconName::Settings, "preferences", OpenSettings),
        command("Zoom in", IconName::Plus, "bigger", ZoomIn),
        command("Zoom out", IconName::Minus, "smaller", ZoomOut),
        command("Actual size", IconName::Frame, "zoom reset", ResetZoom),
        command("Quit Orbit", IconName::WindowClose, "exit", Quit),
    ]
}

/// How well `query` matches `text`: higher is better, `None` is no match.
/// A start of the text beats a start of a word, which beats a substring,
/// which beats letters in order.
pub(crate) fn score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let text = text.to_lowercase();
    if text.starts_with(query) {
        return Some(100);
    }
    if text.split(|c: char| !c.is_alphanumeric()).any(|word| word.starts_with(query)) {
        return Some(80);
    }
    if text.contains(query) {
        return Some(60);
    }
    // Letters in order ("pst" finds "parser streams tokens"), worth less the
    // more spread out they are.
    let mut chars = text.chars().enumerate();
    let mut first = None;
    let mut last = 0;
    for q in query.chars().filter(|c| !c.is_whitespace()) {
        let (ix, _) = chars.find(|(_, c)| *c == q)?;
        first.get_or_insert(ix);
        last = ix;
    }
    let spread = last - first.unwrap_or(0);
    Some((40 - spread as i32).max(1))
}

/// How much a thread's state lifts it in results.
fn attention_boost(thread: &Thread) -> i32 {
    match Attention::of(thread) {
        Attention::Waiting => 30,
        Attention::Failed | Attention::RateLimited => 20,
        Attention::Unread => 12,
        Attention::Working => 6,
        Attention::Idle => 0,
    }
}

type Rows = Vec<(CommandItem, Target)>;

struct Search {
    /// The `#` query the hits belong to.
    query: String,
    hits: Vec<SearchHit>,
    generation: u64,
}

pub fn open(workspace: Entity<Workspace>, initial: &'static str, window: &mut Window, cx: &mut App) {
    let state = cx.new(|cx| CommandState::new(window, cx));
    let app = workspace.read(cx).app().clone();
    let targets: Rc<RefCell<Vec<Vec<Target>>>> = Rc::default();
    let search: Rc<RefCell<Search>> = Rc::new(RefCell::new(Search { query: String::new(), hits: Vec::new(), generation: 0 }));
    let commands = Rc::new(commands());
    let first = Rc::new(std::cell::Cell::new(true));
    let weak = workspace.downgrade();
    let width = window.rem_size() * 40.;

    window.open_dialog(cx, move |dialog, _, _| {
        let (state, app, targets, search, commands, first, weak) =
            (state.clone(), app.clone(), targets.clone(), search.clone(), commands.clone(), first.clone(), weak.clone());
        dialog.close_button(false).p_0().w(width).content(move |content, window, cx| {
            if first.replace(false) {
                let focus = state.clone();
                window.defer(cx, move |window, cx| {
                    focus.update(cx, |state, cx| {
                        state.set_query(initial, window, cx);
                        state.focus(window, cx);
                    });
                });
            }
            let query = state.read(cx).query(cx).to_string();
            let groups = build(&weak, &query, &commands, &search.borrow(), cx);
            *targets.borrow_mut() = groups.iter().map(|(_, rows)| rows.iter().map(|(_, t)| t.clone()).collect()).collect();
            let placeholder = match query.chars().next() {
                Some('>') => "Run a command…",
                Some('#') => "Search the text of every message…",
                _ => "Search threads and commands · > commands · # messages",
            };
            let mut palette = Palette::new(&state).bordered(false).filterable(false).placeholder(placeholder).max_h(rems(28.));
            for (label, rows) in groups {
                palette = palette.group(CommandGroup::new().label(label).items(rows.into_iter().map(|(item, _)| item)));
            }
            let (targets, weak, query_state, search_cell, app) = (targets.clone(), weak.clone(), state.clone(), search.clone(), app.clone());
            let cancel_state = state.clone();
            let confirm_state = state.clone();
            content.child(
                palette
                    .empty(move |state, _, cx| {
                        let theme = cx.theme();
                        let query = state.query(cx);
                        let text = if query.starts_with('#') && query.trim_start_matches('#').trim().chars().count() < 2 {
                            "Type at least two characters to search messages."
                        } else if query.starts_with('#') {
                            "No message contains that."
                        } else {
                            "Nothing matches. Start with # to search inside messages."
                        };
                        div().py_6().text_sm().text_center().text_color(theme.muted_foreground).child(text)
                    })
                    .on_query(move |query, window, cx| {
                        if let Some(text) = query.strip_prefix('#') {
                            run_search(&app, text.trim(), &search_cell, &query_state, window, cx);
                        }
                        window.refresh();
                    })
                    .on_cancel(move |window, cx| {
                        let query = cancel_state.read(cx).query(cx);
                        if query == ">" || query == "#" {
                            cancel_state.update(cx, |state, cx| state.set_query("", window, cx));
                        } else {
                            window.close_dialog(cx);
                        }
                    })
                    .on_confirm(move |ix, window, cx| {
                        let Some(target) = targets.borrow().get(ix.section).and_then(|s| s.get(ix.row)).cloned() else { return };
                        if let Target::Query(prefix) = target {
                            let state = confirm_state.clone();
                            window.defer(cx, move |window, cx| state.update(cx, |s, cx| s.set_query(prefix, window, cx)));
                            return;
                        }
                        window.close_dialog(cx);
                        let weak = weak.clone();
                        window.defer(cx, move |window, cx| match target {
                            Target::Thread(id) => {
                                let _ = weak.update(cx, |ws, cx| ws.navigate(Route::Thread(id), window, cx));
                            }
                            Target::NewThreadIn(project) => {
                                window.dispatch_action(Box::new(crate::sidebar::NewThreadIn { project: project.0 }), cx)
                            }
                            Target::Action(build) => window.dispatch_action(build(), cx),
                            Target::Query(_) => {}
                        });
                    }),
            )
        })
    });
}

/// Runs a `#` search on the engine after a short pause; a newer query makes
/// older answers stale.
fn run_search(
    app: &Entity<AppState>,
    text: &str,
    cell: &Rc<RefCell<Search>>,
    state: &Entity<CommandState>,
    window: &mut Window,
    cx: &mut App,
) {
    let generation = {
        let mut search = cell.borrow_mut();
        search.generation += 1;
        search.generation
    };
    if text.chars().count() < 2 {
        let mut search = cell.borrow_mut();
        search.query = text.to_owned();
        search.hits.clear();
        state.update(cx, |s, cx| s.set_loading(false, window, cx));
        return;
    }
    state.update(cx, |s, cx| s.set_loading(true, window, cx));
    let (app, cell, state, text) = (app.clone(), cell.clone(), state.downgrade(), text.to_owned());
    window
        .spawn(cx, async move |cx| {
            // Typing fast sends one search, not one per key.
            cx.background_executor().timer(Duration::from_millis(140)).await;
            if cell.borrow().generation != generation {
                return;
            }
            let Ok(task) = cx.update(|_, cx| app.update(cx, |app, cx| app.call(Command::Search { query: text.clone(), limit: 40 }, cx)))
            else {
                return;
            };
            let result = task.await;
            let _ = cx.update(|window, cx| {
                if cell.borrow().generation != generation {
                    return;
                }
                {
                    let mut search = cell.borrow_mut();
                    search.query = text.clone();
                    search.hits = match result {
                        Ok(Reply::SearchHits(hits)) => hits,
                        _ => Vec::new(),
                    };
                }
                if let Some(state) = state.upgrade() {
                    state.update(cx, |s, cx| s.set_loading(false, window, cx));
                }
                window.refresh();
            });
        })
        .detach();
}

fn build(workspace: &WeakEntity<Workspace>, query: &str, commands: &[CommandDef], search: &Search, cx: &App) -> Vec<(SharedString, Rows)> {
    let Some(ws) = workspace.upgrade() else { return Vec::new() };
    let ws = ws.read(cx);
    let state = ws.app().read(cx).state();
    let history = ws.history();
    let mut groups: Vec<(SharedString, Rows)> = Vec::new();

    let command_rows = |query: &str, limit: usize| -> Rows {
        let mut scored: Vec<(i32, &CommandDef)> = commands
            .iter()
            .filter_map(|c| {
                let s = score(query, c.label).max(score(query, c.keywords).map(|s| s - 20));
                s.map(|s| (s, c))
            })
            .collect();
        scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, c)| {
                let action = (c.build)();
                let icon = c.icon.clone();
                let label = c.label;
                let item = CommandItem::new().label(label).child(move |_, cx| {
                    row(icon.clone().size_4().into_any_element(), label.into(), None, ui::keycaps(action.as_ref(), cx), cx)
                });
                (item, Target::Action(c.build.clone()))
            })
            .collect()
    };

    let thread_row = |thread: &Thread| -> (CommandItem, Target) {
        let project = thread.project.and_then(|p| state.projects.iter().find(|x| x.id == p)).map(|p| p.name.clone());
        let title: SharedString = display_title(thread).to_owned().into();
        let glyph = Glyph::live(thread, state);
        let id = thread.id;
        let meta: SharedString = match (thread.archived, project) {
            (true, Some(p)) => format!("{p} · archived").into(),
            (true, None) => "Archived".into(),
            (false, Some(p)) => p.into(),
            (false, None) => SharedString::default(),
        };
        let age: SharedString = crate::format::relative_time(thread.updated_ms, now_ms()).into();
        let item = CommandItem::new().label(title.clone()).child(move |_, cx| {
            let theme = cx.theme();
            let lead = if glyph == Glyph::Idle {
                Icon::new(Lucide::MessageSquare).size_4().text_color(theme.muted_foreground).into_any_element()
            } else {
                StatusGlyph::glyph(("palette-glyph", id.0), glyph).into_any_element()
            };
            let trail = div().text_xs().text_color(theme.muted_foreground).child(age.clone()).into_any_element();
            row(lead, title.clone(), Some(meta.clone()), Some(trail), cx)
        });
        (item, Target::Thread(id))
    };

    // The current thread's run targets: `Run Dev`, `Stop Dev`.
    let target_rows = |query: &str| -> Rows {
        let Route::Thread(thread) = ws.route() else { return Vec::new() };
        crate::project::runs::palette_commands(state, *thread)
            .into_iter()
            .filter(|(label, _, _)| score(query, label).is_some())
            .map(|(label, icon, action)| {
                let label: SharedString = label.into();
                let shown = label.clone();
                let item = CommandItem::new().label(label.clone()).child(move |_, cx| {
                    row(icon.clone().size_4().into_any_element(), shown.clone(), None, None, cx)
                });
                let action: Rc<dyn Fn() -> Box<dyn Action>> = Rc::new(move || action.boxed_clone());
                (item, Target::Action(action))
            })
            .collect()
    };

    match query.chars().next() {
        Some('>') => {
            let q = query[1..].trim().to_lowercase();
            let mut rows = target_rows(&q);
            rows.extend(command_rows(&q, usize::MAX));
            groups.push(("Commands".into(), rows));
        }
        Some('#') => {
            let q = query[1..].trim();
            if search.query == q && !search.hits.is_empty() {
                let mut rows = Vec::new();
                for hit in &search.hits {
                    let Some(thread) = state.threads.get(&hit.thread) else { continue };
                    let title: SharedString = display_title(thread).to_owned().into();
                    let excerpt: SharedString = hit.excerpt.replace('\n', " ").into();
                    let glyph = Glyph::live(thread, state);
                    let id = thread.id;
                    let item = CommandItem::new().label(title.clone()).child(move |_, cx| {
                        let theme = cx.theme();
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(h_flex().size_4().flex_none().justify_center().child(StatusGlyph::glyph(("search-glyph", id.0), glyph)))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .py_0p5()
                                    .child(div().truncate().text_sm().child(title.clone()))
                                    .child(div().truncate().text_xs().text_color(theme.muted_foreground).child(excerpt.clone())),
                            )
                    });
                    rows.push((item, Target::Thread(hit.thread)));
                }
                groups.push(("In messages".into(), rows));
            }
        }
        _ if query.trim().is_empty() => {
            let mut needs: Vec<&Thread> = state
                .threads
                .values()
                .filter(|t| !t.archived && t.snoozed_until.is_none() && Attention::of(t) == Attention::Waiting)
                .collect();
            needs.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
            if !needs.is_empty() {
                groups.push(("Needs you".into(), needs.iter().take(5).map(|t| thread_row(t)).collect()));
            }
            // Opened last first, then the most recently active.
            let mut recent: Vec<&Thread> = history.iter().rev().filter_map(|id| state.threads.get(id)).collect();
            let mut by_activity: Vec<&Thread> = state.threads.values().filter(|t| !t.archived).collect();
            by_activity.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
            for t in by_activity {
                if !recent.iter().any(|r| r.id == t.id) {
                    recent.push(t);
                }
            }
            let recent: Rows = recent
                .into_iter()
                .filter(|t| !t.archived && t.settled_ms.is_none() && !needs.iter().any(|n| n.id == t.id))
                .take(6)
                .map(thread_row)
                .collect();
            if !recent.is_empty() {
                groups.push(("Recent".into(), recent));
            }
            let mut rows = command_rows("", 3);
            rows.push((
                CommandItem::new().label("All commands").child(|_, cx| {
                    row(Icon::new(Lucide::Command).size_4().into_any_element(), "All commands".into(), None, Some(ui::keycap(">")), cx)
                }),
                Target::Query(">"),
            ));
            rows.push((
                CommandItem::new().label("Search messages").child(|_, cx| {
                    row(Icon::new(Lucide::SearchCode).size_4().into_any_element(), "Search messages".into(), None, Some(ui::keycap("#")), cx)
                }),
                Target::Query("#"),
            ));
            groups.push(("Commands".into(), rows));
        }
        _ => {
            let q = query.trim().to_lowercase();
            let mut threads: Vec<(i32, &Thread)> = state
                .threads
                .values()
                .filter_map(|t| {
                    let project = t.project.and_then(|p| state.projects.iter().find(|x| x.id == p));
                    let base = score(&q, display_title(t))
                        .max(project.and_then(|p| score(&q, &p.name)).map(|s| s - 25))
                        .max(t.workspace.branch.as_deref().and_then(|b| score(&q, b)).map(|s| s - 20))?;
                    let recency = history.iter().rev().position(|h| *h == t.id).map_or(0, |ix| 10 - ix.min(10) as i32);
                    let archived = if t.archived { -30 } else { 0 };
                    Some((base + attention_boost(t) + recency + archived, t))
                })
                .collect();
            threads.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.updated_ms.cmp(&a.1.updated_ms)));
            let rows: Rows = threads.into_iter().take(30).map(|(_, t)| thread_row(t)).collect();
            if !rows.is_empty() {
                groups.push(("Threads".into(), rows));
            }
            let mut commands = target_rows(&q);
            commands.extend(command_rows(&q, 6));
            if !commands.is_empty() {
                groups.push(("Commands".into(), commands));
            }
            let projects: Rows = state
                .projects
                .iter()
                .filter(|p| score(&q, &p.name).is_some())
                .map(|p| {
                    let (name, seed, id) = (p.name.clone(), p.id.0, p.id);
                    let label: SharedString = format!("New thread in {name}").into();
                    let item = CommandItem::new().label(label.clone()).child(move |_, cx| {
                        row(ui::Monogram::new(name.clone(), seed).into_any_element(), label.clone(), None, None, cx)
                    });
                    (item, Target::NewThreadIn(id))
                })
                .collect();
            if !projects.is_empty() {
                groups.push(("Projects".into(), projects));
            }
            let label: SharedString = format!("Search messages for “{}”", query.trim()).into();
            groups.push((
                "Messages".into(),
                vec![(
                    CommandItem::new().label(label.clone()).child(move |_, cx| {
                        row(Icon::new(Lucide::SearchCode).size_4().into_any_element(), label.clone(), None, Some(ui::keycap("#")), cx)
                    }),
                    Target::Query("#"),
                )],
            ));
        }
    }
    groups
}

/// One palette row: a leading mark, the label, muted context and a trailing
/// hint (keys or age).
fn row(lead: AnyElement, label: SharedString, meta: Option<SharedString>, trail: Option<AnyElement>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    h_flex()
        .flex_1()
        .min_w_0()
        .gap_2()
        .child(h_flex().size_4().flex_none().justify_center().child(lead))
        .child(div().flex_shrink(1.).min_w_0().truncate().text_sm().child(label))
        .when_some(meta.filter(|m| !m.is_empty()), |el, meta| {
            el.child(div().flex_shrink_0().max_w(rems(10.)).truncate().text_xs().text_color(theme.muted_foreground).child(meta))
        })
        .child(div().flex_1())
        .children(trail)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn word_starts_beat_substrings_beat_scattered_letters() {
        let start = score("pars", "Parser streams tokens").unwrap();
        let word = score("stre", "Parser streams tokens").unwrap();
        let inside = score("ream", "Parser streams tokens").unwrap();
        let scattered = score("pst", "Parser streams tokens").unwrap();
        assert!(start > word && word > inside && inside > scattered, "{start} {word} {inside} {scattered}");
        assert_eq!(score("xyz", "Parser"), None);
        assert_eq!(score("", "anything"), Some(0));
    }
}
