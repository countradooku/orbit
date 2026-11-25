//! Text for people. Short lines, the next step last, no color codes (pipes
//! and logs stay clean).

use std::collections::HashSet;
use std::fmt::Write as _;

use orbit_proto::*;
use serde_json::{Value, json};

use crate::json::{self, State};

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn pad(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count > width {
        let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
        cut.push('…');
        cut
    } else {
        format!("{text}{}", " ".repeat(width - count))
    }
}

/// `3m ago`, `in 2h`.
pub fn ago_or_in(ms: u64) -> String {
    let now = now_ms();
    let (delta, future) = if ms > now { (ms - now, true) } else { (now - ms, false) };
    let secs = delta / 1000;
    let span = match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    };
    if future { format!("in {span}") } else { format!("{span} ago") }
}

pub fn projects(list: &[Value]) -> String {
    if list.is_empty() {
        return "No projects yet. Add one with `orbit projects add <folder>`.".into();
    }
    let mut out = String::new();
    for p in list {
        let threads = p["threads"].as_u64().unwrap_or(0);
        let _ = writeln!(out, "{}  {}  {}  ({threads} threads)", str_of(p, "id"), pad(str_of(p, "name"), 20), str_of(p, "path"));
    }
    out.trim_end().to_owned()
}

fn state_of(v: &Value) -> State {
    match str_of(v, "state") {
        "starting" => State::Starting,
        "working" => State::Working,
        "needs-you" => State::NeedsYou,
        "rate-limited" => State::RateLimited,
        "failed" => State::Failed,
        _ => State::Idle,
    }
}

pub fn threads(list: &[Value], total: usize) -> String {
    if list.is_empty() {
        return "No threads match.".into();
    }
    let mut out = String::new();
    for t in list {
        let state = state_of(t);
        let project = t["project"].get("name").and_then(Value::as_str).unwrap_or("-");
        let unread = if t["unread"].as_bool() == Some(true) { "•" } else { " " };
        let updated = t["updated_ms"].as_u64().map(ago_or_in).unwrap_or_default();
        let _ = writeln!(
            out,
            "{}  {} {}  {}{} {}  {}",
            str_of(t, "id"),
            state.glyph(),
            pad(state.as_str(), 12),
            unread,
            pad(str_of(t, "title"), 44),
            pad(project, 16),
            updated
        );
    }
    if list.len() < total {
        let _ = writeln!(out, "… {} more (use --limit 0)", total - list.len());
    }
    out.trim_end().to_owned()
}

pub fn overview(v: &Value) -> String {
    let c = &v["counts"];
    let n = |k: &str| c[k].as_u64().unwrap_or(0);
    let mut out = String::new();
    let engine = &v["engine"];
    let _ = writeln!(
        out,
        "Orbit {} ({}) at {}",
        engine["version"].as_str().unwrap_or("?"),
        engine["kind"].as_str().unwrap_or("?"),
        engine["url"].as_str().unwrap_or("?")
    );
    let _ = writeln!(
        out,
        "{} working · {} need you · {} idle · {} failed · {} unread · {} threads in {} projects",
        n("working"),
        n("needs_you"),
        n("idle"),
        n("failed") + n("rate_limited"),
        n("unread"),
        n("threads"),
        n("projects")
    );
    for p in v["providers"].as_array().into_iter().flatten() {
        let name = str_of(p, "provider");
        let line = if p["installed"].as_bool() == Some(true) {
            let signed = match p["signed_in"].as_bool() {
                Some(true) => "signed in",
                Some(false) => "signed out",
                None => "",
            };
            format!("{name} {} {signed}", p["version"].as_str().unwrap_or(""))
        } else {
            format!("{name}: {}", p["message"].as_str().unwrap_or("not installed"))
        };
        let _ = writeln!(out, "  {}", line.trim());
    }
    let needs = v["needs_you"].as_array().map(Vec::as_slice).unwrap_or_default();
    if !needs.is_empty() {
        let _ = writeln!(out, "Needs you:");
        for t in needs {
            let summary = t["pending"][0]["summary"].as_str().unwrap_or("");
            let _ = writeln!(out, "  {}  {}: {}", str_of(t, "id"), str_of(t, "title"), summary);
        }
        let _ = writeln!(out, "Answer with `orbit approve`, `orbit deny` or `orbit answer`.");
    }
    out.trim_end().to_owned()
}

