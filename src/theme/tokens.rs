//! Colour tokens derived from a theme's colours (docs/DESIGN.md §2.2). `ThemeColors` stays the
//! TOML contract; everything else is computed here, so custom themes get every token, and the
//! state colours (green on, straw idle, purple live) stay fixed per mode, as in Discord.

use super::apply::{is_high_contrast, luminance, mix};
use super::{Rgba, ThemeColors};
use egui::Color32;

/// Which set of fixed state colours a theme gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
    HighContrast,
}

impl Mode {
    pub fn of(c: &ThemeColors) -> Self {
        if is_high_contrast(c) {
            Self::HighContrast
        } else if luminance(c.background) > 0.4 {
            Self::Light
        } else {
            Self::Dark
        }
    }

    /// Opacity of `text` in the `selected` fill (secondary buttons add 4 / 8 % on hover / press).
    pub fn selected_alpha(self) -> f32 {
        match self {
            Self::Dark => 0.10,
            Self::Light => 0.08,
            Self::HighContrast => 0.18,
        }
    }
}

/// Every hue has one job (§1): ember is `ThemeColors::accent`, the rest live here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tokens {
    pub mode: Mode,
    /// Floating layers: menus, popouts, modals, tooltips, pickers, autocomplete.
    pub raised: Color32,
    /// Row and button hover. Translucent, so it works on gradients.
    pub hover: Color32,
    /// Selected row, selected segment, toggled icon button, secondary button.
    pub selected: Color32,
    /// Hairlines: list rows, menu separators, float edges.
    pub rule: Color32,
    /// Component boundaries (>= 3:1): input border, switch-off track, checkbox and radio rings.
    pub edge: Color32,
    /// Keyboard focus ring.
    pub focus: Color32,
    /// Online, voice connected, speaking, switches and checkboxes on, my activity icon.
    pub positive: Color32,
    /// Green text ("Voice connected").
    pub positive_text: Color32,
    /// Check and knob glyph drawn on `positive`.
    pub on_positive: Color32,
    /// Join / Accept call buttons, with `positive_label` on them.
    pub positive_fill: Color32,
    pub positive_label: Color32,
    /// Messages that mention me (bar), idle status.
    pub attention: Color32,
    /// Streaming status.
    pub live: Color32,
    /// Destructive buttons, count badges, "New" and "Live" tags, danger menu hover, with
    /// `danger_label` on them.
    pub danger_fill: Color32,
    pub danger_label: Color32,
    /// Mention and channel chips, app tags.
    pub chip: Color32,
    /// Selected text.
    pub selection: Color32,
    /// Heat bar: cold end, body, white-hot core (flat accent on light and high contrast).
    pub heat: [Color32; 3],
    /// Behind modals.
    pub scrim: Color32,
}

/// Straw (`attention`) of the dark and light modes, for gradient themes' mention tint.
pub(super) const ATTENTION: (Rgba, Rgba) = (Rgba::rgb(0xeb, 0xc1, 0x66), Rgba::rgb(0xa8, 0x6b, 0x00));

impl Tokens {
    pub fn new(c: &ThemeColors) -> Self {
        let mode = Mode::of(c);
        let k = |d: f32, l: f32, h: f32| match mode {
            Mode::Dark => d,
            Mode::Light => l,
            Mode::HighContrast => h,
        };
        let pick = |d: u32, l: u32, h: u32| hex(match mode {
            Mode::Dark => d,
            Mode::Light => l,
            Mode::HighContrast => h,
        });
        let at = |col: Rgba, a: f32| col.to_color32().gamma_multiply(a);
        let hc = mode == Mode::HighContrast;
        let accent = c.accent.to_color32();
        Self {
            mode,
            raised: if mode == Mode::Light { hex(0xffffff) } else { c.surface.to_color32() },
            hover: at(c.text, k(0.06, 0.06, 0.12)),
            selected: at(c.text, mode.selected_alpha()),
            rule: if hc { at(c.text_muted, 0.45) } else { at(c.text, k(0.10, 0.12, 0.0)) },
            edge: mix(c.background, c.text_muted, if hc { 0.8 } else { 0.75 }),
            focus: if hc { accent } else { c.link.to_color32() },
            positive: pick(0x50c76a, 0x1d8b3d, 0x4fe07a),
            positive_text: pick(0x50c76a, 0x00792b, 0x4fe07a),
            on_positive: pick(0x151a1e, 0xffffff, 0x000000),
            positive_fill: pick(0x25803f, 0x25803f, 0x4fe07a),
            positive_label: pick(0xffffff, 0xffffff, 0x000000),
            attention: match mode {
                Mode::Dark => ATTENTION.0.to_color32(),
                Mode::Light => ATTENTION.1.to_color32(),
                Mode::HighContrast => hex(0xffd400),
            },
            live: pick(0xa682e1, 0x7a4aba, 0xc9a2ff),
            danger_fill: pick(0xcc3d3f, 0xcc2827, 0xff5c5c),
            danger_label: pick(0xffffff, 0xffffff, 0x000000),
            chip: at(c.link, k(0.16, 0.12, 0.22)),
            selection: at(c.link, k(0.35, 0.22, 0.35)),
            heat: if mode == Mode::Dark { [hex(0xa42d15), accent, hex(0xfeeec1)] } else { [accent; 3] },
            scrim: Color32::from_black_alpha((255.0 * k(0.6, 0.4, 0.8)).round() as u8),
        }
    }
}

/// `0xrrggbb` through `Rgba::to_color32`, so the Saturation setting applies.
fn hex(rgb: u32) -> Color32 {
    let [_, r, g, b] = rgb.to_be_bytes();
    Rgba::rgb(r, g, b).to_color32()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn ratio(a: Color32, b: Color32) -> f32 {
        let rgba = |c: Color32| Rgba([c.r(), c.g(), c.b(), 255]);
        super::super::apply::contrast_ratio(rgba(a), rgba(b))
    }

    #[test]
    fn builtins_get_their_mode_and_readable_states() {
        let modes: Vec<Mode> = Theme::builtins()[..3].iter().map(|t| Mode::of(&t.colors)).collect();
        assert_eq!(modes, [Mode::Dark, Mode::Light, Mode::HighContrast]);
        for t in &Theme::builtins()[..3] {
            let (c, tk) = (&t.colors, Tokens::new(&t.colors));
            let bg = c.background.to_color32();
            // Component boundaries and state glyphs (the switch check): at least 3:1 (WCAG 1.4.11).
            assert!(ratio(tk.edge, bg) >= 3.0 && ratio(tk.edge, c.surface.to_color32()) >= 3.0, "{} edge", t.name);
            assert!(ratio(tk.positive, bg) >= 3.0 && ratio(tk.on_positive, tk.positive) >= 3.0, "{} positive", t.name);
            assert!(ratio(tk.danger_label, tk.danger_fill) >= 4.5 && ratio(tk.positive_label, tk.positive_fill) >= 4.5, "{} fills", t.name);
            assert!(ratio(c.danger.to_color32(), tk.raised) >= 4.5, "{} danger on menus", t.name);
            assert!(ratio(tk.focus, c.sidebar.to_color32()) >= 3.0, "{} focus", t.name);
        }
        // The spec's value for Ember Dark's edge (mix of background and muted text).
        assert_eq!(Tokens::new(&Theme::ember_dark().colors).edge, Color32::from_rgb(0x89, 0x91, 0x98));
    }
}
