//! Run targets: a project's dev servers, tests and linters, run in a
//! thread's workspace.
//!
//! * The thread header's **Run** control starts the first target (or stops
//!   it while it runs); its menu lists every target with its state.
//! * The palette lists `Run Dev`, `Stop Dev`… for the current thread.
//! * The **dock** at the bottom of the thread shows each target's output in
//!   a tab beside the thread's shell, with its state, exit code, address and
//!   Restart / Stop. Targets run in terminals the engine owns, so they keep
//!   running when the dock or the thread closes; a dock opened later shows
//!   the output kept so far.
//! * Addresses a target names or prints go to the preview browser's
//!   suggestions.

use std::collections::HashMap;

use gpui_kit::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;
use orbit_terminal::{TerminalEvent, TerminalView};

use crate::app_state::{AppEvent, AppState};
use crate::assets::Lucide;
use crate::thread::browser::OutputScanner;
use crate::thread::terminal::TerminalPanel;

/// Runs a target of the current thread's project (restarts it if it runs).
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = project, no_json)]
pub struct RunTarget {
    pub name: String,
}

/// Stops a running target of the current thread's project.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = project, no_json)]
pub struct StopTarget {
    pub name: String,
}

gpui_kit::actions!(
    project,
    [
        /// Runs the project's first target, or stops it while it runs.
        RunDefaultTarget,
    ]
);

pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(crate::actions::platform("cmd-shift-r", "ctrl-shift-r"), RunDefaultTarget, Some("Thread"))]);
}

/// The icon of a target's kind.
pub fn kind_icon(kind: TargetKind) -> Icon {
    match kind {
        TargetKind::Dev => Icon::new(IconName::Globe),
        TargetKind::Test => Icon::new(Lucide::FlaskConical),
        TargetKind::Lint => Icon::new(Lucide::ShieldCheck),
        TargetKind::Build => Icon::new(Lucide::Package),
        TargetKind::Other => Icon::new(Lucide::Terminal),
    }
}

/// A run's state in a few words: `Running`, `Exited`, `Failed (exit 1)`.
pub fn status_label(run: &TargetRun) -> String {
    match &run.status {
        TargetStatus::Running => "Running".into(),
        TargetStatus::Exited { code: Some(0) } => "Finished".into(),
        TargetStatus::Exited { code: Some(code) } => format!("Failed (exit {code})"),
        TargetStatus::Exited { code: None } => "Ended".into(),
        TargetStatus::Stopped => "Stopped".into(),
        TargetStatus::Failed { .. } => "Couldn’t start".into(),
    }
}

/// A dot whose meaning the label carries: running, failed, done.
fn status_dot(run: Option<&TargetRun>, cx: &App) -> Div {
    let theme = cx.theme();
    let color = match run.map(|r| &r.status) {
        Some(TargetStatus::Running) => theme.success,
        Some(status) if run.is_some_and(|r| r.status.is_failure()) || matches!(status, TargetStatus::Failed { .. }) => {
            theme.danger
        }
        Some(_) => theme.muted_foreground.opacity(0.6),
        None => theme.transparent,
    };
    div().flex_none().size_1p5().rounded_full().bg(color)
}

/// Starts (or restarts) a target in the thread's workspace.
pub fn run(app: &Entity<AppState>, thread: ThreadId, target: String, window: &mut Window, cx: &mut App) {
    let command = Command::Workspace(WorkspaceCommand::Run { thread, target, cols: 0, rows: 0 });
    app.update(cx, |app, cx| app.run(command, window, cx));
}

pub fn stop(app: &Entity<AppState>, thread: ThreadId, target: String, window: &mut Window, cx: &mut App) {
    let command = Command::Workspace(WorkspaceCommand::Stop { thread, target });
    app.update(cx, |app, cx| app.run(command, window, cx));
}

