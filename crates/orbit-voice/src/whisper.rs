//! [`WhisperEngine`]: the candle Whisper model, its tokenizer and the mel
//! front end behind the [`Engine`] seam.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use candle_core::Device;

use crate::audio::SAMPLES_PER_SECOND;
use crate::engine::{Engine, Quality, Request, Transcript};
use crate::mel::{HOP, Mel, N_FRAMES};
use crate::model::{self, Dims, Weights, Whisper};
use crate::tokenizer::Tokenizer;

/// Tokens of prompt Whisper accepts (half its text context, less one).
const MAX_PROMPT_TOKENS: usize = 223;
/// Tokens a 30-second window may produce.
const MAX_SAMPLE_TOKENS: usize = 224;
/// Audio context past the end of the speech in a preview window: a second.
const PREVIEW_MARGIN_FRAMES: usize = 100;
/// Below this average log-probability a decode is unreliable.
const LOGPROB_THRESHOLD: f32 = -1.0;
/// Above this probability of `<|nospeech|>` (with a low log-probability)
/// the window is silence and its text is dropped.
const NO_SPEECH_THRESHOLD: f32 = 0.6;
/// The fallback pass's sampling temperature.
const FALLBACK_TEMPERATURE: f32 = 0.4;

pub struct WhisperEngine {
    model: Whisper,
    tokenizer: Tokenizer,
    mel: Mel,
    multilingual: bool,
    tokens: Specials,
}

struct Specials {
    eot: u32,
    sot: u32,
    sot_prev: Option<u32>,
    no_timestamps: u32,
    no_speech: Option<u32>,
    transcribe: Option<u32>,
    translate: Option<u32>,
    /// Token ids of language codes, by code.
    languages: Vec<(&'static str, u32)>,
    /// `" "`: suppressed as the first token.
    blank: Option<u32>,
}

/// How one decoding pass went.
struct Pass {
    tokens: Vec<u32>,
    avg_logprob: f32,
    no_speech: f32,
    looped: bool,
}

impl WhisperEngine {
    /// Loads a GGUF model and its `tokenizer.json`.
    pub fn load(model: &Path, tokenizer: &Path, dims: Dims, device: &Device) -> Result<Self, String> {
        let json = std::fs::read_to_string(tokenizer).map_err(|e| format!("The tokenizer couldn’t be read: {e}"))?;
        let tokenizer = Tokenizer::from_json(&json)?;
        let weights = Weights::read(model, device).map_err(|e| format!("The model couldn’t be read: {e}"))?;
        let model = Whisper::load(weights, dims).map_err(|e| format!("The model couldn’t be loaded: {e}"))?;
        Self::new(model, tokenizer)
    }

    pub fn new(model: Whisper, tokenizer: Tokenizer) -> Result<Self, String> {
        let t = |name: &str| tokenizer.special(name);
        let languages = crate::languages::LANGUAGES
            .iter()
            .filter_map(|(code, _)| t(&format!("<|{code}|>")).map(|id| (*code, id)))
            .collect::<Vec<_>>();
        let tokens = Specials {
            eot: t("<|endoftext|>").ok_or("The tokenizer has no <|endoftext|>.")?,
            sot: t("<|startoftranscript|>").ok_or("The tokenizer has no <|startoftranscript|>.")?,
            sot_prev: t("<|startofprev|>"),
            no_timestamps: t("<|notimestamps|>").ok_or("The tokenizer has no <|notimestamps|>.")?,
            no_speech: t("<|nospeech|>").or_else(|| t("<|nocaptions|>")),
            transcribe: t("<|transcribe|>"),
            translate: t("<|translate|>"),
            blank: tokenizer.encode(" ").first().copied(),
            languages,
        };
        let multilingual = model.dims.n_vocab >= 51865;
        let mel = Mel::new(model.dims.n_mels);
        Ok(Self { model, tokenizer, mel, multilingual, tokens })
    }

    pub fn is_multilingual(&self) -> bool {
        self.multilingual
    }

    fn detect_language(&self, state_for: &candle_core::Tensor) -> candle_core::Result<Option<&'static str>> {
        if self.tokens.languages.is_empty() {
            return Ok(None);
        }
        let mut state = self.model.start(state_for)?;
        let logits = self.model.decode(&[self.tokens.sot], &mut state, &[0])?;
        let logits = model::row(&logits, 0)?;
        Ok(self
            .tokens
            .languages
            .iter()
            .max_by(|a, b| logits[a.1 as usize].total_cmp(&logits[b.1 as usize]))
            .map(|(code, _)| *code))
    }

