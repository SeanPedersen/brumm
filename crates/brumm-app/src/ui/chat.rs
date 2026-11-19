//! Chat view: header (or selection bar), message bubbles grouped by sender and day (stuck to the
//! bottom), delete dialog and the composer. Message selection works Telegram-style: right-click →
//! Select (or Ctrl+click), then click more messages; Copy / Delete act on the selection, Esc leaves.

use std::collections::HashSet;

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
const COPY_SEPARATOR: &str = "\n";

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
    /// Anchor for shift+click range selection.
    last_clicked: Option<usize>,
    #[cfg(test)]
    row_rects: Vec<Rect>,
    pub dialogs: FileDialogs,
}

/// A primary-button press on a message; becomes a drag-selection once it reaches another row.
struct Press {
    anchor: usize,
    origin: egui::Pos2,
    /// Selection before the gesture, kept when Ctrl is held so a drag adds to it.
    base: Vec<MessageId>,
    dragging: bool,
}

/// Where one message row sits this frame.
struct RowSlot {
    id: MessageId,
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
    let Some(view) = state.contact(&contact) else { return };
    let messages = state.conversations.get(&contact).map(Vec::as_slice).unwrap_or_default();
    // Selection never outlives its conversation or deleted messages.
    chat.selection.retain(|id| messages.iter().any(|message| message.id == *id));
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
        .frame(egui::Frame::new().fill(theme::SIDEBAR_BG).inner_margin(egui::Margin::symmetric(12, 8)))
        .show(ui, |ui| {
            let composer = Composer { contact, draft, dialogs: &chat.dialogs, grab_focus };
            let has_focus = composer::show(ui, composer, commands);
            if has_focus && grab_focus {
                chat.pending_focus = None;
            }
        });

    egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::CHAT_BG)).show(ui, |ui| {
        egui::ScrollArea::vertical().auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
            let margin = egui::Margin {
                left: COLUMN_PADDING_LEFT as i8,
                right: COLUMN_PADDING_RIGHT as i8,
                top: 10,
                bottom: 10,
            };
            egui::Frame::new().inner_margin(margin).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                message_list(ui, state, chat, contact, messages, commands);
            });
        });
    });
    delete_dialog(ui, chat, contact, &view.display_name, commands);
}

fn header(ui: &mut egui::Ui, state: &AppState, view: &ContactView) {
    egui::Panel::top("chat_header")
        .frame(egui::Frame::new().fill(theme::SIDEBAR_BG).inner_margin(egui::Margin::symmetric(16, 8)))
        .show(ui, |ui| {
            ui.set_min_height(theme::TOP_BAR_HEIGHT - 16.0);
            ui.horizontal_centered(|ui| {
                let is_notes = state.is_notes(&view.user_key);
                let photo = if is_notes { None } else { state.avatars.get(&view.user_key) };
                avatar::show(ui, theme::HEADER_AVATAR_SIZE, &view.user_key, &view.display_name, photo);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(egui::RichText::new(&view.display_name).size(16.0).strong());
                    let subtitle = if is_notes { "visible on all your devices" } else { "end-to-end encrypted" };
                    ui.label(egui::RichText::new(subtitle).small().color(theme::MUTED_TEXT));
                });
                if is_notes {
                    return;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(&view.fingerprint).monospace().small().color(theme::MUTED_TEXT));
                    ui.label(egui::RichText::new("✔ verified").small().color(theme::ACCENT_LIGHT))
                        .on_hover_text("You exchanged keys directly via invite");
                });
            });
        });
}

/// Replaces the header while messages are selected: count, Copy, Delete, close.
fn selection_bar(ui: &mut egui::Ui, chat: &mut ChatView, messages: &[MessageView]) {
    egui::Panel::top("chat_header")
        .frame(egui::Frame::new().fill(theme::SIDEBAR_BG).inner_margin(egui::Margin::symmetric(16, 8)))
        .show(ui, |ui| {
            ui.set_min_height(theme::TOP_BAR_HEIGHT - 16.0);
            ui.horizontal_centered(|ui| {
                if ui.add(egui::Button::new("✕").fill(Color32::TRANSPARENT)).on_hover_text("Cancel (Esc)").clicked() {
                    chat.clear_selection();
                }
                ui.label(egui::RichText::new(format!("{} selected", chat.selection.len())).size(16.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(theme::danger_button("Delete")).clicked() {
                        open_delete_dialog(chat, messages, chat.selection.clone());
                    }
                    let text = selected_text(messages, &chat.selection);
                    if ui.add_enabled(!text.is_empty(), egui::Button::new("Copy")).on_hover_text("Copy text (Ctrl+C)").clicked() {
                        ui.ctx().copy_text(text);
                    }
                });
            });
        });
    if ui.input(|input| input.events.iter().any(|event| matches!(event, egui::Event::Copy))) {
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
        messages.iter().any(|message| message.id == *id && message.direction == Direction::Outgoing)
    });
    chat.delete_dialog = Some(DeleteDialog { ids, for_everyone: all_outgoing, can_offer_everyone: all_outgoing });
}

