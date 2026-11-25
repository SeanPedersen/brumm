//! One chat bubble: measuring and hand-painting text, image previews and file cards, with the
//! time + delivery marker sharing the last line, plus the selection chrome (highlight, check mark)
//! and the per-message interaction (context menu, click-to-select). Web and mail links open with
//! the system handler. A note longer than a few kilobytes keeps an estimated height and shapes
//! only the lines inside the viewport. Text with markdown is laid out by [`markdown`]; a windowed
//! note stays plain.

use std::sync::Arc;

use brumm_core::api::{DeliveryStatus, Direction, MessageView, ReactionsView};
use egui::{Align2, Color32, CornerRadius, FontId, Galley, Pos2, Rect, Sense, Stroke, Vec2};

use crate::{
    state::LoadedAttachment,
    touch,
    ui::{emoji, files, icons, links::TextLink, markdown, theme, time_fmt},
};

const REACTION_EMOJI_SIZE: f32 = 22.0;
const MENU_FONT_SIZE: f32 = 18.0;
const MENU_BUTTON_PADDING_X: f32 = 14.0;
const MENU_BUTTON_PADDING_Y: f32 = 8.0;
const MENU_ITEM_SPACING_Y: f32 = 4.0;
const MENU_MIN_ROW_HEIGHT: f32 = 44.0;
const META_FONT_SIZE: f32 = 12.0;
const BODY_FONT_SIZE: f32 = 15.0;
const CARD_NAME_FONT_SIZE: f32 = 15.0;
const META_GAP: f32 = 10.0;
/// Fraction of a tick's width by which the second "delivered" tick is shifted over the first.
const DOUBLE_TICK_SHIFT: f32 = 0.45;
/// The pending clock icon: edge length as a share of the meta font size, and the gap before it.
const CLOCK_SIZE_RATIO: f32 = 0.95;
const CLOCK_GAP: f32 = 4.0;
const BUBBLE_MAX_WIDTH_FRACTION: f32 = 0.85;
const IMAGE_PADDING: f32 = 3.0;
const IMAGE_MAX_SIZE: Vec2 = egui::vec2(files::IMAGE_BUBBLE_EDGE, files::IMAGE_BUBBLE_EDGE);
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
/// A longer body is never shaped in full. Only the lines that meet the viewport become a galley.
pub(crate) const WINDOWED_TEXT_BYTES: usize = 8 * 1024;
/// Markdown is laid out in full (one galley per block, so off-screen blocks are culled). Only
/// a plain note, or one over the send limit, is windowed.
const MARKDOWN_MAX_BYTES: usize = brumm_core::wire::MAX_TEXT_BYTES;
/// Row height of `FontId::proportional(15)` with the app font. The windowed note uses this
/// instead of a galley, so it has to match or the visible lines drift.
const ESTIMATE_LINE_HEIGHT: f32 = 17.0;
const ESTIMATE_CHAR_WIDTH: f32 = 7.5;
/// Lines shaped just outside the clip, so a small scroll does not go blank.
const TEXT_WINDOW_OVERSCAN: f32 = 80.0;

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
    /// Screen rects of link glyphs, one run per visual line. Tests click these.
    #[cfg(test)]
    pub link_rects: Vec<Rect>,
}

pub struct Inputs<'a> {
    pub index: usize,
    pub ends_group: bool,
    pub attachment: Option<&'a LoadedAttachment>,
    pub selection_mode: bool,
    pub selected: bool,
    /// Byte offset of each visual line when the body is windowed. Empty for an ordinary message.
    pub line_starts: &'a [u32],
}

enum Content<'a> {
    Text {
        galley: Arc<Galley>,
        links: Vec<TextLink>,
    },
    Markdown(markdown::Rendered),
    Image {
        size: Vec2,
        loaded: Option<&'a LoadedAttachment>,
        uri: String,
    },
    Card {
        name: Arc<Galley>,
        size: Arc<Galley>,
    },
}

/// Bubble height used before a row has been measured, plus the visual-line index of a long note.
/// Must not shape the text: a multi-megabyte note would allocate a galley per line.
pub(crate) fn estimated_height(message: &MessageView, column_width: f32) -> (f32, Vec<u32>) {
    let max_inner = max_inner_width(column_width);
    let line_starts = if message.attachment.is_none() && message.text.len() > WINDOWED_TEXT_BYTES {
        visual_line_starts(&message.text, chars_per_line(max_inner))
    } else {
        Vec::new()
    };
    let mut inner = if message
        .attachment
        .as_ref()
        .is_some_and(|attachment| files::is_image_name(&attachment.name))
    {
        IMAGE_PLACEHOLDER_SIZE.y
    } else if message.attachment.is_some() {
        CARD_ICON_SIZE
    } else if !line_starts.is_empty() {
        line_starts.len() as f32 * ESTIMATE_LINE_HEIGHT
    } else {
        estimated_text_block(&message.text, max_inner)
    };
    if message.reactions.mine.is_some() || message.reactions.theirs.is_some() {
        inner += CHIPS_TOP_GAP + CHIP_HEIGHT;
    }
    (inner + theme::BUBBLE_PADDING.y * 2.0, line_starts)
}

