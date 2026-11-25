//! The composer: write, mention, attach, choose how the agent runs, send.
//!
//! * **Keyboard first.** `@` finds files, folders and threads, `$` the
//!   agent's skills, `/` commands; Up and Down in an empty composer recall
//!   earlier prompts; the send key and what sending does while a turn
//!   runs (queue or steer; the alternate key does the other) come from
//!   Settings › Composer.
//! * **Never lose a word.** The draft (text and attachments) is saved on the
//!   host shortly after typing stops, so it survives restarts, switching
//!   threads and reconnecting from another machine (see `drafts`).
//! * **No lag.** Each edit runs one bounded pass over the word under the
//!   caret; file ranking runs on the host, off the UI thread, and stale
//!   answers are dropped.
//! * **Say why before refusing.** When a message cannot be sent yet
//!   (attachments still storing, too long, agent not installed, offline),
//!   the reason sits next to the disabled Send button.
//!
//! The model list and every model option come from the provider's own
//! catalog at runtime; nothing here names a model or an effort level.

mod attach;
mod drafts;
mod history;
mod menu;
mod model_picker;
mod queue;
mod trigger;

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::component::attachment::{
    Attachment as AttachmentChip, AttachmentActions, AttachmentContent, AttachmentDescription, AttachmentMedia,
    AttachmentStatus, AttachmentTitle,
};
use gpui_kit::component::button::{Button, ButtonRounded, ButtonVariants as _, Toggle};
use gpui_kit::component::input::{
    Enter as InputEnter, Escape as InputEscape, IndentInline, InlineToken, InputContent, InputEvent, InputToken, MoveDown,
    MoveUp, Textarea, TextareaState,
};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::actions::SendAlternate;
use crate::app_state::{AppEvent, AppState};
use crate::assets::Lucide;
use crate::{settings, ui};

pub use attach::ContextChip;
use history::Recall;
use menu::{Builtin, Suggestion};
pub use queue::QueuePanel;
use trigger::{Trigger, TriggerKind};

pub enum ComposerEvent {
    Submit { text: String, attachments: Vec<Attachment>, steer: bool },
    Stop,
    ConfigChanged(ThreadConfig),
    /// Home only: run the new thread in its own worktree or not.
    WorktreeChosen(bool),
}

gpui_kit::actions!(
    composer,
    [
        /// Paste the clipboard's text inline, however long.
        PasteAsText,
        /// Edit the latest queued message in place.
        EditLastQueued,
        /// Step the model's reasoning effort.
        CycleEffort,
        /// Open the agent and model picker.
        OpenModelPicker,
    ]
);

/// The composer's own keys, bound once.
fn bind_keys(cx: &mut App) {
    struct Bound;
    impl Global for Bound {}
    if cx.has_global::<Bound>() {
        return;
    }
    cx.set_global(Bound);
    use crate::actions::platform;
    cx.bind_keys([
        KeyBinding::new(platform("cmd-shift-v", "ctrl-shift-v"), PasteAsText, Some("Composer")),
        KeyBinding::new("alt-up", EditLastQueued, Some("Composer")),
        KeyBinding::new(platform("cmd-shift-e", "ctrl-shift-e"), CycleEffort, Some("Composer")),
        KeyBinding::new(platform("cmd-shift-m", "ctrl-shift-m"), OpenModelPicker, Some("Composer")),
    ]);
}

/// What the send key and the alternate key do while a turn runs, as
/// (send, alternate) verbs.
fn follow_up_verbs(cx: &App) -> (&'static str, &'static str) {
    match settings::get(cx).composer.follow_up {
        FollowUp::Queue => ("queues", "sends now"),
        FollowUp::Steer => ("sends now", "queues"),
    }
}

/// The open completion list.
struct Completion {
    trigger: Trigger,
    items: Vec<Suggestion>,
    selected: usize,
    /// A file search for this trigger is on its way.
    searching: bool,
}

pub struct Composer {
    app: Entity<AppState>,
    target: DraftTarget,
    input: Entity<TextareaState>,
    config: ThreadConfig,
    attachments: Vec<Attachment>,
    /// Attachments being read or stored.
    attaching: usize,
    /// Why the last attachment was refused (too big, unreadable), shown by
    /// Send until the next edit.
    attach_error: Option<SharedString>,
    /// Image previews by blob hash, for images added in this session.
    thumbnails: HashMap<String, Arc<Image>>,
    /// Text of large pastes turned into attachments, by blob hash, so they
    /// can be put back inline.
    pasted: HashMap<String, SharedString>,
    running: bool,
    /// Live usage of the thread, for the meter. `None` on the home screen.
    usage: Option<Usage>,
    /// Where the agent works ("orbit", "Current checkout"), shown under the box.
    context: Option<String>,
    branch: Option<String>,
    /// Home only: whether the next thread gets its own worktree. `None`
    /// hides the choice (an existing thread's workspace is fixed).
    worktree_choice: Option<bool>,
    /// Home only: the project `@` mentions search.
    project: Option<ProjectId>,
    /// Derived from the text once per edit.
    chars: usize,
    has_text: bool,
    completion: Option<Completion>,
    /// The trigger the user closed with Escape (by its start), so the list
    /// stays closed until the caret leaves that word.
    dismissed: Option<usize>,
    /// Latest file search sent; older answers are dropped.
    file_search: u64,
    /// The latest file results, shown while a newer search runs.
    files: Vec<FileMatch>,
    recall: Recall,
    /// The last draft handed to the host.
    saved: Draft,
    save_task: Option<Task<()>>,
    queue_edit: Option<queue::QueueEdit>,
    model_picker: model_picker::ModelPicker,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    /// The Home composer, which starts new threads.
    pub fn new(app: Entity<AppState>, config: ThreadConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(DraftTarget::Home, app, config, window, cx)
    }