fn delete_dialog(ui: &mut egui::Ui, chat: &mut ChatView, contact: UserKey, name: &str, commands: &mut Vec<Command>) {
    let Some(dialog) = chat.delete_dialog.as_mut() else { return };
    let (mut confirmed, mut cancelled) = (false, false);
    let count = dialog.ids.len();
    egui::Modal::new(egui::Id::unique("delete_messages")).show(ui.ctx(), |ui| {
        ui.set_width(320.0);
        let title = if count == 1 { "Delete message?".to_owned() } else { format!("Delete {count} messages?") };
        ui.heading(title);
        ui.add_space(8.0);
        if dialog.can_offer_everyone {
            ui.checkbox(&mut dialog.for_everyone, format!("Also delete for {name}"));
        } else {
            ui.label(egui::RichText::new("This removes the messages from your devices only.").color(theme::MUTED_TEXT));
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
        commands.push(Command::DeleteMessages { contact, ids: dialog.ids.clone(), for_everyone: dialog.for_everyone });
        chat.delete_dialog = None;
        chat.clear_selection();
    } else if cancelled || ui.input(|input| input.key_pressed(egui::Key::Escape)) {
        chat.delete_dialog = None;
    }
}

fn message_list(
    ui: &mut egui::Ui,
    state: &AppState,
    chat: &mut ChatView,
    contact: UserKey,
    messages: &[MessageView],
    commands: &mut Vec<Command>,
) {
    let mut slots: Vec<RowSlot> = Vec::with_capacity(messages.len());
    let mut previous: Option<&MessageView> = None;
    for (index, message) in messages.iter().enumerate() {
        let day = time_fmt::day_of(message.sent_at_ms);
        let new_day = previous.is_none_or(|prev| time_fmt::day_of(prev.sent_at_ms) != day);
        if let (true, Some(day)) = (new_day, day) {
            day_pill(ui, &time_fmt::day_label(day));
        } else if let Some(prev) = previous {
            let gap = if prev.direction == message.direction { theme::GROUP_GAP } else { theme::SENDER_CHANGE_GAP };
            ui.add_space(gap);
        }
        let ends_group = messages
            .get(index + 1)
            .is_none_or(|next| next.direction != message.direction || time_fmt::day_of(next.sent_at_ms) != day);

        let attachment = state.attachments.get(&(contact, message.id));
        request_preview(chat, contact, message, attachment.is_some(), commands);
        let inputs = bubble::Inputs {
            index,
            ends_group,
            attachment,
            selection_mode: chat.is_selecting(),
            selected: chat.selection.contains(&message.id),
        };
        let output = bubble::show(ui, message, &inputs);
        slots.push(RowSlot { id: message.id, row: output.row, hit_area: output.hit_area });
        for action in output.actions {
            apply_action(ui, chat, contact, messages, message, action, commands);
        }
        previous = Some(message);
    }
    #[cfg(test)]
    {
        chat.row_rects = slots.iter().map(|slot| slot.hit_area).collect();
    }
    handle_pointer(ui, chat, &slots);
}

const DRAG_START_DISTANCE: f32 = 4.0;
const AUTOSCROLL_SPEED: f32 = 14.0;

/// Telegram-style gestures: drag over messages to select them, Shift+click selects a range,
/// Ctrl+click toggles one, and a plain click toggles while a selection exists.
fn handle_pointer(ui: &mut egui::Ui, chat: &mut ChatView, slots: &[RowSlot]) {
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
        chat.press = slots.iter().position(|slot| {
            let area = if selecting { slot.row } else { slot.hit_area };
            area.contains(position)
        }).map(|anchor| Press {
            anchor,
            origin: position,
            base: if modifiers.command { chat.selection.clone() } else { Vec::new() },
            dragging: false,
        });
    }
    let Some(press) = chat.press.as_mut() else { return };

    if down {
        let current = row_nearest_to(slots, position.y);
        let moved = (position - press.origin).length() > DRAG_START_DISTANCE;
        if moved && current.is_some_and(|current| current != press.anchor) {
            press.dragging = true;
        }
        if let (true, Some(current)) = (press.dragging, current) {
            let (low, high) = (press.anchor.min(current), press.anchor.max(current));
            let mut selection = press.base.clone();
            selection.extend(slots[low..=high].iter().map(|slot| slot.id).filter(|id| !press.base.contains(id)));
            chat.selection = selection;
            autoscroll(ui, position.y);
        }
    }
    if !released {
        return;
    }
    let Some(press) = chat.press.take() else { return };
    if press.dragging {
        chat.last_clicked = Some(press.anchor);
        return;
    }
    let clicked_in_place = (position - press.origin).length() <= DRAG_START_DISTANCE;
    if !clicked_in_place {
        return;
    }
    if modifiers.shift {
        let from = chat.last_clicked.unwrap_or(press.anchor);
        let (low, high) = (from.min(press.anchor), from.max(press.anchor));
        chat.selection = slots[low..=high].iter().map(|slot| slot.id).collect();
    } else if modifiers.command || selecting {
        chat.toggle(slots[press.anchor].id);
    } else {
        return;
    }
    chat.last_clicked = Some(press.anchor);
}

/// The row whose vertical span is closest to `y` (so dragging above/below the list still works).
fn row_nearest_to(slots: &[RowSlot], y: f32) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| distance_to_span(a.row, y).total_cmp(&distance_to_span(b.row, y)))
        .map(|(index, _)| index)
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
fn request_preview(chat: &mut ChatView, contact: UserKey, message: &MessageView, loaded: bool, commands: &mut Vec<Command>) {
    let is_image = message.attachment.as_ref().is_some_and(|attachment| files::is_image_name(&attachment.name));
    if is_image && !loaded && chat.requested_attachments.insert((contact, message.id)) {
        commands.push(Command::LoadAttachment { contact, id: message.id });
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
            let emoji = if message.reactions.mine.as_deref() == Some(emoji.as_str()) { String::new() } else { emoji };
            commands.push(Command::React { contact, id: message.id, emoji });
        }
        Action::ToggleSelect => chat.toggle(message.id),
        Action::Copy => ui.ctx().copy_text(message.text.clone()),
        Action::SaveAs => {
            let Some(attachment) = &message.attachment else { return };
            chat.requested_attachments.insert((contact, message.id));
            commands.push(Command::LoadAttachment { contact, id: message.id });
            chat.dialogs.pick_save_path(ui.ctx(), contact, message.id, attachment.name.clone());
        }
        Action::Delete => {
            let ids = if chat.selection.contains(&message.id) { chat.selection.clone() } else { vec![message.id] };
            open_delete_dialog(chat, messages, ids);
        }
    }
}