/// The thread header's Run control, when the thread's project has targets:
/// the first target's Run (or Stop while it runs), and every target in its
/// menu.
pub fn header_control(app: &Entity<AppState>, thread: ThreadId, cx: &App) -> Option<AnyElement> {
    let state = app.read(cx).state();
    let targets = state.targets_of(thread);
    let (first, first_run) = targets.first().map(|(t, r)| (t.clone(), r.cloned()))?;
    let running = targets.iter().filter(|(_, r)| r.is_some_and(|r| r.status.is_running())).count();
    let first_running = first_run.as_ref().is_some_and(|r| r.status.is_running());
    let project = state.threads.get(&thread).and_then(|t| t.project);
    let menu_targets: Vec<(ProjectTarget, Option<TargetRun>)> = targets.iter().map(|(t, r)| (t.clone(), r.cloned())).collect();
    let (run_app, menu_app) = (app.clone(), app.clone());
    let name = first.name.clone();
    let label = if running > 1 { format!("{} · {running} running", first.name) } else { first.name.clone() };
    let button = Button::new(("run-target", thread.0))
        .ghost()
        .small()
        .icon(if first_running { Icon::new(IconName::Square) } else { Icon::new(IconName::Play) })
        .label(label)
        .tooltip_with_action(
            if first_running { format!("Stop {}", first.name) } else { format!("Run {}", first.name) },
            &RunDefaultTarget,
            Some("Thread"),
        )
        .on_click(move |_, window, cx| {
            if first_running {
                stop(&run_app, thread, name.clone(), window, cx);
            } else {
                run(&run_app, thread, name.clone(), window, cx);
            }
        });
    Some(
        DropdownButton::new(("run-targets", thread.0))
            .ghost()
            .small()
            .button(button)
            .menu_accessibility_label("Run targets")
            .dropdown_menu(move |menu, _, cx| target_menu(menu, &menu_app, thread, project, &menu_targets, cx))
            .into_any_element(),
    )
}

fn target_menu(
    mut menu: PopupMenu,
    app: &Entity<AppState>,
    thread: ThreadId,
    project: Option<ProjectId>,
    targets: &[(ProjectTarget, Option<TargetRun>)],
    _: &App,
) -> PopupMenu {
    for (target, last) in targets {
        let running = last.as_ref().is_some_and(|r| r.status.is_running());
        let state = last.as_ref().map(status_label).unwrap_or_else(|| "Not run yet".into());
        let label = format!("{} · {state}", target.name);
        let (app, name) = (app.clone(), target.name.clone());
        menu = menu.item(PopupMenuItem::new(label).icon(kind_icon(target.kind)).on_click(move |_, window, cx| {
            if running {
                stop(&app, thread, name.clone(), window, cx);
            } else {
                run(&app, thread, name.clone(), window, cx);
            }
        }));
    }
    let running: Vec<String> = targets.iter().filter(|(_, r)| r.as_ref().is_some_and(|r| r.status.is_running())).map(|(t, _)| t.name.clone()).collect();
    if !running.is_empty() {
        menu = menu.separator();
        for name in running {
            let (app, target) = (app.clone(), name.clone());
            menu = menu.item(PopupMenuItem::new(format!("Restart {name}")).icon(IconName::RefreshCw).on_click(move |_, window, cx| {
                run(&app, thread, target.clone(), window, cx);
            }));
        }
    }
    menu = menu.separator();
    menu.item(PopupMenuItem::new("Edit targets…").icon(IconName::Settings2).on_click(move |_, window, cx| {
        crate::project::settings::open(project, window, cx);
    }))
}

/// Palette rows for the current thread: `Run Dev`, `Stop Dev`…
pub fn palette_commands(state: &orbit_net::state::ClientState, thread: ThreadId) -> Vec<(String, Icon, Box<dyn Action>)> {
    state
        .targets_of(thread)
        .into_iter()
        .map(|(target, run)| {
            let running = run.is_some_and(|r| r.status.is_running());
            if running {
                (format!("Stop {}", target.name), Icon::new(IconName::Square), Box::new(StopTarget { name: target.name }) as Box<dyn Action>)
            } else {
                (format!("Run {}", target.name), kind_icon(target.kind), Box::new(RunTarget { name: target.name }) as Box<dyn Action>)
            }
        })
        .collect()
}