    fn pass(
        &self,
        audio: &candle_core::Tensor,
        prefix: &[u32],
        sot_index: usize,
        max_tokens: usize,
        temperature: f32,
        cancel: &AtomicBool,
    ) -> candle_core::Result<Pass> {
        let mut state = self.model.start(audio)?;
        let logits = self.model.decode(prefix, &mut state, &[sot_index, prefix.len() - 1])?;
        let no_speech = match self.tokens.no_speech {
            Some(id) => model::log_softmax(&model::row(&logits, 0)?)[id as usize].exp(),
            None => 0.0,
        };
        let mut logits = model::row(&logits, 1)?;
        let mut tokens = Vec::new();
        let mut sum_logprob = 0.0f32;
        let mut looped = false;
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let budget = max_tokens.min(self.model.dims.n_text_ctx - prefix.len() - 1);
        for step in 0..budget {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            self.suppress(&mut logits, step == 0);
            let logprobs = model::log_softmax(&logits);
            let next = if temperature > 0.0 {
                sample(&logits, temperature, &mut rng)
            } else {
                model::argmax(&logits)
            } as u32;
            sum_logprob += logprobs[next as usize];
            if next == self.tokens.eot {
                break;
            }
            tokens.push(next);
            if let Some(cut) = repetition(&tokens) {
                tokens.truncate(cut);
                looped = true;
                break;
            }
            let out = self.model.decode(&[next], &mut state, &[0])?;
            logits = model::row(&out, 0)?;
        }
        let avg_logprob = sum_logprob / (tokens.len() + 1) as f32;
        Ok(Pass { tokens, avg_logprob, no_speech, looped })
    }

    fn suppress(&self, logits: &mut [f32], first: bool) {
        // Special tokens (timestamps included) never appear in text; the end
        // of text may, except as the very first token.
        for (id, l) in logits.iter_mut().enumerate() {
            let id = id as u32;
            if self.tokenizer.is_special(id) && (id != self.tokens.eot || first) {
                *l = f32::NEG_INFINITY;
            }
        }
        if first && let Some(blank) = self.tokens.blank {
            logits[blank as usize] = f32::NEG_INFINITY;
        }
    }

    fn run(&mut self, audio: &[f32], request: &Request<'_>, cancel: &AtomicBool) -> candle_core::Result<Transcript> {
        let audio = &audio[..audio.len().min(30 * SAMPLES_PER_SECOND)];
        let frames = match request.quality {
            Quality::Final => N_FRAMES,
            Quality::Preview => ((audio.len() / HOP + PREVIEW_MARGIN_FRAMES).div_ceil(2) * 2).min(N_FRAMES),
        };
        let mel = self.mel.compute(audio, frames);
        let features = self.model.encode(&mel, frames)?;

        let language = match (self.multilingual, request.language) {
            (false, _) => Some("en"),
            (true, Some(code)) => self.tokens.languages.iter().find(|(c, _)| *c == code).map(|(c, _)| *c),
            (true, None) => self.detect_language(&features)?,
        };

        let mut prefix = Vec::new();
        if let Some(prev) = self.tokens.sot_prev
            && !request.prompt.trim().is_empty()
        {
            let prompt = self.tokenizer.encode(&format!(" {}", request.prompt.trim()));
            prefix.push(prev);
            prefix.extend_from_slice(&prompt[prompt.len().saturating_sub(MAX_PROMPT_TOKENS)..]);
        }
        let sot_index = prefix.len();
        prefix.push(self.tokens.sot);
        if self.multilingual {
            if let Some(id) = language.and_then(|code| self.tokens.languages.iter().find(|(c, _)| *c == code)) {
                prefix.push(id.1);
            }
            let task = if request.translate { self.tokens.translate } else { self.tokens.transcribe };
            prefix.extend(task);
        }
        prefix.push(self.tokens.no_timestamps);

        // About five tokens a second of speech is generous; cap it so a
        // looping decode stops early.
        let seconds = audio.len() as f32 / SAMPLES_PER_SECOND as f32;
        let max_tokens = ((seconds * 6.0) as usize + 12).min(MAX_SAMPLE_TOKENS);
        let mut pass = self.pass(&features, &prefix, sot_index, max_tokens, 0.0, cancel)?;
        if request.quality == Quality::Final
            && !cancel.load(Ordering::Relaxed)
            && (pass.looped || pass.avg_logprob < LOGPROB_THRESHOLD)
            && pass.no_speech <= NO_SPEECH_THRESHOLD
        {
            let retry = self.pass(&features, &prefix, sot_index, max_tokens, FALLBACK_TEMPERATURE, cancel)?;
            if !retry.looped && (pass.looped || retry.avg_logprob > pass.avg_logprob) {
                pass = retry;
            }
        }
        let silent = pass.no_speech > NO_SPEECH_THRESHOLD && pass.avg_logprob < LOGPROB_THRESHOLD;
        let text = if silent { String::new() } else { self.tokenizer.decode(&pass.tokens) };
        Ok(Transcript { text, language: language.map(str::to_owned) })
    }
}

