//! Hosts in the interface: the window's host switcher, adding hosts,
//! connecting and installing `orbitd`, picking folders on a host, and
//! sharing this computer (see `docs/REMOTE.md`).
//!
//! The window shows one host at a time ([`crate::app_state`] keeps the
//! others connected in the background). [`HostFrame`] is the window's root:
//! switching hosts closes the shown host's views first (their drafts and
//! timelines go back to that host; terminals keep running there), then
//! shows the other host in a fresh [`Workspace`], where its window was.

pub mod add;
pub mod browse;
pub mod install;
pub mod page;
pub mod panel;
pub mod share;
pub mod switcher;

use std::time::Duration;

use gpui_kit::component::{ActiveTheme as _, TitleBar, v_flex};
use gpui_kit::*;
use orbit_net::ConnectionStatus;
use orbit_net::remote::{HostEntry, HostKind, LinkPhase};

use crate::app_state::{AppState, HostSummary};
use crate::shell::Workspace;

/// Shows another host in this window.
#[derive(Clone, PartialEq, gpui_kit::Action)]
#[action(namespace = orbit, no_json)]
pub struct SwitchHost {
    pub id: SharedString,
}

gpui_kit::actions!(
    orbit,
    [
        /// Adds an SSH host, a WSL distro or a direct orbitd.
        AddHost,
        /// Settings › Hosts.
        ManageHosts,
    ]
);

/// Shows host `id` in `window` (the window's [`HostFrame`] does it).
pub fn switch_to(id: SharedString, window: &mut Window, cx: &mut App) {
    let frame = gpui_kit::base::Root::read(window, cx).view().clone().downcast::<HostFrame>().ok();
    if let Some(frame) = frame {
        frame.update(cx, |frame, cx| frame.switch_host(&SwitchHost { id }, window, cx));
    }
}

/// The window's root: the shown host's workspace, replaced on a switch.
pub struct HostFrame {
    app: Entity<AppState>,
    workspace: Option<Entity<Workspace>>,
    /// A launch for this computer that arrived while the window showed
    /// another host: handled once this computer is shown.
    pending_launch: Option<crate::instance::Launch>,
}

