//! `orbit plugin …`: start, check and pack a plugin folder here; install,
//! list, turn on and off, remove and audit the host's plugins
//! ([`PluginCommand`]).

use std::path::PathBuf;

use orbit_net::state::ClientState;
use orbit_plugin::manifest::{MANIFEST, is_plugin_id};
use orbit_proto::*;
use serde_json::{Map, Value, json};

use crate::conn::Conn;
use crate::error::CliError;
use crate::exec::{Ctx, Outcome};
use crate::spec::exit;

fn s<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn flag(args: &Map<String, Value>, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn folder(ctx: &Ctx, args: &Map<String, Value>) -> PathBuf {
    let path = ctx.env.cwd.join(s(args, "path").unwrap_or("."));
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    PathBuf::from(path.to_string_lossy().trim_start_matches(r"\\?\"))
}

async fn call(conn: &Conn, command: PluginCommand) -> Result<PluginReply, CliError> {
    match conn.call(Command::Plugin(Box::new(command))).await? {
        Reply::Plugin(reply) => Ok(*reply),
        _ => Err(CliError::engine("Unexpected reply.")),
    }
}

fn caps(list: &[Capability]) -> Vec<&'static str> {
    list.iter().map(|c| c.key()).collect()
}

pub fn info_json(p: &PluginInfo) -> Value {
    json!({
        "id": p.id,
        "name": p.name,
        "version": p.version,
        "status": p.status.label().to_lowercase(),
        "detail": match &p.status {
            PluginStatus::Killed { reason } => reason.clone(),
            PluginStatus::Broken { error } => error.clone(),
            PluginStatus::Incompatible { requires } => format!("needs Orbit {requires}"),
            _ => String::new(),
        },
        "tier": p.tier.label().to_lowercase(),
        "source": p.source,
        "dev": p.dev,
        "capabilities": caps(&p.capabilities),
        "granted": caps(&p.granted),
        "contributes": p.summary,
        "hash": p.hash,
    })
}

// ── Local commands ───────────────────────────────────────────────────────

/// `orbit plugin new|validate|pack`: no engine needed.
pub fn local(ctx: &Ctx, path: &[&str], args: &Map<String, Value>) -> Result<Outcome, CliError> {
    match path {
        ["plugin", "new"] => {
            let id = s(args, "id").unwrap_or_default();
            if !is_plugin_id(id) {
                return Err(CliError::usage(format!(
                    "“{id}” isn’t a plugin id: 3 to 64 lowercase letters, digits and single hyphens, starting with a letter."
                )));
            }
            let name = s(args, "name").unwrap_or(id);
            let dir = ctx.env.cwd.join(s(args, "dir").unwrap_or(".")).join(id);
            if dir.exists() {
                return Err(CliError::usage(format!("{} already exists.", dir.display())));
            }
            std::fs::create_dir_all(&dir).map_err(|e| CliError::engine(format!("Couldn’t create {}: {e}", dir.display())))?;
            std::fs::write(dir.join(MANIFEST), orbit_plugin::manifest::scaffold(id, name))
                .map_err(|e| CliError::engine(format!("Couldn’t write the manifest: {e}")))?;
            Ok(Outcome::ok(
                json!({"path": dir.display().to_string()}),
                format!(
                    "Created {}.\nEdit its {MANIFEST}, then `orbit plugin validate {}` and `orbit plugin dev {}`.",
                    dir.display(),
                    dir.display(),
                    dir.display()
                ),
            ))
        }
        ["plugin", "validate"] => {
            let dir = folder(ctx, args);
            match orbit_plugin::package::load(&dir) {
                Ok(loaded) => {
                    let v = &loaded.validated;
                    let m = &v.manifest;
                    let mut human = format!("{} {} ({}) is valid.\n", m.name, m.version, m.id);
                    human.push_str(&format!("Tier: {}\nContributes: {}\n", v.tier.label(), loaded.summary()));
                    if !v.capabilities.is_empty() {
                        human.push_str("Asks to:\n");
                        for c in &v.capabilities {
                            human.push_str(&format!("  {:<15} {}\n", c.key(), c.consent()));
                        }
                    }
                    for agent in &m.agents {
                        human.push_str(&format!("Runs: {}  ({}, {})\n", agent.command_line(), agent.name, agent.protocol));
                    }
                    if let Some(req) = &v.incompatible {
                        human.push_str(&format!("Note: it needs Orbit {req}; this is {}.\n", orbit_plugin::ORBIT_VERSION));
                    }
                    for w in &loaded.warnings {
                        human.push_str(&format!("warning: {w}\n"));
                    }
                    human.push_str(&format!("Hash: {}", loaded.hash));
                    Ok(Outcome::ok(
                        json!({
                            "valid": true,
                            "id": m.id,
                            "version": m.version,
                            "tier": v.tier.label().to_lowercase(),
                            "capabilities": caps(&v.capabilities),
                            "contributes": loaded.summary(),
                            "warnings": loaded.warnings,
                            "hash": loaded.hash,
                        }),
                        human,
                    ))
                }
                Err(problems) => {
                    let mut err = CliError::new(exit::ERROR, format!("{} isn’t a valid plugin:\n  {}", dir.display(), problems.join("\n  ")));
                    if !dir.join(MANIFEST).is_file() {
                        err = err.hint("Start one with `orbit plugin new <id>`.");
                    }
                    Err(err)
                }
            }
        }
        ["plugin", "pack"] => {
            let dir = folder(ctx, args);
            let loaded = orbit_plugin::package::load(&dir)
                .map_err(|p| CliError::new(exit::ERROR, format!("{} isn’t a valid plugin:\n  {}", dir.display(), p.join("\n  "))))?;
            let bytes = orbit_plugin::package::pack(&dir).map_err(CliError::engine)?;
            let m = &loaded.validated.manifest;
            let out = match s(args, "out") {
                Some(out) => ctx.env.cwd.join(out),
                None => ctx.env.cwd.join(format!("{}-{}.{}", m.id, m.version, orbit_plugin::package::EXTENSION)),
            };
            std::fs::write(&out, &bytes).map_err(|e| CliError::engine(format!("Couldn’t write {}: {e}", out.display())))?;
            Ok(Outcome::ok(
                json!({"path": out.display().to_string(), "bytes": bytes.len(), "hash": loaded.hash}),
                format!("Wrote {} ({} KiB).\nHash: {}", out.display(), bytes.len().div_ceil(1024), loaded.hash),
            ))
        }
        _ => Err(CliError::usage("Unknown plugin command.")),
    }
}

// ── Host commands ────────────────────────────────────────────────────────

/// The source an `install` argument names.
fn source(ctx: &Ctx, args: &Map<String, Value>) -> Result<InstallSource, CliError> {
    let raw = s(args, "source").ok_or_else(|| CliError::usage("Name what to install."))?;
    if let Some(catalog) = s(args, "catalog") {
        return Ok(InstallSource::Catalog { catalog: catalog.to_owned(), id: raw.to_owned() });
    }
    if let Some(commit) = s(args, "commit") {
        return Ok(InstallSource::Git { url: raw.to_owned(), commit: commit.to_owned(), hash: s(args, "hash").map(str::to_owned) });
    }
    if raw.starts_with("https://") || raw.starts_with("git@") || raw.starts_with("ssh://") {
        return Err(CliError::usage("Pin a commit with --commit: a branch or tag can change under you."));
    }
    let path = ctx.env.cwd.join(raw);
    if path.is_dir() {
        // Packed here and sent, so a remote host gets the same files.
        let bytes = orbit_plugin::package::pack(&path).map_err(CliError::usage)?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        return Ok(InstallSource::Archive { name, bytes });
    }
    if path.is_file() {
        let bytes = std::fs::read(&path).map_err(|e| CliError::usage(format!("Couldn’t read {}: {e}", path.display())))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        return Ok(InstallSource::Archive { name, bytes });
    }
    Err(CliError::not_found(format!("“{raw}” is neither a folder nor a file.")).hint("Install from a catalog with --catalog <name>."))
}

fn consent_text(p: &PluginInfo) -> String {
    let mut text = String::new();
    for c in &p.capabilities {
        text.push_str(&format!("  {:<15} {}\n", c.key(), c.consent()));
    }
    for a in &p.ui.agents {
        text.push_str(&format!("  runs: {}\n", std::iter::once(&a.command).chain(&a.args).cloned().collect::<Vec<_>>().join(" ")));
    }
    text
}

fn plugin_of<'a>(state: &'a ClientState, id: &str) -> Result<&'a PluginInfo, CliError> {
    state.plugins.plugin(id).ok_or_else(|| {
        CliError::not_found(format!("No plugin “{id}” is installed.")).hint("See them with `orbit plugin list`.")
    })
}

