//! Installed models under `<data>/models/whisper/`, and installing them.
//!
//! An install downloads into `.partial/<id>/`, where it survives restarts
//! and resumes from the bytes already on disk (re-hashing them first). Each
//! file is checked against the catalog's size and SHA-256 before it is
//! kept; a mismatch deletes it. Safetensors weights are then quantized to
//! GGUF, and the folder is renamed to `<id>/` in one step, so a model is
//! either wholly installed or not at all.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use candle_core::quantized::GgmlDType;
use sha2::{Digest as _, Sha256};

use crate::catalog::{ModelFile, ModelInfo, Weights};

pub const MODEL_FILE: &str = "model.gguf";
pub const TOKENIZER_FILE: &str = "tokenizer.json";
const MANIFEST: &str = "manifest.txt";
const PARTIAL: &str = ".partial";

/// A part of a download as it arrives.
pub enum Chunk<'a> {
    /// The response began; `resumed` is false when the server sent the
    /// whole file rather than the requested range.
    Start { resumed: bool },
    Data(&'a [u8]),
}

/// Where model files come from. The app fetches over HTTPS; tests read
/// local files.
pub trait Fetch: Send + Sync {
    /// Streams `url` from byte `offset` into `sink`.
    fn fetch(&self, url: &str, offset: u64, sink: &mut dyn FnMut(Chunk<'_>) -> Result<(), String>) -> Result<(), String>;
}

/// Reads `file://` URLs (and plain paths), honouring the offset.
pub struct FileFetch;

impl Fetch for FileFetch {
    fn fetch(&self, url: &str, offset: u64, sink: &mut dyn FnMut(Chunk<'_>) -> Result<(), String>) -> Result<(), String> {
        let path = url.strip_prefix("file://").unwrap_or(url);
        let mut file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        sink(Chunk::Start { resumed: true })?;
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = file.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                return Ok(());
            }
            sink(Chunk::Data(&buf[..n]))?;
        }
    }
}

/// What an install is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Downloading,
    Verifying,
    Converting,
}

/// Shared between an install and whoever shows it.
#[derive(Default, Debug)]
pub struct Progress {
    done: AtomicU64,
    total: AtomicU64,
    phase: AtomicU8,
    cancelled: AtomicBool,
}