fn max_inner_width(column_width: f32) -> f32 {
    let max_width = theme::BUBBLE_MAX_WIDTH
        .min(column_width * BUBBLE_MAX_WIDTH_FRACTION)
        .max(MIN_BUBBLE_WIDTH);
    (max_width - theme::BUBBLE_PADDING.x * 2.0).max(1.0)
}

fn chars_per_line(max_inner_width: f32) -> usize {
    (max_inner_width / ESTIMATE_CHAR_WIDTH).floor().max(1.0) as usize
}

/// Byte offset of every visual line, wrapping on newlines and on `chars_per_line`.
fn visual_line_starts(text: &str, chars_per_line: usize) -> Vec<u32> {
    let chars_per_line = chars_per_line.max(1);
    let mut starts = Vec::new();
    let mut column = 0usize;
    let mut line_start = 0usize;
    for (index, ch) in text.char_indices() {
        if ch == '\n' {
            starts.push(line_start as u32);
            line_start = index + ch.len_utf8();
            column = 0;
            continue;
        }
        if column == chars_per_line {
            starts.push(line_start as u32);
            line_start = index;
            column = 0;
        }
        column += 1;
    }
    if column > 0 || starts.is_empty() {
        starts.push(line_start as u32);
    }
    starts
}

/// Wrapped line count from the byte length, plus one line per newline. Avoids walking a huge
/// string character by character.
fn estimated_text_block(text: &str, max_inner_width: f32) -> f32 {
    if text.is_empty() {
        return ESTIMATE_LINE_HEIGHT;
    }
    let per_line = (max_inner_width / ESTIMATE_CHAR_WIDTH).max(1.0);
    let wrapped = (text.len() as f32 / per_line).ceil().max(1.0);
    let explicit = text.bytes().filter(|byte| *byte == b'\n').count() as f32 + 1.0;
    wrapped.max(explicit) * ESTIMATE_LINE_HEIGHT
}

