# Voice: choosing and measuring the speech engine (2026-11-25)

The user chose **on-device Whisper**. This note records which Whisper
implementation Orbit uses, why, and what it costs. All numbers are from one
machine: AMD Ryzen 9 7950X (16 cores, 32 threads), 31 GB, Windows 11, release
builds (`lto = "fat"`, `codegen-units = 1`), CPU only.

## Candidates

| | whisper.cpp (`whisper-rs` 0.16 / whisper.cpp 1.8.3) | candle 0.11 (pure Rust) |
| --- | --- | --- |
| Builds with | CMake, a C++ compiler, and **libclang** (bindgen) | `cargo` alone |
| Model files | ggml `.bin` (`ggerganov/whisper.cpp`) | safetensors (OpenAI's own) or GGUF |
| GPU | Metal, Vulkan, CUDA, … (C++ backends) | Metal, CUDA (features) |
| VAD | Silero, built in (exposed by whisper-rs 0.16) | none; Orbit uses earshot |
| Outcome | **Not used** | **Chosen** |

### Why not whisper.cpp

`whisper-rs-sys` generates its bindings with bindgen, which needs a 64-bit
`libclang.dll` on Windows. None is installed here (Visual Studio 18's LLVM
folder only has clang-format and clang-tidy), and the crate's bundled
bindings, generated on Linux, fail to compile on Windows (glibc `FILE` layout
assertions). Making Orbit's default build depend on CMake, a C++ toolchain
and LLVM on every developer's machine was rejected; the user chose a
pure-Rust engine instead. whisper.cpp's speed was therefore not measured
here. (Its CMake step did build; only bindgen failed.)

### Why candle, and why Orbit's own model code

candle builds with plain `cargo` on Windows, macOS and Linux, cross-checks
for other targets without their SDKs, and its quantized matrix kernels pick
AVX2/FMA at run time on x86_64 (`has_avx2_fma`) and use NEON on aarch64, so
no `-C target-cpu` is needed for speed.

`candle-transformers`' Whisper model was measured and then not used:

- **Its decoder keeps no self-attention cache.** Every step runs the whole
  token sequence again (its example passes all tokens each step), so
  decoding is quadratic in the transcript's length.
- It computes the vocabulary projection for every position, not only the
  last.
- It pulls in all of `candle-transformers` (every model candle ships) for one.

`orbit-voice/src/model.rs` is a ~400-line Whisper on `candle-core` and
`candle-nn` with a per-layer key/value cache, cross-attention keys and values
computed once per window, the projection only where logits are needed,
attention computed a few heads at a time (the encoder's 1500 × 1500 weights
are 9 MB per head; this cut peak memory from 334 MB to 218 MB for base.en and
made it faster), and weights read from GGUF with any mix of tensor types. A
test checks that cached, step-by-step decoding matches decoding the whole
sequence.

The log-mel front end (`mel.rs`) is also Orbit's: Whisper's exact recipe
(periodic Hann, centered STFT, Slaney filters computed in code) on one
planned `realfft` per frame with sparse filters, and no work for the silent
frames past the audio. The tokenizer (`tokenizer.rs`) reads `tokenizer.json`
itself, keeping the `tokenizers` crate and its C regex library out.

## Models

candle cannot read whisper.cpp's ggml `.bin` files. The catalog
(`orbit-voice/src/catalog.rs`) instead pins, by commit, size and SHA-256:

- **OpenAI's own safetensors** (`openai/whisper-{tiny,base,small}[.en]`,
  `openai/whisper-large-v3-turbo`), quantized to **q8_0 GGUF on this
  machine** after they verify (0.4 s for base.en; the f32 file is deleted);
- **candle's prebuilt q8_0 GGUF** for tiny (`lmz/candle-whisper`, by
  candle's maintainer), smaller to download.

q8_0 was chosen from the measurements below: the same words as f32 on every
clip, a quarter of the memory, a fifth of the load time, and faster. q4_0 is
smaller still but slower on this CPU and less accurate on the code-terms clip.

## Measurements

Test audio: three sentences recorded with Windows' speech synthesizer at
16 kHz (`System.Speech`): *short* (4.5 s of speech), *long* (28.5 s) and
*terms* (9.1 s, with code names: GPUI, crossfire, keymap.rs, sockudo). Word
error rate is against the script, ignoring case and punctuation. Each figure
is the median of three runs after loading; audio is trimmed by the VAD first,
as dictation does. "Final" is the full-quality pass (Whisper's 30-second
context), which is what the user waits for after release; "Preview" is the
live pass (audio context cut to the speech plus one second). 8 decoding
threads.

Reproduce: `cargo run -p orbit-voice --release --example voice_bench --
--model <file> --tokenizer <json> --dims base.en --wav short.wav,long.wav,terms.wav`.

