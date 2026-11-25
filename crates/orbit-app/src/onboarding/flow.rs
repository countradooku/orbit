//! The first-run guide: welcome, agents, appearance, first project, first
//! task. One column in the middle of the window; Enter goes on, Escape goes
//! back, every control is a tab stop, and each step fades in (instantly with
//! reduced motion).

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::stepper::{Stepper, StepperItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::sample::{self, ProjectKind};
use super::{OnboardingBack, OnboardingContinue};
use crate::app_state::AppState;
use crate::assets::Lucide;

const STEP_ENTER: Duration = Duration::from_millis(260);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Welcome,
    Agents,
    Appearance,
    Project,
    FirstTask,
}

impl Step {
    const ALL: [Step; 5] = [Step::Welcome, Step::Agents, Step::Appearance, Step::Project, Step::FirstTask];

    fn index(self) -> usize {
        self as usize
    }

    fn next(self) -> Option<Step> {
        Step::ALL.get(self.index() + 1).copied()
    }

    fn previous(self) -> Option<Step> {
        self.index().checked_sub(1).map(|ix| Step::ALL[ix])
    }
}

/// A clone in progress or failed.
enum Clone {
    Idle,
    Running { progress: String, _task: Task<()> },
    Failed(String),
}

pub struct OnboardingView {
    app: Entity<AppState>,
    step: Step,
    /// Bumped on every step change, so the entrance animation replays.
    generation: u64,
    focus: FocusHandle,
    clone_url: Entity<InputState>,
    clone_parent: PathBuf,
    clone: Clone,
    adding: bool,
    project: Option<ProjectId>,
    project_error: Option<String>,
    prompt: Entity<TextareaState>,
    /// The project the prompt was chosen for, so a new project gets a new one
    /// unless the user edited it.
    prompt_for: Option<Option<ProjectId>>,
    prompt_edited: bool,
    /// (kind, instance id) of the agent for the first task.
    agent: Option<(ProviderKind, String)>,
    starting: bool,
    _subscriptions: Vec<Subscription>,
}

