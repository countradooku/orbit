//! Theme plugins: GPUI Kit theme sets, checked before they reach the app.
//!
//! A theme file is the format of Orbit's bundled themes
//! (`crates/orbit-app/assets/themes`): `{ "name", "themes": [ { "name",
//! "mode", "colors": { token: "#rrggbb" }, "highlight": {…} } ] }`. The
//! tokens are GPUI Kit's (`background`, `primary.foreground`, …); the
//! terminal takes its ANSI colors from `base.*`. Tokens a theme leaves out
//! fall back to GPUI Kit's defaults.
//!
//! Checks: only colors (no fonts, no unknown value shapes), at most
//! [`MAX_BYTES`], and contrast. `foreground` on `background` must reach
//! 4.5:1 or the theme is refused; any other text/surface pair under its
//! threshold is dropped (both tokens fall back), with a warning. Names
//! that would pose as Orbit's own themes are refused.

use sonic_rs::{JsonContainerTrait as _, JsonValueMutTrait as _, JsonValueTrait as _, Value};

pub const MAX_BYTES: usize = 128 * 1024;

/// Text tokens and the surface they sit on, with the contrast each needs.
const PAIRS: &[(&str, &str, f64)] = &[
    ("foreground", "background", 4.5),
    ("muted.foreground", "background", 3.0),
    ("primary.foreground", "primary.background", 4.5),
    ("secondary.foreground", "secondary.background", 4.5),
    ("accent.foreground", "accent.background", 4.5),
    ("popover.foreground", "popover.background", 4.5),
    ("tab.active.foreground", "tab.active.background", 4.5),
    ("editor.foreground", "editor.background", 4.5),
    ("danger.foreground", "danger.background", 4.5),
];

/// Per-theme keys a plugin theme may set.
const THEME_KEYS: &[&str] = &["name", "mode", "colors", "highlight", "radius", "radius.lg", "shadow"];

/// A checked theme set: the cleaned JSON, the theme names, and what was
/// dropped.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckedTheme {
    pub json: String,
    pub names: Vec<String>,
    pub warnings: Vec<String>,
}

/// Checks a theme-set file and returns it with low-contrast pairs removed.
pub fn check(text: &str) -> Result<CheckedTheme, String> {
    if text.len() > MAX_BYTES {
        return Err(format!("larger than {} KiB.", MAX_BYTES / 1024));
    }
    let mut root: Value = sonic_rs::from_str(text).map_err(|e| format!("not valid JSON ({e})."))?;
    let mut warnings = Vec::new();
    let mut names = Vec::new();
    {
        let obj = root.as_object().ok_or("must be a JSON object.")?;
        for (key, _) in obj.iter() {
            if !matches!(key, "$schema" | "name" | "author" | "url" | "themes") {
                return Err(format!("unknown key “{key}” (expected name, author, url, themes)."));
            }
        }
    }
    if root.get("name").and_then(|n| n.as_str()).is_none_or(|n| n.trim().is_empty()) {
        return Err("needs a name.".into());
    }
    let themes = root.get_mut("themes").and_then(|t| t.as_array_mut()).ok_or("needs a themes array.")?;
    if themes.is_empty() || themes.len() > 8 {
        return Err("needs 1 to 8 themes.".into());
    }
    for theme in themes.iter_mut() {
        let obj = theme.as_object_mut().ok_or("each theme must be an object.")?;
        for (key, _) in obj.iter() {
            if !THEME_KEYS.contains(&key) {
                return Err(format!("theme key “{key}” isn’t allowed (allowed: {}).", THEME_KEYS.join(", ")));
            }
        }
        let name = obj.get(&"name").and_then(|n| n.as_str()).map(str::trim).unwrap_or_default().to_owned();
        if name.is_empty() || name.chars().count() > 64 {
            return Err("each theme needs a name of at most 64 characters.".into());
        }
        let lower = name.to_lowercase();
        if lower.starts_with("orbit ") || lower.starts_with("default ") {
            return Err(format!("“{name}” is a name reserved for Orbit’s own themes."));
        }
        let mode = obj.get(&"mode").and_then(|m| m.as_str()).unwrap_or_default();
        if !matches!(mode, "light" | "dark") {
            return Err(format!("“{name}”: mode must be light or dark."));
        }
        if let Some(radius) = obj.get(&"radius").or_else(|| obj.get(&"radius.lg"))
            && !radius.as_u64().is_some_and(|r| r <= 24)
        {
            return Err(format!("“{name}”: radius must be a whole number from 0 to 24."));
        }
        if let Some(highlight) = obj.get(&"highlight") {
            check_highlight(highlight, &name, 0)?;
        }
        let colors = obj.get_mut(&"colors").and_then(|c| c.as_object_mut()).ok_or_else(|| format!("“{name}”: needs colors."))?;
        let mut parsed = std::collections::HashMap::new();
        for (token, value) in colors.iter() {
            if token.len() > 64 || !token.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_') {
                return Err(format!("“{name}”: “{token}” isn’t a theme token."));
            }
            let text = value.as_str().ok_or_else(|| format!("“{name}”: {token} must be a color string."))?;
            match parse_paint(text) {
                Some(Some(rgba)) => {
                    parsed.insert(token.to_owned(), rgba);
                }
                // GPUI Kit's palette names (`green-600`, `blue/50`): valid,
                // but their contrast can't be judged here.
                Some(None) if !matches!(token, "background" | "foreground") => {}
                _ => return Err(format!("“{name}”: {token} “{text}” isn’t a #rrggbb color.")),
            }
        }
        let background = *parsed.get("background").ok_or_else(|| format!("“{name}”: needs a #rrggbb background color."))?;
        if !parsed.contains_key("foreground") {
            return Err(format!("“{name}”: needs a #rrggbb foreground color."));
        }
        for &(text_token, surface_token, needed) in PAIRS {
            let (Some(text), Some(surface)) = (parsed.get(text_token), parsed.get(surface_token)) else { continue };
            let surface = over(*surface, background);
            let ratio = contrast(over(*text, surface), surface);
            if ratio + 1e-9 >= needed {
                continue;
            }
            if text_token == "foreground" {
                return Err(format!("“{name}”: foreground on background is {ratio:.2}:1; text needs at least 4.5:1."));
            }
            colors.remove(&text_token);
            colors.remove(&surface_token);
            warnings.push(format!("“{name}”: {text_token} on {surface_token} is {ratio:.2}:1 (needs {needed}:1); both use the defaults."));
        }
        names.push(name);
    }
    Ok(CheckedTheme { json: root.to_string(), names, warnings })
}

