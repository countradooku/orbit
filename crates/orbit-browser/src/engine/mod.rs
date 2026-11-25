//! The platform web view behind a [`BrowserView`](crate::BrowserView).
//!
//! Each backend exposes the same surface: create it for a window, give it
//! commands, render its page element, and read what it reports through
//! [`EngineEvent`]s sent on a channel. Platform callbacks may run while GPUI
//! is borrowed (WebView2 pumps messages from inside GPUI calls), so they
//! never touch entities: they only send events, which the view applies from
//! a GPUI task.

#[cfg(all(target_os = "windows", feature = "fast"))]
mod webview2;
#[cfg(all(target_os = "windows", feature = "fast"))]
pub(crate) use webview2::Engine;

#[cfg(not(target_os = "windows"))]
mod wry_engine;
#[cfg(not(target_os = "windows"))]
pub(crate) use wry_engine::Engine;

#[cfg(all(target_os = "windows", not(feature = "fast")))]
mod unsupported;
#[cfg(all(target_os = "windows", not(feature = "fast")))]
pub(crate) use unsupported::Engine;

use futures::channel::mpsc::UnboundedSender;
use gpui_kit::CursorStyle;

/// What the platform web view reports. Wry reports only part of it.
#[cfg_attr(not(all(target_os = "windows", feature = "fast")), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum EngineEvent {
    /// The web view exists and takes commands.
    Ready,
    /// The web view could not be created.
    Unavailable(String),
    /// A navigation of the main frame began.
    NavigationStarted,
    /// A navigation of the main frame ended, with the reason when it failed.
    NavigationFinished(Option<NavigationError>),
    /// The page's address changed: a navigation, a redirect or `history.pushState`.
    UrlChanged(String),
    /// The page's `<title>` changed.
    TitleChanged(String),
    /// Whether there is history to go back or forward to.
    HistoryChanged { can_go_back: bool, can_go_forward: bool },
    /// The page asked for another window, as a `target="_blank"` link does.
    NewWindowRequested(String),
    /// The cursor the page shows under the pointer, or `None` once the
    /// pointer has left the page. Only web views GPUI forwards input to report it.
    CursorChanged(Option<CursorStyle>),
    /// The page's renderer or browser process went away.
    Crashed,
    /// The page took keyboard focus.
    PageFocused,
    /// The address-field shortcut was pressed while the page had the keyboard.
    AddressFocusRequested,
}

/// Why a navigation failed, in terms the empty state can explain.
#[cfg_attr(not(all(target_os = "windows", feature = "fast")), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NavigationError {
    /// The connection was refused or could not be made.
    CannotConnect,
    /// The host name did not resolve.
    HostNotFound,
    /// The server did not answer in time.
    TimedOut,
    /// The server's certificate is not trusted.
    Certificate,
    /// The connection dropped while loading.
    ConnectionLost,
    /// Anything else the engine reports.
    Other,
}

impl NavigationError {
    /// A sentence for the error state, saying what happened.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::CannotConnect => "The server refused the connection.",
            Self::HostNotFound => "The address couldn’t be found. Check it for typos.",
            Self::TimedOut => "The server took too long to answer.",
            Self::Certificate => "The site’s certificate isn’t trusted.",
            Self::ConnectionLost => "The connection was lost while the page loaded.",
            Self::Other => "The page couldn’t be loaded.",
        }
    }
}

/// Where the backend sends what it reports.
pub(crate) type EngineEvents = UnboundedSender<EngineEvent>;

/// Sends an event, ignoring a view that has gone away.
pub(crate) fn report(events: &EngineEvents, event: EngineEvent) {
    let _ = events.unbounded_send(event);
}
