//! The dictation worker: one background thread that owns the model, runs
//! sessions and lets go of everything when idle.
//!
//! - **Lazy.** No thread exists until the first session (or a pre-warm).
//!   The model loads on that thread, never on the UI's.
//! - **Idle unload.** With no session and no command for the idle timeout
//!   (five minutes by default), the thread drops the model and its compute
//!   pool and exits: idle Orbit holds no model memory and no threads.
//! - **Off the UI thread.** Audio and commands arrive over a crossfire
//!   channel and results go back over another, so the UI only ever enqueues.
//!   Decoding runs in a rayon pool sized to the physical cores (at most
//!   eight), leaving the UI and the agents' processes room.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossfire::{AsyncRx, BlockingTxTrait as _, MTx, Rx, mpsc};

use crate::engine::{Engine, Transcript};
use crate::stream::{Job, Preview, Session, SessionConfig};

/// Loads an engine on the worker thread.
pub trait Loader: Send + Sync {
    /// Names the model and its options: a different key reloads.
    fn key(&self) -> String;
    fn load(&self) -> Result<Box<dyn Engine>, String>;
}

/// What a session reports, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The model is loading (first use, or after an idle unload).
    Loading,
    /// The model is ready and audio is being transcribed.
    Ready,
    Preview(Preview),
    /// Whether speech has been heard and how long it has been quiet since,
    /// about twice a second.
    Activity { heard_speech: bool, silence: Duration },
    /// The finished, cleaned-up transcript.
    Final(String),
    Failed(String),
}

pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

enum Command {
    Prewarm(Arc<dyn Loader>),
    Start { id: u64, loader: Arc<dyn Loader>, config: SessionConfig, events: MTx<mpsc::List<Event>>, cancel: Arc<AtomicBool> },
    Audio { id: u64, samples: Vec<f32> },
    Finish { id: u64 },
    Cancel { id: u64 },
}

struct Shared {
    tx: Option<MTx<mpsc::List<Command>>>,
    idle: Duration,
    threads: usize,
    next_id: u64,
}

/// A handle to the worker; clones share it.
#[derive(Clone)]
pub struct Worker {
    shared: Arc<Mutex<Shared>>,
}

impl Default for Worker {
    fn default() -> Self {
        Self::new(DEFAULT_IDLE_TIMEOUT)
    }
}

/// Physical cores, roughly, at most eight.
pub fn default_threads() -> usize {
    let logical = std::thread::available_parallelism().map_or(2, |n| n.get());
    (logical / 2).clamp(1, 8)
}

impl Worker {
    pub fn new(idle: Duration) -> Self {
        Self { shared: Arc::new(Mutex::new(Shared { tx: None, idle, threads: default_threads(), next_id: 0 })) }
    }

    /// How long the model stays loaded with nothing to do. Applies from the
    /// next time the worker starts.
    pub fn set_idle_timeout(&self, idle: Duration) {
        self.lock().idle = idle;
    }

    /// Decoding threads, applied when the model next loads.
    pub fn set_threads(&self, threads: usize) {
        self.lock().threads = threads.max(1);
    }

    /// Whether the worker thread is alive (and so may hold a model).
    pub fn is_running(&self) -> bool {
        self.lock().tx.is_some()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send(&self, command: Command) {
        let mut shared = self.lock();
        if shared.tx.is_none() {
            let (tx, rx) = mpsc::unbounded_blocking::<Command>();
            let (idle, threads) = (shared.idle, shared.threads);
            let handle = self.shared.clone();
            let spawned = std::thread::Builder::new()
                .name("orbit-dictation".into())
                .spawn(move || run(rx, handle, idle, threads));
            if let Err(e) = spawned {
                log::error!("voice: couldn’t start the dictation thread: {e}");
                return;
            }
            shared.tx = Some(tx);
        }
        if let Some(tx) = &shared.tx {
            let _ = tx.send(command);
        }
    }

    /// Loads the model ahead of use, e.g. when the pointer rests on the mic
    /// button. Does nothing if it is loaded.
    pub fn prewarm(&self, loader: Arc<dyn Loader>) {
        self.send(Command::Prewarm(loader));
    }

    /// Starts a session. Push audio through the handle; events arrive on the
    /// receiver until [`Event::Final`] or [`Event::Failed`].
    pub fn start(&self, loader: Arc<dyn Loader>, config: SessionConfig) -> (SessionHandle, AsyncRx<mpsc::List<Event>>) {
        let id = {
            let mut shared = self.lock();
            shared.next_id += 1;
            shared.next_id
        };
        let (events, rx) = mpsc::unbounded_async::<Event>();
        let cancel = Arc::new(AtomicBool::new(false));
        self.send(Command::Start { id, loader, config, events, cancel: cancel.clone() });
        (SessionHandle { id, cancel, worker: self.clone(), done: false }, rx)
    }
}

/// A running session. Dropping it before [`Self::finish`] cancels it.
pub struct SessionHandle {
    id: u64,
    cancel: Arc<AtomicBool>,
    worker: Worker,
    done: bool,
}

impl SessionHandle {
    /// Delivers 16 kHz mono audio.
    pub fn push(&self, samples: Vec<f32>) {
        if !self.done && !samples.is_empty() {
            self.worker.send(Command::Audio { id: self.id, samples });
        }
    }

