//! The speech engine seam: what a dictation session asks of a recognizer.
//!
//! [`crate::whisper::WhisperEngine`] is the real one; tests use a fake that
//! returns scripted text, so the whole pipeline runs without a model.

use std::sync::atomic::AtomicBool;

/// How much effort a decode spends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    /// A live preview while the user speaks: a shortened audio context and a
    /// single greedy pass, for latency.
    Preview,
    /// Text the user keeps: the full 30-second context and a fallback pass
    /// when the first one looks unreliable.
    Final,
}

/// One decode of up to 30 seconds of audio.
#[derive(Clone, Debug)]
pub struct Request<'a> {
    /// A Whisper language code, or `None` to detect it.
    pub language: Option<&'a str>,
    /// Translate to English instead of transcribing.
    pub translate: bool,
    /// Text that conditions the decoder: the vocabulary prompt and the
    /// transcript of the audio before this window.
    pub prompt: &'a str,
    pub quality: Quality,
}

/// The result of one decode.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Transcript {
    pub text: String,
    /// The detected (or requested) language code.
    pub language: Option<String>,
}

/// A loaded recognizer. Runs on the dictation worker's thread.
pub trait Engine: Send {
    /// Transcribes `audio` (16 kHz mono, at most 30 seconds). Returns early
    /// with what it has when `cancel` is set.
    fn transcribe(&mut self, audio: &[f32], request: &Request<'_>, cancel: &AtomicBool) -> Result<Transcript, String>;
}

/// Test support: a scripted engine and synthetic speech, so the dictation
/// pipeline runs end to end without a model.
#[cfg(any(test, feature = "test-support"))]
pub mod fake {
    use super::*;
    use crate::audio::SAMPLES_PER_SECOND;
    use std::sync::{Arc, Mutex};

    /// A recording made of words of synthetic speech separated by pauses,
    /// with a faint unique dither so any stretch of it can be located.
    pub struct Utterance {
        pub audio: Arc<Vec<f32>>,
        /// Each word and the sample range it occupies.
        pub words: Vec<(String, std::ops::Range<usize>)>,
    }

    impl Utterance {
        /// `script` is words and pauses: `"hello world | how are you"` puts
        /// a long pause at `|`. `lead` and `tail` are seconds of silence.
        pub fn new(script: &str, lead: f32, tail: f32) -> Self {
            let mut audio = silence(lead);
            let mut words = Vec::new();
            for token in script.split_whitespace() {
                if token == "|" {
                    audio.extend(silence(1.0));
                    continue;
                }
                let start = audio.len();
                audio.extend(voice(0.45));
                words.push((token.to_owned(), start..audio.len()));
                audio.extend(silence(0.12));
            }
            audio.extend(silence(tail));
            let mut seed = 0x2545_f491_4f6c_dd1du64;
            for s in &mut audio {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *s += ((seed >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 2e-4;
            }
            Self { audio: Arc::new(audio), words }
        }

        pub fn text(&self) -> String {
            self.words.iter().map(|w| w.0.as_str()).collect::<Vec<_>>().join(" ")
        }
    }

    /// Speech-like audio: a voiced, pitch-varying harmonic signal with
    /// syllable-rate amplitude modulation, which the VAD scores as voice.
    pub fn voice(seconds: f32) -> Vec<f32> {
        let n = (SAMPLES_PER_SECOND as f32 * seconds) as usize;
        let mut phase = 0.0f32;
        (0..n)
            .map(|i| {
                let t = i as f32 / SAMPLES_PER_SECOND as f32;
                let pitch = 130.0 + 30.0 * (t * 3.1).sin();
                phase += std::f32::consts::TAU * pitch / SAMPLES_PER_SECOND as f32;
                let syllables = 0.55 + 0.45 * (t * std::f32::consts::TAU * 4.0).sin().abs();
                let harmonics: f32 = [1.0f32, 0.6, 0.45, 0.3, 0.25, 0.2, 0.15, 0.1]
                    .iter()
                    .enumerate()
                    .map(|(k, a)| a * (phase * (k + 1) as f32).sin())
                    .sum();
                harmonics * 0.18 * syllables
            })
            .collect()
    }

    pub fn silence(seconds: f32) -> Vec<f32> {
        vec![0.0; (SAMPLES_PER_SECOND as f32 * seconds) as usize]
    }

    /// What a fake engine was asked.
    #[derive(Default, Debug)]
    pub struct Calls {
        /// Samples, quality and prompt of each decode.
        pub requests: Vec<(usize, Quality, String)>,
    }

    /// Returns the words of an [`Utterance`] whose middle lies in the audio
    /// it is given.
    pub struct FakeEngine {
        utterance: Arc<Vec<f32>>,
        words: Vec<(String, std::ops::Range<usize>)>,
        pub calls: Arc<Mutex<Calls>>,
        /// Time each decode takes, to exercise the worker under load.
        pub delay: std::time::Duration,
    }

    impl FakeEngine {
        pub fn new(utterance: &Utterance) -> Self {
            Self {
                utterance: utterance.audio.clone(),
                words: utterance.words.clone(),
                calls: Arc::default(),
                delay: std::time::Duration::ZERO,
            }
        }

        fn locate(&self, audio: &[f32]) -> Option<usize> {
            let probe = audio.get(..64)?;
            self.utterance.windows(64).position(|w| w == probe)
        }
    }

    impl Engine for FakeEngine {
        fn transcribe(&mut self, audio: &[f32], request: &Request<'_>, _: &AtomicBool) -> Result<Transcript, String> {
            self.calls.lock().unwrap().requests.push((audio.len(), request.quality, request.prompt.to_owned()));
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            let Some(start) = self.locate(audio) else {
                return Ok(Transcript::default());
            };
            let range = start..start + audio.len();
            let words: Vec<&str> = self
                .words
                .iter()
                .filter(|(_, r)| range.contains(&((r.start + r.end) / 2)))
                .map(|(w, _)| w.as_str())
                .collect();
            let text = if words.is_empty() { String::new() } else { format!(" {}", words.join(" ")) };
            Ok(Transcript { text, language: Some("en".into()) })
        }
    }
}