/// Syntax styles: nested objects whose strings are colors or font styles.
fn check_highlight(value: &Value, theme: &str, depth: usize) -> Result<(), String> {
    if depth > 4 {
        return Err(format!("“{theme}”: highlight is nested too deeply."));
    }
    if let Some(obj) = value.as_object() {
        for (key, v) in obj.iter() {
            if let Some(s) = v.as_str() {
                let ok = match key {
                    "font_style" => matches!(s, "normal" | "italic" | "oblique" | "underline"),
                    _ => s.is_empty() || parse_color(s).is_some(),
                };
                if !ok {
                    return Err(format!("“{theme}”: highlight {key} “{s}” isn’t a color."));
                }
            } else if v.is_object() {
                check_highlight(v, theme, depth + 1)?;
            } else if !(v.is_number() || v.is_null() || v.is_boolean()) {
                return Err(format!("“{theme}”: highlight {key} has an unexpected value."));
            }
        }
        Ok(())
    } else {
        Err(format!("“{theme}”: highlight must be an object."))
    }
}

/// A color, or a `linear-gradient([<angle>deg | to <side>,] <stop>, <stop>)`
/// as GPUI Kit reads it. `Some(None)`: valid, but a palette name whose
/// RGB isn't known here; a gradient is judged by its first stop.
fn parse_paint(text: &str) -> Option<Option<[f64; 4]>> {
    let text = text.trim();
    let Some(inner) = text.strip_prefix("linear-gradient(").and_then(|t| t.strip_suffix(')')) else {
        return parse_color(text).map(Some).or_else(|| is_palette_color(text).then_some(None));
    };
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    let stops = match parts.first() {
        Some(first) if first.ends_with("deg") || first.starts_with("to ") => {
            if first.ends_with("deg") && first.trim_end_matches("deg").trim().parse::<f32>().is_err() {
                return None;
            }
            &parts[1..]
        }
        _ => &parts[..],
    };
    if stops.len() != 2 {
        return None;
    }
    let mut first = None;
    for (i, stop) in stops.iter().enumerate() {
        let mut words = stop.split_whitespace();
        let color = words.next()?;
        if let Some(pct) = words.next()
            && pct.strip_suffix('%').is_none_or(|p| p.parse::<f32>().is_err())
        {
            return None;
        }
        let rgba = match parse_color(color) {
            Some(c) => Some(c),
            None if is_palette_color(color) => None,
            None => return None,
        };
        if i == 0 {
            first = rgba;
        }
    }
    Some(first)
}

/// GPUI Kit's palette colors: `name`, `name-scale`, `name/opacity`,
/// `name-scale/opacity`.
fn is_palette_color(text: &str) -> bool {
    const NAMES: &[&str] = &[
        "black", "white", "slate", "gray", "zinc", "neutral", "stone", "red", "orange", "amber", "yellow", "lime", "green",
        "emerald", "teal", "cyan", "sky", "blue", "indigo", "violet", "purple", "fuchsia", "pink", "rose",
    ];
    let (color, opacity) = text.split_once('/').map_or((text, None), |(c, o)| (c, Some(o)));
    if opacity.is_some_and(|o| !o.parse::<f32>().is_ok_and(|o| (0. ..=100.).contains(&o))) {
        return false;
    }
    let (name, scale) = color.split_once('-').map_or((color, None), |(n, s)| (n, Some(s)));
    NAMES.contains(&name) && scale.is_none_or(|s| s.parse::<u16>().is_ok_and(|s| (50..=950).contains(&s)))
}

