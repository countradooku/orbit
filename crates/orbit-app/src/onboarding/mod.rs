//! First run, the activation checklist, and empty states that teach.
//!
//! **First run** ([`flow`]) is a full-window guide shown once, on a fresh
//! data folder: welcome, the agents Orbit found (with exact install and
//! sign-in steps for missing ones), appearance with a live preview, the
//! first project (a folder or a clone), and an optional first task. It can
//! be skipped at any step and run again from Settings › About.
//!
//! **The checklist** ([`checklist`]) sits on Home until every step is done
//! or it is dismissed: add a project, run an agent, review a diff, try a
//! race, open the command palette.
//!
//! Progress lives in `<data folder>/onboarding.json`. A data folder that
//! already has projects or threads when this version first opens it is
//! taken as set up: no guide, no checklist.

pub mod checklist;
mod flow;
pub mod sample;
pub mod teach;

use std::path::{Path, PathBuf};

use gpui_kit::*;
use orbit_proto::ThreadId;
use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::shell::{Route, Workspace};
pub use flow::OnboardingView;

gpui_kit::actions!(
    orbit,
    [
        /// Runs the first-run guide again.
        RunSetup,
        /// The guide's primary step forward (Enter).
        OnboardingContinue,
        /// The guide's step back (Escape).
        OnboardingBack,
    ]
);

const FILE_NAME: &str = "onboarding.json";

/// Steps of the activation checklist the engine's state can't show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Milestone {
    ReviewedDiff,
    UsedPalette,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Progress {
    /// The guide was finished or skipped.
    pub completed: bool,
    pub checklist_dismissed: bool,
    pub reviewed_diff: bool,
    pub used_palette: bool,
}

struct Onboarding {
    path: PathBuf,
    /// `None` until read, or decided for a data folder without a record.
    progress: Option<Progress>,
    app: Entity<AppState>,
    view: Option<Entity<OnboardingView>>,
    workspace: Option<WeakEntity<Workspace>>,
}

impl Global for Onboarding {}

fn read(path: &Path) -> Option<Progress> {
    sonic_rs::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write(path: &Path, progress: &Progress) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, sonic_rs::to_string_pretty(progress).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, path)
}

pub fn init(data_dir: &Path, app: Entity<AppState>, cx: &mut App) {
    let path = data_dir.join(FILE_NAME);
    let progress = read(&path);
    cx.set_global(Onboarding { path, progress, app, view: None, workspace: None });
    cx.bind_keys([
        KeyBinding::new("enter", OnboardingContinue, Some("Onboarding && !Input")),
        KeyBinding::new("escape", OnboardingBack, Some("Onboarding && !Input")),
    ]);
}

/// The engine connection, for views outside the workspace's tree.
pub fn app_state(cx: &App) -> Option<Entity<AppState>> {
    cx.try_global::<Onboarding>().map(|o| o.app.clone())
}

pub fn progress(cx: &App) -> Progress {
    cx.try_global::<Onboarding>().and_then(|o| o.progress.clone()).unwrap_or_default()
}

fn update(cx: &mut App, edit: impl FnOnce(&mut Progress)) {
    let Some(onboarding) = cx.try_global::<Onboarding>() else { return };
    let mut next = onboarding.progress.clone().unwrap_or_default();
    edit(&mut next);
    if onboarding.progress.as_ref() == Some(&next) {
        return;
    }
    let path = onboarding.path.clone();
    cx.global_mut::<Onboarding>().progress = Some(next.clone());
    cx.background_spawn(async move {
        if let Err(err) = write(&path, &next) {
            log::warn!("couldn’t save onboarding progress: {err}");
        }
    })
    .detach();
    cx.refresh_windows();
}

/// Records a checklist step the engine's state can't show.
pub fn mark(milestone: Milestone, cx: &mut App) {
    let progress = progress(cx);
    let done = match milestone {
        Milestone::ReviewedDiff => progress.reviewed_diff,
        Milestone::UsedPalette => progress.used_palette,
    };
    if done || progress.checklist_dismissed {
        return;
    }
    update(cx, |p| match milestone {
        Milestone::ReviewedDiff => p.reviewed_diff = true,
        Milestone::UsedPalette => p.used_palette = true,
    });
}

pub fn dismiss_checklist(cx: &mut App) {
    update(cx, |p| p.checklist_dismissed = true);
}

/// The guide, while it is showing.
pub fn active_view(cx: &App) -> Option<Entity<OnboardingView>> {
    cx.try_global::<Onboarding>().and_then(|o| o.view.clone())
}

/// Opens the guide (first run, or Settings › Run setup again).
pub fn open(window: &mut Window, cx: &mut App) {
    let Some(app) = app_state(cx) else { return };
    if active_view(cx).is_some() {
        return;
    }
    let view = cx.new(|cx| OnboardingView::new(app, window, cx));
    let focus = view.read(cx).focus_handle(cx);
    cx.global_mut::<Onboarding>().view = Some(view);
    window.focus(&focus, cx);
    cx.refresh_windows();
}

/// Closes the guide for good (finished or skipped).
pub(crate) fn close(window: &mut Window, cx: &mut App) {
    if cx.has_global::<Onboarding>() {
        cx.global_mut::<Onboarding>().view = None;
    }
    update(cx, |p| p.completed = true);
    window.dispatch_action(Box::new(crate::actions::GoHome), cx);
    cx.refresh_windows();
}

