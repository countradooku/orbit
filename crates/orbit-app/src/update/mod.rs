//! Keeping Orbit up to date.
//!
//! [`Updater`] (a global entity) checks the channel's manifest
//! ([`manifest`]), downloads a newer release in the background
//! ([`download`]), verifies its checksum and signature ([`verify`]) and
//! unpacks it ([`install::stage`]). The title bar then offers "Restart to
//! update"; restarting asks the host to get ready ([`host`]), hands off to
//! the installer helper and quits ([`install`]). The next start says how it
//! went: release notes after an update, or why it was rolled back.
//!
//! Nothing is downloaded or checked when this build has no release key
//! ([`verify::release_key`]); the Updates settings page says so.
//!
//! Preferences live in `<data folder>/updates.json`; downloads, staged
//! bundles and the installer's records under `<data folder>/updates/`.

pub mod download;
#[cfg(test)]
mod fixture;
pub mod host;
pub mod install;
pub mod manifest;
pub mod verify;
mod view;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;
use serde::{Deserialize, Serialize};

pub use manifest::Channel;
use manifest::{Check, Release};
pub use view::{register_actions, settings_page, title_bar_item};

gpui_kit::actions!(
    orbit,
    [
        /// Checks the update channel now.
        CheckForUpdates,
        /// Opens the ready update's notes, with Restart.
        RestartToUpdate,
        /// This version's release notes.
        ShowWhatsNew,
    ]
);

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where each channel's manifest lives; `{channel}` is replaced. A
/// placeholder host (`.invalid` never resolves) until releases are
/// published; `ORBIT_UPDATE_URL` overrides it (a mirror, or a local
/// fixture). See `docs/RELEASING.md`.
pub const MANIFEST_URL: &str = "https://updates.orbit.invalid/{channel}.json";

/// First automatic check after start, then the interval between checks.
const FIRST_CHECK: Duration = Duration::from_secs(15);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How long the new version must have its window up to count as started.
const HEALTHY_AFTER: Duration = Duration::from_secs(5);

const PREFS_FILE: &str = "updates.json";

/// This desktop's update preferences.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdatePrefs {
    pub channel: Channel,
    pub auto_check: bool,
    pub auto_download: bool,
    /// When the channel was last read successfully (ms since the epoch).
    pub last_check_ms: Option<u64>,
    /// A version that was installed and rolled back: not offered again
    /// automatically.
    pub failed_version: Option<String>,
}

impl Default for UpdatePrefs {
    fn default() -> Self {
        Self { channel: Channel::Stable, auto_check: true, auto_download: true, last_check_ms: None, failed_version: None }
    }
}

fn read_prefs(path: &Path) -> UpdatePrefs {
    std::fs::read_to_string(path).ok().and_then(|t| sonic_rs::from_str(&t).ok()).unwrap_or_default()
}

fn write_prefs(path: &Path, prefs: &UpdatePrefs) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, sonic_rs::to_string_pretty(prefs).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, path)
}

/// Where the updater is.
#[derive(Clone)]
pub enum Phase {
    /// This build has no release key: updates are off.
    Unconfigured,
    Idle,
    Checking,
    UpToDate,
    /// Found, not downloading (automatic downloads are off).
    Available(Release),
    Downloading { release: Release, progress: Arc<download::Progress> },
    /// Verified and unpacked: restart to install.
    Ready { release: Release, payload: PathBuf },
    /// Handing over to the installer.
    Restarting,
    Failed { message: String, release: Option<Release> },
}

pub struct Updater {
    data_dir: PathBuf,
    prefs: UpdatePrefs,
    phase: Phase,
    layout: Result<install::Layout, String>,
    _work: Option<Task<()>>,
    _poll: Option<Task<()>>,
    _schedule: Option<Task<()>>,
}

/// Something the user should hear about: the answer to a check they asked
/// for, or a download that failed. Automatic checks stay quiet.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateNotice {
    UpToDate { channel: Channel },
    CheckFailed(String),
    Downloading(String),
    DownloadFailed { version: String, message: String },
}

impl EventEmitter<UpdateNotice> for Updater {}

struct UpdaterGlobal(Entity<Updater>);

impl Global for UpdaterGlobal {}

/// The updater, once [`init`] ran.
pub fn updater(cx: &App) -> Option<Entity<Updater>> {
    cx.try_global::<UpdaterGlobal>().map(|g| g.0.clone())
}

