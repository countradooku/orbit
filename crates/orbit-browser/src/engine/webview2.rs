//! Windows: a WebView2 composition controller in a GPUI Fast composition surface.
//!
//! GPUI Fast composes a window from DirectComposition visuals: its base
//! content, the native surfaces created through
//! [`Window::enable_window_composition`], then its overlays (menus,
//! popovers, dialogs, tooltips). WebView2's composition ("visual hosting")
//! controller renders the page into one of those native visuals instead of a
//! child window, so GPUI overlays draw above the page and the page is
//! clipped and moved with its element like any other content.
//!
//! A visual takes no input of its own: GPUI hands this module the mouse and
//! wheel input that lands on the page ([`WindowComposition::on_input`]), and
//! it forwards it with `SendMouseInput`. The keyboard reaches WebView2
//! directly once it has focus. The page's cursor is reported back so the
//! element can show it.
//!
//! Creation is asynchronous and never blocks the UI thread: the environment
//! (one per process, shared by every view) and the controller are created
//! with WebView2's completion callbacks, which run on the UI thread from the
//! message loop and only send results to a GPUI task.

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::{Rc, Weak},
};

use anyhow::{Context as _, Result, anyhow};
use futures::channel::oneshot;
use gpui_kit::{
    AnyElement, AnyWindowHandle, App, Bounds, CursorStyle, DispatchEventResult, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Modifiers, MouseButton, MouseDownEvent, NavigationDirection, Pixels,
    PlatformInput, ScrollDelta, Style, Task, Window, WindowCompositionSurface, relative,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use url::Url;
use webview2_com::{
    AcceleratorKeyPressedEventHandler, CoTaskMemPWSTR, CoreWebView2EnvironmentOptions,
    CreateCoreWebView2CompositionControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler,
    CursorChangedEventHandler, DocumentTitleChangedEventHandler, FocusChangedEventHandler, HistoryChangedEventHandler,
    Microsoft::Web::WebView2::Win32::*, NavigationCompletedEventHandler, NavigationStartingEventHandler,
    NewWindowRequestedEventHandler, ProcessFailedEventHandler, SourceChangedEventHandler,
};
use windows::{
    Win32::{
        Foundation::{E_POINTER, HWND, POINT, RECT},
        UI::{
            Input::KeyboardAndMouse::{GetFocus, GetKeyState, SetFocus, VK_CONTROL},
            WindowsAndMessaging::{
                IDC_CROSS, IDC_HAND, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE,
                IDC_SIZEWE, XBUTTON1, XBUTTON2,
            },
        },
    },
    core::{BOOL, HSTRING, IUnknown, Interface, PCWSTR, PWSTR},
};

use super::{EngineEvent, EngineEvents, NavigationError, report};

/// The WebView2 page of one [`BrowserView`](crate::BrowserView).
pub(crate) struct Engine {
    shared: Rc<Shared>,
    _create: Task<()>,
}

/// What the page element, the input handler and WebView2's callbacks share.
struct Shared {
    hwnd: HWND,
    window: AnyWindowHandle,
    events: EngineEvents,
    surface: Option<WindowCompositionSurface>,
    web: RefCell<Option<Web>>,
    /// The page's bounds in window logical pixels and the window's scale,
    /// as last laid out.
    geometry: Cell<(Bounds<Pixels>, f32)>,
    /// What to load once the controller exists.
    pending: RefCell<Option<String>>,
    visible: Cell<bool>,
    /// The cursor the page asked for.
    cursor: Cell<CursorStyle>,
    /// Whether the pointer is over the page.
    hovered: Cell<bool>,
    /// The mouse buttons WebView2 was told are down. GPUI can report the
    /// same press twice; WebView2 must see it once.
    pressed: Cell<u8>,
}

struct Web {
    composition: ICoreWebView2CompositionController,
    controller: ICoreWebView2Controller,
    webview: ICoreWebView2,
}

thread_local! {
    /// Composition surfaces of released pages, kept for the next page in the
    /// same window. GPUI Fast 0.1.19 cannot remove a native surface on
    /// Windows (hiding one, which removal starts with, casts its visual to
    /// `IDCompositionVisual3`, which GPUI's DirectComposition device does not
    /// provide), so surfaces are emptied and reused instead of removed.
    static SPARE_SURFACES: RefCell<Vec<(AnyWindowHandle, WindowCompositionSurface)>> = const { RefCell::new(Vec::new()) };
}

impl Engine {
    /// Creates the composition surface now and the WebView2 controller
    /// asynchronously; [`EngineEvent::Ready`] or [`EngineEvent::Unavailable`]
    /// follows. `initial` loads as soon as the controller exists.
    pub(crate) fn new(initial: Option<&Url>, events: EngineEvents, window: &mut Window, cx: &mut App) -> Self {
        let shared = match Shared::new(window, events.clone(), cx) {
            Ok(shared) => Rc::new(shared),
            Err(error) => {
                report(&events, EngineEvent::Unavailable(format!("{error:#}")));
                let shared = Rc::new(Shared::detached(Window::window_handle(window), events));
                return Self { shared, _create: Task::ready(()) };
            }
        };
        *shared.pending.borrow_mut() = initial.map(|url| url.to_string());

        if let Err(error) = shared.forward_input(window) {
            log::warn!("WebView2 input forwarding unavailable: {error:#}");
        }

        let weak = Rc::downgrade(&shared);
        let create = cx.spawn(async move |_| {
            let result = create_web(weak.clone()).await;
            let Some(shared) = weak.upgrade() else { return };
            match result {
                Ok(()) => report(&shared.events, EngineEvent::Ready),
                Err(error) => report(&shared.events, EngineEvent::Unavailable(format!("{error:#}"))),
            }
        });
        Self { shared, _create: create }
    }

    pub(crate) fn navigate(&self, url: &Url, _: &mut App) {
        let web = self.shared.web.borrow();
        let Some(web) = web.as_ref() else {
            *self.shared.pending.borrow_mut() = Some(url.to_string());
            return;
        };
        let uri = HSTRING::from(url.as_str());
        if let Err(error) = unsafe { web.webview.Navigate(&uri) } {
            log::error!("WebView2 navigation to {url} failed: {error}");
        }
    }

    pub(crate) fn back(&self, _: &mut App) {
        self.with_webview(|webview| unsafe { webview.GoBack() });
    }

    pub(crate) fn forward(&self, _: &mut App) {
        self.with_webview(|webview| unsafe { webview.GoForward() });
    }

    pub(crate) fn reload(&self, _: &mut App) {
        self.with_webview(|webview| unsafe { webview.Reload() });
    }

    pub(crate) fn stop(&self, _: &mut App) {
        self.with_webview(|webview| unsafe { webview.Stop() });
    }

    /// Opens the developer tools in their own window. WebView2 has no call
    /// to close them, so the state is not tracked: `None`.
    pub(crate) fn toggle_devtools(&self, _: &mut App) -> Option<bool> {
        self.with_webview(|webview| unsafe { webview.OpenDevToolsWindow() });
        None
    }

    /// Whether the engine reports [`EngineEvent::HistoryChanged`].
    pub(crate) fn tracks_history(&self) -> bool {
        true
    }

    /// Shows or hides the page. A hidden page keeps its document but lets
    /// WebView2 throttle it.
    pub(crate) fn set_visible(&self, visible: bool, _: &mut App) {
        self.shared.visible.set(visible);
        let (bounds, scale) = self.shared.geometry.get();
        self.shared.place(bounds, scale);
    }

    /// Returns keyboard focus to the GPUI window when the page has it.
    pub(crate) fn focus_parent(&self) {
        self.shared.focus_parent();
    }

    /// The element showing the page; it fills its parent.
    pub(crate) fn page(&self) -> AnyElement {
        PageElement { shared: self.shared.clone() }.into_any_element()
    }

    /// Closes the web view and keeps its emptied surface for the next page.
    pub(crate) fn release(self, _: &mut App) {
        // Dropping does it.
    }

    fn with_webview(&self, command: impl FnOnce(&ICoreWebView2) -> windows::core::Result<()>) {
        if let Some(web) = self.shared.web.borrow().as_ref()
            && let Err(error) = command(&web.webview)
        {
            log::error!("WebView2 command failed: {error}");
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shared.close();
    }
}

impl Shared {
    fn new(window: &mut Window, events: EngineEvents, cx: &App) -> Result<Self> {
        let hwnd = match HasWindowHandle::window_handle(window).map_err(|error| anyhow!("{error:?}"))?.as_raw() {
            RawWindowHandle::Win32(handle) => HWND(handle.hwnd.get() as _),
            _ => anyhow::bail!("the window is not a Win32 window"),
        };
        let handle = Window::window_handle(window);
        let spare = SPARE_SURFACES.with_borrow_mut(|spares| {
            // Forget the surfaces of windows that have closed.
            let open = cx.windows();
            spares.retain(|(window, _)| open.contains(window));
            let ix = spares.iter().position(|(window, _)| *window == handle)?;
            Some(spares.swap_remove(ix).1)
        });
        let surface = match spare {
            Some(surface) => surface,
            None => window
                .enable_window_composition()
                .context("GPUI Fast window composition is unavailable")?
                .create_native_surface()
                .context("creating the page's composition surface")?,
        };
        Ok(Self {
            hwnd,
            window: handle,
            events,
            surface: Some(surface),
            web: RefCell::default(),
            geometry: Cell::new((Bounds::default(), window.scale_factor())),
            pending: RefCell::default(),
            visible: Cell::new(true),
            cursor: Cell::new(CursorStyle::Arrow),
            hovered: Cell::new(false),
            pressed: Cell::new(0),
        })
    }

    /// A stand-in for a web view that could not be created.
    fn detached(window: AnyWindowHandle, events: EngineEvents) -> Self {
        Self {
            hwnd: HWND::default(),
            window,
            events,
            surface: None,
            web: RefCell::default(),
            geometry: Cell::new((Bounds::default(), 1.)),
            pending: RefCell::default(),
            visible: Cell::new(false),
            cursor: Cell::new(CursorStyle::Arrow),
            hovered: Cell::new(false),
            pressed: Cell::new(0),
        }
    }

    /// Has GPUI hand the mouse and wheel input over the page to WebView2.
    fn forward_input(self: &Rc<Self>, window: &mut Window) -> Result<()> {
        let surface = self.surface.as_ref().context("no surface")?.id();
        let weak = Rc::downgrade(self);
        window.enable_window_composition()?.on_input(surface, move |event| match weak.upgrade() {
            Some(shared) => shared.receive_input(event),
            None => DispatchEventResult { propagate: true, default_prevented: false },
        })
    }

    /// Puts the surface at `bounds` while the page is shown and ready, and
    /// empties it otherwise: an empty surface shows nothing and takes no input.
    fn place(&self, bounds: Bounds<Pixels>, scale_factor: f32) {
        self.geometry.set((bounds, scale_factor));
        let Some(surface) = &self.surface else { return };
        let web = self.web.borrow();
        let shown = self.visible.get() && web.is_some();
        let device = if shown { bounds.to_device_pixels(scale_factor) } else { Bounds::default() };
        if let Err(error) = surface.platform_surface().and_then(|surface| surface.set_bounds(device)) {
            log::error!("placing the WebView2 surface: {error:#}");
        }
        if let Some(web) = web.as_ref() {
            unsafe {
                if shown {
                    let size = bounds.to_device_pixels(scale_factor).size;
                    let rect = RECT { left: 0, top: 0, right: size.width.0, bottom: size.height.0 };
                    if let Err(error) = web.controller.SetBounds(rect) {
                        log::error!("sizing WebView2: {error}");
                    }
                }
                let _ = web.controller.SetIsVisible(self.visible.get());
            }
        }
        if !shown {
            self.focus_parent();
        }
    }

    fn focus_parent(&self) {
        if self.hwnd.is_invalid() {
            return;
        }
        unsafe {
            if GetFocus() != self.hwnd {
                let _ = SetFocus(Some(self.hwnd));
            }
        }
    }

    fn close(&self) {
        if let Some(web) = self.web.borrow_mut().take()
            && let Err(error) = unsafe { web.controller.Close() }
        {
            log::warn!("closing WebView2: {error}");
        }
        self.pending.borrow_mut().take();
        self.visible.set(false);
        if let Some(surface) = &self.surface {
            let _ = surface.platform_surface().and_then(|surface| surface.set_bounds(Bounds::default()));
            SPARE_SURFACES.with_borrow_mut(|spares| spares.push((self.window, surface.clone())));
        }
        self.focus_parent();
    }

    fn receive_input(&self, event: &PlatformInput) -> DispatchEventResult {
        let handled = DispatchEventResult { propagate: false, default_prevented: false };
        let web = self.web.borrow();
        let Some(web) = web.as_ref() else { return handled };
        match event {
            PlatformInput::MouseDown(event) => {
                let bit = button_bit(event.button);
                if self.pressed.get() & bit != 0 {
                    return handled;
                }
                self.pressed.set(self.pressed.get() | bit);
                // Keyboard input follows a click into the page, as in a browser.
                if unsafe { GetFocus() } == self.hwnd {
                    let _ = unsafe { web.controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC) };
                }
                let kind = match event.button {
                    MouseButton::Left => COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_DOWN,
                    MouseButton::Right => COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_DOWN,
                    MouseButton::Middle => COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_DOWN,
                    MouseButton::Navigate(_) => COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_DOWN,
                };
                self.send_mouse(
                    web,
                    kind,
                    Some(event.button),
                    event.modifiers,
                    x_button_data(event.button),
                    event.position,
                );
            }
            PlatformInput::MouseUp(event) => {
                self.pressed.set(self.pressed.get() & !button_bit(event.button));
                let kind = match event.button {
                    MouseButton::Left => COREWEBVIEW2_MOUSE_EVENT_KIND_LEFT_BUTTON_UP,
                    MouseButton::Right => COREWEBVIEW2_MOUSE_EVENT_KIND_RIGHT_BUTTON_UP,
                    MouseButton::Middle => COREWEBVIEW2_MOUSE_EVENT_KIND_MIDDLE_BUTTON_UP,
                    MouseButton::Navigate(_) => COREWEBVIEW2_MOUSE_EVENT_KIND_X_BUTTON_UP,
                };
                self.send_mouse(web, kind, None, event.modifiers, x_button_data(event.button), event.position);
            }
            PlatformInput::MouseMove(event) => {
                if !self.hovered.replace(true) {
                    report(&self.events, EngineEvent::CursorChanged(Some(self.cursor.get())));
                }
                self.send_mouse(
                    web,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_MOVE,
                    event.pressed_button,
                    event.modifiers,
                    0,
                    event.position,
                );
            }
            PlatformInput::MouseExited(event) => {
                if self.hovered.replace(false) {
                    report(&self.events, EngineEvent::CursorChanged(None));
                }
                self.send_mouse(
                    web,
                    COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE,
                    event.pressed_button,
                    event.modifiers,
                    0,
                    event.position,
                );
            }
            PlatformInput::Scroll(event) => {
                let amount = match event.delta {
                    ScrollDelta::Pixels(delta) => delta.map(f32::from),
                    // WHEEL_DELTA is one notch, which Windows scrolls by three lines.
                    ScrollDelta::Lines(delta) => delta * (120. / 3.),
                };
                for (kind, amount) in [
                    (COREWEBVIEW2_MOUSE_EVENT_KIND_WHEEL, amount.y),
                    (COREWEBVIEW2_MOUSE_EVENT_KIND_HORIZONTAL_WHEEL, -amount.x),
                ] {
                    if amount != 0. {
                        self.send_mouse(web, kind, None, event.modifiers, amount as i32 as u32, event.position);
                    }
                }
            }
            _ => return DispatchEventResult { propagate: true, default_prevented: false },
        }
        handled
    }

    fn send_mouse(
        &self,
        web: &Web,
        kind: COREWEBVIEW2_MOUSE_EVENT_KIND,
        button: Option<MouseButton>,
        modifiers: Modifiers,
        data: u32,
        position: gpui_kit::Point<Pixels>,
    ) {
        let (bounds, scale) = self.geometry.get();
        let mut keys = COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_NONE;
        if modifiers.control {
            keys |= COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_CONTROL;
        }
        if modifiers.shift {
            keys |= COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_SHIFT;
        }
        if let Some(button) = button {
            keys |= match button {
                MouseButton::Left => COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_LEFT_BUTTON,
                MouseButton::Right => COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_RIGHT_BUTTON,
                MouseButton::Middle => COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_MIDDLE_BUTTON,
                MouseButton::Navigate(NavigationDirection::Back) => COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_X_BUTTON1,
                MouseButton::Navigate(NavigationDirection::Forward) => COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS_X_BUTTON2,
            };
        }
        // WebView2 rejects a leave that carries a position.
        let point = if kind == COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE {
            POINT::default()
        } else {
            POINT {
                x: (f32::from(position.x - bounds.origin.x) * scale) as i32,
                y: (f32::from(position.y - bounds.origin.y) * scale) as i32,
            }
        };
        if let Err(error) = unsafe { web.composition.SendMouseInput(kind, keys, data, point) } {
            log::error!("forwarding mouse input to WebView2: {error}");
        }
    }
}

fn button_bit(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 4,
        MouseButton::Navigate(NavigationDirection::Back) => 8,
        MouseButton::Navigate(NavigationDirection::Forward) => 16,
    }
}

