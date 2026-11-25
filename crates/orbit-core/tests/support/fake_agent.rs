//! A scripted stand-in for ACP agents, `pi --mode rpc` and `opencode serve`,
//! used by the adapter tests. The script is `fake-agent.json` next to the
//! executable (tests copy the binary into a temp dir under the agent's name):
//!
//! ```json
//! {"protocol": "acp", "handlers": {"initialize": {...}}, "steps": [
//!     {"expect": "session/prompt", "reply": false},
//!     {"send": {"method": "session/update", "params": {}}},
//!     {"expectNotify": "session/cancel"},
//!     {"respond": "session/prompt", "result": {"stopReason": "cancelled"}},
//!     {"awaitResponse": 7},
//!     {"exit": 1, "stderr": "boom"}
//! ]}
//! ```
//!
//! * `acp` and `pi` speak JSON lines on stdio. A request is matched by its
//!   `method` (ACP) or `type` (Pi). Requests the current step does not
//!   expect are answered from `handlers` (a value, `{"!error": {...}}`, or
//!   successive answers under `"!seq"`); others get "method not found" (ACP)
//!   or an unsuccessful response (Pi). `"reply": false` on an `expect` holds
//!   the answer until a later `respond` step.
//! * `opencode` serves HTTP on the `--port=` it is given and prints the
//!   line `opencode serve` prints. `routes` maps `"METHOD /path"` to a JSON
//!   body; `on` maps a route to events pushed on `/event` after it is hit;
//!   `events` are pushed as soon as the event stream connects.
//!
//! Everything received is appended to `fake-agent.log.jsonl`.
//!
//! Three more roles for the status tests, chosen before any script is read:
//! * with `ORBIT_FAKE_HOOK=1` it is the hook invoker (`orbit-hook`);
//! * with `--pretend-claude` it plays Claude Code in a terminal, firing the
//!   hooks Orbit injected the way Claude Code would;
//! * with `ORBIT_FAKE_SLEEP_MS` it only sleeps (an orphan holding stdout).
//!
//! The step `{"spawnOrphan": ms}` starts such a sleeper, which inherits
//! stdout and so keeps the pipe open after the fake exits.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value, json};

fn main() {
    if std::env::var_os("ORBIT_FAKE_HOOK").is_some() {
        std::process::exit(orbit_core::status::hook::invoker_main());
    }
    if let Some(ms) = std::env::var("ORBIT_FAKE_SLEEP_MS").ok().and_then(|v| v.parse().ok()) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--pretend-claude") {
        pretend_claude();
        return;
    }
    let dir: PathBuf = std::env::current_exe().unwrap().parent().unwrap().to_owned();
    let script: Value = sonic_rs::from_str(&std::fs::read_to_string(dir.join("fake-agent.json")).unwrap()).unwrap();
    let log = Arc::new(Mutex::new(
        std::fs::OpenOptions::new().create(true).append(true).open(dir.join("fake-agent.log.jsonl")).unwrap(),
    ));
    std::fs::write(dir.join("fake-agent.args"), std::env::args().skip(1).collect::<Vec<_>>().join("\n")).unwrap();
    match script.get("protocol").and_then(|p| p.as_str()).unwrap_or("acp") {
        "opencode" => opencode(script, log),
        protocol => jsonl(protocol == "pi", script, log),
    }
}

// ── JSON lines (ACP, Pi) ─────────────────────────────────────────────────

struct Lines {
    pi: bool,
    handlers: Value,
    served: HashMap<String, usize>,
    held: HashMap<String, Value>,
    log: Arc<Mutex<std::fs::File>>,
    stdin: BufReader<std::io::Stdin>,
}

fn write_line(message: &Value) {
    let mut out = std::io::stdout();
    writeln!(out, "{}", sonic_rs::to_string(message).unwrap()).unwrap();
    out.flush().unwrap();
}

