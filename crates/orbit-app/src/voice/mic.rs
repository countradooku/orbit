//! The microphone: the input device picked in Settings › Voice, captured
//! with cpal and delivered to the kit's speech state as 16 kHz mono.
//!
//! Replaces the kit's `Microphone`, which only opens the default device.
//! The audio callback only mixes down and hands the buffer over; resampling
//! (rubato's FFT resampler, band-limited) happens on the forwarding task.
//! Nothing is recorded to disk, and the stream closes the moment the
//! session's capture ends, so the system's microphone indicator goes off.

use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use cpal::{FromSample, Sample as _, SampleFormat, SizedSample, StreamConfig};
use crossfire::{BlockingTxTrait as _, mpsc};
use gpui_kit::component::speech::{AudioFormat, AudioInput, AudioSink, SpeechError};
use gpui_kit::*;

/// Captures from the device named `device` (or the system default).
#[derive(Clone, Debug, Default)]
pub struct Mic {
    pub device: String,
}

/// The names of the input devices, for the picker.
pub fn devices() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else { return Vec::new() };
    let mut names: Vec<String> = devices.filter_map(|d| d.name().ok()).collect();
    names.dedup();
    names
}

fn open_device(name: &str) -> Result<cpal::Device, SpeechError> {
    let host = cpal::default_host();
    if !name.is_empty()
        && let Ok(mut devices) = host.input_devices()
        && let Some(device) = devices.find(|d| d.name().is_ok_and(|n| n == name))
    {
        return Ok(device);
    }
    // The picked device is gone (unplugged): the default stands in.
    host.default_input_device().ok_or(SpeechError::NoInputDevice)
}

enum Capture {
    Samples(Vec<f32>),
    Error(String),
}

impl AudioInput for Mic {
    fn start(&self, format: AudioFormat, sink: AudioSink, cx: &mut App) -> Result<Subscription, SpeechError> {
        if is_blocked() {
            return Err(SpeechError::PermissionDenied);
        }
        let device = open_device(&self.device)?;
        let supported = device.default_input_config().map_err(|e| map_error(&e.to_string()))?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let source_rate = config.sample_rate.0;
        // About two seconds of 10 ms buffers; a stalled UI drops audio
        // rather than growing without bound.
        let (tx, rx) = mpsc::bounded_blocking_async::<Capture>(256);
        let stream = match sample_format {
            SampleFormat::F32 => build::<f32>(&device, &config, tx),
            SampleFormat::I16 => build::<i16>(&device, &config, tx),
            SampleFormat::U16 => build::<u16>(&device, &config, tx),
            SampleFormat::I32 => build::<i32>(&device, &config, tx),
            other => return Err(SpeechError::input(anyhow_error(format!("unsupported sample format {other}")))),
        }?;
        stream.play().map_err(|e| map_error(&e.to_string()))?;
        let channels = usize::from(format.channels().max(1));
        let target_rate = format.sample_rate();
        let task = cx.spawn(async move |cx| {
            let mut resampler = orbit_voice::audio::Resampler::new(source_rate);
            let rate_ok = target_rate == orbit_voice::audio::SAMPLE_RATE;
            let mut mono = Vec::new();
            let mut out = Vec::new();
            while let Ok(first) = rx.recv().await {
                let mut error = None;
                for capture in std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok())) {
                    match capture {
                        Capture::Samples(s) => mono.extend_from_slice(&s),
                        Capture::Error(e) => error = Some(e),
                    }
                }
                out.clear();
                if rate_ok {
                    resampler.push(&mono, &mut out);
                } else {
                    out.extend_from_slice(&mono);
                }
                mono.clear();
                let samples: Vec<i16> = orbit_voice::audio::f32_to_i16(&out)
                    .into_iter()
                    .flat_map(|s| std::iter::repeat_n(s, channels))
                    .collect();
                cx.update(|cx| {
                    if !samples.is_empty() {
                        sink.push(samples, cx);
                    }
                    if let Some(e) = error.take() {
                        sink.error(map_error(&e), cx);
                    }
                });
            }
        });
        Ok(Subscription::new(move || {
            drop(stream);
            drop(task);
        }))
    }
}