impl OnboardingView {
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let clone_url = cx.new(|cx| InputState::new(window, cx).placeholder("https://github.com/owner/repository"));
        let prompt = cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 8));
        crate::voice::register_field(crate::voice::Field::textarea(&prompt), cx);
        let subscriptions = vec![
            cx.subscribe_in(&clone_url, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.start_clone(window, cx),
                InputEvent::Change => {
                    if matches!(this.clone, Clone::Failed(_)) {
                        this.clone = Clone::Idle;
                        cx.notify();
                    }
                }
                _ => {}
            }),
            cx.subscribe_in(&prompt, window, |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => this.prompt_edited = true,
                InputEvent::PressEnter { secondary: true, .. } => this.start_task(window, cx),
                _ => {}
            }),
            cx.observe(&app, |this, _, cx| {
                this.pick_default_agent(cx);
                cx.notify();
            }),
        ];
        // Pick up where the data folder is: a project added before (the
        // guide reopened from Settings) is the one to try a task in.
        let project = app.read(cx).state().projects.first().map(|p| p.id);
        // Debug builds: ORBIT_DEBUG_ONBOARDING=<step number> opens there, for
        // screenshots and visual checks.
        let step = if cfg!(debug_assertions) {
            std::env::var("ORBIT_DEBUG_ONBOARDING").ok().and_then(|s| s.parse::<usize>().ok()).and_then(|ix| Step::ALL.get(ix).copied())
        } else {
            None
        };
        let mut view = Self {
            app,
            step: step.unwrap_or(Step::Welcome),
            generation: 0,
            focus: cx.focus_handle(),
            clone_url,
            clone_parent: default_clone_parent(),
            clone: Clone::Idle,
            adding: false,
            project,
            project_error: None,
            prompt,
            prompt_for: None,
            prompt_edited: false,
            agent: None,
            starting: false,
            _subscriptions: subscriptions,
        };
        view.pick_default_agent(cx);
        if view.step == Step::FirstTask {
            view.prepare_prompt(window, cx);
        }
        view
    }

    // ── Navigation ───────────────────────────────────────────────────────

    fn go(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        if step == self.step {
            return;
        }
        self.step = step;
        self.generation += 1;
        match step {
            Step::Project if self.project.is_none() => {
                let url = self.clone_url.clone();
                window.defer(cx, move |window, cx| url.update(cx, |input, cx| input.focus(window, cx)));
            }
            Step::FirstTask => {
                self.prepare_prompt(window, cx);
                let prompt = self.prompt.clone();
                window.defer(cx, move |window, cx| prompt.update(cx, |input, cx| input.focus(window, cx)));
            }
            _ => self.focus.focus(window, cx),
        }
        cx.notify();
    }

    fn continue_(&mut self, _: &OnboardingContinue, window: &mut Window, cx: &mut Context<Self>) {
        // A focused button takes Enter itself (it clicks once).
        if !self.focus.is_focused(window) {
            cx.propagate();
            return;
        }
        self.advance(window, cx);
    }

    fn advance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.step.next() {
            Some(next) => self.go(next, window, cx),
            None => self.start_task(window, cx),
        }
    }

    fn back(&mut self, _: &OnboardingBack, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.step.previous() {
            self.go(previous, window, cx);
        }
    }

    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        super::close(window, cx);
        window.dispatch_action(Box::new(crate::actions::FocusComposer), cx);
    }

    // ── Agents ───────────────────────────────────────────────────────────

    /// The built-in agents, one per runtime (ACP agents are set up in
    /// Settings).
    fn agents(&self, cx: &App) -> Vec<ProviderInfo> {
        let state = self.app.read(cx).state();
        [ProviderKind::Claude, ProviderKind::Codex, ProviderKind::OpenCode, ProviderKind::Pi]
            .into_iter()
            .map(|kind| {
                state
                    .providers
                    .iter()
                    .find(|p| p.kind == kind && p.instance_id() == kind.id())
                    .cloned()
                    .unwrap_or_else(|| ProviderInfo { kind, message: Some("Checking…".into()), ..Default::default() })
            })
            .collect()
    }

    fn ready_agents(&self, cx: &App) -> Vec<ProviderInfo> {
        let state = self.app.read(cx).state();
        state.providers.iter().filter(|p| p.installed && p.signed_in != Some(false) && p.kind != ProviderKind::Acp).cloned().collect()
    }

    fn pick_default_agent(&mut self, cx: &mut Context<Self>) {
        let ready = self.ready_agents(cx);
        let still_ready = self.agent.as_ref().is_some_and(|(k, i)| ready.iter().any(|p| p.kind == *k && p.instance_id() == i));
        if still_ready {
            return;
        }
        // The engine's default agent if it is ready, else the first ready.
        let defaults = self.app.read(cx).state().defaults.clone();
        self.agent = ready
            .iter()
            .find(|p| p.serves(&defaults))
            .or_else(|| ready.first())
            .map(|p| (p.kind, p.instance_id().to_owned()));
    }

    fn check_agents(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.app.update(cx, |app, cx| app.run(Command::RefreshProviders, window, cx));
    }

    // ── Project ──────────────────────────────────────────────────────────

    fn project_info(&self, cx: &App) -> Option<Project> {
        let id = self.project?;
        self.app.read(cx).state().projects.iter().find(|p| p.id == id).cloned()
    }

    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add project".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| this.add_project(path, window, cx));
        })
        .detach();
    }

    fn add_project(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.adding = true;
        self.project_error = None;
        cx.notify();
        let task = self.app.update(cx, |app, cx| app.call(Command::AddProject { path: path.display().to_string() }, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.adding = false;
                match result {
                    Ok(Reply::Project(id)) => {
                        this.project = Some(id);
                        this.clone = Clone::Idle;
                        this.focus.focus(window, cx);
                    }
                    Ok(_) => {}
                    Err(message) => this.project_error = Some(message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn choose_clone_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Clone here".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |this, cx| {
                this.clone_parent = path;
                cx.notify();
            });
        })
        .detach();
    }

    /// Clones with the user's own git, on this computer, then adds the
    /// folder as a project. Progress is git's own last line.
    fn start_clone(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.clone, Clone::Running { .. }) {
            return;
        }
        let text = self.clone_url.read(cx).value().to_string();
        let target = match sample::parse_clone_url(&text) {
            Ok(target) => target,
            Err(message) => {
                self.clone = Clone::Failed(message);
                cx.notify();
                return;
            }
        };
        let dest = self.clone_parent.join(&target.folder);
        if dest.exists() {
            self.clone = Clone::Failed(format!(
                "{} already exists. Choose another location, or add that folder instead.",
                dest.display()
            ));
            cx.notify();
            return;
        }
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<String>();
        let (url, parent, job_dest) = (target.url.clone(), self.clone_parent.clone(), dest.clone());
        let run = crate::update::download::on_thread("orbit-clone", move || {
            use std::io::Read as _;
            std::fs::create_dir_all(&parent).map_err(|e| format!("Couldn’t create {}: {e}", parent.display()))?;
            let mut command = std::process::Command::new("git");
            command
                .args(["clone", "--progress", "--"])
                .arg(&url)
                .arg(&job_dest)
                .current_dir(&parent)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                // Never stop to ask for a password in a terminal nobody sees.
                .env("GIT_TERMINAL_PROMPT", "0");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt as _;
                command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            let mut child = command.spawn().map_err(|e| format!("Couldn’t run git: {e}. Install git and try again."))?;
            let mut stderr = child.stderr.take().ok_or("git gave no output")?;
            let mut all = String::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stderr.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                all.push_str(&String::from_utf8_lossy(&buf[..n]));
                if let Some(line) = sample::clone_progress(&all) {
                    let _ = tx.unbounded_send(line);
                }
                // Keep the tail only: progress output can be long.
                if all.len() > 64 * 1024 {
                    all = all.split_off(all.len() - 8 * 1024);
                }
            }
            let status = child.wait().map_err(|e| format!("git failed: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                let last = all.lines().rev().find(|l| l.contains("fatal:") || l.contains("error:")).unwrap_or("git clone failed.");
                Err(last.trim().trim_start_matches("fatal: ").to_owned())
            }
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let progress = async {
                use futures::StreamExt as _;
                while let Some(line) = rx.next().await {
                    let _ = this.update(cx, |this, cx| {
                        if let Clone::Running { progress, .. } = &mut this.clone {
                            *progress = line;
                            cx.notify();
                        }
                    });
                }
            };
            let (result, ()) = futures::join!(run, progress);
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(()) => this.add_project(dest, window, cx),
                Err(message) => {
                    this.clone = Clone::Failed(message);
                    cx.notify();
                }
            });
        });
        self.clone = Clone::Running { progress: format!("Cloning {}…", target.folder), _task: task };
        cx.notify();
    }

    // ── First task ───────────────────────────────────────────────────────

    fn prepare_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let for_project = Some(self.project);
        if self.prompt_for == for_project && !self.prompt.read(cx).value().is_empty() {
            return;
        }
        if self.prompt_edited && self.prompt_for.is_some() {
            return;
        }
        let folder = self.project_info(cx).map(|p| PathBuf::from(p.path));
        let kind = ProjectKind::detect(folder.as_deref());
        let text = sample::sample_prompt(kind);
        self.prompt.update(cx, |input, cx| input.set_value(text, window, cx));
        self.prompt_for = for_project;
        self.prompt_edited = false;
    }

    fn start_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.starting || self.step != Step::FirstTask {
            return;
        }
        let Some((kind, instance)) = self.agent.clone() else { return };
        let prompt = self.prompt.read(cx).value().trim().to_owned();
        if prompt.is_empty() {
            return;
        }
        let defaults = self.app.read(cx).state().defaults.clone();
        let mut config = if defaults.provider == kind && defaults.instance_id() == instance {
            defaults
        } else {
            ThreadConfig { provider: kind, instance: if instance == kind.id() { String::new() } else { instance }, ..Default::default() }
        };
        config.access = crate::settings::get(cx).agents.default_access.for_new_thread();
        let start = ThreadStart {
            project: self.project,
            config,
            worktree: false,
            parent: None,
            title: String::new(),
            prompt: Some(prompt),
        };
        self.starting = true;
        cx.notify();
        let task = self.app.update(cx, |app, cx| app.call(Command::StartThread(start), cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.starting = false;
                match result {
                    Ok(Reply::Thread(thread)) => {
                        super::close(window, cx);
                        super::open_thread(thread, window, cx);
                    }
                    Ok(_) => this.finish(window, cx),
                    Err(message) => {
                        window.push_notification(
                            gpui_kit::component::notification::Notification::error(message).title("Couldn’t start the task"),
                            cx,
                        );
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    // ── Rendering ────────────────────────────────────────────────────────

    fn render_stepper(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        let labels = ["Agents", "Appearance", "Project", "First task"];
        Stepper::new("onboarding-stepper")
            .small()
            .w_full()
            .selected_index(self.step.index().saturating_sub(1))
            .items(labels.iter().map(|label| StepperItem::new().child(*label)))
            .on_click(move |ix, window, cx| {
                let step = Step::ALL[(*ix + 1).min(Step::ALL.len() - 1)];
                let _ = this.update(cx, |this, cx| this.go(step, window, cx));
            })
    }

    fn heading(&self, title: impl Into<SharedString>, help: impl Into<SharedString>, cx: &App) -> impl IntoElement {
        v_flex()
            .gap_1p5()
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(title.into()))
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(help.into()))
    }

    fn render_welcome(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let point = |icon: Icon, title: &'static str, text: &'static str| {
            h_flex()
                .gap_3()
                .items_start()
                .child(div().mt_0p5().child(icon.small().text_color(theme.primary)))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                        .child(div().text_sm().text_color(theme.muted_foreground).child(text)),
                )
        };
        v_flex()
            .items_center()
            .gap_6()
            .child(
                div()
                    .size_16()
                    .rounded(theme.radius_lg)
                    .bg(theme.primary.opacity(0.1))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(Lucide::Orbit).size_8().text_color(theme.primary))
                    .with_animation(
                        "onboarding-orbit",
                        Animation::new(Duration::from_millis(520)).with_easing(ease_out_quint()),
                        |el, t| el.opacity(t).mt(rems(-0.75 * (1.0 - t))),
                    ),
            )
            .child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("Welcome to Orbit"))
                    .child(
                        div()
                            .max_w(rems(28.))
                            .text_center()
                            .text_color(theme.muted_foreground)
                            .child("Run coding agents side by side, see what they change, and ship the best of it. Setup takes about a minute."),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .max_w(rems(26.))
                    .gap_4()
                    .p_5()
                    .rounded(theme.radius_lg)
                    .bg(theme.muted.opacity(0.5))
                    .child(point(Icon::new(IconName::Bot), "Your agents, your sign-in", "Claude Code, Codex, OpenCode and Pi run as you, with your own accounts."))
                    .child(point(Icon::new(Lucide::FileDiff), "Every change in review", "Keep or revert each part of what an agent wrote before it reaches your branch."))
                    .child(point(Icon::new(Lucide::Columns2), "Races", "Send one prompt to several agents and keep the best result.")),
            )
            .into_any_element()
    }

    fn render_agents(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let agents = self.agents(cx);
        let ready = agents.iter().filter(|a| a.installed && a.signed_in != Some(false)).count();
        let checking = agents.iter().any(|a| a.message.as_deref() == Some("Checking…"));
        let summary = match (checking, ready) {
            (true, _) => "Looking for agents on this computer…".to_owned(),
            (false, 0) => "No agent is ready yet. Install one below; Orbit finds it as soon as it’s on your PATH.".to_owned(),
            (false, 1) => "One agent is ready. You can add more any time.".to_owned(),
            (false, n) => format!("{n} agents are ready. Races can pit them against each other."),
        };
        let rows = agents.into_iter().map(|info| {
            let kind = info.kind;
            let checking = info.message.as_deref() == Some("Checking…");
            let (status, color) = match (info.installed, info.signed_in) {
                _ if checking => ("Checking", theme.muted_foreground),
                (false, _) => ("Not installed", theme.muted_foreground),
                (true, Some(false)) => ("Signed out", theme.warning),
                (true, _) => ("Ready", theme.success),
            };
            let mut facts = Vec::new();
            if let Some(version) = &info.version {
                facts.push(version.clone());
            }
            if let Some(account) = &info.account {
                facts.push(account.clone());
            }
            let steps = sample::fix_steps(kind);
            let needs_install = !checking && !info.installed;
            let needs_sign_in = !checking && info.installed && info.signed_in == Some(false);
            v_flex()
                .gap_2()
                .p_3()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .gap_3()
                        .child(crate::ui::provider_icon(kind, cx))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(kind.label()))
                                .when(!facts.is_empty(), |el| {
                                    el.child(div().text_xs().text_color(theme.muted_foreground).truncate().child(facts.join(" · ")))
                                }),
                        )
                        .child(
                            h_flex()
                                .gap_1p5()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .when(checking, |el| el.child(Spinner::new().xsmall()))
                                .when(!checking, |el| el.child(div().size_2().rounded_full().bg(color)))
                                .child(status),
                        ),
                )
                .when_some(steps.filter(|_| needs_install || needs_sign_in), |el, steps| {
                    let command = |id: SharedString, number: usize, label: &'static str, cmd: &'static str, note: Option<&'static str>| {
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .child(div().w_4().text_color(theme.muted_foreground).child(format!("{number}.")))
                            .child(div().w_12().text_color(theme.muted_foreground).child(label))
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .px_2()
                                    .py_0p5()
                                    .rounded(theme.radius)
                                    .bg(theme.muted)
                                    .child(div().flex_1().min_w_0().truncate().font_family(theme.mono_font_family.clone()).child(cmd))
                                    .child(Clipboard::new(id).value(cmd).tooltip("Copy")),
                            )
                            .when_some(note, |el, note| el.child(div().text_color(theme.muted_foreground).child(note)))
                    };
                    let id = |part: &str| SharedString::from(format!("copy-{}-{part}", kind.id()));
                    el.when(needs_install, |el| {
                        el.child(command(id("install"), 1, "Install", steps.install, None))
                            .child(command(id("sign-in"), 2, "Sign in", steps.sign_in, steps.sign_in_note))
                    })
                    .when(needs_sign_in, |el| el.child(command(id("sign-in"), 1, "Sign in", steps.sign_in, steps.sign_in_note)))
                })
        });
        v_flex()
            .gap_4()
            .child(self.heading("Your agents", summary, cx))
            .child(v_flex().gap_2().children(rows))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Button::new("onboarding-check-agents")
                            .outline()
                            .small()
                            .icon(IconName::RefreshCw)
                            .label("Check again")
                            .on_click(cx.listener(|this, _, window, cx| this.check_agents(window, cx))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Installing needs Node.js 18 or later. Orbit never installs anything itself."),
                    ),
            )
            .into_any_element()
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let appearance = crate::settings::get(cx).appearance.clone();
        let dark = theme.mode.is_dark();
        let modes = [(ColorMode::System, "System", Icon::new(Lucide::Monitor)), (ColorMode::Light, "Light", Icon::new(IconName::Sun)), (ColorMode::Dark, "Dark", Icon::new(IconName::Moon))];
        let mode_group = ButtonGroup::new("onboarding-mode")
            .outline()
            .children(modes.iter().enumerate().map(|(ix, (mode, label, icon))| {
                Button::new(("onboarding-mode", ix)).icon(icon.clone()).label(*label).selected(appearance.mode == *mode)
            }))
            .on_click(move |picked: &Vec<usize>, _, cx| {
                if let Some(&ix) = picked.first() {
                    let mode = [ColorMode::System, ColorMode::Light, ColorMode::Dark][ix.min(2)];
                    crate::settings::update(cx, |s| s.appearance.mode = mode);
                }
            });
        let current = crate::theme::resolve(if dark { &appearance.dark_theme } else { &appearance.light_theme }, dark, cx);
        let names = crate::theme::names(dark, cx);
        let theme_picker = Button::new("onboarding-theme")
            .outline()
            .icon(IconName::Palette)
            .label(current.clone())
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for name in &names {
                    let picked = name.clone();
                    menu = menu.item(PopupMenuItem::new(name.clone()).checked(*name == current).on_click(move |_, _, cx| {
                        let orbit = if dark { crate::theme::ORBIT_DARK } else { crate::theme::ORBIT_LIGHT };
                        let value = if picked.as_ref() == orbit { String::new() } else { picked.to_string() };
                        crate::settings::update(cx, |s| {
                            if dark {
                                s.appearance.dark_theme = value;
                            } else {
                                s.appearance.light_theme = value;
                            }
                        });
                    }));
                }
                menu
            });
        let sizes = [(90u16, "Compact"), (100, "Default"), (110, "Large"), (125, "Larger")];
        let size_group = ButtonGroup::new("onboarding-size")
            .outline()
            .children(sizes.iter().enumerate().map(|(ix, (scale, label))| {
                Button::new(("onboarding-size", ix)).label(*label).selected(appearance.ui_scale == *scale)
            }))
            .on_click(move |picked: &Vec<usize>, _, cx| {
                if let Some(&ix) = picked.first() {
                    let scale = sizes[ix.min(sizes.len() - 1)].0;
                    crate::settings::update(cx, |s| s.appearance.ui_scale = scale);
                }
            });
        let row = |label: &'static str, control: AnyElement| {
            h_flex()
                .gap_4()
                .child(div().w_24().flex_shrink_0().text_sm().text_color(theme.muted_foreground).child(label))
                .child(control)
        };
        v_flex()
            .gap_5()
            .child(self.heading("Make it yours", "Changes apply as you pick them. Settings › Appearance has more.", cx))
            .child(preview(cx))
            .child(
                v_flex()
                    .gap_3()
                    .child(row("Appearance", mode_group.into_any_element()))
                    .child(row(if dark { "Dark theme" } else { "Light theme" }, theme_picker.into_any_element()))
                    .child(row("Text size", size_group.into_any_element())),
            )
            .into_any_element()
    }

    fn render_project(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let heading = self.heading(
            "Add your first project",
            "A project is a folder (usually a git repository) that agents work in. You can add more from the sidebar.",
            cx,
        );
        if let Some(project) = self.project_info(cx) {
            return v_flex()
                .gap_5()
                .child(heading)
                .child(
                    h_flex()
                        .gap_3()
                        .p_3()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .child(Icon::new(IconName::CircleCheck).text_color(theme.success))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(project.name.clone()))
                                .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(project.path.clone())),
                        )
                        .child(
                            Button::new("onboarding-other-project")
                                .ghost()
                                .small()
                                .label("Choose another…")
                                .on_click(cx.listener(|this, _, window, cx| this.choose_folder(window, cx))),
                        ),
                )
                .into_any_element();
        }
        let option = |icon: Icon, title: &'static str, help: &'static str| {
            h_flex()
                .gap_2()
                .child(icon.small().text_color(theme.muted_foreground))
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                .child(div().text_xs().text_color(theme.muted_foreground).child(help))
        };
        let cloning = matches!(self.clone, Clone::Running { .. });
        let parent = self.clone_parent.display().to_string();
        v_flex()
            .gap_5()
            .child(heading)
            .child(
                v_flex()
                    .gap_3()
                    .p_4()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(option(Icon::new(IconName::Folder), "Open a folder", "already on this computer"))
                    .child(
                        h_flex().child(
                            Button::new("onboarding-choose-folder")
                                .icon(IconName::FolderOpen)
                                .label("Choose folder…")
                                .loading(self.adding)
                                .disabled(self.adding || cloning)
                                .on_click(cx.listener(|this, _, window, cx| this.choose_folder(window, cx))),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .gap_3()
                    .p_4()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(option(Icon::new(Lucide::GitFork), "Clone a repository", "with your own git and credentials"))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&self.clone_url).disabled(cloning)))
                            .child(
                                Button::new("onboarding-clone")
                                    .label("Clone")
                                    .loading(cloning)
                                    .disabled(cloning || self.adding)
                                    .on_click(cx.listener(|this, _, window, cx| this.start_clone(window, cx))),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Into")
                            .child(div().min_w_0().truncate().font_family(theme.mono_font_family.clone()).child(parent))
                            .child(
                                Button::new("onboarding-clone-parent")
                                    .ghost()
                                    .xsmall()
                                    .label("Change…")
                                    .disabled(cloning)
                                    .on_click(cx.listener(|this, _, window, cx| this.choose_clone_parent(window, cx))),
                            ),
                    )
                    .map(|el| match &self.clone {
                        Clone::Idle => el,
                        Clone::Running { progress, .. } => el.child(
                            h_flex()
                                .gap_2()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(Spinner::new().xsmall())
                                .child(div().min_w_0().truncate().font_family(theme.mono_font_family.clone()).child(progress.clone())),
                        ),
                        Clone::Failed(message) => el.child(div().text_xs().text_color(theme.danger).child(message.clone())),
                    }),
            )
            .when_some(self.project_error.clone(), |el, error| {
                el.child(div().text_sm().text_color(theme.danger).child(format!("Couldn’t add the project: {error}")))
            })
            .into_any_element()
    }

    fn render_first_task(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let ready = self.ready_agents(cx);
        let where_ = self.project_info(cx).map(|p| p.name).unwrap_or_else(|| "a scratch folder".to_owned());
        let agent_label = self
            .agent
            .as_ref()
            .and_then(|(k, i)| ready.iter().find(|p| p.kind == *k && p.instance_id() == i))
            .map(|p| p.display_name().to_owned());
        let this = cx.weak_entity();
        let picked = self.agent.clone();
        let agent_picker = Button::new("onboarding-agent")
            .outline()
            .small()
            .icon(IconName::Bot)
            .label(agent_label.clone().unwrap_or_else(|| "No agent ready".to_owned()))
            .disabled(ready.is_empty())
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for info in &ready {
                    let key = (info.kind, info.instance_id().to_owned());
                    let this = this.clone();
                    let checked = picked.as_ref() == Some(&key);
                    menu = menu.item(PopupMenuItem::new(info.display_name().to_owned()).checked(checked).on_click(move |_, _, cx| {
                        let key = key.clone();
                        let _ = this.update(cx, |this, cx| {
                            this.agent = Some(key);
                            cx.notify();
                        });
                    }));
                }
                menu
            });
        let send_keys = crate::ui::keycap(crate::actions::platform("cmd-enter", "ctrl-enter"));
        v_flex()
            .gap_4()
            .child(self.heading(
                "Try a first task",
                format!("Orbit starts an agent in {where_} with this prompt. It asks before it changes a file, and you review every change."),
                cx,
            ))
            .child(h_flex().gap_2().text_sm().text_color(theme.muted_foreground).child("Agent").child(agent_picker))
            .child(Textarea::new(&self.prompt))
            .child(
                h_flex()
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(send_keys)
                    .child("starts it. Edit the prompt as you like."),
            )
            .when(agent_label.is_none(), |el| {
                el.child(div().text_sm().text_color(theme.warning).child("Install and sign in to an agent first (step 1), or skip this step."))
            })
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let step = self.step;
        let primary: SharedString = match step {
            Step::Welcome => "Get started".into(),
            Step::FirstTask => "Start task".into(),
            _ => "Continue".into(),
        };
        let primary_disabled = step == Step::FirstTask && (self.agent.is_none() || self.starting);
        h_flex()
            .w_full()
            .gap_2()
            .when_some(step.previous(), |el, _| {
                el.child(
                    Button::new("onboarding-back")
                        .ghost()
                        .icon(IconName::ArrowLeft)
                        .label("Back")
                        .on_click(cx.listener(|this, _, window, cx| this.back(&OnboardingBack, window, cx))),
                )
            })
            .child(div().flex_1())
            .child(
                Button::new("onboarding-skip")
                    .ghost()
                    .label(if step == Step::FirstTask { "Skip" } else { "Skip setup" })
                    .on_click(cx.listener(|this, _, window, cx| this.finish(window, cx))),
            )
            .child(
                Button::new("onboarding-continue")
                    .map(|b| if step == Step::Project && self.project.is_none() { b.outline() } else { b.primary() })
                    .label(primary)
                    .loading(self.starting)
                    .disabled(primary_disabled)
                    .on_click(cx.listener(|this, _, window, cx| this.advance(window, cx))),
            )
    }
}