/// Once the window is up: decide whether this data folder needs the guide.
/// That waits for the engine's first snapshot (its provider list is never
/// empty), so a folder with projects is never mistaken for a fresh one.
/// The window's workspace was replaced (it switched hosts).
pub fn set_workspace(workspace: WeakEntity<Workspace>, cx: &mut App) {
    if cx.has_global::<Onboarding>() {
        cx.global_mut::<Onboarding>().workspace = Some(workspace);
    }
}

pub fn on_window_open(window: &mut Window, workspace: WeakEntity<Workspace>, cx: &mut App) {
    if cx.has_global::<Onboarding>() {
        cx.global_mut::<Onboarding>().workspace = Some(workspace);
    }
    let Some(app) = app_state(cx) else { return };
    if let Some(progress) = cx.try_global::<Onboarding>().and_then(|o| o.progress.clone()) {
        // Quit halfway through last time: pick the guide up again.
        if !progress.completed {
            open(window, cx);
        }
        return;
    }
    fn decide(app: Entity<AppState>, window: &mut Window, cx: &mut App) {
        if cx.try_global::<Onboarding>().is_none_or(|o| o.progress.is_some()) {
            return;
        }
        let state = app.read(cx).state();
        if state.providers.is_empty() {
            return;
        }
        let fresh = state.projects.is_empty() && state.threads.is_empty();
        if fresh {
            // Recorded now: a guide left by quitting resumes next time
            // instead of being decided again from what it already added.
            update(cx, |_| {});
            open(window, cx);
        } else {
            // Set up before this version: nothing to teach.
            update(cx, |p| {
                p.completed = true;
                p.checklist_dismissed = true;
            });
        }
    }
    decide(app.clone(), window, cx);
    // Until the snapshot arrives; afterwards a cheap no-op.
    window.observe(&app, cx, decide).detach();
}

fn workspace(cx: &App) -> Option<Entity<Workspace>> {
    cx.try_global::<Onboarding>()?.workspace.as_ref()?.upgrade()
}

/// Opens a thread (the guide's first task).
pub(crate) fn open_thread(thread: ThreadId, window: &mut Window, cx: &mut App) {
    if let Some(workspace) = workspace(cx) {
        workspace.update(cx, |ws, cx| ws.navigate(Route::Thread(thread), window, cx));
    }
}

/// The checklist's "Review a diff": the latest thread with changes (else
/// the latest thread), with its review open.
fn review_latest(window: &mut Window, cx: &mut App) {
    let Some(app) = app_state(cx) else { return };
    let state = app.read(cx).state();
    let latest = |with_changes: bool| {
        state
            .threads
            .values()
            .filter(|t| !t.archived && (!with_changes || state.workspace.get(&t.id).is_some_and(|c| !c.is_empty())))
            .max_by_key(|t| t.updated_ms)
            .map(|t| t.id)
    };
    let Some(thread) = latest(true).or_else(|| latest(false)) else { return };
    open_thread(thread, window, cx);
    window.defer(cx, |window, cx| window.dispatch_action(Box::new(crate::actions::ToggleReview), cx));
}

/// The checklist's "Try a race": Home's composer armed for two agents.
fn set_up_race(window: &mut Window, cx: &mut App) {
    let Some(workspace) = workspace(cx) else { return };
    let home = workspace.update(cx, |ws, cx| {
        ws.navigate(Route::Home, window, cx);
        ws.home().clone()
    });
    home.update(cx, |home, cx| home.arm_race(window, cx));
}

/// The guide's and the checklist's commands, on the workspace's root.
pub fn register_actions<E: InteractiveElement>(el: E) -> E {
    el.on_action(|_: &RunSetup, window, cx| open(window, cx))
        .on_action(|_: &checklist::ReviewLatestThread, window, cx| review_latest(window, cx))
        .on_action(|_: &checklist::SetUpRace, window, cx| set_up_race(window, cx))
}

/// Settings › About › Help: the guide again, diagnostics, About.
pub fn help_settings_group() -> gpui_kit::component::setting::SettingGroup {
    use gpui_kit::component::button::Button;
    use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem};
    use gpui_kit::component::Sizable as _;

    fn command(id: &'static str, label: &'static str, action: fn() -> Box<dyn Action>) -> SettingField<SharedString> {
        SettingField::render(move |_, _, _| {
            Button::new(id).outline().small().label(label).on_click(move |_, window, cx| window.dispatch_action(action(), cx))
        })
    }
    SettingGroup::new().title("Help").items([
        SettingItem::new("Setup guide", command("help-run-setup", "Run setup again…", || Box::new(RunSetup)))
            .description("The first-run guide: agents, appearance, a project and a first task.")
            .keywords(["onboarding", "welcome", "first run", "tutorial"]),
        SettingItem::new(
            "Diagnostics",
            command("help-diagnostics", "Collect diagnostics…", || Box::new(crate::diagnostics::CollectDiagnostics)),
        )
        .description("Saves Orbit’s logs and crash reports, with secrets removed, to a zip you can share. Nothing is sent.")
        .keywords(["logs", "crash", "support", "bug report"]),
        SettingItem::new("About Orbit", command("help-about", "About Orbit…", || Box::new(crate::diagnostics::ShowAbout)))
            .description("Version, commit, build and renderer.")
            .keywords(["version", "build", "commit"]),
    ])
}
