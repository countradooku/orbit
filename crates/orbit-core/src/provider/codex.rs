//! Codex adapter. Placeholder until the adapter lands: reports the runtime as
//! unavailable and ends every session immediately.

use std::path::PathBuf;

use orbit_proto::*;

use super::{AgentEvent, EventSender, SessionHandle, SessionSpec};

pub fn start(spec: SessionSpec, events: EventSender) -> SessionHandle {
    use crossfire::BlockingTxTrait;
    let (input, _rx) = crossfire::mpsc::bounded_blocking(64);
    let _ = events.send((spec.thread, AgentEvent::Exited { error: Some("Codex support is not built yet".into()) }));
    SessionHandle { input }
}

pub async fn probe(binary: Option<PathBuf>) -> ProviderInfo {
    ProviderInfo {
        kind: ProviderKind::Codex,
        installed: false,
        version: None,
        signed_in: None,
        account: None,
        binary: binary.map(|b| b.display().to_string()),
        models: Vec::new(),
        modes: Vec::new(),
        slash_commands: Vec::new(),
        message: Some("Codex support is not built yet".into()),
    }
}
