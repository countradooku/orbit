//! A thread as Markdown: for copying a whole conversation, and quoting a
//! reply into the composer.

use orbit_proto::*;

/// Most characters a quotation carries; longer selections are cut on a word
/// boundary and marked, so a reply never pastes a whole essay back.
pub const QUOTE_LIMIT: usize = 1_500;

/// `text` as a Markdown quotation (`> ` on every line), cut to
/// [`QUOTE_LIMIT`] characters.
pub fn quote(text: &str) -> String {
    let text = text.trim();
    let cut = if text.chars().count() > QUOTE_LIMIT {
        let end = text.char_indices().nth(QUOTE_LIMIT).map(|(at, _)| at).unwrap_or(text.len());
        let end = text[..end].rfind(char::is_whitespace).filter(|&at| at > end / 2).unwrap_or(end);
        format!("{}…", text[..end].trim_end())
    } else {
        text.to_owned()
    };
    cut.lines().map(|line| if line.is_empty() { ">".to_owned() } else { format!("> {line}") }).collect::<Vec<_>>().join("\n")
}

/// The conversation as Markdown: your messages, the agent's replies, its
/// steps as a list, plans as checklists and questions with their answers.
/// Reasoning and bookkeeping are left out.
pub fn thread_markdown(title: &str, agent: &str, items: &[&Item]) -> String {
    let mut out = String::new();
    if !title.is_empty() {
        out.push_str(&format!("# {title}\n\n"));
    }
    let mut last_speaker = "";
    let mut in_steps = false;
    for item in items {
        let speaker = match &item.body {
            ItemBody::User(_) => "You",
            ItemBody::Assistant { .. } | ItemBody::Tool(_) | ItemBody::Plan { .. } | ItemBody::Request(_) => agent,
            _ => last_speaker,
        };
        let block = match &item.body {
            ItemBody::User(message) => message.text.trim().to_owned(),
            ItemBody::Assistant { text } if !text.trim().is_empty() => text.trim().to_owned(),
            ItemBody::Tool(call) => {
                let line = step_line(call, item.status);
                if line.is_empty() {
                    continue;
                }
                if in_steps {
                    out.push_str(&line);
                    out.push('\n');
                    continue;
                }
                in_steps = true;
                line + "\n"
            }
            ItemBody::Plan { steps } => steps
                .iter()
                .map(|s| format!("- [{}] {}", if s.state == StepState::Done { "x" } else { " " }, s.text))
                .collect::<Vec<_>>()
                .join("\n"),
            ItemBody::Request(request) => match (&request.kind, &request.resolution) {
                (RequestKind::Question { question, .. }, Some(Decision::Answer { text })) => {
                    format!("**Asked:** {question}\n\n**Answer:** {}", text.replace('\n', ", "))
                }
                (RequestKind::Question { question, .. }, _) => format!("**Asked:** {question}"),
                (RequestKind::Plan { plan }, _) => format!("**Proposed plan**\n\n{}", plan.trim()),
                _ => continue,
            },
            ItemBody::Notice { level: NoticeLevel::Error | NoticeLevel::Warning, text } => quote(text),
            _ => continue,
        };
        if !matches!(item.body, ItemBody::Tool(_)) && in_steps {
            in_steps = false;
            out.push('\n');
        }
        if speaker != last_speaker && !speaker.is_empty() {
            out.push_str(&format!("**{speaker}**\n\n"));
            last_speaker = speaker;
        }
        out.push_str(&block);
        out.push_str(if in_steps { "" } else { "\n\n" });
    }
    out.trim_end().to_owned() + "\n"
}

/// One tool call as a list item: `- Ran \`cargo test\` (exit 1)`.
fn step_line(call: &ToolCall, status: ItemStatus) -> String {
    let title = if call.title.is_empty() { call.name.as_str() } else { call.title.as_str() };
    let code = format!("`{}`", title.replace('`', "'"));
    let mut line = match call.kind {
        ToolKind::Command => format!("- Ran {code}"),
        ToolKind::Read => format!("- Read {code}"),
        ToolKind::Edit => format!("- Edited {code}"),
        ToolKind::Search => format!("- Searched for {code}"),
        ToolKind::Web => format!("- Looked up {title}"),
        ToolKind::Task => format!("- Subagent: {title}"),
        ToolKind::Todo => return String::new(),
        ToolKind::Mcp | ToolKind::Other => format!("- {title}"),
    };
    let (adds, dels) = call.files.iter().fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
    if adds + dels > 0 {
        line.push_str(&format!(" (+{adds} −{dels})"));
    }
    match (call.exit_code, status) {
        (Some(code), _) if code != 0 => line.push_str(&format!(" (exit {code})")),
        (_, ItemStatus::Failed) => line.push_str(" (failed)"),
        _ => {}
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u64, body: ItemBody) -> Item {
        Item { id: ItemId(id), turn: 1, status: ItemStatus::Done, body }
    }

    fn tool(id: u64, kind: ToolKind, title: &str, exit: Option<i32>) -> Item {
        item(
            id,
            ItemBody::Tool(ToolCall {
                call_id: id.to_string(),
                kind,
                name: "Bash".into(),
                title: title.into(),
                input: String::new(),
                output: String::new(),
                output_blob: None,
                exit_code: exit,
                files: Vec::new(),
            }),
        )
    }

    #[test]
    fn a_thread_reads_as_a_conversation() {
        let items = [
            item(1, ItemBody::User(UserMessage { text: "Fix the build".into(), ..Default::default() })),
            item(2, ItemBody::Reasoning { text: "hidden".into() }),
            tool(3, ToolKind::Command, "cargo build", Some(101)),
            tool(4, ToolKind::Edit, "src/lib.rs", None),
            item(5, ItemBody::Assistant { text: "Fixed: a missing `use`.".into() }),
        ];
        let refs: Vec<&Item> = items.iter().collect();
        assert_eq!(
            thread_markdown("Build", "Claude Code", &refs),
            "# Build\n\n**You**\n\nFix the build\n\n**Claude Code**\n\n- Ran `cargo build` (exit 101)\n- Edited `src/lib.rs`\n\nFixed: a missing `use`.\n"
        );
    }

    #[test]
    fn quotes_mark_every_line_and_stay_short() {
        assert_eq!(quote("one\n\ntwo"), "> one\n>\n> two");
        let long = "word ".repeat(1_000);
        let quoted = quote(&long);
        assert!(quoted.chars().count() < QUOTE_LIMIT + 10);
        assert!(quoted.ends_with("word…"));
    }
}
