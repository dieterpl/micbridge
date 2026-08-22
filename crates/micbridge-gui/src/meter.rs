//! The level meter, the buffer gauge, and the status pill.
//!
//! The window previously drew level with `egui::ProgressBar`. A progress bar answers
//! "how far along is this", which is the wrong question — a level meter answers "how
//! loud right now, and did it clip while I was looking somewhere else". The three
//! things that difference costs are all here: discrete segments, a peak that holds,
//! and a clip indicator that latches until acknowledged.

use eframe::egui::{self, Color32, Rect};

use crate::theme::Palette;

/// Bottom of the meter's scale.
///
/// Defined here rather than in `app.rs` because `scale` draws a tick at exactly this
/// value: two constants that had to agree were one edit away from disagreeing.
pub const FLOOR_DB: f32 = -60.0;

/// Number of segments across the meter.
///
/// Discrete blocks rather than a continuous fill because the eye lands on a position
/// far more accurately than it estimates a length. Thirty-two over a 60 dB scale puts
/// a segment roughly every 2 dB, which is about the smallest step worth showing.
const SEGMENTS: usize = 32;

/// Fraction of the scale, from the top, painted as "clipping".
const BAD_ZONE: f32 = 0.06;
/// Fraction below that painted as "hot".
const WARN_ZONE: f32 = 0.20;

/// How fast the peak marker falls, in dB per frame at the active repaint rate.
///
/// Much slower than the meter itself: the entire purpose of a peak hold is to stay
/// visible after the transient that caused it has gone.
pub const PEAK_FALL_DB: f32 = 0.35;

/// Level at which the clip indicator latches.
///
/// Not 0.0: a sample that reaches exactly full scale is already too late, and the
/// resolution of the meter cannot distinguish -0.1 dBFS from clipping anyway.
pub const CLIP_DB: f32 = -0.5;

/// Live meter state, kept by the app across frames.
#[derive(Debug, Clone, Copy)]
pub struct MeterState {
    /// Smoothed amplitude, 0..1. Rises instantly, falls gradually.
    pub level: f32,
    /// Peak in dBFS, falling slowly.
    pub peak_db: f32,
    /// Set when the signal reached full scale; stays set until acknowledged.
    pub clipped: bool,
}

impl Default for MeterState {
    fn default() -> Self {
        Self { level: 0.0, peak_db: f32::NEG_INFINITY, clipped: false }
    }
}

impl MeterState {
    /// Folds a new amplitude reading in.
    ///
    /// `decay` is the per-frame multiplier for the falling edge. Rising is immediate
    /// because a meter that lags a transient is worse than no meter: it reports a
    /// level the signal no longer has.
    pub fn observe(&mut self, amplitude: f32, decay: f32, floor_db: f32) {
        self.level = if amplitude > self.level { amplitude } else { self.level * decay };

        let db = micbridge_engine::state::level_dbfs(self.level, floor_db);
        if self.level > 0.0 {
            if db > self.peak_db {
                self.peak_db = db;
            } else {
                self.peak_db -= PEAK_FALL_DB;
            }
            if db >= CLIP_DB {
                self.clipped = true;
            }
        }
        if self.peak_db < floor_db {
            self.peak_db = floor_db;
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Maps dBFS onto the meter's 0..1 travel.
///
/// Linear in decibels, not in amplitude: a linear bar spends nearly all its travel in
/// the top few dB and shows nothing at all for quiet speech.
pub fn fraction(db: f32, floor_db: f32) -> f32 {
    ((db - floor_db) / -floor_db).clamp(0.0, 1.0)
}

/// Colour for a segment at `position` up the scale.
fn zone(position: f32, palette: &Palette) -> Color32 {
    if position > 1.0 - BAD_ZONE {
        palette.bad
    } else if position > 1.0 - BAD_ZONE - WARN_ZONE {
        palette.warn
    } else {
        palette.good
    }
}

/// Draws the segmented meter. Returns the rect it occupied.
pub fn meter(ui: &mut egui::Ui, palette: &Palette, db: f32, peak_db: f32, floor_db: f32) -> Rect {
    let height = 18.0;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return rect;
    }

    let painter = ui.painter();
    let gap = 2.0;
    let step = (rect.width() + gap) / SEGMENTS as f32;
    let width = (step - gap).max(1.0);

    let lit = (fraction(db, floor_db) * SEGMENTS as f32).round() as usize;
    // The peak sits *on* a segment, so it is an index rather than a position; below
    // the floor it has nowhere to sit and is not drawn at all.
    let peak = if peak_db > floor_db {
        Some(((fraction(peak_db, floor_db) * SEGMENTS as f32).round() as usize).clamp(1, SEGMENTS))
    } else {
        None
    };

    for i in 0..SEGMENTS {
        let x = rect.left() + i as f32 * step;
        let seg = Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(width, rect.height()));
        let position = (i + 1) as f32 / SEGMENTS as f32;

        let colour = if peak == Some(i + 1) {
            palette.ink
        } else if i < lit {
            zone(position, palette)
        } else {
            palette.dim
        };
        painter.rect_filled(seg, egui::CornerRadius::same(1), colour);
    }
    rect
}

/// The dB scale under the meter.
pub fn scale(ui: &mut egui::Ui, palette: &Palette, floor_db: f32) {
    const TICKS: [f32; 6] = [FLOOR_DB, -40.0, -20.0, -12.0, -6.0, 0.0];

    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 11.0), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter();
    for db in TICKS {
        let t = fraction(db, floor_db);
        // Nudge the end labels inward so neither runs off the edge of the window.
        let anchor = if t <= 0.01 {
            egui::Align2::LEFT_TOP
        } else if t >= 0.99 {
            egui::Align2::RIGHT_TOP
        } else {
            egui::Align2::CENTER_TOP
        };
        painter.text(
            egui::pos2(rect.left() + t * rect.width(), rect.top()),
            anchor,
            format!("{db:.0}"),
            egui::FontId::monospace(9.0),
            palette.muted,
        );
    }
}

