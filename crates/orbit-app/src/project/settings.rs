//! Settings › Projects: one project's name, icon, base branch, the agent
//! new threads start with, worktree scripts, run targets, shared files,
//! ports and environment; importing from the repository; archive and remove.
//!
//! Edits apply at once and are saved shortly after the last keystroke
//! (`UpdateProject` for the logged record, `SetConfig` for the rest). The
//! page keeps a fixed list of items (Settings keys field state by
//! position), so the target list, which grows and shrinks, is one item.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::setting::{NumberFieldOptions, RenderOptions, SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;
use crate::assets::Lucide;

/// Pause after the last edit before it is saved.
const SAVE_DELAY: Duration = Duration::from_millis(400);

/// The page's index among the settings pages (see `settings::page`).
pub const PAGE_TITLE: &str = "Projects";

struct Draft {
    project: Project,
    config: ProjectConfig,
    /// Sent and not echoed yet: kept until the engine's copy matches.
    sent: bool,
}

#[derive(Default)]
struct Model {
    selected: Option<ProjectId>,
    drafts: HashMap<ProjectId, Draft>,
    save: Option<Task<()>>,
    /// Opened from somewhere else: Settings opens on this page.
    requested: bool,
}

impl Global for Model {}

/// Opens Settings › Projects on `project`.
pub fn open(project: Option<ProjectId>, window: &mut Window, cx: &mut App) {
    let model = cx.default_global::<Model>();
    if project.is_some() {
        model.selected = project;
    }
    model.requested = true;
    window.dispatch_action(Box::new(crate::actions::OpenSettings), cx);
}

/// Whether Settings was asked to open on this page (consumed).
pub fn take_request(cx: &mut App) -> bool {
    std::mem::take(&mut cx.default_global::<Model>().requested)
}

fn selected(app: &Entity<AppState>, cx: &App) -> Option<ProjectId> {
    let state = app.read(cx).state();
    let picked = cx.try_global::<Model>().and_then(|m| m.selected);
    picked.filter(|id| state.projects.iter().any(|p| p.id == *id)).or_else(|| state.projects.first().map(|p| p.id))
}

/// The project as edited here (a pending draft) or as the engine has it.
fn current(app: &Entity<AppState>, id: ProjectId, cx: &App) -> Option<(Project, ProjectConfig)> {
    if let Some(draft) = cx.try_global::<Model>().and_then(|m| m.drafts.get(&id)) {
        return Some((draft.project.clone(), draft.config.clone()));
    }
    let state = app.read(cx).state();
    let project = state.projects.iter().find(|p| p.id == id)?.clone();
    Some((project, state.project_config(id).into_owned()))
}

/// Edits a project here and saves it shortly.
fn edit(app: &Entity<AppState>, id: ProjectId, cx: &mut App, change: impl FnOnce(&mut Project, &mut ProjectConfig)) {
    let Some((mut project, mut config)) = current(app, id, cx) else { return };
    let before = (project.clone(), config.clone());
    change(&mut project, &mut config);
    if (project.clone(), config.clone()) == before {
        return;
    }
    cx.default_global::<Model>().drafts.insert(id, Draft { project, config, sent: false });
    cx.refresh_windows();
    let app = app.clone();
    let save = cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVE_DELAY).await;
        cx.update(|cx| flush(&app, cx));
    });
    cx.default_global::<Model>().save = Some(save);
}

/// Sends every pending edit.
fn flush(app: &Entity<AppState>, cx: &mut App) {
    let mut commands = Vec::new();
    {
        let state = app.read(cx).state();
        let Some(model) = cx.try_global::<Model>() else { return };
        for (id, draft) in model.drafts.iter().filter(|(_, d)| !d.sent) {
            if state.projects.iter().find(|p| p.id == *id) != Some(&draft.project) {
                commands.push(Command::UpdateProject { project: draft.project.clone() });
            }
            if *state.project_config(*id) != draft.config {
                commands.push(Command::Workspace(WorkspaceCommand::SetConfig { project: *id, config: Box::new(draft.config.clone()) }));
            }
        }
    }
    for draft in cx.default_global::<Model>().drafts.values_mut() {
        draft.sent = true;
    }
    for command in commands {
        app.read(cx).send(command);
    }
}

