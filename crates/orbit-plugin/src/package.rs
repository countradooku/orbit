//! Reading a plugin folder, and getting one from an archive or git.
//!
//! [`load`] reads and checks everything a plugin's manifest names (themes,
//! panels, snippet and prompt files, templates, the WebAssembly module)
//! and turns it into what the host and clients use. Archives
//! (`.orbitplugin`, a zip) and git checkouts are unpacked into a staging
//! folder first; nothing outside it is ever written, and nothing in it is
//! run.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use orbit_proto::*;

use crate::manifest::{MANIFEST, Manifest, Validated, limits};
use crate::panel::Node;
use crate::{hash, theme};

/// Archive file extension.
pub const EXTENSION: &str = "orbitplugin";

/// A plugin read from its folder.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub validated: Validated,
    pub root: PathBuf,
    pub hash: String,
    /// Contributions for clients (storage left empty).
    pub ui: PluginUi,
    /// Panels, parsed.
    pub panels: Vec<(String, Node)>,
    pub wasm: Option<Vec<u8>>,
    /// Non-fatal problems (dropped theme tokens).
    pub warnings: Vec<String>,
}

impl Loaded {
    pub fn id(&self) -> &str {
        self.validated.id()
    }

    pub fn manifest(&self) -> &Manifest {
        &self.validated.manifest
    }

    pub fn panel(&self, id: &str) -> Option<&Node> {
        self.panels.iter().find(|(p, _)| p == id).map(|(_, n)| n)
    }

    /// "2 themes · 1 agent · 3 commands".
    pub fn summary(&self) -> String {
        summary(&self.validated.manifest)
    }
}

/// What a manifest contributes, in a few words.
pub fn summary(m: &Manifest) -> String {
    let mut parts = Vec::new();
    let mut add = |n: usize, one: &str, many: &str| {
        if n > 0 {
            parts.push(format!("{n} {}", if n == 1 { one } else { many }));
        }
    };
    add(m.themes.len(), "theme", "themes");
    add(m.agents.len(), "agent", "agents");
    add(m.panels.len(), "panel", "panels");
    add(m.commands.len(), "command", "commands");
    add(m.slash_commands.len(), "slash command", "slash commands");
    add(m.snippets.len(), "snippet", "snippets");
    add(m.recipes.len(), "recipe", "recipes");
    add(m.keybindings.len(), "keybinding", "keybindings");
    add(m.templates.len(), "template", "templates");
    add(m.wasm.as_ref().map_or(0, |w| w.hooks.len()), "hook", "hooks");
    if parts.is_empty() { "Nothing yet".into() } else { parts.join(" · ") }
}

fn read_text(root: &Path, rel: &str) -> Result<String, String> {
    let path = root.join(rel);
    let meta = std::fs::symlink_metadata(&path).map_err(|_| format!("{rel} is missing."))?;
    if !meta.is_file() {
        return Err(format!("{rel} isn’t a file."));
    }
    if meta.len() as usize > limits::TEXT_FILE_BYTES {
        return Err(format!("{rel} is larger than {} KiB.", limits::TEXT_FILE_BYTES / 1024));
    }
    std::fs::read_to_string(&path).map_err(|e| format!("{rel}: {e}"))
}

