//! Chat view: header (or selection bar), message bubbles grouped by sender and day (stuck to the
//! bottom), delete dialog and the composer. The message list is windowed: only rows near the
//! viewport are shaped, and a very long note shapes only its visible lines.
//! The header's search icon searches this room from the sidebar. A down button at the bottom right
//! appears while the list is scrolled up and jumps back to the newest message.
//! Message selection works Telegram-style: right-click → Select (or Ctrl+click), then click more
//! messages; Copy / Delete act on the selection, Esc leaves.

use std::collections::{HashMap, HashSet};

use brumm_core::{
    api::{Command, ContactView, Direction, MessageView},
    identity::UserKey,
    wire::MessageId,
};
use egui::{Color32, FontId, Rect, Sense, Vec2};

use crate::{
    state::AppState,
    ui::{
        avatar,
        bubble::{self, Action},
        chat_search::{self, Pending},
        composer::{self, Composer},
        files::{self, FileDialogs},
        sync_hint, theme, time_fmt,
    },
};

/// Fixed side padding of the message list, independent of the window width. The right side also
/// holds the selection markers, so selecting never shifts the bubbles.
const COLUMN_PADDING_LEFT: f32 = 17.0;
const COLUMN_PADDING_RIGHT: f32 = 22.0;
const DAY_PILL_PADDING: Vec2 = egui::vec2(12.0, 4.0);
const DAY_PILL_MARGIN: f32 = 10.0;
/// Day pill plus the margins around it, until the row has been measured.
const DAY_HEADER_ESTIMATE: f32 = DAY_PILL_MARGIN * 2.0 + 21.0;
const COPY_SEPARATOR: &str = "\n";
/// Laid out past the viewport so a small scroll does not pop bubbles in empty.
const WINDOW_OVERSCAN: f32 = 400.0;
const LIST_PADDING_TOP: f32 = 10.0;
const LIST_PADDING_BOTTOM: f32 = 10.0;
/// A measured height replaces the estimate only when it actually differs.
const HEIGHT_EPSILON: f32 = 1.0;
/// A later mouse-wheel notch within this window scrolls further than the one before it.
const WHEEL_BURST_S: f64 = 0.25;
/// Successive notches in one burst, as a multiple of the first notch.
const WHEEL_FACTOR_MAX: f32 = 8.0;
/// A point delta at least this large is one wheel notch, not a finger sample.
const DISCRETE_WHEEL_PX: f32 = 30.0;
/// Two-finger scrolling slower than this stays 1:1.
const TRACKPAD_PRECISE_SPEED: f32 = 400.0;
/// Finger speed at which two-finger scrolling reaches [`TRACKPAD_FACTOR_MAX`].
const TRACKPAD_FULL_SPEED: f32 = 2_200.0;
/// Fastest two-finger gain. A flick approaches this, a slow swipe does not.
const TRACKPAD_FACTOR_MAX: f32 = 4.0;
/// Cap on accelerated two-finger speed, in points per second.
const TRACKPAD_SPEED_CAP: f32 = 9_000.0;
/// How quickly the finger-speed estimate follows the latest sample.
const TRACKPAD_SPEED_BLEND: f32 = 0.55;
/// Avoid treating a stalled frame as an infinite finger speed.
const MIN_WHEEL_DT: f32 = 1.0 / 240.0;
/// A mouse notch is spread over this long when turning it into a coast speed.
const NOTCH_SPREAD_S: f32 = 0.10;
/// How quickly the release speed follows the gesture. One notch stays below the coast threshold.
const COAST_BLEND: f32 = 0.45;
/// Release speed below this stops dead, so a single precise notch does not glide.
const COAST_MIN_SPEED: f32 = 220.0;
/// Fastest glide, in points per second. Matches a short Telegram flick, not a fling across the history.
const COAST_SPEED_CAP: f32 = 2_500.0;
/// Time constant of the glide. Distance covered is about `speed * decay`.
const COAST_DECAY_S: f32 = 0.22;
/// Stop the glide once the remaining distance would be shorter than this.
const COAST_STOP_DISTANCE: f32 = 0.5;
/// A hitch must not turn one coast frame into a jump.
const COAST_MAX_DT: f32 = 1.0 / 30.0;
/// The jump-to-latest button stays hidden while the list is within this of the bottom.
const BOTTOM_SLACK: f32 = 24.0;
/// Older messages are requested when the viewport top comes this close to the first row,
/// so they are usually there before the user reaches the top.
const LOAD_OLDER_MARGIN: f32 = 1200.0;
const JUMP_BUTTON_DIAMETER: f32 = 44.0;
const JUMP_BUTTON_MARGIN: f32 = 14.0;
/// Large upward content move. The scroll area clamps it to the exact end, which re-enables
/// stick-to-bottom. A computed max offset can miss that equality by a fraction of a point.
const JUMP_TO_END_DELTA: f32 = -1.0e6;
/// Soft band behind the message a search result jumped to.
/// Accent light at alpha 40, stored premultiplied so it can be a constant.
const SEARCH_HIT_TINT: Color32 = Color32::from_rgba_premultiplied(0x21, 0x38, 0x4C, 80);
const SEARCH_ICON_BUTTON: f32 = 36.0;

/// View-local chat state: selection, delete confirmation and attachment bookkeeping.
#[derive(Default)]
pub struct ChatView {
    selection: Vec<MessageId>,
    /// Messages awaiting delete confirmation, and whether to also delete them for the contact.
    delete_dialog: Option<DeleteDialog>,
    requested_attachments: HashSet<(UserKey, MessageId)>,
    /// Chat whose composer still has to receive focus after it was opened.
    pending_focus: Option<UserKey>,
    focused_contact: Option<UserKey>,
    /// Pointer gesture in progress on the message list (click, shift/ctrl-click or drag-select).
    press: Option<Press>,
    /// Anchor for shift+click range selection. Index into the message list, not the visible window.
    last_clicked: Option<usize>,
    /// Heights for the open conversation, including the gap or day pill above each bubble.
    /// Off-screen rows keep an estimate and are measured only when they scroll into view.
    rows: Vec<CachedRow>,
    rows_contact: Option<UserKey>,
    /// Column width the cached heights were estimated for, in whole pixels.
    rows_width: i32,
    /// Scroll offset and bottom state of the last frame, to keep the view still when messages
    /// appear or vanish above it (older pages, reloads).
    last_offset: f32,
    was_at_bottom: bool,
    /// Height change of rows above the view measured this frame. The frame is discarded and
    /// laid out again with the offset moved by this much, so the visible rows never jump.
    measured_shift: f32,
    /// Message at the middle of the view last frame. Height changes above it are compensated,
    /// so what the user reads stays put while rows above get measured.
    view_anchor: Option<MessageId>,
    /// Oldest loaded message an older page was requested for; avoids asking twice.
    older_requested: Option<MessageId>,
    /// Wheel and two-finger acceleration for the message list.
    wheel: WheelAccel,
    /// In-chat search. While it is active the sidebar lists matches instead of contacts.
    pub(crate) search: chat_search::Session,
    #[cfg(test)]
    jump_visible: bool,
    #[cfg(test)]
    jump_rect: Option<Rect>,
    #[cfg(test)]
    row_rects: Vec<Rect>,
    #[cfg(test)]
    painted: Vec<MessageId>,
    /// Screen rects of link runs painted this frame.
    #[cfg(test)]
    link_rects: Vec<Rect>,
    pub dialogs: FileDialogs,
}

/// One message's vertical space in the windowed list.
struct CachedRow {
    id: MessageId,
    /// Changes when the text, attachment or reactions change, so the height is recomputed.
    stamp: u64,
    height: f32,
    /// Part of `height` above the bubble: the day pill or the gap to the previous message.
    /// It depends on the predecessor, so it is recomputed when older messages are prepended.
    separator: f32,
    /// Visual-line byte offsets for a long note. Empty for an ordinary message.
    line_starts: Vec<u32>,
    /// True after this row has been laid out, so `height` is no longer an estimate.
    measured: bool,
}

/// A primary-button press on a message; becomes a drag-selection once it reaches another row.
struct Press {
    /// Index into the full message list.
    anchor: usize,
    origin: egui::Pos2,
    /// Selection before the gesture, kept when Ctrl is held so a drag adds to it.
    base: Vec<MessageId>,
    dragging: bool,
}

/// Where one visible message row sits this frame.
struct RowSlot {
    /// Index into the full message list.
    index: usize,
    row: Rect,
    hit_area: Rect,
}

struct DeleteDialog {
    ids: Vec<MessageId>,
    for_everyone: bool,
    can_offer_everyone: bool,
}

impl ChatView {
    pub fn is_selecting(&self) -> bool {
        !self.selection.is_empty()
    }

    fn toggle(&mut self, id: MessageId) {
        match self.selection.iter().position(|selected| *selected == id) {
            Some(position) => {
                self.selection.remove(position);
            }
            None => self.selection.push(id),
        }
    }

