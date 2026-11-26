//! Installing a verified update, and taking it back if it doesn't start.
//!
//! The running Orbit cannot replace itself, so installing is a hand-off:
//!
//! 1. **Stage.** The verified bundle is unpacked under
//!    `<data>/updates/staged/<version>/`; the files that will replace the
//!    installed ones are its *payload*.
//! 2. **Plan.** `<data>/updates/apply.json` lists each swap (a staged entry
//!    and the installed path it replaces), the program to start afterwards,
//!    and both versions.
//! 3. **Hand off.** Orbit starts a helper, itself run with
//!    `--orbit-apply-update <plan>` (on Windows a copy of the executable in
//!    the data folder, since a running image can be renamed but not removed),
//!    and quits.
//! 4. **Swap.** The helper waits for the data folder's instance lock to come
//!    free (the old Orbit has exited), moves each installed path aside as
//!    `<path>.orbit-old`, moves the staged one in, writes
//!    `updates/pending.json`, and starts the new Orbit.
//! 5. **Health.** The new Orbit writes `updates/healthy` a few seconds after
//!    its window opened, then removes the old copies ([`mark_healthy`]). If
//!    instead it exits before that, the helper puts the old files back and
//!    starts the old Orbit, which says what happened ([`take_outcome`]).
//!    Should the helper be gone, the new Orbit's own [`boot_guard`] rolls
//!    back after three starts that never became healthy.
//!
//! Layouts: a folder of executables (Windows zip, Linux tarball), a macOS
//! `.app` bundle replaced whole, or a Linux AppImage replaced as one file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const HELPER_FLAG: &str = "--orbit-apply-update";
const BACKUP_SUFFIX: &str = ".orbit-old";
const INCOMING_SUFFIX: &str = ".orbit-new";
/// The instance lock (`crate::instance`): held for as long as an Orbit runs
/// on the data folder, released by the OS when it exits.
const INSTANCE_LOCK: &str = "app.lock";
/// How long the helper waits for the old Orbit to exit.
const EXIT_WAIT: Duration = Duration::from_secs(60);
/// How long the helper watches the new Orbit for a crash before leaving it
/// to its own boot guard.
const HEALTH_WAIT: Duration = Duration::from_secs(90);
/// Starts of a new version that never became healthy before it rolls back.
const MAX_UNHEALTHY_STARTS: u32 = 3;

#[cfg(windows)]
const APP_EXE: &str = "orbit-desktop.exe";
#[cfg(not(windows))]
const APP_EXE: &str = "orbit-desktop";

// ── Layout ───────────────────────────────────────────────────────────────

/// How this copy of Orbit is installed.
#[derive(Clone, Debug, PartialEq)]
pub enum Layout {
    /// A folder holding `orbit-desktop`, `orbit` and `orbitd` (Windows,
    /// a Linux tarball, a development build).
    Folder { root: PathBuf },
    /// A macOS application bundle, replaced whole.
    MacBundle { app: PathBuf },
    /// A Linux AppImage, replaced as one file.
    AppImage { file: PathBuf },
}

impl Layout {
    /// The layout of the running executable.
    pub fn current() -> Result<Layout, String> {
        let exe = std::env::current_exe().map_err(|e| format!("Couldn’t find Orbit’s own program: {e}"))?;
        let exe = exe.canonicalize().unwrap_or(exe);
        if cfg!(target_os = "linux")
            && let Some(file) = std::env::var_os("APPIMAGE").map(PathBuf::from).filter(|p| p.is_file())
        {
            return Ok(Layout::AppImage { file });
        }
        Ok(Self::of_exe(&exe))
    }

    fn of_exe(exe: &Path) -> Layout {
        // …/Orbit.app/Contents/MacOS/orbit-desktop
        let app = exe.parent().and_then(Path::parent).and_then(Path::parent);
        if cfg!(target_os = "macos")
            && let Some(app) = app.filter(|a| a.extension().is_some_and(|e| e == "app"))
        {
            return Layout::MacBundle { app: app.to_path_buf() };
        }
        Layout::Folder { root: exe.parent().map(Path::to_path_buf).unwrap_or_default() }
    }

