//! Which service hosts a repository, from its remote URL.

use orbit_proto::{ForgeKind, ForgeRepo};

/// Parses a git remote URL into the hosted repository it names. Handles
/// HTTPS, SSH (`ssh://` and scp-like `git@host:path`) and `git://` URLs,
/// and Azure DevOps' `_git` and `v3/` forms. `None` for local paths and
/// hosts Orbit cannot place.
pub fn parse_remote(remote: &str, url: &str) -> Option<ForgeRepo> {
    let url = url.trim();
    let (host, path) = split_url(url)?;
    let host = host.to_ascii_lowercase();
    let path = path.trim_matches('/').trim_end_matches(".git").trim_end_matches('/');
    let kind = kind_for_host(&host)?;
    let (path, web_url) = match kind {
        ForgeKind::AzureDevOps => azure_path(&host, path)?,
        _ => {
            let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            // GitLab nests groups; the others are exactly owner/name.
            let ok = match kind {
                ForgeKind::GitLab => segments.len() >= 2,
                _ => segments.len() == 2,
            };
            if !ok {
                return None;
            }
            let path = segments.join("/");
            let web = format!("https://{host}/{path}");
            (path, web)
        }
    };
    Some(ForgeRepo { kind, host, path, web_url, remote: remote.to_owned() })
}

/// The service a host name belongs to. Self-hosted instances are recognized
/// when their name says what they run (`github.acme.com`,
/// `gitlab.internal`).
pub fn kind_for_host(host: &str) -> Option<ForgeKind> {
    let h = host.to_ascii_lowercase();
    let has = |part: &str| h == part || h.ends_with(&format!(".{part}")) || h.split(['.', '-']).any(|label| label == part);
    if h == "github.com" || h.ends_with(".ghe.com") || has("github") {
        Some(ForgeKind::GitHub)
    } else if h == "gitlab.com" || has("gitlab") {
        Some(ForgeKind::GitLab)
    } else if h == "bitbucket.org" || has("bitbucket") {
        Some(ForgeKind::Bitbucket)
    } else if h == "dev.azure.com" || h == "ssh.dev.azure.com" || h.ends_with(".visualstudio.com") {
        Some(ForgeKind::AzureDevOps)
    } else if h == "codeberg.org" || has("gitea") || has("forgejo") {
        Some(ForgeKind::Gitea)
    } else {
        None
    }
}

/// `(host, path)` of a remote URL.
fn split_url(url: &str) -> Option<(String, String)> {
    if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git") {
            return None;
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = authority.rsplit('@').next()?;
        let host = host.split(':').next()?; // drop a port
        return (!host.is_empty()).then(|| (host.to_owned(), path.to_owned()));
    }
    // scp-like: [user@]host:path, but not a Windows drive (`C:\…`).
    let (before, path) = url.split_once(':')?;
    if before.len() <= 1 || before.contains('/') || before.contains('\\') {
        return None;
    }
    let host = before.rsplit('@').next()?;
    (!host.is_empty()).then(|| (host.to_owned(), path.to_owned()))
}

/// Azure DevOps paths: `org/project/_git/repo` (HTTPS),
/// `v3/org/project/repo` (SSH) and `project/_git/repo` on
/// `org.visualstudio.com`.
fn azure_path(host: &str, path: &str) -> Option<(String, String)> {
    let seg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let (org, project, repo) = match seg.as_slice() {
        ["v3", org, project, repo] => (org.to_string(), project.to_string(), repo.to_string()),
        [org, project, "_git", repo] => (org.to_string(), project.to_string(), repo.to_string()),
        [project, "_git", repo] if host.ends_with(".visualstudio.com") => {
            (host.trim_end_matches(".visualstudio.com").to_owned(), project.to_string(), repo.to_string())
        }
        _ => return None,
    };
    let web = format!("https://dev.azure.com/{org}/{project}/_git/{repo}");
    Some((format!("{org}/{project}/{repo}"), web))
}

