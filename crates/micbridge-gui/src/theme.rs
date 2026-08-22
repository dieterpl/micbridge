//! One palette, and the egui visuals derived from it.
//!
//! Every colour in the window comes from here. Before this module the same eight
//! `Color32::from_rgb(210, 80, 80)` literals were repeated across `app.rs`, which
//! meant "the warning colour" was a fact spread over eight places and nothing
//! stopped two of them from disagreeing.
//!
//! The accent is deliberately a blue that never appears on the level meter. Green,
//! amber and red already mean something specific to anyone looking at a meter — they
//! are read before any label is — so chrome borrowing one of them would make state
//! and decoration indistinguishable at a glance.

use eframe::egui::{self, Color32};

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Window background.
    pub bg: Color32,
    /// Cards and raised surfaces.
    pub panel: Color32,
    /// Inputs and wells, one step from `panel`.
    pub panel2: Color32,
    /// Hairline borders.
    pub line: Color32,
    /// Unlit meter segments and inactive tracks.
    pub dim: Color32,
    /// Primary text.
    pub ink: Color32,
    /// Secondary text and labels.
    pub muted: Color32,
    /// Interactive accent. Never used for state.
    pub accent: Color32,
    /// State: healthy.
    pub good: Color32,
    /// State: worth a look.
    pub warn: Color32,
    /// State: wrong.
    pub bad: Color32,
}

impl Palette {
    pub const DARK: Self = Self {
        bg: Color32::from_rgb(0x0F, 0x15, 0x18),
        panel: Color32::from_rgb(0x18, 0x21, 0x26),
        panel2: Color32::from_rgb(0x1F, 0x2A, 0x30),
        line: Color32::from_rgb(0x2C, 0x3A, 0x41),
        dim: Color32::from_rgb(0x22, 0x30, 0x37),
        ink: Color32::from_rgb(0xDC, 0xE6, 0xEA),
        muted: Color32::from_rgb(0x7E, 0x93, 0x9C),
        accent: Color32::from_rgb(0x3B, 0x9E, 0xEA),
        good: Color32::from_rgb(0x46, 0xB8, 0x7A),
        warn: Color32::from_rgb(0xD6, 0xA9, 0x3C),
        bad: Color32::from_rgb(0xD9, 0x52, 0x4F),
    };

    pub const LIGHT: Self = Self {
        bg: Color32::from_rgb(0xED, 0xF1, 0xF2),
        panel: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        panel2: Color32::from_rgb(0xF4, 0xF7, 0xF8),
        line: Color32::from_rgb(0xD2, 0xDB, 0xDE),
        dim: Color32::from_rgb(0xDF, 0xE6, 0xE8),
        ink: Color32::from_rgb(0x11, 0x1A, 0x1D),
        muted: Color32::from_rgb(0x5B, 0x6E, 0x75),
        accent: Color32::from_rgb(0x1B, 0x72, 0xBC),
        // Darker than the dark theme's: the same green on white is barely legible,
        // and a state colour that cannot be read is not a state colour.
        good: Color32::from_rgb(0x2E, 0x9A, 0x5F),
        warn: Color32::from_rgb(0xB4, 0x86, 0x2A),
        bad: Color32::from_rgb(0xC4, 0x40, 0x3D),
    };

    pub fn of(ctx: &egui::Context) -> Self {
        Self::for_theme(ctx.theme())
    }

    pub fn for_theme(theme: egui::Theme) -> Self {
        match theme {
            egui::Theme::Dark => Self::DARK,
            egui::Theme::Light => Self::LIGHT,
        }
    }
}

/// Builds both styles, once, at startup.
///
/// egui keeps a separate `Style` per theme and switches between them when the system
/// preference changes, so both are filled in here and the window follows the OS with
/// no per-frame check and no visible re-style.
pub fn install(ctx: &egui::Context) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let palette = Palette::for_theme(theme);
        let mut style = (*ctx.style_of(theme)).clone();
        apply(&mut style, &palette);
        ctx.set_style_of(theme, style);
    }
}

fn apply(style: &mut egui::Style, palette: &Palette) {
    let v = &mut style.visuals;

    v.panel_fill = palette.bg;
    v.window_fill = palette.bg;
    v.extreme_bg_color = palette.panel2;
    v.faint_bg_color = palette.panel2;
    v.override_text_color = Some(palette.ink);
    v.hyperlink_color = palette.accent;
    v.selection.bg_fill = palette.accent.gamma_multiply(0.35);
    v.selection.stroke = egui::Stroke::new(1.0, palette.accent);
    v.window_stroke = egui::Stroke::new(1.0, palette.line);

    let radius = egui::CornerRadius::same(6);
    for widget in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        widget.corner_radius = radius;
        widget.bg_fill = palette.panel2;
        widget.weak_bg_fill = palette.panel2;
        widget.bg_stroke = egui::Stroke::new(1.0, palette.line);
        widget.fg_stroke = egui::Stroke::new(1.0, palette.ink);
    }
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, palette.line);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, palette.muted);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, palette.accent.gamma_multiply(0.7));
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, palette.accent);

    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    style.spacing.interact_size.y = 26.0;
}

/// A small uppercase label, letter-spaced by a hair. Used for every field name and
/// tile caption, which is what keeps the two halves of the window looking related.
pub fn label(text: &str, palette: &Palette) -> egui::RichText {
    egui::RichText::new(text.to_uppercase()).size(10.0).color(palette.muted)
}

/// A monospaced value. Digits line up column to column, which matters because the
/// stats change every second and a jittering number is hard to read.
pub fn value(text: impl Into<String>, palette: &Palette) -> egui::RichText {
    egui::RichText::new(text).monospace().size(14.0).color(palette.ink)
}

/// A framed card, used for the settings block and the log.
pub fn card(palette: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.panel)
        .stroke(egui::Stroke::new(1.0, palette.line))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 11))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the module exists to guarantee: chrome and state never share a
    /// colour, so a blue thing is never a status and a green thing always is.
    #[test]
    fn the_accent_is_not_a_state_colour() {
        for palette in [Palette::DARK, Palette::LIGHT] {
            for state in [palette.good, palette.warn, palette.bad] {
                assert_ne!(palette.accent, state, "accent collides with a state colour");
            }
        }
    }

    /// A light theme built by inverting a dark one produces unreadable mid-tones.
    /// This pins that both were designed, by checking text actually contrasts with
    /// the surface it sits on.
    #[test]
    fn text_contrasts_with_its_surface_in_both_themes() {
        fn luminance(c: Color32) -> f32 {
            let f = |v: u8| {
                let v = v as f32 / 255.0;
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
        }
        fn ratio(a: Color32, b: Color32) -> f32 {
            let (x, y) = (luminance(a), luminance(b));
            let (hi, lo) = if x > y { (x, y) } else { (y, x) };
            (hi + 0.05) / (lo + 0.05)
        }

        for (name, p) in [("dark", Palette::DARK), ("light", Palette::LIGHT)] {
            assert!(ratio(p.ink, p.bg) >= 7.0, "{name}: body text is thin on the ground");
            assert!(ratio(p.muted, p.panel) >= 3.5, "{name}: labels are too faint on a card");
            assert!(ratio(p.good, p.panel) >= 2.5, "{name}: the good state is unreadable");
            assert!(ratio(p.bad, p.panel) >= 3.0, "{name}: the bad state is unreadable");
        }
    }
}
