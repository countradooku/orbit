//! Finds URLs in terminal text.
//!
//! Detection is a small scanner, not a regex: it looks for a known scheme
//! around the hovered column and extends to the URL's natural end, trimming
//! the punctuation that usually follows a link in prose (`.`, `,`, `)`…).

/// Schemes opened in the browser or mail client. Anything else (e.g.
/// `javascript:`) is never treated as a link.
const SCHEMES: [&str; 5] = ["https://", "http://", "file://", "mailto:", "ftp://"];

/// The URL covering `col` in `line` (one char per column) and its column
/// range, if any.
pub fn url_at(line: &[char], col: usize) -> Option<(std::ops::Range<usize>, String)> {
    if col >= line.len() || is_break(line[col]) {
        return None;
    }
    // The token around `col`: the maximal run of non-break characters.
    let mut start = col;
    while start > 0 && !is_break(line[start - 1]) {
        start -= 1;
    }
    let mut end = col + 1;
    while end < line.len() && !is_break(line[end]) {
        end += 1;
    }
    let token: String = line[start..end].iter().collect();
    // A scheme may follow a prefix such as `(` or `href="`.
    let (offset, _) = SCHEMES.iter().filter_map(|s| token.find(s).map(|ix| (ix, *s))).min_by_key(|(ix, _)| *ix)?;
    let url_start = start + token[..offset].chars().count();
    if url_start > col {
        return None;
    }
    let mut url_end = end;
    // Trim trailing punctuation, keeping balanced parentheses (Wikipedia).
    while url_end > url_start {
        let c = line[url_end - 1];
        let trim = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '>' | ']' | '}' => true,
            ')' => {
                let opens = line[url_start..url_end].iter().filter(|&&c| c == '(').count();
                let closes = line[url_start..url_end].iter().filter(|&&c| c == ')').count();
                closes > opens
            }
            _ => false,
        };
        if !trim {
            break;
        }
        url_end -= 1;
    }
    if col >= url_end {
        return None;
    }
    let url: String = line[url_start..url_end].iter().collect();
    // A bare scheme is not a link.
    if SCHEMES.contains(&url.as_str()) {
        return None;
    }
    Some((url_start..url_end, url))
}

/// Whether `uri` may be opened (used for OSC 8 hyperlinks as well).
pub fn is_openable(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    SCHEMES.iter().any(|s| lower.starts_with(s))
}

fn is_break(c: char) -> bool {
    c.is_whitespace() || c == '\0' || matches!(c, '<' | '"' | '`' | '\u{2502}' | '\u{2503}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(text: &str, col: usize) -> Option<String> {
        let chars: Vec<char> = text.chars().collect();
        url_at(&chars, col).map(|(_, url)| url)
    }

    #[test]
    fn finds_urls_and_trims_punctuation() {
        let text = "see https://example.com/a?b=c. next";
        assert_eq!(find(text, 10).as_deref(), Some("https://example.com/a?b=c"));
        assert_eq!(find(text, 2), None);
        assert_eq!(find(text, 30), None);
        assert_eq!(
            find("(https://en.wikipedia.org/wiki/Rust_(language))", 5).as_deref(),
            Some("https://en.wikipedia.org/wiki/Rust_(language)")
        );
        assert_eq!(find("href=\"http://x.io\"", 8).as_deref(), Some("http://x.io"));
        assert_eq!(find("javascript:alert(1)", 3), None);
        assert_eq!(find("https://", 2), None);
    }

    #[test]
    fn only_known_schemes_open() {
        assert!(is_openable("https://a.b"));
        assert!(is_openable("MAILTO:x@y.z"));
        assert!(!is_openable("javascript:alert(1)"));
        assert!(!is_openable("vscode://x"));
    }
}