/// Picks the remote to treat as the project's host: `upstream` is where
/// pull requests go in a fork workflow only when the user asks for it, so
/// `origin` wins, then the first remote on a known host.
pub fn pick_remote(remotes: &[(String, String)]) -> Option<ForgeRepo> {
    let parsed: Vec<ForgeRepo> = remotes.iter().filter_map(|(name, url)| parse_remote(name, url)).collect();
    parsed.iter().find(|r| r.remote == "origin").or(parsed.first()).cloned()
}

/// Reads `remote.<name>.url` lines from `git config --get-regexp`.
pub fn parse_remote_config(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(' ')?;
            let name = key.strip_prefix("remote.")?.strip_suffix(".url")?;
            Some((name.to_owned(), value.trim().to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(url: &str) -> Option<(ForgeKind, String, String)> {
        parse_remote("origin", url).map(|r| (r.kind, r.host, r.path))
    }

    #[test]
    fn remotes_of_every_shape_resolve() {
        let gh = Some((ForgeKind::GitHub, "github.com".to_owned(), "acme/widgets".to_owned()));
        assert_eq!(parse("https://github.com/acme/widgets.git"), gh);
        assert_eq!(parse("https://github.com/acme/widgets"), gh);
        assert_eq!(parse("https://token@github.com/acme/widgets/"), gh);
        assert_eq!(parse("git@github.com:acme/widgets.git"), gh);
        assert_eq!(parse("ssh://git@github.com:22/acme/widgets.git"), gh);
        assert_eq!(parse("git@GitHub.com:acme/widgets"), gh);
        assert_eq!(
            parse("git@gitlab.com:group/sub/proj.git"),
            Some((ForgeKind::GitLab, "gitlab.com".into(), "group/sub/proj".into()))
        );
        assert_eq!(
            parse("https://gitlab.acme.io/platform/api"),
            Some((ForgeKind::GitLab, "gitlab.acme.io".into(), "platform/api".into()))
        );
        assert_eq!(
            parse("https://github.acme.com/team/app.git").map(|r| r.0),
            Some(ForgeKind::GitHub),
            "GitHub Enterprise Server"
        );
        assert_eq!(
            parse("git@bitbucket.org:team/repo.git"),
            Some((ForgeKind::Bitbucket, "bitbucket.org".into(), "team/repo".into()))
        );
        assert_eq!(
            parse("https://dev.azure.com/org/proj/_git/repo"),
            Some((ForgeKind::AzureDevOps, "dev.azure.com".into(), "org/proj/repo".into()))
        );
        assert_eq!(
            parse("git@ssh.dev.azure.com:v3/org/proj/repo"),
            Some((ForgeKind::AzureDevOps, "ssh.dev.azure.com".into(), "org/proj/repo".into()))
        );
        assert_eq!(parse("https://codeberg.org/forgejo/forgejo.git").map(|r| r.0), Some(ForgeKind::Gitea));
    }

    #[test]
    fn local_and_unknown_remotes_do_not() {
        assert_eq!(parse("C:\\repos\\bare.git"), None);
        assert_eq!(parse("/srv/git/repo.git"), None);
        assert_eq!(parse("file:///srv/git/repo.git"), None);
        assert_eq!(parse("https://git.example.com/a/b"), None);
        assert_eq!(parse("https://github.com/only-owner"), None);
    }

    #[test]
    fn origin_is_preferred() {
        let config = "remote.upstream.url https://github.com/up/widgets\nremote.origin.url git@github.com:me/widgets.git\n";
        let remotes = parse_remote_config(config);
        assert_eq!(remotes.len(), 2);
        let picked = pick_remote(&remotes).unwrap();
        assert_eq!((picked.remote.as_str(), picked.path.as_str()), ("origin", "me/widgets"));
        assert_eq!(picked.web_url, "https://github.com/me/widgets");
    }
}
