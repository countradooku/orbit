//! `orbitd`: run Orbit's engine headless and serve it to clients.
//!
//! ```text
//! orbitd [--listen 127.0.0.1:7840] [--data <dir>] [--print-token]
//! orbitd --local [--data <dir>] [--grace-ms <ms>]
//! ```
//!
//! **Server mode** (the default) serves remote clients on `--listen`. The
//! access token is generated on first run and kept in `<data>/token`.
//! Listening on loopback is the default; expose it to other machines only
//! through an SSH tunnel or a private network such as Tailscale.
//!
//! **Local mode** (`--local`) is the desktop's background host, started by
//! the app (see `docs/HOST.md`). It owns the data folder, serves the app
//! over a named pipe or Unix socket and the CLI over loopback WebSockets
//! (`endpoint.json`), and exits by itself: once no window is attached and,
//! per `host.json`, nothing is working, after a grace period; or when a
//! restart is requested. It logs to `<data>/logs/orbitd.log`.

mod fake;
mod lifetime;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use orbit_core::{EngineHandle, EngineOptions};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct Args {
    listen: SocketAddr,
    data: PathBuf,
    print_token: bool,
    local: bool,
    /// How long the local host waits with nothing to do before it exits.
    grace: Option<std::time::Duration>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        listen: SocketAddr::from(([127, 0, 0, 1], orbit_net::DEFAULT_PORT)),
        data: EngineOptions::default_data_dir(),
        print_token: false,
        local: false,
        grace: std::env::var("ORBIT_HOST_GRACE_MS").ok().and_then(|v| v.trim().parse().ok()).map(std::time::Duration::from_millis),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--listen" => {
                let value = it.next().ok_or("--listen needs an address")?;
                args.listen = value.parse().map_err(|e| format!("--listen {value}: {e}"))?;
            }
            "--data" => args.data = PathBuf::from(it.next().ok_or("--data needs a folder")?),
            "--print-token" => args.print_token = true,
            "--local" => args.local = true,
            "--grace-ms" => {
                let value = it.next().ok_or("--grace-ms needs a number")?;
                let ms = value.parse().map_err(|e| format!("--grace-ms {value}: {e}"))?;
                args.grace = Some(std::time::Duration::from_millis(ms));
            }
            "-h" | "--help" => {
                println!("orbitd [--listen ADDR] [--data DIR] [--print-token]\norbitd --local [--data DIR] [--grace-ms MS]");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

/// 128 random bits from the OS-seeded hasher keys, hex encoded.
fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

fn load_token(data: &Path) -> std::io::Result<String> {
    let path = data.join("token");
    if let Ok(token) = std::fs::read_to_string(&path) {
        let token = token.trim().to_owned();
        if token.len() >= 32 {
            return Ok(token);
        }
    }
    std::fs::create_dir_all(data)?;
    let token = new_token();
    write_private(&path, &token)?;
    Ok(token)
}

/// Writes the token so only its owner can read it. On Unix that takes mode
/// 0600 (the default umask leaves files world-readable, and the token grants
/// full control of the engine); on Windows the per-user profile's ACL does it.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        // `mode` applies only to a new file; tighten one left by an older run.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(contents.as_bytes())
    }
    #[cfg(not(unix))]
    std::fs::write(path, contents)
}

