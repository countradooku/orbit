//! Reading a workspace's files, safely.
//!
//! Every path a client names is relative to a root (a project's checkout or
//! a thread's worktree) and must stay inside it: absolute paths, drive
//! prefixes and `..` are refused before touching the disk, and the resolved
//! path is canonicalized and checked against the canonical root, so a
//! symbolic link or junction cannot lead out of it either.
//!
//! Everything here blocks; the engine runs it on the blocking pool.

use std::cmp::Ordering;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use orbit_proto::*;

/// Bytes inspected to tell text from binary.
const SNIFF: usize = 8 * 1024;
/// Instruction files found below the root at most.
const MAX_NESTED_MEMORY: usize = 40;
/// Entries walked looking for nested instruction files.
const MAX_MEMORY_WALK: usize = 50_000;

/// `relative` resolved under `root`, refusing anything that leaves it.
/// The file must exist.
pub(crate) fn resolve(root: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.contains('\0') {
        return Err("That path is not valid.".into());
    }
    let relative = relative.trim();
    let rel = Path::new(relative);
    for component in rel.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("“{relative}” is outside the workspace."));
            }
        }
    }
    let root = canonical(root).map_err(|e| format!("Couldn’t open the workspace {}: {e}", root.display()))?;
    let joined = root.join(rel);
    let resolved = canonical(&joined).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("“{relative}” no longer exists."),
        _ => format!("Couldn’t open “{relative}”: {e}"),
    })?;
    if !resolved.starts_with(&root) {
        return Err(format!("“{relative}” is outside the workspace."));
    }
    Ok(resolved)
}

/// `fs::canonicalize` without Windows' `\\?\` verbatim prefix where it is
/// not needed, so paths compare and display like the ones users type.
pub(crate) fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let path = std::fs::canonicalize(path)?;
    Ok(simplify(path))
}

#[cfg(windows)]
fn simplify(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.len() < 260 => PathBuf::from(rest),
        _ => path,
    }
}

#[cfg(not(windows))]
fn simplify(path: PathBuf) -> PathBuf {
    path
}

/// The image formats the client draws, by extension.
fn image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "tif" | "tiff" => "image/tiff",
        _ => return None,
    })
}

/// Reads a file under `root`: text up to `max_bytes`, an image whole (up to
/// [`MAX_READ_IMAGE`]), anything else as binary.
pub(crate) fn read_file(root: &Path, relative: &str, max_bytes: u32) -> Result<FileContent, String> {
    let path = resolve(root, relative)?;
    let meta = std::fs::metadata(&path).map_err(|e| format!("Couldn’t read “{relative}”: {e}"))?;
    if meta.is_dir() {
        return Err(format!("“{relative}” is a folder."));
    }
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64);
    let size = meta.len();
    let mut content = FileContent {
        path: relative.replace('\\', "/"),
        absolute: path.display().to_string(),
        size,
        modified_ms,
        body: FileBody::Binary,
    };
    let read_err = |e: std::io::Error| format!("Couldn’t read “{relative}”: {e}");
    if let Some(mime) = image_mime(&path) {
        if size <= u64::from(MAX_READ_IMAGE) {
            let data = std::fs::read(&path).map_err(read_err)?;
            content.body = FileBody::Image { mime: mime.to_owned(), data };
        }
        return Ok(content);
    }
    let limit = if max_bytes == 0 { MAX_READ_TEXT } else { max_bytes.min(MAX_READ_TEXT) } as u64;
    let mut bytes = Vec::with_capacity(size.min(limit) as usize);
    std::fs::File::open(&path).map_err(read_err)?.take(limit).read_to_end(&mut bytes).map_err(read_err)?;
    if is_binary(&bytes) {
        return Ok(content);
    }
    let truncated = size > bytes.len() as u64;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        // The cut may split a character; drop its replacement.
        while text.ends_with('\u{FFFD}') {
            text.pop();
        }
    }
    content.body = FileBody::Text { text, truncated };
    Ok(content)
}

