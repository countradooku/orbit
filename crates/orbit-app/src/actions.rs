//! Commands and their default key bindings. Every command here is reachable
//! from the command palette, and the important ones from a visible control
//! too. Users override bindings in `<data folder>/keybindings.json`
//! ([`crate::keymap`]).
//!
//! Bindings are written with `mod`, the platform's command modifier: Cmd on
//! macOS, Ctrl elsewhere.

use gpui_kit::*;

gpui_kit::actions!(
    orbit,
    [
        NewThread,
        OpenPalette,
        GoInbox,
        GoFleet,
        GoHome,
        /// The Automations page: scheduled and triggered runs.
        GoAutomations,
        ToggleSidebar,
        ToggleReview,
        OpenSettings,
        AddProject,
        StopTurn,
        NextThread,
        PreviousThread,
        FocusComposer,
        ToggleTerminal,
        ToggleBrowser,
        Quit,
        ZoomIn,
        ZoomOut,
        ResetZoom,
        CloseSettings,
        SendAlternate,
        /// Opens the next thread that needs you.
        NextAttention,
        /// The keyboard shortcuts sheet.
        ShowShortcuts,
        /// Re-reads `keybindings.json`.
        ReloadKeybindings,
        /// Opens `keybindings.json` in the system editor, creating it first.
        OpenKeybindings,
        /// The palette in command mode (`>`).
        RunCommand,
        /// The palette in message-search mode (`#`).
        SearchMessages,
        // The current thread.
        RenameThread,
        SettleThread,
        PinThread,
        ArchiveThread,
        /// Opens the current thread beside the others, as a tile.
        OpenBeside,
        CloseTile,
        CloseOtherTiles,
        MaximizeTile,
        FocusTileLeft,
        FocusTileRight,
        FocusTileUp,
        FocusTileDown,
        /// Clears the sidebar's multi-selection.
        ClearSelection,
        // The Needs-you deck.
        DeckNext,
        DeckPrevious,
        DeckApprove,
        DeckApproveAllOfKind,
        DeckDeny,
        DeckReply,
        DeckSnooze,
        DeckSettle,
        DeckOpen,
    ]
);

/// Picks option `index` (0-based) of the focused deck card: keys 1–9.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = orbit, no_json)]
pub struct DeckPick {
    pub index: usize,
}

/// Opens the `index`-th (0-based) thread in the sidebar's order: `mod-1`…`mod-9`.
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = orbit, no_json)]
pub struct JumpToThread {
    pub index: usize,
}

/// Opens a given thread beside the others, as a tile (the thread menu's
/// "Open beside").
#[derive(Clone, PartialEq, Debug, gpui_kit::Action)]
#[action(namespace = orbit, no_json)]
pub struct OpenThreadBeside {
    pub thread: u64,
}

/// Where a binding applies. Deck letters and `?` never fire while a text
/// field or the terminal has focus.
pub const IN_WORKSPACE: Option<&str> = Some("Workspace");
const OUTSIDE_TEXT: Option<&str> = Some("Workspace && !Input && !Terminal");
const IN_DECK: Option<&str> = Some("Deck && !Input");
const IN_TILES: Option<&str> = Some("Tiles");

/// One default binding: keystrokes (with `mod`), command and context.
pub struct DefaultBinding {
    pub keys: &'static str,
    pub action: Box<dyn Action>,
    pub context: Option<&'static str>,
}

fn bind(keys: &'static str, action: impl Action, context: Option<&'static str>) -> DefaultBinding {
    DefaultBinding { keys, action: Box::new(action), context }
}