pub fn show(ui: &mut egui::Ui, message: &MessageView, inputs: &Inputs) -> Output {
    let outgoing = message.direction == Direction::Outgoing;
    let max_width = theme::BUBBLE_MAX_WIDTH
        .min(ui.available_width() * BUBBLE_MAX_WIDTH_FRACTION)
        .max(MIN_BUBBLE_WIDTH);
    let max_inner_width = max_width - theme::BUBBLE_PADDING.x * 2.0;

    let meta_color = match (outgoing, message.status) {
        (true, DeliveryStatus::Failed) => theme::DANGER,
        (true, _) => theme::OUTGOING_META,
        (false, _) => theme::MUTED_TEXT,
    };
    let meta = layout_meta(ui, message, meta_color);

    let doc = (message.attachment.is_none() && message.text.len() <= MARKDOWN_MAX_BYTES)
        .then(|| markdown::doc(ui.ctx(), &message.text))
        .flatten();
    // A long plain note keeps an estimated height and shapes only the lines inside the clip.
    // Falling back here covers a row whose line index was not cached yet.
    let generated;
    let line_starts = if doc.is_some() {
        &[]
    } else if message.attachment.is_none()
        && inputs.line_starts.is_empty()
        && message.text.len() > WINDOWED_TEXT_BYTES
    {
        generated = visual_line_starts(&message.text, chars_per_line(max_inner_width));
        generated.as_slice()
    } else {
        inputs.line_starts
    };
    let content = if line_starts.is_empty() {
        Some(measure_content(
            ui,
            message,
            doc.as_deref(),
            inputs.attachment,
            max_inner_width,
        ))
    } else {
        None
    };
    let is_image = content
        .as_ref()
        .is_some_and(|content| matches!(content, Content::Image { .. }));
    let body = match &content {
        Some(content) => body_block(content),
        None => Block {
            size: egui::vec2(
                max_inner_width,
                line_starts.len() as f32 * ESTIMATE_LINE_HEIGHT,
            ),
            // Full width puts the time on its own line, matching the reserved height.
            last_row_width: max_inner_width,
        },
    };
    let chip_layout = layout_chips(ui, chips(&message.reactions), max_inner_width);
    let layout = bubble_layout(&body, &chip_layout, meta.size, max_inner_width, is_image);
    let bubble_size = layout.bubble_size;

    let (row, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), bubble_size.y),
        Sense::hover(),
    );
    let left = if outgoing {
        row.right() - bubble_size.x
    } else {
        row.left()
    };
    let rect = Rect::from_min_size(egui::pos2(left, row.top()), bubble_size);

    if inputs.selected {
        let full_width = Rect::from_x_y_ranges(ui.clip_rect().x_range(), row.y_range())
            .expand2(egui::vec2(0.0, 1.0));
        ui.painter().rect_filled(full_width, 0.0, SELECTED_ROW_TINT);
    }
    if inputs.selection_mode {
        let visible = row.intersect(ui.clip_rect());
        let marker_y = if visible.height() > 0.0 {
            visible.center().y
        } else {
            row.center().y
        };
        paint_check(
            ui,
            Pos2::new(ui.clip_rect().right() - CHECK_RIGHT_INSET, marker_y),
            inputs.selected,
        );
    }

    let hit_area = if inputs.selection_mode { row } else { rect };
    let response = ui.interact(
        hit_area,
        ui.scope_id().with(("bubble", inputs.index)),
        Sense::click(),
    );

    ui.painter().rect_filled(
        rect,
        bubble_radius(outgoing, inputs.ends_group),
        if outgoing {
            theme::OUTGOING_BUBBLE
        } else {
            theme::INCOMING_BUBBLE
        },
    );
    // The tail would run into the selection marker, so it is hidden while selecting.
    if outgoing && inputs.ends_group && !inputs.selection_mode {
        paint_tail(ui, rect, theme::OUTGOING_BUBBLE);
    }
    let painted = match content {
        Some(content) => paint_content(ui, rect, content, layout.padding),
        None => paint_windowed_text(
            ui,
            &message.text,
            line_starts,
            rect,
            layout.padding,
            max_inner_width,
        ),
    };
    paint_meta(ui, rect, meta, is_image);

    let mut actions = Vec::new();
    let chips_origin = rect.min
        + egui::vec2(
            layout.padding.x + layout.chips_inset_x,
            layout.padding.y + body.size.y + CHIPS_TOP_GAP,
        );
    paint_chips(ui, chips_origin, chip_layout, inputs, &mut actions);
    let plain_click = ui.input(|input| !input.modifiers.command && !input.modifiers.shift);
    let link_clickable = painted.link.is_some() && plain_click && !inputs.selection_mode;
    if response.double_clicked() && !link_clickable && !inputs.selection_mode {
        actions.push(Action::React(QUICK_REACTIONS[0].to_owned()));
    }
    if response.clicked() && !response.double_clicked() {
        if let Some(url) = painted.link.clone()
            && link_clickable
        {
            ui.ctx().open_url(egui::OpenUrl::same_tab(url));
        } else {
            let on_card = painted
                .card
                .zip(response.interact_pointer_pos())
                .is_some_and(|(card, pos)| card.contains(pos));
            if on_card && plain_click && !inputs.selection_mode {
                actions.push(Action::SaveAs);
            }
        }
    }
    if link_clickable || (response.hovered() && painted.card.is_some() && !inputs.selection_mode) {
        ui.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if link_clickable && let Some(url) = &painted.link {
        response.clone().on_hover_text(url);
    }
    response.context_menu(|ui| context_menu(ui, message, inputs.selected, &mut actions));
    Output {
        actions,
        row,
        hit_area,
        #[cfg(test)]
        link_rects: painted.link_rects,
    }
}

fn context_menu(
    ui: &mut egui::Ui,
    message: &MessageView,
    selected: bool,
    actions: &mut Vec<Action>,
) {
    if touch::is_touch_device() {
        enlarge_for_touch(ui);
    }
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
    if ui
        .button(if selected { "Unselect" } else { "Select" })
        .clicked()
    {
        actions.push(Action::ToggleSelect);
        ui.close();
    }
    ui.separator();
    if ui
        .button(egui::RichText::new("Delete").color(theme::DANGER))
        .clicked()
    {
        actions.push(Action::Delete);
        ui.close();
    }
}

/// Finger-sized menu rows: larger text, taller buttons and more room between them.
fn enlarge_for_touch(ui: &mut egui::Ui) {
    let style = ui.style_mut();
    style.spacing.button_padding = egui::vec2(MENU_BUTTON_PADDING_X, MENU_BUTTON_PADDING_Y);
    style.spacing.item_spacing.y = MENU_ITEM_SPACING_Y;
    style.spacing.interact_size.y = MENU_MIN_ROW_HEIGHT;
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::proportional(MENU_FONT_SIZE),
    );
}

