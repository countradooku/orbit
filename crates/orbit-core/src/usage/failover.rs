//! Which account a thread at its limit can continue on.
//!
//! Another account of the same agent keeps the agent, its tools and its
//! habits; only the sign-in changes. It qualifies when it is a different
//! sign-in (not the same configuration folder under another name), is
//! installed and signed in, and has headroom: no window refused and none
//! past [`FULL`]. Accounts whose limits are known rank before unknown ones,
//! then by headroom.
//!
//! Another agent is a bigger step (other models, other tools), so it is
//! only ever offered, never chosen automatically.

use orbit_proto::*;

/// A window used this much is treated as out.
pub const FULL: f32 = 0.95;

/// An account a thread can move to.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub instance: String,
    pub kind: ProviderKind,
    pub name: String,
    /// Share left in its tightest window, when known.
    pub headroom: Option<f32>,
}

fn usable(info: Option<&ProviderInfo>) -> bool {
    info.is_some_and(|p| p.installed && p.signed_in != Some(false))
}

fn has_room(quota: Option<&AccountQuota>, now_ms: u64) -> bool {
    let Some(quota) = quota else { return true };
    // A window whose reset has passed starts empty, whatever it last read.
    let reset = |w: &QuotaWindowState| w.window.resets_ms > 0 && w.window.resets_ms <= now_ms;
    !quota.limited(now_ms) && quota.windows.iter().all(|w| w.used() < FULL || reset(w))
}

fn target(instance: &ProviderInstance, info: Option<&ProviderInfo>, quota: Option<&AccountQuota>) -> Target {
    let known = quota.filter(|q| !q.windows.is_empty());
    Target {
        instance: instance.id.clone(),
        kind: instance.kind,
        name: info.map(|p| p.display_name().to_owned()).unwrap_or_else(|| instance.display_name().to_owned()),
        headroom: known.map(AccountQuota::headroom),
    }
}

fn rank(targets: &mut [Target]) {
    targets.sort_by(|a, b| match (a.headroom, b.headroom) {
        (Some(x), Some(y)) => y.total_cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.name.cmp(&b.name),
    });
}

/// Other accounts of `from`'s agent with headroom, best first.
/// `instances` are the enabled instances (`presets::effective`).
pub fn same_agent(
    from: &ProviderInstance,
    instances: &[ProviderInstance],
    providers: &[ProviderInfo],
    board: &QuotaBoard,
    now_ms: u64,
) -> Vec<Target> {
    let mut out: Vec<Target> = instances
        .iter()
        .filter(|i| i.kind == from.kind && i.id != from.id && !i.shares_sessions_with(from))
        .filter_map(|i| {
            let info = providers.iter().find(|p| p.instance_id() == i.id);
            let quota = board.account(&i.id);
            (usable(info) && has_room(quota, now_ms)).then(|| target(i, info, quota))
        })
        .collect();
    rank(&mut out);
    out
}

/// Installed, signed-in instances of other agents with headroom, best
/// first: offered with a warning, never chosen automatically.
pub fn other_agents(
    from: &ProviderInstance,
    instances: &[ProviderInstance],
    providers: &[ProviderInfo],
    board: &QuotaBoard,
    now_ms: u64,
) -> Vec<Target> {
    let mut out: Vec<Target> = instances
        .iter()
        .filter(|i| i.kind != from.kind)
        .filter_map(|i| {
            let info = providers.iter().find(|p| p.instance_id() == i.id);
            let quota = board.account(&i.id);
            (usable(info) && has_room(quota, now_ms)).then(|| target(i, info, quota))
        })
        .collect();
    rank(&mut out);
    // Agents with readable plans (Claude, Codex) first: their headroom is known.
    out.sort_by_key(|t| !crate::usage::readable(t.kind));
    out
}

/// What failover does for a thread whose account just hit its limit.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// Switch now, to this instance.
    Switch(Target),
    /// Leave it to the user (the banner offers the targets).
    Offer,
    /// Wait for the reset.
    Wait,
}

/// Applies the policy: automatic switching needs an unpinned thread and a
/// same-agent target; otherwise the banner offers what there is.
pub fn decide(policy: FailoverPolicy, pinned: bool, targets: &[Target]) -> Decision {
    match policy {
        FailoverPolicy::Never => Decision::Wait,
        FailoverPolicy::Auto if !pinned => match targets.first() {
            Some(target) => Decision::Switch(target.clone()),
            None => Decision::Offer,
        },
        _ => Decision::Offer,
    }
}

