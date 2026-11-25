//! Remote hosts: running agents on another computer over SSH, or in a WSL
//! distro, with the window on this one (see `docs/REMOTE.md`).
//!
//! * [`hosts`]: the client's list of hosts (`hosts.json`), never sent to an
//!   engine; hosts found in `~/.ssh/config` ([`ssh_config`]) and WSL
//!   distros ([`wsl`]) are offered when adding one.
//! * [`Remote`]: what is done to a host through the system's own `ssh`,
//!   `scp` and `wsl.exe` (no SSH library, so the user's config, keys, agent,
//!   ProxyJump and known_hosts all apply): probe it for `orbitd`, install
//!   one, and connect with `orbitd --local --stdio` over the program's
//!   stdio ([`crate::transport::exec_url`]).
//! * [`bridge`]: the other end, `orbitd --local --stdio` on the remote: it
//!   starts that computer's background host if needed and relays to it, so
//!   the host outlives the SSH session like a local one outlives the window.
//!
//! Nothing here ever waits for a prompt: SSH runs with `BatchMode=yes`, so
//! a host key to accept or a passphrase to type fails at once with what to
//! do ([`Failure`]).

pub mod bridge;
pub mod hosts;
pub mod link;
pub mod ssh_config;
pub mod wsl;

use std::ffi::OsString;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use hosts::{HostEntry, HostKind, HostsFile};
pub use link::{LinkPhase, RemoteLink};

/// Where `orbitd` is installed on a remote host, relative to its home.
pub const INSTALL_DIR: &str = ".orbit/bin";
/// The installed binary, as the remote shell spells it.
pub const INSTALL_PATH: &str = "~/.orbit/bin/orbitd";

// ── What `orbitd --probe` says ───────────────────────────────────────────

/// One JSON line printed by `orbitd --probe`. Only ever extended.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProbeInfo {
    /// Always 1: tells this line from anything else on stdout.
    pub orbitd: u32,
    pub version: String,
    pub protocol: u16,
    /// `std::env::consts::OS`: `linux`, `macos`, `windows`.
    pub os: String,
    /// `std::env::consts::ARCH`: `x86_64`, `aarch64`.
    pub arch: String,
    /// The binary's own path, which the connection then runs.
    pub exe: String,
    /// The user's home folder there.
    pub home: String,
}

impl ProbeInfo {
    /// This binary.
    pub fn current() -> Self {
        Self {
            orbitd: 1,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol: orbit_proto::PROTOCOL_VERSION,
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            exe: std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
            home: dirs::home_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The probe line in a command's output, if there is one.
    pub fn find(output: &str) -> Option<Self> {
        output
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with('{'))
            .filter_map(|l| serde_json::from_str::<ProbeInfo>(l).ok())
            .find(|p| p.orbitd == 1)
    }

    pub fn platform(&self) -> Platform {
        Platform { os: self.os.clone(), arch: self.arch.clone() }
    }
}

/// A computer's operating system and processor, as Rust names them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub arch: String,
}

impl Platform {
    pub fn current() -> Self {
        Self { os: std::env::consts::OS.to_owned(), arch: std::env::consts::ARCH.to_owned() }
    }

    /// From `uname -sm` output ("Linux x86_64", "Darwin arm64").
    pub fn from_uname(output: &str) -> Option<Self> {
        output.lines().rev().find_map(|line| {
            let mut words = line.split_whitespace();
            let (system, machine) = (words.next()?, words.next()?);
            if words.next().is_some() {
                return None;
            }
            let os = match system {
                "Linux" => "linux",
                "Darwin" => "macos",
                "FreeBSD" => "freebsd",
                _ => return None,
            };
            let arch = match machine {
                "x86_64" | "amd64" => "x86_64",
                "aarch64" | "arm64" | "armv8l" => "aarch64",
                "armv7l" | "armv7" => "arm",
                "riscv64" => "riscv64",
                _ => return None,
            };
            Some(Self { os: os.into(), arch: arch.into() })
        })
    }

    /// The release's target triple, as the update manifest keys downloads
    /// (`docs/RELEASING.md`).
    pub fn triple(&self) -> Option<String> {
        let arch = match self.arch.as_str() {
            "x86_64" | "aarch64" => self.arch.as_str(),
            _ => return None,
        };
        match self.os.as_str() {
            "linux" => Some(format!("{arch}-unknown-linux-gnu")),
            "macos" => Some(format!("{arch}-apple-darwin")),
            "windows" => Some(format!("{arch}-pc-windows-msvc")),
            _ => None,
        }
    }

