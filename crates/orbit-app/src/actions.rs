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
        KeyBinding::new("ctrl-q", Quit, None),
    ]);
}
