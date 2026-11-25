//! A fake `codex app-server` for the adapter tests. It speaks the same
//! newline-delimited JSON-RPC (no `jsonrpc` field) and replays a script.
//!
//! The script is `fake-codex.json` next to the executable (tests copy the
//! binary into a temp dir as `codex[.exe]`):
//!
//! ```json
//! {"runs": [{"handlers": {"model/list": {"data": []}}, "steps": [
//!     {"expect": "turn/start", "result": {"turn": {"id": "t1"}}},
//!     {"send": {"method": "turn/started", "params": {}}},
//!     {"awaitResponse": 7},
//!     {"exit": 1, "stderr": "boom"}
//! ]}]}
//! ```
//!
//! Each start of the process takes the next entry of `runs` (counted in
//! `fake-codex.count`), so a crash test scripts the first process and the
//! restarted one separately. Requests the current step does not expect are
//! answered from `handlers` (a result, or `{"error": {...}}` under the key
//! `"!error"`, or successive answers under `"!seq"`), else with "method
//! not found". Every line received is
//! appended to `fake-codex.run<N>.jsonl` for the test to inspect. After the
//! last step the fake keeps answering handlers until stdin closes.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value, json};

fn main() {
    let dir: PathBuf = std::env::current_exe().unwrap().parent().unwrap().to_owned();
    let script: Value = sonic_rs::from_str(&std::fs::read_to_string(dir.join("fake-codex.json")).unwrap()).unwrap();
    let count_path = dir.join("fake-codex.count");
    let run: usize = std::fs::read_to_string(&count_path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    std::fs::write(&count_path, (run + 1).to_string()).unwrap();
    let runs = script.get("runs").and_then(|r| r.as_array()).expect("runs");
    let Some(run_script) = runs.get(run) else {
        eprintln!("fake codex: no script for run {run}");
        std::process::exit(9);
    };
    let mut fake = Fake {
        handlers: run_script.get("handlers").cloned().unwrap_or_else(|| json!({})),
        served: Default::default(),
        log: std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("fake-codex.run{run}.jsonl"))).unwrap(),
        stdin: BufReader::new(std::io::stdin()),
        stdout: std::io::stdout(),
    };
    let steps = run_script.get("steps").and_then(|s| s.as_array()).cloned().unwrap_or_default();
    for step in steps.iter() {
        if let Some(method) = step.get("expect").and_then(|m| m.as_str()) {
            let Some(id) = fake.wait_request(method) else { return };
            match step.get("error") {
                Some(error) => fake.write(&json!({"id": id, "error": error})),
                None => fake.write(&json!({"id": id, "result": step.get("result").cloned().unwrap_or_else(|| json!({}))})),
            }
        } else if let Some(message) = step.get("send") {
            fake.write(message);
        } else if let Some(id) = step.get("awaitResponse") {
            if !fake.wait_response(id) {
                return;
            }
        } else if let Some(ms) = step.get("sleepMs").and_then(|m| m.as_u64()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        } else if let Some(code) = step.get("exit").and_then(|c| c.as_i64()) {
            if let Some(text) = step.get("stderr").and_then(|t| t.as_str()) {
                eprintln!("{text}");
            }
            std::process::exit(code as i32);
        } else if let Some(raw) = step.get("raw").and_then(|r| r.as_str()) {
            // Not JSON at all: the adapter must skip it.
            writeln!(fake.stdout, "{raw}").unwrap();
            fake.stdout.flush().unwrap();
        }
    }
    // Serve handlers until the client goes away.
    while let Some(message) = fake.next() {
        fake.answer_from_handlers(&message);
    }
}

struct Fake {
    handlers: Value,
    /// Calls answered so far per method, for `{"!seq": [...]}` handlers.
    served: std::collections::HashMap<String, usize>,
    log: std::fs::File,
    stdin: BufReader<std::io::Stdin>,
    stdout: std::io::Stdout,
}

impl Fake {
    fn write(&mut self, message: &Value) {
        let line = sonic_rs::to_string(message).unwrap();
        writeln!(self.stdout, "{line}").unwrap();
        self.stdout.flush().unwrap();
    }

    /// Reads one message, logs it and answers it from the handlers if it is
    /// a request. Returns it, or `None` at end of input.
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
            writeln!(self.log, "{line}").unwrap();
            let message: Value = sonic_rs::from_str(line).expect("client sent invalid JSON");
            assert!(message.get("jsonrpc").is_none(), "Codex app-server lines carry no jsonrpc field");
            return Some(message);
        }
    }

    fn answer_from_handlers(&mut self, message: &Value) {
        let (Some(id), Some(method)) = (message.get("id"), message.get("method").and_then(|m| m.as_str())) else { return };
        let id = id.clone();
        let served = self.served.entry(method.to_owned()).or_default();
        let n = *served;
        *served += 1;
        let handler = self.handlers.get(method).cloned().map(|h| match h.get("!seq").and_then(|s| s.as_array()) {
            // Successive answers; the last one repeats.
            Some(seq) => seq.get(n.min(seq.len().saturating_sub(1))).cloned().unwrap_or_default(),
            None => h,
        });
        match handler {
            Some(result) => match result.get("!error") {
                Some(error) => self.write(&json!({"id": id, "error": error})),
                None => self.write(&json!({"id": id, "result": result})),
            },
            None => self.write(&json!({"id": id, "error": {"code": -32601, "message": format!("unknown method {method}")}})),
        }
    }

    fn wait_request(&mut self, method: &str) -> Option<Value> {
        loop {
            let message = self.next()?;
            let is_request = message.get("id").is_some() && message.get("method").is_some();
            if is_request && message.get("method").and_then(|m| m.as_str()) == Some(method) {
                return message.get("id").cloned();
            }
            self.answer_from_handlers(&message);
        }
    }

    fn wait_response(&mut self, id: &Value) -> bool {
        loop {
            let Some(message) = self.next() else { return false };
            if message.get("method").is_none() && message.get("id") == Some(id) {
                return true;
            }
            self.answer_from_handlers(&message);
        }
    }
}
