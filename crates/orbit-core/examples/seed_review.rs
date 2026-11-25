//! Writes a data folder and real git repositories for checking the review
//! sheet by eye:
//!
//! `cargo run -p orbit-core --example seed_review -- <data dir> <work dir> [--big]`
//!
//! `<work dir>/app` is a project on `main`. Thread "Stream the parser"
//! works in its own worktree with two turns (each with real checkpoints)
//! plus uncommitted edits made after them; thread "Tidy the lexer" works in
//! another worktree and touches one of the same files, so the two overlap.
//! With `--big` the first thread's second turn changes 800 files and about
//! 50,000 lines, for measuring large reviews. Start Orbit with
//! `ORBIT_DATA=<data dir>`.

use std::path::{Path, PathBuf};
use std::process::Command;

use orbit_core::git::{Git, GitConfig};
use orbit_core::store::Store;
use orbit_proto::*;

fn sh(dir: &Path, args: &[&str]) {
    let ok = Command::new("git").args(args).current_dir(dir).status().expect("git runs").success();
    assert!(ok, "git {args:?}");
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn item(turn: u32, body: ItemBody) -> Item {
    Item { id: ItemId::new(), turn, status: ItemStatus::Done, body }
}

const LEXER: &str = "//! Splits source text into tokens.

pub struct Lexer<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(text: &'a str) -> Self {
        Self { text, pos: 0 }
    }

    pub fn next_token(&mut self) -> Option<&'a str> {
        let rest = &self.text[self.pos..];
        let start = rest.find(|c: char| !c.is_whitespace())?;
        let rest = &rest[start..];
        let len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        self.pos += start + len;
        Some(&rest[..len])
    }
}
";

const PARSER: &str = "//! Builds a syntax tree from tokens.

use crate::lexer::Lexer;

pub struct Parser<'a> {
    lexer: Lexer<'a>,
    buffer: Vec<&'a str>,
}

impl<'a> Parser<'a> {
    pub fn new(text: &'a str) -> Self {
        Self { lexer: Lexer::new(text), buffer: Vec::new() }
    }

    /// Reads every token before parsing.
    pub fn parse(&mut self) -> Vec<&'a str> {
        while let Some(token) = self.lexer.next_token() {
            self.buffer.push(token);
        }
        self.buffer.clone()
    }
}
";

