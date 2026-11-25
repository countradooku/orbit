//! Linear through its GraphQL API, with the user's personal API key.

use orbit_proto::*;
use sonic_rs::Value;

use super::json::{self, arr, s, time};
use super::{BoxFuture, ForgeError, IssueTracker, http};

const ENDPOINT: &str = "https://api.linear.app/graphql";

/// The fields every issue query selects.
const ISSUE_FIELDS: &str = "identifier title url branchName updatedAt \
state { name type } assignee { displayName name } creator { displayName name } labels { nodes { name } }";

pub(crate) struct Linear {
    pub api_key: String,
    /// Overridden by tests to point at a local server.
    pub endpoint: String,
}

impl Linear {
    pub fn new(api_key: String) -> Self {
        Self { api_key, endpoint: ENDPOINT.into() }
    }

    async fn query(&self, query: &str, variables: &str) -> Result<Value, ForgeError> {
        let body = format!("{{\"query\":{},\"variables\":{variables}}}", json::quote(query));
        // Personal keys go bare; OAuth tokens as bearer tokens.
        let auth = if self.api_key.starts_with("lin_oauth_") { format!("Bearer {}", self.api_key) } else { self.api_key.clone() };
        let headers = [("authorization", auth), ("content-type", "application/json".to_owned())];
        let response = http::send("POST", &self.endpoint, &headers, Some(body.into_bytes())).await?;
        let v = json::parse(&response.body, "Linear's answer").map_err(|e| match response.status {
            200 => e,
            status => ForgeError::Failed(format!("Linear answered HTTP {status}.")),
        })?;
        if let Some(error) = arr(&v, "errors").first() {
            return Err(classify(&s(error, "extensions/code"), &s(error, "message"), response.retry_after));
        }
        match response.status {
            200 => json::at(&v, "data").cloned().ok_or_else(|| ForgeError::Parse("Linear sent no data.".into())),
            401 | 403 => Err(classify("AUTHENTICATION_ERROR", "", None)),
            429 => Err(classify("RATELIMITED", "", response.retry_after)),
            status => Err(ForgeError::Failed(format!("Linear answered HTTP {status}."))),
        }
    }
}

impl IssueTracker for Linear {
    fn kind(&self) -> TrackerKind {
        TrackerKind::Linear
    }

    fn issues(&self, query: &str) -> BoxFuture<'_, Vec<Issue>> {
        let term = query.trim().to_owned();
        Box::pin(async move {
            if term.is_empty() {
                let q = format!(
                    "query {{ viewer {{ assignedIssues(first: 50, orderBy: updatedAt, \
                     filter: {{ state: {{ type: {{ nin: [\"completed\", \"canceled\"] }} }} }}) {{ nodes {{ {ISSUE_FIELDS} }} }} }} }}"
                );
                let data = self.query(&q, "{}").await?;
                Ok(parse_issues(arr(&data, "viewer/assignedIssues/nodes")))
            } else {
                let q = format!("query($term: String!) {{ searchIssues(term: $term, first: 50) {{ nodes {{ {ISSUE_FIELDS} }} }} }}");
                let data = self.query(&q, &format!("{{\"term\":{}}}", json::quote(&term))).await?;
                Ok(parse_issues(arr(&data, "searchIssues/nodes")))
            }
        })
    }

    fn issue(&self, key: &str) -> BoxFuture<'_, IssueDetail> {
        let key = key.trim().to_owned();
        Box::pin(async move {
            let q = format!(
                "query($id: String!) {{ issue(id: $id) {{ {ISSUE_FIELDS} description \
                 comments(first: 100) {{ nodes {{ body createdAt user {{ displayName name }} }} }} }} }}"
            );
            let data = self.query(&q, &format!("{{\"id\":{}}}", json::quote(&key))).await?;
            let issue = json::at(&data, "issue").ok_or_else(|| ForgeError::NotFound(format!("Linear has no issue {key}.")))?;
            Ok(parse_detail(issue))
        })
    }
}

