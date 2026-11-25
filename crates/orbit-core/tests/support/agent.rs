//! Shared helpers for the tests that drive an adapter against
//! `orbit-fake-agent` (`tests/support/fake_agent.rs`).

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

use orbit_core::provider::{self, AgentEvent, SessionHandle, SessionSpec};
use orbit_proto::*;
use sonic_rs::{JsonValueTrait, Value};

pub type Events = crossfire::AsyncRx<crossfire::mpsc::List<(ThreadId, AgentEvent)>>;

pub const WAIT: Duration = Duration::from_secs(15);

pub struct Fake {
    pub dir: tempfile::TempDir,
    pub exe: PathBuf,
}

impl Fake {
    /// Copies the fake into a temp dir as `name[.exe]` with `script`.
    pub fn new(name: &str, script: Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() });
        std::fs::copy(env!("CARGO_BIN_EXE_orbit-fake-agent"), &exe).unwrap();
        std::fs::write(dir.path().join("fake-agent.json"), sonic_rs::to_string(&script).unwrap()).unwrap();
        Self { dir, exe }
    }

    /// Everything the adapter sent, in order.
    pub fn received(&self) -> Vec<Value> {
        let text = std::fs::read_to_string(self.dir.path().join("fake-agent.log.jsonl")).unwrap_or_default();
        text.lines().map(|l| sonic_rs::from_str(l).unwrap()).collect()
    }

    /// Messages whose `key` (`method`, `type` or `route`) is `name`.
    pub fn sent(&self, key: &str, name: &str) -> Vec<Value> {
        self.received().into_iter().filter(|m| m.get(key).and_then(|v| v.as_str()) == Some(name)).collect()
    }

    /// The command line the fake was started with.
    pub fn args(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("fake-agent.args")).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    pub fn spec(&self, config: ThreadConfig) -> SessionSpec {
        SessionSpec {
            thread: ThreadId::new(),
            cwd: self.dir.path().to_owned(),
            config,
            resume: None,
            resume_at: None,
            binary: Some(self.exe.clone()),
            args: Vec::new(),
            env: Vec::new(),
            extra_dirs: Vec::new(),
        }
    }

    pub fn start(&self, kind: ProviderKind, spec: SessionSpec) -> (SessionHandle, Events) {
        let (tx, rx) = crossfire::mpsc::unbounded_async();
        (provider::start(kind, spec, tx), rx)
    }
}

/// Collects events until `done` matches one (which is included).
pub async fn until(rx: &Events, mut done: impl FnMut(&AgentEvent) -> bool) -> Vec<AgentEvent> {
    let mut seen = Vec::new();
    let collect = async {
        while let Ok((_, event)) = rx.recv().await {
            let stop = done(&event);
            seen.push(event);
            if stop {
                return;
            }
        }
    };
    if tokio::time::timeout(WAIT, collect).await.is_err() {
        panic!("timed out; events so far: {seen:#?}");
    }
    seen
}

pub fn finished(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::TurnFinished { .. })
}

/// Waits until the fake has logged something matching `ok`.
pub async fn logged(fake: &Fake, ok: impl Fn(&[Value]) -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !ok(&fake.received()) {
        assert!(tokio::time::Instant::now() < deadline, "the fake never received it: {:#?}", fake.received());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The value at `path`; numeric segments index arrays.
pub fn at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    use sonic_rs::JsonContainerTrait;
    let mut v = value;
    for seg in path {
        v = match seg.parse::<usize>() {
            Ok(ix) => v.as_array()?.get(ix)?,
            Err(_) => v.get(*seg)?,
        };
    }
    Some(v)
}

/// The string at `path`.
pub fn str_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    at(value, path).and_then(|v| v.as_str())
}
