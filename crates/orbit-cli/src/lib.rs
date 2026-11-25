//! The `orbit` command line and MCP server.
//!
//! Commands are described once in [`spec`]; the parser, `--help`, shell
//! completions, `orbit help --json` and the MCP tools are generated from
//! those specs, and [`exec`] runs them against a live engine reached through
//! its local control endpoint (see `orbit_net::endpoint`).

pub mod args;
pub mod conn;
pub mod error;
pub mod exec;
pub mod help;
pub mod json;
pub mod mcp;
pub mod render;
pub mod select;
pub mod spec;

use std::io::Write;

use serde_json::{Value, json};

pub use conn::Env;
use spec::exit;

/// Runs one command line (without the program name) and returns the exit
/// status. `orbit mcp` serves on the process's real standard input/output.
pub fn run(argv: &[String], env: Env, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parsed = match args::parse(argv) {
        Ok(parsed) => parsed,
        Err(e) => {
            let code = e.exit_code();
            if code == 0 {
                let _ = write!(out, "{}", e.render());
            } else if argv.iter().any(|a| a == "--json") {
                let message = e.kind().as_str().map(str::to_owned).unwrap_or_else(|| e.to_string());
                let error = error::CliError::usage(message).hint(e.to_string().trim().to_owned());
                let _ = writeln!(out, "{}", error.to_json());
            } else {
                let _ = write!(err, "{}", e.render());
            }
            return if code == 0 { exit::OK } else { exit::USAGE };
        }
    };
    if parsed.spec.path == ["mcp"] {
        return mcp::run_stdio(env, parsed.global);
    }
    let json_out = parsed.global.get("json").and_then(Value::as_bool).unwrap_or(false);
    let streamed = parsed.spec.path == ["tail"]
        || (parsed.spec.path == ["log"] && parsed.args.get("follow").and_then(Value::as_bool) == Some(true));
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return exit::ERROR;
        }
    };
    let ctx = exec::Ctx::new(env, parsed.global, exec::Origin::Cli);
    let result = runtime.block_on(exec::execute(&ctx, parsed.spec, &parsed.args, out));
    let code = match result {
        Ok(outcome) => {
            if json_out {
                if !streamed {
                    let result = match (&outcome.value, &outcome.human) {
                        (Value::Null, Some(text)) => Value::String(text.clone()),
                        (value, _) => value.clone(),
                    };
                    let _ = writeln!(out, "{}", json!({"ok": true, "result": result}));
                }
            } else if let Some(text) = outcome.human.as_deref().filter(|t| !t.is_empty()) {
                let _ = writeln!(out, "{text}");
            }
            outcome.exit
        }
        Err(e) => {
            if json_out {
                let _ = writeln!(out, "{}", e.to_json());
            } else {
                let _ = writeln!(err, "{}", e.human());
            }
            e.exit
        }
    };
    let _ = out.flush();
    // Drop the connection before the runtime that runs its pump.
    drop(ctx);
    drop(runtime);
    code
}