    /// "Linux (x86-64)", "macOS (Apple silicon)".
    pub fn label(&self) -> String {
        let os = match self.os.as_str() {
            "linux" => "Linux",
            "macos" => "macOS",
            "windows" => "Windows",
            "freebsd" => "FreeBSD",
            other => other,
        };
        let arch = match (self.os.as_str(), self.arch.as_str()) {
            ("macos", "aarch64") => "Apple silicon",
            ("macos", "x86_64") => "Intel",
            (_, "x86_64") => "x86-64",
            (_, "aarch64") => "ARM64",
            (_, other) => other,
        };
        format!("{os} ({arch})")
    }
}

// ── Programs ─────────────────────────────────────────────────────────────

/// The programs that reach hosts: the system's `ssh`, `scp` and `wsl.exe`
/// unless `ORBIT_SSH`, `ORBIT_SCP` or `ORBIT_WSL` name others. Each is a
/// program and the arguments that lead every call (tests put a fake here).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tools {
    pub ssh: Vec<OsString>,
    pub scp: Vec<OsString>,
    pub wsl: Vec<OsString>,
}

impl Default for Tools {
    fn default() -> Self {
        let tool = |var: &str, default: &str| -> Vec<OsString> {
            vec![std::env::var_os(var).filter(|v| !v.is_empty()).unwrap_or_else(|| default.into())]
        };
        Self { ssh: tool("ORBIT_SSH", "ssh"), scp: tool("ORBIT_SCP", "scp"), wsl: tool("ORBIT_WSL", "wsl.exe") }
    }
}

/// Words for a POSIX shell (`ssh` and `wsl.exe` hand the command line to
/// the login shell): plain words stay as they are, a leading `~/` stays
/// outside the quotes so it still means home, anything else is quoted.
pub fn shell_word(word: &str) -> String {
    let plain = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b));
    if plain(word) || matches!(word, "&&" | "||" | ">") {
        return word.to_owned();
    }
    if let Some(rest) = word.strip_prefix("~/")
        && plain(rest)
    {
        return word.to_owned();
    }
    let (home, rest) = match word.strip_prefix("~/") {
        Some(rest) => ("~/", rest),
        None => ("", word),
    };
    format!("{home}'{}'", rest.replace('\'', r"'\''"))
}

/// Why something done to a host failed, in words for the user, and the
/// fix when there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
    /// A command the user runs once in a terminal (accept a host key, sign
    /// in with a passphrase or password).
    pub run_once: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// SSH needs a key, passphrase or password it may not ask for.
    Auth,
    /// The host key is unknown or changed.
    HostKey,
    /// The host could not be reached.
    Unreachable,
    /// `ssh`, `scp` or `wsl.exe` is not installed here.
    NoTool,
    /// No such WSL distro.
    NoDistro,
    Other,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl Failure {
    pub fn other(message: impl Into<String>) -> Self {
        Self { kind: FailureKind::Other, message: message.into(), run_once: None }
    }
}

/// Reads what SSH or WSL wrote when a command failed. `target` names the
/// host in messages; `interactive` is the command to run by hand.
pub fn diagnose(stderr: &str, target: &str, interactive: Option<String>) -> Failure {
    let lower = stderr.to_ascii_lowercase();
    let has = |needle: &str| lower.contains(needle);
    if has("remote host identification has changed") {
        return Failure {
            kind: FailureKind::HostKey,
            message: format!(
                "{target}’s host key changed since you last connected. If you expected that (the machine was reinstalled), \
                 remove the old key with ssh-keygen -R and connect once by hand; otherwise someone may be intercepting the connection."
            ),
            run_once: interactive,
        };
    }
    if has("host key verification failed") || has("no matching host key") || has("host key for") && has("not known") {
        return Failure {
            kind: FailureKind::HostKey,
            message: format!("SSH doesn’t know {target}’s host key yet. Connect once in a terminal to check and accept it."),
            run_once: interactive,
        };
    }
    if has("permission denied") || has("too many authentication failures") || has("authentication failed") {
        return Failure {
            kind: FailureKind::Auth,
            message: format!(
                "SSH couldn’t sign in to {target} without asking. Add your key to the SSH agent (ssh-add), \
                 or connect once in a terminal to type a passphrase or password."
            ),
            run_once: interactive,
        };
    }
    if has("could not resolve hostname") || has("name or service not known") || has("nodename nor servname") {
        return Failure { kind: FailureKind::Unreachable, message: format!("Couldn’t find {target}. Check its name and your network."), run_once: None };
    }
    if has("connection refused") {
        return Failure { kind: FailureKind::Unreachable, message: format!("{target} refused the SSH connection. Is its SSH server running?"), run_once: None };
    }
    if has("timed out") || has("no route to host") || has("network is unreachable") {
        return Failure { kind: FailureKind::Unreachable, message: format!("No answer from {target}. Is it on and reachable from here?"), run_once: None };
    }
    if has("there is no distribution with the supplied name") || has("wsl_e_distro_not_found") {
        return Failure { kind: FailureKind::NoDistro, message: format!("There is no WSL distro named {target}."), run_once: None };
    }
    match last_line(stderr) {
        Some(line) => Failure::other(format!("{target}: {line}")),
        None => Failure::other(format!("{target} closed the connection.")),
    }
}

