//! The update manifest: one JSON document per channel naming the newest
//! release, its notes, and per-platform downloads with their SHA-256 and
//! minisign signature. See `docs/RELEASING.md` for the format.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Which stream of releases this desktop follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Stable,
    Beta,
    Nightly,
}

impl Channel {
    pub const ALL: [Channel; 3] = [Channel::Stable, Channel::Beta, Channel::Nightly];

    pub fn id(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
            Channel::Nightly => "nightly",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Channel::Stable => "Stable",
            Channel::Beta => "Beta",
            Channel::Nightly => "Nightly",
        }
    }
}

/// One channel's newest release.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub channel: Option<Channel>,
    /// RFC 3339 publication time, shown as-is.
    #[serde(default)]
    pub pub_date: Option<String>,
    /// Release notes in Markdown.
    #[serde(default)]
    pub notes: String,
    /// Downloads by platform key ([`platform_key`]).
    pub platforms: BTreeMap<String, Artifact>,
}

/// One platform's download.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub url: String,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: String,
    /// Bytes, for progress before the server says.
    #[serde(default)]
    pub size: Option<u64>,
    /// The complete minisign signature (`.minisig` contents) of the file.
    /// Its trusted comment must read `orbit <version> <platform>`.
    pub signature: String,
}

/// A release this desktop can install.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: semver::Version,
    pub channel: Channel,
    pub notes: String,
    pub pub_date: Option<String>,
    pub platform: String,
    pub artifact: Artifact,
}

#[derive(Debug, PartialEq)]
pub enum Check {
    /// Nothing newer than this build on the channel.
    UpToDate,
    Available(Box<Release>),
}

/// The platform this build updates as: its target triple, with `.appimage`
/// when it runs from an AppImage (which replaces itself as one file).
pub fn platform_key(target: &str, appimage: bool) -> String {
    if appimage { format!("{target}.appimage") } else { target.to_owned() }
}

pub fn parse(text: &str) -> Result<Manifest, String> {
    sonic_rs::from_str::<Manifest>(text).map_err(|e| format!("The update information is malformed ({e})."))
}

/// What the manifest offers this build: a newer version for `platform`, or
/// nothing. A newer version without a download for this platform counts as
/// up to date (the release skipped it), never as an error the user can't fix.
pub fn evaluate(manifest: Manifest, channel: Channel, current: &str, platform: &str) -> Result<Check, String> {
    let offered = semver::Version::parse(manifest.version.trim())
        .map_err(|_| format!("The update information names an invalid version “{}”.", manifest.version))?;
    let current = semver::Version::parse(current).map_err(|e| format!("This build’s version is invalid ({e})."))?;
    if offered <= current {
        return Ok(Check::UpToDate);
    }
    let Some(artifact) = manifest.platforms.get(platform).cloned() else { return Ok(Check::UpToDate) };
    if artifact.sha256.len() != 64 || !artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("The update information has a malformed checksum.".to_owned());
    }
    Ok(Check::Available(Box::new(Release {
        version: offered,
        channel: manifest.channel.unwrap_or(channel),
        notes: manifest.notes,
        pub_date: manifest.pub_date,
        platform: platform.to_owned(),
        artifact,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: &str) -> Manifest {
        let mut platforms = BTreeMap::new();
        platforms.insert(
            "x86_64-pc-windows-msvc".to_owned(),
            Artifact { url: "https://example.test/a.zip".into(), sha256: "ab".repeat(32), size: Some(10), signature: "sig".into() },
        );
        Manifest { version: version.into(), channel: None, pub_date: None, notes: "## Notes".into(), platforms }
    }

    #[test]
    fn newer_versions_are_offered_only_for_this_platform() {
        let check = evaluate(manifest("0.2.0"), Channel::Stable, "0.1.0", "x86_64-pc-windows-msvc").unwrap();
        let Check::Available(release) = check else { panic!("expected an update") };
        assert_eq!(release.version.to_string(), "0.2.0");
        assert_eq!(release.channel, Channel::Stable);
        assert_eq!(evaluate(manifest("0.2.0"), Channel::Stable, "0.1.0", "aarch64-apple-darwin").unwrap(), Check::UpToDate);
    }

    #[test]
    fn same_or_older_versions_are_never_offered() {
        assert_eq!(evaluate(manifest("0.1.0"), Channel::Stable, "0.1.0", "x86_64-pc-windows-msvc").unwrap(), Check::UpToDate);
        assert_eq!(evaluate(manifest("0.0.9"), Channel::Stable, "0.1.0", "x86_64-pc-windows-msvc").unwrap(), Check::UpToDate);
        // A nightly of the next version is newer than the release before it,
        // and older than that version's release.
        assert!(matches!(
            evaluate(manifest("0.2.0-nightly.20261125"), Channel::Nightly, "0.1.0", "x86_64-pc-windows-msvc").unwrap(),
            Check::Available(_)
        ));
        assert_eq!(
            evaluate(manifest("0.2.0-nightly.20261125"), Channel::Nightly, "0.2.0", "x86_64-pc-windows-msvc").unwrap(),
            Check::UpToDate
        );
    }

    #[test]
    fn malformed_manifests_are_errors() {
        assert!(evaluate(manifest("two"), Channel::Stable, "0.1.0", "x86_64-pc-windows-msvc").is_err());
        let mut bad = manifest("0.2.0");
        bad.platforms.get_mut("x86_64-pc-windows-msvc").unwrap().sha256 = "xyz".into();
        assert!(evaluate(bad, Channel::Stable, "0.1.0", "x86_64-pc-windows-msvc").is_err());
        assert!(parse("{").is_err());
    }

    #[test]
    fn manifests_parse_from_json() {
        let text = r##"{
            "version": "1.2.3",
            "channel": "beta",
            "notes": "# Hi",
            "platforms": {
                "aarch64-apple-darwin": { "url": "https://x/y.zip", "sha256": "00", "signature": "s" }
            }
        }"##;
        let m = parse(text).unwrap();
        assert_eq!(m.channel, Some(Channel::Beta));
        assert_eq!(m.platforms["aarch64-apple-darwin"].size, None);
        assert_eq!(platform_key("x86_64-unknown-linux-gnu", true), "x86_64-unknown-linux-gnu.appimage");
    }
}
