//! Turning what a person or agent types into ids: threads, projects and
//! pending requests.

use std::path::Path;

use orbit_net::state::ClientState;
use orbit_proto::*;

use crate::conn::Env;
use crate::error::CliError;

/// Resolves a thread selector:
/// * a full id (`thr_18c…`, with or without the prefix);
/// * `self` (the thread this process runs in), `parent` (its parent);
/// * `latest` (the most recently active, unarchived thread);
/// * a unique part of an id, or of a title (case-insensitive).
pub fn thread(state: &ClientState, env: &Env, selector: &str) -> Result<ThreadId, CliError> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(CliError::usage("Name a thread."));
    }
    match selector {
        "self" | "." => {
            let own = env.thread.as_deref().ok_or_else(|| {
                CliError::not_found("`self` only works inside an Orbit thread (ORBIT_THREAD is not set).")
            })?;
            return exact(state, own).ok_or_else(|| CliError::not_found(format!("This thread ({own}) no longer exists.")));
        }
        "parent" => {
            let own = thread(state, env, "self")?;
            return state.threads[&own]
                .parent
                .filter(|p| state.threads.contains_key(p))
                .ok_or_else(|| CliError::not_found("This thread has no parent."));
        }
        "latest" | "last" => {
            return state
                .threads
                .values()
                .filter(|t| !t.archived)
                .max_by_key(|t| (t.updated_ms, t.id))
                .map(|t| t.id)
                .ok_or_else(|| CliError::not_found("There are no threads yet."));
        }
        _ => {}
    }
    if let Some(id) = exact(state, selector) {
        return Ok(id);
    }
    let needle = selector.to_lowercase();
    let hex = needle.strip_prefix("thr_").unwrap_or(&needle);
    if hex.len() >= 3 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let hits: Vec<&Thread> = state.threads.values().filter(|t| format!("{:x}", t.id.0).contains(hex)).collect();
        if !hits.is_empty() {
            return one(hits, selector);
        }
    }
    let titled: Vec<&Thread> = state.threads.values().filter(|t| t.title.to_lowercase() == needle).collect();
    if !titled.is_empty() {
        return one(titled, selector);
    }
    let titled: Vec<&Thread> = state.threads.values().filter(|t| t.title.to_lowercase().contains(&needle)).collect();
    if titled.is_empty() {
        return Err(CliError::not_found(format!("No thread matches “{selector}”.")).hint("List them with `orbit threads --all`."));
    }
    one(titled, selector)
}

fn exact(state: &ClientState, selector: &str) -> Option<ThreadId> {
    let id: ThreadId = selector.parse().ok()?;
    // Only a full id: short hex strings are matched as parts below.
    let full = selector.strip_prefix("thr_").unwrap_or(selector).len() >= 12;
    (full && state.threads.contains_key(&id)).then_some(id)
}

fn one(mut hits: Vec<&Thread>, selector: &str) -> Result<ThreadId, CliError> {
    if hits.len() == 1 {
        return Ok(hits[0].id);
    }
    hits.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
    let list: Vec<String> = hits.iter().take(5).map(|t| format!("  {}  {}", t.id, crate::json::title(t))).collect();
    Err(CliError::not_found(format!("“{selector}” matches {} threads.", hits.len()))
        .hint(format!("Use one of these ids:\n{}", list.join("\n"))))
}

/// Resolves a project selector: id, name, path, or `.` / a path inside a
/// project.
pub fn project(state: &ClientState, env: &Env, selector: &str) -> Result<ProjectId, CliError> {
    let selector = selector.trim();
    if let Ok(id) = selector.parse::<ProjectId>()
        && state.projects.iter().any(|p| p.id == id)
        && selector.len() >= 12
    {
        return Ok(id);
    }
    if let Some(p) = state.projects.iter().find(|p| p.name.eq_ignore_ascii_case(selector)) {
        return Ok(p.id);
    }
    let looks_like_path = selector == "." || selector.contains('/') || selector.contains('\\') || Path::new(selector).is_dir();
    if looks_like_path {
        let path = env.cwd.join(selector);
        if let Some(id) = containing(state, &path) {
            return Ok(id);
        }
        return Err(CliError::not_found(format!("“{}” is not inside a project.", path.display()))
            .hint("Add it with `orbit projects add <path>`."));
    }
    let needle = selector.to_lowercase();
    let hits: Vec<&Project> = state.projects.iter().filter(|p| p.name.to_lowercase().contains(&needle)).collect();
    match hits.as_slice() {
        [one] => Ok(one.id),
        [] => Err(CliError::not_found(format!("No project matches “{selector}”.")).hint("List them with `orbit projects`.")),
        many => Err(CliError::not_found(format!(
            "“{selector}” matches {} projects: {}.",
            many.len(),
            many.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        ))),
    }
}