fn measure_content<'a>(
    ui: &egui::Ui,
    message: &MessageView,
    doc: Option<&markdown::Doc>,
    loaded: Option<&'a LoadedAttachment>,
    max_inner_width: f32,
) -> Content<'a> {
    let Some(attachment) = message.attachment.as_ref() else {
        note_shaped(message.text.len());
        if let Some(doc) = doc {
            return Content::Markdown(markdown::layout(ui, doc, BODY_FONT_SIZE, max_inner_width));
        }
        let links = crate::ui::links::find_links(&message.text);
        let galley = layout_body(ui, &message.text, &links, max_inner_width);
        return Content::Text { galley, links };
    };
    let is_previewable = files::is_image_name(&attachment.name)
        && loaded.is_none_or(|loaded| loaded.image_size.is_some());
    if is_previewable {
        let size = loaded
            .and_then(|loaded| loaded.image_size)
            .map_or(IMAGE_PLACEHOLDER_SIZE, |(width, height)| {
                fit_image(width, height)
            });
        return Content::Image {
            size,
            loaded,
            uri: crate::state::attachment_uri(&message.id),
        };
    }
    let name = ui.painter().layout_no_wrap(
        truncate_name(&attachment.name),
        FontId::proportional(CARD_NAME_FONT_SIZE),
        theme::TEXT,
    );
    let size = ui.painter().layout_no_wrap(
        files::format_size(attachment.size),
        FontId::proportional(META_FONT_SIZE),
        theme::MUTED_TEXT,
    );
    Content::Card { name, size }
}

/// Content size without meta or reactions, and the width of its last line (where the meta may go).
struct Block {
    size: Vec2,
    last_row_width: f32,
}

