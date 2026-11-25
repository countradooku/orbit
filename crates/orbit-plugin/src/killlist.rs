//! The kill list: plugins Orbit turns off on every host, signed with the
//! release key.
//!
//! `kill-list.toml` and its signature `kill-list.toml.minisig` (trusted
//! comment `orbit-kill-list …`) live in `<data>/plugins/`. The host reads
//! them at start and whenever a newer signed copy arrives; a listed plugin
//! is disabled even when already installed, and stays off whatever the
//! user chooses until the list drops it. A list that does not verify is
//! ignored (the previous one stays), so a forged list can neither block
//! nor unblock anything.
//!
//! ```toml
//! [[plugins]]
//! id = "prompt-stealer"
//! versions = "<1.2.1"   # a semver requirement; "*" for all
//! reason = "Sent prompts to a third party."
//! ```

use serde::{Deserialize, Serialize};

pub const FILE: &str = "kill-list.toml";
pub const SIGNATURE: &str = "kill-list.toml.minisig";
pub const SIGNED_AS: &str = "orbit-kill-list";
pub const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KillList {
    #[serde(default)]
    pub plugins: Vec<Killed>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Killed {
    pub id: String,
    #[serde(default = "all")]
    pub versions: String,
    pub reason: String,
}

fn all() -> String {
    "*".into()
}

impl KillList {
    /// Verifies and parses a signed list.
    pub fn parse(bytes: &[u8], signature: &str, public_key: &str) -> Result<KillList, String> {
        if bytes.len() > MAX_BYTES {
            return Err("The kill list is too large.".into());
        }
        crate::sign::verify(bytes, signature, public_key, SIGNED_AS).map_err(|e| format!("The kill list’s signature: {e}"))?;
        let text = std::str::from_utf8(bytes).map_err(|_| "The kill list isn’t UTF-8.".to_owned())?;
        let list: KillList = toml::from_str(text).map_err(|e| format!("The kill list: {}", e.message()))?;
        for k in &list.plugins {
            semver::VersionReq::parse(&k.versions).map_err(|_| format!("The kill list’s entry “{}” has a bad version range.", k.id))?;
        }
        Ok(list)
    }

    /// Reads the list in `dir`, verified with the release key. A missing,
    /// unsigned or forged list is empty.
    pub fn load(dir: &std::path::Path) -> KillList {
        let Some(key) = crate::sign::release_key() else { return KillList::default() };
        let (Ok(bytes), Ok(signature)) = (std::fs::read(dir.join(FILE)), std::fs::read_to_string(dir.join(SIGNATURE))) else {
            return KillList::default();
        };
        KillList::parse(&bytes, &signature, &key).unwrap_or_else(|e| {
            log::warn!("plugins: ignoring the kill list: {e}");
            KillList::default()
        })
    }

    /// Why `id` at `version` is killed, if it is.
    pub fn reason(&self, id: &str, version: &str) -> Option<&str> {
        let version = semver::Version::parse(version).ok();
        self.plugins
            .iter()
            .find(|k| {
                k.id == id
                    && match (&version, semver::VersionReq::parse(&k.versions)) {
                        (Some(v), Ok(req)) => req.matches(v),
                        _ => true,
                    }
            })
            .map(|k| k.reason.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sign::testing::TestKey;

    const LIST: &str = "[[plugins]]\nid = \"stealer\"\nversions = \"<1.2.1\"\nreason = \"Sent prompts away.\"\n";

    #[test]
    fn listed_versions_are_killed() {
        let key = TestKey::new(11);
        let sig = key.sign(LIST.as_bytes(), "orbit-kill-list 2026-11-25");
        let list = KillList::parse(LIST.as_bytes(), &sig, &key.public_base64()).unwrap();
        assert_eq!(list.reason("stealer", "1.2.0"), Some("Sent prompts away."));
        assert_eq!(list.reason("stealer", "1.2.1"), None);
        assert_eq!(list.reason("other", "1.0.0"), None);
    }

    #[test]
    fn forged_lists_are_refused() {
        let key = TestKey::new(11);
        let sig = key.sign(LIST.as_bytes(), "orbit-kill-list 2026-11-25");
        let forged = LIST.replace("stealer", "competitor");
        assert!(KillList::parse(forged.as_bytes(), &sig, &key.public_base64()).is_err());
        let other_key = TestKey::new(12);
        assert!(KillList::parse(LIST.as_bytes(), &sig, &other_key.public_base64()).is_err());
        let catalog_sig = key.sign(LIST.as_bytes(), "orbit-catalog x");
        assert!(KillList::parse(LIST.as_bytes(), &catalog_sig, &key.public_base64()).is_err());
    }
}
