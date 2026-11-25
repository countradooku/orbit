//! Orbit's terminal view: a GPUI Kit terminal emulator on `alacritty_terminal`.
//!
//! [`TerminalView`] is source agnostic. Feed it the bytes a PTY (or a remote
//! host) produced with [`TerminalView::feed`], and wire its
//! [`TerminalEvent::Input`] and [`TerminalEvent::Resize`] back to the source:
//!
//! ```ignore
//! let view = cx.new(|cx| TerminalView::new(window, cx));
//! cx.subscribe(&view, move |_, event, _| match event {
//!     TerminalEvent::Input(bytes) => { terminals.input(id, bytes); }
//!     TerminalEvent::Resize { cols, rows } => { terminals.resize(id, TermSize::new(*cols, *rows)).ok(); }
//!     _ => {}
//! }).detach();
//! ```
//!
//! Call [`init`] once after `gpui_kit::init` to register the key bindings.
//!
//! The view answers the terminal queries programs rely on (device
//! attributes, cursor position, colors), so shells such as fish do not stall
//! at startup (T3 Code #6058), and it reports the mouse only in the modes a
//! program asked for.

mod colors;
mod element;
mod keys;
mod links;
mod mouse;
mod view;

pub use colors::Palette;
pub use keys::{encode_keystroke, encode_paste};
pub use view::{TerminalEvent, TerminalView};

use gpui_kit::{App, KeyBinding, NoAction, actions};

/// The key context of a focused terminal. Bind to it to add terminal-only
/// shortcuts, or bind [`NoAction`] to it to send another key to the shell.
pub const KEY_CONTEXT: &str = "Terminal";

/// Lines of scrollback kept by default.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

actions!(
    terminal,
    [
        /// Copies the selection to the clipboard.
        Copy,
        /// Pastes the clipboard into the shell (bracketed when the shell asks).
        Paste,
        /// Clears the scrollback and asks the shell to redraw its prompt.
        Clear,
        /// Selects everything in the scrollback and on screen.
        SelectAll,
        /// Scrolls the scrollback up one line.
        ScrollLineUp,
        /// Scrolls the scrollback down one line.
        ScrollLineDown,
        /// Scrolls the scrollback up one screen.
        ScrollPageUp,
        /// Scrolls the scrollback down one screen.
        ScrollPageDown,
        /// Scrolls to the start of the scrollback.
        ScrollToTop,
        /// Scrolls back to the live screen.
        ScrollToBottom,
    ]
);

/// Registers the terminal's key bindings. Call once at startup, after
/// `gpui_kit::init` and before building menus.
///
/// Keys a shell or TUI needs (Tab, Escape, Ctrl+C, Ctrl+R, Ctrl+W…) are bound
/// to [`NoAction`] in the terminal context, so window-level bindings such as
/// focus navigation on Tab or Root's Ctrl+C copy cannot take them away from
/// the shell (T3 Code #731, #6362). Ctrl+C copies only while there is a
/// selection; otherwise it interrupts.
pub fn init(cx: &mut App) {
    let ctx = Some(KEY_CONTEXT);
    let mut bindings = vec![
        KeyBinding::new("shift-pageup", ScrollPageUp, ctx),
        KeyBinding::new("shift-pagedown", ScrollPageDown, ctx),
        KeyBinding::new("shift-home", ScrollToTop, ctx),
        KeyBinding::new("shift-end", ScrollToBottom, ctx),
        KeyBinding::new("ctrl-shift-up", ScrollLineUp, ctx),
        KeyBinding::new("ctrl-shift-down", ScrollLineDown, ctx),
    ];
    #[cfg(target_os = "macos")]
    bindings.extend([
        KeyBinding::new("cmd-c", Copy, ctx),
        KeyBinding::new("cmd-v", Paste, ctx),
        KeyBinding::new("cmd-k", Clear, ctx),
        KeyBinding::new("cmd-a", SelectAll, ctx),
    ]);
    #[cfg(not(target_os = "macos"))]
    bindings.extend([
        // The last binding of an action is the one menus show.
        KeyBinding::new("ctrl-insert", Copy, ctx),
        KeyBinding::new("ctrl-shift-c", Copy, ctx),
        KeyBinding::new("shift-insert", Paste, ctx),
        KeyBinding::new("ctrl-shift-v", Paste, ctx),
        KeyBinding::new("ctrl-shift-k", Clear, ctx),
        KeyBinding::new("ctrl-shift-a", SelectAll, ctx),
    ]);
    // Windows Terminal convention: Ctrl+V pastes.
    #[cfg(target_os = "windows")]
    bindings.push(KeyBinding::new("ctrl-v", Paste, ctx));
    for key in SHELL_KEYS {
        bindings.push(KeyBinding::new(key, NoAction, ctx));
    }
    cx.bind_keys(bindings);
}

/// Keys that always go to the shell while a terminal is focused.
const SHELL_KEYS: &[&str] = &[
    "escape",
    "tab",
    "shift-tab",
    "enter",
    "backspace",
    "ctrl-a",
    "ctrl-b",
    "ctrl-c",
    "ctrl-d",
    "ctrl-e",
    "ctrl-f",
    "ctrl-h",
    "ctrl-l",
    "ctrl-r",
    "ctrl-u",
    "ctrl-w",
    "ctrl-y",
    "ctrl-z",
    "alt-b",
    "alt-f",
    "alt-d",
];