/// The config `config` becomes on `to`: the same mode and access; on
/// another agent, that agent's default model and options.
pub fn moved_config(config: &ThreadConfig, to: &ProviderInstance) -> ThreadConfig {
    let mut next = config.clone();
    next.instance = if to.id == to.kind.id() { String::new() } else { to.id.clone() };
    if to.kind != config.provider {
        next.provider = to.kind;
        next.model = String::new();
        next.options = Vec::new();
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(id: &str, kind: ProviderKind, dir: &str) -> ProviderInstance {
        ProviderInstance { id: id.into(), kind, enabled: true, config_dir: dir.into(), ..Default::default() }
    }

    fn info(id: &str, kind: ProviderKind, signed_in: bool) -> ProviderInfo {
        ProviderInfo { kind, instance: id.into(), installed: true, signed_in: Some(signed_in), name: id.into(), ..Default::default() }
    }

    fn account(id: &str, used: f32, rejected: bool, resets_ms: u64) -> AccountQuota {
        AccountQuota {
            instance: id.into(),
            windows: vec![QuotaWindowState {
                window: QuotaWindow { window: "five_hour".into(), utilization: Some(used), resets_ms, rejected },
                label: "5-hour".into(),
                span_ms: 0,
                burn_per_hour: None,
                runs_out_ms: None,
            }],
            ..Default::default()
        }
    }

    fn setup() -> (Vec<ProviderInstance>, Vec<ProviderInfo>, QuotaBoard) {
        use ProviderKind::*;
        let instances = vec![
            ProviderInstance::builtin(Claude),
            instance("claude-work", Claude, "~/.claude-work"),
            instance("claude-alias", Claude, ""),
            instance("claude-team", Claude, "~/.claude-team"),
            instance("claude-out", Claude, "~/.claude-out"),
            instance("claude-new", Claude, "~/.claude-new"),
            ProviderInstance::builtin(Codex),
        ];
        let providers = vec![
            info("claude", Claude, true),
            info("claude-work", Claude, true),
            info("claude-alias", Claude, true),
            info("claude-team", Claude, true),
            info("claude-out", Claude, false),
            info("claude-new", Claude, true),
            info("codex", Codex, true),
        ];
        let board = QuotaBoard {
            accounts: vec![
                account("claude", 1.0, true, 5_000),
                account("claude-work", 0.6, false, 9_000),
                account("claude-team", 0.2, false, 9_000),
                account("codex", 0.97, false, 9_000),
            ],
            ..Default::default()
        };
        (instances, providers, board)
    }

    #[test]
    fn the_best_other_account_of_the_same_agent_wins() {
        let (instances, providers, board) = setup();
        let from = &instances[0];
        let targets = same_agent(from, &instances, &providers, &board, 1_000);
        let ids: Vec<&str> = targets.iter().map(|t| t.instance.as_str()).collect();
        // The alias shares the default folder (same sign-in), `claude-out`
        // is signed out, and `claude-new` has no reading yet: it ranks last.
        assert_eq!(ids, ["claude-team", "claude-work", "claude-new"]);
        assert!((targets[0].headroom.unwrap() - 0.8).abs() < 1e-6);
        // Codex is at 97 %: not offered until its window resets.
        assert!(other_agents(from, &instances, &providers, &board, 1_000).is_empty());
        let later = other_agents(from, &instances, &providers, &board, 10_000);
        assert_eq!(later.iter().map(|t| t.instance.as_str()).collect::<Vec<_>>(), ["codex"]);
    }

    #[test]
    fn the_policy_decides_and_pins_hold() {
        let (instances, providers, board) = setup();
        let targets = same_agent(&instances[0], &instances, &providers, &board, 1_000);
        assert_eq!(decide(FailoverPolicy::Auto, false, &targets), Decision::Switch(targets[0].clone()));
        assert_eq!(decide(FailoverPolicy::Auto, true, &targets), Decision::Offer, "a pinned thread is only offered");
        assert_eq!(decide(FailoverPolicy::Auto, false, &[]), Decision::Offer);
        assert_eq!(decide(FailoverPolicy::Ask, false, &targets), Decision::Offer);
        assert_eq!(decide(FailoverPolicy::Never, false, &targets), Decision::Wait);
    }

    #[test]
    fn moving_keeps_the_mode_and_resets_another_agents_model() {
        let config = ThreadConfig {
            provider: ProviderKind::Claude,
            model: "opus".into(),
            options: vec![OptionValue { id: "effort".into(), value: OptionSetting::Choice("high".into()) }],
            mode: Mode::Plan,
            access: Access::AutoEdit,
            instance: String::new(),
        };
        let work = instance("claude-work", ProviderKind::Claude, "~/.claude-work");
        let moved = moved_config(&config, &work);
        assert_eq!((moved.instance.as_str(), moved.model.as_str(), moved.options.len()), ("claude-work", "opus", 1));
        let back = moved_config(&moved, &ProviderInstance::builtin(ProviderKind::Claude));
        assert_eq!(back.instance, "", "the built-in instance is named by leaving it empty");
        let codex = moved_config(&config, &ProviderInstance::builtin(ProviderKind::Codex));
        assert_eq!(codex.provider, ProviderKind::Codex);
        assert_eq!((codex.model.as_str(), codex.options.len(), codex.mode, codex.access), ("", 0, Mode::Plan, Access::AutoEdit));
    }
}
