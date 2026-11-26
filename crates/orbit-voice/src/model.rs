//! The Whisper network on candle, loaded from GGUF.
//!
//! Written here rather than taken from `candle-transformers` because that
//! model's decoder keeps no self-attention cache: every step re-runs the
//! whole token sequence, so decoding is quadratic. Here each layer caches its
//! keys and values, the cross-attention keys and values are computed once
//! per audio window, and only the positions whose logits are needed go
//! through the vocabulary projection. It also keeps the large
//! `candle-transformers` crate (every model candle ships) out of the build.
//!
//! Weights are whatever the GGUF holds per tensor: Orbit's installer writes
//! the large matrices as q8_0 (`QMatMul` uses AVX2/FMA on x86_64, picked at
//! run time, and NEON on aarch64) and small tensors as f32.

use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::Path;

use candle_core::quantized::{QMatMul, QTensor, gguf_file};
use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{Conv1d, Conv1dConfig, LayerNorm};

pub type Result<T> = candle_core::Result<T>;

/// Bytes of attention weights computed at once.
const ATTENTION_BUDGET: usize = 20 << 20;

/// The shape of a Whisper model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dims {
    pub n_mels: usize,
    pub n_audio_ctx: usize,
    pub n_audio_state: usize,
    pub n_audio_head: usize,
    pub n_audio_layer: usize,
    pub n_vocab: usize,
    pub n_text_ctx: usize,
    pub n_text_state: usize,
    pub n_text_head: usize,
    pub n_text_layer: usize,
}

impl Dims {
    /// Dims of the standard sizes; `vocab` is 51864 for English-only
    /// models, 51865 for multilingual ones and 51866 for large-v3.
    pub const fn new(n_mels: usize, state: usize, heads: usize, enc: usize, dec: usize, vocab: usize) -> Self {
        Self {
            n_mels,
            n_audio_ctx: 1500,
            n_audio_state: state,
            n_audio_head: heads,
            n_audio_layer: enc,
            n_vocab: vocab,
            n_text_ctx: 448,
            n_text_state: state,
            n_text_head: heads,
            n_text_layer: dec,
        }
    }
}

/// The tensors of a GGUF file, read once.
pub struct Weights {
    tensors: HashMap<String, QTensor>,
    device: Device,
}

impl Weights {
    pub fn read(path: &Path, device: &Device) -> Result<Self> {
        let mut file = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(path)?);
        Self::from_reader(&mut file, device)
    }

    pub fn from_reader<R: Read + Seek>(reader: &mut R, device: &Device) -> Result<Self> {
        let content = gguf_file::Content::read(reader)?;
        let mut names: Vec<&String> = content.tensor_infos.keys().collect();
        // Read in file order so a buffered reader streams.
        names.sort_by_key(|n| content.tensor_infos[*n].offset);
        let mut tensors = HashMap::with_capacity(names.len());
        for name in names {
            tensors.insert(name.clone(), content.tensor(reader, name, device)?);
        }
        Ok(Self { tensors, device: device.clone() })
    }

    pub fn from_tensors(tensors: HashMap<String, QTensor>, device: &Device) -> Self {
        Self { tensors, device: device.clone() }
    }

    fn take(&mut self, name: &str) -> Result<QTensor> {
        self.tensors.remove(name).ok_or_else(|| candle_core::Error::Msg(format!("the model has no tensor {name}")))
    }

    fn dense(&mut self, name: &str) -> Result<Tensor> {
        self.take(name)?.dequantize(&self.device)
    }

    fn linear(&mut self, prefix: &str, bias: bool) -> Result<Linear> {
        let weight = QMatMul::from_qtensor(self.take(&format!("{prefix}.weight"))?)?;
        let bias = if bias { Some(self.dense(&format!("{prefix}.bias"))?) } else { None };
        Ok(Linear { weight, bias })
    }

    fn layer_norm(&mut self, prefix: &str) -> Result<LayerNorm> {
        Ok(LayerNorm::new(self.dense(&format!("{prefix}.weight"))?, self.dense(&format!("{prefix}.bias"))?, 1e-5))
    }

    fn conv1d(&mut self, prefix: &str, stride: usize) -> Result<Conv1d> {
        let config = Conv1dConfig { padding: 1, stride, dilation: 1, groups: 1, cudnn_fwd_algo: None };
        Ok(Conv1d::new(self.dense(&format!("{prefix}.weight"))?, Some(self.dense(&format!("{prefix}.bias"))?), config))
    }
}

