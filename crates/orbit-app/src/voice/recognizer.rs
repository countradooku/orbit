//! The kit's `SpeechRecognizer` over Orbit's dictation worker.
//!
//! The kit's session pushes 16 kHz audio here on the UI thread; it is
//! converted and handed to the worker over a channel, so the UI thread never
//! decodes. Worker events come back on a task and are applied through the
//! kit's sink. Previews replace the hypothesis; the final, cleaned
//! transcript is committed as the session's one phrase, so the kit's
//! transcript is exactly what Whisper's final pass produced.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::speech::{RecognitionSession, SpeechError, SpeechRecognizer, SpeechSink};
use gpui_kit::*;
use orbit_voice::stream::{Preview, SessionConfig};
use orbit_voice::worker::{Event, Loader, SessionHandle, Worker};

/// What the pill shows beyond the kit's state.
#[derive(Default, Debug, Clone)]
pub struct Live {
    /// The model is being loaded.
    pub loading: bool,
    pub preview: Preview,
    pub heard_speech: bool,
    pub silence: Duration,
}

pub struct WhisperRecognizer {
    pub worker: Worker,
    pub loader: Arc<dyn Loader>,
    pub config: SessionConfig,
    pub live: Rc<RefCell<Live>>,
}

impl SpeechRecognizer for WhisperRecognizer {
    fn start(&self, sink: SpeechSink, cx: &mut App) -> Result<Box<dyn RecognitionSession>, SpeechError> {
        let (handle, events) = self.worker.start(self.loader.clone(), self.config.clone());
        let live = self.live.clone();
        *live.borrow_mut() = Live::default();
        let task = cx.spawn(async move |cx| {
            while let Ok(event) = events.recv().await {
                let done = matches!(event, Event::Final(_) | Event::Failed(_));
                cx.update(|cx| match event {
                    Event::Loading => {
                        live.borrow_mut().loading = true;
                        // Redraws the pill.
                        sink.hypothesis("", cx);
                    }
                    Event::Ready => {
                        live.borrow_mut().loading = false;
                        sink.ready(cx);
                    }
                    Event::Preview(preview) => {
                        let text = join(&preview);
                        live.borrow_mut().preview = preview;
                        sink.hypothesis(text, cx);
                    }
                    Event::Activity { heard_speech, silence } => {
                        let mut live = live.borrow_mut();
                        live.heard_speech = heard_speech;
                        live.silence = silence;
                    }
                    Event::Final(text) => {
                        live.borrow_mut().preview = Preview { stable: text.clone(), unstable: String::new() };
                        sink.phrase(text, cx);
                        sink.finish(cx);
                    }
                    Event::Failed(message) => sink.error(SpeechError::recognizer(std::io::Error::other(message)), cx),
                });
                if done {
                    break;
                }
            }
        });
        Ok(Box::new(Session { handle, samples: Vec::new(), _events: task }))
    }
}

fn join(preview: &Preview) -> String {
    match (preview.stable.is_empty(), preview.unstable.is_empty()) {
        (_, true) => preview.stable.clone(),
        (true, false) => preview.unstable.clone(),
        (false, false) => format!("{} {}", preview.stable, preview.unstable),
    }
}

struct Session {
    handle: SessionHandle,
    samples: Vec<f32>,
    _events: Task<()>,
}

impl RecognitionSession for Session {
    fn push_audio(&mut self, samples: &[i16], _: &mut App) {
        self.samples.clear();
        orbit_voice::audio::i16_to_f32(samples, &mut self.samples);
        self.handle.push(std::mem::take(&mut self.samples));
    }

    fn finish(&mut self, _: &mut App) {
        self.handle.finish();
    }
}

/// A recognizer for the microphone test in Settings: it only listens, so
/// the waveform shows the input level; nothing is transcribed.
pub struct LevelOnly;

impl SpeechRecognizer for LevelOnly {
    fn start(&self, sink: SpeechSink, cx: &mut App) -> Result<Box<dyn RecognitionSession>, SpeechError> {
        sink.ready(cx);
        Ok(Box::new(Listening(sink)))
    }
}

struct Listening(SpeechSink);

impl RecognitionSession for Listening {
    fn push_audio(&mut self, _: &[i16], _: &mut App) {}

    fn finish(&mut self, cx: &mut App) {
        self.0.finish(cx);
    }
}