/// Where clones go unless the user picks: the first of `~/code`,
/// `~/Projects`, `~/src`, `~/dev` that exists, else home.
fn default_clone_parent() -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default();
    ["code", "Code", "Projects", "projects", "src", "dev", "Developer"]
        .iter()
        .map(|d| home.join(d))
        .find(|p| p.is_dir())
        .unwrap_or(home)
}

/// A miniature Orbit drawn with the live theme: the preview of the choices
/// below it, since every choice applies at once.
fn preview(cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let line = |w: f32, color: Hsla| div().h_1p5().w(relative(w)).rounded_full().bg(color);
    div()
        .w_full()
        .h(rems(9.))
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .overflow_hidden()
        .child(
            h_flex()
                .size_full()
                .items_stretch()
                .child(
                    v_flex()
                        .w(relative(0.28))
                        .p_2()
                        .gap_2()
                        .bg(theme.sidebar)
                        .border_r_1()
                        .border_color(theme.sidebar_border)
                        .child(line(0.6, theme.sidebar_foreground.opacity(0.5)))
                        .child(div().p_1().rounded(theme.radius).bg(theme.sidebar_accent).child(line(0.8, theme.sidebar_accent_foreground.opacity(0.7))))
                        .child(line(0.7, theme.sidebar_foreground.opacity(0.3)))
                        .child(line(0.5, theme.sidebar_foreground.opacity(0.3))),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .p_3()
                        .gap_2()
                        .child(line(0.45, theme.foreground.opacity(0.7)))
                        .child(line(0.85, theme.muted_foreground.opacity(0.5)))
                        .child(line(0.7, theme.muted_foreground.opacity(0.5)))
                        .child(div().flex_1())
                        .child(
                            h_flex()
                                .p_1p5()
                                .gap_2()
                                .rounded(theme.radius)
                                .border_1()
                                .border_color(theme.input)
                                .child(div().flex_1().child(line(0.5, theme.muted_foreground.opacity(0.4))))
                                .child(div().size_4().rounded(theme.radius).bg(theme.primary)),
                        ),
                ),
        )
}

