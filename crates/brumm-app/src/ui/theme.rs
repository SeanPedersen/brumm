//! Visual identity: colors and spacing shared across screens.

use egui::{Color32, CornerRadius, Margin};

pub const ACCENT: Color32 = Color32::from_rgb(0xFF, 0xB0, 0x20);
pub const OUTGOING_BUBBLE: Color32 = Color32::from_rgb(0x3A, 0x2E, 0x12);
pub const INCOMING_BUBBLE: Color32 = Color32::from_rgb(0x2A, 0x2D, 0x34);
pub const MUTED_TEXT: Color32 = Color32::from_gray(150);
pub const ERROR_FILL: Color32 = Color32::from_rgb(0x6B, 0x1E, 0x1E);

pub const BUBBLE_RADIUS: CornerRadius = CornerRadius::same(12);
pub const BUBBLE_MARGIN: Margin = Margin::symmetric(10, 6);
pub const BUBBLE_MAX_WIDTH_FRACTION: f32 = 0.7;

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.visuals.selection.bg_fill = ACCENT.linear_multiply(0.35);
        style.visuals.hyperlink_color = ACCENT;
    });
}
