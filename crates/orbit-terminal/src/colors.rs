//! Terminal colors derived from the active GPUI Kit theme.
//!
//! The 16 ANSI colors come from the theme's base hues (`red`, `red_light`, …)
//! and surfaces, so a terminal matches every light, dark or custom theme.
//! The 256-color cube, the grayscale ramp and truecolor are color *data*
//! sent by the program and are converted as such. A program may override
//! palette entries with OSC 4/10/11; those overrides win.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use gpui_kit::component::Theme;
use gpui_kit::{Hsla, Rgba};

/// How much a dim (SGR 2) foreground keeps of its color over the background.
const DIM_OPACITY: f32 = 0.66;
/// The least WCAG contrast ratio text keeps against its background. Programs
/// pick colors for a dark terminal (PSReadLine's yellow commands, white
/// parameters); on a light theme they would be unreadable without this.
const MIN_CONTRAST: f32 = 3.0;

/// The resolved palette for one frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    ansi: [Hsla; 16],
    pub foreground: Hsla,
    pub background: Hsla,
    pub cursor: Hsla,
    pub selection: Hsla,
    pub link: Hsla,
}

impl Palette {
    pub fn from_theme(theme: &Theme) -> Self {
        let fg = theme.foreground;
        let bg = theme.background;
        let mix = |amount: f32| bg.blend(fg.opacity(amount));
        let (black, white, bright_white) =
            if theme.is_dark() { (mix(0.18), mix(0.82), fg) } else { (fg, mix(0.12), mix(0.04)) };
        let ansi = [
            black,
            theme.red,
            theme.green,
            theme.yellow,
            theme.blue,
            theme.magenta,
            theme.cyan,
            white,
            theme.muted_foreground,
            theme.red_light,
            theme.green_light,
            theme.yellow_light,
            theme.blue_light,
            theme.magenta_light,
            theme.cyan_light,
            bright_white,
        ];
        Self { ansi, foreground: fg, background: bg, cursor: theme.caret, selection: theme.selection, link: theme.link }
    }

    /// Resolves a cell color. `overrides` are the program's OSC palette
    /// changes; `is_foreground` picks the default for `Named(Foreground)`.
    pub fn resolve(&self, color: Color, overrides: &Colors) -> Hsla {
        match color {
            Color::Spec(rgb) => rgb_to_hsla(rgb),
            Color::Indexed(ix) => self.indexed(ix as usize, overrides),
            Color::Named(named) => self.named(named, overrides),
        }
    }

    fn indexed(&self, ix: usize, overrides: &Colors) -> Hsla {
        if let Some(rgb) = overrides[ix] {
            return rgb_to_hsla(rgb);
        }
        match ix {
            0..16 => self.ansi[ix],
            16..232 => {
                let ix = ix - 16;
                let level = |v: usize| if v == 0 { 0 } else { (v * 40 + 55) as u8 };
                rgb_to_hsla(Rgb { r: level(ix / 36), g: level((ix / 6) % 6), b: level(ix % 6) })
            }
            232..256 => {
                let v = (8 + (ix - 232) * 10) as u8;
                rgb_to_hsla(Rgb { r: v, g: v, b: v })
            }
            _ => self.foreground,
        }
    }

    fn named(&self, named: NamedColor, overrides: &Colors) -> Hsla {
        if let Some(rgb) = overrides[named] {
            return rgb_to_hsla(rgb);
        }
        let ix = named as usize;
        match named {
            NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
            NamedColor::Background => self.background,
            NamedColor::Cursor => self.cursor,
            NamedColor::DimForeground => self.dim(self.foreground),
            NamedColor::DimBlack
            | NamedColor::DimRed
            | NamedColor::DimGreen
            | NamedColor::DimYellow
            | NamedColor::DimBlue
            | NamedColor::DimMagenta
            | NamedColor::DimCyan
            | NamedColor::DimWhite => {
                let base = ix - NamedColor::DimBlack as usize;
                self.dim(self.indexed(base, overrides))
            }
            _ if ix < 16 => self.indexed(ix, overrides),
            _ => self.foreground,
        }
    }

    /// A dim (faint) version of `color` over the terminal background.
    pub fn dim(&self, color: Hsla) -> Hsla {
        self.background.blend(color.opacity(DIM_OPACITY))
    }