fn main() {
    let mut args = std::env::args().skip(1);
    let data = PathBuf::from(args.next().expect("usage: seed_review <data dir> <work dir> [--big]"));
    let work = PathBuf::from(args.next().expect("usage: seed_review <data dir> <work dir> [--big]"));
    let big = args.any(|a| a == "--big");
    let repo = work.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["init", "-q", "-b", "main"]);
    sh(&repo, &["config", "user.name", "Orbit Demo"]);
    sh(&repo, &["config", "user.email", "demo@example.com"]);
    sh(&repo, &["config", "core.autocrlf", "false"]);
    write(&repo, "src/lexer.rs", LEXER);
    write(&repo, "src/parser.rs", PARSER);
    write(&repo, "src/lib.rs", "pub mod lexer;\npub mod parser;\n");
    write(&repo, "README.md", "# app\n\nA tiny parser.\n");
    write(&repo, "tests/parse.rs", "#[test]\nfn parses() {\n    assert!(true);\n}\n");
    write(&repo, "Cargo.toml", "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    sh(&repo, &["add", "-A"]);
    sh(&repo, &["commit", "-qm", "Initial parser"]);

    let wt_a = work.join("wt-stream");
    let wt_b = work.join("wt-lexer");
    sh(&repo, &["worktree", "add", "-q", "-b", "orbit/stream", wt_a.to_str().unwrap()]);
    sh(&repo, &["worktree", "add", "-q", "-b", "orbit/lexer", wt_b.to_str().unwrap()]);

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let git = Git::new(GitConfig::new(&data));
    let (a, b) = (ThreadId::new(), ThreadId::new());

    // Thread A: two turns, then edits after them.
    let tree = |n: &str| runtime.block_on(git.checkpoint(Path::new(n), a)).unwrap().tree.as_str().to_owned();
    let a_dir = wt_a.to_str().unwrap().to_owned();
    let base = tree(&a_dir);
    write(&wt_a, "src/parser.rs", &PARSER.replace(
        "    /// Reads every token before parsing.\n    pub fn parse(&mut self) -> Vec<&'a str> {\n        while let Some(token) = self.lexer.next_token() {\n            self.buffer.push(token);\n        }\n        self.buffer.clone()\n    }",
        "    /// Yields tokens as the lexer finds them, without buffering.\n    pub fn tokens(&mut self) -> impl Iterator<Item = &'a str> + '_ {\n        std::iter::from_fn(move || self.lexer.next_token())\n    }\n\n    /// Reads every token before parsing.\n    pub fn parse(&mut self) -> Vec<&'a str> {\n        self.tokens().collect()\n    }",
    ).replace("    buffer: Vec<&'a str>,\n", "").replace("Self { lexer: Lexer::new(text), buffer: Vec::new() }", "Self { lexer: Lexer::new(text) }"));
    write(&wt_a, "src/stream.rs", "//! Streaming entry points.\n\nuse crate::parser::Parser;\n\n/// Counts tokens without collecting them.\npub fn count(text: &str) -> usize {\n    Parser::new(text).tokens().count()\n}\n");
    write(&wt_a, "src/lib.rs", "pub mod lexer;\npub mod parser;\npub mod stream;\n");
    let turn1 = tree(&a_dir);
    write(&wt_a, "tests/parse.rs", "use app::stream::count;\n\n#[test]\nfn parses() {\n    assert!(true);\n}\n\n#[test]\nfn counts_tokens_lazily() {\n    assert_eq!(count(\"a b  c\"), 3);\n}\n");
    write(&wt_a, "README.md", "# app\n\nA tiny parser that streams tokens instead of buffering them.\n");
    if big {
        for d in 0..40 {
            for f in 0..20 {
                let body: String = (0..62).map(|l| format!("pub fn f{d}_{f}_{l}(x: u64) -> u64 {{ x.wrapping_mul({l}) + {f} }}\n")).collect();
                write(&wt_a, &format!("gen/m{d:02}/file{f:02}.rs"), &body);
            }
        }
    }
    let turn2 = tree(&a_dir);
    // After the turns: an edit by hand, visible in "Uncommitted" and live.
    write(&wt_a, "src/lexer.rs", &LEXER.replace("//! Splits source text into tokens.", "//! Splits source text into whitespace-separated tokens."));

    // Thread B touches the lexer too.
    write(&wt_b, "src/lexer.rs", &LEXER.replace("    pos: usize,\n", "    pos: usize,\n    line: u32,\n").replace("Self { text, pos: 0 }", "Self { text, pos: 0, line: 1 }"));

    let changes = |from: &str, to: &str| -> Vec<FileChange> {
        let d = runtime
            .block_on(git.diff(&wt_a, &orbit_core::git::TreeId::new(from), Some(&orbit_core::git::TreeId::new(to))))
            .unwrap();
        d.changes()
    };

    let mut store = Store::open(&data).expect("open store");
    let now = now_ms();
    let project = Project {
        id: ProjectId::new(),
        name: "app".into(),
        path: repo.display().to_string(),
        order: 1,
        scripts: ProjectScripts::default(),
        base_branch: Some("main".into()),
    };
    store.commit(Event::ProjectUpserted(Box::new(project.clone()))).unwrap();
    let thread = |id: ThreadId, title: &str, wt: &Path, branch: &str, turns: u32| Thread {
        id,
        project: Some(project.id),
        title: title.into(),
        created_ms: now - 3_600_000,
        updated_ms: now - 60_000,
        turns,
        workspace: Workspace { branch: Some(branch.into()), worktree: Some(wt.display().to_string()), port_base: None },
        ..Default::default()
    };
    store.commit(Event::ThreadUpserted(Box::new(thread(a, "Stream the parser", &wt_a, "orbit/stream", 2)))).unwrap();
    store.commit(Event::ThreadUpserted(Box::new(thread(b, "Tidy the lexer", &wt_b, "orbit/lexer", 0)))).unwrap();

    let end = |base: &str, cp: &str, files: Vec<FileChange>| {
        ItemBody::TurnEnd(TurnSummary {
            duration_ms: 42_000,
            checkpoint: Some(cp.into()),
            base_checkpoint: Some(base.into()),
            files,
            ..Default::default()
        })
    };
    let user = |text: &str| ItemBody::User(UserMessage { text: text.into(), attachments: vec![], steer: false });
    let items = vec![
        item(1, user("Make the parser stream tokens instead of buffering them")),
        item(1, ItemBody::Assistant { text: "The parser now yields tokens lazily through `tokens()`, and `stream::count` uses it.".into() }),
        item(1, end(&base, &turn1, changes(&base, &turn1))),
        item(2, user("Add a test for the streaming path and update the README")),
        item(2, ItemBody::Assistant { text: "Added `counts_tokens_lazily` and described streaming in the README.".into() }),
        item(2, end(&turn1, &turn2, changes(&turn1, &turn2))),
    ];
    for item in items {
        store.commit(Event::ItemUpserted { thread: a, item: Box::new(item) }).unwrap();
    }
    store.sync().unwrap();
    println!("seeded {} and {}", data.display(), work.display());
}
