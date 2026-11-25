//! The plugin host: installed plugins, their state on disk, consent, the
//! audit log, storage, secrets and running their code.
//!
//! Lives in the engine's process (the host, `orbitd`). Layout of
//! `<data>/plugins/`:
//!
//! ```text
//! state.toml                 installed plugins (source, enabled, granted capabilities, hash) and catalogs
//! installed/<id>/            a copy of each plugin's files (dev plugins stay in their own folder)
//! data/<id>/storage.json     its storage; settings.json its settings; secrets in the keychain
//! logs/<id>.jsonl            its log: every host API call and hook decision (the audit trail)
//! catalogs/<name>.toml(.minisig)   the last good copy of each catalog
//! kill-list.toml(.minisig)   the signed kill list
//! staging/                   installs in progress
//! ```
//!
//! Every host API call is checked right before its effect: the plugin is
//! installed, enabled, not on the kill list, its files validate, and the
//! capability is declared and granted. Each call is recorded.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use orbit_proto::*;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sonic_rs::{JsonContainerTrait as _, JsonValueTrait as _, Value};

use crate::catalog::{self, Catalog};
use crate::killlist::KillList;
use crate::manifest::{OnError, check_setting};
use crate::package::{self, Loaded};
use crate::panel::{self, Effect};
use crate::secrets::Secrets;
use crate::wasm::{self, ApiError, CallError, HostApi, Sandbox};
use crate::{hash, now_ms, template};

const STATE: &str = "state.toml";
/// A plugin's log is trimmed to half when it passes this.
const LOG_BYTES: u64 = 256 * 1024;
/// Tool output given to `agent.tool.after`.
const TOOL_OUTPUT: usize = 8 * 1024;

/// How the host is set up.
#[derive(Clone, Debug)]
pub struct HostOptions {
    /// Orbit's data folder; plugins live in `<data_dir>/plugins`.
    pub data_dir: PathBuf,
    /// Keep secrets in the system keychain when there is one.
    pub keychain: bool,
    /// The key the kill list must be signed with (default: the release key).
    pub release_key: Option<String>,
}

impl HostOptions {
    pub fn new(data_dir: &Path) -> Self {
        Self { data_dir: data_dir.to_owned(), keychain: true, release_key: crate::sign::release_key() }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    #[serde(default)]
    plugins: Vec<Record>,
    #[serde(default)]
    catalogs: Vec<CatalogConfig>,
}

/// What the host remembers about an installed plugin.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Record {
    id: String,
    version: String,
    enabled: bool,
    #[serde(default)]
    granted: Vec<String>,
    /// As shown: "Folder …", "url@commit", "Catalog …".
    source: String,
    /// Loaded in place from here (dev mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dev: Option<String>,
    hash: String,
    installed_ms: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct CatalogConfig {
    name: String,
    location: String,
    public_key: String,
}

struct Plugin {
    record: Record,
    loaded: Result<Loaded, String>,
    storage: Value,
    sandbox: Option<Sandbox>,
}

struct Inner {
    dir: PathBuf,
    state: StateFile,
    plugins: BTreeMap<String, Plugin>,
    kill: KillList,
    release_key: Option<String>,
    catalogs: BTreeMap<String, CatalogInfo>,
    /// Releases of each catalog, for installs.
    releases: BTreeMap<String, (String, Catalog)>,
    secrets: Arc<Secrets>,
    threads: Arc<String>,
    /// What hooks called from sessions asked for, until the engine takes it.
    pending: Vec<EngineEffect>,
}

/// A source made concrete: what to fetch, and what it must hash to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub fetch: Fetch,
    pub expected_hash: Option<String>,
    /// As shown in the plugin's row.
    pub origin: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
    Folder { path: PathBuf, dev: bool },
    Archive(Vec<u8>),
    ArchiveFile(PathBuf),
    /// An https URL of an archive; the engine downloads it and stages the
    /// bytes as [`Fetch::Archive`].
    Url(String),
    Git { url: String, commit: String },
}

/// A plugin unpacked, checked and ready to install.
pub struct Staged {
    loaded: Loaded,
    /// The folder to move into place, or `None` for dev plugins.
    folder: Option<PathBuf>,
    staging: Option<PathBuf>,
    dev: Option<PathBuf>,
    origin: String,
}

impl Staged {
    pub fn id(&self) -> &str {
        self.loaded.id()
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(dir) = self.staging.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// A hook's verdict against an agent action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    pub plugin: String,
    pub name: String,
    pub reason: String,
}

impl Denial {
    /// What the agent and the timeline are told.
    pub fn message(&self) -> String {
        format!("Blocked by the {} plugin: {}", self.name, self.reason)
    }
}

/// Where a hook fires.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookContext {
    pub thread: String,
    pub cwd: String,
}

/// Something a plugin asked the engine to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineEffect {
    Notify { plugin: String, title: String, body: String },
    CreateThread { plugin: String, prompt: String },
    Storage { plugin: String, json: String },
}

/// The result of a command or panel action.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub message: String,
    pub effects: Vec<EngineEffect>,
}

/// An agent CLI from an active plugin with the `agents` capability.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentDef {
    pub plugin: String,
    pub instance: ProviderInstance,
    pub models: Vec<ModelInfo>,
    pub modes: Vec<ModeInfo>,
    pub install: String,
}

#[derive(Clone)]
pub struct PluginHost {
    inner: Arc<Mutex<Inner>>,
}

impl PluginHost {
    /// Opens `<data>/plugins`, loading every installed plugin. A plugin
    /// that no longer loads is kept, marked broken.
    pub fn open(options: HostOptions) -> Self {
        let dir = options.data_dir.join("plugins");
        let state: StateFile = std::fs::read_to_string(dir.join(STATE))
            .ok()
            .and_then(|t| {
                toml::from_str(&t)
                    .inspect_err(|e| log::error!("plugins: {STATE} is unreadable ({e}); starting with no plugins"))
                    .ok()
            })
            .unwrap_or_default();
        let kill = match &options.release_key {
            Some(key) => load_kill_list(&dir, key),
            None => KillList::default(),
        };
        let secrets = Arc::new(Secrets::open(&dir.join("data"), options.keychain));
        let mut inner = Inner {
            dir,
            state,
            plugins: BTreeMap::new(),
            kill,
            release_key: options.release_key,
            catalogs: BTreeMap::new(),
            releases: BTreeMap::new(),
            secrets,
            threads: Arc::new("[]".into()),
            pending: Vec::new(),
        };
        let records = inner.state.plugins.clone();
        for record in records {
            inner.load(record);
        }
        for config in inner.state.catalogs.clone() {
            inner.catalogs.insert(config.name.clone(), CatalogInfo { name: config.name.clone(), location: config.location.clone(), ..Default::default() });
            inner.read_cached_catalog(&config);
        }
        Self { inner: Arc::new(Mutex::new(inner)) }
    }

    pub fn dir(&self) -> PathBuf {
        self.inner.lock().dir.clone()
    }

    /// What clients see.
    pub fn state(&self) -> PluginsState {
        let inner = self.inner.lock();
        PluginsState {
            plugins: inner.plugins.values().map(|p| inner.info(p)).collect(),
            catalogs: inner.catalogs.values().cloned().collect(),
            secrets: inner.secrets.protection().map(|p| p.key().to_owned()).unwrap_or_default(),
        }
    }

