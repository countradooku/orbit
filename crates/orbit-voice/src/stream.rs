//! A dictation session: audio arrives while the user speaks; previews,
//! long-recording cuts and the final pass are scheduled from it.
//!
//! - **Preview.** Every [`PREVIEW_STEP`] of new audio, the open window (the
//!   audio since the last cut) is re-decoded at [`Quality::Preview`]. Words
//!   two consecutive previews agree on become stable (LocalAgreement-2) and
//!   never flicker back; the rest show as a tentative tail.
//! - **Cuts.** Whisper sees 30 seconds at a time. Once the open window passes
//!   [`CLOSE_AFTER`], it is cut in the longest pause after [`CUT_SEARCH`] and
//!   that part is decoded at full quality right away, while the user is
//!   still talking. So at release only the last window is left to decode,
//!   and release-to-text latency stays flat for a five-minute recording.
//! - **Final.** At release the open window gets its full-quality pass and
//!   the closed windows' text is joined to it.
//!
//! Every decode is trimmed to its speech (plus [`PAD`]) by the VAD, and
//! windows without speech are not decoded at all: Whisper invents text for
//! silence.

use std::ops::Range;
use std::time::Duration;

use crate::audio::SAMPLES_PER_SECOND;
use crate::engine::{Quality, Request, Transcript};
use crate::postprocess::{Cleanup, clean};
use crate::vad::Vad;

/// New audio between previews.
pub const PREVIEW_STEP: usize = SAMPLES_PER_SECOND * 7 / 10;
/// How long the open window may grow before it is cut.
pub const CLOSE_AFTER: usize = SAMPLES_PER_SECOND * 20;
/// Cuts land at least this far into the open window.
pub const CUT_SEARCH: usize = SAMPLES_PER_SECOND * 12;
/// Audio kept around speech when trimming.
pub const PAD: usize = SAMPLES_PER_SECOND / 4;
/// Previous transcript passed as context to the next window, in characters.
const CONTEXT_CHARS: usize = 400;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionConfig {
    /// A Whisper language code, or `None` to detect it once and keep it.
    pub language: Option<String>,
    pub translate: bool,
    /// The vocabulary prompt (see [`crate::prompt`]).
    pub prompt: String,
    /// Decode live previews while recording.
    pub preview: bool,
    pub cleanup: Cleanup,
}

/// A decode the session wants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    Preview(Range<usize>),
    /// Decode `range` at full quality and close it; the next window starts
    /// at its end.
    Close(Range<usize>),
    Final(Range<usize>),
}

impl Job {
    pub fn quality(&self) -> Quality {
        match self {
            Job::Preview(_) => Quality::Preview,
            Job::Close(_) | Job::Final(_) => Quality::Final,
        }
    }

    pub fn range(&self) -> Range<usize> {
        match self {
            Job::Preview(r) | Job::Close(r) | Job::Final(r) => r.clone(),
        }
    }
}

/// What the user sees while speaking.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preview {
    /// Text that will not change until the final pass, cleaned up.
    pub stable: String,
    /// The tentative tail.
    pub unstable: String,
}

/// One decode's input, borrowed from the session.
pub struct Decode<'a> {
    pub audio: &'a [f32],
    prompt: String,
    language: Option<&'a str>,
    translate: bool,
    quality: Quality,
}

impl Decode<'_> {
    pub fn request(&self) -> Request<'_> {
        Request { language: self.language, translate: self.translate, prompt: &self.prompt, quality: self.quality }
    }
}

pub struct Session {
    config: SessionConfig,
    audio: Vec<f32>,
    vad: Vad,
    /// Full-quality text of the windows already cut.
    closed: Vec<String>,
    open_start: usize,
    previewed_at: usize,
    hypothesis: Vec<String>,
    stable: Vec<String>,
    language: Option<String>,
    final_text: Option<String>,
}

impl Session {
    pub fn new(config: SessionConfig) -> Self {
        let language = config.language.clone();
        Self {
            config,
            audio: Vec::with_capacity(SAMPLES_PER_SECOND * 30),
            vad: Vad::new(),
            closed: Vec::new(),
            open_start: 0,
            previewed_at: 0,
            hypothesis: Vec::new(),
            stable: Vec::new(),
            language,
            final_text: None,
        }
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.audio.extend_from_slice(samples);
        self.vad.push(samples);
    }

