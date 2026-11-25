//! Settings in the engine: persisted like the thread defaults, applied where
//! they matter (provider binaries and environment, the git layer), kept out
//! of the orchestrator's main file.

use std::path::{Path, PathBuf};

use orbit_proto::*;

use super::orchestrator::{CommandResult, Orchestrator};
use crate::git::{Git, GitConfig};

impl Orchestrator {
    /// Replaces the settings and applies what changed. Sessions already
    /// running keep the binary and environment they started with; the next
    /// session of each thread uses the new ones.
    pub(crate) fn update_settings(&mut self, settings: Settings) -> CommandResult {
        let settings = settings.sanitized();
        if *self.store.settings() == settings {
            return Ok(Reply::Done);
        }
        let old = self.store.settings().clone();
        self.emit(Event::SettingsChanged(settings));
        self.apply_settings(&old);
        Ok(Reply::Done)
    }

    fn apply_settings(&mut self, old: &Settings) {
        let new = self.store.settings();
        let git_changed = (&old.workspace.worktree_root, &old.advanced) != (&new.workspace.worktree_root, &new.advanced);
        if git_changed {
            self.git = Git::new(git_config(self.store.dir(), new));
        }
        let changed: Vec<ProviderKind> =
            ProviderKind::ALL.into_iter().filter(|&kind| old.provider(kind) != new.provider(kind)).collect();
        for &kind in &changed {
            self.probe_provider(kind);
        }
        // Instances added or changed (of runtimes not re-probed above), and
        // instances removed, which leave the provider list.
        let new_i = self.store.settings().agents.instances.clone();
        let (old_i, new_i) = (&old.agents.instances, &new_i);
        if old_i != new_i {
            let added: Vec<ProviderInstance> = new_i
                .iter()
                .filter(|i| !changed.contains(&i.kind) && !old_i.contains(i))
                .cloned()
                .collect();
            for instance in &added {
                if let Some(effective) = crate::provider::presets::find(new_i, &instance.id) {
                    self.probe_instance(&effective);
                }
            }
            let gone: Vec<String> = old_i.iter().filter(|o| !new_i.iter().any(|n| n.id == o.id)).map(|o| o.id.clone()).collect();
            for id in gone {
                // A removed override of a preset goes back to the preset.
                match crate::provider::presets::find(new_i, &id) {
                    Some(preset) => self.probe_instance(&preset),
                    None => self.set_provider(ProviderInfo { instance: id, ..Default::default() }),
                }
            }
        }
    }
}

/// The git layer's configuration for `data_dir` under `settings`.
pub(crate) fn git_config(data_dir: &Path, settings: &Settings) -> GitConfig {
    let mut cfg = GitConfig::new(data_dir);
    let path = |s: &str| (!s.is_empty()).then(|| PathBuf::from(s));
    cfg.worktree_root = path(&settings.workspace.worktree_root);
    cfg.git = path(&settings.advanced.git_binary);
    cfg.gh = path(&settings.advanced.gh_binary);
    cfg.max_untracked_file = u64::from(settings.advanced.checkpoint_file_limit_mb) * 1024 * 1024;
    cfg
}


