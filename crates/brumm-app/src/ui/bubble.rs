//! One chat bubble: measuring and hand-painting text, image previews and file cards, with the
//! time + delivery marker sharing the last line, plus the selection chrome (highlight, check mark)
//! and the per-message interaction (context menu, click-to-select).

use std::sync::Arc;

use brumm_core::api::{DeliveryStatus, Direction, MessageView, ReactionsView};
use egui::{Align2, Color32, CornerRadius, FontId, Galley, Pos2, Rect, Sense, Stroke, Vec2};

use crate::{
    state::LoadedAttachment,
    ui::{emoji, files, theme, time_fmt},
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
const CHECK_RADIUS: f32 = 8.0;
/// Distance of the selection marker from the window's right edge (inside the reserved gutter).
const CHECK_RIGHT_INSET: f32 = 11.0;
/// Size of the "Zipfel" (tail) on the user's own bubbles.
const TAIL_WIDTH: f32 = 7.0;
const TAIL_HEIGHT: f32 = 13.0;
const TAIL_CURVE_STEPS: usize = 8;
/// Light-blue wash over a selected message row, like Telegram's.
const SELECTED_ROW_TINT: Color32 = Color32::from_rgba_premultiplied(0x10, 0x28, 0x3A, 0x40);
const MIN_BUBBLE_WIDTH: f32 = 120.0;
/// Reactions offered at the top of the context menu; double-click uses the first.
const QUICK_REACTIONS: [&str; 7] = ["❤", "👍", "😂", "😮", "😢", "🙏", "🔥"];
const CHIP_HEIGHT: f32 = 24.0;
const CHIP_PADDING_X: f32 = 8.0;
const CHIP_GAP: f32 = 4.0;
/// Space between the message content and the reaction chips below it.
const CHIPS_TOP_GAP: f32 = 6.0;
const CHIP_INNER_GAP: f32 = 4.0;
const CHIP_PLAIN_FILL: Color32 = Color32::from_rgba_premultiplied(30, 30, 30, 30);
const CHIP_EMOJI_SIZE: f32 = 15.0;
const CHIP_COUNT_SIZE: f32 = 12.0;
const META_PILL_PADDING: Vec2 = egui::vec2(6.0, 2.0);

/// What the user asked for on a message; the chat view applies it.
pub enum Action {
    /// React with this emoji (the chat view turns re-sending your own emoji into a removal).
    React(String),
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
    let body = body_block(&content);
    let chip_layout = layout_chips(ui, chips(&message.reactions), max_inner_width);
    let layout = bubble_layout(&body, &chip_layout, meta.size(), max_inner_width, is_image);
    let bubble_size = layout.bubble_size;

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
    // The tail would run into the selection marker, so it is hidden while selecting.
    if outgoing && inputs.ends_group && !inputs.selection_mode {
        paint_tail(ui, rect, theme::OUTGOING_BUBBLE);
    }
    let card_area = paint_content(ui, rect, content, layout.padding);
    paint_meta(ui, rect, meta, is_image);

    let mut actions = Vec::new();
    let chips_origin = rect.min + egui::vec2(layout.padding.x + layout.chips_inset_x, layout.padding.y + body.size.y + CHIPS_TOP_GAP);
    paint_chips(ui, chips_origin, chip_layout, inputs, &mut actions);
    if response.double_clicked() && !inputs.selection_mode {
        actions.push(Action::React(QUICK_REACTIONS[0].to_owned()));
    }
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
    reaction_bar(ui, &message.reactions, actions);
    ui.separator();
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

/// Content size without meta or reactions, and the width of its last line (where the meta may go).
struct Block {
    size: Vec2,
    last_row_width: f32,
}

fn body_block(content: &Content) -> Block {
    match content {
        Content::Text(galley) => Block {
            size: galley.size(),
            last_row_width: galley.rows.last().map_or(0.0, |row| row.rect().width()),
        },
        Content::Image { size, .. } => Block { size: *size, last_row_width: 0.0 },
        Content::Card { name, size } => {
            let text_left = CARD_ICON_SIZE + CARD_GAP;
            Block {
                size: egui::vec2(text_left + name.size().x.max(size.size().x), CARD_ICON_SIZE),
                last_row_width: text_left + size.size().x,
            }
        }
    }
}

struct BubbleLayout {
    bubble_size: Vec2,
    /// Offset of the content from the bubble's top-left corner.
    padding: Vec2,
    /// Extra left offset of the chips relative to the content (images are padded less than text).
    chips_inset_x: f32,
}

/// Content, then reaction chips below it, then the time + ticks: on the last line if it fits there,
/// else on a line of its own. Image bubbles show the time as an overlay instead.
fn bubble_layout(body: &Block, chips: &ChipLayout, meta: Vec2, max_inner_width: f32, is_image: bool) -> BubbleLayout {
    let mut size = body.size;
    let mut last_row_width = body.last_row_width;
    if !chips.items.is_empty() {
        size = egui::vec2(size.x.max(chips.size.x), size.y + CHIPS_TOP_GAP + chips.size.y);
        last_row_width = chips.last_row_width;
    }
    if !is_image {
        if last_row_width + META_GAP + meta.x <= max_inner_width {
            size.x = size.x.max(last_row_width + META_GAP + meta.x);
        } else {
            size = egui::vec2(size.x.max(meta.x), size.y + meta.y);
        }
    }
    let has_chips = !chips.items.is_empty();
    let padding = if is_image { Vec2::splat(IMAGE_PADDING) } else { theme::BUBBLE_PADDING };
    let bottom_padding = if is_image && !has_chips { IMAGE_PADDING } else if is_image { theme::BUBBLE_PADDING.y } else { padding.y };
    let chips_inset_x = if is_image { theme::BUBBLE_PADDING.x - IMAGE_PADDING } else { 0.0 };
    let width = (size.x + padding.x * 2.0).max(if has_chips { chips.size.x + theme::BUBBLE_PADDING.x * 2.0 } else { 0.0 });
    BubbleLayout { bubble_size: egui::vec2(width, size.y + padding.y + bottom_padding), padding, chips_inset_x }
}

/// Paints the content; returns the clickable card rect for file cards.
fn paint_content(ui: &egui::Ui, bubble: Rect, content: Content, padding: Vec2) -> Option<Rect> {
    match content {
        Content::Text(galley) => {
            ui.painter().galley(bubble.min + padding, galley, theme::TEXT);
            None
        }
        Content::Image { size, loaded, uri } => {
            let rect = Rect::from_min_size(bubble.min + padding, size);
            paint_image(ui, rect, loaded, uri);
            None
        }
        Content::Card { name, size } => {
            let origin = bubble.min + padding;
            let icon = Rect::from_min_size(origin, Vec2::splat(CARD_ICON_SIZE));
            ui.painter().circle_filled(icon.center(), CARD_ICON_SIZE / 2.0, theme::ACCENT);
            ui.painter().text(icon.center(), Align2::CENTER_CENTER, "🗎", FontId::proportional(22.0), Color32::WHITE);
            let text_left = icon.right() + CARD_GAP;
            ui.painter().galley(Pos2::new(text_left, origin.y + 2.0), name, theme::TEXT);
            ui.painter().galley(Pos2::new(text_left, origin.y + CARD_ICON_SIZE - 18.0), size, theme::MUTED_TEXT);
            Some(Rect::from_min_size(origin, egui::vec2(bubble.width() - padding.x * 2.0, CARD_ICON_SIZE)))
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
        radius.se = 0;
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

/// Quick reactions in a row plus a submenu with every emoji.
fn reaction_bar(ui: &mut egui::Ui, reactions: &ReactionsView, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        for emoji in QUICK_REACTIONS {
            let mine = reactions.mine.as_deref() == Some(emoji);
            let button = egui::Button::new(egui::RichText::new(emoji).size(20.0))
                .fill(if mine { theme::ROW_SELECTED } else { Color32::TRANSPARENT });
            if ui.add(button).clicked() {
                actions.push(Action::React(emoji.to_owned()));
                ui.close();
            }
        }
    });
    ui.menu_button("More reactions…", |ui| {
        if let Some(emoji) = emoji::grid(ui) {
            actions.push(Action::React(emoji.to_owned()));
            ui.close();
        }
    });
}

struct Chip {
    emoji: String,
    count: usize,
    includes_mine: bool,
}

/// One chip per distinct emoji; when both users picked the same emoji it shows a count of two.
fn chips(reactions: &ReactionsView) -> Vec<Chip> {
    match (&reactions.mine, &reactions.theirs) {
        (Some(mine), Some(theirs)) if mine == theirs => vec![Chip { emoji: mine.clone(), count: 2, includes_mine: true }],
        (mine, theirs) => theirs
            .iter()
            .map(|emoji| Chip { emoji: emoji.clone(), count: 1, includes_mine: false })
            .chain(mine.iter().map(|emoji| Chip { emoji: emoji.clone(), count: 1, includes_mine: true }))
            .collect(),
    }
}

struct ChipItem {
    chip: Chip,
    /// Position relative to the chips' origin.
    rect: Rect,
    emoji: Arc<Galley>,
    count: Option<Arc<Galley>>,
}

struct ChipLayout {
    items: Vec<ChipItem>,
    size: Vec2,
    last_row_width: f32,
}

/// Lays chips out left to right, wrapping to a new row when `max_width` is exceeded.
fn layout_chips(ui: &egui::Ui, chips: Vec<Chip>, max_width: f32) -> ChipLayout {
    let (mut x, mut y, mut widest) = (0.0_f32, 0.0_f32, 0.0_f32);
    let mut items = Vec::with_capacity(chips.len());
    for chip in chips {
        let emoji = ui.painter().layout_no_wrap(chip.emoji.clone(), FontId::proportional(CHIP_EMOJI_SIZE), Color32::WHITE);
        let count = (chip.count > 1)
            .then(|| ui.painter().layout_no_wrap(chip.count.to_string(), FontId::proportional(CHIP_COUNT_SIZE), Color32::WHITE));
        let width = CHIP_PADDING_X * 2.0 + emoji.size().x + count.as_ref().map_or(0.0, |count| count.size().x + CHIP_INNER_GAP);
        if x > 0.0 && x + width > max_width {
            x = 0.0;
            y += CHIP_HEIGHT + CHIP_GAP;
        }
        let rect = Rect::from_min_size(egui::pos2(x, y), egui::vec2(width, CHIP_HEIGHT));
        x += width + CHIP_GAP;
        widest = widest.max(rect.right());
        items.push(ChipItem { chip, rect, emoji, count });
    }
    let last_row_width = items.last().map_or(0.0, |item| item.rect.right());
    let height = items.last().map_or(0.0, |item| item.rect.bottom());
    ChipLayout { items, size: egui::vec2(widest, height), last_row_width }
}

/// Paints the chips below the message text and makes them clickable.
fn paint_chips(ui: &mut egui::Ui, origin: Pos2, layout: ChipLayout, inputs: &Inputs, actions: &mut Vec<Action>) {
    for (index, item) in layout.items.into_iter().enumerate() {
        let rect = item.rect.translate(origin.to_vec2());
        let response = ui.interact(rect, ui.scope_id().with(("chip", inputs.index, index)), Sense::click());
        let hovered = response.hovered() && !inputs.selection_mode;
        let fill = if item.chip.includes_mine { theme::ACCENT } else { CHIP_PLAIN_FILL };
        let fill = if hovered { fill.gamma_multiply(1.25) } else { fill };
        ui.painter().rect_filled(rect, CHIP_HEIGHT / 2.0, fill);
        let center_y = rect.center().y;
        let emoji_size = item.emoji.size();
        let mut x = rect.left() + CHIP_PADDING_X;
        ui.painter().galley(Pos2::new(x, center_y - emoji_size.y / 2.0), item.emoji, Color32::WHITE);
        x += emoji_size.x + CHIP_INNER_GAP;
        if let Some(count) = item.count {
            ui.painter().galley(Pos2::new(x, center_y - count.size().y / 2.0), count, Color32::WHITE);
        }
        if response.clicked() && !inputs.selection_mode {
            actions.push(Action::React(item.chip.emoji));
        }
        if hovered {
            ui.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
    }
}

/// The small tail at the bottom-right corner of a user's own bubble: the right edge flares out to a
/// point with a concave curve, like Telegram's.
fn paint_tail(ui: &egui::Ui, bubble: Rect, color: Color32) {
    let start = Pos2::new(bubble.right(), bubble.bottom() - TAIL_HEIGHT);
    let control = Pos2::new(bubble.right(), bubble.bottom());
    let tip = Pos2::new(bubble.right() + TAIL_WIDTH, bubble.bottom());
    let base = Pos2::new(bubble.right() - TAIL_WIDTH, bubble.bottom());
    let mut outline: Vec<Pos2> = (0..=TAIL_CURVE_STEPS)
        .map(|step| {
            let t = step as f32 / TAIL_CURVE_STEPS as f32;
            let (a, b, c) = ((1.0 - t) * (1.0 - t), 2.0 * (1.0 - t) * t, t * t);
            Pos2::new(a * start.x + b * control.x + c * tip.x, a * start.y + b * control.y + c * tip.y)
        })
        .collect();
    outline.push(base);
    // Not convex (the curve bows inward), so fill it as a triangle fan from an interior point.
    let center = Pos2::new(bubble.right() - TAIL_WIDTH / 2.0, bubble.bottom() - TAIL_HEIGHT / 3.0);
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(center, color);
    for point in &outline {
        mesh.colored_vertex(*point, color);
    }
    for index in 1..outline.len() as u32 {
        mesh.add_triangle(0, index, index + 1);
    }
    mesh.add_triangle(0, outline.len() as u32, 1);
    ui.painter().add(egui::Shape::mesh(mesh));
    // The mesh has no anti-aliasing; a thin stroke along the curved edge smooths it.
    ui.painter().add(egui::Shape::line(outline[..=TAIL_CURVE_STEPS].to_vec(), Stroke::new(1.0, color)));
}