pub fn thread_detail(v: &Value) -> String {
    let mut out = String::new();
    let state = state_of(v);
    let _ = writeln!(out, "{}  {}", str_of(v, "id"), str_of(v, "title"));
    let _ = writeln!(out, "State     {} {}", state.glyph(), state.as_str());
    if let Some(err) = v["error"].as_str() {
        let _ = writeln!(out, "Error     {err}");
    }
    if let Some(project) = v["project"].get("name").and_then(Value::as_str) {
        let _ = writeln!(out, "Project   {project}");
    }
    let model = str_of(v, "model");
    let _ = writeln!(
        out,
        "Agent     {}{}{} · {} · {} access",
        str_of(v, "instance"),
        if model.is_empty() { String::new() } else { format!(" {model}") },
        v["effort"].as_str().map(|e| format!(" ({e})")).unwrap_or_default(),
        str_of(v, "mode"),
        str_of(v, "access")
    );
    if let Some(branch) = v["branch"].as_str() {
        let _ = writeln!(out, "Branch    {branch}{}", v["worktree"].as_str().map(|w| format!("  ({w})")).unwrap_or_default());
    }
    let u = &v["usage"];
    let _ = writeln!(
        out,
        "Turns     {} · {} queued · {} in / {} out tokens · ${:.2}",
        v["turns"],
        v["queued"],
        u["input_tokens"],
        u["output_tokens"],
        u["cost_usd"].as_f64().unwrap_or(0.0)
    );
    if let Some(parent) = v["parent"].as_str() {
        let _ = writeln!(out, "Parent    {parent}");
    }
    for child in v["children"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "Child     {}  {}  {}", str_of(child, "id"), str_of(child, "state"), str_of(child, "title"));
    }
    for p in v["pending"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "Pending   {}  {}", str_of(p, "id"), str_of(p, "summary"));
    }
    if let Some(msg) = v["last_message"].as_str() {
        let _ = writeln!(out, "\n{}", msg.trim());
    }
    out.trim_end().to_owned()
}

pub fn wait_entry(v: &Value) -> String {
    let mut out = format!("{}  {}  {}\n", str_of(v, "id"), str_of(v, "state"), str_of(v, "title"));
    for p in v["pending"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "  needs you: {}  ({})", str_of(p, "summary"), str_of(p, "id"));
    }
    if let Some(err) = v["error"].as_str() {
        let _ = writeln!(out, "  error: {err}");
    }
    if let Some(msg) = v["last_message"].as_str() {
        let _ = writeln!(out, "\n{}\n", msg.trim());
    }
    out
}

pub fn diff_stat(files: &[FileChange]) -> String {
    if files.is_empty() {
        return "No changes.".into();
    }
    let mut out = String::new();
    let (mut adds, mut dels) = (0u64, 0u64);
    for f in files {
        adds += f.additions as u64;
        dels += f.deletions as u64;
        let _ = writeln!(out, "{:>6} {:>6}  {}", format!("+{}", f.additions), format!("-{}", f.deletions), f.path);
    }
    let _ = write!(out, "{} files, +{adds} -{dels}", files.len());
    out
}

pub fn terminals(list: &[TerminalInfo]) -> String {
    if list.is_empty() {
        return "No terminals.".into();
    }
    let mut out = String::new();
    for t in list {
        let state = match (t.running, t.exit_code) {
            (true, _) => "running".to_owned(),
            (false, Some(code)) => format!("exited {code}"),
            (false, None) => "closed".to_owned(),
        };
        let what = if t.command.is_empty() { "shell" } else { &t.command };
        let _ = writeln!(out, "{}  {}  {}  {}", t.id, pad(&state, 10), what, t.cwd);
    }
    out.trim_end().to_owned()
}

pub fn search(hits: &[Value]) -> String {
    if hits.is_empty() {
        return "No matches.".into();
    }
    let mut out = String::new();
    for h in hits {
        let _ = writeln!(out, "{}  {}\n    {}", str_of(h, "thread"), str_of(h, "title"), str_of(h, "excerpt").replace('\n', " "));
    }
    out.trim_end().to_owned()
}