    /// The program that starts this layout's Orbit.
    pub fn program(&self) -> PathBuf {
        match self {
            Layout::Folder { root } => root.join(APP_EXE),
            Layout::MacBundle { app } => app.join("Contents/MacOS").join(APP_EXE),
            Layout::AppImage { file } => file.clone(),
        }
    }

    /// The swaps that put `payload` in place of this installation.
    pub fn swaps(&self, payload: &Path) -> Result<Vec<Swap>, String> {
        let swaps = match self {
            Layout::Folder { root } => {
                let mut swaps = Vec::new();
                for entry in fs::read_dir(payload).map_err(|e| format!("Couldn’t read the update: {e}"))? {
                    let entry = entry.map_err(|e| format!("Couldn’t read the update: {e}"))?;
                    swaps.push(Swap { staged: entry.path(), target: root.join(entry.file_name()) });
                }
                swaps.sort_by(|a, b| a.target.cmp(&b.target));
                swaps
            }
            Layout::MacBundle { app } => vec![Swap { staged: payload.to_path_buf(), target: app.clone() }],
            Layout::AppImage { file } => vec![Swap { staged: payload.to_path_buf(), target: file.clone() }],
        };
        if swaps.is_empty() {
            return Err("The update is empty.".to_owned());
        }
        Ok(swaps)
    }
}

// ── Staging ──────────────────────────────────────────────────────────────

/// Unpacks a verified download into `dir` and returns its payload: for a
/// folder, the directory holding `orbit-desktop`; for a bundle, the `.app`;
/// for an AppImage, the executable file.
pub fn stage(download: &Path, layout: &Layout, dir: &Path) -> Result<PathBuf, String> {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).map_err(|e| format!("Couldn’t prepare the update: {e}"))?;
    if let Layout::AppImage { .. } = layout {
        let file = dir.join("Orbit.AppImage");
        fs::copy(download, &file).map_err(|e| format!("Couldn’t prepare the update: {e}"))?;
        set_executable(&file);
        return Ok(file);
    }
    let extracted = dir.join("files");
    let mut magic = [0u8; 4];
    {
        use std::io::Read as _;
        let mut file = fs::File::open(download).map_err(|e| format!("Couldn’t read the update: {e}"))?;
        let _ = file.read(&mut magic);
    }
    match magic {
        [b'P', b'K', 3, 4] => unzip(download, &extracted)?,
        [0x1f, 0x8b, ..] => untar_gz(download, &extracted)?,
        _ => return Err("The update isn’t a zip or tar.gz archive.".to_owned()),
    }
    let payload = match layout {
        Layout::MacBundle { .. } => find(&extracted, 3, &|p| {
            p.extension().is_some_and(|e| e == "app") && p.join("Contents/MacOS").join(APP_EXE).is_file()
        }),
        _ => find(&extracted, 3, &|p| p.join(APP_EXE).is_file()),
    };
    payload.ok_or_else(|| format!("The update doesn’t contain {APP_EXE} for this platform."))
}

/// The first directory at most `depth` levels under `dir` (itself included)
/// that `accept` accepts, breadth first.
fn find(dir: &Path, depth: usize, accept: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    let mut level = vec![dir.to_path_buf()];
    for _ in 0..=depth {
        if let Some(hit) = level.iter().find(|p| accept(p)) {
            return Some(hit.clone());
        }
        let mut next: Vec<PathBuf> = level
            .iter()
            .filter_map(|d| fs::read_dir(d).ok())
            .flat_map(|entries| entries.flatten().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        next.sort();
        level = next;
    }
    None
}

fn unzip(archive: &Path, into: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| format!("Couldn’t read the update: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("The update archive is damaged: {e}"))?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| format!("The update archive is damaged: {e}"))?;
        // `enclosed_name` refuses absolute paths and `..`: nothing lands
        // outside `into`.
        let Some(name) = entry.enclosed_name() else { return Err("The update archive has an unsafe path.".to_owned()) };
        let path = into.join(name);
        if entry.is_dir() {
            fs::create_dir_all(&path).map_err(|e| format!("Couldn’t unpack the update: {e}"))?;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Couldn’t unpack the update: {e}"))?;
        }
        let mut out = fs::File::create(&path).map_err(|e| format!("Couldn’t unpack the update: {e}"))?;
        io::copy(&mut entry, &mut out).map_err(|e| format!("Couldn’t unpack the update: {e}"))?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o7777));
        }
    }
    Ok(())
}