pub fn init(data_dir: &Path, app: Entity<crate::app_state::AppState>, cx: &mut App) {
    host::init(app, cx);
    let data_dir = data_dir.to_path_buf();
    let updater = cx.new(|cx| Updater::new(data_dir, cx));
    cx.set_global(UpdaterGlobal(updater));
}

impl Updater {
    fn new(data_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let prefs = read_prefs(&data_dir.join(PREFS_FILE));
        let configured = verify::release_key().is_some();
        let mut this = Self {
            data_dir,
            prefs,
            phase: if configured { Phase::Idle } else { Phase::Unconfigured },
            layout: install::Layout::current(),
            _work: None,
            _poll: None,
            _schedule: None,
        };
        this.schedule(FIRST_CHECK, cx);
        this
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn prefs(&self) -> &UpdatePrefs {
        &self.prefs
    }

    pub fn is_configured(&self) -> bool {
        !matches!(self.phase, Phase::Unconfigured)
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Changes the preferences and saves them.
    pub fn set_prefs(&mut self, edit: impl FnOnce(&mut UpdatePrefs), cx: &mut Context<Self>) {
        let old = self.prefs.clone();
        edit(&mut self.prefs);
        if self.prefs == old {
            return;
        }
        let (path, prefs) = (self.data_dir.join(PREFS_FILE), self.prefs.clone());
        cx.background_spawn(async move {
            if let Err(err) = write_prefs(&path, &prefs) {
                log::warn!("couldn’t save update preferences: {err}");
            }
        })
        .detach();
        if old.channel != self.prefs.channel {
            // Another channel may offer another release: forget this one.
            if !matches!(self.phase, Phase::Restarting | Phase::Unconfigured) {
                self.phase = Phase::Idle;
                self._work = None;
                self._poll = None;
            }
            self.check(false, cx);
        } else if self.prefs.auto_check && !old.auto_check {
            self.schedule(Duration::ZERO, cx);
        }
        cx.notify();
    }

    fn schedule(&mut self, after: Duration, cx: &mut Context<Self>) {
        if !self.is_configured() {
            return;
        }
        self._schedule = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(after).await;
            loop {
                let keep = this
                    .update(cx, |this, cx| {
                        if this.prefs.auto_check {
                            this.check(false, cx);
                        }
                        this.prefs.auto_check
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
                cx.background_executor().timer(CHECK_EVERY).await;
            }
        }));
    }

    fn manifest_url(&self) -> String {
        let template = std::env::var("ORBIT_UPDATE_URL").ok().filter(|u| !u.trim().is_empty());
        template.as_deref().unwrap_or(MANIFEST_URL).trim().replace("{channel}", self.prefs.channel.id())
    }

    fn platform(&self) -> String {
        let appimage = matches!(self.layout, Ok(install::Layout::AppImage { .. }));
        manifest::platform_key(env!("ORBIT_TARGET"), appimage)
    }

    /// Reads the channel; `manual` when the user asked (then a failure or
    /// "up to date" is worth telling, and a rolled-back version is offered
    /// again).
    pub fn check(&mut self, manual: bool, cx: &mut Context<Self>) {
        match self.phase {
            Phase::Unconfigured
            | Phase::Checking
            | Phase::Downloading { .. }
            | Phase::Ready { .. }
            | Phase::Restarting => return,
            _ => {}
        }
        self.phase = Phase::Checking;
        cx.notify();
        let (url, channel, platform) = (self.manifest_url(), self.prefs.channel, self.platform());
        self._work = Some(cx.spawn(async move |this, cx| {
            let result = download::on_thread("orbit-update-check", move || {
                let text = download::fetch_text(&url)?;
                manifest::evaluate(manifest::parse(&text)?, channel, VERSION, &platform)
            })
            .await;
            let _ = this.update(cx, |this, cx| this.checked(result, manual, cx));
        }));
    }

    fn checked(&mut self, result: Result<Check, String>, manual: bool, cx: &mut Context<Self>) {
        match result {
            Ok(check) => {
                self.set_prefs(|p| p.last_check_ms = Some(orbit_proto::now_ms()), cx);
                match check {
                    Check::Available(release)
                        if manual || self.prefs.failed_version.as_deref() != Some(&release.version.to_string()) =>
                    {
                        if manual || self.prefs.auto_download {
                            if manual {
                                cx.emit(UpdateNotice::Downloading(release.version.to_string()));
                            }
                            self.download(*release, cx);
                        } else {
                            self.phase = Phase::Available(*release);
                        }
                    }
                    _ => {
                        self.phase = Phase::UpToDate;
                        if manual {
                            cx.emit(UpdateNotice::UpToDate { channel: self.prefs.channel });
                        }
                    }
                }
            }
            Err(message) => {
                if manual {
                    cx.emit(UpdateNotice::CheckFailed(message.clone()));
                }
                self.phase = Phase::Failed { message, release: None };
            }
        }
        cx.notify();
    }

    /// Downloads, verifies and stages `release`.
    pub fn download(&mut self, release: Release, cx: &mut Context<Self>) {
        let Some(key) = verify::release_key() else { return };
        let layout = match &self.layout {
            Ok(layout) => layout.clone(),
            Err(message) => {
                self.phase = Phase::Failed { message: message.clone(), release: Some(release) };
                cx.notify();
                return;
            }
        };
        let progress = download::Progress::new();
        self.phase = Phase::Downloading { release: release.clone(), progress: progress.clone() };
        cx.notify();
        let updates = install::updates_dir(&self.data_dir);
        let version = release.version.to_string();
        let file_name = release.artifact.url.rsplit('/').next().unwrap_or("bundle").split('?').next().unwrap_or("bundle");
        let dest = updates.join("downloads").join(format!("{version}-{}", sanitize(file_name)));
        let staged = updates.join("staged").join(&version);
        let notes = notes_path(&self.data_dir, &version);
        let job = release.clone();
        let thread_progress = progress.clone();
        self._work = Some(cx.spawn(async move |this, cx| {
            let result = download::on_thread("orbit-update-download", move || {
                let dir = dest.parent().unwrap_or(&dest);
                std::fs::create_dir_all(dir).map_err(|e| format!("Couldn’t save the download: {e}"))?;
                download::download(&job.artifact.url, &dest, job.artifact.size, &thread_progress)?;
                let verified = verify::verify_file(
                    &dest,
                    &job.artifact.sha256,
                    &job.artifact.signature,
                    &job.version.to_string(),
                    &job.platform,
                    &key,
                );
                if let Err(err) = verified {
                    let _ = std::fs::remove_file(&dest);
                    return Err(err);
                }
                let payload = install::stage(&dest, &layout, &staged)?;
                if let Some(dir) = notes.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&notes, &job.notes);
                let _ = std::fs::remove_file(&dest);
                Ok(payload)
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this._poll = None;
                this.phase = match result {
                    Ok(payload) => Phase::Ready { release, payload },
                    Err(message) if message == "Cancelled." => Phase::Idle,
                    Err(message) => {
                        cx.emit(UpdateNotice::DownloadFailed { version: release.version.to_string(), message: message.clone() });
                        Phase::Failed { message, release: Some(release) }
                    }
                };
                cx.notify();
            });
        }));
        // Progress is read from the shared counters a few times a second,
        // only while downloading.
        self._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
    }

