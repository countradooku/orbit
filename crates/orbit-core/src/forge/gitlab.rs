//! GitLab through the user's own `glab`, using `glab api` so Orbit reads
//! GitLab's documented REST JSON rather than CLI table output.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orbit_proto::*;
use sonic_rs::{JsonValueTrait as _, Value};
use tokio::sync::Semaphore;

use super::cli::{self, Call, Output};
use super::json::{self, arr, b, s, time, u};
use super::{BoxFuture, ForgeError, ForgeHost, IssueTracker, MergeRequest};

const TIMEOUT: Duration = Duration::from_secs(45);

pub(crate) struct GitLab {
    pub glab: PathBuf,
    pub repo: ForgeRepo,
    pub root: PathBuf,
    pub permits: Arc<Semaphore>,
}

impl GitLab {
    /// `projects/<url-encoded path>`.
    fn project(&self) -> String {
        format!("projects/{}", encode(&self.repo.path))
    }

    async fn api(&self, method: &str, endpoint: &str, fields: &[(&str, String)]) -> Result<Output, ForgeError> {
        let mut args = vec!["api".to_owned(), "--hostname".into(), self.repo.host.clone()];
        if method != "GET" {
            args.extend(["-X".into(), method.into()]);
        }
        for (key, value) in fields {
            args.extend(["-f".into(), format!("{key}={value}")]);
        }
        args.push(endpoint.to_owned());
        let call = Call { name: "glab", program: &self.glab, cwd: &self.root, args, stdin: None, timeout: TIMEOUT };
        let out = cli::run(&self.permits, call).await?;
        if out.ok() { Ok(out) } else { Err(classify(&out.message(), &self.repo.host)) }
    }

    async fn get(&self, endpoint: &str, what: &str) -> Result<Value, ForgeError> {
        let out = self.api("GET", endpoint, &[]).await?;
        json::parse(&out.stdout, what)
    }
}

impl ForgeHost for GitLab {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitLab
    }

    fn auth(&self) -> BoxFuture<'_, ForgeAuth> {
        Box::pin(async move {
            let call = Call {
                name: "glab",
                program: &self.glab,
                cwd: &self.root,
                args: vec!["auth".into(), "status".into(), "--hostname".into(), self.repo.host.clone()],
                stdin: None,
                timeout: Duration::from_secs(20),
            };
            let out = cli::run(&self.permits, call).await?;
            Ok(parse_auth(&format!("{}\n{}", out.stdout_text(), out.stderr), &self.repo.host))
        })
    }

    fn pull_requests(&self, filter: PrFilter) -> BoxFuture<'_, Vec<PullRequest>> {
        Box::pin(async move {
            let scope = match filter {
                PrFilter::Open => "state=opened",
                PrFilter::Mine => "state=opened&scope=assigned_to_me",
                PrFilter::Merged => "state=merged",
                PrFilter::Closed => "state=closed",
            };
            let endpoint = format!("{}/merge_requests?{scope}&per_page=50&order_by=updated_at", self.project());
            let v = self.get(&endpoint, "merge requests").await?;
            Ok(json::items(&v).iter().map(parse_mr).collect())
        })
    }

    fn pull_request_for_branch(&self, branch: &str) -> BoxFuture<'_, Option<PullRequest>> {
        let endpoint = format!("{}/merge_requests?source_branch={}&per_page=5&order_by=updated_at", self.project(), encode(branch));
        Box::pin(async move {
            let v = self.get(&endpoint, "merge requests").await?;
            let mut mrs: Vec<PullRequest> = json::items(&v).iter().map(parse_mr).filter(|p| !p.cross_repo).collect();
            mrs.sort_by_key(|p| (!p.state.is_open(), std::cmp::Reverse(p.updated_ms)));
            Ok(mrs.into_iter().next())
        })
    }

    fn pull_request(&self, number: u64) -> BoxFuture<'_, PullRequestDetail> {
        Box::pin(async move {
            let base = format!("{}/merge_requests/{number}", self.project());
            let notes = format!("{base}/notes?sort=asc&order_by=created_at&per_page=100");
            let approvals = format!("{base}/approvals");
            let (mr, notes, approvals) = tokio::join!(
                self.get(&base, "merge request"),
                self.get(&notes, "comments"),
                self.get(&approvals, "approvals")
            );
            let mr = mr?;
            let pipeline = u(&mr, "head_pipeline/id");
            let jobs = if pipeline > 0 {
                let endpoint = format!("{}/pipelines/{pipeline}/jobs?per_page=100", self.project());
                self.get(&endpoint, "pipeline jobs").await.ok()
            } else {
                None
            };
            Ok(parse_mr_detail(&mr, notes.ok().as_ref(), approvals.ok().as_ref(), jobs.as_ref()))
        })
    }

    fn merge(&self, req: MergeRequest) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let base = format!("{}/merge_requests/{}", self.project(), req.number);
            let mut fields = vec![
                ("squash", (req.method == MergeMethod::Squash).to_string()),
                ("should_remove_source_branch", req.delete_branch.to_string()),
            ];
            if !req.head_sha.is_empty() {
                fields.push(("sha", req.head_sha.clone()));
            }
            if req.auto {
                // `auto_merge` on GitLab 17+, the older name before it.
                fields.push(("auto_merge", "true".into()));
                fields.push(("merge_when_pipeline_succeeds", "true".into()));
            }
            self.api("PUT", &format!("{base}/merge"), &fields).await.map(|_| ())
        })
    }

    fn disable_auto_merge(&self, number: u64) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let endpoint = format!("{}/merge_requests/{number}/cancel_merge_when_pipeline_succeeds", self.project());
            self.api("POST", &endpoint, &[]).await.map(|_| ())
        })
    }

    fn failed_log(&self, check: &Check) -> BoxFuture<'_, String> {
        let job = check.job.clone();
        Box::pin(async move {
            if job.is_empty() {
                return Ok(String::new());
            }
            let out = self.api("GET", &format!("{}/jobs/{job}/trace", self.project()), &[]).await?;
            Ok(clean_trace(&out.stdout_text()))
        })
    }

    fn pr_ref(&self, number: u64) -> String {
        format!("refs/merge-requests/{number}/head")
    }
}

