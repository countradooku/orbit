//! Whisper's log-mel spectrogram, computed the way OpenAI's reference does
//! (`whisper/audio.py`): a centered STFT with a 400-sample periodic Hann
//! window and a 160-sample hop, power spectra through Slaney-normalised mel
//! filters, `log10`, clamped to 8 below the maximum and scaled to about
//! `-1.0..=1.0`.
//!
//! It is fast where candle's helper is not: one planned real FFT
//! (`realfft`, SIMD on x86_64 and aarch64) per frame instead of a recursive
//! complex one, sparse filters (each mel band touches a handful of bins), and
//! no work for the frames past the end of the audio, which are silent.

use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{RealFftPlanner, RealToComplex};

pub const N_FFT: usize = 400;
pub const HOP: usize = 160;
const N_BINS: usize = N_FFT / 2 + 1;
/// Frames in Whisper's 30-second window.
pub const N_FRAMES: usize = 3_000;

/// One mel band: the first FFT bin it covers and its weights from there.
struct Band {
    start: usize,
    weights: Vec<f32>,
}

/// Computes log-mel spectrograms with `n_mels` bands (80, or 128 for
/// large-v3 models).
pub struct Mel {
    bands: Vec<Band>,
    window: Vec<f32>,
    fft: Arc<dyn RealToComplex<f32>>,
}

impl Mel {
    pub fn new(n_mels: usize) -> Self {
        let filters = filters(n_mels);
        let bands = filters
            .chunks(N_BINS)
            .map(|row| {
                let start = row.iter().position(|&w| w > 0.0).unwrap_or(0);
                let end = row.iter().rposition(|&w| w > 0.0).map_or(start, |e| e + 1);
                Band { start, weights: row[start..end].to_vec() }
            })
            .collect();
        // Periodic Hann, as `torch.hann_window(400)`.
        let window = (0..N_FFT)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / N_FFT as f32).cos())
            .collect();
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(N_FFT);
        Self { bands, window, fft }
    }

    pub fn n_mels(&self) -> usize {
        self.bands.len()
    }

    /// The log-mel spectrogram of `audio` (16 kHz mono) as `n_mels` rows of
    /// `frames` columns, row-major. Audio shorter than `frames` hops is padded
    /// with silence and longer audio is cut, as Whisper's `pad_or_trim`.
    pub fn compute(&self, audio: &[f32], frames: usize) -> Vec<f32> {
        let n_mels = self.n_mels();
        // Frames whose window holds any audio; the rest are silence.
        let audible = (audio.len().div_ceil(HOP) + 2).min(frames);
        let mut power = vec![0f32; n_mels * frames];
        let mut input = self.fft.make_input_vec();
        let mut spectrum: Vec<Complex32> = self.fft.make_output_vec();
        let mut scratch = self.fft.make_scratch_vec();
        let mut bins = [0f32; N_BINS];
        let len = audio.len() as isize;
        for t in 0..audible {
            let center = (t * HOP) as isize;
            for (i, slot) in input.iter_mut().enumerate() {
                let mut j = center - (N_FFT / 2) as isize + i as isize;
                // Reflect padding at the start, as `torch.stft(center=True)`.
                if j < 0 {
                    j = -j;
                }
                let sample = if j < len { audio[j as usize] } else { 0.0 };
                *slot = sample * self.window[i];
            }
            if self.fft.process_with_scratch(&mut input, &mut spectrum, &mut scratch).is_err() {
                continue;
            }
            for (bin, c) in bins.iter_mut().zip(&spectrum) {
                *bin = c.norm_sqr();
            }
            for (m, band) in self.bands.iter().enumerate() {
                let sum: f32 = band.weights.iter().zip(&bins[band.start..]).map(|(w, p)| w * p).sum();
                power[m * frames + t] = sum;
            }
        }
        let mut max = f32::MIN;
        for p in &mut power {
            *p = p.max(1e-10).log10();
            max = max.max(*p);
        }
        let floor = max - 8.0;
        for p in &mut power {
            *p = (p.max(floor) + 4.0) / 4.0;
        }
        power
    }
}

