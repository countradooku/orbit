//! Dictation: speak into the composer or any text field, on-device.
//!
//! The kit's speech module supplies the session state, the mic button and
//! the waveform; Orbit plugs in its own recognizer (Whisper on candle, in
//! `orbit-voice`, on a worker thread) and its own microphone input (the one
//! picked in Settings › Voice). The kit's system recognizers stay off:
//! Windows' would send audio to Microsoft.
//!
//! - **Start.** Ctrl+M (or the mic button) in the composer, the Home prompt,
//!   search and palette inputs, and settings text fields. With no text field
//!   focused it dictates into the open thread's composer. The first use
//!   without a model offers the download instead of failing.
//! - **Modes.** Auto: a tap toggles, a hold is push-to-talk. Hold. Toggle.
//! - **While recording** a pill shows the waveform, the time, the live
//!   transcript, Cancel (Esc) and Done. Recording stops at five minutes,
//!   with a warning 30 seconds before; in auto and toggle modes it also
//!   stops after the silence timeout.
//! - **The text** lands at the caret or over the selection and stays
//!   editable. Send while recording waits for it, then sends.
//! - **Voice steering** (Ctrl+Shift+M): "api, run the tests" goes to the
//!   thread named api without opening it.
//! - **Idle costs nothing**: no audio stream, no model, no thread until
//!   dictation starts; the model unloads after the idle timeout.

#[cfg(debug_assertions)]
mod demo;
mod dictation;
mod mic;
pub mod models;
mod pill;
mod prefs;
mod recognizer;
pub mod settings;
mod setup;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::command::CommandState;
use gpui_kit::component::input::{InputState, TextareaState};
use gpui_kit::*;
use orbit_proto::*;
use orbit_voice::catalog::{self, ModelInfo};
use orbit_voice::whisper::WhisperLoader;
use orbit_voice::worker::Worker;

use crate::app_state::AppState;

pub use dictation::{Dictation, DictationEvent, Purpose, Sources};
pub use mic::open_privacy_settings;
pub use prefs::VoicePrefs;

gpui_kit::actions!(
    voice,
    [
        /// Dictate into the focused text field, or the open thread's composer.
        Dictate,
        /// Speak an instruction to an agent by name: "api, run the tests".
        SteerByVoice,
        /// Stop dictating and discard what was said.
        CancelDictation,
        /// Stop dictating and insert the transcript.
        ConfirmDictation,
        /// Settings › Voice.
        OpenVoiceSettings,
    ]
);

/// The dictation shortcut: Ctrl+M on every platform, as in Codex and
/// Cursor (Cmd+M is the system's Minimize on macOS). Not in terminals,
/// where Ctrl+M is Return.
pub const DICTATE_KEYS: &str = "ctrl-m";
pub const STEER_KEYS: &str = "ctrl-shift-m";
const NOT_IN_TERMINAL: Option<&str> = Some("!Terminal");

/// How long cached project terms stay fresh.
const TERMS_TTL: Duration = Duration::from_secs(10 * 60);

/// A text field dictation can type into.
#[derive(Clone)]
pub enum Field {
    Input(WeakEntity<InputState>),
    Textarea(WeakEntity<TextareaState>),
    /// The command palette's query.
    Command(WeakEntity<CommandState>),
}

impl Field {
    pub fn input(state: &Entity<InputState>) -> Self {
        Self::Input(state.downgrade())
    }

    pub fn textarea(state: &Entity<TextareaState>) -> Self {
        Self::Textarea(state.downgrade())
    }

    pub fn command(state: &Entity<CommandState>) -> Self {
        Self::Command(state.downgrade())
    }

    fn entity_id(&self) -> EntityId {
        match self {
            Field::Input(w) => w.entity_id(),
            Field::Textarea(w) => w.entity_id(),
            Field::Command(w) => w.entity_id(),
        }
    }

    fn is_alive(&self) -> bool {
        match self {
            Field::Input(w) => w.upgrade().is_some(),
            Field::Textarea(w) => w.upgrade().is_some(),
            Field::Command(w) => w.upgrade().is_some(),
        }
    }

    fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        match self {
            Field::Input(w) => w.upgrade().map(|s| s.read(cx).focus_handle(cx)),
            Field::Textarea(w) => w.upgrade().map(|s| s.read(cx).focus_handle(cx)),
            Field::Command(w) => w.upgrade().map(|s| s.read(cx).focus_handle(cx)),
        }
    }

    fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle(cx).is_some_and(|f| f.is_focused(window))
    }

    /// Puts `transcript` at the caret or over the selection, spaced and
    /// capitalised for where it lands, and leaves the caret after it.
    fn insert(&self, transcript: &str, window: &mut Window, cx: &mut App) -> bool {
        fn apply<T: 'static>(
            state: &Entity<T>,
            transcript: &str,
            window: &mut Window,
            cx: &mut App,
            read: impl Fn(&T) -> (SharedString, std::ops::Range<usize>),
            write: impl FnOnce(&mut T, std::ops::Range<usize>, String, &mut Window, &mut Context<T>),
        ) {
            let (value, selection) = read(state.read(cx));
            let inserted = orbit_voice::insert::insert(&value, selection.clone(), transcript);
            let selection = selection.start.min(value.len())..selection.end.min(value.len());
            let after = value.len() - selection.end;
            let piece = inserted.text[selection.start..inserted.text.len() - after].to_owned();
            state.update(cx, |s, cx| write(s, selection, piece, window, cx));
        }
        match self {
            Field::Input(w) => {
                let Some(state) = w.upgrade() else { return false };
                apply(&state, transcript, window, cx, |s| (s.value(), s.selected_range()), |s, range, piece, window, cx| {
                    s.set_selected_range(range, cx);
                    s.replace(piece, window, cx);
                    s.focus(window, cx);
                });
            }
            Field::Textarea(w) => {
                let Some(state) = w.upgrade() else { return false };
                apply(&state, transcript, window, cx, |s| (s.value(), s.selected_range()), |s, range, piece, window, cx| {
                    s.set_selected_range(range, cx);
                    s.replace(piece, window, cx);
                    s.focus(window, cx);
                });
            }
            Field::Command(w) => {
                // The palette's query takes the words at its end.
                let Some(state) = w.upgrade() else { return false };
                let query = state.read(cx).query(cx);
                let end = query.len();
                let text = orbit_voice::insert::insert(&query, end..end, transcript).text;
                state.update(cx, |s, cx| s.set_query(text, window, cx));
            }
        }
        true
    }
}

/// What a dictation is for: a field, or a composer (which can also send).
#[derive(Clone)]
pub struct Target {
    pub field: Field,
    pub composer: Option<WeakEntity<crate::composer::Composer>>,
    /// The thread or project whose names seed the vocabulary.
    pub scope: Scope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Scope {
    #[default]
    None,
    Thread(ThreadId),
    Project(ProjectId),
}

pub struct Voice {
    app: Entity<AppState>,
    pub(crate) worker: Worker,
    pub(crate) models: Entity<models::Models>,
    active: Option<Entity<Dictation>>,
    /// Fields that take dictation, most recently registered last.
    fields: Vec<(Field, Option<WeakEntity<crate::composer::Composer>>, Scope)>,
    /// File names per scope for the vocabulary prompt, and when fetched.
    terms: HashMap<Scope, (Instant, Vec<String>)>,
}

impl Global for Voice {}

pub fn init(data_dir: &std::path::Path, app: Entity<AppState>, cx: &mut App) {
    let store = orbit_voice::store::Store::new(data_dir.join("models").join("whisper"));
    let prefs = crate::prefs::get(cx).voice.clone();
    let worker = Worker::new(Duration::from_secs(u64::from(prefs.unload_minutes.max(1)) * 60));
    let models = cx.new(|cx| models::Models::new(store, cx));
    cx.set_global(Voice { app, worker, models, active: None, fields: Vec::new(), terms: HashMap::new() });
    cx.bind_keys([
        KeyBinding::new(DICTATE_KEYS, Dictate, NOT_IN_TERMINAL),
        KeyBinding::new(STEER_KEYS, SteerByVoice, NOT_IN_TERMINAL),
        KeyBinding::new("escape", CancelDictation, Some("Dictating")),
    ]);
    cx.on_action(|_: &Dictate, cx| with_active_window(cx, |window, cx| shortcut_down(Purpose::Dictate, window, cx)));
    cx.on_action(|_: &SteerByVoice, cx| with_active_window(cx, |window, cx| shortcut_down(Purpose::Steer, window, cx)));
    cx.on_action(|_: &CancelDictation, cx| cancel(cx));
    cx.on_action(|_: &ConfirmDictation, cx| confirm(cx));
}

/// Changes the voice preferences and applies what takes effect at once.
pub fn update_prefs(cx: &mut App, edit: impl FnOnce(&mut VoicePrefs)) {
    crate::prefs::update(cx, |p| edit(&mut p.voice));
    let unload = crate::prefs::get(cx).voice.unload_minutes.max(1);
    if let Some(voice) = cx.try_global::<Voice>() {
        voice.worker.set_idle_timeout(Duration::from_secs(u64::from(unload) * 60));
    }
}

fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window, &mut App)) {
    if let Some(window) = cx.active_window() {
        let _ = window.update(cx, |_, window, cx| f(window, cx));
    }
}