pub(crate) fn classify(code: &str, message: &str, retry_after: Option<u64>) -> ForgeError {
    match code {
        "AUTHENTICATION_ERROR" | "FORBIDDEN" => ForgeError::TrackerAuth {
            tracker: TrackerKind::Linear,
            fix: "Linear rejected the API key. Create one in Linear › Settings › Security & access, then paste it in \
                  Orbit › Settings › Integrations."
                .into(),
        },
        "RATELIMITED" => ForgeError::RateLimited {
            host: "api.linear.app".into(),
            reset_ms: retry_after.map(|s| orbit_proto::now_ms() + s * 1000).unwrap_or(0),
        },
        "ENTITY_NOT_FOUND" | "INVALID_INPUT" if message.to_ascii_lowercase().contains("not found") => {
            ForgeError::NotFound(message.to_owned())
        }
        _ => ForgeError::Failed(if message.is_empty() { format!("Linear failed ({code}).") } else { format!("Linear: {message}") }),
    }
}

fn person(v: &Value, path: &str) -> String {
    let display = s(v, &format!("{path}/displayName"));
    if display.is_empty() { s(v, &format!("{path}/name")) } else { display }
}

pub(crate) fn parse_issue(v: &Value) -> Issue {
    let state = match s(v, "state/type").as_str() {
        "started" => IssueState::Started,
        "completed" | "canceled" => IssueState::Closed,
        _ => IssueState::Open,
    };
    Issue {
        tracker: TrackerKind::Linear,
        key: s(v, "identifier"),
        title: s(v, "title"),
        state,
        status: s(v, "state/name"),
        author: person(v, "creator"),
        assignee: Some(person(v, "assignee")).filter(|a| !a.is_empty()),
        labels: arr(v, "labels/nodes").iter().map(|l| s(l, "name")).filter(|n| !n.is_empty()).collect(),
        url: s(v, "url"),
        updated_ms: time(v, "updatedAt"),
        comments: arr(v, "comments/nodes").len() as u32,
        branch: json::opt_s(v, "branchName"),
    }
}

pub(crate) fn parse_issues(nodes: &[Value]) -> Vec<Issue> {
    nodes.iter().map(parse_issue).collect()
}

pub(crate) fn parse_detail(v: &Value) -> IssueDetail {
    let comments = arr(v, "comments/nodes")
        .iter()
        .map(|c| Comment {
            author: person(c, "user"),
            body: s(c, "body"),
            created_ms: time(c, "createdAt"),
            path: None,
            line: None,
        })
        .collect::<Vec<_>>();
    let mut comments = comments;
    comments.sort_by_key(|c| c.created_ms);
    let mut issue = parse_issue(v);
    issue.comments = comments.len() as u32;
    IssueDetail { issue, body: s(v, "description"), comments }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/forge/{name}", env!("CARGO_MANIFEST_DIR"));
        json::parse(&std::fs::read(path).unwrap(), name).unwrap()
    }

    #[test]
    fn assigned_issues_parse() {
        let v = fixture("linear_assigned.json");
        let issues = parse_issues(arr(&v, "data/viewer/assignedIssues/nodes"));
        assert_eq!(issues.len(), 2);
        let first = &issues[0];
        assert_eq!(first.key, "ENG-123");
        assert_eq!(first.state, IssueState::Started);
        assert_eq!(first.status, "In Progress");
        assert_eq!(first.assignee.as_deref(), Some("Alice"));
        assert_eq!(first.branch.as_deref(), Some("alice/eng-123-crash-on-empty-workspace"));
        assert_eq!(first.labels, vec!["Bug".to_owned()]);
        assert_eq!(issues[1].state, IssueState::Open);
        assert_eq!(issues[1].assignee, None);
    }

    #[test]
    fn an_issue_with_comments_parses() {
        let v = fixture("linear_issue.json");
        let d = parse_detail(json::at(&v, "data/issue").unwrap());
        assert_eq!(d.issue.key, "ENG-123");
        assert!(d.body.starts_with("Opening an empty workspace"));
        assert_eq!(d.comments.len(), 2);
        assert_eq!(d.comments[0].author, "Bob");
        assert!(d.comments[0].created_ms < d.comments[1].created_ms);
    }

    #[test]
    fn errors_name_the_fix() {
        let v = fixture("linear_auth_error.json");
        let e = arr(&v, "errors").first().unwrap();
        let err = classify(&s(e, "extensions/code"), &s(e, "message"), None);
        assert!(matches!(err, ForgeError::TrackerAuth { tracker: TrackerKind::Linear, .. }));
        assert!(matches!(classify("RATELIMITED", "", Some(30)), ForgeError::RateLimited { reset_ms, .. } if reset_ms > 0));
    }
}
