//! Quick open (`mod-p`): find a file of the thread's workspace by name.
//!
//! The engine ranks the workspace's file index (gitignore-aware, built once
//! and refreshed behind the query), so typing never waits on the disk. An
//! empty query lists the files opened lately. `path:line` opens at a line.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::command::{Command as Palette, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex};
use gpui_kit::*;
use orbit_proto::*;

use super::FilesDock;
use crate::app_state::AppState;

struct Results {
    query: String,
    files: Vec<FileMatch>,
    generation: u64,
}

/// Splits `src/main.rs:42` into the path and the line.
pub(crate) fn split_line(query: &str) -> (&str, Option<u32>) {
    match query.rsplit_once(':') {
        Some((path, line)) if !path.is_empty() && !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()) => {
            (path, line.parse().ok())
        }
        _ => (query, None),
    }
}

pub fn open(
    app: Entity<AppState>,
    thread: ThreadId,
    recent: Vec<String>,
    dock: Entity<FilesDock>,
    window: &mut Window,
    cx: &mut App,
) {
    let state = cx.new(|cx| CommandState::new(window, cx));
    let results = Rc::new(RefCell::new(Results { query: String::new(), files: Vec::new(), generation: 0 }));
    let shown: Rc<RefCell<Vec<String>>> = Rc::default();
    let first = Rc::new(std::cell::Cell::new(true));
    let width = window.rem_size() * 36.;
    window.open_dialog(cx, move |dialog, _, _| {
        let (state, results, shown, first, app, dock, recent) =
            (state.clone(), results.clone(), shown.clone(), first.clone(), app.clone(), dock.clone(), recent.clone());
        dialog.close_button(false).p_0().w(width).content(move |content, window, cx| {
            if first.replace(false) {
                let focus = state.clone();
                window.defer(cx, move |window, cx| focus.update(cx, |s, cx| s.focus(window, cx)));
            }
            let query = state.read(cx).query(cx).to_string();
            let (path_query, _) = split_line(query.trim());
            let paths: Vec<(String, bool)> = if path_query.is_empty() {
                recent.iter().map(|p| (p.clone(), false)).collect()
            } else if results.borrow().query == path_query {
                results.borrow().files.iter().map(|f| (f.path.clone(), f.is_dir)).collect()
            } else {
                // Keep the last answer on screen until the next arrives.
                results.borrow().files.iter().map(|f| (f.path.clone(), f.is_dir)).collect()
            };
            let paths: Vec<String> = paths.into_iter().filter(|(_, dir)| !dir).map(|(p, _)| p).take(60).collect();
            *shown.borrow_mut() = paths.clone();
            let label = if path_query.is_empty() { "Opened lately" } else { "Files" };
            let items = paths.into_iter().map(|path| {
                let (dir, name) = match path.rsplit_once('/') {
                    Some((dir, name)) => (dir.to_owned(), name.to_owned()),
                    None => (String::new(), path.clone()),
                };
                let icon_path = path.clone();
                CommandItem::new().label(path.clone()).child(move |_, cx| {
                    let theme = cx.theme();
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .child(super::file_icon(&icon_path, false, false).small().text_color(theme.muted_foreground))
                        .child(div().flex_none().text_sm().child(name.clone()))
                        .child(div().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(dir.clone()))
                })
            });
            let palette = Palette::new(&state)
                .bordered(false)
                .filterable(false)
                .placeholder("Open a file by name · path:line opens at a line")
                .max_h(rems(26.))
                .group(CommandGroup::new().label(label).items(items));
            let (on_query_app, on_query_results, on_query_state) = (app.clone(), results.clone(), state.clone());
            let (confirm_shown, confirm_dock, confirm_state) = (shown.clone(), dock.clone(), state.clone());
            content.child(
                palette
                    .empty(|_, _, cx| {
                        div().py_6().text_sm().text_center().text_color(cx.theme().muted_foreground).child("No file matches.")
                    })
                    .on_query(move |query, window, cx| {
                        search(&on_query_app, thread, query, &on_query_results, &on_query_state, window, cx);
                        window.refresh();
                    })
                    .on_cancel(|window, cx| window.close_dialog(cx))
                    .on_confirm(move |ix, window, cx| {
                        let Some(path) = confirm_shown.borrow().get(ix.row).cloned() else { return };
                        let (_, line) = split_line(confirm_state.read(cx).query(cx).trim()).to_owned();
                        window.close_dialog(cx);
                        let dock = confirm_dock.clone();
                        window.defer(cx, move |window, cx| dock.update(cx, |d, cx| d.open_file(&path, line, window, cx)));
                    }),
            )
        })
    });
}

fn search(
    app: &Entity<AppState>,
    thread: ThreadId,
    query: &str,
    cell: &Rc<RefCell<Results>>,
    state: &Entity<CommandState>,
    window: &mut Window,
    cx: &mut App,
) {
    let (path, _) = split_line(query.trim());
    let text = path.to_owned();
    let generation = {
        let mut results = cell.borrow_mut();
        results.generation += 1;
        results.generation
    };
    if text.is_empty() {
        return;
    }
    let (app, cell, state) = (app.clone(), cell.clone(), state.downgrade());
    window
        .spawn(cx, async move |cx| {
            // One search per pause in typing, not one per key.
            cx.background_executor().timer(Duration::from_millis(60)).await;
            if cell.borrow().generation != generation {
                return;
            }
            let command = Command::SearchFiles { thread: Some(thread), project: None, query: text.clone(), limit: 80 };
            let Ok(task) = cx.update(|_, cx| app.update(cx, |app, cx| app.call(command, cx))) else { return };
            let result = task.await;
            let _ = cx.update(|window, cx| {
                if cell.borrow().generation != generation {
                    return;
                }
                if let Ok(Reply::Files(files)) = result {
                    let mut results = cell.borrow_mut();
                    results.query = text.clone();
                    results.files = files;
                }
                if let Some(state) = state.upgrade() {
                    state.update(cx, |s, cx| s.set_selected_index(Some(gpui_kit::component::IndexPath::default()), window, cx));
                }
                window.refresh();
            });
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::split_line;

    #[test]
    fn a_line_after_a_colon_is_split_off() {
        assert_eq!(split_line("src/main.rs:42"), ("src/main.rs", Some(42)));
        assert_eq!(split_line("src/main.rs"), ("src/main.rs", None));
        assert_eq!(split_line("C:"), ("C:", None));
        assert_eq!(split_line(":12"), (":12", None));
    }
}