/// Unpacks a `.tar.gz` into `into` (Linux updates; `orbitd` builds for
/// other computers on every platform).
pub fn untar_gz(archive: &Path, into: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| format!("Couldn’t read the update: {e}"))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    fs::create_dir_all(into).map_err(|e| format!("Couldn’t unpack the update: {e}"))?;
    for entry in tar.entries().map_err(|e| format!("The update archive is damaged: {e}"))? {
        let mut entry = entry.map_err(|e| format!("The update archive is damaged: {e}"))?;
        // `unpack_in` refuses paths that leave `into`.
        if !entry.unpack_in(into).map_err(|e| format!("Couldn’t unpack the update: {e}"))? {
            return Err("The update archive has an unsafe path.".to_owned());
        }
    }
    Ok(())
}

fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    }
    let _ = path;
}

// ── Swaps ────────────────────────────────────────────────────────────────

/// One installed path and what replaces it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Swap {
    pub staged: PathBuf,
    pub target: PathBuf,
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

impl Swap {
    fn backup(&self) -> PathBuf {
        suffixed(&self.target, BACKUP_SUFFIX)
    }

    fn incoming(&self) -> PathBuf {
        suffixed(&self.target, INCOMING_SUFFIX)
    }
}

fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    }
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(fs::read_link(from)?, to);
    }
    if meta.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

/// Moves `from` to `to`, copying when they are on different volumes.
fn move_path(from: &Path, to: &Path) -> io::Result<()> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_tree(from, to)?;
    let _ = remove_path(from);
    Ok(())
}

/// Puts every staged path in place, keeping the installed ones as backups.
/// All or nothing: a failure puts back what was already swapped.
pub fn apply_swaps(swaps: &[Swap]) -> Result<(), String> {
    // First bring every new path next to its target (same volume), so the
    // swaps themselves are renames that cannot run out of space halfway.
    for swap in swaps {
        let incoming = swap.incoming();
        let _ = remove_path(&incoming);
        let _ = remove_path(&swap.backup());
        if let Err(err) = move_path(&swap.staged, &incoming) {
            for s in swaps {
                let _ = remove_path(&s.incoming());
            }
            return Err(format!("Couldn’t write {}: {err}", incoming.display()));
        }
    }
    for (done, swap) in swaps.iter().enumerate() {
        let step = (|| -> io::Result<()> {
            if fs::symlink_metadata(&swap.target).is_ok() {
                fs::rename(&swap.target, swap.backup())?;
            }
            fs::rename(swap.incoming(), &swap.target)
        })();
        if let Err(err) = step {
            let _ = restore_swaps(&swaps[..=done]);
            for s in swaps {
                let _ = remove_path(&s.incoming());
            }
            return Err(format!("Couldn’t replace {}: {err}", swap.target.display()));
        }
    }
    Ok(())
}

/// Puts the backups back (newest swap first).
pub fn restore_swaps(swaps: &[Swap]) -> Result<(), String> {
    let mut failed = None;
    for swap in swaps.iter().rev() {
        let backup = swap.backup();
        if fs::symlink_metadata(&backup).is_err() {
            // Nothing was there before: the swap added this path.
            if fs::symlink_metadata(&swap.target).is_ok() && fs::symlink_metadata(swap.incoming()).is_err() {
                let _ = remove_path(&swap.target);
            }
            continue;
        }
        let _ = remove_path(&swap.target);
        if let Err(err) = fs::rename(&backup, &swap.target) {
            failed = Some(format!("Couldn’t restore {}: {err}", swap.target.display()));
        }
    }
    failed.map_or(Ok(()), Err)
}

/// Removes the backups once the new version is known to work. A backup that
/// is still in use (an `orbitd` that kept running) is left for the next
/// start's [`clean_leftovers`].
fn discard_backups(swaps: &[Swap]) {
    for swap in swaps {
        let _ = remove_path(&swap.backup());
        let _ = remove_path(&swap.incoming());
    }
}