/// A whole timeline, as `orbit log` prints it.
pub fn timeline(thread: &Thread, items: &[Item], full: bool) -> String {
    let mut printer = Printer::new(full, false);
    let mut out = printer.header(thread);
    for item in items {
        out.push_str(&printer.item(item));
    }
    out.trim_end().to_owned()
}

/// Prints timeline items and deltas as they arrive, without repeating
/// what it already printed.
pub struct Printer {
    full: bool,
    json: bool,
    /// Items whose text was streamed (their final upsert adds a newline).
    streamed: HashSet<ItemId>,
    /// (item, status) pairs already printed.
    printed: HashSet<(ItemId, u8)>,
    /// The item whose text is mid-line.
    open_line: Option<ItemId>,
    last_state: Option<State>,
}

fn status_key(item: &Item) -> u8 {
    match (&item.body, item.status) {
        (ItemBody::Request(r), _) if r.resolution.is_some() => 10,
        (_, ItemStatus::Streaming) => 0,
        (_, ItemStatus::Queued) => 1,
        (_, ItemStatus::Done) => 2,
        (_, ItemStatus::Failed) => 3,
        (_, ItemStatus::Cancelled) => 4,
    }
}

impl Printer {
    pub fn new(full: bool, json: bool) -> Self {
        Self { full, json, streamed: HashSet::new(), printed: HashSet::new(), open_line: None, last_state: None }
    }

    fn line(v: Value) -> String {
        format!("{v}\n")
    }

    /// Ends a streamed line before printing something else.
    fn close(&mut self) -> String {
        if self.open_line.take().is_some() && !self.json { "\n".into() } else { String::new() }
    }

    pub fn header(&mut self, thread: &Thread) -> String {
        self.last_state = Some(State::of(thread));
        if self.json {
            return Self::line(json!({"event": "thread", "id": thread.id.to_string(), "title": json::title(thread), "state": State::of(thread).as_str()}));
        }
        format!("{}  {}  [{}]\n", thread.id, json::title(thread), State::of(thread).as_str())
    }

    pub fn notice(&mut self, text: &str) -> String {
        let close = self.close();
        if self.json {
            return close + &Self::line(json!({"event": "notice", "text": text}));
        }
        format!("{close}{text}\n")
    }

    pub fn thread(&mut self, thread: &Thread) -> String {
        let state = State::of(thread);
        if self.last_state == Some(state) {
            return String::new();
        }
        self.last_state = Some(state);
        let close = self.close();
        if self.json {
            return close + &Self::line(json!({"event": "state", "id": thread.id.to_string(), "state": state.as_str()}));
        }
        format!("{close}── {} ──\n", state.as_str())
    }

    pub fn delta(&mut self, item: ItemId, delta: &Delta) -> String {
        let text = match delta {
            Delta::Text(t) => t,
            Delta::Reasoning(t) if self.full => t,
            _ => return String::new(),
        };
        if self.json {
            return Self::line(json!({"event": "delta", "item": item.to_string(), "text": text}));
        }
        let mut out = String::new();
        if self.open_line != Some(item) {
            out.push_str(&self.close());
            self.open_line = Some(item);
        }
        self.streamed.insert(item);
        out.push_str(text);
        out
    }

    pub fn item(&mut self, item: &Item) -> String {
        let key = (item.id, status_key(item));
        if !self.printed.insert(key) {
            return String::new();
        }
        if self.json {
            if item.status == ItemStatus::Streaming && !matches!(item.body, ItemBody::Tool(_)) {
                return String::new();
            }
            return Self::line(json!({"event": "item", "item": json::item(item, self.full)}));
        }
        match &item.body {
            ItemBody::Assistant { text } | ItemBody::Reasoning { text } => {
                let reasoning = matches!(item.body, ItemBody::Reasoning { .. });
                if reasoning && !self.full {
                    return String::new();
                }
                if item.status == ItemStatus::Streaming {
                    // Deltas print it.
                    return String::new();
                }
                if self.streamed.remove(&item.id) {
                    // Already on screen; end its line.
                    return if self.open_line == Some(item.id) {
                        self.open_line = None;
                        "\n".into()
                    } else {
                        String::new()
                    };
                }
                let close = self.close();
                let body = if reasoning { indent(text, "  ~ ") } else { format!("{}\n", text.trim_end()) };
                format!("{close}{body}")
            }
            _ => {
                let close = self.close();
                close + &self.other(item)
            }
        }
    }