fn day_pill(ui: &mut egui::Ui, label: &str) {
    ui.add_space(DAY_PILL_MARGIN);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), FontId::proportional(13.0), Color32::WHITE);
    let size = galley.size() + DAY_PILL_PADDING * 2.0;
    let (row, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), size.y), Sense::hover());
    let pill = Rect::from_center_size(row.center(), size);
    ui.painter().rect_filled(pill, size.y / 2.0, theme::SERVICE_PILL);
    ui.painter().galley(pill.min + DAY_PILL_PADDING, galley, Color32::WHITE);
    ui.add_space(DAY_PILL_MARGIN);
}

#[cfg(test)]
mod tests {
    use brumm_core::api::{ContactView, DeliveryStatus, Trust};

    use super::*;

    const CONTACT: UserKey = [1; 32];

    fn state() -> AppState {
        let mut state = AppState { ready: true, selected: Some(CONTACT), ..Default::default() };
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
                direction: if index % 2 == 0 { Direction::Outgoing } else { Direction::Incoming },
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
    }

    impl Harness {
        fn new() -> Self {
            let ctx = egui::Context::default();
            crate::ui::theme::apply(&ctx);
            Self { ctx, state: state(), chat: ChatView::default(), draft: String::new(), sent: Vec::new() }
        }

        fn frame(&mut self, mut events: Vec<egui::Event>, modifiers: egui::Modifiers) {
            events.insert(0, egui::Event::ModifiersChanged(modifiers));
            let screen_rect = Some(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 600.0)));
            let input = egui::RawInput { screen_rect, events, ..Default::default() };
            let Self { ctx, state, chat, draft, sent } = self;
            let mut output = ctx.run_ui(input, |ui| {
                let mut commands = Vec::new();
                show(ui, state, chat, CONTACT, draft, &mut commands);
                sent.extend(commands);
            });
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
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers }
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
        harness.state.conversations.get_mut(&CONTACT).expect("conversation")[2].reactions =
            brumm_core::api::ReactionsView { mine: Some("❤".into()), theirs: None };
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
}
