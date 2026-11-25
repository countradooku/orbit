//! When recording starts and stops: the shortcut's modes, the length cap and
//! the silence timeout, as a clock-driven state machine with no UI in it.

use std::time::{Duration, Instant};

/// How the dictation shortcut and mic button behave.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// A tap toggles recording; holding records until release.
    #[default]
    Auto,
    /// Records while held.
    Hold,
    /// Each press starts or stops.
    Toggle,
}

/// The longest recording.
pub const MAX_DURATION: Duration = Duration::from_secs(5 * 60);
/// How long before the cap the warning shows.
pub const WARN_BEFORE: Duration = Duration::from_secs(30);
/// A press shorter than this is a tap.
pub const TAP: Duration = Duration::from_millis(300);

/// What the caller should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    /// Stop recording and transcribe.
    Stop(StopReason),
    /// Show that recording ends soon.
    Warn { remaining: Duration },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    User,
    /// [`MAX_DURATION`] reached.
    Cap,
    /// The silence timeout passed after speech.
    Silence,
}

#[derive(Debug)]
enum State {
    Idle,
    Recording {
        started: Instant,
        /// While the key is held: when it went down.
        pressed: Option<Instant>,
        /// The press that started recording was a tap (auto mode).
        latched: bool,
        warned: bool,
    },
}

#[derive(Debug)]
pub struct Controller {
    mode: Mode,
    silence_timeout: Option<Duration>,
    max: Duration,
    state: State,
}

impl Controller {
    pub fn new(mode: Mode, silence_timeout: Option<Duration>) -> Self {
        Self { mode, silence_timeout, max: MAX_DURATION, state: State::Idle }
    }

