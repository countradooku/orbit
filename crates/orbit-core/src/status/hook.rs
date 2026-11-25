//! Agent hooks for agents run by hand in Orbit's terminals.
//!
//! When the user types `claude` or `codex` in an Orbit terminal, the agent's
//! own hooks tell Orbit what it does. Nothing in the user's configuration
//! is touched: everything rides on the terminal's environment.
//!
//! * **Endpoint.** The engine listens on a Unix socket in the data folder
//!   (a named pipe on Windows, named after the data folder), so the path is
//!   fixed and a restarted Orbit is reachable at the same place.
//! * **Token.** Each terminal gets a random token in `ORBIT_TERM_TOKEN`; a
//!   call without a live terminal's token is ignored.
//! * **Invoker.** Hooks run `orbit-hook`, a tiny program next to Orbit's
//!   executable ([`invoker_main`]): it reads the hook payload, keeps the
//!   few fields status needs and sends them to the endpoint. It never
//!   blocks the agent for long, never prints, and always exits 0. When the
//!   endpoint cannot be reached (an agent on an SSH host, say) it writes
//!   the same report as an `OSC 7501` sequence to the terminal instead.
//! * **Injection.** Claude Code gets `--settings <orbit file>` with the
//!   hooks, and Codex `-c notify=[…]`, through a wrapper defined only in
//!   Orbit's terminals: a `PATH` shim on Unix, a PowerShell function on
//!   Windows. Subcommands (`claude mcp`, `claude update`…) run untouched,
//!   and Codex is left alone when the user configured `notify` already.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::term::HookCall;

pub const ENV_ENDPOINT: &str = "ORBIT_HOOK_ENDPOINT";
pub const ENV_TOKEN: &str = "ORBIT_TERM_TOKEN";
/// The invoker's path, for custom scripts.
pub const ENV_INVOKER: &str = "ORBIT_HOOK";
pub const ENV_CLAUDE_SETTINGS: &str = "ORBIT_CLAUDE_SETTINGS";
pub const ENV_CODEX_NOTIFY: &str = "ORBIT_CODEX_NOTIFY";
pub const ENV_SHIM_DIR: &str = "ORBIT_SHIM_DIR";

/// Longest message the endpoint reads.
const MAX_MESSAGE: usize = 8 * 1024;
/// Longest field kept from a payload.
const MAX_FIELD: usize = 512;
/// How long the invoker may take to reach the endpoint.
const SEND_TIMEOUT: Duration = Duration::from_millis(500);
const MAGIC: &str = "orbit-hook 1";

/// One hook call as the endpoint receives it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookMessage {
    pub token: String,
    pub call: HookCall,
}

impl HookMessage {
    /// `orbit-hook 1` then one `key value` line per field.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = format!("{MAGIC}\n");
        for (key, value) in [
            ("token", &self.token),
            ("agent", &self.call.agent),
            ("event", &self.call.event),
            ("tool", &self.call.tool),
            ("kind", &self.call.kind),
            ("message", &self.call.message),
        ] {
            if !value.is_empty() {
                out.push_str(key);
                out.push(' ');
                out.push_str(&one_line(value));
                out.push('\n');
            }
        }
        out.into_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let mut lines = text.lines();
        if lines.next()? != MAGIC {
            return None;
        }
        let mut msg = HookMessage::default();
        for line in lines {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            let value = value.to_owned();
            match key {
                "token" => msg.token = value,
                "agent" => msg.call.agent = value,
                "event" => msg.call.event = value,
                "tool" => msg.call.tool = value,
                "kind" => msg.call.kind = value,
                "message" => msg.call.message = value,
                _ => {}
            }
        }
        (!msg.token.is_empty()).then_some(msg)
    }
}

fn one_line(value: &str) -> String {
    let mut out: String = value.chars().map(|c| if c.is_control() { ' ' } else { c }).take(MAX_FIELD).collect();
    out.truncate(out.trim_end().len());
    out
}

