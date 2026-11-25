//! Mouse reports for applications that ask for them (vim, htop, tmux…).
//!
//! Encodes xterm's SGR (1006), UTF-8 (1005) and default X10 formats. Reports
//! are only produced in the modes the application enabled, so raw sequences
//! never appear at a shell prompt (T3 Code #8574).

use alacritty_terminal::term::TermMode;
use gpui_kit::{Modifiers, MouseButton};

/// What happened to the mouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press(MouseButton),
    Release(MouseButton),
    /// Motion with the given button held, or none.
    Move(Option<MouseButton>),
    WheelUp,
    WheelDown,
}

/// The report for `action` at the zero-based cell (`col`, `row`), or `None`
/// when the current mode does not report it.
pub fn report(action: MouseAction, col: usize, row: usize, modifiers: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let button_code = |button: MouseButton| match button {
        MouseButton::Left => Some(0u8),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        MouseButton::Navigate(_) => None,
    };
    let (mut code, pressed) = match action {
        MouseAction::Press(button) => (button_code(button)?, true),
        MouseAction::Release(button) => (button_code(button)?, false),
        MouseAction::Move(button) => {
            let reported = match button {
                Some(_) => mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION),
                None => mode.contains(TermMode::MOUSE_MOTION),
            };
            if !reported {
                return None;
            }
            (button.and_then(button_code).unwrap_or(3) + 32, true)
        }
        MouseAction::WheelUp => (64, true),
        MouseAction::WheelDown => (65, true),
    };
    if modifiers.shift {
        code += 4;
    }
    if modifiers.alt {
        code += 8;
    }
    if modifiers.control {
        code += 16;
    }

    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{code};{};{}{suffix}", col + 1, row + 1).into_bytes());
    }

    // Legacy encodings report a release as button 3 (which one is unknown).
    if !pressed {
        code = (code & !0b11) | 3;
    }
    let mut out = vec![0x1b, b'[', b'M', 32 + code];
    let utf8 = mode.contains(TermMode::UTF8_MOUSE);
    for pos in [col, row] {
        let value = pos as u32 + 1 + 32;
        if utf8 {
            let c = char::from_u32(value.min(2047))?;
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        } else {
            // The X10 format cannot address columns past 222.
            out.push(u8::try_from(value).ok()?);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SGR: TermMode = TermMode::MOUSE_REPORT_CLICK.union(TermMode::SGR_MOUSE);

    #[test]
    fn nothing_is_reported_without_a_mouse_mode() {
        let none = Modifiers::none();
        assert_eq!(report(MouseAction::Press(MouseButton::Left), 0, 0, none, TermMode::default()), None);
        assert_eq!(report(MouseAction::WheelUp, 0, 0, none, TermMode::SGR_MOUSE), None);
    }

    #[test]
    fn sgr_reports() {
        let none = Modifiers::none();
        assert_eq!(report(MouseAction::Press(MouseButton::Left), 4, 9, none, SGR).unwrap(), b"\x1b[<0;5;10M");
        assert_eq!(report(MouseAction::Release(MouseButton::Left), 4, 9, none, SGR).unwrap(), b"\x1b[<0;5;10m");
        assert_eq!(report(MouseAction::WheelDown, 0, 0, none, SGR).unwrap(), b"\x1b[<65;1;1M");
        assert_eq!(
            report(MouseAction::Press(MouseButton::Right), 0, 0, Modifiers::control(), SGR).unwrap(),
            b"\x1b[<18;1;1M"
        );
        // Motion only in the modes that ask for it.
        assert_eq!(report(MouseAction::Move(Some(MouseButton::Left)), 1, 1, none, SGR), None);
        let drag = SGR | TermMode::MOUSE_DRAG;
        assert_eq!(report(MouseAction::Move(Some(MouseButton::Left)), 1, 1, none, drag).unwrap(), b"\x1b[<32;2;2M");
        assert_eq!(report(MouseAction::Move(None), 1, 1, none, drag), None);
    }

    #[test]
    fn x10_reports() {
        let mode = TermMode::MOUSE_REPORT_CLICK;
        let none = Modifiers::none();
        assert_eq!(report(MouseAction::Press(MouseButton::Left), 0, 0, none, mode).unwrap(), b"\x1b[M !!");
        assert_eq!(report(MouseAction::Release(MouseButton::Left), 0, 0, none, mode).unwrap(), b"\x1b[M#!!");
        assert_eq!(report(MouseAction::Press(MouseButton::Left), 300, 0, none, mode), None);
    }
}
