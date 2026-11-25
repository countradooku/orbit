//! The plugin host end to end: installing from every kind of source,
//! consent and per-call re-checks, the kill list, hash and signature
//! checks, WebAssembly hooks (deny, allow, timeouts), panels and the audit
//! log. Uses the bundled examples in `examples/plugins` and local
//! fixtures only; nothing is downloaded.

use std::path::{Path, PathBuf};

use orbit_plugin::host::{EngineEffect, Fetch, HookContext, HostOptions, PluginHost};
use orbit_plugin::package;
use orbit_plugin::sign::testing::TestKey;
use orbit_proto::*;

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins")
}

fn host(data: &Path) -> PluginHost {
    PluginHost::open(HostOptions { data_dir: data.to_owned(), keychain: false, release_key: None })
}

fn install(host: &PluginHost, source: InstallSource, grant: &[Capability]) -> Result<PluginInfo, String> {
    let resolved = host.resolve(source)?;
    let staged = PluginHost::stage(&host.dir(), resolved)?;
    host.commit(staged, grant)
}

fn archive(name: &str) -> InstallSource {
    InstallSource::Archive { name: format!("{name}.orbitplugin"), bytes: package::pack(&examples().join(name)).unwrap() }
}

fn ctx() -> HookContext {
    HookContext { thread: "thr_1".into(), cwd: "/work".into() }
}

fn command(text: &str) -> RequestKind {
    RequestKind::Command { command: text.into(), cwd: "/work".into(), reason: String::new() }
}

#[test]
fn the_theme_example_installs_in_one_click() {
    let data = tempfile::tempdir().unwrap();
    let host = host(data.path());
    let info = install(&host, archive("midnight-theme"), &[]).unwrap();
    assert_eq!(info.status, PluginStatus::Active, "no capabilities, no consent");
    assert_eq!(info.tier, PluginTier::Data);
    assert_eq!(info.ui.themes.len(), 1);
    assert!(info.ui.themes[0].contains("Midnight Daybreak"));
    // It survives a restart, from its installed copy.
    let again = self::host(data.path());
    assert_eq!(again.state().plugin("midnight-theme").unwrap().status, PluginStatus::Active);
}

#[test]
fn the_guard_example_denies_rm_rf_root_and_allows_the_rest() {
    let data = tempfile::tempdir().unwrap();
    let host = host(data.path());
    let info = install(&host, archive("guard-rm"), &[]).unwrap();
    assert_eq!(info.status, PluginStatus::NeedsConsent);
    assert_eq!(info.capabilities, [Capability::HooksTools]);
    assert_eq!(host.tool_before(&ctx(), &command("rm -rf /")), None, "not granted: it never runs");

    host.grant("guard-rm", &[Capability::HooksTools]).unwrap();
    assert!(host.wants("agent.tool.before"));
    for bad in [
        "rm -rf /",
        "sudo rm -rf --no-preserve-root /",
        "cd /tmp && rm -fr ~",
        "rm -r -f \"$HOME\"",
        "FOO=1 rm -Rf /*",
        "Remove-Item -Recurse -Force C:\\",
        "echo ok; /bin/rm -rf /usr/",
    ] {
        let denial = host.tool_before(&ctx(), &command(bad)).unwrap_or_else(|| panic!("{bad} was allowed"));
        assert_eq!(denial.plugin, "guard-rm");
        assert!(denial.reason.contains("Recursively deletes"), "{}", denial.reason);
        assert!(denial.message().starts_with("Blocked by the Guard: rm -rf / plugin"));
    }
    for fine in ["ls -la", "rm -rf ./target", "rm -rf /tmp/build", "rm /etc/hosts.bak", "git status", "echo 'rm -rf /' > notes.txt"] {
        assert_eq!(host.tool_before(&ctx(), &command(fine)), None, "{fine} was blocked");
    }
    // File edits and questions are not commands.
    assert_eq!(host.tool_before(&ctx(), &RequestKind::FileChange { path: "/".into(), diff: String::new() }), None);

    // Disabled: checked again on the next call.
    host.set_enabled("guard-rm", false).unwrap();
    assert_eq!(host.tool_before(&ctx(), &command("rm -rf /")), None);
    host.set_enabled("guard-rm", true).unwrap();
    assert!(host.tool_before(&ctx(), &command("rm -rf /")).is_some());

    // Every decision is in the audit log.
    let logs = host.logs(Some("guard-rm"), 100);
    assert!(logs.iter().any(|l| l.kind == "hook" && l.outcome == "denied"));
    assert!(logs.iter().any(|l| l.kind == "log" && l.detail.contains("Recursively deletes")));
    assert!(logs.iter().any(|l| l.call == "disable"));
}