    /// Seconds of audio so far.
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.audio.len() as f64 / SAMPLES_PER_SECOND as f64)
    }

    pub fn heard_speech(&self) -> bool {
        self.vad.has_speech()
    }

    /// Quiet time since the last speech (or since the start).
    pub fn silence(&self) -> Duration {
        self.vad.trailing_silence()
    }

    /// The next decode while recording, if one is due.
    pub fn next_job(&self) -> Option<Job> {
        let end = self.audio.len();
        if end - self.open_start >= CLOSE_AFTER {
            let cut = self.vad.best_cut(self.open_start + CUT_SEARCH..end);
            return Some(Job::Close(self.open_start..cut));
        }
        if self.config.preview
            && end - self.previewed_at >= PREVIEW_STEP
            && self.vad.has_speech_in(self.open_start..end)
        {
            return Some(Job::Preview(self.open_start..end));
        }
        None
    }

    /// The last decode once recording stopped; `None` when the open window
    /// holds no speech.
    pub fn final_job(&self) -> Option<Job> {
        let end = self.audio.len();
        if end - self.open_start >= CLOSE_AFTER {
            let cut = self.vad.best_cut(self.open_start + CUT_SEARCH..end);
            return Some(Job::Close(self.open_start..cut));
        }
        self.vad.has_speech_in(self.open_start..end).then_some(Job::Final(self.open_start..end))
    }

    /// The audio and request for `job`: its speech, trimmed, and the context
    /// before it. `None` when there is no speech to decode.
    pub fn decode(&self, job: &Job) -> Option<Decode<'_>> {
        let span = self.vad.speech_span(job.range(), PAD)?;
        let context = self.context();
        let prompt = match (self.config.prompt.trim(), context.trim()) {
            ("", c) => c.to_owned(),
            (p, "") => p.to_owned(),
            (p, c) => format!("{p} {c}"),
        };
        Some(Decode {
            audio: &self.audio[span],
            prompt,
            language: self.language.as_deref(),
            translate: self.config.translate,
            quality: job.quality(),
        })
    }

    fn context(&self) -> String {
        let joined = join(&self.closed);
        let start = joined.char_indices().rev().nth(CONTEXT_CHARS).map_or(0, |(i, _)| i);
        joined[start..].to_owned()
    }

    /// Takes in the result of `job` (empty for a job without speech) and
    /// returns the preview to show.
    pub fn apply(&mut self, job: &Job, transcript: Transcript) -> Preview {
        if self.language.is_none() {
            self.language = transcript.language.clone();
        }
        // Windows are kept raw and cleaned once joined: a cut mid-sentence
        // must not capitalise the word after it.
        let text = transcript.text.trim().to_owned();
        match job {
            Job::Preview(range) => {
                self.previewed_at = range.end;
                let words: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
                if words.starts_with(&self.stable) {
                    let s = self.stable.len();
                    let agreed = self.hypothesis.iter().skip(s).zip(&words[s..]).take_while(|(a, b)| a == b).count();
                    self.stable.extend_from_slice(&words[s..s + agreed]);
                }
                self.hypothesis = words;
            }
            Job::Close(range) => {
                self.closed.push(text);
                self.open_start = range.end;
                self.previewed_at = self.previewed_at.max(range.end);
                self.hypothesis.clear();
                self.stable.clear();
            }
            Job::Final(_) => {
                let mut parts = self.closed.clone();
                parts.push(text);
                self.final_text = Some(clean(&join(&parts), &self.config.cleanup));
            }
        }
        self.preview()
    }

    pub fn preview(&self) -> Preview {
        let mut stable = self.closed.clone();
        stable.push(self.stable.join(" "));
        let tail = self.hypothesis.get(self.stable.len()..).unwrap_or_default().join(" ");
        let stable = clean(&join(&stable), &self.config.cleanup);
        // The tail continues the stable text: tidy it without capitalising.
        let tail_cleanup = Cleanup { tidy: stable.is_empty() && self.config.cleanup.tidy, ..self.config.cleanup.clone() };
        Preview { stable, unstable: clean(&tail, &tail_cleanup) }
    }

    /// The transcript once the final job ran (or of the closed windows when
    /// the open one held no speech).
    pub fn final_text(&self) -> String {
        match &self.final_text {
            Some(text) => text.clone(),
            None => clean(&join(&self.closed), &self.config.cleanup),
        }
    }
}