    /// A thread's composer.
    pub fn for_thread(
        thread: ThreadId,
        app: Entity<AppState>,
        config: ThreadConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(DraftTarget::Thread(thread), app, config, window, cx)
    }

    fn build(
        target: DraftTarget,
        app: Entity<AppState>,
        config: ThreadConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        bind_keys(cx);
        let enter_sends = settings::get(cx).composer.submit == SubmitKey::Enter;
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 14)
                .submit_on_enter(enter_sends)
                .placeholder("Describe a change, a fix or a question  ·  @ files  ·  $ skills  ·  / commands")
        });
        let input_sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { shift, secondary } => this.on_press_enter(*shift, *secondary, window, cx),
            InputEvent::Change => this.on_text_changed(cx),
            InputEvent::Blur => this.save_draft_now(cx),
            _ => {}
        });
        // Providers report their catalogs asynchronously; re-render when they do.
        let app_sub = cx.observe(&app, |this, _, cx| {
            this.follow_queue_edit(cx);
            cx.notify();
        });
        let draft_sub = cx.subscribe_in(&app, window, |this, _, event: &AppEvent, window, cx| {
            if let AppEvent::Draft(target) = event
                && *target == this.target
            {
                this.adopt_remote_draft(window, cx);
            }
        });
        let key_sub = cx.observe_global::<settings::SettingsModel>(|this, cx| {
            let enter_sends = settings::get(cx).composer.submit == SubmitKey::Enter;
            this.input.update(cx, |input, cx| input.set_submit_on_enter(enter_sends, cx));
            cx.notify();
        });
        let model_picker = model_picker::ModelPicker::new(window, cx);
        let mut composer = Self {
            app,
            target,
            input,
            config,
            attachments: Vec::new(),
            attaching: 0,
            attach_error: None,
            thumbnails: HashMap::new(),
            pasted: HashMap::new(),
            running: false,
            usage: None,
            context: None,
            branch: None,
            worktree_choice: None,
            project: None,
            chars: 0,
            has_text: false,
            completion: None,
            dismissed: None,
            file_search: 0,
            files: Vec::new(),
            recall: Recall::default(),
            saved: Draft::default(),
            save_task: None,
            queue_edit: None,
            model_picker,
            _subscriptions: vec![input_sub, app_sub, draft_sub, key_sub],
        };
        composer._subscriptions.push(cx.on_release(|this, cx| this.flush_draft(cx)));
        composer.restore_draft(window, cx);
        composer
    }

    pub fn config(&self) -> &ThreadConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: ThreadConfig, cx: &mut Context<Self>) {
        if self.config != config {
            self.config = config;
            cx.notify();
        }
    }

    pub fn set_running(&mut self, running: bool, cx: &mut Context<Self>) {
        if self.running != running {
            self.running = running;
            cx.notify();
        }
    }

    /// Sets the workspace and branch shown under the box.
    pub fn set_context(&mut self, context: Option<String>, branch: Option<String>, cx: &mut Context<Self>) {
        if self.context != context || self.branch != branch {
            self.context = context;
            self.branch = branch;
            cx.notify();
        }
    }

    pub fn set_worktree_choice(&mut self, choice: Option<bool>, cx: &mut Context<Self>) {
        if self.worktree_choice != choice {
            self.worktree_choice = choice;
            cx.notify();
        }
    }

    /// Home only: the project whose files `@` finds.
    pub fn set_project(&mut self, project: Option<ProjectId>, cx: &mut Context<Self>) {
        if self.project != project {
            self.project = project;
            self.files.clear();
            cx.notify();
        }
    }

    pub fn set_usage(&mut self, usage: Option<Usage>, cx: &mut Context<Self>) {
        if self.usage != usage {
            self.usage = usage;
            cx.notify();
        }
    }

    fn thread_id(&self) -> Option<ThreadId> {
        match self.target {
            DraftTarget::Thread(id) => Some(id),
            DraftTarget::Home => None,
        }
    }

    /// Moves keyboard focus into the composer's text field.
    pub fn focus(composer: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let input = composer.read(cx).input.clone();
        input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Adds `markdown` (a quotation the reader is replying to) to the end of
    /// the draft, keeps what was already written (mentions stay tokens), and
    /// puts the caret after it with focus in the box. Used by the timeline's
    /// Reply and Quote.
    pub fn append_to_draft(composer: &Entity<Self>, markdown: &str, window: &mut Window, cx: &mut App) {
        composer.update(cx, |this, cx| {
            this.input.update(cx, |input, cx| {
                let draft = input.value();
                let kept = draft.trim_end().len();
                let text = if kept == 0 { format!("{markdown}

") } else { format!("

{markdown}

") };
                // Drop trailing blank space, then append after what stays.
                input.set_selected_range(kept..draft.len(), cx);
                input.replace(text, window, cx);
                input.focus(window, cx);
            });
            this.on_text_changed(cx);
        });
    }

    /// Puts a message back (a failed send): its text and attachments.
    pub fn restore(composer: &Entity<Self>, text: &str, attachments: Vec<Attachment>, window: &mut Window, cx: &mut App) {
        composer.update(cx, |this, cx| {
            for a in attachments {
                if !this.attachments.iter().any(|x| x.hash == a.hash) {
                    this.attachments.push(a);
                }
            }
            this.replace_text(text, window, cx);
        });
    }

    /// Sets the text, showing `@` mentions as tokens, caret at the end.
    fn replace_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let mut content = InputContent::new(text.to_owned());
        for range in trigger::mention_ranges(text) {
            let mention = &text[range.clone()];
            let token = mention_token(trigger::mention_path(mention), mention);
            if let Ok(next) = content.clone().with_token(range, token) {
                content = next;
            }
        }
        self.input.update(cx, |input, cx| {
            input.set_value(content, window, cx);
            let end = input.value().len();
            input.set_selected_range(end..end, cx);
        });
        self.on_text_changed(cx);
    }

    // ── Editing ──────────────────────────────────────────────────────────

    /// Everything derived from the text, once per edit: the counts behind
    /// Send, the completion list, the draft timer.
    fn on_text_changed(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value();
        self.chars = text.chars().count();
        self.has_text = !text.trim().is_empty();
        if !self.recall.is_showing(&text) {
            self.recall.reset();
        }
        if self.attaching == 0 {
            self.attach_error = None;
        }
        self.update_completion(&text, cx);
        self.schedule_draft_save(cx);
        cx.notify();
    }

    fn update_completion(&mut self, text: &str, cx: &mut Context<Self>) {
        let input = self.input.read(cx);
        let caret = input.cursor();
        let selection_empty = input.selected_range().is_empty();
        let in_token = input.tokens().iter().any(|t| t.range().start < caret && caret <= t.range().end);
        let trigger = trigger::active_trigger(text, caret).filter(|_| selection_empty && !in_token);
        let Some(trigger) = trigger else {
            self.completion = None;
            self.dismissed = None;
            return;
        };
        if self.dismissed == Some(trigger.range.start) {
            self.completion = None;
            return;
        }
        self.dismissed = None;
        let previous = self.completion.take();
        let same_word = previous
            .as_ref()
            .is_some_and(|c| c.trigger.range.start == trigger.range.start && c.trigger.kind == trigger.kind);
        let searching = trigger.kind == TriggerKind::Mention && self.search_files(&trigger.query, cx);
        let items = self.suggestions(&trigger, cx);
        let selected = if same_word { previous.map_or(0, |p| p.selected).min(items.len().saturating_sub(1)) } else { 0 };
        self.completion = Some(Completion { trigger, items, selected, searching });
    }

    /// The rows for a trigger from what is known now; file rows come from
    /// the last search until the next answer lands.
    fn suggestions(&self, trigger: &Trigger, cx: &App) -> Vec<Suggestion> {
        let state = self.app.read(cx).state();
        let query = trigger.query.as_str();
        match trigger.kind {
            TriggerKind::Command => {
                let mut items: Vec<Suggestion> = menu::rank_builtins(query, self.thread_id().is_some()).into_iter().map(Suggestion::Builtin).collect();
                if let Some(provider) = state.provider_for(&self.config) {
                    items.extend(
                        menu::rank_commands(query, &provider.slash_commands, menu::MAX_ROWS)
                            .into_iter()
                            .filter(|c| !Builtin::replaces(&c.name))
                            .map(Suggestion::Command),
                    );
                }
                items.truncate(menu::MAX_ROWS);
                items
            }
            TriggerKind::Skill => state
                .provider_for(&self.config)
                .map(|p| menu::rank_commands(query, &p.slash_commands, menu::MAX_ROWS))
                .unwrap_or_default()
                .into_iter()
                .map(Suggestion::Command)
                .collect(),
            TriggerKind::Mention => {
                let mut items: Vec<Suggestion> = self
                    .files
                    .iter()
                    .filter(|f| query.is_empty() || orbit_core::fuzzy::score_path(query, &f.path).is_some())
                    .take(menu::MAX_ROWS)
                    .cloned()
                    .map(Suggestion::File)
                    .collect();
                let projects: HashMap<ProjectId, &str> = state.projects.iter().map(|p| (p.id, p.name.as_str())).collect();
                let threads = state.threads.values().filter(|t| !t.archived).map(|t| {
                    (t.id, t.title.as_str(), t.project.and_then(|p| projects.get(&p).copied()), t.updated_ms)
                });
                items.extend(menu::rank_threads(query, threads, self.thread_id(), menu::MAX_THREAD_ROWS));
                items
            }
        }
    }

    /// Asks the host for files matching `query`. Returns whether a search
    /// was sent.
    fn search_files(&mut self, query: &str, cx: &mut Context<Self>) -> bool {
        let thread = self.thread_id();
        let project = if thread.is_some() { None } else { self.project };
        if thread.is_none() && project.is_none() {
            return false;
        }
        self.file_search += 1;
        let seq = self.file_search;
        let command =
            Command::SearchFiles { thread, project, query: query.to_owned(), limit: menu::MAX_ROWS as u32 };
        let task = self.app.update(cx, |app, cx| app.call(command, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if seq != this.file_search {
                    return; // a newer search is on its way
                }
                if let Ok(Reply::Files(files)) = result {
                    this.files = files;
                }
                let trigger = match this.completion.as_ref() {
                    Some(c) if c.trigger.kind == TriggerKind::Mention => c.trigger.clone(),
                    _ => return,
                };
                let items = this.suggestions(&trigger, cx);
                if let Some(completion) = this.completion.as_mut() {
                    completion.searching = false;
                    completion.selected = completion.selected.min(items.len().saturating_sub(1));
                    completion.items = items;
                }
                cx.notify();
            });
        })
        .detach();
        true
    }

    fn accept_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(completion) = self.completion.take() else { return };
        let Some(item) = completion.items.get(completion.selected).cloned() else { return };
        let range = completion.trigger.range.clone();
        match item {
            Suggestion::File(file) => {
                let text = trigger::mention_text(&file.path);
                let token = mention_token(&file.path, &text);
                self.insert_token(range, token, window, cx);
            }
            Suggestion::Thread { id, title, .. } => {
                self.replace_range(range, "", window, cx);
                self.add_context(ContextChip::Thread { id, title }, window, cx);
            }
            Suggestion::Command(command) if completion.trigger.kind == TriggerKind::Command => {
                self.replace_range(range, &format!("/{} ", command.name), window, cx);
            }
            Suggestion::Command(command) => {
                let text = self.skill_text(&command.name, range.start == 0);
                let token = InlineToken::new(format!("skill:{}", command.name), text).with_label(command.name.clone());
                self.insert_token(range, token, window, cx);
            }
            Suggestion::Builtin(builtin) => {
                self.replace_range(range, "", window, cx);
                match builtin {
                    Builtin::Model => self.model_picker.open(window, cx),
                    Builtin::Plan => {
                        let plan = self.config.mode != Mode::Plan;
                        self.update_config(|c| c.mode = if plan { Mode::Plan } else { Mode::Build }, cx);
                    }
                    Builtin::Clear => {
                        if let Some(thread) = self.thread_id() {
                            self.app.update(cx, |app, cx| app.run(Command::NewSession { thread }, window, cx));
                        }
                    }
                }
            }
        }
        self.dismissed = None;
        self.on_text_changed(cx);
    }

    /// Replaces `range` with a token and a space after it.
    fn insert_token(&mut self, range: std::ops::Range<usize>, token: InlineToken, window: &mut Window, cx: &mut Context<Self>) {
        let plain = token.text().to_string();
        self.input.update(cx, |input, cx| {
            if input.replace_range_with_token(range.clone(), token, window, cx).is_ok() {
                input.insert(" ", window, cx);
            } else {
                input.set_selected_range(range, cx);
                input.replace(format!("{plain} "), window, cx);
            }
        });
    }

    /// How a skill is written for the current agent: Codex reads `$name`
    /// anywhere; Claude Code runs `/name` when it opens the message.
    fn skill_text(&self, name: &str, at_start: bool) -> String {
        match self.config.provider {
            ProviderKind::Codex => format!("${name}"),
            _ if at_start => format!("/{name}"),
            _ => format!("the /{name} skill"),
        }
    }

    fn replace_range(&mut self, range: std::ops::Range<usize>, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_selected_range(range, cx);
            input.replace(text.to_owned(), window, cx);
        });
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(c) = self.completion.as_mut()
            && !c.items.is_empty()
        {
            let len = c.items.len() as isize;
            c.selected = (c.selected as isize + delta).rem_euclid(len) as usize;
            cx.notify();
        }
    }

    fn has_menu(&self) -> bool {
        self.completion.as_ref().is_some_and(|c| !c.items.is_empty())
    }

    fn input_focused(&self, window: &Window, cx: &App) -> bool {
        self.input.read(cx).focus_handle(cx).is_focused(window)
    }

    // ── Keys ─────────────────────────────────────────────────────────────

    fn on_move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        if !self.input_focused(window, cx) {
            return;
        }
        if self.has_menu() {
            self.move_selection(-1, cx);
            cx.stop_propagation();
            return;
        }
        let text = self.input.read(cx).value();
        if !text.trim().is_empty() && !self.recall.is_showing(&text) {
            return;
        }
        let entries = self.history_entries(cx);
        if let Some(prompt) = self.recall.older(move || entries).map(str::to_owned) {
            self.show_recalled(&prompt, window, cx);
            cx.stop_propagation();
        }
    }

    fn on_move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        if !self.input_focused(window, cx) {
            return;
        }
        if self.has_menu() {
            self.move_selection(1, cx);
            cx.stop_propagation();
            return;
        }
        let text = self.input.read(cx).value();
        if !self.recall.is_showing(&text) {
            return;
        }
        if let Some(prompt) = self.recall.newer().map(str::to_owned) {
            self.show_recalled(&prompt, window, cx);
            cx.stop_propagation();
        }
    }

    fn show_recalled(&mut self, prompt: &str, window: &mut Window, cx: &mut Context<Self>) {
        // Showing a recalled prompt is not an edit: keep the walk going.
        let mut recall = std::mem::take(&mut self.recall);
        self.replace_text(prompt, window, cx);
        std::mem::swap(&mut self.recall, &mut recall);
    }

    /// Prompts to recall, oldest first: this thread's messages, or on Home
    /// everything sent in this session.
    fn history_entries(&self, cx: &App) -> Vec<String> {
        match self.target {
            DraftTarget::Home => history::session_prompts(cx),
            DraftTarget::Thread(id) => self
                .app
                .read(cx)
                .state()
                .timeline(id)
                .map(|t| {
                    t.items()
                        .iter()
                        .filter_map(|i| match &i.body {
                            ItemBody::User(m) if i.status == ItemStatus::Done => Some(m.text.clone()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn on_enter_action(&mut self, _: &InputEnter, window: &mut Window, cx: &mut Context<Self>) {
        if self.input_focused(window, cx) && self.has_menu() {
            self.accept_completion(window, cx);
            cx.stop_propagation();
        }
    }

    fn on_tab(&mut self, _: &IndentInline, window: &mut Window, cx: &mut Context<Self>) {
        if self.input_focused(window, cx) && self.has_menu() {
            self.accept_completion(window, cx);
            cx.stop_propagation();
        }
    }

    fn on_escape(&mut self, _: &InputEscape, window: &mut Window, cx: &mut Context<Self>) {
        if !self.input_focused(window, cx) {
            return;
        }
        if let Some(completion) = self.completion.take() {
            self.dismissed = Some(completion.trigger.range.start);
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn on_paste_as_text(&mut self, _: &PasteAsText, window: &mut Window, cx: &mut Context<Self>) {
        if self.input_focused(window, cx) {
            self.paste_as_text(window, cx);
        }
    }

    fn on_edit_last_queued(&mut self, _: &EditLastQueued, window: &mut Window, cx: &mut Context<Self>) {
        if !self.input_focused(window, cx) || !self.edit_last_queued(window, cx) {
            cx.propagate();
        }
    }

    fn on_cycle_effort(&mut self, _: &CycleEffort, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_effort(cx);
    }

    fn on_open_model_picker(&mut self, _: &OpenModelPicker, window: &mut Window, cx: &mut Context<Self>) {
        self.model_picker.open(window, cx);
    }

    fn on_press_enter(&mut self, shift: bool, secondary: bool, window: &mut Window, cx: &mut Context<Self>) {
        let alternate = match (settings::get(cx).composer.submit, secondary, shift) {
            (SubmitKey::Enter, false, false) | (SubmitKey::ModEnter, true, false) => false,
            (SubmitKey::Enter, true, _) => true,
            _ => return, // a new line
        };
        self.submit(alternate, window, cx);
    }

    /// The alternate send key: the other follow-up while a turn runs. With
    /// nothing typed it sends the oldest queued message now.
    fn on_send_alternate(&mut self, _: &SendAlternate, window: &mut Window, cx: &mut Context<Self>) {
        if self.has_text || !self.attachments.is_empty() {
            self.submit(true, window, cx);
        } else {
            self.steer_oldest_queued(window, cx);
        }
    }

    /// Why Send is unavailable right now, if it is.
    fn blocker(&self, cx: &App) -> Option<String> {
        let app = self.app.read(cx);
        let state = app.state();
        if let Some(orbit_net::ConnectionStatus::Disconnected { .. }) = app.connection() {
            return Some("Offline. Your message is kept; send it once reconnected.".into());
        }
        if let Some(thread) = self.thread_id().and_then(|id| state.threads.get(&id))
            && thread.archived
        {
            return Some("This thread is archived. Unarchive it to continue.".into());
        }
        if let Some(provider) = state.provider_for(&self.config) {
            let checking = provider.message.as_deref() == Some("Checking…");
            if !provider.installed && !checking {
                return Some(format!("{} isn’t installed. Pick another agent.", provider.display_name()));
            }
            if provider.signed_in == Some(false) {
                return Some(format!("Sign in to {} first.", provider.display_name()));
            }
        }
        if self.attaching > 0 {
            return Some(match self.attaching {
                1 => "Storing 1 attachment…".into(),
                n => format!("Storing {n} attachments…"),
            });
        }
        if self.chars > MAX_MESSAGE_CHARS {
            return Some(format!("{} characters over the limit", group_digits((self.chars - MAX_MESSAGE_CHARS) as u64)));
        }
        if self.attachments.len() > MAX_ATTACHMENTS {
            return Some(format!("Up to {MAX_ATTACHMENTS} attachments"));
        }
        None
    }

    /// Sends the message. While a turn runs it is queued or steers the turn
    /// as the follow-up setting says; `alternate` does the opposite.
    fn submit(&mut self, alternate: bool, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::component::notification::Notification;
        if self.has_menu() {
            return;
        }
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        if let Some(reason) = self.blocker(cx) {
            if self.attaching == 0 {
                window.push_notification(Notification::warning(reason), cx);
            }
            return;
        }
        if let Err(reason) = validate_message(&text, &self.attachments) {
            window.push_notification(Notification::warning(reason), cx);
            return;
        }
        history::remember(&text, cx);
        self.recall.reset();
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        let attachments = std::mem::take(&mut self.attachments);
        self.completion = None;
        self.on_text_changed(cx);
        self.clear_draft(cx);
        let steer_by_default = settings::get(cx).composer.follow_up == FollowUp::Steer;
        cx.emit(ComposerEvent::Submit { text, attachments, steer: self.running && (steer_by_default != alternate) });
        cx.notify();
    }

    fn update_config(&mut self, f: impl FnOnce(&mut ThreadConfig), cx: &mut Context<Self>) {
        let mut config = self.config.clone();
        f(&mut config);
        if config != self.config {
            self.config = config.clone();
            cx.emit(ComposerEvent::ConfigChanged(config));
            cx.notify();
        }
    }

    // ── Rendering ────────────────────────────────────────────────────────

    fn render_completion(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let completion = self.completion.as_ref()?;
        let theme = cx.theme().clone();
        let empty_note: Option<SharedString> = if completion.items.is_empty() {
            Some(match completion.trigger.kind {
                TriggerKind::Mention if completion.searching && self.files.is_empty() => "Finding files…".into(),
                TriggerKind::Mention if self.thread_id().is_none() && self.project.is_none() => {
                    "Pick a project to mention its files.".into()
                }
                TriggerKind::Mention => format!("No files or threads match “{}”", completion.trigger.query).into(),
                TriggerKind::Skill => match self.app.read(cx).state().provider_for(&self.config) {
                    Some(p) if p.slash_commands.is_empty() => {
                        format!("{} hasn’t reported any skills yet.", p.display_name()).into()
                    }
                    _ => format!("No skills match “{}”", completion.trigger.query).into(),
                },
                TriggerKind::Command => return None,
            })
        } else {
            None
        };
        let selected = completion.selected;
        let mut list = v_flex().gap_0p5();
        let mut previous_section: Option<&'static str> = None;
        for (ix, item) in completion.items.iter().enumerate() {
            let section = match item {
                Suggestion::File(_) => "Files",
                Suggestion::Thread { .. } => "Threads",
                Suggestion::Command(_) | Suggestion::Builtin(_) => "Commands",
            };
            if completion.trigger.kind == TriggerKind::Mention && previous_section != Some(section) {
                list = list.child(
                    div()
                        .px_2()
                        .pt_1()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(section),
                );
            }
            previous_section = Some(section);
            list = list.child(self.render_suggestion(ix, item, ix == selected, &completion.trigger, cx));
        }
        let has_items = !completion.items.is_empty();
        Some(
            v_flex()
                .id("composer-completion")
                .mb_1()
                .p_1()
                .max_h(rems(22.))
                .overflow_y_scroll()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .text_color(theme.popover_foreground)
                .shadow_md()
                .child(list)
                .when_some(empty_note, |el, note| {
                    el.child(
                        h_flex()
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .when(completion.searching, |el| el.child(Spinner::new().xsmall()))
                            .child(note),
                    )
                })
                .when(has_items, |el| {
                    el.child(
                        div()
                            .px_2()
                            .pt_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("↑↓ to choose · Enter or Tab to insert · Esc to close"),
                    )
                }),
        )
    }

    fn render_suggestion(
        &self,
        ix: usize,
        item: &Suggestion,
        selected: bool,
        trigger: &Trigger,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let highlight =
            HighlightStyle { color: Some(theme.primary), font_weight: Some(FontWeight::SEMIBOLD), ..Default::default() };
        let query = trigger.query.as_str();
        let muted = theme.muted_foreground;
        let row = h_flex()
            .id(SharedString::from(format!("suggestion-{ix}")))
            .px_2()
            .py_1()
            .gap_2()
            .rounded(theme.radius)
            .text_sm()
            .cursor_default()
            .when(selected, |el| el.bg(theme.accent).text_color(theme.accent_foreground))
            .when(!selected, |el| el.hover(|s| s.bg(theme.muted)))
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(c) = this.completion.as_mut() {
                    c.selected = ix;
                }
                this.accept_completion(window, cx);
            }));
        match item {
            Suggestion::File(file) => {
                let (name, folder) = menu::split_path(&file.path);
                let positions = orbit_core::fuzzy::find_path(query, &file.path).map(|m| m.positions).unwrap_or_default();
                let name_start = file.path.len() - name.len();
                let marks: Vec<(std::ops::Range<usize>, HighlightStyle)> = positions
                    .iter()
                    .filter(|&&p| p >= name_start)
                    .map(|&p| (p - name_start..char_end(&file.path, p) - name_start, highlight))
                    .collect();
                row.child(Icon::new(if file.is_dir { IconName::Folder } else { IconName::File }).xsmall().text_color(muted))
                    .child(div().flex_none().child(StyledText::new(name.to_owned()).with_highlights(marks)))
                    .child(div().flex_1().min_w_0().truncate().text_xs().text_color(muted).child(folder.to_owned()))
            }
            Suggestion::Thread { title, project, .. } => row
                .child(Icon::new(Lucide::MessageSquare).xsmall().text_color(muted))
                .child(div().flex_1().min_w_0().truncate().child(title.clone()))
                .when_some(project.clone(), |el, p| el.child(div().text_xs().text_color(muted).child(p))),
            Suggestion::Command(command) => {
                let skill = trigger.kind == TriggerKind::Skill;
                row.child(Icon::new(if skill { Lucide::Sparkles } else { Lucide::Command }).xsmall().text_color(muted))
                    .child(
                        div()
                            .flex_none()
                            .font_family(theme.mono_font_family.clone())
                            .child(format!("{}{}", if skill { "$" } else { "/" }, command.name)),
                    )
                    .when(!command.argument_hint.is_empty(), |el| {
                        el.child(div().flex_none().text_xs().text_color(muted).child(command.argument_hint.clone()))
                    })
                    .child(div().flex_1().min_w_0().truncate().text_xs().text_color(muted).child(command.description.clone()))
            }
            Suggestion::Builtin(builtin) => row
                .child(Icon::new(Lucide::Orbit).xsmall().text_color(muted))
                .child(div().flex_none().font_family(theme.mono_font_family.clone()).child(format!("/{}", builtin.name())))
                .child(div().flex_1().min_w_0().truncate().text_xs().text_color(muted).child(builtin.description())),
        }
    }

    fn render_plan_toggle(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let supports_plan = self
            .app
            .read(cx)
            .state()
            .provider_for(&self.config)
            .is_none_or(|p| p.modes.is_empty() || p.modes.iter().any(|m| m.mode == Mode::Plan));
        if !supports_plan {
            return None;
        }
        let planning = self.config.mode == Mode::Plan;
        Some(
            Toggle::new("plan-mode")
                .small()
                .icon(Icon::new(Lucide::ListTodo))
                .label("Plan")
                .tooltip("Plan first: the agent proposes a plan and waits for your go-ahead (/plan)")
                .checked(planning)
                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                    let checked = *checked;
                    this.update_config(|c| c.mode = if checked { Mode::Plan } else { Mode::Build }, cx)
                }))
                .into_any_element(),
        )
    }

    /// Workspace, branch and access: chosen once, read often. They sit
    /// under the box, quiet, out of the way of writing.
    fn render_context_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let access = self.config.access;
        let warn = access == Access::Full;
        let this = cx.weak_entity();
        h_flex()
            .px_1()
            .pt_1p5()
            .gap_1()
            .text_xs()
            .text_color(theme.muted_foreground)
            .map(|el| match (self.worktree_choice, self.context.clone()) {
                (Some(worktree), _) => {
                    let this = this.clone();
                    el.child(
                        Button::new("workspace-choice")
                            .ghost()
                            .xsmall()
                            .icon(if worktree { Icon::new(Lucide::GitFork) } else { Icon::new(IconName::Folder) })
                            .label(if worktree { "New worktree" } else { "Current checkout" })
                            .dropdown_menu(move |menu, _, _| {
                                let (a, b) = (this.clone(), this.clone());
                                menu.item(
                                    PopupMenuItem::new("Current checkout — work in the project folder")
                                        .checked(!worktree)
                                        .on_click(move |_, _, cx| {
                                            let _ = a.update(cx, |_, cx| cx.emit(ComposerEvent::WorktreeChosen(false)));
                                        }),
                                )
                                .item(
                                    PopupMenuItem::new("New worktree — an isolated branch and folder")
                                        .checked(worktree)
                                        .on_click(move |_, _, cx| {
                                            let _ = b.update(cx, |_, cx| cx.emit(ComposerEvent::WorktreeChosen(true)));
                                        }),
                                )
                            }),
                    )
                }
                (None, Some(label)) => {
                    el.child(h_flex().gap_1().px_1p5().child(Icon::new(IconName::Folder).xsmall()).child(label))
                }
                (None, None) => el,
            })
            .when_some(self.branch.clone(), |el, branch| {
                el.child(h_flex().gap_1().px_1p5().child(Icon::new(Lucide::GitBranch).xsmall()).child(branch))
            })
            .child(div().flex_1())
            .when_some(self.usage, |el, usage| el.child(ui::ContextMeter::new("composer-meter", usage)))
            .child(
                Button::new("access")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(if warn { Lucide::ShieldAlert } else { Lucide::ShieldCheck }))
                    .label(access.label())
                    .when(warn, |b| b.text_color(theme.warning))
                    .dropdown_menu(move |mut menu, _, _| {
                        for level in Access::ALL {
                            let this = this.clone();
                            menu = menu.item(PopupMenuItem::new(access_description(level)).checked(level == access).on_click(
                                move |_, _, cx| {
                                    let _ = this.update(cx, |this, cx| this.update_config(|c| c.access = level, cx));
                                },
                            ));
                        }
                        menu
                    }),
            )
    }

    fn render_attachments(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.attachments.is_empty() && self.attaching == 0 {
            return None;
        }
        let mut row = h_flex().flex_wrap().gap_2().px_3().pt_3();
        for attachment in &self.attachments {
            let hash = attachment.hash.clone();
            let kind = attach::context_kind(&attachment.mime);
            let description: SharedString = match (kind, self.pasted.get(&hash)) {
                (Some(_), Some(text)) => {
                    format!("{} lines · {}", group_digits(text.lines().count() as u64), attach::size_label(attachment.size))
                        .into()
                }
                (Some(kind), None) => kind.label().into(),
                (None, _) => attach::size_label(attachment.size).into(),
            };
            let mut chip = AttachmentChip::new()
                .id(SharedString::from(format!("att-{hash}")))
                .small()
                .content(
                    AttachmentContent::new()
                        .title(AttachmentTitle::new(attachment.name.clone()))
                        .description(AttachmentDescription::new(description)),
                )
                .on_remove({
                    let hash = hash.clone();
                    cx.listener(move |this, _, _, cx| {
                        this.attachments.retain(|a| a.hash != hash);
                        this.schedule_draft_save(cx);
                        cx.notify();
                    })
                });
            if let Some(image) = self.thumbnails.get(&hash) {
                chip = chip.media(AttachmentMedia::new().src(image.clone()));
            } else if let Some(kind) = kind {
                chip = chip.tooltip(kind.tooltip());
            }
            if self.pasted.contains_key(&hash) {
                let hash = hash.clone();
                chip = chip.actions(
                    AttachmentActions::new().child(
                        Button::new(SharedString::from(format!("inline-{hash}")))
                            .ghost()
                            .xsmall()
                            .label("Insert as text")
                            .on_click(cx.listener(move |this, _, window, cx| this.inline_pasted(&hash, window, cx))),
                    ),
                );
            }
            row = row.child(chip);
        }
        if self.attaching > 0 {
            row = row.child(
                AttachmentChip::new()
                    .id("att-storing")
                    .small()
                    .status(AttachmentStatus::Uploading)
                    .content(AttachmentContent::new().title(AttachmentTitle::new(match self.attaching {
                        1 => "Storing…".to_owned(),
                        n => format!("Storing {n}…"),
                    }))),
            );
        }
        Some(row)
    }

    fn render_send(&self, blocker: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let has_content = self.has_text || !self.attachments.is_empty();
        if self.running && !has_content {
            return Button::new("stop")
                .icon(Icon::new(IconName::Square))
                .rounded(ButtonRounded::Size(px(999.)))
                .tooltip("Stop (Ctrl+.)")
                .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::Stop)))
                .into_any_element();
        }
        let (send_key, alternate_key) = settings::send_keys(cx);
        let tooltip: SharedString = match blocker {
            Some(reason) => reason.to_owned().into(),
            None if self.running => {
                let (verb, other) = follow_up_verbs(cx);
                format!("{send_key} {verb} · {alternate_key} {other}").into()
            }
            None => format!("Send ({send_key}) · right-click to change the send key").into(),
        };
        let submit = settings::get(cx).composer.submit;
        let mod_key = settings::mod_key();
        div()
            .id("send-wrap")
            .child(
                Button::new("send")
                    .primary()
                    .icon(Icon::new(IconName::ArrowUp))
                    .rounded(ButtonRounded::Size(px(999.)))
                    .tooltip(tooltip)
                    .disabled(!has_content || blocker.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.submit(false, window, cx))),
            )
            // A quick switch for the send key; Settings › Composer has it too.
            .context_menu(move |menu, _, _| {
                menu.label("Send with")
                    .item(
                        PopupMenuItem::new("Enter (Shift+Enter for a new line)")
                            .checked(submit == SubmitKey::Enter)
                            .on_click(|_, _, cx| settings::update(cx, |s| s.composer.submit = SubmitKey::Enter)),
                    )
                    .item(
                        PopupMenuItem::new(format!("{mod_key}+Enter (Enter for a new line)"))
                            .checked(submit == SubmitKey::ModEnter)
                            .on_click(|_, _, cx| settings::update(cx, |s| s.composer.submit = SubmitKey::ModEnter)),
                    )
            })
            .into_any_element()
    }

    fn render_status(&self, blocker: Option<&str>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let has_content = self.has_text || !self.attachments.is_empty();
        let (text, warn): (SharedString, bool) = match blocker {
            None if self.attach_error.is_some() => (self.attach_error.clone().unwrap_or_default(), true),
            // Storing is progress, not a problem.
            Some(reason) if self.attaching > 0 => (reason.to_owned().into(), false),
            Some(reason) if has_content => (reason.to_owned().into(), true),
            _ if self.running && has_content => {
                let (send_key, alternate_key) = settings::send_keys(cx);
                let (verb, other) = follow_up_verbs(cx);
                (format!("{send_key} {verb} · {alternate_key} {other}").into(), false)
            }
            _ if self.running => ("Working…".into(), false),
            _ if self.chars > MAX_MESSAGE_CHARS * 9 / 10 => (
                format!("{} / {}", group_digits(self.chars as u64), group_digits(MAX_MESSAGE_CHARS as u64)).into(),
                false,
            ),
            _ => return None,
        };
        Some(
            h_flex()
                .mr_1()
                .gap_1()
                .min_w_0()
                .text_xs()
                .text_color(if warn { theme.warning } else { theme.muted_foreground })
                .when(warn, |el| el.child(Icon::new(IconName::TriangleAlert).xsmall()))
                .when(self.attaching > 0, |el| el.child(Spinner::new().xsmall()))
                .child(div().truncate().child(text))
                .into_any_element(),
        )
    }
}

