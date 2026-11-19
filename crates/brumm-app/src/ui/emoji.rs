//! Emoji picker popup and cursor-aware insertion into the composer.
//! egui renders emoji monochrome (bundled Noto Emoji), so only widely supported ones are listed.

use egui::{Id, Popup, PopupCloseBehavior, Response, RichText, text::{CCursor, CCursorRange}};

const POPUP_WIDTH: f32 = 320.0;
const POPUP_HEIGHT: f32 = 260.0;
const EMOJI_SIZE: f32 = 22.0;
const EMOJI_CELL: f32 = 34.0;

const CATEGORIES: [(&str, &str); 5] = [
    ("Smileys", "😀 😃 😄 😁 😆 😅 😂 😊 😇 🙂 😉 😍 😘 😋 😛 😜 😎 🤓 😏 😒 😞 😔 😟 😕 😢 😭 😤 😠 😡 😱 😨 😰 😳 😴 😷"),
    ("Gestures", "👍 👎 👌 ✌ 👊 ✊ 👋 👏 🙌 🙏 💪 👀 👂 👃 👄 👅"),
    ("Hearts", "❤ 💔 💕 💖 💗 💙 💚 💛 💜 💘 💝 ✨ ⭐ 🔥 💯 ✅ ❌ ⚡"),
    ("Nature & food", "🌞 🌙 ⭐ ☁ ☔ ❄ 🌸 🌹 🌲 🐶 🐱 🐭 🐸 🐵 🐔 🐧 🍎 🍌 🍕 🍔 🍺 ☕ 🎂"),
    ("Objects", "🎉 🎁 🎈 🎵 🎶 📷 📞 💻 📱 📚 ✉ 🔒 🔑 💡 🚀 ✈ 🚗 🏠 ⚽ 🏆"),
];

/// Shows the popup anchored to `button`; returns an emoji if one was clicked.
pub fn picker(button: &Response) -> Option<&'static str> {
    let mut chosen = None;
    Popup::from_toggle_button_response(button)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .width(POPUP_WIDTH)
        .show(|ui| {
            ui.set_min_size(egui::vec2(POPUP_WIDTH, POPUP_HEIGHT));
            egui::ScrollArea::vertical().max_height(POPUP_HEIGHT).show(ui, |ui| {
                for (title, emojis) in CATEGORIES {
                    ui.label(RichText::new(title).small().color(crate::ui::theme::MUTED_TEXT));
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
                        for emoji in emojis.split(' ') {
                            let cell = egui::Button::new(RichText::new(emoji).size(EMOJI_SIZE))
                                .frame(false)
                                .min_size(egui::vec2(EMOJI_CELL, EMOJI_CELL));
                            if ui.add(cell).clicked() {
                                chosen = Some(emoji);
                            }
                        }
                    });
                    ui.add_space(6.0);
                }
            });
        });
    chosen
}

/// Inserts `text` at the text cursor of the edit with `edit_id` (or appends if it has none).
pub fn insert_at_cursor(ctx: &egui::Context, edit_id: Id, draft: &mut String, text: &str) {
    let mut state = egui::TextEdit::load_state(ctx, edit_id).unwrap_or_default();
    let char_count = draft.chars().count();
    let range = state.cursor.char_range();
    let (start, end) = range.map_or((char_count, char_count), |range| {
        let (a, b) = (range.primary.index.0, range.secondary.index.0);
        (a.min(b).min(char_count), a.max(b).min(char_count))
    });
    let byte_of = |char_index: usize| draft.char_indices().nth(char_index).map_or(draft.len(), |(byte, _)| byte);
    draft.replace_range(byte_of(start)..byte_of(end), text);
    let caret = CCursor::new(start + text.chars().count());
    state.cursor.set_char_range(Some(CCursorRange::one(caret)));
    state.store(ctx, edit_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_at_end_without_state() {
        let ctx = egui::Context::default();
        let mut draft = "hi ".to_owned();
        insert_at_cursor(&ctx, Id::unique("edit"), &mut draft, "😀");
        assert_eq!(draft, "hi 😀");
    }
}
