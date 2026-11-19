//! Message composer: attach button, a one-line pill that grows with its content (Shift+Enter adds
//! a line), emoji picker and a round send button. Enter sends.

use brumm_core::{api::Command, identity::UserKey};
use egui::{Align2, Color32, FontId, Sense, Vec2};

use crate::ui::{emoji, files::FileDialogs, theme};

const PILL_MIN_HEIGHT: f32 = 44.0;
const PILL_MAX_TEXT_HEIGHT: f32 = 140.0;
const ROUND_BUTTON_SIZE: f32 = 44.0;
const ICON_FONT_SIZE: f32 = 22.0;

pub struct Composer<'a> {
    pub contact: UserKey,
    pub draft: &'a mut String,
    pub dialogs: &'a FileDialogs,
    /// Set for one frame after switching chats so typing can start immediately.
    pub grab_focus: bool,
}

pub fn show(ui: &mut egui::Ui, composer: Composer, commands: &mut Vec<Command>) {
    let Composer { contact, draft, dialogs, grab_focus } = composer;
    let input_id = ui.make_persistent_id(("composer", contact));
    let enter_pressed = ui.memory(|memory| memory.has_focus(input_id))
        && ui.input_mut(|input| !input.modifiers.shift && input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
    if grab_focus {
        ui.memory_mut(|memory| memory.request_focus(input_id));
    }

    let mut send_clicked = false;
    ui.horizontal(|ui| {
        if round_button(ui, "📎", "Send a file", false) {
            dialogs.pick_attachment(ui.ctx(), contact);
        }
        // Right-to-left: the send button takes its space first, the pill fills the rest. A
        // hand-computed width would overflow and fight the sidebar resize.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            send_clicked = round_button(ui, "➤", "Send", !draft.trim().is_empty());
            pill(ui, input_id, draft);
        });
    });

    if (enter_pressed || send_clicked) && !draft.trim().is_empty() {
        commands.push(Command::SendText { contact, text: draft.trim().to_owned() });
        draft.clear();
        ui.memory_mut(|memory| memory.request_focus(input_id));
    }
}

fn pill(ui: &mut egui::Ui, input_id: egui::Id, draft: &mut String) {
    let width = ui.available_width();
    egui::Frame::new()
        .fill(theme::INPUT_BG)
        .corner_radius(theme::INPUT_RADIUS)
        .inner_margin(egui::Margin { left: 18, right: 6, top: 0, bottom: 0 })
        .show(ui, |ui| {
            ui.set_width(width - 24.0);
            ui.set_min_height(PILL_MIN_HEIGHT);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let emoji_button = ui.add(
                    egui::Button::new(egui::RichText::new("☺").size(ICON_FONT_SIZE).color(theme::MUTED_TEXT))
                        .fill(Color32::TRANSPARENT)
                        .corner_radius(egui::CornerRadius::same(16)),
                );
                if let Some(chosen) = emoji::picker(&emoji_button.on_hover_text("Emoji")) {
                    emoji::insert_at_cursor(ui.ctx(), input_id, draft, chosen);
                    ui.memory_mut(|memory| memory.request_focus(input_id));
                }
                // Vertical padding that makes one text line exactly fill the pill's minimum height.
                let line_height = ui.text_style_height(&egui::TextStyle::Body);
                let text_margin = ((PILL_MIN_HEIGHT - line_height) / 2.0).floor() as i8;
                // An explicit height budget: inside a horizontal row the available height is a single
                // line, which would make the input scroll instead of grow.
                let mut budget = ui.available_rect_before_wrap();
                budget.max.y = budget.min.y + PILL_MAX_TEXT_HEIGHT;
                ui.scope_builder(egui::UiBuilder::new().max_rect(budget).layout(egui::Layout::top_down(egui::Align::Min)), |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(draft)
                                .id(input_id)
                                .hint_text("Write a message…")
                                .desired_rows(1)
                                .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(0, text_margin)))
                                .desired_width(f32::INFINITY),
                        );
                    });
                });
            });
        });
}

/// Round icon button; `active` fills it with the accent color (used for "send").
fn round_button(ui: &mut egui::Ui, icon: &str, tooltip: &str, active: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(ROUND_BUTTON_SIZE), Sense::click());
    let fill = match (active, response.hovered()) {
        (true, true) => theme::ACCENT_HOVER,
        (true, false) => theme::ACCENT,
        (false, true) => theme::INPUT_BG,
        (false, false) => Color32::TRANSPARENT,
    };
    let icon_color = if active { Color32::WHITE } else { theme::MUTED_TEXT };
    ui.painter().circle_filled(rect.center(), ROUND_BUTTON_SIZE / 2.0, fill);
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(ICON_FONT_SIZE), icon_color);
    response.on_hover_text(tooltip).clicked() && (active || icon != "➤")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Height of the composer after a few frames for the given draft.
    fn composer_height(draft: &str) -> f32 {
        let ctx = egui::Context::default();
        crate::ui::theme::apply(&ctx);
        let dialogs = FileDialogs::default();
        let mut draft = draft.to_owned();
        let mut height = 0.0;
        for _ in 0..3 {
            let screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 600.0)));
            let mut output = ctx.run_ui(egui::RawInput { screen_rect, ..Default::default() }, |ui| {
                let mut commands = Vec::new();
                egui::Panel::bottom("composer").show(ui, |ui| {
                    let composer = Composer { contact: [1; 32], draft: &mut draft, dialogs: &dialogs, grab_focus: false };
                    show(ui, composer, &mut commands);
                    height = ui.min_rect().height();
                });
            });
            output.textures_delta.clear();
        }
        height
    }

    #[test]
    fn single_line_by_default_and_grows_with_lines() {
        let single = composer_height("");
        let multi = composer_height("one\ntwo\nthree");
        println!("single={single} multi={multi} six={}", composer_height("1\n2\n3\n4\n5\n6"));
        assert!(single < 70.0, "empty composer should be one line, was {single}");
        assert!(multi > single + 5.0, "composer should grow: {single} -> {multi}");
    }
}