/// The endpoint of the data folder `dir`.
pub fn endpoint(dir: &Path) -> String {
    #[cfg(unix)]
    {
        let path = dir.join("hooks.sock");
        // Socket paths are limited to about 100 bytes.
        if path.as_os_str().len() < 100 {
            return path.display().to_string();
        }
        std::env::temp_dir().join(format!("orbit-hooks-{}.sock", short_hash(dir))).display().to_string()
    }
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\orbit-hooks-{}", short_hash(dir))
    }
}

fn short_hash(dir: &Path) -> String {
    blake3::hash(dir.as_os_str().as_encoded_bytes()).to_hex()[..16].to_owned()
}

/// Starts the endpoint; every well-formed message goes to `deliver`.
/// Must be called inside a Tokio runtime. The listener lives until the
/// runtime ends.
pub fn serve(endpoint: &str, deliver: impl Fn(HookMessage) + Send + Sync + 'static) -> std::io::Result<()> {
    let deliver = Arc::new(deliver);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::remove_file(endpoint);
        let listener = tokio::net::UnixListener::bind(endpoint)?;
        let _ = std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600));
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let deliver = deliver.clone();
                        tokio::spawn(async move { receive(stream, &*deliver).await });
                    }
                    Err(err) => {
                        log::warn!("hook endpoint: {err}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
        Ok(())
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        let name = endpoint.to_owned();
        let mut server = ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(&name)?;
        tokio::spawn(async move {
            loop {
                if let Err(err) = server.connect().await {
                    log::debug!("hook endpoint: {err}");
                }
                let next = match ServerOptions::new().reject_remote_clients(true).create(&name) {
                    Ok(next) => next,
                    Err(err) => {
                        log::warn!("hook endpoint stopped: {err}");
                        return;
                    }
                };
                let conn = std::mem::replace(&mut server, next);
                let deliver = deliver.clone();
                tokio::spawn(async move { receive(conn, &*deliver).await });
            }
        });
        Ok(())
    }
}

async fn receive(mut stream: impl tokio::io::AsyncRead + Unpin, deliver: &(dyn Fn(HookMessage) + Send + Sync)) {
    use tokio::io::AsyncReadExt as _;
    let mut buf = Vec::with_capacity(512);
    let read = async {
        let mut chunk = [0u8; 1024];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.len() > MAX_MESSAGE {
                        buf.clear();
                        break;
                    }
                }
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(2), read).await.is_err() {
        return;
    }
    if let Some(msg) = HookMessage::decode(&buf) {
        deliver(msg);
    }
}

/// Sends one message to the endpoint (the invoker's side).
pub fn send(endpoint: &str, msg: &HookMessage) -> std::io::Result<()> {
    let bytes = msg.encode();
    #[cfg(unix)]
    {
        let mut stream = std::os::unix::net::UnixStream::connect(endpoint)?;
        stream.set_write_timeout(Some(SEND_TIMEOUT))?;
        stream.write_all(&bytes)
    }
    #[cfg(windows)]
    {
        const ERROR_PIPE_BUSY: i32 = 231;
        let deadline = std::time::Instant::now() + SEND_TIMEOUT;
        loop {
            match std::fs::OpenOptions::new().write(true).open(endpoint) {
                Ok(mut pipe) => return pipe.write_all(&bytes),
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) && std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => return Err(err),
            }
        }
    }
}

// ── The invoker ──────────────────────────────────────────────────────────

/// `orbit-hook`'s `main`: reports one hook call and exits 0, whatever
/// happens. Arguments: an optional agent name, and for Codex's `notify`
/// the JSON payload last; otherwise the payload is read from stdin.
pub fn invoker_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    invoke(&args, std::io::stdin().lock(), env(ENV_ENDPOINT), env(ENV_TOKEN))
}

