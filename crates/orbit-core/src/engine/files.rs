//! Files the composer can point at: `@` mention search and thread digests.
//!
//! **Search.** Each workspace root gets an index: every file and folder the
//! project does not ignore (`.gitignore`, `.ignore`, global excludes; `.git`
//! skipped), bounded to [`MAX_ENTRIES`] entries and [`MAX_DEPTH`] levels. The
//! walk and the ranking both run on the blocking pool, never on the engine
//! thread. A query is answered from the current index at once; an index
//! older than [`STALE_AFTER`] is rebuilt in the background for the next
//! query (stale-while-revalidate), so typing never waits on the disk after
//! the first `@`. Ranking is [`crate::fuzzy::score_path`].
//!
//! **Digests.** [`Command::ExportThread`] turns a thread into a short
//! Markdown transcript stored as a blob, so another thread's agent can read
//! it like any attachment, on demand.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use orbit_proto::*;
use parking_lot::Mutex;

use super::orchestrator::{CommandResult, Deferred, Orchestrator};
use crate::fuzzy;
use crate::store::Store;

/// Entries kept per root; a monorepo beyond this is searched partially.
const MAX_ENTRIES: usize = 200_000;
/// Folder depth walked.
const MAX_DEPTH: usize = 40;
/// An index older than this is refreshed after answering.
const STALE_AFTER: Duration = Duration::from_secs(15);
/// Roots kept in memory; the least recently used is dropped.
const MAX_ROOTS: usize = 12;
/// Results one query may ask for.
const MAX_RESULTS: usize = 200;
/// Bytes of a thread digest; older turns are dropped first.
const MAX_DIGEST: usize = 192 * 1024;

/// Mime of a thread digest. The `orbit-context` parameter lets composers
/// show the attachment as a context chip.
pub const THREAD_DIGEST_MIME: &str = "text/markdown; orbit-context=thread";

/// File indexes by workspace root, shared with the tasks that build them.
#[derive(Clone, Default)]
pub struct FileIndex {
    roots: Arc<Mutex<HashMap<PathBuf, Arc<Root>>>>,
}

#[derive(Default)]
struct Root {
    listing: Mutex<Option<Arc<Listing>>>,
    /// Held while a walk runs: one walk per root at a time.
    building: tokio::sync::Mutex<()>,
    last_used: Mutex<Option<Instant>>,
}

struct Listing {
    entries: Vec<Entry>,
    built: Instant,
}

struct Entry {
    /// Relative, `/`-separated; folders end in `/`.
    path: Box<str>,
    is_dir: bool,
}

impl FileIndex {
    fn root(&self, path: &Path) -> Arc<Root> {
        let mut roots = self.roots.lock();
        if !roots.contains_key(path) && roots.len() >= MAX_ROOTS {
            let oldest = roots.iter().min_by_key(|(_, r)| *r.last_used.lock()).map(|(p, _)| p.clone());
            if let Some(oldest) = oldest {
                roots.remove(&oldest);
            }
        }
        let root = roots.entry(path.to_owned()).or_default().clone();
        *root.last_used.lock() = Some(Instant::now());
        root
    }

    /// Ranks the root's files against `query`, building the index first if
    /// there is none.
    pub async fn search(&self, root: PathBuf, query: String, limit: usize) -> Result<Vec<FileMatch>, String> {
        let index = self.root(&root);
        let current = index.listing.lock().clone();
        let listing = match current {
            Some(listing) => {
                if listing.built.elapsed() > STALE_AFTER {
                    refresh_in_background(index.clone(), root.clone());
                }
                listing
            }
            None => {
                let _guard = index.building.lock().await;
                let existing = index.listing.lock().clone();
                match existing {
                    Some(listing) => listing,
                    None => {
                        let listing = walk_blocking(root.clone()).await?;
                        *index.listing.lock() = Some(listing.clone());
                        listing
                    }
                }
            }
        };
        let limit = limit.clamp(1, MAX_RESULTS);
        tokio::task::spawn_blocking(move || rank(&listing, &query, limit))
            .await
            .map_err(|e| format!("File search failed: {e}"))
    }
}

fn refresh_in_background(index: Arc<Root>, root: PathBuf) {
    tokio::spawn(async move {
        // Someone else is already walking this root.
        let Ok(_guard) = index.building.try_lock() else { return };
        if let Ok(listing) = walk_blocking(root).await {
            *index.listing.lock() = Some(listing);
        }
    });
}

async fn walk_blocking(root: PathBuf) -> Result<Arc<Listing>, String> {
    tokio::task::spawn_blocking(move || walk(&root))
        .await
        .map_err(|e| format!("File search failed: {e}"))?
        .map(Arc::new)
}

fn walk(root: &Path) -> Result<Listing, String> {
    if !root.is_dir() {
        return Err(format!("“{}” is not a folder Orbit can read.", root.display()));
    }
    let mut entries = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .max_depth(Some(MAX_DEPTH))
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    for entry in walker.flatten() {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else { continue };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        let mut path = relative.to_string_lossy().replace('\\', "/");
        if is_dir {
            path.push('/');
        }
        entries.push(Entry { path: path.into_boxed_str(), is_dir });
    }
    Ok(Listing { entries, built: Instant::now() })
}