    pub fn cancel_download(&mut self, cx: &mut Context<Self>) {
        if let Phase::Downloading { progress, .. } = &self.phase {
            progress.cancel();
        }
        cx.notify();
    }

    /// Hands off to the installer and quits. Call after the user agreed.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        let Phase::Ready { release, payload } = self.phase.clone() else { return };
        let layout = match &self.layout {
            Ok(layout) => layout.clone(),
            Err(message) => {
                self.phase = Phase::Failed { message: message.clone(), release: Some(release) };
                cx.notify();
                return;
            }
        };
        let swaps = match layout.swaps(&payload) {
            Ok(swaps) => swaps,
            Err(message) => {
                self.phase = Phase::Failed { message, release: Some(release) };
                cx.notify();
                return;
            }
        };
        let version = release.version.to_string();
        let plan = install::Plan {
            data_dir: self.data_dir.clone(),
            from_version: VERSION.to_owned(),
            to_version: version.clone(),
            swaps,
            relaunch: layout.program(),
            args: Vec::new(),
            cleanup: vec![install::updates_dir(&self.data_dir).join("staged").join(&version)],
        };
        self.phase = Phase::Restarting;
        cx.notify();
        let prepare = host::prepare(cx);
        self._work = Some(cx.spawn(async move |this, cx| {
            let ready = prepare.await;
            let handed = ready.and_then(|()| install::hand_off(&plan));
            let _ = this.update(cx, |this, cx| match handed {
                Ok(()) => {
                    crate::diagnostics::note_clean_exit(&this.data_dir);
                    cx.quit();
                }
                Err(message) => {
                    this.phase = Phase::Failed { message, release: Some(release) };
                    cx.notify();
                }
            });
        }));
    }
}

