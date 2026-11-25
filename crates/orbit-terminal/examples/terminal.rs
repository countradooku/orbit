//! A window with one terminal running the user's shell.
//!
//! `cargo run -p orbit-terminal --example terminal [-- --dark]`
//!
//! The PTY comes from `orbit_core::terminal`; the view knows nothing about
//! it. Output is drained in batches (one `feed`, so one redraw, per batch),
//! and the view's input and size changes go back to the PTY.

use std::rc::Rc;

use crossfire::mpsc;
use gpui_kit::*;
use orbit_core::terminal::{OUTPUT_CAPACITY, TermId, TermSize, Terminals};
use orbit_terminal::{TerminalEvent, TerminalView};

/// Most bytes applied to the view in one redraw.
const BATCH: usize = 1024 * 1024;

struct Standalone {
    terminal: Entity<TerminalView>,
    _subscription: Subscription,
    _output: Task<()>,
    _exit: Task<()>,
}

impl Standalone {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (output_tx, output_rx) = mpsc::bounded_blocking_async(OUTPUT_CAPACITY);
        let (exit_tx, exit_rx) = mpsc::unbounded_async();
        let terminals = Rc::new(Terminals::new(output_tx, exit_tx));
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let id = terminals.open(&cwd, TermSize::new(100, 30), None).expect("failed to start the shell");

        let terminal = cx.new(|cx| TerminalView::new(window, cx));
        terminal.focus_handle(cx).focus(window, cx);

        let subscription = {
            let terminals = terminals.clone();
            cx.subscribe_in(&terminal, window, move |_, _, event, window, _| match event {
                TerminalEvent::Input(bytes) => {
                    terminals.input(id, bytes);
                }
                TerminalEvent::Resize { cols, rows } => {
                    let _ = terminals.resize(id, TermSize::new(*cols, *rows));
                }
                TerminalEvent::TitleChanged(title) => window.set_window_title(title.as_deref().unwrap_or("Orbit")),
                TerminalEvent::Bell => {}
            })
        };

        let output = {
            let terminal = terminal.downgrade();
            cx.spawn(async move |_, cx| {
                let mut batch = Vec::with_capacity(BATCH);
                while let Ok((_, bytes)) = output_rx.recv().await {
                    batch.extend_from_slice(&bytes);
                    while batch.len() < BATCH {
                        match output_rx.try_recv() {
                            Ok((_, more)) => batch.extend_from_slice(&more),
                            Err(_) => break,
                        }
                    }
                    if terminal.update(cx, |view, cx| view.feed(&batch, cx)).is_err() {
                        break;
                    }
                    batch.clear();
                }
            })
        };

        let exit = cx.spawn(async move |_, cx| {
            if let Ok((exited, code)) = exit_rx.recv().await {
                let exited: TermId = exited;
                eprintln!("shell {exited} exited with {code:?}");
                terminals.remove_exited(exited);
                cx.update(|cx| cx.quit());
            }
        });

        Self { terminal, _subscription: subscription, _output: output, _exit: exit }
    }
}

impl Render for Standalone {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.terminal.clone())
    }
}

fn main() {
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(|cx| {
        gpui_kit::init(cx);
        orbit_terminal::init(cx);
        if std::env::args().any(|arg| arg == "--dark") {
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        }
        let bounds = Bounds::centered(None, size(px(960.), px(600.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions { title: Some("Orbit terminal".into()), ..Default::default() }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Standalone::new(window, cx)))
            .expect("failed to open the window");
        cx.activate(true);
    });
}
