//! Teams and races on disk: one small file per record in `<data>/teams/`.
//!
//! These records live outside the meta log on purpose. They change often
//! while a team works (mail, notes, worker states), are read only by the
//! engine, and have their own schema version, so growing them never forces
//! a migration of the meta log. Each file is a header (magic and version)
//! and one CRC-checked frame, replaced atomically (written beside, then
//! renamed), so a crash leaves either the old record or the new one.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use orbit_proto::*;

/// Bumped when [`Team`] or [`Race`] change layout; older versions get a
/// frozen copy and a conversion here, like the meta log's `legacy` module.
/// Version 1 was written in bitcode; version 2 in Orbit's wire format.
pub const TEAMS_VERSION: u32 = 2;
const TEAMS_BITCODE: u32 = 1;
const MAGIC: &[u8; 8] = b"orbteam\0";

pub struct TeamFiles {
    dir: PathBuf,
}

impl TeamFiles {
    pub fn new(data_dir: &Path) -> Self {
        Self { dir: data_dir.join("teams") }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Every team and race on disk. Unreadable files are set aside (never
    /// deleted) and reported in the log.
    pub fn load(&self) -> (Vec<Team>, Vec<Race>) {
        let (mut teams, mut races) = (Vec::new(), Vec::new());
        let Ok(entries) = fs::read_dir(&self.dir) else { return (teams, races) };
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_owned();
            if ext != "team" && ext != "race" {
                continue;
            }
            let loaded = fs::read(&path).map_err(|e| e.to_string()).and_then(|bytes| {
                let (version, payload) = payload(&bytes)?;
                let old = version == TEAMS_BITCODE;
                if ext == "team" {
                    let team = if old { bitcode_decode::<Team>(payload) } else { decode::<Team>(payload) };
                    team.map(|t| teams.push(t)).map_err(|e| e.to_string())
                } else {
                    let race = if old { bitcode_decode::<Race>(payload) } else { decode::<Race>(payload) };
                    race.map(|r| races.push(r)).map_err(|e| e.to_string())
                }
            });
            if let Err(err) = loaded {
                log::warn!("unreadable team record {}: {err}", path.display());
                let _ = fs::rename(&path, path.with_extension(format!("{ext}.bad")));
            }
        }
        for t in &teams {
            observe_raw_id(t.id.0);
        }
        for r in &races {
            observe_raw_id(r.id.0);
        }
        teams.sort_by_key(|t| t.id);
        races.sort_by_key(|r| r.id);
        (teams, races)
    }

    pub fn save_team(&self, team: &Team) -> io::Result<()> {
        self.write(&format!("{}.team", team.id), &encode(team))
    }

    pub fn save_race(&self, race: &Race) -> io::Result<()> {
        self.write(&format!("{}.race", race.id), &encode(race))
    }

    pub fn remove_team(&self, id: TeamId) {
        let _ = fs::remove_file(self.path(&format!("{id}.team")));
    }

    pub fn remove_race(&self, id: RaceId) {
        let _ = fs::remove_file(self.path(&format!("{id}.race")));
    }

    /// Writes a session's MCP configuration (Claude Code reads it as a
    /// file) and returns its path.
    pub fn write_mcp_config(&self, thread: ThreadId, json: &str) -> io::Result<PathBuf> {
        let dir = self.dir.join("mcp");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{thread}.json"));
        if fs::read_to_string(&path).ok().as_deref() != Some(json) {
            replace(&path, json.as_bytes())?;
        }
        Ok(path)
    }

    fn write(&self, name: &str, record: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let mut bytes = Vec::with_capacity(record.len() + 32);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&TEAMS_VERSION.to_le_bytes());
        write_raw_frame(record, &mut bytes);
        replace(&self.path(name), &bytes)
    }
}

/// Writes beside `path`, then renames over it.
fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// A record's schema version and payload.
fn payload(bytes: &[u8]) -> Result<(u32, &[u8]), String> {
    if bytes.len() < 12 || &bytes[..8] != MAGIC {
        return Err("not a team record".into());
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != TEAMS_VERSION && version != TEAMS_BITCODE {
        return Err(format!("written by schema {version}; this Orbit reads up to {TEAMS_VERSION}"));
    }
    FrameReader::new(&bytes[12..]).next_payload().map(|p| (version, p)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_survive_a_reload_and_bad_files_are_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let files = TeamFiles::new(dir.path());
        let team = Team {
            id: TeamId::new(),
            architect: ThreadId(7),
            notes: vec![Note { id: 1, author: ThreadId(7), author_label: "architect".into(), text: "POST /a".into(), at_ms: 1 }],
            ..Default::default()
        };
        let race = Race { id: RaceId::new(), prompt: "go".into(), ..Default::default() };
        files.save_team(&team).unwrap();
        files.save_race(&race).unwrap();
        fs::write(files.dir().join("junk.team"), b"nope").unwrap();
        let (teams, races) = files.load();
        assert_eq!(teams, vec![team.clone()]);
        assert_eq!(races, vec![race]);
        assert!(files.dir().join("junk.team.bad").exists());
        files.remove_team(team.id);
        assert!(files.load().0.is_empty());
    }

    #[test]
    fn version_1_bitcode_records_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let files = TeamFiles::new(dir.path());
        let team = Team { id: TeamId::new(), architect: ThreadId(8), ..Default::default() };
        fs::create_dir_all(files.dir()).unwrap();
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&TEAMS_BITCODE.to_le_bytes());
        write_raw_frame(&bitcode::encode(&team), &mut bytes);
        fs::write(files.dir().join(format!("{}.team", team.id)), bytes).unwrap();
        assert_eq!(files.load().0, vec![team]);
    }
}
