//! The composer: write, attach, choose how the agent runs, send.
//!
//! The model list and every model option (reasoning effort, fast mode, …)
//! come from the provider's own catalog at runtime; nothing here names a
//! model or an effort level. While a turn runs, Enter queues the message and
//! Ctrl+Enter steers the running turn, and the composer says which will
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

use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::ui;

pub enum ComposerEvent {
    Submit { text: String, attachments: Vec<Attachment>, steer: bool },
    Stop,
    ConfigChanged(ThreadConfig),
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
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(app: Entity<AppState>, config: ThreadConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 14)
                .submit_on_enter(true)
                .placeholder("Describe a change, a fix or a question — / for commands")
        });
        let input_sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift: false, secondary } = event {
                this.submit(*secondary, window, cx);
            }
        });
        // Providers report their catalogs asynchronously; re-render when they do.
        let app_sub = cx.observe(&app, |_, _, cx| cx.notify());
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
            _subscriptions: vec![input_sub, app_sub],
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

    fn submit(&mut self, steer_key: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        if self.attaching > 0 {
            return; // wait for attachments to finish storing
        }
        self.input.update(cx, |input, cx| input.set_value("", window, cx));
        let attachments = std::mem::take(&mut self.attachments);
        cx.emit(ComposerEvent::Submit { text, attachments, steer: self.running && steer_key });
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
            for path in paths {
                let _ = this.update(cx, |this, cx| {
                    this.attaching += 1;
                    cx.notify();
                });
                let read = cx
                    .background_spawn({
                        let path = path.clone();
                        async move { std::fs::read(&path).map(|data| (path, data)) }
                    })
                    .await;
                let Ok((path, data)) = read else {
                    let _ = this.update(cx, |this, cx| this.attaching -= 1);
                    continue;
                };
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let mime = mime_for(&name).to_owned();
                let stored = this.update(cx, |this, cx| {
                    this.app.update(cx, |app, cx| app.call(Command::PutBlob { name, mime, data }, cx))
                });
                let Ok(stored) = stored else { return };
                let result = stored.await;
                let _ = this.update(cx, |this, cx| {
                    this.attaching -= 1;
                    if let Ok(Reply::Blob(attachment)) = result {
                        this.attachments.push(attachment);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
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
            .when_some(self.context.clone(), |el, label| {
                el.child(
                    h_flex()
                        .gap_1()
                        .px_1p5()
                        .child(Icon::new(IconName::Folder).xsmall())
                        .child(label),
                )
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
                .tooltip(if self.running { "Queue (Enter) · Steer the running turn (Ctrl+Enter)" } else { "Send (Enter)" })
                .disabled(!has_text || self.attaching > 0)
                .on_click(cx.listener(|this, _, window, cx| this.submit(false, window, cx)))
        };
        v_flex()
            .w_full()
            .child(
                v_flex()
                    .w_full()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.input)
                    .bg(theme.popover)
                    .shadow_sm()
                    .children(self.render_attachments(cx))
                    .child(div().px_3().pt_3().child(Textarea::new(&self.input).appearance(false)))
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
                                        .child(if has_text { "Enter queues · Ctrl+Enter steers" } else { "Working…" }),
                                )
                            })
                            .child(self.render_model_pill(cx))
                            .child(send),
                    ),
            )
            .child(self.render_context_bar(cx))
    }
}
