//! A fake `gh` / `glab` for the forge tests. Tests copy this binary into a
//! temp folder (as `gh[.exe]`) next to `fake-forge.json`:
//!
//! ```json
//! {"rules": [
//!   {"args": ["pr", "view", "7"], "stdout_file": "pr_view.json"},
//!   {"args": ["pr", "view"], "stderr": "GraphQL: Could not resolve", "exit": 1}
//! ]}
//! ```
//!
//! The first rule whose `args` all appear in the command line answers it:
//! `stdout` or the contents of `stdout_file` (next to the binary), `stderr`,
//! and `exit` (default 0). Each command line is appended to
//! `fake-forge.calls` as a JSON array, for the test to inspect. No rule
//! means exit 2 with "fake forge: no rule".

use std::io::Write;
use std::path::PathBuf;

use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

fn main() {
    let dir: PathBuf = std::env::current_exe().unwrap().parent().unwrap().to_owned();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut calls = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("fake-forge.calls")).unwrap();
    writeln!(calls, "{}", sonic_rs::to_string(&args).unwrap()).unwrap();
    let script: Value = sonic_rs::from_str(&std::fs::read_to_string(dir.join("fake-forge.json")).unwrap()).unwrap();
    let rules = script.get("rules").and_then(|r| r.as_array()).expect("rules");
    let rule = rules.iter().find(|rule| {
        rule.get("args")
            .and_then(|a| a.as_array())
            .is_some_and(|want| want.iter().all(|w| args.iter().any(|a| Some(a.as_str()) == w.as_str())))
    });
    let Some(rule) = rule else {
        eprintln!("fake forge: no rule for {args:?}");
        std::process::exit(2);
    };
    let text = |key: &str| rule.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_owned();
    let mut stdout = text("stdout");
    if let Some(file) = rule.get("stdout_file").and_then(|v| v.as_str()) {
        stdout = std::fs::read_to_string(dir.join(file)).unwrap();
    }
    print!("{stdout}");
    eprint!("{}", text("stderr"));
    std::process::exit(rule.get("exit").and_then(|v| v.as_i64()).unwrap_or(0) as i32);
}
