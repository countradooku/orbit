//! Dictation preferences, kept with this computer's shell preferences
//! (`shell.json`): the model and microphone belong to the machine, not to
//! the host the client is connected to.

use orbit_voice::control::Mode;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoicePrefs {
    /// A catalog id (`orbit_voice::catalog`).
    pub model: String,
    pub mode: Mode,
    /// A Whisper language code, or `auto` to detect it.
    pub language: String,
    pub translate: bool,
    /// The user's vocabulary: terms, and `heard => written` replacements.
    pub vocabulary: String,
    /// Add the project's name and file names to the vocabulary.
    pub project_terms: bool,
    pub remove_fillers: bool,
    /// Fix spacing and capitalisation.
    pub tidy: bool,
    pub live_preview: bool,
    /// The input device's name; empty for the system default.
    pub microphone: String,
    /// Stop after this many seconds of silence (auto and toggle modes); 0
    /// never stops.
    pub silence_timeout: u32,
    /// Use the GPU where the build has a backend for it.
    pub gpu: bool,
    /// Unload the model after this many idle minutes.
    pub unload_minutes: u32,
}

impl Default for VoicePrefs {
    fn default() -> Self {
        Self {
            model: orbit_voice::catalog::DEFAULT_MODEL.to_owned(),
            mode: Mode::Auto,
            language: "auto".to_owned(),
            translate: false,
            vocabulary: String::new(),
            project_terms: true,
            remove_fillers: true,
            tidy: true,
            live_preview: true,
            microphone: String::new(),
            silence_timeout: 30,
            gpu: true,
            unload_minutes: 5,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_defaults() {
        let prefs: VoicePrefs = sonic_rs::from_str(r#"{"model":"tiny.en","mode":"hold"}"#).unwrap();
        assert_eq!(prefs.model, "tiny.en");
        assert_eq!(prefs.mode, Mode::Hold);
        assert_eq!(prefs.silence_timeout, 30);
        assert!(prefs.live_preview);
    }
}