    /// One plugin's storage as JSON.
    pub fn storage(&self, id: &str) -> Option<String> {
        self.inner.lock().plugins.get(id).map(|p| p.storage.to_string())
    }

    // ── Installing ───────────────────────────────────────────────────────

    /// Turns a source into what to fetch (a catalog entry into its
    /// location and pinned hash).
    pub fn resolve(&self, source: InstallSource) -> Result<Resolved, String> {
        Ok(match source {
            InstallSource::Folder { path, dev } => {
                let path = PathBuf::from(&path);
                if !path.is_absolute() {
                    return Err("Give the plugin folder as a full path.".into());
                }
                let origin = if dev { format!("Dev folder {}", path.display()) } else { format!("Folder {}", path.display()) };
                Resolved { fetch: Fetch::Folder { path, dev }, expected_hash: None, origin }
            }
            InstallSource::Archive { name, bytes } => {
                if bytes.len() as u64 > crate::manifest::limits::PLUGIN_BYTES {
                    return Err("The archive is too large.".into());
                }
                // A folder packed by a client keeps its folder's name.
                let origin = if name.ends_with(&format!(".{}", package::EXTENSION)) { format!("Archive {name}") } else { format!("Folder {name}") };
                Resolved { fetch: Fetch::Archive(bytes), expected_hash: None, origin }
            }
            InstallSource::Git { url, commit, hash } => {
                if !package::is_git_url(&url) {
                    return Err(format!("“{url}” isn’t a git address Orbit fetches from (https, ssh or a local path)."));
                }
                if !package::is_commit(&commit) {
                    return Err("Pin a full commit id (40 hex digits): a branch or tag can change under you.".into());
                }
                let origin = format!("{url}@{}", &commit[..12]);
                Resolved { fetch: Fetch::Git { url, commit }, expected_hash: hash.filter(|h| !h.is_empty()), origin }
            }
            InstallSource::Catalog { catalog, id } => {
                let inner = self.inner.lock();
                let (location, listing) = inner.releases.get(&catalog).ok_or_else(|| format!("No catalog “{catalog}” has been read."))?;
                let release = listing.plugins.iter().find(|r| r.id == id).ok_or_else(|| format!("“{catalog}” doesn’t list “{id}”."))?;
                let fetch = if let Some(archive) = &release.source.archive {
                    let at = catalog::resolve(location, archive)?;
                    if at.starts_with("https://") || at.starts_with("http://") { Fetch::Url(at) } else { Fetch::ArchiveFile(PathBuf::from(at)) }
                } else if let Some(path) = &release.source.path {
                    if location.starts_with("https://") || location.starts_with("http://") {
                        return Err("A catalog served over HTTPS can’t point at folders.".into());
                    }
                    Fetch::Folder { path: PathBuf::from(catalog::resolve(location, path)?), dev: false }
                } else {
                    let (url, commit) = (release.source.git.clone().unwrap_or_default(), release.source.commit.clone().unwrap_or_default());
                    if !package::is_git_url(&url) || !package::is_commit(&commit) {
                        return Err(format!("The catalog’s source for “{id}” isn’t a pinned git commit."));
                    }
                    Fetch::Git { url, commit }
                };
                Resolved { fetch, expected_hash: Some(release.hash.clone()), origin: format!("Catalog {catalog}") }
            }
        })
    }

    /// Fetches (except URLs) and checks a resolved source into a staging
    /// folder. Blocking: run it off the engine thread.
    pub fn stage(dir: &Path, resolved: Resolved) -> Result<Staged, String> {
        let staging_root = dir.join("staging");
        std::fs::create_dir_all(&staging_root).map_err(|e| format!("Couldn’t prepare the install: {e}"))?;
        let staging = staging_root.join(format!("s{}-{}", now_ms(), std::process::id()));
        let mut dev = None;
        let (root, staging) = match resolved.fetch {
            Fetch::Folder { path, dev: true } => {
                dev = Some(path.clone());
                (path, None)
            }
            Fetch::Folder { path, dev: false } => {
                copy_plugin(&path, &staging)?;
                (staging.clone(), Some(staging))
            }
            Fetch::Archive(bytes) => (package::unpack(&bytes, &staging)?, Some(staging)),
            Fetch::ArchiveFile(path) => {
                let bytes = std::fs::read(&path).map_err(|e| format!("Couldn’t read {}: {e}", path.display()))?;
                (package::unpack(&bytes, &staging)?, Some(staging))
            }
            Fetch::Git { url, commit } => (package::fetch_git(&url, &commit, &staging)?, Some(staging)),
            Fetch::Url(url) => return Err(format!("{url} must be downloaded first.")),
        };
        let mut staged = Staged { loaded: placeholder_loaded(), folder: None, staging, dev, origin: resolved.origin };
        staged.loaded = package::load(&root).map_err(|problems| problems.join("\n"))?;
        if let Some(expected) = &resolved.expected_hash
            && !hash::matches(expected, &staged.loaded.hash)
        {
            return Err(format!(
                "The plugin’s files don’t match the hash they were published with ({} ≠ {expected}); it was not installed.",
                staged.loaded.hash
            ));
        }
        if staged.dev.is_none() {
            staged.folder = Some(root);
        }
        Ok(staged)
    }

    /// The plugin as it would appear once installed, and the version it
    /// would replace.
    pub fn preview(&self, staged: &Staged) -> (PluginInfo, Option<String>) {
        let inner = self.inner.lock();
        let existing = inner.plugins.get(staged.id());
        let granted = existing.map(|p| p.record.granted.clone()).unwrap_or_default();
        let record = Record {
            id: staged.id().to_owned(),
            version: staged.loaded.validated.manifest.version.clone(),
            enabled: true,
            granted,
            source: staged.origin.clone(),
            dev: staged.dev.as_ref().map(|d| d.display().to_string()),
            hash: staged.loaded.hash.clone(),
            installed_ms: now_ms(),
        };
        let plugin = Plugin { record, loaded: Ok(staged.loaded.clone()), storage: Value::new_object(), sandbox: None };
        let mut info = inner.info(&plugin);
        // What consent needs: the programs it would run, nothing else.
        info.ui = PluginUi { agents: staged.loaded.ui.agents.clone(), ..Default::default() };
        (info, existing.map(|p| p.record.version.clone()))
    }

