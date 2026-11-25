//! Attachments and context: files, images, large pastes, and context chips
//! (another thread, a terminal selection, a review comment).
//!
//! Everything the agent should read travels as an ordinary attachment: a
//! blob on the host, stored once by content hash, which the agent opens on
//! demand. Context chips are attachments whose mime carries an
//! `orbit-context=` parameter, so they survive drafts and restarts and the
//! composer can show them for what they are. Files and folders of the
//! workspace are not copied at all: they become `@` mentions the agent reads
//! from the workspace itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::component::input::InlineToken;
use gpui_kit::*;
use orbit_proto::*;

use super::{Composer, mention_token, trigger};

/// Pastes this large become a text attachment ("Insert as text" puts them
/// back); Ctrl+Shift+V always pastes inline.
pub const LARGE_PASTE: usize = 32 * 1024;

/// Something to put in front of the agent, from anywhere in the app.
#[derive(Clone, Debug)]
pub enum ContextChip {
    /// A file or folder, relative to the workspace or absolute: inserted as
    /// an `@` mention at the caret.
    Path(String),
    /// Another thread: its conversation, as a digest the agent can read.
    Thread { id: ThreadId, title: String },
    /// Output selected in a terminal. (For the terminal's "Add to chat".)
    #[allow(dead_code)]
    TerminalSelection { text: String, cwd: Option<String> },
    /// A comment on a line of a diff. (For review's "Send to agent".)
    #[allow(dead_code)]
    ReviewComment { path: String, line: Option<u32>, quote: String, comment: String },
}

/// What a context attachment is, from its mime's `orbit-context` parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextKind {
    Thread,
    Terminal,
    Comment,
    Paste,
}

impl ContextKind {
    pub fn label(self) -> &'static str {
        match self {
            ContextKind::Thread => "Thread",
            ContextKind::Terminal => "Terminal selection",
            ContextKind::Comment => "Review comment",
            ContextKind::Paste => "Pasted text",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            ContextKind::Thread => "The other thread’s conversation, for the agent to read",
            ContextKind::Terminal => "The selected terminal output, for the agent to read",
            ContextKind::Comment => "Your comment on the diff, with the lines it is about",
            ContextKind::Paste => "A long paste, sent as a file",
        }
    }

    fn mime(self) -> &'static str {
        match self {
            ContextKind::Thread => orbit_core::engine::THREAD_DIGEST_MIME,
            ContextKind::Terminal => "text/markdown; orbit-context=terminal",
            ContextKind::Comment => "text/markdown; orbit-context=comment",
            ContextKind::Paste => "text/plain; orbit-context=paste",
        }
    }
}

pub fn context_kind(mime: &str) -> Option<ContextKind> {
    let value = mime.split(';').skip(1).find_map(|p| p.trim().strip_prefix("orbit-context="))?;
    Some(match value.trim() {
        "thread" => ContextKind::Thread,
        "terminal" => ContextKind::Terminal,
        "comment" => ContextKind::Comment,
        "paste" => ContextKind::Paste,
        _ => return None,
    })
}

pub fn size_label(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

pub fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "md" | "txt" | "rs" | "ts" | "tsx" | "js" | "json" | "toml" | "yaml" | "yml" | "py" | "log" | "csv" => "text/plain",
        _ => "application/octet-stream",
    }
}

fn image_format(mime: &str) -> Option<ImageFormat> {
    Some(match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        _ => return None,
    })
}

/// A terminal selection as the agent reads it.
fn terminal_digest(text: &str, cwd: Option<&str>) -> String {
    let fence = if text.contains("```") { "````" } else { "```" };
    let place = cwd.map(|c| format!(" in `{c}`")).unwrap_or_default();
    format!("Terminal output selected{place}:\n\n{fence}\n{}\n{fence}\n", text.trim_end())
}

