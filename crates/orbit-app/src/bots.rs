//! The chat bots and the guard list, as Settings › Bots edits them.
//!
//! The bots run with the engine (in this process, or on the `orbitd` host
//! Orbit is connected to). This global mirrors their state for the page,
//! which reads it through `&App` closures: it is read when Orbit starts and
//! whenever the page is drawn without it, and then follows the engine's
//! `Event::Bots` and `Event::GuardChanged`. Tokens never come back from the
//! engine; only a hint of each (`…x9Q2`) does.

pub mod page;

use std::time::{Duration, Instant};

use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;

/// Guard edits are saved after this pause, so typing a rule costs one write.
const SAVE_DELAY: Duration = Duration::from_millis(500);
/// A host without bots is asked again after this long while the page shows.
const RETRY_UNAVAILABLE: Duration = Duration::from_secs(10);

/// The Slack app manifest for "Create an app › From a manifest": Socket
/// Mode (no public URL), the scopes and events the Slack bot uses, and
/// interactivity for its buttons.
pub const SLACK_MANIFEST: &str = r#"{
  "display_information": {
    "name": "Orbit",
    "description": "Drive your Orbit coding agents from Slack."
  },
  "features": {
    "app_home": {
      "messages_tab_enabled": true,
      "messages_tab_read_only_enabled": false
    },
    "bot_user": {
      "display_name": "Orbit",
      "always_online": true
    },
    "slash_commands": [
      {
        "command": "/orbit",
        "description": "Orbit: status, new, threads, switch, stop, diff, help",
        "usage_hint": "status",
        "should_escape": false
      }
    ]
  },
  "oauth_config": {
    "scopes": {
      "bot": [
        "app_mentions:read",
        "chat:write",
        "commands",
        "im:history",
        "im:read",
        "im:write",
        "files:write",
        "users:read"
      ]
    }
  },
  "settings": {
    "event_subscriptions": {
      "bot_events": ["app_mention", "message.im"]
    },
    "interactivity": {
      "is_enabled": true
    },
    "org_deploy_enabled": false,
    "socket_mode_enabled": true,
    "token_rotation_enabled": false
  }
}"#;

pub struct BotsModel {
    app: Entity<AppState>,
    state: Option<BotsState>,
    guard: Option<GuardSettings>,
    /// Why the bots could not be read, e.g. a host that runs none.
    unavailable: Option<String>,
    loading: bool,
    loaded_at: Option<Instant>,
    guard_save: Option<Task<()>>,
}

impl Global for BotsModel {}

pub fn init(app: Entity<AppState>, cx: &mut App) {
    cx.set_global(BotsModel {
        app,
        state: None,
        guard: None,
        unavailable: None,
        loading: false,
        loaded_at: None,
        guard_save: None,
    });
    reload(cx);
    // Paired chats get needs-you pushes while no Orbit window is in front.
    cx.observe_new::<crate::shell::Workspace>(|_, window, cx| {
        let Some(window) = window else { return };
        set_app_active(window.is_window_active(), cx);
        cx.observe_window_activation(window, |_, window, cx| set_app_active(window.is_window_active(), cx)).detach();
    })
    .detach();
}

fn set_app_active(active: bool, cx: &mut App) {
    let Some(model) = cx.try_global::<BotsModel>() else { return };
    model.app.read(cx).send(Command::Bots(BotCommand::AppActive(active)));
}

/// Reads the bots' state and the guard list from the engine.
pub fn reload(cx: &mut App) {
    let Some(model) = cx.try_global::<BotsModel>() else { return };
    if model.loading {
        return;
    }
    let app = model.app.clone();
    cx.global_mut::<BotsModel>().loading = true;
    let bots = app.update(cx, |app, cx| app.call(Command::Bots(BotCommand::State), cx));
    let guard = app.update(cx, |app, cx| app.call(Command::Guard(None), cx));
    cx.spawn(async move |cx| {
        let (bots, guard) = (bots.await, guard.await);
        cx.update(|cx| {
            let model = cx.global_mut::<BotsModel>();
            model.loading = false;
            model.loaded_at = Some(Instant::now());
            match bots {
                Ok(Reply::Bots(BotReply::State(state))) => {
                    model.state = Some(state);
                    model.unavailable = None;
                }
                Ok(_) => {}
                Err(err) => model.unavailable = Some(err),
            }
            // An edit made while loading wins.
            if let Ok(Reply::Guard(guard)) = guard
                && model.guard_save.is_none()
            {
                model.guard = Some(guard);
            }
            cx.refresh_windows();
        });
    })
    .detach();
}

/// Called while the page is drawn: reads the state if it never arrived, or
/// asks a host that had no bots again after a while.
pub fn ensure_loaded(cx: &mut App) {
    let Some(model) = cx.try_global::<BotsModel>() else { return };
    let stale = match model.loaded_at {
        None => true,
        Some(at) => model.unavailable.is_some() && at.elapsed() > RETRY_UNAVAILABLE,
    };
    if stale && !model.loading {
        reload(cx);
    }
}