impl Progress {
    /// Bytes downloaded (or, while converting, the share converted) and the total.
    pub fn read(&self) -> (u64, u64) {
        (self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed))
    }

    pub fn fraction(&self) -> f32 {
        let (done, total) = self.read();
        if total == 0 { 0.0 } else { (done as f64 / total as f64).min(1.0) as f32 }
    }

    pub fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Relaxed) {
            1 => Phase::Verifying,
            2 => Phase::Converting,
            _ => Phase::Downloading,
        }
    }

    fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallError {
    Cancelled,
    /// A file did not match its pinned size or checksum and was deleted.
    Mismatch(String),
    /// The transfer failed; what arrived is kept for the next attempt.
    Transfer(String),
    Disk(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Download cancelled."),
            Self::Mismatch(what) => write!(f, "{what} didn’t match its published checksum, so it was discarded."),
            Self::Transfer(e) => write!(f, "The download stopped: {e}"),
            Self::Disk(e) => write!(f, "The model couldn’t be saved: {e}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Installed,
    /// Bytes from an interrupted download, kept to resume.
    Partial(u64),
    Missing,
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// A store at `root`, normally `<data>/models/whisper`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir(&self, model: &ModelInfo) -> PathBuf {
        self.root.join(model.id)
    }

    fn staging(&self, model: &ModelInfo) -> PathBuf {
        self.root.join(PARTIAL).join(model.id)
    }

    fn manifest(model: &ModelInfo) -> String {
        format!("{}\n{}\n", model.weights.file().sha256, model.tokenizer.sha256)
    }

    pub fn status(&self, model: &ModelInfo) -> Status {
        if self.paths(model).is_some() {
            return Status::Installed;
        }
        let bytes: u64 = fs::read_dir(self.staging(model))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();
        if bytes > 0 { Status::Partial(bytes) } else { Status::Missing }
    }

    /// The model and tokenizer files of an installed model. An install made
    /// for different catalog checksums does not count.
    pub fn paths(&self, model: &ModelInfo) -> Option<(PathBuf, PathBuf)> {
        let dir = self.dir(model);
        let manifest = fs::read_to_string(dir.join(MANIFEST)).ok()?;
        let (weights, tokenizer) = (dir.join(MODEL_FILE), dir.join(TOKENIZER_FILE));
        (manifest == Self::manifest(model) && weights.is_file() && tokenizer.is_file()).then_some((weights, tokenizer))
    }

    /// Removes an installed model and any partial download of it.
    pub fn delete(&self, model: &ModelInfo) -> std::io::Result<()> {
        for dir in [self.dir(model), self.staging(model)] {
            match fs::remove_dir_all(&dir) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }

    /// Downloads, verifies and installs `model`. Blocking: run it off the UI
    /// thread.
    pub fn install(&self, model: &ModelInfo, fetch: &dyn Fetch, progress: &Progress) -> Result<(), InstallError> {
        let disk = |e: std::io::Error| InstallError::Disk(e.to_string());
        let staging = self.staging(model);
        fs::create_dir_all(&staging).map_err(disk)?;
        let weights_name = match model.weights {
            Weights::Gguf(_) => MODEL_FILE,
            Weights::Safetensors(_) => "model.safetensors",
        };
        let files = [(TOKENIZER_FILE, &model.tokenizer), (weights_name, model.weights.file())];
        progress.total.store(files.iter().map(|f| f.1.size).sum(), Ordering::Relaxed);
        let mut base = 0;
        for (name, file) in files {
            let dest = staging.join(name);
            // Files are renamed into place only after they verify.
            let converted = name == "model.safetensors" && staging.join(MODEL_FILE).is_file();
            if converted || fs::metadata(&dest).is_ok_and(|m| m.len() == file.size) {
                base += file.size;
                progress.done.store(base, Ordering::Relaxed);
                continue;
            }
            let part = staging.join(format!("{name}.part"));
            download(fetch, file, &part, progress, base, name)?;
            fs::rename(&part, &dest).map_err(disk)?;
            base += file.size;
        }
        if let Weights::Safetensors(_) = model.weights
            && !staging.join(MODEL_FILE).is_file()
        {
            progress.set_phase(Phase::Converting);
            let total = progress.total.load(Ordering::Relaxed);
            let src = staging.join(weights_name);
            crate::convert::safetensors_to_gguf(
                &src,
                &staging.join(MODEL_FILE),
                GgmlDType::Q8_0,
                &|f| progress.done.store((f as f64 * total as f64) as u64, Ordering::Relaxed),
                &|| progress.is_cancelled(),
            )
            .map_err(|e| if progress.is_cancelled() { InstallError::Cancelled } else { InstallError::Disk(e) })?;
            fs::remove_file(&src).map_err(disk)?;
        }
        fs::write(staging.join(MANIFEST), Self::manifest(model)).map_err(disk)?;
        let dir = self.dir(model);
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(disk)?;
        }
        fs::rename(&staging, &dir).map_err(disk)?;
        let _ = fs::remove_dir(self.root.join(PARTIAL));
        Ok(())
    }
}

/// Downloads `file` into `part`, resuming from what is there, and verifies it.
fn download(
    fetch: &dyn Fetch,
    file: &ModelFile,
    part: &Path,
    progress: &Progress,
    base: u64,
    name: &str,
) -> Result<(), InstallError> {
    let disk = |e: std::io::Error| InstallError::Disk(e.to_string());
    progress.set_phase(Phase::Verifying);
    let mut hasher = Sha256::new();
    let mut have = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if have > file.size {
        fs::remove_file(part).map_err(disk)?;
        have = 0;
    }
    if have > 0 {
        // Hash what an earlier attempt left, then continue from there.
        let mut existing = File::open(part).map_err(disk)?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = existing.read(&mut buf).map_err(disk)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }
    progress.done.store(base + have, Ordering::Relaxed);
    progress.set_phase(Phase::Downloading);
    // Not `append`: Windows then refuses `set_len` for a restart.
    let mut out = OpenOptions::new().create(true).write(true).truncate(false).open(part).map_err(disk)?;
    out.seek(SeekFrom::Start(have)).map_err(disk)?;
    let mut write_error = None;
    let result = fetch.fetch(file.url, have, &mut |chunk| {
        if progress.is_cancelled() {
            return Err("cancelled".to_owned());
        }
        match chunk {
            Chunk::Start { resumed } => {
                if !resumed && have > 0 {
                    out.set_len(0).map_err(|e| e.to_string())?;
                    out.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
                    hasher = Sha256::new();
                    have = 0;
                }
            }
            Chunk::Data(data) => {
                if have + data.len() as u64 > file.size {
                    return Err("the file is larger than published".to_owned());
                }
                if let Err(e) = out.write_all(data) {
                    write_error = Some(e.to_string());
                    return Err("write failed".to_owned());
                }
                hasher.update(data);
                have += data.len() as u64;
                progress.done.store(base + have, Ordering::Relaxed);
            }
        }
        Ok(())
    });
    out.sync_all().map_err(disk)?;
    drop(out);
    if progress.is_cancelled() {
        return Err(InstallError::Cancelled);
    }
    if let Some(e) = write_error {
        return Err(InstallError::Disk(e));
    }
    if let Err(e) = result {
        if e.contains("larger than published") {
            let _ = fs::remove_file(part);
            return Err(InstallError::Mismatch(name.to_owned()));
        }
        return Err(InstallError::Transfer(e));
    }
    if have != file.size {
        return Err(InstallError::Transfer(format!("it ended after {have} of {} bytes", file.size)));
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if hex != file.sha256 {
        let _ = fs::remove_file(part);
        return Err(InstallError::Mismatch(name.to_owned()));
    }
    Ok(())
}

/// The SHA-256 of `bytes` as hex, for tests and tools.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::find;
    use std::sync::Mutex;

    /// A fake catalog entry serving `weights` and `tokenizer` from files.
    fn entry(dir: &Path, weights: &[u8], tokenizer: &[u8], gguf: bool) -> ModelInfo {
        let w = dir.join("src-weights");
        let t = dir.join("src-tokenizer");
        fs::write(&w, weights).unwrap();
        fs::write(&t, tokenizer).unwrap();
        let leak = |p: PathBuf| -> &'static str { Box::leak(format!("file://{}", p.display()).into_boxed_str()) };
        let file = |url, bytes: &[u8]| ModelFile { url, size: bytes.len() as u64, sha256: Box::leak(sha256_hex(bytes).into_boxed_str()) };
        let weights_file = file(leak(w), weights);
        ModelInfo {
            id: "test",
            weights: if gguf { Weights::Gguf(weights_file) } else { Weights::Safetensors(weights_file) },
            tokenizer: file(leak(t), tokenizer),
            ..*find("tiny.en").unwrap()
        }
    }

    /// Fails the weights after `limit` bytes, once; ignores ranges when
    /// `no_ranges`.
    struct Flaky {
        limit: Mutex<Option<u64>>,
        no_ranges: bool,
        offsets: Mutex<Vec<u64>>,
    }

    impl Fetch for Flaky {
        fn fetch(&self, url: &str, offset: u64, sink: &mut dyn FnMut(Chunk<'_>) -> Result<(), String>) -> Result<(), String> {
            self.offsets.lock().unwrap().push(offset);
            let bytes = fs::read(url.strip_prefix("file://").unwrap()).unwrap();
            let start = if self.no_ranges { 0 } else { offset as usize };
            sink(Chunk::Start { resumed: !self.no_ranges })?;
            let limit = if url.contains("src-weights") { self.limit.lock().unwrap().take() } else { None };
            for (i, chunk) in bytes[start..].chunks(1000).enumerate() {
                if let Some(limit) = limit
                    && (i * 1000) as u64 >= limit
                {
                    return Err("connection reset".to_owned());
                }
                sink(Chunk::Data(chunk))?;
            }
            Ok(())
        }
    }

    #[test]
    fn installs_atomically_and_reports_installed() {
        let dir = tempfile::tempdir().unwrap();
        let model = entry(dir.path(), &[7u8; 5_000], b"{}", true);
        let store = Store::new(dir.path().join("models"));
        assert_eq!(store.status(&model), Status::Missing);
        let progress = Progress::default();
        store.install(&model, &FileFetch, &progress).unwrap();
        assert_eq!(store.status(&model), Status::Installed);
        let (weights, tokenizer) = store.paths(&model).unwrap();
        assert_eq!(fs::read(weights).unwrap(), vec![7u8; 5_000]);
        assert_eq!(fs::read(tokenizer).unwrap(), b"{}");
        assert_eq!(progress.read(), (5_002, 5_002));
        assert!(!dir.path().join("models").join(PARTIAL).exists());
        store.delete(&model).unwrap();
        assert_eq!(store.status(&model), Status::Missing);
    }

    #[test]
    fn a_checksum_mismatch_is_rejected_and_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let mut model = entry(dir.path(), &[1u8; 3_000], b"{}", true);
        if let Weights::Gguf(f) = &mut model.weights {
            f.sha256 = "0000000000000000000000000000000000000000000000000000000000000000";
        }
        let store = Store::new(dir.path().join("models"));
        let err = store.install(&model, &FileFetch, &Progress::default()).unwrap_err();
        assert_eq!(err, InstallError::Mismatch(MODEL_FILE.into()));
        assert!(err.to_string().contains("checksum"));
        assert_eq!(store.paths(&model), None);
        assert!(!store.staging(&model).join("model.gguf.part").exists(), "bad bytes are not kept");
    }

    #[test]
    fn an_interrupted_download_resumes_where_it_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let weights: Vec<u8> = (0..20_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let model = entry(dir.path(), &weights, b"{\"t\":1}", true);
        let store = Store::new(dir.path().join("models"));
        let flaky = Flaky { limit: Mutex::new(Some(8_000)), no_ranges: false, offsets: Mutex::default() };
        // The tokenizer completes; the weights stop at 8 000 bytes.
        let err = store.install(&model, &flaky, &Progress::default()).unwrap_err();
        assert!(matches!(err, InstallError::Transfer(_)), "{err:?}");
        assert!(matches!(store.status(&model), Status::Partial(n) if n >= 8_000));
        // The second attempt asks for the rest only.
        store.install(&model, &flaky, &Progress::default()).unwrap();
        let offsets = flaky.offsets.lock().unwrap().clone();
        assert_eq!(offsets, vec![0, 0, 8_000], "tokenizer, weights, weights resumed");
        assert_eq!(fs::read(store.paths(&model).unwrap().0).unwrap(), weights);
    }

    #[test]
    fn a_server_without_ranges_restarts_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let weights = vec![9u8; 12_000];
        let model = entry(dir.path(), &weights, b"{}", true);
        let store = Store::new(dir.path().join("models"));
        let flaky = Flaky { limit: Mutex::new(Some(5_000)), no_ranges: true, offsets: Mutex::default() };
        assert!(store.install(&model, &flaky, &Progress::default()).is_err());
        store.install(&model, &flaky, &Progress::default()).unwrap();
        assert_eq!(fs::read(store.paths(&model).unwrap().0).unwrap(), weights);
    }

    #[test]
    fn cancel_keeps_the_partial_download() {
        let dir = tempfile::tempdir().unwrap();
        let model = entry(dir.path(), &[3u8; 4_000], b"{}", true);
        let store = Store::new(dir.path().join("models"));
        let progress = Progress::default();
        progress.cancel();
        assert_eq!(store.install(&model, &FileFetch, &progress).unwrap_err(), InstallError::Cancelled);
        assert_eq!(store.paths(&model), None);
    }

    #[test]
    fn safetensors_are_converted_and_removed() {
        use crate::model::tests::{TINY, random_weights};
        let dir = tempfile::tempdir().unwrap();
        let dense: std::collections::HashMap<String, candle_core::Tensor> = random_weights(TINY)
            .into_iter()
            .map(|(n, q)| (n, q.dequantize(&candle_core::Device::Cpu).unwrap()))
            .collect();
        let st = dir.path().join("w.safetensors");
        candle_core::safetensors::save(&dense, &st).unwrap();
        let model = entry(dir.path(), &fs::read(&st).unwrap(), b"{}", false);
        let store = Store::new(dir.path().join("models"));
        store.install(&model, &FileFetch, &Progress::default()).unwrap();
        let (weights, _) = store.paths(&model).unwrap();
        assert!(weights.ends_with(MODEL_FILE));
        assert!(!store.dir(&model).join("model.safetensors").exists());
    }

    #[test]
    fn an_install_for_other_checksums_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        let model = entry(dir.path(), &[5u8; 2_000], b"{}", true);
        let store = Store::new(dir.path().join("models"));
        store.install(&model, &FileFetch, &Progress::default()).unwrap();
        let mut updated = model;
        updated.tokenizer.sha256 = "1111111111111111111111111111111111111111111111111111111111111111";
        assert_eq!(store.status(&updated), Status::Missing);
    }
}