/// The last line worth showing from a program's stderr.
fn last_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with("Warning: Permanently added"))
        .map(|l| l.trim_start_matches("orbitd: ").to_owned())
}

/// The reason a connection through a program (`exec:`) ended, for the
/// connection status.
pub(crate) fn explain_exit(stderr: &str, status: Option<std::process::ExitStatus>) -> String {
    if last_line(stderr).is_none() {
        return match status {
            Some(status) if !status.success() => format!("The connection ended ({status})."),
            _ => "The host closed the connection.".to_owned(),
        };
    }
    diagnose(stderr, "The host", None).message
}

/// A program that could not be started.
pub(crate) fn explain_spawn(program: &str, err: &std::io::Error) -> String {
    if err.kind() == std::io::ErrorKind::NotFound {
        format!("Couldn’t run {program}: it is not installed or not on PATH.")
    } else {
        format!("Couldn’t run {program}: {err}")
    }
}

// ── Operations ───────────────────────────────────────────────────────────

/// What a host has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// An `orbitd` this client can talk to.
    Ready(ProbeInfo),
    /// An `orbitd` of another protocol: install this version's.
    Outdated(ProbeInfo),
    /// No `orbitd` (or one too old to answer `--probe`). `platform` is what
    /// to install, when `uname` answered.
    Missing { platform: Option<Platform> },
}

impl Probe {
    /// The platform to install for.
    pub fn platform(&self) -> Option<Platform> {
        match self {
            Probe::Ready(info) | Probe::Outdated(info) => Some(info.platform()),
            Probe::Missing { platform } => platform.clone(),
        }
    }
}

/// A remote host and the programs that reach it.
pub struct Remote<'a> {
    pub entry: &'a HostEntry,
    pub tools: &'a Tools,
}

/// How long a probe or install step may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(60);

impl<'a> Remote<'a> {
    pub fn new(entry: &'a HostEntry, tools: &'a Tools) -> Self {
        Self { entry, tools }
    }

    /// The program and leading arguments that run a shell command there
    /// (the command's words follow). `None` for hosts that are not reached
    /// through a program.
    pub fn prefix(&self) -> Option<Vec<OsString>> {
        match &self.entry.kind {
            HostKind::Ssh { target, port, identity } => {
                let mut argv = self.tools.ssh.clone();
                argv.extend(ssh_options(*port, identity, false).into_iter().map(OsString::from));
                argv.push("-T".into());
                argv.push("--".into());
                argv.push(target.into());
                Some(argv)
            }
            HostKind::Wsl { distro } => {
                let mut argv = self.tools.wsl.clone();
                argv.extend(["-d".into(), distro.into(), "--".into()]);
                Some(argv)
            }
            HostKind::Local | HostKind::Url { .. } => None,
        }
    }

    /// The `orbitd` to try first: the host's own setting, else the install
    /// location.
    fn orbitd(&self) -> String {
        let custom = self.entry.orbitd.trim();
        shell_word(if custom.is_empty() { INSTALL_PATH } else { custom })
    }