// ── The dock ─────────────────────────────────────────────────────────────

pub enum RunDockEvent {
    /// A target printed (or names) dev-server addresses.
    Urls(Vec<orbit_browser::Url>),
    /// Show this address in the preview browser.
    Preview(orbit_browser::Url),
    /// The user closed the shell's tab.
    CloseShell,
}

/// Which tab the dock shows.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Active {
    Shell,
    Run(String),
}

struct RunTab {
    name: String,
    term: Option<TermId>,
    view: Entity<TerminalView>,
    scanner: OutputScanner,
    /// Output that arrived while the kept output was being read.
    buffered: Option<Vec<Vec<u8>>>,
    _subscription: Subscription,
}

/// The bottom of a thread: its shell and its run targets, as tabs.
pub struct RunDock {
    app: Entity<AppState>,
    thread: ThreadId,
    shell: Option<Entity<TerminalPanel>>,
    tabs: Vec<RunTab>,
    active: Active,
    /// Closed by the user while targets still run; a new run opens it.
    hidden: bool,
    size: (u16, u16),
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RunDockEvent> for RunDock {}

impl RunDock {
    pub fn new(app: Entity<AppState>, thread: ThreadId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe_in(&app, window, |this, _, event: &AppEvent, window, cx| match event {
            AppEvent::Runs(thread) if *thread == this.thread => this.sync(true, window, cx),
            AppEvent::TerminalOutput { term, data } => this.output(*term, data, cx),
            _ => {}
        });
        let mut dock = Self {
            app,
            thread,
            shell: None,
            tabs: Vec::new(),
            active: Active::Shell,
            hidden: true,
            size: (0, 0),
            _subscriptions: vec![subscription],
        };
        // Targets already running show when the dock opens.
        dock.sync(false, window, cx);
        dock
    }

    /// Whether the dock has anything to show.
    pub fn visible(&self) -> bool {
        self.shell.is_some() || (!self.hidden && !self.tabs.is_empty())
    }

    /// The thread's shell panel opened (`Some`) or closed.
    pub fn set_shell(&mut self, shell: Option<Entity<TerminalPanel>>, cx: &mut Context<Self>) {
        let opened = shell.is_some();
        self.shell = shell;
        self.sync_chrome(cx);
        if opened {
            self.active = Active::Shell;
        } else if self.active == Active::Shell {
            self.active = self.tabs.first().map(|t| Active::Run(t.name.clone())).unwrap_or(Active::Shell);
            if self.tabs.is_empty() {
                self.hidden = true;
            }
        }
        cx.notify();
    }

    /// With target tabs the dock's bar replaces the shell's own header.
    fn sync_chrome(&mut self, cx: &mut Context<Self>) {
        let chrome = self.tabs.is_empty() || self.hidden;
        if let Some(shell) = &self.shell {
            shell.update(cx, |s, cx| s.set_chrome(chrome, cx));
        }
    }

    /// Runs a target by name (the palette and keys).
    pub fn run(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        run(&self.app, self.thread, name.to_owned(), window, cx);
    }

    pub fn stop(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        stop(&self.app, self.thread, name.to_owned(), window, cx);
    }

    /// The header key: run the first target, or stop it while it runs.
    pub fn toggle_default(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let first = self.app.read(cx).state().targets_of(self.thread).into_iter().next().map(|(t, r)| (t.name, r.is_some_and(|r| r.status.is_running())));
        match first {
            Some((name, true)) => self.stop(&name, window, cx),
            Some((name, false)) => self.run(&name, window, cx),
            None => {}
        }
    }

