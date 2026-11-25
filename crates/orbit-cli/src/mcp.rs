//! `orbit mcp`: an MCP server (JSON-RPC 2.0 over newline-delimited stdio)
//! whose tools are generated from the command specs, and `orbit mcp
//! install`, which registers it with Claude Code and Codex.
//!
//! Only the server side of the tools capability is needed, so the protocol
//! is implemented here directly: `initialize`, `ping`, `tools/list`,
//! `tools/call` and `notifications/cancelled`. Tool calls run concurrently
//! (a `wait` must not block a `list_threads`), each in its own task.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::args;
use crate::error::CliError;
use crate::exec::{self, Ctx, Origin, Outcome};
use crate::spec::{self, SPECS, exit};

/// Protocol revisions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// The `tools/list` result.
pub fn tools() -> Value {
    let tools: Vec<Value> = SPECS
        .iter()
        .filter_map(|spec| {
            let tool = spec.tool.as_ref()?;
            let description =
                if spec.details.is_empty() { format!("{}.", spec.about) } else { format!("{}. {}", spec.about, spec.details) };
            Some(json!({
                "name": tool.name,
                "title": tool.title,
                "description": description,
                "inputSchema": args::input_schema(spec),
                "annotations": {
                    "title": tool.title,
                    "readOnlyHint": tool.read_only,
                    "destructiveHint": tool.destructive,
                    "idempotentHint": tool.idempotent,
                    "openWorldHint": false,
                },
            }))
        })
        .collect();
    json!({ "tools": tools })
}

fn response(id: &Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

fn error(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
    let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "orbit", "title": "Orbit", "version": env!("CARGO_PKG_VERSION")},
        "instructions": crate::help::agent_context(),
    })
}

/// A tool call's result: the command's JSON as structured content and as
/// text (for clients that read only text).
async fn call_tool(ctx: &Ctx, params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).ok_or((INVALID_PARAMS, "Missing tool name.".to_owned()))?;
    let spec = spec::tool(name).ok_or_else(|| (INVALID_PARAMS, format!("Unknown tool “{name}”.")))?;
    let outcome = match args::from_tool(spec, params.get("arguments")) {
        Ok(args) => exec::execute(ctx, spec, &args, &mut std::io::sink()).await,
        Err(err) => Err(err),
    };
    Ok(match outcome {
        Ok(Outcome { value, exit, .. }) => {
            let mut value = if value.is_object() { value } else { json!({ "result": value }) };
            if exit != exit::OK {
                value["exit"] = json!(exit);
            }
            json!({
                "content": [{"type": "text", "text": value.to_string()}],
                "structuredContent": value,
                "isError": false,
            })
        }
        Err(err) => {
            let text = match &err.hint {
                Some(hint) => format!("{}\n{hint}", err.message),
                None => err.message.clone(),
            };
            json!({
                "content": [{"type": "text", "text": text}],
                "structuredContent": err.to_json(),
                "isError": true,
            })
        }
    })
}

/// Serves MCP on `input`/`output` until `input` closes. Runs tool calls on
/// a `LocalSet`, so call this inside one.
pub async fn serve<R, W>(ctx: Ctx, input: R, mut output: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<std::io::Result<String>>();
    tokio::task::spawn_local(async move {
        let mut lines = BufReader::new(input).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => drop(line_tx.send(Ok(line))),
                Ok(None) => break,
                Err(e) => {
                    let _ = line_tx.send(Err(e));
                    break;
                }
            }
        }
    });
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = tokio::task::spawn_local(async move {
        while let Some(line) = rx.recv().await {
            if output.write_all(line.as_bytes()).await.is_err()
                || output.write_all(b"
").await.is_err()
                || output.flush().await.is_err()
            {
                break;
            }
        }
    });
    let result = serve_lines(ctx, line_rx, tx).await;
    let _ = writer.await;
    result
}