impl Lines {
    fn name(&self, message: &Value) -> Option<String> {
        let key = if self.pi { "type" } else { "method" };
        message.get(key).and_then(|m| m.as_str()).map(str::to_owned)
    }

    fn is_request(&self, message: &Value) -> bool {
        message.get("id").is_some() && self.name(message).is_some()
    }

    fn reply(&self, message: &Value, result: Value) {
        let id = message.get("id").cloned().unwrap_or_default();
        if self.pi {
            let command = self.name(message).unwrap_or_default();
            match result.get("!error") {
                Some(error) => write_line(&json!({"type": "response", "id": id, "command": command, "success": false, "error": error})),
                None => write_line(&json!({"type": "response", "id": id, "command": command, "success": true, "data": result})),
            }
        } else {
            match result.get("!error") {
                Some(error) => write_line(&json!({"jsonrpc": "2.0", "id": id, "error": error})),
                None => write_line(&json!({"jsonrpc": "2.0", "id": id, "result": result})),
            }
        }
    }

    fn next(&mut self) -> Option<Value> {
        loop {
            let mut line = String::new();
            if self.stdin.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            writeln!(self.log.lock().unwrap(), "{line}").unwrap();
            let message: Value = sonic_rs::from_str(line).expect("client sent invalid JSON");
            if !self.pi {
                assert_eq!(message.get("jsonrpc").and_then(|v| v.as_str()), Some("2.0"), "ACP messages are JSON-RPC 2.0");
            }
            return Some(message);
        }
    }

    fn answer_from_handlers(&mut self, message: &Value) {
        if !self.is_request(message) {
            return;
        }
        let method = self.name(message).unwrap();
        let served = self.served.entry(method.clone()).or_default();
        let n = *served;
        *served += 1;
        let handler = self.handlers.get(&method).cloned().map(|h| match h.get("!seq").and_then(|s| s.as_array()) {
            Some(seq) => seq.get(n.min(seq.len().saturating_sub(1))).cloned().unwrap_or_default(),
            None => h,
        });
        match handler {
            Some(result) => self.reply(message, result),
            None => self.reply(message, json!({"!error": {"code": -32601, "message": format!("unknown method {method}")}})),
        }
    }

    fn wait(&mut self, want: &str, request: bool) -> Option<Value> {
        loop {
            let message = self.next()?;
            let matches = self.name(&message).as_deref() == Some(want) && (self.is_request(&message) == request);
            if matches {
                return Some(message);
            }
            self.answer_from_handlers(&message);
        }
    }

    fn wait_response(&mut self, id: &Value) -> bool {
        loop {
            let Some(message) = self.next() else { return false };
            if self.name(&message).is_none() && message.get("id") == Some(id) {
                return true;
            }
            // Pi's own `extension_ui_response` is a command, not a response.
            if self.pi && message.get("id") == Some(id) {
                return true;
            }
            self.answer_from_handlers(&message);
        }
    }
}