struct Linear {
    weight: QMatMul,
    bias: Option<Tensor>,
}

impl Linear {
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let y = self.weight.forward(x)?;
        match &self.bias {
            Some(b) => y.broadcast_add(b),
            None => Ok(y),
        }
    }
}

struct Attention {
    query: Linear,
    key: Linear,
    value: Linear,
    out: Linear,
    n_head: usize,
}

impl Attention {
    fn load(w: &mut Weights, prefix: &str, n_head: usize) -> Result<Self> {
        Ok(Self {
            query: w.linear(&format!("{prefix}.q_proj"), true)?,
            key: w.linear(&format!("{prefix}.k_proj"), false)?,
            value: w.linear(&format!("{prefix}.v_proj"), true)?,
            out: w.linear(&format!("{prefix}.out_proj"), true)?,
            n_head,
        })
    }

    /// `(batch, ctx, state)` to `(batch, heads, ctx, state / heads)`.
    fn heads(&self, x: &Tensor) -> Result<Tensor> {
        let (b, n, s) = x.dims3()?;
        x.reshape((b, n, self.n_head, s / self.n_head))?.transpose(1, 2)?.contiguous()
    }

    /// Keys and values of `x`, split into heads and scaled.
    fn key_value(&self, x: &Tensor) -> Result<(Tensor, Tensor)> {
        let (_, _, s) = x.dims3()?;
        let scale = ((s / self.n_head) as f64).powf(-0.25);
        let k = (self.heads(&self.key.forward(x)?)? * scale)?;
        let v = self.heads(&self.value.forward(x)?)?;
        Ok((k, v))
    }

    fn attend(&self, x: &Tensor, k: &Tensor, v: &Tensor, mask: Option<&Tensor>) -> Result<Tensor> {
        let (_, _, s) = x.dims3()?;
        let scale = ((s / self.n_head) as f64).powf(-0.25);
        let q = (self.heads(&self.query.forward(x)?)? * scale)?;
        let (n, ctx) = (q.dim(2)?, k.dim(2)?);
        // The encoder's 1500 x 1500 attention weights take 9 MB per head:
        // a few heads at a time keeps peak memory down at no cost in speed.
        let per_head = n * ctx * 4;
        let group = (ATTENTION_BUDGET / per_head.max(1)).clamp(1, self.n_head);
        let mut outputs = Vec::with_capacity(self.n_head.div_ceil(group));
        for start in (0..self.n_head).step_by(group) {
            let len = group.min(self.n_head - start);
            let (q, k, v) = (q.narrow(1, start, len)?, k.narrow(1, start, len)?, v.narrow(1, start, len)?);
            let mut w = q.matmul(&k.t()?)?;
            if let Some(mask) = mask {
                w = w.broadcast_add(mask)?;
            }
            let w = candle_nn::ops::softmax_last_dim(&w)?;
            outputs.push(w.matmul(&v)?);
        }
        let y = if outputs.len() == 1 { outputs.pop().unwrap_or_else(|| unreachable!()) } else { Tensor::cat(&outputs, 1)? };
        let y = y.transpose(1, 2)?.flatten_from(2)?;
        self.out.forward(&y)
    }
}

struct Block {
    attn: Attention,
    attn_ln: LayerNorm,
    cross: Option<(Attention, LayerNorm)>,
    fc1: Linear,
    fc2: Linear,
    mlp_ln: LayerNorm,
}

impl Block {
    fn load(w: &mut Weights, prefix: &str, n_head: usize, cross: bool) -> Result<Self> {
        let cross = if cross {
            Some((
                Attention::load(w, &format!("{prefix}.encoder_attn"), n_head)?,
                w.layer_norm(&format!("{prefix}.encoder_attn_layer_norm"))?,
            ))
        } else {
            None
        };
        Ok(Self {
            attn: Attention::load(w, &format!("{prefix}.self_attn"), n_head)?,
            attn_ln: w.layer_norm(&format!("{prefix}.self_attn_layer_norm"))?,
            cross,
            fc1: w.linear(&format!("{prefix}.fc1"), true)?,
            fc2: w.linear(&format!("{prefix}.fc2"), true)?,
            mlp_ln: w.layer_norm(&format!("{prefix}.final_layer_norm"))?,
        })
    }

