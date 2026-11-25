//! Home: start a thread. One focused task, one primary control.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::actions::*;
use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::composer::{Composer, ComposerEvent};
use crate::shell::{Route, Workspace};
use crate::thread::display_title;
use crate::ui::{Attention, StatusGlyph};
use crate::format;

pub struct HomeView {
    app: Entity<AppState>,
    workspace: WeakEntity<Workspace>,
    composer: Entity<Composer>,
    /// `None` until the user picks; then `Some(None)` means "no project".
    picked: Option<Option<ProjectId>>,
    config_touched: bool,
    sending: bool,
    /// Start the next thread on a new worktree. Follows Settings ›
    /// Workspace until the user picks in the composer.
    worktree: bool,
    worktree_touched: bool,
    _subscriptions: Vec<Subscription>,
}

impl HomeView {
    pub fn new(app: Entity<AppState>, workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let defaults = new_thread_config(&app, cx);
        let composer = cx.new(|cx| Composer::new(app.clone(), defaults, window, cx));
        let subs = vec![
            cx.subscribe_in(&composer, window, |this, _, event: &ComposerEvent, window, cx| match event {
                ComposerEvent::Submit { text, attachments, .. } => this.start(text.clone(), attachments.clone(), window, cx),
                ComposerEvent::ConfigChanged(config) => {
                    this.config_touched = true;
                    let config = config.clone();
                    this.app.update(cx, |app, _| app.send(Command::SetDefaults { config }));
                }
                ComposerEvent::WorktreeChosen(worktree) => {
                    this.worktree = *worktree;
                    this.worktree_touched = true;
                    this.sync_context(cx);
                }
                ComposerEvent::Stop => {}
            }),
            cx.observe(&app, |this, app, cx| {
                // Adopt the last used settings until the user changes them here.
                if !this.config_touched {
                    let defaults = new_thread_config(&app, cx);
                    this.composer.update(cx, |c, cx| c.set_config(defaults, cx));
                }
                this.sync_context(cx);
                cx.notify();
            }),
            cx.observe_global::<crate::settings::SettingsModel>(|this, cx| {
                this.sync_context(cx);
                cx.notify();
            }),
        ];
        let mut home = Self {
            app,
            workspace,
            composer,
            picked: None,
            config_touched: false,
            sending: false,
            worktree: crate::settings::get(cx).workspace.worktree_by_default,
            worktree_touched: false,
            _subscriptions: subs,
        };
        home.sync_context(cx);
        home
    }

    pub fn select_project(&mut self, project: Option<ProjectId>, cx: &mut Context<Self>) {
        self.picked = Some(project);
        self.sync_context(cx);
        cx.notify();
    }

    fn sync_context(&mut self, cx: &mut Context<Self>) {
        if !self.worktree_touched {
            self.worktree = crate::settings::get(cx).workspace.worktree_by_default;
        }
        let project = self.project(cx);
        let state = self.app.read(cx).state();
        let project = project.and_then(|id| state.projects.iter().find(|p| p.id == id));
        let context = Some(project.map(|_| "Current checkout".to_owned()).unwrap_or_else(|| "Scratch folder".into()));
        let branch = project.and_then(|p| p.base_branch.clone());
        // Worktrees need a project; the choice only shows when one is picked.
        let choice = project.map(|_| self.worktree);
        let mentions = project.map(|p| p.id);
        self.composer.update(cx, |c, cx| {
            c.set_context(context, branch, cx);
            c.set_worktree_choice(choice, cx);
            c.set_project(mentions, cx);
        });
    }

    pub fn focus_composer(home: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let composer = home.read(cx).composer.clone();
        Composer::focus(&composer, window, cx);
    }

    /// The picked project, else the one used most recently, else the first.
    fn project(&self, cx: &App) -> Option<ProjectId> {
        if let Some(picked) = self.picked {
            return picked;
        }
        let state = self.app.read(cx).state();
        state
            .threads
            .values()
            .filter(|t| t.project.is_some())
            .max_by_key(|t| t.updated_ms)
            .and_then(|t| t.project)
            .or_else(|| state.projects.first().map(|p| p.id))
    }

    fn start(&mut self, text: String, attachments: Vec<Attachment>, window: &mut Window, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        self.sending = true;
        let project = self.project(cx);
        let config = self.composer.read(cx).config().clone();
        let app = self.app.clone();
        let worktree = self.worktree && project.is_some();
        let create = app.update(cx, |app, cx| app.call(Command::CreateThread { project, config, worktree }, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = create.await;
            let thread = match result {
                Ok(Reply::Thread(id)) => id,
                Ok(_) => return,
                Err(message) => {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.sending = false;
                        // Put the whole message back, attachments too.
                        Composer::restore(&this.composer.clone(), &text, attachments, window, cx);
                        window.push_notification(
                            gpui_kit::component::notification::Notification::error(message).title("Couldn’t start the thread"),
                            cx,
                        );
                    });
                    return;
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.sending = false;
                this.app.update(cx, |app, cx| {
                    app.run(Command::Send { thread, text, attachments, steer: false }, window, cx)
                });
                let _ = this.workspace.update(cx, |ws, cx| ws.navigate(Route::Thread(thread), window, cx));
            });
        })
        .detach();
    }
}

