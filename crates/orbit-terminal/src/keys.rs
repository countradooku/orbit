//! Keystroke to byte encoding (xterm conventions).
//!
//! Only keys that do not produce text are encoded here. Printable characters
//! reach the terminal through the platform's text input (`EntityInputHandler`),
//! so keyboard layouts, AltGr, dead keys and IME composition all work: a German
//! `AltGr+Q` types `@` instead of being taken for `Ctrl+Alt+Q` (T3 Code #13046).

use std::borrow::Cow;

use alacritty_terminal::term::TermMode;
use gpui_kit::Keystroke;

/// Encodes a keystroke that does not produce text by itself, or returns
/// `None` to let the character reach the terminal through text input.
///
/// `alt_is_meta` makes `Alt+key` send `ESC key` (the readline Meta prefix).
pub fn encode_keystroke(keystroke: &Keystroke, mode: TermMode, alt_is_meta: bool) -> Option<Cow<'static, [u8]>> {
    let m = &keystroke.modifiers;
    let key = keystroke.key.as_str();
    let modifier_param = 1 + (m.shift as u8) + 2 * (m.alt as u8) + 4 * (m.control as u8);
    let modified = modifier_param > 1;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);

    // Cursor and editing keys: CSI 1;m X when modified, SS3 in application mode.
    let cursor_final = match key {
        "up" => Some(b'A'),
        "down" => Some(b'B'),
        "right" => Some(b'C'),
        "left" => Some(b'D'),
        "home" => Some(b'H'),
        "end" => Some(b'F'),
        _ => None,
    };
    if let Some(final_byte) = cursor_final {
        if m.platform {
            return None;
        }
        return Some(if modified {
            Cow::Owned(format!("\x1b[1;{modifier_param}{}", final_byte as char).into_bytes())
        } else if app_cursor {
            Cow::Owned(vec![0x1b, b'O', final_byte])
        } else {
            Cow::Owned(vec![0x1b, b'[', final_byte])
        });
    }

    let tilde_code = match key {
        "insert" => Some(2),
        "delete" => Some(3),
        "pageup" => Some(5),
        "pagedown" => Some(6),
        "f5" => Some(15),
        "f6" => Some(17),
        "f7" => Some(18),
        "f8" => Some(19),
        "f9" => Some(20),
        "f10" => Some(21),
        "f11" => Some(23),
        "f12" => Some(24),
        "f13" => Some(25),
        "f14" => Some(26),
        "f15" => Some(28),
        "f16" => Some(29),
        "f17" => Some(31),
        "f18" => Some(32),
        "f19" => Some(33),
        "f20" => Some(34),
        _ => None,
    };
    if let Some(code) = tilde_code {
        return Some(Cow::Owned(if modified {
            format!("\x1b[{code};{modifier_param}~").into_bytes()
        } else {
            format!("\x1b[{code}~").into_bytes()
        }));
    }

    let ss3_function = match key {
        "f1" => Some(b'P'),
        "f2" => Some(b'Q'),
        "f3" => Some(b'R'),
        "f4" => Some(b'S'),
        _ => None,
    };
    if let Some(final_byte) = ss3_function {
        return Some(Cow::Owned(if modified {
            format!("\x1b[1;{modifier_param}{}", final_byte as char).into_bytes()
        } else {
            vec![0x1b, b'O', final_byte]
        }));
    }

    let meta = m.alt && alt_is_meta;
    let with_meta = |bytes: &'static [u8]| -> Cow<'static, [u8]> {
        if meta {
            let mut out = Vec::with_capacity(bytes.len() + 1);
            out.push(0x1b);
            out.extend_from_slice(bytes);
            Cow::Owned(out)
        } else {
            Cow::Borrowed(bytes)
        }
    };

    match key {
        "enter" => return Some(with_meta(if mode.contains(TermMode::LINE_FEED_NEW_LINE) { b"\r\n" } else { b"\r" })),
        "escape" => return Some(with_meta(b"\x1b")),
        "tab" if m.shift => return Some(Cow::Borrowed(b"\x1b[Z")),
        "tab" => return Some(with_meta(b"\t")),
        "backspace" if m.control => return Some(with_meta(b"\x08")),
        "backspace" => return Some(with_meta(b"\x7f")),
        "space" if m.control => return Some(with_meta(b"\x00")),
        "space" if meta => return Some(Cow::Borrowed(b"\x1b ")),
        _ => {}
    }

    if m.platform || m.function {
        return None;
    }

    // Ctrl+key: the C0 control code. Ctrl+Alt+key adds the Meta prefix.
    if m.control {
        let code = control_code(key, keystroke.key_char.as_deref())?;
        let mut out = Vec::with_capacity(2);
        if meta {
            out.push(0x1b);
        }
        out.push(code);
        return Some(Cow::Owned(out));
    }

    // Alt+character as Meta: ESC followed by the character Alt did not change.
    if meta {
        let text = keystroke.key_char.as_deref().filter(|c| !c.is_empty()).unwrap_or(key);
        if text.chars().count() == 1 {
            let mut out = Vec::with_capacity(text.len() + 1);
            out.push(0x1b);
            out.extend_from_slice(text.as_bytes());
            return Some(Cow::Owned(out));
        }
    }
    None
}