    /// The command to run by hand once (to accept a host key or type a
    /// passphrase): `ssh box` or `ssh -p 2222 user@box`.
    pub fn interactive_command(&self) -> Option<String> {
        match &self.entry.kind {
            HostKind::Ssh { target, port, identity } => {
                let mut words = vec!["ssh".to_owned()];
                if let Some(port) = port {
                    words.push(format!("-p {port}"));
                }
                if !identity.trim().is_empty() {
                    words.push(format!("-i {}", quote_local(identity.trim())));
                }
                words.push(target.clone());
                Some(words.join(" "))
            }
            HostKind::Wsl { distro } => Some(format!("wsl.exe -d {}", quote_local(distro))),
            _ => None,
        }
    }

    /// The host's name in messages.
    pub fn label(&self) -> String {
        self.entry.label()
    }

    /// Runs `words` there; returns stdout, or why it failed. `input` is
    /// written to its stdin (a file being copied).
    fn run(&self, words: &[String], input: Option<&Path>) -> Result<String, Failure> {
        let prefix = self.prefix().ok_or_else(|| Failure::other("This host is not reached over SSH or WSL."))?;
        let mut argv = prefix;
        argv.extend(words.iter().map(OsString::from));
        let output = run_program(&argv, input)?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        Err(self.failure(&output))
    }

    fn failure(&self, output: &std::process::Output) -> Failure {
        let stderr = decode_output(&output.stderr);
        let mut failure = diagnose(&stderr, &self.label(), self.interactive_command());
        if failure.kind == FailureKind::Other && stderr.trim().is_empty() {
            failure.message = format!("{} failed ({}).", self.label(), output.status);
        }
        failure
    }

    /// What `orbitd` the host has, and its platform. One round trip:
    /// `orbitd --probe || orbitd --probe || uname -sm`.
    pub fn probe(&self) -> Result<Probe, Failure> {
        let words: Vec<String> =
            [self.orbitd().as_str(), "--probe", "||", "orbitd", "--probe", "||", "uname", "-sm"].map(str::to_owned).into();
        let prefix = self.prefix().ok_or_else(|| Failure::other("This host is not reached over SSH or WSL."))?;
        let mut argv = prefix;
        argv.extend(words.iter().map(OsString::from));
        let output = run_program(&argv, None)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(info) = ProbeInfo::find(&stdout) {
            return Ok(if info.protocol == orbit_proto::PROTOCOL_VERSION { Probe::Ready(info) } else { Probe::Outdated(info) });
        }
        if let Some(platform) = Platform::from_uname(&stdout) {
            return Ok(Probe::Missing { platform: Some(platform) });
        }
        // Neither answered: SSH itself failed (exit 255), or the shell is
        // not a POSIX one.
        if output.status.success() {
            return Ok(Probe::Missing { platform: None });
        }
        Err(self.failure(&output))
    }

    /// Copies `binary` (an `orbitd` built for the host's platform) to
    /// [`INSTALL_PATH`] and checks that it runs.
    pub fn install(&self, binary: &Path) -> Result<ProbeInfo, Failure> {
        if !binary.is_file() {
            return Err(Failure::other(format!("{} is not a file.", binary.display())));
        }
        let staged = format!("{INSTALL_DIR}/orbitd.new");
        let words = |w: &[&str]| -> Vec<String> { w.iter().map(|s| (*s).to_owned()).collect() };
        match &self.entry.kind {
            HostKind::Ssh { target, port, identity } => {
                self.run(&words(&["mkdir", "-p", &format!("~/{INSTALL_DIR}")]), None)?;
                // scp's remote paths are relative to the home folder.
                let mut argv = self.tools.scp.clone();
                argv.extend(ssh_options(*port, identity, true).into_iter().map(OsString::from));
                argv.push("--".into());
                argv.push(binary.as_os_str().to_owned());
                argv.push(format!("{target}:{staged}").into());
                let output = run_program(&argv, None)?;
                if !output.status.success() {
                    return Err(self.failure(&output));
                }
            }
            HostKind::Wsl { .. } => {
                self.run(&words(&["mkdir", "-p", &format!("~/{INSTALL_DIR}")]), None)?;
                self.run(&words(&["cat", ">", &format!("~/{staged}")]), Some(binary))?;
            }
            HostKind::Local | HostKind::Url { .. } => return Err(Failure::other("Only SSH and WSL hosts are installed to.")),
        }
        let out = self.run(
            &words(&[
                "chmod",
                "755",
                &format!("~/{staged}"),
                "&&",
                "mv",
                "-f",
                &format!("~/{staged}"),
                INSTALL_PATH,
                "&&",
                INSTALL_PATH,
                "--probe",
            ]),
            None,
        )?;
        let info = ProbeInfo::find(&out).ok_or_else(|| {
            Failure::other(format!("The copied orbitd doesn’t run on {}: is it built for that computer?", self.label()))
        })?;
        if info.protocol != orbit_proto::PROTOCOL_VERSION {
            return Err(Failure::other(format!(
                "The copied orbitd is Orbit {} (protocol {}); this Orbit needs protocol {}.",
                info.version,
                info.protocol,
                orbit_proto::PROTOCOL_VERSION
            )));
        }
        Ok(info)
    }

