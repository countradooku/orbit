//! Catalogs: signed lists of plugin releases to browse and install.
//!
//! A catalog is a `catalog.toml` next to its minisign signature
//! `catalog.toml.minisig`, whose trusted comment starts with
//! `orbit-catalog`. It is read from a folder or file on the host, or over
//! HTTPS. Each release pins its files with a content hash; the signature
//! covers the hash, so what gets installed is exactly what the publisher
//! signed, whichever mirror or git host served it.
//!
//! ```toml
//! name = "Orbit examples"
//!
//! [[plugins]]
//! id = "midnight-theme"
//! name = "Midnight"
//! version = "1.0.0"
//! description = "A dark theme"
//! tier = "data"
//! capabilities = []
//! hash = "blake3:…"
//! source = { archive = "midnight-theme-1.0.0.orbitplugin" }
//! # or { git = "https://github.com/o/r", commit = "<40 hex>" }, or { path = "midnight" }
//! ```

use orbit_proto::*;
use serde::{Deserialize, Serialize};

use crate::manifest::is_plugin_id;

/// The catalog file's name, and its signature's.
pub const FILE: &str = "catalog.toml";
pub const SIGNATURE: &str = "catalog.toml.minisig";
/// What the signature's trusted comment must start with.
pub const SIGNED_AS: &str = "orbit-catalog";
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 2048;

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub name: String,
    #[serde(default)]
    pub plugins: Vec<Release>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub categories: Vec<String>,
    /// `data`, `agent`, `panel` or `wasm`.
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub hash: String,
    pub source: Source,
}

/// Where a release's files are. Relative locations are resolved against
/// the catalog's own.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// Verifies and parses a catalog.
pub fn parse(bytes: &[u8], signature: &str, public_key: &str) -> Result<Catalog, String> {
    if bytes.len() > MAX_BYTES {
        return Err(format!("The catalog is larger than {} MiB.", MAX_BYTES / 1024 / 1024));
    }
    crate::sign::verify(bytes, signature, public_key, SIGNED_AS).map_err(|e| format!("The catalog’s signature: {e}"))?;
    let text = std::str::from_utf8(bytes).map_err(|_| "The catalog isn’t UTF-8.".to_owned())?;
    let catalog: Catalog = toml::from_str(text).map_err(|e| format!("The catalog: {}", e.message()))?;
    if catalog.plugins.len() > MAX_ENTRIES {
        return Err(format!("The catalog lists more than {MAX_ENTRIES} plugins."));
    }
    for r in &catalog.plugins {
        if !is_plugin_id(&r.id) || semver::Version::parse(&r.version).is_err() || !r.hash.starts_with(crate::hash::PREFIX) {
            return Err(format!("The catalog’s entry “{}” is malformed (id, version or hash).", r.id));
        }
        let sources = usize::from(r.source.archive.is_some()) + usize::from(r.source.path.is_some()) + usize::from(r.source.git.is_some());
        if sources != 1 || r.source.git.is_some() != r.source.commit.is_some() {
            return Err(format!("The catalog’s entry “{}” needs one source (archive, path, or git with commit).", r.id));
        }
    }
    Ok(catalog)
}

impl Release {
    pub fn entry(&self) -> CatalogEntry {
        CatalogEntry {
            id: self.id.clone(),
            name: self.name.clone(),
            version: self.version.clone(),
            description: self.description.clone(),
            publisher: self.publisher.clone(),
            categories: self.categories.clone(),
            tier: match self.tier.as_str() {
                "agent" => PluginTier::Agent,
                "panel" => PluginTier::Panel,
                "wasm" => PluginTier::Wasm,
                _ => PluginTier::Data,
            },
            capabilities: self.capabilities.iter().filter_map(|c| Capability::from_key(c)).collect(),
            hash: self.hash.clone(),
        }
    }
}

/// `relative` resolved against the catalog's `location` (a URL or a path
/// to the catalog file or its folder). Absolute URLs pass through; a
/// relative one may not climb out with `..`.
pub fn resolve(location: &str, relative: &str) -> Result<String, String> {
    if relative.starts_with("https://") {
        return Ok(relative.to_owned());
    }
    if !crate::manifest::is_relative_path(relative) {
        return Err(format!("“{relative}” must be an https address or a relative path."));
    }
    if location.starts_with("https://") || location.starts_with("http://") {
        let base = location.rsplit_once('/').map(|(b, _)| b).unwrap_or(location);
        return Ok(format!("{base}/{relative}"));
    }
    let path = std::path::Path::new(location);
    let dir = if path.extension().is_some_and(|e| e == "toml") { path.parent().unwrap_or(path) } else { path };
    Ok(dir.join(relative).display().to_string())
}

/// Where a catalog's file and signature are, given its location.
pub fn files(location: &str) -> (String, String) {
    let catalog = if location.ends_with(".toml") {
        location.to_owned()
    } else if location.starts_with("https://") || location.starts_with("http://") {
        format!("{}/{FILE}", location.trim_end_matches('/'))
    } else {
        std::path::Path::new(location).join(FILE).display().to_string()
    };
    let signature = format!("{catalog}.minisig");
    (catalog, signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sign::testing::TestKey;

    const LISTING: &str = r#"
name = "Examples"
[[plugins]]
id = "midnight-theme"
name = "Midnight"
version = "1.0.0"
tier = "data"
hash = "blake3:00"
source = { archive = "midnight.orbitplugin" }
"#;

    #[test]
    fn a_signed_catalog_parses_and_a_tampered_one_does_not() {
        let key = TestKey::new(5);
        let sig = key.sign(LISTING.as_bytes(), "orbit-catalog examples");
        let catalog = parse(LISTING.as_bytes(), &sig, &key.public_base64()).unwrap();
        assert_eq!(catalog.plugins[0].entry().tier, PluginTier::Data);
        let tampered = LISTING.replace("blake3:00", "blake3:11");
        assert!(parse(tampered.as_bytes(), &sig, &key.public_base64()).unwrap_err().contains("signature"));
        let kill = key.sign(LISTING.as_bytes(), "orbit-kill-list 1");
        assert!(parse(LISTING.as_bytes(), &kill, &key.public_base64()).is_err());
    }

    #[test]
    fn sources_resolve_against_the_catalog() {
        assert_eq!(resolve("https://x.dev/plugins/catalog.toml", "a.orbitplugin").unwrap(), "https://x.dev/plugins/a.orbitplugin");
        assert!(resolve("https://x.dev/catalog.toml", "../secret").is_err());
        let local = resolve("/srv/catalog", "a/b").unwrap();
        assert!(local.ends_with("b"), "{local}");
        assert_eq!(files("https://x.dev/plugins").0, "https://x.dev/plugins/catalog.toml");
    }
}