impl IssueTracker for GitLab {
    fn kind(&self) -> TrackerKind {
        TrackerKind::GitLab
    }

    fn issues(&self, query: &str) -> BoxFuture<'_, Vec<Issue>> {
        let query = query.trim().to_owned();
        Box::pin(async move {
            let mut endpoint = format!("{}/issues?per_page=50&order_by=updated_at", self.project());
            if query.is_empty() {
                endpoint.push_str("&state=opened");
            } else {
                endpoint.push_str(&format!("&search={}", encode(&query)));
            }
            let v = self.get(&endpoint, "issues").await?;
            Ok(json::items(&v).iter().map(parse_issue).collect())
        })
    }

    fn issue(&self, key: &str) -> BoxFuture<'_, IssueDetail> {
        let iid = key.trim_start_matches('#').to_owned();
        Box::pin(async move {
            let base = format!("{}/issues/{iid}", self.project());
            let notes = format!("{base}/notes?sort=asc&order_by=created_at&per_page=100");
            let (issue, notes) = tokio::join!(self.get(&base, "issue"), self.get(&notes, "comments"));
            let issue = issue?;
            Ok(IssueDetail {
                body: s(&issue, "description"),
                comments: notes.map(|n| parse_notes(&n)).unwrap_or_default(),
                issue: parse_issue(&issue),
            })
        })
    }
}

// ── Parsing ──────────────────────────────────────────────────────────────

/// Percent-encodes a path segment or query value.
pub(crate) fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub(crate) fn classify(message: &str, host: &str) -> ForgeError {
    let lower = message.to_ascii_lowercase();
    if lower.contains("401") || lower.contains("glab auth login") || lower.contains("unauthorized") || lower.contains("no token") {
        return ForgeError::SignedOut { program: "glab".into(), fix: format!("glab auth login --hostname {host}") };
    }
    if lower.contains("429") || lower.contains("rate limit") {
        return ForgeError::RateLimited { host: host.to_owned(), reset_ms: 0 };
    }
    if lower.contains("404") || lower.contains("not found") {
        return ForgeError::NotFound(message.trim().to_owned());
    }
    super::github::classify(message, host)
}

pub(crate) fn parse_auth(text: &str, host: &str) -> ForgeAuth {
    for line in text.lines() {
        if let Some(rest) = line.split("Logged in to ").nth(1) {
            let (h, rest) = rest.split_once(' ').unwrap_or((rest, ""));
            if h == host {
                let account = rest.strip_prefix("as ").and_then(|r| r.split_whitespace().next()).unwrap_or_default();
                return ForgeAuth::Ready { account: account.to_owned() };
            }
        }
    }
    ForgeAuth::SignedOut { program: "glab".into(), fix: format!("glab auth login --hostname {host}") }
}