    /// No more audio: finish with the final pass.
    pub fn finish(&mut self) {
        if !self.done {
            self.done = true;
            self.worker.send(Command::Finish { id: self.id });
        }
    }

    /// Stops at once, mid-decode if need be, reporting nothing more.
    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.done = true;
        self.worker.send(Command::Cancel { id: self.id });
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        if !self.done {
            self.cancel();
        }
    }
}

struct Active {
    id: u64,
    session: Session,
    events: MTx<mpsc::List<Event>>,
    cancel: Arc<AtomicBool>,
    finishing: bool,
    /// Last activity reported: speech heard, and silence in half seconds.
    reported: Option<(bool, u64)>,
}

impl Active {
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    fn has_work(&self) -> bool {
        self.finishing || self.session.next_job().is_some()
    }
}

struct Loaded {
    key: String,
    engine: Box<dyn Engine>,
    pool: Option<rayon::ThreadPool>,
}

fn run(rx: Rx<mpsc::List<Command>>, shared: Arc<Mutex<Shared>>, idle: Duration, threads: usize) {
    let mut loaded: Option<Loaded> = None;
    let mut active: Option<Active> = None;
    loop {
        let command = match &active {
            Some(a) if a.has_work() => rx.try_recv().ok(),
            Some(_) => match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => break,
            },
            None => match rx.recv_timeout(idle) {
                Ok(c) => Some(c),
                Err(crossfire::RecvTimeoutError::Timeout) => {
                    let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
                    if rx.is_empty() {
                        shared.tx = None;
                        break;
                    }
                    continue;
                }
                Err(crossfire::RecvTimeoutError::Disconnected) => break,
            },
        };
        let mut next = command;
        while let Some(command) = next {
            handle(command, &mut loaded, &mut active, threads);
            next = rx.try_recv().ok();
        }
        if let Some(a) = active.as_mut() {
            report_activity(a);
            if step(a, &mut loaded) {
                active = None;
            }
        }
    }
    log::debug!("voice: dictation worker idle; model unloaded");
}

fn ensure_engine(loaded: &mut Option<Loaded>, loader: &dyn Loader, threads: usize) -> Result<bool, String> {
    let key = loader.key();
    if loaded.as_ref().is_some_and(|l| l.key == key) {
        return Ok(false);
    }
    *loaded = None;
    let started = std::time::Instant::now();
    let engine = loader.load()?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("orbit-whisper-{i}"))
        .build()
        .map_err(|e| log::warn!("voice: no decoding pool ({e}); using rayon's global one"))
        .ok();
    log::info!("voice: loaded {key} in {} ms", started.elapsed().as_millis());
    *loaded = Some(Loaded { key, engine, pool });
    Ok(true)
}