    /// The `exec:` address that reaches the host's background host through
    /// the `orbitd` the probe found (starting the host when it is not
    /// running).
    pub fn connect_url(&self, probe: &ProbeInfo) -> Option<String> {
        let mut argv: Vec<String> = self.prefix()?.into_iter().map(|w| w.to_string_lossy().into_owned()).collect();
        argv.push(shell_word(&probe.exe));
        argv.push("--local".into());
        argv.push("--stdio".into());
        let data = self.entry.data.trim();
        if !data.is_empty() {
            argv.push("--data".into());
            argv.push(shell_word(data));
        }
        Some(crate::transport::exec_url(&argv))
    }
}

/// SSH options for every call: never prompt, give up on a dead network
/// within seconds, notice a dropped link within a minute. `scp` spells the
/// port `-P`.
fn ssh_options(port: Option<u16>, identity: &str, scp: bool) -> Vec<String> {
    let mut options = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=15".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ];
    if let Some(port) = port {
        options.push(if scp { "-P" } else { "-p" }.into());
        options.push(port.to_string());
    }
    let identity = identity.trim();
    if !identity.is_empty() {
        options.push("-i".into());
        options.push(identity.into());
    }
    options
}

/// Quotes a word for the user's own shell, for commands shown to copy.
fn quote_local(word: &str) -> String {
    if word.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./:@~\\".contains(&b)) {
        word.to_owned()
    } else {
        format!("\"{}\"", word.replace('"', "\\\""))
    }
}