/// The protocol loop: requests in as lines, responses out as lines.
async fn serve_lines(
    ctx: Ctx,
    mut input: tokio::sync::mpsc::UnboundedReceiver<std::io::Result<String>>,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
) -> std::io::Result<()> {
    let ctx = Rc::new(ctx);
    let running: Rc<RefCell<HashMap<String, tokio::task::AbortHandle>>> = Rc::default();
    while let Some(line) = input.recv().await {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(e) => {
                let _ = tx.send(error(&Value::Null, PARSE_ERROR, &format!("Parse error: {e}")));
                continue;
            }
        };
        let Value::Object(message) = message else {
            let _ = tx.send(error(&Value::Null, INVALID_REQUEST, "Batches are not supported; send one message per line."));
            continue;
        };
        let Some(method) = message.get("method").and_then(Value::as_str).map(str::to_owned) else {
            // A response to a request we never send.
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = message.get("id").cloned() else {
            if method == "notifications/cancelled"
                && let Some(request) = params.get("requestId")
                && let Some(handle) = running.borrow_mut().remove(&request.to_string())
            {
                handle.abort();
            }
            continue;
        };
        match method.as_str() {
            "initialize" => drop(tx.send(response(&id, initialize(&params)))),
            "ping" => drop(tx.send(response(&id, json!({})))),
            "tools/list" => drop(tx.send(response(&id, tools()))),
            "tools/call" => {
                let (ctx, tx2, running2) = (ctx.clone(), tx.clone(), running.clone());
                let key = id.to_string();
                let task = tokio::task::spawn_local(async move {
                    let reply = match call_tool(&ctx, &params).await {
                        Ok(result) => response(&id, result),
                        Err((code, message)) => error(&id, code, &message),
                    };
                    running2.borrow_mut().remove(&id.to_string());
                    let _ = tx2.send(reply);
                });
                running.borrow_mut().insert(key, task.abort_handle());
            }
            "resources/list" => drop(tx.send(response(&id, json!({"resources": []})))),
            "prompts/list" => drop(tx.send(response(&id, json!({"prompts": []})))),
            _ => drop(tx.send(error(&id, METHOD_NOT_FOUND, &format!("Method not found: {method}")))),
        }
    }
    // Input closed: the client is gone, so nobody reads answers any more.
    let pending: Vec<tokio::task::AbortHandle> = running.borrow().values().cloned().collect();
    for handle in pending {
        handle.abort();
    }
    Ok(())
}

/// Runs the server on standard input and output.
pub fn run_stdio(env: crate::conn::Env, global: Map<String, Value>) -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: {e}");
            return exit::ERROR;
        }
    };
    let ctx = Ctx::new(env, global, Origin::Tool);
    // Plain threads for stdio: a blocked console read must never hold up
    // the runtime, and every response is flushed as one line.
    let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<std::io::Result<String>>();
    let reader = std::thread::Builder::new().name("mcp-stdin".into()).spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let failed = line.is_err();
            if line_tx.send(line).is_err() || failed {
                break;
            }
        }
    });
    if let Err(e) = reader {
        eprintln!("error: {e}");
        return exit::ERROR;
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let writer = std::thread::Builder::new().name("mcp-stdout".into()).spawn(move || {
        use std::io::Write;
        let stdout = std::io::stdout();
        while let Some(line) = rx.blocking_recv() {
            let mut out = stdout.lock();
            if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    });
    let writer = match writer {
        Ok(writer) => writer,
        Err(e) => {
            eprintln!("error: {e}");
            return exit::ERROR;
        }
    };
    let local = tokio::task::LocalSet::new();
    let result = local.block_on(&runtime, serve_lines(ctx, line_rx, tx));
    // Dropping the set drops cancelled calls and their senders, which
    // lets the writer finish.
    drop(local);
    let _ = writer.join();
    match result {
        Ok(()) => exit::OK,
        Err(e) => {
            eprintln!("error: {e}");
            exit::ERROR
        }
    }
}

// ── Registering the server with agents ──────────────────────────────────

fn exe() -> Result<PathBuf, CliError> {
    let exe = std::env::current_exe().map_err(|e| CliError::engine(format!("Couldn’t find this program: {e}")))?;
    let text = exe.to_string_lossy();
    Ok(PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text)))
}

/// The `claude mcp add` arguments.
pub fn claude_args(exe: &Path) -> Vec<String> {
    vec!["mcp".into(), "add".into(), "--scope".into(), "user".into(), "orbit".into(), "--".into(), exe.display().to_string(), "mcp".into()]
}

fn shell_quote(word: &str) -> String {
    if word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:\\".contains(c)) {
        word.to_owned()
    } else {
        format!("\"{}\"", word.replace('"', "\\\""))
    }
}

/// Where Codex keeps its configuration.
pub fn codex_config_path() -> Option<PathBuf> {
    match std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        Some(home) => Some(PathBuf::from(home).join("config.toml")),
        None => dirs::home_dir().map(|h| h.join(".codex").join("config.toml")),
    }
}

fn toml_string(text: &str) -> String {
    if !text.contains('\'') && !text.contains('\n') {
        format!("'{text}'")
    } else {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// The `[mcp_servers.orbit]` table for Codex.
pub fn codex_table(exe: &Path) -> String {
    format!("[mcp_servers.orbit]\ncommand = {}\nargs = [\"mcp\"]\n", toml_string(&exe.display().to_string()))
}

/// `config` with its `[mcp_servers.orbit]` table replaced by (or appended
/// as) `table`; everything else is kept byte for byte.
pub fn upsert_codex_table(config: &str, table: &str) -> String {
    let mut out = String::new();
    let mut lines = config.split_inclusive('\n').peekable();
    let mut replaced = false;
    while let Some(line) = lines.next() {
        if line.trim() == "[mcp_servers.orbit]" {
            // Skip the old table up to the next table header.
            while let Some(next) = lines.peek() {
                if next.trim_start().starts_with('[') {
                    break;
                }
                lines.next();
            }
            out.push_str(table);
            if lines.peek().is_some() {
                out.push('\n');
            }
            replaced = true;
            continue;
        }
        out.push_str(line);
    }
    if !replaced {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
        }
        out.push_str(table);
    }
    out
}

fn apply_codex(path: &Path, table: &str) -> Result<(), CliError> {
    let current = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(CliError::engine(format!("Couldn’t read {}: {e}", path.display()))),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| CliError::engine(format!("Couldn’t create {}: {e}", dir.display())))?;
    }
    std::fs::write(path, upsert_codex_table(&current, table))
        .map_err(|e| CliError::engine(format!("Couldn’t write {}: {e}", path.display())))
}