fn x_button_data(button: MouseButton) -> u32 {
    match button {
        MouseButton::Navigate(NavigationDirection::Back) => u32::from(XBUTTON1),
        MouseButton::Navigate(NavigationDirection::Forward) => u32::from(XBUTTON2),
        _ => 0,
    }
}

thread_local! {
    /// The process's WebView2 environment: one browser process shared by every view.
    static ENVIRONMENT: RefCell<Option<ICoreWebView2Environment3>> = const { RefCell::new(None) };
}

/// Where WebView2 keeps cookies, cache and storage: `%LOCALAPPDATA%\Orbit\WebView2`.
fn user_data_folder() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Orbit")
        .join("WebView2")
}

async fn environment() -> Result<ICoreWebView2Environment3> {
    if let Some(environment) = ENVIRONMENT.with_borrow(Clone::clone) {
        return Ok(environment);
    }
    let (tx, rx) = oneshot::channel();
    let options = CoreWebView2EnvironmentOptions::default();
    unsafe { options.set_scroll_bar_style(COREWEBVIEW2_SCROLLBAR_STYLE_FLUENT_OVERLAY) };
    let folder = HSTRING::from(user_data_folder().as_os_str());
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, environment| {
        let _ = tx.send(result.and_then(|()| environment.ok_or_else(|| E_POINTER.into())));
        Ok(())
    }));
    unsafe {
        CreateCoreWebView2EnvironmentWithOptions(
            PCWSTR::null(),
            &folder,
            &ICoreWebView2EnvironmentOptions::from(options),
            &handler,
        )
    }
    .context("Microsoft Edge WebView2 Runtime is not installed")?;
    let environment = rx
        .await
        .context("WebView2 environment creation was abandoned")?
        .context("creating the WebView2 environment")?;
    let environment = environment.cast::<ICoreWebView2Environment3>().context("the WebView2 Runtime is too old")?;
    ENVIRONMENT.set(Some(environment.clone()));
    Ok(environment)
}