    /// Brings the tabs in line with the thread's runs. `reveal`: a run that
    /// just started opens the dock on its tab.
    fn sync(&mut self, reveal: bool, window: &mut Window, cx: &mut Context<Self>) {
        let runs: Vec<TargetRun> = self
            .app
            .read(cx)
            .state()
            .runs
            .values()
            .filter(|r| r.thread == self.thread)
            .cloned()
            .collect();
        let mut opened = None;
        for run in &runs {
            let existing = self.tabs.iter().position(|t| t.name == run.target);
            match existing {
                Some(ix) if self.tabs[ix].term == run.term || run.term.is_none() => {}
                _ => {
                    // New, or restarted in a new terminal: a fresh screen.
                    let tab = self.new_tab(run, !reveal, window, cx);
                    match existing {
                        Some(ix) => self.tabs[ix] = tab,
                        None => self.tabs.push(tab),
                    }
                    if reveal && run.status.is_running() {
                        opened = Some(run.target.clone());
                    }
                }
            }
            if let Some(url) = run.url.as_deref().and_then(|u| orbit_browser::Url::parse(u).ok()) {
                cx.emit(RunDockEvent::Urls(vec![url]));
            }
        }
        // Keep the order the project lists its targets in.
        let order: HashMap<String, usize> =
            self.app.read(cx).state().targets_of(self.thread).iter().enumerate().map(|(ix, (t, _))| (t.name.clone(), ix)).collect();
        self.tabs.sort_by_key(|t| order.get(&t.name).copied().unwrap_or(usize::MAX));
        if let Some(name) = opened {
            self.active = Active::Run(name);
            self.hidden = false;
        }
        self.sync_chrome(cx);
        cx.notify();
    }

