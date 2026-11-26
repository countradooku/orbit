//! Debug builds only: a scripted dictation for screenshots and visual
//! checks, with no microphone and no model (`ORBIT_DEBUG_DICTATION`).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::speech::{AudioFormat, AudioInput, AudioSink, RecognitionSession, SpeechError, SpeechRecognizer, SpeechSink};
use gpui_kit::*;
use orbit_voice::stream::Preview;

use super::recognizer::Live;
use super::dictation::Sources;
use super::{Dictation, Purpose};

pub fn on_window_open(window: &mut Window, cx: &mut App) {
    let Ok(script) = std::env::var("ORBIT_DEBUG_DICTATION") else { return };
    if script == "setup" {
        super::setup::open(window, cx);
        return;
    }
    // Give the workspace a moment to show its composer.
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_secs(2)).await;
        let _ = handle.update(cx, |_, window, cx| start(script, window, cx));
    })
    .detach();
}

fn start(script: String, window: &mut Window, cx: &mut App) {
    let Some(target) = super::focused_target(window, cx) else {
        log::warn!("voice demo: no composer to dictate into");
        return;
    };
    let live = Rc::new(RefCell::new(Live::default()));
    let sources = Sources {
        recognizer: Rc::new(Scripted { words: script.split_whitespace().map(str::to_owned).collect(), live: live.clone() }),
        input: Rc::new(SyntheticVoice),
        live,
    };
    let release = (String::from("m"), Modifiers::control());
    let dictation = cx.new(|cx| Dictation::new(target, Purpose::Dictate, sources, release, false, window, cx));
    super::activate(dictation, cx);
}

/// Speech-like audio in real time: a pitch-varying harmonic tone with
/// syllable-rate loudness, 40 ms at a time.
struct SyntheticVoice;

impl AudioInput for SyntheticVoice {
    fn start(&self, _: AudioFormat, sink: AudioSink, cx: &mut App) -> Result<Subscription, SpeechError> {
        let task = cx.spawn(async move |cx| {
            let mut n = 0u64;
            loop {
                cx.background_executor().timer(Duration::from_millis(40)).await;
                let samples: Vec<i16> = (0..640)
                    .map(|_| {
                        n += 1;
                        let t = n as f32 / 16_000.0;
                        let pitch = 130.0 + 30.0 * (t * 3.1).sin();
                        let loud = 0.25 + 0.75 * (t * std::f32::consts::TAU * 2.2).sin().abs();
                        let v: f32 = (1..6).map(|k| (t * pitch * k as f32 * std::f32::consts::TAU).sin() / k as f32).sum();
                        (v * loud * 6_000.0) as i16
                    })
                    .collect();
                cx.update(|cx| sink.push(samples, cx));
            }
        });
        Ok(Subscription::new(move || drop(task)))
    }
}

/// Reveals the script a word every 350 ms; the last two words stay
/// tentative.
struct Scripted {
    words: Vec<String>,
    live: Rc<RefCell<Live>>,
}

impl SpeechRecognizer for Scripted {
    fn start(&self, sink: SpeechSink, cx: &mut App) -> Result<Box<dyn RecognitionSession>, SpeechError> {
        sink.ready(cx);
        let (words, live, tick_sink) = (self.words.clone(), self.live.clone(), sink.clone());
        let task = cx.spawn(async move |cx| {
            for shown in 1..=words.len() {
                cx.background_executor().timer(Duration::from_millis(350)).await;
                let stable = words[..shown.saturating_sub(2)].join(" ");
                let unstable = words[shown.saturating_sub(2)..shown].join(" ");
                live.borrow_mut().preview = Preview { stable: stable.clone(), unstable: unstable.clone() };
                cx.update(|cx| tick_sink.hypothesis(format!("{stable} {unstable}"), cx));
            }
            // Then Done, as the user would: the text lands in the composer.
            cx.background_executor().timer(Duration::from_secs(4)).await;
            cx.update(crate::voice::confirm);
        });
        Ok(Box::new(ScriptedSession { sink, text: self.words.join(" "), _task: task }))
    }
}

struct ScriptedSession {
    sink: SpeechSink,
    text: String,
    _task: Task<()>,
}

impl RecognitionSession for ScriptedSession {
    fn push_audio(&mut self, _: &[i16], _: &mut App) {}

    fn finish(&mut self, cx: &mut App) {
        self.sink.phrase(self.text.clone(), cx);
        self.sink.finish(cx);
    }
}