    #[cfg(test)]
    pub fn select_for_test(&mut self, id: MessageId) {
        self.selection.push(id);
    }

    pub fn clear_selection(&mut self) {
        self.selection.clear();
    }
}

pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    chat: &mut ChatView,
    contact: UserKey,
    draft: &mut String,
    commands: &mut Vec<Command>,
) {
    let Some(view) = state.contact(&contact) else {
        return;
    };
    let messages = state
        .conversations
        .get(&contact)
        .map(Vec::as_slice)
        .unwrap_or_default();
    // Selection never outlives its conversation or deleted messages.
    chat.selection
        .retain(|id| messages.iter().any(|message| message.id == *id));
    chat.requested_attachments
        .retain(|(key, _)| *key == contact);
    chat.search.note_open_chat(Some(contact));
    if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
        if chat.is_selecting() {
            chat.clear_selection();
        } else if chat.delete_dialog.is_none() && chat.search.is_active() {
            chat.search.close();
        }
    }
    let find_key = ui.input(|input| {
        input.modifiers.command && !input.modifiers.shift && input.key_pressed(egui::Key::F)
    });
    if find_key && !chat.is_selecting() && chat.delete_dialog.is_none() {
        chat.search.open_or_focus(contact);
    }

    if chat.is_selecting() {
        selection_bar(ui, chat, messages);
    } else {
        header(ui, state, chat, view);
    }
    if chat.focused_contact != Some(contact) {
        chat.pending_focus = Some(contact);
    }
    chat.focused_contact = Some(contact);
    // The sidebar search field owns the keyboard while a room search is open.
    let grab_focus = chat.pending_focus == Some(contact) && !chat.search.is_active();
    egui::Panel::bottom("composer")
        .frame(
            egui::Frame::new()
                .fill(theme::SIDEBAR_BG)
                .inner_margin(egui::Margin::symmetric(12, 8)),
        )
        .show(ui, |ui| {
            let composer = Composer {
                contact,
                draft,
                dialogs: &chat.dialogs,
                grab_focus,
            };
            let has_focus = composer::show(ui, composer, commands);
            if has_focus && grab_focus {
                chat.pending_focus = None;
            }
        });

    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(theme::CHAT_BG))
        .show(ui, |ui| {
            if chat.search.pending() != Pending::None {
                // A jump replaces any glide left over from the previous gesture.
                chat.wheel.coast = 0.0;
            }
            apply_wheel_acceleration(ui, &mut chat.wheel);
            if chat.search.pending() != Pending::None {
                chat.wheel.coast = 0.0;
            }
            if chat.rows_contact != Some(contact) {
                // A chat opens at its newest message.
                chat.was_at_bottom = true;
            }
            let anchor_shift = anchor_shift(chat, contact, messages);
            let mut scroll_area = egui::ScrollArea::vertical()
                .auto_shrink(false)
                .animated(false)
                .stick_to_bottom(true);
            let shift = anchor_shift + std::mem::take(&mut chat.measured_shift);
            let anchoring = shift != 0.0 && !chat.was_at_bottom;
            let rubber_band = ui.spacing().scroll.kinetic.rubber_band;
            if anchoring {
                // Same frame as the change: the messages on screen do not move. The scroll
                // physics still has last frame's bounds, so the new offset would look like
                // overscroll and spring back; no rubber band for this one frame.
                scroll_area = scroll_area.vertical_scroll_offset(chat.last_offset + shift);
                ui.spacing_mut().scroll.kinetic.rubber_band = false;
            }
            let output = scroll_area.show_viewport(ui, |ui, viewport| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let column_width =
                    (ui.available_width() - COLUMN_PADDING_LEFT - COLUMN_PADDING_RIGHT).max(1.0);
                sync_row_heights(chat, contact, messages, column_width);
                let rows_height: f32 = chat.rows.iter().map(|row| row.height).sum();
                let total_height = LIST_PADDING_TOP + rows_height + LIST_PADDING_BOTTOM;
                ui.set_height(total_height);
                apply_pending_scroll(ui, chat, viewport.min.y, viewport.height(), total_height);
                // Scrolled up near the first row, or too few rows to scroll at all.
                let near_top = viewport.min.y < LOAD_OLDER_MARGIN && !chat.was_at_bottom;
                let fits = total_height <= viewport.height();
                if near_top || fits || chat.search.is_active() {
                    request_older(state, chat, contact, commands);
                }

                let (start, end, prefix) = visible_span(&chat.rows, viewport.min.y, viewport.max.y);
                if start == end {
                    return;
                }
                let visible_height: f32 = chat.rows[start..end].iter().map(|row| row.height).sum();
                let y_min = ui.max_rect().top() + LIST_PADDING_TOP + prefix;
                let rect = Rect::from_x_y_ranges(
                    (ui.max_rect().left() + COLUMN_PADDING_LEFT)
                        ..=(ui.max_rect().right() - COLUMN_PADDING_RIGHT),
                    y_min..=(y_min + visible_height).max(y_min + 1.0),
                );
                let slots = ui
                    .scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                        ui.set_width(rect.width());
                        ui.spacing_mut().item_spacing.y = 0.0;
                        paint_rows(ui, state, chat, contact, messages, start..end, commands)
                    })
                    .inner;
                let middle = ui.clip_rect().center().y;
                chat.view_anchor = slots
                    .iter()
                    .find(|slot| slot.row.y_range().contains(middle))
                    .or_else(|| slots.get(slots.len() / 2))
                    .map(|slot| messages[slot.index].id);
                handle_pointer(ui, chat, messages, &slots);
            });
            if let Some(&(received, expected)) = state.history_sync.get(&contact) {
                let progress = sync_hint::SyncProgress { received, expected };
                // Once one message exists, only the small pill shows (no switching back).
                let chat_is_empty = !state.has_messages(&contact);
                sync_hint::show(ui, output.inner_rect, &progress, chat_is_empty);
            }
            if chat.measured_shift != 0.0 {
                if chat.was_at_bottom {
                    chat.measured_shift = 0.0;
                } else {
                    ui.ctx()
                        .request_discard("chat rows above the view were measured");
                }
            }
            ui.spacing_mut().scroll.kinetic.rubber_band = rubber_band;
            chat.last_offset = output.state.clamped_offset().y;
            chat.was_at_bottom = !away_from_bottom(
                output.state.clamped_offset().y,
                output.content_size.y,
                output.inner_rect.height(),
            );
            let jumped = chat.search.take_jumped();
            chat.search
                .note_offset(output.state.clamped_offset().y, jumped);
            #[cfg(test)]
            {
                chat.jump_visible = false;
                chat.jump_rect = None;
            }
            if away_from_bottom(
                output.state.clamped_offset().y,
                output.content_size.y,
                output.inner_rect.height(),
            ) {
                let rect = jump_button_rect(output.inner_rect);
                #[cfg(test)]
                {
                    chat.jump_visible = true;
                    chat.jump_rect = Some(rect);
                }
                if jump_down_button(ui, rect).clicked() {
                    chat.search.jump_to_latest();
                }
            }
        });
    delete_dialog(ui, chat, contact, &view.display_name, commands);
}

fn header(ui: &mut egui::Ui, state: &AppState, chat: &mut ChatView, view: &ContactView) {
    egui::Panel::top("chat_header")
        .frame(
            egui::Frame::new()
                .fill(theme::SIDEBAR_BG)
                .inner_margin(egui::Margin::symmetric(16, 8)),
        )
        .show(ui, |ui| {
            ui.set_min_height(theme::TOP_BAR_HEIGHT - 16.0);
            ui.horizontal_centered(|ui| {
                let is_notes = state.is_notes(&view.user_key);
                let photo = if is_notes {
                    None
                } else {
                    state.avatars.get(&view.user_key)
                };
                avatar::show(
                    ui,
                    theme::HEADER_AVATAR_SIZE,
                    &view.user_key,
                    &view.display_name,
                    photo,
                );
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(egui::RichText::new(&view.display_name).size(16.0).strong());
                    let subtitle = if is_notes {
                        "visible on all your devices"
                    } else {
                        "end-to-end encrypted"
                    };
                    ui.label(
                        egui::RichText::new(subtitle)
                            .small()
                            .color(theme::MUTED_TEXT),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if search_button(ui, chat.search.is_active()) {
                        chat.search.toggle(view.user_key);
                    }
                    if is_notes {
                        return;
                    }
                    ui.label(
                        egui::RichText::new(&view.fingerprint)
                            .monospace()
                            .small()
                            .color(theme::MUTED_TEXT),
                    );
                    ui.label(
                        egui::RichText::new("✔ verified")
                            .small()
                            .color(theme::ACCENT_LIGHT),
                    )
                    .on_hover_text("You exchanged keys directly via invite");
                });
            });
        });
}