    /// Installs a staged plugin (replacing an installed one of the same id)
    /// and grants `grant` (only what it declares). Capabilities it keeps
    /// from an earlier version stay granted; new ones need consent.
    pub fn commit(&self, mut staged: Staged, grant: &[Capability]) -> Result<PluginInfo, String> {
        let mut inner = self.inner.lock();
        let id = staged.id().to_owned();
        if let Some(reason) = inner.kill.reason(&id, &staged.loaded.validated.manifest.version) {
            return Err(format!("“{id}” is on Orbit’s kill list: {reason}"));
        }
        let declared = &staged.loaded.validated.capabilities;
        let mut granted: Vec<String> = inner
            .plugins
            .get(&id)
            .map(|p| p.record.granted.iter().filter(|g| declared.iter().any(|d| d.key() == g.as_str())).cloned().collect())
            .unwrap_or_default();
        for c in grant {
            if declared.contains(c) && !granted.iter().any(|g| g == c.key()) {
                granted.push(c.key().to_owned());
            }
        }
        let installed = inner.dir.join("installed").join(&id);
        if let Some(folder) = staged.folder.take() {
            std::fs::create_dir_all(inner.dir.join("installed")).map_err(|e| e.to_string())?;
            let old = inner.dir.join("staging").join(format!("old-{id}-{}", now_ms()));
            if installed.exists() {
                std::fs::rename(&installed, &old).map_err(|e| format!("Couldn’t replace the installed copy: {e}"))?;
            }
            if let Err(e) = std::fs::rename(&folder, &installed) {
                let _ = std::fs::rename(&old, &installed);
                return Err(format!("Couldn’t install: {e}"));
            }
            let _ = std::fs::remove_dir_all(&old);
        } else if installed.exists() {
            // A dev install replaces a copied one.
            let _ = std::fs::remove_dir_all(&installed);
        }
        let record = Record {
            id: id.clone(),
            version: staged.loaded.validated.manifest.version.clone(),
            enabled: true,
            granted,
            source: staged.origin.clone(),
            dev: staged.dev.as_ref().map(|d| d.display().to_string()),
            hash: staged.loaded.hash.clone(),
            installed_ms: now_ms(),
        };
        inner.state.plugins.retain(|r| r.id != id);
        inner.state.plugins.push(record.clone());
        inner.save()?;
        inner.log(&id, "lifecycle", "install", "info", &format!("Installed {} from {}", record.version, record.source), 0);
        inner.load(record);
        let plugin = inner.plugins.get(&id).expect("just loaded");
        Ok(inner.info(plugin))
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        let mut inner = self.inner.lock();
        inner.update_record(id, |r| r.enabled = enabled)?;
        if !enabled {
            inner.plugins.get_mut(id).expect("exists").sandbox = None;
        }
        inner.log(id, "lifecycle", if enabled { "enable" } else { "disable" }, "info", "", 0);
        Ok(())
    }

    /// Grants capabilities the plugin declares (the consent dialog).
    pub fn grant(&self, id: &str, grant: &[Capability]) -> Result<(), String> {
        let mut inner = self.inner.lock();
        let declared = inner.plugin(id)?.loaded.as_ref().map(|l| l.validated.capabilities.clone()).unwrap_or_default();
        let keys: Vec<String> = grant.iter().filter(|c| declared.contains(c)).map(|c| c.key().to_owned()).collect();
        inner.update_record(id, |r| {
            for k in keys {
                if !r.granted.contains(&k) {
                    r.granted.push(k);
                }
            }
        })?;
        let granted = inner.plugin(id)?.record.granted.join(", ");
        inner.log(id, "lifecycle", "grant", "info", &format!("Granted: {granted}"), 0);
        Ok(())
    }

    /// Uninstalls: its files (not a dev folder), storage, settings,
    /// secrets and log.
    pub fn remove(&self, id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock();
        let plugin = inner.plugins.remove(id).ok_or_else(|| format!("No plugin “{id}” is installed."))?;
        inner.state.plugins.retain(|r| r.id != id);
        inner.save()?;
        if plugin.record.dev.is_none() {
            let _ = std::fs::remove_dir_all(inner.dir.join("installed").join(id));
        }
        inner.secrets.forget(id);
        let _ = std::fs::remove_dir_all(inner.dir.join("data").join(id));
        let _ = std::fs::remove_file(inner.dir.join("logs").join(format!("{id}.jsonl")));
        Ok(())
    }

    /// Reads a plugin (or all) from disk again.
    pub fn reload(&self, id: Option<&str>) -> Result<(), String> {
        let mut inner = self.inner.lock();
        let records: Vec<Record> = match id {
            Some(id) => vec![inner.plugin(id)?.record.clone()],
            None => inner.state.plugins.clone(),
        };
        for mut record in records {
            if let Some(dev) = &record.dev
                && let Ok(h) = hash::content_hash(Path::new(dev))
            {
                record.hash = h;
            }
            let id = record.id.clone();
            inner.load(record);
            let problem = inner.plugins.get(&id).and_then(|p| p.loaded.as_ref().err().cloned());
            inner.log(&id, "lifecycle", "reload", if problem.is_some() { "error" } else { "info" }, problem.as_deref().unwrap_or(""), 0);
        }
        Ok(())
    }

    /// Dev plugins and their folders, for watching.
    pub fn dev_folders(&self) -> Vec<(String, PathBuf)> {
        let inner = self.inner.lock();
        inner.plugins.values().filter_map(|p| Some((p.record.id.clone(), PathBuf::from(p.record.dev.as_ref()?)))).collect()
    }

    /// The newest log lines of one plugin (or every plugin), oldest first.
    pub fn logs(&self, id: Option<&str>, limit: usize) -> Vec<PluginLogEntry> {
        let inner = self.inner.lock();
        let ids: Vec<String> = match id {
            Some(id) => vec![id.to_owned()],
            None => inner.plugins.keys().cloned().collect(),
        };
        let mut out: Vec<PluginLogEntry> = ids.iter().flat_map(|id| read_log(&inner.dir, id)).collect();
        out.sort_by_key(|e| e.at_ms);
        let skip = out.len().saturating_sub(limit);
        out.split_off(skip)
    }

    /// Reads (`None`) or writes a declared setting.
    pub fn setting(&self, id: &str, key: &str, value: Option<&str>) -> Result<String, String> {
        let inner = self.inner.lock();
        let entry = {
            let plugin = inner.plugin(id)?;
            let loaded = plugin.loaded.as_ref().map_err(|e| e.clone())?;
            loaded.validated.manifest.setting(key).cloned().ok_or_else(|| format!("“{id}” has no setting “{key}”."))?
        };
        let secret = entry.kind == "secret";
        match value {
            None if secret => Ok(if inner.secrets.has(id, &format!("setting.{key}")) { "(set)".into() } else { String::new() }),
            None => Ok(inner.settings(id).get(key).cloned().unwrap_or(entry.default.clone())),
            Some(v) if secret => {
                if v.is_empty() {
                    inner.secrets.delete(id, &format!("setting.{key}"))?;
                } else {
                    inner.secrets.set(id, &format!("setting.{key}"), v)?;
                }
                inner.log(id, "audit", "settings.set", "allowed", key, 0);
                Ok(String::new())
            }
            Some(v) => {
                let v = check_setting(&entry, v).map_err(|e| format!("{key} {e}"))?;
                let mut settings = inner.settings(id);
                settings.insert(key.to_owned(), v.clone());
                let path = inner.dir.join("data").join(id).join("settings.json");
                write_atomic(&path, sonic_rs::to_string(&settings).unwrap_or_default().as_bytes())?;
                inner.log(id, "audit", "settings.set", "allowed", key, 0);
                Ok(v)
            }
        }
    }

    // ── Catalogs and the kill list ───────────────────────────────────────