/// Removes `.orbit-old` and `.orbit-new` leftovers beside this installation.
pub fn clean_leftovers(layout: &Layout) {
    let dir = match layout {
        Layout::Folder { root } => Some(root.clone()),
        Layout::MacBundle { app } | Layout::AppImage { file: app } => app.parent().map(Path::to_path_buf),
    };
    let Some(dir) = dir else { return };
    let Ok(entries) = fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(BACKUP_SUFFIX) || name.ends_with(INCOMING_SUFFIX) {
            let _ = remove_path(&entry.path());
        }
    }
}

// ── Records in the data folder ───────────────────────────────────────────

pub fn updates_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("updates")
}

/// What the helper is to do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub data_dir: PathBuf,
    pub from_version: String,
    pub to_version: String,
    pub swaps: Vec<Swap>,
    /// The program to start afterwards (the same path, now the new version).
    pub relaunch: PathBuf,
    pub args: Vec<String>,
    /// Staged files and downloads to remove once installed.
    pub cleanup: Vec<PathBuf>,
}

/// A swap done and not yet confirmed healthy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub plan: Plan,
    /// Starts of the new version so far.
    pub starts: u32,
    /// The new version has shown its release notes.
    #[serde(default)]
    pub announced: bool,
}

/// An update that didn't happen, for the next Orbit to say. (One that did
/// is told by the new version itself: [`take_just_installed`].)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    RolledBack { from: String, to: String, reason: String },
    Failed { to: String, reason: String },
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = sonic_rs::to_string_pretty(value).map_err(io::Error::other)?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(tmp, path)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    sonic_rs::from_str(&fs::read_to_string(path).ok()?).ok()
}

fn pending_path(data_dir: &Path) -> PathBuf {
    updates_dir(data_dir).join("pending.json")
}

fn outcome_path(data_dir: &Path) -> PathBuf {
    updates_dir(data_dir).join("outcome.json")
}

fn healthy_path(data_dir: &Path) -> PathBuf {
    updates_dir(data_dir).join("healthy")
}

pub fn plan_path(data_dir: &Path) -> PathBuf {
    updates_dir(data_dir).join("apply.json")
}

fn record_outcome(data_dir: &Path, outcome: &Outcome) {
    if let Err(err) = write_json(&outcome_path(data_dir), outcome) {
        log::warn!("couldn’t record the update outcome: {err}");
    }
}

/// How the last update went, once: the record is removed as it is read.
pub fn take_outcome(data_dir: &Path) -> Option<Outcome> {
    let path = outcome_path(data_dir);
    let outcome = read_json(&path);
    let _ = fs::remove_file(path);
    outcome
}

// ── The running app's side ───────────────────────────────────────────────

/// Writes the plan and starts the helper. The caller quits right after.
pub fn hand_off(plan: &Plan) -> Result<(), String> {
    let data_dir = &plan.data_dir;
    let plan_file = plan_path(data_dir);
    write_json(&plan_file, plan).map_err(|e| format!("Couldn’t prepare the update: {e}"))?;
    let _ = fs::remove_file(healthy_path(data_dir));
    let exe = std::env::current_exe().map_err(|e| format!("Couldn’t find Orbit’s own program: {e}"))?;
    // Windows can rename a running image but not delete it: the helper runs
    // from a copy, so the installation's own files are free to move.
    let helper = if cfg!(windows) {
        let helper = updates_dir(data_dir).join("helper").join(exe.file_name().unwrap_or_default());
        fs::create_dir_all(helper.parent().unwrap_or(data_dir)).map_err(|e| format!("Couldn’t prepare the update: {e}"))?;
        fs::copy(&exe, &helper).map_err(|e| format!("Couldn’t prepare the update: {e}"))?;
        helper
    } else {
        exe
    };
    spawn_detached(&helper, &[HELPER_FLAG.into(), plan_file.display().to_string()])
        .map(drop)
        .map_err(|e| format!("Couldn’t start the installer: {e}"))
}

fn spawn_detached(program: &Path, args: &[String]) -> io::Result<std::process::Child> {
    let mut command = Command::new(program);
    command.args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command.spawn()
}