/// The invoker with its inputs passed in (tests run it in-process).
pub fn invoke(args: &[String], stdin: impl Read, endpoint: Option<String>, token: Option<String>) -> i32 {
    let json_arg = args.iter().rev().find(|a| a.trim_start().starts_with('{'));
    let agent = args.first().filter(|a| !a.trim_start().starts_with('{')).cloned().unwrap_or_default();
    let payload = match json_arg {
        Some(json) => json.clone().into_bytes(),
        None => read_payload(stdin),
    };
    let call = parse_payload(&agent, &payload);
    let Some(token) = token else { return 0 };
    let msg = HookMessage { token, call };
    let sent = endpoint.as_deref().is_some_and(|e| send(e, &msg).is_ok());
    if !sent {
        write_osc_fallback(&msg.call);
    }
    0
}

/// Reads the whole payload (the agent must never block on a full pipe)
/// but keeps at most 1 MiB of it.
fn read_payload(mut stdin: impl Read) -> Vec<u8> {
    const KEEP: u64 = 1 << 20;
    let mut payload = Vec::new();
    let _ = stdin.by_ref().take(KEEP).read_to_end(&mut payload);
    let _ = std::io::copy(&mut stdin, &mut std::io::sink());
    payload
}

/// Keeps what status needs from a hook payload: Claude Code's stdin JSON
/// (`hook_event_name`, `tool_name`, `notification_type`, `message`) or
/// Codex's `notify` argument (`type`).
pub fn parse_payload(agent: &str, payload: &[u8]) -> HookCall {
    let field = |path: &str| -> String {
        sonic_rs::get_from_slice(payload, [path])
            .ok()
            .and_then(|v| sonic_rs::JsonValueTrait::as_str(&v).map(str::to_owned))
            .map(|s| one_line(&s))
            .unwrap_or_default()
    };
    let mut call = HookCall {
        agent: agent.to_owned(),
        event: field("hook_event_name"),
        tool: field("tool_name"),
        kind: field("notification_type"),
        message: field("message"),
    };
    if call.event.is_empty() {
        call.event = field("type");
    }
    if call.message.is_empty() {
        call.message = field("error");
    }
    if call.agent.is_empty() {
        call.agent = match call.event.as_str() {
            "agent-turn-complete" => "codex".into(),
            "" => String::new(),
            _ if field("transcript_path").contains(".claude") => "claude".into(),
            _ => String::new(),
        };
    }
    call
}

/// Reports through the terminal itself when the endpoint is unreachable.
fn write_osc_fallback(call: &HookCall) {
    #[cfg(unix)]
    {
        use super::term::{HookEffect, hook_effect};
        use orbit_proto::{AgentState, NeedKind};
        let (state, kind) = match hook_effect(call) {
            HookEffect::State { state, .. } => match state {
                AgentState::Working => ("working", ""),
                AgentState::NeedsYou(NeedKind::Permission) => ("blocked", "permission"),
                AgentState::NeedsYou(NeedKind::Auth) => ("blocked", "auth"),
                AgentState::NeedsYou(NeedKind::Question) => ("blocked", "question"),
                AgentState::Idle => ("idle", ""),
                AgentState::Failed => ("failed", ""),
                AgentState::RateLimited => ("ratelimited", ""),
                AgentState::Unverified => return,
            },
            HookEffect::Started => ("idle", ""),
            HookEffect::Ended => ("exited", ""),
            HookEffect::Alive => return,
        };
        let mut fields = vec![("state", state), ("agent", call.agent.as_str())];
        if !kind.is_empty() {
            fields.push(("kind", kind));
        }
        if !call.tool.is_empty() {
            fields.push(("msg", call.tool.as_str()));
        }
        if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
            let _ = tty.write_all(&super::osc::report_sequence(&fields));
        }
    }
    #[cfg(not(unix))]
    let _ = call;
}

// ── Injection into a terminal ────────────────────────────────────────────

static INVOKER: Mutex<Option<Option<PathBuf>>> = Mutex::new(None);