    fn other(&self, item: &Item) -> String {
        match &item.body {
            ItemBody::User(m) => {
                let queued = if item.status == ItemStatus::Queued { " (queued)" } else { "" };
                let steer = if m.steer { " (steer)" } else { "" };
                format!("\n› {}{queued}{steer}\n", m.text.trim().replace('\n', "\n  "))
            }
            ItemBody::Tool(call) => {
                let title = if call.title.is_empty() { call.name.as_str() } else { call.title.as_str() };
                let mark = match call.kind {
                    ToolKind::Command => "$",
                    ToolKind::Edit => "✎",
                    ToolKind::Read => "▸",
                    ToolKind::Search => "⌕",
                    ToolKind::Task => "⇢",
                    _ => "·",
                };
                match item.status {
                    ItemStatus::Streaming if title.is_empty() => String::new(),
                    ItemStatus::Streaming => format!("  {mark} {}\n", json::one_line(title, 160)),
                    status => {
                        let mut out = String::new();
                        let result = match (status, call.exit_code) {
                            (ItemStatus::Failed, _) => " failed".to_owned(),
                            (ItemStatus::Cancelled, _) => " cancelled".to_owned(),
                            (_, Some(code)) if code != 0 => format!(" exit {code}"),
                            _ => String::new(),
                        };
                        if self.printed.contains(&(item.id, 0)) {
                            if !result.is_empty() {
                                let _ = writeln!(out, "    →{result}");
                            }
                        } else {
                            let _ = writeln!(out, "  {mark} {}{result}", json::one_line(title, 160));
                        }
                        for f in &call.files {
                            let _ = writeln!(out, "    {} +{} -{}", f.path, f.additions, f.deletions);
                        }
                        if self.full && !call.output.trim().is_empty() {
                            out.push_str(&indent(call.output.trim_end(), "    │ "));
                        }
                        out
                    }
                }
            }
            ItemBody::Plan { steps } => {
                let mut out = String::from("  Plan:\n");
                for s in steps {
                    let mark = match s.state {
                        StepState::Done => "[x]",
                        StepState::Active => "[>]",
                        StepState::Pending => "[ ]",
                    };
                    let _ = writeln!(out, "    {mark} {}", s.text);
                }
                out
            }
            ItemBody::Request(r) => match &r.resolution {
                Some(decision) => {
                    let what = match decision {
                        Decision::Allow => "allowed".to_owned(),
                        Decision::AllowForSession => "allowed for the session".to_owned(),
                        Decision::Deny { feedback } if feedback.is_empty() => "denied".to_owned(),
                        Decision::Deny { feedback } => format!("denied: {feedback}"),
                        Decision::Answer { text } => format!("answered: {}", json::one_line(text, 100)),
                    };
                    format!("  ? {} → {what}\n", json::request_summary(&r.kind))
                }
                None => {
                    let mut out = format!("  ? Needs you: {}  ({})\n", json::request_summary(&r.kind), r.id);
                    if let RequestKind::Question { options, .. } = &r.kind
                        && !options.is_empty()
                    {
                        let _ = writeln!(out, "    Options: {}", options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>().join(" | "));
                    }
                    let _ = writeln!(out, "    Answer with `orbit approve`, `orbit deny` or `orbit answer`.");
                    out
                }
            },
            ItemBody::Notice { level, text } => {
                let tag = match level {
                    NoticeLevel::Info => "note",
                    NoticeLevel::Warning => "warning",
                    NoticeLevel::Error => "error",
                };
                format!("  [{tag}] {text}\n")
            }
            ItemBody::TurnEnd(s) => {
                let files = if s.files.is_empty() { String::new() } else { format!(" · {} files", s.files.len()) };
                let stopped = if s.interrupted { " · stopped" } else { "" };
                format!("── turn {} · {:.1}s{files}{stopped} ──\n", item.turn, s.duration_ms as f64 / 1000.0)
            }
            ItemBody::Handoff(h) => format!("── handed over from {} to {} ──
", h.from, h.to),
            ItemBody::Assistant { .. } | ItemBody::Reasoning { .. } => String::new(),
        }
    }
}

fn indent(text: &str, prefix: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        out.push_str(prefix);
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Removes terminal escape sequences from a byte stream, keeping text and
/// line breaks. Keeps its state between chunks.
#[derive(Default)]
pub struct AnsiStripper {
    state: Ansi,
    /// A carriage return waiting to see whether a line feed follows.
    cr: bool,
    /// Bytes of an incomplete UTF-8 character.
    partial: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Ansi {
    #[default]
    Text,
    Esc,
    Csi,
    /// OSC, DCS and friends: until BEL or ESC \.
    String,
    StringEsc,
}

impl AnsiStripper {
    pub fn feed(&mut self, data: &[u8]) -> String {
        let mut bytes = std::mem::take(&mut self.partial);
        for &b in data {
            match self.state {
                Ansi::Text => match b {
                    0x1b => self.state = Ansi::Esc,
                    b'\r' => {
                        if self.cr {
                            bytes.push(b'\n');
                        }
                        self.cr = true;
                        continue;
                    }
                    b'\n' => {
                        self.cr = false;
                        bytes.push(b'\n');
                    }
                    b'\t' => bytes.push(b),
                    0x00..=0x1f | 0x7f => {}
                    _ => {
                        if self.cr {
                            // A lone CR (progress bars): start a new line.
                            bytes.push(b'\n');
                            self.cr = false;
                        }
                        bytes.push(b);
                    }
                },
                Ansi::Esc => {
                    self.state = match b {
                        b'[' => Ansi::Csi,
                        b']' | b'P' | b'X' | b'^' | b'_' => Ansi::String,
                        // Two-byte sequence with an intermediate: skip one more.
                        b'(' | b')' | b'*' | b'+' | b'#' | b'%' => Ansi::Esc,
                        _ => Ansi::Text,
                    };
                }
                Ansi::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = Ansi::Text;
                    }
                }
                Ansi::String => match b {
                    0x07 => self.state = Ansi::Text,
                    0x1b => self.state = Ansi::StringEsc,
                    _ => {}
                },
                Ansi::StringEsc => self.state = if b == b'\\' { Ansi::Text } else { Ansi::String },
            }
        }
        // Keep an incomplete trailing UTF-8 sequence for the next chunk.
        let keep = incomplete_utf8_tail(&bytes);
        self.partial = bytes.split_off(bytes.len() - keep);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

fn incomplete_utf8_tail(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let b = bytes[bytes.len() - back];
        if b & 0xc0 == 0x80 {
            continue;
        }
        let need = match b {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        return if need > back { back } else { 0 };
    }
    0
}

pub fn strip_ansi(data: &[u8]) -> String {
    let mut stripper = AnsiStripper::default();
    let mut out = stripper.feed(data);
    out.push_str(&String::from_utf8_lossy(&stripper.partial));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escapes_and_keeps_lines() {
        let raw = b"\x1b[?25l\x1b]0;title\x07\x1b[32mok\x1b[0m done\r\nnext\r\n";
        assert_eq!(strip_ansi(raw), "ok done\nnext\n");
        assert_eq!(strip_ansi(b"10%\r20%\r\n"), "10%\n20%\n");
    }

    #[test]
    fn escapes_and_characters_split_across_chunks() {
        let mut s = AnsiStripper::default();
        let text = "é\x1b[31mx";
        let bytes = text.as_bytes();
        let mut out = String::new();
        for b in bytes {
            out.push_str(&s.feed(std::slice::from_ref(b)));
        }
        assert_eq!(out, "éx");
    }

    #[test]
    fn streamed_text_is_not_printed_twice() {
        let mut p = Printer::new(false, false);
        let id = ItemId::new();
        let mut out = p.delta(id, &Delta::Text("Hel".into()));
        out += &p.delta(id, &Delta::Text("lo".into()));
        out += &p.item(&Item { id, turn: 1, status: ItemStatus::Done, body: ItemBody::Assistant { text: "Hello".into() } });
        assert_eq!(out, "Hello\n");
    }
}
