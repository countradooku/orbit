//! GitHub through the user's own `gh`: JSON output, no token of Orbit's.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use orbit_proto::*;
use sonic_rs::Value;
use tokio::sync::Semaphore;

use super::cli::{self, Call, Output};
use super::json::{self, arr, b, s, time, u};
use super::{BoxFuture, ForgeError, ForgeHost, IssueTracker, MergeRequest};

const TIMEOUT: Duration = Duration::from_secs(45);
/// Pull requests per list. One GraphQL query; enough for a working set.
const LIST_LIMIT: &str = "50";

const PR_FIELDS: &str = "number,title,author,state,isDraft,headRefName,baseRefName,url,createdAt,updatedAt,\
statusCheckRollup,reviewDecision,additions,deletions,labels,isCrossRepository";
const PR_DETAIL_FIELDS: &str = "body,headRefOid,mergeable,mergeStateStatus,autoMergeRequest,reviews,comments,changedFiles";
const ISSUE_FIELDS: &str = "number,title,state,author,assignees,labels,url,updatedAt";

pub(crate) struct GitHub {
    pub gh: PathBuf,
    pub repo: ForgeRepo,
    pub root: PathBuf,
    pub permits: Arc<Semaphore>,
}

impl GitHub {
    /// `owner/name`, or `host/owner/name` for GitHub Enterprise.
    fn repo_arg(&self) -> String {
        if self.repo.host == "github.com" { self.repo.path.clone() } else { format!("{}/{}", self.repo.host, self.repo.path) }
    }

    async fn run(&self, args: Vec<String>, stdin: Option<&[u8]>) -> Result<Output, ForgeError> {
        let call = Call { name: "gh", program: &self.gh, cwd: &self.root, args, stdin, timeout: TIMEOUT };
        let out = cli::run(&self.permits, call).await?;
        if out.ok() { Ok(out) } else { Err(classify(&out.message(), &self.repo.host)) }
    }

    async fn json(&self, args: Vec<String>, what: &str) -> Result<Value, ForgeError> {
        let out = self.run(args, None).await?;
        json::parse(&out.stdout, what)
    }

    fn args(&self, parts: &[&str]) -> Vec<String> {
        let mut args: Vec<String> = parts.iter().map(|p| p.to_string()).collect();
        args.push("--repo".into());
        args.push(self.repo_arg());
        args
    }

    async fn merge_methods(&self) -> Vec<MergeMethod> {
        let args = vec![
            "repo".into(),
            "view".into(),
            self.repo_arg(),
            "--json".into(),
            "mergeCommitAllowed,squashMergeAllowed,rebaseMergeAllowed".into(),
        ];
        match self.json(args, "repository settings").await {
            Ok(v) => parse_merge_methods(&v),
            Err(_) => Vec::new(),
        }
    }
}