/// Drops drafts the engine has caught up with.
fn prune(app: &Entity<AppState>, cx: &mut App) {
    let state = app.read(cx).state();
    let done: Vec<ProjectId> = cx
        .try_global::<Model>()
        .map(|m| {
            m.drafts
                .iter()
                .filter(|(id, d)| {
                    let gone = !state.projects.iter().any(|p| p.id == **id);
                    let caught_up = d.sent
                        && state.projects.iter().find(|p| p.id == **id) == Some(&d.project)
                        && *state.project_config(**id) == d.config.clone().sanitized();
                    gone || caught_up
                })
                .map(|(id, _)| *id)
                .collect()
        })
        .unwrap_or_default();
    if !done.is_empty() {
        let model = cx.default_global::<Model>();
        for id in done {
            model.drafts.remove(&id);
        }
    }
}

// ── The page ─────────────────────────────────────────────────────────────

pub fn page(app: &Entity<AppState>, cx: &mut App) -> SettingPage {
    prune(app, cx);
    let id = selected(app, cx);
    let none = id.is_none();
    let description = match id.and_then(|id| current(app, id, cx)) {
        Some((project, _)) => project.path.clone(),
        None => "Add a project to set it up here.".to_owned(),
    };
    let is_repo = id.is_none_or(|id| app.read(cx).state().git.get(&WorkspaceRoot::Project(id)).is_none_or(|g| g.repo));
    let a = app.clone();
    SettingPage::new(PAGE_TITLE).icon(Icon::new(IconName::Folder)).resettable(false).groups([
        SettingGroup::new().title("Project").items([
            SettingItem::new("Project", picker_field(app.clone())).description(description).disabled(none),
            SettingItem::new("Status", status_field(app.clone()))
                .description(if is_repo {
                    "Live from the checkout: branch, commits ahead and behind, uncommitted changes and conflicts."
                } else {
                    "Not a git repository: checkpoints are kept by Orbit. Branches, commits and worktrees need git."
                })
                .disabled(none),
        ]),
        SettingGroup::new().title("General").items([
            SettingItem::new(
                "Name",
                text(app, "name", "Project name", |p, _| p.name.clone(), |p, _, v| {
                    if !v.trim().is_empty() {
                        p.name = v.trim().to_owned();
                    }
                }),
            )
            .disabled(none),
            SettingItem::new("Icon", text(app, "icon", "🚀 or two letters", |_, c| c.icon.clone(), |_, c, v| c.icon = v))
                .description("Shown in the sidebar and pickers. Leave empty for the project’s initials.")
                .disabled(none),
            SettingItem::new(
                "Base branch",
                text(app, "base", "main", |p, _| p.base_branch.clone().unwrap_or_default(), |p, _, v| {
                    p.base_branch = Some(v.trim().to_owned()).filter(|b| !b.is_empty())
                }),
            )
            .description("New worktrees branch from it, and ahead and behind count against it when a branch has no upstream.")
            .disabled(none || !is_repo),
            SettingItem::new("New threads start on a worktree", worktree_field(app.clone()))
                .description("Each thread gets its own branch and folder, so parallel agents never edit the same files.")
                .disabled(none || !is_repo),
            SettingItem::new("Agent for new threads", agent_field(app.clone()))
                .description("New threads in this project start with this agent, model and options.")
                .disabled(none),
        ]),
        SettingGroup::new().title("Worktree scripts").items([
            SettingItem::new(
                "Setup",
                script(app, "setup", "npm ci", |p| p.scripts.setup.clone().unwrap_or_default(), |p, v| {
                    p.scripts.setup = Some(v).filter(|s| !s.trim().is_empty())
                }),
            )
            .layout(Axis::Vertical)
            .description("Runs in each new worktree before the agent starts; messages wait for it. Gets PORT and the ORBIT_* variables.")
            .disabled(none || !is_repo),
            SettingItem::new(
                "Teardown",
                script(app, "teardown", "docker compose down", |p| p.scripts.teardown.clone().unwrap_or_default(), |p, v| {
                    p.scripts.teardown = Some(v).filter(|s| !s.trim().is_empty())
                }),
            )
            .layout(Axis::Vertical)
            .description("Runs in a worktree before it is removed.")
            .disabled(none || !is_repo),
        ]),
        SettingGroup::new()
            .title("Run targets")
            .description("Dev servers, tests and linters, run in a thread’s folder from its header, the palette or Ctrl+Shift+R.")
            .item(SettingItem::render(move |_, window, cx| targets_list(&a, window, cx))),
        SettingGroup::new().title("Worktrees").items([
            SettingItem::new("Shared files", lines(app, "include", ".env\nconfig/local.json", |c| c.include.join("\n"), |c, v| {
                c.include = v.lines().map(str::to_owned).collect()
            }))
            .layout(Axis::Vertical)
            .description("Untracked files copied from the main checkout into each new worktree, one per line (gitignore patterns). Added to the repository’s .worktreeinclude.")
            .keywords(["worktreeinclude", "env", "copy"])
            .disabled(none || !is_repo),
            SettingItem::new("Ports per worktree", ports_field(app.clone()))
                .description("Each worktree reserves this many ports; scripts get the first as PORT, so parallel dev servers never collide.")
                .disabled(none || !is_repo),
            SettingItem::new("Environment", lines(app, "env", "API_URL=http://localhost:8080", |c| format_env(&c.env), |c, v| {
                c.env = parse_env(&v)
            }))
            .layout(Axis::Vertical)
            .description("NAME=value lines, for the setup and teardown scripts and the run targets.")
            .keywords(["variables", "env"])
            .disabled(none),
        ]),
        SettingGroup::new().title("Configuration in the repository").item(
            SettingItem::new("Import", import_field(app.clone()))
                .description(
                    "Reads orbit.toml, conductor.json, Codex environments, devcontainer.json and package.json, and shows what would change.",
                )
                .keywords(["orbit.toml", "conductor", "codex", "devcontainer"])
                .disabled(none),
        ),
        SettingGroup::new().title("Archive or remove").items([
            SettingItem::new(
                "Archived",
                SettingField::switch(
                    {
                        let app = app.clone();
                        move |cx| selected(&app, cx).and_then(|id| current(&app, id, cx)).is_some_and(|(_, c)| c.archived)
                    },
                    {
                        let app = app.clone();
                        move |v, cx| {
                            if let Some(id) = selected(&app, cx) {
                                edit(&app, id, cx, |_, c| c.archived = v);
                            }
                        }
                    },
                ),
            )
            .description("Hidden from the sidebar and pickers, and no longer watched. Its threads are kept.")
            .disabled(none),
            SettingItem::new("Remove", remove_field(app.clone()))
                .description("Removes the project and its threads from Orbit. Files in the folder are not touched.")
                .disabled(none),
        ]),
    ])
}

