//! One-shot text generation in the engine: thread titles after the first
//! turn, and [`Command::GenerateText`] for commit messages, pull request
//! text and anything else other parts of Orbit need written.
//!
//! The text is written by the thread's own instance when it can do so
//! cheaply (Claude Code with Haiku, Codex with low effort), so it runs on
//! the account the user picked for that thread; otherwise by the first
//! signed-in Claude Code or Codex, and failing both by one headless turn of
//! the thread's own agent. Nothing waits on it: the engine hands back a
//! future and applies the result when it lands.

use std::collections::HashMap;

use crossfire::BlockingTxTrait;
use orbit_proto::*;

use super::orchestrator::{Deferred, Internal, Orchestrator};
use crate::provider::{Launch, oneshot};

/// Titles are cut to this many characters.
const TITLE_MAX: usize = 60;
/// How much of the conversation a title prompt carries.
const TITLE_INPUT: usize = 4_000;
/// How much input other tasks carry (diffs are long).
const TASK_INPUT: usize = 60_000;

/// The engine's text generator: runs a prompt on an instance. The default
/// uses [`oneshot`]; tests use the session launcher.
pub(crate) enum Generator {
    /// Claude Code and Codex through their one-shot CLIs, everything else
    /// through a headless session.
    Native,
    /// Always a headless session through the engine's launcher (tests and
    /// benchmarks, whose launcher is scripted).
    Launcher,
}

impl Orchestrator {
    /// The instance that writes text for a thread (or for no thread).
    fn text_launch(&self, thread: Option<ThreadId>) -> Option<Launch> {
        let config = thread.and_then(|t| self.store.thread(t)).map(|t| t.config.clone()).unwrap_or_else(|| self.store.defaults().clone());
        let own = self.instance_of(&config);
        let usable = |id: &str| self.providers.iter().any(|p| p.instance_id() == id && p.installed && p.signed_in != Some(false));
        let cheap = matches!(own.kind, ProviderKind::Claude | ProviderKind::Codex);
        if (cheap && usable(&own.id)) || matches!(self.generator, Generator::Launcher) {
            return Some(self.launch_of(&own));
        }
        for kind in [ProviderKind::Claude, ProviderKind::Codex] {
            if usable(kind.id()) {
                return Some(self.launch_of(&ProviderInstance::builtin(kind)));
            }
        }
        usable(&own.id).then(|| self.launch_of(&own))
    }

    /// A future that generates text for `prompt`.
    pub(crate) fn generate(&mut self, launch: Launch, prompt: String) -> Deferred {
        let native = matches!(self.generator, Generator::Native) && matches!(launch.kind, ProviderKind::Claude | ProviderKind::Codex);
        if native {
            return Box::pin(async move { oneshot::generate(launch, prompt).await.map(Reply::Text) });
        }
        let (tx, rx) = crossfire::mpsc::unbounded_async();
        let handle = (self.launcher)(launch.kind, oneshot::headless_spec(&launch), tx);
        Box::pin(async move {
            tokio::time::timeout(oneshot::TIMEOUT, oneshot::via_session(handle, rx, prompt))
                .await
                .unwrap_or_else(|_| Err("No answer in time.".into()))
                .map(Reply::Text)
        })
    }

    /// [`Command::GenerateText`].
    pub(crate) fn generate_text(&mut self, thread: Option<ThreadId>, task: TextTask, input: String) -> Deferred {
        let Some(launch) = self.text_launch(thread) else {
            return Box::pin(async { Err("No signed-in agent can write this. Set one up in Settings.".to_owned()) });
        };
        let prompt = task_prompt(&task, &input);
        let future = self.generate(launch, prompt);
        Box::pin(async move {
            match future.await? {
                Reply::Text(text) => Ok(Reply::Text(clean(&task, &text))),
                other => Ok(other),
            }
        })
    }

    /// Remembers the title seeded from the first message, so a generated
    /// title replaces it only if nobody renamed the thread meanwhile.
    pub(crate) fn note_seeded_title(&mut self, thread: ThreadId, title: String) {
        self.seeded_titles.insert(thread, title);
    }

    /// After a thread's first completed turn, writes it a real title.
    pub(crate) fn after_turn_for_title(&mut self, thread: ThreadId, completed: bool) {
        if !completed || !self.store.settings().agents.generate_titles {
            return;
        }
        let Some(seed) = self.seeded_titles.remove(&thread) else { return };
        let Some(t) = self.store.thread(thread) else { return };
        if t.title != seed {
            return; // renamed by the user or by the agent
        }
        let Ok(timeline) = self.store.load_timeline(thread) else { return };
        let conversation = title_input(timeline.items());
        if conversation.trim().is_empty() {
            return;
        }
        let Some(launch) = self.text_launch(Some(thread)) else { return };
        let future = self.generate(launch, task_prompt(&TextTask::ThreadTitle, &conversation));
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let title = match future.await {
                Ok(Reply::Text(text)) => clean(&TextTask::ThreadTitle, &text),
                Ok(_) => return,
                Err(err) => {
                    log::debug!("no generated title for {thread}: {err}");
                    return;
                }
            };
            let _ = tx.send(Internal::TitleGenerated { thread, seed, title });
        });
    }

    pub(crate) fn on_title_generated(&mut self, thread: ThreadId, seed: String, title: String) {
        if title.is_empty() {
            return;
        }
        self.update_thread(thread, |t| {
            if t.title == seed {
                t.title = title;
            }
        });
    }
}