/// Overrides where the invoker is (tests; tools that bundle it elsewhere).
/// `None` turns hook injection off.
pub fn set_invoker(path: Option<PathBuf>) {
    *INVOKER.lock().unwrap() = Some(path);
}

/// The invoker: `orbit-hook` next to the running executable, unless
/// overridden. `None` when it is not there.
pub fn invoker() -> Option<PathBuf> {
    let mut slot = INVOKER.lock().unwrap();
    slot.get_or_insert_with(|| {
        let exe = std::env::current_exe().ok()?;
        let path = exe.with_file_name(if cfg!(windows) { "orbit-hook.exe" } else { "orbit-hook" });
        path.is_file().then_some(path)
    })
    .clone()
}

/// How a terminal's shell is started so its agents report to Orbit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Injection {
    pub env: Vec<(String, String)>,
    /// Arguments added after the shell's own.
    pub args: Vec<String>,
}

/// The environment (and for PowerShell, arguments) for a terminal with
/// `token`, whose shell is `shell`. Writes Orbit's own files under
/// `dir/hooks` once. Without an invoker, only the endpoint and token are
/// set: OSC reports and heuristics still work.
pub fn injection(dir: &Path, endpoint: &str, token: &str, shell: &Path) -> Injection {
    let mut out = Injection::default();
    out.env.push((ENV_ENDPOINT.into(), endpoint.into()));
    out.env.push((ENV_TOKEN.into(), token.into()));
    let Some(invoker) = invoker() else { return out };
    let hooks = dir.join("hooks");
    if let Err(err) = std::fs::create_dir_all(&hooks) {
        log::warn!("hooks folder {}: {err}", hooks.display());
        return out;
    }
    out.env.push((ENV_INVOKER.into(), invoker.display().to_string()));
    let settings = hooks.join("claude-settings.json");
    if write_if_changed(&settings, claude_settings(&invoker).as_bytes()) {
        out.env.push((ENV_CLAUDE_SETTINGS.into(), settings.display().to_string()));
    }
    if !codex_has_notify()
        && let Some(notify) = codex_notify(&invoker)
    {
        out.env.push((ENV_CODEX_NOTIFY.into(), notify));
    }
    #[cfg(unix)]
    {
        let _ = shell;
        let bin = hooks.join("bin");
        if std::fs::create_dir_all(&bin).is_ok()
            && write_script(&bin.join("claude"), &unix_shim("claude", CLAUDE_SUBCOMMANDS, r#"--settings "$ORBIT_CLAUDE_SETTINGS""#, ENV_CLAUDE_SETTINGS))
            && write_script(&bin.join("codex"), &unix_shim("codex", &[], r#"-c "notify=$ORBIT_CODEX_NOTIFY""#, ENV_CODEX_NOTIFY))
        {
            let dir = bin.display().to_string();
            let path = std::env::var("PATH").unwrap_or_default();
            out.env.push((ENV_SHIM_DIR.into(), dir.clone()));
            out.env.push(("PATH".into(), if path.is_empty() { dir } else { format!("{dir}:{path}") }));
        }
    }
    #[cfg(windows)]
    {
        let stem = shell.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        if matches!(stem.as_str(), "pwsh" | "powershell") {
            // An inline command, not a script file: it runs even where the
            // execution policy blocks scripts.
            out.args.extend(["-NoExit".into(), "-EncodedCommand".into(), encode_powershell(&powershell_functions())]);
        }
    }
    out
}

/// Claude Code subcommands that take no session flags.
const CLAUDE_SUBCOMMANDS: &[&str] = &[
    "mcp", "config", "update", "upgrade", "doctor", "install", "migrate-installer", "setup-token", "plugin", "plugins",
    "auth", "logs", "stop", "kill", "rm", "daemon", "attach", "respawn", "gateway", "auto-mode", "--version", "-v",
    "--help", "-h",
];

/// Hook events Orbit listens to. Only long-standing ones: an unknown event
/// name could make an older Claude Code reject the whole file.
const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Notification", "Stop", "PreCompact",
];

fn claude_settings(invoker: &Path) -> String {
    let command = format!("{} claude", shell_path(invoker));
    let entry = format!(
        r#"[{{"hooks":[{{"type":"command","command":{},"timeout":5}}]}}]"#,
        sonic_rs::to_string(&command).unwrap_or_default()
    );
    let events: Vec<String> = CLAUDE_EVENTS.iter().map(|e| format!(r#""{e}":{entry}"#)).collect();
    format!("{{\"hooks\":{{{}}}}}\n", events.join(","))
}

/// The invoker's path as a hook command: single-quoted for `sh`; on
/// Windows with forward slashes, and without spaces when a short name
/// exists, so both Git Bash and PowerShell run it.
fn shell_path(path: &Path) -> String {
    #[cfg(unix)]
    {
        format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
    }
    #[cfg(windows)]
    {
        let long = path.display().to_string();
        let path = if long.contains(' ') { short_path(path).unwrap_or(long) } else { long };
        let path = path.replace('\\', "/");
        if path.contains(' ') { format!("\"{path}\"") } else { path }
    }
}

#[cfg(windows)]
fn short_path(path: &Path) -> Option<String> {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetShortPathNameW(long: *const u16, short: *mut u16, len: u32) -> u32;
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut buf = vec![0u16; 1024];
    // SAFETY: both buffers are valid for the lengths given; the input is
    // NUL-terminated.
    let len = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (len > 0 && len < buf.len()).then(|| std::ffi::OsString::from_wide(&buf[..len]).to_string_lossy().into_owned())
}

/// The TOML value for Codex's `notify`, or `None` when the path cannot be
/// written as a TOML literal string.
fn codex_notify(invoker: &Path) -> Option<String> {
    let path = invoker.display().to_string().replace('\\', "/");
    (!path.contains('\'')).then(|| format!("['{path}','codex']"))
}

/// Whether the user configured Codex's `notify` themselves; Orbit never
/// replaces it.
fn codex_has_notify() -> bool {
    let home = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".codex")));
    let Some(config) = home.map(|h| h.join("config.toml")) else { return false };
    std::fs::read_to_string(config).is_ok_and(|text| has_top_level_notify(&text))
}

fn has_top_level_notify(toml: &str) -> bool {
    // Top-level keys come before the first table header.
    toml.lines()
        .map(str::trim)
        .take_while(|l| !l.starts_with('['))
        .any(|l| l.strip_prefix("notify").is_some_and(|rest| rest.trim_start().starts_with('=')))
}

#[cfg(unix)]
fn unix_shim(name: &str, subcommands: &[&str], flags: &str, needs: &str) -> String {
    let passthrough = if subcommands.is_empty() {
        String::new()
    } else {
        format!("case \"$1\" in {}) exec \"$real\" \"$@\";; esac\n", subcommands.join("|"))
    };
    format!(
        r#"#!/bin/sh
# Orbit terminal integration, for Orbit's terminals only: lets {name} report
# its status to Orbit. Your own {name} configuration is not changed.
real=
old_ifs=$IFS
IFS=:
set -f
for d in $PATH; do
  [ -z "$d" ] && continue
  [ "$d" = "$ORBIT_SHIM_DIR" ] && continue
  if [ -f "$d/{name}" ] && [ -x "$d/{name}" ]; then real="$d/{name}"; break; fi
done
set +f
IFS=$old_ifs
if [ -z "$real" ]; then echo "{name}: command not found" >&2; exit 127; fi
{passthrough}[ -n "${needs}" ] || exec "$real" "$@"
exec "$real" {flags} "$@"
"#
    )
}

#[cfg(windows)]
fn powershell_functions() -> String {
    let subs = CLAUDE_SUBCOMMANDS.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",");
    format!(
        r#"function global:claude {{
  $real = Get-Command -Name claude -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source
  if (-not $real) {{ Write-Error 'claude: command not found'; return }}
  if (($args.Count -gt 0 -and @({subs}) -contains $args[0]) -or -not $env:{ENV_CLAUDE_SETTINGS}) {{ & $real @args; return }}
  & $real --settings $env:{ENV_CLAUDE_SETTINGS} @args
}}
function global:codex {{
  $real = Get-Command -Name codex -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty Source
  if (-not $real) {{ Write-Error 'codex: command not found'; return }}
  if (-not $env:{ENV_CODEX_NOTIFY}) {{ & $real @args; return }}
  & $real -c "notify=$env:{ENV_CODEX_NOTIFY}" @args
}}"#
    )
}