    /// `fg`, darkened or lightened just enough to read on `bg`. Text the
    /// program deliberately hid (same color as its background) stays hidden.
    pub fn readable(fg: Hsla, bg: Hsla) -> Hsla {
        if fg == bg || contrast(fg, bg) >= MIN_CONTRAST {
            return fg;
        }
        let bg_luminance = luminance(bg);
        // Move away from the background's lightness, keeping hue and saturation.
        let toward_dark = bg_luminance > 0.18;
        let (mut lo, mut hi) = if toward_dark { (0.0, fg.l) } else { (fg.l, 1.0) };
        for _ in 0..12 {
            let mid = (lo + hi) / 2.;
            let candidate = Hsla { l: mid, ..fg };
            let ok = contrast(candidate, bg) >= MIN_CONTRAST;
            match (toward_dark, ok) {
                (true, true) | (false, false) => lo = mid,
                (true, false) | (false, true) => hi = mid,
            }
        }
        Hsla { l: if toward_dark { lo } else { hi }, ..fg }
    }

    /// Whether `color` is the default background (and needs no paint).
    pub fn is_default_background(color: Color) -> bool {
        matches!(color, Color::Named(NamedColor::Background))
    }

    /// The color for an ANSI index 0–15, for OSC 4 queries.
    pub fn rgb_for_index(&self, ix: usize, overrides: &Colors) -> Rgb {
        let color = match ix {
            256 => self.foreground,
            257 => self.background,
            258 => self.cursor,
            ix if ix < 256 => self.indexed(ix, overrides),
            _ => self.foreground,
        };
        hsla_to_rgb(color)
    }
}

/// WCAG relative luminance.
fn luminance(color: Hsla) -> f32 {
    let rgba = color.to_rgb();
    let channel = |c: f32| if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
    0.2126 * channel(rgba.r) + 0.7152 * channel(rgba.g) + 0.0722 * channel(rgba.b)
}

/// WCAG contrast ratio, 1–21.
fn contrast(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn rgb_to_hsla(rgb: Rgb) -> Hsla {
    Rgba { r: rgb.r as f32 / 255., g: rgb.g as f32 / 255., b: rgb.b as f32 / 255., a: 1. }.into()
}

fn hsla_to_rgb(color: Hsla) -> Rgb {
    let rgba = color.to_rgb();
    let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    Rgb { r: byte(rgba.r), g: byte(rgba.g), b: byte(rgba.b) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette::from_theme(&Theme::default())
    }

    #[test]
    fn ansi_colors_come_from_the_theme() {
        let theme = Theme::default();
        let p = palette();
        let none = Colors::default();
        assert_eq!(p.resolve(Color::Named(NamedColor::Red), &none), theme.red);
        assert_eq!(p.resolve(Color::Indexed(9), &none), theme.red_light);
        assert_eq!(p.resolve(Color::Named(NamedColor::Foreground), &none), theme.foreground);
        assert_eq!(p.resolve(Color::Named(NamedColor::Background), &none), theme.background);
    }

    #[test]
    fn cube_grayscale_and_truecolor() {
        let p = palette();
        let none = Colors::default();
        let rgb = |c: Color| hsla_to_rgb(p.resolve(c, &none));
        assert_eq!(rgb(Color::Indexed(16)), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(rgb(Color::Indexed(196)), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(rgb(Color::Indexed(231)), Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(rgb(Color::Indexed(232)), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(rgb(Color::Spec(Rgb { r: 12, g: 200, b: 99 })), Rgb { r: 12, g: 200, b: 99 });
    }

    #[test]
    fn text_stays_readable() {
        let white = rgb_to_hsla(Rgb { r: 255, g: 255, b: 255 });
        let black = rgb_to_hsla(Rgb { r: 0, g: 0, b: 0 });
        let pale_yellow = rgb_to_hsla(Rgb { r: 250, g: 230, b: 120 });
        let fixed = Palette::readable(pale_yellow, white);
        assert!(contrast(fixed, white) >= MIN_CONTRAST - 0.05, "{}", contrast(fixed, white));
        assert!((fixed.h - pale_yellow.h).abs() < 1e-3);
        let dark_blue = rgb_to_hsla(Rgb { r: 10, g: 10, b: 60 });
        assert!(contrast(Palette::readable(dark_blue, black), black) >= MIN_CONTRAST - 0.05);
        // Readable colors and deliberately hidden text are left alone.
        assert_eq!(Palette::readable(black, white), black);
        assert_eq!(Palette::readable(white, white), white);
    }

    #[test]
    fn program_overrides_win() {
        let p = palette();
        let mut colors = Colors::default();
        colors[1] = Some(Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(hsla_to_rgb(p.resolve(Color::Named(NamedColor::Red), &colors)), Rgb { r: 1, g: 2, b: 3 });
    }
}
