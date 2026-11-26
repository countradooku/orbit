//! The review sheet's dialogs: commit, pull request, local merge, restoring
//! to before a turn (with undo), and editing a message with its files.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::{DialogAction, DialogClose};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use std::cell::Cell;
use std::rc::Rc;

use super::state::ReviewMarks;
use crate::app_state::AppState;

fn thread_title(app: &Entity<AppState>, thread: ThreadId, cx: &App) -> String {
    app.read(cx).state().threads.get(&thread).map(|t| crate::thread::display_title(t).to_owned()).unwrap_or_default()
}

fn notify_error(window: &mut Window, title: &str, message: String, cx: &mut App) {
    window.push_notification(Notification::error(message).title(title.to_owned()), cx);
}

/// Has the engine write text for `task` from `input` (the change), then
/// hands it to `apply`. `busy` is set while it writes.
#[allow(clippy::too_many_arguments)]
fn generate(
    app: &Entity<AppState>,
    thread: ThreadId,
    task: TextTask,
    input: String,
    busy: Rc<Cell<bool>>,
    window: &mut Window,
    cx: &mut App,
    apply: impl FnOnce(String, &mut Window, &mut App) + 'static,
) {
    if busy.replace(true) {
        return;
    }
    window.refresh();
    let task = app.update(cx, |app, cx| app.call(Command::GenerateText { thread: Some(thread), task, input }, cx));
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| {
                busy.set(false);
                window.refresh();
                match result {
                    Ok(Reply::Text(text)) if !text.trim().is_empty() => apply(text.trim().to_owned(), window, cx),
                    Ok(_) => {}
                    Err(message) => notify_error(window, "Couldn’t write it", message, cx),
                }
            });
        })
        .detach();
}

/// A labelled field header with a "Generate" button.
fn field_header(label: &'static str, id: &'static str, busy: bool, muted: Hsla, on_click: impl Fn(&mut Window, &mut App) + 'static) -> impl IntoElement {
    h_flex()
        .justify_between()
        .child(div().text_xs().text_color(muted).child(label))
        .child(
            Button::new(id)
                .xsmall()
                .ghost()
                .icon(crate::assets::Lucide::Sparkles)
                .label(if busy { "Writing…" } else { "Generate" })
                .loading(busy)
                .disabled(busy)
                .on_click(move |_, window, cx| on_click(window, cx)),
        )
}

/// Commit everything in the thread's workspace, optionally pushing. The
/// message is written from `context` (the change) while the dialog opens,
/// unless the user starts typing first.
pub fn commit(app: Entity<AppState>, thread: ThreadId, context: String, after: super::AfterGit, window: &mut Window, cx: &mut App) {
    let initial = thread_title(&app, thread, cx);
    let message = cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 10).default_value(initial.clone()));
    crate::voice::register_field(crate::voice::Field::textarea(&message), cx);
    let push = Rc::new(Cell::new(true));
    let busy = Rc::new(Cell::new(false));
    let write = {
        let (app, message, busy, context) = (app.clone(), message.clone(), busy.clone(), context.clone());
        Rc::new(move |force: bool, window: &mut Window, cx: &mut App| {
            let (message, initial) = (message.clone(), initial.clone());
            generate(&app, thread, TextTask::CommitMessage, context.clone(), busy.clone(), window, cx, move |text, window, cx| {
                if force || message.read(cx).value().trim() == initial.trim() {
                    message.update(cx, |m, cx| m.set_value(text, window, cx));
                }
            });
        })
    };
    write(false, window, cx);
    window.open_dialog(cx, move |dialog, _, cx| {
        let (message, app, push, after) = (message.clone(), app.clone(), push.clone(), after.clone());
        let push_box = push.clone();
        let write = write.clone();
        let header = field_header("Message", "generate-commit", busy.get(), cx.theme().muted_foreground, move |window, cx| {
            write(true, window, cx)
        });
        dialog
            .title("Commit changes")
            .w(px(520.))
            .child(
                v_flex()
                    .gap_3()
                    .child(v_flex().gap_1().child(header).child(Textarea::new(&message)))
                    .child(
                        Checkbox::new("push")
                            .label("Push after committing")
                            .checked(push_box.get())
                            .on_click(move |checked: &bool, _, _| push_box.set(*checked)),
                    ),
            )
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("commit").primary().label("Commit"))),
            )
            .on_ok(move |_, window, cx| {
                let text = message.read(cx).value().trim().to_owned();
                if text.is_empty() {
                    return false;
                }
                let task = app.update(cx, |app, cx| app.call(Command::Commit { thread, message: text, push: push.get() }, cx));
                let after = after.clone();
                window
                    .spawn(cx, async move |cx| {
                        let result = task.await;
                        let _ = cx.update(|window, cx| {
                            match result {
                                Ok(_) => window.push_notification("Committed", cx),
                                Err(message) => notify_error(window, "Couldn’t commit", message, cx),
                            }
                            after(window, cx);
                        });
                    })
                    .detach();
                true
            })
    });
}

