//! Audio in the format Whisper consumes: 16 kHz mono `f32` in `-1.0..=1.0`.
//!
//! [`Resampler`] turns a device's own rate into that with rubato's FFT
//! resampler, which is band-limited (no aliasing from a 48 kHz microphone
//! folding into the speech band) and cheap: one forward and one inverse FFT
//! per 10 ms chunk.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler as _};

/// The rate Whisper and the voice activity detector run at.
pub const SAMPLE_RATE: u32 = 16_000;

/// Samples per second of audio at [`SAMPLE_RATE`].
pub const SAMPLES_PER_SECOND: usize = SAMPLE_RATE as usize;

/// Appends `samples` converted to `f32` in `-1.0..1.0` to `out`.
pub fn i16_to_f32(samples: &[i16], out: &mut Vec<f32>) {
    out.extend(samples.iter().map(|&s| s as f32 / 32_768.0));
}

/// Converts `f32` samples to `i16`, clamping out-of-range values.
pub fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples.iter().map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).collect()
}

/// Streams mono audio at any rate to [`SAMPLE_RATE`].
///
/// Feed it whatever chunk sizes the device delivers; it buffers to the
/// resampler's fixed 10 ms input and carries the remainder to the next push.
pub struct Resampler {
    inner: Option<Inner>,
}

struct Inner {
    fft: Fft<f32>,
    /// Input waiting for a whole chunk.
    pending: Vec<f32>,
    out: Vec<f32>,
    /// Output samples still to drop: the resampler's group delay, so output
    /// lines up with input.
    delay: usize,
}

impl Resampler {
    /// A resampler from `source_rate` to [`SAMPLE_RATE`]. A source already at
    /// that rate passes straight through.
    pub fn new(source_rate: u32) -> Self {
        let source_rate = source_rate.max(1);
        if source_rate == SAMPLE_RATE {
            return Self { inner: None };
        }
        let chunk = (source_rate as usize / 100).max(1);
        let inner = Fft::<f32>::new(source_rate as usize, SAMPLE_RATE as usize, chunk, 1, FixedSync::Input)
            .ok()
            .map(|fft| {
                let out = vec![0.0; fft.output_frames_max()];
                let delay = fft.output_delay();
                Inner { fft, pending: Vec::with_capacity(chunk * 2), out, delay }
            });
        if inner.is_none() {
            log::warn!("voice: no resampler for {source_rate} Hz; audio passes through unresampled");
        }
        Self { inner }
    }

    /// Resamples `input` and appends the result to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let Some(inner) = self.inner.as_mut() else {
            out.extend_from_slice(input);
            return;
        };
        inner.pending.extend_from_slice(input);
        let need = inner.fft.input_frames_next();
        let mut start = 0;
        while inner.pending.len() - start >= need {
            inner.process(start, need, None, out);
            start += need;
        }
        inner.pending.drain(..start);
    }

    /// Resamples whatever is buffered, padding the last chunk with silence.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        let Some(inner) = self.inner.as_mut() else { return };
        let need = inner.fft.input_frames_next();
        let have = inner.pending.len();
        if have == 0 {
            return;
        }
        inner.pending.resize(need, 0.0);
        inner.process(0, need, Some(have), out);
        inner.pending.clear();
    }
}

impl Inner {
    fn process(&mut self, start: usize, len: usize, partial: Option<usize>, out: &mut Vec<f32>) {
        let Ok(input) = InterleavedSlice::new(&self.pending[start..start + len], 1, len) else { return };
        let frames = self.out.len();
        let Ok(mut output) = InterleavedSlice::new_mut(&mut self.out[..], 1, frames) else { return };
        let indexing = partial.map(|partial_len| rubato::Indexing { partial_len: Some(partial_len), ..Default::default() });
        match self.fft.process_into_buffer(&input, &mut output, indexing.as_ref()) {
            Ok((_, written)) => {
                let skip = self.delay.min(written);
                self.delay -= skip;
                out.extend_from_slice(&self.out[skip..written]);
            }
            Err(err) => log::warn!("voice: resampling failed: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f32, seconds: f32) -> Vec<f32> {
        let n = (rate as f32 * seconds) as usize;
        (0..n).map(|i| (std::f32::consts::TAU * freq * i as f32 / rate as f32).sin() * 0.5).collect()
    }

    /// The strength of `freq` in `signal` at 16 kHz (one DFT bin).
    fn power_at(signal: &[f32], freq: f32) -> f32 {
        let (mut re, mut im) = (0.0f32, 0.0f32);
        for (i, s) in signal.iter().enumerate() {
            let phase = std::f32::consts::TAU * freq * i as f32 / SAMPLE_RATE as f32;
            re += s * phase.cos();
            im += s * phase.sin();
        }
        (re * re + im * im).sqrt() / signal.len() as f32
    }

    #[test]
    fn sixteen_khz_passes_through() {
        let mut r = Resampler::new(16_000);
        let mut out = Vec::new();
        r.push(&[0.1, 0.2, 0.3], &mut out);
        r.flush(&mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn downsampling_keeps_length_and_tone_whatever_the_chunking() {
        for rate in [48_000u32, 44_100, 22_050, 8_000] {
            let input = sine(rate, 440.0, 1.0);
            let mut r = Resampler::new(rate);
            let mut out = Vec::new();
            // Uneven device buffers.
            for chunk in input.chunks(rate as usize / 97) {
                r.push(chunk, &mut out);
            }
            r.flush(&mut out);
            let expected = SAMPLES_PER_SECOND as isize;
            assert!((out.len() as isize - expected).abs() <= 400, "{rate} Hz gave {} samples", out.len());
            let steady = &out[2000..14000];
            assert!(power_at(steady, 440.0) > 0.2, "{rate} Hz lost the tone");
        }
    }

    #[test]
    fn tones_above_the_new_nyquist_are_filtered_not_aliased() {
        // 12 kHz at 48 kHz would alias to 4 kHz at 16 kHz without filtering.
        let input = sine(48_000, 12_000.0, 1.0);
        let mut r = Resampler::new(48_000);
        let mut out = Vec::new();
        r.push(&input, &mut out);
        r.flush(&mut out);
        let steady = &out[2000..14000];
        assert!(power_at(steady, 4_000.0) < 0.005, "aliasing: {}", power_at(steady, 4_000.0));
    }

    #[test]
    fn sample_conversion_round_trips() {
        let mut f = Vec::new();
        i16_to_f32(&[0, 16_384, -32_768, 32_767], &mut f);
        assert_eq!(f[0], 0.0);
        assert!((f[1] - 0.5).abs() < 1e-6);
        assert_eq!(f[2], -1.0);
        assert_eq!(f32_to_i16(&[2.0, -2.0, 0.0]), vec![i16::MAX, -i16::MAX, 0]);
    }
}
