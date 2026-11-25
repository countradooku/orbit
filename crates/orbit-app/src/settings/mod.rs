//! Settings: the record the interface edits, how edits reach the engine, and
//! how they take effect.
//!
//! [`SettingsModel`] is a global so every view (and the settings page's
//! field closures, which only get `&App`) reads one copy. An edit:
//!
//! 1. replaces the local copy and takes effect at once (theme, zoom, motion;
//!    views that read the rest observe the global and re-render);
//! 2. is sent to the engine as `UpdateSettings` after a short pause, so
//!    typing a path costs one write, not one per keystroke;
//! 3. comes back as `SettingsChanged`, which is recognized as our own echo
//!    and ignored, so it never overwrites what the user typed meanwhile.
//!
//! A `SettingsChanged` from elsewhere (another client of the same host)
//! replaces the local copy when nothing local is pending.
//!
//! Which host to connect to ([`RemoteSettings`]) is not engine state: it is
//! read before any engine exists, from `<data folder>/connection`.

mod page;

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;

pub use page::render as page;

/// Pause after the last edit before it is sent to the engine.
const SAVE_DELAY: Duration = Duration::from_millis(400);

/// Interface zoom steps offered in the picker and by the zoom commands.
pub const ZOOM_STEPS: [u16; 7] = [75, 90, 100, 110, 125, 140, 150];

pub struct SettingsModel {
    app: Entity<AppState>,
    current: Settings,
    /// Edited here and not sent yet.
    dirty: bool,
    /// Sent and not echoed back yet.
    in_flight: Option<Settings>,
    save: Option<Task<()>>,
    /// Where this client keeps its own files.
    data_dir: PathBuf,
    /// The connection record as saved (takes effect on the next start).
    remote: RemoteSettings,
    /// The host this process is connected to, if remote.
    connected_host: Option<String>,
    /// The host came from `ORBIT_HOST`, which wins over the saved record.
    host_from_env: bool,
    /// Bytes under the data folder, once measured.
    storage: Option<u64>,
    measuring: Option<Task<()>>,
}

impl Global for SettingsModel {}

/// Where the client runs and connects, decided before the engine starts.
pub struct Startup {
    pub data_dir: PathBuf,
    pub remote: RemoteSettings,
    pub connected_host: Option<String>,
    pub host_from_env: bool,
}

pub fn init(app: Entity<AppState>, startup: Startup, cx: &mut App) {
    cx.set_global(SettingsModel {
        app,
        current: Settings::default(),
        dirty: false,
        in_flight: None,
        save: None,
        data_dir: startup.data_dir,
        remote: startup.remote,
        connected_host: startup.connected_host,
        host_from_env: startup.host_from_env,
        storage: None,
        measuring: None,
    });
}

/// The current settings (defaults before the engine has answered).
pub fn get(cx: &App) -> &Settings {
    static DEFAULT: std::sync::OnceLock<Settings> = std::sync::OnceLock::new();
    match cx.try_global::<SettingsModel>() {
        Some(model) => &model.current,
        None => DEFAULT.get_or_init(Settings::default),
    }
}

/// Edits the settings: applies the change now and saves it shortly.
pub fn update(cx: &mut App, edit: impl FnOnce(&mut Settings)) {
    let Some(model) = cx.try_global::<SettingsModel>() else { return };
    let old = model.current.clone();
    let mut new = old.clone();
    edit(&mut new);
    if new == old {
        return;
    }
    cx.update_global::<SettingsModel, _>(|model, _| {
        model.current = new.clone();
        model.dirty = true;
    });
    apply_effects(&old, &new, cx);
    // Views that read settings while rendering (chat width, the composer's
    // hints, the page itself) pick the change up on this frame.
    cx.refresh_windows();
    let save = cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVE_DELAY).await;
        cx.update(flush);
    });
    cx.update_global::<SettingsModel, _>(|model, _| model.save = Some(save));
}