/// What `main` does before anything else when an update is in flight.
#[derive(Debug, PartialEq)]
pub enum Boot {
    Continue,
    /// The new version never became healthy and was rolled back; the old
    /// one has been started. Exit.
    RolledBack,
}

/// Counts starts of a just-installed version; after
/// [`MAX_UNHEALTHY_STARTS`] that never became healthy, puts the old version
/// back and starts it. Also clears records left by an old version.
pub fn boot_guard(data_dir: &Path, version: &str) -> Boot {
    let path = pending_path(data_dir);
    let Some(mut pending) = read_json::<Pending>(&path) else { return Boot::Continue };
    if pending.plan.to_version != version {
        // The old version is running again (rolled back by the helper), or
        // the user installed another build by hand.
        let _ = fs::remove_file(&path);
        return Boot::Continue;
    }
    pending.starts += 1;
    if pending.starts <= MAX_UNHEALTHY_STARTS {
        let _ = write_json(&path, &pending);
        return Boot::Continue;
    }
    let reason = format!("Orbit {version} didn’t finish starting {MAX_UNHEALTHY_STARTS} times.");
    match restore_swaps(&pending.plan.swaps) {
        Ok(()) => {
            let _ = fs::remove_file(&path);
            record_outcome(
                data_dir,
                &Outcome::RolledBack { from: pending.plan.from_version.clone(), to: version.to_owned(), reason },
            );
            let _ = spawn_detached(&pending.plan.relaunch, &pending.plan.args);
            Boot::RolledBack
        }
        Err(err) => {
            log::error!("couldn’t roll back the update: {err}");
            Boot::Continue
        }
    }
}

/// The new version started properly: keep it, and tidy up.
pub fn mark_healthy(data_dir: &Path, version: &str) {
    let _ = fs::create_dir_all(updates_dir(data_dir));
    let _ = fs::write(healthy_path(data_dir), version);
    let path = pending_path(data_dir);
    let Some(pending) = read_json::<Pending>(&path) else { return };
    if pending.plan.to_version != version {
        return;
    }
    discard_backups(&pending.plan.swaps);
    for leftover in &pending.plan.cleanup {
        let _ = remove_path(leftover);
    }
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(plan_path(data_dir));
}

/// When this version was just installed by the updater and hasn't said so
/// yet: the version it replaced. Says it once.
pub fn take_just_installed(data_dir: &Path, version: &str) -> Option<String> {
    let path = pending_path(data_dir);
    let mut pending = read_json::<Pending>(&path)?;
    if pending.plan.to_version != version || pending.announced {
        return None;
    }
    pending.announced = true;
    let _ = write_json(&path, &pending);
    Some(pending.plan.from_version)
}

// ── The helper ───────────────────────────────────────────────────────────

