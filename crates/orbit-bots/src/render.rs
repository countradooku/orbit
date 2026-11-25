//! Agent markdown as each platform shows it, cut to its message limit.
//!
//! Agents answer in GitHub-flavoured markdown. Telegram takes a small HTML
//! subset (more forgiving than its MarkdownV2, which rejects a message over
//! one stray character), Slack its own `mrkdwn`, and Discord markdown much
//! like the agents'. Long code does not belong in a chat bubble: blocks
//! over [`CODE_INLINE_MAX`] characters or [`CODE_LINES_MAX`] lines become
//! attached files, and the text says so where they were.

/// The markup a platform reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    /// Telegram's HTML subset (`parse_mode: "HTML"`).
    Html,
    /// Slack's `mrkdwn`.
    Mrkdwn,
    /// Discord's markdown.
    Markdown,
}

/// Code blocks longer than this go to a file.
pub const CODE_INLINE_MAX: usize = 1200;
pub const CODE_LINES_MAX: usize = 30;

/// A rendered message: text chunks each within the limit, and files that
/// hold the long code blocks.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    pub chunks: Vec<String>,
    pub files: Vec<(String, Vec<u8>)>,
}

enum Block<'a> {
    Text(&'a str),
    Code { lang: &'a str, body: &'a str },
}

fn blocks(text: &str) -> Vec<Block<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    loop {
        let Some(open) = find_fence(rest) else {
            if !rest.is_empty() {
                out.push(Block::Text(rest));
            }
            return out;
        };
        if open > 0 {
            out.push(Block::Text(&rest[..open]));
        }
        let after = &rest[open + 3..];
        let line_end = after.find('\n').unwrap_or(after.len());
        let lang = after[..line_end].trim();
        let body_start = (line_end + 1).min(after.len());
        let body = &after[body_start..];
        match find_fence(body) {
            Some(close) => {
                out.push(Block::Code { lang, body: body[..close].trim_end_matches('\n') });
                let tail = &body[close + 3..];
                rest = tail.strip_prefix('\n').unwrap_or(tail);
            }
            None => {
                // An unclosed fence runs to the end, as markdown renders it.
                out.push(Block::Code { lang, body: body.trim_end_matches('\n') });
                return out;
            }
        }
    }
}

/// A ``` at the start of a line.
fn find_fence(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(pos) = text[from..].find("```") {
        let at = from + pos;
        let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
        if text[line_start..at].trim().is_empty() {
            return Some(at);
        }
        from = at + 3;
    }
    None
}

fn extension(lang: &str) -> &str {
    match lang.to_ascii_lowercase().as_str() {
        "rust" | "rs" => "rs",
        "python" | "py" => "py",
        "javascript" | "js" => "js",
        "typescript" | "ts" => "ts",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "sh" | "bash" | "shell" | "zsh" => "sh",
        "powershell" | "ps1" | "pwsh" => "ps1",
        "diff" | "patch" => "diff",
        "go" => "go",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        "md" | "markdown" => "md",
        "c" => "c",
        "cpp" | "c++" => "cpp",
        "java" => "java",
        "kotlin" | "kt" => "kt",
        "swift" => "swift",
        _ => "txt",
    }
}

/// Renders `markdown` for `flavor` in chunks of at most `limit` characters.
pub fn render(markdown: &str, flavor: Flavor, limit: usize) -> Rendered {
    let mut files = Vec::new();
    let mut pieces: Vec<String> = Vec::new();
    for block in blocks(markdown) {
        match block {
            Block::Text(text) => {
                for line in text.lines() {
                    pieces.push(inline_line(line, flavor));
                }
            }
            Block::Code { lang, body } => {
                let lines = body.lines().count();
                if body.chars().count() > CODE_INLINE_MAX || lines > CODE_LINES_MAX {
                    let name = format!("snippet-{}.{}", files.len() + 1, extension(lang));
                    pieces.push(match flavor {
                        Flavor::Html => format!("<i>Attached {name} · {lines} lines</i>"),
                        Flavor::Mrkdwn | Flavor::Markdown => format!("_Attached {name} · {lines} lines_"),
                    });
                    files.push((name, body.as_bytes().to_vec()));
                } else {
                    pieces.push(code_block(lang, body, flavor));
                }
            }
        }
    }
    // Collapse runs of blank lines the extraction may leave.
    let mut lines: Vec<String> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        if piece.trim().is_empty() && lines.last().is_some_and(|l| l.trim().is_empty()) {
            continue;
        }
        lines.push(piece);
    }
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    Rendered { chunks: chunk(&lines, limit), files }
}

