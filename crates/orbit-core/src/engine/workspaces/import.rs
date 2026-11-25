//! Project configuration checked into a repository.
//!
//! `orbit.toml` is Orbit's own file (documented in `docs/orbit-toml.md`).
//! Other tools' files are read where that is cheap, so a team switching to
//! Orbit keeps its setup: Conductor's `conductor.json`, Codex's
//! `.codex/environments/*.toml`, a dev container's `devcontainer.json`, and
//! as suggestions the scripts of `package.json` and a Cargo workspace.
//!
//! Sources are applied from the weakest to the strongest, so `orbit.toml`
//! wins: suggestions only fill what is empty, the other files override.

use std::collections::BTreeMap;
use std::path::Path;

use orbit_proto::*;
use serde::Deserialize;

/// Files larger than this are not read.
const MAX_FILE: u64 = 256 * 1024;

/// The project and configuration with what the repository configures
/// merged in, and the files that contributed.
pub(crate) fn import(root: &Path, project: &Project, config: &ProjectConfig) -> (Project, ProjectConfig, Vec<String>) {
    let mut merged = Merge { project: project.clone(), config: config.clone(), sources: Vec::new() };
    suggestions(root, &mut merged);
    devcontainer(root, &mut merged);
    codex(root, &mut merged);
    conductor(root, &mut merged);
    if let Some(text) = read(&root.join("orbit.toml")) {
        match orbit_toml(&text) {
            Ok(file) => {
                file.apply(&mut merged);
                merged.sources.push("orbit.toml".into());
            }
            Err(err) => log::warn!("{}: {err}", root.join("orbit.toml").display()),
        }
    }
    let mut config = merged.config;
    config.imported_from = merged.sources.clone();
    (merged.project, config.sanitized(), merged.sources)
}

/// Parses `orbit.toml`; the error says what is wrong and where.
pub(crate) fn orbit_toml(text: &str) -> Result<OrbitToml, String> {
    toml::from_str::<OrbitToml>(text).map_err(|e| e.to_string())
}

struct Merge {
    project: Project,
    config: ProjectConfig,
    sources: Vec<String>,
}

impl Merge {
    fn set_setup(&mut self, script: String, weak: bool) {
        let script = script.trim().to_owned();
        if script.is_empty() || (weak && self.project.scripts.setup.as_deref().is_some_and(|s| !s.trim().is_empty())) {
            return;
        }
        self.project.scripts.setup = Some(script);
    }

    fn set_teardown(&mut self, script: String) {
        let script = script.trim().to_owned();
        if !script.is_empty() {
            self.project.scripts.teardown = Some(script);
        }
    }

    /// Adds a target, replacing one of the same name unless `weak`.
    fn target(&mut self, target: ProjectTarget, weak: bool) {
        if target.name.trim().is_empty() || target.command.trim().is_empty() {
            return;
        }
        match self.config.targets.iter_mut().find(|t| t.name.eq_ignore_ascii_case(&target.name)) {
            Some(_) if weak => {}
            Some(slot) => *slot = target,
            None => self.config.targets.push(target),
        }
    }

    fn env(&mut self, name: &str, value: &str) {
        if !is_env_name(name) {
            return;
        }
        self.config.env.retain(|v| v.name != name);
        self.config.env.push(EnvVar::new(name, value));
    }
}

