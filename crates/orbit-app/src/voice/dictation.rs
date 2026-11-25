//! One dictation: the kit's speech state with Orbit's recognizer and
//! microphone, the shortcut's mode, the cap and silence timer, and where the
//! transcript goes when it is done.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::speech::{AudioInput, SpeechError, SpeechEvent, SpeechRecognizer, SpeechState};
use gpui_kit::*;
use orbit_proto::*;
use orbit_voice::control::{Action as ControlAction, Controller, StopReason};
use orbit_voice::stream::SessionConfig;
use orbit_voice::whisper::WhisperLoader;

use super::mic::{HAS_PRIVACY_SETTINGS, Mic};
use super::recognizer::{Live, WhisperRecognizer};
use super::{Target, Voice};

/// How often the timer, the cap and push-to-talk release are checked.
const TICK: Duration = Duration::from_millis(100);
/// How long the kit waits for the final pass after capture stops: the last
/// window is at most 20 seconds of audio, which large-v3-turbo on a slow
/// CPU decodes well within this.
const FINAL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// Text into a field.
    Dictate,
    /// An instruction for the agent it names.
    Steer,
}

pub enum DictationEvent {
    Ended,
}

/// Where a dictation's audio and text come from: Whisper and the
/// microphone, or (tests and the debug demo) stand-ins.
pub struct Sources {
    pub recognizer: Rc<dyn SpeechRecognizer>,
    pub input: Rc<dyn AudioInput>,
    /// What the recognizer shares with the pill.
    pub live: Rc<RefCell<Live>>,
}

pub struct Dictation {
    target: Target,
    purpose: Purpose,
    state: Entity<SpeechState>,
    live: Rc<RefCell<Live>>,
    controller: Controller,
    /// The shortcut's key and modifiers: releasing either ends a hold.
    release_key: String,
    release_modifiers: Modifiers,
    started: Instant,
    /// Seconds shown, to redraw the timer once a second.
    shown_seconds: u64,
    warned: bool,
    stopped_by: Option<StopReason>,
    send_after: bool,
    ended: bool,
    _tick: Task<()>,
    _speech: Subscription,
    _redraw: Subscription,
}

impl EventEmitter<DictationEvent> for Dictation {}

impl Render for Dictation {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let floating = !self.has_composer();
        let pill = super::pill::render(self, floating, cx);
        if floating {
            // Over the window, bottom centre, clear of the status bar.
            div()
                .absolute()
                .bottom_10()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(div().w(rems(36.)).child(pill))
                .into_any_element()
        } else {
            div().w_full().child(pill).into_any_element()
        }
    }
}

impl Dictation {
    /// A dictation with Whisper and the picked microphone.
    pub(super) fn whisper(
        target: Target,
        purpose: Purpose,
        loader: Arc<WhisperLoader>,
        config: SessionConfig,
        release: (String, Modifiers),
        by_key: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let live = Rc::new(RefCell::new(Live::default()));
        let worker = cx.global::<Voice>().worker.clone();
        let recognizer = Rc::new(WhisperRecognizer { worker, loader, config, live: live.clone() });
        let mic = Rc::new(Mic { device: crate::prefs::get(cx).voice.microphone.clone() });
        Self::new(target, purpose, Sources { recognizer, input: mic, live }, release, by_key, window, cx)
    }