    /// A shorter cap, for tests.
    pub fn with_max(mut self, max: Duration) -> Self {
        self.max = max;
        self
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    pub fn set_silence_timeout(&mut self, timeout: Option<Duration>) {
        self.silence_timeout = timeout;
    }

    pub fn is_recording(&self) -> bool {
        matches!(self.state, State::Recording { .. })
    }

    /// Whether the shortcut is down and recording depends on its release.
    pub fn is_held(&self) -> bool {
        matches!(self.state, State::Recording { pressed: Some(_), .. })
    }

    /// Time recorded, at `now`.
    pub fn elapsed(&self, now: Instant) -> Duration {
        match self.state {
            State::Recording { started, .. } => now.saturating_duration_since(started),
            State::Idle => Duration::ZERO,
        }
    }

    /// The shortcut went down. Key repeat while held is ignored.
    pub fn key_down(&mut self, now: Instant) -> Option<Action> {
        match &mut self.state {
            State::Idle => {
                self.state = State::Recording { started: now, pressed: Some(now), latched: false, warned: false };
                Some(Action::Start)
            }
            State::Recording { pressed: Some(_), .. } => None,
            State::Recording { pressed, .. } => match self.mode {
                Mode::Hold => {
                    *pressed = Some(now);
                    None
                }
                Mode::Auto | Mode::Toggle => self.stop(StopReason::User),
            },
        }
    }

    /// The shortcut came up.
    pub fn key_up(&mut self, now: Instant) -> Option<Action> {
        let mode = self.mode;
        let State::Recording { pressed, latched, .. } = &mut self.state else { return None };
        let down = pressed.take()?;
        match mode {
            Mode::Hold => self.stop(StopReason::User),
            Mode::Toggle => None,
            Mode::Auto if !*latched && now.saturating_duration_since(down) < TAP => {
                *latched = true;
                None
            }
            Mode::Auto if *latched => None,
            Mode::Auto => self.stop(StopReason::User),
        }
    }

    /// A click on the mic button: always a toggle, whatever the mode.
    pub fn click(&mut self, now: Instant) -> Option<Action> {
        match self.state {
            State::Idle => {
                self.state = State::Recording { started: now, pressed: None, latched: true, warned: false };
                Some(Action::Start)
            }
            State::Recording { .. } => self.stop(StopReason::User),
        }
    }

    /// Recording ended elsewhere (Cancel, Send, an error).
    pub fn reset(&mut self) {
        self.state = State::Idle;
    }

    /// Advances the clock. `silence` is the quiet time since speech, when
    /// speech has been heard.
    pub fn tick(&mut self, now: Instant, silence: Option<Duration>) -> Option<Action> {
        let held = matches!(self.state, State::Recording { pressed: Some(_), latched: false, .. });
        let State::Recording { started, warned, .. } = &mut self.state else { return None };
        let elapsed = now.saturating_duration_since(*started);
        if elapsed >= self.max {
            return self.stop(StopReason::Cap);
        }
        // Holding the key means the user is still dictating, however quiet.
        if !held
            && let (Some(timeout), Some(quiet)) = (self.silence_timeout, silence)
            && quiet >= timeout
        {
            return self.stop(StopReason::Silence);
        }
        if !*warned && elapsed + WARN_BEFORE.min(self.max / 2) >= self.max {
            *warned = true;
            return Some(Action::Warn { remaining: self.max - elapsed });
        }
        None
    }

    fn stop(&mut self, reason: StopReason) -> Option<Action> {
        self.state = State::Idle;
        Some(Action::Stop(reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn auto_tap_toggles_and_hold_is_push_to_talk() {
        let t0 = Instant::now();
        let mut c = Controller::new(Mode::Auto, None);
        // Tap: starts and stays on.
        assert_eq!(c.key_down(t0), Some(Action::Start));
        assert_eq!(c.key_up(t0 + ms(120)), None);
        assert!(c.is_recording());
        // Next press stops.
        assert_eq!(c.key_down(t0 + ms(3_000)), Some(Action::Stop(StopReason::User)));
        assert_eq!(c.key_up(t0 + ms(3_100)), None);
        assert!(!c.is_recording());
        // Hold: stops on release.
        assert_eq!(c.key_down(t0 + ms(5_000)), Some(Action::Start));
        assert_eq!(c.key_down(t0 + ms(5_400)), None, "key repeat is ignored");
        assert_eq!(c.key_up(t0 + ms(7_000)), Some(Action::Stop(StopReason::User)));
    }

    #[test]
    fn hold_and_toggle_modes() {
        let t0 = Instant::now();
        let mut hold = Controller::new(Mode::Hold, None);
        assert_eq!(hold.key_down(t0), Some(Action::Start));
        assert_eq!(hold.key_up(t0 + ms(50)), Some(Action::Stop(StopReason::User)), "even a tap stops");
        let mut toggle = Controller::new(Mode::Toggle, None);
        assert_eq!(toggle.key_down(t0), Some(Action::Start));
        assert_eq!(toggle.key_up(t0 + ms(2_000)), None, "a long press does not stop");
        assert_eq!(toggle.key_down(t0 + ms(4_000)), Some(Action::Stop(StopReason::User)));
    }

    #[test]
    fn clicking_the_button_toggles_in_every_mode() {
        let t0 = Instant::now();
        let mut c = Controller::new(Mode::Hold, None);
        assert_eq!(c.click(t0), Some(Action::Start));
        assert_eq!(c.tick(t0 + ms(1_000), None), None);
        assert_eq!(c.click(t0 + ms(2_000)), Some(Action::Stop(StopReason::User)));
    }

    #[test]
    fn the_five_minute_cap_warns_then_stops() {
        let t0 = Instant::now();
        let mut c = Controller::new(Mode::Toggle, None);
        c.key_down(t0);
        c.key_up(t0);
        assert_eq!(c.tick(t0 + Duration::from_secs(200), None), None);
        assert_eq!(c.tick(t0 + Duration::from_secs(271), None), Some(Action::Warn { remaining: Duration::from_secs(29) }));
        assert_eq!(c.tick(t0 + Duration::from_secs(280), None), None, "warns once");
        assert_eq!(c.tick(t0 + MAX_DURATION, None), Some(Action::Stop(StopReason::Cap)));
        assert!(!c.is_recording());
        assert_eq!(c.elapsed(t0 + MAX_DURATION), Duration::ZERO);
    }

    #[test]
    fn silence_stops_a_latched_recording_but_not_a_held_one() {
        let t0 = Instant::now();
        let quiet = Some(Duration::from_secs(10));
        let mut c = Controller::new(Mode::Auto, Some(Duration::from_secs(8)));
        // Held: silence is ignored.
        c.key_down(t0);
        assert_eq!(c.tick(t0 + Duration::from_secs(12), quiet), None);
        c.key_up(t0 + Duration::from_secs(12));
        // Tapped: silence after speech stops it.
        c.key_down(t0 + Duration::from_secs(20));
        c.key_up(t0 + Duration::from_secs(20) + ms(100));
        assert_eq!(c.tick(t0 + Duration::from_secs(25), Some(Duration::from_secs(5))), None);
        assert_eq!(c.tick(t0 + Duration::from_secs(31), quiet), Some(Action::Stop(StopReason::Silence)));
        // No timeout configured: never.
        let mut off = Controller::new(Mode::Toggle, None);
        off.key_down(t0);
        off.key_up(t0);
        assert_eq!(off.tick(t0 + Duration::from_secs(100), Some(Duration::from_secs(90))), None);
    }

    #[test]
    fn reset_returns_to_idle() {
        let t0 = Instant::now();
        let mut c = Controller::new(Mode::Auto, None);
        c.key_down(t0);
        c.reset();
        assert!(!c.is_recording());
        assert_eq!(c.key_up(t0 + ms(10)), None);
    }
}