/// NUL bytes in the first few KB, or mostly invalid UTF-8, mean binary.
pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(SNIFF)];
    if memchr::memchr(0, head).is_some() {
        return true;
    }
    // UTF-16 with a byte order mark reads as text elsewhere, but not here.
    if head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]) {
        return true;
    }
    match std::str::from_utf8(head) {
        Ok(_) => false,
        // A cut at the end of the sniffed window is not an error.
        Err(e) if e.error_len().is_none() => false,
        Err(_) => {
            let bad = String::from_utf8_lossy(head).chars().filter(|c| *c == '\u{FFFD}').count();
            bad * 10 > head.len().max(1)
        }
    }
}

/// A folder's entries under `root`: folders first, then files, each in
/// natural order (`file2` before `file10`); ignored files and `.git` left out.
pub(crate) fn list_dir(root: &Path, relative: &str) -> Result<Vec<DirEntry>, String> {
    let dir = if relative.trim().is_empty() {
        canonical(root).map_err(|e| format!("Couldn’t open the workspace {}: {e}", root.display()))?
    } else {
        resolve(root, relative)?
    };
    if !dir.is_dir() {
        return Err(format!("“{relative}” is not a folder."));
    }
    let walker = ignore::WalkBuilder::new(&dir)
        .max_depth(Some(1))
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    let mut entries = Vec::new();
    for entry in walker.flatten() {
        if entry.depth() == 0 {
            continue;
        }
        if entries.len() >= MAX_DIR_ENTRIES {
            break;
        }
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir())
            // A link to a folder opens as one.
            || (entry.path_is_symlink() && entry.path().is_dir());
        let size = if is_dir { 0 } else { entry.metadata().map(|m| m.len()).unwrap_or(0) };
        entries.push(DirEntry { name: entry.file_name().to_string_lossy().into_owned(), is_dir, size });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| natural_cmp(&a.name, &b.name)));
    Ok(entries)
}

/// Compares names the way people count: digit runs by value, letters
/// without regard to case (case only breaks ties).
pub(crate) fn natural_cmp(a: &str, b: &str) -> Ordering {
    natural(a, b, true).then_with(|| natural(a, b, false))
}

fn natural(a: &str, b: &str, fold: bool) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut digits = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(c);
                        it.next();
                    }
                    digits
                };
                let (da, db) = (take(&mut a), take(&mut b));
                let (ta, tb) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
                let order = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb)).then_with(|| db.len().cmp(&da.len()));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = if fold { x.to_lowercase().cmp(y.to_lowercase()) } else { x.cmp(&y) };
                if order != Ordering::Equal {
                    return order;
                }
                a.next();
                b.next();
            }
        }
    }
}

