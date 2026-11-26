//! Measures the Whisper engine on real recordings.
//!
//! ```text
//! cargo run -p orbit-voice --release --example voice_bench -- \
//!     --model base.en-model.safetensors --tokenizer base.en-tokenizer.json \
//!     --dims base.en --precision q8_0 --wav short.wav,long.wav [--threads 8] [--prompt "GPUI, keymap.rs"]
//! ```
//!
//! A `.safetensors` model is converted first (timed) at `--precision`
//! (`q8_0`, `q4_0`, `f16` or `f32`), into `--keep <path>` if given; a
//! `.gguf` loads as is. Each WAV with a
//! `.txt` beside it is scored by word error rate. Prints load time, the
//! final pass's real-time factor and latency (what the user waits for after
//! release), the preview pass's, and peak memory.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use candle_core::Device;
use candle_core::quantized::GgmlDType;
use orbit_voice::engine::{Engine, Quality, Request};
use orbit_voice::testing::{to_mono_16k, word_error_rate};
use orbit_voice::vad::Vad;
use orbit_voice::whisper::WhisperEngine;

fn peak_mb() -> (f64, f64) {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb);
        (c.WorkingSetSize as f64 / 1e6, c.PeakWorkingSetSize as f64 / 1e6)
    }
    #[cfg(not(windows))]
    (0.0, 0.0)
}

fn read_wav(path: &Path) -> Vec<f32> {
    let reader = hound::WavReader::open(path).expect("open wav");
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader.into_samples::<i32>().map(|s| s.unwrap() as f32 / scale).collect()
        }
        hound::SampleFormat::Float => reader.into_samples::<f32>().map(|s| s.unwrap()).collect(),
    };
    to_mono_16k(&samples, spec.channels as usize, spec.sample_rate)
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let model = PathBuf::from(arg(&args, "--model").expect("--model"));
    let tokenizer = PathBuf::from(arg(&args, "--tokenizer").expect("--tokenizer"));
    let dims = orbit_voice::catalog::find(&arg(&args, "--dims").unwrap_or("base.en".into())).expect("--dims").dims;
    let precision = arg(&args, "--precision").unwrap_or("q8_0".into());
    let prompt = arg(&args, "--prompt").unwrap_or_default();
    let threads: usize = arg(&args, "--threads").map_or_else(orbit_voice::worker::default_threads, |t| t.parse().unwrap());
    let wavs: Vec<PathBuf> = arg(&args, "--wav").expect("--wav").split(',').map(PathBuf::from).collect();
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let gguf = if model.extension().is_some_and(|e| e == "safetensors") {
        let dtype = match precision.as_str() {
            "f32" => GgmlDType::F32,
            "f16" => GgmlDType::F16,
            "q4_0" => GgmlDType::Q4_0,
            _ => GgmlDType::Q8_0,
        };
        let dest = arg(&args, "--keep").map_or_else(|| dir.path().join("model.gguf"), PathBuf::from);
        let t = Instant::now();
        orbit_voice::convert::safetensors_to_gguf(&model, &dest, dtype, &|_| {}, &|| false).unwrap();
        println!(
            "convert {precision}: {:.2} s, {:.1} MB",
            t.elapsed().as_secs_f64(),
            std::fs::metadata(&dest).unwrap().len() as f64 / 1e6
        );
        dest
    } else {
        model
    };

    let before = peak_mb();
    let t = Instant::now();
    let mut engine = WhisperEngine::load(&gguf, &tokenizer, dims, &Device::Cpu).unwrap();
    let load = t.elapsed();
    let after = peak_mb();
    println!(
        "load: {:.0} ms; memory {:.0} -> {:.0} MB (peak {:.0} MB); threads {threads}",
        load.as_secs_f64() * 1e3,
        before.0,
        after.0,
        after.1
    );

    let cancel = AtomicBool::new(false);
    for wav in &wavs {
        let audio = read_wav(wav);
        let reference = std::fs::read_to_string(wav.with_extension("txt")).ok();
        // As dictation does: trim to the speech first.
        let mut vad = Vad::new();
        vad.push(&audio);
        let span = vad.speech_span(0..audio.len(), orbit_voice::stream::PAD).unwrap_or(0..audio.len());
        let speech = &audio[span];
        let seconds = speech.len() as f64 / 16_000.0;
        for quality in [Quality::Final, Quality::Preview] {
            let request = Request { language: Some("en"), translate: false, prompt: &prompt, quality };
            let mut times = Vec::new();
            let mut text = String::new();
            for _ in 0..3 {
                // Longer than 30 s is decoded in 30 s windows.
                let t = Instant::now();
                text.clear();
                for chunk in speech.chunks(30 * 16_000) {
                    let out = pool.install(|| engine.transcribe(chunk, &request, &cancel)).unwrap();
                    text.push_str(&out.text);
                }
                times.push(t.elapsed().as_secs_f64());
            }
            times.sort_by(f64::total_cmp);
            let median = times[1];
            let wer = reference.as_deref().map(|r| word_error_rate(r, &text));
            println!(
                "{} {:?}: {seconds:.1} s audio, {:.0} ms (RTF {:.3}), WER {}, peak {:.0} MB\n    {}",
                wav.file_name().unwrap().to_string_lossy(),
                quality,
                median * 1e3,
                median / seconds,
                wer.map_or("-".into(), |w| format!("{:.1}%", w * 100.0)),
                peak_mb().1,
                text.trim()
            );
        }
    }
}