/// Sends pending edits to the engine now (also called at quit).
pub fn flush(cx: &mut App) {
    let Some(model) = cx.try_global::<SettingsModel>().filter(|m| m.dirty) else { return };
    let (settings, app) = (model.current.clone(), model.app.clone());
    cx.update_global::<SettingsModel, _>(|model, _| {
        model.dirty = false;
        model.in_flight = Some(settings.clone());
    });
    app.read(cx).send(Command::UpdateSettings { settings: Box::new(settings) });
}

/// The engine's settings arrived (snapshot or `SettingsChanged`).
pub fn receive(engine: Settings, cx: &mut App) {
    let Some(model) = cx.try_global::<SettingsModel>() else { return };
    if model.dirty {
        return; // our pending edit will replace them
    }
    if let Some(sent) = &model.in_flight {
        // Our own echo, possibly sanitized by the engine: keep the local
        // copy, which may hold text the user is still typing.
        let ours = sent.clone().sanitized() == engine;
        if ours {
            cx.update_global::<SettingsModel, _>(|model, _| model.in_flight = None);
        }
        return;
    }
    if model.current == engine {
        return;
    }
    let old = std::mem::replace(&mut cx.global_mut::<SettingsModel>().current, engine.clone());
    apply_effects(&old, &engine, cx);
    cx.refresh_windows();
}

fn apply_effects(old: &Settings, new: &Settings, cx: &mut App) {
    let (a, b) = (&old.appearance, &new.appearance);
    crate::format::set_clock(b.clock);
    let theme_changed = (a.mode, &a.light_theme, &a.dark_theme, a.ui_scale, a.code_font_size)
        != (b.mode, &b.light_theme, &b.dark_theme, b.ui_scale, b.code_font_size);
    if theme_changed {
        crate::theme::apply(b, None, cx);
    }
    if a.motion != b.motion {
        gpui_kit::base::set_reduce_motion_preference(
            match b.motion {
                Motion::System => gpui_kit::base::ReduceMotionPreference::System,
                Motion::Reduce => gpui_kit::base::ReduceMotionPreference::Reduce,
                Motion::Allow => gpui_kit::base::ReduceMotionPreference::Allow,
            },
            cx,
        );
    }
}

/// Re-applies the appearance, for example after the system switched between
/// light and dark.
pub fn reapply_appearance(window: &mut Window, cx: &mut App) {
    let appearance = get(cx).appearance.clone();
    crate::theme::apply(&appearance, Some(window), cx);
}

/// Steps interface zoom by `step` positions of [`ZOOM_STEPS`]; 0 resets it.
pub fn zoom(step: i32, cx: &mut App) {
    update(cx, |s| {
        let scale = &mut s.appearance.ui_scale;
        *scale = if step == 0 {
            100
        } else {
            let at = ZOOM_STEPS.iter().position(|&z| z >= *scale).unwrap_or(ZOOM_STEPS.len() - 1) as i32;
            let exact = ZOOM_STEPS.get(at as usize) == Some(scale);
            let target = if step > 0 { at + if exact { 1 } else { 0 } } else { at - 1 };
            ZOOM_STEPS[target.clamp(0, ZOOM_STEPS.len() as i32 - 1) as usize]
        };
    });
}

/// Maximum width of the reading column (threads, inbox, home).
pub fn chat_column(cx: &App) -> Length {
    match get(cx).appearance.chat_width.rems() {
        Some(width) => rems(width).into(),
        None => relative(1.).into(),
    }
}

// ── Composer keys ────────────────────────────────────────────────────────

/// "Ctrl" or "⌘", as the platform names the secondary modifier.
pub fn mod_key() -> &'static str {
    if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" }
}

/// How the composer's send keys read: (send, alternate).
pub fn send_keys(cx: &App) -> (String, String) {
    match get(cx).composer.submit {
        SubmitKey::Enter => ("Enter".into(), format!("{}+Enter", mod_key())),
        SubmitKey::ModEnter => (format!("{}+Enter", mod_key()), format!("{}+Shift+Enter", mod_key())),
    }
}

