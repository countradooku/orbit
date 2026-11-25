//! Jira through its REST API: Cloud with an email and API token (v3, Basic
//! authentication), Server and Data Center with a personal access token
//! (v2, bearer).

use orbit_proto::*;
use sonic_rs::{JsonValueTrait as _, Value};

use super::json::{self, arr, s, time, u};
use super::{BoxFuture, ForgeError, IssueTracker, http};

const FIELDS: &str = "summary,status,assignee,reporter,labels,updated,comment";
const ASSIGNED: &str = "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC";

pub(crate) struct Jira {
    /// `https://acme.atlassian.net`, no trailing slash.
    pub site: String,
    pub email: String,
    pub token: String,
}

impl Jira {
    fn cloud(&self) -> bool {
        !self.email.is_empty()
    }

    fn api(&self) -> &'static str {
        if self.cloud() { "rest/api/3" } else { "rest/api/2" }
    }

    async fn get(&self, path: &str, what: &str) -> Result<Value, ForgeError> {
        let auth = if self.cloud() {
            format!("Basic {}", http::base64(&format!("{}:{}", self.email, self.token)))
        } else {
            format!("Bearer {}", self.token)
        };
        let url = format!("{}/{path}", self.site);
        let headers = [("authorization", auth), ("accept", "application/json".to_owned())];
        let response = http::send("GET", &url, &headers, None).await?;
        match response.status {
            200 => json::parse(&response.body, what),
            401 | 403 => Err(ForgeError::TrackerAuth {
                tracker: TrackerKind::Jira,
                fix: if self.cloud() {
                    "Jira rejected the email and API token. Create a token at id.atlassian.com › Security › API tokens, \
                     then update Orbit › Settings › Integrations."
                        .into()
                } else {
                    "Jira rejected the personal access token. Create one in Jira › Profile › Personal access tokens, then \
                     update Orbit › Settings › Integrations."
                        .into()
                },
            }),
            404 => Err(ForgeError::NotFound(error_text(&response.body).unwrap_or_else(|| format!("Jira has no {what} here.")))),
            429 => Err(ForgeError::RateLimited {
                host: self.site.clone(),
                reset_ms: response.retry_after.map(|s| now_ms() + s * 1000).unwrap_or(0),
            }),
            status => Err(ForgeError::Failed(match error_text(&response.body) {
                Some(text) => format!("Jira: {text}"),
                None => format!("Jira answered HTTP {status}."),
            })),
        }
    }

    fn search_path(&self, jql: &str) -> String {
        let jql = super::gitlab::encode(jql);
        if self.cloud() {
            format!("{}/search/jql?jql={jql}&fields={FIELDS}&maxResults=50", self.api())
        } else {
            format!("{}/search?jql={jql}&fields={FIELDS}&maxResults=50", self.api())
        }
    }
}

impl IssueTracker for Jira {
    fn kind(&self) -> TrackerKind {
        TrackerKind::Jira
    }

    fn issues(&self, query: &str) -> BoxFuture<'_, Vec<Issue>> {
        let jql = jql_for(query);
        Box::pin(async move {
            let v = self.get(&self.search_path(&jql), "issues").await?;
            Ok(arr(&v, "issues").iter().map(|i| parse_issue(i, &self.site)).collect())
        })
    }

    fn issue(&self, key: &str) -> BoxFuture<'_, IssueDetail> {
        let key = key.trim().to_owned();
        Box::pin(async move {
            let path = format!("{}/issue/{}?fields={FIELDS},description", self.api(), super::gitlab::encode(&key));
            let v = self.get(&path, "issue").await?;
            Ok(parse_detail(&v, &self.site))
        })
    }
}

/// The JQL for a search box: assigned and open by default, a key lookup for
/// `ABC-123`, else a text search.
pub(crate) fn jql_for(query: &str) -> String {
    let q = query.trim();
    if q.is_empty() {
        return ASSIGNED.into();
    }
    let looks_like_key = q.split_once('-').is_some_and(|(project, n)| {
        !project.is_empty()
            && project.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && project.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && !n.is_empty()
            && n.chars().all(|c| c.is_ascii_digit())
    });
    if looks_like_key {
        return format!("key = {}", q.to_ascii_uppercase());
    }
    let escaped = q.replace('\\', "\\\\").replace('"', "\\\"");
    format!("text ~ \"{escaped}\" ORDER BY updated DESC")
}

