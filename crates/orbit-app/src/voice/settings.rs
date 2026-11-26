//! Settings › Voice: the model, the language, the vocabulary, the
//! microphone and how the shortcut behaves.

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::progress::Progress as ProgressBar;
use gpui_kit::component::setting::{RenderOptions, SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_kit::component::speech::{SpeechState, SpeechWaveform};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_voice::catalog::{MODELS, ModelInfo};
use orbit_voice::control::Mode;
use orbit_voice::store::{Phase, Status};

use super::mic::{self, HAS_PRIVACY_SETTINGS, Mic};
use super::models::size;
use super::recognizer::LevelOnly;
use super::{VoicePrefs, update_prefs};

/// The page's title.
pub const PAGE_TITLE: &str = "Voice";
/// Where Settings › Voice is among the pages.
pub const PAGE: usize = 3;

#[derive(Default)]
struct Request(bool);

impl Global for Request {}

/// Opens Settings on this page.
pub fn open(window: &mut Window, cx: &mut App) {
    cx.set_global(Request(true));
    window.dispatch_action(Box::new(crate::actions::OpenSettings), cx);
}

/// Whether Settings was asked to open here (once).
pub fn take_request(cx: &mut App) -> bool {
    std::mem::take(&mut cx.default_global::<Request>().0)
}

fn prefs(cx: &App) -> &VoicePrefs {
    &crate::prefs::get(cx).voice
}

pub fn page(cx: &App) -> SettingPage {
    let model = super::selected_model(cx);
    SettingPage::new(PAGE_TITLE).icon(Icon::new(IconName::Mic)).groups([
        SettingGroup::new()
            .title("Model")
            .description("Whisper runs on this computer. A model downloads from Hugging Face only when you choose it.")
            .items([
                SettingItem::render(|_, window, cx| models_list(window, cx)).keywords(["whisper", "download", "model", "speech"]),
                SettingItem::new("Language", language_field())
                    .description(if model.english_only {
                        "English-only models always transcribe English. Pick a multilingual model for other languages."
                    } else {
                        "Detect it from the first words, or always use one."
                    })
                    .disabled(model.english_only)
                    .keywords(["language", "locale"]),
                SettingItem::new(
                    "Translate to English",
                    switch(|p| p.translate, |p, v| p.translate = v, false),
                )
                .description("Speak any language; English is inserted.")
                .disabled(model.english_only),
            ]),
        SettingGroup::new().title("Dictation").items([
            SettingItem::new("Shortcut", bound_key(Box::new(super::Dictate)))
                .description("In the composer, the Home prompt, search and settings fields. Rebind voice::Dictate in keybindings.json.")
                .keywords(["shortcut", "hotkey", "push to talk"]),
            SettingItem::new("Steer an agent", bound_key(Box::new(super::SteerByVoice))).description(
                "Say the thread’s name, then the instruction: “api, run the tests”. It is sent without opening the thread.",
            ),
            SettingItem::new("Shortcut mode", mode_field())
                .description("Auto: a tap starts and stops, a hold records until you let go. The mic button always toggles.")
                .keywords(["push to talk", "hold", "toggle"]),
            SettingItem::new("Stop after silence", silence_field())
                .description("In auto and toggle modes, stop once nothing has been said for this long.")
                .keywords(["silence", "timeout", "auto stop"]),
            SettingItem::new("Live preview", switch(|p| p.live_preview, |p, v| p.live_preview = v, true))
                .description("Show the words as you speak. The final text is transcribed again from the whole recording."),
        ]),
        SettingGroup::new().title("Text").items([
            SettingItem::new("Vocabulary", vocabulary_field())
                .layout(Axis::Vertical)
                .description(
                    "Names and terms to spell your way, one per line or comma. “soccer do => sockudo” also fixes what Whisper hears.",
                )
                .keywords(["dictionary", "words", "spelling", "prompt"]),
            SettingItem::new("Add project and file names", switch(|p| p.project_terms, |p, v| p.project_terms = v, true))
                .description("The project’s name and its distinctive file names join the vocabulary."),
            SettingItem::new("Remove filler words", switch(|p| p.remove_fillers, |p, v| p.remove_fillers = v, true))
                .description("“Um”, “uh”, “erm” and the like."),
            SettingItem::new("Fix punctuation and capitals", switch(|p| p.tidy, |p, v| p.tidy = v, true)),
        ]),
        SettingGroup::new().title("Microphone").items([
            SettingItem::new("Input", microphone_field(cx)).keywords(["microphone", "input", "device", "headset"]),
            SettingItem::render(|_, window, cx| mic_test(window, cx)).keywords(["test", "level", "microphone"]),
        ]),
        SettingGroup::new().title("Performance").items([
            SettingItem::new("Use the GPU", switch(|p| p.gpu, |p, v| p.gpu = v, true))
                .description(if orbit_voice::whisper::HAS_GPU_BACKEND {
                    "Metal on this Mac. Off: the CPU only."
                } else {
                    "This build runs Whisper on the CPU. GPU builds use Metal on macOS, or CUDA with the cuda feature."
                })
                .disabled(!orbit_voice::whisper::HAS_GPU_BACKEND),
            SettingItem::new("Unload the model after", unload_field())
                .description("Idle time before the model leaves memory. It loads again in a moment on the next dictation."),
        ]),
        SettingGroup::new().title("Privacy").item(
            SettingItem::new("Audio", SettingField::render(|_, _, cx| muted("Stays on this computer", cx)))
                .description(
                    "Audio is held in memory while you dictate and discarded afterwards. It is never written to disk or sent anywhere.",
                )
                .keywords(["privacy", "offline", "local", "cloud"]),
        ),
    ])
}

fn muted(text: &str, cx: &App) -> Div {
    div().text_sm().text_color(cx.theme().muted_foreground).child(text.to_owned())
}

fn switch(
    value: impl Fn(&VoicePrefs) -> bool + 'static,
    set: impl Fn(&mut VoicePrefs, bool) + 'static,
    default: bool,
) -> SettingField<bool> {
    SettingField::switch(move |cx| value(prefs(cx)), move |v, cx| update_prefs(cx, |p| set(p, v))).default_value(default)
}

/// A dropdown over values stored as strings.
fn dropdown(
    options: Vec<(SharedString, SharedString)>,
    value: impl Fn(&VoicePrefs) -> String + 'static,
    set: impl Fn(&mut VoicePrefs, String) + 'static,
    default: &str,
    scrollable: bool,
) -> SettingField<SharedString> {
    let read = move |cx: &App| SharedString::from(value(prefs(cx)));
    let write = move |v: SharedString, cx: &mut App| update_prefs(cx, |p| set(p, v.to_string()));
    let field = if scrollable {
        SettingField::scrollable_dropdown(options, read, write)
    } else {
        SettingField::dropdown(options, read, write)
    };
    field.default_value(SharedString::from(default.to_owned()))
}

fn language_field() -> SettingField<SharedString> {
    let mut options = vec![("auto".into(), "Detect automatically".into())];
    options.extend(orbit_voice::languages::LANGUAGES.iter().map(|(code, name)| (SharedString::from(*code), SharedString::from(*name))));
    dropdown(options, |p| p.language.clone(), |p, v| p.language = v, "auto", true)
}

fn mode_field() -> SettingField<SharedString> {
    let options = vec![("auto".into(), "Auto".into()), ("hold".into(), "Hold to talk".into()), ("toggle".into(), "Toggle".into())];
    dropdown(
        options,
        |p| match p.mode {
            Mode::Auto => "auto",
            Mode::Hold => "hold",
            Mode::Toggle => "toggle",
        }
        .to_owned(),
        |p, v| {
            p.mode = match v.as_str() {
                "hold" => Mode::Hold,
                "toggle" => Mode::Toggle,
                _ => Mode::Auto,
            }
        },
        "auto",
        false,
    )
}

fn silence_field() -> SettingField<SharedString> {
    let options = [0u32, 5, 10, 30, 60, 120]
        .iter()
        .map(|s| (SharedString::from(s.to_string()), SharedString::from(if *s == 0 { "Never".to_owned() } else { format!("{s} seconds") })))
        .collect();
    dropdown(options, |p| p.silence_timeout.to_string(), |p, v| p.silence_timeout = v.parse().unwrap_or(30), "30", false)
}

fn unload_field() -> SettingField<SharedString> {
    let options = [1u32, 5, 15, 60]
        .iter()
        .map(|m| (SharedString::from(m.to_string()), SharedString::from(if *m == 1 { "1 minute".to_owned() } else { format!("{m} minutes") })))
        .collect();
    dropdown(options, |p| p.unload_minutes.to_string(), |p, v| p.unload_minutes = v.parse().unwrap_or(5), "5", false)
}

fn bound_key(action: Box<dyn Action>) -> SettingField<SharedString> {
    SettingField::render(move |_, _, cx| match crate::ui::keycaps(action.as_ref(), cx) {
        Some(keys) => keys,
        None => muted("Not bound", cx).into_any_element(),
    })
}

// ── Models ───────────────────────────────────────────────────────────────

/// Onboarding's optional dictation step: download the default model in the
/// background while setup continues.
pub fn onboarding_control(cx: &App) -> AnyElement {
    let Some(voice) = super::voice(cx) else { return div().into_any_element() };
    let theme = cx.theme().clone();
    let model = super::selected_model(cx);
    let models = voice.models.clone();
    let m = models.read(cx);
    let keys = super::shortcut_label(&super::Dictate, cx).unwrap_or_else(|| "Ctrl+M".into());
    if m.is_installed(model) {
        return muted(&format!("Ready. Press {keys} to speak instead of typing."), cx).into_any_element();
    }
    if let Some(p) = m.progress(model) {
        return h_flex()
            .gap_3()
            .child(div().w_32().child(ProgressBar::new("onboarding-voice-progress").value(p.fraction() * 100.0)))
            .child(muted(&format!("Downloading {}… you can continue.", super::setup::describe(model)), cx))
            .into_any_element();
    }
    h_flex()
        .gap_3()
        .child(
            Button::new("onboarding-voice")
                .outline()
                .icon(IconName::Mic)
                .label(format!("Download {} · {}", super::setup::describe(model), size(model.download_bytes())))
                .on_click(move |_, _, cx| models.update(cx, |m, cx| m.install(model, cx))),
        )
        .child(div().text_sm().text_color(theme.muted_foreground).child("Optional. Runs on this computer."))
        .into_any_element()
}

/// An element id for a model's row or control, from the model's id.
fn item_id(part: &str, model: &str) -> ElementId {
    ElementId::Name(format!("{part}-{model}").into())
}

fn speed(model: &ModelInfo) -> &'static str {
    match model.speed {
        4 => "fastest",
        3 => "fast",
        2 => "moderate",
        _ => "slow on a CPU",
    }
}