fn handle(command: Command, loaded: &mut Option<Loaded>, active: &mut Option<Active>, threads: usize) {
    match command {
        Command::Prewarm(loader) => {
            if let Err(e) = ensure_engine(loaded, loader.as_ref(), threads) {
                log::warn!("voice: pre-warm failed: {e}");
            }
        }
        Command::Start { id, loader, config, events, cancel } => {
            let a = Active { id, session: Session::new(config), events, cancel, finishing: false, reported: None };
            let needs_load = loaded.as_ref().is_none_or(|l| l.key != loader.key());
            if needs_load {
                a.emit(Event::Loading);
            }
            match ensure_engine(loaded, loader.as_ref(), threads) {
                Ok(_) => {
                    a.emit(Event::Ready);
                    *active = Some(a);
                }
                Err(e) => {
                    a.emit(Event::Failed(e));
                    *active = None;
                }
            }
        }
        Command::Audio { id, samples } => {
            if let Some(a) = active.as_mut().filter(|a| a.id == id) {
                a.session.push(&samples);
            }
        }
        Command::Finish { id } => {
            if let Some(a) = active.as_mut().filter(|a| a.id == id) {
                a.finishing = true;
            }
        }
        Command::Cancel { id } => {
            if active.as_ref().is_some_and(|a| a.id == id) {
                *active = None;
            }
        }
    }
}

fn report_activity(a: &mut Active) {
    if a.finishing {
        return;
    }
    let now = (a.session.heard_speech(), a.session.silence().as_millis() as u64 / 500);
    if a.reported != Some(now) {
        a.reported = Some(now);
        a.emit(Event::Activity { heard_speech: now.0, silence: a.session.silence() });
    }
}

