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
        ZoomIn,
        ZoomOut,
        ResetZoom,
        CloseSettings,
        SendAlternate,
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
        KeyBinding::new(platform("cmd-=", "ctrl-="), ZoomIn, None),
        KeyBinding::new(platform("cmd-+", "ctrl-+"), ZoomIn, None),
        KeyBinding::new(platform("cmd--", "ctrl--"), ZoomOut, None),
        KeyBinding::new(platform("cmd-0", "ctrl-0"), ResetZoom, None),
        KeyBinding::new("escape", CloseSettings, Some("SettingsPage")),
        KeyBinding::new(platform("cmd-shift-enter", "ctrl-shift-enter"), SendAlternate, Some("Composer")),
    ]);
}

/// The macOS or the other platforms' spelling of a binding. `KeyBinding::new`
/// parses statically, so it does not accept the `secondary-` alias.
pub const fn platform(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") { mac } else { other }
}
