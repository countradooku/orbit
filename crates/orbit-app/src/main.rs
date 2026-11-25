#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! Orbit: a native control surface for coding agents.
//!
//! The engine runs in this process on its own thread; set `ORBIT_HOST` and
//! `ORBIT_TOKEN` to drive a remote `orbitd` instead.

mod actions;
mod app_state;
mod assets;
mod composer;
mod fleet;
mod format;
mod home;
mod inbox;
mod palette;
mod review;
mod settings;
mod shell;
mod sidebar;
mod theme;
mod thread;
mod ui;

use gpui_kit::component::TitleBar;
use gpui_kit::*;
use orbit_core::{EngineHandle, EngineOptions};
use orbit_net::{Backend, RemoteClient};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn backend() -> std::io::Result<Backend> {
    if let Ok(host) = std::env::var("ORBIT_HOST") {
        let token = std::env::var("ORBIT_TOKEN").unwrap_or_default();
        return Ok(Backend::Remote(RemoteClient::connect(host, token)?));
    }
    let data_dir = std::env::var_os("ORBIT_DATA").map(Into::into).unwrap_or_else(EngineOptions::default_data_dir);
    Ok(Backend::Local(EngineHandle::start(EngineOptions { data_dir, probe_providers: true })?))
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let backend = match backend() {
        Ok(backend) => backend,
        Err(err) => {
            eprintln!("Orbit couldn’t start its engine: {err}");
            std::process::exit(1);
        }
    };
    let shutdown = backend.clone();

    gpui_kit::application().with_assets(assets::OrbitAssets).run(move |cx| {
        gpui_kit::init(cx);
        theme::install(cx);
        actions::bind(cx);
        cx.set_global(settings::Preferences::default());
        cx.on_action(|_: &actions::Quit, cx| cx.quit());
        cx.on_app_quit(move |_| {
            let backend = shutdown.clone();
            async move { backend.shutdown() }
        })
        .detach();

        let app = cx.new(|cx| app_state::AppState::new(backend, cx));
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(820.)), cx))),
            window_min_size: Some(size(px(720.), px(480.))),
            app_id: Some("dev.orbit.Orbit".into()),
            ..TitleBar::window_options()
        };
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            theme::apply(theme::Appearance::System, window, cx);
            window.set_window_title("Orbit");
            cx.new(|cx| shell::Workspace::new(app, window, cx))
        });
        match opened {
            Ok((_, workspace)) => {
                let _ = workspace;
            }
            Err(err) => {
                eprintln!("Orbit couldn’t open its window: {err}");
                cx.quit();
            }
        }
    });
}