pub(crate) fn parse_mr(v: &Value) -> PullRequest {
    let state = match s(v, "state").as_str() {
        "merged" => PrState::Merged,
        "closed" | "locked" => PrState::Closed,
        _ if b(v, "draft") || b(v, "work_in_progress") => PrState::Draft,
        _ => PrState::Open,
    };
    let mut checks = ChecksSummary::default();
    if let Some(status) = json::opt_s(v, "head_pipeline/status") {
        checks.add(job_state(&status));
    }
    PullRequest {
        number: u(v, "iid"),
        title: s(v, "title"),
        author: s(v, "author/username"),
        state,
        head: s(v, "source_branch"),
        base: s(v, "target_branch"),
        url: s(v, "web_url"),
        created_ms: time(v, "created_at"),
        updated_ms: time(v, "updated_at"),
        checks,
        review: ReviewDecision::None,
        additions: 0,
        deletions: 0,
        labels: arr(v, "labels").iter().filter_map(|l| l.as_str().map(str::to_owned)).collect(),
        cross_repo: u(v, "source_project_id") != u(v, "target_project_id"),
    }
}

pub(crate) fn job_state(status: &str) -> CheckState {
    match status {
        "success" => CheckState::Passed,
        "failed" => CheckState::Failed,
        "running" => CheckState::Running,
        "canceled" | "canceling" => CheckState::Cancelled,
        "skipped" => CheckState::Skipped,
        "manual" => CheckState::Neutral,
        _ => CheckState::Queued, // created, pending, preparing, scheduled, waiting_for_resource
    }
}