    pub(super) fn new(
        target: Target,
        purpose: Purpose,
        sources: Sources,
        release: (String, Modifiers),
        by_key: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let prefs = crate::prefs::get(cx).voice.clone();
        let Sources { recognizer, input, live } = sources;
        let state = cx.new(|cx| {
            SpeechState::new(cx).recognizer(recognizer).input(input).system_fallback(false).stop_timeout(FINAL_TIMEOUT)
        });
        let speech = cx.subscribe_in(&state, window, Self::on_speech);
        let redraw = cx.observe(&state, |_, _, cx| cx.notify());
        let silence = (prefs.silence_timeout > 0).then(|| Duration::from_secs(u64::from(prefs.silence_timeout)));
        let mut controller = Controller::new(prefs.mode, silence);
        let now = Instant::now();
        if by_key {
            controller.key_down(now);
        } else {
            controller.click(now);
        }
        let tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if this.update_in(cx, |this, window, cx| this.tick(window, cx)).is_err() {
                    break;
                }
            }
        });
        state.update(cx, |s, cx| s.start(cx));
        Self {
            target,
            purpose,
            state,
            live,
            controller,
            release_key: release.0,
            release_modifiers: release.1,
            started: now,
            shown_seconds: 0,
            warned: false,
            stopped_by: None,
            send_after: false,
            ended: false,
            _tick: tick,
            _speech: speech,
            _redraw: redraw,
        }
    }

    pub fn state(&self) -> &Entity<SpeechState> {
        &self.state
    }

    pub fn purpose(&self) -> Purpose {
        self.purpose
    }

    pub fn live(&self) -> Live {
        self.live.borrow().clone()
    }

    pub fn has_composer(&self) -> bool {
        self.target.composer.is_some()
    }

    pub fn target_id(&self) -> EntityId {
        self.target.field.entity_id()
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Recording ends soon (the five-minute cap).
    pub fn near_cap(&self) -> bool {
        self.warned
    }

    pub fn stopped_by(&self) -> Option<StopReason> {
        self.stopped_by
    }

    /// Whether the session is recording or finishing.
    pub fn is_running(&self, cx: &App) -> bool {
        self.state.read(cx).status().is_active()
    }

    /// Send was pressed: it happens when the transcript is in.
    pub fn sends_when_done(&self) -> bool {
        self.send_after
    }

    pub(super) fn key_down(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let action = self.controller.key_down(Instant::now());
        self.apply(action, cx);
    }

    pub(super) fn key_up(&mut self, key: &str, _: &mut Window, cx: &mut Context<Self>) {
        if key.eq_ignore_ascii_case(&self.release_key) {
            let action = self.controller.key_up(Instant::now());
            self.apply(action, cx);
        }
    }

    pub(super) fn click(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let action = self.controller.click(Instant::now());
        self.apply(action, cx);
    }

    /// Stop recording and transcribe.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        self.controller.reset();
        self.state.update(cx, |s, cx| s.stop(cx));
        cx.refresh_windows();
    }

    /// Stop and discard.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.controller.reset();
        self.send_after = false;
        self.state.update(cx, |s, cx| s.cancel(cx));
    }

    pub(super) fn send_when_done(&mut self, cx: &mut Context<Self>) {
        self.send_after = true;
        if self.state.read(cx).status().is_capturing() {
            self.stop(cx);
        }
        cx.notify();
    }

    fn apply(&mut self, action: Option<ControlAction>, cx: &mut Context<Self>) {
        match action {
            Some(ControlAction::Stop(reason)) => {
                self.stopped_by = Some(reason);
                self.state.update(cx, |s, cx| s.stop(cx));
                // The composer's mic button shows the change too.
                cx.refresh_windows();
            }
            Some(ControlAction::Warn { .. }) => {
                self.warned = true;
                cx.notify();
            }
            Some(ControlAction::Start) | None => {}
        }
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ended {
            return;
        }
        // The field went away (the palette closed): nothing to type into.
        if !self.target.field.is_alive() {
            self.cancel(cx);
            return;
        }
        let status = self.state.read(cx).status();
        if !status.is_capturing() {
            self.controller.reset();
            return;
        }
        let now = Instant::now();
        // A hold ends when the shortcut's modifier comes up, even if the key's
        // own release went to another window.
        if self.controller.is_held() && !holds(window.modifiers(), self.release_modifiers) {
            let action = self.controller.key_up(now);
            self.apply(action, cx);
            return;
        }
        let live = self.live.borrow().clone();
        let silence = live.heard_speech.then_some(live.silence);
        let action = self.controller.tick(now, silence);
        self.apply(action, cx);
        let seconds = self.started.elapsed().as_secs();
        if seconds != self.shown_seconds {
            self.shown_seconds = seconds;
            cx.notify();
        }
    }

    fn on_speech(&mut self, _: &Entity<SpeechState>, event: &SpeechEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            SpeechEvent::Started => cx.refresh_windows(),
            SpeechEvent::Partial(_) => cx.notify(),
            SpeechEvent::Final(text) => {
                let text = text.to_string();
                self.deliver(&text, window, cx);
                self.end(cx);
            }
            SpeechEvent::Cancelled => self.end(cx),
            SpeechEvent::Error(error) => {
                report(error, window, cx);
                self.end(cx);
            }
        }
    }

    fn end(&mut self, cx: &mut Context<Self>) {
        if !self.ended {
            self.ended = true;
            self.controller.reset();
            cx.emit(DictationEvent::Ended);
            cx.refresh_windows();
        }
    }

    fn deliver(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.purpose == Purpose::Steer && !text.is_empty() && steer(text, window, cx) {
            return;
        }
        if !text.is_empty() {
            self.target.field.insert(text, window, cx);
        }
        if self.send_after
            && let Some(composer) = self.target.composer.as_ref().and_then(|c| c.upgrade())
        {
            composer.update(cx, |c, cx| c.send_dictated(window, cx));
        }
    }
}

fn holds(now: Modifiers, needed: Modifiers) -> bool {
    (!needed.control || now.control) && (!needed.alt || now.alt) && (!needed.shift || now.shift) && (!needed.platform || now.platform)
}

