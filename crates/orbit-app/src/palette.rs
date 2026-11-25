//! Command palette (Ctrl+K): every command, thread and project by name.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::command::{Command as Palette, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::{IconName, WindowExt as _};
use gpui_kit::*;
use orbit_proto::*;

use crate::assets::Lucide;
use crate::shell::{Route, Workspace};
use crate::thread::display_title;

#[derive(Clone)]
enum Target {
    Route(Route),
    NewThread,
    NewThreadIn(ProjectId),
    AddProject,
    ToggleSidebar,
    Settings,
}

pub fn open(workspace: Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let state = cx.new(|cx| CommandState::new(window, cx));
    let app = workspace.read(cx).app().clone();
    let snapshot = app.read(cx).state();

    let actions: Vec<(&'static str, gpui_kit::component::Icon, Target)> = vec![
        ("New thread", Lucide::SquarePen.into(), Target::NewThread),
        ("Inbox", IconName::Inbox.into(), Target::Route(Route::Inbox)),
        ("Fleet", IconName::LayoutDashboard.into(), Target::Route(Route::Fleet)),
        ("Add project…", Lucide::FolderPlus.into(), Target::AddProject),
        ("Toggle sidebar", IconName::PanelLeft.into(), Target::ToggleSidebar),
        ("Settings", IconName::Settings.into(), Target::Settings),
    ];
    let mut threads: Vec<&Thread> = snapshot.threads.values().collect();
    threads.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
    let threads: Vec<(ThreadId, String, String)> = threads
        .iter()
        .map(|t| {
            let project = t
                .project
                .and_then(|p| snapshot.projects.iter().find(|x| x.id == p))
                .map(|p| p.name.clone())
                .unwrap_or_default();
            (t.id, display_title(t).to_owned(), project)
        })
        .collect();
    let projects: Vec<(ProjectId, String)> = snapshot.projects.iter().map(|p| (p.id, p.name.clone())).collect();

    // Targets indexed exactly like the groups below (section, row).
    let targets: Rc<Vec<Vec<Target>>> = Rc::new(vec![
        actions.iter().map(|(_, _, t)| t.clone()).collect(),
        threads.iter().map(|(id, _, _)| Target::Route(Route::Thread(*id))).collect(),
        projects.iter().map(|(id, _)| Target::NewThreadIn(*id)).collect(),
    ]);
    let actions = Rc::new(actions);
    let threads = Rc::new(threads);
    let projects = Rc::new(projects);
    let first = Rc::new(Cell::new(true));
    let weak = workspace.downgrade();

    window.open_dialog(cx, move |dialog, _, _| {
        let (state, targets, actions, threads, projects, first, weak) =
            (state.clone(), targets.clone(), actions.clone(), threads.clone(), projects.clone(), first.clone(), weak.clone());
        dialog.close_button(false).p_0().w(px(560.)).content(move |content, window, cx| {
            if first.replace(false) {
                let focus = state.clone();
                window.defer(cx, move |window, cx| {
                    let handle = focus.read(cx).focus_handle(cx);
                    handle.focus(window, cx);
                });
            }
            let (targets, weak) = (targets.clone(), weak.clone());
            content.child(
                Palette::new(&state)
                    .bordered(false)
                    .placeholder("Search threads, projects and commands…")
                    .max_h(rems(26.))
                    .group(CommandGroup::new().label("Commands").items(
                        actions.iter().map(|(label, icon, _)| CommandItem::new().label(*label).icon(icon.clone())),
                    ))
                    .group(CommandGroup::new().label("Threads").items(threads.iter().map(|(_, title, project)| {
                        CommandItem::new()
                            .label(title.clone())
                            .icon(Lucide::MessageSquare)
                            .keywords([project.clone()])
                    })))
                    .group(CommandGroup::new().label("New thread in").items(
                        projects.iter().map(|(_, name)| CommandItem::new().label(name.clone()).icon(IconName::Folder)),
                    ))
                    .on_cancel(|window, cx| window.close_dialog(cx))
                    .on_confirm(move |ix, window, cx| {
                        window.close_dialog(cx);
                        let Some(target) = targets.get(ix.section).and_then(|s| s.get(ix.row)).cloned() else { return };
                        let _ = weak.update(cx, |ws, cx| match target {
                            Target::Route(route) => ws.navigate(route, window, cx),
                            Target::NewThread => ws.navigate(Route::Home, window, cx),
                            Target::NewThreadIn(project) => {
                                window.dispatch_action(Box::new(crate::sidebar::NewThreadIn { project: project.0 }), cx)
                            }
                            Target::AddProject => ws.add_project(&crate::actions::AddProject, window, cx),
                            Target::ToggleSidebar => window.dispatch_action(Box::new(crate::actions::ToggleSidebar), cx),
                            Target::Settings => ws.navigate(Route::Settings, window, cx),
                        });
                    }),
            )
        })
    });
}