pub async fn run(ctx: &Ctx, conn: &Conn, path: &[&str], args: &Map<String, Value>) -> Result<Outcome, CliError> {
    match path {
        ["plugin", "install"] | ["plugin", "dev"] => {
            let source = if path[1] == "dev" {
                let dir = folder(ctx, args);
                if !dir.join(MANIFEST).is_file() {
                    return Err(CliError::usage(format!("{} has no {MANIFEST}.", dir.display())));
                }
                InstallSource::Folder { path: dir.display().to_string(), dev: true }
            } else {
                source(ctx, args)?
            };
            let PluginReply::Preview { plugin, replaces } = call(conn, PluginCommand::Preview(source.clone())).await? else {
                return Err(CliError::engine("Unexpected reply."));
            };
            let yes = flag(args, "yes");
            let grant = if yes { plugin.capabilities.clone() } else { plugin.granted.clone() };
            let PluginReply::Installed(info) = call(conn, PluginCommand::Install { source, grant }).await? else {
                return Err(CliError::engine("Unexpected reply."));
            };
            let verb = match replaces {
                Some(old) if old != info.version => format!("Updated {} from {old} to {}", info.name, info.version),
                Some(_) => format!("Reinstalled {} {}", info.name, info.version),
                None => format!("Installed {} {}", info.name, info.version),
            };
            let mut human = format!("{verb} ({}).", info.summary);
            if info.status == PluginStatus::NeedsConsent {
                human.push_str(&format!(
                    "\nIt’s waiting for approval. It asks to:\n{}Approve with `orbit plugin enable {} --yes`, or in Settings › Plugins.",
                    consent_text(&info),
                    info.id
                ));
            } else if let PluginStatus::Incompatible { requires } = &info.status {
                human.push_str(&format!("\nIt needs Orbit {requires}, so it stays off."));
            }
            if info.dev {
                human.push_str("\nWatching its folder: changes reload it.");
            }
            Ok(Outcome::ok(info_json(&info), human))
        }
        ["plugin", "list"] => {
            let state = conn.state();
            let list: Vec<Value> = state.plugins.plugins.iter().map(info_json).collect();
            let human = if list.is_empty() {
                "No plugins. Install one with `orbit plugin install <folder|file|url>`.".to_owned()
            } else {
                let rows: Vec<String> = state
                    .plugins
                    .plugins
                    .iter()
                    .map(|p| format!("{:<24} {:<9} {:<15} {}", p.id, p.version, p.status.label(), p.summary))
                    .collect();
                format!("{:<24} {:<9} {:<15} {}\n{}", "ID", "VERSION", "STATUS", "CONTRIBUTES", rows.join("\n"))
            };
            Ok(Outcome::ok(json!({"plugins": list, "secrets": state.plugins.secrets}), human))
        }
        ["plugin", "enable"] => {
            let id = s(args, "id").unwrap_or_default().to_owned();
            let plugin = plugin_of(&conn.state(), &id)?.clone();
            if flag(args, "yes") && !plugin.capabilities.is_empty() {
                call(conn, PluginCommand::Grant { id: id.clone(), grant: plugin.capabilities.clone() }).await?;
            }
            call(conn, PluginCommand::Enable { id: id.clone() }).await?;
            let missing: Vec<Capability> = plugin.capabilities.iter().filter(|c| !plugin.granted.contains(c)).copied().collect();
            let human = if !missing.is_empty() && !flag(args, "yes") {
                format!(
                    "{} is on, but waits for approval. It asks to:\n{}Approve with `orbit plugin enable {id} --yes`.",
                    plugin.name,
                    consent_text(&plugin)
                )
            } else {
                format!("{} is on.", plugin.name)
            };
            Ok(Outcome::ok(json!({"id": id, "enabled": true}), human))
        }
        ["plugin", "disable"] | ["plugin", "remove"] => {
            let id = s(args, "id").unwrap_or_default().to_owned();
            let name = plugin_of(&conn.state(), &id)?.name.clone();
            let (command, done) = if path[1] == "disable" {
                (PluginCommand::Disable { id: id.clone() }, "is off")
            } else {
                (PluginCommand::Remove { id: id.clone() }, "was removed")
            };
            call(conn, command).await?;
            Ok(Outcome::ok(json!({"id": id}), format!("{name} {done}.")))
        }
        ["plugin", "logs"] => {
            let id = s(args, "id").map(str::to_owned);
            if let Some(id) = &id {
                plugin_of(&conn.state(), id)?;
            }
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100).min(5000) as u32;
            let PluginReply::Logs(lines) = call(conn, PluginCommand::Logs { id, limit }).await? else {
                return Err(CliError::engine("Unexpected reply."));
            };
            let human = if lines.is_empty() {
                "Nothing logged yet.".to_owned()
            } else {
                lines
                    .iter()
                    .map(|l| {
                        let time = crate::render::ago_or_in(l.at_ms);
                        let took = if l.micros > 0 { format!(" ({} µs)", l.micros) } else { String::new() };
                        let detail = if l.detail.is_empty() { String::new() } else { format!(": {}", l.detail) };
                        format!("{time:>8}  {:<12} {:<8} {}{took}{detail}", l.plugin, l.outcome, l.call)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let value: Vec<Value> = lines
                .iter()
                .map(|l| json!({"at_ms": l.at_ms, "plugin": l.plugin, "kind": l.kind, "call": l.call, "outcome": l.outcome, "detail": l.detail, "micros": l.micros}))
                .collect();
            Ok(Outcome::ok(json!({"logs": value}), human))
        }
        _ => Err(CliError::usage("Unknown plugin command.")),
    }
}