/// Pushes the thread's branch (setting its upstream the first time) and
/// says where it went.
pub fn push(app: Entity<AppState>, thread: ThreadId, after: super::AfterGit, window: &mut Window, cx: &mut App) {
    let task = app.update(cx, |app, cx| app.call(Command::Push { thread }, cx));
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| {
                match result {
                    Ok(Reply::Text(target)) => window.push_notification(Notification::success(format!("Pushed to {target}")), cx),
                    Ok(_) => window.push_notification(Notification::success("Pushed"), cx),
                    Err(message) => notify_error(window, "Couldn’t push", message, cx),
                }
                after(window, cx);
            });
        })
        .detach();
}

/// Push the branch and open a pull request; title and body are written
/// from `context` while the dialog opens, unless the user edits them first.
pub fn create_pr(
    app: Entity<AppState>,
    thread: ThreadId,
    files: Vec<String>,
    context: String,
    after: super::AfterGit,
    window: &mut Window,
    cx: &mut App,
) {
    let initial_title = thread_title(&app, thread, cx);
    let title = cx.new(|cx| InputState::new(window, cx).default_value(initial_title.clone()));
    crate::voice::register_field(crate::voice::Field::input(&title), cx);
    let body_text = format!("## Changes\n\n{}", files.iter().map(|f| format!("- `{f}`")).collect::<Vec<_>>().join("\n"));
    let body = cx.new(|cx| TextareaState::new(window, cx).auto_grow(6, 16).default_value(body_text.clone()));
    crate::voice::register_field(crate::voice::Field::textarea(&body), cx);
    let busy = Rc::new(Cell::new(false));
    let write = {
        let (app, title, body, busy) = (app.clone(), title.clone(), body.clone(), busy.clone());
        Rc::new(move |force: bool, window: &mut Window, cx: &mut App| {
            let (title, body, initial_title, body_text) = (title.clone(), body.clone(), initial_title.clone(), body_text.clone());
            generate(&app, thread, TextTask::PullRequest, context.clone(), busy.clone(), window, cx, move |text, window, cx| {
                let (head, rest) = text.split_once('\n').unwrap_or((&text, ""));
                let head = head.trim().trim_start_matches('#').trim().to_owned();
                if force || title.read(cx).value().trim() == initial_title.trim() {
                    title.update(cx, |t, cx| t.set_value(head, window, cx));
                }
                if !rest.trim().is_empty() && (force || body.read(cx).value() == body_text.as_str()) {
                    body.update(cx, |b, cx| b.set_value(rest.trim().to_owned(), window, cx));
                }
            });
        })
    };
    write(false, window, cx);
    window.open_dialog(cx, move |dialog, _, cx| {
        let (title, body, app, after) = (title.clone(), body.clone(), app.clone(), after.clone());
        let write = write.clone();
        let header = field_header("Title and description", "generate-pr", busy.get(), cx.theme().muted_foreground, move |window, cx| {
            write(true, window, cx)
        });
        dialog
            .title("Create pull request")
            .w(px(600.))
            .child(v_flex().gap_3().child(v_flex().gap_1().child(header).child(Input::new(&title))).child(Textarea::new(&body)))
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("create-pr").primary().label("Create pull request"))),
            )
            .on_ok(move |_, window, cx| {
                let (title, body) = (title.read(cx).value().trim().to_owned(), body.read(cx).value().to_string());
                if title.is_empty() {
                    return false;
                }
                let task = app.update(cx, |app, cx| app.call(Command::CreatePullRequest { thread, title, body }, cx));
                let after = after.clone();
                window
                    .spawn(cx, async move |cx| {
                        let result = task.await;
                        let _ = cx.update(|window, cx| {
                            match result {
                                Ok(Reply::PullRequest { url }) => {
                                    let open = url.clone();
                                    window.push_notification(
                                        Notification::success(url)
                                            .title("Pull request created")
                                            .on_click(move |_, _, cx| cx.open_url(&open)),
                                        cx,
                                    )
                                }
                                Ok(_) => {}
                                Err(message) => notify_error(window, "Couldn’t create the pull request", message, cx),
                            }
                            after(window, cx);
                        });
                    })
                    .detach();
                true
            })
    });
}