fn rank(listing: &Listing, query: &str, limit: usize) -> Vec<FileMatch> {
    let query = query.trim();
    let to_match = |e: &Entry| FileMatch { path: e.path.to_string(), is_dir: e.is_dir };
    if query.is_empty() {
        // Nothing typed yet: the top of the tree, folders first.
        let mut top: Vec<&Entry> = listing.entries.iter().filter(|e| e.path.trim_end_matches('/').find('/').is_none()).collect();
        top.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.path.to_lowercase().cmp(&b.path.to_lowercase())));
        return top.into_iter().take(limit).map(to_match).collect();
    }
    let ranked = fuzzy::top(
        listing.entries.iter().filter_map(|e| fuzzy::score_path(query, &e.path).map(|s| (s, (s, e)))),
        limit,
    );
    fuzzy::strong(ranked).into_iter().map(to_match).collect()
}

impl Orchestrator {
    /// The folder `@` mentions search: the thread's workspace, else the
    /// project's folder.
    fn mention_root(&self, thread: Option<ThreadId>, project: Option<ProjectId>) -> Result<PathBuf, String> {
        if let Some(id) = thread {
            let thread = self.store.thread(id).ok_or("That thread no longer exists.")?;
            return Ok(self.workspace_dir(thread));
        }
        let project = project.ok_or("Pick a project to mention its files.")?;
        let project = self.store.project(project).ok_or("That project no longer exists.")?;
        Ok(PathBuf::from(&project.path))
    }

    pub(crate) fn search_files(
        &mut self,
        thread: Option<ThreadId>,
        project: Option<ProjectId>,
        query: String,
        limit: u32,
    ) -> Deferred {
        let root = self.mention_root(thread, project);
        let index = self.files.clone();
        Box::pin(async move {
            let root = root?;
            index.search(root, query, limit as usize).await.map(Reply::Files)
        })
    }

    pub(crate) fn export_thread(&mut self, id: ThreadId) -> Deferred {
        let Some(thread) = self.store.thread(id).cloned() else {
            return Box::pin(async { Err("That thread no longer exists.".to_owned()) });
        };
        let project = thread.project.and_then(|p| self.store.project(p)).map(|p| p.name.clone());
        let loaded = self.store.timeline(id).map(|t| t.items().to_vec());
        let dir = self.store.dir().to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || -> CommandResult {
                let items = loaded.unwrap_or_else(|| Store::scan_timeline(&dir, id));
                let text = digest(&thread, project.as_deref(), &items);
                let name = format!("thread-{}.md", slug(&thread.title));
                Store::put_blob_in(&dir, name, THREAD_DIGEST_MIME.to_owned(), text.as_bytes())
                    .map(Reply::Blob)
                    .map_err(|e| format!("Couldn’t store the thread: {e}"))
            })
            .await
            .unwrap_or_else(|e| Err(format!("Couldn’t read the thread: {e}")))
        })
    }
}

/// A file-name-safe slug of a title.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() { "untitled".to_owned() } else { out }
}

