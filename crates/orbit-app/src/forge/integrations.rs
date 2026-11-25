//! The host and tracker settings as the interface edits them.
//!
//! The engine keeps them in their own file (tokens stay out of the logged
//! settings record); this global mirrors them for the settings page, which
//! reads through `&App` closures, and saves edits after a short pause so a
//! pasted token costs one write.

use std::time::Duration;

use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;

const SAVE_DELAY: Duration = Duration::from_millis(500);

pub struct Integrations {
    app: Entity<AppState>,
    current: IntegrationSettings,
    save: Option<Task<()>>,
}

impl Global for Integrations {}

pub fn init(app: Entity<AppState>, cx: &mut App) {
    cx.set_global(Integrations { app, current: IntegrationSettings::default(), save: None });
    reload(cx);
}

/// Reads the engine's copy (at startup).
pub fn reload(cx: &mut App) {
    let app = cx.global::<Integrations>().app.clone();
    let task = app.update(cx, |app, cx| app.call(Command::Forge(ForgeCommand::Integrations), cx));
    cx.spawn(async move |cx| {
        if let Ok(Reply::Forge(ForgeReply::Integrations(settings))) = task.await {
            cx.update(|cx| {
                let model = cx.global_mut::<Integrations>();
                // An edit made while loading wins.
                if model.save.is_none() {
                    model.current = settings;
                }
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

pub fn get(cx: &App) -> &IntegrationSettings {
    &cx.global::<Integrations>().current
}

pub fn update(cx: &mut App, edit: impl FnOnce(&mut IntegrationSettings)) {
    let model = cx.global_mut::<Integrations>();
    let before = model.current.clone();
    edit(&mut model.current);
    if model.current == before {
        return;
    }
    let app = model.app.clone();
    let task = cx.spawn(async move |cx| {
        cx.background_executor().timer(SAVE_DELAY).await;
        cx.update(|cx| {
            let model = cx.global_mut::<Integrations>();
            model.save = None;
            let settings = model.current.clone();
            app.update(cx, |app, _| app.send(Command::Forge(ForgeCommand::SetIntegrations { settings })));
        });
    });
    cx.global_mut::<Integrations>().save = Some(task);
    cx.refresh_windows();
}