impl Engine for WhisperEngine {
    fn transcribe(&mut self, audio: &[f32], request: &Request<'_>, cancel: &AtomicBool) -> Result<Transcript, String> {
        self.run(audio, request, cancel).map_err(|e| format!("Transcription failed: {e}"))
    }
}

/// Loads an installed model on the dictation worker.
#[derive(Clone, Debug)]
pub struct WhisperLoader {
    pub model: std::path::PathBuf,
    pub tokenizer: std::path::PathBuf,
    pub dims: Dims,
    /// Use the GPU where this build has a backend for it (Metal on macOS,
    /// CUDA with the `cuda` feature); the CPU otherwise.
    pub gpu: bool,
}

impl WhisperLoader {
    fn device(&self) -> Device {
        if self.gpu {
            #[cfg(feature = "metal")]
            match Device::new_metal(0) {
                Ok(device) => return device,
                Err(e) => log::warn!("voice: Metal unavailable ({e}); using the CPU"),
            }
            #[cfg(feature = "cuda")]
            match Device::new_cuda(0) {
                Ok(device) => return device,
                Err(e) => log::warn!("voice: CUDA unavailable ({e}); using the CPU"),
            }
        }
        Device::Cpu
    }
}

/// Whether this build can run Whisper on a GPU.
pub const HAS_GPU_BACKEND: bool = cfg!(any(feature = "metal", feature = "cuda"));

impl crate::worker::Loader for WhisperLoader {
    fn key(&self) -> String {
        format!("{}|{}", self.model.display(), self.gpu)
    }

    fn load(&self) -> Result<Box<dyn Engine>, String> {
        Ok(Box::new(WhisperEngine::load(&self.model, &self.tokenizer, self.dims, &self.device())?))
    }
}

/// Samples a token from `logits` at `temperature` (xorshift, deterministic).
fn sample(logits: &[f32], temperature: f32, rng: &mut u64) -> usize {
    let max = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let weights: Vec<f32> = logits.iter().map(|l| ((l - max) / temperature).exp()).collect();
    let total: f32 = weights.iter().sum();
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    let mut target = (*rng >> 11) as f32 / (1u64 << 53) as f32 * total;
    for (i, w) in weights.iter().enumerate() {
        target -= w;
        if target <= 0.0 {
            return i;
        }
    }
    model::argmax(logits)
}

/// Where a decode that has started repeating itself should be cut: the end
/// of the first copy when the last 2 to 16 tokens occur three times in a row.
fn repetition(tokens: &[u32]) -> Option<usize> {
    let n = tokens.len();
    for k in 2..=16 {
        if n < 3 * k {
            break;
        }
        let a = &tokens[n - k..];
        if a == &tokens[n - 2 * k..n - k] && a == &tokens[n - 3 * k..n - 2 * k] {
            return Some(n - 2 * k);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::{TINY, random_weights};

    #[test]
    fn repetition_is_cut_after_the_first_copy() {
        assert_eq!(repetition(&[1, 2, 3, 4]), None);
        assert_eq!(repetition(&[9, 1, 2, 1, 2, 1, 2]), Some(3));
        assert_eq!(repetition(&[5, 5, 5, 5, 5, 5]), Some(2));
    }

    #[test]
    fn sampling_at_low_temperature_follows_the_argmax() {
        let mut rng = 7;
        let logits = [0.0, 10.0, 0.0];
        assert!((0..20).all(|_| sample(&logits, 0.1, &mut rng) == 1));
    }

    #[test]
    fn a_random_model_runs_end_to_end() {
        // Structure, not accuracy: random weights decode to some tokens and
        // stop within budget, for both qualities, without panicking.
        let tokenizer = crate::tokenizer::tests::fixture();
        let dims = Dims { n_vocab: tokenizer.vocab_size(), ..TINY };
        let model = Whisper::load(Weights::from_tensors(random_weights(dims), &Device::Cpu), dims).unwrap();
        let mut engine = WhisperEngine::new(model, tokenizer).unwrap();
        let audio = crate::engine::fake::voice(1.0);
        for quality in [Quality::Preview, Quality::Final] {
            let request = Request { language: None, translate: false, prompt: "Orbit, GPUI", quality };
            let out = engine.transcribe(&audio, &request, &AtomicBool::new(false)).unwrap();
            assert_eq!(out.language.as_deref(), Some("en"));
        }
        // Cancelled before it starts: nothing decoded.
        let request = Request { language: Some("en"), translate: false, prompt: "", quality: Quality::Final };
        let out = engine.transcribe(&audio, &request, &AtomicBool::new(true)).unwrap();
        assert_eq!(out.text, "");
    }
}
