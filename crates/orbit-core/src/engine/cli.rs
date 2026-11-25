//! Engine commands for automation: the `orbit` CLI and its MCP server.
//!
//! Everything here is plain request/response on top of the existing model;
//! waiting and streaming are done client-side from the event stream, so the
//! engine keeps no per-client state and costs nothing when no client runs.

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use orbit_proto::*;

use super::orchestrator::{CommandResult, Orchestrator};
use crate::terminal::{SCROLLBACK_BYTES, Scrollback, Shell, TermSize};

/// Bytes of output kept per terminal for [`Command::ReadTerminal`] and for
/// re-attaching screens (`engine/host.rs`).
const TAIL_BYTES: usize = SCROLLBACK_BYTES;
/// Exited terminals whose output and exit code are kept for reading.
const KEEP_EXITED: usize = 16;

/// What the automation commands remember between calls.
#[derive(Default)]
pub(crate) struct CliState {
    pub(crate) terminals: HashMap<TermId, TermRecord>,
    /// Exited terminals, oldest first; trimmed to [`KEEP_EXITED`].
    exited: VecDeque<TermId>,
}

pub(crate) struct TermRecord {
    pub(crate) info: TerminalInfo,
    pub(crate) tail: Scrollback,
}

impl TermRecord {
    fn new(id: TermId, command: String, cwd: String) -> Self {
        Self {
            info: TerminalInfo { id, command, cwd, running: true, exit_code: None },
            tail: Scrollback::new(TAIL_BYTES),
        }
    }
}

impl CliState {
    /// Keeps the newest output of every terminal, so `orbit terminal read`
    /// works for terminals the desktop app opened too. A memcpy per chunk.
    /// Returns whether this is a command terminal (no screen attached).
    pub(crate) fn capture(&mut self, term: TermId, data: &[u8]) -> bool {
        let record =
            self.terminals.entry(term).or_insert_with(|| TermRecord::new(term, String::new(), String::new()));
        record.tail.push(data);
        !record.info.command.is_empty()
    }

    pub(crate) fn exited(&mut self, term: TermId, code: Option<i32>) {
        let Some(record) = self.terminals.get_mut(&term) else { return };
        record.info.running = false;
        record.info.exit_code = code;
        self.exited.push_back(term);
        while self.exited.len() > KEEP_EXITED {
            if let Some(old) = self.exited.pop_front() {
                self.terminals.remove(&old);
            }
        }
    }
}

impl Orchestrator {
    /// Output of any terminal. A command terminal has no screen to answer
    /// the pseudoconsole's cursor-position query (`ESC[6n`, sent by ConPTY
    /// at startup), and the command would wait for it forever; answer it.
    pub(crate) fn cli_terminal_output(&mut self, term: TermId, data: &[u8]) {
        if self.cli.capture(term, data) && memchr::memmem::find(data, b"[6n").is_some() {
            self.terminals.input(term, b"[1;1R");
        }
    }

    pub(crate) fn cli_command(&mut self, command: Command) -> CommandResult {
        match command {
            Command::StartThread(start) => self.start_thread(start),
            Command::ReadTimeline { id, after, limit } => self.read_timeline(id, after, limit),
            Command::RunInTerminal { cwd, command, cols, rows } => self.run_in_terminal(cwd, command, cols, rows),
            Command::ReadTerminal { term, max_bytes } => self.read_terminal(term, max_bytes),
            Command::ListTerminals => Ok(Reply::Terminals(self.terminal_list())),
            _ => Err("not an automation command".into()),
        }
    }

    /// Creates a thread like [`Command::CreateThread`], but linked to its
    /// parent, optionally titled, and started with its first message in one
    /// step. Unlike the composer it leaves the user's defaults alone: an
    /// agent's choice of model for a helper is not the user's new default.
    pub(crate) fn start_thread(&mut self, start: ThreadStart) -> CommandResult {
        let ThreadStart { project, config, worktree, parent, title, prompt } = start;
        let project_record = match project {
            Some(id) => Some(self.store.project(id).cloned().ok_or("That project no longer exists.")?),
            None => None,
        };
        if worktree && project_record.is_none() {
            return Err("A worktree needs a project: pick one first.".into());
        }
        if let Some(parent) = parent
            && self.store.thread(parent).is_none()
        {
            return Err("The parent thread no longer exists.".into());
        }
        let prompt = prompt.filter(|p| !p.trim().is_empty());
        let now = now_ms();
        let thread = Thread {
            id: ThreadId::new(),
            project,
            title: title.trim().to_owned(),
            config,
            created_ms: now,
            updated_ms: now,
            parent,
            status: if worktree { ThreadStatus::Starting } else { ThreadStatus::Idle },
            ..Default::default()
        };
        let id = thread.id;
        self.emit(Event::ThreadUpserted(Box::new(thread)));
        if let (true, Some(project)) = (worktree, project_record) {
            self.create_worktree(id, project);
        }
        if let Some(prompt) = prompt {
            self.send(id, prompt, Vec::new(), false)?;
        }
        Ok(Reply::Thread(id))
    }

