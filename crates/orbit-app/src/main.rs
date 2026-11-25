#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! Orbit: a native control surface for coding agents.
//!
//! Where the engine runs:
//!
//! * On the host the window showed last: another computer over SSH, a WSL
//!   distro or a direct `orbitd` (`docs/REMOTE.md`). `ORBIT_HOST` and
//!   `ORBIT_TOKEN` name a direct host for one run.
//! * On this computer, in the background host, `orbitd --local` next to
//!   this executable, which this app starts when it is not running and
//!   which outlives the window (`docs/HOST.md`).
//! * In this process when the host cannot start (a notice says why), or
//!   with `--in-process` / `ORBIT_IN_PROCESS=1`.
//!
//! `ORBIT_DATA` moves the data folder.

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
mod host;
mod hosts;
mod inbox;
mod instance;
mod keymap;
mod notify;
mod onboarding;
mod palette;
mod plugins;
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
mod usage;
mod update;
mod voice;

use gpui_kit::component::TitleBar;
use gpui_kit::*;
use orbit_core::EngineOptions;
use orbit_net::Backend;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The host the window opens on, and the list it switches between.
///
/// `ORBIT_HOST` (and `ORBIT_TOKEN`) name a direct host for this run only;
/// otherwise the window opens on the host it showed last (`hosts.json`,
/// see `docs/REMOTE.md`), this computer by default. The single remote host
/// older versions kept in `<data>/connection` joins the list once.
fn startup() -> std::io::Result<(app_state::Connection, orbit_net::remote::HostsFile, settings::Startup)> {
    use orbit_net::remote::{HostEntry, HostsFile, Tools};
    let data_dir: std::path::PathBuf =
        std::env::var_os("ORBIT_DATA").map(Into::into).unwrap_or_else(EngineOptions::default_data_dir);
    let mut hosts = HostsFile::load(&data_dir);
    let legacy = settings::read_connection(&data_dir);
    if legacy.is_remote()
        && let Some(id) = hosts.adopt_connection(&legacy.host, &legacy.token)
    {
        hosts.active = id;
        if let Err(err) = hosts.save(&data_dir) {
            log::warn!("couldn’t save the hosts: {err}");
        }
    }
    let env_host = std::env::var("ORBIT_HOST").ok().filter(|h| !h.trim().is_empty());
    let entry = match &env_host {
        Some(url) => {
            let mut entry = HostEntry::url(url, &std::env::var("ORBIT_TOKEN").unwrap_or_default());
            entry.id = format!("env:{}", url.trim());
            entry
        }
        None => hosts.get(&hosts.active).cloned().unwrap_or_else(HostEntry::local),
    };
    let startup = settings::Startup { data_dir: data_dir.clone(), host_from_env: env_host.is_some() };
    let connection = app_state::connect(&entry, &data_dir, &Tools::default())?;
    Ok((connection, hosts, startup))
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
    // Restarted for an update: the previous window is still closing.
    host::wait_for_previous(&launch.args);
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
    // Reaching the engine (starting the background host and waiting for it)
    // overlaps with the interface's own setup; it is joined just before the
    // first view needs it.
    let starting = std::thread::Builder::new().name("orbit-start".into()).spawn(startup).expect("spawn the startup thread");
    let quit_data_dir = data_dir.clone();

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
        prefs::init(&data_dir, cx);
        host::init(&data_dir, cx);
        keymap::install(&data_dir, cx);
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
        let (connection, hosts, startup) = match starting.join() {
            Ok(Ok(started)) => started,
            Ok(Err(err)) => {
                eprintln!("Orbit couldn’t start its engine: {err}");
                std::process::exit(1);
            }
            Err(_) => std::process::exit(1),
        };
        let fallback = connection.fallback.clone();
        let endpoint = control_endpoint(&connection.backend, &startup.data_dir);
        // Chat bots run beside the engine they drive: only with an
        // in-process one (the background host runs its own).
        let chat_bots = match &connection.backend {
            Backend::Local(engine) => orbit_bots::Bots::start(engine.clone(), &startup.data_dir)
                .inspect_err(|err| log::warn!("no chat bots: {err}"))
                .ok(),
            Backend::Remote(_) => None,
        };
        let chat_bots = std::sync::Mutex::new(chat_bots);
        let quit_data_dir = quit_data_dir.clone();
        let app_data_dir = startup.data_dir.clone();
        let app = cx.new(|cx| app_state::AppState::new(connection, hosts, app_data_dir, cx));
        let quitting = app.downgrade();
        cx.on_app_quit(move |cx| {
            // Terminals stay in the background host; start no host again.
            let backends = quitting.update(cx, |app, _| {
                app.detach();
                app.backends()
            });
            diagnostics::note_clean_exit(&quit_data_dir);
            endpoint.iter().for_each(orbit_net::endpoint::LocalEndpoint::close);
            if let Some(bots) = chat_bots.lock().ok().and_then(|mut b| b.take()) {
                bots.stop();
            }
            instance::remove_endpoint_file(&quit_data_dir);
            settings::flush(cx);
            async move {
                for backend in backends.into_iter().flatten() {
                    backend.shutdown();
                }
            }
        })
        .detach();

        diagnostics::init(crash, cx);
        update::init(&startup.data_dir, app.clone(), cx);
        // With the background host, an update restarts the window only.
        host::install_restart(&app, cx);
        onboarding::init(&startup.data_dir, app.clone(), cx);
        voice::init(&startup.data_dir, app.clone(), cx);
        settings::init(app.clone(), startup, cx);
        forge::init(app.clone(), cx);
        bots::init(app.clone(), cx);
        usage::init(app.clone(), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(820.)), cx))),
            window_min_size: Some(size(px(720.), px(480.))),
            app_id: Some("dev.orbit.Orbit".into()),
            ..TitleBar::window_options()
        };
        let host_app = app.clone();
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            settings::reapply_appearance(window, cx);
            window.set_window_title("Orbit");
            // Closing the window leaves its terminals running in the host
            // (on Windows the window closes before the app quits).
            let closing = app.downgrade();
            window.on_window_should_close(cx, move |_, cx| {
                let _ = closing.update(cx, |app, _| app.detach());
                true
            });
            cx.new(|cx| hosts::HostFrame::new(app, window, cx))
        });
        match opened {
            Ok((window, frame)) => {
                // What the window should know about the background host.
                if let Some(reason) = fallback {
                    let _ = window.update(cx, |_, window, cx| host::show_fallback(&reason, window, cx));
                }
                cx.subscribe(&host_app, move |app, event: &app_state::AppEvent, cx| match event {
                    app_state::AppEvent::Host(notice) => {
                        let _ = window.update(cx, |_, window, cx| host::show(notice, &app, window, cx));
                    }
                    app_state::AppEvent::PluginNotice { title, body } => {
                        let _ = window.update(cx, |_, window, cx| {
                            use gpui_kit::component::WindowExt as _;
                            let note = gpui_kit::component::notification::Notification::info(body.clone()).title(title.clone());
                            window.push_notification(note, cx);
                        });
                    }
                    _ => {}
                })
                .detach();
                // This launch's own arguments, then later launches' as they come.
                let mut instance = instance;
                let _ = window.update(cx, |_, window, cx| {
                    frame.update(cx, |frame, cx| frame.handle_launch(launch, window, cx));
                    if let Some(workspace) = frame.read(cx).workspace() {
                        onboarding::on_window_open(window, workspace.downgrade(), cx);
                    }
                    diagnostics::on_window_open(window, cx);
                    update::on_window_open(window, cx);
                    voice::on_window_open(window, cx);
                });
                cx.spawn(async move |cx| {
                    use futures::StreamExt as _;
                    while let Some(launch) = instance.launches.next().await {
                        let frame = frame.clone();
                        let _ = window.update(cx, |_, window, cx| {
                            frame.update(cx, |frame, cx| frame.handle_launch(launch, window, cx))
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