/// Waits until no Orbit holds the data folder.
fn wait_for_exit(data_dir: &Path, timeout: Duration) -> bool {
    let started = Instant::now();
    let Ok(lock) = fs::OpenOptions::new().create(true).truncate(false).write(true).open(data_dir.join(INSTANCE_LOCK))
    else {
        return false;
    };
    loop {
        if lock.try_lock().is_ok() {
            let _ = lock.unlock();
            return true;
        }
        if started.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `orbit-desktop --orbit-apply-update <plan>`: installs and watches.
/// Returns the process exit code.
pub fn run_helper(plan_file: &Path) -> i32 {
    let Some(plan) = read_json::<Plan>(plan_file) else { return 2 };
    run_plan(&plan, HEALTH_WAIT)
}

fn run_plan(plan: &Plan, health_wait: Duration) -> i32 {
    let data_dir = &plan.data_dir;
    if !wait_for_exit(data_dir, EXIT_WAIT) {
        record_outcome(
            data_dir,
            &Outcome::Failed { to: plan.to_version.clone(), reason: "Orbit didn’t quit, so the update wasn’t installed.".into() },
        );
        return 3;
    }
    if let Err(reason) = apply_swaps(&plan.swaps) {
        record_outcome(data_dir, &Outcome::Failed { to: plan.to_version.clone(), reason });
        let _ = spawn_detached(&plan.relaunch, &plan.args);
        return 4;
    }
    let _ = fs::remove_file(healthy_path(data_dir));
    if let Err(err) = write_json(&pending_path(data_dir), &Pending { plan: plan.clone(), starts: 0, announced: false }) {
        log::warn!("couldn’t record the pending update: {err}");
    }
    let mut child = match spawn_detached(&plan.relaunch, &plan.args) {
        Ok(child) => child,
        Err(err) => {
            roll_back(plan, format!("Orbit {} couldn’t be started: {err}", plan.to_version));
            return 5;
        }
    };
    let started = Instant::now();
    while started.elapsed() < health_wait {
        if fs::read_to_string(healthy_path(data_dir)).is_ok_and(|v| v.trim() == plan.to_version) {
            return 0;
        }
        if let Ok(Some(status)) = child.try_wait() {
            // It may have handed over to another instance and exited
            // cleanly after becoming healthy; check once more.
            if fs::read_to_string(healthy_path(data_dir)).is_ok_and(|v| v.trim() == plan.to_version) {
                return 0;
            }
            roll_back(plan, format!("Orbit {} quit while starting ({status}).", plan.to_version));
            return 6;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Still running and not healthy yet (a slow first start): its own boot
    // guard takes over from here.
    0
}

fn roll_back(plan: &Plan, reason: String) {
    let data_dir = &plan.data_dir;
    match restore_swaps(&plan.swaps) {
        Ok(()) => {
            let _ = fs::remove_file(pending_path(data_dir));
            record_outcome(
                data_dir,
                &Outcome::RolledBack { from: plan.from_version.clone(), to: plan.to_version.clone(), reason },
            );
        }
        Err(err) => record_outcome(data_dir, &Outcome::Failed { to: plan.to_version.clone(), reason: format!("{reason} {err}") }),
    }
    let _ = spawn_detached(&plan.relaunch, &plan.args);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    /// An installed folder with two programs, and a payload replacing one
    /// and adding a file.
    fn folder_fixture() -> (tempfile::TempDir, Layout, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("install");
        write(&root.join(APP_EXE), "old app");
        write(&root.join("orbitd"), "old daemon");
        let payload = dir.path().join("payload");
        write(&payload.join(APP_EXE), "new app");
        write(&payload.join("README.md"), "new readme");
        (dir, Layout::Folder { root }, payload)
    }

    #[test]
    fn swaps_install_and_restore_exactly() {
        let (_dir, layout, payload) = folder_fixture();
        let Layout::Folder { root } = &layout else { unreachable!() };
        let swaps = layout.swaps(&payload).unwrap();
        assert_eq!(swaps.len(), 2);
        apply_swaps(&swaps).unwrap();
        assert_eq!(read(&root.join(APP_EXE)), "new app");
        assert_eq!(read(&root.join("README.md")), "new readme");
        assert_eq!(read(&root.join("orbitd")), "old daemon");
        assert_eq!(read(&suffixed(&root.join(APP_EXE), BACKUP_SUFFIX)), "old app");
        restore_swaps(&swaps).unwrap();
        assert_eq!(read(&root.join(APP_EXE)), "old app");
        assert!(!root.join("README.md").exists(), "an added file goes away on rollback");
        let names: Vec<String> = fs::read_dir(root).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }

    #[test]
    fn a_failed_swap_leaves_the_installation_as_it_was() {
        let (dir, layout, payload) = folder_fixture();
        let Layout::Folder { root } = &layout else { unreachable!() };
        let mut swaps = layout.swaps(&payload).unwrap();
        // A staged path that doesn't exist fails the first phase.
        swaps.push(Swap { staged: dir.path().join("missing"), target: root.join("zzz") });
        assert!(apply_swaps(&swaps).is_err());
        assert_eq!(read(&root.join(APP_EXE)), "old app");
        assert!(!root.join("README.md").exists());
    }

    #[test]
    fn mac_bundles_and_appimages_swap_whole() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Orbit.app");
        write(&app.join("Contents/MacOS").join(APP_EXE), "old");
        let staged = dir.path().join("stage/Orbit.app");
        write(&staged.join("Contents/MacOS").join(APP_EXE), "new");
        let layout = Layout::MacBundle { app: app.clone() };
        let swaps = layout.swaps(&staged).unwrap();
        apply_swaps(&swaps).unwrap();
        assert_eq!(read(&layout.program()), "new");
        restore_swaps(&swaps).unwrap();
        assert_eq!(read(&layout.program()), "old");

        let file = dir.path().join("Orbit-x86_64.AppImage");
        write(&file, "old image");
        let image = dir.path().join("new.AppImage");
        write(&image, "new image");
        let layout = Layout::AppImage { file: file.clone() };
        apply_swaps(&layout.swaps(&image).unwrap()).unwrap();
        assert_eq!(read(&file), "new image");
    }

    fn zip_bundle(path: &Path, top: &str, files: &[(&str, &str)]) {
        let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
        for (name, text) in files {
            zip.start_file(format!("{top}/{name}"), options).unwrap();
            io::Write::write_all(&mut zip, text.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn staging_finds_the_payload_in_zips_and_tarballs() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::Folder { root: dir.path().join("install") };
        let archive = dir.path().join("orbit.zip");
        zip_bundle(&archive, "orbit-0.2.0-x86_64-pc-windows-msvc", &[(APP_EXE, "new app"), ("orbit", "cli")]);
        let payload = stage(&archive, &layout, &dir.path().join("staged")).unwrap();
        assert_eq!(read(&payload.join(APP_EXE)), "new app");
        assert_eq!(payload.file_name().unwrap(), "orbit-0.2.0-x86_64-pc-windows-msvc");

        let tarball = dir.path().join("orbit.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(fs::File::create(&tarball).unwrap(), flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            let mut header = tar::Header::new_gnu();
            header.set_size(7);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, format!("orbit-0.2.0/{APP_EXE}"), &b"tar app"[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let payload = stage(&tarball, &layout, &dir.path().join("staged2")).unwrap();
        assert_eq!(read(&payload.join(APP_EXE)), "tar app");

        // An archive without the app is refused.
        let wrong = dir.path().join("wrong.zip");
        zip_bundle(&wrong, "x", &[("readme", "hi")]);
        assert!(stage(&wrong, &layout, &dir.path().join("staged3")).is_err());
        // So is something that isn't an archive.
        let junk = dir.path().join("junk.bin");
        write(&junk, "not an archive");
        assert!(stage(&junk, &layout, &dir.path().join("staged4")).is_err());
    }

    #[test]
    fn zip_slip_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("evil.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
        zip.start_file("../evil.txt", zip::write::SimpleFileOptions::default()).unwrap();
        io::Write::write_all(&mut zip, b"x").unwrap();
        zip.finish().unwrap();
        let layout = Layout::Folder { root: dir.path().join("install") };
        assert!(stage(&archive, &layout, &dir.path().join("staged")).is_err());
        assert!(!dir.path().join("evil.txt").exists());
    }

    fn plan(dir: &Path, layout: &Layout, payload: &Path, relaunch: PathBuf) -> Plan {
        Plan {
            data_dir: dir.join("data"),
            from_version: "0.1.0".into(),
            to_version: "0.2.0".into(),
            swaps: layout.swaps(payload).unwrap(),
            relaunch,
            args: vec![],
            cleanup: vec![],
        }
    }

    /// A program that exits at once: a new version that crashes on start.
    fn crashing_program() -> PathBuf {
        if cfg!(windows) {
            return PathBuf::from(std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into()));
        }
        // macOS has only `/usr/bin/false`; some Linux systems only `/bin/false`.
        ["/usr/bin/false", "/bin/false"].into_iter().map(PathBuf::from).find(|p| p.exists()).expect("a `false` program")
    }

    #[test]
    fn the_helper_rolls_back_a_version_that_quits_while_starting() {
        let (dir, layout, payload) = folder_fixture();
        let Layout::Folder { root } = &layout else { unreachable!() };
        // `cmd.exe` with no console and stdin closed exits at once, as
        // does `false`.
        let plan = plan(dir.path(), &layout, &payload, crashing_program());
        fs::create_dir_all(&plan.data_dir).unwrap();
        let code = run_plan(&plan, Duration::from_secs(20));
        assert_eq!(code, 6);
        assert_eq!(read(&root.join(APP_EXE)), "old app");
        assert!(!root.join("README.md").exists());
        match take_outcome(&plan.data_dir) {
            Some(Outcome::RolledBack { from, to, .. }) => assert_eq!((from.as_str(), to.as_str()), ("0.1.0", "0.2.0")),
            other => panic!("expected a rollback, got {other:?}"),
        }
        assert!(take_outcome(&plan.data_dir).is_none(), "an outcome is told once");
        assert!(!pending_path(&plan.data_dir).exists());
    }

    #[test]
    fn the_helper_keeps_a_version_that_reports_healthy() {
        let (dir, layout, payload) = folder_fixture();
        let Layout::Folder { root } = &layout else { unreachable!() };
        // A "new version" that stays up a few seconds; the healthy flag is
        // written for it meanwhile.
        let (sleeper, args): (&str, &[&str]) =
            if cfg!(windows) { ("ping.exe", &["-n", "6", "127.0.0.1"]) } else { ("/bin/sleep", &["5"]) };
        let mut plan = plan(dir.path(), &layout, &payload, PathBuf::from(sleeper));
        plan.args = args.iter().map(|a| a.to_string()).collect();
        fs::create_dir_all(&plan.data_dir).unwrap();
        let data = plan.data_dir.clone();
        let marker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            mark_healthy(&data, "0.2.0");
        });
        let code = run_plan(&plan, Duration::from_secs(20));
        marker.join().unwrap();
        assert_eq!(code, 0);
        assert_eq!(read(&root.join(APP_EXE)), "new app");
        assert!(!suffixed(&root.join(APP_EXE), BACKUP_SUFFIX).exists(), "backups go once healthy");
        assert!(!pending_path(&plan.data_dir).exists());
        assert_eq!(take_outcome(&plan.data_dir), None, "nothing went wrong to tell");
    }

    #[test]
    fn the_boot_guard_rolls_back_after_repeated_unhealthy_starts() {
        let (dir, layout, payload) = folder_fixture();
        let Layout::Folder { root } = &layout else { unreachable!() };
        let plan = plan(dir.path(), &layout, &payload, crashing_program());
        apply_swaps(&plan.swaps).unwrap();
        write_json(&pending_path(&plan.data_dir), &Pending { plan: plan.clone(), starts: 0, announced: false }).unwrap();
        assert_eq!(take_just_installed(&plan.data_dir, "0.2.0").as_deref(), Some("0.1.0"));
        assert_eq!(take_just_installed(&plan.data_dir, "0.2.0"), None, "told once");
        for _ in 0..MAX_UNHEALTHY_STARTS {
            assert_eq!(boot_guard(&plan.data_dir, "0.2.0"), Boot::Continue);
        }
        assert_eq!(read(&root.join(APP_EXE)), "new app");
        assert_eq!(boot_guard(&plan.data_dir, "0.2.0"), Boot::RolledBack);
        assert_eq!(read(&root.join(APP_EXE)), "old app");
        assert!(matches!(take_outcome(&plan.data_dir), Some(Outcome::RolledBack { .. })));
        // The old version starting clears nothing it shouldn't.
        assert_eq!(boot_guard(&plan.data_dir, "0.1.0"), Boot::Continue);
    }

    #[test]
    fn the_helper_waits_for_the_running_orbit_to_quit() {
        let dir = tempfile::tempdir().unwrap();
        let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.path().join(INSTANCE_LOCK)).unwrap();
        lock.lock().unwrap();
        assert!(!wait_for_exit(dir.path(), Duration::from_millis(300)));
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(lock);
        });
        assert!(wait_for_exit(dir.path(), Duration::from_secs(5)));
        release.join().unwrap();
    }

    #[test]
    fn layouts_are_recognized() {
        let folder = Layout::of_exe(Path::new("/opt/orbit/orbit-desktop"));
        if cfg!(target_os = "macos") {
            assert_eq!(
                Layout::of_exe(Path::new("/Applications/Orbit.app/Contents/MacOS/orbit-desktop")),
                Layout::MacBundle { app: "/Applications/Orbit.app".into() }
            );
        }
        assert_eq!(folder, Layout::Folder { root: "/opt/orbit".into() });
        assert_eq!(suffixed(Path::new("/a/b.exe"), ".orbit-old"), PathBuf::from("/a/b.exe.orbit-old"));
    }
}