/// `wsl.exe` writes its own messages in UTF-16; everything else in UTF-8.
pub fn decode_output(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes.len().is_multiple_of(2) && bytes.iter().skip(1).step_by(2).filter(|b| **b == 0).count() * 2 >= bytes.len() / 2 {
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        return String::from_utf16_lossy(&units).trim_start_matches('\u{feff}').to_owned();
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Runs a program to completion (with `input` as its stdin) within
/// [`STEP_TIMEOUT`], with no console window.
fn run_program(argv: &[OsString], input: Option<&Path>) -> Result<std::process::Output, Failure> {
    let (program, args) = argv.split_first().ok_or_else(|| Failure::other("nothing to run"))?;
    let mut cmd = Command::new(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
    no_window(&mut cmd);
    let name = Path::new(program).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut child = cmd.spawn().map_err(|e| Failure {
        kind: if e.kind() == std::io::ErrorKind::NotFound { FailureKind::NoTool } else { FailureKind::Other },
        message: explain_spawn(&name, &e),
        run_once: None,
    })?;
    let writer = match (input, child.stdin.take()) {
        (Some(path), Some(mut stdin)) => {
            let path = path.to_owned();
            Some(std::thread::spawn(move || -> std::io::Result<()> {
                let mut file = std::fs::File::open(path)?;
                std::io::copy(&mut file, &mut stdin)?;
                stdin.flush()
            }))
        }
        _ => None,
    };
    // Read both pipes on threads so a chatty program never blocks, and
    // stop waiting after the timeout.
    let (tx, rx) = std::sync::mpsc::channel();
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = match rx.recv_timeout(STEP_TIMEOUT) {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => return Err(Failure::other(format!("{name} failed: {err}"))),
        Err(_) => {
            kill(pid);
            return Err(Failure { kind: FailureKind::Unreachable, message: format!("{name} took longer than a minute; it was stopped."), run_once: None });
        }
    };
    if let Some(writer) = writer
        && let Ok(Err(err)) = writer.join()
        && output.status.success()
    {
        return Err(Failure::other(format!("Couldn’t send the file: {err}")));
    }
    Ok(output)
}

/// No console window for a program started from a windowed app.
pub fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

fn kill(pid: u32) {
    #[cfg(unix)]
    // SAFETY: plain signal to a child we started.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/F", "/T", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null());
        no_window(&mut cmd);
        let _ = cmd.status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_lines_are_found_among_other_output() {
        let info = ProbeInfo { orbitd: 1, version: "0.1.0".into(), protocol: 13, os: "linux".into(), arch: "x86_64".into(), ..Default::default() };
        let out = format!("motd line\n{}\n", info.to_json());
        assert_eq!(ProbeInfo::find(&out), Some(info));
        assert_eq!(ProbeInfo::find("{\"something\":1}\nLinux x86_64\n"), None);
    }

    #[test]
    fn uname_output_names_the_platform() {
        let linux = Platform::from_uname("bash: orbitd: command not found\nLinux x86_64\n").unwrap();
        assert_eq!(linux, Platform { os: "linux".into(), arch: "x86_64".into() });
        assert_eq!(linux.triple().as_deref(), Some("x86_64-unknown-linux-gnu"));
        let mac = Platform::from_uname("Darwin arm64").unwrap();
        assert_eq!(mac.triple().as_deref(), Some("aarch64-apple-darwin"));
        assert_eq!(mac.label(), "macOS (Apple silicon)");
        assert_eq!(Platform::from_uname("MINGW64_NT-10.0 x86_64"), None);
    }

    #[test]
    fn shell_words_keep_home_and_quote_the_rest() {
        assert_eq!(shell_word("~/.orbit/bin/orbitd"), "~/.orbit/bin/orbitd");
        assert_eq!(shell_word("--stdio"), "--stdio");
        assert_eq!(shell_word("~/my tools/orbitd"), "~/'my tools/orbitd'");
        assert_eq!(shell_word("/opt/it's/orbitd"), "'/opt/it'\\''s/orbitd'");
        assert_eq!(shell_word("&&"), "&&");
    }

    #[test]
    fn ssh_errors_become_fixes() {
        let auth = diagnose("user@box: Permission denied (publickey).\r\n", "box", Some("ssh box".into()));
        assert_eq!(auth.kind, FailureKind::Auth);
        assert_eq!(auth.run_once.as_deref(), Some("ssh box"));
        let key = diagnose("No ED25519 host key is known for box and you have requested strict checking.\nHost key verification failed.\n", "box", None);
        assert_eq!(key.kind, FailureKind::HostKey);
        let dns = diagnose("ssh: Could not resolve hostname nope: No such host is known.\n", "nope", None);
        assert_eq!(dns.kind, FailureKind::Unreachable);
        let other = diagnose("orbitd: The host here is busy.\n", "box", None);
        assert_eq!(other.message, "box: The host here is busy.");
    }

    #[test]
    fn wsl_messages_in_utf16_are_read() {
        let text = "There is no distribution with the supplied name.\r\n";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_output(&bytes), text);
        assert_eq!(decode_output(b"plain"), "plain");
        assert_eq!(diagnose(&decode_output(&bytes), "Ubuntu", None).kind, FailureKind::NoDistro);
    }

    #[test]
    fn ssh_hosts_are_reached_without_prompts() {
        let entry = HostEntry::ssh("dev@box", Some(2222), "");
        let tools = Tools { ssh: vec!["ssh".into()], scp: vec!["scp".into()], wsl: vec!["wsl.exe".into()] };
        let remote = Remote::new(&entry, &tools);
        let prefix: Vec<String> = remote.prefix().unwrap().into_iter().map(|w| w.into_string().unwrap()).collect();
        assert!(prefix.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(prefix.windows(2).any(|w| w == ["-p", "2222"]));
        assert_eq!(prefix.last().map(String::as_str), Some("dev@box"));
        assert_eq!(remote.interactive_command().as_deref(), Some("ssh -p 2222 dev@box"));
        let probe = ProbeInfo { exe: "/home/dev/.orbit/bin/orbitd".into(), ..Default::default() };
        let url = remote.connect_url(&probe).unwrap();
        let argv = crate::transport::exec_argv(&url).unwrap();
        assert_eq!(&argv[argv.len() - 3..], ["/home/dev/.orbit/bin/orbitd", "--local", "--stdio"]);
        let wsl = HostEntry::wsl("Ubuntu");
        let prefix: Vec<String> = Remote::new(&wsl, &tools).prefix().unwrap().into_iter().map(|w| w.into_string().unwrap()).collect();
        assert_eq!(prefix, ["wsl.exe", "-d", "Ubuntu", "--"]);
    }
}