pub fn voice(cx: &App) -> Option<&Voice> {
    cx.try_global::<Voice>()
}

fn voice_mut(cx: &mut App) -> Option<&mut Voice> {
    if cx.has_global::<Voice>() { Some(cx.global_mut::<Voice>()) } else { None }
}

/// Lets a text field take dictation while it is focused.
pub fn register_field(field: Field, cx: &mut App) {
    register(field, None, Scope::None, cx);
}

/// Lets a composer take dictation (and send after it).
pub(crate) fn register_composer(
    input: &Entity<TextareaState>,
    composer: WeakEntity<crate::composer::Composer>,
    scope: Scope,
    cx: &mut App,
) {
    register(Field::textarea(input), Some(composer), scope, cx);
}

/// Updates the scope of a registered composer (Home's project changes).
pub(crate) fn set_scope(input: &Entity<TextareaState>, scope: Scope, cx: &mut App) {
    if let Some(voice) = voice_mut(cx) {
        let id = input.entity_id();
        for entry in &mut voice.fields {
            if entry.0.entity_id() == id {
                entry.2 = scope;
            }
        }
    }
}

fn register(field: Field, composer: Option<WeakEntity<crate::composer::Composer>>, scope: Scope, cx: &mut App) {
    let Some(voice) = voice_mut(cx) else { return };
    voice.fields.retain(|f| f.0.is_alive() && f.0.entity_id() != field.entity_id());
    voice.fields.push((field, composer, scope));
}

/// The model picked in Settings › Voice.
pub fn selected_model(cx: &App) -> &'static ModelInfo {
    let id = &crate::prefs::get(cx).voice.model;
    catalog::find(id).or_else(|| catalog::find(catalog::DEFAULT_MODEL)).unwrap_or(&catalog::MODELS[0])
}

/// A loader for the selected model, if it is installed.
fn loader(cx: &App) -> Option<Arc<WhisperLoader>> {
    let voice = voice(cx)?;
    let model = selected_model(cx);
    let (weights, tokenizer) = voice.models.read(cx).paths(model)?;
    let gpu = crate::prefs::get(cx).voice.gpu && orbit_voice::whisper::HAS_GPU_BACKEND;
    Some(Arc::new(WhisperLoader { model: weights, tokenizer, dims: model.dims, gpu }))
}

/// Loads the model ahead of use (the pointer rests on a mic button).
pub fn prewarm(scope: Scope, cx: &mut App) {
    refresh_terms(scope, cx);
    if let (Some(loader), Some(voice)) = (loader(cx), voice(cx)) {
        voice.worker.prewarm(loader);
    }
}

/// The dictation in progress, if any.
pub fn active(cx: &App) -> Option<Entity<Dictation>> {
    voice(cx)?.active.clone()
}

