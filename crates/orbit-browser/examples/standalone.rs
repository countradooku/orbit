//! A window with one preview browser.
//!
//! `cargo run -p orbit-browser --example standalone [-- [ADDRESS] [--dark] [--overlay dialog|popover|notification]]`
//!
//! Loads `https://example.com` unless an address (a URL, `localhost:3000`
//! or a port) is given. The title bar's Popover button and Overlays menu
//! open GPUI overlays over the page. `--overlay` opens one a few seconds
//! after start, for screenshots.

use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, Theme, ThemeMode, TitleBar, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{DropdownMenu as _, PopupMenuItem},
    popover::Popover,
    v_flex,
};
use gpui_kit::*;
use orbit_browser::{BrowserEvent, BrowserView, parse_address};

struct Standalone {
    browser: Entity<BrowserView>,
    title: SharedString,
    popover_open: bool,
    _subscription: Subscription,
    _overlay: Task<()>,
}

impl Standalone {
    fn new(address: &str, overlay: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let browser = cx.new(|cx| BrowserView::new(window, cx));
        let url = parse_address(address).expect("not an address");
        browser.update(cx, |browser, cx| browser.navigate(url, window, cx));

        let subscription = cx.subscribe_in(&browser, window, |this, _, event, window, cx| {
            if let BrowserEvent::TitleChanged(title) = event {
                this.title = title.clone().unwrap_or_else(|| "Orbit preview".into());
                window.set_window_title(&this.title);
                cx.notify();
            }
        });

        let overlay = cx.spawn_in(window, async move |this, cx| {
            let Some(overlay) = overlay else { return };
            cx.background_executor().timer(Duration::from_secs(4)).await;
            let _ = this.update_in(cx, |this, window, cx| match overlay.as_str() {
                "dialog" => Self::open_dialog(window, cx),
                "popover" => {
                    this.popover_open = true;
                    cx.notify();
                }
                "notification" => Self::notify(window, cx),
                other => eprintln!("unknown overlay {other}"),
            });
        });

        Self {
            browser,
            title: "Orbit preview".into(),
            popover_open: false,
            _subscription: subscription,
            _overlay: overlay,
        }
    }

    fn open_dialog(window: &mut Window, cx: &mut App) {
        window.open_dialog(cx, |dialog, _, cx| {
            dialog.title("Dialog over the page").w(px(420.)).child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("GPUI draws this dialog and its backdrop above the native web view."),
            )
        });
    }

    fn notify(window: &mut Window, cx: &mut App) {
        window.push_notification("GPUI notifications draw above the page too.", cx);
    }
}

impl Render for Standalone {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let overlays =
            Button::new("overlays").ghost().small().label("Overlays").dropdown_menu(move |menu, window, _| {
                menu.item(PopupMenuItem::new("Open dialog…").on_click(|_, window, cx| Self::open_dialog(window, cx)))
                    .item(PopupMenuItem::new("Show popover").on_click(window.listener_for(&view, |this, _, _, cx| {
                        this.popover_open = true;
                        cx.notify();
                    })))
                    .item(PopupMenuItem::new("Show notification").on_click(|_, window, cx| Self::notify(window, cx)))
                    .separator()
                    .item(PopupMenuItem::new("Suspend browser").on_click(window.listener_for(
                        &view,
                        |this, _, _, cx| {
                            this.browser.update(cx, |browser, cx| browser.suspend(cx));
                        },
                    )))
                    .item(PopupMenuItem::new("Resume browser").on_click(window.listener_for(
                        &view,
                        |this, _, _, cx| {
                            this.browser.update(cx, |browser, cx| browser.set_visible(true, cx));
                        },
                    )))
            });
        let popover = Popover::new("demo-popover")
            .anchor(Anchor::TopRight)
            .trigger(Button::new("popover-trigger").ghost().small().label("Popover"))
            .open(self.popover_open)
            .on_open_change(cx.listener(|this, open: &bool, _, cx| {
                this.popover_open = *open;
                cx.notify();
            }))
            .child(
                v_flex()
                    .gap_1()
                    .w(px(280.))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Popover over the page"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Anchored in the title bar, drawn by GPUI above the web view."),
                    ),
            );

        v_flex()
            .size_full()
            .child(
                TitleBar::new().child(
                    h_flex()
                        .w_full()
                        .pr_2()
                        .justify_between()
                        .child(div().text_sm().truncate().child(self.title.clone()))
                        .child(h_flex().gap_1().child(popover).child(overlays)),
                ),
            )
            .child(div().flex_1().min_h_0().child(self.browser.clone()))
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("orbit_browser=info")).init();
    let mut address = "https://example.com".to_string();
    let mut overlay = None;
    let mut dark = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dark" => dark = true,
            "--overlay" => overlay = args.next(),
            _ => address = arg,
        }
    }

    // WebKitGTK embeds only into X11 windows (XWayland under Wayland).
    #[cfg(target_os = "linux")]
    let app = gpui_kit::platform::linux(WindowingModes::X11);
    #[cfg(not(target_os = "linux"))]
    let app = gpui_kit::application();

    app.with_assets(gpui_kit::assets::Assets).run(move |cx| {
        gpui_kit::init(cx);
        orbit_browser::init(cx);
        if dark {
            Theme::change(ThemeMode::Dark, None, cx);
        }
        let bounds = Bounds::centered(None, size(px(1100.), px(760.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(480.), px(320.))),
            ..TitleBar::window_options()
        };
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Standalone::new(&address, overlay, window, cx)))
            .expect("failed to open the window");
        cx.activate(true);
    });
}