// ── Fields ───────────────────────────────────────────────────────────────

fn picker_field(app: Entity<AppState>) -> SettingField<SharedString> {
    SettingField::render(move |options: &RenderOptions, _, cx| {
        let state = app.read(cx).state();
        let id = selected(&app, cx);
        let projects: Vec<(ProjectId, String, bool)> = state
            .projects
            .iter()
            .map(|p| (p.id, p.name.clone(), state.project_config(p.id).archived))
            .collect();
        let label = id.and_then(|id| projects.iter().find(|p| p.0 == id)).map(|p| p.1.clone()).unwrap_or_else(|| "No projects".into());
        let mark = id.and_then(|id| state.projects.iter().find(|p| p.id == id)).map(|p| super::mark(p, &state.project_config(p.id)));
        h_flex()
            .gap_2()
            .children(mark)
            .child(
                Button::new("project-picker")
                    .outline()
                    .small()
                    .label(label)
                    .dropdown_caret(true)
                    .disabled(options.is_disabled())
                    .dropdown_menu(move |mut menu, _, _| {
                        for (pid, name, archived) in &projects {
                            let pid = *pid;
                            let label = if *archived { format!("{name} (archived)") } else { name.clone() };
                            menu = menu.item(PopupMenuItem::new(label).checked(Some(pid) == id).on_click(move |_, window, cx| {
                                cx.default_global::<Model>().selected = Some(pid);
                                window.refresh();
                            }));
                        }
                        menu.separator().item(PopupMenuItem::new("Add project…").icon(Lucide::FolderPlus).on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(crate::actions::AddProject), cx)
                        }))
                    }),
            )
            .into_any_element()
    })
}

