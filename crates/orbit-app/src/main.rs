#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! Orbit: a native control surface for coding agents.
//!
//! The engine runs in this process on its own thread, unless Settings ›
//! Remote names an `orbitd` host (or `ORBIT_HOST` and `ORBIT_TOKEN` do, which
//! win). `ORBIT_DATA` moves the data folder.

mod actions;
mod app_state;
mod assets;
mod automations;
mod bots;
mod composer;
mod files;
#[cfg(debug_assertions)]
mod demo;
mod diagnostics;
mod fleet;
mod forge;
mod format;
mod home;
mod inbox;
mod instance;
mod keymap;
mod notify;
mod onboarding;
mod palette;
mod prefs;
mod project;
mod review;
mod settings;
mod shortcuts;
mod shell;
mod sidebar;
mod teams;
mod theme;
mod thread;
mod thread_menu;
mod tiles;
mod ui;
mod update;

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
    let data_dir: std::path::PathBuf =
        std::env::var_os("ORBIT_DATA").map(Into::into).unwrap_or_else(EngineOptions::default_data_dir);
    // The log file and the crash reporter, before anything can fail.
    diagnostics::install(&data_dir);
    // The update installer runs as this program; a just-updated Orbit that
    // never got going is rolled back here.
    if update::handle_launch(&data_dir) {
        return;
    }
    // One Orbit per data folder: a second launch hands over and exits.
    let launch = instance::this_launch();
    let instance = match instance::claim(&data_dir, &launch) {
        Ok(instance::Claim::Primary(instance)) => instance,
        Ok(instance::Claim::Forwarded) => return,
        Err(err) => {
            eprintln!("Orbit is already running for {}, but did not answer: {err}", data_dir.display());
            std::process::exit(1);
        }
    };
    // Last run's crash report, if it left one (only the running Orbit looks).
    let crash = diagnostics::startup(&data_dir);
    let (backend, startup) = match backend() {
        Ok(started) => started,
        Err(err) => {
            eprintln!("Orbit couldn’t start its engine: {err}");
            std::process::exit(1);
        }
    };
    let shutdown = backend.clone();
    let endpoint = control_endpoint(&backend, &startup.data_dir);
    // Chat bots run beside the engine they drive: only with a local one.
    let chat_bots = match &backend {
        Backend::Local(engine) => orbit_bots::Bots::start(engine.clone(), &startup.data_dir)
            .inspect_err(|err| log::warn!("no chat bots: {err}"))
            .ok(),
        Backend::Remote(_) => None,
    };
    let chat_bots = std::sync::Mutex::new(chat_bots);
    let quit_data_dir = startup.data_dir.clone();

    // The preview browser's WebKitGTK embeds only into X11 windows
    // (XWayland under Wayland), so Linux runs on X11.
    #[cfg(target_os = "linux")]
    let app = gpui_kit::platform::linux(WindowingModes::X11);
    #[cfg(not(target_os = "linux"))]
    let app = gpui_kit::application();

    app.with_assets(assets::OrbitAssets).run(move |cx| {
        gpui_kit::init(cx);
        orbit_browser::init(cx);
        // System notifications need an identity on Windows; harmless elsewhere.
        cx.set_app_identity("dev.orbit.Orbit", "Orbit");
        orbit_terminal::init(cx);
        theme::install(cx);
        prefs::init(&startup.data_dir, cx);
        keymap::install(&startup.data_dir, cx);
        thread::init(cx);
        files::init(cx);
        project::init(cx);
        // The user's bindings go last so they win over every default.
        keymap::reload(cx);
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
        // macOS shows an application menu even without one; give it About,
        // updates, Quit and a Help menu.
        #[cfg(target_os = "macos")]
        cx.set_menus([
            Menu::new("Orbit").items([
                MenuItem::action("About Orbit", diagnostics::ShowAbout),
                MenuItem::action("Check for Updates…", update::CheckForUpdates),
                MenuItem::separator(),
                MenuItem::action("Quit Orbit", actions::Quit),
            ]),
            Menu::new("Help").items([
                MenuItem::action("Keyboard Shortcuts", actions::ShowShortcuts),
                MenuItem::action("What’s New", update::ShowWhatsNew),
                MenuItem::action("Run Setup Again…", onboarding::RunSetup),
                MenuItem::separator(),
                MenuItem::action("Collect Diagnostics…", diagnostics::CollectDiagnostics),
            ]),
        ]);
        cx.on_app_quit(move |cx| {
            diagnostics::note_clean_exit(&quit_data_dir);
            endpoint.iter().for_each(orbit_net::endpoint::LocalEndpoint::close);
            if let Some(bots) = chat_bots.lock().ok().and_then(|mut b| b.take()) {
                bots.stop();
            }
            instance::remove_endpoint_file(&quit_data_dir);
            settings::flush(cx);
            let backend = shutdown.clone();
            async move { backend.shutdown() }
        })
        .detach();

        let app = cx.new(|cx| app_state::AppState::new(backend, cx));
        diagnostics::init(crash, cx);
        update::init(&startup.data_dir, app.clone(), cx);
        onboarding::init(&startup.data_dir, app.clone(), cx);
        settings::init(app.clone(), startup, cx);
        forge::init(app.clone(), cx);
        bots::init(app.clone(), cx);
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
            Ok((window, workspace)) => {
                // This launch's own arguments, then later launches' as they come.
                let mut instance = instance;
                let _ = window.update(cx, |_, window, cx| {
                    workspace.update(cx, |ws, cx| ws.handle_launch(launch, window, cx));
                    diagnostics::on_window_open(window, cx);
                    update::on_window_open(window, cx);
                    onboarding::on_window_open(window, workspace.downgrade(), cx);
                });
                cx.spawn(async move |cx| {
                    use futures::StreamExt as _;
                    while let Some(launch) = instance.launches.next().await {
                        let workspace = workspace.clone();
                        let _ = window.update(cx, |_, window, cx| {
                            workspace.update(cx, |ws, cx| ws.handle_launch(launch, window, cx))
                        });
                    }
                })
                .detach();
            }
            Err(err) => {
                eprintln!("Orbit couldn’t open its window: {err}");
                cx.quit();
            }
        }
    });
}
