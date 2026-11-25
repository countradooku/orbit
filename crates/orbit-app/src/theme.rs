//! Orbit's theme: GPUI Kit's component system with Orbit's color roles.
//!
//! Colors live only in theme files (the theme layer): Orbit's own pair in
//! `assets/orbit-theme.json`, plus the GPUI Kit theme collection in
//! `assets/themes/` (copied from the GPUI Kit repository, Apache-2.0). Views
//! read semantic tokens from `cx.theme()`.
//!
//! [`apply`] is the one place appearance settings reach the theme: the light
//! and dark theme choice, the color mode, interface zoom (the base font, so
//! everything on the rem scale follows) and the code font size.

use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
use gpui_kit::*;
use orbit_proto::{AppearanceSettings, ColorMode};

const ORBIT_THEMES: &str = include_str!("../assets/orbit-theme.json");
const BUNDLED_THEMES: &[&str] = &[
    include_str!("../assets/themes/adventure.json"),
    include_str!("../assets/themes/alduin.json"),
    include_str!("../assets/themes/asciinema.json"),
    include_str!("../assets/themes/aurora.json"),
    include_str!("../assets/themes/ayu.json"),
    include_str!("../assets/themes/catppuccin.json"),
    include_str!("../assets/themes/everforest.json"),
    include_str!("../assets/themes/fahrenheit.json"),
    include_str!("../assets/themes/flexoki.json"),
    include_str!("../assets/themes/gruvbox.json"),
    include_str!("../assets/themes/harper.json"),
    include_str!("../assets/themes/hybrid.json"),
    include_str!("../assets/themes/jellybeans.json"),
    include_str!("../assets/themes/kibble.json"),
    include_str!("../assets/themes/macos-classic.json"),
    include_str!("../assets/themes/mellifluous.json"),
    include_str!("../assets/themes/molokai.json"),
    include_str!("../assets/themes/solarized.json"),
    include_str!("../assets/themes/spaceduck.json"),
    include_str!("../assets/themes/tokyonight.json"),
    include_str!("../assets/themes/twilight.json"),
];

pub const ORBIT_LIGHT: &str = "Orbit Light";
pub const ORBIT_DARK: &str = "Orbit Dark";

/// The base font at 100 % zoom; the whole interface is derived from it.
const BASE_FONT: f32 = 14.;
const RADIUS: f32 = 7.;
const RADIUS_LG: f32 = 10.;

pub fn install(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    if let Err(err) = registry.load_themes_from_str(ORBIT_THEMES) {
        log::error!("Orbit theme failed to load: {err}");
    }
    for themes in BUNDLED_THEMES {
        if let Err(err) = registry.load_themes_from_str(themes) {
            log::warn!("A bundled theme failed to load: {err}");
        }
    }
    apply(&AppearanceSettings::default(), None, cx);
}

/// Names of the installed themes for light or dark mode: Orbit's first,
/// then the rest by name.
pub fn names(dark: bool, cx: &App) -> Vec<SharedString> {
    let orbit = if dark { ORBIT_DARK } else { ORBIT_LIGHT };
    let plugin_themes = crate::plugins::themes(cx);
    let mut names: Vec<SharedString> = ThemeRegistry::global(cx)
        .themes()
        .values()
        .chain(plugin_themes.iter())
        .filter(|t| t.mode.is_dark() == dark && t.name.as_ref() != orbit && !t.name.starts_with("Default "))
        .map(|t| t.name.clone())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.insert(0, orbit.into());
    names
}

/// The theme a setting resolves to: an empty, unknown or wrong-mode name
/// falls back to Orbit's.
pub fn resolve(name: &str, dark: bool, cx: &App) -> SharedString {
    match lookup(name, cx) {
        Some(theme) if theme.mode.is_dark() == dark => theme.name.clone(),
        _ => (if dark { ORBIT_DARK } else { ORBIT_LIGHT }).into(),
    }
}

/// A theme by name: a bundled one, or one a plugin adds (see
/// `crate::plugins`, which keeps them out of the registry so they can go
/// away again).
fn lookup(name: &str, cx: &App) -> Option<std::rc::Rc<gpui_kit::component::ThemeConfig>> {
    ThemeRegistry::global(cx).themes().get(name).cloned().or_else(|| crate::plugins::theme(name, cx))
}

/// Applies appearance settings to the theme and refreshes every window.
/// With `ColorMode::System`, `window` (when given) supplies the appearance.
pub fn apply(settings: &AppearanceSettings, window: Option<&mut Window>, cx: &mut App) {
    let light_name = resolve(&settings.light_theme, false, cx);
    let dark_name = resolve(&settings.dark_theme, true, cx);
    let (light, dark) = (lookup(&light_name, cx), lookup(&dark_name, cx));
    Theme::update(cx, |theme| {
        if let Some(light) = light {
            theme.light_theme = light;
        }
        if let Some(dark) = dark {
            theme.dark_theme = dark;
        }
    });
    // Re-applies the config for the resulting mode, even when it is unchanged.
    match settings.mode {
        ColorMode::System => Theme::sync_system_appearance(window, cx),
        ColorMode::Light => Theme::change(ThemeMode::Light, None, cx),
        ColorMode::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
    let scale = f32::from(settings.ui_scale) / 100.;
    let code = f32::from(settings.code_font_size);
    Theme::update(cx, |theme| {
        let config = if theme.mode.is_dark() { theme.dark_theme.clone() } else { theme.light_theme.clone() };
        // Zoom is one setting: the rem follows the base font.
        theme.font_size = px((BASE_FONT * scale).round());
        theme.mono_font_size = px((code * scale).round());
        if config.radius.is_none() {
            theme.radius = px(RADIUS);
        }
        if config.radius_lg.is_none() {
            theme.radius_lg = px(RADIUS_LG);
        }
    });
}
