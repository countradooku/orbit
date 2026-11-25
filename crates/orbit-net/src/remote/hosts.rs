//! The client's hosts: this computer, SSH hosts, WSL distros and direct
//! `orbitd --listen` addresses. Kept in `<data>/hosts.json` on the client
//! (owner-only: a direct host's token is in it), never sent to an engine.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The file in the client's data folder.
pub const FILE: &str = "hosts.json";
/// The id of this computer's own host.
pub const LOCAL_ID: &str = "local";

/// One host the window can show.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEntry {
    /// Stable and unique in the list: `local`, `ssh:dev@box`, `wsl:Ubuntu`,
    /// `url:ws://…`.
    pub id: String,
    /// What the user calls it; empty shows [`HostEntry::label`]'s default.
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub kind: HostKind,
    /// Where `orbitd` is on the host when not in `~/.orbit/bin` or on PATH.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub orbitd: String,
    /// The host's data folder when not its default.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub data: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum HostKind {
    /// This computer's background host.
    Local,
    /// `target` is what `ssh` takes: an alias from `~/.ssh/config`, `host`
    /// or `user@host`. The port and identity file are optional; the user's
    /// SSH config, agent, ProxyJump and known_hosts apply as for `ssh`.
    Ssh {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        identity: String,
    },
    /// A WSL distro on this Windows computer.
    Wsl { distro: String },
    /// An `orbitd` listening at `url` (`ws://host:port/`), with its token.
    Url { url: String, token: String },
}

impl HostEntry {
    pub fn local() -> Self {
        Self { id: LOCAL_ID.into(), name: String::new(), kind: HostKind::Local, orbitd: String::new(), data: String::new() }
    }

    pub fn ssh(target: &str, port: Option<u16>, identity: &str) -> Self {
        let target = target.trim().to_owned();
        let id = match port {
            Some(port) => format!("ssh:{target}:{port}"),
            None => format!("ssh:{target}"),
        };
        Self {
            id,
            name: String::new(),
            kind: HostKind::Ssh { target, port, identity: identity.trim().to_owned() },
            orbitd: String::new(),
            data: String::new(),
        }
    }

    pub fn wsl(distro: &str) -> Self {
        let distro = distro.trim().to_owned();
        Self {
            id: format!("wsl:{distro}"),
            name: String::new(),
            kind: HostKind::Wsl { distro },
            orbitd: String::new(),
            data: String::new(),
        }
    }

    pub fn url(url: &str, token: &str) -> Self {
        let url = url.trim().to_owned();
        Self {
            id: format!("url:{url}"),
            name: String::new(),
            kind: HostKind::Url { url, token: token.trim().to_owned() },
            orbitd: String::new(),
            data: String::new(),
        }
    }