fn body_block(content: &Content) -> Block {
    match content {
        Content::Text { galley, .. } => Block {
            size: galley.size(),
            last_row_width: galley.rows.last().map_or(0.0, |row| row.rect().width()),
        },
        Content::Markdown(rendered) => Block {
            size: rendered.size,
            last_row_width: rendered.last_row_width,
        },
        Content::Image { size, .. } => Block {
            size: *size,
            last_row_width: 0.0,
        },
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
fn bubble_layout(
    body: &Block,
    chips: &ChipLayout,
    meta: Vec2,
    max_inner_width: f32,
    is_image: bool,
) -> BubbleLayout {
    let mut size = body.size;
    let mut last_row_width = body.last_row_width;
    if !chips.items.is_empty() {
        size = egui::vec2(
            size.x.max(chips.size.x),
            size.y + CHIPS_TOP_GAP + chips.size.y,
        );
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
    let padding = if is_image {
        Vec2::splat(IMAGE_PADDING)
    } else {
        theme::BUBBLE_PADDING
    };
    let bottom_padding = if is_image && !has_chips {
        IMAGE_PADDING
    } else if is_image {
        theme::BUBBLE_PADDING.y
    } else {
        padding.y
    };
    let chips_inset_x = if is_image {
        theme::BUBBLE_PADDING.x - IMAGE_PADDING
    } else {
        0.0
    };
    let width = (size.x + padding.x * 2.0).max(if has_chips {
        chips.size.x + theme::BUBBLE_PADDING.x * 2.0
    } else {
        0.0
    });
    BubbleLayout {
        bubble_size: egui::vec2(width, size.y + padding.y + bottom_padding),
        padding,
        chips_inset_x,
    }
}

/// What a painted bubble can be clicked: a file card, or the URL under the pointer.
struct PaintHit {
    card: Option<Rect>,
    link: Option<String>,
    #[cfg(test)]
    link_rects: Vec<Rect>,
}

impl PaintHit {
    fn none() -> Self {
        Self {
            card: None,
            link: None,
            #[cfg(test)]
            link_rects: Vec::new(),
        }
    }
}

fn layout_body(ui: &egui::Ui, text: &str, links: &[TextLink], wrap: f32) -> Arc<Galley> {
    if links.is_empty() {
        return ui.painter().layout(
            text.to_owned(),
            FontId::proportional(BODY_FONT_SIZE),
            theme::TEXT,
            wrap,
        );
    }
    let plain =
        egui::text::TextFormat::simple(FontId::proportional(BODY_FONT_SIZE), Color32::PLACEHOLDER);
    let mut linked =
        egui::text::TextFormat::simple(FontId::proportional(BODY_FONT_SIZE), theme::ACCENT_LIGHT);
    linked.underline = Stroke::new(1.0, theme::ACCENT_LIGHT);
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = wrap;
    job.break_on_newline = true;
    let mut cursor = 0;
    for link in links {
        if link.end > text.len() || link.start < cursor || link.start >= link.end {
            continue;
        }
        if link.start > cursor {
            job.append(&text[cursor..link.start], 0.0, plain.clone());
        }
        job.append(&text[link.start..link.end], 0.0, linked.clone());
        cursor = link.end;
    }
    if cursor < text.len() {
        job.append(&text[cursor..], 0.0, plain);
    }
    ui.painter().layout_job(job)
}

fn link_hit(ui: &egui::Ui, galley: &Galley, origin: Pos2, links: &[TextLink]) -> PaintHit {
    let link = ui
        .input(|input| input.pointer.latest_pos())
        .and_then(|pointer| link_under(galley, origin, pointer, links))
        .map(|link| link.url.clone());
    PaintHit {
        card: None,
        link,
        #[cfg(test)]
        link_rects: link_screen_rects(galley, origin, links),
    }
}

/// [`link_hit`] over every text of a markdown body.
fn markdown_link_hit(ui: &egui::Ui, rendered: &markdown::Rendered, origin: Pos2) -> PaintHit {
    let pointer = ui.input(|input| input.pointer.latest_pos());
    let link = pointer.and_then(|pointer| {
        rendered.texts.iter().find_map(|text| {
            link_under(&text.galley, origin + text.offset, pointer, &text.links)
                .map(|link| link.url.clone())
        })
    });
    PaintHit {
        card: None,
        link,
        #[cfg(test)]
        link_rects: rendered
            .texts
            .iter()
            .flat_map(|text| link_screen_rects(&text.galley, origin + text.offset, &text.links))
            .collect(),
    }
}

fn link_under<'a>(
    galley: &Galley,
    origin: Pos2,
    pointer: Pos2,
    links: &'a [TextLink],
) -> Option<&'a TextLink> {
    if links.is_empty() {
        return None;
    }
    let byte = byte_under(galley, pointer - origin)?;
    links
        .iter()
        .find(|link| (link.start..link.end).contains(&byte))
}

/// Byte offset of the glyph under a galley-local point. Misses the gaps between lines.
fn byte_under(galley: &Galley, local: Vec2) -> Option<usize> {
    let mut chars = galley.text().char_indices();
    for row in &galley.rows {
        let in_row = local.y >= row.min_y() && local.y < row.max_y();
        let x = local.x - row.pos.x;
        for glyph in &row.glyphs {
            let (byte, _) = chars.next()?;
            if in_row {
                let rect = glyph.logical_rect();
                if x >= rect.min.x && x < rect.max.x {
                    return Some(byte);
                }
            }
        }
        if row.ends_with_newline {
            chars.next()?;
        }
    }
    None
}

#[cfg(test)]
fn link_screen_rects(galley: &Galley, origin: Pos2, links: &[TextLink]) -> Vec<Rect> {
    if links.is_empty() {
        return Vec::new();
    }
    let mut rects = Vec::new();
    let mut chars = galley.text().char_indices();
    for row in &galley.rows {
        let mut run: Option<Rect> = None;
        for glyph in &row.glyphs {
            let Some((byte, _)) = chars.next() else {
                return rects;
            };
            let on_link = links
                .iter()
                .any(|link| (link.start..link.end).contains(&byte));
            if on_link && glyph.advance_width > 0.0 {
                let rect = glyph
                    .logical_rect()
                    .translate(row.pos.to_vec2())
                    .translate(origin.to_vec2());
                run = Some(match run {
                    Some(acc) => acc.union(rect),
                    None => rect,
                });
            } else if let Some(acc) = run.take() {
                rects.push(acc);
            }
        }
        if let Some(acc) = run.take() {
            rects.push(acc);
        }
        if row.ends_with_newline && chars.next().is_none() {
            return rects;
        }
    }
    rects
}

/// Shapes the lines of a long note that meet the clip, and paints them inside the estimated block.
fn paint_windowed_text(
    ui: &egui::Ui,
    text: &str,
    line_starts: &[u32],
    bubble: Rect,
    padding: Vec2,
    max_inner: f32,
) -> PaintHit {
    let line_count = line_starts.len();
    if line_count == 0 {
        return PaintHit::none();
    }
    let text_rect = Rect::from_min_size(
        bubble.min + padding,
        egui::vec2(max_inner, line_count as f32 * ESTIMATE_LINE_HEIGHT),
    );
    let view = ui.clip_rect().expand(TEXT_WINDOW_OVERSCAN);
    let visible = text_rect.intersect(view);
    if visible.height() <= 0.0 {
        return PaintHit::none();
    }
    let first = ((visible.top() - text_rect.top()) / ESTIMATE_LINE_HEIGHT)
        .floor()
        .max(0.0) as usize;
    let last = ((visible.bottom() - text_rect.top()) / ESTIMATE_LINE_HEIGHT).ceil() as usize;
    let first = first.min(line_count - 1);
    let last = last.clamp(first + 1, line_count);
    let start = line_starts[first] as usize;
    let mut end = if last < line_count {
        line_starts[last] as usize
    } else {
        text.len()
    };
    if end > start && text.as_bytes()[end - 1] == b'\n' {
        end -= 1;
    }
    if start >= end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return PaintHit::none();
    }
    let slice = &text[start..end];
    note_shaped(slice.len());
    let links = crate::ui::links::links_in_slice(text, start, end);
    let galley = layout_body(ui, slice, &links, max_inner);
    let origin = text_rect.min + egui::vec2(0.0, first as f32 * ESTIMATE_LINE_HEIGHT);
    let clip = text_rect.intersect(ui.clip_rect());
    ui.painter()
        .with_clip_rect(clip)
        .galley(origin, galley.clone(), theme::TEXT);
    link_hit(ui, &galley, origin, &links)
}