    fn new_tab(&mut self, run: &TargetRun, replay: bool, window: &mut Window, cx: &mut Context<Self>) -> RunTab {
        let view = cx.new(|cx| TerminalView::new(window, cx));
        let name = run.target.clone();
        let subscription = cx.subscribe(&view, {
            let name = name.clone();
            move |this, _, event: &TerminalEvent, cx| this.on_view_event(&name, event, cx)
        });
        let mut tab = RunTab { name, term: run.term, view, scanner: OutputScanner::default(), buffered: None, _subscription: subscription };
        if let (true, Some(term)) = (replay, run.term) {
            // Started before this view existed: show what it printed so far.
            tab.buffered = Some(Vec::new());
            let task = self.app.update(cx, |app, cx| app.call(Command::ReadTerminal { term, max_bytes: 0 }, cx));
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    let Some(tab) = this.tabs.iter_mut().find(|t| t.term == Some(term)) else { return };
                    let buffered = tab.buffered.take().unwrap_or_default();
                    let data = match result {
                        Ok(Reply::TerminalTail { data, .. }) => data,
                        // Gone: show whatever arrived meanwhile.
                        _ => buffered.concat(),
                    };
                    tab.view.update(cx, |view, cx| view.feed(&data, cx));
                    let urls = tab.scanner.feed(&data);
                    if !urls.is_empty() {
                        cx.emit(RunDockEvent::Urls(urls));
                    }
                });
            })
            .detach();
        }
        tab
    }

    fn output(&mut self, term: TermId, data: &[u8], cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.term == Some(term)) else { return };
        if let Some(buffer) = &mut tab.buffered {
            buffer.push(data.to_vec());
            return;
        }
        tab.view.update(cx, |view, cx| view.feed(data, cx));
        let urls = tab.scanner.feed(data);
        if !urls.is_empty() {
            cx.emit(RunDockEvent::Urls(urls));
        }
    }

    fn on_view_event(&mut self, name: &str, event: &TerminalEvent, cx: &mut Context<Self>) {
        let Some(term) = self.tabs.iter().find(|t| t.name == name).and_then(|t| t.term) else { return };
        let running = self.run_of(name, cx).is_some_and(|r| r.status.is_running());
        match event {
            // The engine answers cursor queries for target terminals; the
            // view's own answer would reach the program as typed text.
            TerminalEvent::Input(data) if running && !is_cursor_report(data) => {
                self.app.update(cx, |app, _| app.send(Command::TerminalInput { term, data: data.clone() }));
            }
            TerminalEvent::Resize { cols, rows } => {
                self.size = (*cols, *rows);
                if running {
                    self.app.update(cx, |app, _| app.send(Command::ResizeTerminal { term, cols: *cols, rows: *rows }));
                }
            }
            _ => {}
        }
    }

    fn run_of(&self, name: &str, cx: &App) -> Option<TargetRun> {
        self.app.read(cx).state().runs.get(&(self.thread, name.to_owned())).cloned()
    }

    fn close_tab(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.run_of(name, cx).is_some_and(|r| r.status.is_running()) {
            self.stop(name, window, cx);
        }
        self.tabs.retain(|t| t.name != name);
        if self.active == Active::Run(name.to_owned()) {
            self.active = match (&self.shell, self.tabs.first()) {
                (Some(_), _) => Active::Shell,
                (None, Some(tab)) => Active::Run(tab.name.clone()),
                (None, None) => Active::Shell,
            };
        }
        if self.tabs.is_empty() && self.shell.is_none() {
            self.hidden = true;
        }
        self.sync_chrome(cx);
        cx.notify();
    }

    fn render_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        let Active::Run(name) = &self.active else {
            let ended = self.shell.as_ref().is_some_and(|s| s.read(cx).ended());
            let shell = self.shell.clone();
            return h_flex()
                .gap_1()
                .when_some(shell.filter(|_| ended), |el, shell| {
                    el.child(
                        Button::new("dock-restart-shell")
                            .ghost()
                            .xsmall()
                            .icon(IconName::RefreshCw)
                            .label("Restart")
                            .on_click(move |_, window, cx| shell.update(cx, |s, cx| s.restart(window, cx))),
                    )
                })
                .child(Button::new("dock-close-shell")
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .tooltip("Close terminal (Ctrl+`)")
                .on_click(cx.listener(|_, _, _, cx| cx.emit(RunDockEvent::CloseShell))))
                .into_any_element();
        };
        let run = self.run_of(name, cx);
        let running = run.as_ref().is_some_and(|r| r.status.is_running());
        let url = run.as_ref().and_then(|r| r.url.clone()).and_then(|u| orbit_browser::Url::parse(&u).ok());
        let (restart, stop_name, close_name) = (name.clone(), name.clone(), name.clone());
        h_flex()
            .gap_1()
            .when_some(url, |el, url| {
                let host = orbit_browser::host_label(&url);
                el.child(
                    Button::new("dock-preview")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Globe)
                        .label(host)
                        .tooltip(format!("Show {url} in the preview browser"))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(RunDockEvent::Preview(url.clone())))),
                )
            })
            .child(
                Button::new("dock-restart")
                    .ghost()
                    .xsmall()
                    .icon(IconName::RefreshCw)
                    .label(if running { "Restart" } else { "Run again" })
                    .on_click(cx.listener(move |this, _, window, cx| this.run(&restart, window, cx))),
            )
            .when(running, |el| {
                el.child(
                    Button::new("dock-stop")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Square)
                        .label("Stop")
                        .on_click(cx.listener(move |this, _, window, cx| this.stop(&stop_name, window, cx))),
                )
            })
            .child(
                Button::new("dock-close-tab")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip(if running { "Stop and close" } else { "Close" })
                    .on_click(cx.listener(move |this, _, window, cx| this.close_tab(&close_name, window, cx))),
            )
            .into_any_element()
    }
}

/// `ESC [ row ; col R`: a terminal's answer to a cursor-position query.
fn is_cursor_report(data: &[u8]) -> bool {
    let Some(body) = data.strip_prefix(b"\x1b[").and_then(|b| b.strip_suffix(b"R")) else { return false };
    !body.is_empty() && body.iter().all(|b| b.is_ascii_digit() || *b == b';')
}

