//! The composer: write, attach, choose how the agent runs, send.
//!
//! The model list and every model option (reasoning effort, fast mode, …)
//! come from the provider's own catalog at runtime; nothing here names a
//! model or an effort level. The send key (Enter or Ctrl+Enter) and what
//! sending does while a turn runs (queue or steer; the alternate key does the
//! other) come from Settings › Composer, and the composer says which will
//! happen before you press anything.

use std::rc::Rc;

use gpui_kit::component::attachment::{Attachment as AttachmentChip, AttachmentContent, AttachmentTitle};
use gpui_kit::component::button::{Button, ButtonRounded, ButtonVariants as _, Toggle};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::actions::SendAlternate;
use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::{settings, ui};

pub enum ComposerEvent {
    Submit { text: String, attachments: Vec<Attachment>, steer: bool },
    Stop,
    ConfigChanged(ThreadConfig),
    /// Home only: run the new thread in its own worktree or not.
    WorktreeChosen(bool),
}

pub struct Composer {
    app: Entity<AppState>,
    input: Entity<TextareaState>,
    config: ThreadConfig,
    attachments: Vec<Attachment>,
    attaching: usize,
    running: bool,
    /// Live usage of the thread, for the meter. `None` on the home screen.
    usage: Option<Usage>,
    /// Where the agent works ("orbit", "Current checkout"), shown under the box.
    context: Option<String>,
    branch: Option<String>,
    /// Home only: whether the next thread gets its own worktree. `None`
    /// hides the choice (an existing thread's workspace is fixed).
    worktree_choice: Option<bool>,
    /// Slash-command completion: the selected suggestion and whether the
    /// user dismissed the list for the current word.
    slash_ix: usize,
    slash_dismissed: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(app: Entity<AppState>, config: ThreadConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let enter_sends = settings::get(cx).composer.submit == SubmitKey::Enter;
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 14)
                .submit_on_enter(enter_sends)
                .placeholder("Describe a change, a fix or a question — / for commands")
        });
        let input_sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match *event {
            InputEvent::PressEnter { shift, secondary } => {
                let send = match (settings::get(cx).composer.submit, secondary, shift) {
                    (SubmitKey::Enter, false, false) | (SubmitKey::ModEnter, true, false) => Some(false),
                    (SubmitKey::Enter, true, _) => Some(true),
                    _ => None, // a new line
                };
                if let Some(alternate) = send {
                    if !this.slash_matches(cx).is_empty() && !alternate {
                        this.complete_slash(window, cx);
                    } else {
                        this.submit(alternate, window, cx);
                    }
                }
            }
            InputEvent::Change => {
                this.slash_ix = 0;
                if !this.input.read(cx).value().starts_with('/') {
                    this.slash_dismissed = false;
                }
                cx.notify();
            }
            _ => {}
        });
        // Providers report their catalogs asynchronously; re-render when they do.
        let app_sub = cx.observe(&app, |_, _, cx| cx.notify());
        let settings_sub = cx.observe_global::<settings::SettingsModel>(|this, cx| {
            let enter_sends = settings::get(cx).composer.submit == SubmitKey::Enter;
            this.input.update(cx, |input, cx| input.set_submit_on_enter(enter_sends, cx));
            cx.notify();
        });
        Self {
            app,
            input,
            config,
            attachments: Vec::new(),
            attaching: 0,
            running: false,
            usage: None,
            context: None,
            branch: None,
            worktree_choice: None,
            slash_ix: 0,
            slash_dismissed: false,
            _subscriptions: vec![input_sub, app_sub, settings_sub],
        }
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

    pub fn set_usage(&mut self, usage: Option<Usage>, cx: &mut Context<Self>) {
        if self.usage != usage {
            self.usage = usage;
            cx.notify();
        }
    }

    /// Moves keyboard focus into the composer's text field.
    pub fn focus(composer: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let input = composer.read(cx).input.clone();
        input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Replaces the composer's text (for example to restore a failed send).
    pub fn set_text(composer: &Entity<Self>, text: &str, window: &mut Window, cx: &mut App) {
        let input = composer.read(cx).input.clone();
        input.update(cx, |input, cx| input.set_value(text, window, cx));
    }

    /// Adds `markdown` (a quotation the reader is replying to) to the end of
    /// the draft, keeps what was already written, and puts the caret after
    /// it with focus in the box. Used by the timeline's Reply and Quote.
    pub fn append_to_draft(composer: &Entity<Self>, markdown: &str, window: &mut Window, cx: &mut App) {
        let input = composer.read(cx).input.clone();
        input.update(cx, |input, cx| {
            let draft = input.value();
            let text = match draft.trim_end() {
                "" => format!("{markdown}\n\n"),
                draft => format!("{draft}\n\n{markdown}\n\n"),
            };
            input.set_value("", window, cx);
            input.insert(text, window, cx);
            input.focus(window, cx);
        });
    }

    /// The provider's slash commands matching the word being typed, while
    /// the text is a lone `/word`.
    fn slash_matches(&self, cx: &App) -> Vec<SlashCommand> {
        if self.slash_dismissed {
            return Vec::new();
        }
        let text = self.input.read(cx).value();
        let Some(word) = text.strip_prefix('/') else { return Vec::new() };
        if word.contains(char::is_whitespace) {
            return Vec::new();
        }
        let word = word.to_lowercase();
        let Some(provider) = self.app.read(cx).state().provider(self.config.provider) else { return Vec::new() };
        let mut hits: Vec<SlashCommand> = provider
            .slash_commands
            .iter()
            // Names starting with "__" are the runtime's internal commands.
            .filter(|c| !c.name.starts_with("__") && c.name.to_lowercase().contains(&word))
            .cloned()
            .collect();
        // Prefix matches first, then alphabetical.
        hits.sort_by_key(|c| (!c.name.to_lowercase().starts_with(&word), c.name.clone()));
        hits.truncate(8);
        hits
    }

    fn complete_slash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let matches = self.slash_matches(cx);
        let Some(command) = matches.get(self.slash_ix.min(matches.len().saturating_sub(1))) else { return };
        let text = format!("/{} ", command.name);
        self.input.update(cx, |input, cx| input.set_value(text, window, cx));
        self.slash_dismissed = true;
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.slash_matches(cx).len();
        if count == 0 {
            return;
        }
        match event.keystroke.key.as_str() {
            "down" => self.slash_ix = (self.slash_ix + 1) % count,
            "up" => self.slash_ix = (self.slash_ix + count - 1) % count,
            "tab" => self.complete_slash(window, cx),
            "escape" => self.slash_dismissed = true,
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn render_slash_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let matches = self.slash_matches(cx);
        if matches.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let selected = self.slash_ix.min(matches.len() - 1);
        Some(
            v_flex()
                .mb_1()
                .p_1()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_md()
                .children(matches.into_iter().enumerate().map(|(ix, command)| {
                    let name = command.name.clone();
                    h_flex()
                        .id(SharedString::from(format!("slash-{name}")))
                        .px_2()
                        .py_1()
                        .gap_2()
                        .rounded(theme.radius)
                        .text_sm()
                        .cursor_default()
                        .when(ix == selected, |el| el.bg(theme.accent))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.slash_ix = ix;
                            this.complete_slash(window, cx);
                        }))
                        .child(div().font_family(theme.mono_font_family.clone()).child(format!("/{name}")))
                        .when(!command.argument_hint.is_empty(), |el| {
                            el.child(div().text_xs().text_color(theme.muted_foreground).child(command.argument_hint.clone()))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(command.description.clone()),
                        )
                }))
                .child(
                    div()
                        .px_2()
                        .pt_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("↑↓ to choose · Tab or Enter to complete · Esc to dismiss"),
                ),
        )
    }

    /// Sends the message. While a turn runs it is queued or steers the turn
    /// as the follow-up setting says; `alternate` does the opposite.
    fn submit(&mut self, alternate: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        if self.attaching > 0 {
            return; // wait for attachments to finish storing
        }
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        let attachments = std::mem::take(&mut self.attachments);
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

    fn attach_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let _ = this.update(cx, |this, cx| this.attach_paths(paths, cx));
        })
        .detach();
    }

    /// Attaches files from disk (picker, drop, pasted files). Folders are
    /// skipped: agents read the project directly.
    pub fn attach_paths(&mut self, paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        for path in paths {
            if !path.is_file() {
                continue;
            }
            self.attaching += 1;
            cx.notify();
            let read = cx.background_spawn({
                let path = path.clone();
                async move { std::fs::read(&path) }
            });
            cx.spawn(async move |this, cx| {
                let data = read.await;
                let _ = this.update(cx, |this, cx| {
                    this.attaching -= 1;
                    if let Ok(data) = data {
                        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let mime = mime_for(&name).to_owned();
                        this.attach_bytes(name, mime, data, cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    /// Stores bytes in the blob store (once per content) and attaches them.
    fn attach_bytes(&mut self, name: String, mime: String, data: Vec<u8>, cx: &mut Context<Self>) {
        self.attaching += 1;
        let task = self.app.update(cx, |app, cx| app.call(Command::PutBlob { name, mime, data }, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.attaching -= 1;
                if let Ok(Reply::Blob(attachment)) = result
                    && !this.attachments.iter().any(|a| a.hash == attachment.hash)
                {
                    this.attachments.push(attachment);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Images and files on the clipboard become attachments; text pastes
    /// normally. Returns whether the paste was handled here.
    fn paste(&mut self, item: &ClipboardItem, cx: &mut Context<Self>) -> bool {
        let mut handled = false;
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    let (ext, mime) = match image.format {
                        ImageFormat::Png => ("png", "image/png"),
                        ImageFormat::Jpeg => ("jpg", "image/jpeg"),
                        ImageFormat::Webp => ("webp", "image/webp"),
                        ImageFormat::Gif => ("gif", "image/gif"),
                        _ => ("png", "image/png"),
                    };
                    let n = self.attachments.len() + self.attaching + 1;
                    self.attach_bytes(format!("pasted-image-{n}.{ext}"), mime.into(), image.bytes.clone(), cx);
                    handled = true;
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    self.attach_paths(paths.paths().to_vec(), cx);
                    handled = true;
                }
                ClipboardEntry::String(_) => {}
            }
        }
        handled
    }

    /// The current model and its non-default options, read from the
    /// provider's catalog: ("Opus 5.5", "High").
    fn model_summary(&self, cx: &App) -> (String, Option<String>) {
        let state = self.app.read(cx).state();
        let name = ui::model_label(&self.config, &state.providers);
        let Some(model) = selected_model(&self.config, &state.providers) else { return (name, None) };
        let mut extras = Vec::new();
        for option in &model.options {
            match (&option.kind, chosen(&self.config, option)) {
                (OptionKind::Select { choices }, OptionSetting::Choice(id)) if id != "default" => {
                    if let Some(c) = choices.iter().find(|c| c.id == id) {
                        extras.push(c.label.clone());
                    }
                }
                (OptionKind::Toggle { default }, OptionSetting::Toggle(on)) if on != *default && on => {
                    extras.push(option.label.clone());
                }
                _ => {}
            }
        }
        (name, (!extras.is_empty()).then(|| extras.join(" · ")))
    }

    /// One pill for everything about the model: which agent, which model,
    /// how hard it thinks. Opens a single popover with all of it.
    fn render_model_pill(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (name, extras) = self.model_summary(cx);
        let providers: Rc<Vec<ProviderInfo>> = Rc::new(self.app.read(cx).state().providers.clone());
        let config = self.config.clone();
        let this = cx.weak_entity();
        let icon = ui::provider_icon(self.config.provider, cx);
        Popover::new("model-picker")
            .anchor(Anchor::BottomRight)
            .trigger(
                Button::new("model-pill")
                    .ghost()
                    .small()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(icon.xsmall())
                            .child(div().font_weight(FontWeight::MEDIUM).child(name))
                            .when_some(extras, |el, extras| {
                                el.child(div().text_color(theme.muted_foreground).child(extras))
                            }),
                    ),
            )
            .content(move |_, _, cx| render_model_menu(&providers, &config, &this, cx))
    }

    fn render_plan_toggle(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let supports_plan = self
            .app
            .read(cx)
            .state()
            .provider(self.config.provider)
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
                .tooltip("Plan first: the agent proposes a plan and waits for your go-ahead")
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
                (None, Some(label)) => el.child(
                    h_flex().gap_1().px_1p5().child(Icon::new(IconName::Folder).xsmall()).child(label),
                ),
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
        for (ix, attachment) in self.attachments.iter().enumerate() {
            let hash = attachment.hash.clone();
            row = row.child(
                AttachmentChip::new()
                    .id(SharedString::from(format!("att-{hash}-{ix}")))
                    .small()
                    .content(AttachmentContent::new().title(AttachmentTitle::new(attachment.name.clone())))
                    .on_remove(cx.listener(move |this, _, _, cx| {
                        this.attachments.retain(|a| a.hash != hash);
                        cx.notify();
                    })),
            );
        }
        if self.attaching > 0 {
            row = row.child(
                div().text_xs().text_color(cx.theme().muted_foreground).child(format!("Storing {}…", self.attaching)),
            );
        }
        Some(row)
    }
}

fn selected_model<'a>(config: &ThreadConfig, providers: &'a [ProviderInfo]) -> Option<&'a ModelInfo> {
    let provider = providers.iter().find(|p| p.kind == config.provider)?;
    provider
        .models
        .iter()
        .find(|m| m.slug == config.model)
        .or_else(|| if config.model.is_empty() { provider.models.iter().find(|m| m.is_default) } else { None })
}

fn chosen(config: &ThreadConfig, option: &OptionDescriptor) -> OptionSetting {
    config.options.iter().find(|v| v.id == option.id).map(|v| v.value.clone()).unwrap_or_else(|| option.default_setting())
}

/// The model popover: agents and their models, then the selected model's
/// options as segmented choices and switches.
fn render_model_menu(
    providers: &[ProviderInfo],
    config: &ThreadConfig,
    this: &WeakEntity<Composer>,
    cx: &mut App,
) -> Div {
    let theme = cx.theme().clone();
    let mut list = v_flex().gap_0p5();
    for provider in providers {
        list = list.child(
            h_flex()
                .px_2()
                .pt_2()
                .pb_1()
                .gap_1p5()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted_foreground)
                .child(ui::provider_icon(provider.kind, cx).xsmall())
                .child(provider.kind.label())
                .when(!provider.installed, |el| el.child(div().flex_1().text_right().child("Not installed"))),
        );
        if !provider.installed {
            list = list.child(
                div()
                    .px_2()
                    .pb_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child(provider.message.clone().unwrap_or_else(|| "Install its command-line tool to use it.".into())),
            );
        }
        for model in &provider.models {
            let selected = config.provider == provider.kind
                && (config.model == model.slug || (config.model.is_empty() && model.is_default));
            let (kind, slug, this) = (provider.kind, model.slug.clone(), this.clone());
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("model-{}-{}", provider.kind.label(), model.slug)))
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .rounded(theme.radius)
                    .cursor_default()
                    .when(selected, |el| el.bg(theme.accent))
                    .when(!selected, |el| el.hover(|s| s.bg(theme.muted)))
                    .on_click(move |_, _, cx| {
                        let slug = slug.clone();
                        let _ = this.update(cx, |this, cx| {
                            this.update_config(
                                |c| {
                                    if c.provider != kind {
                                        c.options.clear();
                                    }
                                    c.provider = kind;
                                    c.model = slug;
                                },
                                cx,
                            )
                        });
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().child(model.name.clone()))
                            .when_some(model.description.clone(), |el, d| {
                                el.child(div().text_xs().text_color(theme.muted_foreground).truncate().child(d))
                            }),
                    )
                    .when(selected, |el| el.child(Icon::new(IconName::Check).xsmall().text_color(theme.primary))),
            );
        }
    }
    // Models scroll; the selected model's options stay in view below them.
    let models = div()
        .id("model-list")
        .max_h(rems(19.))
        .overflow_y_scroll()
        .child(list);
    let mut list = v_flex().w(rems(22.)).child(models);
    if let Some(model) = selected_model(config, providers)
        && !model.options.is_empty()
    {
        list = list.child(div().my_1().h_px().bg(theme.border));
        for option in &model.options {
            let current = chosen(config, option);
            let option_id = option.id.clone();
            match &option.kind {
                OptionKind::Select { choices } => {
                    let mut group = h_flex().gap_0p5().p_0p5().rounded(theme.radius).bg(theme.muted);
                    for choice in choices {
                        let selected = current == OptionSetting::Choice(choice.id.clone());
                        let (this, option_id, id) = (this.clone(), option_id.clone(), choice.id.clone());
                        group = group.child(
                            div()
                                .id(SharedString::from(format!("choice-{}-{}", option.id, choice.id)))
                                .flex_1()
                                .py_1()
                                .rounded(theme.radius)
                                .text_center()
                                .text_xs()
                                .cursor_default()
                                .when(selected, |el| el.bg(theme.background).shadow_xs().font_weight(FontWeight::MEDIUM))
                                .when(!selected, |el| el.text_color(theme.muted_foreground).hover(|s| s.text_color(theme.foreground)))
                                .when_some(choice.description.clone(), |el, d| {
                                    el.tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(d.clone()).build(window, cx))
                                })
                                .on_click(move |_, _, cx| {
                                    let (option_id, id) = (option_id.clone(), id.clone());
                                    let _ = this.update(cx, |this, cx| {
                                        this.update_config(|c| set_option(c, option_id, OptionSetting::Choice(id)), cx)
                                    });
                                })
                                .child(choice.label.clone()),
                        );
                    }
                    list = list.child(
                        v_flex()
                            .px_2()
                            .py_1()
                            .gap_1()
                            .child(div().text_xs().text_color(theme.muted_foreground).child(option.label.clone()))
                            .child(group),
                    );
                }
                OptionKind::Toggle { .. } => {
                    let on = current == OptionSetting::Toggle(true);
                    let this = this.clone();
                    list = list.child(
                        h_flex()
                            .px_2()
                            .py_1p5()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .child(div().text_sm().child(option.label.clone()))
                                    .when_some(option.description.clone(), |el, d| {
                                        el.child(div().text_xs().text_color(theme.muted_foreground).child(d))
                                    }),
                            )
                            .child(Switch::new(SharedString::from(format!("switch-{}", option.id))).small().checked(on).on_click(
                                move |checked: &bool, _, cx| {
                                    let (option_id, checked) = (option_id.clone(), *checked);
                                    let _ = this.update(cx, |this, cx| {
                                        this.update_config(|c| set_option(c, option_id, OptionSetting::Toggle(checked)), cx)
                                    });
                                },
                            )),
                    );
                }
            }
        }
    }
    list
}