/// Reads and checks the plugin in `root`. Every problem is reported.
pub fn load(root: &Path) -> Result<Loaded, Vec<String>> {
    let text = read_text(root, MANIFEST).map_err(|e| vec![e])?;
    let validated = Manifest::parse(&text)?;
    let hash = hash::content_hash(root).map_err(|e| vec![e])?;
    let m = &validated.manifest;
    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    let mut ui = PluginUi::default();

    for t in &m.themes {
        match read_text(root, &t.path).and_then(|text| theme::check(&text).map_err(|e| format!("{}: {e}", t.path))) {
            Ok(checked) => {
                warnings.extend(checked.warnings);
                ui.themes.push(checked.json);
            }
            Err(e) => problems.push(e),
        }
    }
    for s in &m.snippets {
        let text = match (&s.text, &s.path) {
            (Some(text), _) => Ok(text.clone()),
            (None, Some(path)) => read_text(root, path),
            (None, None) => Ok(String::new()),
        };
        match text {
            Ok(text) => ui.snippets.push(PluginSnippet { name: s.name.clone(), description: s.description.clone(), text }),
            Err(e) => problems.push(e),
        }
    }
    for s in &m.slash_commands {
        let prompt = match (&s.prompt, &s.path) {
            (Some(p), _) => Ok(p.clone()),
            (None, Some(path)) => read_text(root, path),
            (None, None) => Ok(String::new()),
        };
        match prompt {
            Ok(prompt) => ui.slash_commands.push(PluginSlashCommand {
                name: s.name.clone(),
                description: s.description.clone(),
                argument_hint: s.argument_hint.clone(),
                prompt,
            }),
            Err(e) => problems.push(e),
        }
    }
    for t in &m.templates {
        match read_text(root, &t.path) {
            Ok(text) => match toml::from_str::<toml::Table>(&text) {
                Ok(_) => ui.templates.push(PluginTemplate { name: t.name.clone(), description: t.description.clone(), orbit_toml: text }),
                Err(e) => problems.push(format!("{}: not valid TOML ({}).", t.path, e.message().trim_end_matches('.'))),
            },
            Err(e) => problems.push(e),
        }
    }
    ui.recipes = m
        .recipes
        .iter()
        .map(|r| PluginRecipe {
            id: r.id.clone(),
            title: r.title.clone(),
            blurb: r.blurb.clone(),
            prompt: r.prompt.clone(),
            rule: r.schedule.clone(),
            worktree: r.target != "project",
            plan_mode: r.mode == "plan",
            open_pr: r.open_pr,
        })
        .collect();
    ui.keybindings = m
        .keybindings
        .iter()
        .map(|k| PluginKeybinding { key: k.key.clone(), command: k.command.clone(), when: k.when.clone() })
        .collect();
    ui.commands = m
        .commands
        .iter()
        .map(|c| PluginCommandInfo {
            id: c.id.clone(),
            title: c.title.clone(),
            description: c.description.clone(),
            kind: match (&c.prompt, &c.panel, &c.agent) {
                (Some(p), _, _) => PluginCommandKind::Prompt(p.clone()),
                (_, Some(p), _) => PluginCommandKind::Panel(p.clone()),
                (_, _, Some(a)) => PluginCommandKind::Terminal(a.clone()),
                _ => PluginCommandKind::Wasm,
            },
        })
        .collect();
    ui.agents = m.agents.iter().map(|a| a.summary()).collect();

    let mut panels = Vec::new();
    for p in &m.panels {
        match read_text(root, &p.path).and_then(|text| Node::parse(&text).map_err(|e| format!("{}: {e}", p.path))) {
            Ok(node) => {
                let issues = node.problems(&validated.capabilities, m.wasm.is_some());
                if issues.is_empty() {
                    ui.panels.push(PluginPanel { id: p.id.clone(), title: p.title.clone(), tree: node.to_json() });
                    panels.push((p.id.clone(), node));
                } else {
                    problems.extend(issues.into_iter().map(|i| format!("{}: {i}", p.path)));
                }
            }
            Err(e) => problems.push(e),
        }
    }

    let mut wasm = None;
    if let Some(w) = &m.wasm {
        let path = root.join(&w.module);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() && meta.len() as usize <= limits::WASM_BYTES => match std::fs::read(&path) {
                Ok(bytes) if bytes.starts_with(b"\0asm") => wasm = Some(bytes),
                Ok(_) => problems.push(format!("{} isn’t a WebAssembly module.", w.module)),
                Err(e) => problems.push(format!("{}: {e}", w.module)),
            },
            Ok(meta) if meta.is_file() => problems.push(format!("{} is larger than {} MiB.", w.module, limits::WASM_BYTES / 1024 / 1024)),
            _ => problems.push(format!("{} is missing.", w.module)),
        }
    }

    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(Loaded { validated, root: root.to_owned(), hash, ui, panels, wasm, warnings })
}

/// Unpacks a `.orbitplugin` archive into `into` (which must be empty).
/// The manifest may be at the root or inside one top folder; returns the
/// plugin's root.
pub fn unpack(bytes: &[u8], into: &Path) -> Result<PathBuf, String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|_| "That isn’t a .orbitplugin archive (a zip).".to_owned())?;
    if zip.len() > limits::PLUGIN_FILES * 2 {
        return Err(format!("The archive has more than {} entries.", limits::PLUGIN_FILES * 2));
    }
    let mut total = 0u64;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| format!("The archive is damaged: {e}"))?;
        let name = entry.name().map(|n| n.into_owned()).unwrap_or_default();
        if entry.is_symlink() {
            return Err(format!("{name} is a symbolic link; plugins can’t contain links."));
        }
        let rel = entry.enclosed_name().ok_or_else(|| format!("“{name}” would land outside the plugin."))?;
        let out = into.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut data = Vec::new();
        let left = limits::PLUGIN_BYTES - total;
        (&mut entry).take(left + 1).read_to_end(&mut data).map_err(|e| format!("The archive is damaged: {e}"))?;
        total += data.len() as u64;
        if total > limits::PLUGIN_BYTES {
            return Err(format!("The plugin is larger than {} MiB unpacked.", limits::PLUGIN_BYTES / 1024 / 1024));
        }
        std::fs::write(&out, data).map_err(|e| e.to_string())?;
    }
    plugin_root(into)
}

