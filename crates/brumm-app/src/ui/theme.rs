//! Visual identity modelled on Telegram Desktop's "Night" theme: palette, metrics and the
//! global egui style shared by every screen.

use egui::{Color32, CornerRadius, FontId, TextStyle};

pub const SIDEBAR_BG: Color32 = Color32::from_rgb(0x17, 0x21, 0x2B);
pub const CHAT_BG: Color32 = Color32::from_rgb(0x0E, 0x16, 0x21);
pub const INPUT_BG: Color32 = Color32::from_rgb(0x24, 0x2F, 0x3D);
pub const ROW_HOVER: Color32 = Color32::from_rgb(0x20, 0x2B, 0x36);
pub const ROW_SELECTED: Color32 = Color32::from_rgb(0x2B, 0x52, 0x78);
pub const ACCENT: Color32 = Color32::from_rgb(0x52, 0x88, 0xC1);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x4A, 0x7B, 0xB0);
pub const ACCENT_LIGHT: Color32 = Color32::from_rgb(0x6A, 0xB2, 0xF2);
pub const OUTGOING_BUBBLE: Color32 = Color32::from_rgb(0x2B, 0x52, 0x78);
pub const INCOMING_BUBBLE: Color32 = Color32::from_rgb(0x18, 0x25, 0x33);
pub const TEXT: Color32 = Color32::from_rgb(0xF5, 0xF5, 0xF5);
pub const MUTED_TEXT: Color32 = Color32::from_rgb(0x6D, 0x7F, 0x8F);
pub const MUTED_TEXT_ON_SELECTED: Color32 = Color32::from_rgb(0xB5, 0xCC, 0xE0);
pub const OUTGOING_META: Color32 = Color32::from_rgb(0x7D, 0xA8, 0xD4);
pub const SERVICE_PILL: Color32 = Color32::from_rgba_premultiplied(0x10, 0x18, 0x22, 0xB0);
/// Corner radius of a system line ("Bob joined"); it may wrap, so not fully round.
pub const SYSTEM_PILL_RADIUS: f32 = 11.0;
pub const DANGER: Color32 = Color32::from_rgb(0xEC, 0x66, 0x6D);
pub const ERROR_FILL: Color32 = Color32::from_rgb(0x6B, 0x1E, 0x1E);

pub const AVATAR_COLORS: [Color32; 7] = [
    Color32::from_rgb(0xE1, 0x70, 0x76),
    Color32::from_rgb(0x65, 0xAA, 0xDD),
    Color32::from_rgb(0xA6, 0x95, 0xE7),
    Color32::from_rgb(0xEE, 0x7F, 0xAB),
    Color32::from_rgb(0x6E, 0xC9, 0xCB),
    Color32::from_rgb(0xFA, 0xA7, 0x74),
    Color32::from_rgb(0x7B, 0xC8, 0x62),
];

pub const SIDEBAR_WIDTH: f32 = 340.0;
pub const SIDEBAR_MIN_WIDTH: f32 = 240.0;
pub const SIDEBAR_MAX_WIDTH: f32 = 480.0;
pub const CONTACT_ROW_HEIGHT: f32 = 68.0;
pub const CONTACT_AVATAR_SIZE: f32 = 50.0;
pub const HEADER_AVATAR_SIZE: f32 = 40.0;
pub const TOP_BAR_HEIGHT: f32 = 56.0;
pub const BACK_BUTTON_SIZE: f32 = 40.0;

pub const BUBBLE_RADIUS: u8 = 16;
pub const BUBBLE_TAIL_RADIUS: u8 = 4;
pub const BUBBLE_PADDING: egui::Vec2 = egui::vec2(12.0, 7.0);
pub const BUBBLE_MAX_WIDTH: f32 = 460.0;
pub const GROUP_GAP: f32 = 2.0;
pub const SENDER_CHANGE_GAP: f32 = 8.0;
pub const INPUT_RADIUS: CornerRadius = CornerRadius::same(22);
/// Scrollbar thumb never shrinks below this, so a long history stays grabbable.
pub const SCROLLBAR_MIN_LENGTH: f32 = 48.0;
/// Width of the thumb while the pointer is over the list but not the bar.
pub const SCROLLBAR_REST_WIDTH: f32 = 8.0;
/// Width of the thumb while it is hovered or dragged.
pub const SCROLLBAR_HOVER_WIDTH: f32 = 12.0;

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    // Before any other font provider: the color emoji provider appends to the bold family too.
    crate::ui::markdown::install_fonts(ctx);
    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(14.0, 7.0);
        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(15.0));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(15.0));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(12.0));
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(20.0));
        style.spacing.scroll.handle_min_length = SCROLLBAR_MIN_LENGTH;
        style.spacing.scroll.floating_width = SCROLLBAR_REST_WIDTH;
        style.spacing.scroll.bar_width = SCROLLBAR_HOVER_WIDTH;

        let visuals = &mut style.visuals;
        visuals.panel_fill = CHAT_BG;
        visuals.window_fill = SIDEBAR_BG;
        visuals.extreme_bg_color = INPUT_BG;
        visuals.window_corner_radius = CornerRadius::same(12);
        visuals.menu_corner_radius = CornerRadius::same(10);
        visuals.window_stroke = egui::Stroke::new(1.0, Color32::from_rgb(0x0B, 0x12, 0x1A));
        visuals.selection.bg_fill = ACCENT.linear_multiply(0.5);
        visuals.hyperlink_color = ACCENT_LIGHT;
        visuals.widgets.noninteractive.fg_stroke.color = TEXT;
        visuals.widgets.noninteractive.bg_stroke =
            egui::Stroke::new(1.0, Color32::from_rgb(0x0B, 0x12, 0x1A));
        for widget in [
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
        ] {
            widget.corner_radius = CornerRadius::same(8);
            widget.fg_stroke.color = TEXT;
        }
        visuals.widgets.inactive.weak_bg_fill = INPUT_BG;
        visuals.widgets.inactive.bg_fill = INPUT_BG;
        visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2E, 0x3C, 0x4D);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x2E, 0x3C, 0x4D);
        visuals.widgets.active.weak_bg_fill = ROW_SELECTED;
        visuals.widgets.active.bg_fill = ROW_SELECTED;
    });
}

/// Filled accent button, used for the one primary action of a screen.
pub fn primary_button(label: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(label).color(Color32::WHITE).strong())
        .fill(ACCENT)
        .corner_radius(CornerRadius::same(8))
}

/// Destructive confirmation button (delete).
pub fn danger_button(label: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(label).color(Color32::WHITE).strong())
        .fill(DANGER)
        .corner_radius(CornerRadius::same(8))
}

/// Gap between a dialog and the screen edges on a narrow screen.
const DIALOG_SCREEN_MARGIN: f32 = 8.0;

/// A dialog window that fits the screen: `width` on desktop, the screen width (minus a margin)
/// on a phone, anchored at the top so the on-screen keyboard does not cover it.
pub fn dialog(title: &str, ctx: &egui::Context, width: f32) -> egui::Window<'static> {
    let screen = ctx.content_rect();
    let fitted = width
        .min(screen.width() - DIALOG_SCREEN_MARGIN * 2.0)
        .max(1.0);
    let window = egui::Window::new(title.to_owned())
        .collapsible(false)
        .resizable(false)
        .default_width(fitted)
        .max_width(fitted)
        .max_height(screen.height() - DIALOG_SCREEN_MARGIN * 2.0)
        .constrain(true);
    if fitted < width {
        return window.anchor(egui::Align2::CENTER_TOP, [0.0, DIALOG_SCREEN_MARGIN]);
    }
    window
}