/// A review comment as the agent reads it.
fn comment_digest(path: &str, line: Option<u32>, quote: &str, comment: &str) -> String {
    let at = line.map(|l| format!(", line {l}")).unwrap_or_default();
    let mut out = format!("Review comment on `{path}`{at}:\n\n");
    if !quote.trim().is_empty() {
        out.push_str("```\n");
        out.push_str(quote.trim_end());
        out.push_str("\n```\n\n");
    }
    out.push_str(comment.trim());
    out.push('\n');
    out
}

fn file_stem(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.chars().map(|c| if c.is_alphanumeric() || c == '.' || c == '-' { c } else { '-' }).collect()
}

impl Composer {
    /// Puts context in front of the agent: a mention at the caret, or a
    /// chip above the text. The API other features use ("Add to chat").
    pub fn add_context(&mut self, chip: ContextChip, window: &mut Window, cx: &mut Context<Self>) {
        match chip {
            ContextChip::Path(path) => self.insert_mention(&path, window, cx),
            ContextChip::Thread { id, title } => {
                self.attaching += 1;
                let task = self.app.update(cx, |app, cx| app.call(Command::ExportThread { thread: id }, cx));
                cx.spawn(async move |this, cx| {
                    let result = task.await;
                    let _ = this.update(cx, |this, cx| {
                        this.attaching -= 1;
                        match result {
                            Ok(Reply::Blob(attachment)) => this.push_attachment(attachment, cx),
                            Ok(_) => {}
                            Err(err) => this.attach_error = Some(format!("Couldn’t add “{title}”: {err}").into()),
                        }
                        cx.notify();
                    });
                })
                .detach();
                cx.notify();
            }
            ContextChip::TerminalSelection { text, cwd } => {
                let body = terminal_digest(&text, cwd.as_deref());
                self.store_bytes("terminal-selection.md".into(), ContextKind::Terminal.mime().into(), body.into_bytes(), None, cx);
            }
            ContextChip::ReviewComment { path, line, quote, comment } => {
                let body = comment_digest(&path, line, &quote, &comment);
                let name = match line {
                    Some(l) => format!("comment-{}-L{l}.md", file_stem(&path)),
                    None => format!("comment-{}.md", file_stem(&path)),
                };
                self.store_bytes(name, ContextKind::Comment.mime().into(), body.into_bytes(), None, cx);
            }
        }
    }

    /// Inserts `@path` at the caret as a token.
    fn insert_mention(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = trigger::mention_text(path);
        let token: InlineToken = mention_token(path, &text);
        let caret = self.input.read(cx).cursor();
        // Keep a space between the mention and a word before it.
        let value = self.input.read(cx).value();
        if caret > 0 && !value[..caret].ends_with(char::is_whitespace) {
            self.input.update(cx, |input, cx| input.insert(" ", window, cx));
        }
        let caret = self.input.read(cx).cursor();
        self.insert_token(caret..caret, token, window, cx);
        self.on_text_changed(cx);
    }

    /// The folder the agent works in, when this client can see it.
    fn workspace_root(&self, cx: &App) -> Option<PathBuf> {
        let state = self.app.read(cx).state();
        let (worktree, project) = match self.thread_id().and_then(|id| state.threads.get(&id)) {
            Some(thread) => (thread.workspace.worktree.clone(), thread.project),
            None => (None, self.project),
        };
        worktree
            .or_else(|| project.and_then(|p| state.projects.iter().find(|x| x.id == p)).map(|p| p.path.clone()))
            .map(PathBuf::from)
    }