/// Keeps a file name to letters, digits, dots and dashes.
fn sanitize(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect()
}

/// Where a version's release notes are kept after it was downloaded.
fn notes_path(data_dir: &Path, version: &str) -> PathBuf {
    install::updates_dir(data_dir).join("notes").join(format!("{}.md", sanitize(version)))
}

/// The release notes of `version`, if they were downloaded with it.
pub fn notes_for(data_dir: &Path, version: &str) -> Option<String> {
    std::fs::read_to_string(notes_path(data_dir, version)).ok().filter(|n| !n.trim().is_empty())
}

/// Before the app starts: the installer helper's entry point, and the new
/// version's guard against a crash loop. Returns `true` when `main` should
/// exit at once.
pub fn handle_launch(data_dir: &Path) -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(install::HELPER_FLAG) {
        let code = args.get(1).map_or(2, |plan| install::run_helper(Path::new(plan)));
        std::process::exit(code);
    }
    install::boot_guard(data_dir, VERSION) == install::Boot::RolledBack
}

/// Once the window is up: count this start as healthy after a few seconds,
/// and say how the last update went.
pub fn on_window_open(window: &mut Window, cx: &mut App) {
    let Some(updater) = updater(cx) else { return };
    let data_dir = updater.read(cx).data_dir.clone();
    let layout = updater.read(cx).layout.clone();
    let timer = cx.background_executor().timer(HEALTHY_AFTER);
    cx.background_spawn(async move {
        timer.await;
        install::mark_healthy(&data_dir, VERSION);
        if let Ok(layout) = layout {
            install::clean_leftovers(&layout);
        }
    })
    .detach();
    window
        .subscribe(&updater, cx, |_, notice: &UpdateNotice, window, cx| view::announce_notice(notice, window, cx))
        .detach();
    let data_dir = updater.read(cx).data_dir.clone();
    if install::take_just_installed(&data_dir, VERSION).is_some() {
        // Show the notes once the window has settled.
        window.defer(cx, view::open_whats_new);
    }
    if let Some(outcome) = install::take_outcome(&data_dir) {
        if let install::Outcome::RolledBack { to, .. } = &outcome {
            let to = to.clone();
            updater.update(cx, |u, cx| u.set_prefs(|p| p.failed_version = Some(to), cx));
        }
        view::announce_outcome(outcome, window, cx);
    }
    if cfg!(debug_assertions) {
        debug_hooks(&updater, window, cx);
    }
}

/// Debug builds: `ORBIT_DEBUG_UPDATE` = `check` (check now, as if asked),
/// `sheet` (open the notes once an update is ready) or `restart` (install it
/// once ready), for end-to-end checks against a local fixture.
fn debug_hooks(updater: &Entity<Updater>, window: &mut Window, cx: &mut App) {
    let Ok(hook) = std::env::var("ORBIT_DEBUG_UPDATE") else { return };
    if hook == "check" {
        updater.update(cx, |u, cx| u.check(true, cx));
        return;
    }
    let fired = std::rc::Rc::new(std::cell::Cell::new(false));
    window
        .observe(updater, cx, move |updater, window, cx| {
            if fired.get() || !matches!(updater.read(cx).phase(), Phase::Ready { .. }) {
                return;
            }
            fired.set(true);
            match hook.as_str() {
                "sheet" => view::open_release_sheet(window, cx),
                "restart" => updater.update(cx, |u, cx| u.restart(cx)),
                _ => {}
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that would bring in `gpui_kit::test` in place of `#[test]`.
    use super::{Channel, PREFS_FILE, UpdatePrefs, read_prefs, sanitize, write_prefs};

    #[test]
    fn prefs_round_trip_with_defaults_for_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PREFS_FILE);
        assert_eq!(read_prefs(&path), UpdatePrefs::default());
        let prefs = UpdatePrefs { channel: Channel::Nightly, auto_download: false, ..UpdatePrefs::default() };
        write_prefs(&path, &prefs).unwrap();
        assert_eq!(read_prefs(&path), prefs);
        std::fs::write(&path, r#"{"channel":"beta"}"#).unwrap();
        assert_eq!(read_prefs(&path), UpdatePrefs { channel: Channel::Beta, ..UpdatePrefs::default() });
    }

    #[test]
    fn download_names_are_sanitized() {
        assert_eq!(sanitize("orbit-0.2.0 x86_64.zip"), "orbit-0.2.0_x86_64.zip");
        assert_eq!(sanitize("../../evil"), ".._.._evil");
    }
}
