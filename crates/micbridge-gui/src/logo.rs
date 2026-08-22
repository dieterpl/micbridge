//! The micbridge mark, drawn with the painter.
//!
//! The geometry is a copy of `BARS` in `scripts/render-logo.py`, which generates the
//! SVG, the PNGs, the `.ico` and the `.icns`. Drawing it here rather than embedding a
//! bitmap is what keeps the window's mark identical to the one on the README and in
//! the Dock: there is one shape, expressed twice, and the test below pins the copy to
//! the original.
//!
//! Five bars rising to a centre peak — a level meter whose silhouette is also a span.

use eframe::egui::{self, Color32, Rect};

/// `(x, y, width, height)` in the 64x64 design space, matching `scripts/render-logo.py`.
const BARS: [(f32, f32, f32, f32); 5] = [
    (11.0, 38.0, 6.5, 13.0),
    (21.0, 30.0, 6.5, 21.0),
    (28.75, 22.0, 6.5, 29.0),
    (36.5, 30.0, 6.5, 21.0),
    (46.5, 38.0, 6.5, 13.0),
];

/// The design space the constants above are expressed in.
const DESIGN: f32 = 64.0;

/// Paints the mark to fill `rect`, preserving its square aspect.
pub fn paint(painter: &egui::Painter, rect: Rect, color: Color32) {
    let side = rect.width().min(rect.height());
    let scale = side / DESIGN;
    let origin = rect.center() - egui::vec2(side / 2.0, side / 2.0);

    for (x, y, w, h) in BARS {
        let min = origin + egui::vec2(x * scale, y * scale);
        let bar = Rect::from_min_size(min, egui::vec2(w * scale, h * scale));
        // Fully rounded ends: the radius is half the bar width, so a bar is a
        // capsule at every size rather than a rectangle with softened corners.
        let radius = (w * scale / 2.0).round().max(1.0) as u8;
        painter.rect_filled(bar, egui::CornerRadius::same(radius), color);
    }
}

/// Allocates `size` square and paints the mark into it.
pub fn ui(ui: &mut egui::Ui, size: f32, color: Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        paint(ui.painter(), rect, color);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mark is drawn twice — here and in `scripts/render-logo.py` — so the two
    /// have to be checked against each other. Reading the script is what makes this
    /// a real check rather than a restatement of the constant.
    #[test]
    fn the_geometry_matches_the_asset_generator() {
        let script = include_str!("../../../scripts/render-logo.py");
        // Parsed as numbers rather than compared as text: Rust prints an f32 11.0
        // as "11" and Python writes "11.0", so a string comparison fails on two
        // spellings of the same geometry and says nothing about the shape.
        let bars: Vec<Vec<f32>> = script
            .lines()
            .skip_while(|line| !line.starts_with("BARS = ["))
            .skip(1)
            .take_while(|line| !line.starts_with(']'))
            .filter(|line| line.trim_start().starts_with('('))
            .map(|line| {
                line.trim()
                    .trim_end_matches(',')
                    .trim_matches(['(', ')'])
                    .split(',')
                    .map(|n| n.trim().parse::<f32>().expect("a number in BARS"))
                    .collect()
            })
            .collect();

        assert_eq!(bars.len(), BARS.len(), "bar count differs from the generator");
        for (from_script, &(x, y, w, h)) in bars.iter().zip(BARS.iter()) {
            assert_eq!(
                from_script.as_slice(),
                [x, y, w, h].as_slice(),
                "the painter and scripts/render-logo.py disagree about a bar"
            );
        }
    }

    /// The silhouette is the whole idea: outer bars short, inner taller, centre
    /// tallest, and symmetric about the middle.
    #[test]
    fn the_bars_form_a_symmetric_arch() {
        let heights: Vec<f32> = BARS.iter().map(|&(_, _, _, h)| h).collect();
        assert!(heights[0] < heights[1] && heights[1] < heights[2], "should rise to the centre");
        assert_eq!(heights[0], heights[4], "outer pair should match");
        assert_eq!(heights[1], heights[3], "inner pair should match");

        let centre = BARS[2].0 + BARS[2].2 / 2.0;
        assert!((centre - DESIGN / 2.0).abs() < 0.01, "the centre bar is off-centre");
    }
}