    pub fn add_catalog(&self, name: &str, location: &str, public_key: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_alphanumeric() || " -_.".contains(c)) {
            return Err("Name the catalog with letters, digits, spaces, “-”, “_” or “.”.".into());
        }
        if !crate::sign::is_public_key(public_key) {
            return Err("That isn’t a minisign public key (the second line of a minisign.pub file).".into());
        }
        let location = location.trim();
        if !(location.starts_with("https://") || Path::new(location).is_absolute()) {
            return Err("Give the catalog as an https:// address or a full path on the host.".into());
        }
        let mut inner = self.inner.lock();
        inner.state.catalogs.retain(|c| c.name != name);
        inner.state.catalogs.push(CatalogConfig { name: name.into(), location: location.into(), public_key: public_key.trim().into() });
        inner.catalogs.insert(name.into(), CatalogInfo { name: name.into(), location: location.into(), ..Default::default() });
        inner.save()
    }

    pub fn remove_catalog(&self, name: &str) -> Result<(), String> {
        let mut inner = self.inner.lock();
        inner.state.catalogs.retain(|c| c.name != name);
        inner.catalogs.remove(name);
        inner.releases.remove(name);
        let _ = std::fs::remove_file(inner.dir.join("catalogs").join(format!("{}.toml", safe_name(name))));
        let _ = std::fs::remove_file(inner.dir.join("catalogs").join(format!("{}.toml.minisig", safe_name(name))));
        inner.save()
    }

    /// Catalogs to read: (name, location).
    pub fn catalog_locations(&self) -> Vec<(String, String)> {
        self.inner.lock().state.catalogs.iter().map(|c| (c.name.clone(), c.location.clone())).collect()
    }

    /// A catalog's listing and signature as read (or why reading failed).
    /// A listing that verifies replaces the cached copy.
    pub fn set_catalog(&self, name: &str, read: Result<(Vec<u8>, String), String>) {
        let mut inner = self.inner.lock();
        let Some(config) = inner.state.catalogs.iter().find(|c| c.name == name).cloned() else { return };
        let result = read.and_then(|(bytes, sig)| catalog::parse(&bytes, &sig, &config.public_key).map(|c| (c, bytes, sig)));
        let info = inner.catalogs.entry(name.to_owned()).or_default();
        info.fetched_ms = now_ms();
        match result {
            Ok((listing, bytes, sig)) => {
                info.error = None;
                info.entries = listing.plugins.iter().map(|r| r.entry()).collect();
                let dir = inner.dir.join("catalogs");
                let _ = std::fs::create_dir_all(&dir);
                let _ = write_atomic(&dir.join(format!("{}.toml", safe_name(name))), &bytes);
                let _ = write_atomic(&dir.join(format!("{}.toml.minisig", safe_name(name))), sig.as_bytes());
                inner.releases.insert(name.to_owned(), (config.location.clone(), listing));
            }
            Err(e) => info.error = Some(e),
        }
    }

    /// Applies a newly fetched kill list if it verifies; returns the ids it
    /// turned off.
    pub fn set_kill_list(&self, bytes: &[u8], signature: &str) -> Result<Vec<String>, String> {
        let mut inner = self.inner.lock();
        let key = inner.release_key.clone().ok_or("This build has no release key to check the kill list with.")?;
        let list = KillList::parse(bytes, signature, &key)?;
        write_atomic(&inner.dir.join(crate::killlist::FILE), bytes)?;
        write_atomic(&inner.dir.join(crate::killlist::SIGNATURE), signature.as_bytes())?;
        inner.kill = list;
        let killed: Vec<String> = inner
            .plugins
            .values()
            .filter(|p| inner.kill.reason(&p.record.id, &p.record.version).is_some())
            .map(|p| p.record.id.clone())
            .collect();
        for id in &killed {
            if let Some(p) = inner.plugins.get_mut(id) {
                p.sandbox = None;
            }
            inner.log(id, "lifecycle", "kill-list", "denied", "Turned off by Orbit’s kill list.", 0);
        }
        Ok(killed)
    }

    // ── Running ──────────────────────────────────────────────────────────

    /// Agent CLIs of active plugins granted `agents`.
    pub fn agents(&self) -> Vec<AgentDef> {
        let inner = self.inner.lock();
        let mut out = Vec::new();
        for p in inner.plugins.values() {
            let Ok(loaded) = &p.loaded else { continue };
            if inner.check(&p.record.id, Capability::Agents).is_err() {
                continue;
            }
            for agent in &loaded.validated.manifest.agents {
                if let Some(instance) = agent.instance() {
                    let (models, modes) = agent.catalog();
                    out.push(AgentDef { plugin: p.record.id.clone(), instance, models, modes, install: agent.install.clone() });
                }
            }
        }
        out
    }

    /// The command line of a terminal agent, checked like any call.
    pub fn terminal_agent(&self, id: &str, agent: &str) -> Result<String, String> {
        let inner = self.inner.lock();
        let checked = inner.check(id, Capability::Agents);
        inner.log(id, "audit", "agents.terminal", if checked.is_ok() { "allowed" } else { "denied" }, agent, 0);
        checked?;
        let loaded = inner.plugin(id)?.loaded.as_ref().map_err(|e| e.clone())?;
        let def = loaded
            .validated
            .manifest
            .agents
            .iter()
            .find(|a| a.id == agent && a.protocol == "terminal")
            .ok_or_else(|| format!("“{id}” has no terminal agent “{agent}”."))?;
        Ok(def.command_line())
    }

    /// Whether an active plugin registers `hook`.
    pub fn wants(&self, hook: &str) -> bool {
        let inner = self.inner.lock();
        inner.hooked(hook).next().is_some()
    }

    /// Whether any active plugin may read threads (the engine then keeps
    /// [`Self::set_threads`] current).
    pub fn wants_threads(&self) -> bool {
        let inner = self.inner.lock();
        inner.plugins.keys().any(|id| inner.check(id, Capability::ThreadsRead).is_ok())
    }

    /// The thread list `threads.list` answers with (JSON array).
    pub fn set_threads(&self, json: String) {
        self.inner.lock().threads = Arc::new(json);
    }

    /// `agent.prompt.before`: the first plugin that blocks the prompt.
    pub fn prompt_before(&self, ctx: &HookContext, prompt: &str) -> Option<Denial> {
        let input = format!(
            "{{\"hook\":\"agent.prompt.before\",\"thread\":{},\"prompt\":{}}}",
            thread_json(ctx),
            json_str(prompt)
        );
        self.run_gate("agent.prompt.before", &input)
    }

    /// `agent.tool.before`: the first plugin that blocks the tool call.
    pub fn tool_before(&self, ctx: &HookContext, kind: &RequestKind) -> Option<Denial> {
        if matches!(kind, RequestKind::Question { .. } | RequestKind::Plan { .. }) {
            return None;
        }
        let input = format!("{{\"hook\":\"agent.tool.before\",\"thread\":{},\"tool\":{}}}", thread_json(ctx), tool_json(kind));
        self.run_gate("agent.tool.before", &input)
    }

    /// `agent.tool.after`: observers only.
    pub fn tool_after(&self, ctx: &HookContext, tool: &ToolCall, output: &str, is_error: bool) {
        if !self.wants("agent.tool.after") {
            return;
        }
        let cut = output.char_indices().nth(TOOL_OUTPUT).map_or(output, |(i, _)| &output[..i]);
        let input = format!(
            "{{\"hook\":\"agent.tool.after\",\"thread\":{},\"tool\":{{\"name\":{},\"title\":{},\"input\":{}}},\"output\":{},\"error\":{is_error}}}",
            thread_json(ctx),
            json_str(&tool.name),
            json_str(&tool.title),
            json_str(&tool.input),
            json_str(cut)
        );
        self.run_observers("agent.tool.after", &input);
    }

    /// `thread.finished`: observers only.
    pub fn thread_finished(&self, ctx: &HookContext, turn: u32, outcome: &str) -> Vec<EngineEffect> {
        if !self.wants("thread.finished") {
            return Vec::new();
        }
        let input = format!(
            "{{\"hook\":\"thread.finished\",\"thread\":{},\"turn\":{turn},\"outcome\":{}}}",
            thread_json(ctx),
            json_str(outcome)
        );
        self.run_observers("thread.finished", &input)
    }

    fn run_gate(&self, hook: &str, input: &str) -> Option<Denial> {
        let mut inner = self.inner.lock();
        let ids: Vec<String> = inner.hooked(hook).collect();
        for id in ids {
            let (result, mut effects) = inner.invoke(&id, wasm::ENTRY_HOOK, hook, input);
            inner.pending.append(&mut effects);
            let plugin = inner.plugins.get(&id).expect("hooked plugins exist");
            let loaded = plugin.loaded.as_ref().expect("hooked plugins load");
            let name = loaded.validated.manifest.name.clone();
            match result {
                Ok(answer) => {
                    let reason = answer.as_deref().and_then(deny_reason);
                    if let Some(reason) = reason {
                        inner.log(&id, "hook", hook, "denied", &reason, 0);
                        return Some(Denial { plugin: id, name, reason });
                    }
                }
                Err(e) if loaded.validated.on_error == OnError::Deny => {
                    return Some(Denial {
                        plugin: id,
                        name,
                        reason: format!("it couldn’t decide ({e}), and it blocks when that happens"),
                    });
                }
                Err(_) => {}
            }
        }
        None
    }

    fn run_observers(&self, hook: &str, input: &str) -> Vec<EngineEffect> {
        let mut inner = self.inner.lock();
        let ids: Vec<String> = inner.hooked(hook).collect();
        let mut effects = Vec::new();
        for id in ids {
            let (_, mut fx) = inner.invoke(&id, wasm::ENTRY_HOOK, hook, input);
            effects.append(&mut fx);
        }
        effects
    }

    /// What hooks asked for since the last call (storage changes,
    /// notifications, threads), for the engine to do.
    pub fn take_effects(&self) -> Vec<EngineEffect> {
        std::mem::take(&mut self.inner.lock().pending)
    }

    /// Runs a palette command of a WebAssembly plugin.
    pub fn run_command(&self, id: &str, command: &str) -> Result<Outcome, String> {
        let mut inner = self.inner.lock();
        let loaded = inner.plugin(id)?.loaded.as_ref().map_err(|e| e.clone())?;
        let entry = loaded.validated.manifest.commands.iter().find(|c| c.id == command).ok_or_else(|| format!("“{id}” has no command “{command}”."))?;
        if !entry.wasm {
            return Err("That command runs in the app, not on the host.".into());
        }
        inner.check_active(id)?;
        let input = format!("{{\"command\":{}}}", json_str(command));
        let (result, effects) = inner.invoke(id, wasm::ENTRY_COMMAND, &format!("command {command}"), &input);
        let answer = result.map_err(|e| format!("The command failed: {e}"))?;
        Ok(Outcome { message: answer.as_deref().map(message_of).unwrap_or_default(), effects })
    }

    /// Runs a panel action: found again in the installed tree, checked,
    /// applied to storage, then any effect.
    pub fn panel_action(&self, id: &str, panel_id: &str, action: &str, inputs: &[(String, String)]) -> Result<Outcome, String> {
        let mut inner = self.inner.lock();
        inner.check_active(id)?;
        let tree = {
            let loaded = inner.plugin(id)?.loaded.as_ref().map_err(|e| e.clone())?;
            loaded.panel(panel_id).cloned().ok_or_else(|| format!("“{id}” has no panel “{panel_id}”."))?
        };
        let mut storage = inner.plugins.get(id).expect("checked").storage.clone();
        let applied = panel::apply(&tree, action, inputs, &mut storage);
        let (method, effect) = match applied {
            Ok(done) => done,
            Err(e) => {
                inner.log(id, "audit", &format!("panel {panel_id} {action}"), "error", &e, 0);
                return Err(e);
            }
        };
        let needed = panel::METHODS.iter().find(|m| m.0 == method).and_then(|m| m.2);
        if let Some(cap) = needed
            && let Err(e) = inner.check(id, cap)
        {
            inner.log(id, "audit", &method, "denied", &e, 0);
            return Err(e);
        }
        inner.log(id, "audit", &method, "allowed", &format!("panel {panel_id}"), 0);
        let mut outcome = Outcome::default();
        let changed = {
            let plugin = inner.plugins.get_mut(id).expect("checked");
            let changed = plugin.storage != storage;
            plugin.storage = storage;
            changed
        };
        if changed {
            inner.save_storage(id)?;
            outcome.effects.push(EngineEffect::Storage { plugin: id.to_owned(), json: inner.plugins[id].storage.to_string() });
        }
        match effect {
            Effect::None => {}
            Effect::Notify { title, body } => outcome.effects.push(EngineEffect::Notify { plugin: id.to_owned(), title, body }),
            Effect::CreateThread { prompt } => outcome.effects.push(EngineEffect::CreateThread { plugin: id.to_owned(), prompt }),
            Effect::Wasm { name, input } => {
                let input = format!("{{\"action\":{},\"context\":{}}}", json_str(&name), input);
                let (result, mut effects) = inner.invoke(id, wasm::ENTRY_ACTION, &format!("action {name}"), &input);
                outcome.effects.append(&mut effects);
                let answer = result.map_err(|e| format!("The action failed: {e}"))?;
                outcome.message = answer.as_deref().map(message_of).unwrap_or_default();
            }
        }
        Ok(outcome)
    }
}