/// Magnifying glass at the top right of the room. Active means the sidebar is searching this room.
fn search_button(ui: &mut egui::Ui, active: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(SEARCH_ICON_BUTTON), Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.painter()
            .circle_filled(rect.center(), SEARCH_ICON_BUTTON / 2.0, theme::ROW_HOVER);
        ui.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let color = if active || hovered {
        theme::ACCENT_LIGHT
    } else {
        theme::MUTED_TEXT
    };
    let center = rect.center() + egui::vec2(-1.5, -1.5);
    let radius = 6.5;
    ui.painter()
        .circle_stroke(center, radius, egui::Stroke::new(1.6, color));
    let handle_from = center + egui::vec2(radius * 0.70, radius * 0.70);
    let handle_to = handle_from + egui::vec2(5.0, 5.0);
    ui.painter()
        .line_segment([handle_from, handle_to], egui::Stroke::new(1.8, color));
    let tip = if active {
        "Close search"
    } else {
        "Search messages"
    };
    response.on_hover_text(tip).clicked()
}

/// Where the message sits in content space so a jump lands it in the viewport.
fn jump_offset(row_top: f32, row_height: f32, viewport: f32, total: f32) -> f32 {
    let max_offset = (total - viewport).max(0.0);
    let desired = if row_height >= viewport {
        row_top
    } else {
        row_top - (viewport - row_height) * 0.5
    };
    desired.clamp(0.0, max_offset)
}

/// Asks for the next older page once per oldest message. Search needs every message, so an
/// active search loads all of them.
fn request_older(
    state: &AppState,
    chat: &mut ChatView,
    contact: UserKey,
    commands: &mut Vec<Command>,
) {
    let Some(oldest) = state
        .conversations
        .get(&contact)
        .and_then(|messages| messages.first())
    else {
        return;
    };
    if chat.older_requested == Some(oldest.id) {
        return;
    }
    if let Some(command) = state.older_command(contact, chat.search.is_active()) {
        chat.older_requested = Some(oldest.id);
        commands.push(command);
    }
}

/// How far the content above the first still-present message moved since the last frame
/// (older messages prepended, or rows above it removed). Rebuilds the row cache with the last
/// frame's width so the shift is known before the list is laid out.
fn anchor_shift(chat: &mut ChatView, contact: UserKey, messages: &[MessageView]) -> f32 {
    if chat.rows_contact != Some(contact) || chat.rows.is_empty() || rows_match(chat, messages) {
        return 0.0;
    }
    let new_index: HashMap<MessageId, usize> = messages
        .iter()
        .enumerate()
        .map(|(index, message)| (message.id, index))
        .collect();
    let Some((old_position, anchor)) = chat
        .rows
        .iter()
        .position(|row| new_index.contains_key(&row.id))
        .map(|position| (position, chat.rows[position].id))
    else {
        return 0.0;
    };
    // The anchor's bubble, not its row top: its own day pill may turn into a gap.
    let bubble_top = |rows: &[CachedRow], index: usize| {
        rows[..index].iter().map(|row| row.height).sum::<f32>() + rows[index].separator
    };
    let old_top = bubble_top(&chat.rows, old_position);
    sync_row_heights(chat, contact, messages, chat.rows_width as f32);
    bubble_top(&chat.rows, new_index[&anchor]) - old_top
}

fn rows_match(chat: &ChatView, messages: &[MessageView]) -> bool {
    chat.rows.len() == messages.len()
        && chat
            .rows
            .iter()
            .zip(messages)
            .all(|(row, message)| row.id == message.id && row.stamp == row_stamp(message))
}

fn away_from_bottom(offset: f32, content: f32, viewport: f32) -> bool {
    let max_offset = (content - viewport).max(0.0);
    max_offset - offset > BOTTOM_SLACK
}

fn apply_pending_scroll(
    ui: &egui::Ui,
    chat: &mut ChatView,
    offset: f32,
    viewport: f32,
    total: f32,
) {
    match chat.search.pending() {
        Pending::None => {}
        Pending::Bottom => {
            scroll_to_bottom(ui);
            chat.search.finish_jump();
        }
        Pending::Message(id) => {
            let Some(index) = chat.rows.iter().position(|row| row.id == id) else {
                chat.search.finish_jump();
                return;
            };
            let prefix: f32 = chat.rows[..index].iter().map(|row| row.height).sum();
            let row_height = chat.rows[index].height;
            let desired = jump_offset(LIST_PADDING_TOP + prefix, row_height, viewport, total);
            let max_offset = (total - viewport).max(0.0);
            if (desired - max_offset).abs() <= 1.0 {
                scroll_to_bottom(ui);
            } else {
                scroll_content_to(ui, offset, desired);
            }
            let measured = chat.rows[index].measured;
            if !chat.search.note_jump_attempt(measured) {
                ui.ctx().request_repaint();
            }
        }
    }
}

/// `current` and `desired` are scroll offsets. Positive offset is further down the history.
fn scroll_content_to(ui: &egui::Ui, current: f32, desired: f32) {
    let delta = current - desired;
    if delta.abs() < 0.5 {
        return;
    }
    ui.input_mut(|input| input.smooth_scroll_delta.y = 0.0);
    ui.scroll_with_delta_animation(egui::vec2(0.0, delta), egui::style::ScrollAnimation::none());
}

fn scroll_to_bottom(ui: &egui::Ui) {
    ui.input_mut(|input| input.smooth_scroll_delta.y = 0.0);
    ui.scroll_with_delta_animation(
        egui::vec2(0.0, JUMP_TO_END_DELTA),
        egui::style::ScrollAnimation::none(),
    );
}

fn jump_button_rect(area: Rect) -> Rect {
    let size = JUMP_BUTTON_DIAMETER;
    // Sit just left of the scrollbar so the thumb stays grabbable.
    let right_inset = JUMP_BUTTON_MARGIN + theme::SCROLLBAR_HOVER_WIDTH;
    let mut rect = Rect::from_min_size(
        egui::pos2(
            area.right() - right_inset - size,
            area.bottom() - JUMP_BUTTON_MARGIN - size,
        ),
        Vec2::splat(size),
    );
    if rect.top() < area.top() {
        rect = rect.translate(egui::vec2(0.0, area.top() - rect.top()));
    }
    if rect.left() < area.left() {
        rect = rect.translate(egui::vec2(area.left() - rect.left(), 0.0));
    }
    rect
}

fn jump_down_button(ui: &mut egui::Ui, rect: Rect) -> egui::Response {
    let response = ui.interact(rect, ui.make_persistent_id("jump_latest"), Sense::click());
    let center = rect.center();
    let radius = rect.width() / 2.0;
    ui.painter().circle_filled(
        center + egui::vec2(0.0, 1.0),
        radius,
        Color32::from_black_alpha(90),
    );
    let fill = if response.hovered() {
        Color32::from_rgb(0x2E, 0x3C, 0x4D)
    } else {
        theme::SIDEBAR_BG
    };
    ui.painter().circle_filled(center, radius, fill);
    ui.painter().circle_stroke(
        center,
        radius - 0.5,
        egui::Stroke::new(1.0, Color32::from_white_alpha(28)),
    );
    let color = if response.hovered() {
        theme::ACCENT_LIGHT
    } else {
        theme::TEXT
    };
    let mid = center + egui::vec2(0.0, 2.0);
    ui.painter().add(egui::Shape::line(
        vec![
            mid + egui::vec2(-6.0, -3.5),
            mid,
            mid + egui::vec2(6.0, -3.5),
        ],
        egui::Stroke::new(2.0, color),
    ));
    if response.hovered() {
        ui.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response.on_hover_text("Jump to latest")
}

/// Replaces the header while messages are selected: count, Copy, Delete, close.
fn selection_bar(ui: &mut egui::Ui, chat: &mut ChatView, messages: &[MessageView]) {
    egui::Panel::top("chat_header")
        .frame(
            egui::Frame::new()
                .fill(theme::SIDEBAR_BG)
                .inner_margin(egui::Margin::symmetric(16, 8)),
        )
        .show(ui, |ui| {
            ui.set_min_height(theme::TOP_BAR_HEIGHT - 16.0);
            ui.horizontal_centered(|ui| {
                if ui
                    .add(egui::Button::new("✕").fill(Color32::TRANSPARENT))
                    .on_hover_text("Cancel (Esc)")
                    .clicked()
                {
                    chat.clear_selection();
                }
                ui.label(
                    egui::RichText::new(format!("{} selected", chat.selection.len()))
                        .size(16.0)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(theme::danger_button("Delete")).clicked() {
                        open_delete_dialog(chat, messages, chat.selection.clone());
                    }
                    let text = selected_text(messages, &chat.selection);
                    if ui
                        .add_enabled(!text.is_empty(), egui::Button::new("Copy"))
                        .on_hover_text("Copy text (Ctrl+C)")
                        .clicked()
                    {
                        ui.ctx().copy_text(text);
                    }
                });
            });
        });
    if ui.input(|input| {
        input
            .events
            .iter()
            .any(|event| matches!(event, egui::Event::Copy))
    }) {
        ui.ctx().copy_text(selected_text(messages, &chat.selection));
    }
    let select_all = ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::A));
    if select_all {
        chat.selection = messages.iter().map(|message| message.id).collect();
    }
}