    pub(super) fn attach_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let _ = this.update(cx, |this, cx| this.attach_paths(paths, cx));
        })
        .detach();
    }

    /// Dropped files and folders: what is in the workspace becomes a
    /// mention (the agent reads the live file), images and outside files are
    /// attached, outside folders are mentioned by absolute path.
    pub(super) fn drop_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.workspace_root(cx);
        let mut attach = Vec::new();
        for path in paths {
            let is_dir = path.is_dir();
            let is_image = image_format(mime_for(&path.to_string_lossy())).is_some();
            let relative = root.as_deref().and_then(|root| relative_to(&path, root));
            match (relative, is_dir, is_image) {
                (_, false, true) => attach.push(path),
                (Some(rel), dir, _) => {
                    self.add_context(ContextChip::Path(if dir { format!("{rel}/") } else { rel }), window, cx)
                }
                (None, true, _) => self.add_context(ContextChip::Path(format!("{}/", path.display())), window, cx),
                (None, false, false) => attach.push(path),
            }
        }
        if !attach.is_empty() {
            self.attach_paths(attach, cx);
        }
        Composer::focus_input_now(self, window, cx);
    }

    /// Attaches files from disk (picker, drop, pasted files). Folders are
    /// skipped: agents read the project directly.
    pub fn attach_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        for path in paths {
            if !path.is_file() {
                continue;
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mime = mime_for(&name).to_owned();
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if let Err(err) = validate_attachment(&name, &mime, size) {
                self.attach_error = Some(err.into());
                cx.notify();
                continue;
            }
            self.attaching += 1;
            cx.notify();
            let read = cx.background_spawn(async move { std::fs::read(&path) });
            cx.spawn(async move |this, cx| {
                let data = read.await;
                let _ = this.update(cx, |this, cx| {
                    this.attaching -= 1;
                    match data {
                        Ok(data) => {
                            let thumbnail =
                                image_format(&mime).map(|format| Arc::new(Image::from_bytes(format, data.clone())));
                            this.store_bytes(name, mime, data, thumbnail, cx);
                        }
                        Err(err) => this.attach_error = Some(format!("Couldn’t read “{name}”: {err}").into()),
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// Stores bytes in the blob store (once per content) and attaches them.
    fn store_bytes(&mut self, name: String, mime: String, data: Vec<u8>, thumbnail: Option<Arc<Image>>, cx: &mut Context<Self>) {
        if self.attachments.len() + self.attaching >= MAX_ATTACHMENTS {
            self.attach_error = Some(format!("A message can carry up to {MAX_ATTACHMENTS} attachments.").into());
            cx.notify();
            return;
        }
        if let Err(err) = validate_attachment(&name, &mime, data.len() as u64) {
            self.attach_error = Some(err.into());
            cx.notify();
            return;
        }
        self.attach_error = None;
        self.attaching += 1;
        let pasted = (context_kind(&mime) == Some(ContextKind::Paste)).then(|| String::from_utf8_lossy(&data).into_owned());
        let task = self.app.update(cx, |app, cx| app.call(Command::PutBlob { name, mime, data }, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.attaching -= 1;
                match result {
                    Ok(Reply::Blob(attachment)) => {
                        if let Some(thumbnail) = thumbnail {
                            this.thumbnails.insert(attachment.hash.clone(), thumbnail);
                        }
                        if let Some(text) = pasted {
                            this.pasted.insert(attachment.hash.clone(), text.into());
                        }
                        this.push_attachment(attachment, cx);
                    }
                    Ok(_) => {}
                    Err(err) => this.attach_error = Some(err.into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn push_attachment(&mut self, attachment: Attachment, cx: &mut Context<Self>) {
        if !self.attachments.iter().any(|a| a.hash == attachment.hash) {
            self.attachments.push(attachment);
            self.schedule_draft_save(cx);
        }
    }

    /// Images and files on the clipboard become attachments; a very long
    /// text paste becomes a text attachment; other text pastes normally.
    /// Returns whether the paste was handled here.
    pub(super) fn paste(&mut self, item: &ClipboardItem, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut handled = false;
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    let (ext, mime) = match image.format {
                        ImageFormat::Png => ("png", "image/png"),
                        ImageFormat::Jpeg => ("jpg", "image/jpeg"),
                        ImageFormat::Webp => ("webp", "image/webp"),
                        ImageFormat::Gif => ("gif", "image/gif"),
                        _ => ("png", "image/png"),
                    };
                    let n = self.attachments.len() + self.attaching + 1;
                    let thumbnail = Arc::new(Image::from_bytes(image.format, image.bytes.clone()));
                    self.store_bytes(format!("pasted-image-{n}.{ext}"), mime.into(), image.bytes.clone(), Some(thumbnail), cx);
                    handled = true;
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    self.attach_paths(paths.paths().to_vec(), cx);
                    handled = true;
                }
                ClipboardEntry::String(text) if text.text().len() >= LARGE_PASTE => {
                    let text = text.text();
                    let n = self.attachments.iter().filter(|a| context_kind(&a.mime) == Some(ContextKind::Paste)).count() + 1;
                    let lines = text.lines().count();
                    let name = format!("pasted-text-{n} ({lines} lines).txt");
                    self.store_bytes(name, ContextKind::Paste.mime().into(), text.as_bytes().to_vec(), None, cx);
                    handled = true;
                }
                ClipboardEntry::String(_) => {}
            }
        }
        handled
    }

    /// Ctrl+Shift+V: the clipboard's text inline, however long.
    pub(super) fn paste_as_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else { return };
        self.input.update(cx, |input, cx| input.insert(text, window, cx));
        self.on_text_changed(cx);
    }

    /// Puts a long paste that became an attachment back into the text.
    pub(super) fn inline_pasted(&mut self, hash: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.pasted.remove(hash) else { return };
        self.attachments.retain(|a| a.hash != hash);
        self.input.update(cx, |input, cx| {
            input.insert(text, window, cx);
            input.focus(window, cx);
        });
        self.on_text_changed(cx);
    }

    fn focus_input_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }
}

/// `path` relative to `root`, `/`-separated, if it is inside it.
fn relative_to(path: &Path, root: &Path) -> Option<String> {
    let path = dunce_like(path);
    let root = dunce_like(root);
    let rel = path.strip_prefix(&root).ok()?;
    let rel = rel.to_string_lossy().replace('\\', "/");
    (!rel.is_empty()).then_some(rel)
}

/// Case-folded on Windows, where `C:\Repo` and `c:\repo` are one folder.
fn dunce_like(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    if cfg!(windows) { PathBuf::from(s.to_lowercase()) } else { PathBuf::from(s) }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{ContextKind, comment_digest, context_kind, file_stem, mime_for, relative_to, size_label, terminal_digest};

    #[test]
    fn context_kinds_ride_in_the_mime() {
        assert_eq!(context_kind(orbit_core::engine::THREAD_DIGEST_MIME), Some(ContextKind::Thread));
        assert_eq!(context_kind(ContextKind::Paste.mime()), Some(ContextKind::Paste));
        assert_eq!(context_kind("text/plain"), None);
        assert_eq!(context_kind("text/plain; charset=utf-8"), None);
    }

    #[test]
    fn digests_read_well() {
        let t = terminal_digest("error: oops\n", Some("/repo"));
        assert_eq!(t, "Terminal output selected in `/repo`:\n\n```\nerror: oops\n```\n");
        assert!(terminal_digest("has ``` inside", None).contains("````"));
        let c = comment_digest("src/a.rs", Some(12), "let x = 1;", "Use a constant.");
        assert_eq!(c, "Review comment on `src/a.rs`, line 12:\n\n```\nlet x = 1;\n```\n\nUse a constant.\n");
        assert_eq!(file_stem("src/my file.rs"), "my-file.rs");
    }

    #[test]
    fn paths_inside_the_workspace_become_relative() {
        let root = Path::new("/home/me/repo");
        assert_eq!(relative_to(Path::new("/home/me/repo/src/main.rs"), root).as_deref(), Some("src/main.rs"));
        assert_eq!(relative_to(Path::new("/home/me/other/x.rs"), root), None);
        assert_eq!(relative_to(root, root), None);
        assert_eq!(size_label(512), "512 B");
        assert_eq!(size_label(48 * 1024), "48 KB");
        assert_eq!(mime_for("shot.PNG"), "image/png");
    }
}
