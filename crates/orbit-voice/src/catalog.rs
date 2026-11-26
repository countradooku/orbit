//! The Whisper models Orbit offers, each file pinned by URL (at a fixed
//! commit), size and SHA-256. A download that does not match is rejected.
//!
//! Sources, in order of preference:
//! - **OpenAI's own weights** (`openai/whisper-*`, safetensors), quantized to
//!   q8_0 GGUF on this machine after they verify. q8_0 is within noise of
//!   the full-precision model for Whisper, takes a quarter of the memory and
//!   loads in a fraction of the time.
//! - **candle's prebuilt q8_0 GGUF** for tiny (`lmz/candle-whisper`, by
//!   candle's maintainer), which is smaller to download than OpenAI's f32
//!   file.
//!
//! The model dimensions are compiled in, so no `config.json` is fetched.

use crate::model::Dims;

/// One file to download.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelFile {
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

/// The format of a model's weights as published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weights {
    /// Ready to load.
    Gguf(ModelFile),
    /// Quantized to q8_0 GGUF after download.
    Safetensors(ModelFile),
}

impl Weights {
    pub fn file(&self) -> &ModelFile {
        match self {
            Weights::Gguf(f) | Weights::Safetensors(f) => f,
        }
    }
}

/// A relative rating from 1 (least) to 4 (most).
pub type Rating = u8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    /// Stable id, also the folder name: `base.en`.
    pub id: &'static str,
    pub name: &'static str,
    pub english_only: bool,
    pub weights: Weights,
    pub tokenizer: ModelFile,
    pub dims: Dims,
    /// Bytes on disk once installed.
    pub installed_bytes: u64,
    /// Typical peak memory while loaded and decoding.
    pub memory_bytes: u64,
    pub speed: Rating,
    pub quality: Rating,
}

impl ModelInfo {
    /// Bytes to download.
    pub fn download_bytes(&self) -> u64 {
        self.weights.file().size + self.tokenizer.size
    }
}

const MB: u64 = 1_000_000;

const TOKENIZER_EN: ModelFile = ModelFile {
    url: "https://huggingface.co/openai/whisper-base.en/resolve/911407f4214e0e1d82085af863093ec0b66f9cd6/tokenizer.json",
    size: 2_405_679,
    sha256: "5eb60cec1e77aeeb6869a2bb5a8e01a84c3fe5d072d75369343021fe6f5310d0",
};

const TOKENIZER_MULTI: ModelFile = ModelFile {
    url: "https://huggingface.co/openai/whisper-base/resolve/e37978b90ca9030d5170a5c07aadb050351a65bb/tokenizer.json",
    size: 2_480_466,
    sha256: "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566",
};

const TOKENIZER_V3: ModelFile = ModelFile {
    url: "https://huggingface.co/openai/whisper-large-v3-turbo/resolve/41f01f3fe87f28c78e2fbf8b568835947dd65ed9/tokenizer.json",
    size: 2_710_337,
    sha256: "297b13372ac43916285644fb9687add3cc62ee2a1adb60da3dc25cc94c1871fd",
};

const VOCAB_EN: usize = 51_864;
const VOCAB_MULTI: usize = 51_865;
const VOCAB_V3: usize = 51_866;

pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "tiny.en",
        name: "Tiny",
        english_only: true,
        weights: Weights::Gguf(ModelFile {
            url: "https://huggingface.co/lmz/candle-whisper/resolve/02d8350e5402f18725eadb6101b4963d181b0b5e/model-tiny-en-q80.gguf",
            size: 41_841_216,
            sha256: "07997efe0b8d3577fc45d9b307ef578a01346f4088a5da4bd92377aaa3f72986",
        }),
        tokenizer: TOKENIZER_EN,
        dims: Dims::new(80, 384, 6, 4, 4, VOCAB_EN),
        installed_bytes: 44 * MB,
        memory_bytes: 160 * MB,
        speed: 4,
        quality: 1,
    },
    ModelInfo {
        id: "tiny",
        name: "Tiny",
        english_only: false,
        weights: Weights::Gguf(ModelFile {
            url: "https://huggingface.co/lmz/candle-whisper/resolve/02d8350e5402f18725eadb6101b4963d181b0b5e/model-tiny-q80.gguf",
            size: 41_841_632,
            sha256: "edcc907db61aef092f1244dc1e53c55056b472be343e9bbb4dc12ebd4740392f",
        }),
        tokenizer: TOKENIZER_MULTI,
        dims: Dims::new(80, 384, 6, 4, 4, VOCAB_MULTI),
        installed_bytes: 44 * MB,
        memory_bytes: 160 * MB,
        speed: 4,
        quality: 1,
    },
    ModelInfo {
        id: "base.en",
        name: "Base",
        english_only: true,
        weights: Weights::Safetensors(ModelFile {
            url: "https://huggingface.co/openai/whisper-base.en/resolve/911407f4214e0e1d82085af863093ec0b66f9cd6/model.safetensors",
            size: 290_401_888,
            sha256: "d4dd5542fd6a1d35639e21384238f3bfe6c557c849d392b5905d33ee29e71db5",
        }),
        tokenizer: TOKENIZER_EN,
        dims: Dims::new(80, 512, 8, 6, 6, VOCAB_EN),
        installed_bytes: 82 * MB,
        memory_bytes: 260 * MB,
        speed: 3,
        quality: 2,
    },
    ModelInfo {
        id: "base",
        name: "Base",
        english_only: false,
        weights: Weights::Safetensors(ModelFile {
            url: "https://huggingface.co/openai/whisper-base/resolve/e37978b90ca9030d5170a5c07aadb050351a65bb/model.safetensors",
            size: 290_403_936,
            sha256: "07cadb9f25677c8d50df603e66a98fbd842cce45047139baeb16e6219a1e807b",
        }),
        tokenizer: TOKENIZER_MULTI,
        dims: Dims::new(80, 512, 8, 6, 6, VOCAB_MULTI),
        installed_bytes: 82 * MB,
        memory_bytes: 260 * MB,
        speed: 3,
        quality: 2,
    },
    ModelInfo {
        id: "small.en",
        name: "Small",
        english_only: true,
        weights: Weights::Safetensors(ModelFile {
            url: "https://huggingface.co/openai/whisper-small.en/resolve/e8727524f962ee844a7319d92be39ac1bd25655a/model.safetensors",
            size: 966_992_008,
            sha256: "6014ac49b506df900f66f4aca6b0801eed7245594ace97bcaf73e0ae5b863066",
        }),
        tokenizer: TOKENIZER_EN,
        dims: Dims::new(80, 768, 12, 12, 12, VOCAB_EN),
        installed_bytes: 262 * MB,
        memory_bytes: 560 * MB,
        speed: 2,
        quality: 3,
    },
    ModelInfo {
        id: "small",
        name: "Small",
        english_only: false,
        weights: Weights::Safetensors(ModelFile {
            url: "https://huggingface.co/openai/whisper-small/resolve/973afd24965f72e36ca33b3055d56a652f456b4d/model.safetensors",
            size: 966_995_080,
            sha256: "1d7734884874f1a1513ed9aa760a4f8e97aaa02fd6d93a3a85d27b2ae9ca596b",
        }),
        tokenizer: TOKENIZER_MULTI,
        dims: Dims::new(80, 768, 12, 12, 12, VOCAB_MULTI),
        installed_bytes: 262 * MB,
        memory_bytes: 560 * MB,
        speed: 2,
        quality: 3,
    },
    ModelInfo {
        id: "large-v3-turbo",
        name: "Large v3 Turbo",
        english_only: false,
        weights: Weights::Safetensors(ModelFile {
            url: "https://huggingface.co/openai/whisper-large-v3-turbo/resolve/41f01f3fe87f28c78e2fbf8b568835947dd65ed9/model.safetensors",
            size: 1_617_824_864,
            sha256: "542566a422ae4f3fd23f1ba11add198fca01bbf82e66e6a2857b3f608b1eb9d1",
        }),
        tokenizer: TOKENIZER_V3,
        dims: Dims::new(128, 1280, 20, 32, 4, VOCAB_V3),
        installed_bytes: 870 * MB,
        memory_bytes: 1_400 * MB,
        speed: 1,
        quality: 4,
    },
];

/// The model offered first: English base, a good balance on any CPU.
pub const DEFAULT_MODEL: &str = "base.en";

pub fn find(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| m.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_is_pinned() {
        let mut ids = std::collections::HashSet::new();
        for m in MODELS {
            assert!(ids.insert(m.id), "duplicate {}", m.id);
            for f in [m.weights.file(), &m.tokenizer] {
                assert!(f.url.starts_with("https://huggingface.co/"), "{}", f.url);
                // A commit, not a branch: the bytes behind the URL never change.
                let rev = f.url.split("/resolve/").nth(1).and_then(|r| r.split('/').next()).unwrap();
                assert_eq!(rev.len(), 40, "{} is not pinned to a commit", f.url);
                assert_eq!(f.sha256.len(), 64);
                assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
                assert!(f.size > 1_000_000);
            }
            assert_eq!(m.english_only, m.dims.n_vocab == VOCAB_EN, "{}", m.id);
            assert!(m.installed_bytes < m.download_bytes() || matches!(m.weights, Weights::Gguf(_)));
        }
        assert!(find(DEFAULT_MODEL).is_some());
    }
}