fn jsonl(pi: bool, script: Value, log: Arc<Mutex<std::fs::File>>) {
    let mut fake = Lines {
        pi,
        handlers: script.get("handlers").cloned().unwrap_or_else(|| json!({})),
        served: HashMap::new(),
        held: HashMap::new(),
        log,
        stdin: BufReader::new(std::io::stdin()),
    };
    let steps = script.get("steps").and_then(|s| s.as_array()).cloned().unwrap_or_default();
    for step in steps.iter() {
        if let Some(method) = step.get("expect").and_then(|m| m.as_str()) {
            let Some(message) = fake.wait(method, true) else { return };
            if step.get("reply").and_then(|r| r.as_bool()) == Some(false) {
                fake.held.insert(method.to_owned(), message);
            } else if let Some(error) = step.get("error") {
                fake.reply(&message, json!({"!error": error}));
            } else {
                fake.reply(&message, step.get("result").cloned().unwrap_or_else(|| json!({})));
            }
        } else if let Some(method) = step.get("expectNotify").and_then(|m| m.as_str()) {
            if fake.wait(method, false).is_none() {
                return;
            }
        } else if let Some(method) = step.get("respond").and_then(|m| m.as_str()) {
            let message = fake.held.remove(method).expect("respond to a held request");
            fake.reply(&message, step.get("result").cloned().unwrap_or_else(|| json!({})));
        } else if let Some(message) = step.get("send") {
            write_line(message);
        } else if let Some(id) = step.get("awaitResponse") {
            if !fake.wait_response(id) {
                return;
            }
        } else if let Some(ms) = step.get("spawnOrphan").and_then(|m| m.as_u64()) {
            std::process::Command::new(std::env::current_exe().unwrap())
                .env("ORBIT_FAKE_SLEEP_MS", ms.to_string())
                .stdin(std::process::Stdio::null())
                .spawn()
                .unwrap();
        } else if let Some(ms) = step.get("sleepMs").and_then(|m| m.as_u64()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        } else if let Some(code) = step.get("exit").and_then(|c| c.as_i64()) {
            if let Some(text) = step.get("stderr").and_then(|t| t.as_str()) {
                eprintln!("{text}");
            }
            std::process::exit(code as i32);
        }
    }
    while let Some(message) = fake.next() {
        fake.answer_from_handlers(&message);
    }
}

// ── HTTP (OpenCode) ──────────────────────────────────────────────────────

type Streams = Arc<Mutex<Vec<std::net::TcpStream>>>;

fn opencode(script: Value, log: Arc<Mutex<std::fs::File>>) {
    let port: u16 = std::env::args()
        .find_map(|a| a.strip_prefix("--port=").map(|p| p.parse().unwrap()))
        .expect("--port=");
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    let password = std::env::var("OPENCODE_SERVER_PASSWORD").expect("a server password");
    println!("opencode server listening on http://127.0.0.1:{port}");
    std::io::stdout().flush().unwrap();
    let streams: Streams = Arc::new(Mutex::new(Vec::new()));
    let script = Arc::new(script);
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let (script, log, streams, password) = (script.clone(), log.clone(), streams.clone(), password.clone());
        std::thread::spawn(move || serve(conn, &script, &log, &streams, &password));
    }
}

fn push(streams: &Streams, event: &Value) {
    let data = format!("data: {}\n\n", sonic_rs::to_string(event).unwrap());
    let mut streams = streams.lock().unwrap();
    streams.retain_mut(|s| s.write_all(data.as_bytes()).and_then(|_| s.flush()).is_ok());
}

fn serve(mut conn: std::net::TcpStream, script: &Value, log: &Arc<Mutex<std::fs::File>>, streams: &Streams, password: &str) {
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("").to_owned());
    let (mut length, mut auth) = (0usize, String::new());
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let lower = header.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            length = v.trim().parse().unwrap();
        }
        if lower.starts_with("authorization:") {
            auth = header["authorization:".len()..].trim().to_owned();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let body = String::from_utf8_lossy(&body).into_owned();
    use base64::Engine as _;
    let expected = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}")));
    if auth != expected {
        let _ = conn.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
        return;
    }
    let route = format!("{method} {path}");
    let logged = json!({"route": route, "body": if body.is_empty() { Value::new() } else { sonic_rs::from_str(&body).unwrap_or_else(|_| Value::from(body.as_str())) }});
    writeln!(log.lock().unwrap(), "{}", sonic_rs::to_string(&logged).unwrap()).unwrap();
    if route == "GET /event" {
        let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n");
        let _ = conn.flush();
        streams.lock().unwrap().push(conn);
        if let Some(events) = script.get("events").and_then(|e| e.as_array()) {
            for event in events.iter() {
                push(streams, event);
            }
        }
        return;
    }
    let reply = script.get("routes").and_then(|r| r.get(&route)).cloned();
    let (status, text) = match reply {
        Some(value) => (200, sonic_rs::to_string(&value).unwrap()),
        None => (404, "{\"error\":\"not found\"}".to_owned()),
    };
    let head = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", text.len());
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(text.as_bytes());
    let _ = conn.flush();
    drop(conn);
    if let Some(events) = script.get("on").and_then(|o| o.get(&route)).and_then(|e| e.as_array()) {
        for event in events.iter() {
            std::thread::sleep(std::time::Duration::from_millis(5));
            push(streams, event);
        }
    }
}