impl ForgeHost for GitHub {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    fn auth(&self) -> BoxFuture<'_, ForgeAuth> {
        Box::pin(async move {
            let call = Call {
                name: "gh",
                program: &self.gh,
                cwd: &self.root,
                args: vec!["auth".into(), "status".into(), "--hostname".into(), self.repo.host.clone()],
                stdin: None,
                timeout: Duration::from_secs(20),
            };
            let out = cli::run(&self.permits, call).await?;
            let text = format!("{}\n{}", out.stdout_text(), out.stderr);
            Ok(parse_auth(&text, &self.repo.host))
        })
    }

    fn pull_requests(&self, filter: PrFilter) -> BoxFuture<'_, Vec<PullRequest>> {
        Box::pin(async move {
            let mut args = self.args(&["pr", "list", "--limit", LIST_LIMIT, "--json", PR_FIELDS]);
            match filter {
                PrFilter::Open => args.extend(["--state".into(), "open".into()]),
                PrFilter::Mine => args.extend(["--state".into(), "open".into(), "--search".into(), "involves:@me".into()]),
                PrFilter::Merged => args.extend(["--state".into(), "merged".into()]),
                PrFilter::Closed => args.extend(["--state".into(), "closed".into()]),
            }
            let v = self.json(args, "pull requests").await?;
            let mut prs: Vec<PullRequest> = json::items(&v).iter().map(parse_pr).collect();
            if filter == PrFilter::Closed {
                // `closed` includes merged ones; those have their own list.
                prs.retain(|p| p.state == PrState::Closed);
            }
            Ok(prs)
        })
    }

    fn pull_request_for_branch(&self, branch: &str) -> BoxFuture<'_, Option<PullRequest>> {
        let branch = branch.to_owned();
        Box::pin(async move {
            let args = self.args(&["pr", "list", "--head", &branch, "--state", "all", "--limit", "5", "--json", PR_FIELDS]);
            let v = self.json(args, "pull requests").await?;
            let mut prs: Vec<PullRequest> = json::items(&v).iter().map(parse_pr).filter(|p| !p.cross_repo).collect();
            // An open one wins over an old merged one (T3 Code #4970).
            prs.sort_by_key(|p| (!p.state.is_open(), std::cmp::Reverse(p.updated_ms)));
            Ok(prs.into_iter().next())
        })
    }

    fn pull_request(&self, number: u64) -> BoxFuture<'_, PullRequestDetail> {
        Box::pin(async move {
            let fields = format!("{PR_FIELDS},{PR_DETAIL_FIELDS}");
            let n = number.to_string();
            let detail = self.json(self.args(&["pr", "view", &n, "--json", &fields]), "pull request");
            let (detail, methods) = tokio::join!(detail, self.merge_methods());
            Ok(parse_pr_detail(&detail?, methods))
        })
    }

    fn merge(&self, req: MergeRequest) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let n = req.number.to_string();
            let method = match req.method {
                MergeMethod::Merge => "--merge",
                MergeMethod::Squash => "--squash",
                MergeMethod::Rebase => "--rebase",
            };
            // Never `--delete-branch`: gh would also switch and delete the
            // local branch in the user's checkout. The remote branch is
            // deleted through the API instead.
            let mut args = self.args(&["pr", "merge", &n, method]);
            if req.auto {
                args.push("--auto".into());
            }
            if !req.head_sha.is_empty() {
                args.extend(["--match-head-commit".into(), req.head_sha.clone()]);
            }
            self.run(args, None).await?;
            if req.delete_branch && !req.auto && !req.head.is_empty() && !req.cross_repo {
                let path = format!("repos/{}/git/refs/heads/{}", self.repo.path, req.head);
                let mut args = vec!["api".into(), "-X".into(), "DELETE".into(), path];
                if self.repo.host != "github.com" {
                    args.extend(["--hostname".into(), self.repo.host.clone()]);
                }
                // Already gone (the repository deletes merged branches) is fine.
                if let Err(e) = self.run(args, None).await
                    && !matches!(e, ForgeError::NotFound(_))
                    && !e.to_string().contains("Reference does not exist")
                {
                    log::warn!("deleting the merged branch failed: {e}");
                }
            }
            Ok(())
        })
    }

    fn disable_auto_merge(&self, number: u64) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let n = number.to_string();
            self.run(self.args(&["pr", "merge", &n, "--disable-auto"]), None).await.map(|_| ())
        })
    }

    fn failed_log(&self, check: &Check) -> BoxFuture<'_, String> {
        let job = check.job.clone();
        Box::pin(async move {
            if job.is_empty() {
                return Ok(String::new());
            }
            let out = self.run(self.args(&["run", "view", "--job", &job, "--log-failed"]), None).await?;
            Ok(clean_actions_log(&out.stdout_text()))
        })
    }

    fn pr_ref(&self, number: u64) -> String {
        format!("refs/pull/{number}/head")
    }
}

impl IssueTracker for GitHub {
    fn kind(&self) -> TrackerKind {
        TrackerKind::GitHub
    }

    fn issues(&self, query: &str) -> BoxFuture<'_, Vec<Issue>> {
        let query = query.trim().to_owned();
        Box::pin(async move {
            let mut args = self.args(&["issue", "list", "--limit", LIST_LIMIT, "--json", ISSUE_FIELDS]);
            if query.is_empty() {
                args.extend(["--state".into(), "open".into()]);
            } else {
                args.extend(["--state".into(), "all".into(), "--search".into(), query]);
            }
            let v = self.json(args, "issues").await?;
            Ok(json::items(&v).iter().map(parse_issue).collect())
        })
    }

    fn issue(&self, key: &str) -> BoxFuture<'_, IssueDetail> {
        let n = key.trim_start_matches('#').to_owned();
        Box::pin(async move {
            let fields = format!("{ISSUE_FIELDS},body,comments");
            let v = self.json(self.args(&["issue", "view", &n, "--json", &fields]), "issue").await?;
            Ok(parse_issue_detail(&v))
        })
    }
}

