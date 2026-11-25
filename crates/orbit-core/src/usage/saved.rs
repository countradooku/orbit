//! `<data>/usage.bin`: the usage settings, each account's last readings
//! and their history, so the rings show the last known state at once after a
//! restart. Written whole (beside, then renamed); a file Orbit cannot read
//! starts over rather than failing.

use std::path::Path;

use orbit_proto::*;

use super::burn::Sample;

const FILE: &str = "usage.bin";
const VERSION: u8 = 1;

#[derive(Clone, Debug, Default, PartialEq, wincode::SchemaWrite, wincode::SchemaRead)]
pub struct Saved {
    pub settings: UsageSettings,
    pub accounts: Vec<SavedAccount>,
}

#[derive(Clone, Debug, PartialEq, wincode::SchemaWrite, wincode::SchemaRead)]
pub struct SavedAccount {
    /// The provider instance id.
    pub instance: String,
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
    pub updated_ms: u64,
    pub source: QuotaSource,
    /// Each window's samples, oldest first.
    pub history: Vec<(String, Vec<Sample>)>,
}

/// What `dir` holds; the defaults when nothing (readable) is there.
pub fn load(dir: &Path) -> Saved {
    let path = dir.join(FILE);
    let Ok(bytes) = std::fs::read(&path) else { return Saved::default() };
    match bytes.split_first() {
        Some((&VERSION, rest)) => decode(rest).unwrap_or_else(|e| {
            log::warn!("{}: unreadable ({e}); starting over", path.display());
            Saved::default()
        }),
        _ => Saved::default(),
    }
}

pub fn save(dir: &Path, saved: &Saved) -> std::io::Result<()> {
    let path = dir.join(FILE);
    let mut bytes = vec![VERSION];
    encode_into(saved, &mut bytes);
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_round_trips_and_a_bad_one_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), Saved::default());
        let saved = Saved {
            settings: UsageSettings { failover: FailoverPolicy::Auto, ..Default::default() },
            accounts: vec![SavedAccount {
                instance: "claude".into(),
                plan: Some("Max 20x".into()),
                windows: vec![QuotaWindow { window: "five_hour".into(), utilization: Some(0.4), resets_ms: 9, rejected: false }],
                updated_ms: 7,
                source: QuotaSource::Provider,
                history: vec![("five_hour".into(), vec![Sample { at_ms: 1, used: 0.3, resets_ms: 9 }])],
            }],
        };
        save(dir.path(), &saved).unwrap();
        assert_eq!(load(dir.path()), saved);
        std::fs::write(dir.path().join(FILE), [VERSION, 0xff, 0xff]).unwrap();
        assert_eq!(load(dir.path()), Saved::default());
    }
}
