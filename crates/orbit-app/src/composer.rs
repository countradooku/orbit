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
use gpui_kit::component::button::{Button, ButtonVariants as _, Toggle};
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
                .placeholder("Ask for a change, a fix, or an explanation…")
        });
        let input_sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift, secondary } = *event {
                match (settings::get(cx).composer.submit, secondary, shift) {
                    (SubmitKey::Enter, false, false) | (SubmitKey::ModEnter, true, false) => this.submit(false, window, cx),
                    (SubmitKey::Enter, true, _) => this.submit(true, window, cx),
                    _ => {} // a new line
                }
            }
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

    fn render_model_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.app.read(cx).state();
        let providers: Rc<Vec<ProviderInfo>> = Rc::new(state.providers.clone());
        let label = ui::model_label(&self.config, &providers);
        let current = self.config.clone();
        let this = cx.weak_entity();
        Button::new("model")
            .ghost()
            .small()
            .icon(Icon::new(Lucide::Sparkles))
            .label(label)
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.min_w(px(260.));
                for provider in providers.iter() {
                    menu = menu.label(provider.kind.label());
                    if !provider.installed {
                        let note = provider.message.clone().unwrap_or_else(|| "Not installed".into());
                        menu = menu.item(PopupMenuItem::new(note).disabled(true));
                        continue;
                    }
                    for model in &provider.models {
                        let selected = current.provider == provider.kind
                            && (current.model == model.slug || (current.model.is_empty() && model.is_default));
                        let (kind, slug) = (provider.kind, model.slug.clone());
                        let this = this.clone();
                        menu = menu.item(PopupMenuItem::new(model.name.clone()).checked(selected).on_click(
                            move |_, _, cx| {
                                let slug = slug.clone();
                                let _ = this.update(cx, |this, cx| {
                                    this.update_config(
                                        |c| {
                                            c.provider = kind;
                                            c.model = slug;
                                        },
                                        cx,
                                    )
                                });
                            },
                        ));
                    }
                    menu = menu.separator();
                }
                menu
            })
    }

    /// One control per option the selected model reports.
    fn render_model_options(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let state = self.app.read(cx).state();
        let Some(provider) = state.provider(self.config.provider) else { return Vec::new() };
        let model = provider
            .models
            .iter()
            .find(|m| m.slug == self.config.model)
            .or_else(|| provider.models.iter().find(|m| m.is_default));
        let Some(model) = model else { return Vec::new() };
        let mut out = Vec::new();
        for option in &model.options {
            let chosen = self
                .config
                .options
                .iter()
                .find(|v| v.id == option.id)
                .map(|v| v.value.clone())
                .unwrap_or_else(|| option.default_setting());
            let this = cx.weak_entity();
            let option_id = option.id.clone();
            match (&option.kind, chosen) {
                (OptionKind::Select { choices }, OptionSetting::Choice(chosen)) => {
                    // "Default" alone says nothing; name the option instead.
                    let label = match choices.iter().find(|c| c.id == chosen) {
                        Some(c) if c.id == "default" => option.label.clone(),
                        Some(c) => c.label.clone(),
                        None => chosen.clone(),
                    };
                    let choices = Rc::new(choices.clone());
                    out.push(
                        Button::new(SharedString::from(format!("opt-{option_id}")))
                            .ghost()
                            .small()
                            .icon(Icon::new(Lucide::Brain))
                            .label(label)
                            .tooltip(option.label.clone())
                            .dropdown_caret(true)
                            .dropdown_menu(move |mut menu, _, _| {
                                for choice in choices.iter() {
                                    let (this, option_id, id) = (this.clone(), option_id.clone(), choice.id.clone());
                                    let mut item = PopupMenuItem::new(choice.label.clone()).checked(choice.id == chosen);
                                    item = item.on_click(move |_, _, cx| {
                                        let (option_id, id) = (option_id.clone(), id.clone());
                                        let _ = this.update(cx, |this, cx| {
                                            this.update_config(|c| set_option(c, option_id, OptionSetting::Choice(id)), cx)
                                        });
                                    });
                                    menu = menu.item(item);
                                }
                                menu
                            })
                            .into_any_element(),
                    );
                }
                (OptionKind::Toggle { .. }, OptionSetting::Toggle(on)) => {
                    out.push(
                        Toggle::new(SharedString::from(format!("opt-{option_id}")))
                            .small()
                            .label(option.label.clone())
                            .checked(on)
                            .on_click(move |checked: &bool, _, cx| {
                                let (option_id, checked) = (option_id.clone(), *checked);
                                let _ = this.update(cx, |this, cx| {
                                    this.update_config(|c| set_option(c, option_id, OptionSetting::Toggle(checked)), cx)
                                });
                            })
                            .into_any_element(),
                    );
                }
                _ => {}
            }
        }
        out
    }

    fn render_mode_and_access(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme();
        let supports_plan = self
            .app
            .read(cx)
            .state()
            .provider(self.config.provider)
            .is_none_or(|p| p.modes.is_empty() || p.modes.iter().any(|m| m.mode == Mode::Plan));
        let mut out = Vec::new();
        if supports_plan {
            let this = cx.weak_entity();
            out.push(
                Toggle::new("plan-mode")
                    .small()
                    .icon(Icon::new(Lucide::ListTodo))
                    .label("Plan")
                    .tooltip("Plan first: the agent proposes a plan and waits for your go-ahead")
                    .checked(self.config.mode == Mode::Plan)
                    .on_click(move |checked: &bool, _, cx| {
                        let checked = *checked;
                        let _ = this.update(cx, |this, cx| {
                            this.update_config(|c| c.mode = if checked { Mode::Plan } else { Mode::Build }, cx)
                        });
                    })
                    .into_any_element(),
            );
        }
        let access = self.config.access;
        let this = cx.weak_entity();
        let warn = access == Access::Full;
        out.push(
            Button::new("access")
                .ghost()
                .small()
                .icon(Icon::new(if warn { Lucide::ShieldAlert } else { Lucide::ShieldCheck }))
                .label(access.label())
                .when(warn, |b| b.text_color(theme.warning))
                .dropdown_caret(true)
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
                })
                .into_any_element(),
        );
        out
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
                .icon(Icon::new(Lucide::CircleStop))
                .tooltip("Stop (Ctrl+.)")
                .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::Stop)))
        } else {
            Button::new("send")
                .primary()
                .icon(Icon::new(IconName::ArrowUp))
                .tooltip(send_tooltip)
                .disabled(!has_text || self.attaching > 0)
                .on_click(cx.listener(|this, _, window, cx| this.submit(false, window, cx)))
        };
        v_flex()
            .w_full()
            .key_context("Composer")
            .on_action(cx.listener(|this, _: &SendAlternate, window, cx| this.submit(true, window, cx)))
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.input)
            .bg(theme.popover)
            .shadow_sm()
            .children(self.render_attachments(cx))
            .child(
                div().px_3().pt_2p5().child(
                    Textarea::new(&self.input).appearance(false).on_paste(|_, _, _| false),
                ),
            )
            .child(
                h_flex()
                    .px_2()
                    .pt_1()
                    .pb_2()
                    .gap_1()
                    .child(
                        // Controls keep their size and wrap to a second line
                        // in a narrow window rather than overlapping.
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .flex_wrap()
                            .gap_0p5()
                            .child(div().flex_none().child(self.render_model_picker(cx)))
                            .children(self.render_model_options(cx).into_iter().map(|el| div().flex_none().child(el)))
                            .children(self.render_mode_and_access(cx).into_iter().map(|el| div().flex_none().child(el)))
                            .child(
                                div().flex_none().child(
                                    Button::new("attach")
                                        .ghost()
                                        .small()
                                        .icon(Icon::new(Lucide::Paperclip))
                                        .tooltip("Attach files…")
                                        .on_click(cx.listener(|this, _, window, cx| this.attach_files(window, cx))),
                                ),
                            ),
                    )
                    .when_some(self.usage, |el, usage| el.child(div().flex_none().child(ui::ContextMeter::new("composer-meter", usage))))
                    .child(div().flex_none().child(send)),
            )
            .when_some(hint, |el, hint| {
                el.child(div().px_3().pb_2().text_xs().text_color(theme.muted_foreground).child(hint))
            })
    }
}
