//! The usage-limit banner's offer: continue the thread on another account of
//! the same agent at once, or on another agent after a warning. The engine
//! does the move (handoff, a notice in the timeline, the limited turn sent
//! again); this only offers it.

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{Sizable as _, WindowExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_core::provider::presets;
use orbit_core::usage::failover::{self, Target};
use orbit_proto::*;

use crate::app_state::AppState;
use crate::assets::Lucide;

/// Where a limited thread can go: other accounts of its agent, then other
/// agents. Empty when the policy is Never.
pub fn targets(thread: &Thread, app: &Entity<AppState>, cx: &App) -> (Vec<Target>, Vec<Target>) {
    if super::settings(cx).failover == FailoverPolicy::Never {
        return (Vec::new(), Vec::new());
    }
    let state = app.read(cx).state();
    let configured = &crate::settings::get(cx).agents.instances;
    let instances: Vec<ProviderInstance> = presets::effective(configured).into_iter().map(|(i, _)| i).collect();
    let from = presets::find(configured, thread.config.instance_id())
        .filter(|i| i.kind == thread.config.provider)
        .unwrap_or_else(|| ProviderInstance::builtin(thread.config.provider));
    let now = now_ms();
    (
        failover::same_agent(&from, &instances, &state.providers, &state.quota, now),
        failover::other_agents(&from, &instances, &state.providers, &state.quota, now),
    )
}

fn move_to(app: &Entity<AppState>, thread: ThreadId, instance: String, window: &mut Window, cx: &mut App) {
    app.update(cx, |app, cx| app.run(Command::Usage(UsageCommand::Failover { thread, instance }), window, cx));
}

/// Asks before moving to another agent: it starts from a summary, with its
/// own models and tools.
fn confirm_other_agent(app: Entity<AppState>, thread: ThreadId, from: String, to: Target, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (app, to_id) = (app.clone(), to.instance.clone());
        alert
            .title(format!("Continue on {}?", to.name))
            .description(format!(
                "{} is a different agent. It continues from a summary of this conversation, with its own models, tools \
                 and permissions; {from}’s session is not carried over. Your files stay as they are.",
                to.name
            ))
            .confirm()
            .ok_text(format!("Continue on {}", to.name))
            .ok_variant(ButtonVariant::Primary)
            .on_ok(move |_, window, cx| {
                move_to(&app, thread, to_id.clone(), window, cx);
                true
            })
    });
}

fn headroom(target: &Target) -> String {
    match target.headroom {
        Some(room) => format!("{} · {}% left", target.name, (room * 100.0).round()),
        None => target.name.clone(),
    }
}

/// The banner's buttons, or nothing when there is nowhere to go.
pub fn offer(thread: &Thread, app: &Entity<AppState>, cx: &App) -> Option<AnyElement> {
    let (same, other) = targets(thread, app, cx);
    if same.is_empty() && other.is_empty() {
        return None;
    }
    let id = thread.id;
    let from = match app.read(cx).state().provider_for(&thread.config) {
        Some(p) => p.display_name().to_owned(),
        None => thread.config.provider.label().to_owned(),
    };
    let primary = same.first().cloned().map(|best| {
        let app = app.clone();
        Button::new("failover-best")
            .small()
            .primary()
            .icon(Lucide::ArrowLeftRight)
            .label(format!("Continue on {}", best.name))
            .tooltip("Moves this thread to that account and sends your message again there")
            .on_click(move |_, window, cx| move_to(&app, id, best.instance.clone(), window, cx))
    });
    let rest: Vec<Target> = same.iter().skip(1).cloned().collect();
    let more = (!rest.is_empty() || !other.is_empty()).then(|| {
        let app = app.clone();
        let label = if same.is_empty() { "Switch agent…" } else { "Other…" };
        Button::new("failover-more").small().outline().label(label).dropdown_caret(true).dropdown_menu(move |mut menu, _, _| {
            for target in &rest {
                let (app, instance) = (app.clone(), target.instance.clone());
                menu = menu.item(PopupMenuItem::new(headroom(target)).on_click(move |_, window, cx| move_to(&app, id, instance.clone(), window, cx)));
            }
            if !rest.is_empty() && !other.is_empty() {
                menu = menu.separator();
            }
            for target in &other {
                let (app, target, from) = (app.clone(), target.clone(), from.clone());
                menu = menu.item(
                    PopupMenuItem::new(format!("{}…", headroom(&target)))
                        .on_click(move |_, window, cx| confirm_other_agent(app.clone(), id, from.clone(), target.clone(), window, cx)),
                );
            }
            menu
        })
    });
    Some(h_flex().gap_1().children(primary).when_some(more, |el, more| el.child(more)).into_any_element())
}
