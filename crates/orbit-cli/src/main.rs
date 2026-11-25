//! `orbit`: drive a running Orbit from the command line, scripts and agents.

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let env = orbit_cli::Env::from_process();
    let code = {
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        orbit_cli::run(&argv, env, &mut stdout.lock(), &mut stderr.lock())
    };
    std::process::exit(code);
}