/// The C0 control code for Ctrl+`key` (`Ctrl+A` is 0x01, `Ctrl+[` is ESC).
fn control_code(key: &str, key_char: Option<&str>) -> Option<u8> {
    let mut chars = key.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let c = c.to_ascii_lowercase();
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        '@' | '2' | ' ' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' | '~' | '`' => 0x1e,
        '_' | '7' | '-' | '/' => 0x1f,
        '8' | '?' => 0x7f,
        _ => {
            // A layout where the key's own character differs (e.g. AZERTY):
            // fall back to what the key types.
            let typed = key_char?.chars().next()?.to_ascii_lowercase();
            if typed.is_ascii_lowercase() { typed as u8 - b'a' + 1 } else { return None }
        }
    })
}

/// Wraps pasted text for the shell: bracketed when the application asked for
/// it, with line endings normalized to carriage returns otherwise. Escape
/// characters are removed so a paste can never end the bracket early or
/// inject a control sequence.
pub fn encode_paste(text: &str, mode: TermMode) -> Vec<u8> {
    let clean: String = text.chars().filter(|&c| c != '\x1b').collect();
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = Vec::with_capacity(clean.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(clean.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        clean.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(spec: &'static str) -> Keystroke {
        Keystroke::parse(spec).unwrap()
    }

    fn enc(spec: &'static str) -> Option<Vec<u8>> {
        encode_keystroke(&ks(spec), TermMode::default(), true).map(|c| c.into_owned())
    }

    fn enc_mode(spec: &'static str, mode: TermMode) -> Option<Vec<u8>> {
        encode_keystroke(&ks(spec), mode, true).map(|c| c.into_owned())
    }

    #[test]
    fn plain_text_is_left_to_text_input() {
        assert_eq!(enc("a"), None);
        assert_eq!(enc("shift-a"), None);
        assert_eq!(enc("space"), None);
        assert_eq!(enc("1"), None);
    }

    #[test]
    fn arrows_follow_cursor_key_mode_and_modifiers() {
        assert_eq!(enc("up").unwrap(), b"\x1b[A");
        assert_eq!(enc_mode("up", TermMode::APP_CURSOR).unwrap(), b"\x1bOA");
        assert_eq!(enc("ctrl-right").unwrap(), b"\x1b[1;5C");
        assert_eq!(enc("shift-left").unwrap(), b"\x1b[1;2D");
        assert_eq!(enc("alt-down").unwrap(), b"\x1b[1;3B");
        assert_eq!(enc_mode("ctrl-up", TermMode::APP_CURSOR).unwrap(), b"\x1b[1;5A");
        assert_eq!(enc("home").unwrap(), b"\x1b[H");
        assert_eq!(enc_mode("end", TermMode::APP_CURSOR).unwrap(), b"\x1bOF");
        assert_eq!(enc("ctrl-shift-home").unwrap(), b"\x1b[1;6H");
    }

    #[test]
    fn editing_and_function_keys() {
        assert_eq!(enc("pageup").unwrap(), b"\x1b[5~");
        assert_eq!(enc("pagedown").unwrap(), b"\x1b[6~");
        assert_eq!(enc("delete").unwrap(), b"\x1b[3~");
        assert_eq!(enc("insert").unwrap(), b"\x1b[2~");
        assert_eq!(enc("shift-pageup").unwrap(), b"\x1b[5;2~");
        assert_eq!(enc("f1").unwrap(), b"\x1bOP");
        assert_eq!(enc("f4").unwrap(), b"\x1bOS");
        assert_eq!(enc("shift-f1").unwrap(), b"\x1b[1;2P");
        assert_eq!(enc("f5").unwrap(), b"\x1b[15~");
        assert_eq!(enc("f12").unwrap(), b"\x1b[24~");
        assert_eq!(enc("ctrl-f12").unwrap(), b"\x1b[24;5~");
    }

    #[test]
    fn control_and_meta() {
        assert_eq!(enc("ctrl-c").unwrap(), b"\x03");
        assert_eq!(enc("ctrl-a").unwrap(), b"\x01");
        assert_eq!(enc("ctrl-z").unwrap(), b"\x1a");
        assert_eq!(enc("ctrl-shift-x").unwrap(), b"\x18");
        assert_eq!(enc("ctrl-[").unwrap(), b"\x1b");
        assert_eq!(enc("ctrl-space").unwrap(), b"\x00");
        assert_eq!(enc("ctrl-alt-b").unwrap(), b"\x1b\x02");
        assert_eq!(enc("alt-b").unwrap(), b"\x1bb");
        assert_eq!(enc("alt-.").unwrap(), b"\x1b.");
        assert_eq!(encode_keystroke(&ks("alt-b"), TermMode::default(), false), None);
    }

    #[test]
    fn special_keys() {
        assert_eq!(enc("enter").unwrap(), b"\r");
        assert_eq!(enc("alt-enter").unwrap(), b"\x1b\r");
        assert_eq!(enc("backspace").unwrap(), b"\x7f");
        assert_eq!(enc("ctrl-backspace").unwrap(), b"\x08");
        assert_eq!(enc("alt-backspace").unwrap(), b"\x1b\x7f");
        assert_eq!(enc("tab").unwrap(), b"\t");
        assert_eq!(enc("shift-tab").unwrap(), b"\x1b[Z");
        assert_eq!(enc("escape").unwrap(), b"\x1b");
    }

    #[test]
    fn paste_is_bracketed_and_sanitized() {
        assert_eq!(encode_paste("a\r\nb\nc", TermMode::default()), b"a\rb\rc");
        assert_eq!(encode_paste("x\x1b[201~y", TermMode::BRACKETED_PASTE), b"\x1b[200~x[201~y\x1b[201~");
    }
}