### Precision and size (final pass, the user's wait after release)

| Model | Load | Short 4.5 s | Long 28.5 s | Terms 9.1 s | WER short / long / terms | Peak memory |
| --- | ---: | ---: | ---: | ---: | --- | ---: |
| **base.en q8_0** (default) | 60–140 ms | **461 ms** (RTF 0.10) | 838 ms (0.029) | 595 ms (0.065) | 8.3 / 2.7 / 30.4 % | **220 MB** |
| base.en f32 | 306 ms | 649 ms (0.14) | 1,559 ms (0.055) | 708 ms (0.078) | 8.3 / 4.1 / 30.4 % | 416 MB |
| base.en q4_0 | 98 ms | 738 ms (0.16) | 1,127 ms (0.040) | 865 ms (0.095) | 8.3 / 4.1 / 30.4 % | 141 MB |
| tiny.en q8_0 | 82 ms | 319 ms (0.071) | 477 ms (0.017) | 356 ms (0.039) | 8.3 / 4.1 / 30.4 % | 132 MB |

Live preview pass (shortened context): base.en q8_0 98 ms on the short clip
(RTF 0.022), 173 ms on terms (0.019); tiny.en 78 ms and 117 ms. A preview runs
every 0.7 s of new speech, so the CPU is busy a fraction of the time even with
base.

Memory is the whole process's peak working set (the benchmark binary,
Windows), model included: base.en q8_0 is 102 MB once loaded, 220 MB at
peak while decoding a 30-second window. Converting OpenAI's f32 file to q8_0
at install takes 0.4–1.3 s and peaks at 381 MB, once.

The only word wrong on the short clip, in every model, is "caret" heard as
"carrot"; TTS audio is clean, so tiny scores as well as base here. On real
speech tiny is markedly less accurate, which is why base.en is the default.

### Before Orbit's model code

The first measurements used `candle-transformers`' model structure with
synthetic weights (same arithmetic): its full-context encoder took ~400 ms
for base and each decode step re-ran every token. With Orbit's model the
same base.en short clip went from 518 ms to 461 ms and its peak memory from
334 MB to 218 MB (attention a few heads at a time).

### Build cost

- `orbit-voice` alone, clean release build: **66 s** on this machine
  (candle-core, candle-nn, gemm, realfft, rubato, earshot; most other
  dependencies are already in Orbit).
- The benchmark executable (engine, resampler, VAD, tokenizer, WAV reader)
  is 3.4 MB; a candle-transformers Whisper benchmark was 1.5 MB larger than an
  empty program before Orbit's additions.
- `orbit-desktop.exe` (release): 52.8 MB without voice (base `6e619ba`),
  56.5 MB with it: **+3.7 MB**. Part of that is GPUI Kit's `speech` feature,
  which also compiles the system recognisers Orbit turns off.
- Idle cost: none measurable. Working set 71.3 → 71.2 MB, private 98.9 →
  94.6 MB, 50 threads either way, 0.00 % CPU (median of three launches,
  10 s after an 8 s settle).

### Real-model test

`cargo test -p orbit-voice --release --test real_model` (base.en, q8_0) runs a
whole dictation session on the worker: 0.5 s of silence, a synthesized
sentence, 1 s of silence, pushed in 100 ms buffers at four times real time.
Result: the sentence word for word (WER 0 %), 11 live previews, **619 ms from
release to the final text** (this includes the last preview still running at
release).

### Threads

base.en q8_0, short clip, final pass: 4 threads 603 ms, 8 threads 443–518 ms,
16 threads 575 ms. Orbit uses half the logical cores, at most 8
(`worker::default_threads`), in a pool of its own that exists only while a
model is loaded.

### Vocabulary prompt

The *terms* clip, base.en q8_0: WER 30.4 % without a prompt, **17.4 %** with
"GPUI, crossfire, keymap.rs, sockudo." as the vocabulary (it spells
`crossfire` and `keymap.rs` right; TTS's "GPUI" and "sockudo" stay hard).

### GPU

- **Metal** is on for macOS builds (`orbit-voice/metal`, enabled by
  orbit-app for `target_os = "macos"`); it needs nothing beyond Xcode's
  command-line tools. Not measured (no Mac here).
- **CUDA** is a feature (`orbit-voice/cuda`) that needs the CUDA toolkit at
  build time, so it is off by default. Not measured.
- Vulkan: candle has no Vulkan backend.

## What was not measured

- whisper.cpp (did not build; see above).
- Real voices and microphones: all audio here is synthesized speech, which
  is cleaner than real speech; real-world error rates are higher, more so for
  tiny.
- Frame times while decoding: decoding runs on its own threads and the UI
  thread only enqueues audio and applies results, so it is not expected to
  stutter, but it was not measured.
- macOS and Linux runs.
