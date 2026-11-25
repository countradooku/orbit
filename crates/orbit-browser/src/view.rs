//! [`BrowserView`]: the toolbar, the embedded page and its states.

use std::time::Duration;

use futures::{StreamExt as _, channel::mpsc};
use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    progress::Progress,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use url::Url;

use crate::address::{NewWindowTarget, host_label, is_loopback, new_window_target, parse_address};
use crate::engine::{Engine, EngineEvent, NavigationError};
use crate::{Back, FocusAddress, Forward, KEY_CONTEXT, OpenInSystemBrowser, Reload, Stop, ToggleDevTools, probe};

/// How often an unreachable local server is asked again.
const RETRY_INTERVAL: Duration = Duration::from_millis(1000);

/// What a [`BrowserView`] tells its owner.
#[derive(Clone, Debug, PartialEq)]
pub enum BrowserEvent {
    /// The page's title changed; `None` while a new page has none yet.
    TitleChanged(Option<SharedString>),
    /// The address changed: the owner navigated, or the page did (a link, a
    /// redirect, `history.pushState`).
    UrlChanged(Option<Url>),
    /// The page started (`true`) or finished (`false`) loading.
    LoadingChanged(bool),
}

/// What the area below the toolbar shows.
#[derive(Clone, Debug, PartialEq)]
enum Page {
    /// No address yet.
    Blank,
    /// Asking whether a local server answers before loading it. A page
    /// already shown stays visible meanwhile.
    Probing,
    /// The web view shows the page.
    Live,
    /// Nothing listens at a local address. Asked again every second, and
    /// loaded as soon as it answers.
    Unreachable,
    /// The page failed to load.
    Failed(NavigationError),
    /// The page's browser process went away.
    Crashed,
    /// No web view could be created.
    Unavailable(SharedString),
}

/// A preview browser: an address toolbar over a native web page.
///
/// Owns its web view, which is created the first time the view is rendered
/// with an address to show, so a browser that is never shown costs no
/// browser process. [`suspend`](Self::suspend) releases the web view while
/// keeping the address; dropping the view releases it too.
///
/// The page is a platform web view (WebView2, WKWebView or WebKitGTK)
/// composited inside the GPUI window; GPUI menus, popovers, dialogs and
/// tooltips draw above it (see the crate docs for the platform details).
/// While the page has keyboard focus it takes keys first, as a browser does.
///
/// Local addresses (`localhost`, `127.0.0.1`…) are probed before loading, so
/// a dev server that is not running shows "Nothing is listening on
/// localhost:3000" at once, and the page loads by itself when the server
/// comes up.
pub struct BrowserView {
    focus_handle: FocusHandle,
    address: Entity<InputState>,
    url: Option<Url>,
    title: Option<SharedString>,
    loading: bool,
    can_go_back: bool,
    can_go_forward: bool,
    devtools_open: bool,
    page: Page,
    engine: Option<Engine>,
    engine_ready: bool,
    /// Whether creating the engine is already scheduled.
    engine_scheduled: bool,
    visible: bool,
    suggestions: Vec<Url>,
    _engine_events: Task<()>,
    probe: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<BrowserEvent> for BrowserView {}

impl Focusable for BrowserView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BrowserView {
    /// A browser with no page. Nothing native is created until it is shown
    /// with an address.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let address = cx.new(|cx| InputState::new(window, cx).placeholder("Address or port"));
        let subscriptions = vec![
            cx.subscribe_in(&address, window, Self::on_address_event),
            cx.on_release(|this, cx| {
                if let Some(engine) = this.engine.take() {
                    engine.release(cx);
                }
            }),
        ];
        Self {
            focus_handle: cx.focus_handle(),
            address,
            url: None,
            title: None,
            loading: false,
            can_go_back: false,
            can_go_forward: false,
            devtools_open: false,
            page: Page::Blank,
            engine: None,
            engine_ready: false,
            engine_scheduled: false,
            visible: true,
            suggestions: Vec::new(),
            _engine_events: Task::ready(()),
            probe: Task::ready(()),
            _subscriptions: subscriptions,
        }
    }

    /// The page's address, or the address being loaded.
    pub fn url(&self) -> Option<&Url> {
        self.url.as_ref()
    }

    /// The page's title, once it has one.
    pub fn title(&self) -> Option<&SharedString> {
        self.title.as_ref()
    }

    /// Whether a page is loading, or a local server is being asked.
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Whether the web view has been released by [`suspend`](Self::suspend)
    /// or was never created.
    pub fn is_suspended(&self) -> bool {
        self.engine.is_none()
    }