fn error_text(body: &[u8]) -> Option<String> {
    let v: Value = sonic_rs::from_slice(body).ok()?;
    let messages: Vec<String> = arr(&v, "errorMessages").iter().filter_map(|m| m.as_str().map(str::to_owned)).collect();
    (!messages.is_empty()).then(|| messages.join(" "))
}

pub(crate) fn parse_issue(v: &Value, site: &str) -> Issue {
    let key = s(v, "key");
    let state = match s(v, "fields/status/statusCategory/key").as_str() {
        "done" => IssueState::Closed,
        "indeterminate" => IssueState::Started,
        _ => IssueState::Open,
    };
    Issue {
        tracker: TrackerKind::Jira,
        title: s(v, "fields/summary"),
        state,
        status: s(v, "fields/status/name"),
        author: s(v, "fields/reporter/displayName"),
        assignee: json::opt_s(v, "fields/assignee/displayName"),
        labels: arr(v, "fields/labels").iter().filter_map(|l| l.as_str().map(str::to_owned)).collect(),
        url: format!("{site}/browse/{key}"),
        updated_ms: time(v, "fields/updated"),
        comments: u(v, "fields/comment/total") as u32,
        branch: None,
        key,
    }
}

pub(crate) fn parse_detail(v: &Value, site: &str) -> IssueDetail {
    let issue = parse_issue(v, site);
    let body = json::at(v, "fields/description").map(rich_text).unwrap_or_default();
    let mut comments: Vec<Comment> = arr(v, "fields/comment/comments")
        .iter()
        .map(|c| Comment {
            author: s(c, "author/displayName"),
            body: json::at(c, "body").map(rich_text).unwrap_or_default(),
            created_ms: time(c, "created"),
            path: None,
            line: None,
        })
        .collect();
    comments.sort_by_key(|c| c.created_ms);
    IssueDetail { issue, body, comments }
}

/// Jira v2 sends wiki text as a string; v3 sends Atlassian Document Format,
/// which becomes Markdown here.
pub(crate) fn rich_text(v: &Value) -> String {
    if let Some(text) = v.as_str() {
        return text.to_owned();
    }
    let mut out = String::new();
    adf_blocks(arr(v, "content"), &mut out, "");
    out.trim_end().to_owned()
}