// ── Parsing ──────────────────────────────────────────────────────────────

/// Turns a failed `gh` run into a typed error with the fix.
pub(crate) fn classify(message: &str, host: &str) -> ForgeError {
    let lower = message.to_ascii_lowercase();
    if lower.contains("gh auth login")
        || lower.contains("not logged in")
        || lower.contains("authentication required")
        || lower.contains("http 401")
        || lower.contains("bad credentials")
    {
        return ForgeError::SignedOut { program: "gh".into(), fix: format!("gh auth login --hostname {host}") };
    }
    if lower.contains("rate limit") || lower.contains("http 429") || lower.contains("abuse detection") {
        return ForgeError::RateLimited { host: host.to_owned(), reset_ms: 0 };
    }
    if lower.contains("could not resolve to")
        || lower.contains("no pull requests found")
        || lower.contains("http 404")
        || lower.contains("not found")
    {
        return ForgeError::NotFound(message.trim().to_owned());
    }
    if lower.contains("could not resolve host") || lower.contains("connection refused") || lower.contains("no such host") {
        return ForgeError::Offline(message.trim().to_owned());
    }
    ForgeError::Failed(message.trim().to_owned())
}

/// Reads `gh auth status`: signed in when it reports an account for the
/// host, whatever else it says about other accounts.
pub(crate) fn parse_auth(text: &str, host: &str) -> ForgeAuth {
    // (account, active) per "Logged in to <host>" line; newer gh lists every
    // account with an "Active account" line under it.
    let mut accounts: Vec<(String, bool)> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.split("Logged in to ").nth(1) {
            let (h, rest) = rest.split_once(' ').unwrap_or((rest, ""));
            if h == host {
                let name = rest.strip_prefix("account ").or_else(|| rest.strip_prefix("as ")).and_then(|r| r.split_whitespace().next());
                accounts.push((name.unwrap_or_default().to_owned(), false));
            }
        } else if line.contains("Active account: true")
            && let Some(last) = accounts.last_mut()
        {
            last.1 = true;
        }
    }
    let account = accounts.iter().find(|(_, active)| *active).or(accounts.first()).map(|(name, _)| name.clone());
    match account {
        Some(account) => ForgeAuth::Ready { account },
        None => ForgeAuth::SignedOut { program: "gh".into(), fix: format!("gh auth login --hostname {host}") },
    }
}

pub(crate) fn parse_pr(v: &Value) -> PullRequest {
    let state = match s(v, "state").as_str() {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ if b(v, "isDraft") => PrState::Draft,
        _ => PrState::Open,
    };
    let review = match s(v, "reviewDecision").as_str() {
        "APPROVED" => ReviewDecision::Approved,
        "CHANGES_REQUESTED" => ReviewDecision::ChangesRequested,
        "REVIEW_REQUIRED" => ReviewDecision::ReviewRequired,
        _ => ReviewDecision::None,
    };
    let (_, checks) = parse_checks(arr(v, "statusCheckRollup"));
    PullRequest {
        number: u(v, "number"),
        title: s(v, "title"),
        author: s(v, "author/login"),
        state,
        head: s(v, "headRefName"),
        base: s(v, "baseRefName"),
        url: s(v, "url"),
        created_ms: time(v, "createdAt"),
        updated_ms: time(v, "updatedAt"),
        checks,
        review,
        additions: u(v, "additions") as u32,
        deletions: u(v, "deletions") as u32,
        labels: arr(v, "labels").iter().map(|l| s(l, "name")).filter(|n| !n.is_empty()).collect(),
        cross_repo: b(v, "isCrossRepository"),
    }
}