/// Plain text (no markup interpreted) escaped for `flavor`, in chunks.
pub fn plain(text: &str, flavor: Flavor, limit: usize) -> Vec<String> {
    let lines: Vec<String> = text.lines().map(|l| escape(l, flavor)).collect();
    chunk(&lines, limit)
}

/// Joins lines into chunks of at most `limit` characters, breaking between
/// lines (a code block counts as one line) and inside a line only when it
/// alone is too long.
fn chunk(lines: &[String], limit: usize) -> Vec<String> {
    let limit = limit.max(16);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for line in lines {
        let len = line.chars().count();
        let extra = if current.is_empty() { len } else { len + 1 };
        if current_len + extra <= limit {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
            current_len += extra;
            continue;
        }
        if !current.trim().is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current.clear();
        current_len = 0;
        if len <= limit {
            current.push_str(line);
            current_len = len;
        } else {
            let chars: Vec<char> = line.chars().collect();
            for part in chars.chunks(limit) {
                chunks.push(part.iter().collect());
            }
        }
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    chunks
}

fn code_block(lang: &str, body: &str, flavor: Flavor) -> String {
    match flavor {
        Flavor::Html => {
            let lang = lang.split_whitespace().next().unwrap_or("");
            if lang.is_empty() {
                format!("<pre>{}</pre>", escape(body, flavor))
            } else {
                format!("<pre><code class=\"language-{}\">{}</code></pre>", escape(lang, flavor), escape(body, flavor))
            }
        }
        // Slack shows no language; a newline after the fence keeps the
        // first line in the block.
        Flavor::Mrkdwn => format!("```\n{}\n```", escape(body, flavor)),
        Flavor::Markdown => format!("```{lang}\n{body}\n```"),
    }
}

/// Escapes what the platform would read as markup.
pub fn escape(text: &str, flavor: Flavor) -> String {
    match flavor {
        Flavor::Html | Flavor::Mrkdwn => text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"),
        Flavor::Markdown => text.to_owned(),
    }
}

/// One line of markdown, its inline markup converted.
fn inline_line(line: &str, flavor: Flavor) -> String {
    if flavor == Flavor::Markdown {
        return line.to_owned();
    }
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    // Headings read as bold lines.
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
        let text = inline(trimmed[hashes..].trim(), flavor);
        return match flavor {
            Flavor::Html => format!("<b>{text}</b>"),
            _ => format!("*{text}*"),
        };
    }
    // Bullets as dots.
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            return format!("{indent}• {}", inline(rest, flavor));
        }
    }
    if let Some(rest) = trimmed.strip_prefix("> ") {
        return match flavor {
            Flavor::Html => format!("<blockquote>{}</blockquote>", inline(rest, flavor)),
            _ => format!("&gt; {}", inline(rest, flavor)),
        };
    }
    format!("{indent}{}", inline(trimmed, flavor))
}

/// Inline spans: `code`, **bold**, *italic*, ~~strike~~ and [links](url).
fn inline(text: &str, flavor: Flavor) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut rest = text;
    // Code spans first: nothing inside them is markup.
    while let Some(open) = rest.find('`') {
        let Some(close) = rest[open + 1..].find('`') else { break };
        out.push_str(&spans(&rest[..open], flavor));
        let code = &rest[open + 1..open + 1 + close];
        match flavor {
            Flavor::Html => {
                out.push_str("<code>");
                out.push_str(&escape(code, flavor));
                out.push_str("</code>");
            }
            _ => {
                out.push('`');
                out.push_str(&escape(code, flavor));
                out.push('`');
            }
        }
        rest = &rest[open + 1 + close + 1..];
    }
    out.push_str(&spans(rest, flavor));
    out
}

fn spans(text: &str, flavor: Flavor) -> String {
    let text = links(&escape(text, flavor), flavor);
    let (bold, italic, strike) = match flavor {
        Flavor::Html => (("<b>", "</b>"), ("<i>", "</i>"), ("<s>", "</s>")),
        _ => (("\u{1}", "\u{1}"), ("_", "_"), ("~", "~")),
    };
    let text = pairs(&text, "**", bold);
    let text = pairs(&text, "__", bold);
    let text = pairs(&text, "~~", strike);
    let text = pairs(&text, "*", italic);
    // Slack bold is a single `*`, placed last so italics don't eat it.
    text.replace('\u{1}', "*")
}

