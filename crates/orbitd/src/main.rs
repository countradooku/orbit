//! `orbitd`: run Orbit's engine headless and serve it to remote clients.
//!
//! ```text
//! orbitd [--listen 127.0.0.1:7840] [--data <dir>] [--print-token]
//! ```
//!
//! The access token is generated on first run and kept in `<data>/token`.
//! Listening on loopback is the default; expose it to other machines only
//! through an SSH tunnel or a private network such as Tailscale.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use orbit_core::{EngineHandle, EngineOptions};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct Args {
    listen: SocketAddr,
    data: PathBuf,
    print_token: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        listen: SocketAddr::from(([127, 0, 0, 1], orbit_net::DEFAULT_PORT)),
        data: EngineOptions::default_data_dir(),
        print_token: false,
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
            "-h" | "--help" => {
                println!("orbitd [--listen ADDR] [--data DIR] [--print-token]");
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

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("orbitd: {err}");
            std::process::exit(2);
        }
    };
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
    shutdown_engine.shutdown();
}