/// Check runs and commit statuses, failures first.
pub(crate) fn parse_checks(rollup: &[Value]) -> (Vec<Check>, ChecksSummary) {
    let mut checks: Vec<Check> = rollup
        .iter()
        .map(|c| {
            let status_context = s(c, "__typename") == "StatusContext";
            let state = if status_context {
                match s(c, "state").as_str() {
                    "SUCCESS" => CheckState::Passed,
                    "FAILURE" | "ERROR" => CheckState::Failed,
                    "PENDING" | "EXPECTED" => CheckState::Running,
                    _ => CheckState::Neutral,
                }
            } else {
                match (s(c, "status").as_str(), s(c, "conclusion").as_str()) {
                    ("COMPLETED", "SUCCESS") => CheckState::Passed,
                    ("COMPLETED", "FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE") => CheckState::Failed,
                    ("COMPLETED", "CANCELLED") => CheckState::Cancelled,
                    ("COMPLETED", "SKIPPED") => CheckState::Skipped,
                    ("COMPLETED", _) => CheckState::Neutral,
                    ("IN_PROGRESS", _) => CheckState::Running,
                    _ => CheckState::Queued,
                }
            };
            let url = if status_context { s(c, "targetUrl") } else { s(c, "detailsUrl") };
            Check {
                name: if status_context { s(c, "context") } else { s(c, "name") },
                group: s(c, "workflowName"),
                state,
                started_ms: time(c, "startedAt"),
                finished_ms: time(c, "completedAt"),
                job: if status_context { String::new() } else { actions_job(&url) },
                url,
            }
        })
        .collect();
    checks.sort_by_key(|c| (check_rank(c.state), c.group.clone(), c.name.clone()));
    let mut summary = ChecksSummary::default();
    for c in &checks {
        summary.add(c.state);
    }
    (checks, summary)
}

pub(crate) fn check_rank(state: CheckState) -> u8 {
    match state {
        CheckState::Failed => 0,
        CheckState::Running => 1,
        CheckState::Queued => 2,
        CheckState::Cancelled => 3,
        CheckState::Passed => 4,
        CheckState::Neutral => 5,
        CheckState::Skipped => 6,
    }
}

/// The Actions job id in `…/actions/runs/<run>/job/<job>`.
pub(crate) fn actions_job(url: &str) -> String {
    let mut parts = url.split('/');
    while let Some(part) = parts.next() {
        if part == "job"
            && let Some(id) = parts.next()
            && !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
        {
            return id.to_owned();
        }
    }
    String::new()
}

pub(crate) fn parse_merge_methods(v: &Value) -> Vec<MergeMethod> {
    let mut methods = Vec::new();
    if b(v, "squashMergeAllowed") {
        methods.push(MergeMethod::Squash);
    }
    if b(v, "mergeCommitAllowed") {
        methods.push(MergeMethod::Merge);
    }
    if b(v, "rebaseMergeAllowed") {
        methods.push(MergeMethod::Rebase);
    }
    methods
}

fn merge_method(name: &str) -> Option<MergeMethod> {
    match name {
        "MERGE" => Some(MergeMethod::Merge),
        "SQUASH" => Some(MergeMethod::Squash),
        "REBASE" => Some(MergeMethod::Rebase),
        _ => None,
    }
}

pub(crate) fn parse_pr_detail(v: &Value, merge_methods: Vec<MergeMethod>) -> PullRequestDetail {
    let pr = parse_pr(v);
    let (checks, _) = parse_checks(arr(v, "statusCheckRollup"));
    let mergeable = match (s(v, "mergeable").as_str(), s(v, "mergeStateStatus").as_str()) {
        _ if pr.state == PrState::Draft => Mergeability::Draft,
        ("CONFLICTING", _) | (_, "DIRTY") => Mergeability::Conflicting,
        (_, "CLEAN" | "HAS_HOOKS") => Mergeability::Clean,
        (_, "UNSTABLE") => Mergeability::Unstable,
        (_, "BEHIND") => Mergeability::Behind,
        (_, "BLOCKED") => Mergeability::Blocked,
        (_, "DRAFT") => Mergeability::Draft,
        _ => Mergeability::Unknown,
    };
    let reviews = arr(v, "reviews")
        .iter()
        .map(|r| Review {
            author: s(r, "author/login"),
            state: match s(r, "state").as_str() {
                "APPROVED" => ReviewState::Approved,
                "CHANGES_REQUESTED" => ReviewState::ChangesRequested,
                "DISMISSED" => ReviewState::Dismissed,
                "PENDING" => ReviewState::Pending,
                _ => ReviewState::Commented,
            },
            body: s(r, "body"),
            submitted_ms: time(r, "submittedAt"),
        })
        .collect();
    PullRequestDetail {
        body: s(v, "body"),
        head_sha: s(v, "headRefOid"),
        mergeable,
        merge_methods,
        auto_merge: json::at(v, "autoMergeRequest").map(|a| merge_method(&s(a, "mergeMethod")).unwrap_or_default()),
        checks,
        reviews,
        comments: parse_comments(arr(v, "comments")),
        changed_files: u(v, "changedFiles") as u32,
        pr,
    }
}