fn quality(model: &ModelInfo) -> &'static str {
    match model.quality {
        1 => "basic accuracy",
        2 => "good accuracy",
        3 => "better accuracy",
        _ => "best accuracy",
    }
}

fn models_list(_: &mut Window, cx: &mut App) -> AnyElement {
    let Some(voice) = super::voice(cx) else { return div().into_any_element() };
    let models = voice.models.clone();
    let selected = super::selected_model(cx).id;
    let theme = cx.theme().clone();
    let rows = MODELS.iter().enumerate().map(|(ix, model)| {
        let m = models.read(cx);
        let status = m.status(model);
        let progress = m.progress(model);
        let error = m.error(model).map(str::to_owned);
        let in_use = model.id == selected;
        // The name says English or 99 languages (two models share a size).
        let detail = format!(
            "{} download, {} on disk · {}, {}",
            size(model.download_bytes()),
            size(model.installed_bytes),
            speed(model),
            quality(model)
        );
        let id = model.id;
        let actions = h_flex().gap_1().flex_shrink_0().map(|el| {
            let models = models.clone();
            if let Some(p) = progress.clone() {
                let label = match p.phase() {
                    Phase::Downloading => format!("{} %", (p.fraction() * 100.0).round()),
                    Phase::Verifying => "Checking…".to_owned(),
                    Phase::Converting => "Preparing…".to_owned(),
                };
                return el
                    .child(div().w_24().child(ProgressBar::new(item_id("voice-model-progress", id)).value(p.fraction() * 100.0)))
                    .child(div().w_16().text_xs().text_color(theme.muted_foreground).child(label))
                    .child(
                        Button::new(item_id("voice-model-cancel", id))
                            .ghost()
                            .small()
                            .label("Cancel")
                            .on_click(move |_, _, cx| models.update(cx, |m, cx| m.cancel(model, cx))),
                    );
            }
            match status {
                Status::Installed => {
                    let delete = models.clone();
                    el.when(!in_use, |el| {
                        el.child(
                            Button::new(item_id("voice-model-use", id))
                                .small()
                                .label("Use")
                                .on_click(move |_, _, cx| update_prefs(cx, |p| p.model = id.to_owned())),
                        )
                    })
                    .child(
                        Button::new(item_id("voice-model-delete", id))
                            .ghost()
                            .small()
                            .label("Delete")
                            .tooltip(format!("Frees {}", size(model.installed_bytes)))
                            .on_click(move |_, window, cx| {
                                if let Err(e) = delete.update(cx, |m, cx| m.delete(model, cx)) {
                                    use gpui_kit::component::WindowExt as _;
                                    window.push_notification(gpui_kit::component::notification::Notification::error(e), cx);
                                }
                            }),
                    )
                }
                Status::Partial(_) | Status::Missing => {
                    let label = if matches!(status, Status::Partial(_)) { "Resume" } else { "Download" };
                    el.child(Button::new(item_id("voice-model-download", id)).small().label(label).on_click(move |_, _, cx| {
                        update_prefs(cx, |p| p.model = id.to_owned());
                        models.update(cx, |m, cx| m.install(model, cx));
                    }))
                }
            }
        });
        h_flex()
            .id(item_id("voice-model", id))
            .when(ix > 0, |el| el.border_t_1().border_color(theme.border))
            .w_full()
            .gap_3()
            .py_2()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(super::setup::describe(model)))
                            .when(in_use && status == Status::Installed, |el| {
                                el.child(div().text_xs().text_color(theme.muted_foreground).child("In use"))
                            })
                            .when(in_use && status != Status::Installed, |el| {
                                el.child(div().text_xs().text_color(theme.muted_foreground).child("Selected, not downloaded"))
                            }),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).truncate().child(detail))
                    .when_some(error, |el, e| el.child(div().text_xs().text_color(theme.danger).child(e))),
            )
            .child(actions)
    });
    v_flex().w_full().children(rows).into_any_element()
}

