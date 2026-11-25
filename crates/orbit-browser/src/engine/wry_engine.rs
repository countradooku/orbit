//! macOS and Linux: WKWebView or WebKitGTK through Wry, hosted by
//! `gpui-webview`.
//!
//! With the `fast` feature, `gpui-webview` puts the native view in a GPUI
//! Fast composition surface, so GPUI overlays draw above the page: the view
//! itself on macOS, an X11 child window that GPUI cuts its overlays out of
//! (or blends them over, with a compositing manager) on Linux. WebKitGTK
//! cannot embed into a Wayland surface, so on Linux the application must run
//! on X11 or XWayland; on a Wayland window creation fails and the view shows
//! the error.
//!
//! Wry reports page loads and title changes but neither load failures nor
//! history, so the view probes local servers itself and keeps Back and
//! Forward available.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::{AnyElement, App, AppContext as _, Entity, IntoElement, Window, div};
use gpui_webview::WebView;
use url::Url;
use wry::{NewWindowResponse, PageLoadEvent, WebViewBuilder};

use super::{EngineEvent, EngineEvents, report};

pub(crate) struct Engine {
    view: Option<Entity<WebView>>,
    devtools_open: Rc<Cell<bool>>,
}

impl Engine {
    pub(crate) fn new(initial: Option<&Url>, events: EngineEvents, window: &mut Window, cx: &mut App) -> Self {
        let mut builder = WebViewBuilder::new()
            .with_devtools(true)
            .with_on_page_load_handler({
                let events = events.clone();
                move |event, url| match event {
                    PageLoadEvent::Started => {
                        report(&events, EngineEvent::NavigationStarted);
                        report(&events, EngineEvent::UrlChanged(url));
                    }
                    PageLoadEvent::Finished => {
                        report(&events, EngineEvent::UrlChanged(url));
                        report(&events, EngineEvent::NavigationFinished(None));
                    }
                }
            })
            .with_document_title_changed_handler({
                let events = events.clone();
                move |title| report(&events, EngineEvent::TitleChanged(title))
            })
            .with_new_window_req_handler({
                // The handler must be `Sync`; WebKit calls it on the UI thread.
                let events = std::sync::Mutex::new(events.clone());
                move |url, _| {
                    if let Ok(events) = events.lock() {
                        report(&events, EngineEvent::NewWindowRequested(url));
                    }
                    NewWindowResponse::Deny
                }
            });
        if let Some(url) = initial {
            builder = builder.with_url(url.as_str());
        }
        match WebView::build(builder, window, cx) {
            Ok(view) => {
                report(&events, EngineEvent::Ready);
                Self { view: Some(cx.new(|_| view)), devtools_open: Rc::default() }
            }
            Err(error) => {
                report(&events, EngineEvent::Unavailable(error.to_string()));
                Self { view: None, devtools_open: Rc::default() }
            }
        }
    }

    pub(crate) fn navigate(&self, url: &Url, cx: &mut App) {
        self.update(cx, |view| view.load_url(url.as_str()));
    }

    pub(crate) fn back(&self, cx: &mut App) {
        self.update(cx, |view| {
            let _ = view.back();
        });
    }

    pub(crate) fn forward(&self, cx: &mut App) {
        self.update(cx, |view| {
            let _ = view.forward();
        });
    }

    pub(crate) fn reload(&self, cx: &mut App) {
        self.update(cx, |view| {
            let _ = view.raw().reload();
        });
    }

    pub(crate) fn stop(&self, cx: &mut App) {
        self.update(cx, |view| {
            let _ = view.raw().evaluate_script("window.stop();");
        });
    }

    /// Opens or closes the inspector and returns whether it is open.
    pub(crate) fn toggle_devtools(&self, cx: &mut App) -> Option<bool> {
        let devtools_open = self.devtools_open.clone();
        self.update(cx, move |view| {
            let open = !view.raw().is_devtools_open();
            if open {
                view.raw().open_devtools();
            } else {
                view.raw().close_devtools();
            }
            devtools_open.set(open);
        });
        Some(self.devtools_open.get())
    }

    pub(crate) fn tracks_history(&self) -> bool {
        false
    }

    pub(crate) fn set_visible(&self, visible: bool, cx: &mut App) {
        self.update(cx, |view| {
            let _ = view.set_visible(visible);
        });
    }

    pub(crate) fn focus_parent(&self) {}

    pub(crate) fn page(&self) -> AnyElement {
        match &self.view {
            Some(view) => view.clone().into_any_element(),
            None => div().into_any_element(),
        }
    }

    /// Drops the native view, which `gpui-webview` hides and detaches.
    pub(crate) fn release(mut self, cx: &mut App) {
        if let Some(view) = self.view.take() {
            view.update(cx, |view, _| view.hide());
        }
    }

    fn update(&self, cx: &mut App, f: impl FnOnce(&mut WebView)) {
        if let Some(view) = &self.view {
            view.update(cx, |view, _| f(view));
        }
    }
}