/// The local host has no console: it logs to `<data>/logs/orbitd.log`,
/// moved to `orbitd.log.old` once it has grown past a megabyte.
fn init_local_log(data: &Path) {
    let path = data.join(orbit_net::host::LOG_FILE);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1024 * 1024) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    let mut builder = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }
    builder.init();
    // There is no stderr to print a panic to.
    std::panic::set_hook(Box::new(|info| log::error!("{info}")));
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("orbitd: {err}");
            std::process::exit(2);
        }
    };
    if args.local {
        init_local_log(&args.data);
        std::process::exit(run_local(&args));
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let token = match load_token(&args.data) {
        Ok(token) => token,
        Err(err) => {
            eprintln!("orbitd: couldn’t read or create the token in {}: {err}", args.data.display());
            std::process::exit(1);
        }
    };
    if args.print_token {
        println!("{token}");
        return;
    }
    let engine = match EngineHandle::start(EngineOptions { data_dir: args.data.clone(), probe_providers: true }) {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("orbitd: couldn’t open {}: {err}", args.data.display());
            std::process::exit(1);
        }
    };
    // The local control endpoint for the `orbit` CLI (see orbit_net::endpoint).
    let endpoint = orbit_net::endpoint::start(engine.clone(), &args.data, "orbitd")
        .inspect_err(|err| log::warn!("no local control endpoint: {err}"))
        .ok();
    // Chat bots (Telegram, Slack, Discord), outbound connections only.
    let bots = orbit_bots::Bots::start(engine.clone(), &args.data)
        .inspect_err(|err| log::warn!("no chat bots: {err}"))
        .ok();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let shutdown_engine = engine.clone();
    runtime.block_on(async move {
        tokio::select! {
            result = orbit_net::serve(engine, orbit_net::ServerOptions { listen: args.listen, token }) => {
                if let Err(err) = result {
                    eprintln!("orbitd: {err}");
                }
            }
            _ = tokio::signal::ctrl_c() => log::info!("shutting down"),
        }
    });
    if let Some(endpoint) = endpoint {
        endpoint.close();
    }
    if let Some(bots) = bots {
        bots.stop();
    }
    shutdown_engine.shutdown();
}

/// The desktop's background host. Returns the process exit code.
fn run_local(args: &Args) -> i32 {
    log::info!(
        "background host {} starting on {} (pid {})",
        env!("CARGO_PKG_VERSION"),
        args.data.display(),
        std::process::id()
    );
    // The store's lock makes this the only engine on the folder. A host
    // that is shutting down (a restart) releases it within seconds; one
    // that keeps it is another live host, and this one stops.
    let started = std::time::Instant::now();
    let engine = loop {
        let options = EngineOptions { data_dir: args.data.clone(), probe_providers: true };
        let opened = match fake::launcher_from_env() {
            Some(launcher) => EngineHandle::start_with(EngineOptions { probe_providers: false, ..options }, launcher),
            None => EngineHandle::start(options),
        };
        match opened {
            Ok(engine) => break engine,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock && started.elapsed().as_secs() < 5 => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(err) => {
                log::error!("couldn’t open {}: {err}", args.data.display());
                return if err.kind() == std::io::ErrorKind::WouldBlock { 3 } else { 1 };
            }
        }
    };
    let control = Arc::new(orbit_net::host::HostControl::default());
    let options = orbit_net::endpoint::EndpointOptions { control: Some(control.clone()), local: true };
    let endpoint = match orbit_net::endpoint::start_with(engine.clone(), &args.data, "host", options) {
        Ok(endpoint) => endpoint,
        Err(err) => {
            log::error!("couldn’t serve the app: {err}");
            engine.shutdown();
            return 1;
        }
    };
    let bots = orbit_bots::Bots::start(engine.clone(), &args.data)
        .inspect_err(|err| log::warn!("no chat bots: {err}"))
        .ok();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let grace = lifetime::Grace::with_override(args.grace);
    let exit = runtime.block_on(async {
        tokio::select! {
            exit = lifetime::run(engine.clone(), control, args.data.clone(), grace) => exit,
            _ = tokio::signal::ctrl_c() => lifetime::Exit::Signal,
        }
    });
    log::info!("background host stopping: {exit:?}");
    // Stop taking connections first, so a reconnecting app starts the next
    // host instead of reaching this one while it shuts down.
    endpoint.close();
    if let Some(bots) = bots {
        bots.stop();
    }
    engine.shutdown();
    log::info!("background host stopped");
    0
}