/// Texts of the selected messages in conversation order.
fn selected_text(messages: &[MessageView], selection: &[MessageId]) -> String {
    messages
        .iter()
        .filter(|message| selection.contains(&message.id) && !message.text.is_empty())
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>()
        .join(COPY_SEPARATOR)
}

fn open_delete_dialog(chat: &mut ChatView, messages: &[MessageView], ids: Vec<MessageId>) {
    let all_outgoing = ids.iter().all(|id| {
        messages
            .iter()
            .any(|message| message.id == *id && message.direction == Direction::Outgoing)
    });
    chat.delete_dialog = Some(DeleteDialog {
        ids,
        for_everyone: all_outgoing,
        can_offer_everyone: all_outgoing,
    });
}

fn delete_dialog(
    ui: &mut egui::Ui,
    chat: &mut ChatView,
    contact: UserKey,
    name: &str,
    commands: &mut Vec<Command>,
) {
    let Some(dialog) = chat.delete_dialog.as_mut() else {
        return;
    };
    let (mut confirmed, mut cancelled) = (false, false);
    let count = dialog.ids.len();
    egui::Modal::new(egui::Id::unique("delete_messages")).show(ui.ctx(), |ui| {
        ui.set_width(320.0);
        let title = if count == 1 {
            "Delete message?".to_owned()
        } else {
            format!("Delete {count} messages?")
        };
        ui.heading(title);
        ui.add_space(8.0);
        if dialog.can_offer_everyone {
            ui.checkbox(&mut dialog.for_everyone, format!("Also delete for {name}"));
        } else {
            ui.label(
                egui::RichText::new("This removes the messages from your devices only.")
                    .color(theme::MUTED_TEXT),
            );
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                confirmed = ui.add(theme::danger_button("Delete")).clicked();
                cancelled = ui.button("Cancel").clicked();
            });
        });
    });
    if confirmed {
        commands.push(Command::DeleteMessages {
            contact,
            ids: dialog.ids.clone(),
            for_everyone: dialog.for_everyone,
        });
        chat.delete_dialog = None;
        chat.clear_selection();
    } else if cancelled || ui.input(|input| input.key_pressed(egui::Key::Escape)) {
        chat.delete_dialog = None;
    }
}

/// Remembers how fast the current wheel or two-finger gesture is going.
#[derive(Clone, Copy)]
struct WheelAccel {
    factor: f32,
    last_event_s: f64,
    /// +1 or -1 while a gesture is running, 0 when it has lapsed.
    last_sign: i8,
    /// Smoothed two-finger speed, in points per second.
    speed: f32,
    /// Release velocity in content space (same sign as a wheel delta), points per second.
    coast: f32,
    /// Pixels of wheel input this frame, already multiplied by [`Self::factor`].
    impulse: f32,
    impulse_discrete: bool,
    impulse_continuous: bool,
}

impl Default for WheelAccel {
    fn default() -> Self {
        Self {
            factor: 1.0,
            last_event_s: f64::NEG_INFINITY,
            last_sign: 0,
            speed: 0.0,
            coast: 0.0,
            impulse: 0.0,
            impulse_discrete: false,
            impulse_continuous: false,
        }
    }
}

/// Scales this frame's scroll delta, then lets the list glide after the gesture ends.
/// A mouse notch repeats further than the last one while the wheel is still spinning.
/// A two-finger swipe grows with finger speed and stays 1:1 when the swipe is slow.
fn apply_wheel_acceleration(ui: &mut egui::Ui, wheel: &mut WheelAccel) {
    if ui.input(|input| input.pointer.any_down()) {
        wheel.coast = 0.0;
        return;
    }
    let here = ui.rect_contains_pointer(ui.max_rect());
    let zooming = ui.input(|input| input.modifiers.command);
    let line_px = ui
        .ctx()
        .options(|options| options.input_options.line_scroll_speed);
    let page_px = ui.clip_rect().height();
    let (samples, time, dt, smooth) = ui.input(|input| {
        let mut samples = Vec::new();
        for event in &input.events {
            let egui::Event::MouseWheel {
                unit,
                delta,
                phase,
                source,
                ..
            } = event
            else {
                continue;
            };
            if *phase != egui::TouchPhase::Move || *source == egui::MouseWheelSource::Momentum {
                continue;
            }
            let pixels = match unit {
                egui::MouseWheelUnit::Point => delta.y,
                egui::MouseWheelUnit::Line => delta.y * line_px,
                egui::MouseWheelUnit::Page => delta.y * page_px,
            };
            if pixels == 0.0 {
                continue;
            }
            let discrete =
                !matches!(unit, egui::MouseWheelUnit::Point) || pixels.abs() >= DISCRETE_WHEEL_PX;
            samples.push((pixels, discrete));
        }
        (
            samples,
            input.time,
            input.stable_dt,
            input.smooth_scroll_delta.y,
        )
    });
    if !here || zooming {
        // The wheel belongs to whatever is under the pointer. Don't carry our glide into that.
        if !samples.is_empty() || smooth.abs() > 0.5 {
            wheel.coast = 0.0;
        } else {
            glide(ui, wheel, dt);
        }
        return;
    }
    update_wheel(wheel, time, dt, &samples);
    if !samples.is_empty() {
        let factor = wheel.factor;
        if factor != 1.0 {
            ui.input_mut(|input| input.smooth_scroll_delta.y *= factor);
        }
        return;
    }
    // The notch or swipe has stopped. A fast release keeps moving on its own, and the smoothed
    // tail is dropped so the list does not stop and then kick. A single precise notch keeps the tail.
    if wheel.coast.abs() >= COAST_MIN_SPEED {
        ui.input_mut(|input| input.smooth_scroll_delta.y = 0.0);
        glide(ui, wheel, dt);
    } else if smooth.abs() <= 0.5 {
        wheel.coast = 0.0;
    }
}

fn glide(ui: &mut egui::Ui, wheel: &mut WheelAccel, dt: f32) {
    let step = glide_step(&mut wheel.coast, dt);
    if step == 0.0 {
        return;
    }
    ui.scroll_with_delta_animation(egui::vec2(0.0, step), egui::style::ScrollAnimation::none());
    ui.ctx().request_repaint();
}

/// One frame of the release glide. Returns the content delta to apply, and decays `velocity`.
fn glide_step(velocity: &mut f32, dt: f32) -> f32 {
    if velocity.abs() < COAST_MIN_SPEED {
        *velocity = 0.0;
        return 0.0;
    }
    let dt = dt.clamp(MIN_WHEEL_DT, COAST_MAX_DT);
    let step = *velocity * dt;
    *velocity *= (-dt / COAST_DECAY_S).exp();
    if velocity.abs() * COAST_DECAY_S < COAST_STOP_DISTANCE {
        *velocity = 0.0;
    }
    step
}

/// `(delta points, discrete notch)` samples that arrived together.
fn update_wheel(state: &mut WheelAccel, now: f64, dt: f32, samples: &[(f32, bool)]) {
    state.impulse = 0.0;
    state.impulse_discrete = false;
    state.impulse_continuous = false;
    if samples.is_empty() {
        if now - state.last_event_s > WHEEL_BURST_S {
            state.factor = 1.0;
            state.speed = 0.0;
            state.last_sign = 0;
        }
        return;
    }
    let mut index = 0;
    while index < samples.len() {
        if samples[index].1 {
            note_wheel_notch(state, now, samples[index].0);
            index += 1;
            continue;
        }
        let mut sum = 0.0;
        while index < samples.len() && !samples[index].1 {
            sum += samples[index].0;
            index += 1;
        }
        note_finger_scroll(state, now, dt, sum);
    }
    if state.impulse != 0.0 {
        let spread = if state.impulse_discrete && !state.impulse_continuous {
            NOTCH_SPREAD_S
        } else {
            dt.max(MIN_WHEEL_DT)
        };
        let instant = (state.impulse / spread).clamp(-COAST_SPEED_CAP, COAST_SPEED_CAP);
        state.coast = state.coast * (1.0 - COAST_BLEND) + instant * COAST_BLEND;
    }
}

fn note_wheel_notch(state: &mut WheelAccel, now: f64, delta: f32) {
    let sign = scroll_sign(delta);
    let burst = state.last_sign == sign && now - state.last_event_s <= WHEEL_BURST_S;
    state.factor = if burst {
        (state.factor + 1.0).min(WHEEL_FACTOR_MAX)
    } else {
        1.0
    };
    state.speed = 0.0;
    state.impulse += delta * state.factor;
    state.impulse_discrete = true;
    state.last_event_s = now;
    state.last_sign = sign;
}

