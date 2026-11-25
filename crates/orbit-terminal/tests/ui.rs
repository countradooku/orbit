//! UI integration tests: a real `TerminalView` in a headless window, driven
//! through GPUI's key dispatch and text input.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AppContext as _, Bounds, Entity, Focusable as _, TestAppContext, WindowBounds, WindowOptions, px, size,
};
use orbit_terminal::{ScrollToTop, SelectAll, TerminalEvent, TerminalView};

type Events = Rc<RefCell<Vec<TerminalEvent>>>;

fn open(cx: &mut TestAppContext) -> (gpui_kit::AnyWindowHandle, Entity<TerminalView>, Events) {
    open_with(cx, |view| view)
}

fn open_with(
    cx: &mut TestAppContext,
    build: impl FnOnce(TerminalView) -> TerminalView + 'static,
) -> (gpui_kit::AnyWindowHandle, Entity<TerminalView>, Events) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        orbit_terminal::init(cx);
    });
    let events: Events = Rc::default();
    let (handle, view) = cx.update(|cx| {
        let events = events.clone();
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(Default::default(), size(px(800.), px(480.))))),
                ..Default::default()
            },
            cx,
            move |window, cx| {
                let view = cx.new(|cx| build(TerminalView::new(window, cx)));
                cx.subscribe(&view, move |_, event: &TerminalEvent, _| events.borrow_mut().push(event.clone()))
                    .detach();
                view.focus_handle(cx).focus(window, cx);
                view
            },
        )
        .expect("open test window")
    });
    (handle, view, events)
}

fn inputs(events: &Events) -> Vec<Vec<u8>> {
    events
        .borrow_mut()
        .drain(..)
        .filter_map(|event| match event {
            TerminalEvent::Input(bytes) => Some(bytes),
            _ => None,
        })
        .collect()
}

#[gpui_kit::test]
fn lays_out_to_the_window_and_reports_its_size(cx: &mut TestAppContext) {
    let (handle, view, events) = open(cx);
    cx.update_window(handle, |_, window, cx| window.render_frame(cx)).unwrap();
    let (cols, rows) = cx.update(|cx| view.read(cx).grid_size());
    assert!(cols > 40 && rows > 10, "{cols}x{rows}");
    assert!(events.borrow().contains(&TerminalEvent::Resize { cols, rows }));
}

#[gpui_kit::test]
fn keys_reach_the_shell_with_terminal_encodings(cx: &mut TestAppContext) {
    let (handle, _view, events) = open(cx);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.press("enter", cx);
        window.press("up", cx);
        window.press("ctrl-right", cx);
        // Root binds Tab (focus) and Ctrl+C (copy); in a terminal they are the shell's.
        window.press("tab", cx);
        window.press("ctrl-c", cx);
        window.press("escape", cx);
        window.press("f5", cx);
        window.press("alt-b", cx);
    })
    .unwrap();
    assert_eq!(
        inputs(&events),
        vec![
            b"\r".to_vec(),
            b"\x1b[A".to_vec(),
            b"\x1b[1;5C".to_vec(),
            b"\t".to_vec(),
            b"\x03".to_vec(),
            b"\x1b".to_vec(),
            b"\x1b[15~".to_vec(),
            b"\x1bb".to_vec(),
        ]
    );
}

#[gpui_kit::test]
fn text_arrives_through_text_input(cx: &mut TestAppContext) {
    let (handle, _view, events) = open(cx);
    cx.update_window(handle, |_, window, cx| window.input("ls é中", cx)).unwrap();
    let typed: Vec<u8> = inputs(&events).concat();
    assert_eq!(String::from_utf8(typed).unwrap(), "ls é中");
}

#[gpui_kit::test]
fn application_cursor_mode_changes_arrow_keys(cx: &mut TestAppContext) {
    let (handle, view, events) = open(cx);
    view.update(cx, |view, cx| view.feed(b"\x1b[?1h", cx));
    cx.update_window(handle, |_, window, cx| window.press("up", cx)).unwrap();
    assert_eq!(inputs(&events), vec![b"\x1bOA".to_vec()]);
}

#[gpui_kit::test]
fn answers_device_queries(cx: &mut TestAppContext) {
    let (handle, view, events) = open(cx);
    cx.update_window(handle, |_, window, cx| window.render_frame(cx)).unwrap();
    inputs(&events);
    // Primary device attributes (fish waits for this) and a cursor position report.
    view.update(cx, |view, cx| view.feed(b"ab\x1b[c\x1b[6n", cx));
    assert_eq!(inputs(&events), vec![b"\x1b[?6c".to_vec(), b"\x1b[1;3R".to_vec()]);
}

#[gpui_kit::test]
fn scrollback_is_bounded(cx: &mut TestAppContext) {
    let (handle, view, _events) = open_with(cx, |view| view.scrollback(100));
    cx.update_window(handle, |_, window, cx| window.render_frame(cx)).unwrap();
    let output: String = (0..1000).map(|i| format!("line {i}\r\n")).collect();
    view.update(cx, |view, cx| view.feed(output.as_bytes(), cx));
    cx.update_window(handle, |_, window, cx| {
        window.dispatch_action(Box::new(ScrollToTop), cx);
        window.render_frame(cx);
    })
    .unwrap();
    let (offset, text) = cx.update(|cx| (view.read(cx).display_offset(), view.read(cx).visible_text()));
    assert_eq!(offset, 100);
    let (_, rows) = cx.update(|cx| view.read(cx).grid_size());
    // 1000 lines and the empty line after them; the oldest ones are gone.
    let first = 1001 - rows as usize - 100;
    assert_eq!(text.lines().next(), Some(format!("line {first}").as_str()));
    // Typing returns to the live screen.
    cx.update_window(handle, |_, window, cx| window.input("x", cx)).unwrap();
    assert_eq!(cx.update(|cx| view.read(cx).display_offset()), 0);
}

#[gpui_kit::test]
fn ctrl_c_copies_a_selection_instead_of_interrupting(cx: &mut TestAppContext) {
    let (handle, view, events) = open(cx);
    view.update(cx, |view, cx| view.feed(b"hello terminal", cx));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.dispatch_action(Box::new(SelectAll), cx);
    })
    .unwrap();
    // Dispatched actions run at the end of the update.
    assert!(cx.update(|cx| view.read(cx).has_selection()));
    cx.update_window(handle, |_, window, cx| window.press("ctrl-c", cx)).unwrap();
    let sent = inputs(&events);
    assert!(sent.is_empty(), "{sent:?}");
    let copied = cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default();
    assert!(copied.starts_with("hello terminal"), "{copied:?}");
    // The selection is gone, so the next Ctrl+C interrupts.
    assert!(!cx.update(|cx| view.read(cx).has_selection()));
    cx.update_window(handle, |_, window, cx| window.press("ctrl-c", cx)).unwrap();
    assert_eq!(inputs(&events), vec![b"\x03".to_vec()]);
}

#[gpui_kit::test]
fn paste_is_bracketed_when_the_shell_asks(cx: &mut TestAppContext) {
    let (_handle, view, events) = open(cx);
    view.update(cx, |view, cx| {
        view.paste_text("a\nb", cx);
        view.feed(b"\x1b[?2004h", cx);
        view.paste_text("a\nb", cx);
    });
    assert_eq!(inputs(&events), vec![b"a\rb".to_vec(), b"\x1b[200~a\nb\x1b[201~".to_vec()]);
}