/// `[text](url)` → `<a href>` or `<url|text>`.
fn links(text: &str, flavor: Flavor) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let parsed = after.find("](").and_then(|mid| {
            let close = after[mid + 2..].find(')')?;
            Some((&after[..mid], &after[mid + 2..mid + 2 + close], mid + 2 + close + 1))
        });
        match parsed {
            Some((label, url, used)) if !url.contains(' ') && !label.contains('[') => {
                out.push_str(&rest[..open]);
                match flavor {
                    Flavor::Html => out.push_str(&format!("<a href=\"{}\">{label}</a>", url.replace('"', "&quot;"))),
                    _ => out.push_str(&format!("<{url}|{label}>")),
                }
                rest = &after[used..];
            }
            _ => {
                out.push_str(&rest[..open + 1]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Replaces matched `delim … delim` pairs (non-space inside the edges)
/// with `open … close`; an unmatched delimiter stays as typed.
fn pairs(text: &str, delim: &str, (open, close): (&str, &str)) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find(delim) else { break };
        let inner_start = start + delim.len();
        let inner = &rest[inner_start..];
        let starts_word = inner.chars().next().is_some_and(|c| !c.is_whitespace());
        let end = inner.find(delim).filter(|&e| e > 0 && !inner[..e].ends_with(char::is_whitespace));
        match (starts_word, end) {
            (true, Some(end)) => {
                out.push_str(&rest[..start]);
                out.push_str(open);
                out.push_str(&inner[..end]);
                out.push_str(close);
                rest = &inner[end + delim.len()..];
            }
            _ => {
                out.push_str(&rest[..inner_start]);
                rest = inner;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_gets_safe_html() {
        let r = render("# Done\nUse **bold**, *it*, `a<b>` and [docs](https://x.dev/a?b=1&c).\n- one", Flavor::Html, 4096);
        assert_eq!(
            r.chunks,
            vec![
                "<b>Done</b>\nUse <b>bold</b>, <i>it</i>, <code>a&lt;b&gt;</code> and <a href=\"https://x.dev/a?b=1&amp;c\">docs</a>.\n• one"
            ]
        );
        let r = render("```rust\nfn main() {}\n```", Flavor::Html, 4096);
        assert_eq!(r.chunks, vec!["<pre><code class=\"language-rust\">fn main() {}</code></pre>"]);
    }

    #[test]
    fn slack_gets_mrkdwn() {
        let r = render("**Fixed** the *flaky* test in `a_b.rs`, see [PR](https://x/1) ~~old~~", Flavor::Mrkdwn, 3000);
        assert_eq!(r.chunks, vec!["*Fixed* the _flaky_ test in `a_b.rs`, see <https://x/1|PR> ~old~"]);
        assert_eq!(render("snake_case and 2 * 3 * 4", Flavor::Mrkdwn, 3000).chunks, vec!["snake_case and 2 * 3 * 4"]);
    }

    #[test]
    fn discord_keeps_markdown() {
        let text = "## Plan\n**bold** and\n```\ncode\n```";
        assert_eq!(render(text, Flavor::Markdown, 4000).chunks.join("\n"), text);
    }

    #[test]
    fn long_text_is_chunked_on_lines_within_the_limit() {
        let text: String = (0..500).map(|i| format!("Line {i} of a long answer\n")).collect();
        for (flavor, limit) in [(Flavor::Html, 4096), (Flavor::Mrkdwn, 3000), (Flavor::Markdown, 4000)] {
            let r = render(&text, flavor, limit);
            assert!(r.chunks.len() > 1);
            assert!(r.chunks.iter().all(|c| c.chars().count() <= limit), "{flavor:?}");
            assert_eq!(r.chunks.join("\n").lines().count(), 500, "no line lost");
        }
        let one = "x".repeat(9000);
        let r = render(&one, Flavor::Markdown, 4000);
        assert_eq!(r.chunks.iter().map(|c| c.len()).collect::<Vec<_>>(), vec![4000, 4000, 1000]);
    }

    #[test]
    fn long_code_becomes_a_file() {
        let code: String = (0..80).map(|i| format!("let x{i} = {i};\n")).collect();
        let text = format!("Here:\n```rust\n{code}```\nThat's all.");
        let r = render(&text, Flavor::Html, 4096);
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.files[0].0, "snippet-1.rs");
        assert!(String::from_utf8_lossy(&r.files[0].1).starts_with("let x0 = 0;"));
        assert_eq!(r.chunks, vec!["Here:\n<i>Attached snippet-1.rs · 80 lines</i>\nThat's all."]);
    }
}
