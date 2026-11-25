//! Voice activity detection: where speech is in a recording.
//!
//! Built on `earshot`, a small neural VAD in pure Rust (40 KB of weights
//! compiled in, no model file to download) that scores 16 ms frames at
//! 16 kHz in microseconds. Scores pass through hysteresis, and blips shorter
//! than [`MIN_SPEECH_FRAMES`] (a click, a keyboard tap) do not count.
//!
//! The dictation pipeline uses it to trim leading and trailing silence before
//! decoding (Whisper hallucinates text in silence), to skip decoding windows
//! with no speech at all, to cut long recordings at pauses, and to stop after
//! a silence timeout.

use std::ops::Range;
use std::time::Duration;

use crate::audio::SAMPLES_PER_SECOND;

/// Samples per VAD frame: 16 ms at 16 kHz.
pub const FRAME: usize = 256;

/// A frame scoring above this starts speech.
const ENTER: f32 = 0.5;
/// Speech continues until a frame scores below this.
const EXIT: f32 = 0.3;
/// Speech shorter than this (in frames, 80 ms) is noise.
pub const MIN_SPEECH_FRAMES: usize = 5;
/// Silence shorter than this (in frames, 250 ms) inside speech is a gap
/// between words, not a pause.
const MIN_PAUSE_FRAMES: usize = 16;

/// Tracks speech in a stream of 16 kHz audio.
pub struct Vad {
    detector: Box<earshot::Detector>,
    pending: Vec<f32>,
    /// One flag per frame: speech after hysteresis.
    speech: Vec<bool>,
    in_speech: bool,
    /// Speech regions (in frames) of at least [`MIN_SPEECH_FRAMES`], merged
    /// across gaps shorter than [`MIN_PAUSE_FRAMES`].
    regions: Vec<Range<usize>>,
    /// Start of the run of speech frames being measured.
    run_start: Option<usize>,
}

impl Default for Vad {
    fn default() -> Self {
        Self::new()
    }
}

impl Vad {
    pub fn new() -> Self {
        Self {
            detector: earshot::Detector::default_boxed(),
            pending: Vec::with_capacity(FRAME),
            speech: Vec::new(),
            in_speech: false,
            regions: Vec::new(),
            run_start: None,
        }
    }

