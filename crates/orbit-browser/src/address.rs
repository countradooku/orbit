//! Addresses: what the address field accepts, and the dev-server URLs found
//! in terminal or agent output. Pure functions, no GPUI.

use std::fmt;

use url::{Host, Url};

/// Why text typed into the address field is not an address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddressError {
    /// Nothing but whitespace.
    Empty,
    /// Text that is neither a URL, a `host:port`, nor a port.
    Invalid(String),
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("Enter an address or a port."),
            Self::Invalid(text) => write!(f, "“{text}” isn’t an address or a port."),
        }
    }
}

impl std::error::Error for AddressError {}

/// Turns what was typed into the address field into a URL.
///
/// - `3000` and `:3000` mean `http://localhost:3000/`.
/// - `localhost:3000`, `127.0.0.1:8080/docs` and other local hosts get `http://`.
/// - `example.com` and other hosts get `https://`.
/// - A full URL (`http:`, `https:`, `file:`, `about:`, `data:`) is kept as is.
///
/// `0.0.0.0` and `[::]` are bind-all addresses that browsers cannot connect
/// to everywhere, so they become `localhost`.
pub fn parse_address(input: &str) -> Result<Url, AddressError> {
    let text = input.trim();
    if text.is_empty() {
        return Err(AddressError::Empty);
    }
    let invalid = || AddressError::Invalid(text.to_string());

    if let Some(port) = parse_port(text.strip_prefix(':').unwrap_or(text)) {
        return Ok(localhost(port));
    }
    if text.chars().any(char::is_whitespace) {
        return Err(invalid());
    }

    if let Some((scheme, rest)) = text.split_once(':') {
        let is_url = match scheme.to_ascii_lowercase().as_str() {
            "http" | "https" | "file" => rest.starts_with("//"),
            "about" | "data" => true,
            _ => false,
        };
        if is_url {
            let url = Url::parse(text).map_err(|_| invalid())?;
            return Ok(normalize_bind_all(url));
        }
    }
    if text.contains("://") {
        return Err(invalid());
    }

    // `host`, `host:port` or `host:port/path` without a scheme.
    let host_end = text.find(['/', '?', '#']).unwrap_or(text.len());
    let authority = &text[..host_end];
    let host = authority_host(authority);
    if host.is_empty() {
        return Err(invalid());
    }
    let scheme = if is_local_host_name(host) { "http" } else { "https" };
    // A bare word such as `todo` is not an address; a dotted name, an IP
    // literal, a local name or an explicit port is.
    let has_port = authority.len() > host.len();
    if !has_port && !host.contains('.') && !host.starts_with('[') && !is_local_host_name(host) {
        return Err(invalid());
    }
    let url = Url::parse(&format!("{scheme}://{text}")).map_err(|_| invalid())?;
    if url.host().is_none() {
        return Err(invalid());
    }
    Ok(normalize_bind_all(url))
}

/// Finds the dev-server URLs in terminal or agent output, in order of first
/// appearance and without duplicates.
///
/// A dev-server URL points at this machine: `localhost` (or a `*.localhost`
/// name), `127.0.0.1`, `[::1]`, or a bind-all `0.0.0.0` / `[::]`, with an
/// explicit port, as in `http://localhost:5173/` or `127.0.0.1:8000`. ANSI
/// colors and OSC 8 hyperlinks are ignored, so Vite's bold port number still
/// reads as one URL. Bind-all hosts are returned as `localhost`.
pub fn detect_urls(text: &str) -> Vec<Url> {
    let plain = strip_ansi(text);
    let mut urls: Vec<Url> = Vec::new();
    let bytes = plain.as_bytes();
    let mut ix = 0;
    while ix < bytes.len() {
        let rest = &plain[ix..];
        let (scheme, after_scheme) = if let Some(after) = strip_prefix_ignore_case(rest, "http://") {
            ("http", after)
        } else if let Some(after) = strip_prefix_ignore_case(rest, "https://") {
            ("https", after)
        } else {
            // A scheme-less `localhost:3000` must start a word.
            let starts_word = ix == 0 || !is_word_byte(bytes[ix - 1]);
            if starts_word && starts_with_local_host(rest) {
                ("http", rest)
            } else {
                ix += rest.chars().next().map_or(1, char::len_utf8);
                continue;
            }
        };
        let candidate_len =
            after_scheme.find(|c: char| c.is_whitespace() || is_url_terminator(c)).unwrap_or(after_scheme.len());
        let candidate = trim_trailing_punctuation(&after_scheme[..candidate_len]);
        let consumed = plain.len() - after_scheme.len() + candidate_len.max(1);
        if let Some(url) = dev_server_url(scheme, candidate)
            && !urls.contains(&url)
        {
            urls.push(url);
        }
        ix = consumed;
    }
    urls
}