#[cfg(windows)]
fn encode_powershell(script: &str) -> String {
    use base64::Engine as _;
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

/// Writes `bytes` unless the file already holds them. False on failure.
fn write_if_changed(path: &Path, bytes: &[u8]) -> bool {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return true;
    }
    let tmp = path.with_extension("tmp");
    match std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => true,
        Err(err) => {
            log::warn!("writing {}: {err}", path.display());
            false
        }
    }
}

#[cfg(unix)]
fn write_script(path: &Path, text: &str) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    write_if_changed(path, text.as_bytes())
        && std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_and_reject_garbage() {
        let msg = HookMessage {
            token: "abc".into(),
            call: HookCall {
                agent: "claude".into(),
                event: "Notification".into(),
                tool: String::new(),
                kind: "permission_prompt".into(),
                message: "Claude needs\nyour permission".into(),
            },
        };
        let back = HookMessage::decode(&msg.encode()).unwrap();
        assert_eq!(back.call.message, "Claude needs your permission");
        assert_eq!((back.token.as_str(), back.call.kind.as_str()), ("abc", "permission_prompt"));
        assert_eq!(HookMessage::decode(b"GET / HTTP/1.1\r\n"), None);
        assert_eq!(HookMessage::decode(b"orbit-hook 1\nagent claude\n"), None, "no token");
        assert_eq!(HookMessage::decode(&[0xff, 0xfe]), None);
    }

    #[test]
    fn payloads_keep_only_what_status_needs() {
        let big = "x".repeat(200_000);
        let claude = format!(
            r#"{{"session_id":"s","transcript_path":"/home/u/.claude/projects/p/s.jsonl","hook_event_name":"PreToolUse","tool_name":"Write","tool_input":{{"content":"{big}"}}}}"#
        );
        let call = parse_payload("", claude.as_bytes());
        assert_eq!((call.agent.as_str(), call.event.as_str(), call.tool.as_str()), ("claude", "PreToolUse", "Write"));
        let codex = r#"{"type":"agent-turn-complete","thread-id":"t","last-assistant-message":"Done"}"#;
        let call = parse_payload("codex", codex.as_bytes());
        assert_eq!((call.agent.as_str(), call.event.as_str(), call.message.as_str()), ("codex", "agent-turn-complete", ""));
        let call = parse_payload("", b"not json");
        assert_eq!(call, HookCall::default());
    }

    #[test]
    fn claude_settings_are_valid_json_with_every_event() {
        let json = claude_settings(Path::new("/opt/Orbit Tools/orbit-hook"));
        use sonic_rs::JsonValueTrait as _;
        let value: sonic_rs::Value = sonic_rs::from_str(&json).unwrap();
        for event in CLAUDE_EVENTS {
            let command = value.pointer(&sonic_rs::pointer!["hooks", *event, 0, "hooks", 0, "command"]);
            let command = command.and_then(|c| c.as_str()).unwrap_or_default();
            assert!(command.ends_with(" claude") && command.contains("orbit-hook"), "{event} in {json}");
        }
    }

    #[test]
    fn a_user_notify_setting_is_detected() {
        assert!(!has_top_level_notify("model = \"gpt-5\"\n[tui]\nnotify = true\n"), "a table's key is not top-level");
        assert!(has_top_level_notify("notify = [\"say\"]\n"));
        assert!(!has_top_level_notify("notifications = true\n"));
    }
}