fn join(parts: &[String]) -> String {
    parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::engine::fake::{FakeEngine, Utterance};
    use std::sync::atomic::AtomicBool;

    fn run(session: &mut Session, engine: &mut FakeEngine, job: &Job) -> Preview {
        let transcript = match session.decode(job) {
            Some(decode) => engine.transcribe(decode.audio, &decode.request(), &AtomicBool::new(false)).unwrap(),
            None => Transcript::default(),
        };
        session.apply(job, transcript)
    }

    /// Feeds `utterance` in 100 ms pushes, running every job as it falls due.
    fn record(utterance: &Utterance, config: SessionConfig) -> (Session, FakeEngine, Vec<Preview>) {
        let mut engine = FakeEngine::new(utterance);
        let mut session = Session::new(config);
        let mut previews = Vec::new();
        for chunk in utterance.audio.chunks(1_600) {
            session.push(chunk);
            while let Some(job) = session.next_job() {
                previews.push(run(&mut session, &mut engine, &job));
            }
        }
        while let Some(job) = session.final_job() {
            let done = matches!(job, Job::Final(_));
            run(&mut session, &mut engine, &job);
            if done {
                break;
            }
        }
        (session, engine, previews)
    }

    fn config() -> SessionConfig {
        SessionConfig { preview: true, prompt: "Orbit".into(), ..Default::default() }
    }

    #[test]
    fn a_short_utterance_previews_then_finalizes_trimmed() {
        let utterance = Utterance::new("please refactor the composer", 1.0, 1.5);
        let (session, engine, previews) = record(&utterance, config());
        assert_eq!(session.final_text(), "Please refactor the composer");
        assert!(previews.len() >= 2, "{previews:?}");
        // Previews grow and their stable part never shrinks.
        let mut last_stable = 0;
        for p in &previews {
            let words = p.stable.split_whitespace().count();
            assert!(words >= last_stable, "{previews:?}");
            last_stable = words;
        }
        let calls = engine.calls.lock().unwrap();
        let (samples, quality, prompt) = calls.requests.last().unwrap();
        assert_eq!(*quality, Quality::Final);
        assert_eq!(prompt, "Orbit");
        // 2.3 s of speech plus padding, not the 4.8 s recorded.
        let seconds = *samples as f32 / SAMPLES_PER_SECOND as f32;
        assert!((2.2..3.6).contains(&seconds), "decoded {seconds} s");
    }

    #[test]
    fn previews_can_be_turned_off() {
        let utterance = Utterance::new("hello there", 0.5, 0.5);
        let (session, engine, previews) =
            record(&utterance, SessionConfig { preview: false, ..config() });
        assert!(previews.is_empty());
        assert_eq!(engine.calls.lock().unwrap().requests.len(), 1);
        assert_eq!(session.final_text(), "Hello there");
    }

    #[test]
    fn silence_is_never_decoded() {
        let utterance = Utterance::new("", 3.0, 0.0);
        let (session, engine, _) = record(&utterance, config());
        assert!(engine.calls.lock().unwrap().requests.is_empty());
        assert_eq!(session.final_text(), "");
        assert!(!session.heard_speech());
        assert!(session.silence() >= Duration::from_millis(2_900));
    }

    #[test]
    fn long_recordings_are_cut_in_pauses_and_decoded_while_recording() {
        // About 46 seconds: five phrases separated by pauses.
        let phrase = "one two three four five six seven eight nine ten eleven twelve |";
        let script = std::iter::repeat_n(phrase, 5).collect::<Vec<_>>().join(" ");
        let utterance = Utterance::new(&script, 0.5, 1.0);
        let (session, engine, previews) = record(&utterance, config());
        // Every word once, in order: no word lost or doubled at a cut.
        let expected = clean(&utterance.text(), &Cleanup::default());
        assert_eq!(session.final_text(), expected);
        let calls = engine.calls.lock().unwrap();
        let closes = calls.requests.iter().filter(|r| r.1 == Quality::Final).count();
        assert!(closes >= 3, "{closes} full-quality decodes");
        // Later windows are conditioned on the text before them.
        let last = calls.requests.last().unwrap();
        assert!(last.2.starts_with("Orbit ") && last.2.contains("twelve"), "{}", last.2);
        // No decode exceeds Whisper's window.
        assert!(calls.requests.iter().all(|r| r.0 <= 30 * SAMPLES_PER_SECOND));
        assert!(!previews.is_empty());
    }

    #[test]
    fn local_agreement_confirms_words_two_previews_share() {
        let mut session = Session::new(config());
        session.push(&crate::engine::fake::voice(1.0));
        let job = Job::Preview(0..16_000);
        let say = |text: &str| Transcript { text: text.into(), language: None };
        let p = session.apply(&job, say(" Please re"));
        assert_eq!((p.stable.as_str(), p.unstable.as_str()), ("", "Please re"));
        let p = session.apply(&job, say(" Please refactor the"));
        assert_eq!((p.stable.as_str(), p.unstable.as_str()), ("Please", "refactor the"));
        let p = session.apply(&job, say(" Please refactor the composer"));
        assert_eq!((p.stable.as_str(), p.unstable.as_str()), ("Please refactor the", "composer"));
        // A preview that disagrees with stable words leaves them in place.
        let p = session.apply(&job, say(" Pleased to"));
        assert_eq!(p.stable, "Please refactor the");
    }
}