/// The URL a worktree's dev server would listen on, from its environment's
/// `PORT` variable: `PORT=4000` suggests `http://localhost:4000/`.
///
/// Pass the worktree's environment, such as `std::env::vars()` or the
/// variables a project's `.env` defines.
pub fn port_url<K, V>(env: impl IntoIterator<Item = (K, V)>) -> Option<Url>
where
    K: AsRef<str>,
    V: AsRef<str>,
{
    env.into_iter()
        .find(|(key, _)| key.as_ref() == "PORT")
        .and_then(|(_, value)| parse_port(value.as_ref().trim()))
        .map(localhost)
}

/// Whether `url` points at this machine, where "nothing is listening" is a
/// question a connection attempt can answer quickly.
pub fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => is_localhost_name(domain),
        Some(Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        None => false,
    }
}

/// `host:port` for messages, such as `localhost:3000`, or the host alone
/// when the URL uses its scheme's default port.
pub fn host_label(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None if url.scheme() == "file" => url.path().to_string(),
        None => host.to_string(),
    }
}

/// Where a page's request to open a new window goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NewWindowTarget {
    /// Replaces the current page: the link stays on the site being previewed.
    InPlace,
    /// Opens in the system browser: the link leaves the site, or is not a web page.
    System,
}

/// Links on the previewed site open in place; anything else, such as
/// documentation on another site or a `mailto:` link, opens in the system
/// browser.
pub(crate) fn new_window_target(current: Option<&Url>, requested: &Url) -> NewWindowTarget {
    let is_web = matches!(requested.scheme(), "http" | "https");
    match current {
        Some(current) if is_web && current.origin() == requested.origin() => NewWindowTarget::InPlace,
        _ => NewWindowTarget::System,
    }
}

fn localhost(port: u16) -> Url {
    Url::parse(&format!("http://localhost:{port}/")).expect("a localhost URL with a port is valid")
}

fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<u16>().ok().filter(|port| *port != 0)
}

/// The host part of `host[:port]`, keeping IPv6 brackets.
fn authority_host(authority: &str) -> &str {
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    if authority.starts_with('[') {
        return authority.find(']').map_or(authority, |end| &authority[..=end]);
    }
    authority.split_once(':').map_or(authority, |(host, _)| host)
}

fn is_localhost_name(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost")
}

/// Hosts served over plain HTTP by default: this machine, the local network
/// (`.local`, `.test`, `.internal`) and IP literals.
fn is_local_host_name(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    is_localhost_name(&lower)
        || [".local", ".test", ".internal", ".lan", ".home.arpa"].iter().any(|suffix| lower.ends_with(suffix))
        || lower.parse::<std::net::IpAddr>().is_ok()
        || (lower.starts_with('[') && lower.ends_with(']'))
}

fn normalize_bind_all(mut url: Url) -> Url {
    let bind_all = match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_unspecified(),
        Some(Host::Ipv6(ip)) => ip.is_unspecified(),
        _ => false,
    };
    if bind_all {
        // Setting a domain host on an http(s) URL cannot fail.
        let _ = url.set_host(Some("localhost"));
    }
    url
}

/// A URL in output, without its scheme, if it is a dev server's.
fn dev_server_url(scheme: &str, candidate: &str) -> Option<Url> {
    let host_end = candidate.find(['/', '?', '#']).unwrap_or(candidate.len());
    let authority = &candidate[..host_end];
    let host = authority_host(authority);
    let port = authority[host.len()..].strip_prefix(':')?;
    parse_port(port)?;
    let url = Url::parse(&format!("{scheme}://{candidate}")).ok()?;
    if !is_loopback(&url) {
        return None;
    }
    Some(normalize_bind_all(url))
}

const LOCAL_HOSTS: [&str; 5] = ["localhost:", "127.0.0.1:", "0.0.0.0:", "[::1]:", "[::]:"];

fn starts_with_local_host(text: &str) -> bool {
    LOCAL_HOSTS.iter().any(|host| strip_prefix_ignore_case(text, host).is_some())
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b'@')
}