#[test]
fn the_agent_example_adds_instances_once_granted() {
    let data = tempfile::tempdir().unwrap();
    let host = host(data.path());
    let info = install(&host, archive("agents"), &[]).unwrap();
    assert_eq!(info.status, PluginStatus::NeedsConsent);
    assert!(host.agents().is_empty(), "no programs run before consent");
    assert!(host.terminal_agent("more-agents", "aider").is_err());

    host.grant("more-agents", &[Capability::Agents]).unwrap();
    let agents = host.agents();
    assert_eq!(agents.len(), 1, "the terminal agent is not a chat instance");
    let gemini = &agents[0];
    assert_eq!(gemini.instance.id, "gemini-flash");
    assert_eq!(gemini.instance.kind, ProviderKind::Acp);
    assert_eq!(gemini.instance.args, ["--experimental-acp", "--model", "gemini-2.5-flash"]);
    assert_eq!(gemini.models[0].slug, "gemini-2.5-flash");
    assert_eq!(gemini.modes.len(), 2);
    assert_eq!(host.terminal_agent("more-agents", "aider").unwrap(), "aider --no-auto-commits");
    let state = host.state();
    let ui = &state.plugin("more-agents").unwrap().ui;
    assert_eq!(ui.agents.len(), 2);
    assert_eq!(ui.commands[0].kind, PluginCommandKind::Terminal("aider".into()));
}

#[test]
fn updates_keep_granted_capabilities_and_ask_for_new_ones() {
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let manifest = |caps: &str| {
        format!("id = \"notes\"\nname = \"Notes\"\nversion = \"1.0.{}\"\nengines = {{ orbit = \">=0.1.0\" }}\ncapabilities = [{caps}]\n", caps.len())
    };
    std::fs::write(src.path().join("orbit-plugin.toml"), manifest("\"storage\"")).unwrap();
    let host = host(data.path());
    let folder = InstallSource::Folder { path: src.path().display().to_string(), dev: false };
    let info = install(&host, folder.clone(), &[Capability::Storage, Capability::Agents]).unwrap();
    assert_eq!(info.granted, [Capability::Storage], "only what it declares is granted");
    assert_eq!(info.status, PluginStatus::Active);

    std::fs::write(src.path().join("orbit-plugin.toml"), manifest("\"storage\", \"notifications\"")).unwrap();
    let info = install(&host, folder.clone(), &[]).unwrap();
    assert_eq!(info.status, PluginStatus::NeedsConsent, "a new capability asks again");
    assert_eq!(info.granted, [Capability::Storage]);
    // The copy is independent of the source folder.
    std::fs::write(src.path().join("orbit-plugin.toml"), "garbage").unwrap();
    host.reload(Some("notes")).unwrap();
    assert_eq!(host.state().plugin("notes").unwrap().status, PluginStatus::NeedsConsent);

    host.remove("notes").unwrap();
    assert!(host.state().plugins.is_empty());
    assert!(!data.path().join("plugins/installed/notes").exists());
}