fn parse_comments(items: &[Value]) -> Vec<Comment> {
    let mut comments: Vec<Comment> = items
        .iter()
        .map(|c| Comment {
            author: s(c, "author/login"),
            body: s(c, "body"),
            created_ms: time(c, "createdAt"),
            path: None,
            line: None,
        })
        .collect();
    comments.sort_by_key(|c| c.created_ms);
    comments
}

pub(crate) fn parse_issue(v: &Value) -> Issue {
    let number = u(v, "number");
    Issue {
        tracker: TrackerKind::GitHub,
        key: format!("#{number}"),
        title: s(v, "title"),
        state: if s(v, "state") == "CLOSED" { IssueState::Closed } else { IssueState::Open },
        status: if s(v, "state") == "CLOSED" { "Closed".into() } else { "Open".into() },
        author: s(v, "author/login"),
        assignee: arr(v, "assignees").first().map(|a| s(a, "login")).filter(|a| !a.is_empty()),
        labels: arr(v, "labels").iter().map(|l| s(l, "name")).filter(|n| !n.is_empty()).collect(),
        url: s(v, "url"),
        updated_ms: time(v, "updatedAt"),
        comments: arr(v, "comments").len() as u32,
        branch: None,
    }
}

pub(crate) fn parse_issue_detail(v: &Value) -> IssueDetail {
    IssueDetail { issue: parse_issue(v), body: s(v, "body"), comments: parse_comments(arr(v, "comments")) }
}

/// `gh run view --log-failed` prints `job<TAB>step<TAB>timestamp text`.
/// Keeps the text, drops log-group markers and timestamps.
pub(crate) fn clean_actions_log(raw: &str) -> String {
    let raw = cli::strip_ansi(raw);
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        let text = strip_stamp(line.splitn(3, '\t').nth(2).unwrap_or(line));
        if text.starts_with("##[group]") || text.starts_with("##[endgroup]") {
            continue;
        }
        match text.strip_prefix("##[error]") {
            Some(error) => {
                out.push_str("error: ");
                out.push_str(error);
            }
            None => out.push_str(text),
        }
        out.push('\n');
    }
    out
}