// ── Claude Code in a terminal ────────────────────────────────────────────

/// Fires Claude Code's hooks through the command in the settings file Orbit
/// injected, prompting on the terminal between them.
fn pretend_claude() {
    let settings = std::env::var("ORBIT_CLAUDE_SETTINGS").expect("Orbit injected no Claude settings");
    let text = std::fs::read_to_string(&settings).unwrap();
    let json: Value = sonic_rs::from_str(&text).unwrap();
    let command = json
        .pointer(&sonic_rs::pointer!["hooks", "UserPromptSubmit", 0, "hooks", 0, "command"])
        .and_then(|c| c.as_str())
        .expect("a UserPromptSubmit hook")
        .to_owned();
    let (program, args) = split_command(&command);
    let fire = |payload: Value| {
        let mut child = std::process::Command::new(&program)
            .args(&args)
            .env("ORBIT_FAKE_HOOK", "1")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(sonic_rs::to_string(&payload).unwrap().as_bytes()).unwrap();
        assert!(child.wait().unwrap().success(), "the hook command failed");
    };
    let transcript = "/home/u/.claude/projects/p/s.jsonl";
    let mut out = std::io::stdout();
    let mut line = String::new();
    write!(out, "\x1b]0;\u{2733} Claude Code\x07").unwrap();
    out.flush().unwrap();
    fire(json!({"hook_event_name": "SessionStart", "transcript_path": transcript, "source": "startup"}));
    fire(json!({"hook_event_name": "UserPromptSubmit", "transcript_path": transcript, "prompt": "fix it"}));
    fire(json!({"hook_event_name": "PreToolUse", "transcript_path": transcript, "tool_name": "Bash", "tool_input": {"command": "rm -rf build"}}));
    fire(json!({"hook_event_name": "Notification", "transcript_path": transcript, "notification_type": "permission_prompt",
                "message": "Claude needs your permission to use Bash"}));
    writeln!(out, "PRETEND: Do you want to proceed?").unwrap();
    out.flush().unwrap();
    std::io::stdin().read_line(&mut line).unwrap();
    fire(json!({"hook_event_name": "PostToolUse", "transcript_path": transcript, "tool_name": "Bash"}));
    fire(json!({"hook_event_name": "Stop", "transcript_path": transcript}));
    writeln!(out, "PRETEND: done").unwrap();
    out.flush().unwrap();
    line.clear();
    std::io::stdin().read_line(&mut line).unwrap();
    fire(json!({"hook_event_name": "SessionEnd", "transcript_path": transcript, "reason": "prompt_input_exit"}));
    writeln!(out, "PRETEND: bye").unwrap();
}

/// Splits a hook command the way a shell would for the forms Orbit writes:
/// a quoted or bare program, then plain arguments.
fn split_command(command: &str) -> (String, Vec<String>) {
    let command = command.trim();
    let (program, rest) = match command.chars().next() {
        Some(q @ ('\'' | '"')) => {
            let end = command[1..].find(q).expect("closing quote") + 1;
            (command[1..end].replace("'\''", "'"), &command[end + 1..])
        }
        _ => match command.split_once(' ') {
            Some((program, rest)) => (program.to_owned(), rest),
            None => (command.to_owned(), ""),
        },
    };
    (program, rest.split_whitespace().map(str::to_owned).collect())
}