/// The clip badge. Clicking it clears the latch; returns true when clicked.
pub fn clip_badge(ui: &mut egui::Ui, palette: &Palette, lit: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(38.0, 17.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let (fill, stroke, text) = if lit {
            (palette.bad, palette.bad, Color32::WHITE)
        } else {
            (palette.panel2, palette.line, palette.muted)
        };
        let painter = ui.painter();
        painter.rect_filled(rect, egui::CornerRadius::same(4), fill);
        painter.rect_stroke(
            rect,
            egui::CornerRadius::same(4),
            egui::Stroke::new(1.0, stroke),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "CLIP",
            egui::FontId::proportional(9.0),
            text,
        );
    }
    if lit {
        response.clone().on_hover_text("Clipped since the session started — click to clear");
    }
    response.clicked()
}

/// A rounded status pill with a coloured dot.
pub fn pill(ui: &mut egui::Ui, palette: &Palette, text: &str, colour: Color32) {
    let font = egui::FontId::proportional(11.0);
    let galley = ui.painter().layout_no_wrap(text.to_uppercase(), font.clone(), colour);
    let size = egui::vec2(galley.size().x + 30.0, 20.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }

    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(10), palette.panel);
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(10),
        egui::Stroke::new(1.0, colour.gamma_multiply(0.55)),
        egui::StrokeKind::Inside,
    );
    painter.circle_filled(egui::pos2(rect.left() + 11.0, rect.center().y), 3.5, colour);
    painter.galley(
        egui::pos2(rect.left() + 21.0, rect.center().y - galley.size().y / 2.0),
        galley,
        colour,
    );
}