/// `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` as RGBA in 0..=1.
pub fn parse_color(text: &str) -> Option<[f64; 4]> {
    let hex = text.strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let digits: Vec<u8> = match hex.len() {
        3 | 4 => hex.chars().map(|c| u8::from_str_radix(&format!("{c}{c}"), 16).ok()).collect::<Option<_>>()?,
        6 | 8 => (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect::<Option<_>>()?,
        _ => return None,
    };
    let a = digits.get(3).copied().unwrap_or(255);
    Some([f64::from(digits[0]) / 255., f64::from(digits[1]) / 255., f64::from(digits[2]) / 255., f64::from(a) / 255.])
}

/// `top` composited over an opaque `bottom`.
fn over(top: [f64; 4], bottom: [f64; 4]) -> [f64; 4] {
    let a = top[3];
    [top[0] * a + bottom[0] * (1. - a), top[1] * a + bottom[1] * (1. - a), top[2] * a + bottom[2] * (1. - a), 1.]
}

/// WCAG 2 contrast ratio of two opaque colors.
pub fn contrast(a: [f64; 4], b: [f64; 4]) -> f64 {
    let lum = |c: [f64; 4]| {
        let ch = |v: f64| if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
        0.2126 * ch(c[0]) + 0.7152 * ch(c[1]) + 0.0722 * ch(c[2])
    };
    let (la, lb) = (lum(a), lum(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(colors: &str) -> String {
        format!(r#"{{"name":"Midnight","author":"me","themes":[{{"name":"Midnight Dark","mode":"dark","colors":{{{colors}}}}}]}}"#)
    }

    #[test]
    fn a_readable_theme_passes_unchanged() {
        let checked = check(&theme(r##""background":"#101418","foreground":"#e6e6e6","primary.background":"#1d4ed8","primary.foreground":"#ffffff""##)).unwrap();
        assert_eq!(checked.names, ["Midnight Dark"]);
        assert!(checked.warnings.is_empty(), "{:?}", checked.warnings);
        assert!(checked.json.contains("primary.foreground"));
    }

    #[test]
    fn unreadable_text_is_refused_or_dropped() {
        let err = check(&theme(r##""background":"#101418","foreground":"#202428""##)).unwrap_err();
        assert!(err.contains("4.5:1"), "{err}");
        let checked = check(&theme(r##""background":"#101418","foreground":"#e6e6e6","primary.background":"#777777","primary.foreground":"#888888""##)).unwrap();
        assert_eq!(checked.warnings.len(), 1);
        assert!(!checked.json.contains("primary.foreground"));
        assert!(!checked.json.contains("primary.background"));
        // Translucent text is judged as it is drawn.
        let faint = check(&theme(r##""background":"#ffffff","foreground":"#00000020""##)).unwrap_err();
        assert!(faint.contains("foreground on background"));
    }

    #[test]
    fn only_colors_and_known_shapes() {
        assert!(check(&theme(r##""background":"red","foreground":"#fff""##)).unwrap_err().contains("isn’t a #rrggbb"));
        assert!(check(&theme(r##""background":"#000","foreground":"#fff","border":"green-600","ring":"blue/50""##)).is_ok());
        assert!(check(&theme(r##""background":"#000","foreground":"#fff","border":"url(x)""##)).is_err());
        let fonts = r##"{"name":"X","themes":[{"name":"X","mode":"dark","font.family":"Comic","colors":{"background":"#000","foreground":"#fff"}}]}"##;
        assert!(check(fonts).unwrap_err().contains("font.family"));
        let orbit = r##"{"name":"X","themes":[{"name":"Orbit Dark","mode":"dark","colors":{"background":"#000","foreground":"#fff"}}]}"##;
        assert!(check(orbit).unwrap_err().contains("reserved"));
        assert!(check("[]").is_err());
        assert!(check(&"x".repeat(MAX_BYTES + 1)).is_err());
    }

    #[test]
    fn contrast_matches_wcag() {
        let black = parse_color("#000").unwrap();
        let white = parse_color("#ffffff").unwrap();
        assert!((contrast(black, white) - 21.0).abs() < 1e-6);
        assert!((contrast(white, white) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn the_bundled_themes_pass() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../orbit-app/assets/themes");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            // Bundled themes may set fonts; a plugin may not. Strip them and check the rest.
            let mut value: Value = sonic_rs::from_str(&text).unwrap();
            for t in value.get_mut("themes").unwrap().as_array_mut().unwrap().iter_mut() {
                let obj = t.as_object_mut().unwrap();
                let extra: Vec<String> = obj.iter().map(|(k, _)| k.to_owned()).filter(|k| !THEME_KEYS.contains(&k.as_str())).collect();
                for k in extra {
                    obj.remove(&k);
                }
            }
            if let Err(e) = check(&value.to_string()) {
                panic!("{}: {e}", path.display());
            }
        }
    }
}
