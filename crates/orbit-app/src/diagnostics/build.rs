//! What this build is: version, commit, profile, GPUI backend, target, and
//! the system it runs on.

use std::sync::OnceLock;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const COMMIT: &str = env!("ORBIT_COMMIT");
pub const TARGET: &str = env!("ORBIT_TARGET");

/// `release` or `debug`.
pub fn profile() -> &'static str {
    if cfg!(debug_assertions) { "debug" } else { "release" }
}

/// The GPUI renderer this binary was built with.
pub fn backend() -> &'static str {
    if cfg!(feature = "fast") { "GPUI Fast" } else { "GPUI (upstream)" }
}

/// `0.1.0 (abc1234, release, GPUI Fast)`.
pub fn summary() -> String {
    format!("{VERSION} ({COMMIT}, {}, {})", profile(), backend())
}

/// The operating system's name and version, read once in the background
/// by [`warm_up`] (a crash report must not start processes).
static SYSTEM: OnceLock<String> = OnceLock::new();

pub fn system() -> String {
    SYSTEM.get().cloned().unwrap_or_else(|| format!("{} {}", std::env::consts::OS, std::env::consts::ARCH))
}

/// Reads the system's version for [`system`], off the UI thread.
pub fn warm_up() {
    let _ = std::thread::Builder::new().name("orbit-system-info".into()).spawn(|| {
        let _ = SYSTEM.set(read_system());
    });
}

fn command_line(program: &str, args: &[&str]) -> Option<String> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = command.output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn read_system() -> String {
    let arch = std::env::consts::ARCH;
    let name = if cfg!(windows) {
        // "Microsoft Windows [Version 10.0.26100.1234]"
        command_line("cmd", &["/c", "ver"]).map(|v| v.replace("Microsoft ", "").replace(['[', ']'], ""))
    } else if cfg!(target_os = "macos") {
        command_line("sw_vers", &["-productVersion"]).map(|v| format!("macOS {v}"))
    } else {
        std::fs::read_to_string("/etc/os-release").ok().and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_owned())
        })
    };
    format!("{} {arch}", name.unwrap_or_else(|| std::env::consts::OS.to_owned()))
}