/// Sends an addressed instruction to the thread it names. Returns whether
/// one was named.
fn steer(text: &str, window: &mut Window, cx: &mut App) -> bool {
    let Some(voice) = cx.try_global::<Voice>() else { return false };
    let app = voice.app.clone();
    let state = app.read(cx).state();
    let candidates: Vec<orbit_voice::address::Candidate<ThreadId>> = state
        .threads
        .values()
        .filter(|t| !t.archived)
        .map(|t| {
            let mut names = vec![crate::thread::display_title(t).to_owned()];
            names.extend(t.workspace.branch.clone());
            orbit_voice::address::Candidate { key: t.id, names }
        })
        .collect();
    let Some(addressed) = orbit_voice::address::resolve(text, &candidates) else {
        window.push_notification(
            Notification::info("Say the agent’s name first, as in “api, run the tests”. The text went into the composer."),
            cx,
        );
        return false;
    };
    let running = state.threads.get(&addressed.key).is_some_and(|t| t.status.is_busy());
    let steer = running && crate::settings::get(cx).composer.follow_up == FollowUp::Steer;
    let command = Command::Send { thread: addressed.key, text: addressed.message.clone(), attachments: Vec::new(), steer };
    app.update(cx, |app, cx| app.run(command, window, cx));
    window.push_notification(Notification::success(format!("Sent to “{}”: {}", addressed.name, addressed.message)), cx);
    true
}

/// Tells the user why dictation stopped, with the way to fix it.
fn report(error: &SpeechError, window: &mut Window, cx: &mut App) {
    let notification = match error {
        SpeechError::PermissionDenied if HAS_PRIVACY_SETTINGS => {
            let message = if cfg!(target_os = "macos") {
                "Orbit can’t use the microphone. Allow it in System Settings › Privacy & Security › Microphone."
            } else {
                "Orbit can’t use the microphone. Turn on microphone access for desktop apps in Settings › Privacy & security › Microphone."
            };
            Notification::warning(message).action(|_, _, _| {
                Button::new("voice-privacy").label("Open privacy settings").on_click(|_, _, cx| super::open_privacy_settings(cx))
            })
        }
        SpeechError::PermissionDenied => Notification::warning("Orbit can’t use the microphone."),
        SpeechError::NoInputDevice => {
            Notification::warning("No microphone found. Connect one, or pick another in Settings › Voice.")
        }
        SpeechError::Unsupported => Notification::warning("Dictation isn’t available in this build."),
        SpeechError::Input(e) => Notification::warning(format!("The microphone stopped: {e}")),
        SpeechError::Recognizer(e) => Notification::error(format!("{e}")),
    };
    window.push_notification(notification, cx);
}

#[cfg(test)]
mod tests {
    //! Dictation end to end in a window: the kit's speech state, Orbit's
    //! recognizer on the real worker thread with a scripted engine, synthetic
    //! speech pushed as the microphone would, and the text landing in a
    //! textarea.

    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::Duration;

    use gpui_kit::component::input::{Textarea, TextareaState};
    use gpui_kit::component::speech::{AudioFormat, AudioInput, AudioSink, SpeechError, SpeechStatus};
    use gpui_kit::{
        AnyWindowHandle, App, AppContext as _, Context, Entity, IntoElement, Modifiers, ParentElement as _, Render, Styled as _,
        Subscription, TestAppContext, Window, WindowOptions, div,
    };
    use orbit_voice::engine::Engine;
    use orbit_voice::engine::fake::{FakeEngine, Utterance};
    use orbit_voice::stream::SessionConfig;
    use orbit_voice::worker::{Loader, Worker};

    use super::{Dictation, DictationEvent, Live, Purpose, Sources, Target, WhisperRecognizer};
    use crate::voice::{Field, Scope};

    struct ScriptLoader(Arc<Utterance>);

    impl Loader for ScriptLoader {
        fn key(&self) -> String {
            "script".into()
        }

        fn load(&self) -> Result<Box<dyn Engine>, String> {
            Ok(Box::new(FakeEngine::new(&self.0)))
        }
    }

    /// Hands its sink to the test, which pushes the audio.
    #[derive(Clone, Default)]
    struct FakeMic(Rc<RefCell<Option<AudioSink>>>);

    impl AudioInput for FakeMic {
        fn start(&self, _: AudioFormat, sink: AudioSink, _: &mut App) -> Result<Subscription, SpeechError> {
            *self.0.borrow_mut() = Some(sink);
            let mic = self.0.clone();
            Ok(Subscription::new(move || *mic.borrow_mut() = None))
        }
    }