// ── Restore ───────────────────────────────────────────────────────────────

/// Asks before restoring the workspace to how it was before `turn`, naming
/// the files that change; the notification afterwards offers Undo.
pub fn confirm_restore(app: Entity<AppState>, thread: ThreadId, turn: u32, window: &mut Window, cx: &mut App) {
    let task = app.update(cx, |app, cx| app.call(Command::Diff { thread, scope: DiffScope::SinceTurn(turn) }, cx));
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| {
                let files = match result {
                    Ok(Reply::Diff(patch)) => diff::parse(&patch),
                    Ok(_) => Vec::new(),
                    Err(message) => return notify_error(window, "Couldn’t prepare the restore", message, cx),
                };
                if files.is_empty() {
                    window.push_notification(format!("Nothing to restore: the files already match turn {turn}’s start."), cx);
                    return;
                }
                open_restore_dialog(app.clone(), thread, turn, files, window, cx);
            });
        })
        .detach();
}

fn open_restore_dialog(app: Entity<AppState>, thread: ThreadId, turn: u32, files: Vec<FileDiff>, window: &mut Window, cx: &mut App) {
    let count = files.len();
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme().clone();
        let app = app.clone();
        let shown = 8;
        let list = v_flex().gap_0p5().children(files.iter().take(shown).map(|f| {
            h_flex()
                .gap_2()
                .text_sm()
                .child(div().w(px(12.)).text_color(theme.muted_foreground).child(super::model::kind_letter(f.kind)))
                .child(div().flex_1().min_w_0().truncate().child(f.path().to_owned()))
                .child(div().text_xs().text_color(theme.success).child(format!("+{}", f.additions)))
                .child(div().text_xs().text_color(theme.danger).child(format!("−{}", f.deletions)))
        }));
        let more = count.saturating_sub(shown);
        dialog
            .title(format!("Restore files to before turn {turn}?"))
            .w(px(560.))
            .child(
                v_flex()
                    .gap_3()
                    .child(div().text_sm().child(if count == 1 {
                        "This file goes back to how it was when the turn started, including any edits made since:".to_owned()
                    } else {
                        format!("These {count} files go back to how they were when the turn started, including any edits made since:")
                    }))
                    .child(list)
                    .when(more > 0, |el| el.child(div().text_xs().text_color(theme.muted_foreground).child(format!("and {more} more"))))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("The conversation stays as it is. You can undo the restore afterwards."),
                    ),
            )
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("restore").primary().label(if count == 1 {
                        "Restore 1 file".to_owned()
                    } else {
                        format!("Restore {count} files")
                    }))),
            )
            .on_ok(move |_, window, cx| {
                restore(app.clone(), thread, turn, count, window, cx);
                true
            })
    });
}

fn restore(app: Entity<AppState>, thread: ThreadId, turn: u32, count: usize, window: &mut Window, cx: &mut App) {
    let task = app.update(cx, |app, cx| app.call(Command::RestoreCheckpoint { thread, turn }, cx));
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Ok(_) => {
                    ReviewMarks::update(cx, thread, |m| m.restored = Some(turn));
                    let files = if count == 1 { "1 file".to_owned() } else { format!("{count} files") };
                    let app = app.clone();
                    window.push_notification(
                        Notification::success(format!("Restored {files} to before turn {turn}")).action(move |_, _, _| {
                            let app = app.clone();
                            Button::new("undo-restore").small().label("Undo").on_click(move |_, window, cx| {
                                undo_restore(app.clone(), thread, window, cx);
                            })
                        }),
                        cx,
                    );
                }
                Err(message) => notify_error(window, "Couldn’t restore", message, cx),
            });
        })
        .detach();
}

/// Puts the files back the way they were before the last restore.
pub fn undo_restore(app: Entity<AppState>, thread: ThreadId, window: &mut Window, cx: &mut App) {
    let task = app.update(cx, |app, cx| app.call(Command::UndoRestore { thread }, cx));
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| match result {
                Ok(_) => {
                    ReviewMarks::update(cx, thread, |m| m.restored = None);
                    window.push_notification(Notification::success("Restore undone"), cx);
                }
                Err(message) => notify_error(window, "Couldn’t undo the restore", message, cx),
            });
        })
        .detach();
}