fn status_field(app: Entity<AppState>) -> SettingField<SharedString> {
    SettingField::render(move |_, _, cx| {
        let Some(id) = selected(&app, cx) else { return div().into_any_element() };
        let state = app.read(cx).state();
        match state.git.get(&WorkspaceRoot::Project(id)) {
            Some(summary) if !summary.repo => {
                div().text_sm().text_color(cx.theme().muted_foreground).child("No git").into_any_element()
            }
            summary => super::GitBadge::new(("project-status", id.0), summary).into_any_element(),
        }
    })
}

struct TextState {
    input: Entity<InputState>,
    _change: Subscription,
}

/// A one-line text field bound to the selected project.
fn text(
    app: &Entity<AppState>,
    key: &'static str,
    placeholder: &'static str,
    value: impl Fn(&Project, &ProjectConfig) -> String + 'static,
    set: impl Fn(&mut Project, &mut ProjectConfig, String) + 'static,
) -> SettingField<SharedString> {
    let (app, value, set) = (app.clone(), Rc::new(value), Rc::new(set));
    SettingField::render(move |options: &RenderOptions, window, cx| {
        let Some(id) = selected(&app, cx) else { return Input::new(&dummy_input(window, cx)).disabled(true).w_80().into_any_element() };
        let current: SharedString = current(&app, id, cx).map(|(p, c)| value(&p, &c)).unwrap_or_default().into();
        let state = window.use_keyed_state(SharedString::from(format!("project-{}-{key}", id.0)), cx, {
            let (app, set, current) = (app.clone(), set.clone(), current.clone());
            move |window, cx| {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(current));
                let _change = cx.subscribe(&input, move |_, input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        let text = input.read(cx).value().to_string();
                        let set = set.clone();
                        edit(&app, id, cx, move |p, c| set(p, c, text));
                    }
                });
                TextState { input, _change }
            }
        });
        let input = state.read(cx).input.clone();
        if !input.read(cx).presentation().focus_handle().is_focused(window) && input.read(cx).value() != current {
            input.update(cx, |input, cx| input.set_value(current.clone(), window, cx));
        }
        Input::new(&input).with_size(options.size()).disabled(options.is_disabled()).w_80().into_any_element()
    })
}

fn dummy_input(window: &mut Window, cx: &mut App) -> Entity<InputState> {
    window.use_keyed_state(SharedString::from("project-none"), cx, |window, cx| cx.new(|cx| InputState::new(window, cx))).read(cx).clone()
}

struct AreaState {
    input: Entity<TextareaState>,
    _change: Subscription,
}

/// A multi-line field bound to the selected project's configuration.
fn lines(
    app: &Entity<AppState>,
    key: &'static str,
    placeholder: &'static str,
    value: impl Fn(&ProjectConfig) -> String + 'static,
    set: impl Fn(&mut ProjectConfig, String) + 'static,
) -> SettingField<SharedString> {
    let (value, set) = (Rc::new(value), Rc::new(set));
    area(app, key, placeholder, move |_, c| value(c), move |_, c, v| set(c, v))
}

/// A script field bound to the selected project's record.
fn script(
    app: &Entity<AppState>,
    key: &'static str,
    placeholder: &'static str,
    value: impl Fn(&Project) -> String + 'static,
    set: impl Fn(&mut Project, String) + 'static,
) -> SettingField<SharedString> {
    let (value, set) = (Rc::new(value), Rc::new(set));
    area(app, key, placeholder, move |p, _| value(p), move |p, _, v| set(p, v))
}

fn area(
    app: &Entity<AppState>,
    key: &'static str,
    placeholder: &'static str,
    value: impl Fn(&Project, &ProjectConfig) -> String + 'static,
    set: impl Fn(&mut Project, &mut ProjectConfig, String) + 'static,
) -> SettingField<SharedString> {
    let (app, value, set) = (app.clone(), Rc::new(value), Rc::new(set));
    SettingField::render(move |options: &RenderOptions, window, cx| {
        let id = selected(&app, cx).unwrap_or_default();
        let current = current(&app, id, cx).map(|(p, c)| value(&p, &c)).unwrap_or_default();
        let state = window.use_keyed_state(SharedString::from(format!("project-{}-{key}", id.0)), cx, {
            let (app, set, current) = (app.clone(), set.clone(), current.clone());
            move |window, cx| {
                let input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 12).placeholder(placeholder).default_value(current));
                let _change = cx.subscribe(&input, move |_, input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        let text = input.read(cx).value().to_string();
                        let set = set.clone();
                        edit(&app, id, cx, move |p, c| set(p, c, text));
                    }
                });
                AreaState { input, _change }
            }
        });
        let input = state.read(cx).input.clone();
        let focused = input.read(cx).presentation().focus_handle().is_focused(window);
        if !focused && input.read(cx).value().as_ref() != current {
            input.update(cx, |input, cx| input.set_value(current.clone(), window, cx));
        }
        div()
            .w_full()
            .font_family(cx.theme().mono_font_family.clone())
            .child(Textarea::new(&input).disabled(options.is_disabled()))
            .into_any_element()
    })
}

