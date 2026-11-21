//! Minimal line icons drawn as vector strokes on a 24×24 grid (like an SVG `viewBox`).
//! No font glyphs or rasterized assets, so they stay crisp at any scale and follow the theme colors.

use egui::{Color32, Painter, Pos2, Rect, Stroke, Vec2, vec2};
use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI, TAU};

const GRID: f32 = 24.0;
const STROKE_WIDTH: f32 = 1.7;
const ARC_SEGMENTS: usize = 14;
const GRID_CENTER: Pos2 = Pos2::new(12.0, 12.0);

/// Maps grid coordinates into the target rect.
struct Canvas<'a> {
    painter: &'a Painter,
    rect: Rect,
    stroke: Stroke,
}

impl<'a> Canvas<'a> {
    fn new(painter: &'a Painter, rect: Rect, color: Color32) -> Self {
        let scale = rect.width().min(rect.height()) / GRID;
        Self {
            painter,
            rect,
            stroke: Stroke::new(STROKE_WIDTH * scale, color),
        }
    }

    fn scale(&self) -> f32 {
        self.rect.width().min(self.rect.height()) / GRID
    }

    fn map(&self, point: Pos2) -> Pos2 {
        self.rect.center() + (point - GRID_CENTER) * self.scale()
    }

    fn polyline(&self, points: &[Pos2]) {
        let mapped = points.iter().map(|&point| self.map(point)).collect();
        self.painter.add(egui::Shape::line(mapped, self.stroke));
    }
}

/// Appends an arc around `center` from `from` to `to` (radians, y points down).
fn arc_to(path: &mut Vec<Pos2>, center: Pos2, radius: f32, from: f32, to: f32) {
    for step in 0..=ARC_SEGMENTS {
        let angle = from + (to - from) * step as f32 / ARC_SEGMENTS as f32;
        path.push(center + radius * vec2(angle.cos(), angle.sin()));
    }
}

fn rotate_around_center(path: &mut [Pos2], angle: f32) {
    let (sin, cos) = angle.sin_cos();
    for point in path {
        let offset: Vec2 = *point - GRID_CENTER;
        *point = GRID_CENTER
            + vec2(
                offset.x * cos - offset.y * sin,
                offset.x * sin + offset.y * cos,
            );
    }
}

/// Paperclip: outer and inner loop of one wire, tilted 45°.
pub fn paperclip(painter: &Painter, rect: Rect, color: Color32) {
    let mut path = vec![Pos2::new(17.0, 7.0), Pos2::new(17.0, 16.0)];
    arc_to(&mut path, Pos2::new(12.5, 16.0), 4.5, 0.0, PI);
    path.push(Pos2::new(8.0, 6.0));
    arc_to(&mut path, Pos2::new(11.5, 6.0), 3.5, PI, TAU);
    path.push(Pos2::new(15.0, 15.0));
    arc_to(&mut path, Pos2::new(12.5, 15.0), 2.5, 0.0, PI);
    path.push(Pos2::new(10.0, 9.0));
    rotate_around_center(&mut path, FRAC_PI_4);
    Canvas::new(painter, rect, color).polyline(&path);
}

/// Paper plane: outline plus the fold line from the cockpit to the tip.
pub fn send_plane(painter: &Painter, rect: Rect, color: Color32) {
    let canvas = Canvas::new(painter, rect, color);
    let tip = Pos2::new(21.0, 3.0);
    let fold = Pos2::new(10.0, 13.5);
    canvas.polyline(&[tip, Pos2::new(3.0, 10.5), fold, Pos2::new(13.5, 21.0), tip]);
    canvas.polyline(&[fold, tip]);
}

/// Smiley: outline, two eyes and a smile.
pub fn smiley(painter: &Painter, rect: Rect, color: Color32) {
    const FACE_RADIUS: f32 = 9.0;
    const EYE_RADIUS: f32 = 1.0;
    const SMILE_RADIUS: f32 = 5.0;
    const SMILE_HALF_SPAN: f32 = FRAC_PI_2 * 0.75;
    let canvas = Canvas::new(painter, rect, color);

    let mut face = Vec::new();
    arc_to(&mut face, GRID_CENTER, FACE_RADIUS, 0.0, TAU);
    canvas.polyline(&face);

    for eye in [Pos2::new(9.0, 9.5), Pos2::new(15.0, 9.5)] {
        painter.circle_filled(canvas.map(eye), EYE_RADIUS * canvas.scale() * 1.2, color);
    }

    let mut smile = Vec::new();
    arc_to(
        &mut smile,
        GRID_CENTER,
        SMILE_RADIUS,
        FRAC_PI_2 - SMILE_HALF_SPAN,
        FRAC_PI_2 + SMILE_HALF_SPAN,
    );
    canvas.polyline(&smile);
}