/// A file or folder mention as an inline token: the text the agent reads
/// (`@src/main.rs`) shown as a labelled chip.
fn mention_token(path: &str, text: &str) -> InlineToken {
    let trimmed = path.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or(trimmed);
    let label = if path.ends_with('/') { format!("{name}/") } else { name.to_owned() };
    InlineToken::new(format!("path:{path}"), text.to_owned()).with_label(label)
}

/// End of the character starting at byte `p`.
fn char_end(s: &str, p: usize) -> usize {
    s[p..].chars().next().map_or(p + 1, |c| p + c.len_utf8())
}

fn access_description(level: Access) -> &'static str {
    match level {
        Access::Readonly => "Readonly — reads files, changes nothing",
        Access::Ask => "Ask first — asks before commands and edits",
        Access::AutoEdit => "Auto-edit — edits freely, asks before commands",
        Access::Full => "Full access — never asks",
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let blocker = self.blocker(cx);
        let token_muted = theme.muted_foreground;
        let this = cx.weak_entity();
        let pill = self.model_picker.render_pill(&self.config, &self.app, window, cx);
        v_flex()
            .w_full()
            .key_context("Composer")
            .capture_action(cx.listener(Self::on_move_up))
            .capture_action(cx.listener(Self::on_move_down))
            .capture_action(cx.listener(Self::on_enter_action))
            .capture_action(cx.listener(Self::on_tab))
            .capture_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_paste_as_text))
            .on_action(cx.listener(Self::on_edit_last_queued))
            .on_action(cx.listener(Self::on_cycle_effort))
            .on_action(cx.listener(Self::on_open_model_picker))
            .on_action(cx.listener(Self::on_send_alternate))
            .children(self.render_completion(cx))
            .child(
                v_flex()
                    .id("composer-box")
                    .w_full()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.input)
                    .drag_over::<ExternalPaths>(|style, _, _, cx| {
                        style.border_color(cx.theme().drag_border).bg(cx.theme().drop_target)
                    })
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                        this.drop_paths(paths.paths().to_vec(), window, cx)
                    }))
                    .bg(theme.popover)
                    .shadow_sm()
                    .children(self.render_attachments(cx))
                    .child(
                        div().px_3().pt_3().child(
                            Textarea::new(&self.input)
                                .appearance(false)
                                .on_paste(move |item, window, cx| {
                                    this.update(cx, |this, cx| this.paste(item, window, cx)).unwrap_or(false)
                                })
                                .token(move |token, _, _| {
                                    let id = token.token().id();
                                    let icon = if id.starts_with("skill:") {
                                        Icon::new(Lucide::Sparkles)
                                    } else if id.ends_with('/') {
                                        Icon::new(IconName::Folder)
                                    } else {
                                        Icon::new(IconName::File)
                                    };
                                    InputToken::new(token).icon(icon.text_color(token_muted))
                                }),
                        ),
                    )
                    .child(
                        h_flex()
                            .px_2()
                            .pt_1()
                            .pb_2()
                            .gap_1()
                            .child(
                                Button::new("attach")
                                    .ghost()
                                    .small()
                                    .icon(Icon::new(Lucide::Paperclip))
                                    .tooltip("Attach files… (or drop or paste them)")
                                    .on_click(cx.listener(|this, _, window, cx| this.attach_files(window, cx))),
                            )
                            .children(self.render_plan_toggle(cx))
                            .child(div().flex_1())
                            .children(self.render_status(blocker.as_deref(), cx))
                            .child(pill)
                            .child(self.render_send(blocker.as_deref(), cx)),
                    ),
            )
            .child(self.render_context_bar(cx))
    }
}