fn set_option(config: &mut ThreadConfig, id: String, value: OptionSetting) {
    match config.options.iter_mut().find(|o| o.id == id) {
        Some(slot) => slot.value = value,
        None => config.options.push(OptionValue { id, value }),
    }
}

fn access_description(level: Access) -> &'static str {
    match level {
        Access::Readonly => "Readonly — reads files, changes nothing",
        Access::Ask => "Ask first — asks before commands and edits",
        Access::AutoEdit => "Auto-edit — edits freely, asks before commands",
        Access::Full => "Full access — never asks",
    }
}

fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "md" | "txt" | "rs" | "ts" | "tsx" | "js" | "json" | "toml" | "yaml" | "yml" | "py" => "text/plain",
        _ => "application/octet-stream",
    }
}

impl Render for Composer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let has_text = !self.input.read(cx).value().trim().is_empty() || !self.attachments.is_empty();
        let (send_key, alternate_key) = settings::send_keys(cx);
        let (primary, other) = match settings::get(cx).composer.follow_up {
            FollowUp::Queue => ("queues", "steers the running turn"),
            FollowUp::Steer => ("steers the running turn", "queues"),
        };
        let hint = self.running.then(|| format!("{send_key} {primary} · {alternate_key} {other}"));
        let send_tooltip: SharedString = if self.running {
            let (primary, other) = match settings::get(cx).composer.follow_up {
                FollowUp::Queue => ("Queue", "Steer"),
                FollowUp::Steer => ("Steer", "Queue"),
            };
            format!("{primary} ({send_key}) · {other} ({alternate_key})").into()
        } else {
            format!("Send ({send_key})").into()
        };
        let send = if self.running && !has_text {
            Button::new("stop")
                .icon(Icon::new(IconName::Square))
                .rounded(ButtonRounded::Size(px(999.)))
                .tooltip("Stop (Ctrl+.)")
                .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::Stop)))
        } else {
            Button::new("send")
                .primary()
                .icon(Icon::new(IconName::ArrowUp))
                .rounded(ButtonRounded::Size(px(999.)))
                .tooltip(send_tooltip)
                .disabled(!has_text || self.attaching > 0)
                .on_click(cx.listener(|this, _, window, cx| this.submit(false, window, cx)))
        };
        v_flex()
            .w_full()
            .key_context("Composer")
            .on_action(cx.listener(|this, _: &SendAlternate, window, cx| this.submit(true, window, cx)))
            .capture_key_down(cx.listener(Self::on_key))
            .children(self.render_slash_menu(cx))
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
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.attach_paths(paths.paths().to_vec(), cx)))
                    .bg(theme.popover)
                    .shadow_sm()
                    .children(self.render_attachments(cx))
                    .child({
                        let this = cx.weak_entity();
                        div().px_3().pt_3().child(Textarea::new(&self.input).appearance(false).on_paste(move |item, _, cx| {
                            this.update(cx, |this, cx| this.paste(item, cx)).unwrap_or(false)
                        }))
                    })
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
                                    .tooltip("Attach files…")
                                    .on_click(cx.listener(|this, _, window, cx| this.attach_files(window, cx))),
                            )
                            .children(self.render_plan_toggle(cx))
                            .child(div().flex_1())
                            .when(self.running, |el| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .mr_1()
                                        .child(if has_text { hint.clone().unwrap_or_default() } else { "Working…".to_owned() }),
                                )
                            })
                            .child(self.render_model_pill(cx))
                            .child(send),
                    ),
            )
            .child(self.render_context_bar(cx))
    }
}
