//! `orbit-hook`: what agents' hooks run inside Orbit's terminals.
//!
//! Claude Code's hooks and Codex's `notify` call it with the hook payload;
//! it passes the few fields status needs to the running Orbit and exits 0
//! at once, whatever happens. Orbit finds it next to its own executable.
//! See `orbit_core::status::hook`.

fn main() {
    std::process::exit(orbit_core::status::hook::invoker_main());
}