// ── Connection ───────────────────────────────────────────────────────────

const CONNECTION_FILE: &str = "connection";

/// Reads the saved connection (`host=` and `token=` lines).
pub fn read_connection(data_dir: &Path) -> RemoteSettings {
    let Ok(text) = std::fs::read_to_string(data_dir.join(CONNECTION_FILE)) else { return RemoteSettings::default() };
    let mut remote = RemoteSettings::default();
    for line in text.lines() {
        match line.split_once('=') {
            Some(("host", value)) => remote.host = value.trim().to_owned(),
            Some(("token", value)) => remote.token = value.trim().to_owned(),
            _ => {}
        }
    }
    remote
}

fn write_connection(data_dir: &Path, remote: &RemoteSettings) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(CONNECTION_FILE);
    if !remote.is_remote() && remote.token.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("host={}\ntoken={}\n", remote.host.trim(), remote.token.trim()))?;
    std::fs::rename(tmp, path)
}

pub fn remote(cx: &App) -> RemoteSettings {
    cx.try_global::<SettingsModel>().map(|m| m.remote.clone()).unwrap_or_default()
}

/// Saves the connection record; it takes effect the next time Orbit starts.
pub fn set_remote(cx: &mut App, edit: impl FnOnce(&mut RemoteSettings)) {
    if !cx.has_global::<SettingsModel>() {
        return;
    }
    let model = cx.global_mut::<SettingsModel>();
    edit(&mut model.remote);
    if let Err(err) = write_connection(&model.data_dir, &model.remote) {
        log::warn!("couldn’t save the connection: {err}");
    }
    cx.refresh_windows();
}

/// The host this process is connected to, and whether `ORBIT_HOST` set it.
pub fn connected_host(cx: &App) -> (Option<String>, bool) {
    cx.try_global::<SettingsModel>().map(|m| (m.connected_host.clone(), m.host_from_env)).unwrap_or_default()
}

// ── Data folder ──────────────────────────────────────────────────────────

pub fn data_dir(cx: &App) -> PathBuf {
    cx.try_global::<SettingsModel>()
        .map(|m| m.data_dir.clone())
        .unwrap_or_else(orbit_core::EngineOptions::default_data_dir)
}

pub fn storage(cx: &App) -> Option<u64> {
    cx.try_global::<SettingsModel>().and_then(|m| m.storage)
}

/// Measures the data folder in the background.
pub fn measure_storage(cx: &mut App) {
    let dir = data_dir(cx);
    let task = cx.spawn(async move |cx| {
        let bytes = cx.background_spawn(async move { folder_size(&dir) }).await;
        cx.update(|cx| {
            if cx.has_global::<SettingsModel>() {
                let model = cx.global_mut::<SettingsModel>();
                model.storage = Some(bytes);
                model.measuring = None;
            }
            cx.refresh_windows();
        });
    });
    if cx.has_global::<SettingsModel>() {
        cx.global_mut::<SettingsModel>().measuring = Some(task);
    }
}

fn folder_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            // `file_type` does not follow links, so a link never loops.
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(t) if t.is_file() => total += entry.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that would bring in `gpui_kit::test` in place of `#[test]`.
    use super::{CONNECTION_FILE, folder_size, read_connection, write_connection};
    use orbit_proto::RemoteSettings;

    #[test]
    fn connection_file_round_trips_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_connection(dir.path()), RemoteSettings::default());
        let remote = RemoteSettings { host: "ws://box:7840/".into(), token: "abc".into() };
        write_connection(dir.path(), &remote).unwrap();
        assert_eq!(read_connection(dir.path()), remote);
        write_connection(dir.path(), &RemoteSettings::default()).unwrap();
        assert!(!dir.path().join(CONNECTION_FILE).exists());
    }

    #[test]
    fn folder_size_counts_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/c"), [0u8; 100]).unwrap();
        std::fs::write(dir.path().join("d"), [0u8; 20]).unwrap();
        assert_eq!(folder_size(dir.path()), 120);
    }
}