fn apply_claude(args: &[String]) -> Result<String, CliError> {
    let candidates: &[&str] = if cfg!(windows) { &["claude.exe", "claude.cmd"] } else { &["claude"] };
    let mut last = String::from("not found on PATH");
    for program in candidates {
        // Replace an existing registration so the path stays current.
        let _ = std::process::Command::new(program)
            .args(["mcp", "remove", "--scope", "user", "orbit"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match std::process::Command::new(program).args(args).output() {
            Ok(out) if out.status.success() => return Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned()),
            Ok(out) => return Err(CliError::engine(format!("claude mcp add failed: {}", String::from_utf8_lossy(&out.stderr).trim()))),
            Err(e) => last = e.to_string(),
        }
    }
    Err(CliError::engine(format!("Couldn’t run Claude Code’s `claude` ({last}).")))
}

pub fn install(client: &str, apply: bool, _json: bool) -> Result<Outcome, CliError> {
    let exe = exe()?;
    let mut value = json!({"applied": apply});
    let mut human = String::new();
    if client == "claude" || client == "all" {
        let args = claude_args(&exe);
        let line = format!("claude {}", args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "));
        value["claude"] = json!({"command": line});
        if apply {
            let said = apply_claude(&args)?;
            human.push_str(&format!("Claude Code: registered orbit. {said}\n"));
        } else {
            human.push_str(&format!("Claude Code — run:\n  {line}\n\n"));
        }
    }
    if client == "codex" || client == "all" {
        let path = codex_config_path().ok_or_else(|| CliError::engine("Couldn’t find your home folder."))?;
        let table = codex_table(&exe);
        value["codex"] = json!({"config": path.display().to_string(), "table": table});
        if apply {
            apply_codex(&path, &table)?;
            human.push_str(&format!("Codex: wrote [mcp_servers.orbit] to {}\n", path.display()));
        } else {
            human.push_str(&format!("Codex — add to {}:\n\n{table}\n", path.display()));
        }
    }
    if !apply {
        human.push_str("Or run `orbit mcp install --apply` to do it for you.");
    }
    Ok(Outcome { value, exit: exit::OK, human: Some(human.trim_end().to_owned()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_table_is_inserted_or_replaced_without_touching_the_rest() {
        let table = codex_table(Path::new(r"C:\Program Files\Orbit\orbit.exe"));
        assert!(table.contains(r"command = 'C:\Program Files\Orbit\orbit.exe'"));
        assert_eq!(upsert_codex_table("", &table), table);
        let config = "model = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let added = upsert_codex_table(config, &table);
        assert!(added.starts_with(config));
        assert!(added.ends_with(&table));
        let old = "model = \"o3\"\n[mcp_servers.orbit]\ncommand = \"old\"\nargs = []\n\n[profiles.x]\nmodel = \"y\"\n";
        let replaced = upsert_codex_table(old, &table);
        assert!(!replaced.contains("old"));
        assert!(replaced.contains("[profiles.x]\nmodel = \"y\"\n"));
        assert_eq!(replaced.matches("[mcp_servers.orbit]").count(), 1);
        assert_eq!(upsert_codex_table(&replaced, &table), replaced, "idempotent");
    }

    #[test]
    fn codex_apply_writes_only_the_given_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        apply_codex(&path, &codex_table(Path::new("/usr/bin/orbit"))).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[mcp_servers.orbit]\ncommand = '/usr/bin/orbit'\nargs = [\"mcp\"]\n");
    }

    #[test]
    fn install_prints_by_default() {
        let out = install("all", false, false).unwrap();
        let text = out.human.unwrap();
        assert!(text.contains("claude mcp add --scope user orbit --"));
        assert!(text.contains("[mcp_servers.orbit]"));
        assert_eq!(out.value["applied"], false);
    }

    #[test]
    fn tool_list_has_schemas_and_hints() {
        let list = tools();
        let tools = list["tools"].as_array().unwrap();
        let start = tools.iter().find(|t| t["name"] == "start_thread").unwrap();
        assert_eq!(start["inputSchema"]["type"], "object");
        assert_eq!(start["annotations"]["readOnlyHint"], false);
        let wait = tools.iter().find(|t| t["name"] == "wait").unwrap();
        assert_eq!(wait["annotations"]["readOnlyHint"], true);
        assert!(tools.iter().all(|t| t["description"].as_str().unwrap().len() > 10));
    }
}
