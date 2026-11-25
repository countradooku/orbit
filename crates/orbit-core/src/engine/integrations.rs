//! The integrations file (`<data>/integrations.bin`): tracker credentials
//! ([`IntegrationSettings`]) and the chat bots' tokens ([`BotSecrets`]).
//!
//! Secrets live here, not in the settings record: settings travel in
//! logged, replayed events to every client, and tokens must not. The file
//! is written atomically and is readable by the user only.
//!
//! Layout: one version byte, then bitcode. Version 1 held the tracker
//! settings alone; version 2 holds both. Unknown versions read as empty.

use std::path::Path;

use orbit_proto::*;

pub(crate) const FILE: &str = "integrations.bin";
const V1: u8 = 1;
const V2: u8 = 2;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Integrations {
    pub forge: IntegrationSettings,
    pub bots: BotSecrets,
}

pub(crate) fn load(path: &Path) -> Integrations {
    let Ok(bytes) = std::fs::read(path) else { return Integrations::default() };
    let unreadable = |e: bitcode::Error| {
        log::warn!("{}: unreadable ({e}); starting empty", path.display());
        Integrations::default()
    };
    match bytes.split_first() {
        Some((&V1, rest)) => bitcode::decode::<IntegrationSettings>(rest)
            .map(|forge| Integrations { forge, bots: BotSecrets::default() })
            .unwrap_or_else(unreadable),
        Some((&V2, rest)) => bitcode::decode::<(IntegrationSettings, BotSecrets)>(rest)
            .map(|(forge, bots)| Integrations { forge, bots })
            .unwrap_or_else(unreadable),
        _ => Integrations::default(),
    }
}

pub(crate) fn save(path: &Path, integrations: &Integrations) -> std::io::Result<()> {
    let mut bytes = vec![V2];
    bytes.extend(bitcode::encode(&(integrations.forge.clone(), integrations.bots.clone())));
    let tmp = path.with_extension("bin.tmp");
    {
        use std::io::Write as _;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // Tokens: readable by the user only.
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Reads the file, changes it and writes it back, keeping what `edit` does
/// not touch (the forge's settings when the bots save, and the reverse).
pub(crate) fn update(path: &Path, edit: impl FnOnce(&mut Integrations)) -> std::io::Result<Integrations> {
    let mut current = load(path);
    edit(&mut current);
    save(path, &current)?;
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_1_files_keep_their_tracker_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let forge = IntegrationSettings { linear_api_key: "lin_api_x".into(), ..Default::default() };
        let mut v1 = vec![V1];
        v1.extend(bitcode::encode(&forge));
        std::fs::write(&path, v1).unwrap();
        assert_eq!(load(&path), Integrations { forge: forge.clone(), bots: BotSecrets::default() });
        // A bot token saved later keeps them, and the reverse.
        update(&path, |i| i.bots.set(Platform::Telegram, "123:abc", "", false)).unwrap();
        let both = load(&path);
        assert_eq!((both.forge.linear_api_key.as_str(), both.bots.telegram.as_str()), ("lin_api_x", "123:abc"));
        update(&path, |i| i.forge.jira_url = "https://a.atlassian.net".into()).unwrap();
        assert_eq!(load(&path).bots.telegram, "123:abc");
        std::fs::write(&path, [9, 9]).unwrap();
        assert_eq!(load(&path), Integrations::default());
    }
}
