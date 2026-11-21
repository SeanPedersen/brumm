//! Message composer: attach button, a one-line pill that grows with its content (Shift+Enter adds
//! a line) and an emoji picker. Enter sends.

use brumm_core::{api::Command, identity::UserKey};
use egui::{Rect, Sense, Vec2};

use crate::ui::{emoji, files::FileDialogs, icons, theme};

const PILL_MIN_HEIGHT: f32 = 44.0;
const PILL_MAX_TEXT_HEIGHT: f32 = 140.0;
const ATTACH_BUTTON_WIDTH: f32 = ICON_SIZE;
const ATTACH_GAP: f32 = 6.0;
const ICON_SIZE: f32 = 24.0;
const EMOJI_BUTTON_SIZE: f32 = 32.0;

pub struct Composer<'a> {
    pub contact: UserKey,
    pub draft: &'a mut String,
    pub dialogs: &'a FileDialogs,
    /// Set after switching chats until the input actually holds focus, so typing can start
    /// immediately even if the click that opened the chat takes focus away on the same frame.
    pub grab_focus: bool,
}

/// Returns whether the input already held keyboard focus when the frame began.
pub fn show(ui: &mut egui::Ui, composer: Composer, commands: &mut Vec<Command>) -> bool {
    let Composer {
        contact,
        draft,
        dialogs,
        grab_focus,
    } = composer;
    let input_id = ui.make_persistent_id(("composer", contact));
    let had_focus = ui.memory(|memory| memory.has_focus(input_id));
    let enter_pressed = had_focus
        && ui.input_mut(|input| {
            !input.modifiers.shift && input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
        });
    if grab_focus {
        ui.memory_mut(|memory| memory.request_focus(input_id));
    }

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = ATTACH_GAP;
        if attach_button(ui) {
            dialogs.pick_attachment(ui.ctx(), contact);
        }
        pill(ui, input_id, draft);
    });

    if enter_pressed && !draft.trim().is_empty() {
        commands.push(Command::SendText {
            contact,
            text: draft.trim().to_owned(),
        });
        draft.clear();
        ui.memory_mut(|memory| memory.request_focus(input_id));
    }
    had_focus
}

fn pill(ui: &mut egui::Ui, input_id: egui::Id, draft: &mut String) {
    let width = ui.available_width();
    egui::Frame::new()
        .fill(theme::INPUT_BG)
        .corner_radius(theme::INPUT_RADIUS)
        .inner_margin(egui::Margin {
            left: 18,
            right: 6,
            top: 0,
            bottom: 0,
        })
        .show(ui, |ui| {
            ui.set_width(width - 24.0);
            ui.set_min_height(PILL_MIN_HEIGHT);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (emoji_rect, emoji_button) =
                    ui.allocate_exact_size(Vec2::splat(EMOJI_BUTTON_SIZE), Sense::click());
                let emoji_color = if emoji_button.hovered() {
                    theme::TEXT
                } else {
                    theme::MUTED_TEXT
                };
                icons::smiley(
                    ui.painter(),
                    Rect::from_center_size(emoji_rect.center(), Vec2::splat(ICON_SIZE)),
                    emoji_color,
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
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(budget)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                    |ui| {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                let output = egui::TextEdit::multiline(draft)
                                    .id(input_id)
                                    .hint_text("Write a message…")
                                    .desired_rows(1)
                                    .frame(
                                        egui::Frame::new()
                                            .inner_margin(egui::Margin::symmetric(0, text_margin)),
                                    )
                                    .desired_width(f32::INFINITY)
                                    .show(ui);
                                paste_menu(&output.response, input_id);
                            });
                    },
                );
            });
        });
}

/// Marker in egui's temp data: the user chose "Paste" in the input's context menu.
pub fn paste_menu_requested_id() -> egui::Id {
    egui::Id::NULL.with("composer_paste_requested")
}

/// Right-click menu of the input. Paste is resolved in the main screen, which can also send copied files.
fn paste_menu(response: &egui::Response, input_id: egui::Id) {
    response.context_menu(|ui| {
        if ui.button("Paste").clicked() {
            ui.data_mut(|data| data.insert_temp(paste_menu_requested_id(), true));
            ui.memory_mut(|memory| memory.request_focus(input_id));
            ui.ctx().request_repaint();
            ui.close();
        }
    });
}

/// Paperclip button: narrow, but as tall as the input pill so the icon is centered on it.
fn attach_button(ui: &mut egui::Ui) -> bool {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ATTACH_BUTTON_WIDTH, PILL_MIN_HEIGHT), Sense::click());
    let color = if response.hovered() {
        theme::TEXT
    } else {
        theme::MUTED_TEXT
    };
    icons::paperclip(
        ui.painter(),
        Rect::from_center_size(rect.center(), Vec2::splat(ICON_SIZE)),
        color,
    );
    response.on_hover_text("Send a file").clicked()
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
            let screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            ));
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect,
                    ..Default::default()
                },
                |ui| {
                    let mut commands = Vec::new();
                    egui::Panel::bottom("composer").show(ui, |ui| {
                        let composer = Composer {
                            contact: [1; 32],
                            draft: &mut draft,
                            dialogs: &dialogs,
                            grab_focus: false,
                        };
                        show(ui, composer, &mut commands);
                        height = ui.min_rect().height();
                    });
                },
            );
            output.textures_delta.clear();
        }
        height
    }

    #[test]
    fn single_line_by_default_and_grows_with_lines() {
        let single = composer_height("");
        let multi = composer_height("one\ntwo\nthree");
        println!(
            "single={single} multi={multi} six={}",
            composer_height("1\n2\n3\n4\n5\n6")
        );
        assert!(
            single < 70.0,
            "empty composer should be one line, was {single}"
        );
        assert!(
            multi > single + 5.0,
            "composer should grow: {single} -> {multi}"
        );
    }
}