    /// Scores `samples` (16 kHz, `-1.0..=1.0`), carrying partial frames over.
    pub fn push(&mut self, samples: &[f32]) {
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (FRAME - self.pending.len()).min(rest.len());
            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.pending.len() == FRAME {
                let frame: [f32; FRAME] = std::array::from_fn(|i| self.pending[i].clamp(-1.0, 1.0));
                self.pending.clear();
                let score = self.detector.predict_f32(&frame);
                self.on_score(score);
            }
        }
    }

    fn on_score(&mut self, score: f32) {
        self.in_speech = if self.in_speech { score >= EXIT } else { score > ENTER };
        let ix = self.speech.len();
        self.speech.push(self.in_speech);
        match (self.in_speech, self.run_start) {
            (true, None) => self.run_start = Some(ix),
            (false, Some(start)) => {
                self.run_start = None;
                self.close_run(start..ix);
            }
            _ => {}
        }
    }

    fn close_run(&mut self, run: Range<usize>) {
        if run.len() < MIN_SPEECH_FRAMES {
            return;
        }
        match self.regions.last_mut() {
            Some(last) if run.start - last.end < MIN_PAUSE_FRAMES => last.end = run.end,
            _ => self.regions.push(run),
        }
    }

    /// Speech regions in frames, including a run still open at the end if it
    /// is long enough.
    fn regions(&self) -> Vec<Range<usize>> {
        let mut regions = self.regions.clone();
        if let Some(start) = self.run_start {
            let run = start..self.speech.len();
            if run.len() >= MIN_SPEECH_FRAMES {
                match regions.last_mut() {
                    Some(last) if run.start - last.end < MIN_PAUSE_FRAMES => last.end = run.end,
                    _ => regions.push(run),
                }
            }
        }
        regions
    }

    /// Samples scored so far (whole frames).
    pub fn samples(&self) -> usize {
        self.speech.len() * FRAME
    }

    /// Whether any speech has been heard.
    pub fn has_speech(&self) -> bool {
        !self.regions().is_empty()
    }

    /// Whether speech is heard within `range` (in samples).
    pub fn has_speech_in(&self, range: Range<usize>) -> bool {
        self.speech_span(range, 0).is_some()
    }

    /// The span of speech within `range` (in samples), widened by `pad`
    /// samples on each side and clipped to `range`; `None` without speech.
    pub fn speech_span(&self, range: Range<usize>, pad: usize) -> Option<Range<usize>> {
        let regions = self.regions();
        let inside = regions
            .iter()
            .map(|r| r.start * FRAME..r.end * FRAME)
            .filter(|r| r.start < range.end && r.end > range.start)
            .collect::<Vec<_>>();
        let first = inside.first()?;
        let last = inside.last()?;
        let start = first.start.saturating_sub(pad).max(range.start);
        // Speech still running at the end of what has been scored reaches the
        // end of the range: its last frame may not be scored yet.
        let end = if last.end >= self.samples() { range.end } else { (last.end + pad).min(range.end) };
        Some(start..end)
    }

    /// How long it has been quiet: since the end of the last speech, or since
    /// the start if there has been none.
    pub fn trailing_silence(&self) -> Duration {
        let frames = self.speech.len();
        let quiet_frames = match self.regions().last() {
            Some(last) => frames - last.end,
            None => frames,
        };
        Duration::from_secs_f64((quiet_frames * FRAME) as f64 / SAMPLES_PER_SECOND as f64)
    }

    /// The best place to cut audio within `range` (in samples): the middle of
    /// the longest pause, or the end of the range if there is no pause.
    pub fn best_cut(&self, range: Range<usize>) -> usize {
        let from = range.start.div_ceil(FRAME);
        let to = (range.end / FRAME).min(self.speech.len());
        let regions = self.regions();
        let mut best: Option<Range<usize>> = None;
        let mut gap_start = from;
        // A pause shorter than a gap between words is no place to cut.
        let mut consider = |gap: Range<usize>| {
            if gap.len() >= MIN_PAUSE_FRAMES / 2 && best.as_ref().is_none_or(|b| gap.len() > b.len()) {
                best = Some(gap);
            }
        };
        for region in regions.iter().filter(|r| r.end > from && r.start < to) {
            consider(gap_start..region.start.max(from));
            gap_start = region.end.min(to);
        }
        consider(gap_start..to);
        match best {
            Some(gap) => ((gap.start + gap.end) / 2 * FRAME).clamp(range.start, range.end),
            None => range.end,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::fake::{silence, voice};

    fn secs(samples: usize) -> f32 {
        samples as f32 / SAMPLES_PER_SECOND as f32
    }

    #[test]
    fn silence_has_no_speech() {
        let mut vad = Vad::new();
        vad.push(&silence(2.0));
        assert!(!vad.has_speech());
        assert!(vad.trailing_silence() >= Duration::from_millis(1900));
        assert_eq!(vad.speech_span(0..32_000, 0), None);
    }

    #[test]
    fn speech_span_trims_leading_and_trailing_silence() {
        let mut vad = Vad::new();
        let audio = [silence(1.0), voice(2.0), silence(1.5)].concat();
        // Odd chunk sizes, as a device delivers.
        for chunk in audio.chunks(333) {
            vad.push(chunk);
        }
        assert!(vad.has_speech());
        let span = vad.speech_span(0..audio.len(), 0).unwrap();
        assert!((secs(span.start) - 1.0).abs() < 0.25, "starts at {}", secs(span.start));
        assert!((secs(span.end) - 3.0).abs() < 0.3, "ends at {}", secs(span.end));
        let padded = vad.speech_span(0..audio.len(), 4_800).unwrap();
        assert!(padded.start + 4_800 >= span.start && padded.start < span.start);
        let quiet = vad.trailing_silence().as_secs_f32();
        assert!((quiet - 1.5).abs() < 0.3, "quiet for {quiet}");
    }

    #[test]
    fn speech_running_at_the_end_reaches_the_end() {
        let mut vad = Vad::new();
        let audio = [silence(0.5), voice(1.0)].concat();
        vad.push(&audio);
        let span = vad.speech_span(0..audio.len(), 0).unwrap();
        assert_eq!(span.end, audio.len());
        assert!(vad.trailing_silence() < Duration::from_millis(100));
    }

    #[test]
    fn clicks_are_not_speech() {
        let mut vad = Vad::new();
        let mut audio = silence(1.0);
        for s in &mut audio[8_000..8_040] {
            *s = 0.9;
        }
        vad.push(&audio);
        assert!(!vad.has_speech());
    }

    #[test]
    fn cuts_land_in_the_longest_pause() {
        let mut vad = Vad::new();
        let audio = [voice(2.0), silence(0.4), voice(2.0), silence(1.2), voice(2.0)].concat();
        vad.push(&audio);
        let cut = secs(vad.best_cut(0..audio.len()));
        // The 1.2 s pause runs from 4.4 s to 5.6 s.
        assert!((4.5..5.5).contains(&cut), "cut at {cut}");
        // Without a pause in range, cut at its end.
        let range = 0..16_000;
        assert_eq!(vad.best_cut(range.clone()), range.end);
    }
}