/// The project whose folder (or one of whose thread worktrees) contains
/// `path`, preferring the deepest.
pub fn containing(state: &ClientState, path: &Path) -> Option<ProjectId> {
    let path = canonical(path);
    let mut best: Option<(usize, ProjectId)> = None;
    let mut consider = |root: &str, id: ProjectId| {
        let root = canonical(Path::new(root));
        if path.starts_with(&root) {
            let depth = root.components().count();
            if best.is_none_or(|(d, _)| depth > d) {
                best = Some((depth, id));
            }
        }
    };
    for project in &state.projects {
        consider(&project.path, project.id);
    }
    for thread in state.threads.values() {
        if let (Some(worktree), Some(project)) = (&thread.workspace.worktree, thread.project) {
            consider(worktree, project);
        }
    }
    best.map(|(_, id)| id)
}

/// Case-insensitive on Windows and macOS, where paths are; canonical when
/// the path exists so `..` and symlinks compare equal.
fn canonical(path: &Path) -> std::path::PathBuf {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    if cfg!(any(windows, target_os = "macos")) { text.to_lowercase().into() } else { text.into() }
}

/// What kind of pending request a command answers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Answering {
    Approval,
    Question,
    Any,
}

/// The pending request a command acts on: `--request`, else the only one
/// that fits.
pub fn request(thread: &Thread, selector: Option<&str>, answering: Answering) -> Result<PendingRequest, CliError> {
    let fits = |p: &&PendingRequest| match answering {
        Answering::Approval => !matches!(p.request.kind, RequestKind::Question { .. }),
        Answering::Question => matches!(p.request.kind, RequestKind::Question { .. }),
        Answering::Any => true,
    };
    if let Some(selector) = selector {
        let needle = selector.trim().to_lowercase();
        let needle = needle.strip_prefix("req_").unwrap_or(&needle);
        let hits: Vec<&PendingRequest> =
            thread.pending.iter().filter(|p| format!("{:x}", p.request.id.0).contains(needle)).collect();
        return match hits.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(CliError::not_found(format!("No pending request matches “{selector}”."))),
            _ => Err(CliError::not_found(format!("“{selector}” matches more than one request."))),
        };
    }
    let candidates: Vec<&PendingRequest> = thread.pending.iter().filter(fits).collect();
    match candidates.as_slice() {
        [one] => Ok((*one).clone()),
        [] if thread.pending.is_empty() => {
            Err(CliError::not_found(format!("“{}” has nothing pending.", crate::json::title(thread))))
        }
        [] => Err(CliError::not_found(match answering {
            Answering::Question => "The pending request is an approval: use `orbit approve` or `orbit deny`.",
            _ => "The pending request is a question: use `orbit answer`.",
        })),
        many => Err(CliError::not_found(format!("“{}” has {} pending requests.", crate::json::title(thread), many.len()))
            .hint(format!(
                "Pick one with --request:\n{}",
                many.iter()
                    .map(|p| format!("  {}  {}", p.request.id, crate::json::request_summary(&p.request.kind)))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(titles: &[&str]) -> (ClientState, Vec<ThreadId>) {
        let mut state = ClientState::default();
        let mut ids = Vec::new();
        for (ix, title) in titles.iter().enumerate() {
            let id = ThreadId::new();
            ids.push(id);
            state.threads.insert(id, Thread { id, title: (*title).into(), updated_ms: ix as u64, ..Default::default() });
        }
        (state, ids)
    }

    #[test]
    fn selects_by_id_title_and_keyword() {
        let (state, ids) = state_with(&["Fix login", "Fix logout", "Docs"]);
        let env = Env { thread: Some(ids[2].to_string()), ..Env::default() };
        assert_eq!(thread(&state, &env, &ids[0].to_string()).unwrap(), ids[0]);
        assert_eq!(thread(&state, &env, &format!("{:x}", ids[1].0)).unwrap(), ids[1]);
        assert_eq!(thread(&state, &env, "docs").unwrap(), ids[2]);
        assert_eq!(thread(&state, &env, "fix login").unwrap(), ids[0], "exact title wins over substring");
        assert_eq!(thread(&state, &env, "latest").unwrap(), ids[2]);
        assert_eq!(thread(&state, &env, "self").unwrap(), ids[2]);
        let err = thread(&state, &env, "fix").unwrap_err();
        assert_eq!(err.exit, crate::spec::exit::NOT_FOUND);
        assert!(err.hint.unwrap().contains(&ids[0].to_string()));
        assert!(thread(&state, &Env::default(), "self").is_err());
        assert!(thread(&state, &env, "nothing like it").is_err());
    }

    #[test]
    fn picks_the_only_fitting_request() {
        let request = |kind| PendingRequest {
            item: ItemId::new(),
            request: AgentRequest { id: RequestId::new(), provider_ref: String::new(), kind, resolution: None },
            created_ms: 0,
        };
        let approval = request(RequestKind::Command { command: "ls".into(), cwd: ".".into(), reason: String::new() });
        let question = request(RequestKind::Question { question: "Which?".into(), options: vec![], multi_select: false });
        let thread = Thread { pending: vec![approval.clone(), question.clone()], ..Default::default() };
        assert_eq!(super::request(&thread, None, Answering::Approval).unwrap(), approval);
        assert_eq!(super::request(&thread, None, Answering::Question).unwrap(), question);
        assert!(super::request(&thread, None, Answering::Any).is_err());
        let id = format!("{}", question.request.id);
        assert_eq!(super::request(&thread, Some(&id), Answering::Any).unwrap(), question);
    }
}
