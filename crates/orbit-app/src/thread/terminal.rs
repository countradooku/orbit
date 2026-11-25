//! A thread's terminal: a shell in the thread's folder, at the bottom.
//!
//! The PTY lives in the engine (so it also works against a remote host); this
//! panel only draws it. The shell is started on the view's first real size so
//! it never begins at a wrong width, keystrokes typed before it exists are
//! kept, and closing the panel or the thread closes the shell.
//!
//! Selected output goes to the thread's composer as a chip: right-click ›
//! "Add selection to composer", or Ctrl+Shift+L (Cmd+Shift+L on macOS).

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;
use orbit_terminal::{TerminalEvent, TerminalView};

use crate::app_state::{AppEvent, AppState};
use crate::assets::Lucide;

pub enum TerminalPanelEvent {
    Close,
    /// Put this selected output in the thread's composer.
    AddToComposer { text: String, cwd: String },
    /// The shell printed dev-server addresses (each once).
    Urls(Vec<orbit_browser::Url>),
}

pub struct TerminalPanel {
    app: Entity<AppState>,
    /// The thread this terminal belongs to: agents started in it count
    /// toward that thread's status.
    thread: ThreadId,
    view: Entity<TerminalView>,
    cwd: String,
    term: Option<TermId>,
    opening: bool,
    pending: Vec<u8>,
    size: (u16, u16),
    exited: Option<Option<i32>>,
    title: Option<String>,
    error: Option<String>,
    /// Draws its own header; off inside the dock's tab bar.
    chrome: bool,
    urls: super::browser::OutputScanner,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TerminalPanelEvent> for TerminalPanel {}

impl TerminalPanel {
    pub fn new(app: Entity<AppState>, thread: ThreadId, cwd: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = cx.new(|cx| TerminalView::new(window, cx));
        let mut panel = Self {
            app: app.clone(),
            thread,
            view: view.clone(),
            cwd,
            term: None,
            opening: false,
            pending: Vec::new(),
            size: (0, 0),
            exited: None,
            title: None,
            error: None,
            chrome: true,
            urls: Default::default(),
            _subscriptions: Vec::new(),
        };
        panel._subscriptions = vec![
            cx.subscribe(&view, |this, _, event: &TerminalEvent, cx| this.on_view_event(event, cx)),
            cx.subscribe(&app, |this, _, event: &AppEvent, cx| match event {
                AppEvent::TerminalOutput { term, data } if Some(*term) == this.term => {
                    this.view.update(cx, |view, cx| view.feed(data, cx));
                    let urls = this.urls.feed(data);
                    if !urls.is_empty() {
                        cx.emit(TerminalPanelEvent::Urls(urls));
                    }
                }
                AppEvent::TerminalExited { term, code } if Some(*term) == this.term => {
                    this.exited = Some(*code);
                    this.term = None;
                    cx.notify();
                }
                _ => {}
            }),
            cx.on_release(|this, cx| {
                if let Some(term) = this.term {
                    this.app.update(cx, |app, _| app.send(Command::CloseTerminal { term }));
                }
            }),
        ];
        panel
    }

    pub fn focus(panel: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let view = panel.read(cx).view.clone();
        let handle = view.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    }

    fn on_view_event(&mut self, event: &TerminalEvent, cx: &mut Context<Self>) {
        match event {
            TerminalEvent::Input(data) => match self.term {
                Some(term) => self.app.update(cx, |app, _| app.send(Command::TerminalInput { term, data: data.clone() })),
                None => self.pending.extend_from_slice(data),
            },
            TerminalEvent::Resize { cols, rows } => {
                self.size = (*cols, *rows);
                match self.term {
                    Some(term) => self.app.update(cx, |app, _| {
                        app.send(Command::ResizeTerminal { term, cols: *cols, rows: *rows })
                    }),
                    None if !self.opening && self.exited.is_none() && self.error.is_none() => self.open(cx),
                    None => {}
                }
            }
            TerminalEvent::TitleChanged(title) => {
                self.title = title.clone();
                cx.notify();
            }
            TerminalEvent::Bell => {}
        }
    }