/// Creates the controller for `shared`'s surface and starts loading.
async fn create_web(shared: Weak<Shared>) -> Result<()> {
    let environment = environment().await?;
    let hwnd = shared.upgrade().context("closed")?.hwnd;
    let (tx, rx) = oneshot::channel();
    let handler =
        CreateCoreWebView2CompositionControllerCompletedHandler::create(Box::new(move |result, controller| {
            let _ = tx.send(result.and_then(|()| controller.ok_or_else(|| E_POINTER.into())));
            Ok(())
        }));
    unsafe { environment.CreateCoreWebView2CompositionController(hwnd, &handler) }
        .context("starting the WebView2 controller")?;
    let composition =
        rx.await.context("WebView2 controller creation was abandoned")?.context("creating the WebView2 controller")?;

    let Some(shared) = shared.upgrade() else {
        // The view went away while the controller was created.
        let _ = unsafe { composition.cast::<ICoreWebView2Controller>()?.Close() };
        return Ok(());
    };
    let surface = shared.surface.as_ref().context("no surface")?;
    let visual = surface.platform_surface()?.platform_handle()?;
    let visual = visual.downcast::<IUnknown>().map_err(|_| anyhow!("the surface is not a DirectComposition visual"))?;
    unsafe { composition.SetRootVisualTarget(&*visual) }.context("attaching WebView2 to its visual")?;
    let controller = composition.cast::<ICoreWebView2Controller>().context("WebView2 controller")?;
    let webview = unsafe { controller.CoreWebView2() }.context("WebView2 core")?;

    let settings = unsafe { webview.Settings() }?;
    unsafe {
        settings.SetAreDevToolsEnabled(true)?;
        // Orbit draws its own error states.
        settings.SetIsBuiltInErrorPageEnabled(false)?;
    }
    subscribe(&shared, &composition, &controller, &webview).context("subscribing to WebView2 events")?;

    *shared.web.borrow_mut() = Some(Web { composition, controller, webview: webview.clone() });
    let (bounds, scale) = shared.geometry.get();
    shared.place(bounds, scale);
    if let Some(url) = shared.pending.borrow_mut().take() {
        unsafe { webview.Navigate(&HSTRING::from(url)) }?;
    }
    Ok(())
}

