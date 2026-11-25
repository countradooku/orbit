//! This computer's shell preferences: what the window remembers about how
//! the user arranged it, and choices that belong to this desktop rather than
//! to the engine (a remote host's other clients should not start beeping).
//!
//! Kept in `<data folder>/shell.json`, written atomically and only when a
//! value actually changes.

use std::path::{Path, PathBuf};

use gpui_kit::*;
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "shell.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellPrefs {
    /// A short system sound when an agent needs you or fails while Orbit is
    /// in the background.
    pub sounds: bool,
    /// Sidebar grouped by project rather than by what threads need.
    pub group_by_project: bool,
    /// Sidebar sections the user closed (open by default) or opened (the
    /// Snoozed and Settled shelves), by name.
    pub toggled_sections: Vec<String>,
}

impl Default for ShellPrefs {
    fn default() -> Self {
        Self { sounds: true, group_by_project: false, toggled_sections: Vec::new() }
    }
}

struct Prefs {
    path: PathBuf,
    current: ShellPrefs,
}

impl Global for Prefs {}

/// Reads the preferences; a missing or unreadable file means defaults.
pub fn init(data_dir: &Path, cx: &mut App) {
    let path = data_dir.join(FILE_NAME);
    let current = read(&path);
    cx.set_global(Prefs { path, current });
}

pub fn get(cx: &App) -> &ShellPrefs {
    static DEFAULT: std::sync::OnceLock<ShellPrefs> = std::sync::OnceLock::new();
    match cx.try_global::<Prefs>() {
        Some(prefs) => &prefs.current,
        None => DEFAULT.get_or_init(ShellPrefs::default),
    }
}

/// Changes the preferences and saves them if anything changed.
pub fn update(cx: &mut App, edit: impl FnOnce(&mut ShellPrefs)) {
    let Some(prefs) = cx.try_global::<Prefs>() else { return };
    let mut next = prefs.current.clone();
    edit(&mut next);
    if next == prefs.current {
        return;
    }
    let path = prefs.path.clone();
    cx.global_mut::<Prefs>().current = next.clone();
    // Small and rare: written on the spot, off the UI thread.
    cx.background_spawn(async move {
        if let Err(err) = write(&path, &next) {
            log::warn!("couldn’t save {}: {err}", path.display());
        }
    })
    .detach();
    cx.refresh_windows();
}

fn read(path: &Path) -> ShellPrefs {
    std::fs::read_to_string(path).ok().and_then(|text| sonic_rs::from_str(&text).ok()).unwrap_or_default()
}

fn write(path: &Path, prefs: &ShellPrefs) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = sonic_rs::to_string_pretty(prefs).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::{ShellPrefs, read, write};

    #[test]
    fn prefs_round_trip_and_tolerate_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shell.json");
        assert_eq!(read(&path), ShellPrefs::default());
        let prefs = ShellPrefs { sounds: false, group_by_project: true, toggled_sections: vec!["Settled".into()] };
        write(&path, &prefs).unwrap();
        assert_eq!(read(&path), prefs);
        // A file from an older build with fewer fields keeps the rest at defaults.
        std::fs::write(&path, r#"{"sounds": false}"#).unwrap();
        assert_eq!(read(&path), ShellPrefs { sounds: false, ..ShellPrefs::default() });
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(read(&path), ShellPrefs::default());
    }
}