#[cfg(test)]
thread_local! {
    static SHAPED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn note_shaped(len: usize) {
    #[cfg(test)]
    SHAPED_BYTES.with(|shaped| {
        if len > shaped.get() {
            shaped.set(len);
        }
    });
    #[cfg(not(test))]
    let _ = len;
}

#[cfg(test)]
pub(crate) fn reset_shaped_bytes() {
    SHAPED_BYTES.set(0);
}

#[cfg(test)]
pub(crate) fn shaped_bytes() -> usize {
    SHAPED_BYTES.get()
}

/// Paints the content and reports a file card or the link under the pointer.
fn paint_content(ui: &egui::Ui, bubble: Rect, content: Content, padding: Vec2) -> PaintHit {
    match content {
        Content::Text { galley, links } => {
            let origin = bubble.min + padding;
            ui.painter().galley(origin, galley.clone(), theme::TEXT);
            link_hit(ui, &galley, origin, &links)
        }
        Content::Markdown(rendered) => {
            let origin = bubble.min + padding;
            rendered.paint(ui.painter(), origin);
            markdown_link_hit(ui, &rendered, origin)
        }
        Content::Image { size, loaded, uri } => {
            let rect = Rect::from_min_size(bubble.min + padding, size);
            paint_image(ui, rect, loaded, uri);
            PaintHit::none()
        }
        Content::Card { name, size } => {
            let origin = bubble.min + padding;
            let icon = Rect::from_min_size(origin, Vec2::splat(CARD_ICON_SIZE));
            ui.painter()
                .circle_filled(icon.center(), CARD_ICON_SIZE / 2.0, theme::ACCENT);
            ui.painter().text(
                icon.center(),
                Align2::CENTER_CENTER,
                "🗎",
                FontId::proportional(22.0),
                Color32::WHITE,
            );
            let text_left = icon.right() + CARD_GAP;
            ui.painter()
                .galley(Pos2::new(text_left, origin.y + 2.0), name, theme::TEXT);
            ui.painter().galley(
                Pos2::new(text_left, origin.y + CARD_ICON_SIZE - 18.0),
                size,
                theme::MUTED_TEXT,
            );
            PaintHit {
                card: Some(Rect::from_min_size(
                    origin,
                    egui::vec2(bubble.width() - padding.x * 2.0, CARD_ICON_SIZE),
                )),
                ..PaintHit::none()
            }
        }
    }
}

fn paint_image(ui: &egui::Ui, rect: Rect, loaded: Option<&LoadedAttachment>, uri: String) {
    let radius = CornerRadius::same(IMAGE_CORNER_RADIUS);
    let Some(loaded) = loaded else {
        ui.painter().rect_filled(rect, radius, theme::CHAT_BG);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Loading…",
            FontId::proportional(13.0),
            theme::MUTED_TEXT,
        );
        return;
    };
    egui::Image::from_bytes(uri, egui::load::Bytes::Shared(loaded.bytes.clone()))
        .corner_radius(radius)
        .paint_at(ui, rect);
}

/// Time + ticks: inline on the last text line, as an overlay pill on images, else on its own line.
fn paint_meta(ui: &egui::Ui, bubble: Rect, meta: Meta, is_image: bool) {
    let meta_size = meta.size;
    if is_image {
        let pill = Rect::from_min_size(
            bubble.max - Vec2::splat(IMAGE_PADDING + 6.0) - meta_size - META_PILL_PADDING,
            meta_size + META_PILL_PADDING * 2.0,
        );
        ui.painter()
            .rect_filled(pill, pill.height() / 2.0, theme::SERVICE_PILL);
        meta.paint(ui, pill.min + META_PILL_PADDING, Color32::WHITE);
        return;
    }
    meta.paint(
        ui,
        bubble.max - theme::BUBBLE_PADDING - meta_size,
        theme::MUTED_TEXT,
    );
}

/// Time + status tick(s). A delivered message draws its second tick overlapping the first
/// because a "✓✓" string is spaced too widely by the fallback font.
struct Meta {
    text: Arc<Galley>,
    second_tick: Option<(Arc<Galley>, f32)>,
    /// Edge length of the clock icon drawn after the time while the message is pending.
    clock: Option<f32>,
    size: Vec2,
}