/// The field dictation should go to: the focused registered field, else the
/// composer most recently shown (the open thread's or Home's).
fn focused_target(window: &Window, cx: &App) -> Option<Target> {
    let voice = voice(cx)?;
    let alive = voice.fields.iter().filter(|f| f.0.is_alive());
    let focused = alive.clone().rev().find(|f| f.0.is_focused(window, cx));
    let entry = focused.or_else(|| alive.rev().find(|f| f.1.as_ref().is_some_and(|c| c.upgrade().is_some())))?;
    Some(Target { field: entry.0.clone(), composer: entry.1.clone(), scope: entry.2 })
}

/// The shortcut went down.
fn shortcut_down(purpose: Purpose, window: &mut Window, cx: &mut App) {
    if let Some(active) = active(cx) {
        active.update(cx, |d, cx| d.key_down(window, cx));
        return;
    }
    let Some(target) = focused_target(window, cx) else { return };
    start(target, purpose, true, window, cx);
}

/// The mic button of `target` was clicked.
pub fn click(target: Target, window: &mut Window, cx: &mut App) {
    if let Some(active) = active(cx) {
        active.update(cx, |d, cx| d.click(window, cx));
        return;
    }
    start(target, Purpose::Dictate, false, window, cx);
}

fn start(target: Target, purpose: Purpose, by_key: bool, window: &mut Window, cx: &mut App) {
    let Some(loader) = loader(cx) else {
        setup::open(window, cx);
        return;
    };
    let config = session_config(target.scope, cx);
    let keys = if purpose == Purpose::Steer { STEER_KEYS } else { DICTATE_KEYS };
    // Push-to-talk ends when the shortcut's key or its modifiers come up:
    // read them from the live keymap, so a rebound shortcut holds too.
    let release = window
        .bindings_for_action(if purpose == Purpose::Steer { &SteerByVoice as &dyn Action } else { &Dictate })
        .last()
        .and_then(|b| b.keystrokes().last().map(|k| (k.key().to_owned(), *k.modifiers())))
        .or_else(|| Keystroke::parse(keys).ok().map(|k| (k.key.clone(), k.modifiers)))
        .unwrap_or_else(|| ("m".to_owned(), Modifiers::control()));
    let dictation = cx.new(|cx| Dictation::whisper(target, purpose, loader, config, release, by_key, window, cx));
    activate(dictation, cx);
}

/// Makes `dictation` the one in progress until it ends.
fn activate(dictation: Entity<Dictation>, cx: &mut App) {
    if let Some(voice) = voice_mut(cx) {
        voice.active = Some(dictation.clone());
    }
    cx.subscribe(&dictation, |_, event: &DictationEvent, cx| {
        if matches!(event, DictationEvent::Ended)
            && let Some(voice) = voice_mut(cx)
        {
            voice.active = None;
            cx.refresh_windows();
        }
    })
    .detach();
    cx.refresh_windows();
}

/// Debug builds: `ORBIT_DEBUG_DICTATION` = `setup` opens the setup sheet;
/// any other text is dictated into the composer by a synthetic voice and a
/// scripted recognizer (no microphone, no model), for screenshots.
pub fn on_window_open(window: &mut Window, cx: &mut App) {
    #[cfg(debug_assertions)]
    demo::on_window_open(window, cx);
    let _ = (window, cx);
}

/// A key came up anywhere in the workspace: ends push-to-talk.
pub fn key_up(event: &KeyUpEvent, window: &mut Window, cx: &mut App) {
    if let Some(active) = active(cx) {
        active.update(cx, |d, cx| d.key_up(&event.keystroke.key, window, cx));
    }
}

pub fn cancel(cx: &mut App) {
    if let Some(active) = active(cx) {
        active.update(cx, |d, cx| d.cancel(cx));
    }
}

pub fn confirm(cx: &mut App) {
    if let Some(active) = active(cx) {
        active.update(cx, |d, cx| d.stop(cx));
    }
}

/// Send was pressed in `composer` while dictating into it: send once the
/// transcript is in. Returns whether it will.
pub fn send_after_transcript(input: EntityId, cx: &mut App) -> bool {
    match active(cx) {
        Some(active) if active.read(cx).target_id() == input && active.read(cx).is_running(cx) => {
            active.update(cx, |d, cx| d.send_when_done(cx));
            true
        }
        _ => false,
    }
}