fn adf_blocks(nodes: &[Value], out: &mut String, indent: &str) {
    for node in nodes {
        let content = arr(node, "content");
        match s(node, "type").as_str() {
            "paragraph" => {
                out.push_str(indent);
                adf_inline(content, out);
                out.push_str("\n\n");
            }
            "heading" => {
                let level = u(node, "attrs/level").clamp(1, 6) as usize;
                out.push_str(&"#".repeat(level));
                out.push(' ');
                adf_inline(content, out);
                out.push_str("\n\n");
            }
            "bulletList" | "orderedList" => {
                let ordered = s(node, "type") == "orderedList";
                for (i, item) in content.iter().enumerate() {
                    let marker = if ordered { format!("{}. ", i + 1) } else { "- ".to_owned() };
                    let mut inner = String::new();
                    adf_blocks(arr(item, "content"), &mut inner, "");
                    let inner = inner.trim_end();
                    let continuation = format!("\n{indent}  ");
                    out.push_str(indent);
                    out.push_str(&marker);
                    out.push_str(&inner.replace("\n\n", "\n").replace('\n', &continuation));
                    out.push('\n');
                }
                out.push('\n');
            }
            "codeBlock" => {
                out.push_str("```");
                out.push_str(&s(node, "attrs/language"));
                out.push('\n');
                adf_inline(content, out);
                out.push_str("\n```\n\n");
            }
            "blockquote" => {
                let mut inner = String::new();
                adf_blocks(content, &mut inner, "");
                for line in inner.trim_end().lines() {
                    out.push_str("> ");
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }
            "rule" => out.push_str("---\n\n"),
            "panel" | "expand" | "nestedExpand" | "layoutSection" | "layoutColumn" | "tableCell" | "tableHeader" => {
                adf_blocks(content, out, indent)
            }
            "table" | "tableRow" => adf_blocks(content, out, indent),
            "mediaSingle" | "mediaGroup" => out.push_str("[attachment]\n\n"),
            _ => {
                adf_inline(content, out);
                if !content.is_empty() {
                    out.push_str("\n\n");
                }
            }
        }
    }
}

fn adf_inline(nodes: &[Value], out: &mut String) {
    for node in nodes {
        match s(node, "type").as_str() {
            "text" => {
                let mut text = s(node, "text");
                for mark in arr(node, "marks") {
                    text = match s(mark, "type").as_str() {
                        "strong" => format!("**{text}**"),
                        "em" => format!("_{text}_"),
                        "code" => format!("`{text}`"),
                        "strike" => format!("~~{text}~~"),
                        "link" => format!("[{text}]({})", s(mark, "attrs/href")),
                        _ => text,
                    };
                }
                out.push_str(&text);
            }
            "hardBreak" => out.push('\n'),
            "mention" => out.push_str(&s(node, "attrs/text")),
            "emoji" => out.push_str(&s(node, "attrs/text")),
            "inlineCard" | "blockCard" => out.push_str(&s(node, "attrs/url")),
            _ => adf_inline(arr(node, "content"), out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/forge/{name}", env!("CARGO_MANIFEST_DIR"));
        json::parse(&std::fs::read(path).unwrap(), name).unwrap()
    }

    const SITE: &str = "https://acme.atlassian.net";

    #[test]
    fn a_cloud_search_parses() {
        let v = fixture("jira_search.json");
        let issues: Vec<Issue> = arr(&v, "issues").iter().map(|i| parse_issue(i, SITE)).collect();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].key, "PROJ-7");
        assert_eq!(issues[0].state, IssueState::Started);
        assert_eq!(issues[0].status, "In Progress");
        assert_eq!(issues[0].url, "https://acme.atlassian.net/browse/PROJ-7");
        assert_eq!(issues[0].assignee.as_deref(), Some("Alice Doe"));
        assert_eq!(issues[0].comments, 2);
        assert_eq!(issues[0].updated_ms, 1_764_000_000_000);
        assert_eq!(issues[1].state, IssueState::Open);
        assert_eq!(issues[1].assignee, None);
    }

    #[test]
    fn an_issue_in_document_format_becomes_markdown() {
        let d = parse_detail(&fixture("jira_issue.json"), SITE);
        assert_eq!(d.issue.key, "PROJ-7");
        assert_eq!(
            d.body,
            "## Steps\n\n1. Open **Settings**\n2. Press `Save`\n\nSee [the log](https://example.com/log).\n\n```rust\nlet x = 1;\n```"
        );
        assert_eq!(d.comments.len(), 2);
        assert_eq!(d.comments[0].body, "First look: it is the cache.");
        assert_eq!(d.comments[1].author, "Bob");
    }

    #[test]
    fn server_wiki_text_passes_through() {
        let v: Value = sonic_rs::from_str(r#"{"key":"OPS-1","fields":{"summary":"x","description":"h1. Title\n* item"}}"#).unwrap();
        assert_eq!(parse_detail(&v, SITE).body, "h1. Title\n* item");
    }

    #[test]
    fn search_text_becomes_jql() {
        assert_eq!(jql_for(""), ASSIGNED);
        assert_eq!(jql_for("proj-12"), "key = PROJ-12");
        assert_eq!(jql_for("login \"fails\""), "text ~ \"login \\\"fails\\\"\" ORDER BY updated DESC");
        assert_eq!(jql_for("2-fa"), "text ~ \"2-fa\" ORDER BY updated DESC");
    }
}