impl Inner {
    fn plugin(&self, id: &str) -> Result<&Plugin, String> {
        self.plugins.get(id).ok_or_else(|| format!("No plugin “{id}” is installed."))
    }

    fn root(&self, record: &Record) -> PathBuf {
        match &record.dev {
            Some(dev) => PathBuf::from(dev),
            None => self.dir.join("installed").join(&record.id),
        }
    }

    fn load(&mut self, record: Record) {
        let root = self.root(&record);
        let loaded = package::load(&root).map_err(|p| p.join("\n")).and_then(|l| {
            if l.id() != record.id {
                Err(format!("Its manifest now says id “{}”.", l.id()))
            } else if record.dev.is_none() && !hash::matches(&record.hash, &l.hash) {
                Err("Its files changed after it was installed; reinstall it.".into())
            } else {
                Ok(l)
            }
        });
        let storage = std::fs::read_to_string(self.dir.join("data").join(&record.id).join("storage.json"))
            .ok()
            .and_then(|t| sonic_rs::from_str::<Value>(&t).ok())
            .filter(|v| v.is_object())
            .unwrap_or_else(Value::new_object);
        if let Err(e) = &loaded {
            log::warn!("plugins: {} doesn’t load: {e}", record.id);
        }
        self.plugins.insert(record.id.clone(), Plugin { record, loaded, storage, sandbox: None });
    }