fn worktree_field(app: Entity<AppState>) -> SettingField<SharedString> {
    let items = vec![
        (SharedString::from("follow"), SharedString::from("As in Settings › Workspace")),
        ("on".into(), "Always".into()),
        ("off".into(), "Never".into()),
    ];
    let read = app.clone();
    SettingField::dropdown(
        items,
        move |cx| {
            let value = selected(&read, cx).and_then(|id| current(&read, id, cx)).and_then(|(_, c)| c.worktree_by_default);
            SharedString::from(match value {
                None => "follow",
                Some(true) => "on",
                Some(false) => "off",
            })
        },
        move |picked: SharedString, cx| {
            if let Some(id) = selected(&app, cx) {
                edit(&app, id, cx, |_, c| {
                    c.worktree_by_default = match picked.as_ref() {
                        "on" => Some(true),
                        "off" => Some(false),
                        _ => None,
                    }
                });
            }
        },
    )
}

fn ports_field(app: Entity<AppState>) -> SettingField<f64> {
    let read = app.clone();
    SettingField::number_input(
        NumberFieldOptions { min: 0., max: 100., step: 1. },
        move |cx| {
            let ports = selected(&read, cx).and_then(|id| current(&read, id, cx)).map_or(0, |(_, c)| c.ports);
            f64::from(if ports == 0 { 10 } else { ports })
        },
        move |v, cx| {
            if let Some(id) = selected(&app, cx) {
                edit(&app, id, cx, |_, c| c.ports = (v.round().clamp(1., 100.)) as u16);
            }
        },
    )
    .default_value(10.)
}

fn agent_field(app: Entity<AppState>) -> SettingField<SharedString> {
    SettingField::render(move |options: &RenderOptions, _, cx| {
        let Some(id) = selected(&app, cx) else { return div().into_any_element() };
        let config = current(&app, id, cx).and_then(|(_, c)| c.default_config);
        let state = app.read(cx).state();
        let label = match &config {
            Some(config) => crate::ui::model_label(config, &state.providers),
            None => "Last used".to_owned(),
        };
        let defaults = state.defaults.clone();
        let (use_app, clear_app) = (app.clone(), app.clone());
        h_flex()
            .gap_2()
            .child(div().text_sm().child(label))
            .child(
                Button::new("project-agent")
                    .outline()
                    .small()
                    .label("Change")
                    .dropdown_caret(true)
                    .disabled(options.is_disabled())
                    .dropdown_menu(move |menu, _, _| {
                        let (use_app, clear_app, defaults) = (use_app.clone(), clear_app.clone(), defaults.clone());
                        menu.item(PopupMenuItem::new("Use the current model and options").on_click(move |_, _, cx| {
                            let mut config = defaults.clone();
                            config.access = config.access.for_new_thread();
                            edit(&use_app, id, cx, |_, c| c.default_config = Some(config));
                        }))
                        .item(PopupMenuItem::new("Follow the last used").on_click(move |_, _, cx| {
                            edit(&clear_app, id, cx, |_, c| c.default_config = None);
                        }))
                    }),
            )
            .into_any_element()
    })
}

fn import_field(app: Entity<AppState>) -> SettingField<SharedString> {
    SettingField::render(move |options: &RenderOptions, _, cx| {
        let app = app.clone();
        let sources = selected(&app, cx).and_then(|id| current(&app, id, cx)).map(|(_, c)| c.imported_from).unwrap_or_default();
        h_flex()
            .gap_2()
            .when(!sources.is_empty(), |el| {
                el.child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!("Last from {}", sources.join(", "))))
            })
            .child(
                Button::new("project-import")
                    .outline()
                    .small()
                    .icon(IconName::ArrowDown)
                    .label("Import…")
                    .disabled(options.is_disabled())
                    .on_click(move |_, window, cx| import(&app, window, cx)),
            )
            .into_any_element()
    })
}

