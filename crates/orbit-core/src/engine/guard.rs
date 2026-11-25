//! The guard in the engine: a policy every agent request passes through.
//!
//! The list ([`GuardSettings`]) lives in `<data>/guard.bin`, outside the
//! logged settings, so changing it needs no schema change. The engine:
//!
//! * hands it to every session it starts ([`crate::provider::SessionSpec`]),
//!   so providers that would approve on their own (Full access, auto-edit)
//!   raise a matching request instead;
//! * turns "allow for this session" on a guarded request into a one-time
//!   approval, so the next match asks again;
//! * refuses approvals of guarded requests that arrive [`Command::Via`] a
//!   remote origin such as a chat bot. Denying is always allowed.
//!
//! Other features reuse it through [`Orchestrator::guard_hit`].

use std::path::Path;

use orbit_proto::*;

use super::orchestrator::{CommandResult, Orchestrator};

const FILE: &str = "guard.bin";
/// Version 1 was written in bitcode; version 2 is Orbit's wire format.
const BITCODE: u8 = 1;
const VERSION: u8 = 2;

#[derive(Default)]
pub(crate) struct GuardState {
    /// Loaded on first use.
    settings: Option<GuardSettings>,
}

impl Orchestrator {
    /// The guard list in force.
    pub(crate) fn guard(&mut self) -> &GuardSettings {
        if self.guard.settings.is_none() {
            self.guard.settings = Some(load(&self.store.dir().join(FILE)));
        }
        self.guard.settings.as_ref().expect("loaded above")
    }

    /// `Command::Guard`: reads or replaces the list.
    pub(crate) fn guard_command(&mut self, settings: Option<GuardSettings>) -> CommandResult {
        let Some(settings) = settings else { return Ok(Reply::Guard(self.guard().clone())) };
        let settings = settings.sanitized();
        if *self.guard() != settings {
            save(&self.store.dir().join(FILE), &settings).map_err(|e| format!("Couldn’t save the guard list: {e}"))?;
            self.guard.settings = Some(settings.clone());
            self.emit(Event::GuardChanged(settings.clone()));
        }
        Ok(Reply::Guard(settings))
    }

    /// The rule a pending request of `thread` matches, if any.
    pub fn guard_hit(&mut self, thread: ThreadId, request: RequestId) -> Option<GuardHit> {
        let kind = self.store.thread(thread)?.pending.iter().find(|p| p.request.id == request)?.request.kind.clone();
        self.guard().check(&kind)
    }

    /// `Command::Resolve` from the app: a guarded request is approved once,
    /// never for the session.
    pub(crate) fn resolve_guarded(&mut self, thread: ThreadId, request: RequestId, decision: Decision) -> CommandResult {
        let decision = match decision {
            Decision::AllowForSession if self.guard_hit(thread, request).is_some() => Decision::Allow,
            other => other,
        };
        self.resolve(thread, request, decision)
    }
}

fn load(path: &Path) -> GuardSettings {
    match std::fs::read(path) {
        Ok(bytes) => match bytes.split_first() {
            Some((&version @ (BITCODE | VERSION), rest)) => {
                let read = if version == BITCODE { bitcode_decode(rest) } else { decode(rest) };
                read.unwrap_or_else(|e| {
                    log::warn!("{}: unreadable ({e}); using the default guard list", path.display());
                    GuardSettings::default()
                })
            }
            _ => GuardSettings::default(),
        },
        Err(_) => GuardSettings::default(),
    }
}

fn save(path: &Path, settings: &GuardSettings) -> std::io::Result<()> {
    let mut bytes = vec![VERSION];
    encode_into(settings, &mut bytes);
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_list_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        assert_eq!(load(&path), GuardSettings::default(), "a new install starts with the defaults");
        let custom = GuardSettings { enabled: true, rules: vec![GuardRule::new("make release", "Releases")] };
        save(&path, &custom).unwrap();
        assert_eq!(load(&path), custom);
    }
}
