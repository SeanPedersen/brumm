//! Group-only parts of the message list: who wrote a row (runs of bubbles break when the author
//! changes, not only the direction), the sender's name above the first bubble of a run, and
//! system lines ("Bob joined") as centered service pills instead of bubbles.

use brumm_core::{
    api::{Direction, MessageView},
    identity::UserKey,
};
use egui::{Color32, FontId, Rect, Sense, Vec2};

use crate::ui::{avatar, theme};

const SENDER_FONT_SIZE: f32 = 13.0;
const SENDER_GAP_BELOW: f32 = 2.0;
/// Inset of the name from the bubble's left edge.
const SENDER_INSET_X: f32 = 12.0;
/// Sender line height including the gap below it, until the row is measured.
pub const SENDER_LABEL_ESTIMATE: f32 = 17.0 + SENDER_GAP_BELOW;
const SYSTEM_FONT_SIZE: f32 = 13.0;
const SYSTEM_PADDING: Vec2 = egui::vec2(12.0, 4.0);
const SYSTEM_MARGIN: f32 = 6.0;
/// System pill plus its margins, until the row is measured.
pub const SYSTEM_ROW_ESTIMATE: f32 = SYSTEM_MARGIN * 2.0 + 21.0;

/// The author of a row, for grouping consecutive bubbles.
#[derive(PartialEq, Eq)]
pub enum Author {
    System,
    Own,
    /// A contact in a 1:1 chat (`None`) or a group member.
    Other(Option<UserKey>),
}

pub fn author(message: &MessageView) -> Author {
    if message.system {
        return Author::System;
    }
    match message.direction {
        Direction::Outgoing => Author::Own,
        Direction::Incoming => Author::Other(message.sender.as_ref().map(|sender| sender.user_key)),
    }
}

/// The name to show above an incoming group bubble that starts a run.
pub fn sender_label<'a>(previous: Option<&MessageView>, message: &'a MessageView, new_day: bool) -> Option<&'a str> {
    let sender = message.sender.as_ref()?;
    if message.system || message.direction == Direction::Outgoing {
        return None;
    }
    let continues = !new_day && previous.is_some_and(|previous| author(previous) == author(message));
    (!continues).then_some(sender.display_name.as_str())
}

/// Paints the sender's name in a color derived from their key (like their avatar).
pub fn paint_sender(ui: &mut egui::Ui, name: &str, key: &UserKey) {
    let color = avatar::color_of(key);
    let galley = ui
        .painter()
        .layout_no_wrap(name.to_owned(), FontId::proportional(SENDER_FONT_SIZE), color);
    let (row, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), galley.size().y), Sense::hover());
    ui.painter()
        .galley(egui::pos2(row.left() + SENDER_INSET_X, row.top()), galley, color);
    ui.add_space(SENDER_GAP_BELOW);
}

/// Paints a system line as a centered pill; returns the row it took.
pub fn paint_system(ui: &mut egui::Ui, text: &str) -> Rect {
    ui.add_space(SYSTEM_MARGIN);
    let wrap = (ui.available_width() - SYSTEM_PADDING.x * 2.0).max(1.0);
    let galley = ui.painter().layout(
        text.to_owned(),
        FontId::proportional(SYSTEM_FONT_SIZE),
        Color32::WHITE,
        wrap,
    );
    let size = galley.size() + SYSTEM_PADDING * 2.0;
    let (row, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), size.y), Sense::hover());
    let pill = Rect::from_center_size(row.center(), size);
    ui.painter()
        .rect_filled(pill, theme::SYSTEM_PILL_RADIUS, theme::SERVICE_PILL);
    ui.painter()
        .galley(pill.min + SYSTEM_PADDING, galley, Color32::WHITE);
    ui.add_space(SYSTEM_MARGIN);
    row
}

#[cfg(test)]
mod tests {
    use brumm_core::api::{ConversationId, DeliveryStatus, SenderView};

    use super::*;

    fn message(direction: Direction, sender: Option<u8>, system: bool) -> MessageView {
        MessageView {
            id: [0; 16],
            conversation: ConversationId::Group([7; 32]),
            direction,
            text: "hi".into(),
            attachment: None,
            reactions: Default::default(),
            sent_at_ms: 0,
            status: DeliveryStatus::Delivered,
            sender: sender.map(|byte| SenderView {
                user_key: [byte; 32],
                display_name: format!("member {byte}"),
            }),
            system,
        }
    }

    #[test]
    fn the_name_shows_once_per_run_of_one_member() {
        let bob = message(Direction::Incoming, Some(1), false);
        let carol = message(Direction::Incoming, Some(2), false);
        assert_eq!(sender_label(None, &bob, false), Some("member 1"));
        assert_eq!(sender_label(Some(&bob), &bob, false), None);
        assert_eq!(sender_label(Some(&bob), &carol, false), Some("member 2"));
        assert_eq!(sender_label(Some(&bob), &bob, true), Some("member 1"), "a new day starts a run");
    }

    #[test]
    fn own_messages_system_lines_and_direct_chats_have_no_name() {
        let own = message(Direction::Outgoing, Some(9), false);
        let system = message(Direction::Incoming, None, true);
        let direct = message(Direction::Incoming, None, false);
        assert_eq!(sender_label(None, &own, false), None);
        assert_eq!(sender_label(None, &system, false), None);
        assert_eq!(sender_label(None, &direct, false), None);
        assert!(author(&system) != author(&direct));
    }
}