fn remove_field(app: Entity<AppState>) -> SettingField<SharedString> {
    SettingField::render(move |options: &RenderOptions, _, _| {
        let app = app.clone();
        Button::new("project-remove")
            .danger()
            .small()
            .label("Remove project…")
            .disabled(options.is_disabled())
            .on_click(move |_, window, cx| confirm_remove(&app, window, cx))
            .into_any_element()
    })
}

/// Reads the repository's configuration and shows what would change.
fn import(app: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    let Some(id) = selected(app, cx) else { return };
    let Some((project, config)) = current(app, id, cx) else { return };
    let task = app.update(cx, |app, cx| app.call(Command::Workspace(WorkspaceCommand::Import { project: id }), cx));
    let app = app.clone();
    window
        .spawn(cx, async move |cx| {
            let result = task.await;
            let _ = cx.update(|window, cx| {
                let (imported, imported_config, sources) = match result {
                    Ok(Reply::Workspace(WorkspaceReply::Imported { project, config, sources })) => (*project, *config, sources),
                    Ok(_) => return,
                    Err(message) => {
                        window.push_notification(gpui_kit::component::notification::Notification::error(message), cx);
                        return;
                    }
                };
                let changes = describe_import(&project, &config, &imported, &imported_config);
                if sources.is_empty() || changes.is_empty() {
                    let text = if sources.is_empty() {
                        "No orbit.toml, conductor.json, Codex environment, dev container or package.json was found."
                    } else {
                        "The project already matches what its files configure."
                    };
                    window.push_notification(gpui_kit::component::notification::Notification::info(text), cx);
                    return;
                }
                window.open_dialog(cx, move |dialog, _, cx| {
                    let theme = cx.theme().clone();
                    let (app, imported, imported_config) = (app.clone(), imported.clone(), imported_config.clone());
                    dialog
                        .title("Import project configuration?")
                        .w(px(520.))
                        .child(
                            v_flex()
                                .gap_2()
                                .text_sm()
                                .child(div().text_color(theme.muted_foreground).child(format!("From {}:", sources.join(", "))))
                                .children(changes.iter().map(|c| {
                                    h_flex()
                                        .gap_2()
                                        .items_start()
                                        .child(Icon::new(IconName::Check).xsmall().mt_0p5().text_color(theme.muted_foreground))
                                        .child(div().flex_1().child(c.clone()))
                                })),
                        )
                        .footer(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
                                .child(DialogAction::new().child(Button::new("apply").primary().label("Import"))),
                        )
                        .on_ok(move |_, _, cx| {
                            let (imported, imported_config) = (imported.clone(), imported_config.clone());
                            edit(&app, id, cx, move |p, c| {
                                *p = imported;
                                *c = imported_config;
                            });
                            true
                        })
                });
            });
        })
        .detach();
}