    fn open(&mut self, cx: &mut Context<Self>) {
        let (cols, rows) = self.size;
        if cols == 0 || rows == 0 {
            return;
        }
        self.opening = true;
        self.exited = None;
        self.error = None;
        let task = self.app.update(cx, |app, cx| app.call(Command::OpenTerminal { cwd: self.cwd.clone(), cols, rows }, cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.opening = false;
                match result {
                    Ok(Reply::Terminal(term)) => {
                        this.term = Some(term);
                        // Said explicitly: the engine binds by folder only when
                        // exactly one thread works there.
                        let thread = Some(this.thread);
                        this.app.update(cx, |app, _| app.send(Command::BindTerminal { term, thread }));
                        if !this.pending.is_empty() {
                            let data = std::mem::take(&mut this.pending);
                            this.app.update(cx, |app, _| app.send(Command::TerminalInput { term, data }));
                        }
                    }
                    Ok(_) => {}
                    Err(message) => this.error = Some(message),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Hands the selected output to the composer; nothing when none is selected.
    fn add_selection(&mut self, _: &super::AddSelectionToComposer, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.view.read(cx).selected_text().filter(|t| !t.trim().is_empty()) else { return };
        cx.emit(TerminalPanelEvent::AddToComposer { text, cwd: self.cwd.clone() });
    }

    /// Whether the panel draws its own header (the dock draws one for it
    /// when it shows run targets beside the shell).
    pub(crate) fn set_chrome(&mut self, chrome: bool, cx: &mut Context<Self>) {
        if self.chrome != chrome {
            self.chrome = chrome;
            cx.notify();
        }
    }

    /// The shell has ended or could not start.
    pub(crate) fn ended(&self) -> bool {
        self.exited.is_some() || self.error.is_some()
    }

    pub(crate) fn restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(term) = self.term.take() {
            self.app.update(cx, |app, _| app.send(Command::CloseTerminal { term }));
        }
        // A fresh screen for the fresh shell.
        self.view = cx.new(|cx| TerminalView::new(window, cx));
        let view = self.view.clone();
        self._subscriptions.push(cx.subscribe(&view, |this, _, event: &TerminalEvent, cx| this.on_view_event(event, cx)));
        self.exited = None;
        self.error = None;
        self.pending.clear();
        self.open(cx);
        cx.notify();
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // Shells on Windows title the window with their own executable's
        // path, which says nothing; the folder does.
        let label = self
            .title
            .clone()
            .filter(|t| !t.trim().is_empty() && !t.to_ascii_lowercase().ends_with(".exe"))
            .unwrap_or_else(|| self.cwd.clone());
        let ended = self.exited.is_some() || self.error.is_some();
        let view = self.view.clone();
        v_flex()
            .size_full()
            .bg(theme.background)
            .on_action(cx.listener(Self::add_selection))
            .when(self.chrome, |el| el.child(
                h_flex()
                    .h_8()
                    .flex_none()
                    .px_3()
                    .gap_2()
                    .border_t_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(Lucide::Terminal).xsmall())
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .when_some(self.exited, |el, code| {
                        el.child(match code {
                            Some(0) | None => "Exited".to_string(),
                            Some(code) => format!("Exited with {code}"),
                        })
                    })
                    .when(ended, |el| {
                        el.child(
                            Button::new("terminal-restart")
                                .ghost()
                                .xsmall()
                                .icon(IconName::RefreshCw)
                                .label("Restart")
                                .on_click(cx.listener(|this, _, window, cx| this.restart(window, cx))),
                        )
                    })
                    .child(
                        Button::new("terminal-close")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Close terminal (Ctrl+`)")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(TerminalPanelEvent::Close))),
                    ),
            ))
            .child(match &self.error {
                Some(error) => crate::ui::empty_state(
                    Icon::new(Lucide::Terminal),
                    "The terminal didn’t start",
                    error.clone(),
                    Some(
                        Button::new("terminal-try-again")
                            .outline()
                            .icon(IconName::RefreshCw)
                            .label("Try again")
                            .on_click(cx.listener(|this, _, window, cx| this.restart(window, cx)))
                            .into_any_element(),
                    ),
                    cx,
                )
                .into_any_element(),
                None => div()
                    .id("terminal-body")
                    .flex_1()
                    .min_h_0()
                    .px_2()
                    .pt_1()
                    .child(self.view.clone())
                    .context_menu(move |menu, _, cx| {
                        let terminal = view.read(cx);
                        let selected = terminal.has_selection();
                        menu.action_context(terminal.focus_handle(cx))
                            .menu_with_disabled("Copy", Box::new(orbit_terminal::Copy), !selected)
                            .menu("Paste", Box::new(orbit_terminal::Paste))
                            .menu_with_disabled("Add selection to composer", Box::new(super::AddSelectionToComposer), !selected)
                            .separator()
                            .menu("Select all", Box::new(orbit_terminal::SelectAll))
                            .menu("Clear", Box::new(orbit_terminal::Clear))
                    })
                    .into_any_element(),
            })
    }
}
