//! The first-use sheet: dictation without a model offers the download
//! instead of failing.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::progress::Progress as ProgressBar;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_voice::catalog::ModelInfo;
use orbit_voice::store::{Phase, Status};

use super::models::{Models, ModelsEvent, size};

pub fn open(window: &mut Window, cx: &mut App) {
    let Some(voice) = super::voice(cx) else { return };
    let models = voice.models.clone();
    let model = super::selected_model(cx);
    let view = cx.new(|cx| Setup::new(models, model, window, cx));
    window.open_dialog(cx, move |dialog, _, _| dialog.title("Set up dictation").w(px(480.)).child(view.clone()));
}

/// A one-line description of a model: name, languages, sizes.
pub fn describe(model: &ModelInfo) -> String {
    let languages = if model.english_only { "English" } else { "99 languages" };
    format!("Whisper {} · {languages}", model.name)
}

struct Setup {
    models: Entity<Models>,
    model: &'static ModelInfo,
    _subscriptions: Vec<Subscription>,
}

impl Setup {
    fn new(models: Entity<Models>, model: &'static ModelInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&models, |_, _, cx| cx.notify());
        let installed = cx.subscribe_in(&models, window, move |_, _, event: &ModelsEvent, window, cx| {
            let ModelsEvent::Installed(id) = event;
            if *id == model.id {
                window.close_dialog(cx);
                let keys = super::shortcut_label(&super::Dictate, cx).unwrap_or_else(|| "Ctrl+M".into());
                window.push_notification(
                    Notification::success(format!("Dictation is ready. Press {keys} or the microphone button.")),
                    cx,
                );
            }
        });
        Self { models, model, _subscriptions: vec![observe, installed] }
    }
}

impl Render for Setup {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let models = self.models.read(cx);
        let model = self.model;
        let progress = models.progress(model);
        let error = models.error(model).map(str::to_owned);
        let partial = matches!(models.status(model), Status::Partial(_));
        let total = model.download_bytes();
        let body = v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .child("Dictation runs OpenAI’s Whisper model on this computer. Your voice never leaves it and isn’t saved."),
            )
            .child(
                v_flex()
                    .gap_1()
                    .p_3()
                    .rounded(theme.radius)
                    .bg(theme.muted)
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(describe(model)))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(format!(
                        "{} download from Hugging Face, {} on disk. Other models are in Settings › Voice.",
                        size(total),
                        size(model.installed_bytes)
                    )))
                    .when_some(progress.clone(), |el, p| {
                        let (done, all) = p.read();
                        let label = match p.phase() {
                            Phase::Downloading => format!("{} of {}", size(done), size(all)),
                            Phase::Verifying => "Checking what was already downloaded…".to_owned(),
                            Phase::Converting => "Preparing the model…".to_owned(),
                        };
                        el.child(
                            v_flex()
                                .pt_2()
                                .gap_1()
                                .child(ProgressBar::new("voice-setup-progress").value(p.fraction() * 100.0))
                                .child(div().text_xs().text_color(theme.muted_foreground).child(label)),
                        )
                    }),
            )
            .when_some(error, |el, e| el.child(div().text_sm().text_color(theme.danger).child(e)));
        let downloading = progress.is_some();
        let models = self.models.clone();
        let footer = h_flex()
            .w_full()
            .justify_end()
            .gap_2()
            .child(Button::new("voice-setup-later").ghost().label(if downloading { "Hide" } else { "Not now" }).on_click(
                |_, window, cx| window.close_dialog(cx),
            ))
            .map(|el| {
                if downloading {
                    let models = models.clone();
                    el.child(
                        Button::new("voice-setup-cancel")
                            .label("Cancel download")
                            .on_click(move |_, _, cx| models.update(cx, |m, cx| m.cancel(model, cx))),
                    )
                } else {
                    let label = if partial { "Resume download".to_owned() } else { format!("Download {}", size(total)) };
                    el.child(
                        Button::new("voice-setup-download")
                            .primary()
                            .label(label)
                            .on_click(move |_, _, cx| models.update(cx, |m, cx| m.install(model, cx))),
                    )
                }
            });
        v_flex().gap_4().child(body).child(footer)
    }
}