/// Routes WebView2's events to the view's channel.
fn subscribe(
    shared: &Rc<Shared>,
    composition: &ICoreWebView2CompositionController,
    controller: &ICoreWebView2Controller,
    webview: &ICoreWebView2,
) -> Result<()> {
    let mut token = 0i64;
    unsafe {
        let events = shared.events.clone();
        webview.add_NavigationStarting(
            &NavigationStartingEventHandler::create(Box::new(move |_, _| {
                report(&events, EngineEvent::NavigationStarted);
                Ok(())
            })),
            &mut token,
        )?;

        let events = shared.events.clone();
        webview.add_NavigationCompleted(
            &NavigationCompletedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut success = BOOL::default();
                args.IsSuccess(&mut success)?;
                let mut status = COREWEBVIEW2_WEB_ERROR_STATUS_UNKNOWN;
                args.WebErrorStatus(&mut status)?;
                let error = (!success.as_bool()).then(|| navigation_error(status)).flatten();
                report(&events, EngineEvent::NavigationFinished(error));
                Ok(())
            })),
            &mut token,
        )?;

        let events = shared.events.clone();
        webview.add_SourceChanged(
            &SourceChangedEventHandler::create(Box::new(move |sender, _| {
                if let Some(webview) = sender {
                    let mut source = PWSTR::null();
                    webview.Source(&mut source)?;
                    report(&events, EngineEvent::UrlChanged(CoTaskMemPWSTR::from(source).to_string()));
                }
                Ok(())
            })),
            &mut token,
        )?;

        let events = shared.events.clone();
        webview.add_DocumentTitleChanged(
            &DocumentTitleChangedEventHandler::create(Box::new(move |sender, _| {
                if let Some(webview) = sender {
                    let mut title = PWSTR::null();
                    webview.DocumentTitle(&mut title)?;
                    report(&events, EngineEvent::TitleChanged(CoTaskMemPWSTR::from(title).to_string()));
                }
                Ok(())
            })),
            &mut token,
        )?;

        let events = shared.events.clone();
        webview.add_HistoryChanged(
            &HistoryChangedEventHandler::create(Box::new(move |sender, _| {
                if let Some(webview) = sender {
                    let (mut back, mut forward) = (BOOL::default(), BOOL::default());
                    webview.CanGoBack(&mut back)?;
                    webview.CanGoForward(&mut forward)?;
                    report(
                        &events,
                        EngineEvent::HistoryChanged { can_go_back: back.as_bool(), can_go_forward: forward.as_bool() },
                    );
                }
                Ok(())
            })),
            &mut token,
        )?;

        let events = shared.events.clone();
        webview.add_NewWindowRequested(
            &NewWindowRequestedEventHandler::create(Box::new(move |_, args| {
                if let Some(args) = args {
                    let mut uri = PWSTR::null();
                    args.Uri(&mut uri)?;
                    args.SetHandled(true)?;
                    report(&events, EngineEvent::NewWindowRequested(CoTaskMemPWSTR::from(uri).to_string()));
                }
                Ok(())
            })),
            &mut token,
        )?;

        let events = shared.events.clone();
        webview.add_ProcessFailed(
            &ProcessFailedEventHandler::create(Box::new(move |_, _| {
                report(&events, EngineEvent::Crashed);
                Ok(())
            })),
            &mut token,
        )?;

        let weak = Rc::downgrade(shared);
        composition.add_CursorChanged(
            &CursorChangedEventHandler::create(Box::new(move |sender, _| {
                let (Some(shared), Some(composition)) = (weak.upgrade(), sender) else { return Ok(()) };
                let mut id = 0u32;
                composition.SystemCursorId(&mut id)?;
                let cursor = cursor_style(id);
                shared.cursor.set(cursor);
                if shared.hovered.get() {
                    report(&shared.events, EngineEvent::CursorChanged(Some(cursor)));
                }
                Ok(())
            })),
            &mut token,
        )?;

        // Ctrl+L focuses the address field even while the page has the keyboard.
        let events = shared.events.clone();
        controller.add_AcceleratorKeyPressed(
            &AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()) };
                let mut kind = COREWEBVIEW2_KEY_EVENT_KIND_KEY_UP;
                args.KeyEventKind(&mut kind)?;
                let mut key = 0u32;
                args.VirtualKey(&mut key)?;
                let control = GetKeyState(i32::from(VK_CONTROL.0)) < 0;
                if kind == COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN && control && key == u32::from(b'L') {
                    args.SetHandled(true)?;
                    report(&events, EngineEvent::AddressFocusRequested);
                }
                Ok(())
            })),
            &mut token,
        )?;

        let events = shared.events.clone();
        controller.add_GotFocus(
            &FocusChangedEventHandler::create(Box::new(move |_, _| {
                report(&events, EngineEvent::PageFocused);
                Ok(())
            })),
            &mut token,
        )?;
    }
    Ok(())
}

