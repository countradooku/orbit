//! On-device dictation for Orbit.
//!
//! Speech is recognized by OpenAI's Whisper running on [candle] in pure Rust:
//! no C++ toolchain, no service, and audio never leaves the machine or
//! touches the disk. The pieces, from microphone to text:
//!
//! - [`audio`]: resampling the device's audio to 16 kHz mono.
//! - [`vad`]: where the speech is (`earshot`), to trim silence, skip it and
//!   stop after it.
//! - [`mel`], [`model`], [`tokenizer`], [`whisper`]: the recognizer.
//! - [`stream`]: a session's previews, long-recording cuts and final pass.
//! - [`worker`]: the background thread that owns the model, loaded lazily
//!   and dropped when idle.
//! - [`postprocess`], [`prompt`], [`insert`]: the text: cleanup, the
//!   vocabulary prompt and placing it where the user is typing.
//! - [`address`]: which agent a spoken instruction is for (voice steering).
//! - [`control`]: the shortcut's modes, the five-minute cap and the silence
//!   timeout.
//! - [`catalog`], [`store`], [`convert`]: the pinned model list, verified
//!   resumable installs and quantization to q8_0.
//!
//! [candle]: https://github.com/huggingface/candle

pub mod address;
pub mod audio;
pub mod catalog;
pub mod control;
pub mod convert;
pub mod engine;
pub mod insert;
pub mod languages;
pub mod mel;
pub mod model;
pub mod postprocess;
pub mod prompt;
pub mod store;
pub mod stream;
pub mod testing;
pub mod tokenizer;
pub mod vad;
pub mod whisper;
pub mod worker;
