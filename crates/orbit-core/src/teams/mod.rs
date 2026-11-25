//! Agent teams and races: the engine's state for them, their files, their
//! scheduling and the text agents read. The engine side (commands, turn
//! boundaries, merges, tests, the judge) is `engine/teams.rs`.

pub mod files;
pub mod plan;
pub mod text;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use orbit_proto::*;

pub use files::TeamFiles;

/// What the engine keeps for teams and races.
pub struct TeamsState {
    pub files: TeamFiles,
    pub teams: BTreeMap<TeamId, Team>,
    pub races: BTreeMap<RaceId, Race>,
    /// Member thread → its team.
    pub member: HashMap<ThreadId, TeamId>,
    /// Candidate thread → its race.
    pub candidate: HashMap<ThreadId, RaceId>,
    /// When each member was last steered with urgent mail.
    pub last_steer: HashMap<ThreadId, Instant>,
    /// Paths each working member changed, against the architect's branch.
    pub touched: HashMap<ThreadId, Vec<String>>,
    pub touched_pending: bool,
    pub next_touch: Option<Instant>,
    /// The `orbit` command line, for the members' MCP server: resolved once.
    cli: Option<Option<PathBuf>>,
    pub global_limit: u32,
    /// Candidates whose stats or tests are being gathered.
    pub gathering: std::collections::HashSet<ThreadId>,
}

impl TeamsState {
    pub fn load(data_dir: &Path) -> Self {
        let files = TeamFiles::new(data_dir);
        let (teams, races) = files.load();
        let mut state = Self {
            files,
            teams: BTreeMap::new(),
            races: BTreeMap::new(),
            member: HashMap::new(),
            candidate: HashMap::new(),
            last_steer: HashMap::new(),
            touched: HashMap::new(),
            touched_pending: false,
            next_touch: None,
            cli: None,
            global_limit: plan::GLOBAL_WORKERS,
            gathering: Default::default(),
        };
        for team in teams {
            state.index_team(&team);
            state.teams.insert(team.id, team);
        }
        for race in races {
            for c in &race.candidates {
                state.candidate.insert(c.thread, race.id);
            }
            state.races.insert(race.id, race);
        }
        state
    }

    pub fn index_team(&mut self, team: &Team) {
        self.member.insert(team.architect, team.id);
        for w in &team.workers {
            if let Some(t) = w.thread {
                self.member.insert(t, team.id);
            }
        }
    }

    pub fn board(&self) -> TeamBoard {
        TeamBoard { teams: self.teams.values().cloned().collect(), races: self.races.values().cloned().collect() }
    }

    pub fn is_idle(&self) -> bool {
        self.teams.is_empty() && self.races.values().all(|r| r.state != RaceState::Running)
    }

    /// Overrides where the `orbit` command line is (tests; `None`: none).
    pub fn set_cli(&mut self, cli: Option<PathBuf>) {
        self.cli = Some(cli);
    }

    /// The `orbit` command line: `ORBIT_CLI` if set, else next to this
    /// executable (or one folder up, for test binaries in `deps/`).
    pub fn cli(&mut self) -> Option<PathBuf> {
        self.cli
            .get_or_insert_with(|| {
                if let Some(path) = std::env::var_os("ORBIT_CLI").map(PathBuf::from) {
                    return path.is_file().then_some(path);
                }
                let exe = std::env::current_exe().ok()?;
                let name = if cfg!(windows) { "orbit.exe" } else { "orbit" };
                let dir = exe.parent()?;
                [dir.join(name), dir.parent()?.join(name)].into_iter().find(|p| p.is_file())
            })
            .clone()
    }
}
