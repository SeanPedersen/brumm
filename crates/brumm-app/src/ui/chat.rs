//! Chat view: header (or selection bar), message bubbles grouped by sender and day (stuck to the
//! bottom), delete dialog and the composer. The message list is windowed: only rows near the
//! viewport are shaped, and a very long note shapes only its visible lines.
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
        composer::{self, Composer},
        files::{self, FileDialogs},
        theme, time_fmt,
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
    /// Wheel and two-finger acceleration for the message list.
    wheel: WheelAccel,
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
    /// Visual-line byte offsets for a long note. Empty for an ordinary message.
    line_starts: Vec<u32>,
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
    if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
        chat.clear_selection();
    }

    if chat.is_selecting() {
        selection_bar(ui, chat, messages);
    } else {
        header(ui, state, view);
    }
    if chat.focused_contact != Some(contact) {
        chat.pending_focus = Some(contact);
    }
    chat.focused_contact = Some(contact);
    let grab_focus = chat.pending_focus == Some(contact);
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
            apply_wheel_acceleration(ui, &mut chat.wheel);
            egui::ScrollArea::vertical()
                .auto_shrink(false)
                .animated(false)
                .stick_to_bottom(true)
                .show_viewport(ui, |ui, viewport| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let column_width =
                        (ui.available_width() - COLUMN_PADDING_LEFT - COLUMN_PADDING_RIGHT)
                            .max(1.0);
                    sync_row_heights(chat, contact, messages, column_width);
                    let rows_height: f32 = chat.rows.iter().map(|row| row.height).sum();
                    let total_height = LIST_PADDING_TOP + rows_height + LIST_PADDING_BOTTOM;
                    ui.set_height(total_height);

                    let (start, end, prefix) =
                        visible_span(&chat.rows, viewport.min.y, viewport.max.y);
                    if start == end {
                        return;
                    }
                    let visible_height: f32 =
                        chat.rows[start..end].iter().map(|row| row.height).sum();
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
                    handle_pointer(ui, chat, messages, &slots);
                });
        });
    delete_dialog(ui, chat, contact, &view.display_name, commands);
}

fn header(ui: &mut egui::Ui, state: &AppState, view: &ContactView) {
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
                if is_notes {
                    return;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
) -> (f32, Vec<u32>) {
    let (bubble, line_starts) = bubble::estimated_height(message, column_width);
    let mut height = bubble;
    let day = time_fmt::day_of(message.sent_at_ms);
    let new_day = previous.is_none_or(|prev| time_fmt::day_of(prev.sent_at_ms) != day);
    if new_day && day.is_some() {
        height += DAY_HEADER_ESTIMATE;
    } else if let Some(prev) = previous {
        height += if prev.direction == message.direction {
            theme::GROUP_GAP
        } else {
            theme::SENDER_CHANGE_GAP
        };
    }
    (height.max(1.0), line_starts)
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
    if chat.rows.len() == messages.len()
        && chat
            .rows
            .iter()
            .zip(messages)
            .all(|(row, message)| row.id == message.id && row.stamp == row_stamp(message))
    {
        return;
    }
    let mut previous: HashMap<MessageId, CachedRow> =
        chat.rows.drain(..).map(|row| (row.id, row)).collect();
    let mut prev_message: Option<&MessageView> = None;
    chat.rows.reserve(messages.len());
    for message in messages {
        let stamp = row_stamp(message);
        let (height, line_starts) = match previous.remove(&message.id) {
            Some(cached) if cached.stamp == stamp => (cached.height, cached.line_starts),
            _ => estimate_row(prev_message, message, column_width),
        };
        chat.rows.push(CachedRow {
            id: message.id,
            stamp,
            height,
            line_starts,
        });
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
        request_preview(chat, contact, message, attachment.is_some(), commands);
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
        let used = (ui.cursor().min.y - top).max(1.0);
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
    use brumm_core::api::{ContactView, DeliveryStatus, Trust};

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
}