// ── Vocabulary ───────────────────────────────────────────────────────────

struct VocabularyState {
    input: Entity<TextareaState>,
    _change: Subscription,
}

fn vocabulary_field() -> SettingField<SharedString> {
    SettingField::render(move |options: &RenderOptions, window, cx| {
        let current = prefs(cx).vocabulary.clone();
        let state = window.use_keyed_state("voice-vocabulary", cx, {
            let current = current.clone();
            move |window, cx| {
                let input = cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(3, 10)
                        .placeholder("GPUI, crossfire, keymap.rs\nsoccer do => sockudo")
                        .default_value(current)
                });
                super::register_field(super::Field::textarea(&input), cx);
                let _change = cx.subscribe(&input, |_, input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        let text = input.read(cx).value().to_string();
                        update_prefs(cx, |p| p.vocabulary = text);
                    }
                });
                VocabularyState { input, _change }
            }
        });
        let input = state.read(cx).input.clone();
        let focused = input.read(cx).presentation().focus_handle().is_focused(window);
        if !focused && input.read(cx).value().as_ref() != current {
            input.update(cx, |input, cx| input.set_value(current.clone(), window, cx));
        }
        div().w_full().child(Textarea::new(&input).disabled(options.is_disabled()))
    })
}

// ── Microphone ───────────────────────────────────────────────────────────