/// The engine broadcast new state (or the connection was reset).
pub fn receive(state: Option<BotsState>, guard: Option<GuardSettings>, reset: bool, cx: &mut App) {
    if cx.try_global::<BotsModel>().is_none() {
        return;
    }
    if reset {
        // Called while `AppState` applies a batch; read once it is done.
        cx.defer(reload);
        return;
    }
    let model = cx.global_mut::<BotsModel>();
    if let Some(state) = state {
        model.state = Some(state);
        model.unavailable = None;
    }
    if let Some(guard) = guard
        && model.guard_save.is_none()
    {
        model.guard = Some(guard);
    }
    cx.refresh_windows();
}

pub fn state(cx: &App) -> Option<&BotsState> {
    cx.try_global::<BotsModel>().and_then(|m| m.state.as_ref())
}

pub fn guard(cx: &App) -> Option<&GuardSettings> {
    cx.try_global::<BotsModel>().and_then(|m| m.guard.as_ref())
}

pub fn unavailable(cx: &App) -> Option<&str> {
    cx.try_global::<BotsModel>().and_then(|m| m.unavailable.as_deref())
}

/// Sends a bot command. The reply's state replaces ours; a failure is shown
/// as a notification and the state is read again, undoing optimistic edits.
pub fn send(command: BotCommand, cx: &mut App) {
    let Some(model) = cx.try_global::<BotsModel>() else { return };
    let app = model.app.clone();
    let task = app.update(cx, |app, cx| app.call(Command::Bots(command), cx));
    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| match result {
            Ok(Reply::Bots(BotReply::State(state))) => receive(Some(state), None, false, cx),
            Ok(_) => reload(cx),
            Err(err) => {
                report(err, cx);
                reload(cx);
            }
        });
    })
    .detach();
}

/// Applies `edit` to our copy of the state at once (switches respond on
/// the click), then sends `command`.
pub fn send_optimistic(edit: impl FnOnce(&mut BotsState), command: BotCommand, cx: &mut App) {
    if cx.try_global::<BotsModel>().is_some_and(|m| m.state.is_some()) {
        if let Some(state) = cx.global_mut::<BotsModel>().state.as_mut() {
            edit(state);
        }
        cx.refresh_windows();
    }
    send(command, cx);
}

/// Edits the chat options and saves them.
pub fn edit_options(edit: impl FnOnce(&mut BotOptions), cx: &mut App) {
    let Some(mut options) = state(cx).map(|s| s.options.clone()) else { return };
    edit(&mut options);
    if state(cx).is_some_and(|s| s.options == options) {
        return;
    }
    let sent = options.clone();
    send_optimistic(move |s| s.options = options, BotCommand::SetOptions(sent), cx);
}

/// Edits the guard list now and saves it shortly.
pub fn edit_guard(edit: impl FnOnce(&mut GuardSettings), cx: &mut App) {
    if cx.try_global::<BotsModel>().is_none() {
        return;
    }
    let model = cx.global_mut::<BotsModel>();
    let mut guard = model.guard.clone().unwrap_or_default();
    edit(&mut guard);
    if model.guard.as_ref() == Some(&guard) {
        return;
    }
    model.guard = Some(guard);
    let app = model.app.clone();
    let save = cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVE_DELAY).await;
        let task = cx.update(|cx| {
            let model = cx.global_mut::<BotsModel>();
            model.guard_save = None;
            let guard = model.guard.clone().unwrap_or_default();
            app.update(cx, |app, cx| app.call(Command::Guard(Some(guard)), cx))
        });
        let result = task.await;
        cx.update(|cx| match result {
            Ok(Reply::Guard(saved)) => receive(None, Some(saved), false, cx),
            Ok(_) => {}
            Err(err) => report(err, cx),
        });
    });
    cx.global_mut::<BotsModel>().guard_save = Some(save);
    cx.refresh_windows();
}

/// Shows a failure in the window in front.
fn report(message: String, cx: &mut App) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::notification::Notification;
    if let Some(window) = cx.active_window().or_else(|| cx.windows().first().copied()) {
        let _ = window.update(cx, |_, window, cx| window.push_notification(Notification::error(message), cx));
    }
}

#[cfg(test)]
mod tests {
    use sonic_rs::{JsonContainerTrait as _, JsonValueTrait as _};

    #[test]
    fn the_slack_manifest_is_valid_json_with_socket_mode() {
        let manifest: sonic_rs::Value = sonic_rs::from_str(super::SLACK_MANIFEST).unwrap();
        let settings = manifest.get("settings").unwrap();
        assert_eq!(settings.get("socket_mode_enabled").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(settings.get("interactivity").and_then(|i| i.get("is_enabled")).and_then(|v| v.as_bool()), Some(true));
        let scopes = manifest.pointer(["oauth_config", "scopes", "bot"]).and_then(|v| v.as_array()).unwrap();
        for scope in ["app_mentions:read", "chat:write", "im:history", "files:write", "users:read"] {
            assert!(scopes.iter().any(|s| s.as_str() == Some(scope)), "{scope}");
        }
    }
}