/// What importing would change, one line each.
fn describe_import(project: &Project, config: &ProjectConfig, new: &Project, new_config: &ProjectConfig) -> Vec<String> {
    let mut out = Vec::new();
    if new.name != project.name {
        out.push(format!("Name: {}", new.name));
    }
    if new.base_branch != project.base_branch {
        out.push(format!("Base branch: {}", new.base_branch.as_deref().unwrap_or("none")));
    }
    if new.scripts.setup != project.scripts.setup {
        out.push(format!("Setup: {}", new.scripts.setup.as_deref().unwrap_or("none")));
    }
    if new.scripts.teardown != project.scripts.teardown {
        out.push(format!("Teardown: {}", new.scripts.teardown.as_deref().unwrap_or("none")));
    }
    for target in &new_config.targets {
        match config.targets.iter().find(|t| t.name == target.name) {
            None => out.push(format!("New target {}: {}", target.name, target.command)),
            Some(old) if old != target => out.push(format!("Target {}: {}", target.name, target.command)),
            _ => {}
        }
    }
    if new_config.icon != config.icon {
        out.push(format!("Icon: {}", new_config.icon));
    }
    if new_config.include != config.include {
        out.push(format!("Shared files: {}", new_config.include.join(", ")));
    }
    if new_config.env != config.env {
        out.push(format!("Environment: {}", new_config.env.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if new_config.ports != config.ports {
        out.push(format!("Ports per worktree: {}", new_config.ports));
    }
    if new_config.worktree_by_default != config.worktree_by_default {
        out.push(format!("New threads on a worktree: {}", if new_config.worktree_by_default == Some(true) { "always" } else { "never" }));
    }
    if new_config.default_config != config.default_config {
        out.push("The agent for new threads".into());
    }
    out
}

fn confirm_remove(app: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    let Some(id) = selected(app, cx) else { return };
    let state = app.read(cx).state();
    let Some(project) = state.projects.iter().find(|p| p.id == id).cloned() else { return };
    let threads = state.threads.values().filter(|t| t.project == Some(id)).count();
    let app = app.clone();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (remove_app, archive_app) = (app.clone(), app.clone());
        let threads_text = match threads {
            0 => "It has no threads.".to_owned(),
            1 => "Its thread, with its history, checkpoints and worktree, is deleted.".to_owned(),
            n => format!("Its {n} threads, with their history, checkpoints and worktrees, are deleted."),
        };
        alert
            .title(format!("Remove “{}”?", project.name))
            .description(format!("{threads_text} Files in {} are not touched. Archive it instead to keep its threads.", project.path))
            .confirm()
            .ok_text("Remove")
            .ok_variant(ButtonVariant::Danger)
            .cancel_text("Archive instead")
            .on_cancel(move |_, _, cx| {
                edit(&archive_app, id, cx, |_, c| c.archived = true);
                true
            })
            .on_ok(move |_, window, cx| {
                remove_app.update(cx, |app, cx| app.run(Command::RemoveProject { id }, window, cx));
                true
            })
    });
}

// ── Run targets ──────────────────────────────────────────────────────────

fn targets_list(app: &Entity<AppState>, _: &mut Window, cx: &mut App) -> AnyElement {
    let theme = cx.theme().clone();
    let Some(id) = selected(app, cx) else { return div().into_any_element() };
    let targets = current(app, id, cx).map(|(_, c)| c.targets).unwrap_or_default();
    let add_app = app.clone();
    v_flex()
        .w_full()
        .gap_1()
        .when(targets.is_empty(), |el| {
            el.child(
                div()
                    .py_2()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No targets yet. Add the commands you run while working: a dev server, the tests, the linter."),
            )
        })
        .children(targets.into_iter().enumerate().map(|(ix, target)| {
            let (edit_app, remove_app, edit_target, name) = (app.clone(), app.clone(), target.clone(), target.name.clone());
            h_flex()
                .id(SharedString::from(format!("target-{}", target.name)))
                .w_full()
                .min_h_10()
                .px_2()
                .gap_3()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .child(super::runs::kind_icon(target.kind).small().text_color(theme.muted_foreground))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .py_1p5()
                        .child(h_flex().gap_2().text_sm().child(target.name.clone()).when(
                            // "Test · Tests" says nothing twice.
                            !target.kind.label().to_lowercase().starts_with(&target.name.to_lowercase()),
                            |el| el.child(div().text_xs().text_color(theme.muted_foreground).child(target.kind.label())),
                        ))
                        .child(
                            div()
                                .truncate()
                                .text_xs()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(theme.muted_foreground)
                                .child(target.command.clone()),
                        )
                        .when(!target.url.is_empty(), |el| {
                            el.child(div().truncate().text_xs().text_color(theme.muted_foreground).child(target.url.clone()))
                        }),
                )
                .child(
                    Button::new(("target-edit", ix))
                        .ghost()
                        .xsmall()
                        .label("Edit…")
                        .on_click(move |_, window, cx| edit_target_dialog(&edit_app, id, Some(edit_target.clone()), window, cx)),
                )
                .child(
                    Button::new(("target-remove", ix))
                        .ghost()
                        .xsmall()
                        .icon(Lucide::Trash)
                        .tooltip(format!("Remove {name}"))
                        .on_click(move |_, _, cx| {
                            let name = name.clone();
                            edit(&remove_app, id, cx, move |_, c| c.targets.retain(|t| t.name != name));
                        }),
                )
        }))
        .child(
            h_flex().pt_1().child(
                Button::new("target-add")
                    .outline()
                    .small()
                    .icon(IconName::Plus)
                    .label("Add target…")
                    .on_click(move |_, window, cx| edit_target_dialog(&add_app, id, None, window, cx)),
            ),
        )
        .into_any_element()
}