#[test]
fn dev_plugins_load_in_place_and_break_visibly() {
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("orbit-plugin.toml"), "id = \"live\"\nname = \"Live\"\nversion = \"0.1.0\"\nengines = { orbit = \">=0.1.0\" }\n").unwrap();
    let host = host(data.path());
    let info = install(&host, InstallSource::Folder { path: src.path().display().to_string(), dev: true }, &[]).unwrap();
    assert!(info.dev);
    assert_eq!(host.dev_folders()[0].1, src.path());
    std::fs::write(src.path().join("orbit-plugin.toml"), "id = \"live\"\nname = \"Live\"\nversion = \"0.1.0\"\nengines = { orbit = \">=0.1.0\" }\ncolor = 1\n").unwrap();
    host.reload(Some("live")).unwrap();
    match &host.state().plugin("live").unwrap().status {
        PluginStatus::Broken { error } => assert!(error.contains("unknown field `color`"), "{error}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn installed_files_that_change_are_refused() {
    let data = tempfile::tempdir().unwrap();
    let host = host(data.path());
    install(&host, archive("midnight-theme"), &[]).unwrap();
    let theme = data.path().join("plugins/installed/midnight-theme/themes/midnight.json");
    std::fs::write(&theme, std::fs::read_to_string(&theme).unwrap().replace("#0b1020", "#000000")).unwrap();
    let again = self::host(data.path());
    match &again.state().plugin("midnight-theme").unwrap().status {
        PluginStatus::Broken { error } => assert!(error.contains("changed after it was installed")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_kill_list_turns_plugins_off_and_forgeries_are_ignored() {
    let data = tempfile::tempdir().unwrap();
    let key = TestKey::new(21);
    let options = HostOptions { data_dir: data.path().to_owned(), keychain: false, release_key: Some(key.public_base64()) };
    let host = PluginHost::open(options.clone());
    install(&host, archive("guard-rm"), &[Capability::HooksTools]).unwrap();
    assert!(host.tool_before(&ctx(), &command("rm -rf /")).is_some());

    let list = "[[plugins]]\nid = \"guard-rm\"\nversions = \"<2.0.0\"\nreason = \"Test entry.\"\n";
    let forged = TestKey::new(22).sign(list.as_bytes(), "orbit-kill-list 1");
    assert!(host.set_kill_list(list.as_bytes(), &forged).is_err());
    assert_eq!(host.state().plugin("guard-rm").unwrap().status, PluginStatus::Active);

    let signed = key.sign(list.as_bytes(), "orbit-kill-list 1");
    assert_eq!(host.set_kill_list(list.as_bytes(), &signed).unwrap(), ["guard-rm"]);
    assert_eq!(host.state().plugin("guard-rm").unwrap().status, PluginStatus::Killed { reason: "Test entry.".into() });
    assert_eq!(host.tool_before(&ctx(), &command("rm -rf /")), None, "a killed plugin never runs");
    host.set_enabled("guard-rm", true).unwrap();
    assert!(matches!(host.state().plugin("guard-rm").unwrap().status, PluginStatus::Killed { .. }), "the user can't turn it back on");

    // The list persists, and it also refuses new installs.
    let reopened = PluginHost::open(options);
    assert!(matches!(reopened.state().plugin("guard-rm").unwrap().status, PluginStatus::Killed { .. }));
    assert!(install(&reopened, archive("guard-rm"), &[]).unwrap_err().contains("kill list"));
}

/// A signed catalog in a folder, listing the theme example as an archive.
fn catalog(dir: &Path, key: &TestKey, hash: &str) {
    std::fs::write(dir.join("midnight.orbitplugin"), package::pack(&examples().join("midnight-theme")).unwrap()).unwrap();
    let listing = format!(
        "name = \"Local\"\n[[plugins]]\nid = \"midnight-theme\"\nname = \"Midnight\"\nversion = \"1.0.0\"\ntier = \"data\"\nhash = \"{hash}\"\nsource = {{ archive = \"midnight.orbitplugin\" }}\n"
    );
    std::fs::write(dir.join("catalog.toml"), &listing).unwrap();
    std::fs::write(dir.join("catalog.toml.minisig"), key.sign(listing.as_bytes(), "orbit-catalog local")).unwrap();
}

fn read_catalog(host: &PluginHost, name: &str, dir: &Path) {
    let read = std::fs::read(dir.join("catalog.toml"))
        .map_err(|e| e.to_string())
        .and_then(|b| std::fs::read_to_string(dir.join("catalog.toml.minisig")).map(|s| (b, s)).map_err(|e| e.to_string()));
    host.set_catalog(name, read);
}

#[test]
fn catalogs_are_signed_and_their_hashes_are_enforced() {
    let data = tempfile::tempdir().unwrap();
    let served = tempfile::tempdir().unwrap();
    let key = TestKey::new(31);
    let real = orbit_plugin::hash::content_hash(&examples().join("midnight-theme")).unwrap();
    catalog(served.path(), &key, &real);
    let host = host(data.path());
    host.add_catalog("Local", &served.path().display().to_string(), &key.public_base64()).unwrap();
    read_catalog(&host, "Local", served.path());
    let state = host.state();
    assert_eq!(state.catalogs[0].error, None);
    assert_eq!(state.catalogs[0].entries[0].id, "midnight-theme");

    let resolved = host.resolve(InstallSource::Catalog { catalog: "Local".into(), id: "midnight-theme".into() }).unwrap();
    assert!(matches!(resolved.fetch, Fetch::ArchiveFile(_)));
    assert_eq!(resolved.expected_hash.as_deref(), Some(real.as_str()));
    let info = host.commit(PluginHost::stage(&host.dir(), resolved).unwrap(), &[]).unwrap();
    assert_eq!(info.source, "Catalog Local");

    // A listing whose pinned hash doesn't match the archive: refused.
    let wrong = format!("blake3:{}", "0".repeat(64));
    catalog(served.path(), &key, &wrong);
    read_catalog(&host, "Local", served.path());
    let resolved = host.resolve(InstallSource::Catalog { catalog: "Local".into(), id: "midnight-theme".into() }).unwrap();
    let err = PluginHost::stage(&host.dir(), resolved).err().unwrap();
    assert!(err.contains("don’t match the hash"), "{err}");

    // A listing signed with another key: refused, the last good copy stays.
    let other = TestKey::new(32);
    catalog(served.path(), &other, &real);
    read_catalog(&host, "Local", served.path());
    let state = host.state();
    assert!(state.catalogs[0].error.as_deref().unwrap().contains("signature"));
    assert!(!state.catalogs[0].entries.is_empty());
}

#[test]
fn git_sources_install_at_a_pinned_commit_with_a_matching_hash() {
    let repo = tempfile::tempdir().unwrap();
    let copy = |from: &Path, to: &Path| {
        for (rel, path) in orbit_plugin::hash::files(from).unwrap() {
            let dest = to.join(rel);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::copy(path, dest).unwrap();
        }
    };
    copy(&examples().join("midnight-theme"), repo.path());
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git").args(args).current_dir(repo.path()).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&["-c", "core.autocrlf=false", "add", "."]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "theme"]);
    let commit = git(&["rev-parse", "HEAD"]);
    let hash = orbit_plugin::hash::content_hash(&examples().join("midnight-theme")).unwrap();

    let data = tempfile::tempdir().unwrap();
    let host = host(data.path());
    let url = repo.path().display().to_string();
    let info = install(&host, InstallSource::Git { url: url.clone(), commit: commit.clone(), hash: Some(hash) }, &[]).unwrap();
    assert!(info.source.contains(&commit[..12]));
    let wrong = format!("blake3:{}", "1".repeat(64));
    let err = install(&host, InstallSource::Git { url, commit, hash: Some(wrong) }, &[]).unwrap_err();
    assert!(err.contains("don’t match"), "{err}");
}

/// A WebAssembly plugin from text: a hook that loops forever.
fn looping(dir: &Path, on_error: &str) {
    let wat = r#"(module (memory (export "memory") 1)
        (func (export "orbit_alloc") (param i32) (result i32) (i32.const 1024))
        (func (export "orbit_hook") (param i32 i32) (result i64) (loop $l (br $l)) (i64.const 0)))"#;
    std::fs::write(dir.join("spin.wasm"), wat::parse_str(wat).unwrap()).unwrap();
    std::fs::write(
        dir.join("orbit-plugin.toml"),
        format!(
            "id = \"spinner\"\nname = \"Spinner\"\nversion = \"1.0.0\"\nengines = {{ orbit = \">=0.1.0\" }}\ncapabilities = [\"hooks.tools\"]\n[wasm]\nmodule = \"spin.wasm\"\nhooks = [\"agent.tool.before\"]\non_error = \"{on_error}\"\nfuel = 100000\n"
        ),
    )
    .unwrap();
}

#[test]
fn hooks_that_never_finish_are_stopped_and_follow_their_error_policy() {
    for (policy, blocks) in [("allow", false), ("deny", true)] {
        let data = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        looping(src.path(), policy);
        let host = host(data.path());
        install(&host, InstallSource::Folder { path: src.path().display().to_string(), dev: false }, &[Capability::HooksTools]).unwrap();
        let started = std::time::Instant::now();
        let verdict = host.tool_before(&ctx(), &command("ls"));
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "fuel stops it");
        assert_eq!(verdict.is_some(), blocks, "{policy}");
        if let Some(d) = verdict {
            assert!(d.reason.contains("ran out of fuel"), "{}", d.reason);
        }
        assert!(host.logs(Some("spinner"), 10).iter().any(|l| l.outcome == "error" && l.detail.contains("fuel")));
    }
}

#[test]
fn panel_actions_are_checked_against_the_installed_tree_and_capabilities() {
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join("panels")).unwrap();
    std::fs::write(
        src.path().join("orbit-plugin.toml"),
        "id = \"todo\"\nname = \"Todo\"\nversion = \"1.0.0\"\nengines = { orbit = \">=0.1.0\" }\ncapabilities = [\"storage\", \"notifications\"]\n[[panels]]\nid = \"list\"\ntitle = \"Todo\"\npath = \"panels/list.toml\"\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("panels/list.toml"),
        r#"type = "stack"
children = [
  { type = "input", bind = "draft", submit = { call = "storage.append", args = { key = "todos", value = "{{input.draft}}" } } },
  { type = "button", label = "Ping", action = { call = "notifications.show", args = { title = "{{todos.0}}" } } },
]"#,
    )
    .unwrap();
    let host = host(data.path());
    install(&host, InstallSource::Folder { path: src.path().display().to_string(), dev: false }, &[Capability::Storage]).unwrap();
    // Not yet granted notifications: the plugin isn't running at all.
    assert!(host.panel_action("todo", "list", "a0", &[("draft".into(), "write docs".into())]).is_err());
    host.grant("todo", &[Capability::Notifications]).unwrap();
    let outcome = host.panel_action("todo", "list", "a0", &[("draft".into(), "write docs".into())]).unwrap();
    assert!(matches!(&outcome.effects[0], EngineEffect::Storage { json, .. } if json.contains("write docs")));
    let outcome = host.panel_action("todo", "list", "a1", &[]).unwrap();
    assert_eq!(outcome.effects, [EngineEffect::Notify { plugin: "todo".into(), title: "write docs".into(), body: String::new() }]);
    assert!(host.panel_action("todo", "list", "a7", &[]).is_err(), "an action the tree doesn't have");
    assert!(host.panel_action("todo", "nope", "a0", &[]).is_err());
    assert!(host.storage("todo").unwrap().contains("write docs"));
    // Storage persists.
    assert!(self::host(data.path()).storage("todo").unwrap().contains("write docs"));
    let audit: Vec<_> = host.logs(Some("todo"), 50).into_iter().filter(|l| l.kind == "audit").collect();
    assert!(audit.iter().any(|l| l.call == "storage.append" && l.outcome == "allowed"));
    assert!(audit.iter().any(|l| l.call == "notifications.show"));
}

#[test]
fn settings_are_declared_and_typed() {
    let data = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::fs::write(
        src.path().join("orbit-plugin.toml"),
        "id = \"cfg\"\nname = \"Cfg\"\nversion = \"1.0.0\"\nengines = { orbit = \">=0.1.0\" }\ncapabilities = [\"secrets\"]\n[[settings]]\nkey = \"limit\"\nlabel = \"Limit\"\ntype = \"number\"\ndefault = \"3\"\n[[settings]]\nkey = \"token\"\nlabel = \"Token\"\ntype = \"secret\"\n",
    )
    .unwrap();
    let host = host(data.path());
    install(&host, InstallSource::Folder { path: src.path().display().to_string(), dev: false }, &[Capability::Secrets]).unwrap();
    assert_eq!(host.setting("cfg", "limit", None).unwrap(), "3");
    assert!(host.setting("cfg", "limit", Some("many")).is_err());
    assert_eq!(host.setting("cfg", "limit", Some("5")).unwrap(), "5");
    assert!(host.setting("cfg", "other", None).is_err());
    host.setting("cfg", "token", Some("s3cret")).unwrap();
    assert_eq!(host.setting("cfg", "token", None).unwrap(), "(set)", "secrets are never read back");
    assert_eq!(host.state().secrets, "file");
}

/// Writes a signed local catalog of the bundled examples, for trying the
/// plugin browser by hand (docs/PLUGINS.md, "Trying a catalog locally"):
///
/// ```text
/// ORBIT_FIXTURE_DIR=<out> cargo test -p orbit-plugin --test host make_catalog_fixture -- --ignored
/// ```
///
/// It writes `catalog.toml`, its signature, one `.orbitplugin` per example
/// and `public-key.txt` (the test key to add the catalog with).
#[test]
#[ignore = "writes a fixture for manual checks"]
fn make_catalog_fixture() {
    let dir = PathBuf::from(std::env::var("ORBIT_FIXTURE_DIR").expect("set ORBIT_FIXTURE_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let key = TestKey::new(77);
    let mut listing = String::from("name = \"Orbit examples\"\n");
    for name in ["midnight-theme", "agents", "guard-rm"] {
        let root = examples().join(name);
        let loaded = package::load(&root).unwrap();
        let m = &loaded.validated.manifest;
        let file = format!("{}-{}.orbitplugin", m.id, m.version);
        std::fs::write(dir.join(&file), package::pack(&root).unwrap()).unwrap();
        let caps: Vec<String> = loaded.validated.capabilities.iter().map(|c| format!("\"{}\"", c.key())).collect();
        listing.push_str(&format!(
            "\n[[plugins]]\nid = \"{}\"\nname = \"{}\"\nversion = \"{}\"\ndescription = \"{}\"\npublisher = \"{}\"\ncategories = [{}]\ntier = \"{}\"\ncapabilities = [{}]\nhash = \"{}\"\nsource = {{ archive = \"{file}\" }}\n",
            m.id,
            m.name,
            m.version,
            m.description,
            m.publisher,
            m.categories.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", "),
            loaded.validated.tier.label().to_lowercase().replace("webassembly", "wasm"),
            caps.join(", "),
            loaded.hash
        ));
    }
    std::fs::write(dir.join("catalog.toml"), &listing).unwrap();
    std::fs::write(dir.join("catalog.toml.minisig"), key.sign(listing.as_bytes(), "orbit-catalog examples")).unwrap();
    std::fs::write(dir.join("public-key.txt"), key.public_base64()).unwrap();
}