    fn update_record(&mut self, id: &str, f: impl FnOnce(&mut Record)) -> Result<(), String> {
        let plugin = self.plugins.get_mut(id).ok_or_else(|| format!("No plugin “{id}” is installed."))?;
        f(&mut plugin.record);
        let record = plugin.record.clone();
        if let Some(r) = self.state.plugins.iter_mut().find(|r| r.id == id) {
            *r = record;
        }
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let text = toml::to_string(&self.state).map_err(|e| e.to_string())?;
        write_atomic(&self.dir.join(STATE), format!("# Orbit's installed plugins. Edit with `orbit plugin`.\n{text}").as_bytes())
    }

    fn save_storage(&self, id: &str) -> Result<(), String> {
        let plugin = self.plugin(id)?;
        write_atomic(&self.dir.join("data").join(id).join("storage.json"), plugin.storage.to_string().as_bytes())
    }

    fn settings(&self, id: &str) -> BTreeMap<String, String> {
        let mut values = self
            .plugins
            .get(id)
            .and_then(|p| p.loaded.as_ref().ok())
            .map(|l| crate::manifest::setting_defaults(&l.validated.manifest))
            .unwrap_or_default();
        let saved: BTreeMap<String, String> = std::fs::read_to_string(self.dir.join("data").join(id).join("settings.json"))
            .ok()
            .and_then(|t| sonic_rs::from_str(&t).ok())
            .unwrap_or_default();
        for (k, v) in saved {
            if values.contains_key(&k) {
                values.insert(k, v);
            }
        }
        values
    }

    fn status(&self, plugin: &Plugin) -> PluginStatus {
        let loaded = match &plugin.loaded {
            Ok(l) => l,
            Err(e) => return PluginStatus::Broken { error: e.clone() },
        };
        if let Some(reason) = self.kill.reason(&plugin.record.id, &plugin.record.version) {
            return PluginStatus::Killed { reason: reason.to_owned() };
        }
        if let Some(requires) = &loaded.validated.incompatible {
            return PluginStatus::Incompatible { requires: requires.clone() };
        }
        if !plugin.record.enabled {
            return PluginStatus::Disabled;
        }
        if loaded.validated.capabilities.iter().any(|c| !plugin.record.granted.iter().any(|g| g == c.key())) {
            return PluginStatus::NeedsConsent;
        }
        PluginStatus::Active
    }

    fn check_active(&self, id: &str) -> Result<(), String> {
        let plugin = self.plugin(id)?;
        match self.status(plugin) {
            PluginStatus::Active => Ok(()),
            other => Err(format!("“{id}” isn’t running ({}).", other.label().to_lowercase())),
        }
    }

    /// The per-call check: active, and `capability` declared and granted.
    fn check(&self, id: &str, capability: Capability) -> Result<(), String> {
        self.check_active(id)?;
        let plugin = self.plugin(id)?;
        let declared = plugin.loaded.as_ref().is_ok_and(|l| l.validated.capabilities.contains(&capability));
        if !declared || !plugin.record.granted.iter().any(|g| g == capability.key()) {
            return Err(format!("“{id}” hasn’t been granted “{}”.", capability.key()));
        }
        Ok(())
    }

    /// Active plugins registering `hook`, in id order.
    fn hooked<'a>(&'a self, hook: &'a str) -> impl Iterator<Item = String> + 'a {
        self.plugins.values().filter_map(move |p| {
            let loaded = p.loaded.as_ref().ok()?;
            let cap = crate::manifest::HOOKS.iter().find(|(h, _)| *h == hook)?.1;
            (loaded.validated.hooks.iter().any(|h| h == hook) && self.check(&p.record.id, cap).is_ok()).then(|| p.record.id.clone())
        })
    }

    fn info(&self, plugin: &Plugin) -> PluginInfo {
        let status = self.status(plugin);
        let granted: Vec<Capability> = plugin.record.granted.iter().filter_map(|g| Capability::from_key(g)).collect();
        match &plugin.loaded {
            Ok(loaded) => {
                let m = &loaded.validated.manifest;
                // An inactive plugin contributes nothing; its agents' command
                // lines stay listed for the consent dialog.
                let mut ui = if status.is_active() { loaded.ui.clone() } else { PluginUi { agents: loaded.ui.agents.clone(), ..Default::default() } };
                if status.is_active() {
                    ui.storage = plugin.storage.to_string();
                    if !granted.contains(&Capability::Agents) {
                        ui.agents.clear();
                    }
                }
                PluginInfo {
                    id: plugin.record.id.clone(),
                    name: m.name.clone(),
                    version: m.version.clone(),
                    description: m.description.clone(),
                    publisher: m.publisher.clone(),
                    homepage: m.homepage.clone(),
                    tier: loaded.validated.tier,
                    source: plugin.record.source.clone(),
                    dev: plugin.record.dev.is_some(),
                    status,
                    capabilities: loaded.validated.capabilities.clone(),
                    granted,
                    hash: loaded.hash.clone(),
                    installed_ms: plugin.record.installed_ms,
                    summary: loaded.summary(),
                    settings: self.setting_infos(&plugin.record.id, m),
                    ui,
                }
            }
            Err(_) => PluginInfo {
                id: plugin.record.id.clone(),
                name: plugin.record.id.clone(),
                version: plugin.record.version.clone(),
                source: plugin.record.source.clone(),
                dev: plugin.record.dev.is_some(),
                status,
                granted,
                hash: plugin.record.hash.clone(),
                installed_ms: plugin.record.installed_ms,
                ..Default::default()
            },
        }
    }

    fn setting_infos(&self, id: &str, manifest: &crate::Manifest) -> Vec<PluginSetting> {
        if manifest.settings.is_empty() {
            return Vec::new();
        }
        let values = self.settings(id);
        manifest
            .settings
            .iter()
            .map(|s| {
                let secret = s.kind == "secret";
                PluginSetting {
                    key: s.key.clone(),
                    label: s.label.clone(),
                    description: s.description.clone(),
                    kind: s.kind.clone(),
                    choices: s.choices.clone(),
                    value: if secret { String::new() } else { values.get(&s.key).cloned().unwrap_or_default() },
                    set: secret && self.secrets.has(id, &format!("setting.{}", s.key)),
                }
            })
            .collect()
    }

