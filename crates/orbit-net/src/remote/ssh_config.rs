//! Hosts named in the user's OpenSSH config, offered when adding a host.
//!
//! Only `Host` blocks with plain names count: patterns (`*`, `?`, `!`) and
//! `Match` blocks are not hosts one connects to by name. `Include` is
//! followed (relative paths are under `~/.ssh`, `*` and `?` in the file
//! name are expanded), as `ssh` does. `HostName`, `User` and `Port` are
//! read for display only; connecting passes the alias to `ssh`, which
//! applies everything else (ProxyJump, IdentityFile…) itself.

use std::path::{Path, PathBuf};

/// One `Host` alias.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshHost {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
}

impl SshHost {
    /// "dev@10.0.0.4:2222", for showing beside the alias.
    pub fn detail(&self) -> Option<String> {
        let host = self.hostname.as_deref().filter(|h| *h != self.alias);
        if host.is_none() && self.user.is_none() && self.port.is_none() {
            return None;
        }
        let mut out = String::new();
        if let Some(user) = &self.user {
            out.push_str(user);
            out.push('@');
        }
        out.push_str(host.unwrap_or(&self.alias));
        if let Some(port) = self.port {
            out.push_str(&format!(":{port}"));
        }
        Some(out)
    }
}

/// The hosts in `~/.ssh/config`, in file order, each alias once.
pub fn read_user_config() -> Vec<SshHost> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let ssh_dir = home.join(".ssh");
    read(&ssh_dir.join("config"), &ssh_dir)
}

/// The hosts in the config file at `path`; `ssh_dir` anchors relative
/// `Include`s.
pub fn read(path: &Path, ssh_dir: &Path) -> Vec<SshHost> {
    let mut hosts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    collect(path, ssh_dir, 0, &mut hosts, &mut seen);
    hosts
}

fn collect(path: &Path, ssh_dir: &Path, depth: usize, hosts: &mut Vec<SshHost>, seen: &mut std::collections::HashSet<String>) {
    // ssh itself stops at 16 levels.
    if depth > 16 {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else { return };
    // Aliases of the block being read; directives fill them in.
    let mut current: Vec<usize> = Vec::new();
    for raw in text.lines() {
        let Some((key, value)) = directive(raw) else { continue };
        match key.as_str() {
            "host" => {
                current.clear();
                for alias in words(&value) {
                    if alias.contains(['*', '?', '!']) || alias.is_empty() || !seen.insert(alias.clone()) {
                        continue;
                    }
                    current.push(hosts.len());
                    hosts.push(SshHost { alias, ..Default::default() });
                }
            }
            "match" => current.clear(),
            "include" => {
                for pattern in words(&value) {
                    for file in expand_include(&pattern, ssh_dir) {
                        collect(&file, ssh_dir, depth + 1, hosts, seen);
                    }
                }
            }
            // The first value of a directive wins, as in ssh.
            "hostname" => {
                for &ix in &current {
                    hosts[ix].hostname.get_or_insert_with(|| value.clone());
                }
            }
            "user" => {
                for &ix in &current {
                    hosts[ix].user.get_or_insert_with(|| value.clone());
                }
            }
            "port" => {
                if let Ok(port) = value.parse() {
                    for &ix in &current {
                        hosts[ix].port.get_or_insert(port);
                    }
                }
            }
            _ => {}
        }
    }
}

/// `Keyword value` or `Keyword=value`, comments and blank lines skipped.
fn directive(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let split = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let key = line[..split].to_ascii_lowercase();
    let value = line[split..].trim_start_matches(|c: char| c.is_whitespace() || c == '=').trim();
    Some((key, value.to_owned()))
}

/// Space-separated words, double quotes grouping.
fn words(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in value.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// The files an `Include` names, sorted as ssh reads them.
fn expand_include(pattern: &str, ssh_dir: &Path) -> Vec<PathBuf> {
    let path = match pattern.strip_prefix("~/") {
        Some(rest) => match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => return Vec::new(),
        },
        None if Path::new(pattern).is_absolute() => PathBuf::from(pattern),
        None => ssh_dir.join(pattern),
    };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let Some(dir) = path.parent() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| glob_match(&name, &e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    files.sort();
    files
}

/// `*` and `?` matching over a whole name.
fn glob_match(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match (p.first(), n.first()) {
            (None, None) => true,
            (Some('*'), _) => go(&p[1..], n) || (!n.is_empty() && go(p, &n[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &n[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &n[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_and_includes_are_read_like_ssh_does() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path();
        std::fs::create_dir_all(ssh.join("config.d")).unwrap();
        std::fs::write(
            ssh.join("config"),
            "# work machines\n\
             Host devbox build\n\
             \x20   HostName 10.0.0.4\n\
             \x20   User dev\n\
             \x20   Port 2222\n\
             \x20   Port 22\n\
             Host *.internal !bastion\n\
             \x20   User nobody\n\
             Host=gpu\n\
             \x20 HostName=gpu.example.com\n\
             Match host foo\n\
             \x20   User matched\n\
             Include config.d/*.conf\n\
             Include missing-file\n\
             Host devbox\n\
             \x20   User later\n",
        )
        .unwrap();
        std::fs::write(ssh.join("config.d/a.conf"), "Host \"pi zero\" pi\n  User pi\n").unwrap();
        std::fs::write(ssh.join("config.d/b.txt"), "Host ignored\n").unwrap();
        let hosts = read(&ssh.join("config"), ssh);
        let aliases: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, ["devbox", "build", "gpu", "pi zero", "pi"]);
        let devbox = &hosts[0];
        assert_eq!(devbox.hostname.as_deref(), Some("10.0.0.4"));
        assert_eq!(devbox.user.as_deref(), Some("dev"), "the first value wins");
        assert_eq!(devbox.port, Some(2222));
        assert_eq!(devbox.detail().as_deref(), Some("dev@10.0.0.4:2222"));
        assert_eq!(hosts[2].detail().as_deref(), Some("gpu.example.com"));
        assert_eq!(hosts[4].user.as_deref(), Some("pi"));
        assert_eq!(hosts[4].detail().as_deref(), Some("pi@pi"));
    }

    #[test]
    fn a_missing_config_has_no_hosts() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(&dir.path().join("config"), dir.path()).is_empty());
    }

    #[test]
    fn includes_that_loop_stop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config"), "Include config\nHost a\n").unwrap();
        let hosts = read(&dir.path().join("config"), dir.path());
        assert_eq!(hosts.iter().map(|h| h.alias.as_str()).collect::<Vec<_>>(), ["a"]);
    }
}
