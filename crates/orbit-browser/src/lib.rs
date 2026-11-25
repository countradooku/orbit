//! Orbit's preview browser: a native web page inside the GPUI Kit window.
//!
//! [`BrowserView`] is an address toolbar over a platform web view. It is
//! meant for previewing the dev server an agent runs: give it a URL, or let
//! [`detect_urls`] find one in terminal output and offer it:
//!
//! ```ignore
//! let browser = cx.new(|cx| BrowserView::new(window, cx));
//! browser.update(cx, |browser, cx| browser.set_suggested_urls(orbit_browser::detect_urls(&output), cx));
//! cx.subscribe(&browser, |this, _, event, cx| match event {
//!     BrowserEvent::TitleChanged(title) => this.set_tab_title(title.clone(), cx),
//!     BrowserEvent::UrlChanged(_) | BrowserEvent::LoadingChanged(_) => cx.notify(),
//! }).detach();
//! ```
//!
//! Call [`init`] once after `gpui_kit::init` to register the key bindings.
//!
//! # How the page is embedded
//!
//! A preview that hides GPUI's menus, popovers, dialogs and tooltips behind
//! the page is not usable, so the page must be *composited* with the window,
//! not laid over it as a child window. GPUI Fast's window composition does
//! that: a window presents its base content, then native surfaces, then its
//! overlays, each on its own platform layer. This crate puts the page in such
//! a native surface on every platform:
//!
//! | Platform | Web view | Embedding |
//! | --- | --- | --- |
//! | Windows | WebView2 (Edge) | A WebView2 *composition controller* renders into a DirectComposition visual that GPUI Fast stacks under its overlays. GPUI forwards mouse and wheel input to it; the keyboard reaches it directly. |
//! | macOS | WKWebView | `gpui-webview` (Wry) registers the view in the window's composition tree. |
//! | Linux | WebKitGTK | `gpui-webview` builds the view in a composition surface, an X11 child window under GPUI's overlay window. The app must run on X11 or XWayland: WebKitGTK cannot render into a Wayland surface. |
//!
//! Why not one crate for all three: `gpui-webview` (GPUI Kit's Wry
//! integration) embeds WebView2 as a child window, which covers every GPUI
//! overlay in its bounds and needs GPUI's DirectComposition turned off. GPUI
//! Fast's own `native_webview` example shows the composition-controller path
//! instead, which keeps DirectComposition and puts overlays above the page;
//! that is what the Windows backend does. On macOS and Linux `gpui-webview`
//! already uses GPUI Fast's composition, and Wry handles the engines.
//!
//! The `fast` feature (on by default) selects GPUI Fast. Without it, macOS
//! and Linux still embed the page, but over GPUI's overlays, and Windows
//! shows an explanation with "Open in system browser" instead of a page.
//!
//! # Lifetime and cost
//!
//! No web view, and so no browser process, exists until a view is first
//! rendered with an address. [`BrowserView::suspend`] releases it and keeps
//! the address; dropping the view releases it as well. WebView2 shares one
//! browser process per Orbit process, with its data under
//! `%LOCALAPPDATA%\Orbit\WebView2`.

mod address;
mod engine;
mod probe;
mod view;

pub use address::{AddressError, detect_urls, host_label, is_loopback, parse_address, port_url};
pub use url::Url;
pub use view::{BrowserEvent, BrowserView};

use gpui_kit::{App, KeyBinding, actions};

/// The key context of a [`BrowserView`]. Its bindings apply while GPUI has
/// the keyboard (the toolbar, the address field); the page takes keys first
/// while it has focus, and handles Back, Forward, Reload and its developer
/// tools itself. Ctrl+L reaches the address field from the page on Windows.
pub const KEY_CONTEXT: &str = "BrowserView";

actions!(
    browser,
    [
        /// Goes back in the page's history.
        Back,
        /// Goes forward in the page's history.
        Forward,
        /// Reloads the page, or tries a failed address again.
        Reload,
        /// Stops loading.
        Stop,
        /// Focuses the address field.
        FocusAddress,
        /// Opens the address in the system browser.
        OpenInSystemBrowser,
        /// Opens or closes the page's developer tools.
        ToggleDevTools,
    ]
);

/// Registers the browser's key bindings. Call once at startup, after
/// `gpui_kit::init` and before building menus.
pub fn init(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("alt-left", Back, context),
        KeyBinding::new("alt-right", Forward, context),
        KeyBinding::new("f5", Reload, context),
        KeyBinding::new("f12", ToggleDevTools, context),
    ]);
    // `KeyBinding::new` does not resolve `secondary-`, so spell it per OS.
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-r", Reload, context),
        KeyBinding::new("cmd-l", FocusAddress, context),
        KeyBinding::new("cmd-shift-o", OpenInSystemBrowser, context),
        KeyBinding::new("cmd-[", Back, context),
        KeyBinding::new("cmd-]", Forward, context),
        KeyBinding::new("cmd-alt-i", ToggleDevTools, context),
    ]);
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([
        KeyBinding::new("ctrl-r", Reload, context),
        KeyBinding::new("ctrl-l", FocusAddress, context),
        KeyBinding::new("ctrl-shift-o", OpenInSystemBrowser, context),
        KeyBinding::new("ctrl-shift-i", ToggleDevTools, context),
    ]);
}