/// The device list, read off the UI thread and refreshed at most every
/// few seconds, and the level test.
#[derive(Default)]
struct MicPage {
    devices: Vec<String>,
    read_at: Option<Instant>,
    reading: bool,
    test: Option<Entity<SpeechState>>,
}

impl Global for MicPage {}

/// Refreshes the device list in the background when it is stale.
fn device_names(cx: &mut App) {
    let page = cx.default_global::<MicPage>();
    let stale = page.read_at.is_none_or(|t| t.elapsed() > Duration::from_secs(5));
    if stale && !page.reading {
        page.reading = true;
        let task = cx.background_spawn(async { mic::devices() });
        cx.spawn(async move |cx| {
            let devices = task.await;
            cx.update(|cx| {
                let page = cx.default_global::<MicPage>();
                page.devices = devices;
                page.read_at = Some(Instant::now());
                page.reading = false;
                cx.refresh_windows();
            });
        })
        .detach();
    }
}

/// The input picker, over the devices last read (the test row below keeps
/// the list fresh while the page is open).
fn microphone_field(cx: &App) -> SettingField<SharedString> {
    let devices = cx.try_global::<MicPage>().map(|p| p.devices.clone()).unwrap_or_default();
    let current = prefs(cx).microphone.clone();
    let mut items: Vec<(SharedString, SharedString)> = vec![("".into(), "System default".into())];
    items.extend(devices.iter().map(|d| (SharedString::from(d.clone()), SharedString::from(d.clone()))));
    if !current.is_empty() && !devices.contains(&current) {
        items.push((current.clone().into(), format!("{current} (not connected)").into()));
    }
    dropdown(items, |p| p.microphone.clone(), |p, v| p.microphone = v, "", true)
}