impl HostFrame {
    pub fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace = cx.new(|cx| Workspace::new(app.clone(), window, cx));
        Self { app, workspace: Some(workspace), pending_launch: None }
    }

    /// A launch's folders and threads are this computer's: show it first.
    pub fn handle_launch(&mut self, launch: crate::instance::Launch, window: &mut Window, cx: &mut Context<Self>) {
        let local = self.app.read(cx).active_entry().is_local();
        if local || crate::instance::intents(&launch).is_empty() {
            if let Some(workspace) = &self.workspace {
                workspace.update(cx, |ws, cx| ws.handle_launch(launch, window, cx));
            }
            return;
        }
        self.pending_launch = Some(launch);
        self.switch_host(&SwitchHost { id: orbit_net::remote::hosts::LOCAL_ID.into() }, window, cx);
    }

    /// Hands a pending launch to the workspace once this computer's
    /// threads are there (or after a few seconds).
    fn deliver_pending_launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(launch) = self.pending_launch.take() else { return };
        cx.spawn_in(window, async move |this, cx| {
            for _ in 0..100 {
                let ready = this
                    .update(cx, |this, cx| matches!(this.app.read(cx).connection(), None | Some(orbit_net::ConnectionStatus::Connected)))
                    .unwrap_or(true);
                if ready {
                    break;
                }
                cx.background_executor().timer(Duration::from_millis(50)).await;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(workspace) = &this.workspace {
                    workspace.update(cx, |ws, cx| ws.handle_launch(launch, window, cx));
                }
            });
        })
        .detach();
    }

    /// The workspace on screen (none for the moment of a switch).
    pub fn workspace(&self) -> Option<Entity<Workspace>> {
        self.workspace.clone()
    }

    fn switch_host(&mut self, action: &SwitchHost, window: &mut Window, cx: &mut Context<Self>) {
        let id = action.id.to_string();
        if self.app.read(cx).active_entry().id == id || self.workspace.is_none() {
            return;
        }
        // Edits not saved yet belong to the host being left.
        crate::settings::flush(cx);
        let left = self.app.read(cx).active_entry().id.clone();
        if let Some(workspace) = self.workspace.take() {
            let memory = workspace.read(cx).memory();
            self.app.update(cx, |app, _| {
                app.remember(left, memory);
                app.begin_switch();
            });
        }
        cx.notify();
        // The views just dropped close in this update, still talking to the
        // host they showed; the other host is shown after.
        cx.spawn_in(window, async move |this, cx| {
            let _ = this.update_in(cx, |this, window, cx| {
                let switched = this.app.update(cx, |app, cx| app.activate(&id, cx));
                let memory = this.app.update(cx, |app, _| {
                    let id = app.active_entry().id.clone();
                    app.take_memory(&id)
                });
                let workspace = cx.new(|cx| Workspace::new(this.app.clone(), window, cx));
                workspace.update(cx, |ws, cx| ws.restore(memory, window, cx));
                crate::onboarding::set_workspace(workspace.downgrade(), cx);
                this.workspace = Some(workspace);
                this.deliver_pending_launch(window, cx);
                if let Err(message) = switched {
                    use gpui_kit::component::WindowExt as _;
                    window.push_notification(gpui_kit::component::notification::Notification::error(message), cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for HostFrame {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.workspace {
            Some(workspace) => workspace.clone().into_any_element(),
            // For the frame between closing one host's views and showing
            // the next: the window keeps its chrome.
            None => v_flex().size_full().bg(cx.theme().background).child(TitleBar::new()).into_any_element(),
        };
        div()
            .size_full()
            .on_action(cx.listener(Self::switch_host))
            .on_action(cx.listener(|_, _: &AddHost, window, cx| add::open(window, cx)))
            .on_action(cx.listener(|_, _: &ManageHosts, window, cx| page::open(window, cx)))
            .child(content)
    }
}

// ── Words and marks shared by the host views ─────────────────────────────

/// Tokens of direct hosts, never to appear in a diagnostics bundle.
pub fn tokens(cx: &App) -> Vec<String> {
    let Some(app) = app_entity(cx) else { return Vec::new() };
    app.read(cx)
        .hosts()
        .hosts
        .iter()
        .filter_map(|h| match &h.kind {
            HostKind::Url { token, .. } => Some(token.clone()),
            _ => None,
        })
        .collect()
}

fn app_entity(cx: &App) -> Option<Entity<AppState>> {
    cx.try_global::<crate::settings::SettingsModel>().map(|m| m.app().clone())
}

/// How a host stands, in a few words, and whether that is good, pending or
/// bad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Good,
    Pending,
    Bad,
    /// Not connected from this window yet.
    Idle,
}

pub fn status_line(entry: &HostEntry, summary: &HostSummary) -> (Tone, String) {
    match (&summary.connection, &summary.phase) {
        (None, _) => (Tone::Idle, "Not connected".into()),
        (Some(ConnectionStatus::Connected), _) => {
            let text = match summary.latency {
                Some(latency) if !entry.is_local() => format!("Connected · {}", latency_text(latency)),
                _ => "Connected".to_owned(),
            };
            (Tone::Good, text)
        }
        (_, Some(LinkPhase::NeedsInstall(_))) => (Tone::Bad, "orbitd not installed".into()),
        (_, Some(LinkPhase::Failed(failure))) if failure.kind != orbit_net::remote::FailureKind::Unreachable => {
            (Tone::Bad, short_failure(failure))
        }
        (Some(ConnectionStatus::Connecting), _) | (_, Some(LinkPhase::Probing)) => (Tone::Pending, "Connecting…".into()),
        (Some(ConnectionStatus::Disconnected { reason }), _) => (Tone::Pending, format!("Reconnecting · {reason}")),
    }
}

fn short_failure(failure: &orbit_net::remote::Failure) -> String {
    use orbit_net::remote::FailureKind as K;
    match failure.kind {
        K::Auth => "Needs sign-in".into(),
        K::HostKey => "Host key to check".into(),
        K::NoTool => "ssh not found".into(),
        K::NoDistro => "No such distro".into(),
        K::Unreachable | K::Other => failure.message.clone(),
    }
}

/// "38 ms", "<1 ms", "1.2 s".
pub fn latency_text(latency: Duration) -> String {
    let ms = latency.as_secs_f64() * 1000.0;
    if ms < 1.0 {
        "<1 ms".into()
    } else if ms < 1000.0 {
        format!("{} ms", ms.round() as u64)
    } else {
        format!("{:.1} s", ms / 1000.0)
    }
}

/// The icon for a kind of host.
pub fn kind_icon(entry: &HostEntry) -> crate::assets::Lucide {
    use crate::assets::Lucide;
    match entry.kind {
        HostKind::Local => Lucide::Laptop,
        HostKind::Ssh { .. } => Lucide::Server,
        HostKind::Wsl { .. } => Lucide::Terminal,
        HostKind::Url { .. } => Lucide::Wifi,
    }
}

/// A status dot.
pub fn dot(tone: Tone, cx: &App) -> Div {
    let theme = cx.theme();
    let color = match tone {
        Tone::Good => theme.success,
        Tone::Pending => theme.warning,
        Tone::Bad => theme.danger,
        Tone::Idle => theme.muted_foreground.opacity(0.5),
    };
    div().flex_none().size_1p5().rounded_full().bg(color)
}