/// The conversation as Markdown: messages, replies, plans and a one-line
/// summary of each tool call. Bounded; when too long the oldest turns go.
pub(crate) fn digest(thread: &Thread, project: Option<&str>, items: &[Item]) -> String {
    let title = if thread.title.is_empty() { "Untitled thread" } else { &thread.title };
    let mut header = format!("# {title}\n\n");
    header.push_str(&format!("A conversation from another Orbit thread ({}", thread.config.provider.label()));
    if let Some(project) = project {
        header.push_str(&format!(", project {project}"));
    }
    header.push_str(&format!(", {} turns).\n", thread.turns));
    let mut turns: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut tools = Vec::new();
    let flush_tools = |current: &mut String, tools: &mut Vec<String>| {
        if !tools.is_empty() {
            current.push_str("\nTools used:\n");
            for t in tools.drain(..) {
                current.push_str(&format!("- {t}\n"));
            }
        }
    };
    for item in items {
        match &item.body {
            ItemBody::User(m) => {
                flush_tools(&mut current, &mut tools);
                if !current.is_empty() {
                    turns.push(std::mem::take(&mut current));
                }
                current.push_str(&format!("\n## You{}\n\n{}\n", if m.steer { " (while it worked)" } else { "" }, m.text.trim()));
                for a in &m.attachments {
                    current.push_str(&format!("\n[attached: {}]\n", a.name));
                }
            }
            ItemBody::Assistant { text } if !text.trim().is_empty() => {
                flush_tools(&mut current, &mut tools);
                current.push_str(&format!("\n## Agent\n\n{}\n", text.trim()));
            }
            ItemBody::Tool(call) if tools.len() < 40 => {
                let title = if call.title.is_empty() { &call.name } else { &call.title };
                tools.push(format!("`{}`", title.replace('`', "'")));
            }
            ItemBody::Plan { steps } => {
                flush_tools(&mut current, &mut tools);
                current.push_str("\n## Plan\n\n");
                for step in steps {
                    let mark = if step.state == StepState::Done { "x" } else { " " };
                    current.push_str(&format!("- [{mark}] {}\n", step.text));
                }
            }
            ItemBody::TurnEnd(summary) if !summary.files.is_empty() => {
                flush_tools(&mut current, &mut tools);
                let files: Vec<&str> = summary.files.iter().take(30).map(|f| f.path.as_str()).collect();
                current.push_str(&format!("\nFiles changed: {}\n", files.join(", ")));
            }
            _ => {}
        }
    }
    flush_tools(&mut current, &mut tools);
    if !current.is_empty() {
        turns.push(current);
    }
    // Keep the newest turns that fit.
    let mut kept: Vec<&String> = Vec::new();
    let mut size = header.len();
    for turn in turns.iter().rev() {
        if size + turn.len() > MAX_DIGEST && !kept.is_empty() {
            break;
        }
        size += turn.len();
        kept.push(turn);
    }
    let dropped = turns.len() - kept.len();
    let mut out = header;
    if dropped > 0 {
        out.push_str(&format!("\n_{dropped} earlier messages are left out._\n"));
    }
    for turn in kept.into_iter().rev() {
        if turn.len() > MAX_DIGEST {
            // One enormous turn: keep its end.
            let mut cut = turn.len() - MAX_DIGEST;
            while !turn.is_char_boundary(cut) {
                cut += 1;
            }
            out.push_str("\n…");
            out.push_str(&turn[cut..]);
        } else {
            out.push_str(turn);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            let path = dir.path().join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        dir
    }

    #[test]
    fn the_walk_respects_ignore_files_and_skips_git() {
        let dir = tree(&["src/main.rs", "src/lib.rs", "target/debug/app.exe", ".git/HEAD", ".github/ci.yml", ".gitignore"]);
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        let listing = walk(dir.path()).unwrap();
        let mut paths: Vec<&str> = listing.entries.iter().map(|e| &*e.path).collect();
        paths.sort();
        assert_eq!(paths, vec![".github/", ".github/ci.yml", ".gitignore", "src/", "src/lib.rs", "src/main.rs"]);
        assert!(listing.entries.iter().any(|e| e.is_dir && &*e.path == "src/"));
    }

    #[test]
    fn an_empty_query_lists_the_top_level_folders_first() {
        let dir = tree(&["b.txt", "a/x.rs", "c/y.rs"]);
        let listing = walk(dir.path()).unwrap();
        let top: Vec<String> = rank(&listing, "", 10).into_iter().map(|m| m.path).collect();
        assert_eq!(top, vec!["a/", "c/", "b.txt"]);
    }

    #[test]
    fn digests_keep_the_conversation_and_drop_the_noise() {
        let thread = Thread { title: "Fix the parser".into(), turns: 1, ..Default::default() };
        let items = vec![
            Item { id: ItemId(1), turn: 1, status: ItemStatus::Done, body: ItemBody::User(UserMessage { text: "Fix it".into(), ..Default::default() }) },
            Item { id: ItemId(2), turn: 1, status: ItemStatus::Done, body: ItemBody::Reasoning { text: "secret thoughts".into() } },
            Item {
                id: ItemId(3),
                turn: 1,
                status: ItemStatus::Done,
                body: ItemBody::Tool(ToolCall {
                    call_id: "c".into(),
                    kind: ToolKind::Command,
                    name: "Bash".into(),
                    title: "cargo test".into(),
                    input: String::new(),
                    output: "lots of output".into(),
                    output_blob: None,
                    exit_code: Some(0),
                    files: vec![],
                }),
            },
            Item { id: ItemId(4), turn: 1, status: ItemStatus::Done, body: ItemBody::Assistant { text: "Fixed.".into() } },
        ];
        let text = digest(&thread, Some("orbit"), &items);
        assert!(text.starts_with("# Fix the parser"));
        assert!(text.contains("project orbit"));
        assert!(text.contains("## You\n\nFix it"));
        assert!(text.contains("- `cargo test`"));
        assert!(text.contains("## Agent\n\nFixed."));
        assert!(!text.contains("secret thoughts") && !text.contains("lots of output"));
        assert_eq!(slug("Fix the parser: now!"), "fix-the-parser-now");
        assert_eq!(slug("   "), "untitled");
    }

    #[test]
    fn long_digests_keep_the_newest_turns() {
        let thread = Thread::default();
        let big = "y".repeat(MAX_DIGEST / 2);
        let items: Vec<Item> = (0..6)
            .map(|i| Item {
                id: ItemId(i),
                turn: i as u32,
                status: ItemStatus::Done,
                body: ItemBody::User(UserMessage { text: format!("turn {i} {big}"), ..Default::default() }),
            })
            .collect();
        let text = digest(&thread, None, &items);
        assert!(text.len() <= MAX_DIGEST + 4096);
        assert!(text.contains("turn 5") && !text.contains("turn 0"));
        assert!(text.contains("earlier messages are left out"));
    }
}