impl Focusable for OnboardingView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for OnboardingView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let body = match self.step {
            Step::Welcome => self.render_welcome(cx),
            Step::Agents => self.render_agents(cx),
            Step::Appearance => self.render_appearance(cx),
            Step::Project => self.render_project(cx),
            Step::FirstTask => self.render_first_task(cx),
        };
        let generation = self.generation;
        v_flex()
            .id("onboarding")
            .key_context("Onboarding")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::continue_))
            .on_action(cx.listener(Self::back))
            .size_full()
            .bg(theme.background)
            .child(
                div().id("onboarding-scroll").flex_1().min_h_0().overflow_y_scrollbar().child(
                    v_flex()
                        .w_full()
                        .items_center()
                        .px_6()
                        .pt(rems(3.))
                        .pb_8()
                        .child(
                            v_flex()
                                .w_full()
                                .max_w(rems(36.))
                                .gap_8()
                                .when(self.step != Step::Welcome, |el| el.child(self.render_stepper(cx)))
                                .child(div().w_full().child(body).with_animation(
                                    ElementId::NamedInteger("onboarding-step".into(), generation),
                                    Animation::new(STEP_ENTER).with_easing(ease_out_quint()),
                                    |el, t| el.opacity(t).mt(rems(0.75 * (1.0 - t))),
                                )),
                        ),
                ),
            )
            .child(
                div().w_full().border_t_1().border_color(theme.border).child(
                    h_flex().w_full().justify_center().px_6().py_3().child(div().w_full().max_w(rems(36.)).child(self.render_footer(cx))),
                ),
            )
    }
}
