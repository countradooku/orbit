//! A streaming scanner for the few OSC sequences status detection needs,
//! run over raw PTY output.
//!
//! * **Zero-copy.** A sequence that lies inside one read is handed out as a
//!   slice of that read. Only a sequence split across reads is copied, into
//!   a small buffer capped at [`MAX_OSC`] bytes.
//! * **Cheap on bulk output.** `memchr` jumps from ESC to ESC; colour codes
//!   (`ESC [`) are rejected on their second byte.
//! * **Garbage-proof.** Unterminated, oversized or interrupted sequences are
//!   dropped without affecting what follows; nothing panics on any input.
//!
//! Sequences reported:
//! * `OSC 7501 ; <report>` — the program-status report agents and Orbit's
//!   hook invoker write ([`parse_report`]).
//! * `OSC 0 ;` / `OSC 2 ;` — the window title (heuristics).
//! * `OSC 133 ; A` — a shell-integration prompt mark: the shell is back at
//!   its prompt, so no agent runs in the foreground.

use memchr::{memchr, memchr2};
use orbit_proto::{AgentState, NeedKind};

/// Longest OSC payload kept across reads. Longer ones are skipped.
pub const MAX_OSC: usize = 4096;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// One recognised sequence. Borrowed from the scanned bytes when possible.
#[derive(Debug, PartialEq, Eq)]
pub enum Osc<'a> {
    /// The payload after `7501;`.
    Status(&'a [u8]),
    Title(&'a [u8]),
    Prompt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    /// The last read ended on an ESC.
    Esc,
    /// Inside an OSC body; `skip` once it outgrew the buffer.
    Body { skip: bool },
    /// Inside an OSC body and the last read ended on an ESC (maybe ST).
    BodyEsc { skip: bool },
}

/// Scans a byte stream read in arbitrary pieces.
#[derive(Debug)]
pub struct OscScanner {
    state: State,
    buf: Vec<u8>,
}

impl Default for OscScanner {
    fn default() -> Self {
        Self { state: State::Ground, buf: Vec::new() }
    }
}

enum End {
    /// Terminated after `len` body bytes; `skip` terminator bytes follow.
    Done { len: usize, skip: usize },
    /// An ESC that does not start ST at `at`: the OSC is abandoned and the
    /// ESC begins whatever comes next.
    Abort { at: usize },
    /// No terminator yet; `trailing_esc` when the data ends on an ESC.
    Open { trailing_esc: bool },
}

fn find_end(body: &[u8]) -> End {
    match memchr2(BEL, ESC, body) {
        None => End::Open { trailing_esc: false },
        Some(p) if body[p] == BEL => End::Done { len: p, skip: 1 },
        Some(p) if p + 1 == body.len() => End::Open { trailing_esc: true },
        Some(p) if body[p + 1] == b'\\' => End::Done { len: p, skip: 2 },
        Some(p) => End::Abort { at: p },
    }
}

impl OscScanner {
    /// Scans `data`, calling `found` for each complete sequence in order.
    pub fn scan(&mut self, data: &[u8], mut found: impl FnMut(Osc<'_>)) {
        let mut i = 0;
        while i < data.len() {
            match self.state {
                State::Ground => {
                    let Some(p) = memchr(ESC, &data[i..]) else { return };
                    let at = i + p;
                    if at + 1 == data.len() {
                        self.state = State::Esc;
                        return;
                    }
                    if data[at + 1] != b']' {
                        i = at + 1;
                        continue;
                    }
                    i = self.body(data, at + 2, &mut found);
                }
                State::Esc => {
                    if data[i] == b']' {
                        self.buf.clear();
                        i = self.body(data, i + 1, &mut found);
                    } else {
                        self.state = State::Ground;
                    }
                }
                State::Body { skip } => {
                    i = self.continue_body(data, i, skip, &mut found);
                }
                State::BodyEsc { skip } => {
                    if data[i] == b'\\' {
                        if !skip {
                            if let Some(osc) = classify(&self.buf) {
                                found(osc);
                            }
                        }
                        self.buf.clear();
                        self.state = State::Ground;
                        i += 1;
                    } else {
                        // The ESC began something else; re-scan it.
                        self.buf.clear();
                        self.state = State::Esc;
                    }
                }
            }
        }
    }

    /// An OSC body starts at `start` in `data`: report it if it ends there,
    /// else start buffering. Returns where scanning continues.
    fn body(&mut self, data: &[u8], start: usize, found: &mut impl FnMut(Osc<'_>)) -> usize {
        let body = &data[start..];
        match find_end(body) {
            End::Done { len, skip } => {
                // Oversized sequences are dropped however they were split.
                if len <= MAX_OSC
                    && let Some(osc) = classify(&body[..len])
                {
                    found(osc);
                }
                self.state = State::Ground;
                start + len + skip
            }
            End::Abort { at } => {
                self.state = State::Ground;
                start + at
            }
            End::Open { trailing_esc } => {
                let keep = if trailing_esc { &body[..body.len() - 1] } else { body };
                self.buf.clear();
                let skip = keep.len() > MAX_OSC;
                if !skip {
                    self.buf.extend_from_slice(keep);
                }
                self.state = if trailing_esc { State::BodyEsc { skip } } else { State::Body { skip } };
                data.len()
            }
        }
    }

    fn continue_body(&mut self, data: &[u8], start: usize, skip: bool, found: &mut impl FnMut(Osc<'_>)) -> usize {
        let body = &data[start..];
        match find_end(body) {
            End::Done { len, skip: term } => {
                if !skip && self.buf.len() + len <= MAX_OSC {
                    self.buf.extend_from_slice(&body[..len]);
                    if let Some(osc) = classify(&self.buf) {
                        found(osc);
                    }
                }
                self.buf.clear();
                self.state = State::Ground;
                start + len + term
            }
            End::Abort { at } => {
                self.buf.clear();
                self.state = State::Ground;
                start + at
            }
            End::Open { trailing_esc } => {
                let keep = if trailing_esc { &body[..body.len() - 1] } else { body };
                let skip = skip || self.buf.len() + keep.len() > MAX_OSC;
                if skip {
                    self.buf.clear();
                } else {
                    self.buf.extend_from_slice(keep);
                }
                self.state = if trailing_esc { State::BodyEsc { skip } } else { State::Body { skip } };
                data.len()
            }
        }
    }
}

fn classify(body: &[u8]) -> Option<Osc<'_>> {
    if let Some(rest) = body.strip_prefix(b"7501;") {
        return Some(Osc::Status(rest));
    }
    if let Some(rest) = body.strip_prefix(b"0;").or_else(|| body.strip_prefix(b"2;")) {
        return Some(Osc::Title(rest));
    }
    if body == b"133;A" || body.starts_with(b"133;A;") {
        return Some(Osc::Prompt);
    }
    None
}

/// What an `OSC 7501` report says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub agent: Option<String>,
    /// `None` for a report without a state (a probe, a detail update).
    pub state: Option<ReportState>,
    pub detail: Option<String>,
    /// The `?` probe a program sends to ask whether the terminal listens.
    pub probe: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportState {
    Is(AgentState),
    /// The program ended.
    Ended,
}

/// Parses a program-status report: `;`-separated `key=value` fields with
/// `%XX` escapes (`state=working;agent=claude;msg=Edit%3B main.rs`). A
/// bare first word is taken as the state. Unknown keys and values are
/// ignored, so newer programs never confuse Orbit.
pub fn parse_report(payload: &[u8]) -> Report {
    let mut report = Report::default();
    let mut kind = None;
    let mut state = None;
    for (n, field) in payload.split(|b| *b == b';').enumerate() {
        let (key, value) = match field.iter().position(|b| *b == b'=') {
            Some(eq) => (&field[..eq], &field[eq + 1..]),
            None if n == 0 => (&b"state"[..], field),
            None => continue,
        };
        let value = percent_decode(value);
        match key {
            b"state" | b"s" => {
                if value == "?" {
                    report.probe = true;
                } else {
                    state = Some(value.to_ascii_lowercase());
                }
            }
            b"kind" | b"k" => kind = Some(value.to_ascii_lowercase()),
            b"agent" | b"program" | b"name" => report.agent = Some(value).filter(|v| !v.is_empty()),
            b"msg" | b"message" | b"m" | b"tool" | b"detail" => report.detail = Some(value).filter(|v| !v.is_empty()),
            _ => {}
        }
    }
    if payload == b"?" {
        report.probe = true;
    }
    report.state = state.and_then(|s| state_word(&s, kind.as_deref()));
    report
}

/// The state a word names, as agents and hooks spell it.
pub fn state_word(word: &str, kind: Option<&str>) -> Option<ReportState> {
    let need = || match kind {
        Some("permission" | "approval" | "approve") => NeedKind::Permission,
        Some("auth" | "login" | "signin") => NeedKind::Auth,
        _ => NeedKind::Question,
    };
    Some(ReportState::Is(match word {
        "working" | "busy" | "running" | "thinking" | "active" => AgentState::Working,
        "blocked" | "waiting" | "needs-input" | "input" | "attention" => AgentState::NeedsYou(need()),
        "permission" | "approval" => AgentState::NeedsYou(NeedKind::Permission),
        "question" => AgentState::NeedsYou(NeedKind::Question),
        "auth" => AgentState::NeedsYou(NeedKind::Auth),
        "idle" | "done" | "ready" | "finished" | "complete" | "completed" | "interrupted" => AgentState::Idle,
        "failed" | "error" => AgentState::Failed,
        "ratelimited" | "rate-limited" | "rate_limited" | "quota" => AgentState::RateLimited,
        "exited" | "exit" | "ended" | "end" | "stopped" => return Some(ReportState::Ended),
        _ => return None,
    }))
}

fn percent_decode(value: &[u8]) -> String {
    let mut out = Vec::with_capacity(value.len());
    let mut i = 0;
    while i < value.len() {
        if value[i] == b'%'
            && i + 2 < value.len()
            && let (Some(h), Some(l)) = (hex(value[i + 1]), hex(value[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(value[i]);
            i += 1;
        }
    }
    // Control characters never reach interface copy.
    String::from_utf8_lossy(&out).chars().filter(|c| !c.is_control()).collect()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `%`-escapes what would break a report field (`;`, `%`, `=` and control
/// characters).
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            ';' | '%' | '=' => out.push_str(&format!("%{:02X}", c as u32)),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// The full sequence for a report, as the hook invoker writes it.
pub fn report_sequence(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut out = b"\x1b]7501;".to_vec();
    for (n, (key, value)) in fields.iter().enumerate() {
        if n > 0 {
            out.push(b';');
        }
        out.extend_from_slice(key.as_bytes());
        out.push(b'=');
        out.extend_from_slice(percent_encode(value).as_bytes());
    }
    out.push(BEL);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(osc: Osc<'_>) -> (u8, Vec<u8>) {
        match osc {
            Osc::Status(b) => (b'S', b.to_vec()),
            Osc::Title(b) => (b'T', b.to_vec()),
            Osc::Prompt => (b'P', Vec::new()),
        }
    }

    fn scan_pieces(pieces: &[&[u8]]) -> Vec<(u8, Vec<u8>)> {
        let mut scanner = OscScanner::default();
        let mut out = Vec::new();
        for piece in pieces {
            scanner.scan(piece, |o| out.push(owned(o)));
        }
        out
    }

    const STREAM: &[u8] = b"\x1b[31mred\x1b[0m\x1b]0;\xe2\x9c\xb3 Claude Code\x07text\x1b]7501;state=working;agent=claude\x1b\\more\x1b]8;;http://x\x07link\x1b]8;;\x07\x1b]133;A\x07$ \x1b]7501;state=blocked;kind=permission;msg=Bash%3B ls\x07";

    fn expected() -> Vec<(u8, Vec<u8>)> {
        vec![
            (b'T', "✳ Claude Code".as_bytes().to_vec()),
            (b'S', b"state=working;agent=claude".to_vec()),
            (b'P', Vec::new()),
            (b'S', b"state=blocked;kind=permission;msg=Bash%3B ls".to_vec()),
        ]
    }

    #[test]
    fn finds_sequences_in_one_read() {
        assert_eq!(scan_pieces(&[STREAM]), expected());
    }

    #[test]
    fn every_split_point_gives_the_same_result() {
        for cut in 0..=STREAM.len() {
            let (a, b) = STREAM.split_at(cut);
            assert_eq!(scan_pieces(&[a, b]), expected(), "split at {cut}");
        }
        // Byte by byte.
        let bytes: Vec<&[u8]> = STREAM.chunks(1).collect();
        assert_eq!(scan_pieces(&bytes), expected());
        // Every pair of cuts.
        for x in 0..STREAM.len() {
            for y in x..STREAM.len() {
                let pieces = [&STREAM[..x], &STREAM[x..y], &STREAM[y..]];
                assert_eq!(scan_pieces(&pieces), expected(), "cuts at {x} and {y}");
            }
        }
    }

    #[test]
    fn garbage_never_panics_and_never_poisons_what_follows() {
        // A deterministic pseudo-random stream rich in ESC, ], BEL and \.
        let alphabet = [0x1b, b']', 0x07, b'\\', b'7', b'5', b'0', b'1', b';', b'a', b'=', 0x9c, 0xff, b'\n'];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut noise = Vec::new();
        for _ in 0..200_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            noise.push(alphabet[(seed % alphabet.len() as u64) as usize]);
        }
        let mut scanner = OscScanner::default();
        let mut found = 0usize;
        let mut pos = 0;
        while pos < noise.len() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 97) as usize + 1;
            let end = (pos + len).min(noise.len());
            scanner.scan(&noise[pos..end], |o| {
                if let Osc::Status(p) | Osc::Title(p) = o {
                    assert!(p.len() <= MAX_OSC);
                    let _ = parse_report(p);
                }
                found += 1;
            });
            pos = end;
        }
        // Whatever state the noise left, a terminated sequence after a
        // reset byte is still seen.
        let mut after = Vec::new();
        scanner.scan(b"\x07\x07plain\x1b]7501;state=idle\x07", |o| after.push(owned(o)));
        assert_eq!(after.last(), Some(&(b'S', b"state=idle".to_vec())));
        let _ = found;
    }

    #[test]
    fn oversized_and_interrupted_sequences_are_dropped() {
        let mut big = b"\x1b]7501;msg=".to_vec();
        big.extend(std::iter::repeat_n(b'x', MAX_OSC * 3));
        big.extend_from_slice(b"\x07\x1b]7501;state=done\x07");
        for chunk in [7usize, 100, 4096, 70_000] {
            let pieces: Vec<&[u8]> = big.chunks(chunk).collect();
            assert_eq!(scan_pieces(&pieces), vec![(b'S', b"state=done".to_vec())], "chunk {chunk}");
        }
        // An OSC cut off by another escape sequence never swallows it.
        let cut = b"\x1b]7501;state=work\x1b[2J\x1b]0;title\x07";
        assert_eq!(scan_pieces(&[cut]), vec![(b'T', b"title".to_vec())]);
        let pieces: Vec<&[u8]> = cut.chunks(3).collect();
        assert_eq!(scan_pieces(&pieces), vec![(b'T', b"title".to_vec())]);
    }

    #[test]
    fn reports_parse_leniently() {
        let r = parse_report(b"state=blocked;kind=permission;agent=claude;msg=Bash%3B ls");
        assert_eq!(r.state, Some(ReportState::Is(AgentState::NeedsYou(NeedKind::Permission))));
        assert_eq!((r.agent.as_deref(), r.detail.as_deref()), (Some("claude"), Some("Bash; ls")));
        assert_eq!(parse_report(b"working").state, Some(ReportState::Is(AgentState::Working)));
        assert_eq!(parse_report(b"state=exited").state, Some(ReportState::Ended));
        assert!(parse_report(b"?").probe);
        assert_eq!(parse_report(b"state=teleporting;x=y").state, None);
        assert_eq!(parse_report(b"msg=%zz%4").detail.as_deref(), Some("%zz%4"));
        assert_eq!(parse_report(b"msg=a%07b").detail.as_deref(), Some("ab"), "control characters are dropped");
        let seq = report_sequence(&[("state", "blocked"), ("msg", "a;b=c%")]);
        let mut seen = Vec::new();
        OscScanner::default().scan(&seq, |o| seen.push(owned(o)));
        assert_eq!(parse_report(&seen[0].1).detail.as_deref(), Some("a;b=c%"));
    }
}
