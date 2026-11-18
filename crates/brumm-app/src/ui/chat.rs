//! Chat view: header, message bubbles (stuck to the bottom) and the composer.
//! Enter sends, Shift+Enter inserts a newline.

use brumm_core::{
    api::{Command, DeliveryStatus, Direction, MessageView},
    identity::UserKey,
};
use jiff::{Timestamp, tz::TimeZone};

use crate::{state::AppState, ui::theme};

const COMPOSER_ROWS: usize = 2;

pub fn show(ui: &mut egui::Ui, state: &AppState, contact: UserKey, draft: &mut String, commands: &mut Vec<Command>) {
    let Some(view) = state.contact(&contact) else { return };
    egui::Panel::top("chat_header").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.heading(&view.display_name);
            ui.label(egui::RichText::new("✔ verified").small().color(theme::ACCENT))
                .on_hover_text("You exchanged keys directly via invite");
            ui.label(egui::RichText::new(&view.fingerprint).monospace().small().color(theme::MUTED_TEXT));
        });
    });
    egui::Panel::bottom("composer").show(ui, |ui| composer(ui, contact, draft, commands));

    let messages = state.conversations.get(&contact).map(Vec::as_slice).unwrap_or_default();
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for message in messages {
                bubble(ui, message);
            }
        });
}

fn composer(ui: &mut egui::Ui, contact: UserKey, draft: &mut String, commands: &mut Vec<Command>) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let send_width = 64.0;
        let input = ui.add(
            egui::TextEdit::multiline(draft)
                .hint_text("Message")
                .desired_rows(COMPOSER_ROWS)
                .desired_width(ui.available_width() - send_width),
        );
        let enter_pressed = input.has_focus()
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        let clicked = ui.add_sized([send_width - 8.0, 32.0], egui::Button::new("Send")).clicked();
        if (enter_pressed || clicked) && !draft.trim().is_empty() {
            commands.push(Command::SendText { contact, text: std::mem::take(draft) });
            input.request_focus();
        }
    });
    ui.add_space(4.0);
}

fn bubble(ui: &mut egui::Ui, message: &MessageView) {
    let outgoing = message.direction == Direction::Outgoing;
    let (layout, fill) = if outgoing {
        (egui::Layout::right_to_left(egui::Align::TOP), theme::OUTGOING_BUBBLE)
    } else {
        (egui::Layout::left_to_right(egui::Align::TOP), theme::INCOMING_BUBBLE)
    };
    let max_width = ui.available_width() * theme::BUBBLE_MAX_WIDTH_FRACTION;
    ui.with_layout(layout, |ui| {
        egui::Frame::new()
            .fill(fill)
            .corner_radius(theme::BUBBLE_RADIUS)
            .inner_margin(theme::BUBBLE_MARGIN)
            .show(ui, |ui| {
                ui.set_max_width(max_width);
                ui.vertical(|ui| {
                    ui.add(egui::Label::new(&message.text).wrap());
                    let meta = format!("{} {}", local_time(message.sent_at_ms), status_marker(message));
                    ui.label(egui::RichText::new(meta.trim_end()).small().color(theme::MUTED_TEXT));
                });
            });
    });
    ui.add_space(2.0);
}

fn status_marker(message: &MessageView) -> &'static str {
    if message.direction == Direction::Incoming {
        return "";
    }
    match message.status {
        DeliveryStatus::Pending => "🕓",
        DeliveryStatus::Sent => "✔",
        DeliveryStatus::Delivered => "✔✔",
        DeliveryStatus::Failed => "⚠ not delivered",
    }
}

fn local_time(unix_ms: u64) -> String {
    Timestamp::from_millisecond(unix_ms as i64)
        .map(|timestamp| timestamp.to_zoned_datetime(TimeZone::system()).strftime("%H:%M").to_string())
        .unwrap_or_default()
}
