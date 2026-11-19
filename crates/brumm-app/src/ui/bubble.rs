//! One chat bubble: measuring and hand-painting text, image previews and file cards, with the
//! time + delivery marker sharing the last line, plus the selection chrome (highlight, check mark)
//! and the per-message interaction (context menu, click-to-select).

use std::sync::Arc;

use brumm_core::api::{DeliveryStatus, Direction, MessageView};
use egui::{Align2, Color32, CornerRadius, FontId, Galley, Pos2, Rect, Sense, Stroke, Vec2};

use crate::{
    state::LoadedAttachment,
    ui::{files, theme, time_fmt},
};

const META_FONT_SIZE: f32 = 12.0;
const BODY_FONT_SIZE: f32 = 15.0;
const CARD_NAME_FONT_SIZE: f32 = 15.0;
const META_GAP: f32 = 10.0;
const BUBBLE_MAX_WIDTH_FRACTION: f32 = 0.85;
const IMAGE_PADDING: f32 = 3.0;
const IMAGE_MAX_SIZE: Vec2 = egui::vec2(320.0, 320.0);
const IMAGE_PLACEHOLDER_SIZE: Vec2 = egui::vec2(240.0, 160.0);
const IMAGE_CORNER_RADIUS: u8 = 13;
const CARD_ICON_SIZE: f32 = 44.0;
const CARD_GAP: f32 = 10.0;
const CARD_MAX_NAME_CHARS: usize = 26;
const CHECK_RADIUS: f32 = 10.0;
/// Distance of the selection marker from the window's right edge (inside the reserved gutter).
const CHECK_RIGHT_INSET: f32 = 24.0;
/// Light-blue wash over a selected message row, like Telegram's.
const SELECTED_ROW_TINT: Color32 = Color32::from_rgba_premultiplied(0x10, 0x28, 0x3A, 0x40);
const MIN_BUBBLE_WIDTH: f32 = 120.0;
const META_PILL_PADDING: Vec2 = egui::vec2(6.0, 2.0);

/// What the user asked for on a message; the chat view applies it.
pub enum Action {
    ToggleSelect,
    Copy,
    SaveAs,
    Delete,
}

/// Result of laying out one bubble.
pub struct Output {
    pub actions: Vec<Action>,
    /// Full-width row and the part of it that starts a selection gesture.
    pub row: Rect,
    pub hit_area: Rect,
}

pub struct Inputs<'a> {
    pub index: usize,
    pub ends_group: bool,
    pub attachment: Option<&'a LoadedAttachment>,
    pub selection_mode: bool,
    pub selected: bool,
}

enum Content<'a> {
    Text(Arc<Galley>),
    Image { size: Vec2, loaded: Option<&'a LoadedAttachment>, uri: String },
    Card { name: Arc<Galley>, size: Arc<Galley> },
}

pub fn show(ui: &mut egui::Ui, message: &MessageView, inputs: &Inputs) -> Output {
    let outgoing = message.direction == Direction::Outgoing;
    let max_width = theme::BUBBLE_MAX_WIDTH.min(ui.available_width() * BUBBLE_MAX_WIDTH_FRACTION).max(MIN_BUBBLE_WIDTH);
    let max_inner_width = max_width - theme::BUBBLE_PADDING.x * 2.0;

    let meta_color = match (outgoing, message.status) {
        (true, DeliveryStatus::Failed) => theme::DANGER,
        (true, _) => theme::OUTGOING_META,
        (false, _) => theme::MUTED_TEXT,
    };
    let meta = ui.painter().layout_no_wrap(meta_text(message), FontId::proportional(META_FONT_SIZE), meta_color);

    let content = measure_content(ui, message, inputs.attachment, max_inner_width);
    let is_image = matches!(content, Content::Image { .. });
    let content_size = content_layout(&content, &meta, max_inner_width);
    let bubble_size = content_size + if is_image { Vec2::splat(IMAGE_PADDING * 2.0) } else { theme::BUBBLE_PADDING * 2.0 };

    let (row, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), bubble_size.y), Sense::hover());
    let left = if outgoing { row.right() - bubble_size.x } else { row.left() };
    let rect = Rect::from_min_size(egui::pos2(left, row.top()), bubble_size);

    if inputs.selected {
        let full_width = Rect::from_x_y_ranges(ui.clip_rect().x_range(), row.y_range()).expand2(egui::vec2(0.0, 1.0));
        ui.painter().rect_filled(full_width, 0.0, SELECTED_ROW_TINT);
    }
    if inputs.selection_mode {
        paint_check(ui, Pos2::new(ui.clip_rect().right() - CHECK_RIGHT_INSET, row.center().y), inputs.selected);
    }

    let hit_area = if inputs.selection_mode { row } else { rect };
    let response = ui.interact(hit_area, ui.scope_id().with(("bubble", inputs.index)), Sense::click());

    ui.painter().rect_filled(
        rect,
        bubble_radius(outgoing, inputs.ends_group),
        if outgoing { theme::OUTGOING_BUBBLE } else { theme::INCOMING_BUBBLE },
    );
    let card_area = paint_content(ui, rect, content);
    paint_meta(ui, rect, meta, is_image);

    let mut actions = Vec::new();
    if response.clicked() {
        let plain_click = ui.input(|input| !input.modifiers.command && !input.modifiers.shift);
        let on_card = card_area.zip(response.interact_pointer_pos()).is_some_and(|(card, pos)| card.contains(pos));
        if on_card && plain_click && !inputs.selection_mode {
            actions.push(Action::SaveAs);
        }
    }
    if response.hovered() && card_area.is_some() && !inputs.selection_mode {
        ui.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.context_menu(|ui| context_menu(ui, message, inputs.selected, &mut actions));
    Output { actions, row, hit_area }
}