/// Runs the session's next job; true when the session is over.
fn step(a: &mut Active, loaded: &mut Option<Loaded>) -> bool {
    if a.cancel.load(Ordering::Relaxed) {
        return true;
    }
    let job = if a.finishing { a.session.final_job() } else { a.session.next_job() };
    let Some(job) = job else {
        if a.finishing {
            a.emit(Event::Final(a.session.final_text()));
            return true;
        }
        return false;
    };
    let Some(l) = loaded.as_mut() else {
        a.emit(Event::Failed("The speech model isn’t loaded.".into()));
        return true;
    };
    let result = match a.session.decode(&job) {
        Some(decode) => {
            let (engine, cancel) = (&mut l.engine, &a.cancel);
            let mut run = || engine.transcribe(decode.audio, &decode.request(), cancel);
            match &l.pool {
                Some(pool) => pool.install(run),
                None => run(),
            }
        }
        None => Ok(Transcript::default()),
    };
    if a.cancel.load(Ordering::Relaxed) {
        return true;
    }
    match result {
        Ok(transcript) => {
            let preview = a.session.apply(&job, transcript);
            if let Job::Final(_) = job {
                a.emit(Event::Final(a.session.final_text()));
                return true;
            }
            a.emit(Event::Preview(preview));
            false
        }
        Err(e) => {
            a.emit(Event::Failed(e));
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::SAMPLES_PER_SECOND;
    use crate::engine::fake::{FakeEngine, Utterance};
    use crate::postprocess::Cleanup;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    struct FakeLoader {
        utterance: Arc<Utterance>,
        loads: Arc<AtomicUsize>,
        delay: Duration,
        fail: bool,
    }

    impl Loader for FakeLoader {
        fn key(&self) -> String {
            "fake".into()
        }

        fn load(&self) -> Result<Box<dyn Engine>, String> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err("The model file is missing.".into());
            }
            let mut engine = FakeEngine::new(&self.utterance);
            engine.delay = self.delay;
            Ok(Box::new(engine))
        }
    }

    fn loader(script: &str, delay: Duration) -> (Arc<FakeLoader>, Arc<Utterance>) {
        let utterance = Arc::new(Utterance::new(script, 0.6, 0.8));
        let l = Arc::new(FakeLoader { utterance: utterance.clone(), loads: Arc::default(), delay, fail: false });
        (l, utterance)
    }

    fn config() -> SessionConfig {
        SessionConfig { preview: true, cleanup: Cleanup::default(), ..Default::default() }
    }

    /// Collects events until the session ends.
    fn drain(rx: &AsyncRx<mpsc::List<Event>>, timeout: Duration) -> Vec<Event> {
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(e) => {
                    let end = matches!(e, Event::Final(_) | Event::Failed(_));
                    out.push(e);
                    if end {
                        break;
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        out
    }

    #[test]
    fn a_session_streams_previews_then_the_final_text() {
        let (l, utterance) = loader("send the tests to the api agent", Duration::from_millis(15));
        let worker = Worker::new(Duration::from_secs(60));
        assert!(!worker.is_running(), "nothing runs before first use");
        let (mut session, rx) = worker.start(l.clone(), config());
        // Real time-ish: 100 ms of audio every 10 ms.
        for chunk in utterance.audio.chunks(SAMPLES_PER_SECOND / 10) {
            session.push(chunk.to_vec());
            std::thread::sleep(Duration::from_millis(10));
        }
        session.finish();
        let events = drain(&rx, Duration::from_secs(10));
        assert_eq!(events.first(), Some(&Event::Loading));
        assert_eq!(events.get(1), Some(&Event::Ready));
        assert!(events.iter().any(|e| matches!(e, Event::Preview(_))), "{events:?}");
        assert!(events.iter().any(|e| matches!(e, Event::Activity { heard_speech: true, .. })));
        assert_eq!(events.last(), Some(&Event::Final("Send the tests to the api agent".into())));
        assert!(worker.is_running());
        assert_eq!(l.loads.load(Ordering::SeqCst), 1);

        // A second session reuses the loaded model.
        let (mut again, rx) = worker.start(l.clone(), config());
        again.push(utterance.audio.to_vec());
        again.finish();
        let events = drain(&rx, Duration::from_secs(10));
        assert_eq!(events.first(), Some(&Event::Ready));
        assert_eq!(l.loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancel_ends_the_session_without_a_result() {
        let (l, utterance) = loader("this will be thrown away", Duration::from_millis(200));
        let worker = Worker::new(Duration::from_secs(60));
        let (mut session, rx) = worker.start(l, config());
        session.push(utterance.audio.to_vec());
        std::thread::sleep(Duration::from_millis(50));
        session.cancel();
        let events = drain(&rx, Duration::from_millis(800));
        assert!(!events.iter().any(|e| matches!(e, Event::Final(_) | Event::Failed(_))), "{events:?}");
    }

    #[test]
    fn dropping_the_handle_cancels() {
        let (l, utterance) = loader("dropped", Duration::ZERO);
        let worker = Worker::new(Duration::from_secs(60));
        let (session, rx) = worker.start(l, config());
        session.push(utterance.audio.to_vec());
        drop(session);
        let events = drain(&rx, Duration::from_millis(500));
        assert!(!events.iter().any(|e| matches!(e, Event::Final(_))), "{events:?}");
    }

    #[test]
    fn the_model_unloads_after_the_idle_timeout_and_reloads_on_use() {
        let (l, utterance) = loader("hello", Duration::ZERO);
        let worker = Worker::new(Duration::from_millis(150));
        worker.prewarm(l.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        while l.loads.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(worker.is_running());
        // Idle: the thread exits, dropping the model.
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker.is_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!worker.is_running(), "the worker is still alive after the idle timeout");
        // Used again: a new thread loads it again.
        let (mut session, rx) = worker.start(l.clone(), config());
        session.push(utterance.audio.to_vec());
        session.finish();
        let events = drain(&rx, Duration::from_secs(5));
        assert_eq!(events.last(), Some(&Event::Final("Hello".into())));
        assert_eq!(l.loads.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_model_that_fails_to_load_fails_the_session() {
        let utterance = Arc::new(Utterance::new("x", 0.1, 0.1));
        let l = Arc::new(FakeLoader { utterance, loads: Arc::default(), delay: Duration::ZERO, fail: true });
        let worker = Worker::new(Duration::from_secs(60));
        let (_session, rx) = worker.start(l, config());
        let events = drain(&rx, Duration::from_secs(5));
        assert_eq!(events, vec![Event::Loading, Event::Failed("The model file is missing.".into())]);
    }

    #[test]
    fn a_finished_session_with_no_speech_returns_empty_text() {
        let (l, _) = loader("unused", Duration::ZERO);
        let worker = Worker::new(Duration::from_secs(60));
        let (mut session, rx) = worker.start(l, config());
        session.push(vec![0.0; SAMPLES_PER_SECOND * 2]);
        session.finish();
        let events = drain(&rx, Duration::from_secs(5));
        assert_eq!(events.last(), Some(&Event::Final(String::new())));
    }
}
