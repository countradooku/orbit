//! `orbit`: drive a running Orbit from the command line, scripts and agents.

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let env = orbit_cli::Env::from_process();
    // Unlocked handles: `orbit mcp` writes from its own thread.
    let code = orbit_cli::run(&argv, env, &mut std::io::stdout(), &mut std::io::stderr());
    std::process::exit(code);
}