    fn mlp(&self, x: &Tensor) -> Result<Tensor> {
        let h = self.fc1.forward(&self.mlp_ln.forward(x)?)?.gelu_erf()?;
        x + self.fc2.forward(&h)?
    }
}

/// Sinusoidal positions, for files that leave the encoder's out.
fn sinusoids(length: usize, channels: usize, device: &Device) -> Result<Tensor> {
    let half = channels / 2;
    let increment = (10_000f32).ln() / (half - 1) as f32;
    let mut data = vec![0f32; length * channels];
    for t in 0..length {
        for i in 0..half {
            let angle = t as f32 * (-increment * i as f32).exp();
            data[t * channels + i] = angle.sin();
            data[t * channels + half + i] = angle.cos();
        }
    }
    Tensor::from_vec(data, (length, channels), device)
}

pub struct Whisper {
    pub dims: Dims,
    device: Device,
    conv1: Conv1d,
    conv2: Conv1d,
    audio_positions: Tensor,
    encoder: Vec<Block>,
    encoder_ln: LayerNorm,
    /// `(vocab, state)`: looks tokens up and, transposed, projects to logits.
    embed_tokens: QMatMul,
    text_positions: Tensor,
    decoder: Vec<Block>,
    decoder_ln: LayerNorm,
}

/// Per-window decoder state: cached keys and values of every layer.
pub struct DecodeState {
    cross: Vec<(Tensor, Tensor)>,
    own: Vec<Option<(Tensor, Tensor)>>,
    len: usize,
}

impl DecodeState {
    /// Tokens decoded so far.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Whisper {
    pub fn load(mut w: Weights, dims: Dims) -> Result<Self> {
        let device = w.device.clone();
        let audio_positions = match w.dense("model.encoder.embed_positions.weight") {
            Ok(t) => t,
            Err(_) => sinusoids(dims.n_audio_ctx, dims.n_audio_state, &device)?,
        };
        let encoder = (0..dims.n_audio_layer)
            .map(|i| Block::load(&mut w, &format!("model.encoder.layers.{i}"), dims.n_audio_head, false))
            .collect::<Result<Vec<_>>>()?;
        let decoder = (0..dims.n_text_layer)
            .map(|i| Block::load(&mut w, &format!("model.decoder.layers.{i}"), dims.n_text_head, true))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            dims,
            conv1: w.conv1d("model.encoder.conv1", 1)?,
            conv2: w.conv1d("model.encoder.conv2", 2)?,
            audio_positions,
            encoder,
            encoder_ln: w.layer_norm("model.encoder.layer_norm")?,
            embed_tokens: QMatMul::from_qtensor(w.take("model.decoder.embed_tokens.weight")?)?,
            text_positions: w.dense("model.decoder.embed_positions.weight")?,
            decoder,
            decoder_ln: w.layer_norm("model.decoder.layer_norm")?,
            device,
        })
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Encodes a `(n_mels, frames)` row-major log-mel spectrogram. `frames`
    /// is even and at most 3000; fewer frames encode faster (a shorter audio
    /// context).
    pub fn encode(&self, mel: &[f32], frames: usize) -> Result<Tensor> {
        let x = Tensor::from_slice(mel, (1, self.dims.n_mels, frames), &self.device)?;
        let x = self.conv1.forward(&x)?.gelu_erf()?;
        let x = self.conv2.forward(&x)?.gelu_erf()?;
        let x = x.transpose(1, 2)?;
        let ctx = x.dim(1)?;
        let mut x = x.broadcast_add(&self.audio_positions.narrow(0, 0, ctx)?)?;
        for block in &self.encoder {
            let h = block.attn_ln.forward(&x)?;
            let (k, v) = block.attn.key_value(&h)?;
            x = (&x + block.attn.attend(&h, &k, &v, None)?)?;
            x = block.mlp(&x)?;
        }
        self.encoder_ln.forward(&x)
    }

    /// A decoder state for `audio` (the encoder's output).
    pub fn start(&self, audio: &Tensor) -> Result<DecodeState> {
        let cross = self
            .decoder
            .iter()
            .map(|b| match &b.cross {
                Some((attn, _)) => attn.key_value(audio),
                None => unreachable!("decoder blocks attend to the audio"),
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(DecodeState { own: vec![None; self.decoder.len()], cross, len: 0 })
    }

    /// Feeds `tokens` and returns the logits after the positions in `pick`
    /// (indices into `tokens`), `(pick.len(), vocab)`.
    pub fn decode(&self, tokens: &[u32], state: &mut DecodeState, pick: &[usize]) -> Result<Tensor> {
        let n = tokens.len();
        let offset = state.len;
        if offset + n > self.dims.n_text_ctx {
            candle_core::bail!("the transcript is longer than the model's text context");
        }
        let ids = Tensor::new(tokens, &self.device)?.unsqueeze(0)?;
        let mut x = self.embed_tokens.embedding(&ids)?.to_dtype(DType::F32)?;
        x = x.broadcast_add(&self.text_positions.narrow(0, offset, n)?)?;
        let mask = if n > 1 {
            let total = offset + n;
            let data: Vec<f32> = (0..n)
                .flat_map(|i| (0..total).map(move |j| if j > offset + i { f32::NEG_INFINITY } else { 0.0 }))
                .collect();
            Some(Tensor::from_vec(data, (n, total), &self.device)?)
        } else {
            None
        };
        for (i, block) in self.decoder.iter().enumerate() {
            let h = block.attn_ln.forward(&x)?;
            let (k, v) = block.attn.key_value(&h)?;
            let (k, v) = match &state.own[i] {
                Some((pk, pv)) => (Tensor::cat(&[pk, &k], 2)?, Tensor::cat(&[pv, &v], 2)?),
                None => (k, v),
            };
            x = (&x + block.attn.attend(&h, &k, &v, mask.as_ref())?)?;
            state.own[i] = Some((k, v));
            if let Some((cross, ln)) = &block.cross {
                let (ck, cv) = &state.cross[i];
                x = (&x + cross.attend(&ln.forward(&x)?, ck, cv, None)?)?;
            }
            x = block.mlp(&x)?;
        }
        state.len += n;
        let x = self.decoder_ln.forward(&x)?;
        let picked = if pick.len() == n && pick.iter().enumerate().all(|(a, &b)| a == b) {
            x
        } else {
            let index = Tensor::new(pick.iter().map(|&p| p as u32).collect::<Vec<_>>(), &self.device)?;
            x.index_select(&index, 1)?
        };
        let logits = self.embed_tokens.forward(&picked)?;
        logits.squeeze(0)?.to_dtype(DType::F32)
    }
}

/// Log-softmax over the last dimension, as a vector.
pub fn log_softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let sum: f32 = logits.iter().map(|l| (l - max).exp()).sum();
    let log_sum = sum.ln() + max;
    logits.iter().map(|l| l - log_sum).collect()
}