fn context_menu(ui: &mut egui::Ui, message: &MessageView, selected: bool, actions: &mut Vec<Action>) {
    if !message.text.is_empty() && ui.button("Copy text").clicked() {
        actions.push(Action::Copy);
        ui.close();
    }
    if message.attachment.is_some() && ui.button("Save as…").clicked() {
        actions.push(Action::SaveAs);
        ui.close();
    }
    if ui.button(if selected { "Unselect" } else { "Select" }).clicked() {
        actions.push(Action::ToggleSelect);
        ui.close();
    }
    ui.separator();
    if ui.button(egui::RichText::new("Delete").color(theme::DANGER)).clicked() {
        actions.push(Action::Delete);
        ui.close();
    }
}

fn measure_content<'a>(ui: &egui::Ui, message: &MessageView, loaded: Option<&'a LoadedAttachment>, max_inner_width: f32) -> Content<'a> {
    let Some(attachment) = message.attachment.as_ref() else {
        let galley = ui.painter().layout(message.text.clone(), FontId::proportional(BODY_FONT_SIZE), theme::TEXT, max_inner_width);
        return Content::Text(galley);
    };
    let is_previewable = files::is_image_name(&attachment.name) && loaded.is_none_or(|loaded| loaded.image_size.is_some());
    if is_previewable {
        let size = loaded
            .and_then(|loaded| loaded.image_size)
            .map_or(IMAGE_PLACEHOLDER_SIZE, |(width, height)| fit_image(width, height));
        return Content::Image { size, loaded, uri: format!("bytes://attachment/{}", hex(&message.id)) };
    }
    let name = ui.painter().layout_no_wrap(truncate_name(&attachment.name), FontId::proportional(CARD_NAME_FONT_SIZE), theme::TEXT);
    let size = ui.painter().layout_no_wrap(files::format_size(attachment.size), FontId::proportional(META_FONT_SIZE), theme::MUTED_TEXT);
    Content::Card { name, size }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Content size, leaving room for the meta (time + ticks) on the last line or below it.
fn content_layout(content: &Content, meta: &Galley, max_inner_width: f32) -> Vec2 {
    let meta_size = meta.size();
    match content {
        Content::Text(galley) => {
            let text_size = galley.size();
            let last_row_width = galley.rows.last().map_or(0.0, |row| row.rect().width());
            let fits = last_row_width + META_GAP + meta_size.x <= max_inner_width;
            if fits {
                egui::vec2(text_size.x.max(last_row_width + META_GAP + meta_size.x), text_size.y)
            } else {
                egui::vec2(text_size.x.max(meta_size.x), text_size.y + meta_size.y)
            }
        }
        Content::Image { size, .. } => *size,
        Content::Card { name, size } => {
            let width = CARD_ICON_SIZE + CARD_GAP + name.size().x.max(size.size().x + META_GAP + meta_size.x);
            egui::vec2(width, CARD_ICON_SIZE)
        }
    }
}

