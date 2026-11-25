//! Plugins in a running engine, with real processes: an agent manifest
//! becomes a provider instance that runs a session (against the scripted
//! `orbit-fake-agent`), and the WebAssembly guard example vetoes that
//! agent's `rm -rf /` before anyone is asked.

#[path = "support/agent.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use orbit_core::{EngineHandle, EngineOptions};
use orbit_proto::*;
use sonic_rs::{JsonValueTrait, Value, json};
use support::*;

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugins")
}

fn plugin(engine: &EngineHandle, command: PluginCommand) -> Result<PluginReply, String> {
    match engine.call(Command::Plugin(Box::new(command))).recv().unwrap()? {
        Reply::Plugin(reply) => Ok(*reply),
        other => panic!("{other:?}"),
    }
}

/// Waits for an event `pick` accepts.
fn wait_for<T>(sub: &orbit_core::Subscribed, what: &str, mut pick: impl FnMut(&Event) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match sub.events.try_recv() {
            Ok(env) => {
                if let Some(found) = pick(&env.event) {
                    return found;
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn an_agent_manifest_runs_a_session_and_the_guard_vetoes_its_rm_rf() {
    let permission = json!({"jsonrpc": "2.0", "id": 500, "method": "session/request_permission", "params": {
        "sessionId": "s-1",
        "toolCall": {"toolCallId": "c1", "title": "Clean up", "kind": "execute", "rawInput": {"command": "rm -rf /"}},
        "options": [{"optionId": "yes", "name": "Allow", "kind": "allow_once"}, {"optionId": "no", "name": "Reject", "kind": "reject_once"}]
    }});
    let fake = Fake::new(
        "fake-acp",
        json!({"protocol": "acp",
          "handlers": {
            "initialize": {"protocolVersion": 1, "agentCapabilities": {"loadSession": false}, "agentInfo": {"name": "fake", "version": "1.2.3"}},
            "session/new": {"sessionId": "s-1"},
            "session/set_model": {}
          },
          "steps": [
            {"expect": "session/prompt", "reply": false},
            {"send": permission},
            {"awaitResponse": 500},
            {"send": {"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s-1", "update":
                {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "I left / alone."}}}}},
            {"respond": "session/prompt", "result": {"stopReason": "end_turn"}}
          ]}),
    );
    let manifest_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        manifest_dir.path().join("orbit-plugin.toml"),
        format!(
            r#"id = "fake-agents"
name = "Fake agents"
version = "1.0.0"
engines = {{ orbit = ">=0.1.0" }}
capabilities = ["agents"]
[[agents]]
id = "fake-acp"
name = "Fake ACP"
protocol = "acp"
command = {}
args = ["--experimental-acp"]
models = [{{ id = "fake-model", name = "Fake model", default = true }}]
"#,
            sonic_rs::to_string(&fake.exe.display().to_string()).unwrap()
        ),
    )
    .unwrap();

    let data = tempfile::tempdir().unwrap();
    let launcher: orbit_core::engine::Launcher = Box::new(orbit_core::provider::start);
    let engine = EngineHandle::start_with(EngineOptions { data_dir: data.path().to_owned(), probe_providers: false }, launcher).unwrap();
    let sub = engine.subscribe(0, 0).recv().unwrap();
    assert!(sub.snapshot.as_ref().unwrap().plugins.plugins.is_empty());

    // Previewing shows what consent is asked for, and installs nothing.
    let folder = InstallSource::Folder { path: manifest_dir.path().display().to_string(), dev: false };
    let PluginReply::Preview { plugin: preview, replaces } = plugin(&engine, PluginCommand::Preview(folder.clone())).unwrap() else { panic!() };
    assert_eq!((preview.capabilities.as_slice(), replaces), ([Capability::Agents].as_slice(), None));
    assert_eq!(preview.ui.agents.len(), 1, "a preview lists the programs it would run");
    assert!(preview.ui.themes.is_empty() && preview.ui.commands.is_empty(), "and nothing else");

    let PluginReply::Installed(info) = plugin(&engine, PluginCommand::Install { source: folder, grant: vec![Capability::Agents] }).unwrap() else {
        panic!()
    };
    assert_eq!(info.status, PluginStatus::Active);
    let guard = InstallSource::Folder { path: examples().join("guard-rm").display().to_string(), dev: false };
    plugin(&engine, PluginCommand::Install { source: guard, grant: vec![Capability::HooksTools] }).unwrap();

    // The agent is probed like a built-in one, with the manifest's model.
    let probed = wait_for(&sub, "the plugin agent's probe", |e| match e {
        Event::Providers(list) => list.iter().find(|p| p.instance_id() == "fake-acp" && p.installed).cloned(),
        _ => None,
    });
    assert_eq!(probed.name, "Fake ACP");
    assert_eq!(probed.version.as_deref(), Some("1.2.3"));
    assert_eq!(probed.models[0].slug, "fake-model", "the manifest's models when the agent reports none");

    let config = ThreadConfig { provider: ProviderKind::Acp, instance: "fake-acp".into(), model: "fake-model".into(), ..Default::default() };
    let Ok(Reply::Thread(thread)) = engine.call(Command::CreateThread { project: None, config, worktree: false }).recv().unwrap() else {
        panic!("no thread")
    };
    engine.call(Command::Send { thread, text: "Free some disk space".into(), attachments: vec![], steer: false }).recv().unwrap().unwrap();
    let notice = wait_for(&sub, "the guard's notice", |e| match e {
        Event::ItemUpserted { thread: t, item } if *t == thread => match &item.body {
            ItemBody::Notice { text, .. } if text.contains("Blocked by the") => Some(text.clone()),
            _ => None,
        },
        _ => None,
    });
    assert!(notice.contains("Guard: rm -rf /") && notice.contains("Recursively deletes /"), "{notice}");
    wait_for(&sub, "the turn's end", |e| match e {
        Event::ThreadUpserted(t) if t.id == thread && t.status == ThreadStatus::Idle && t.turns == 1 => Some(()),
        _ => None,
    });
    // The agent was told no, without the thread ever waiting on a person.
    let answer: Value = fake
        .received()
        .into_iter()
        .find(|m| m.get("id").and_then(|v| v.as_u64()) == Some(500) && m.get("method").is_none())
        .expect("the permission was answered");
    assert_eq!(str_at(&answer, &["result", "outcome", "optionId"]), Some("no"));
    let PluginReply::Logs(logs) = plugin(&engine, PluginCommand::Logs { id: Some("guard-rm".into()), limit: 50 }).unwrap() else { panic!() };
    assert!(logs.iter().any(|l| l.outcome == "denied"), "{logs:?}");

    // Disabling the agent plugin takes its instance away.
    plugin(&engine, PluginCommand::Disable { id: "fake-agents".into() }).unwrap();
    wait_for(&sub, "the instance to go", |e| match e {
        Event::Providers(list) if !list.iter().any(|p| p.instance_id() == "fake-acp") => Some(()),
        _ => None,
    });
    engine.shutdown();
}
