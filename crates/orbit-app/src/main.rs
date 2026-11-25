#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! Orbit: a native control surface for coding agents.
//!
//! The engine runs in this process on its own thread, unless Settings ›
//! Remote names an `orbitd` host (or `ORBIT_HOST` and `ORBIT_TOKEN` do, which
//! win). `ORBIT_DATA` moves the data folder.

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
mod thread_menu;
mod ui;

use gpui_kit::component::TitleBar;
use gpui_kit::*;
use orbit_core::{EngineHandle, EngineOptions};
use orbit_net::{Backend, RemoteClient};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn backend() -> std::io::Result<(Backend, settings::Startup)> {
    let data_dir: std::path::PathBuf =
        std::env::var_os("ORBIT_DATA").map(Into::into).unwrap_or_else(EngineOptions::default_data_dir);
    let remote = settings::read_connection(&data_dir);
    let env_host = std::env::var("ORBIT_HOST").ok().filter(|h| !h.trim().is_empty());
    let target = match &env_host {
        Some(host) => Some((host.clone(), std::env::var("ORBIT_TOKEN").unwrap_or_default())),
        None if remote.is_remote() => Some((remote.host.trim().to_owned(), remote.token.clone())),
        None => None,
    };
    let startup = settings::Startup {
        data_dir: data_dir.clone(),
        remote,
        connected_host: target.as_ref().map(|(host, _)| host.clone()),
        host_from_env: env_host.is_some(),
    };
    let backend = match target {
        Some((host, token)) => Backend::Remote(RemoteClient::connect(host, token)?),
        None => Backend::Local(EngineHandle::start(EngineOptions { data_dir, probe_providers: true })?),
    };
    Ok((backend, startup))
}

/// Serves the in-process engine to the `orbit` CLI on loopback.
fn control_endpoint(backend: &Backend, data_dir: &std::path::Path) -> Option<orbit_net::endpoint::LocalEndpoint> {
    let Backend::Local(engine) = backend else { return None };
    orbit_net::endpoint::start(engine.clone(), data_dir, "desktop")
        .inspect_err(|err| log::warn!("no local control endpoint: {err}"))
        .ok()
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let (backend, startup) = match backend() {
        Ok(started) => started,
        Err(err) => {
            eprintln!("Orbit couldn’t start its engine: {err}");
            std::process::exit(1);
        }
    };
    let shutdown = backend.clone();
    let endpoint = control_endpoint(&backend, &startup.data_dir);

    gpui_kit::application().with_assets(assets::OrbitAssets).run(move |cx| {
        gpui_kit::init(cx);
        // System notifications need an identity on Windows; harmless elsewhere.
        cx.set_app_identity("dev.orbit.Orbit", "Orbit");
        orbit_terminal::init(cx);
        theme::install(cx);
        actions::bind(cx);
        cx.on_action(|_: &actions::Quit, cx| cx.quit());
        // Orbit is one window. Windows quits when its last window closes;
        // macOS and Linux keep the process alive with nothing to show, so
        // quit there too (and shut the engine down via `on_app_quit`).
        #[cfg(not(windows))]
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        // macOS shows an application menu even without one; give it Quit.
        #[cfg(target_os = "macos")]
        cx.set_menus([Menu::new("Orbit").items([MenuItem::action("Quit Orbit", actions::Quit)])]);
        cx.on_app_quit(move |cx| {
            endpoint.iter().for_each(orbit_net::endpoint::LocalEndpoint::close);
            settings::flush(cx);
            let backend = shutdown.clone();
            async move { backend.shutdown() }
        })
        .detach();

        let app = cx.new(|cx| app_state::AppState::new(backend, cx));
        settings::init(app.clone(), startup, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(820.)), cx))),
            window_min_size: Some(size(px(720.), px(480.))),
            app_id: Some("dev.orbit.Orbit".into()),
            ..TitleBar::window_options()
        };
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            settings::reapply_appearance(window, cx);
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