/// The logits of row `row` of `(rows, vocab)` logits.
pub fn row(logits: &Tensor, row: usize) -> Result<Vec<f32>> {
    logits.get(row)?.to_vec1::<f32>()
}

/// Index of the largest value.
pub fn argmax(values: &[f32]) -> usize {
    values.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map_or(0, |(i, _)| i)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use candle_core::quantized::GgmlDType;

    /// A tiny random model with Whisper's structure and tensor names,
    /// quantized as the installer would.
    pub(crate) fn random_weights(dims: Dims) -> HashMap<String, QTensor> {
        let dev = Device::Cpu;
        let mut tensors = HashMap::new();
        let mut seed = 1u64;
        let mut rand = |shape: &[usize]| {
            let n: usize = shape.iter().product();
            let data: Vec<f32> = (0..n)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    ((seed % 2000) as f32 / 1000.0 - 1.0) * 0.1
                })
                .collect();
            Tensor::from_vec(data, shape, &dev).unwrap()
        };
        let mut put = |name: String, t: Tensor| {
            let q = if t.rank() == 2 && t.dim(1).unwrap().is_multiple_of(32) && !name.contains("positions") {
                QTensor::quantize(&t, GgmlDType::Q8_0).unwrap()
            } else {
                QTensor::quantize(&t, GgmlDType::F32).unwrap()
            };
            tensors.insert(name, q);
        };
        let s = dims.n_audio_state;
        put("model.encoder.conv1.weight".into(), rand(&[s, dims.n_mels, 3]));
        put("model.encoder.conv1.bias".into(), rand(&[s]));
        put("model.encoder.conv2.weight".into(), rand(&[s, s, 3]));
        put("model.encoder.conv2.bias".into(), rand(&[s]));
        put("model.encoder.layer_norm.weight".into(), rand(&[s]));
        put("model.encoder.layer_norm.bias".into(), rand(&[s]));
        put("model.decoder.layer_norm.weight".into(), rand(&[s]));
        put("model.decoder.layer_norm.bias".into(), rand(&[s]));
        put("model.decoder.embed_tokens.weight".into(), rand(&[dims.n_vocab, s]));
        put("model.decoder.embed_positions.weight".into(), rand(&[dims.n_text_ctx, s]));
        for (side, layers) in [("encoder", dims.n_audio_layer), ("decoder", dims.n_text_layer)] {
            for i in 0..layers {
                let p = format!("model.{side}.layers.{i}");
                let mut attns = vec!["self_attn"];
                if side == "decoder" {
                    attns.push("encoder_attn");
                }
                for a in attns {
                    for proj in ["q_proj", "k_proj", "v_proj", "out_proj"] {
                        put(format!("{p}.{a}.{proj}.weight"), rand(&[s, s]));
                        if proj != "k_proj" {
                            put(format!("{p}.{a}.{proj}.bias"), rand(&[s]));
                        }
                    }
                    put(format!("{p}.{a}_layer_norm.weight"), rand(&[s]));
                    put(format!("{p}.{a}_layer_norm.bias"), rand(&[s]));
                }
                put(format!("{p}.fc1.weight"), rand(&[s * 4, s]));
                put(format!("{p}.fc1.bias"), rand(&[s * 4]));
                put(format!("{p}.fc2.weight"), rand(&[s, s * 4]));
                put(format!("{p}.fc2.bias"), rand(&[s]));
                put(format!("{p}.final_layer_norm.weight"), rand(&[s]));
                put(format!("{p}.final_layer_norm.bias"), rand(&[s]));
            }
        }
        tensors
    }

    pub(crate) const TINY: Dims = Dims {
        n_mels: 80,
        n_audio_ctx: 1500,
        n_audio_state: 64,
        n_audio_head: 2,
        n_audio_layer: 1,
        n_vocab: 300,
        n_text_ctx: 448,
        n_text_state: 64,
        n_text_head: 2,
        n_text_layer: 2,
    };

    #[test]
    fn cached_decoding_matches_decoding_the_whole_sequence() {
        let dev = Device::Cpu;
        let model = Whisper::load(Weights::from_tensors(random_weights(TINY), &dev), TINY).unwrap();
        let mel = vec![0.1f32; 80 * 200];
        let audio = model.encode(&mel, 200).unwrap();
        assert_eq!(audio.dims(), &[1, 100, 64]);
        let tokens = [5u32, 17, 42, 99, 3];

        let mut whole = model.start(&audio).unwrap();
        let all = model.decode(&tokens, &mut whole, &[0, 1, 2, 3, 4]).unwrap();

        let mut stepwise = model.start(&audio).unwrap();
        let first = model.decode(&tokens[..3], &mut stepwise, &[2]).unwrap();
        let fourth = model.decode(&tokens[3..4], &mut stepwise, &[0]).unwrap();
        let fifth = model.decode(&tokens[4..], &mut stepwise, &[0]).unwrap();
        assert_eq!(stepwise.len(), 5);

        let close = |a: Vec<f32>, b: Vec<f32>| {
            let diff = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).fold(0f32, f32::max);
            assert!(diff < 1e-3, "differs by {diff}");
        };
        close(row(&all, 2).unwrap(), row(&first, 0).unwrap());
        close(row(&all, 3).unwrap(), row(&fourth, 0).unwrap());
        close(row(&all, 4).unwrap(), row(&fifth, 0).unwrap());
    }

    #[test]
    fn weights_round_trip_through_gguf() {
        let dev = Device::Cpu;
        let tensors = random_weights(TINY);
        let mut buf = std::io::Cursor::new(Vec::new());
        let refs: Vec<(&str, &QTensor)> = tensors.iter().map(|(n, t)| (n.as_str(), t)).collect();
        gguf_file::write(&mut buf, &[], &refs).unwrap();
        buf.set_position(0);
        let weights = Weights::from_reader(&mut buf, &dev).unwrap();
        let model = Whisper::load(weights, TINY).unwrap();
        // The encoder's positions were not in the file: sinusoids stand in.
        assert_eq!(model.audio_positions.dims(), &[1500, 64]);
    }

    #[test]
    fn helpers() {
        assert_eq!(argmax(&[0.1, 3.0, -1.0]), 1);
        let ls = log_softmax(&[0.0, 0.0]);
        assert!((ls[0] - (0.5f32).ln()).abs() < 1e-6);
    }
}