/// Where the manifest is: `dir` or its only subfolder.
pub fn plugin_root(dir: &Path) -> Result<PathBuf, String> {
    if dir.join(MANIFEST).is_file() {
        return Ok(dir.to_owned());
    }
    let subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n != "__MACOSX"))
        .collect();
    match subdirs.as_slice() {
        [one] if one.join(MANIFEST).is_file() => Ok(one.clone()),
        _ => Err(format!("No {MANIFEST} at the top of the plugin.")),
    }
}

/// Packs a plugin folder into a `.orbitplugin` archive (to send it to a
/// host, or to publish it). `.git` and `target` are left out.
pub fn pack(root: &Path) -> Result<Vec<u8>, String> {
    if !root.join(MANIFEST).is_file() {
        return Err(format!("{} has no {MANIFEST}.", root.display()));
    }
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut out);
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (rel, path) in hash::files(root)? {
            if rel.starts_with("target/") {
                continue;
            }
            zip.start_file(rel.as_str(), options).map_err(|e| e.to_string())?;
            let bytes = std::fs::read(&path).map_err(|e| format!("{rel}: {e}"))?;
            std::io::Write::write_all(&mut zip, &bytes).map_err(|e| e.to_string())?;
        }
        zip.finish().map_err(|e| e.to_string())?;
    }
    Ok(out.into_inner())
}

/// Whether `commit` is a full SHA-1 or SHA-256 object id.
pub fn is_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64) && commit.chars().all(|c| c.is_ascii_hexdigit())
}

/// Whether `url` is a git location Orbit fetches from: https, ssh, or a
/// local path. Transports that run programs (`ext::`) and anything that
/// could be read as an option are refused.
pub fn is_git_url(url: &str) -> bool {
    let url = url.trim();
    if url.is_empty() || url.starts_with('-') || url.contains(['\n', '\r', '\0']) || url.contains("::") {
        return false;
    }
    url.starts_with("https://")
        || url.starts_with("ssh://")
        || url.starts_with("file://")
        || (url.contains('@') && url.contains(':') && !url.contains("://"))
        || Path::new(url).is_absolute()
}