    /// Calls into a plugin's module with a fresh API object holding what
    /// it was granted right now; records the call and applies what it did.
    fn invoke(&mut self, id: &str, entry: &str, call: &str, input: &str) -> (Result<Option<String>, CallError>, Vec<EngineEffect>) {
        let started = std::time::Instant::now();
        let granted: Vec<Capability> = Capability::ALL.into_iter().filter(|c| self.check(id, *c).is_ok()).collect();
        let Some(plugin) = self.plugins.get_mut(id) else { return (Err(CallError::Missing(id.to_owned())), Vec::new()) };
        let Ok(loaded) = &plugin.loaded else { return (Err(CallError::Missing(entry.to_owned())), Vec::new()) };
        let Some(bytes) = loaded.wasm.clone() else { return (Err(CallError::Missing(entry.to_owned())), Vec::new()) };
        let limits = wasm::Limits {
            fuel: loaded.validated.fuel(),
            memory_bytes: loaded.validated.memory_bytes(),
            timeout: Duration::from_millis(u64::from(loaded.validated.timeout_ms())),
        };
        if plugin.sandbox.is_none() {
            match Sandbox::new(&bytes, limits) {
                Ok(s) => plugin.sandbox = Some(s),
                Err(e) => {
                    self.log(id, "hook", call, "error", &format!("The module couldn’t start: {e}"), 0);
                    return (Err(CallError::Trap(e)), Vec::new());
                }
            }
        }
        let settings = self.settings(id);
        let plugin = self.plugins.get_mut(id).expect("present");
        let api = Box::new(PluginApi {
            id: id.to_owned(),
            granted,
            storage: plugin.storage.clone(),
            storage_dirty: false,
            settings,
            secrets: self.secrets.clone(),
            threads: self.threads.clone(),
            effects: Vec::new(),
            records: Vec::new(),
        });
        let sandbox = plugin.sandbox.as_mut().expect("created above");
        let (api, result) = sandbox.invoke(entry, input, api);
        let micros = started.elapsed().as_micros() as u64;
        // The API object comes back as the concrete type it went in as.
        let api: Box<PluginApi> = match downcast(api) {
            Some(api) => api,
            None => return (result, Vec::new()),
        };
        let PluginApi { storage, storage_dirty, mut effects, records, .. } = *api;
        for (kind, method, outcome, detail) in records {
            self.log(id, kind, &method, outcome, &detail, 0);
        }
        if storage_dirty {
            if let Some(p) = self.plugins.get_mut(id) {
                p.storage = storage;
            }
            if let Err(e) = self.save_storage(id) {
                log::warn!("plugins: couldn’t save {id}’s storage: {e}");
            }
            effects.push(EngineEffect::Storage { plugin: id.to_owned(), json: self.plugins[id].storage.to_string() });
        }
        let (outcome, detail) = match &result {
            Ok(Some(answer)) => ("allowed", answer.chars().take(200).collect::<String>()),
            Ok(None) => ("allowed", String::new()),
            Err(e) => ("error", e.to_string()),
        };
        self.log(id, "hook", call, outcome, &detail, micros);
        (result, effects)
    }

    /// Appends to a plugin's log, trimming it when it grows past its cap.
    fn log(&self, id: &str, kind: &str, call: &str, outcome: &str, detail: &str, micros: u64) {
        let entry = PluginLogEntry {
            at_ms: now_ms(),
            plugin: id.to_owned(),
            kind: kind.to_owned(),
            call: call.to_owned(),
            outcome: outcome.to_owned(),
            detail: detail.chars().take(2000).collect(),
            micros,
        };
        let dir = self.dir.join("logs");
        let path = dir.join(format!("{id}.jsonl"));
        let line = LogLine::from(&entry);
        let Ok(mut text) = sonic_rs::to_string(&line) else { return };
        text.push('\n');
        let _ = std::fs::create_dir_all(&dir);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_BYTES)
            && let Ok(old) = std::fs::read_to_string(&path)
        {
            let keep = old.len() / 2;
            let cut = old[keep..].find('\n').map_or(old.len(), |i| keep + i + 1);
            let _ = write_atomic(&path, &old.as_bytes()[cut..]);
        }
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = file.write_all(text.as_bytes());
        }
    }

    fn read_cached_catalog(&mut self, config: &CatalogConfig) {
        let dir = self.dir.join("catalogs");
        let (Ok(bytes), Ok(sig)) = (
            std::fs::read(dir.join(format!("{}.toml", safe_name(&config.name)))),
            std::fs::read_to_string(dir.join(format!("{}.toml.minisig", safe_name(&config.name)))),
        ) else {
            return;
        };
        if let Ok(listing) = catalog::parse(&bytes, &sig, &config.public_key)
            && let Some(info) = self.catalogs.get_mut(&config.name)
        {
            info.entries = listing.plugins.iter().map(|r| r.entry()).collect();
            self.releases.insert(config.name.clone(), (config.location.clone(), listing));
        }
    }
}

/// Recovers the concrete API object after a call.
fn downcast(api: Box<dyn HostApi>) -> Option<Box<PluginApi>> {
    let any: Box<dyn std::any::Any> = api;
    any.downcast::<PluginApi>().ok()
}

/// What a module can reach, for one call.
struct PluginApi {
    id: String,
    /// Capabilities that passed the check when the call began.
    granted: Vec<Capability>,
    storage: Value,
    storage_dirty: bool,
    settings: BTreeMap<String, String>,
    secrets: Arc<Secrets>,
    threads: Arc<String>,
    effects: Vec<EngineEffect>,
    /// (kind, method, outcome, detail) to log.
    records: Vec<(&'static str, String, &'static str, String)>,
}

impl PluginApi {
    fn need(&mut self, method: &str, capability: Capability) -> Result<(), ApiError> {
        if self.granted.contains(&capability) {
            Ok(())
        } else {
            self.records.push(("audit", method.to_owned(), "denied", format!("needs {}", capability.key())));
            Err(ApiError::new("capability_denied", format!("{method} needs the “{}” capability.", capability.key())))
        }
    }
}

fn param<'a>(params: &'a Value, name: &str) -> Result<&'a str, ApiError> {
    params.get(name).and_then(|v| v.as_str()).ok_or_else(|| ApiError::new("invalid_params", format!("params.{name} must be a string")))
}