    /// Reads a timeline without the side effects of opening it: the thread
    /// stays unread for the user, and a timeline nobody shows is unloaded
    /// again right away.
    fn read_timeline(&mut self, id: ThreadId, after: Option<ItemId>, limit: u32) -> CommandResult {
        if self.store.thread(id).is_none() {
            return Err("That thread no longer exists.".into());
        }
        let was_loaded = self.store.timeline(id).is_some();
        let items = {
            let timeline = self.store.load_timeline(id).map_err(|e| e.to_string())?;
            let mut items: Vec<Item> = match after {
                // Ids are time-ordered, so "after" survives the item itself
                // being removed in the meantime.
                Some(after) => timeline.items().iter().filter(|i| i.id > after).cloned().collect(),
                None => timeline.items().to_vec(),
            };
            if limit > 0 && items.len() > limit as usize {
                items.drain(..items.len() - limit as usize);
            }
            items
        };
        let mark = next_raw_id();
        self.emit(Event::TimelineMark { thread: id, mark });
        let busy = self.runtimes.get(&id).is_some_and(|r| r.turn.is_some());
        if !was_loaded && !busy && !self.open.contains(&id) {
            self.store.unload_timeline(id);
        }
        Ok(Reply::Timeline { items, mark })
    }

    fn run_in_terminal(&mut self, cwd: String, command: String, cols: u16, rows: u16) -> CommandResult {
        if command.trim().is_empty() {
            return Err("Give a command to run.".into());
        }
        let shell = command_shell(&command);
        let size = TermSize::new(if cols == 0 { 200 } else { cols.max(2) }, if rows == 0 { 50 } else { rows.max(1) });
        let id = self
            .terminals
            .open(Path::new(&cwd), size, Some(shell))
            .map_err(|e| format!("Couldn’t start “{command}” in {cwd}: {e}"))?;
        self.cli.terminals.insert(id, TermRecord::new(id, command, cwd));
        Ok(Reply::Terminal(id))
    }

    fn read_terminal(&mut self, term: TermId, max_bytes: u32) -> CommandResult {
        let running = self.terminals.is_open(term);
        let Some(record) = self.cli.terminals.get(&term) else {
            if running {
                let info = TerminalInfo { id: term, command: String::new(), cwd: String::new(), running, exit_code: None };
                return Ok(Reply::TerminalTail { info, data: Vec::new(), dropped: 0 });
            }
            return Err("That terminal is closed, or was closed too long ago to read.".into());
        };
        let data = record.tail.tail(max_bytes as usize);
        let skip = record.tail.len() - data.len();
        let mut info = record.info.clone();
        info.running = info.running && running;
        Ok(Reply::TerminalTail { info, data, dropped: record.tail.dropped() + skip as u64 })
    }

    fn terminal_list(&self) -> Vec<TerminalInfo> {
        let mut list: Vec<TerminalInfo> = self.cli.terminals.values().map(|r| r.info.clone()).collect();
        for id in self.terminals.ids() {
            if !self.cli.terminals.contains_key(&id) {
                list.push(TerminalInfo { id, command: String::new(), cwd: String::new(), running: true, exit_code: None });
            }
        }
        for info in &mut list {
            info.running = info.running && self.terminals.is_open(info.id);
        }
        list.sort_by_key(|t| t.id);
        list
    }
}

/// The user's shell, set up to run one command and exit with its status.
pub(crate) fn command_shell(command: &str) -> Shell {
    let shell = Shell::user_default();
    let program = shell.program().to_owned();
    let name = program.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    if name == "pwsh" || name == "powershell" {
        Shell::new(program).args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", command])
    } else if name == "cmd" {
        Shell::new(program).args(["/D", "/S", "/C", command])
    } else {
        // POSIX shells. Keep a login shell's `-l` (macOS) so PATH matches
        // what the user's terminal sees.
        Shell::new(program).args(shell.arguments().iter().cloned()).args(["-c", command])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_tail_keeps_the_newest_bytes() {
        let mut state = CliState::default();
        let term = TermId::new();
        state.capture(term, &vec![b'a'; TAIL_BYTES - 10]);
        state.capture(term, b"0123456789XYZ");
        let record = &state.terminals[&term];
        assert_eq!(record.tail.len(), TAIL_BYTES);
        assert_eq!(record.tail.dropped(), 3);
        assert_eq!(record.tail.tail(3), b"XYZ");
        state.capture(term, &vec![b'b'; TAIL_BYTES + 5]);
        let record = &state.terminals[&term];
        assert_eq!(record.tail.len(), TAIL_BYTES);
        assert_eq!(record.tail.dropped(), 3 + TAIL_BYTES as u64 + 5);
    }

    #[test]
    fn only_recent_exited_terminals_are_kept() {
        let mut state = CliState::default();
        let ids: Vec<TermId> = (0..KEEP_EXITED + 3).map(|_| TermId::new()).collect();
        for id in &ids {
            state.capture(*id, b"x");
            state.exited(*id, Some(0));
        }
        assert_eq!(state.terminals.len(), KEEP_EXITED);
        assert!(!state.terminals.contains_key(&ids[0]));
        assert!(state.terminals[ids.last().unwrap()].info.exit_code == Some(0));
    }
}