    /// Parses what a user types for an SSH host: `user@host`, `host:2222`,
    /// `ssh://user@host:2222`, or an alias.
    pub fn parse_ssh(input: &str, identity: &str) -> Result<Self, String> {
        let text = input.trim().trim_start_matches("ssh://").trim_end_matches('/');
        if text.is_empty() {
            return Err("Enter a host, such as dev@box or an alias from your SSH config.".into());
        }
        if text.chars().any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | ';' | '|' | '&' | '`' | '$')) {
            return Err("A host is written user@host or user@host:port.".into());
        }
        if text.starts_with('-') {
            return Err("A host can’t start with “-”.".into());
        }
        // `host:port`, but not an IPv6 address.
        let (target, port) = match text.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') || host.ends_with(']') => {
                let port: u16 = port.parse().map_err(|_| format!("“{port}” is not a port number."))?;
                (host.trim_start_matches('[').trim_end_matches(']').to_owned(), Some(port))
            }
            _ => (text.to_owned(), None),
        };
        Ok(Self::ssh(&target, port, identity))
    }

    pub fn is_local(&self) -> bool {
        matches!(self.kind, HostKind::Local)
    }

    /// The name shown: the user's, else a default from what it is.
    pub fn label(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.trim().to_owned();
        }
        match &self.kind {
            HostKind::Local => "This computer".into(),
            HostKind::Ssh { target, .. } => target.clone(),
            HostKind::Wsl { distro } => format!("{distro} (WSL)"),
            HostKind::Url { url, .. } => url.trim_start_matches("ws://").trim_start_matches("wss://").trim_end_matches('/').to_owned(),
        }
    }

    /// "SSH", "WSL", "Direct" or "Local".
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            HostKind::Local => "Local",
            HostKind::Ssh { .. } => "SSH",
            HostKind::Wsl { .. } => "WSL",
            HostKind::Url { .. } => "Direct",
        }
    }

    /// The SSH name VS Code's Remote-SSH and others take
    /// (`code --remote ssh-remote+<this>`).
    pub fn ssh_remote(&self) -> Option<String> {
        match &self.kind {
            HostKind::Ssh { target, port: None, .. } => Some(target.clone()),
            // Remote-SSH reads ports from the SSH config only; an explicit
            // port is passed as `host:port`, which it also accepts.
            HostKind::Ssh { target, port: Some(port), .. } => Some(format!("{target}:{port}")),
            _ => None,
        }
    }
}

/// `hosts.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostsFile {
    #[serde(default = "version")]
    pub version: u32,
    /// In the order shown; this computer is always first.
    #[serde(default)]
    pub hosts: Vec<HostEntry>,
    /// The host the window showed last (an id from `hosts`).
    #[serde(default)]
    pub active: String,
}

fn version() -> u32 {
    1
}

impl Default for HostsFile {
    fn default() -> Self {
        Self { version: 1, hosts: vec![HostEntry::local()], active: LOCAL_ID.into() }
    }
}

impl HostsFile {
    /// Reads the list; a missing or unreadable file is the default (this
    /// computer only). Always valid: this computer first, ids unique, the
    /// active one in the list.
    pub fn load(data_dir: &Path) -> Self {
        let file = std::fs::read_to_string(data_dir.join(FILE))
            .ok()
            .and_then(|text| serde_json::from_str::<HostsFile>(&text).inspect_err(|e| log::warn!("{FILE}: {e}")).ok())
            .unwrap_or_default();
        file.normalized()
    }

    /// The list made valid (see [`Self::load`]).
    pub fn normalized(mut self) -> Self {
        let local = HostEntry { kind: HostKind::Local, id: LOCAL_ID.into(), ..self.local_entry() };
        let mut seen = std::collections::HashSet::new();
        self.hosts.retain(|h| !h.is_local() && h.id != LOCAL_ID && seen.insert(h.id.clone()));
        self.hosts.insert(0, local);
        if !self.hosts.iter().any(|h| h.id == self.active) {
            self.active = LOCAL_ID.into();
        }
        self
    }

    /// The saved entry for this computer (its name), or a fresh one.
    fn local_entry(&self) -> HostEntry {
        self.hosts.iter().find(|h| h.is_local()).cloned().unwrap_or_else(HostEntry::local)
    }