/// Seeded titles waiting for their first turn, by thread.
pub(crate) type SeededTitles = HashMap<ThreadId, String>;

fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut at = max;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    &text[..at]
}

/// The first exchange of a thread, for its title.
fn title_input(items: &[Item]) -> String {
    let user = items.iter().find_map(|i| match &i.body {
        ItemBody::User(m) => Some(m.text.clone()),
        _ => None,
    });
    let reply = items.iter().rev().find_map(|i| match &i.body {
        ItemBody::Assistant { text } if i.turn == 1 && !text.trim().is_empty() => Some(text.clone()),
        _ => None,
    });
    let mut out = String::new();
    if let Some(user) = user {
        out.push_str(&format!("User: {}\n", cut(user.trim(), TITLE_INPUT / 2)));
    }
    if let Some(reply) = reply {
        out.push_str(&format!("Agent: {}\n", cut(reply.trim(), TITLE_INPUT / 2)));
    }
    out
}

/// The prompt for a task.
pub fn task_prompt(task: &TextTask, input: &str) -> String {
    let input = cut(input, TASK_INPUT);
    match task {
        TextTask::ThreadTitle => format!(
            "Write a title for this coding conversation: 3 to 7 words, sentence case, naming the task, \
             no quotes, no trailing period. Reply with the title only.\n\n<conversation>\n{input}\n</conversation>"
        ),
        TextTask::CommitMessage => format!(
            "Write a git commit message for these changes. First line: imperative summary, at most 72 \
             characters, no trailing period. Then a blank line and a short body explaining what changed and \
             why, wrapped at 72 columns, only if it adds something. Follow the repository's conventions if \
             the input shows them. Reply with the message only.\n\n<changes>\n{input}\n</changes>"
        ),
        TextTask::PullRequest => format!(
            "Write a pull request for these changes. First line: the title, at most 72 characters, no \
             trailing period. Then a blank line and the body in Markdown with a short \"## Summary\" and a \
             \"## Testing\" section. Follow a pull request template if the input includes one. Reply with \
             the title and body only.\n\n<changes>\n{input}\n</changes>"
        ),
        TextTask::Custom { instructions } => format!("{instructions}\n\n<input>\n{input}\n</input>"),
    }
}

/// Tidies a model's answer for its task: no code fences, no labels or
/// quotes around titles, bounded length.
pub fn clean(task: &TextTask, text: &str) -> String {
    let text = text.trim();
    let text = text
        .strip_prefix("```")
        .map(|rest| rest.split_once('\n').map(|(_, body)| body).unwrap_or(rest))
        .and_then(|body| body.trim_end().strip_suffix("```"))
        .unwrap_or(text)
        .trim();
    match task {
        TextTask::ThreadTitle => {
            let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
            let line = line.strip_prefix("Title:").or_else(|| line.strip_prefix("title:")).unwrap_or(line).trim();
            let line = line.trim_matches(['"', '\'', '`', '*', '#', ' ']).trim_end_matches('.');
            let mut title: String = line.chars().take(TITLE_MAX).collect();
            if line.chars().count() > TITLE_MAX {
                title = title.trim_end().to_owned();
                title.push('…');
            }
            if title.eq_ignore_ascii_case("title") { String::new() } else { title }
        }
        _ => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_lose_labels_quotes_and_periods() {
        assert_eq!(clean(&TextTask::ThreadTitle, "Title: \"Fix the login bug.\"\n"), "Fix the login bug");
        assert_eq!(clean(&TextTask::ThreadTitle, "```\nAdd dark mode\n```"), "Add dark mode");
        assert_eq!(clean(&TextTask::ThreadTitle, "title"), "");
        let long = "word ".repeat(30);
        assert!(clean(&TextTask::ThreadTitle, &long).chars().count() <= TITLE_MAX + 1);
    }

    #[test]
    fn prompts_carry_their_input_bounded() {
        let prompt = task_prompt(&TextTask::CommitMessage, &"x".repeat(TASK_INPUT * 2));
        assert!(prompt.len() < TASK_INPUT + 1_000);
        assert!(task_prompt(&TextTask::ThreadTitle, "User: hi").contains("User: hi"));
    }
}