impl Meta {
    fn paint(self, ui: &egui::Ui, origin: Pos2, fallback_color: Color32) {
        let painter = ui.painter();
        if let Some((tick, shift)) = self.second_tick {
            let tick_x = origin.x + self.text.size().x - tick.size().x + shift;
            painter.galley(Pos2::new(tick_x, origin.y), tick, fallback_color);
        }
        if let Some(edge) = self.clock {
            let top_left = Pos2::new(
                origin.x + self.text.size().x + CLOCK_GAP,
                origin.y + (self.text.size().y - edge) / 2.0,
            );
            let rect = Rect::from_min_size(top_left, Vec2::splat(edge));
            icons::clock(painter, rect, fallback_color);
        }
        painter.galley(origin, self.text, fallback_color);
    }
}

fn layout_meta(ui: &egui::Ui, message: &MessageView, color: Color32) -> Meta {
    let font = FontId::proportional(META_FONT_SIZE);
    let text = ui
        .painter()
        .layout_no_wrap(meta_text(message), font.clone(), color);
    let is_delivered =
        message.direction == Direction::Outgoing && message.status == DeliveryStatus::Delivered;
    if is_pending(message) {
        let edge = META_FONT_SIZE * CLOCK_SIZE_RATIO;
        let size = text.size() + egui::vec2(CLOCK_GAP + edge, 0.0);
        return Meta {
            text,
            second_tick: None,
            clock: Some(edge),
            size,
        };
    }
    if !is_delivered {
        let size = text.size();
        return Meta {
            text,
            second_tick: None,
            clock: None,
            size,
        };
    }
    let tick = ui.painter().layout_no_wrap("✓".to_owned(), font, color);
    let shift = tick.size().x * DOUBLE_TICK_SHIFT;
    let size = text.size() + egui::vec2(shift, 0.0);
    Meta {
        text,
        second_tick: Some((tick, shift)),
        clock: None,
        size,
    }
}

fn paint_check(ui: &egui::Ui, center: Pos2, selected: bool) {
    if selected {
        ui.painter()
            .circle_filled(center, CHECK_RADIUS, theme::ACCENT);
        ui.painter().text(
            center,
            Align2::CENTER_CENTER,
            "✓",
            FontId::proportional(14.0),
            Color32::WHITE,
        );
    } else {
        ui.painter()
            .circle_stroke(center, CHECK_RADIUS, Stroke::new(1.5, theme::MUTED_TEXT));
    }
}

fn fit_image(width: u32, height: u32) -> Vec2 {
    let natural = egui::vec2(width.max(1) as f32, height.max(1) as f32);
    let scale = (IMAGE_MAX_SIZE.x / natural.x)
        .min(IMAGE_MAX_SIZE.y / natural.y)
        .min(1.0);
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

fn is_pending(message: &MessageView) -> bool {
    message.direction == Direction::Outgoing && message.status == DeliveryStatus::Pending
}

fn meta_text(message: &MessageView) -> String {
    let time = time_fmt::clock(message.sent_at_ms);
    let marker = status_marker(message);
    if marker.is_empty() {
        time
    } else {
        format!("{time} {marker}")
    }
}

fn status_marker(message: &MessageView) -> &'static str {
    if message.direction == Direction::Incoming {
        return "";
    }
    match message.status {
        // Drawn as an icon (`Meta::paint`), not as text.
        DeliveryStatus::Pending => "",
        DeliveryStatus::Sent => "✓",
        DeliveryStatus::Delivered => "✓",
        DeliveryStatus::Failed => "⚠ not delivered",
    }
}

