//! Helpers shared by the benchmark and the real-model test: reading WAV
//! files and scoring a transcript against its script.

use crate::audio::{Resampler, SAMPLE_RATE};

/// Word error rate of `hypothesis` against `reference`, ignoring case and
/// punctuation: (substitutions + insertions + deletions) / reference words.
pub fn word_error_rate(reference: &str, hypothesis: &str) -> f32 {
    let words = |s: &str| -> Vec<String> {
        s.split_whitespace()
            .map(|w| w.chars().filter(|c| c.is_alphanumeric() || *c == '\'').collect::<String>().to_lowercase())
            .filter(|w| !w.is_empty())
            .collect()
    };
    let (r, h) = (words(reference), words(hypothesis));
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }
    let mut prev: Vec<usize> = (0..=h.len()).collect();
    for i in 1..=r.len() {
        let mut cur = vec![i; h.len() + 1];
        for j in 1..=h.len() {
            let cost = usize::from(r[i - 1] != h[j - 1]);
            cur[j] = (prev[j - 1] + cost).min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        prev = cur;
    }
    prev[h.len()] as f32 / r.len() as f32
}

/// Mixes interleaved `channels` down to mono and resamples to 16 kHz.
pub fn to_mono_16k(samples: &[f32], channels: usize, rate: u32) -> Vec<f32> {
    let channels = channels.max(1);
    let mono: Vec<f32> = samples.chunks(channels).map(|f| f.iter().sum::<f32>() / f.len() as f32).collect();
    if rate == SAMPLE_RATE {
        return mono;
    }
    let mut out = Vec::new();
    let mut resampler = Resampler::new(rate);
    resampler.push(&mono, &mut out);
    resampler.flush(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wer_counts_edits_per_reference_word() {
        assert_eq!(word_error_rate("Hello, world.", "hello world"), 0.0);
        assert_eq!(word_error_rate("a b c d", "a x c"), 0.5);
        assert_eq!(word_error_rate("", ""), 0.0);
    }

    #[test]
    fn stereo_mixes_down() {
        assert_eq!(to_mono_16k(&[1.0, 0.0, 0.5, 0.5], 2, 16_000), vec![0.5, 0.5]);
    }
}
