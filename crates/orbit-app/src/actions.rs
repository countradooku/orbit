//! Commands and their key bindings. Every command here is reachable from the
//! command palette, and the important ones from a visible control too.

use gpui_kit::*;

gpui_kit::actions!(
    orbit,
    [
        NewThread,
        OpenPalette,
        GoInbox,
        GoFleet,
        GoHome,
        ToggleSidebar,
        ToggleReview,
        OpenSettings,
        AddProject,
        StopTurn,
        NextThread,
        PreviousThread,
        FocusComposer,
        ToggleTerminal,
        Quit,
    ]
);

pub fn bind(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-n", NewThread, None),
        KeyBinding::new("ctrl-k", OpenPalette, None),
        KeyBinding::new("ctrl-shift-p", OpenPalette, None),
        KeyBinding::new("ctrl-i", GoInbox, None),
        KeyBinding::new("ctrl-shift-f", GoFleet, None),
        KeyBinding::new("ctrl-b", ToggleSidebar, None),
        KeyBinding::new("ctrl-r", ToggleReview, Some("Workspace")),
        KeyBinding::new("ctrl-,", OpenSettings, None),
        KeyBinding::new("ctrl-o", AddProject, None),
        KeyBinding::new("ctrl-.", StopTurn, Some("Workspace")),
        KeyBinding::new("ctrl-tab", NextThread, None),
        KeyBinding::new("ctrl-shift-tab", PreviousThread, None),
        KeyBinding::new("ctrl-l", FocusComposer, Some("Workspace")),
        KeyBinding::new("ctrl-`", ToggleTerminal, None),
        KeyBinding::new("ctrl-j", ToggleTerminal, Some("Workspace")),
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
    // macOS: the same commands on Cmd, as Mac apps expect (Ctrl-Tab stays).
    // Bound after the Ctrl set, so menus and key hints show these.
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-n", NewThread, None),
        KeyBinding::new("cmd-k", OpenPalette, None),
        KeyBinding::new("cmd-shift-p", OpenPalette, None),
        KeyBinding::new("cmd-i", GoInbox, None),
        KeyBinding::new("cmd-shift-f", GoFleet, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-r", ToggleReview, Some("Workspace")),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-o", AddProject, None),
        KeyBinding::new("cmd-.", StopTurn, Some("Workspace")),
        KeyBinding::new("cmd-l", FocusComposer, Some("Workspace")),
        KeyBinding::new("cmd-j", ToggleTerminal, Some("Workspace")),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
}