fn mic_test(_: &mut Window, cx: &mut App) -> AnyElement {
    device_names(cx);
    let theme = cx.theme().clone();
    let blocked = mic::is_blocked();
    let test = cx.default_global::<MicPage>().test.clone();
    let running = test.as_ref().is_some_and(|t| t.read(cx).status().is_active());
    let button = Button::new("voice-mic-test")
        .small()
        .label(if running { "Stop test" } else { "Test microphone" })
        .disabled(blocked)
        .on_click(move |_, _, cx| toggle_test(cx));
    let level: AnyElement = match test.filter(|_| running) {
        Some(state) => SpeechWaveform::new(&state).w_full().text_color(theme.primary).into_any_element(),
        None => muted(if blocked { "" } else { "Speak to see the level. Nothing is recorded." }, cx).into_any_element(),
    };
    v_flex()
        .w_full()
        .gap_2()
        .child(h_flex().w_full().gap_3().child(button).child(div().flex_1().min_w_0().child(level)))
        .when(blocked, |el| {
            el.child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::TriangleAlert).small().text_color(theme.warning))
                    .child(div().text_sm().child("The system blocks Orbit from the microphone."))
                    .when(HAS_PRIVACY_SETTINGS, |el| {
                        el.child(
                            Button::new("voice-open-privacy")
                                .ghost()
                                .small()
                                .label("Open privacy settings")
                                .on_click(|_, _, cx| super::open_privacy_settings(cx)),
                        )
                    }),
            )
        })
        .into_any_element()
}

fn toggle_test(cx: &mut App) {
    let page = cx.default_global::<MicPage>();
    if let Some(test) = page.test.take() {
        test.update(cx, |s, cx| s.cancel(cx));
        cx.refresh_windows();
        return;
    }
    let device = prefs(cx).microphone.clone();
    let state = cx.new(|cx| SpeechState::new(cx).recognizer(LevelOnly).input(Mic { device }).system_fallback(false));
    state.update(cx, |s, cx| s.start(cx));
    // Redraw as levels arrive; stop on its own after half a minute.
    let observe = Rc::new(cx.observe(&state, |_, cx| cx.refresh_windows()));
    let weak = state.downgrade();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_secs(30)).await;
        let _keep = observe;
        if let Some(state) = weak.upgrade() {
            cx.update(|cx| state.update(cx, |s, cx| s.cancel(cx)));
        }
    })
    .detach();
    cx.default_global::<MicPage>().test = Some(state);
    cx.refresh_windows();
}
