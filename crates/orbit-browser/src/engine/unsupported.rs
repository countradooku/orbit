//! Windows on upstream GPUI (`fast` off): no window composition, so no
//! embedded page. The view shows why and offers the system browser.

use gpui_kit::{AnyElement, App, IntoElement, Window, div};
use url::Url;

use super::{EngineEvent, EngineEvents, report};

pub(crate) struct Engine;

impl Engine {
    pub(crate) fn new(_: Option<&Url>, events: EngineEvents, _: &mut Window, _: &mut App) -> Self {
        report(
            &events,
            EngineEvent::Unavailable(
                "The embedded preview needs Orbit’s GPUI Fast renderer on Windows. Build with the `fast` feature."
                    .into(),
            ),
        );
        Self
    }

    pub(crate) fn navigate(&self, _: &Url, _: &mut App) {}
    pub(crate) fn back(&self, _: &mut App) {}
    pub(crate) fn forward(&self, _: &mut App) {}
    pub(crate) fn reload(&self, _: &mut App) {}
    pub(crate) fn stop(&self, _: &mut App) {}

    pub(crate) fn toggle_devtools(&self, _: &mut App) -> Option<bool> {
        None
    }

    pub(crate) fn tracks_history(&self) -> bool {
        false
    }

    pub(crate) fn set_visible(&self, _: bool, _: &mut App) {}
    pub(crate) fn focus_parent(&self) {}

    pub(crate) fn page(&self) -> AnyElement {
        div().into_any_element()
    }

    pub(crate) fn release(self, _: &mut App) {}
}