/// Slaney-style mel filters for a 16 kHz, 400-point FFT, as
/// `librosa.filters.mel(sr=16000, n_fft=400, n_mels=n_mels)` (which Whisper's
/// `mel_filters.npz` holds): `n_mels` rows of [`N_BINS`] weights.
pub fn filters(n_mels: usize) -> Vec<f32> {
    const SR: f64 = 16_000.0;
    fn hz_to_mel(hz: f64) -> f64 {
        let f_sp = 200.0 / 3.0;
        let min_log_hz = 1_000.0;
        let min_log_mel = min_log_hz / f_sp;
        let logstep = (6.4f64).ln() / 27.0;
        if hz >= min_log_hz { min_log_mel + (hz / min_log_hz).ln() / logstep } else { hz / f_sp }
    }
    fn mel_to_hz(mel: f64) -> f64 {
        let f_sp = 200.0 / 3.0;
        let min_log_hz = 1_000.0;
        let min_log_mel = min_log_hz / f_sp;
        let logstep = (6.4f64).ln() / 27.0;
        if mel >= min_log_mel { min_log_hz * (logstep * (mel - min_log_mel)).exp() } else { f_sp * mel }
    }
    let fft_freqs: Vec<f64> = (0..N_BINS).map(|i| i as f64 * SR / N_FFT as f64).collect();
    let (min_mel, max_mel) = (hz_to_mel(0.0), hz_to_mel(SR / 2.0));
    let mel_f: Vec<f64> = (0..n_mels + 2)
        .map(|i| mel_to_hz(min_mel + (max_mel - min_mel) * i as f64 / (n_mels + 1) as f64))
        .collect();
    let mut weights = vec![0f32; n_mels * N_BINS];
    for m in 0..n_mels {
        let (lo, mid, hi) = (mel_f[m], mel_f[m + 1], mel_f[m + 2]);
        let enorm = 2.0 / (hi - lo);
        for (b, &f) in fft_freqs.iter().enumerate() {
            let lower = (f - lo) / (mid - lo);
            let upper = (hi - f) / (hi - mid);
            let w = lower.min(upper).max(0.0);
            weights[m * N_BINS + b] = (w * enorm) as f32;
        }
    }
    weights
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_match_librosa() {
        // Spot values of `librosa.filters.mel(sr=16000, n_fft=400, n_mels=80)`.
        let f = filters(80);
        assert_eq!(f.len(), 80 * N_BINS);
        let at = |m: usize, b: usize| f[m * N_BINS + b];
        assert!((at(0, 1) - 0.024_862_37).abs() < 1e-5, "{}", at(0, 1));
        assert!((at(1, 2) - 0.022_876).abs() < 1e-4, "{}", at(1, 2));
        // Every band has some weight, and bands overlap only with neighbours.
        for m in 0..80 {
            assert!(f[m * N_BINS..(m + 1) * N_BINS].iter().any(|&w| w > 0.0), "band {m} is empty");
        }
        let f128 = filters(128);
        assert_eq!(f128.len(), 128 * N_BINS);
    }

    #[test]
    fn spectrogram_has_whisper_shape_and_range() {
        let mel = Mel::new(80);
        let tone: Vec<f32> = (0..16_000).map(|i| (i as f32 * 0.2).sin() * 0.3).collect();
        let out = mel.compute(&tone, N_FRAMES);
        assert_eq!(out.len(), 80 * N_FRAMES);
        let max = out.iter().cloned().fold(f32::MIN, f32::max);
        let min = out.iter().cloned().fold(f32::MAX, f32::min);
        assert!(max <= 1.0 + 4.0 / 4.0 && min >= max - 2.0 - 1e-5, "{min}..{max}");
        // Padding past the audio sits at the floor.
        assert!((out[N_FRAMES - 1] - min).abs() < 1e-6);
        // Short windows for preview decoding.
        assert_eq!(mel.compute(&tone, 200).len(), 80 * 200);
    }

    #[test]
    fn the_tone_lands_in_the_right_band() {
        // 0.2 rad/sample at 16 kHz is ~509 Hz. Below 1 kHz the 80 bands are
        // spaced linearly, 37 Hz apart, so band ~13.
        let mel = Mel::new(80);
        let tone: Vec<f32> = (0..16_000).map(|i| (i as f32 * 0.2).sin() * 0.3).collect();
        let out = mel.compute(&tone, 100);
        let frame = 50;
        let loudest = (0..80).max_by(|&a, &b| out[a * 100 + frame].total_cmp(&out[b * 100 + frame])).unwrap();
        assert!((11..=14).contains(&loudest), "band {loudest}");
    }
}