/// Paints the content; returns the clickable card rect for file cards.
fn paint_content(ui: &egui::Ui, bubble: Rect, content: Content) -> Option<Rect> {
    match content {
        Content::Text(galley) => {
            ui.painter().galley(bubble.min + theme::BUBBLE_PADDING, galley, theme::TEXT);
            None
        }
        Content::Image { size, loaded, uri } => {
            let rect = Rect::from_min_size(bubble.min + Vec2::splat(IMAGE_PADDING), size);
            paint_image(ui, rect, loaded, uri);
            None
        }
        Content::Card { name, size } => {
            let origin = bubble.min + theme::BUBBLE_PADDING;
            let icon = Rect::from_min_size(origin, Vec2::splat(CARD_ICON_SIZE));
            ui.painter().circle_filled(icon.center(), CARD_ICON_SIZE / 2.0, theme::ACCENT);
            ui.painter().text(icon.center(), Align2::CENTER_CENTER, "🗎", FontId::proportional(22.0), Color32::WHITE);
            let text_left = icon.right() + CARD_GAP;
            ui.painter().galley(Pos2::new(text_left, origin.y + 2.0), name, theme::TEXT);
            ui.painter().galley(Pos2::new(text_left, origin.y + CARD_ICON_SIZE - 18.0), size, theme::MUTED_TEXT);
            Some(Rect::from_min_size(origin, egui::vec2(bubble.width() - theme::BUBBLE_PADDING.x * 2.0, CARD_ICON_SIZE)))
        }
    }
}

fn paint_image(ui: &egui::Ui, rect: Rect, loaded: Option<&LoadedAttachment>, uri: String) {
    let radius = CornerRadius::same(IMAGE_CORNER_RADIUS);
    let Some(loaded) = loaded else {
        ui.painter().rect_filled(rect, radius, theme::CHAT_BG);
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, "Loading…", FontId::proportional(13.0), theme::MUTED_TEXT);
        return;
    };
    egui::Image::from_bytes(uri, egui::load::Bytes::Shared(loaded.bytes.clone())).corner_radius(radius).paint_at(ui, rect);
}

/// Time + ticks: inline on the last text line, as an overlay pill on images, else on its own line.
fn paint_meta(ui: &egui::Ui, bubble: Rect, meta: Arc<Galley>, is_image: bool) {
    let meta_size = meta.size();
    if is_image {
        let pill = Rect::from_min_size(
            bubble.max - Vec2::splat(IMAGE_PADDING + 6.0) - meta_size - META_PILL_PADDING,
            meta_size + META_PILL_PADDING * 2.0,
        );
        ui.painter().rect_filled(pill, pill.height() / 2.0, theme::SERVICE_PILL);
        ui.painter().galley(pill.min + META_PILL_PADDING, meta, Color32::WHITE);
        return;
    }
    let position = bubble.max - theme::BUBBLE_PADDING - meta_size;
    ui.painter().galley(position, meta, theme::MUTED_TEXT);
}

fn paint_check(ui: &egui::Ui, center: Pos2, selected: bool) {
    if selected {
        ui.painter().circle_filled(center, CHECK_RADIUS, theme::ACCENT);
        ui.painter().text(center, Align2::CENTER_CENTER, "✓", FontId::proportional(14.0), Color32::WHITE);
    } else {
        ui.painter().circle_stroke(center, CHECK_RADIUS, Stroke::new(1.5, theme::MUTED_TEXT));
    }
}

fn fit_image(width: u32, height: u32) -> Vec2 {
    let natural = egui::vec2(width.max(1) as f32, height.max(1) as f32);
    let scale = (IMAGE_MAX_SIZE.x / natural.x).min(IMAGE_MAX_SIZE.y / natural.y).min(1.0);
    natural * scale
}

fn truncate_name(name: &str) -> String {
    if name.chars().count() <= CARD_MAX_NAME_CHARS {
        return name.to_owned();
    }
    let kept: String = name.chars().take(CARD_MAX_NAME_CHARS - 1).collect();
    format!("{kept}…")
}

/// The last bubble of a group gets the small "tail" corner on the sender's side.
fn bubble_radius(outgoing: bool, ends_group: bool) -> CornerRadius {
    let mut radius = CornerRadius::same(theme::BUBBLE_RADIUS);
    if !ends_group {
        return radius;
    }
    if outgoing {
        radius.se = theme::BUBBLE_TAIL_RADIUS;
    } else {
        radius.sw = theme::BUBBLE_TAIL_RADIUS;
    }
    radius
}

fn meta_text(message: &MessageView) -> String {
    let time = time_fmt::clock(message.sent_at_ms);
    let marker = status_marker(message);
    if marker.is_empty() { time } else { format!("{time} {marker}") }
}

fn status_marker(message: &MessageView) -> &'static str {
    if message.direction == Direction::Incoming {
        return "";
    }
    match message.status {
        DeliveryStatus::Pending => "🕓",
        DeliveryStatus::Sent => "✓",
        DeliveryStatus::Delivered => "✓✓",
        DeliveryStatus::Failed => "⚠ not delivered",
    }
}