/// Instruction files an agent working in `root` reads: the root's own,
/// those of its subfolders, of the folders above it and the user's.
pub(crate) fn memory_files(root: &Path, home: Option<&Path>) -> Result<Vec<MemoryFile>, String> {
    let root = canonical(root).map_err(|e| format!("Couldn’t open the workspace {}: {e}", root.display()))?;
    let mut out = Vec::new();
    let label_of = |path: &Path| -> String {
        if let Ok(rel) = path.strip_prefix(&root) {
            return rel.to_string_lossy().replace('\\', "/");
        }
        if let Some(home) = home
            && let Ok(rel) = path.strip_prefix(home)
        {
            return format!("~/{}", rel.to_string_lossy().replace('\\', "/"));
        }
        path.display().to_string()
    };
    let push = |out: &mut Vec<MemoryFile>, path: PathBuf, scope: MemoryScope| {
        let Some(kind) = memory_kind(&path) else { return };
        let is_dir = path.is_dir();
        if !(is_dir || path.is_file()) || out.iter().any(|m: &MemoryFile| Path::new(&m.absolute) == path) {
            return;
        }
        out.push(MemoryFile { label: label_of(&path), absolute: path.display().to_string(), scope, kind, is_dir });
    };
    const NAMES: [&str; 4] = ["CLAUDE.md", "CLAUDE.local.md", "AGENTS.md", ".claude"];
    for name in NAMES {
        push(&mut out, root.join(name), MemoryScope::Root);
    }
    // Below the root, ignored folders skipped; bounded for monorepos.
    let walker = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .max_depth(Some(12))
        .filter_entry(|e| e.file_name() != ".git" && e.file_name() != "node_modules")
        .build();
    let mut nested = 0;
    for (walked, entry) in walker.flatten().enumerate() {
        if walked >= MAX_MEMORY_WALK || nested >= MAX_NESTED_MEMORY {
            break;
        }
        if entry.depth() < 2 {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if matches!(name.as_ref(), "CLAUDE.md" | "CLAUDE.local.md" | "AGENTS.md")
            // `.claude/` folders' own markdown (commands, agents) are not
            // instruction files of their folder.
            && !entry.path().components().any(|c| c.as_os_str() == ".claude")
        {
            push(&mut out, entry.path().to_owned(), MemoryScope::Nested);
            nested += 1;
        }
    }
    for dir in root.ancestors().skip(1) {
        if home.is_some_and(|h| h == dir) {
            // The home folder's own files are the user's, listed below.
            break;
        }
        for name in ["CLAUDE.md", "AGENTS.md"] {
            push(&mut out, dir.join(name), MemoryScope::Parent);
        }
    }
    if let Some(home) = home {
        push(&mut out, home.join(".claude").join("CLAUDE.md"), MemoryScope::User);
        push(&mut out, home.join(".codex").join("AGENTS.md"), MemoryScope::User);
        push(&mut out, home.join(".config").join("opencode").join("AGENTS.md"), MemoryScope::User);
    }
    Ok(out)
}

fn memory_kind(path: &Path) -> Option<MemoryKind> {
    match path.file_name()?.to_str()? {
        "CLAUDE.md" | "CLAUDE.local.md" => Some(MemoryKind::Claude),
        "AGENTS.md" => Some(MemoryKind::Agents),
        ".claude" => Some(MemoryKind::ClaudeFolder),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    #[test]
    fn paths_that_leave_the_root_are_refused() {
        let outer = tree(&[("secret.txt", "s"), ("ws/src/main.rs", "fn main() {}")]);
        let root = outer.path().join("ws");
        assert!(resolve(&root, "src/main.rs").is_ok());
        assert!(resolve(&root, "./src/main.rs").is_ok());
        for bad in ["../secret.txt", "src/../../secret.txt", "..", "a\0b"] {
            let err = resolve(&root, bad).unwrap_err();
            assert!(err.contains("outside") || err.contains("not valid"), "{bad}: {err}");
        }
        let absolute = outer.path().join("secret.txt");
        assert!(resolve(&root, &absolute.display().to_string()).unwrap_err().contains("outside"));
        #[cfg(windows)]
        {
            assert!(resolve(&root, r"..\secret.txt").unwrap_err().contains("outside"));
            assert!(resolve(&root, r"C:\Windows\win.ini").unwrap_err().contains("outside"));
            assert!(resolve(&root, r"C:secret.txt").unwrap_err().contains("outside"));
            assert!(resolve(&root, r"\\server\share\x").unwrap_err().contains("outside"));
        }
        assert!(resolve(&root, "missing.rs").unwrap_err().contains("no longer exists"));
        assert!(read_file(&root, "../secret.txt", 0).is_err());
        assert!(list_dir(&root, "..").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn links_out_of_the_root_are_refused() {
        let outer = tree(&[("secret.txt", "s"), ("ws/a.txt", "a")]);
        let root = outer.path().join("ws");
        std::os::unix::fs::symlink(outer.path().join("secret.txt"), root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(outer.path(), root.join("up")).unwrap();
        assert!(resolve(&root, "link.txt").unwrap_err().contains("outside"));
        assert!(resolve(&root, "up/secret.txt").unwrap_err().contains("outside"));
    }

    #[cfg(windows)]
    #[test]
    fn junctions_out_of_the_root_are_refused() {
        let outer = tree(&[("secret/s.txt", "s"), ("ws/a.txt", "a")]);
        let root = outer.path().join("ws");
        let target = outer.path().join("secret");
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(root.join("up"))
            .arg(&target)
            .output()
            .unwrap();
        if status.status.success() {
            assert!(resolve(&root, "up/s.txt").unwrap_err().contains("outside"));
        }
    }

    #[test]
    fn text_is_bounded_and_binary_is_detected() {
        let dir = tree(&[("a.txt", "héllo wörld"), ("big.txt", &"x".repeat(10_000))]);
        std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2, 3, 0, 5]).unwrap();
        std::fs::write(dir.path().join("pic.png"), [0x89, b'P', b'N', b'G']).unwrap();
        let FileBody::Text { text, truncated } = read_file(dir.path(), "a.txt", 0).unwrap().body else { panic!() };
        assert_eq!((text.as_str(), truncated), ("héllo wörld", false));
        // A cut inside "é" drops the half character.
        let FileBody::Text { text, truncated } = read_file(dir.path(), "a.txt", 2).unwrap().body else { panic!() };
        assert_eq!((text.as_str(), truncated), ("h", true));
        let big = read_file(dir.path(), "big.txt", 100).unwrap();
        assert_eq!(big.size, 10_000);
        assert!(matches!(big.body, FileBody::Text { ref text, truncated: true } if text.len() == 100));
        assert_eq!(read_file(dir.path(), "bin.dat", 0).unwrap().body, FileBody::Binary);
        assert!(matches!(read_file(dir.path(), "pic.png", 0).unwrap().body, FileBody::Image { ref mime, .. } if mime == "image/png"));
        assert!(read_file(dir.path(), "", 0).unwrap_err().contains("folder"));
        assert!(is_binary(&[0xFF, 0xFE, b'a', 0]));
        assert!(!is_binary("ok ✓".as_bytes()));
    }

    #[test]
    fn folders_list_ignore_aware_in_natural_order() {
        let dir = tree(&[
            ("src/file10.rs", ""),
            ("src/file2.rs", ""),
            ("src/File1.rs", ""),
            ("src/nested/x.rs", ""),
            ("target/out.bin", ""),
            (".gitignore", "target/\n*.log\n"),
            ("debug.log", ""),
            (".git/HEAD", ""),
            (".github/ci.yml", ""),
        ]);
        let top: Vec<(String, bool)> = list_dir(dir.path(), "").unwrap().into_iter().map(|e| (e.name, e.is_dir)).collect();
        assert_eq!(
            top,
            vec![(".github".into(), true), ("src".into(), true), (".gitignore".into(), false)],
        );
        let src: Vec<String> = list_dir(dir.path(), "src").unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(src, vec!["nested", "File1.rs", "file2.rs", "file10.rs"]);
        assert!(list_dir(dir.path(), ".gitignore").unwrap_err().contains("not a folder"));
        assert_eq!(natural_cmp("a9", "a10"), Ordering::Less);
        assert_eq!(natural_cmp("v1.2", "v1.10"), Ordering::Less);
        assert_eq!(natural_cmp("B", "a"), Ordering::Greater);
    }

    #[test]
    fn memory_files_cover_root_nested_parent_and_user() {
        let outer = tree(&[
            ("home/.claude/CLAUDE.md", "user"),
            ("home/code/AGENTS.md", "parent"),
            ("home/code/app/CLAUDE.md", "root"),
            ("home/code/app/.claude/settings.json", "{}"),
            ("home/code/app/.claude/commands/ship.md", "not memory"),
            ("home/code/app/web/AGENTS.md", "nested"),
            ("home/code/app/vendor/CLAUDE.md", "ignored"),
            ("home/code/app/.gitignore", "vendor/\n"),
        ]);
        let home = canonical(&outer.path().join("home")).unwrap();
        let root = home.join("code").join("app");
        let files = memory_files(&root, Some(&home)).unwrap();
        let summary: Vec<(String, MemoryScope, MemoryKind)> = files.iter().map(|m| (m.label.clone(), m.scope, m.kind)).collect();
        assert_eq!(
            summary,
            vec![
                ("CLAUDE.md".into(), MemoryScope::Root, MemoryKind::Claude),
                (".claude".into(), MemoryScope::Root, MemoryKind::ClaudeFolder),
                ("web/AGENTS.md".into(), MemoryScope::Nested, MemoryKind::Agents),
                ("~/code/AGENTS.md".into(), MemoryScope::Parent, MemoryKind::Agents),
                ("~/.claude/CLAUDE.md".into(), MemoryScope::User, MemoryKind::Claude),
            ]
        );
        assert!(files[1].is_dir);
    }
}
