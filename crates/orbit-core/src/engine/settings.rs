//! Settings in the engine: persisted like the thread defaults, applied where
//! they matter (provider binaries and environment, the git layer), kept out
//! of the orchestrator's main file.

use std::path::{Path, PathBuf};

use crossfire::BlockingTxTrait;
use orbit_proto::*;

use super::orchestrator::{CommandResult, Internal, Orchestrator};
use crate::git::{Git, GitConfig};
use crate::provider;

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
        for kind in changed {
            self.probe_provider(kind);
        }
    }

    /// Probes one provider with its configured binary and environment.
    pub(crate) fn probe_provider(&self, kind: ProviderKind) {
        let (binary, env) = provider_launch(self.store.settings(), kind);
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let info = provider::probe(kind, binary, env).await;
            let _ = tx.send(Internal::Probed(info));
        });
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

/// The binary override and extra environment a provider is started with.
pub(crate) fn provider_launch(settings: &Settings, kind: ProviderKind) -> (Option<PathBuf>, Vec<(String, String)>) {
    let provider = settings.provider(kind);
    let binary = (!provider.binary.is_empty()).then(|| PathBuf::from(&provider.binary));
    (binary, provider.env_pairs())
}
