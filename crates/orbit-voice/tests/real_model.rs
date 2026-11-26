//! Dictation end to end on a real Whisper model and real speech.
//!
//! Skipped unless `ORBIT_WHISPER_MODEL` names a model file (`.gguf`, or
//! `.safetensors`, which is quantized to q8_0 first). Also read:
//! - `ORBIT_WHISPER_TOKENIZER`: its `tokenizer.json` (default: beside it);
//! - `ORBIT_WHISPER_DIMS`: its catalog id (default `base.en`);
//! - `ORBIT_WHISPER_WAV` and `ORBIT_WHISPER_TEXT`: a recording and its
//!   script. Without them, on Windows, the system's speech synthesizer
//!   records one into a temporary folder; elsewhere the test is skipped.
//!
//! Nothing is downloaded.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use orbit_voice::stream::SessionConfig;
use orbit_voice::testing::{to_mono_16k, word_error_rate};
use orbit_voice::whisper::WhisperLoader;
use orbit_voice::worker::{Event, Worker};

const SCRIPT: &str = "Please refactor the composer so that dictation inserts text at the cursor, then run the tests.";

fn synthesize(dir: &Path) -> Option<(PathBuf, String)> {
    if !cfg!(windows) {
        return None;
    }
    let wav = dir.join("speech.wav");
    let script = format!(
        "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; \
         $f = New-Object System.Speech.AudioFormat.SpeechAudioFormatInfo(16000, [System.Speech.AudioFormat.AudioBitsPerSample]::Sixteen, [System.Speech.AudioFormat.AudioChannel]::Mono); \
         $s.SetOutputToWaveFile('{}', $f); $s.Speak('{SCRIPT}'); $s.Dispose()",
        wav.display()
    );
    let ok = std::process::Command::new("powershell").args(["-NoProfile", "-Command", &script]).status().ok()?.success();
    ok.then(|| (wav, SCRIPT.to_owned()))
}

#[test]
fn dictation_transcribes_real_speech() {
    let Ok(model) = std::env::var("ORBIT_WHISPER_MODEL") else {
        eprintln!("skipped: set ORBIT_WHISPER_MODEL to run the real-model test");
        return;
    };
    let model = PathBuf::from(model);
    let tokenizer = std::env::var("ORBIT_WHISPER_TOKENIZER")
        .map(PathBuf::from)
        .unwrap_or_else(|_| model.with_file_name("tokenizer.json"));
    let dims = orbit_voice::catalog::find(&std::env::var("ORBIT_WHISPER_DIMS").unwrap_or("base.en".into()))
        .expect("ORBIT_WHISPER_DIMS is not a catalog id")
        .dims;
    let dir = tempfile::tempdir().unwrap();
    let (wav, reference) = match (std::env::var("ORBIT_WHISPER_WAV"), std::env::var("ORBIT_WHISPER_TEXT")) {
        (Ok(wav), Ok(text)) => (PathBuf::from(wav), text),
        _ => match synthesize(dir.path()) {
            Some(pair) => pair,
            None => {
                eprintln!("skipped: set ORBIT_WHISPER_WAV and ORBIT_WHISPER_TEXT");
                return;
            }
        },
    };
    let model = if model.extension().is_some_and(|e| e == "safetensors") {
        let gguf = dir.path().join("model.gguf");
        orbit_voice::convert::safetensors_to_gguf(&model, &gguf, candle_core::quantized::GgmlDType::Q8_0, &|_| {}, &|| false)
            .unwrap();
        gguf
    } else {
        model
    };

    let reader = hound::WavReader::open(&wav).unwrap();
    let spec = reader.spec();
    let samples: Vec<f32> = reader.into_samples::<i16>().map(|s| s.unwrap() as f32 / 32_768.0).collect();
    let mut audio = vec![0.0; 8_000];
    audio.extend(to_mono_16k(&samples, spec.channels as usize, spec.sample_rate));
    audio.extend(vec![0.0; 16_000]);

    let loader = Arc::new(WhisperLoader { model, tokenizer, dims, gpu: false });
    let worker = Worker::new(Duration::from_secs(30));
    let config = SessionConfig { preview: true, prompt: "Orbit, composer.".into(), ..Default::default() };
    let (mut session, rx) = worker.start(loader, config);
    // As a microphone delivers it: 100 ms at a time, in real time / 4.
    for chunk in audio.chunks(1_600) {
        session.push(chunk.to_vec());
        std::thread::sleep(Duration::from_millis(25));
    }
    let released = Instant::now();
    session.finish();
    let mut previews = 0;
    let text = loop {
        match rx.try_recv() {
            Ok(Event::Final(text)) => break text,
            Ok(Event::Failed(e)) => panic!("{e}"),
            Ok(Event::Preview(_)) => previews += 1,
            Ok(_) => {}
            Err(_) => {
                assert!(released.elapsed() < Duration::from_secs(60), "no result");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    };
    let latency = released.elapsed();
    let wer = word_error_rate(&reference, &text);
    eprintln!("final: {text:?}\nWER {:.1}%, {previews} previews, {} ms from release", wer * 100.0, latency.as_millis());
    assert!(previews > 0, "no live preview");
    assert!(wer < 0.25, "WER {wer} for {text:?}");
}