/// Quick reactions in a row plus a submenu with every emoji.
fn reaction_bar(ui: &mut egui::Ui, reactions: &ReactionsView, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        for emoji in QUICK_REACTIONS {
            let mine = reactions.mine.as_deref() == Some(emoji);
            let button = egui::Button::new(egui::RichText::new(emoji).size(REACTION_EMOJI_SIZE))
                .fill(if mine {
                    theme::ROW_SELECTED
                } else {
                    Color32::TRANSPARENT
                });
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

/// One chip per distinct emoji with how many picked it; ours is marked (a group counts every
/// member, a 1:1 chat has at most the contact's and ours).
fn chips(reactions: &ReactionsView) -> Vec<Chip> {
    let mut chips: Vec<Chip> = reactions
        .theirs
        .iter()
        .map(|emoji| (emoji.clone(), 1))
        .chain(
            reactions
                .others
                .iter()
                .map(|(emoji, count)| (emoji.clone(), *count as usize)),
        )
        .map(|(emoji, count)| Chip {
            emoji,
            count,
            includes_mine: false,
        })
        .collect();
    let Some(mine) = &reactions.mine else {
        return chips;
    };
    match chips.iter_mut().find(|chip| chip.emoji == *mine) {
        Some(chip) => {
            chip.count += 1;
            chip.includes_mine = true;
        }
        None => chips.push(Chip {
            emoji: mine.clone(),
            count: 1,
            includes_mine: true,
        }),
    }
    chips
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
        let emoji = ui.painter().layout_no_wrap(
            chip.emoji.clone(),
            FontId::proportional(CHIP_EMOJI_SIZE),
            Color32::WHITE,
        );
        let count = (chip.count > 1).then(|| {
            ui.painter().layout_no_wrap(
                chip.count.to_string(),
                FontId::proportional(CHIP_COUNT_SIZE),
                Color32::WHITE,
            )
        });
        let width = CHIP_PADDING_X * 2.0
            + emoji.size().x
            + count
                .as_ref()
                .map_or(0.0, |count| count.size().x + CHIP_INNER_GAP);
        if x > 0.0 && x + width > max_width {
            x = 0.0;
            y += CHIP_HEIGHT + CHIP_GAP;
        }
        let rect = Rect::from_min_size(egui::pos2(x, y), egui::vec2(width, CHIP_HEIGHT));
        x += width + CHIP_GAP;
        widest = widest.max(rect.right());
        items.push(ChipItem {
            chip,
            rect,
            emoji,
            count,
        });
    }
    let last_row_width = items.last().map_or(0.0, |item| item.rect.right());
    let height = items.last().map_or(0.0, |item| item.rect.bottom());
    ChipLayout {
        items,
        size: egui::vec2(widest, height),
        last_row_width,
    }
}

/// Paints the chips below the message text and makes them clickable.
fn paint_chips(
    ui: &mut egui::Ui,
    origin: Pos2,
    layout: ChipLayout,
    inputs: &Inputs,
    actions: &mut Vec<Action>,
) {
    for (index, item) in layout.items.into_iter().enumerate() {
        let rect = item.rect.translate(origin.to_vec2());
        let response = ui.interact(
            rect,
            ui.scope_id().with(("chip", inputs.index, index)),
            Sense::click(),
        );
        let hovered = response.hovered() && !inputs.selection_mode;
        let fill = if item.chip.includes_mine {
            theme::ACCENT
        } else {
            CHIP_PLAIN_FILL
        };
        let fill = if hovered {
            fill.gamma_multiply(1.25)
        } else {
            fill
        };
        ui.painter().rect_filled(rect, CHIP_HEIGHT / 2.0, fill);
        let center_y = rect.center().y;
        let emoji_size = item.emoji.size();
        let mut x = rect.left() + CHIP_PADDING_X;
        ui.painter().galley(
            Pos2::new(x, center_y - emoji_size.y / 2.0),
            item.emoji,
            Color32::WHITE,
        );
        x += emoji_size.x + CHIP_INNER_GAP;
        if let Some(count) = item.count {
            ui.painter().galley(
                Pos2::new(x, center_y - count.size().y / 2.0),
                count,
                Color32::WHITE,
            );
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
            Pos2::new(
                a * start.x + b * control.x + c * tip.x,
                a * start.y + b * control.y + c * tip.y,
            )
        })
        .collect();
    outline.push(base);
    // Not convex (the curve bows inward), so fill it as a triangle fan from an interior point.
    let center = Pos2::new(
        bubble.right() - TAIL_WIDTH / 2.0,
        bubble.bottom() - TAIL_HEIGHT / 3.0,
    );
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
    ui.painter().add(egui::Shape::line(
        outline[..=TAIL_CURVE_STEPS].to_vec(),
        Stroke::new(1.0, color),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(reactions: &ReactionsView) -> Vec<(String, usize, bool)> {
        chips(reactions)
            .into_iter()
            .map(|chip| (chip.emoji, chip.count, chip.includes_mine))
            .collect()
    }

    #[test]
    fn group_chips_count_members_and_mark_ours() {
        let reactions = ReactionsView {
            mine: Some("👍".into()),
            theirs: None,
            others: vec![("❤".into(), 3), ("👍".into(), 2)],
        };
        assert_eq!(
            summary(&reactions),
            vec![("❤".into(), 3, false), ("👍".into(), 3, true)]
        );
    }

    #[test]
    fn direct_chips_merge_the_same_emoji() {
        let same = ReactionsView {
            mine: Some("🔥".into()),
            theirs: Some("🔥".into()),
            others: Vec::new(),
        };
        assert_eq!(summary(&same), vec![("🔥".into(), 2, true)]);
        let different = ReactionsView {
            mine: Some("🔥".into()),
            theirs: Some("❤".into()),
            others: Vec::new(),
        };
        assert_eq!(
            summary(&different),
            vec![("❤".into(), 1, false), ("🔥".into(), 1, true)]
        );
    }
}