/// Orbit's default bindings, in the order they are registered. When an
/// action has several, the last one is the one menus and hints show.
pub fn defaults() -> Vec<DefaultBinding> {
    let mut out = vec![
        bind("mod-n", NewThread, None),
        bind("mod-shift-p", OpenPalette, None),
        bind("mod-k", OpenPalette, None),
        bind("mod-i", GoInbox, None),
        bind("mod-shift-f", GoFleet, None),
        bind("mod-shift-u", GoAutomations, None),
        bind("mod-b", ToggleSidebar, None),
        bind("mod-r", ToggleReview, IN_WORKSPACE),
        bind("mod-,", OpenSettings, None),
        bind("mod-o", AddProject, None),
        bind("mod-.", StopTurn, IN_WORKSPACE),
        bind("ctrl-tab", NextThread, None),
        bind("ctrl-shift-tab", PreviousThread, None),
        bind("mod-shift-]", NextThread, None),
        bind("mod-shift-[", PreviousThread, None),
        bind("mod-shift-a", NextAttention, None),
        bind("mod-l", FocusComposer, IN_WORKSPACE),
        bind("ctrl-`", ToggleTerminal, None),
        bind("mod-j", ToggleTerminal, IN_WORKSPACE),
        bind("mod-q", Quit, None),
        bind("mod-+", ZoomIn, None),
        bind("mod-=", ZoomIn, None),
        bind("mod--", ZoomOut, None),
        bind("mod-0", ResetZoom, None),
        bind("escape", CloseSettings, Some("SettingsPage")),
        bind("mod-shift-enter", SendAlternate, Some("Composer")),
        bind("shift-/", ShowShortcuts, OUTSIDE_TEXT),
        bind("mod-/", ShowShortcuts, None),
        bind("f2", RenameThread, OUTSIDE_TEXT),
        bind("mod-shift-s", SettleThread, IN_WORKSPACE),
        bind("mod-alt-p", PinThread, IN_WORKSPACE),
        bind("mod-shift-\\", OpenBeside, IN_WORKSPACE),
        bind("escape", ClearSelection, OUTSIDE_TEXT),
        // Tiles.
        bind("mod-w", CloseTile, IN_TILES),
        bind("mod-alt-enter", MaximizeTile, IN_TILES),
        bind("mod-alt-left", FocusTileLeft, IN_TILES),
        bind("mod-alt-right", FocusTileRight, IN_TILES),
        bind("mod-alt-up", FocusTileUp, IN_TILES),
        bind("mod-alt-down", FocusTileDown, IN_TILES),
        // The deck: one key per decision.
        bind("down", DeckNext, IN_DECK),
        bind("j", DeckNext, IN_DECK),
        bind("up", DeckPrevious, IN_DECK),
        bind("k", DeckPrevious, IN_DECK),
        bind("a", DeckApprove, IN_DECK),
        bind("shift-a", DeckApproveAllOfKind, IN_DECK),
        bind("d", DeckDeny, IN_DECK),
        bind("r", DeckReply, IN_DECK),
        bind("s", DeckSnooze, IN_DECK),
        bind("e", DeckSettle, IN_DECK),
        bind("enter", DeckOpen, IN_DECK),
    ];
    const DIGITS: [&str; 9] = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];
    const JUMPS: [&str; 9] = ["mod-1", "mod-2", "mod-3", "mod-4", "mod-5", "mod-6", "mod-7", "mod-8", "mod-9"];
    for (index, keys) in DIGITS.into_iter().enumerate() {
        out.push(bind(keys, DeckPick { index }, IN_DECK));
    }
    for (index, keys) in JUMPS.into_iter().enumerate() {
        out.push(bind(keys, JumpToThread { index }, IN_WORKSPACE));
    }
    out
}

/// `mod-` as this platform spells it.
pub fn expand_mod(keys: &str) -> String {
    keys.replace("mod-", if cfg!(target_os = "macos") { "cmd-" } else { "ctrl-" })
}

/// The macOS or the other platforms' spelling of a binding. `KeyBinding::new`
/// parses statically, so it does not accept the `secondary-` alias.
pub const fn platform(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") { mac } else { other }
}

#[cfg(test)]
mod tests {
    use super::{defaults, expand_mod};
    use gpui_kit::{KeyBindingContextPredicate, Keystroke};

    #[test]
    fn mod_is_the_platform_modifier() {
        let expected = if cfg!(target_os = "macos") { "cmd-shift-p" } else { "ctrl-shift-p" };
        assert_eq!(expand_mod("mod-shift-p"), expected);
        // `mod--` is zoom out: the trailing `-` is the key.
        assert!(expand_mod("mod--").ends_with("--"));
        assert_eq!(expand_mod("ctrl-tab"), "ctrl-tab");
    }

    #[test]
    fn every_default_parses() {
        for binding in defaults() {
            let keys: &'static str = Box::leak(expand_mod(binding.keys).into_boxed_str());
            for stroke in keys.split_whitespace() {
                assert!(Keystroke::parse(stroke).is_ok(), "{} does not parse", binding.keys);
            }
            if let Some(context) = binding.context {
                assert!(KeyBindingContextPredicate::parse(context).is_ok(), "{context} does not parse");
            }
        }
    }
}