fn note_finger_scroll(state: &mut WheelAccel, now: f64, dt: f32, delta: f32) {
    let sign = scroll_sign(delta);
    if state.last_sign != sign {
        state.speed = 0.0;
    }
    let instant = delta.abs() / dt.max(MIN_WHEEL_DT);
    state.speed = state.speed * (1.0 - TRACKPAD_SPEED_BLEND) + instant * TRACKPAD_SPEED_BLEND;
    let t = ((state.speed - TRACKPAD_PRECISE_SPEED)
        / (TRACKPAD_FULL_SPEED - TRACKPAD_PRECISE_SPEED))
        .clamp(0.0, 1.0);
    let mut factor = 1.0 + (TRACKPAD_FACTOR_MAX - 1.0) * t * t;
    if instant > 1.0 {
        factor = factor.min(TRACKPAD_SPEED_CAP / instant);
    }
    state.factor = factor.max(1.0);
    state.impulse += delta * state.factor;
    state.impulse_continuous = true;
    state.last_event_s = now;
    state.last_sign = sign;
}

fn scroll_sign(delta: f32) -> i8 {
    if delta > 0.0 { 1 } else { -1 }
}

fn row_stamp(message: &MessageView) -> u64 {
    let mut stamp = message.text.len() as u64;
    if message.reactions.mine.is_some() {
        stamp |= 1u64 << 62;
    }
    if message.reactions.theirs.is_some() {
        stamp |= 1u64 << 63;
    }
    if let Some(attachment) = &message.attachment {
        stamp ^= (attachment.size as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        stamp ^= (attachment.name.len() as u64) << 32;
    }
    stamp
}

fn estimate_row(
    previous: Option<&MessageView>,
    message: &MessageView,
    column_width: f32,
) -> (f32, f32, Vec<u32>) {
    let (bubble, line_starts) = bubble::estimated_height(message, column_width);
    let separator = estimate_separator(previous, message);
    ((bubble + separator).max(1.0), separator, line_starts)
}

/// Day pill when the day changes, otherwise the gap between bubbles (none for the first row).
fn estimate_separator(previous: Option<&MessageView>, message: &MessageView) -> f32 {
    let day = time_fmt::day_of(message.sent_at_ms);
    let new_day = previous.is_none_or(|prev| time_fmt::day_of(prev.sent_at_ms) != day);
    if new_day && day.is_some() {
        return DAY_HEADER_ESTIMATE;
    }
    match previous {
        Some(prev) if prev.direction == message.direction => theme::GROUP_GAP,
        Some(_) => theme::SENDER_CHANGE_GAP,
        None => 0.0,
    }
}

/// Keeps one height per message. Unseen rows get an estimate that does not shape their text.
fn sync_row_heights(
    chat: &mut ChatView,
    contact: UserKey,
    messages: &[MessageView],
    column_width: f32,
) {
    let width = column_width.round() as i32;
    if chat.rows_contact != Some(contact) || chat.rows_width != width {
        chat.rows.clear();
        chat.rows_contact = Some(contact);
        chat.rows_width = width;
    }
    if rows_match(chat, messages) {
        return;
    }
    let mut previous: HashMap<MessageId, (CachedRow, Option<MessageId>)> = HashMap::new();
    let mut prior_id = None;
    for row in chat.rows.drain(..) {
        let id = row.id;
        previous.insert(id, (row, prior_id));
        prior_id = Some(id);
    }
    let mut prev_message: Option<&MessageView> = None;
    chat.rows.reserve(messages.len());
    for message in messages {
        let stamp = row_stamp(message);
        let row = match previous.remove(&message.id) {
            Some((mut cached, prior)) if cached.stamp == stamp => {
                // A new predecessor (older page prepended, or one deleted) can turn a day pill
                // into a plain gap: swap only the separator, keep the measured bubble height.
                if prior != prev_message.map(|prev| prev.id) {
                    let separator = estimate_separator(prev_message, message);
                    cached.height = (cached.height - cached.separator + separator).max(1.0);
                    cached.separator = separator;
                }
                cached
            }
            _ => {
                let (height, separator, line_starts) =
                    estimate_row(prev_message, message, column_width);
                CachedRow {
                    id: message.id,
                    stamp,
                    height,
                    separator,
                    line_starts,
                    measured: false,
                }
            }
        };
        chat.rows.push(row);
        prev_message = Some(message);
    }
}

/// `(first_row, end_row, height of the rows above first_row)`.
fn visible_span(rows: &[CachedRow], view_min: f32, view_max: f32) -> (usize, usize, f32) {
    let top = (view_min - WINDOW_OVERSCAN).max(0.0);
    let bottom = view_max + WINDOW_OVERSCAN;
    let mut y = LIST_PADDING_TOP;
    let mut start = 0;
    while start < rows.len() && y + rows[start].height < top {
        y += rows[start].height;
        start += 1;
    }
    let prefix = y - LIST_PADDING_TOP;
    let mut end = start;
    while end < rows.len() && y < bottom {
        y += rows[end].height;
        end += 1;
    }
    (start, end, prefix.max(0.0))
}

fn paint_rows(
    ui: &mut egui::Ui,
    state: &AppState,
    chat: &mut ChatView,
    contact: UserKey,
    messages: &[MessageView],
    range: std::ops::Range<usize>,
    commands: &mut Vec<Command>,
) -> Vec<RowSlot> {
    #[cfg(test)]
    chat.link_rects.clear();
    let mut slots = Vec::with_capacity(range.end - range.start);
    let mut heights_changed = false;
    let anchor_index = chat
        .view_anchor
        .and_then(|anchor| messages.iter().position(|message| message.id == anchor));
    for index in range {
        let message = &messages[index];
        let previous = index.checked_sub(1).map(|prev| &messages[prev]);
        let top = ui.cursor().min.y;
        let day = time_fmt::day_of(message.sent_at_ms);
        let new_day = previous.is_none_or(|prev| time_fmt::day_of(prev.sent_at_ms) != day);
        if let (true, Some(day)) = (new_day, day) {
            day_pill(ui, &time_fmt::day_label(day));
        } else if let Some(prev) = previous {
            let gap = if prev.direction == message.direction {
                theme::GROUP_GAP
            } else {
                theme::SENDER_CHANGE_GAP
            };
            ui.add_space(gap);
        }
        let ends_group = messages.get(index + 1).is_none_or(|next| {
            next.direction != message.direction || time_fmt::day_of(next.sent_at_ms) != day
        });

        let attachment = state.attachments.get(&(contact, message.id));
        request_preview(
            chat,
            contact,
            message,
            attachment.is_some(),
            files::preview_max_edge(ui.pixels_per_point()),
            commands,
        );
        let highlight = chat.search.highlighted() == Some(message.id);
        let band = highlight.then(|| ui.painter().add(egui::Shape::Noop));
        let bubble_top = ui.cursor().min.y;
        let output = {
            let inputs = bubble::Inputs {
                index,
                ends_group,
                attachment,
                selection_mode: chat.is_selecting(),
                selected: chat.selection.contains(&message.id),
                line_starts: &chat.rows[index].line_starts,
            };
            bubble::show(ui, message, &inputs)
        };
        if let Some(band) = band {
            let rect =
                Rect::from_x_y_ranges(ui.clip_rect().x_range(), bubble_top..=ui.cursor().min.y);
            ui.painter()
                .set(band, egui::Shape::rect_filled(rect, 0.0, SEARCH_HIT_TINT));
        }
        chat.rows[index].measured = true;
        chat.rows[index].separator = bubble_top - top;
        let used = (ui.cursor().min.y - top).max(1.0);
        if (used - chat.rows[index].height).abs() > HEIGHT_EPSILON
            && anchor_index.is_some_and(|anchor| index < anchor)
        {
            // Above the message being read: its new height would push that message.
            chat.measured_shift += used - chat.rows[index].height;
        }
        if (used - chat.rows[index].height).abs() > HEIGHT_EPSILON {
            chat.rows[index].height = used;
            heights_changed = true;
        }
        slots.push(RowSlot {
            index,
            row: output.row,
            hit_area: output.hit_area,
        });
        #[cfg(test)]
        chat.link_rects.extend(output.link_rects);
        for action in output.actions {
            apply_action(ui, chat, contact, messages, message, action, commands);
        }
    }
    if heights_changed {
        ui.ctx().request_repaint();
    }
    #[cfg(test)]
    {
        chat.row_rects = slots.iter().map(|slot| slot.hit_area).collect();
        chat.painted = slots.iter().map(|slot| messages[slot.index].id).collect();
    }
    slots
}

const DRAG_START_DISTANCE: f32 = 4.0;
const AUTOSCROLL_SPEED: f32 = 14.0;

/// Telegram-style gestures: drag over messages to select them, Shift+click selects a range,
/// Ctrl+click toggles one, and a plain click toggles while a selection exists.
fn handle_pointer(
    ui: &mut egui::Ui,
    chat: &mut ChatView,
    messages: &[MessageView],
    slots: &[RowSlot],
) {
    let (pressed, down, released, position, modifiers) = ui.input(|input| {
        (
            input.pointer.primary_pressed(),
            input.pointer.primary_down(),
            input.pointer.primary_released(),
            input.pointer.latest_pos(),
            input.modifiers,
        )
    });
    let Some(position) = position else { return };
    let selecting = chat.is_selecting();
    let in_viewport = ui.clip_rect().contains(position);

    if pressed && in_viewport {
        chat.press = slots
            .iter()
            .find(|slot| {
                let area = if selecting { slot.row } else { slot.hit_area };
                area.contains(position)
            })
            .map(|slot| Press {
                anchor: slot.index,
                origin: position,
                base: if modifiers.command {
                    chat.selection.clone()
                } else {
                    Vec::new()
                },
                dragging: false,
            });
    }
    let Some(press) = chat.press.as_mut() else {
        return;
    };

    if down {
        let current = row_nearest_to(slots, position.y);
        let moved = (position - press.origin).length() > DRAG_START_DISTANCE;
        if moved && current.is_some_and(|current| current != press.anchor) {
            press.dragging = true;
        }
        if let (true, Some(current)) = (press.dragging, current) {
            let (anchor, base) = (press.anchor, press.base.clone());
            select_range(chat, messages, anchor, current, &base);
            autoscroll(ui, position.y);
        }
    }
    if !released {
        return;
    }
    let Some(press) = chat.press.take() else {
        return;
    };
    if press.dragging {
        chat.last_clicked = Some(press.anchor);
        return;
    }
    let clicked_in_place = (position - press.origin).length() <= DRAG_START_DISTANCE;
    if !clicked_in_place || press.anchor >= messages.len() {
        return;
    }
    if modifiers.shift {
        let from = chat
            .last_clicked
            .unwrap_or(press.anchor)
            .min(messages.len() - 1);
        select_range(chat, messages, from, press.anchor, &[]);
    } else if modifiers.command || selecting {
        chat.toggle(messages[press.anchor].id);
    } else {
        return;
    }
    chat.last_clicked = Some(press.anchor);
}

/// Selects every message from `from` through `to`, keeping `base` when a drag adds to it.
fn select_range(
    chat: &mut ChatView,
    messages: &[MessageView],
    from: usize,
    to: usize,
    base: &[MessageId],
) {
    let Some(last) = messages.len().checked_sub(1) else {
        return;
    };
    let (low, high) = (from.min(to).min(last), from.max(to).min(last));
    let mut selection = base.to_vec();
    for message in &messages[low..=high] {
        if !selection.contains(&message.id) {
            selection.push(message.id);
        }
    }
    chat.selection = selection;
}

/// The message index whose row is closest to `y` (so dragging above or below the list still works).
fn row_nearest_to(slots: &[RowSlot], y: f32) -> Option<usize> {
    slots
        .iter()
        .min_by(|a, b| distance_to_span(a.row, y).total_cmp(&distance_to_span(b.row, y)))
        .map(|slot| slot.index)
}

fn distance_to_span(row: Rect, y: f32) -> f32 {
    if y < row.top() {
        row.top() - y
    } else if y > row.bottom() {
        y - row.bottom()
    } else {
        0.0
    }
}

/// Scrolls the list while a drag-selection is held above or below the visible area.
fn autoscroll(ui: &mut egui::Ui, pointer_y: f32) {
    let viewport = ui.clip_rect();
    let delta = if pointer_y < viewport.top() {
        AUTOSCROLL_SPEED
    } else if pointer_y > viewport.bottom() {
        -AUTOSCROLL_SPEED
    } else {
        return;
    };
    ui.scroll_with_delta(egui::vec2(0.0, delta));
    ui.ctx().request_repaint();
}

/// Image messages load their bytes once, when first shown.
fn request_preview(
    chat: &mut ChatView,
    contact: UserKey,
    message: &MessageView,
    loaded: bool,
    max_edge: u32,
    commands: &mut Vec<Command>,
) {
    let is_image = message
        .attachment
        .as_ref()
        .is_some_and(|attachment| files::is_image_name(&attachment.name));
    if is_image && !loaded && chat.requested_attachments.insert((contact, message.id)) {
        commands.push(Command::LoadAttachment {
            contact,
            id: message.id,
            max_edge,
        });
    }
}

fn apply_action(
    ui: &egui::Ui,
    chat: &mut ChatView,
    contact: UserKey,
    messages: &[MessageView],
    message: &MessageView,
    action: Action,
    commands: &mut Vec<Command>,
) {
    match action {
        Action::React(emoji) => {
            // Picking the emoji you already reacted with removes the reaction, like Telegram.
            let emoji = if message.reactions.mine.as_deref() == Some(emoji.as_str()) {
                String::new()
            } else {
                emoji
            };
            commands.push(Command::React {
                contact,
                id: message.id,
                emoji,
            });
        }
        Action::ToggleSelect => chat.toggle(message.id),
        Action::Copy => ui.ctx().copy_text(message.text.clone()),
        Action::SaveAs => {
            let Some(attachment) = &message.attachment else {
                return;
            };
            chat.dialogs
                .pick_save_path(ui.ctx(), contact, message.id, attachment.name.clone());
        }
        Action::Delete => {
            let ids = if chat.selection.contains(&message.id) {
                chat.selection.clone()
            } else {
                vec![message.id]
            };
            open_delete_dialog(chat, messages, ids);
        }
    }
}

fn day_pill(ui: &mut egui::Ui, label: &str) {
    ui.add_space(DAY_PILL_MARGIN);
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_owned(), FontId::proportional(13.0), Color32::WHITE);
    let size = galley.size() + DAY_PILL_PADDING * 2.0;
    let (row, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), size.y), Sense::hover());
    let pill = Rect::from_center_size(row.center(), size);
    ui.painter()
        .rect_filled(pill, size.y / 2.0, theme::SERVICE_PILL);
    ui.painter()
        .galley(pill.min + DAY_PILL_PADDING, galley, Color32::WHITE);
    ui.add_space(DAY_PILL_MARGIN);
}