pub(crate) fn parse_mr_detail(mr: &Value, notes: Option<&Value>, approvals: Option<&Value>, jobs: Option<&Value>) -> PullRequestDetail {
    let mut pr = parse_mr(mr);
    let mut checks: Vec<Check> = jobs
        .map(|jobs| {
            json::items(jobs)
                .iter()
                .map(|j| {
                    let mut state = job_state(&s(j, "status"));
                    // An allowed failure does not fail the pipeline.
                    if state == CheckState::Failed && b(j, "allow_failure") {
                        state = CheckState::Neutral;
                    }
                    Check {
                        name: s(j, "name"),
                        group: s(j, "stage"),
                        state,
                        started_ms: time(j, "started_at"),
                        finished_ms: time(j, "finished_at"),
                        url: s(j, "web_url"),
                        job: u(j, "id").to_string(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    checks.sort_by_key(|c| (super::github::check_rank(c.state), c.group.clone(), c.name.clone()));
    if !checks.is_empty() {
        let mut summary = ChecksSummary::default();
        for c in &checks {
            summary.add(c.state);
        }
        pr.checks = summary;
    }
    let mergeable = match json::opt_s(mr, "detailed_merge_status").or_else(|| json::opt_s(mr, "merge_status")).as_deref() {
        _ if pr.state == PrState::Draft => Mergeability::Draft,
        Some("mergeable" | "can_be_merged") => Mergeability::Clean,
        Some("conflict" | "broken_status" | "cannot_be_merged") => Mergeability::Conflicting,
        Some("need_rebase") => Mergeability::Behind,
        Some("draft_status") => Mergeability::Draft,
        Some("ci_must_pass" | "ci_still_running") => Mergeability::Blocked,
        Some("not_approved" | "discussions_not_resolved" | "blocked_status" | "not_open" | "jira_association_missing") => {
            Mergeability::Blocked
        }
        _ => Mergeability::Unknown,
    };
    let reviews: Vec<Review> = approvals
        .map(|a| {
            arr(a, "approved_by")
                .iter()
                .map(|x| Review {
                    author: s(x, "user/username"),
                    state: ReviewState::Approved,
                    body: String::new(),
                    submitted_ms: 0,
                })
                .collect()
        })
        .unwrap_or_default();
    if !reviews.is_empty() {
        pr.review = ReviewDecision::Approved;
    } else if approvals.is_some_and(|a| u(a, "approvals_left") > 0) {
        pr.review = ReviewDecision::ReviewRequired;
    }
    let auto = b(mr, "merge_when_pipeline_succeeds") || b(mr, "auto_merge_enabled");
    PullRequestDetail {
        body: s(mr, "description"),
        head_sha: s(mr, "sha"),
        mergeable,
        merge_methods: vec![MergeMethod::Merge, MergeMethod::Squash],
        auto_merge: auto.then(|| if b(mr, "squash") { MergeMethod::Squash } else { MergeMethod::Merge }),
        checks,
        reviews,
        comments: notes.map(parse_notes).unwrap_or_default(),
        changed_files: s(mr, "changes_count").trim_end_matches('+').parse().unwrap_or(0),
        pr,
    }
}

/// User comments, without GitLab's system notes ("added 1 commit").
pub(crate) fn parse_notes(notes: &Value) -> Vec<Comment> {
    json::items(notes)
        .iter()
        .filter(|n| !b(n, "system"))
        .map(|n| Comment {
            author: s(n, "author/username"),
            body: s(n, "body"),
            created_ms: time(n, "created_at"),
            path: json::opt_s(n, "position/new_path"),
            line: json::at(n, "position/new_line").and_then(|_| u32::try_from(u(n, "position/new_line")).ok()),
        })
        .collect()
}

pub(crate) fn parse_issue(v: &Value) -> Issue {
    let closed = s(v, "state") == "closed";
    Issue {
        tracker: TrackerKind::GitLab,
        key: format!("#{}", u(v, "iid")),
        title: s(v, "title"),
        state: if closed { IssueState::Closed } else { IssueState::Open },
        status: if closed { "Closed".into() } else { "Open".into() },
        author: s(v, "author/username"),
        assignee: arr(v, "assignees").first().map(|a| s(a, "username")).filter(|a| !a.is_empty()),
        labels: arr(v, "labels").iter().filter_map(|l| l.as_str().map(str::to_owned)).collect(),
        url: s(v, "web_url"),
        updated_ms: time(v, "updated_at"),
        comments: u(v, "user_notes_count") as u32,
        branch: None,
    }
}

/// A job trace with its section markers and colors removed.
pub(crate) fn clean_trace(raw: &str) -> String {
    let raw = cli::strip_ansi(raw);
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        // `section_start:1700000000:step_script\r` markers.
        let visible = line.rsplit('\r').next().unwrap_or(line);
        let marker_only = visible.is_empty() && line.len() > visible.len();
        if marker_only || visible.starts_with("section_start:") || visible.starts_with("section_end:") {
            continue;
        }
        out.push_str(visible);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/forge/{name}", env!("CARGO_MANIFEST_DIR"));
        json::parse(&std::fs::read(path).unwrap(), name).unwrap()
    }

    #[test]
    fn merge_requests_parse() {
        let v = fixture("glab_mr_list.json");
        let mrs: Vec<PullRequest> = json::items(&v).iter().map(parse_mr).collect();
        assert_eq!(mrs.len(), 2);
        assert_eq!(mrs[0].number, 12);
        assert_eq!(mrs[0].state, PrState::Draft);
        assert_eq!(mrs[0].head, "feature/login");
        assert_eq!(mrs[0].labels, vec!["backend".to_owned()]);
        assert!(!mrs[0].cross_repo);
        assert!(mrs[1].cross_repo);
        assert_eq!(mrs[1].checks.overall(), Some(CheckState::Failed));
    }

    #[test]
    fn a_merge_request_detail_parses_jobs_notes_and_approvals() {
        let mr = fixture("glab_mr.json");
        let d = parse_mr_detail(&mr, Some(&fixture("glab_notes.json")), Some(&fixture("glab_approvals.json")), Some(&fixture("glab_jobs.json")));
        assert_eq!(d.pr.number, 13);
        assert_eq!(d.mergeable, Mergeability::Blocked);
        assert_eq!(d.head_sha, "b83d6e391c22777fca1ed3012fce84f633d7fed0");
        assert_eq!(d.checks.len(), 3);
        assert_eq!(d.checks[0].state, CheckState::Failed);
        assert_eq!(d.checks[0].job, "8");
        assert_eq!(d.checks.iter().find(|c| c.name == "lint").unwrap().state, CheckState::Neutral, "allowed failure");
        assert_eq!(d.pr.checks, ChecksSummary { passed: 1, failed: 1, pending: 0, other: 1 });
        assert_eq!(d.comments.len(), 1, "system notes are dropped");
        assert_eq!(d.comments[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(d.comments[0].line, Some(42));
        assert_eq!(d.reviews.len(), 1);
        assert_eq!(d.pr.review, ReviewDecision::Approved);
        assert_eq!(d.auto_merge, Some(MergeMethod::Squash));
        assert_eq!(d.changed_files, 4);
    }

    #[test]
    fn auth_and_paths() {
        assert_eq!(
            parse_auth("gitlab.com\n  ✓ Logged in to gitlab.com as alice (C:\\Users\\a\\glab\\config.yml)\n", "gitlab.com"),
            ForgeAuth::Ready { account: "alice".into() }
        );
        assert!(matches!(parse_auth("x gitlab.com: api call failed", "gitlab.com"), ForgeAuth::SignedOut { .. }));
        assert_eq!(encode("group/sub proj"), "group%2Fsub%20proj");
        assert!(matches!(classify("glab: 401 Unauthorized (HTTP 401)", "gitlab.com"), ForgeError::SignedOut { .. }));
        assert!(matches!(classify("glab: 404 Not Found (HTTP 404)", "gitlab.com"), ForgeError::NotFound(_)));
    }

    #[test]
    fn traces_lose_section_markers() {
        let raw = "section_start:1:step_script\r\u{1b}[0KRunning tests\nok\nsection_end:1:step_script\r\u{1b}[0K\n";
        assert_eq!(clean_trace(raw), "Running tests\nok\n");
    }
}