    /// Whether the page is shown. See [`set_visible`](Self::set_visible).
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Loads `url`, probing a local server first.
    pub fn navigate(&mut self, url: Url, window: &mut Window, cx: &mut Context<Self>) {
        self.set_url(Some(url.clone()), window, cx);
        self.set_title(None, cx);
        if is_loopback(&url) {
            self.set_page(Page::Probing, cx);
            self.set_loading(true, cx);
            self.probe_then_load(url, window, cx);
        } else {
            self.probe = Task::ready(());
            self.load(cx);
        }
    }

    /// Loads what was typed: a URL, `localhost:3000`, or a port such as `3000`.
    pub fn open_address(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Url, crate::AddressError> {
        let url = parse_address(text)?;
        self.navigate(url.clone(), window, cx);
        Ok(url)
    }

    /// Offers these addresses, such as the dev servers
    /// [`detect_urls`](crate::detect_urls) found, while no page is open.
    pub fn set_suggested_urls(&mut self, urls: Vec<Url>, cx: &mut Context<Self>) {
        if self.suggestions != urls {
            self.suggestions = urls;
            cx.notify();
        }
    }

    /// Shows or hides the page without releasing it. Call it with `false`
    /// when the panel holding this view stops rendering it: the page is a
    /// native surface that otherwise stays where it was last laid out.
    /// `true` also resumes a [suspended](Self::suspend) view.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        self.sync_page_visibility(cx);
        cx.notify();
    }

    /// Hides the page and releases the web view and its browser process,
    /// keeping the address. [`set_visible(true)`](Self::set_visible) loads
    /// the address again in a new web view.
    pub fn suspend(&mut self, cx: &mut Context<Self>) {
        self.visible = false;
        self.release_engine(cx);
        self.set_loading(false, cx);
        cx.notify();
    }