fn navigation_error(status: COREWEBVIEW2_WEB_ERROR_STATUS) -> Option<NavigationError> {
    Some(match status {
        // A navigation replaced by another one, or stopped.
        COREWEBVIEW2_WEB_ERROR_STATUS_OPERATION_CANCELED => return None,
        COREWEBVIEW2_WEB_ERROR_STATUS_CANNOT_CONNECT | COREWEBVIEW2_WEB_ERROR_STATUS_SERVER_UNREACHABLE => {
            NavigationError::CannotConnect
        }
        COREWEBVIEW2_WEB_ERROR_STATUS_HOST_NAME_NOT_RESOLVED => NavigationError::HostNotFound,
        COREWEBVIEW2_WEB_ERROR_STATUS_TIMEOUT => NavigationError::TimedOut,
        COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_COMMON_NAME_IS_INCORRECT
        | COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_EXPIRED
        | COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_IS_INVALID
        | COREWEBVIEW2_WEB_ERROR_STATUS_CERTIFICATE_REVOKED
        | COREWEBVIEW2_WEB_ERROR_STATUS_CLIENT_CERTIFICATE_CONTAINS_ERRORS => NavigationError::Certificate,
        COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_ABORTED
        | COREWEBVIEW2_WEB_ERROR_STATUS_CONNECTION_RESET
        | COREWEBVIEW2_WEB_ERROR_STATUS_DISCONNECTED => NavigationError::ConnectionLost,
        _ => NavigationError::Other,
    })
}

