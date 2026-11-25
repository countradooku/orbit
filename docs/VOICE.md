# Voice: on-device dictation

Speak into the composer, the Home prompt, search and settings fields, or
straight to an agent. Speech is recognised by OpenAI's Whisper running inside
Orbit, on this computer: there is no cloud service, and audio is never written
to disk or sent anywhere.

## Using it

| | |
| --- | --- |
| **Dictate** | `Ctrl+M` (every platform; `Cmd+M` is macOS's Minimize), or the microphone button left of Send. Not in terminals, where `Ctrl+M` is Return. |
| **Steer an agent** | `Ctrl+Shift+M`, then say the thread's name and the instruction: "api, run the tests then open a PR". It is sent to that thread without opening it. "Tell the docs agent to fix the typo", "hey api, …" and a typed "@api …" work too. |
| **Modes** (Settings › Voice) | *Auto* (default): a tap starts and stops, a hold records until you let go. *Hold to talk*. *Toggle*. The mic button always toggles. |
| **While recording** | A pill shows the waveform, the time, the words heard so far (sure words in the text colour, the tentative tail muted), **Cancel** (`Esc`) and **Done**. |
| **Limits** | Five minutes per recording, with a warning 30 seconds before. In auto and toggle modes, recording also stops after the silence timeout (30 s by default; Off, 5, 10, 30, 60, 120 s). |
| **The text** | Lands at the caret, or replaces the selection, spaced and capitalised for where it lands ("Please \| now" + "Fix the parser." → "Please fix the parser now"), and stays editable. |
| **Send while recording** | Pressing Send (or Enter) stops recording, waits for the transcript, inserts it and sends. |

With no text field focused, `Ctrl+M` dictates into the open thread's composer
(or Home's). The first use without a model opens a small sheet offering the
download ("Download 293 MB") instead of an error.

### Settings › Voice

- **Model**: tiny, base, small (English-only or 99 languages) and large-v3
  turbo, each with its download and disk size, speed and accuracy; Download,
  Resume, Cancel, Use and Delete. Base (English) is the default.
- **Language** (detect, or one of 99) and **Translate to English**
  (multilingual models).
- **Vocabulary**: terms to spell your way, one per line or comma.
  `soccer do => sockudo` also replaces what Whisper hears. With **Add project
  and file names** on, the project's name and its distinctive file names
  (camelCase, snake_case, `keymap.rs`) join it. The vocabulary is Whisper's
  initial prompt; on the synthesized test sentence with code terms it cut the
  word error rate from 30.4 % to 17.4 % (see `docs/research/voice.md`).
- **Remove filler words** ("um", "uh", "erm"…) and **Fix punctuation and
  capitals**.
- **Microphone**: the input device (or the system default), and a test with
  a live level meter.
- **Shortcut mode**, **Stop after silence**, **Live preview**.
- **Use the GPU** (Metal on macOS builds) and **Unload the model after**
  (1, 5, 15 or 60 idle minutes).

### Privacy and permissions

- Audio stays in memory while you dictate and is dropped afterwards. The
  microphone stream is open only while recording, so the system's microphone
  indicator is off otherwise.
- Models download from Hugging Face only when you choose one, over HTTPS, from
  URLs pinned to a commit, and each file must match its published size and
  SHA-256 or it is discarded.
- **Windows**: if Settings › Privacy & security › Microphone blocks desktop
  apps, dictation says so and offers **Open privacy settings**
  (`ms-settings:privacy-microphone`).
- **macOS**: the app asks for the microphone on first use
  (`NSMicrophoneUsageDescription` is in the bundle's `Info.plist`; signed
  builds carry the `com.apple.security.device.audio-input` entitlement the
  hardened runtime requires). A denied permission is reported with a link to
  System Settings › Privacy & Security › Microphone.
- **Linux**: capture goes through ALSA (PipeWire and PulseAudio provide ALSA
  devices).

## How it works

```text
cpal (picked device) ─► mono ─► rubato FFT resampler ─► 16 kHz i16
   └─► kit SpeechState (level meter, waveform, session) ─► WhisperRecognizer
        └─► crossfire ─► dictation worker thread
              earshot VAD ─► session: previews · cuts at pauses · final pass
              └─► candle Whisper (q8_0 GGUF, KV-cached decoder) in a rayon pool
        ◄── crossfire ◄── Preview / Activity / Final(cleaned text)
   SpeechEvent::Final ─► insert at the caret ─► (send)
```

- **Crates.** `orbit-voice` holds everything that is not UI: audio
  (`audio`), voice activity (`vad`), the recogniser (`mel`, `model`,
  `tokenizer`, `whisper`), sessions (`stream`), the worker (`worker`), text
  (`postprocess`, `prompt`, `insert`, `address`), the shortcut's state
  machine (`control`) and models (`catalog`, `store`, `convert`).
  `orbit-app/src/voice` plugs it into GPUI Kit's speech module.
- **GPUI Kit's speech module** supplies the session state, the waveform and
  the indicator the mic button shows. Orbit adds its own `SpeechRecognizer`
  (Whisper) and `AudioInput` (the picked microphone) and turns the kit's
  system recogniser fallback off (Windows' would send audio to Microsoft).
- **Live preview.** Every 0.7 s of new audio the open window is re-decoded
  with a shortened audio context; words two consecutive previews agree on
  become stable (LocalAgreement-2) and never flicker back.
- **Long recordings.** Past 20 s, the open window is cut at the longest pause
  after 12 s and decoded at full quality right away, while you are still
  speaking, so release-to-text latency stays flat for a five-minute
  recording. Each window is conditioned on the text before it.
- **Final pass.** At release the last window is decoded with Whisper's full
  30-second context; a looping or low-confidence result is retried once with
  temperature fallback, and a window Whisper scores as no speech is dropped.
- **Silence.** The VAD (earshot, a 40 KB neural VAD compiled in; no model to
  download) trims leading and trailing silence, skips windows without speech
  (Whisper invents text for silence) and drives the silence timeout.
- **Idle.** No thread, stream or model exists until dictation starts (or the
  pointer rests on the mic button, which pre-warms the model). After the idle
  timeout the worker drops the model and its compute pool and exits.
- **Remote hosts.** Dictation runs in the desktop app, where the microphone
  is; only the text crosses the wire. No protocol change.

Models live in `<data folder>/models/whisper/<id>/` (`model.gguf`,
`tokenizer.json`, `manifest.txt`); downloads in progress in
`<data folder>/models/whisper/.partial/<id>/`, resumed from where they
stopped. OpenAI's safetensors weights are quantized to q8_0 on this machine
after they verify (about a second for base), which makes them a quarter of the
size in memory and load in ~60 ms.

## Building

Dictation is pure Rust and always built. Beyond the usual Rust toolchain:

| OS | Needs |
| --- | --- |
| Windows | Nothing more (WASAPI). |
| macOS | Xcode's command-line tools (as for any Orbit build). Metal is on for macOS builds. |
| Linux | ALSA headers for cpal: `libasound2-dev` (Debian/Ubuntu), `alsa-lib-devel` (Fedora), `alsa-lib` (Arch); and `pkg-config`. |

No CMake, libclang or C++ compiler is involved (whisper.cpp was not used for
that reason; see `docs/research/voice.md`).

Optional: `cargo build -p orbit-voice --features cuda` runs Whisper on NVIDIA
GPUs; it needs the CUDA toolkit (`nvcc`) at build time, so it is off by
default and orbit-app does not enable it.

## Testing

- `cargo test -p orbit-voice`: resampling, VAD trimming and cuts, the mel
  front end, tokenizer, the model's cached decoder against a full decode,
  sessions (previews, cuts, final), the worker (lazy start, idle unload,
  cancel), the catalog, verified and resumable installs (file source),
  quantization, cleanup, the vocabulary prompt, insertion at the caret,
  addressing and the shortcut's state machine.
- `cargo test -p orbit-app voice`: dictation end to end in a window (kit
  state, the real worker with a scripted engine, synthetic speech, text in a
  textarea), cancel, send-while-recording, and HTTP range resumption against
  a loopback server.
- Real model (skipped unless set; nothing is downloaded):

  ```powershell
  $env:ORBIT_WHISPER_MODEL = "…\base.en-model.safetensors"   # or a .gguf
  $env:ORBIT_WHISPER_TOKENIZER = "…\base.en-tokenizer.json"
  cargo test -p orbit-voice --release --test real_model -- --nocapture
  ```

  On Windows it records its own sentence with the system's speech
  synthesizer; elsewhere set `ORBIT_WHISPER_WAV` and `ORBIT_WHISPER_TEXT`.
- Benchmark: `cargo run -p orbit-voice --release --example voice_bench --
  --model <file> --tokenizer <json> --dims base.en --wav a.wav,b.wav`.
- Debug builds: `ORBIT_DEBUG_DICTATION=setup` opens the setup sheet;
  `ORBIT_DEBUG_DICTATION="some words"` dictates them into the composer with a
  synthetic voice and a scripted recogniser, for screenshots.