    /// Goes back in the page's history.
    pub fn back(&mut self, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.back(cx);
        }
    }

    /// Goes forward in the page's history.
    pub fn forward(&mut self, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.forward(cx);
        }
    }

    /// Reloads the page, or tries an address that failed again.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match (&self.page, &self.engine) {
            (Page::Live, Some(engine)) if self.engine_ready => engine.reload(cx),
            _ => {
                if matches!(self.page, Page::Crashed | Page::Unavailable(_)) {
                    self.release_engine(cx);
                }
                if let Some(url) = self.url.clone() {
                    self.navigate(url, window, cx);
                }
            }
        }
    }

    /// Stops loading.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if self.page == Page::Probing {
            self.probe = Task::ready(());
            self.set_page(if self.engine.is_some() { Page::Live } else { Page::Blank }, cx);
        }
        if let Some(engine) = &self.engine {
            engine.stop(cx);
        }
        self.set_loading(false, cx);
    }

    /// Opens the address in the system's default browser.
    pub fn open_in_system_browser(&mut self, cx: &mut Context<Self>) {
        if let Some(url) = &self.url {
            cx.open_url(url.as_str());
        }
    }

    /// Opens the page's developer tools, or closes them where the platform
    /// can (macOS and Linux). On Windows they open in their own window.
    pub fn toggle_devtools(&mut self, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine
            && let Some(open) = engine.toggle_devtools(cx)
        {
            self.devtools_open = open;
            cx.notify();
        }
    }

    /// Moves keyboard focus to the address field and selects its text.
    pub fn focus_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.focus_parent();
        }
        self.address.update(cx, |address, cx| {
            address.focus(window, cx);
            address.select_all(window, cx);
        });
    }

    fn on_address_event(
        &mut self,
        address: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            let text = address.read(cx).value();
            match parse_address(&text) {
                Ok(url) => {
                    // Show the address as it will load (`3000` becomes
                    // `http://localhost:3000/`), then hand the keyboard back.
                    let text = url.to_string();
                    address.update(cx, |address, cx| address.set_value(text, window, cx));
                    window.focus(&self.focus_handle, cx);
                    self.navigate(url, window, cx);
                }
                Err(error) => window.push_notification(Notification::warning(error.to_string()), cx),
            }
        }
    }

    fn probe_then_load(&mut self, url: Url, window: &mut Window, cx: &mut Context<Self>) {
        self.probe = cx.spawn_in(window, async move |this, cx| {
            let addresses = probe::addresses(&url);
            let mut first = true;
            loop {
                let candidates = addresses.clone();
                let listening = cx.background_spawn(async move { probe::is_listening(&candidates) }).await;
                let still_current = this
                    .update(cx, |this, cx| {
                        if this.url.as_ref() != Some(&url) {
                            return false;
                        }
                        if listening {
                            this.load(cx);
                        } else if first {
                            this.set_page(Page::Unreachable, cx);
                            this.set_loading(false, cx);
                        }
                        true
                    })
                    .unwrap_or(false);
                if listening || !still_current {
                    return;
                }
                first = false;
                cx.background_executor().timer(RETRY_INTERVAL).await;
            }
        });
    }

    /// Shows the current address in the web view, creating it if needed.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(url) = self.url.clone() else { return };
        self.set_page(Page::Live, cx);
        self.set_loading(true, cx);
        match &self.engine {
            Some(engine) => engine.navigate(&url, cx),
            // Created by the next render, which loads `url`.
            None => cx.notify(),
        }
    }

    fn needs_engine(&self) -> bool {
        self.engine.is_none() && self.visible && self.page == Page::Live && self.url.is_some()
    }

    fn create_engine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.engine_scheduled = false;
        if !self.needs_engine() {
            return;
        }
        let (events, mut received) = mpsc::unbounded();
        // A new channel per engine: events of a released one never arrive.
        self._engine_events = cx.spawn_in(window, async move |this, cx| {
            while let Some(event) = received.next().await {
                if this.update_in(cx, |this, window, cx| this.apply(event, window, cx)).is_err() {
                    break;
                }
            }
        });
        self.engine_ready = false;
        self.engine = Some(Engine::new(self.url.as_ref(), events, window, cx));
        self.set_loading(true, cx);
    }

    fn release_engine(&mut self, cx: &mut Context<Self>) {
        if let Some(engine) = self.engine.take() {
            engine.release(cx);
        }
        self._engine_events = Task::ready(());
        self.engine_ready = false;
        self.can_go_back = false;
        self.can_go_forward = false;
        self.devtools_open = false;
    }

    fn apply(&mut self, event: EngineEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            EngineEvent::Ready => {
                self.engine_ready = true;
                self.sync_page_visibility(cx);
            }
            EngineEvent::Unavailable(message) => {
                self.release_engine(cx);
                self.set_loading(false, cx);
                self.set_page(Page::Unavailable(message.into()), cx);
            }
            EngineEvent::NavigationStarted => self.set_loading(true, cx),
            EngineEvent::NavigationFinished(error) => {
                self.set_loading(false, cx);
                match error {
                    Some(NavigationError::CannotConnect) if self.url.as_ref().is_some_and(is_loopback) => {
                        // The server went away between the probe and the load.
                        if let Some(url) = self.url.clone() {
                            self.navigate(url, window, cx);
                        }
                    }
                    Some(error) => self.set_page(Page::Failed(error), cx),
                    None => {}
                }
            }
            EngineEvent::UrlChanged(text) => {
                if let Ok(url) = Url::parse(&text)
                    && self.url.as_ref() != Some(&url)
                    && url.as_str() != "about:blank"
                {
                    self.set_url(Some(url), window, cx);
                }
            }
            EngineEvent::TitleChanged(title) => {
                let title = title.trim();
                self.set_title((!title.is_empty()).then(|| SharedString::from(title.to_string())), cx);
            }
            EngineEvent::HistoryChanged { can_go_back, can_go_forward } => {
                self.can_go_back = can_go_back;
                self.can_go_forward = can_go_forward;
                cx.notify();
            }
            EngineEvent::NewWindowRequested(text) => {
                if let Ok(url) = Url::parse(&text) {
                    match new_window_target(self.url.as_ref(), &url) {
                        NewWindowTarget::InPlace => self.navigate(url, window, cx),
                        NewWindowTarget::System => cx.open_url(url.as_str()),
                    }
                }
            }
            // The page element reads the cursor; it only needs a repaint.
            EngineEvent::CursorChanged(_) => cx.notify(),
            EngineEvent::Crashed => {
                self.release_engine(cx);
                self.set_loading(false, cx);
                self.set_page(Page::Crashed, cx);
            }
            EngineEvent::PageFocused => {
                // GPUI's own focus leaves the address field, whose caret
                // would otherwise keep blinking while the page takes keys.
                if !self.focus_handle.is_focused(window) {
                    window.focus(&self.focus_handle, cx);
                }
            }
            EngineEvent::AddressFocusRequested => self.focus_address(window, cx),
        }
    }

    fn set_page(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page != page {
            self.page = page;
            self.sync_page_visibility(cx);
            cx.notify();
        }
    }

    fn sync_page_visibility(&mut self, cx: &mut Context<Self>) {
        let shown = self.visible && matches!(self.page, Page::Live | Page::Probing);
        if let Some(engine) = &self.engine {
            engine.set_visible(shown, cx);
        }
    }

    fn set_url(&mut self, url: Option<Url>, window: &mut Window, cx: &mut Context<Self>) {
        if self.url == url {
            return;
        }
        self.url = url;
        let text = self.url.as_ref().map(Url::to_string).unwrap_or_default();
        // Leave what the user is typing alone.
        if !self.address.focus_handle(cx).is_focused(window) {
            self.address.update(cx, |address, cx| address.set_value(text, window, cx));
        }
        cx.emit(BrowserEvent::UrlChanged(self.url.clone()));
        cx.notify();
    }

    fn set_title(&mut self, title: Option<SharedString>, cx: &mut Context<Self>) {
        if self.title != title {
            self.title = title;
            cx.emit(BrowserEvent::TitleChanged(self.title.clone()));
        }
    }

    fn set_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        if self.loading != loading {
            self.loading = loading;
            cx.emit(BrowserEvent::LoadingChanged(loading));
            cx.notify();
        }
    }

    /// Back and Forward without history reports (Wry) stay available once a
    /// page is live; the page ignores them when there is nowhere to go.
    fn history_available(&self) -> (bool, bool) {
        if !matches!(self.page, Page::Live | Page::Probing) {
            return (false, false);
        }
        match &self.engine {
            Some(engine) if engine.tracks_history() => (self.can_go_back, self.can_go_forward),
            Some(_) => {
                let live = self.page == Page::Live;
                (live, live)
            }
            None => (false, false),
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (can_go_back, can_go_forward) = self.history_available();
        let has_url = self.url.is_some();
        let has_page = self.engine.is_some() && self.engine_ready;

        let reload = if self.loading {
            Button::new("browser-stop")
                .icon(IconName::Close)
                .tooltip_with_action("Stop", &Stop, Some(KEY_CONTEXT))
                .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
        } else {
            Button::new("browser-reload")
                .icon(IconName::RotateCw)
                .disabled(!has_url)
                .tooltip_with_action("Reload", &Reload, Some(KEY_CONTEXT))
                .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx)))
        };

        h_flex()
            .relative()
            .flex_none()
            .w_full()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("browser-back")
                    .ghost()
                    .small()
                    .icon(IconName::ArrowLeft)
                    .disabled(!can_go_back)
                    .tooltip_with_action("Back", &Back, Some(KEY_CONTEXT))
                    .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
            )
            .child(
                Button::new("browser-forward")
                    .ghost()
                    .small()
                    .icon(IconName::ArrowRight)
                    .disabled(!can_go_forward)
                    .tooltip_with_action("Forward", &Forward, Some(KEY_CONTEXT))
                    .on_click(cx.listener(|this, _, _, cx| this.forward(cx))),
            )
            .child(reload.ghost().small())
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.address)
                        .small()
                        .prefix(Icon::new(IconName::Globe).xsmall().text_color(theme.muted_foreground)),
                ),
            )
            .child(
                Button::new("browser-open-external")
                    .ghost()
                    .small()
                    .icon(IconName::ExternalLink)
                    .disabled(!has_url)
                    .tooltip_with_action("Open in system browser", &OpenInSystemBrowser, Some(KEY_CONTEXT))
                    .on_click(cx.listener(|this, _, _, cx| this.open_in_system_browser(cx))),
            )
            .child(
                Button::new("browser-devtools")
                    .ghost()
                    .small()
                    .icon(IconName::Inspector)
                    .selected(self.devtools_open)
                    .disabled(!has_page)
                    .tooltip_with_action("Developer tools", &ToggleDevTools, Some(KEY_CONTEXT))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_devtools(cx))),
            )
            .when(self.loading, |toolbar| {
                toolbar.child(
                    div().absolute().left_0().right_0().bottom_0().child(
                        Progress::new("browser-loading")
                            .loading(true)
                            .h_0p5()
                            .rounded_none()
                            .accessibility_label("Loading"),
                    ),
                )
            })
    }

    fn render_content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let host = self.url.as_ref().map(host_label).unwrap_or_default();
        match &self.page {
            Page::Live | Page::Probing => match &self.engine {
                Some(engine) => engine.page(),
                None => div().into_any_element(),
            },
            Page::Blank => self.render_blank(cx).into_any_element(),
            Page::Unreachable => EmptyState::new(IconName::Network, format!("Nothing is listening on {host}"))
                .description(
                    "Start your dev server, then reload. The preview also opens by itself once the server answers.",
                )
                .action(self.reload_button(cx))
                .into_any_element(),
            Page::Failed(error) => EmptyState::new(IconName::TriangleAlert, format!("Couldn’t open {host}"))
                .description(error.message())
                .action(self.reload_button(cx))
                .action(self.open_external_button(cx))
                .into_any_element(),
            Page::Crashed => EmptyState::new(IconName::TriangleAlert, "The page stopped unexpectedly")
                .description("Its browser process exited. Reload to start it again.")
                .action(self.reload_button(cx))
                .into_any_element(),
            Page::Unavailable(message) => EmptyState::new(IconName::CircleAlert, "The preview couldn’t start")
                .description(message.clone())
                .when(self.url.is_some(), |state| state.action(self.open_external_button(cx)))
                .into_any_element(),
        }
    }

    fn render_blank(&self, cx: &mut Context<Self>) -> EmptyState {
        let state = EmptyState::new(IconName::Globe, "No page open")
            .description("Enter an address or a dev server’s port, such as 3000.");
        self.suggestions.iter().fold(state, |state, url| {
            let target = url.clone();
            state.action(
                Button::new(SharedString::from(format!("browser-suggestion-{url}")))
                    .outline()
                    .small()
                    .icon(IconName::Network)
                    .label(host_label(url))
                    .on_click(cx.listener(move |this, _, window, cx| this.navigate(target.clone(), window, cx))),
            )
        })
    }

    fn reload_button(&self, cx: &mut Context<Self>) -> Button {
        Button::new("browser-retry")
            .small()
            .icon(IconName::RotateCw)
            .label("Reload")
            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx)))
    }

    fn open_external_button(&self, cx: &mut Context<Self>) -> Button {
        Button::new("browser-retry-external")
            .small()
            .outline()
            .icon(IconName::ExternalLink)
            .label("Open in system browser")
            .on_click(cx.listener(|this, _, _, cx| this.open_in_system_browser(cx)))
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.needs_engine() && !self.engine_scheduled {
            // First shown with an address: create the web view after this frame.
            self.engine_scheduled = true;
            cx.defer_in(window, |this, window, cx| this.create_engine(window, cx));
        }
        let background = cx.theme().background;
        v_flex()
            .id("browser-view")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(background)
            .on_action(cx.listener(|this, _: &Back, _, cx| this.back(cx)))
            .on_action(cx.listener(|this, _: &Forward, _, cx| this.forward(cx)))
            .on_action(cx.listener(|this, _: &Reload, window, cx| this.reload(window, cx)))
            .on_action(cx.listener(|this, _: &Stop, _, cx| this.stop(cx)))
            .on_action(cx.listener(|this, _: &FocusAddress, window, cx| this.focus_address(window, cx)))
            .on_action(cx.listener(|this, _: &OpenInSystemBrowser, _, cx| this.open_in_system_browser(cx)))
            .on_action(cx.listener(|this, _: &ToggleDevTools, _, cx| this.toggle_devtools(cx)))
            .child(self.render_toolbar(cx))
            .child(div().relative().flex_1().min_h_0().w_full().child(self.render_content(cx)))
    }
}

/// The centered message that replaces the page: what happened, and what to do.
#[derive(IntoElement)]
struct EmptyState {
    icon: IconName,
    title: SharedString,
    description: Option<SharedString>,
    actions: Vec<Button>,
}

impl EmptyState {
    fn new(icon: IconName, title: impl Into<SharedString>) -> Self {
        Self { icon, title: title.into(), description: None, actions: Vec::new() }
    }

    fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    fn action(mut self, action: Button) -> Self {
        self.actions.push(action);
        self
    }
}

impl RenderOnce for EmptyState {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_4()
            .p_8()
            .child(Icon::new(self.icon).large().text_color(theme.muted_foreground))
            .child(
                v_flex()
                    .items_center()
                    .gap_1()
                    .max_w_96()
                    .child(div().text_base().font_weight(FontWeight::SEMIBOLD).text_center().child(self.title))
                    .when_some(self.description, |this, description| {
                        this.child(div().text_sm().text_center().text_color(theme.muted_foreground).child(description))
                    }),
            )
            .when(!self.actions.is_empty(), |this| {
                this.child(h_flex().flex_wrap().justify_center().gap_2().children(self.actions))
            })
    }
}
