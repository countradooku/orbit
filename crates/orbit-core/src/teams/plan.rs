//! Who starts next: the pure part of team scheduling.

use std::collections::{BTreeMap, HashMap};

use orbit_proto::*;

/// Workers working at once across every team.
pub const GLOBAL_WORKERS: u32 = 8;

/// What to do with a waiting worker now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Start,
    Wait(WaitReason),
}

/// Counts the scheduler works against.
#[derive(Clone, Copy, Debug)]
pub struct Slots {
    /// Workers of this team working now.
    pub team: u32,
    /// Workers of every team working now.
    pub global: u32,
    pub global_limit: u32,
}

/// Decides one waiting worker. `busy` holds, for each working teammate, a
/// label and the paths it plans to edit or has edited.
pub fn decide(team: &Team, worker: &Worker, slots: Slots, busy: &[(String, Vec<String>)]) -> Verdict {
    let mut unfinished = Vec::new();
    for key in &worker.depends_on {
        match team.worker(key).map(|w| &w.state) {
            Some(s) if s.succeeded() => {}
            Some(WorkerState::Failed { .. } | WorkerState::Stopped) | None => {
                return Verdict::Wait(WaitReason::DependencyFailed { key: key.clone() });
            }
            Some(_) => unfinished.push(key.clone()),
        }
    }
    if !unfinished.is_empty() {
        return Verdict::Wait(WaitReason::Dependency { on: unfinished });
    }
    if team.breaker.is_some() {
        return Verdict::Wait(WaitReason::Breaker);
    }
    for path in &worker.scope {
        for (label, paths) in busy {
            if let Some(hit) = paths.iter().find(|p| overlaps(path, p)) {
                let shown = if normalize(hit).len() >= normalize(path).len() { hit } else { path };
                return Verdict::Wait(WaitReason::FileScope { path: normalize(shown), with: label.clone() });
            }
        }
    }
    if slots.team >= team.limits.max_workers.max(1) {
        return Verdict::Wait(WaitReason::NoSlot { running: slots.team, limit: team.limits.max_workers.max(1), global: false });
    }
    if slots.global >= slots.global_limit {
        return Verdict::Wait(WaitReason::NoSlot { running: slots.global, limit: slots.global_limit, global: true });
    }
    Verdict::Start
}

/// `a/b/` and `.\a\b` are the same path: `a/b`. Empty means everything.
pub fn normalize(path: &str) -> String {
    let p = path.trim().replace('\\', "/");
    let p = p.trim_start_matches("./").trim_start_matches('/').trim_end_matches('/');
    if p == "." { String::new() } else { p.to_owned() }
}

/// Two scopes touch when they are the same path or one holds the other.
pub fn overlaps(a: &str, b: &str) -> bool {
    let (a, b) = (normalize(a), normalize(b));
    a.is_empty()
        || b.is_empty()
        || a == b
        || b.strip_prefix(a.as_str()).is_some_and(|r| r.starts_with('/'))
        || a.strip_prefix(b.as_str()).is_some_and(|r| r.starts_with('/'))
}

/// Files more than one working member changed.
pub fn file_overlaps(touched: &HashMap<ThreadId, Vec<String>>) -> Vec<FileOverlap> {
    let mut by_path: BTreeMap<&str, Vec<ThreadId>> = BTreeMap::new();
    for (thread, paths) in touched {
        for p in paths {
            let threads = by_path.entry(p.as_str()).or_default();
            if !threads.contains(thread) {
                threads.push(*thread);
            }
        }
    }
    by_path
        .into_iter()
        .filter(|(_, t)| t.len() > 1)
        .map(|(path, mut threads)| {
            threads.sort();
            FileOverlap { path: path.to_owned(), threads }
        })
        .collect()
}

/// How deep `thread` sits in the team: 0 for the architect, 1 for its
/// workers, 2 for theirs…
pub fn depth(team: &Team, thread: ThreadId) -> u32 {
    let mut at = thread;
    for d in 0..32 {
        if at == team.architect {
            return d;
        }
        match team.worker_of(at) {
            Some(w) => at = w.parent,
            None => return u32::MAX,
        }
    }
    u32::MAX
}