/// The jitter buffer against its target: fill is actual, the tick is the target.
///
/// Turns "buffer 19.4 ms" into something readable without arithmetic — which matters,
/// because a buffer walking away from its target is the first visible sign that drift
/// correction is losing.
pub fn buffer_gauge(ui: &mut egui::Ui, palette: &Palette, fill_ms: f32, target_ms: f32) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 9.0), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    // Full scale is twice the target, so an on-target buffer sits at the midpoint and
    // the tick is always in the same place regardless of what the target is set to.
    let full = (target_ms * 2.0).max(1.0);
    let t = (fill_ms / full).clamp(0.0, 1.0);

    // Amber below half the target: still playing, but with little margin left.
    let colour = if fill_ms < target_ms * 0.5 { palette.warn } else { palette.accent };

    let painter = ui.painter();
    painter.rect_filled(rect, egui::CornerRadius::same(3), palette.dim);
    if t > 0.0 {
        let filled = Rect::from_min_size(rect.min, egui::vec2(rect.width() * t, rect.height()));
        painter.rect_filled(filled, egui::CornerRadius::same(3), colour);
    }
    let x = rect.left() + rect.width() * 0.5;
    painter.rect_filled(
        Rect::from_min_size(
            egui::pos2(x - 1.0, rect.top() - 2.0),
            egui::vec2(2.0, rect.height() + 4.0),
        ),
        egui::CornerRadius::ZERO,
        palette.ink,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOOR: f32 = FLOOR_DB;

    /// Carried over from the ProgressBar implementation, because the mapping was
    /// already right and the new meter must not quietly change it.
    #[test]
    fn the_scale_maps_dbfs_onto_the_bar() {
        assert!((fraction(0.0, FLOOR) - 1.0).abs() < 1e-6, "full scale fills the bar");
        assert!((fraction(FLOOR, FLOOR)).abs() < 1e-6, "the floor empties it");
        assert!((fraction(-30.0, FLOOR) - 0.5).abs() < 1e-6, "halfway in dB is halfway across");
        assert_eq!(
            fraction(-200.0, FLOOR),
            0.0,
            "below the floor clamps rather than going negative"
        );
    }

    #[test]
    fn the_meter_rises_instantly_and_falls_gradually() {
        let mut m = MeterState::default();
        m.observe(0.8, 0.90, FLOOR);
        assert_eq!(m.level, 0.8, "a rise is immediate");

        let before = m.level;
        m.observe(0.0, 0.90, FLOOR);
        assert!(m.level < before && m.level > 0.5, "a fall is gradual, got {}", m.level);

        for _ in 0..500 {
            m.observe(0.0, 0.90, FLOOR);
        }
        assert!(m.level < 1e-6, "the meter should decay away, left at {}", m.level);
    }

    /// The reason a peak hold exists: a transient gone in one frame must still be
    /// readable several frames later.
    #[test]
    fn the_peak_outlives_the_signal_that_set_it() {
        let mut m = MeterState::default();
        m.observe(1.0, 0.90, FLOOR);
        let peak = m.peak_db;
        assert!(peak > -1.0, "a full-scale hit should peak near 0 dB, got {peak}");

        m.observe(0.0, 0.90, FLOOR);
        assert!(m.peak_db > peak - 1.0, "the peak fell too fast");

        // And it does eventually come back down rather than sticking forever.
        for _ in 0..1000 {
            m.observe(0.0, 0.90, FLOOR);
        }
        assert!(m.peak_db <= FLOOR + 1e-3, "the peak should settle to the floor");
    }

    /// The failure a latch prevents: clipping that happened while nobody was
    /// watching leaves no trace, and the recording is already ruined.
    #[test]
    fn clipping_latches_until_it_is_acknowledged() {
        let mut m = MeterState::default();
        m.observe(0.5, 0.90, FLOOR);
        assert!(!m.clipped, "half scale is not clipping");

        m.observe(1.0, 0.90, FLOOR);
        assert!(m.clipped, "full scale should latch");

        for _ in 0..2000 {
            m.observe(0.0, 0.90, FLOOR);
        }
        assert!(m.clipped, "the latch must survive the signal going away");

        m.reset();
        assert!(!m.clipped, "reset is the only thing that clears it");
    }

    /// Silence must not latch a clip, and must not park the peak at the top.
    #[test]
    fn silence_leaves_the_meter_alone() {
        let mut m = MeterState::default();
        for _ in 0..100 {
            m.observe(0.0, 0.90, FLOOR);
        }
        assert!(!m.clipped);
        assert_eq!(m.level, 0.0);
        assert!(m.peak_db <= FLOOR, "peak should not rise out of silence");
    }

    #[test]
    fn zones_run_green_then_amber_then_red_up_the_scale() {
        let p = Palette::DARK;
        assert_eq!(zone(0.5, &p), p.good, "mid scale is healthy");
        assert_eq!(zone(0.85, &p), p.warn, "approaching full scale is a warning");
        assert_eq!(zone(1.0, &p), p.bad, "the top of the scale is clipping");
    }
}