fn anyhow_error(message: String) -> std::io::Error {
    std::io::Error::other(message)
}

/// A device error, as the kit's error kinds.
fn map_error(message: &str) -> SpeechError {
    let lower = message.to_lowercase();
    if lower.contains("access is denied") || lower.contains("0x80070005") || lower.contains("permission") {
        SpeechError::PermissionDenied
    } else if lower.contains("not available") || lower.contains("no longer available") {
        SpeechError::NoInputDevice
    } else {
        SpeechError::input(anyhow_error(message.to_owned()))
    }
}

/// Opens a stream that sends each callback's frames, mixed to mono. Runs on
/// the audio thread, so it only converts and sends.
fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    tx: crossfire::MTx<mpsc::Array<Capture>>,
) -> Result<cpal::Stream, SpeechError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels.max(1));
    let error_tx = tx.clone();
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let mono: Vec<f32> = data
                    .chunks(channels)
                    .map(|frame| frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / frame.len() as f32)
                    .collect();
                let _ = tx.try_send(Capture::Samples(mono));
            },
            move |error| {
                let _ = error_tx.try_send(Capture::Error(error.to_string()));
            },
            None,
        )
        .map_err(|error| match error {
            cpal::BuildStreamError::DeviceNotAvailable => SpeechError::NoInputDevice,
            error => map_error(&error.to_string()),
        })
}

/// Whether the system blocks Orbit from the microphone.
pub fn is_blocked() -> bool {
    #[cfg(windows)]
    return windows::is_blocked();
    #[cfg(target_os = "macos")]
    return macos::is_denied();
    #[allow(unreachable_code)]
    false
}

/// Whether the platform has a privacy page to open for the microphone.
pub const HAS_PRIVACY_SETTINGS: bool = cfg!(any(windows, target_os = "macos"));

/// Opens the system's microphone privacy settings.
pub fn open_privacy_settings(cx: &mut App) {
    #[cfg(windows)]
    cx.open_url("ms-settings:privacy-microphone");
    #[cfg(target_os = "macos")]
    cx.open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone");
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = cx;
}

#[cfg(windows)]
mod windows {
    //! Settings › Privacy › Microphone. With access off, WASAPI either fails
    //! to open the device or delivers silence, so it is read first.

    use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};

    const STORE: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn denied(root: HKEY, key: &str) -> bool {
        let (key, name) = (wide(key), wide("Value"));
        let mut buf = [0u16; 32];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: valid NUL-terminated strings and a buffer of `len` bytes.
        let status = unsafe {
            RegGetValueW(root, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len)
        };
        if status != 0 {
            return false;
        }
        let value = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
        value.eq_ignore_ascii_case("deny")
    }

    pub fn is_blocked() -> bool {
        // Off for the whole device, off for this user, or off for desktop
        // apps (Orbit is not packaged).
        denied(HKEY_LOCAL_MACHINE, STORE) || denied(HKEY_CURRENT_USER, STORE) || denied(HKEY_CURRENT_USER, &format!(r"{STORE}\NonPackaged"))
    }
}

#[cfg(target_os = "macos")]
mod macos {
    //! Capturing without access yields silence on macOS, not an error, so the
    //! authorization is read first. Until the user answers the system's
    //! prompt (shown on first capture) the status is undetermined, which
    //! proceeds.

    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

    pub fn is_denied() -> bool {
        // SAFETY: reading an immutable framework constant.
        let Some(audio) = (unsafe { AVMediaTypeAudio }) else { return false };
        // SAFETY: `audio` is a media type the method accepts.
        let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) };
        status == AVAuthorizationStatus::Denied || status == AVAuthorizationStatus::Restricted
    }
}