fn read(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// `orbit.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct OrbitToml {
    name: Option<String>,
    icon: Option<String>,
    base_branch: Option<String>,
    /// New threads start on their own worktree.
    worktree: Option<bool>,
    ports: Option<u16>,
    include: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    scripts: Option<ScriptsToml>,
    targets: Option<Vec<TargetToml>>,
    agent: Option<AgentToml>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ScriptsToml {
    setup: Option<String>,
    teardown: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TargetToml {
    name: String,
    command: String,
    kind: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AgentToml {
    provider: Option<String>,
    model: Option<String>,
    mode: Option<String>,
    access: Option<String>,
}

impl OrbitToml {
    fn apply(self, merged: &mut Merge) {
        if let Some(name) = self.name.filter(|n| !n.trim().is_empty()) {
            merged.project.name = name.trim().to_owned();
        }
        if let Some(branch) = self.base_branch.filter(|b| !b.trim().is_empty()) {
            merged.project.base_branch = Some(branch.trim().to_owned());
        }
        if let Some(icon) = self.icon {
            merged.config.icon = icon;
        }
        if let Some(worktree) = self.worktree {
            merged.config.worktree_by_default = Some(worktree);
        }
        if let Some(ports) = self.ports {
            merged.config.ports = ports;
        }
        if let Some(include) = self.include {
            merged.config.include = include;
        }
        for (name, value) in self.env.unwrap_or_default() {
            merged.env(&name, &value);
        }
        if let Some(scripts) = self.scripts {
            if let Some(setup) = scripts.setup {
                merged.set_setup(setup, false);
            }
            if let Some(teardown) = scripts.teardown {
                merged.set_teardown(teardown);
            }
        }
        for target in self.targets.unwrap_or_default() {
            let kind = target.kind.as_deref().map(parse_kind).unwrap_or_else(|| TargetKind::guess(&target.name));
            merged.target(
                ProjectTarget { name: target.name, command: target.command, kind, url: target.url.unwrap_or_default() },
                false,
            );
        }
        if let Some(agent) = self.agent {
            let mut config = merged.config.default_config.clone().unwrap_or_default();
            if let Some(provider) = agent.provider {
                let id = provider.trim().to_ascii_lowercase();
                match ProviderKind::ALL.into_iter().find(|k| k.id() == id) {
                    Some(kind) => {
                        config.provider = kind;
                        config.instance = String::new();
                    }
                    // Another configured instance (`claude-work`, `gemini`).
                    None if is_instance_id(&id) => config.instance = id,
                    None => {}
                }
            }
            if let Some(model) = agent.model {
                config.model = model.trim().to_owned();
            }
            match agent.mode.as_deref().map(str::trim) {
                Some("plan") => config.mode = Mode::Plan,
                Some("build") => config.mode = Mode::Build,
                _ => {}
            }
            config.access = match agent.access.as_deref().map(str::trim) {
                Some("readonly" | "read-only") => Access::Readonly,
                Some("auto-edit" | "auto_edit" | "autoedit") => Access::AutoEdit,
                Some("ask") => Access::Ask,
                _ => config.access,
            }
            // A checked-in file never grants full access.
            .for_new_thread();
            merged.config.default_config = Some(config);
        }
    }
}

fn parse_kind(kind: &str) -> TargetKind {
    match kind.trim().to_ascii_lowercase().as_str() {
        "dev" | "server" | "serve" => TargetKind::Dev,
        "test" | "tests" => TargetKind::Test,
        "lint" | "check" => TargetKind::Lint,
        "build" => TargetKind::Build,
        _ => TargetKind::Other,
    }
}

/// Conductor: `conductor.json` with `scripts.setup`, `scripts.run` and
/// `scripts.archive`.
fn conductor(root: &Path, merged: &mut Merge) {
    let Some(text) = read(&root.join("conductor.json")) else { return };
    let Ok(value) = sonic_rs::from_str::<sonic_rs::Value>(&text) else { return };
    use sonic_rs::{JsonContainerTrait as _, JsonValueTrait as _};
    let Some(scripts) = value.get("scripts").and_then(|s| s.as_object()) else { return };
    let get = |key: &str| scripts.get(&key).and_then(|v| v.as_str()).map(str::to_owned);
    if let Some(setup) = get("setup") {
        merged.set_setup(setup, false);
    }
    if let Some(archive) = get("archive") {
        merged.set_teardown(archive);
    }
    if let Some(run) = get("run") {
        merged.target(ProjectTarget { name: "Run".into(), kind: TargetKind::guess(&run), command: run, url: String::new() }, false);
    }
    merged.sources.push("conductor.json".into());
}

/// Codex: `.codex/environments/*.toml`, `[setup] script` and `[[actions]]`
/// with `name` and `command`.
fn codex(root: &Path, merged: &mut Merge) {
    let dir = root.join(".codex").join("environments");
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let mut files: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "toml")).collect();
    files.sort();
    for path in files {
        let Some(text) = read(&path) else { continue };
        let Ok(table) = toml::from_str::<toml::Table>(&text) else { continue };
        let setup = match table.get("setup") {
            Some(toml::Value::String(script)) => Some(script.clone()),
            Some(toml::Value::Table(setup)) => setup.get("script").and_then(|s| s.as_str()).map(str::to_owned),
            _ => None,
        };
        if let Some(setup) = setup {
            merged.set_setup(setup, false);
        }
        for action in table.get("actions").and_then(|a| a.as_array()).into_iter().flatten() {
            let name = action.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            let command = action.get("command").and_then(|c| c.as_str()).unwrap_or_default();
            merged.target(
                ProjectTarget { name: name.to_owned(), command: command.to_owned(), kind: TargetKind::guess(name), url: String::new() },
                false,
            );
        }
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        merged.sources.push(rel);
    }
}

/// A dev container: `postCreateCommand` as setup, `containerEnv` and
/// `remoteEnv`, and the first forwarded port as the dev server's address.
fn devcontainer(root: &Path, merged: &mut Merge) {
    let candidates = [root.join(".devcontainer").join("devcontainer.json"), root.join(".devcontainer.json")];
    let Some((path, text)) = candidates.iter().find_map(|p| read(p).map(|t| (p, t))) else { return };
    let Ok(value) = sonic_rs::from_str::<sonic_rs::Value>(&strip_json_comments(&text)) else { return };
    use sonic_rs::{JsonContainerTrait as _, JsonValueTrait as _};
    if let Some(command) = value.get("postCreateCommand") {
        let script = if let Some(s) = command.as_str() {
            Some(s.to_owned())
        } else if let Some(parts) = command.as_array() {
            Some(parts.iter().filter_map(|p| p.as_str()).collect::<Vec<_>>().join(" "))
        } else {
            command.as_object().map(|o| o.iter().filter_map(|(_, v)| v.as_str()).collect::<Vec<_>>().join(" && "))
        };
        if let Some(script) = script {
            merged.set_setup(script, false);
        }
    }
    for key in ["containerEnv", "remoteEnv"] {
        if let Some(env) = value.get(key).and_then(|e| e.as_object()) {
            for (name, value) in env.iter() {
                // `${localEnv:X}` and friends only make sense in a container.
                if let Some(value) = value.as_str().filter(|v| !v.contains("${")) {
                    merged.env(name, value);
                }
            }
        }
    }
    if let Some(port) = value.get("forwardPorts").and_then(|p| p.as_array()).and_then(|a| a.iter().find_map(|p| p.as_u64())) {
        for target in merged.config.targets.iter_mut().filter(|t| t.kind == TargetKind::Dev && t.url.is_empty()) {
            target.url = format!("http://localhost:{port}");
        }
    }
    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/");
    merged.sources.push(rel);
}

/// `//` and `/* */` comments removed outside strings (JSON with comments,
/// as dev containers allow), and trailing commas before `}` or `]`.
fn strip_json_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
            }
            (',', _) => {
                // Drop the comma when only whitespace separates it from a closer.
                let rest: String = chars.clone().take_while(|c| c.is_whitespace()).collect();
                let after = chars.clone().nth(rest.chars().count());
                if !matches!(after, Some('}' | ']')) {
                    out.push(',');
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Suggestions from the project's own build files: `package.json` scripts
/// (with its package manager) and a Cargo workspace. They only fill what
/// nothing else configures.
fn suggestions(root: &Path, merged: &mut Merge) {
    if let Some(text) = read(&root.join("package.json"))
        && let Ok(value) = sonic_rs::from_str::<sonic_rs::Value>(&text)
    {
        use sonic_rs::{JsonContainerTrait as _, JsonValueTrait as _};
        let manager = if root.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if root.join("yarn.lock").is_file() {
            "yarn"
        } else if root.join("bun.lockb").is_file() || root.join("bun.lock").is_file() {
            "bun"
        } else {
            "npm"
        };
        let mut added = false;
        if let Some(scripts) = value.get("scripts").and_then(|s| s.as_object()) {
            for (script, label) in [
                ("dev", "Dev"),
                ("start", "Start"),
                ("storybook", "Storybook"),
                ("test", "Test"),
                ("lint", "Lint"),
                ("typecheck", "Typecheck"),
                ("build", "Build"),
            ] {
                if scripts.get(&script).is_none() {
                    continue;
                }
                // `start` is the dev server only where there is no `dev`.
                if script == "start" && scripts.get(&"dev").is_some() {
                    continue;
                }
                let command = if manager == "npm" && script != "start" && script != "test" {
                    format!("npm run {script}")
                } else {
                    format!("{manager} {}", if manager == "npm" { script.to_owned() } else { format!("run {script}") })
                };
                merged.target(ProjectTarget { name: label.into(), command, kind: TargetKind::guess(script), url: String::new() }, true);
                added = true;
            }
        }
        let lockfile = ["package-lock.json", "pnpm-lock.yaml", "yarn.lock", "bun.lockb", "bun.lock"].iter().any(|f| root.join(f).is_file());
        if lockfile {
            let install = if manager == "npm" { "npm ci".to_owned() } else { format!("{manager} install") };
            merged.set_setup(install, true);
            added = true;
        }
        if added {
            merged.sources.push("package.json".into());
        }
    }
    if root.join("Cargo.toml").is_file() {
        merged.target(ProjectTarget { name: "Test".into(), command: "cargo test".into(), kind: TargetKind::Test, url: String::new() }, true);
        merged.target(
            ProjectTarget { name: "Lint".into(), command: "cargo clippy --all-targets".into(), kind: TargetKind::Lint, url: String::new() },
            true,
        );
        merged.sources.push("Cargo.toml".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Project {
        Project {
            id: ProjectId(1),
            name: "app".into(),
            path: String::new(),
            order: 0,
            scripts: ProjectScripts::default(),
            base_branch: Some("main".into()),
        }
    }

    fn files(list: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in list {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    const ORBIT_TOML: &str = r#"
name = "Shop"
icon = "🛒"
base_branch = "develop"
worktree = true
ports = 4
include = [".env", "config/local.json"]

[env]
NODE_ENV = "development"

[scripts]
setup = "pnpm install"
teardown = "docker compose down"

[[targets]]
name = "Dev"
command = "pnpm dev --port $PORT"
url = "http://localhost:$PORT"

[[targets]]
name = "E2E"
command = "pnpm playwright test"
kind = "test"

[agent]
provider = "codex"
model = "gpt-5-codex"
mode = "plan"
access = "full"
"#;

    #[test]
    fn orbit_toml_configures_everything() {
        let dir = files(&[("orbit.toml", ORBIT_TOML)]);
        let (project, config, sources) = import(dir.path(), &project(), &ProjectConfig::default());
        assert_eq!(sources, vec!["orbit.toml"]);
        assert_eq!(project.name, "Shop");
        assert_eq!(project.base_branch.as_deref(), Some("develop"));
        assert_eq!(project.scripts.setup.as_deref(), Some("pnpm install"));
        assert_eq!(project.scripts.teardown.as_deref(), Some("docker compose down"));
        assert_eq!(config.icon, "🛒");
        assert_eq!(config.worktree_by_default, Some(true));
        assert_eq!(config.ports, 4);
        assert_eq!(config.include, vec![".env", "config/local.json"]);
        assert_eq!(config.env, vec![EnvVar::new("NODE_ENV", "development")]);
        assert_eq!(config.targets.len(), 2);
        assert_eq!(config.targets[0].kind, TargetKind::Dev);
        assert_eq!(config.targets[0].url, "http://localhost:$PORT");
        assert_eq!(config.targets[1].kind, TargetKind::Test);
        let agent = config.default_config.unwrap();
        assert_eq!((agent.provider, agent.model.as_str(), agent.mode), (ProviderKind::Codex, "gpt-5-codex", Mode::Plan));
        assert_ne!(agent.access, Access::Full, "a checked-in file never grants full access");
        assert_eq!(config.imported_from, vec!["orbit.toml"]);
    }

    #[test]
    fn unknown_keys_are_reported_not_ignored() {
        let err = orbit_toml("[scripts]\nsetpu = \"x\"\n").unwrap_err();
        assert!(err.contains("setpu"), "{err}");
    }

    #[test]
    fn other_tools_and_suggestions_merge_with_orbit_toml_winning() {
        let dir = files(&[
            ("package.json", r#"{"scripts": {"dev": "vite", "test": "vitest", "lint": "eslint ."}}"#),
            ("pnpm-lock.yaml", ""),
            ("conductor.json", r#"{"scripts": {"setup": "pnpm i && cp $CONDUCTOR_ROOT_PATH/.env .", "run": "pnpm dev", "archive": "rm -rf tmp"}}"#),
            (
                ".devcontainer/devcontainer.json",
                "{\n // a comment\n \"postCreateCommand\": [\"make\", \"setup\"],\n \"containerEnv\": {\"DB\": \"postgres\", \"HOME_DIR\": \"${localEnv:HOME}\"},\n \"forwardPorts\": [5173, 5432],\n}",
            ),
            (".codex/environments/environment.toml", "version = 1\n[setup]\nscript = '''\nuv sync\n'''\n[[actions]]\nname = \"Serve\"\ncommand = \"uv run serve\"\n"),
            ("orbit.toml", "[[targets]]\nname = \"Test\"\ncommand = \"pnpm test --run\"\n"),
        ]);
        let (project, config, sources) = import(dir.path(), &project(), &ProjectConfig::default());
        assert_eq!(
            sources,
            vec!["package.json", ".devcontainer/devcontainer.json", ".codex/environments/environment.toml", "conductor.json", "orbit.toml"]
        );
        // Conductor applied after Codex and the dev container.
        assert_eq!(project.scripts.setup.as_deref(), Some("pnpm i && cp $CONDUCTOR_ROOT_PATH/.env ."));
        assert_eq!(project.scripts.teardown.as_deref(), Some("rm -rf tmp"));
        let targets: Vec<(&str, &str)> = config.targets.iter().map(|t| (t.name.as_str(), t.command.as_str())).collect();
        assert_eq!(
            targets,
            vec![("Dev", "pnpm run dev"), ("Test", "pnpm test --run"), ("Lint", "pnpm run lint"), ("Serve", "uv run serve"), ("Run", "pnpm dev")]
        );
        assert_eq!(config.targets[0].url, "http://localhost:5173");
        assert_eq!(config.env, vec![EnvVar::new("DB", "postgres")]);
    }

    #[test]
    fn suggestions_never_override_what_is_set() {
        let dir = files(&[("package.json", r#"{"scripts": {"dev": "next dev"}}"#), ("package-lock.json", "{}")]);
        let mut current = project();
        current.scripts.setup = Some("make bootstrap".into());
        let config = ProjectConfig {
            targets: vec![ProjectTarget { name: "Dev".into(), command: "make dev".into(), ..Default::default() }],
            ..Default::default()
        };
        let (project, config, _) = import(dir.path(), &current, &config);
        assert_eq!(project.scripts.setup.as_deref(), Some("make bootstrap"));
        assert_eq!(config.targets[0].command, "make dev");
        let empty = files(&[("package.json", r#"{"scripts": {"start": "node ."}}"#), ("package-lock.json", "{}")]);
        let (project, config, _) = import(empty.path(), &self::project(), &ProjectConfig::default());
        assert_eq!(project.scripts.setup.as_deref(), Some("npm ci"));
        assert_eq!(config.targets[0].command, "npm start");
    }

    #[test]
    fn json_comments_and_trailing_commas_are_stripped() {
        let text = "{ \"a\": \"// not a comment\", /* gone */ \"b\": [1, 2,], }";
        let value: sonic_rs::Value = sonic_rs::from_str(&strip_json_comments(text)).unwrap();
        use sonic_rs::JsonValueTrait as _;
        assert_eq!(value.get("a").and_then(|v| v.as_str()), Some("// not a comment"));
    }
}