/// The next free worker key (`w1`, `w2`…).
pub fn next_key(team: &Team) -> String {
    let n = team.workers.iter().filter_map(|w| w.key.strip_prefix('w')?.parse::<u32>().ok()).max().unwrap_or(0);
    format!("w{}", n + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(key: &str, state: WorkerState) -> Worker {
        Worker { key: key.into(), state, ..Default::default() }
    }

    const FREE: Slots = Slots { team: 0, global: 0, global_limit: GLOBAL_WORKERS };

    #[test]
    fn waiting_is_explained_in_order_of_what_unblocks_first() {
        let mut team = Team { limits: TeamLimits { max_workers: 2, ..Default::default() }, ..Default::default() };
        team.workers.push(worker("w1", WorkerState::Working));
        let mut w = Worker { depends_on: vec!["w1".into()], scope: vec!["src/api".into()], ..Default::default() };
        assert_eq!(decide(&team, &w, FREE, &[]), Verdict::Wait(WaitReason::Dependency { on: vec!["w1".into()] }));
        team.workers[0].state = WorkerState::Done;
        team.breaker = Some(Breaker { reason: "x".into(), at_ms: 0 });
        assert_eq!(decide(&team, &w, FREE, &[]), Verdict::Wait(WaitReason::Breaker));
        team.breaker = None;
        let full = Slots { team: 2, ..FREE };
        assert!(matches!(decide(&team, &w, full, &[]), Verdict::Wait(WaitReason::NoSlot { global: false, limit: 2, .. })));
        // A teammate editing the same files says more than a full crew.
        let busy = vec![("w3".to_owned(), vec!["src".to_owned()])];
        assert!(matches!(decide(&team, &w, full, &busy), Verdict::Wait(WaitReason::FileScope { .. })));
        let global = Slots { global: GLOBAL_WORKERS, ..FREE };
        assert!(matches!(decide(&team, &w, global, &[]), Verdict::Wait(WaitReason::NoSlot { global: true, .. })));
        let busy = vec![("w3".to_owned(), vec!["src/api/users.rs".to_owned()])];
        assert_eq!(
            decide(&team, &w, FREE, &busy),
            Verdict::Wait(WaitReason::FileScope { path: "src/api/users.rs".into(), with: "w3".into() })
        );
        assert_eq!(decide(&team, &w, FREE, &[("w3".into(), vec!["src/apiary".into()])]), Verdict::Start);
        team.workers[0].state = WorkerState::Failed { message: "x".into() };
        w.scope.clear();
        assert_eq!(decide(&team, &w, FREE, &[]), Verdict::Wait(WaitReason::DependencyFailed { key: "w1".into() }));
    }

    #[test]
    fn paths_overlap_by_containment() {
        assert!(overlaps("src", "src/a.rs"));
        assert!(overlaps(".\\src\\a.rs", "src/a.rs"));
        assert!(overlaps(".", "anything"));
        assert!(!overlaps("src/a", "src/ab"));
    }

    #[test]
    fn overlaps_list_files_two_members_touched() {
        let touched = HashMap::from([
            (ThreadId(1), vec!["a.rs".to_owned(), "b.rs".to_owned()]),
            (ThreadId(2), vec!["b.rs".to_owned()]),
        ]);
        assert_eq!(file_overlaps(&touched), vec![FileOverlap { path: "b.rs".into(), threads: vec![ThreadId(1), ThreadId(2)] }]);
    }

    #[test]
    fn depth_and_keys() {
        let mut team = Team { architect: ThreadId(1), ..Default::default() };
        team.workers.push(Worker { key: "w1".into(), parent: ThreadId(1), thread: Some(ThreadId(2)), ..Default::default() });
        team.workers.push(Worker { key: "w2".into(), parent: ThreadId(2), thread: Some(ThreadId(3)), ..Default::default() });
        assert_eq!((depth(&team, ThreadId(1)), depth(&team, ThreadId(2)), depth(&team, ThreadId(3))), (0, 1, 2));
        assert_eq!(depth(&team, ThreadId(9)), u32::MAX);
        assert_eq!(next_key(&team), "w3");
    }
}