/// The GPUI cursor for a Win32 system cursor id (`IDC_*`).
fn cursor_style(id: u32) -> CursorStyle {
    let is = |cursor: PCWSTR| cursor.0 as usize as u32 == id;
    if is(IDC_IBEAM) {
        CursorStyle::IBeam
    } else if is(IDC_HAND) {
        CursorStyle::PointingHand
    } else if is(IDC_CROSS) {
        CursorStyle::Crosshair
    } else if is(IDC_SIZEWE) {
        CursorStyle::ResizeLeftRight
    } else if is(IDC_SIZENS) {
        CursorStyle::ResizeUpDown
    } else if is(IDC_SIZENWSE) {
        CursorStyle::ResizeUpLeftDownRight
    } else if is(IDC_SIZENESW) {
        CursorStyle::ResizeUpRightDownLeft
    } else if is(IDC_SIZEALL) {
        CursorStyle::OpenHand
    } else if is(IDC_NO) {
        CursorStyle::OperationNotAllowed
    } else {
        // Arrow, and the busy cursors GPUI has no style for.
        CursorStyle::Arrow
    }
}

/// Fills its parent and keeps the page's surface on it.
struct PageElement {
    shared: Rc<Shared>,
}

impl Element for PageElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) {
        // The surface must be placed while prepainting, which also stacks it
        // with this element's layer (below overlays).
        let visible = bounds.intersect(&window.content_mask().bounds);
        self.shared.place(visible, window.scale_factor());
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        if self.shared.hovered.get() {
            window.set_window_cursor_style(self.shared.cursor.get());
        }
        // A press anywhere else in the window takes the keyboard back from the page.
        let shared = Rc::downgrade(&self.shared);
        window.on_mouse_event(move |event: &MouseDownEvent, _, _, _| {
            if !bounds.contains(&event.position)
                && let Some(shared) = shared.upgrade()
            {
                shared.focus_parent();
            }
        });
    }
}