/// The last used model and options, with the access level from settings: a
/// new thread starts at the chosen default, never at full access.
fn new_thread_config(app: &Entity<AppState>, cx: &App) -> ThreadConfig {
    let mut config = app.read(cx).state().defaults.clone();
    config.access = crate::settings::get(cx).agents.default_access.for_new_thread();
    config
}

impl Render for HomeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = self.app.read(cx).state();
        let project_id = self.project(cx);
        let project_name = project_id
            .and_then(|id| state.projects.iter().find(|p| p.id == id))
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "No project".into());
        let projects: Vec<(ProjectId, String)> = state.projects.iter().map(|p| (p.id, p.name.clone())).collect();
        let mut recent: Vec<&Thread> = state.threads.values().filter(|t| !t.archived).collect();
        recent.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
        let recent: Vec<Thread> = recent.into_iter().take(5).cloned().collect();
        let this = cx.weak_entity();
        let now = now_ms();
        let picker = Button::new("home-project")
            .ghost()
            .small()
            .icon(Icon::new(gpui_kit::component::IconName::Folder))
            .label(project_name)
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for (id, name) in &projects {
                    let (this, id) = (this.clone(), *id);
                    menu = menu.item(PopupMenuItem::new(name.clone()).checked(project_id == Some(id)).on_click(
                        move |_, _, cx| {
                            let _ = this.update(cx, |this, cx| this.select_project(Some(id), cx));
                        },
                    ));
                }
                let none = this.clone();
                menu.item(PopupMenuItem::new("No project (scratch folder)").checked(project_id.is_none()).on_click(
                    move |_, _, cx| {
                        let _ = none.update(cx, |this, cx| this.select_project(None, cx));
                    },
                ))
                .separator()
                .item(PopupMenuItem::new("Add project…").icon(Lucide::FolderPlus).on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(AddProject), cx)
                }))
            });
        let shortcut = |label: &'static str, action: &dyn Action| {
            h_flex()
                .gap_1p5()
                .text_xs()
                .text_color(theme.muted_foreground)
                .children(Kbd::binding_for_action(action, None, window))
                .child(label)
        };
        v_flex()
            .size_full()
            .id("home")
            .overflow_y_scroll()
            .items_center()
            .child(
                v_flex()
                    .w_full()
                    .max_w(crate::settings::chat_column(cx))
                    .px_4()
                    .pt(relative(0.16))
                    .pb_8()
                    .gap_4()
                    .child(
                        v_flex()
                            .items_center()
                            .gap_2()
                            .child(Icon::new(Lucide::Orbit).large().text_color(theme.primary))
                            .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("What should we build?"))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("Working in")
                                    .child(picker),
                            ),
                    )
                    .child(self.composer.clone())
                    .child(
                        h_flex()
                            .justify_center()
                            .gap_4()
                            .child(shortcut("Search and commands", &OpenPalette))
                            .child(shortcut("Inbox", &GoInbox))
                            .child(shortcut("Add project", &AddProject)),
                    )
                    .when(!recent.is_empty(), |el| {
                        el.child(
                            v_flex()
                                .mt_6()
                                .gap_0p5()
                                .child(crate::ui::section_label("Recent", cx).px_2())
                                .children(recent.iter().map(|thread| {
                                    let id = thread.id;
                                    let project = thread
                                        .project
                                        .and_then(|p| state.projects.iter().find(|x| x.id == p))
                                        .map(|p| p.name.clone());
                                    h_flex()
                                        .id(("recent", id.0))
                                        .h_9()
                                        .px_2()
                                        .gap_2()
                                        .rounded(theme.radius)
                                        .text_sm()
                                        .cursor_default()
                                        .hover(|s| s.bg(theme.muted))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            let _ = this.workspace.update(cx, |ws, cx| ws.navigate(Route::Thread(id), window, cx));
                                        }))
                                        .child(StatusGlyph::attention(("recent-glyph", id.0), Attention::of(thread)))
                                        .child(div().flex_1().min_w_0().truncate().child(display_title(thread).to_owned()))
                                        .when_some(project, |el, p| {
                                            el.child(div().text_xs().text_color(theme.muted_foreground).child(p))
                                        })
                                        .child(
                                            div()
                                                .w_8()
                                                .text_right()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(format::relative_time(thread.updated_ms, now)),
                                        )
                                })),
                        )
                    }),
            )
    }
}