/// Adds a target (`None`) or edits one.
fn edit_target_dialog(app: &Entity<AppState>, project: ProjectId, target: Option<ProjectTarget>, window: &mut Window, cx: &mut App) {
    let original = target.as_ref().map(|t| t.name.clone());
    let target = target.unwrap_or_default();
    let name = cx.new(|cx| InputState::new(window, cx).placeholder("Dev").default_value(target.name.clone()));
    let command = cx.new(|cx| InputState::new(window, cx).placeholder("npm run dev -- --port $PORT").default_value(target.command.clone()));
    let url = cx.new(|cx| InputState::new(window, cx).placeholder("http://localhost:$PORT").default_value(target.url.clone()));
    let kind = Rc::new(std::cell::Cell::new(target.kind));
    let focus = name.clone();
    window.defer(cx, move |window, cx| focus.update(cx, |input, cx| input.focus(window, cx)));
    let app = app.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme().clone();
        let (name, command, url, kind, app, original) = (name.clone(), command.clone(), url.clone(), kind.clone(), app.clone(), original.clone());
        let field = |label: &'static str, input: AnyElement, help: Option<&'static str>| {
            v_flex()
                .gap_1()
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(label))
                .child(input)
                .when_some(help, |el, help| el.child(div().text_xs().text_color(theme.muted_foreground).child(help)))
        };
        let kind_menu = kind.clone();
        let kind_button = Button::new("target-kind").outline().small().label(kind.get().label()).dropdown_caret(true).dropdown_menu(
            move |mut menu, _, _| {
                for k in TargetKind::ALL {
                    let kind = kind_menu.clone();
                    menu = menu.item(PopupMenuItem::new(k.label()).checked(kind.get() == k).on_click(move |_, window, _| {
                        kind.set(k);
                        window.refresh();
                    }));
                }
                menu
            },
        );
        dialog
            .title(if original.is_some() { "Edit run target" } else { "Add run target" })
            .w(px(520.))
            .child(
                v_flex()
                    .gap_4()
                    .child(field("Name", Input::new(&name).into_any_element(), None))
                    .child(field(
                        "Command",
                        div().font_family(theme.mono_font_family.clone()).child(Input::new(&command)).into_any_element(),
                        Some("Runs through your shell in the thread’s folder, with PORT and the project’s environment."),
                    ))
                    .child(field("Kind", kind_button.into_any_element(), None))
                    .child(field(
                        "Address",
                        Input::new(&url).into_any_element(),
                        Some("Optional. Offered to the preview browser; addresses the command prints are found too."),
                    )),
            )
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("save").primary().label(if original.is_some() { "Save" } else { "Add" }))),
            )
            .on_ok(move |_, _, cx| {
                let target = ProjectTarget {
                    name: name.read(cx).value().trim().to_owned(),
                    command: command.read(cx).value().trim().to_owned(),
                    kind: kind.get(),
                    url: url.read(cx).value().trim().to_owned(),
                };
                if target.name.is_empty() || target.command.is_empty() {
                    return false;
                }
                let original = original.clone();
                edit(&app, project, cx, move |_, c| match original.and_then(|o| c.targets.iter().position(|t| t.name == o)) {
                    Some(ix) => c.targets[ix] = target,
                    None => c.targets.push(target),
                });
                true
            })
    });
}

#[cfg(test)]
mod tests {
    use super::describe_import;
    use orbit_proto::*;

    #[test]
    fn imports_are_described_line_by_line() {
        let project = Project {
            id: ProjectId(1),
            name: "app".into(),
            path: String::new(),
            order: 0,
            scripts: ProjectScripts::default(),
            base_branch: Some("main".into()),
        };
        let mut new = project.clone();
        new.scripts.setup = Some("pnpm i".into());
        let config = ProjectConfig::default();
        let new_config = ProjectConfig {
            targets: vec![ProjectTarget { name: "Dev".into(), command: "pnpm dev".into(), ..Default::default() }],
            ..Default::default()
        };
        assert_eq!(describe_import(&project, &config, &new, &new_config), vec!["Setup: pnpm i", "New target Dev: pnpm dev"]);
        assert!(describe_import(&project, &config, &project, &config).is_empty());
    }
}