impl Render for RunDock {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // With no targets, the shell is the whole dock, with its own header.
        if self.tabs.is_empty() || self.hidden {
            return match &self.shell {
                Some(shell) => div().size_full().child(shell.clone()).into_any_element(),
                None => div().into_any_element(),
            };
        }
        let state = self.app.read(cx).state();
        let mut labels: Vec<(Active, Tab)> = Vec::new();
        if self.shell.is_some() {
            labels.push((Active::Shell, Tab::new().label("Terminal").prefix(Icon::new(Lucide::Terminal).xsmall().mr_1())));
        }
        for tab in &self.tabs {
            let run = state.runs.get(&(self.thread, tab.name.clone()));
            let tip = run.map(status_label).unwrap_or_default();
            labels.push((
                Active::Run(tab.name.clone()),
                Tab::new().label(tab.name.clone()).aria_label(format!("{} · {tip}", tab.name)).prefix(status_dot(run, cx).mr_1p5()),
            ));
        }
        let selected = labels.iter().position(|(a, _)| *a == self.active).unwrap_or(0);
        let keys: Vec<Active> = labels.iter().map(|(a, _)| a.clone()).collect();
        let view = cx.entity().downgrade();
        let status = match &self.active {
            Active::Run(name) => state.runs.get(&(self.thread, name.clone())).map(|run| {
                let mut text = status_label(run);
                if let TargetStatus::Failed { message } = &run.status {
                    text = message.clone();
                }
                (text, run.status.is_failure())
            }),
            Active::Shell => None,
        };
        let body: AnyElement = match &self.active {
            Active::Shell => match &self.shell {
                Some(shell) => shell.clone().into_any_element(),
                None => div().into_any_element(),
            },
            Active::Run(name) => match self.tabs.iter().find(|t| &t.name == name) {
                Some(tab) => div().id("run-body").size_full().px_2().pt_1().child(tab.view.clone()).into_any_element(),
                None => div().into_any_element(),
            },
        };
        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                h_flex()
                    .h_8()
                    .flex_none()
                    .pl_1()
                    .pr_2()
                    .gap_2()
                    .border_t_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        TabBar::new("run-dock-tabs")
                            .xsmall()
                            .underline()
                            .children(labels.into_iter().map(|(_, tab)| tab))
                            .selected_index(selected)
                            .on_click(move |ix, _, cx| {
                                let Some(active) = keys.get(*ix).cloned() else { return };
                                let _ = view.update(cx, |this, cx| {
                                    this.active = active;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(div().flex_1())
                    .when_some(status, |el, (text, failed)| {
                        el.child(
                            div()
                                .text_xs()
                                .truncate()
                                .text_color(if failed { theme.danger } else { theme.muted_foreground })
                                .child(text),
                        )
                    })
                    .child(self.render_actions(cx)),
            )
            .child(div().flex_1().min_h_0().child(body))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{is_cursor_report, status_label};
    use orbit_proto::{TargetRun, TargetStatus, ThreadId};

    #[test]
    fn cursor_reports_are_recognized() {
        assert!(is_cursor_report(b"\x1b[1;1R"));
        assert!(is_cursor_report(b"\x1b[24;80R"));
        assert!(!is_cursor_report(b"R"));
        assert!(!is_cursor_report(b"\x1b[AR"));
        assert!(!is_cursor_report(b"q"));
    }

    #[test]
    fn run_states_read_as_words() {
        let run = |status| TargetRun { thread: ThreadId(1), target: "Dev".into(), term: None, status, url: None, started_ms: 0, ended_ms: None };
        assert_eq!(status_label(&run(TargetStatus::Running)), "Running");
        assert_eq!(status_label(&run(TargetStatus::Exited { code: Some(0) })), "Finished");
        assert_eq!(status_label(&run(TargetStatus::Exited { code: Some(2) })), "Failed (exit 2)");
        assert_eq!(status_label(&run(TargetStatus::Stopped)), "Stopped");
    }
}