fn strip_stamp(text: &str) -> &str {
    let text = text.trim_start_matches('\u{feff}');
    if text.len() > 28 && text.as_bytes()[4] == b'-' && text.as_bytes()[10] == b'T' {
        text.split_once(' ').map(|(_, rest)| rest).unwrap_or("")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/forge/{name}", env!("CARGO_MANIFEST_DIR"));
        json::parse(&std::fs::read(path).unwrap(), name).unwrap()
    }

    #[test]
    fn a_recorded_pr_list_parses() {
        let v = fixture("gh_pr_list.json");
        let prs: Vec<PullRequest> = json::items(&v).iter().map(parse_pr).collect();
        assert_eq!(prs.len(), 3);
        let first = &prs[0];
        assert!(first.number > 0 && !first.title.is_empty() && !first.author.is_empty());
        assert!(first.url.starts_with("https://github.com/cli/cli/pull/"));
        assert!(first.updated_ms > 1_700_000_000_000);
        assert_eq!(first.base, "trunk");
        let with_checks = prs.iter().find(|p| p.checks.total() > 0).expect("a PR with checks");
        assert_eq!(with_checks.checks.overall(), Some(CheckState::Passed));
    }

    #[test]
    fn a_recorded_pr_detail_parses() {
        let v = fixture("gh_pr_view.json");
        let d = parse_pr_detail(&v, vec![MergeMethod::Squash, MergeMethod::Merge]);
        assert_eq!(d.pr.number, 15108);
        assert_eq!(d.mergeable, Mergeability::Conflicting);
        assert_eq!(d.head_sha.len(), 40);
        assert_eq!(d.checks.len(), 18);
        assert!(d.checks.iter().all(|c| !c.name.is_empty()));
        let job = d.checks.iter().find(|c| !c.job.is_empty()).expect("an Actions job");
        assert!(job.url.ends_with(&job.job));
        assert!(job.finished_ms >= job.started_ms && job.started_ms > 0);
        assert_eq!(d.reviews.len(), 2);
        assert!(d.reviews.iter().any(|r| r.state == ReviewState::Approved));
        assert!(d.body.contains("expiring"));
        assert_eq!(d.auto_merge, None);
    }

    #[test]
    fn recorded_issues_parse() {
        let v = fixture("gh_issue_list.json");
        let issues: Vec<Issue> = json::items(&v).iter().map(parse_issue).collect();
        assert_eq!(issues.len(), 2);
        assert!(issues[0].key.starts_with('#'));
        assert!(issues[0].labels.contains(&"bug".to_owned()));
        let d = parse_issue_detail(&fixture("gh_issue_view.json"));
        assert!(d.body.contains("Describe the bug"));
        assert_eq!(d.issue.state, IssueState::Open);
    }

    #[test]
    fn checks_map_every_state_and_sort_failures_first() {
        let rollup: Value = sonic_rs::from_str(
            r#"[
            {"__typename":"CheckRun","name":"build","status":"COMPLETED","conclusion":"SUCCESS","workflowName":"CI"},
            {"__typename":"CheckRun","name":"test","status":"COMPLETED","conclusion":"FAILURE","workflowName":"CI",
             "detailsUrl":"https://github.com/o/r/actions/runs/11/job/22","startedAt":"2024-01-01T00:00:00Z","completedAt":"2024-01-01T00:02:05Z"},
            {"__typename":"CheckRun","name":"e2e","status":"IN_PROGRESS","conclusion":null},
            {"__typename":"CheckRun","name":"docs","status":"COMPLETED","conclusion":"SKIPPED"},
            {"__typename":"StatusContext","context":"ci/external","state":"ERROR","targetUrl":"https://ci.example/1"}
        ]"#,
        )
        .unwrap();
        let (checks, summary) = parse_checks(json::items(&rollup));
        assert_eq!(summary, ChecksSummary { passed: 1, failed: 2, pending: 1, other: 1 });
        assert_eq!(checks[0].state, CheckState::Failed);
        let test = checks.iter().find(|c| c.name == "test").unwrap();
        assert_eq!(test.job, "22");
        assert_eq!(test.finished_ms - test.started_ms, 125_000);
        assert_eq!(checks.iter().find(|c| c.name == "ci/external").unwrap().job, "");
    }

    #[test]
    fn auth_status_reports_the_active_account() {
        let text = "github.com\n  ✓ Logged in to github.com account alice (keyring)\n  - Active account: true\n\n  ✓ Logged in to github.com account bob (keyring)\n  - Active account: false\n";
        assert_eq!(parse_auth(text, "github.com"), ForgeAuth::Ready { account: "alice".into() });
        let old = "github.com\n  ✓ Logged in to github.com as carol (oauth_token)\n";
        assert_eq!(parse_auth(old, "github.com"), ForgeAuth::Ready { account: "carol".into() });
        let out = "You are not logged into any GitHub hosts. To log in, run: gh auth login\n";
        assert_eq!(
            parse_auth(out, "github.com"),
            ForgeAuth::SignedOut { program: "gh".into(), fix: "gh auth login --hostname github.com".into() }
        );
    }

    #[test]
    fn errors_are_classified_with_their_fix() {
        assert!(matches!(classify("To get started with GitHub CLI, please run:  gh auth login", "github.com"), ForgeError::SignedOut { .. }));
        assert!(matches!(
            classify("GraphQL: API rate limit exceeded for user ID 1.", "github.com"),
            ForgeError::RateLimited { .. }
        ));
        assert!(matches!(classify("GraphQL: Could not resolve to a PullRequest with the number of 9.", "github.com"), ForgeError::NotFound(_)));
        assert!(matches!(classify("something else", "github.com"), ForgeError::Failed(_)));
    }

    #[test]
    fn actions_logs_lose_their_noise() {
        let raw = "test\tRun tests\t\u{feff}2026-11-24T22:40:07.7842481Z ##[group]Run cargo test\n\
                   test\tRun tests\t2026-11-24T22:40:08.0000000Z running 3 tests\n\
                   test\tRun tests\t2026-11-24T22:40:09.0000000Z \u{1b}[31mFAILED\u{1b}[0m tests::it_works\n\
                   test\tRun tests\t2026-11-24T22:40:09.1000000Z ##[endgroup]\n\
                   test\tRun tests\t2026-11-24T22:40:10.0000000Z ##[error]Process completed with exit code 101.\n";
        assert_eq!(clean_actions_log(raw), "running 3 tests\nFAILED tests::it_works\nerror: Process completed with exit code 101.\n");
    }
}