/// Edit an earlier message and resend it. With "Restore files too", the
/// workspace first goes back to how it was before that message's turn, so
/// the new reply starts from the same files the original did. For the
/// thread view's "Edit" on a message.
pub fn edit_message(
    app: Entity<AppState>,
    thread: ThreadId,
    item: ItemId,
    turn: u32,
    text: String,
    window: &mut Window,
    cx: &mut App,
) {
    let input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 16).default_value(text));
    crate::voice::register_field(crate::voice::Field::textarea(&input), cx);
    let has_files = app.read(cx).state().threads.get(&thread).is_some_and(|t| t.project.is_some());
    let restore_files = std::rc::Rc::new(std::cell::Cell::new(has_files));
    window.open_dialog(cx, move |dialog, _, cx| {
        let (input, app, restore_files) = (input.clone(), app.clone(), restore_files.clone());
        let toggle = restore_files.clone();
        let muted = cx.theme().muted_foreground;
        dialog
            .title("Edit message")
            .w(px(600.))
            .child(
                v_flex()
                    .gap_2()
                    .child(Textarea::new(&input))
                    .child(div().text_xs().text_color(muted).child("Later messages in this thread are replaced by the new reply."))
                    .child(
                        Checkbox::new("restore-files")
                            .label(format!("Restore files to before turn {turn} too"))
                            .checked(toggle.get())
                            .disabled(!has_files)
                            .on_click(move |checked: &bool, _, _| toggle.set(*checked)),
                    )
                    .when(!has_files, |el| {
                        el.child(div().text_xs().text_color(muted).child("This thread isn’t in a project, so it has no files to restore."))
                    }),
            )
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("resend").primary().label("Resend"))),
            )
            .on_ok(move |_, window, cx| {
                let text = input.read(cx).value().trim().to_owned();
                if text.is_empty() {
                    return false;
                }
                let restore = restore_files.get();
                let app = app.clone();
                let restore_task = restore.then(|| app.update(cx, |app, cx| app.call(Command::RestoreCheckpoint { thread, turn }, cx)));
                window
                    .spawn(cx, async move |cx| {
                        if let Some(task) = restore_task
                            && let Err(message) = task.await
                        {
                            // Nothing was resent: the user decides again.
                            let _ = cx.update(|window, cx| notify_error(window, "Couldn’t restore the files, so nothing was resent", message, cx));
                            return;
                        }
                        let _ = cx.update(|window, cx| {
                            if restore {
                                ReviewMarks::update(cx, thread, |m| m.restored = Some(turn));
                            }
                            app.update(cx, |app, cx| app.run(Command::EditAndResend { thread, item, text }, window, cx));
                        });
                    })
                    .detach();
                true
            })
    });
}

// ── Merge ─────────────────────────────────────────────────────────────────

struct MergePanel {
    preview: Option<Result<MergeInfo, String>>,
    message: Entity<InputState>,
    into: String,
    elsewhere: Vec<String>,
}

impl Render for MergePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = |text: String| div().text_xs().text_color(theme.muted_foreground).child(text);
        let Some(preview) = &self.preview else {
            return h_flex().gap_2().py_4().justify_center().child(Spinner::new().small()).child(muted("Checking the merge…".into())).into_any_element();
        };
        let info = match preview {
            Err(message) => {
                return gpui_kit::component::alert::Alert::error("merge-error", message.clone()).title("Can’t merge").into_any_element();
            }
            Ok(info) => info,
        };
        let (adds, dels) = info.files.iter().fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        let mut summary = vec![];
        if info.commits > 0 {
            summary.push(format!("{} commit{}", info.commits, if info.commits == 1 { "" } else { "s" }));
        }
        summary.push(format!("{} file{}", info.files.len(), if info.files.len() == 1 { "" } else { "s" }));
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(format!("{} → {}", info.source, info.target))
                    .child(div().text_color(theme.muted_foreground).child(summary.join(", ")))
                    .child(div().text_xs().text_color(theme.success).child(format!("+{adds}")))
                    .child(div().text_xs().text_color(theme.danger).child(format!("−{dels}"))),
            )
            .when(info.fast_forward && info.uncommitted == 0, |el| el.child(muted(format!("{} only moves forward; no merge commit is needed.", info.target))))
            .when(!info.conflicts.is_empty(), |el| {
                el.child(
                    gpui_kit::component::alert::Alert::error(
                        "merge-conflicts",
                        format!("Both branches changed {}. Resolve this on the branch first.", info.conflicts.join(", ")),
                    )
                    .title("This merge would conflict"),
                )
            })
            .when(info.conflicts.is_empty() && !self.elsewhere.is_empty(), |el| {
                el.child(
                    gpui_kit::component::alert::Alert::warning(
                        "merge-elsewhere",
                        format!("Other live worktrees also changed {}.", self.elsewhere.join(", ")),
                    )
                    .title("Other threads may conflict later"),
                )
            })
            .when(info.uncommitted > 0 && info.conflicts.is_empty(), |el| {
                el.child(
                    v_flex()
                        .gap_1()
                        .child(div().text_sm().child(format!(
                            "{} uncommitted file{} {} committed first with this message:",
                            info.uncommitted,
                            if info.uncommitted == 1 { "" } else { "s" },
                            if info.uncommitted == 1 { "is" } else { "are" }
                        )))
                        .child(Input::new(&self.message)),
                )
            })
            .into_any_element()
    }
}

