//! The installed speech models and the downloads in progress.
//!
//! Downloads start only when the user asks (Settings › Voice, or the setup
//! sheet on first use). Each runs on its own thread over HTTPS from Hugging
//! Face, resumes after a failure or a restart, and is checked against the
//! catalog's SHA-256 before it is installed (see `orbit_voice::store`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;
use orbit_voice::catalog::ModelInfo;
use orbit_voice::store::{Chunk, Fetch, InstallError, Progress, Status, Store};

/// Fetches over HTTPS with the updater's client.
struct Https;

impl Fetch for Https {
    fn fetch(&self, url: &str, offset: u64, sink: &mut dyn FnMut(Chunk<'_>) -> Result<(), String>) -> Result<(), String> {
        crate::update::download::fetch_from(url, offset, sink)
    }
}

pub enum ModelsEvent {
    Installed(&'static str),
}

pub struct Models {
    store: Store,
    /// Downloads in progress, by model id.
    installing: HashMap<&'static str, (Arc<Progress>, Task<()>)>,
    /// Why the last download of a model failed.
    errors: HashMap<&'static str, String>,
    /// Status per model, read from disk when something changes.
    status: HashMap<&'static str, Status>,
    fetch: Arc<dyn Fetch>,
}

impl EventEmitter<ModelsEvent> for Models {}

impl Models {
    pub fn new(store: Store, _: &mut Context<Self>) -> Self {
        let mut models =
            Self { store, installing: HashMap::new(), errors: HashMap::new(), status: HashMap::new(), fetch: Arc::new(Https) };
        models.rescan();
        models
    }

    /// Models from another source, for tests.
    #[cfg(test)]
    pub fn with_fetch(store: Store, fetch: Arc<dyn Fetch>) -> Self {
        let mut models = Self { store, installing: HashMap::new(), errors: HashMap::new(), status: HashMap::new(), fetch };
        models.rescan();
        models
    }

    fn rescan(&mut self) {
        for model in orbit_voice::catalog::MODELS {
            self.status.insert(model.id, self.store.status(model));
        }
    }

    pub fn status(&self, model: &ModelInfo) -> Status {
        self.status.get(model.id).copied().unwrap_or(Status::Missing)
    }

    pub fn is_installed(&self, model: &ModelInfo) -> bool {
        self.status(model) == Status::Installed
    }

    pub fn paths(&self, model: &ModelInfo) -> Option<(PathBuf, PathBuf)> {
        if self.is_installed(model) { self.store.paths(model) } else { None }
    }

    /// The download in progress, if any.
    pub fn progress(&self, model: &ModelInfo) -> Option<Arc<Progress>> {
        self.installing.get(model.id).map(|i| i.0.clone())
    }

    pub fn error(&self, model: &ModelInfo) -> Option<&str> {
        self.errors.get(model.id).map(String::as_str)
    }

    pub fn install(&mut self, model: &'static ModelInfo, cx: &mut Context<Self>) {
        if self.installing.contains_key(model.id) || self.is_installed(model) {
            return;
        }
        self.errors.remove(model.id);
        let progress = Arc::new(Progress::default());
        let (store, fetch, work) = (self.store.clone(), self.fetch.clone(), progress.clone());
        let task = cx.spawn(async move |this, cx| {
            let install = crate::update::download::on_thread("orbit-model-download", move || {
                Ok(store.install(model, fetch.as_ref(), &work))
            });
            // Redraw the progress while the download runs.
            let watch = {
                let this = this.clone();
                cx.spawn(async move |cx| {
                    loop {
                        cx.background_executor().timer(Duration::from_millis(200)).await;
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            break;
                        }
                    }
                })
            };
            let result = install.await;
            drop(watch);
            let _ = this.update(cx, |this, cx| {
                this.installing.remove(model.id);
                match result {
                    Ok(Ok(())) => {
                        log::info!("voice: installed {}", model.id);
                        cx.emit(ModelsEvent::Installed(model.id));
                    }
                    Ok(Err(InstallError::Cancelled)) => {}
                    Ok(Err(e)) => {
                        this.errors.insert(model.id, e.to_string());
                    }
                    Err(e) => {
                        this.errors.insert(model.id, e);
                    }
                }
                this.status.insert(model.id, this.store.status(model));
                cx.notify();
            });
        });
        self.installing.insert(model.id, (progress, task));
        cx.notify();
    }

    /// Stops a download; what arrived is kept to resume.
    pub fn cancel(&mut self, model: &ModelInfo, cx: &mut Context<Self>) {
        if let Some((progress, _)) = self.installing.get(model.id) {
            progress.cancel();
        }
        cx.notify();
    }

    /// Removes an installed model, or the kept part of a download.
    pub fn delete(&mut self, model: &ModelInfo, cx: &mut Context<Self>) -> Result<(), String> {
        if self.installing.contains_key(model.id) {
            return Err("Cancel the download first.".into());
        }
        let result = self.store.delete(model).map_err(|e| format!("The model couldn’t be deleted: {e}"));
        self.errors.remove(model.id);
        self.status.insert(model.id, self.store.status(model));
        cx.notify();
        result
    }
}

/// `1.2 GB`, `293 MB`.
pub fn size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{} MB", (bytes as f64 / 1e6).round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use gpui_kit::{AppContext as _, TestAppContext};
    use orbit_voice::store::{Chunk, Fetch, Store};

    use super::{Models, size};

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size(292_807_567), "293 MB");
        assert_eq!(size(1_620_535_201), "1.6 GB");
    }

    #[gpui_kit::test]
    fn a_download_that_fails_its_checksum_reports_and_installs_nothing(cx: &mut TestAppContext) {
        // A fake source answering every catalog URL with the wrong bytes.
        struct Wrong;
        impl Fetch for Wrong {
            fn fetch(&self, _: &str, _: u64, sink: &mut dyn FnMut(Chunk<'_>) -> Result<(), String>) -> Result<(), String> {
                sink(Chunk::Start { resumed: false })?;
                sink(Chunk::Data(b"not a model"))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let models = cx.new(|_| Models::with_fetch(store, Arc::new(Wrong)));
        let tiny = orbit_voice::catalog::find("tiny.en").unwrap();
        models.update(cx, |m, cx| m.install(tiny, cx));
        cx.run_until_parked();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while models.read_with(cx, |m, _| m.progress(tiny).is_some()) && std::time::Instant::now() < deadline {
            cx.executor().allow_parking();
            std::thread::sleep(Duration::from_millis(20));
            cx.run_until_parked();
        }
        models.read_with(cx, |m, _| {
            assert!(!m.is_installed(tiny));
            assert!(m.error(tiny).is_some_and(|e| e.contains("checksum") || e.contains("stopped")), "{:?}", m.error(tiny));
        });
    }
}