    /// Saves atomically, readable by the owner only (a direct host's token
    /// is in it).
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        crate::endpoint::write_private(&data_dir.join(FILE), &text)
    }

    pub fn get(&self, id: &str) -> Option<&HostEntry> {
        self.hosts.iter().find(|h| h.id == id)
    }

    /// Adds `entry`, or replaces the entry with its id. Returns its id.
    pub fn upsert(&mut self, entry: HostEntry) -> String {
        let id = entry.id.clone();
        match self.hosts.iter_mut().find(|h| h.id == id) {
            Some(existing) => *existing = entry,
            None => self.hosts.push(entry),
        }
        id
    }

    /// Removes a host (never this computer). The window falls back to
    /// this computer when it showed it.
    pub fn remove(&mut self, id: &str) -> bool {
        if id == LOCAL_ID {
            return false;
        }
        let before = self.hosts.len();
        self.hosts.retain(|h| h.id != id);
        if self.active == id {
            self.active = LOCAL_ID.into();
        }
        self.hosts.len() != before
    }

    /// Takes over the single remote connection of older versions
    /// (`<data>/connection`: `host=` and `token=`) as a direct host, once.
    pub fn adopt_connection(&mut self, url: &str, token: &str) -> Option<String> {
        let url = url.trim();
        if url.is_empty() || self.hosts.iter().any(|h| matches!(&h.kind, HostKind::Url { url: u, .. } if u == url)) {
            return None;
        }
        Some(self.upsert(HostEntry::url(url, token)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_round_trips_and_always_starts_with_this_computer() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = HostsFile::load(dir.path());
        assert_eq!(fresh, HostsFile::default());
        let mut file = fresh;
        let ssh = file.upsert(HostEntry::ssh("dev@box", Some(2222), "~/.ssh/id_box"));
        file.upsert(HostEntry::wsl("Ubuntu"));
        file.upsert(HostEntry::url("ws://10.0.0.2:7840/", "secret"));
        file.active = ssh.clone();
        file.save(dir.path()).unwrap();
        let back = HostsFile::load(dir.path());
        assert_eq!(back, file);
        assert_eq!(back.hosts[0].id, LOCAL_ID);
        assert_eq!(back.get(&ssh).unwrap().label(), "dev@box");
        assert_eq!(back.get("wsl:Ubuntu").unwrap().label(), "Ubuntu (WSL)");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_damaged_or_odd_file_is_made_valid() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "{not json").unwrap();
        assert_eq!(HostsFile::load(dir.path()), HostsFile::default());
        let odd = r#"{"hosts":[{"id":"wsl:A","kind":"wsl","distro":"A"},{"id":"wsl:A","kind":"wsl","distro":"A"},
            {"id":"local","kind":"local","name":"Desk"}],"active":"gone","future_field":1}"#;
        std::fs::write(dir.path().join(FILE), odd).unwrap();
        let file = HostsFile::load(dir.path());
        assert_eq!(file.hosts.len(), 2, "duplicates dropped");
        assert_eq!(file.hosts[0].label(), "Desk", "this computer first, its name kept");
        assert_eq!(file.active, LOCAL_ID, "an unknown active host falls back");
    }

    #[test]
    fn removing_never_takes_this_computer() {
        let mut file = HostsFile::default();
        let id = file.upsert(HostEntry::wsl("Debian"));
        file.active = id.clone();
        assert!(!file.remove(LOCAL_ID));
        assert!(file.remove(&id));
        assert_eq!(file.active, LOCAL_ID);
        assert_eq!(file.adopt_connection("ws://box:7840/", "t").as_deref(), Some("url:ws://box:7840/"));
        assert_eq!(file.adopt_connection("ws://box:7840/", "t"), None, "only once");
    }

    #[test]
    fn typed_ssh_hosts_are_parsed() {
        let h = HostEntry::parse_ssh(" ssh://dev@box:2222/ ", "").unwrap();
        assert_eq!(h.kind, HostKind::Ssh { target: "dev@box".into(), port: Some(2222), identity: String::new() });
        assert_eq!(h.ssh_remote().as_deref(), Some("dev@box:2222"));
        let alias = HostEntry::parse_ssh("devbox", "").unwrap();
        assert_eq!(alias.id, "ssh:devbox");
        let v6 = HostEntry::parse_ssh("[fe80::1]:22", "").unwrap();
        assert!(matches!(v6.kind, HostKind::Ssh { ref target, port: Some(22), .. } if target == "fe80::1"));
        assert!(HostEntry::parse_ssh("box; rm -rf /", "").is_err());
        assert!(HostEntry::parse_ssh("-oProxyCommand=x", "").is_err());
        assert!(HostEntry::parse_ssh("box:http", "").is_err());
        assert!(HostEntry::parse_ssh("", "").is_err());
    }
}