/// Checks out `url` at `commit` into `into` (a new folder) and removes its
/// `.git`. Runs `git` with prompts off and no hooks; nothing in the
/// repository is executed.
pub fn fetch_git(url: &str, commit: &str, into: &Path) -> Result<PathBuf, String> {
    if !is_git_url(url) {
        return Err(format!("“{url}” isn’t a git address Orbit fetches from (https, ssh or a local path)."));
    }
    if !is_commit(commit) {
        return Err("Pin a full commit id (40 hex digits): a branch or tag can change under you.".into());
    }
    // Hooks from the user's global configuration must not run either: point
    // them at a folder that does not exist.
    let no_hooks = format!("core.hooksPath={}", into.with_extension("no-hooks").display());
    let git = |args: &[&str], cwd: Option<&Path>| -> Result<String, String> {
        let mut cmd = Command::new("git");
        cmd.args(["-c", "protocol.ext.allow=never", "-c", &no_hooks, "-c", "advice.detachedHead=false"])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .stdin(std::process::Stdio::null());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let out = cmd.output().map_err(|e| format!("Couldn’t run git: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            Err(format!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim()))
        }
    };
    std::fs::create_dir_all(into).map_err(|e| e.to_string())?;
    let dir = into.to_str().ok_or("The staging folder isn’t UTF-8.")?;
    git(&["clone", "--quiet", "--no-checkout", "--", url.trim(), dir], None)?;
    // Check out the committed bytes exactly: no line-ending conversion, no
    // filters, whatever the user's or the repository's attributes say, so
    // the content hash is the same on every OS.
    std::fs::create_dir_all(into.join(".git/info")).map_err(|e| e.to_string())?;
    std::fs::write(into.join(".git/info/attributes"), "* -text -filter -ident -working-tree-encoding
").map_err(|e| e.to_string())?;
    git(&["-c", "core.autocrlf=false", "checkout", "--quiet", "--detach", commit], Some(into))?;
    let head = git(&["rev-parse", "HEAD"], Some(into))?;
    if !head.eq_ignore_ascii_case(commit) {
        return Err(format!("The checkout is at {head}, not {commit}."));
    }
    std::fs::remove_dir_all(into.join(".git")).map_err(|e| format!("Couldn’t clean up the checkout: {e}"))?;
    plugin_root(into)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Writes a small plugin with a theme, a snippet and a panel.
    pub fn sample(dir: &Path) {
        std::fs::create_dir_all(dir.join("themes")).unwrap();
        std::fs::create_dir_all(dir.join("panels")).unwrap();
        std::fs::write(
            dir.join(MANIFEST),
            r#"
id = "sample"
name = "Sample"
version = "1.0.0"
engines = { orbit = ">=0.1.0" }
capabilities = ["storage"]
[[themes]]
path = "themes/night.json"
[[snippets]]
name = "Checklist"
path = "checklist.md"
[[panels]]
id = "notes"
title = "Notes"
path = "panels/notes.toml"
[[commands]]
id = "open-notes"
title = "Open notes"
panel = "notes"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("themes/night.json"),
            r##"{"name":"Night","themes":[{"name":"Night","mode":"dark","colors":{"background":"#000000","foreground":"#eeeeee"}}]}"##,
        )
        .unwrap();
        std::fs::write(dir.join("checklist.md"), "- tests\n- docs\n").unwrap();
        std::fs::write(
            dir.join("panels/notes.toml"),
            "type = \"stack\"\nchildren = [{ type = \"input\", bind = \"n\", submit = { call = \"storage.append\", args = { key = \"notes\", value = \"{{input.n}}\" } } }]\n",
        )
        .unwrap();
    }

    #[test]
    fn a_folder_loads_with_everything_it_names() {
        let dir = tempfile::tempdir().unwrap();
        sample(dir.path());
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.id(), "sample");
        assert_eq!(loaded.ui.themes.len(), 1);
        assert_eq!(loaded.ui.snippets[0].text, "- tests\n- docs\n");
        assert_eq!(loaded.ui.panels[0].id, "notes");
        assert_eq!(loaded.ui.commands[0].kind, PluginCommandKind::Panel("notes".into()));
        assert_eq!(loaded.summary(), "1 theme · 1 panel · 1 command · 1 snippet");
        assert!(loaded.hash.starts_with("blake3:"));
    }

    #[test]
    fn missing_and_bad_files_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        sample(dir.path());
        std::fs::remove_file(dir.path().join("checklist.md")).unwrap();
        std::fs::write(dir.path().join("themes/night.json"), "{").unwrap();
        let problems = load(dir.path()).unwrap_err().join("\n");
        assert!(problems.contains("checklist.md is missing"), "{problems}");
        assert!(problems.contains("themes/night.json"), "{problems}");
    }

    #[test]
    fn archives_round_trip_and_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        sample(dir.path());
        let bytes = pack(dir.path()).unwrap();
        let out = tempfile::tempdir().unwrap();
        let root = unpack(&bytes, out.path()).unwrap();
        assert_eq!(load(&root).unwrap().hash, load(dir.path()).unwrap().hash, "same content, same hash");

        let mut evil = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut evil);
            zip.start_file("../evil.txt", zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut zip, b"x").unwrap();
            zip.finish().unwrap();
        }
        let out = tempfile::tempdir().unwrap();
        assert!(unpack(evil.get_ref(), &out.path().join("p")).unwrap_err().contains("outside"));
        assert!(!out.path().join("evil.txt").exists());
        assert!(unpack(b"not a zip", out.path()).is_err());
    }

    #[test]
    fn git_sources_are_pinned_and_safe() {
        assert!(is_commit(&"a".repeat(40)));
        assert!(!is_commit("main"));
        for good in ["https://github.com/a/b", "git@github.com:a/b.git", "ssh://git@host/a"] {
            assert!(is_git_url(good), "{good}");
        }
        for bad in ["ext::sh -c touch% /tmp/pwned", "--upload-pack=touch /tmp/x", "http://insecure/a", "relative/path"] {
            assert!(!is_git_url(bad), "{bad}");
        }
    }

    #[test]
    fn a_local_repository_checks_out_at_the_pinned_commit() {
        let repo = tempfile::tempdir().unwrap();
        sample(repo.path());
        let git = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(repo.path()).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "one"]);
        let first = git(&["rev-parse", "HEAD"]);
        std::fs::write(repo.path().join("checklist.md"), "changed").unwrap();
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-am", "two"]);

        let out = tempfile::tempdir().unwrap();
        let url = repo.path().to_str().unwrap();
        let root = fetch_git(url, &first, &out.path().join("checkout")).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("checklist.md")).unwrap(), "- tests\n- docs\n");
        assert!(!root.join(".git").exists());
        assert!(fetch_git(url, "main", &out.path().join("x")).unwrap_err().contains("full commit"));
    }
}