fn session_config(scope: Scope, cx: &App) -> orbit_voice::stream::SessionConfig {
    let prefs = &crate::prefs::get(cx).voice;
    let model = selected_model(cx);
    let vocabulary = orbit_voice::prompt::Vocabulary::parse(&prefs.vocabulary);
    let (project, files) = if prefs.project_terms { project_terms(scope, cx) } else { (Vec::new(), Vec::new()) };
    let language = if model.english_only || prefs.language == "auto" { None } else { Some(prefs.language.clone()) };
    orbit_voice::stream::SessionConfig {
        language: if model.english_only { Some("en".into()) } else { language },
        translate: prefs.translate && !model.english_only,
        prompt: orbit_voice::prompt::build(&vocabulary, &project, &files),
        preview: prefs.live_preview,
        cleanup: orbit_voice::postprocess::Cleanup {
            remove_fillers: prefs.remove_fillers,
            tidy: prefs.tidy,
            replacements: vocabulary.replacements,
        },
    }
}

/// Names from the project: the project, its threads' titles and branches,
/// and (cached from the host) distinctive file names.
fn project_terms(scope: Scope, cx: &App) -> (Vec<String>, Vec<String>) {
    let Some(voice) = voice(cx) else { return Default::default() };
    let state = voice.app.read(cx).state();
    let project = match scope {
        Scope::Thread(id) => state.threads.get(&id).and_then(|t| t.project),
        Scope::Project(id) => Some(id),
        Scope::None => None,
    };
    let mut names = Vec::new();
    if let Some(project) = project.and_then(|p| state.projects.iter().find(|x| x.id == p)) {
        names.push(project.name.clone());
    }
    let files = voice.terms.get(&scope).map(|t| t.1.clone()).unwrap_or_default();
    (names, files)
}

/// Fetches the project's file names from the host in the background, at
/// most every ten minutes, for the next dictation's vocabulary prompt.
fn refresh_terms(scope: Scope, cx: &mut App) {
    if !crate::prefs::get(cx).voice.project_terms {
        return;
    }
    let Some(voice) = voice(cx) else { return };
    if voice.terms.get(&scope).is_some_and(|t| t.0.elapsed() < TERMS_TTL) {
        return;
    }
    let (thread, project) = match scope {
        Scope::Thread(id) => (Some(id), None),
        Scope::Project(id) => (None, Some(id)),
        Scope::None => return,
    };
    let app = voice.app.clone();
    if let Some(voice) = voice_mut(cx) {
        // Mark it fetched now, so hovering does not send a search per frame.
        voice.terms.entry(scope).or_insert((Instant::now(), Vec::new())).0 = Instant::now();
    }
    let command = Command::SearchFiles { thread, project, query: String::new(), limit: 400 };
    let task = app.update(cx, |app, cx| app.call(command, cx));
    cx.spawn(async move |cx| {
        if let Ok(Reply::Files(files)) = task.await {
            let terms = orbit_voice::prompt::file_terms(files.iter().map(|f| f.path.as_str()));
            cx.update(|cx| {
                if let Some(voice) = voice_mut(cx) {
                    voice.terms.insert(scope, (Instant::now(), terms));
                }
            });
        }
    })
    .detach();
}

/// The dictation pill floating over the window, for a dictation into a
/// field that does not show one itself (search, palette, settings fields).
pub fn floating_pill(cx: &App) -> Option<AnyElement> {
    let active = active(cx)?;
    (!active.read(cx).has_composer()).then(|| active.into_any_element())
}

/// The pill for a dictation into `input` (the composer shows it inline).
pub fn pill_for(input: EntityId, cx: &App) -> Option<Entity<Dictation>> {
    active(cx).filter(|d| d.read(cx).target_id() == input)
}

/// The shortcut for `action` as text: "Ctrl+M".
pub fn shortcut_label(action: &dyn Action, cx: &App) -> Option<String> {
    let strokes = crate::keymap::keystrokes_for(action, cx)?;
    Some(strokes.iter().map(gpui_kit::component::kbd::Kbd::format).collect::<Vec<_>>().join(" "))
}