impl HostApi for PluginApi {
    fn call(&mut self, method: &str, params: &str) -> Result<String, ApiError> {
        let params: Value = if params.trim().is_empty() {
            Value::new_object()
        } else {
            sonic_rs::from_str(params).map_err(|_| ApiError::new("invalid_params", "params must be JSON"))?
        };
        let result = match method {
            "storage.get" => {
                self.need(method, Capability::Storage)?;
                let key = param(&params, "key")?;
                Ok(template::lookup(&self.storage, key).map(|v| v.to_string()).unwrap_or_else(|| "null".into()))
            }
            "storage.set" => {
                self.need(method, Capability::Storage)?;
                let key = param(&params, "key")?.to_owned();
                let value = params.get("value").cloned().unwrap_or_default();
                let mut next = self.storage.clone();
                panel::set_path(&mut next, &key, value).map_err(|e| ApiError::new("invalid_params", e))?;
                panel::check_size(&next).map_err(|e| ApiError::new("too_large", e))?;
                self.storage = next;
                self.storage_dirty = true;
                Ok("null".into())
            }
            "storage.delete" => {
                self.need(method, Capability::Storage)?;
                let key = param(&params, "key")?.to_owned();
                if let Some(obj) = sonic_rs::JsonValueMutTrait::as_object_mut(&mut self.storage) {
                    obj.remove(&key);
                }
                self.storage_dirty = true;
                Ok("null".into())
            }
            "storage.keys" => {
                self.need(method, Capability::Storage)?;
                let keys: Vec<String> = self.storage.as_object().map(|o| o.iter().map(|(k, _)| k.to_owned()).collect()).unwrap_or_default();
                Ok(sonic_rs::to_string(&keys).unwrap_or_default())
            }
            "secrets.get" | "secrets.set" | "secrets.delete" => {
                self.need(method, Capability::Secrets)?;
                let key = param(&params, "key")?;
                if key.starts_with("setting.") {
                    return Err(ApiError::new("invalid_params", "Use settings.get for secret settings."));
                }
                let r = match method {
                    "secrets.get" => self.secrets.get(&self.id, key).map(|v| v.map_or("null".into(), |v| json_str(&v))),
                    "secrets.set" => self.secrets.set(&self.id, key, param(&params, "value")?).map(|_| "null".into()),
                    _ => self.secrets.delete(&self.id, key).map(|_| "null".into()),
                };
                r.map_err(|e| ApiError::new("unavailable", e))
            }
            "settings.get" => {
                let key = param(&params, "key")?;
                match self.settings.get(key) {
                    Some(v) => Ok(json_str(v)),
                    None if self.granted.contains(&Capability::Secrets) => self
                        .secrets
                        .get(&self.id, &format!("setting.{key}"))
                        .map(|v| v.map_or("null".into(), |v| json_str(&v)))
                        .map_err(|e| ApiError::new("unavailable", e)),
                    None => Ok("null".into()),
                }
            }
            "notifications.show" => {
                self.need(method, Capability::Notifications)?;
                let title = param(&params, "title")?.chars().take(120).collect();
                let body = params.get("body").and_then(|b| b.as_str()).unwrap_or_default().chars().take(1000).collect();
                self.effects.push(EngineEffect::Notify { plugin: self.id.clone(), title, body });
                Ok("null".into())
            }
            "threads.list" => {
                self.need(method, Capability::ThreadsRead)?;
                Ok(self.threads.as_str().to_owned())
            }
            "threads.create" => {
                self.need(method, Capability::ThreadsCreate)?;
                let prompt = param(&params, "prompt")?.to_owned();
                if prompt.trim().is_empty() || prompt.len() > 64 * 1024 {
                    return Err(ApiError::new("invalid_params", "prompt must be 1 byte to 64 KiB"));
                }
                if self.effects.iter().filter(|e| matches!(e, EngineEffect::CreateThread { .. })).count() >= 3 {
                    return Err(ApiError::new("rate_limited", "At most 3 threads per call."));
                }
                self.effects.push(EngineEffect::CreateThread { plugin: self.id.clone(), prompt });
                Ok("null".into())
            }
            _ => Err(ApiError::new("not_found", format!("No host method “{method}”."))),
        };
        let outcome = if result.is_ok() { "allowed" } else { "error" };
        self.records.push(("audit", method.to_owned(), outcome, result.as_ref().err().map(|e| e.message.clone()).unwrap_or_default()));
        result
    }

    fn log(&mut self, line: &str) {
        self.records.push(("log", "log".into(), "info", line.to_owned()));
    }
}

/// `{"deny": "reason"}` → the reason.
fn deny_reason(answer: &str) -> Option<String> {
    let value: Value = sonic_rs::from_str(answer).ok()?;
    let reason = value.get("deny")?;
    let text = reason.as_str().map(str::to_owned).unwrap_or_else(|| "no reason given".into());
    Some(if text.trim().is_empty() { "no reason given".into() } else { text.chars().take(500).collect() })
}

/// `{"message": "…"}` → the message; anything else as is.
fn message_of(answer: &str) -> String {
    sonic_rs::from_str::<Value>(answer)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_else(|| answer.chars().take(500).collect())
}

fn thread_json(ctx: &HookContext) -> String {
    format!("{{\"id\":{},\"cwd\":{}}}", json_str(&ctx.thread), json_str(&ctx.cwd))
}

fn tool_json(kind: &RequestKind) -> String {
    match kind {
        RequestKind::Command { command, cwd, reason } => {
            format!("{{\"kind\":\"command\",\"command\":{},\"cwd\":{},\"reason\":{}}}", json_str(command), json_str(cwd), json_str(reason))
        }
        RequestKind::FileChange { path, diff } => format!("{{\"kind\":\"file\",\"path\":{},\"diff\":{}}}", json_str(path), json_str(diff)),
        RequestKind::Tool { name, input } => format!("{{\"kind\":\"tool\",\"name\":{},\"input\":{}}}", json_str(name), json_str(input)),
        RequestKind::Question { question, .. } => format!("{{\"kind\":\"question\",\"question\":{}}}", json_str(question)),
        RequestKind::Plan { plan } => format!("{{\"kind\":\"plan\",\"plan\":{}}}", json_str(plan)),
    }
}

fn json_str(s: &str) -> String {
    sonic_rs::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn safe_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn load_kill_list(dir: &Path, key: &str) -> KillList {
    let (Ok(bytes), Ok(sig)) = (std::fs::read(dir.join(crate::killlist::FILE)), std::fs::read_to_string(dir.join(crate::killlist::SIGNATURE))) else {
        return KillList::default();
    };
    KillList::parse(&bytes, &sig, key).unwrap_or_else(|e| {
        log::warn!("plugins: ignoring the kill list: {e}");
        KillList::default()
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension(format!("{}.tmp", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Copies a plugin folder (its files as [`hash::files`] lists them).
fn copy_plugin(from: &Path, to: &Path) -> Result<(), String> {
    for (rel, path) in hash::files(from)? {
        let dest = to.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::copy(&path, &dest).map_err(|e| format!("Couldn’t copy {rel}: {e}"))?;
    }
    if !to.exists() {
        std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn placeholder_loaded() -> Loaded {
    Loaded {
        validated: crate::manifest::Validated {
            manifest: Default::default(),
            version: semver::Version::new(0, 0, 0),
            capabilities: Vec::new(),
            tier: PluginTier::Data,
            hooks: Vec::new(),
            on_error: OnError::Allow,
            incompatible: None,
        },
        root: PathBuf::new(),
        hash: String::new(),
        ui: PluginUi::default(),
        panels: Vec::new(),
        wasm: None,
        warnings: Vec::new(),
    }
}

/// One log line on disk.
#[derive(Deserialize, Serialize)]
struct LogLine {
    at: u64,
    kind: String,
    call: String,
    outcome: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    detail: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    us: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl From<&PluginLogEntry> for LogLine {
    fn from(e: &PluginLogEntry) -> Self {
        Self { at: e.at_ms, kind: e.kind.clone(), call: e.call.clone(), outcome: e.outcome.clone(), detail: e.detail.clone(), us: e.micros }
    }
}

fn read_log(dir: &Path, id: &str) -> Vec<PluginLogEntry> {
    let Ok(text) = std::fs::read_to_string(dir.join("logs").join(format!("{id}.jsonl"))) else { return Vec::new() };
    text.lines()
        .filter_map(|l| sonic_rs::from_str::<LogLine>(l).ok())
        .map(|l| PluginLogEntry {
            at_ms: l.at,
            plugin: id.to_owned(),
            kind: l.kind,
            call: l.call,
            outcome: l.outcome,
            detail: l.detail,
            micros: l.us,
        })
        .collect()
}