impl MergePanel {
    fn ready(&self) -> bool {
        matches!(&self.preview, Some(Ok(info)) if info.conflicts.is_empty() && (info.commits > 0 || info.uncommitted > 0))
    }
}

/// Merge the thread's branch into its project's base branch locally, after
/// a preview that shows the files, the commits and any conflict.
pub fn merge(app: Entity<AppState>, thread: ThreadId, view: WeakEntity<super::ReviewView>, window: &mut Window, cx: &mut App) {
    let state = app.read(cx).state();
    let into = state
        .threads
        .get(&thread)
        .and_then(|t| t.project)
        .and_then(|p| state.projects.iter().find(|x| x.id == p))
        .and_then(|p| p.base_branch.clone())
        .unwrap_or_else(|| "main".into());
    let elsewhere: Vec<String> =
        state.conflicts.iter().filter(|c| c.threads.contains(&thread)).map(|c| c.path.clone()).collect();
    let title = thread_title(&app, thread, cx);
    let message = cx.new(|cx| InputState::new(window, cx).default_value(title));
    crate::voice::register_field(crate::voice::Field::input(&message), cx);
    let panel = cx.new(|_| MergePanel { preview: None, message, into: into.clone(), elsewhere });
    let preview = app.update(cx, |app, cx| {
        app.call(Command::MergeLocal { thread, into: into.clone(), message: String::new(), dry_run: true }, cx)
    });
    let weak = panel.downgrade();
    window
        .spawn(cx, async move |cx| {
            let result = preview.await;
            let _ = cx.update(|_, cx| {
                let _ = weak.update(cx, |panel, cx| {
                    panel.preview = Some(match result {
                        Ok(Reply::Merge(info)) => Ok(info),
                        Ok(_) => Err("The engine sent an unexpected reply.".into()),
                        Err(message) => Err(message),
                    });
                    cx.notify();
                });
            });
        })
        .detach();
    window.open_dialog(cx, move |dialog, _, cx| {
        let (panel, app, view) = (panel.clone(), app.clone(), view.clone());
        let ready = panel.read(cx).ready();
        let into = panel.read(cx).into.clone();
        dialog
            .title(format!("Merge into {into}"))
            .w(px(560.))
            .child(panel.clone())
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("merge").primary().label(format!("Merge into {into}")).disabled(!ready))),
            )
            .on_ok(move |_, window, cx| {
                let p = panel.read(cx);
                if !p.ready() {
                    return false;
                }
                let message = p.message.read(cx).value().trim().to_owned();
                let into = p.into.clone();
                let task = app.update(cx, |app, cx| app.call(Command::MergeLocal { thread, into, message, dry_run: false }, cx));
                let view = view.clone();
                window
                    .spawn(cx, async move |cx| {
                        let result = task.await;
                        let _ = cx.update(|window, cx| {
                            match result {
                                Ok(Reply::Merge(info)) => window.push_notification(
                                    Notification::success(format!("Merged {} into {}", info.source, info.target)),
                                    cx,
                                ),
                                Ok(_) => {}
                                Err(message) => notify_error(window, "Couldn’t merge", message, cx),
                            }
                            let _ = view.update(cx, |view, cx| view.load(cx));
                        });
                    })
                    .detach();
                true
            })
    });
}
