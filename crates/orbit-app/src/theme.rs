//! Orbit's theme: GPUI Kit's component system with Orbit's color roles.
//!
//! Colors live only in `assets/orbit-theme.json` (the theme layer); views
//! read semantic tokens from `cx.theme()`. The app follows the system's light
//! or dark appearance unless the user picks one.

use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
use gpui_kit::*;

const ORBIT_THEMES: &str = include_str!("../assets/orbit-theme.json");

/// The user's appearance preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

pub fn install(cx: &mut App) {
    if let Err(err) = ThemeRegistry::global_mut(cx).load_themes_from_str(ORBIT_THEMES) {
        log::error!("Orbit theme failed to load: {err}");
    }
    let (light, dark) = {
        let themes = ThemeRegistry::global(cx).themes();
        (themes.get("Orbit Light").cloned(), themes.get("Orbit Dark").cloned())
    };
    Theme::update(cx, |theme| {
        if let Some(light) = light {
            theme.light_theme = light;
        }
        if let Some(dark) = dark {
            theme.dark_theme = dark;
        }
        // Base scale for the whole interface (the rem). Everything else is
        // derived from it, so zoom is one setting.
        theme.font_size = px(14.);
        theme.radius = px(7.);
        theme.radius_lg = px(10.);
    });
}

/// Applies `appearance` to `window`.
pub fn apply(appearance: Appearance, window: &mut Window, cx: &mut App) {
    match appearance {
        Appearance::System => Theme::sync_system_appearance(Some(window), cx),
        Appearance::Light => Theme::change(ThemeMode::Light, None, cx),
        Appearance::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
}
