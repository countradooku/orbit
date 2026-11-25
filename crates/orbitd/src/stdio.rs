//! `orbitd --local --stdio` and `orbitd --probe`: how a client on another
//! computer reaches this one over SSH or WSL (`docs/REMOTE.md`). The relay
//! itself is `orbit_net::remote::bridge`.

use std::path::Path;

/// `orbitd --probe`: one JSON line saying what this binary is.
pub fn probe_json() -> String {
    orbit_net::remote::ProbeInfo::current().to_json()
}

/// Relays stdin/stdout to the background host on `data`, starting it from
/// this binary when it is not running. Returns the exit code.
pub fn run(data: &Path) -> i32 {
    // Only warnings, on stderr: the client shows the last line when the
    // connection fails, and stdout carries the frames.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .target(env_logger::Target::Stderr)
        .init();
    let binary = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("orbitd: couldn’t find this program’s own path: {err}");
            return 1;
        }
    };
    orbit_net::remote::bridge::run(data, &binary)
}