#[cfg(test)]
mod tests {
    use brumm_core::api::{ContactView, DeliveryStatus, Event, Trust};

    use super::*;

    const CONTACT: UserKey = [1; 32];

    fn state() -> AppState {
        let mut state = AppState {
            ready: true,
            selected: Some(CONTACT),
            ..Default::default()
        };
        state.contacts.push(ContactView {
            user_key: CONTACT,
            display_name: "Bob".into(),
            fingerprint: "ab".into(),
            trust: Trust::Verified,
            added_at_ms: 0,
        });
        let messages = (1..=5u8)
            .map(|index| MessageView {
                id: [index; 16],
                contact: CONTACT,
                direction: if index % 2 == 0 {
                    Direction::Outgoing
                } else {
                    Direction::Incoming
                },
                text: format!("message {index}"),
                attachment: None,
                reactions: Default::default(),
                sent_at_ms: 1_700_000_000_000 + u64::from(index) * 60_000,
                status: DeliveryStatus::Delivered,
            })
            .collect();
        state.conversations.insert(CONTACT, messages);
        state
    }

    struct Harness {
        ctx: egui::Context,
        state: AppState,
        chat: ChatView,
        draft: String,
        sent: Vec<Command>,
        opened_urls: Vec<String>,
    }

    impl Harness {
        fn new() -> Self {
            let ctx = egui::Context::default();
            crate::ui::theme::apply(&ctx);
            Self {
                ctx,
                state: state(),
                chat: ChatView::default(),
                draft: String::new(),
                sent: Vec::new(),
                opened_urls: Vec::new(),
            }
        }