/// Characters that end a URL in prose and logs.
fn is_url_terminator(c: char) -> bool {
    matches!(c, '"' | '\'' | '`' | '<' | '>' | '|' | '{' | '}' | '\\' | '^') || c.is_control()
}

fn trim_trailing_punctuation(candidate: &str) -> &str {
    let mut end = candidate.len();
    loop {
        let trimmed = &candidate[..end];
        let Some(last) = trimmed.chars().last() else { return trimmed };
        let unbalanced_paren = last == ')' && trimmed.matches('(').count() < trimmed.matches(')').count();
        let unbalanced_bracket = last == ']' && trimmed.matches('[').count() < trimmed.matches(']').count();
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?') || unbalanced_paren || unbalanced_bracket {
            end -= last.len_utf8();
        } else {
            return trimmed;
        }
    }
}

/// Removes CSI sequences (colors, cursor movement) and OSC sequences (titles,
/// OSC 8 hyperlinks), keeping the text they decorate.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // Parameters and intermediates, then one final byte in @..~.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // Ends at BEL or ST (ESC \).
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                // A two-character escape such as ESC ( B.
                chars.next();
            }
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(text: &str) -> Vec<String> {
        detect_urls(text).into_iter().map(String::from).collect()
    }

    fn address(text: &str) -> String {
        parse_address(text).map(String::from).unwrap_or_else(|error| format!("error: {error}"))
    }

    #[test]
    fn a_port_means_localhost() {
        assert_eq!(address("3000"), "http://localhost:3000/");
        assert_eq!(address(" :5173 "), "http://localhost:5173/");
        assert_eq!(address("65535"), "http://localhost:65535/");
    }

    #[test]
    fn out_of_range_ports_are_not_addresses() {
        assert!(parse_address("0").is_err());
        assert!(parse_address("65536").is_err());
        assert!(parse_address("123456").is_err());
    }

    #[test]
    fn local_hosts_get_http() {
        assert_eq!(address("localhost:3000"), "http://localhost:3000/");
        assert_eq!(address("localhost:3000/docs?x=1"), "http://localhost:3000/docs?x=1");
        assert_eq!(address("127.0.0.1:8080"), "http://127.0.0.1:8080/");
        assert_eq!(address("[::1]:4000"), "http://[::1]:4000/");
        assert_eq!(address("app.localhost:3000"), "http://app.localhost:3000/");
        assert_eq!(address("myapp.test"), "http://myapp.test/");
        assert_eq!(address("192.168.1.20:5173"), "http://192.168.1.20:5173/");
        assert_eq!(address("localhost"), "http://localhost/");
    }

    #[test]
    fn bind_all_hosts_become_localhost() {
        assert_eq!(address("0.0.0.0:8000"), "http://localhost:8000/");
        assert_eq!(address("http://0.0.0.0:8000/x"), "http://localhost:8000/x");
        assert_eq!(address("[::]:8000"), "http://localhost:8000/");
    }

    #[test]
    fn other_hosts_get_https() {
        assert_eq!(address("example.com"), "https://example.com/");
        assert_eq!(address("docs.rs/url"), "https://docs.rs/url");
    }

    #[test]
    fn full_urls_are_kept() {
        assert_eq!(address("http://example.com"), "http://example.com/");
        assert_eq!(address("HTTPS://Example.com/A"), "https://example.com/A");
        assert_eq!(address("about:blank"), "about:blank");
        assert_eq!(address("file:///C:/site/index.html"), "file:///C:/site/index.html");
    }

    #[test]
    fn words_and_unknown_schemes_are_rejected() {
        assert_eq!(parse_address("   "), Err(AddressError::Empty));
        assert!(parse_address("todo").is_err());
        assert!(parse_address("two words").is_err());
        assert!(parse_address("ftp://example.com").is_err());
        assert!(parse_address("javascript:alert(1)").is_err());
    }

    #[test]
    fn finds_dev_server_urls_in_output() {
        let output = "\
  VITE v5.4.0  ready in 312 ms

  ➜  Local:   http://localhost:5173/
  ➜  Network: http://192.168.1.20:5173/
";
        assert_eq!(urls(output), ["http://localhost:5173/"]);
        assert_eq!(urls("- Local:        http://localhost:3000"), ["http://localhost:3000/"]);
        assert_eq!(urls("Serving on http://127.0.0.1:8000/docs."), ["http://127.0.0.1:8000/docs"]);
        assert_eq!(urls("Listening on [::1]:4000"), ["http://[::1]:4000/"]);
    }

    #[test]
    fn bind_all_urls_in_output_become_localhost() {
        assert_eq!(urls("Uvicorn running on http://0.0.0.0:8000 (Press CTRL+C to quit)"), ["http://localhost:8000/"]);
        assert_eq!(urls("Listening on http://[::]:8080"), ["http://localhost:8080/"]);
    }

    #[test]
    fn ansi_colors_do_not_split_a_url() {
        let vite = "  \u{1b}[32m➜\u{1b}[39m  \u{1b}[1mLocal\u{1b}[22m:   \u{1b}[36mhttp://localhost:\u{1b}[1m5173\u{1b}[22m/\u{1b}[39m";
        assert_eq!(urls(vite), ["http://localhost:5173/"]);
        let hyperlink = "\u{1b}]8;;http://localhost:3000\u{1b}\\http://localhost:3000\u{1b}]8;;\u{1b}\\";
        assert_eq!(urls(hyperlink), ["http://localhost:3000/"]);
    }

    #[test]
    fn duplicates_are_reported_once_in_order() {
        let output = "http://localhost:3000 then http://localhost:4000/ and again http://localhost:3000";
        assert_eq!(urls(output), ["http://localhost:3000/", "http://localhost:4000/"]);
    }

    #[test]
    fn trailing_punctuation_and_quotes_are_not_part_of_the_url() {
        assert_eq!(urls("(see http://localhost:3000/app)."), ["http://localhost:3000/app"]);
        assert_eq!(urls("url=\"http://localhost:3000/a\","), ["http://localhost:3000/a"]);
        assert_eq!(urls("open 'localhost:8080'!"), ["http://localhost:8080/"]);
    }

    #[test]
    fn remote_and_portless_urls_are_not_dev_servers() {
        assert!(urls("Docs: https://vitejs.dev/config and http://localhost/").is_empty());
        assert!(urls("http://10.0.0.5:3000").is_empty());
        assert!(urls("mylocalhost:3000 and xlocalhost:3000").is_empty());
        assert!(urls("http://localhost:99999").is_empty());
    }

    #[test]
    fn suggests_a_url_from_the_port_variable() {
        assert_eq!(
            port_url([("NODE_ENV", "development"), ("PORT", "4000")]).map(String::from),
            Some("http://localhost:4000/".into())
        );
        assert_eq!(port_url([("PORT", " 8080 ")]).map(String::from), Some("http://localhost:8080/".into()));
        assert_eq!(port_url([("PORT", "abc")]), None);
        assert_eq!(port_url([("PORT", "0")]), None);
        assert_eq!(port_url(Vec::<(String, String)>::new()), None);
    }

    #[test]
    fn loopback_hosts() {
        let is = |text: &str| is_loopback(&Url::parse(text).unwrap());
        assert!(is("http://localhost:3000"));
        assert!(is("http://app.localhost:3000"));
        assert!(is("http://127.0.0.1:3000"));
        assert!(is("http://127.1.2.3:3000"));
        assert!(is("http://[::1]:3000"));
        assert!(!is("https://example.com"));
        assert!(!is("http://192.168.1.2:3000"));
        assert!(!is("file:///tmp/index.html"));
    }

    #[test]
    fn host_labels() {
        assert_eq!(host_label(&Url::parse("http://localhost:3000/a").unwrap()), "localhost:3000");
        assert_eq!(host_label(&Url::parse("https://example.com/a").unwrap()), "example.com");
    }

    #[test]
    fn new_windows_stay_on_the_previewed_site() {
        let current = Url::parse("http://localhost:3000/").unwrap();
        let same = Url::parse("http://localhost:3000/login").unwrap();
        let other_port = Url::parse("http://localhost:4000/").unwrap();
        let docs = Url::parse("https://react.dev/").unwrap();
        let mail = Url::parse("mailto:a@b.c").unwrap();
        assert_eq!(new_window_target(Some(&current), &same), NewWindowTarget::InPlace);
        assert_eq!(new_window_target(Some(&current), &other_port), NewWindowTarget::System);
        assert_eq!(new_window_target(Some(&current), &docs), NewWindowTarget::System);
        assert_eq!(new_window_target(Some(&current), &mail), NewWindowTarget::System);
        assert_eq!(new_window_target(None, &same), NewWindowTarget::System);
    }
}