    struct Host {
        input: Entity<TextareaState>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(Textarea::new(&self.input))
        }
    }

    struct Fixture {
        window: AnyWindowHandle,
        host: Entity<Host>,
        mic: FakeMic,
        utterance: Arc<Utterance>,
        worker: Worker,
    }

    fn fixture(cx: &mut TestAppContext, text: &str, caret: usize) -> Fixture {
        cx.update(gpui_kit::init);
        let text = text.to_owned();
        let (window, host) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
                let input = cx.new(|cx| TextareaState::new(window, cx).default_value(text.clone()));
                input.update(cx, |s, cx| s.set_selected_range(caret..caret, cx));
                cx.new(|_| Host { input })
            })
            .unwrap()
        });
        let utterance = Arc::new(Utterance::new("fix the parser", 0.4, 0.6));
        Fixture { window, host, mic: FakeMic::default(), utterance, worker: Worker::new(Duration::from_secs(60)) }
    }

    fn start(f: &Fixture, cx: &mut TestAppContext) -> Entity<Dictation> {
        let live = Rc::new(RefCell::new(Live::default()));
        let recognizer = WhisperRecognizer {
            worker: f.worker.clone(),
            loader: Arc::new(ScriptLoader(f.utterance.clone())),
            config: SessionConfig { preview: true, ..Default::default() },
            live: live.clone(),
        };
        let sources = Sources { recognizer: Rc::new(recognizer), input: Rc::new(f.mic.clone()), live };
        let input = f.host.read_with(cx, |h, _| h.input.clone());
        let target = Target { field: Field::textarea(&input), composer: None, scope: Scope::None };
        f.window
            .update(cx, |_, window, cx| {
                cx.new(|cx| Dictation::new(target, Purpose::Dictate, sources, ("m".into(), Modifiers::control()), false, window, cx))
            })
            .unwrap()
    }

    /// Pushes the utterance as 100 ms microphone buffers.
    fn speak(f: &Fixture, cx: &mut TestAppContext) {
        let sink = f.mic.0.borrow().clone().expect("the microphone is open");
        for chunk in f.utterance.audio.chunks(1_600) {
            let samples = orbit_voice::audio::f32_to_i16(chunk);
            cx.update(|cx| sink.push(samples, cx));
        }
        cx.run_until_parked();
    }

    /// Runs the app until `done`, letting the worker thread answer.
    fn until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        cx.executor().allow_parking();
        for _ in 0..1_000 {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out");
    }

    fn text(f: &Fixture, cx: &mut TestAppContext) -> String {
        f.host.read_with(cx, |h, cx| h.input.read(cx).value().to_string())
    }

    #[gpui_kit::test]
    fn done_inserts_the_final_transcript_at_the_caret(cx: &mut TestAppContext) {
        let f = fixture(cx, "Please  now", 7);
        let dictation = start(&f, cx);
        let ended = Rc::new(RefCell::new(false));
        let flag = ended.clone();
        cx.update(|cx| cx.subscribe(&dictation, move |_, _: &DictationEvent, _| *flag.borrow_mut() = true).detach());
        until(cx, |cx| dictation.read_with(cx, |d, cx| d.state().read(cx).status() == SpeechStatus::Recording));
        speak(&f, cx);
        // A live preview arrives while recording.
        until(cx, |cx| dictation.read_with(cx, |d, _| !d.live().preview.stable.is_empty() || !d.live().preview.unstable.is_empty()));
        dictation.update(cx, |d, cx| d.stop(cx));
        assert!(f.mic.0.borrow().is_none(), "Done closes the microphone at once");
        until(cx, |_| *ended.borrow());
        assert_eq!(text(&f, cx), "Please fix the parser now");
    }

    #[gpui_kit::test]
    fn cancel_discards_and_leaves_the_text_alone(cx: &mut TestAppContext) {
        let f = fixture(cx, "Keep me", 7);
        let dictation = start(&f, cx);
        until(cx, |cx| dictation.read_with(cx, |d, cx| d.state().read(cx).status() == SpeechStatus::Recording));
        speak(&f, cx);
        dictation.update(cx, |d, cx| d.cancel(cx));
        assert_eq!(dictation.read_with(cx, |d, cx| d.state().read(cx).status()), SpeechStatus::Idle);
        cx.executor().allow_parking();
        std::thread::sleep(Duration::from_millis(100));
        cx.run_until_parked();
        assert_eq!(text(&f, cx), "Keep me");
    }

    #[gpui_kit::test]
    fn send_while_recording_stops_and_waits_for_the_transcript(cx: &mut TestAppContext) {
        let f = fixture(cx, "", 0);
        let dictation = start(&f, cx);
        until(cx, |cx| dictation.read_with(cx, |d, cx| d.state().read(cx).status() == SpeechStatus::Recording));
        speak(&f, cx);
        dictation.update(cx, |d, cx| d.send_when_done(cx));
        dictation.read_with(cx, |d, cx| {
            assert!(d.sends_when_done());
            assert_eq!(d.state().read(cx).status(), SpeechStatus::Stopping, "it waits for the final pass");
        });
        until(cx, |cx| !text(&f, cx).is_empty());
        assert_eq!(text(&f, cx), "Fix the parser");
    }
}