        fn frame(&mut self, mut events: Vec<egui::Event>, modifiers: egui::Modifiers) {
            events.insert(0, egui::Event::ModifiersChanged(modifiers));
            let screen_rect = Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            ));
            let input = egui::RawInput {
                screen_rect,
                events,
                ..Default::default()
            };
            let Self {
                ctx,
                state,
                chat,
                draft,
                sent,
                opened_urls,
            } = self;
            let mut output = ctx.run_ui(input, |ui| {
                let mut commands = Vec::new();
                show(ui, state, chat, CONTACT, draft, &mut commands);
                sent.extend(commands);
            });
            *opened_urls = output
                .platform_output
                .commands
                .iter()
                .filter_map(|command| match command {
                    egui::OutputCommand::OpenUrl(open) => Some(open.url.clone()),
                    _ => None,
                })
                .collect();
            output.textures_delta.clear();
        }

        fn center(&self, row: usize) -> egui::Pos2 {
            self.chat.row_rects[row].center()
        }

        fn press(&mut self, at: egui::Pos2, modifiers: egui::Modifiers) {
            self.frame(vec![egui::Event::PointerMoved(at)], modifiers);
            self.frame(vec![button(at, true, modifiers)], modifiers);
        }

        fn release(&mut self, at: egui::Pos2, modifiers: egui::Modifiers) {
            self.frame(vec![egui::Event::PointerMoved(at)], modifiers);
            self.frame(vec![button(at, false, modifiers)], modifiers);
        }

        fn selected(&self) -> Vec<u8> {
            self.chat.selection.iter().map(|id| id[0]).collect()
        }
    }

    fn button(pos: egui::Pos2, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers,
        }
    }

    fn warmed_up() -> Harness {
        let mut harness = Harness::new();
        for _ in 0..3 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        harness
    }

    #[test]
    fn drag_over_messages_selects_the_range() {
        let mut harness = warmed_up();
        let (start, end) = (harness.center(1), harness.center(3));
        harness.press(start, egui::Modifiers::NONE);
        harness.release(end, egui::Modifiers::NONE);
        assert_eq!(harness.selected(), vec![2, 3, 4]);
    }

    #[test]
    fn plain_click_without_selection_selects_nothing() {
        let mut harness = warmed_up();
        let at = harness.center(2);
        harness.press(at, egui::Modifiers::NONE);
        harness.release(at, egui::Modifiers::NONE);
        assert!(harness.selected().is_empty());
    }

    #[test]
    fn ctrl_click_starts_selection_then_plain_click_and_shift_extend_it() {
        let mut harness = warmed_up();
        let ctrl = egui::Modifiers::COMMAND;
        let first = harness.center(0);
        harness.press(first, ctrl);
        harness.release(first, ctrl);
        assert_eq!(harness.selected(), vec![1]);

        let third = harness.center(2);
        harness.press(third, egui::Modifiers::NONE);
        harness.release(third, egui::Modifiers::NONE);
        assert_eq!(harness.selected(), vec![1, 3]);

        let fifth = harness.center(4);
        let shift = egui::Modifiers::SHIFT;
        harness.press(fifth, shift);
        harness.release(fifth, shift);
        assert_eq!(harness.selected(), vec![3, 4, 5]);
    }

    #[test]
    fn clicking_own_reaction_chip_removes_it() {
        let mut harness = Harness::new();
        harness
            .state
            .conversations
            .get_mut(&CONTACT)
            .expect("conversation")[2]
            .reactions = brumm_core::api::ReactionsView {
            mine: Some("❤".into()),
            theirs: None,
        };
        for _ in 0..3 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        let bubble = harness.chat.row_rects[2];
        // The chip row sits under the text, left-aligned inside the bubble's padding.
        let chip = egui::pos2(bubble.left() + 12.0 + 14.0, bubble.bottom() - 7.0 - 12.0);
        harness.press(chip, egui::Modifiers::NONE);
        harness.release(chip, egui::Modifiers::NONE);
        let removal = harness.sent.iter().any(|command| {
            matches!(command, Command::React { id, emoji, .. } if *id == [3; 16] && emoji.is_empty())
        });
        assert!(removal, "expected a removal, got {:?}", harness.sent);
        assert!(harness.selected().is_empty(), "chip click must not select");
    }

    #[test]
    fn clicking_a_link_opens_it_and_leaves_the_message_unselected() {
        let mut harness = Harness::new();
        harness
            .state
            .conversations
            .get_mut(&CONTACT)
            .expect("conversation")[2]
            .text = "see https://example.com/a now".into();
        for _ in 0..4 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        let link = harness
            .chat
            .link_rects
            .first()
            .copied()
            .expect("painted link");
        let at = link.center();
        harness.press(at, egui::Modifiers::NONE);
        harness.release(at, egui::Modifiers::NONE);
        assert_eq!(
            harness.opened_urls,
            vec!["https://example.com/a".to_owned()],
            "expected the system handler to be asked to open the link"
        );
        assert!(
            harness.selected().is_empty(),
            "a link click must not select the message"
        );
    }

    #[test]
    fn wheel_notches_in_a_burst_scroll_further() {
        let mut wheel = WheelAccel::default();
        update_wheel(&mut wheel, 1.0, 0.016, &[(40.0, true)]);
        assert_eq!(wheel.factor, 1.0);
        update_wheel(&mut wheel, 1.1, 0.016, &[(40.0, true)]);
        assert_eq!(wheel.factor, 2.0);
        update_wheel(&mut wheel, 1.2, 0.016, &[(40.0, true)]);
        assert_eq!(wheel.factor, 3.0);
        for step in 0..8 {
            update_wheel(
                &mut wheel,
                1.3 + f64::from(step) * 0.02,
                0.016,
                &[(40.0, true)],
            );
        }
        assert_eq!(wheel.factor, WHEEL_FACTOR_MAX);
        update_wheel(&mut wheel, 1.5, 0.016, &[(-40.0, true)]);
        assert_eq!(wheel.factor, 1.0, "reversing starts again");
        update_wheel(&mut wheel, 1.6, 0.016, &[(-40.0, true)]);
        assert_eq!(wheel.factor, 2.0);
        update_wheel(&mut wheel, 1.6 + WHEEL_BURST_S + 0.05, 0.016, &[]);
        assert_eq!(wheel.factor, 1.0, "a pause ends the burst");
    }

    #[test]
    fn a_slow_two_finger_swipe_stays_precise_and_a_flick_speeds_up() {
        let mut slow = WheelAccel::default();
        for frame in 0..12 {
            let now = f64::from(frame) * 0.016;
            update_wheel(&mut slow, now, 0.016, &[(4.0, false)]);
        }
        assert_eq!(
            slow.factor, 1.0,
            "a slow swipe stays 1:1, got {}",
            slow.factor
        );

        let mut flick = WheelAccel::default();
        for frame in 0..12 {
            let now = f64::from(frame) * 0.016;
            update_wheel(&mut flick, now, 0.016, &[(40.0, false)]);
        }
        assert!(
            flick.factor > 2.0,
            "a flick should accelerate, got {}",
            flick.factor
        );
        assert!(flick.factor <= TRACKPAD_FACTOR_MAX, "got {}", flick.factor);
        let output_speed = 40.0 / 0.016 * flick.factor;
        assert!(
            output_speed <= TRACKPAD_SPEED_CAP + 1.0,
            "got {output_speed}"
        );
    }

    #[test]
    fn a_release_glides_a_short_distance_and_a_single_notch_does_not() {
        let mut slow = 200.0;
        assert_eq!(glide_step(&mut slow, 1.0 / 60.0), 0.0);
        assert_eq!(slow, 0.0);

        let mut velocity = 2_000.0;
        let mut distance = 0.0;
        for _ in 0..300 {
            let step = glide_step(&mut velocity, 1.0 / 60.0);
            if step == 0.0 {
                break;
            }
            distance += step;
        }
        assert_eq!(velocity, 0.0);
        assert!(
            (350.0..520.0).contains(&distance),
            "a flick should coast a short way, glided {distance}"
        );

        let mut one = WheelAccel::default();
        update_wheel(&mut one, 1.0, 0.016, &[(40.0, true)]);
        let mut coast = one.coast;
        assert_eq!(
            glide_step(&mut coast, 1.0 / 60.0),
            0.0,
            "one notch stays put, coast {}",
            one.coast
        );

        let mut burst = WheelAccel::default();
        for step in 0..4 {
            update_wheel(
                &mut burst,
                1.0 + f64::from(step) * 0.05,
                0.016,
                &[(40.0, true)],
            );
        }
        let mut coast = burst.coast;
        let step = glide_step(&mut coast, 1.0 / 60.0);
        assert!(
            step > 0.0,
            "a spinning wheel keeps going, coast {}",
            burst.coast
        );
    }

    #[test]
    fn a_fast_release_keeps_scrolling_after_the_gesture() {
        let mut harness = Harness::new();
        let messages = (0..200u8)
            .map(|index| MessageView {
                id: [index; 16],
                contact: CONTACT,
                direction: Direction::Outgoing,
                text: "hello".into(),
                attachment: None,
                reactions: Default::default(),
                sent_at_ms: 1_700_000_000_000 + u64::from(index) * 60_000,
                status: DeliveryStatus::Delivered,
            })
            .collect();
        harness.state.conversations.insert(CONTACT, messages);
        for _ in 0..4 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        let before = harness
            .chat
            .painted
            .iter()
            .map(|id| id[0])
            .min()
            .expect("painted rows");
        harness.chat.wheel.coast = 2_000.0;
        let over_chat = egui::pos2(620.0, 300.0);
        for _ in 0..40 {
            harness.frame(
                vec![egui::Event::PointerMoved(over_chat)],
                egui::Modifiers::NONE,
            );
        }
        let after = harness
            .chat
            .painted
            .iter()
            .map(|id| id[0])
            .min()
            .expect("painted rows");
        assert!(
            after < before,
            "expected the list to glide toward older messages, before {before} after {after}"
        );
        assert!(
            harness.chat.wheel.coast < 2_000.0,
            "the glide should spend its speed"
        );
    }

    #[test]
    fn long_history_lays_out_only_the_visible_rows() {
        let mut harness = Harness::new();
        let messages = (0..200u8)
            .map(|index| MessageView {
                id: [index; 16],
                contact: CONTACT,
                direction: Direction::Outgoing,
                text: "hello".into(),
                attachment: None,
                reactions: Default::default(),
                sent_at_ms: 1_700_000_000_000 + u64::from(index) * 60_000,
                status: DeliveryStatus::Delivered,
            })
            .collect();
        harness.state.conversations.insert(CONTACT, messages);
        for _ in 0..4 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        let laid_out = harness.chat.row_rects.len();
        assert!(
            laid_out > 3,
            "expected the on-screen rows, laid out {laid_out}"
        );
        assert!(
            laid_out < 80,
            "expected a window, laid out {laid_out} of 200"
        );
    }

    #[test]
    fn long_note_shapes_only_the_visible_lines_and_keeps_the_tail() {
        let mut harness = Harness::new();
        let mut messages = Vec::new();
        for index in 0..20u8 {
            messages.push(MessageView {
                id: [index; 16],
                contact: CONTACT,
                direction: Direction::Outgoing,
                text: format!("older {index}"),
                attachment: None,
                reactions: Default::default(),
                sent_at_ms: 1_700_000_000_000 + u64::from(index) * 60_000,
                status: DeliveryStatus::Delivered,
            });
        }
        let mut long = String::new();
        for _ in 0..4_000 {
            long.push_str("line of a long note\n");
        }
        let long_len = long.len();
        messages.push(MessageView {
            id: [200; 16],
            contact: CONTACT,
            direction: Direction::Outgoing,
            text: long,
            attachment: None,
            reactions: Default::default(),
            sent_at_ms: 1_700_000_000_000 + 30 * 60_000,
            status: DeliveryStatus::Delivered,
        });
        for index in 0..4u8 {
            messages.push(MessageView {
                id: [210 + index; 16],
                contact: CONTACT,
                direction: Direction::Outgoing,
                text: format!("tail {index}"),
                attachment: None,
                reactions: Default::default(),
                sent_at_ms: 1_700_000_000_000 + 40 * 60_000 + u64::from(index) * 60_000,
                status: DeliveryStatus::Delivered,
            });
        }
        harness.state.conversations.insert(CONTACT, messages);
        bubble::reset_shaped_bytes();
        for _ in 0..4 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        let shaped = bubble::shaped_bytes();
        assert!(
            shaped < long_len / 4,
            "shaped {shaped} bytes of a {long_len} byte note"
        );
        assert!(
            harness.chat.painted.iter().any(|id| id[0] == 213),
            "newest message was not on screen: {:?}",
            harness
                .chat
                .painted
                .iter()
                .map(|id| id[0])
                .collect::<Vec<_>>()
        );
        assert!(
            harness.chat.painted.iter().all(|id| id[0] >= 200),
            "rows from the top of the history were shaped: {:?}",
            harness
                .chat
                .painted
                .iter()
                .map(|id| id[0])
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn jump_offset_centers_a_short_row_and_pins_a_tall_one() {
        assert!((jump_offset(1000.0, 40.0, 600.0, 5000.0) - 720.0).abs() < 0.1);
        assert!((jump_offset(100.0, 800.0, 600.0, 5000.0) - 100.0).abs() < 0.1);
        assert_eq!(jump_offset(10.0, 40.0, 600.0, 200.0), 0.0);
    }

    #[test]
    fn away_from_bottom_is_false_when_the_room_fits_or_is_near_the_end() {
        assert!(away_from_bottom(0.0, 1000.0, 400.0));
        assert!(!away_from_bottom(590.0, 1000.0, 400.0));
        assert!(!away_from_bottom(0.0, 100.0, 400.0));
    }

    fn history(count: u8) -> Vec<MessageView> {
        (0..count)
            .map(|index| MessageView {
                id: [index; 16],
                contact: CONTACT,
                direction: Direction::Incoming,
                text: format!("message {index}"),
                attachment: None,
                reactions: Default::default(),
                sent_at_ms: 1_700_000_000_000 + u64::from(index) * 1_000,
                status: DeliveryStatus::Delivered,
            })
            .collect()
    }

    fn painted_top(harness: &Harness, id: u8) -> Option<f32> {
        let index = harness
            .chat
            .painted
            .iter()
            .position(|painted| painted[0] == id)?;
        Some(harness.chat.row_rects[index].top())
    }

    #[test]
    fn scrolling_up_loads_older_messages_without_moving_the_view() {
        let mut harness = Harness::new();
        let all = history(120);
        harness
            .state
            .conversations
            .insert(CONTACT, all[60..].to_vec());
        harness.state.has_older.insert(CONTACT);
        for _ in 0..4 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        let older_requests = |harness: &Harness| {
            harness
                .sent
                .iter()
                .filter(|command| matches!(command, Command::LoadOlder { before, .. } if *before == [60; 16]))
                .count()
        };
        assert_eq!(
            older_requests(&harness),
            0,
            "at the bottom nothing older is needed"
        );

        // Near the top of the loaded page: the next page is requested once.
        harness.chat.search.reveal([62; 16]);
        // Let the jump and any spring-back settle, so only the prepend can move the view.
        for _ in 0..120 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        assert_eq!(older_requests(&harness), 1);
        let anchor = 66;
        let before = painted_top(&harness, anchor).expect("anchor message on screen");

        // The page arrives: rows appear above, the message on screen stays where it was.
        harness.state.apply(
            Event::OlderMessages {
                contact: CONTACT,
                messages: all[..60].to_vec(),
                has_older: false,
            },
            &harness.ctx.clone(),
        );
        harness.frame(vec![], egui::Modifiers::NONE);
        let after = painted_top(&harness, anchor).expect("anchor still on screen");
        assert!(
            (after - before).abs() < 1.0,
            "view moved by {} when older messages arrived",
            after - before
        );
        for _ in 0..4 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        let settled = painted_top(&harness, anchor).expect("anchor still on screen");
        assert!(
            (settled - before).abs() < 1.0,
            "view drifted by {} after the older rows were measured",
            settled - before
        );
    }

    #[test]
    fn choosing_a_search_hit_scrolls_that_message_into_view() {
        let mut harness = Harness::new();
        harness.state.conversations.insert(CONTACT, history(60));
        for _ in 0..4 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        assert!(
            harness.chat.painted.iter().any(|id| id[0] == 59),
            "expected the newest message, saw {:?}",
            harness.chat.painted
        );
        assert!(
            harness.chat.painted.iter().all(|id| id[0] != 0),
            "the oldest message was already on screen"
        );
        harness.chat.search.reveal([0; 16]);
        for _ in 0..6 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        assert!(
            harness.chat.painted.iter().any(|id| id[0] == 0),
            "jump did not reach the oldest message: {:?}",
            harness
                .chat
                .painted
                .iter()
                .map(|id| id[0])
                .collect::<Vec<_>>()
        );
        assert!(
            harness.chat.painted.iter().all(|id| id[0] < 40),
            "jump left the newest messages on screen: {:?}",
            harness
                .chat
                .painted
                .iter()
                .map(|id| id[0])
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn jump_button_shows_when_scrolled_up_and_returns_to_the_latest() {
        let mut harness = Harness::new();
        harness.state.conversations.insert(CONTACT, history(60));
        for _ in 0..4 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        assert!(
            !harness.chat.jump_visible,
            "the button is hidden while stuck to the latest message"
        );
        let wheel = egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, 3.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Wheel,
            modifiers: egui::Modifiers::NONE,
        };
        for _ in 0..6 {
            harness.frame(
                vec![
                    egui::Event::PointerMoved(egui::pos2(450.0, 300.0)),
                    wheel.clone(),
                ],
                egui::Modifiers::NONE,
            );
        }
        for _ in 0..2 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        assert!(
            harness.chat.jump_visible,
            "scrolling up should show the jump button"
        );
        let at = harness.chat.jump_rect.expect("button rect").center();
        assert!(
            at.x > 600.0,
            "jump button should sit on the right, center was {at:?}"
        );
        harness.frame(vec![egui::Event::PointerMoved(at)], egui::Modifiers::NONE);
        harness.frame(
            vec![button(at, true, egui::Modifiers::NONE)],
            egui::Modifiers::NONE,
        );
        harness.frame(
            vec![button(at, false, egui::Modifiers::NONE)],
            egui::Modifiers::NONE,
        );
        for _ in 0..4 {
            harness.frame(vec![], egui::Modifiers::NONE);
        }
        assert!(
            !harness.chat.jump_visible,
            "the button stays up after jumping to the latest message"
        );
        assert!(
            harness.chat.painted.iter().any(|id| id[0] == 59),
            "jump did not return to the newest message: {:?}",
            harness
                .chat
                .painted
                .iter()
                .map(|id| id[0])
                .collect::<Vec<_>>()
        );
    }
}
